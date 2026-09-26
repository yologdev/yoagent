//! [`SystemOneBackend`]: the SystemOne HTTP API (`POST /v1/systemone`).
//!
//! Served by TypeSafe (`https://api.typesafe.ai`), by OpenCode Zen
//! (`https://opencode.ai/zen`), and by self-hosted TypeSafe-style servers
//! such as JevK5. Responses are parsed leniently: unknown fields are ignored,
//! and a missing `confidence`, `choice`, `score` or `legend` is computed from
//! what is present.

use super::answer::{Answer, ChoiceAnswer, DecisionUsage, Evaluation, NoulAnswer, ScoreAnswer};
use super::backend::{Capabilities, DecisionBackend};
use super::error::DecisionError;
use super::question::{Question, QuestionKind, Request};
use crate::retry::RetryConfig;
use serde_json::Value;
use std::time::Duration;

/// TypeSafe's hosted API.
pub const TYPESAFE_BASE_URL: &str = "https://api.typesafe.ai";
/// OpenCode Zen's SystemOne endpoint base.
pub const OPENCODE_ZEN_BASE_URL: &str = "https://opencode.ai/zen";
/// Environment variable read for the TypeSafe API key.
pub const TYPESAFE_API_KEY_ENV: &str = "TYPESAFE_API_KEY";
/// Environment variable that overrides [`TYPESAFE_BASE_URL`].
pub const TYPESAFE_BASE_URL_ENV: &str = "TYPESAFE_BASE_URL";
/// Environment variable read for the OpenCode API key.
pub const OPENCODE_API_KEY_ENV: &str = "OPENCODE_API_KEY";

/// Longest error-body excerpt kept in a [`DecisionError`].
const MAX_ERROR_BODY: usize = 2_000;

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
/// Cheap to clone (the HTTP client is shared). Keys named by environment
/// variable are read on every request, so rotating a key needs no rebuild;
/// the key never appears in `Debug` output or errors.
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
    /// local, no token limits (the server enforces its own).
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            endpoint: Endpoint::Fixed(base_url.into()),
            key: Key::None,
            retry: RetryConfig::default(),
            capabilities: Capabilities::new(QuestionKind::all()).with_local(true),
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

    /// Use this HTTP client (proxies, custom TLS, ...).
    pub fn with_http_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
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

    /// `GET /v1/models`: the names this key may send in `model`.
    pub async fn list_models(&self) -> Result<Vec<ModelInfo>, DecisionError> {
        let mut req = self.client.get(format!("{}/v1/models", self.base()));
        if let Some(key) = self.key()? {
            req = req.bearer_auth(key);
        }
        let response = req
            .send()
            .await
            .map_err(|e| DecisionError::Transport(e.to_string()))?;
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let text = response
            .text()
            .await
            .map_err(|e| DecisionError::Transport(e.to_string()))?;
        if !(200..300).contains(&status) {
            return Err(status_error(status, &headers, &text));
        }
        let body: Value = serde_json::from_str(&text)
            .map_err(|e| DecisionError::BadResponse(format!("models list is not JSON: {e}")))?;
        let models = body
            .get("models")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                DecisionError::BadResponse("models list has no `models` array".into())
            })?;
        Ok(models
            .iter()
            .filter_map(|m| {
                let name = m.get("name")?.as_str()?.to_string();
                let text = |k: &str| m.get(k).and_then(Value::as_str).map(str::to_string);
                Some(ModelInfo {
                    name,
                    description: text("description"),
                    release_date: text("release_date"),
                })
            })
            .collect())
    }

    async fn send_once(&self, request: &Request, body: &[u8]) -> Result<Evaluation, DecisionError> {
        let mut req = self
            .client
            .post(self.endpoint_url())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_vec());
        if let Some(key) = self.key()? {
            req = req.bearer_auth(key);
        }
        let response = req
            .send()
            .await
            .map_err(|e| DecisionError::Transport(e.to_string()))?;
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let text = response
            .text()
            .await
            .map_err(|e| DecisionError::Transport(e.to_string()))?;
        if !(200..300).contains(&status) {
            return Err(status_error(status, &headers, &text));
        }
        let value: Value = serde_json::from_str(&text).map_err(|e| {
            DecisionError::BadResponse(format!(
                "response is not JSON ({e}): {}",
                excerpt(&text, 200)
            ))
        })?;
        parse_evaluation(&value, request)
    }
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
        let mut attempt = 0usize;
        loop {
            match self.send_once(request, &body).await {
                Err(e) if e.is_retryable() && attempt < self.retry.max_retries => {
                    attempt += 1;
                    let delay = e
                        .retry_after()
                        .map(|d| d.min(Duration::from_millis(self.retry.max_delay_ms)))
                        .unwrap_or_else(|| self.retry.delay_for_attempt(attempt));
                    tracing::warn!(
                        "decision model error (attempt {}/{}), retrying in {:.1}s: {}",
                        attempt,
                        self.retry.max_retries,
                        delay.as_secs_f64(),
                        e
                    );
                    tokio::time::sleep(delay).await;
                }
                other => return other,
            }
        }
    }
}

/// One entry of `GET /v1/models`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ModelInfo {
    /// The id or alias to send as `model`.
    pub name: String,
    pub description: Option<String>,
    pub release_date: Option<String>,
}

fn excerpt(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

fn status_error(status: u16, headers: &reqwest::header::HeaderMap, body: &str) -> DecisionError {
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

fn number(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64).filter(|x| x.is_finite())
}

fn probability(v: Option<&Value>, field: &str) -> Result<f64, DecisionError> {
    let p = number(v)
        .ok_or_else(|| DecisionError::BadResponse(format!("{field}: missing or not a number")))?;
    // Tolerate float noise at the edges; reject anything else.
    if !(-1e-6..=1.0 + 1e-6).contains(&p) {
        return Err(DecisionError::BadResponse(format!(
            "{field}: {p} is not a probability"
        )));
    }
    Ok(p.clamp(0.0, 1.0))
}

fn level_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Parse a SystemOne response against the request that produced it.
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
        per_answer_input += raw.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
        let answer = parse_answer(id, raw, question)?;
        eval = eval.with_answer(id.clone(), answer);
    }

    let usage = body.get("usage");
    let tokens = |k: &str| usage.and_then(|u| u.get(k)).and_then(Value::as_u64);
    eval.usage = DecisionUsage::new(
        tokens("input_tokens").unwrap_or(per_answer_input),
        tokens("output_tokens").unwrap_or(0),
    );
    Ok(eval)
}

fn parse_answer(id: &str, raw: &Value, question: &Question) -> Result<Answer, DecisionError> {
    let confidence = number(raw.get("confidence")).map(|c| c.clamp(0.0, 1.0));
    match question.kind() {
        QuestionKind::Noul => {
            let p = probability(raw.get("noul"), &format!("answers.{id}.noul"))?;
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
                .ok_or_else(|| {
                    DecisionError::BadResponse(format!("answers.{id}.probabilities: missing"))
                })?;
            let mut pairs = Vec::with_capacity(probs.len());
            // Request order first, so an argmax tie resolves to the option the
            // caller listed first rather than alphabetically.
            for option in &options {
                if let Some(v) = probs.get(*option) {
                    let p = probability(Some(v), &format!("answers.{id}.probabilities.{option}"))?;
                    pairs.push((option.to_string(), p));
                }
            }
            if let Some(extra) = probs.keys().find(|k| !options.contains(&k.as_str())) {
                return Err(DecisionError::BadResponse(format!(
                    "answers.{id}.probabilities: {extra:?} is not one of the options"
                )));
            }
            if pairs.is_empty() {
                return Err(DecisionError::BadResponse(format!(
                    "answers.{id}.probabilities: empty"
                )));
            }
            let mut a = ChoiceAnswer::new(pairs);
            if let Some(choice) = raw.get("choice").and_then(Value::as_str) {
                if !options.contains(&choice) {
                    return Err(DecisionError::BadResponse(format!(
                        "answers.{id}.choice: {choice:?} is not one of the options"
                    )));
                }
                a.choice = choice.to_string();
            }
            if let Some(c) = confidence {
                a = a.with_confidence(c);
            }
            Ok(a.into())
        }
        QuestionKind::Score => {
            let levels = question.levels().unwrap_or_default();
            let n = levels.len();
            let mut probs = vec![0.0; n];
            match raw.get("probabilities") {
                Some(Value::Object(map)) => {
                    for (k, v) in map {
                        let i: usize = k.parse().ok().filter(|i| *i < n).ok_or_else(|| {
                            DecisionError::BadResponse(format!(
                                "answers.{id}.probabilities: {k:?} is not a level index below {n}"
                            ))
                        })?;
                        probs[i] =
                            probability(Some(v), &format!("answers.{id}.probabilities.{k}"))?;
                    }
                }
                Some(Value::Array(list)) if list.len() == n => {
                    for (i, v) in list.iter().enumerate() {
                        probs[i] =
                            probability(Some(v), &format!("answers.{id}.probabilities[{i}]"))?;
                    }
                }
                _ => {
                    return Err(DecisionError::BadResponse(format!(
                        "answers.{id}.probabilities: missing or not one entry per level"
                    )))
                }
            }
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
