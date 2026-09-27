//! [`SystemOneBackend`]: the SystemOne HTTP API (`POST /v1/systemone`).
//!
//! Served by TypeSafe (`https://api.typesafe.ai`), by OpenCode Zen
//! (`https://opencode.ai/zen`), and by self-hosted TypeSafe-style servers
//! such as JevK5. Responses are parsed leniently: unknown fields are ignored,
//! and a missing `confidence`, `choice`, `score` or `legend` is computed from
//! what is present. What is present is then validated like every backend's
//! answers (see [`DecisionBackend`]).

use super::answer::{Answer, ChoiceAnswer, DecisionUsage, Evaluation, NoulAnswer, ScoreAnswer};
use super::backend::{Capabilities, DecisionBackend};
use super::error::DecisionError;
use super::question::{Question, QuestionKind, Request};
use crate::retry::RetryConfig;
use serde_json::Value;
use std::time::Duration;

pub(crate) const TYPESAFE_BASE_URL: &str = "https://api.typesafe.ai";
pub(crate) const TYPESAFE_HOST: &str = "api.typesafe.ai";
pub(crate) const OPENCODE_ZEN_BASE_URL: &str = "https://opencode.ai/zen";
pub(crate) const TYPESAFE_API_KEY_ENV: &str = "TYPESAFE_API_KEY";
pub(crate) const TYPESAFE_BASE_URL_ENV: &str = "TYPESAFE_BASE_URL";
pub(crate) const OPENCODE_API_KEY_ENV: &str = "OPENCODE_API_KEY";

/// Longest error-body excerpt kept in a [`DecisionError`].
pub(crate) const MAX_ERROR_BODY: usize = 2_000;

#[derive(Clone)]
enum Endpoint {
    Fixed(String),
    /// Read the variable at call time, falling back to the default.
    EnvOr {
        var: String,
        default: String,
    },
}

#[derive(Clone)]
enum Key {
    None,
    Fixed(String),
    /// Read at call time; unset or empty is an error.
    Env(String),
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
            Key::Env(var) => format!("${var}"),
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
            key: Key::Env(TYPESAFE_API_KEY_ENV.into()),
            capabilities: Capabilities::systemone(),
            ..Self::new(TYPESAFE_BASE_URL)
        }
    }

    /// OpenCode Zen's SystemOne endpoint, key from `OPENCODE_API_KEY` (read
    /// at call time).
    pub fn opencode_zen() -> Self {
        Self {
            key: Key::Env(OPENCODE_API_KEY_ENV.into()),
            capabilities: Capabilities::systemone(),
            ..Self::new(OPENCODE_ZEN_BASE_URL)
        }
    }

    /// Send this key (replaces any environment variable).
    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.key = Key::Fixed(key.into());
        self
    }

    /// Read the key from this environment variable at call time.
    pub fn with_api_key_env(mut self, var: impl Into<String>) -> Self {
        self.key = Key::Env(var.into());
        self
    }

    /// Use this base URL (replaces any environment variable).
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.endpoint = Endpoint::Fixed(base_url.into());
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
            Endpoint::Fixed(url) => url.clone(),
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
        format!("{}/v1/systemone", self.base())
    }

    /// Whether requests currently go to TypeSafe's own host (so TypeSafe's
    /// list prices apply).
    pub(crate) fn is_typesafe_host(&self) -> bool {
        let base = self.base();
        let rest = base
            .strip_prefix("https://")
            .or_else(|| base.strip_prefix("http://"))
            .unwrap_or(&base);
        let host = rest.split(['/', ':', '?', '#']).next().unwrap_or("");
        host.eq_ignore_ascii_case(TYPESAFE_HOST)
    }

    fn key(&self) -> Result<Option<String>, DecisionError> {
        match &self.key {
            Key::None => Ok(None),
            Key::Fixed(k) => Ok(Some(k.clone())),
            Key::Env(var) => match std::env::var(var) {
                Ok(v) if !v.trim().is_empty() => Ok(Some(v)),
                _ => Err(DecisionError::MissingApiKey(var.clone())),
            },
        }
    }

    async fn send_once(&self, request: &Request, body: &[u8]) -> Result<Evaluation, DecisionError> {
        let key = self.key()?;
        let value = post_json(&self.client, &self.endpoint_url(), key.as_deref(), body).await?;
        parse_evaluation(&value, request)
    }
}

/// POST a JSON `body` (bearer `key` when set) and read a JSON response,
/// mapping failures to [`DecisionError`]s the retry policy understands.
/// Shared by the HTTP backends.
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
    let response = req
        .send()
        .await
        .map_err(|e| DecisionError::transport_with_source(e.to_string(), e))?;
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

#[async_trait::async_trait]
impl DecisionBackend for SystemOneBackend {
    fn capabilities(&self) -> Capabilities {
        self.capabilities.clone()
    }

    async fn evaluate(&self, request: &Request) -> Result<Evaluation, DecisionError> {
        // Fail on a missing key before serializing anything.
        self.key()?;
        let body = serde_json::to_vec(request)
            .map_err(|e| DecisionError::Invalid(format!("request does not serialize: {e}")))?;
        with_retries(&self.retry, || self.send_once(request, &body)).await
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
                tokio::time::sleep(delay).await;
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
    match status {
        429 | 529 => DecisionError::rate_limited(
            status,
            crate::provider::traits::parse_retry_after(headers).map(Duration::from_millis),
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
}
