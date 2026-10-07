//! [`SystemOneBackend`]: the SystemOne HTTP API (`POST /v1/systemone`).
//!
//! Served by TypeSafe (`https://api.typesafe.ai`), by OpenCode Zen
//! (`https://opencode.ai/zen`), by Cloudflare Workers AI (Clef, at its own
//! URL — see [`SystemOneBackend::workers_ai`]), and by self-hosted
//! TypeSafe-style servers such as JevK5. Responses are parsed leniently: unknown fields are ignored,
//! and a missing `confidence`, `choice`, `score` or `legend` is computed from
//! what is present. What is present is then validated like every backend's
//! answers (see [`DecisionBackend`]).

use super::answer::{Answer, ChoiceAnswer, DecisionUsage, Evaluation, NoulAnswer, ScoreAnswer};
use super::backend::{Capabilities, DecisionBackend};
use super::error::DecisionError;
use super::question::{Question, QuestionKind, Request};
use super::{add_billed, billed_only};
use crate::retry::RetryConfig;
use serde_json::Value;
use std::sync::Mutex;
use std::time::Duration;

pub(crate) const TYPESAFE_BASE_URL: &str = "https://api.typesafe.ai";
pub(crate) const TYPESAFE_HOST: &str = "api.typesafe.ai";
pub(crate) const OPENCODE_ZEN_BASE_URL: &str = "https://opencode.ai/zen";
pub(crate) const TYPESAFE_API_KEY_ENV: &str = "TYPESAFE_API_KEY";
pub(crate) const TYPESAFE_BASE_URL_ENV: &str = "TYPESAFE_BASE_URL";
pub(crate) const OPENCODE_API_KEY_ENV: &str = "OPENCODE_API_KEY";
/// Cloudflare's REST API, under which Workers AI models run.
pub(crate) const WORKERS_AI_API_BASE: &str = "https://api.cloudflare.com/client/v4";
/// Wrangler's name for a Cloudflare API token, read first.
pub(crate) const CLOUDFLARE_API_TOKEN_ENV: &str = "CLOUDFLARE_API_TOKEN";
/// The name Workers AI's model pages use, read second.
pub(crate) const CLOUDFLARE_AUTH_TOKEN_ENV: &str = "CLOUDFLARE_AUTH_TOKEN";
/// The host that bills Workers AI at its list price: the REST API.
const CLOUDFLARE_HOST: &str = "api.cloudflare.com";
/// Workers AI's error code for an account that used up its daily free
/// allocation: answered with a 429, but no retry succeeds until the reset.
const CLOUDFLARE_DAILY_LIMIT: u64 = 3036;
/// Workers AI's error code for a request that does not match the model's
/// input schema.
const CLOUDFLARE_SCHEMA_ERROR: u64 = 5006;

/// Longest error-body excerpt kept in a [`DecisionError`].
pub(crate) const MAX_ERROR_BODY: usize = 2_000;

#[derive(Clone)]
enum Endpoint {
    /// A base URL; requests go to `{base}/v1/systemone`.
    Fixed(String),
    /// The exact URL requests go to (a Workers AI model, a proxy).
    Exact(String),
    /// Read the variable at call time, falling back to the default.
    EnvOr { var: String, default: String },
}

#[derive(Clone)]
enum Key {
    None,
    Fixed(String),
    /// Read at call time, the first set and non-empty variable winning;
    /// none set is an error.
    Env(Vec<String>),
}

/// An HTTP client for the SystemOne API.
///
/// Cheap to clone. Keys named by environment variable are read on every
/// request, so rotating a key needs no rebuild; the key never appears in
/// `Debug` output or errors.
#[derive(Clone)]
pub struct SystemOneBackend {
    client: reqwest::Client,
    endpoint: Endpoint,
    key: Key,
    retry: RetryConfig,
    capabilities: Capabilities,
}

impl std::fmt::Debug for SystemOneBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let key = match &self.key {
            Key::None => "none".to_string(),
            Key::Fixed(_) => "[redacted]".to_string(),
            Key::Env(vars) => vars
                .iter()
                .map(|v| format!("${v}"))
                .collect::<Vec<_>>()
                .join(" or "),
        };
        f.debug_struct("SystemOneBackend")
            .field("endpoint", &self.endpoint_url())
            .field("key", &key)
            .field("retry", &self.retry)
            .field("capabilities", &self.capabilities)
            .finish_non_exhaustive()
    }
}

impl SystemOneBackend {
    /// A backend for a SystemOne-compatible server at `base_url`, sending no
    /// key. `base_url` may be the host (`http://localhost:8000`), end in
    /// `/v1`, or be the full `/v1/systemone` URL.
    ///
    /// Capabilities: every question type, 255 options, 10 levels, batching,
    /// no token limits (the server enforces its own). Not marked
    /// [`local`](Capabilities::local) — use
    /// [`DecisionModel::local`](super::DecisionModel::local) for a
    /// self-hosted server, or [`with_capabilities`](Self::with_capabilities).
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            endpoint: Endpoint::Fixed(base_url.into()),
            key: Key::None,
            retry: RetryConfig::default(),
            capabilities: Capabilities::new(QuestionKind::all()),
        }
    }

    /// TypeSafe's hosted API: base from `TYPESAFE_BASE_URL` (default
    /// `https://api.typesafe.ai`), key from `TYPESAFE_API_KEY`, both read at
    /// call time.
    pub fn typesafe() -> Self {
        Self {
            endpoint: Endpoint::EnvOr {
                var: TYPESAFE_BASE_URL_ENV.into(),
                default: TYPESAFE_BASE_URL.into(),
            },
            key: Key::Env(vec![TYPESAFE_API_KEY_ENV.into()]),
            capabilities: Capabilities::systemone(),
            ..Self::new(TYPESAFE_BASE_URL)
        }
    }

    /// OpenCode Zen's SystemOne endpoint, key from `OPENCODE_API_KEY` (read
    /// at call time).
    pub fn opencode_zen() -> Self {
        Self {
            key: Key::Env(vec![OPENCODE_API_KEY_ENV.into()]),
            capabilities: Capabilities::systemone(),
            ..Self::new(OPENCODE_ZEN_BASE_URL)
        }
    }

    /// A Workers AI model on Cloudflare account `account_id`, such as
    /// `@cf/cloudflare/clef`: requests go to
    /// `https://api.cloudflare.com/client/v4/accounts/{account_id}/ai/run/{model_path}`
    /// and the key is read at call time from `CLOUDFLARE_API_TOKEN`, then
    /// `CLOUDFLARE_AUTH_TOKEN`. Cloudflare's `{"result": ...}` envelope is
    /// unwrapped. Most callers want [`DecisionModel::clef`](super::DecisionModel::clef).
    pub fn workers_ai(account_id: impl AsRef<str>, model_path: impl AsRef<str>) -> Self {
        let url = format!(
            "{}/accounts/{}/ai/run/{}",
            WORKERS_AI_API_BASE,
            account_id.as_ref().trim(),
            model_path.as_ref().trim().trim_start_matches('/'),
        );
        Self {
            endpoint: Endpoint::Exact(url),
            key: Key::Env(vec![
                CLOUDFLARE_API_TOKEN_ENV.into(),
                CLOUDFLARE_AUTH_TOKEN_ENV.into(),
            ]),
            capabilities: Capabilities::systemone(),
            ..Self::new(WORKERS_AI_API_BASE)
        }
    }

    /// Send this key (replaces any environment variable).
    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.key = Key::Fixed(key.into());
        self
    }

    /// Read the key from this environment variable at call time.
    pub fn with_api_key_env(mut self, var: impl Into<String>) -> Self {
        self.key = Key::Env(vec![var.into()]);
        self
    }

    /// Use this base URL (replaces any environment variable); requests go
    /// to `{base_url}/v1/systemone`.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.endpoint = Endpoint::Fixed(base_url.into());
        self
    }

    /// Send requests to exactly this URL, with no `/v1/systemone` appended:
    /// a SystemOne model served at its own path (a Workers AI model, or a
    /// proxy in front of one).
    pub fn with_endpoint_url(mut self, url: impl Into<String>) -> Self {
        self.endpoint = Endpoint::Exact(url.into());
        self
    }

    /// Retry policy for 429 / 529 / transport failures. Defaults to
    /// [`RetryConfig::default`] (3 retries, 1 s initial, 2x, 30 s cap); a
    /// server `retry-after` wins over the backoff, clamped to the cap.
    pub fn with_retry(mut self, retry: RetryConfig) -> Self {
        self.retry = retry;
        self
    }

    /// Report these capabilities (e.g. a local server with different limits).
    pub fn with_capabilities(mut self, capabilities: Capabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    fn base(&self) -> String {
        let raw = match &self.endpoint {
            Endpoint::Fixed(url) | Endpoint::Exact(url) => url.clone(),
            Endpoint::EnvOr { var, default } => std::env::var(var)
                .ok()
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| default.clone()),
        };
        let trimmed = raw.trim().trim_end_matches('/');
        let trimmed = trimmed.strip_suffix("/v1/systemone").unwrap_or(trimmed);
        let trimmed = trimmed.strip_suffix("/v1").unwrap_or(trimmed);
        trimmed.to_string()
    }

    /// The URL requests are sent to, resolved now.
    pub fn endpoint_url(&self) -> String {
        match &self.endpoint {
            Endpoint::Exact(url) => url.trim().to_string(),
            _ => format!("{}/v1/systemone", self.base()),
        }
    }

    /// The host requests currently go to.
    fn host(&self) -> String {
        let url = self.endpoint_url();
        let rest = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"))
            .unwrap_or(&url);
        rest.split(['/', ':', '?', '#'])
            .next()
            .unwrap_or("")
            .to_ascii_lowercase()
    }

    /// Whether requests currently go to TypeSafe's own host (so TypeSafe's
    /// list prices apply).
    pub(crate) fn is_typesafe_host(&self) -> bool {
        self.host() == TYPESAFE_HOST
    }

    /// Whether requests currently go to Cloudflare's REST API, so Workers
    /// AI's list prices apply.
    pub(crate) fn is_cloudflare_host(&self) -> bool {
        self.host() == CLOUDFLARE_HOST
    }

    /// The key to send, and the variable it came from (to name in an auth
    /// error). The value is trimmed: a trailing newline in an exported token
    /// would otherwise fail the request before it is sent.
    fn key(&self) -> Result<Option<(String, Option<&str>)>, DecisionError> {
        match &self.key {
            Key::None => Ok(None),
            Key::Fixed(k) if k.trim().is_empty() => Err(DecisionError::MissingApiKey(
                "a non-empty key (the one given to with_api_key is empty)".into(),
            )),
            Key::Fixed(k) => Ok(Some((k.trim().to_string(), None))),
            Key::Env(vars) => {
                let found = vars.iter().find_map(|var| {
                    let value = std::env::var(var).ok()?;
                    let value = value.trim();
                    (!value.is_empty()).then(|| (value.to_string(), Some(var.as_str())))
                });
                match found {
                    Some(found) => Ok(Some(found)),
                    None => {
                        let mut names = vars.join(" or ");
                        if vars.iter().any(|v| std::env::var_os(v).is_some()) {
                            names.push_str(" (set, but empty)");
                        }
                        Err(DecisionError::MissingApiKey(names))
                    }
                }
            }
        }
    }

    async fn send_once(
        &self,
        request: &Request,
        body: &[u8],
        billed: &Mutex<Option<Evaluation>>,
    ) -> Result<Evaluation, DecisionError> {
        let key = self.key()?;
        let value = post_json_billed(
            &self.client,
            &self.endpoint_url(),
            key.as_ref().map(|(k, _)| k.as_str()),
            body,
            billed,
            &request.model,
        )
        .await
        .map_err(|e| match (e, key.as_ref().and_then(|(_, var)| *var)) {
            // Say which variable the rejected key came from.
            (DecisionError::Http { status, body }, Some(var)) if status == 401 || status == 403 => {
                DecisionError::http(status, format!("{body} (key from ${var})"))
            }
            (e, _) => e,
        })?;
        parse_billed(value, request, billed)
    }

    /// [`evaluate`](DecisionBackend::evaluate), adding to `billed` what a
    /// response that arrived but could not be used was billed for.
    pub(crate) async fn evaluate_billed(
        &self,
        request: &Request,
        billed: &Mutex<Option<Evaluation>>,
    ) -> Result<Evaluation, DecisionError> {
        // Fail on a missing key before serializing anything.
        self.key()?;
        let body = serde_json::to_vec(request)
            .map_err(|e| DecisionError::Invalid(format!("request does not serialize: {e}")))?;
        with_retries(&self.retry, || self.send_once(request, &body, billed)).await
    }
}

/// [`parse_systemone_response`], adding to `billed` what a response that
/// was answered but could not be used was billed for: its reported usage, or
/// unknown spend when the envelope held no usable response. A Cloudflare
/// `"success": false` is a failure, not an answer, and adds nothing.
fn parse_billed(
    value: Value,
    request: &Request,
    billed: &Mutex<Option<Evaluation>>,
) -> Result<Evaluation, DecisionError> {
    let body = match unwrap_envelope(value) {
        Ok(body) => body,
        Err(e) => {
            if matches!(e, DecisionError::BadResponse(_)) {
                add_billed(
                    billed,
                    &billed_only(&request.model, DecisionUsage::default(), false),
                );
            }
            return Err(e);
        }
    };
    parse_evaluation(&body, request).inspect_err(|_| add_billed(billed, &billed_by(&body, request)))
}

/// What a SystemOne body says it was billed for, answers aside: its model
/// and `usage` — or, without a `usage` object, the per-answer
/// `input_tokens` of every answer present.
fn billed_by(body: &Value, request: &Request) -> Evaluation {
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .filter(|m| !m.is_empty())
        .unwrap_or(&request.model);
    let per_answer: Vec<u64> = body
        .get("answers")
        .and_then(Value::as_object)
        .map(|answers| {
            answers
                .values()
                .filter_map(|a| a.get("input_tokens").and_then(Value::as_u64))
                .collect()
        })
        .unwrap_or_default();
    let usage = body.get("usage").filter(|u| u.is_object());
    let tokens = |k: &str| usage.and_then(|u| u.get(k)).and_then(Value::as_u64);
    billed_only(
        model,
        DecisionUsage::new(
            tokens("input_tokens").unwrap_or_else(|| per_answer.iter().sum()),
            tokens("output_tokens").unwrap_or(0),
        ),
        tokens("input_tokens").is_some() || !per_answer.is_empty(),
    )
}

/// Cloudflare's REST API wraps a model's output as
/// `{"result": {...}, "success": true, "errors": [], "messages": []}`; take
/// the `result`. A plain SystemOne response (it has `answers`) passes through.
/// A 2xx body that says `"success": false` is an error carrying Cloudflare's
/// `errors` (non-2xx responses never reach here: [`status_error`] classifies
/// them by status).
fn unwrap_envelope(value: Value) -> Result<Value, DecisionError> {
    if value.get("answers").is_some() {
        return Ok(value);
    }
    if value.get("success").and_then(Value::as_bool) == Some(false) {
        let errors = match value.get("errors") {
            Some(errors) => excerpt(&errors.to_string(), MAX_ERROR_BODY),
            None => "(no errors given)".to_string(),
        };
        if cloudflare_codes(&value).contains(&CLOUDFLARE_SCHEMA_ERROR) {
            return Err(DecisionError::Invalid(format!(
                "Cloudflare rejected the request: {errors}"
            )));
        }
        return Err(DecisionError::backend(format!(
            "Cloudflare reported failure: {errors}"
        )));
    }
    let Value::Object(mut map) = value else {
        return Ok(value);
    };
    let Some(result) = map.remove("result") else {
        return Ok(Value::Object(map));
    };
    match result {
        Value::Object(_) => Ok(result),
        // A model may hand back its JSON as text.
        Value::String(text) => match serde_json::from_str::<Value>(&text) {
            Ok(parsed @ Value::Object(_)) => Ok(parsed),
            _ => Err(DecisionError::BadResponse(format!(
                "Cloudflare's `result` is text, not a SystemOne response: {}",
                excerpt(&text, 200)
            ))),
        },
        other => Err(DecisionError::BadResponse(format!(
            "Cloudflare's `result` is {}, not a SystemOne response",
            json_kind(&other)
        ))),
    }
}

/// The `code` of each entry in a Cloudflare body's `errors`.
fn cloudflare_codes(body: &Value) -> Vec<u64> {
    body.get("errors")
        .and_then(Value::as_array)
        .map(|errors| {
            errors
                .iter()
                .filter_map(|e| e.get("code").and_then(Value::as_u64))
                .collect()
        })
        .unwrap_or_default()
}

fn json_kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// [`post_json`], adding unknown spend to `billed` when a success arrived
/// whose body is unusable (unreadable or not JSON): the server answered, so
/// it billed, but nothing says how much — unpriced, never $0.
pub(crate) async fn post_json_billed(
    client: &reqwest::Client,
    url: &str,
    key: Option<&str>,
    body: &[u8],
    billed: &Mutex<Option<Evaluation>>,
    model: &str,
) -> Result<Value, DecisionError> {
    post_json(client, url, key, body).await.inspect_err(|e| {
        if matches!(e, DecisionError::BadResponse(_)) {
            add_billed(billed, &billed_only(model, DecisionUsage::default(), false));
        }
    })
}

/// POST a JSON `body` (bearer `key` when set) and read a JSON response,
/// mapping failures to [`DecisionError`]s the retry policy understands.
/// Shared by the HTTP backends. [`DecisionError::BadResponse`] means a 2xx
/// arrived whose body is unusable (and only that: other statuses are
/// classified by [`status_error`]), which [`post_json_billed`] relies on.
pub(crate) async fn post_json(
    client: &reqwest::Client,
    url: &str,
    key: Option<&str>,
    body: &[u8],
) -> Result<Value, DecisionError> {
    let mut req = client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body.to_vec());
    if let Some(key) = key {
        req = req.bearer_auth(key);
    }
    let response = req.send().await.map_err(|e| {
        // A request that could not even be built (a malformed URL) will
        // never succeed: not a transport failure to retry.
        if e.is_builder() {
            DecisionError::Invalid(format!("the request could not be built: {e}"))
        } else {
            DecisionError::transport_with_source(e.to_string(), e)
        }
    })?;
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let ok = (200..300).contains(&status);
    let text = match response.text().await {
        Ok(text) => text,
        // A success whose body cannot be read has been processed (and
        // billed): retrying would pay twice, so it is not a transport
        // error.
        Err(e) if ok => {
            return Err(DecisionError::BadResponse(format!(
                "could not read the response body: {e}"
            )))
        }
        Err(e) => return Err(DecisionError::transport_with_source(e.to_string(), e)),
    };
    if !ok {
        return Err(status_error(status, &headers, &text));
    }
    serde_json::from_str(&text).map_err(|e| {
        DecisionError::BadResponse(format!(
            "response is not JSON ({e}): {}",
            excerpt(&text, 200)
        ))
    })
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl DecisionBackend for SystemOneBackend {
    fn capabilities(&self) -> Capabilities {
        self.capabilities.clone()
    }

    async fn evaluate(&self, request: &Request) -> Result<Evaluation, DecisionError> {
        self.evaluate_billed(request, &Mutex::new(None)).await
    }
}

/// Run `send` until it succeeds, fails with a non-retryable error, or the
/// retries are spent: rate limits, overload and transport failures are
/// retried with `retry`'s backoff, a server `retry-after` winning (clamped
/// to the cap). Shared by the HTTP backends.
pub(crate) async fn with_retries<T, F, Fut>(
    retry: &RetryConfig,
    mut send: F,
) -> Result<T, DecisionError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, DecisionError>>,
{
    let mut attempt = 0usize;
    loop {
        match send().await {
            Err(e) if e.is_retryable() && attempt < retry.max_retries => {
                attempt += 1;
                let delay = e
                    .retry_after()
                    .map(|d| d.min(Duration::from_millis(retry.max_delay_ms)))
                    .unwrap_or_else(|| retry.delay_for_attempt(attempt));
                tracing::warn!(
                    "decision model error (attempt {}/{}), retrying in {:.1}s: {}",
                    attempt,
                    retry.max_retries,
                    delay.as_secs_f64(),
                    e
                );
                crate::rt::sleep(delay).await;
            }
            other => return other,
        }
    }
}

pub(crate) fn excerpt(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

pub(crate) fn status_error(
    status: u16,
    headers: &reqwest::header::HeaderMap,
    body: &str,
) -> DecisionError {
    let parsed = || serde_json::from_str::<Value>(body).unwrap_or_default();
    match status {
        // A daily allowance that ran out is a 429 no retry can fix before
        // the reset: report it, with its explanation, without retrying.
        429 if cloudflare_codes(&parsed()).contains(&CLOUDFLARE_DAILY_LIMIT) => {
            DecisionError::http(status, excerpt(body, MAX_ERROR_BODY))
        }
        429 | 529 => DecisionError::rate_limited_with_body(
            status,
            crate::provider::traits::parse_retry_after(headers).map(Duration::from_millis),
            excerpt(body, MAX_ERROR_BODY),
        ),
        422 => DecisionError::Invalid(format!(
            "server rejected the request (422): {}",
            excerpt(body, MAX_ERROR_BODY)
        )),
        _ => DecisionError::http(status, excerpt(body, MAX_ERROR_BODY)),
    }
}

/// A number; `None` when absent or `null`; NaN when present but not a
/// number — so validation rejects it rather than a default slipping through.
/// For a required field `None` is itself an error; for an optional or
/// derivable one (`confidence`, `score`) it means "compute it".
fn number(v: Option<&Value>) -> Option<f64> {
    v.filter(|v| !v.is_null())
        .map(|v| v.as_f64().unwrap_or(f64::NAN))
}

fn level_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Parse a SystemOne response `body` against the `request` that produced it,
/// for a backend that moves the request itself — such as a Cloudflare
/// Workers AI binding (`yoagent-workers`), which calls the model without
/// HTTP. Cloudflare's `{"result": ...}` envelope is unwrapped, and
/// `"success": false` is a [`DecisionError::BadResponse`].
///
/// Parses leniently, like [`SystemOneBackend`]: unknown fields are ignored and
/// a missing `confidence`, `choice`, `score` or `legend` is computed. It checks
/// shape only; [`DecisionModel`](super::DecisionModel) validates the values of
/// every backend's answers.
pub fn parse_systemone_response(
    body: Value,
    request: &Request,
) -> Result<Evaluation, DecisionError> {
    parse_evaluation(&unwrap_envelope(body)?, request)
}

/// Parse a SystemOne response against the request that produced it. Shape
/// only; values are validated centrally.
pub(crate) fn parse_evaluation(
    body: &Value,
    request: &Request,
) -> Result<Evaluation, DecisionError> {
    let answers = body
        .get("answers")
        .and_then(Value::as_object)
        .ok_or_else(|| DecisionError::BadResponse("response has no `answers` object".into()))?;
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .filter(|m| !m.is_empty())
        .unwrap_or(&request.model)
        .to_string();

    let mut per_answer_input = 0u64;
    let mut per_answer_reported = false;
    let mut eval = Evaluation::new(model, DecisionUsage::default());
    for (id, question) in &request.questions {
        let raw = answers
            .get(id)
            .ok_or_else(|| DecisionError::BadResponse(format!("answers.{id}: missing")))?;
        if let Some(t) = raw.get("type").and_then(Value::as_str) {
            if t != question.kind().as_str() {
                return Err(DecisionError::BadResponse(format!(
                    "answers.{id}: type {t:?} answers a {} question",
                    question.kind()
                )));
            }
        }
        if let Some(n) = raw.get("input_tokens").and_then(Value::as_u64) {
            per_answer_input += n;
            per_answer_reported = true;
        }
        let answer = parse_answer(id, raw, question)?;
        eval = eval.with_answer(id.clone(), answer);
    }

    let usage = body.get("usage").filter(|u| u.is_object());
    let tokens = |k: &str| usage.and_then(|u| u.get(k)).and_then(Value::as_u64);
    eval.usage = DecisionUsage::new(
        tokens("input_tokens").unwrap_or(per_answer_input),
        tokens("output_tokens").unwrap_or(0),
    );
    // No usage at all — neither a `usage` object nor per-answer counts —
    // means the evaluation cannot be priced (never a priced $0).
    eval.usage_reported = tokens("input_tokens").is_some() || per_answer_reported;
    Ok(eval)
}

fn parse_answer(id: &str, raw: &Value, question: &Question) -> Result<Answer, DecisionError> {
    let confidence = number(raw.get("confidence"));
    let missing =
        |field: &str| DecisionError::BadResponse(format!("answers.{id}.{field}: missing"));
    match question.kind() {
        QuestionKind::Noul => {
            let p = number(raw.get("noul")).ok_or_else(|| missing("noul"))?;
            let mut a = NoulAnswer::new(p);
            if let Some(c) = confidence {
                a = a.with_confidence(c);
            }
            Ok(a.into())
        }
        QuestionKind::Choice => {
            let options = question.options().unwrap_or_default();
            let probs = raw
                .get("probabilities")
                .and_then(Value::as_object)
                .ok_or_else(|| missing("probabilities"))?;
            if let Some(extra) = probs.keys().find(|k| !options.contains(&k.as_str())) {
                return Err(DecisionError::BadResponse(format!(
                    "answers.{id}.probabilities: {extra:?} is not one of the options"
                )));
            }
            // Option order, so an argmax tie resolves to the option the
            // caller listed first.
            let pairs: Vec<(String, f64)> = options
                .iter()
                .filter_map(|o| {
                    probs
                        .get(*o)
                        .map(|v| (o.to_string(), number(Some(v)).unwrap_or(f64::NAN)))
                })
                .collect();
            if pairs.is_empty() {
                return Err(missing("probabilities"));
            }
            let mut a = ChoiceAnswer::new(pairs);
            if let Some(choice) = raw.get("choice").and_then(Value::as_str) {
                a = a.with_choice(choice);
            }
            if let Some(c) = confidence {
                a = a.with_confidence(c);
            }
            Ok(a.into())
        }
        QuestionKind::Score => {
            let levels = question.levels().unwrap_or_default();
            let n = levels.len();
            let mut probs = vec![f64::NAN; n];
            match raw.get("probabilities") {
                Some(Value::Object(map)) => {
                    for (k, v) in map {
                        let i: usize = k.parse().ok().filter(|i| *i < n).ok_or_else(|| {
                            DecisionError::BadResponse(format!(
                                "answers.{id}.probabilities: {k:?} is not a level index below {n}"
                            ))
                        })?;
                        probs[i] = number(Some(v)).unwrap_or(f64::NAN);
                    }
                }
                Some(Value::Array(list)) if list.len() == n => {
                    for (i, v) in list.iter().enumerate() {
                        probs[i] = number(Some(v)).unwrap_or(f64::NAN);
                    }
                }
                _ => {
                    return Err(DecisionError::BadResponse(format!(
                        "answers.{id}.probabilities: missing or not one entry per level"
                    )))
                }
            }
            // A level the server left out is NaN, which validation rejects.
            let mut legend: Vec<String> = levels.iter().map(level_text).collect();
            if let Some(Value::Object(map)) = raw.get("legend") {
                for (k, v) in map {
                    if let Some(i) = k.parse::<usize>().ok().filter(|i| *i < n) {
                        legend[i] = level_text(v);
                    }
                }
            }
            let mut a = ScoreAnswer::new(legend, probs);
            if let Some(score) = number(raw.get("score")) {
                a = a.with_score(score);
            }
            if let Some(c) = confidence {
                a = a.with_confidence(c);
            }
            Ok(a.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typesafe_host_detection() {
        assert!(SystemOneBackend::new("https://api.typesafe.ai").is_typesafe_host());
        assert!(SystemOneBackend::new("https://API.typesafe.ai/v1/").is_typesafe_host());
        assert!(!SystemOneBackend::new("https://opencode.ai/zen").is_typesafe_host());
        assert!(!SystemOneBackend::new("http://127.0.0.1:8000").is_typesafe_host());
        assert!(!SystemOneBackend::new("https://api.typesafe.ai.evil.example").is_typesafe_host());
        assert!(!SystemOneBackend::opencode_zen().is_typesafe_host());
    }

    #[test]
    fn workers_ai_posts_to_the_model_url() {
        let b = SystemOneBackend::workers_ai(" acct ", "/@cf/cloudflare/clef");
        assert_eq!(
            b.endpoint_url(),
            "https://api.cloudflare.com/client/v4/accounts/acct/ai/run/@cf/cloudflare/clef"
        );
        assert!(b.is_cloudflare_host());
        assert!(!b.is_typesafe_host());
        // An exact URL is used as given: no `/v1/systemone` appended.
        let proxy = b.with_endpoint_url("https://proxy.example/ai/run/m");
        assert_eq!(proxy.endpoint_url(), "https://proxy.example/ai/run/m");
        assert!(!proxy.is_cloudflare_host());
        assert!(SystemOneBackend::new("https://API.Cloudflare.com/client/v4").is_cloudflare_host());
        assert!(
            !SystemOneBackend::new("https://api.cloudflare.com.evil.example").is_cloudflare_host()
        );
        // The key names both variables, never a value.
        let debug = format!("{:?}", SystemOneBackend::workers_ai("a", "m"));
        assert!(
            debug.contains("$CLOUDFLARE_API_TOKEN or $CLOUDFLARE_AUTH_TOKEN"),
            "{debug}"
        );
    }

    #[test]
    fn the_cloudflare_envelope_is_unwrapped() {
        let inner = serde_json::json!({"model": "clef", "answers": {}});
        let wrapped = serde_json::json!({
            "result": inner, "success": true, "errors": [], "messages": []
        });
        assert_eq!(unwrap_envelope(wrapped).unwrap(), inner);
        // A plain SystemOne response passes through untouched, even with a
        // field named `result`.
        let plain = serde_json::json!({"answers": {}, "result": {"x": 1}});
        assert_eq!(unwrap_envelope(plain.clone()).unwrap(), plain);
        // A schema error is the request's fault; any other failure is
        // Cloudflare's.
        let failed = serde_json::json!({
            "result": null, "success": false,
            "errors": [{"code": 5006, "message": "Error: oneOf at '/' not met"}]
        });
        match unwrap_envelope(failed) {
            Err(DecisionError::Invalid(m)) => assert!(m.contains("oneOf"), "{m}"),
            other => panic!("expected Invalid, got {other:?}"),
        }
        let failed = serde_json::json!({"success": false});
        match unwrap_envelope(failed) {
            Err(DecisionError::Backend { message, .. }) => {
                assert!(message.contains("(no errors given)"), "{message}")
            }
            other => panic!("expected Backend, got {other:?}"),
        }
        // A `result` that is not a SystemOne object says so.
        match unwrap_envelope(serde_json::json!({"result": null, "success": true})) {
            Err(DecisionError::BadResponse(m)) => assert!(m.contains("`result` is null"), "{m}"),
            other => panic!("expected BadResponse, got {other:?}"),
        }
        match unwrap_envelope(serde_json::json!({"result": "sorry", "success": true})) {
            Err(DecisionError::BadResponse(m)) => assert!(m.contains("sorry"), "{m}"),
            other => panic!("expected BadResponse, got {other:?}"),
        }
        // JSON handed back as text is read.
        let text = serde_json::json!({"result": inner.to_string(), "success": true});
        assert_eq!(unwrap_envelope(text).unwrap(), inner);
    }

    #[test]
    fn a_used_up_daily_allocation_is_not_retried_but_capacity_is() {
        let headers = reqwest::header::HeaderMap::new();
        let daily = r#"{"success":false,"errors":[{"code":3036,"message":"You have used up your daily free allocation of 10,000 neurons."}]}"#;
        let e = status_error(429, &headers, daily);
        assert!(
            matches!(e, DecisionError::Http { status: 429, .. }),
            "{e:?}"
        );
        assert!(!e.is_retryable());
        assert!(e.to_string().contains("daily free allocation"), "{e}");

        let busy = r#"{"success":false,"errors":[{"code":3040,"message":"Capacity temporarily exceeded, please try again."}]}"#;
        let e = status_error(429, &headers, busy);
        assert!(e.is_retryable());
        assert!(
            e.to_string().contains("Capacity temporarily exceeded"),
            "{e}"
        );
        // A plain 429 with no body still reads cleanly.
        assert_eq!(
            status_error(429, &headers, "").to_string(),
            "decision model rate limited or overloaded (HTTP 429)"
        );
    }
}
