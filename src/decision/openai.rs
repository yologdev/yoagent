//! [`OpenAiDecisionBackend`]: OpenAI's Decisions API (`POST /v1/decisions`).
//!
//! The API takes shared `input` and an ordered list of named questions —
//! `predicate`, `choice`, `score` — and answers each with probabilities. Only
//! `gpt-6-luna` serves it today (public beta). yoagent's question model maps
//! onto it one to one:
//!
//! | yoagent | Decisions API |
//! |---------|---------------|
//! | `state` (text) | `input`, as is |
//! | `state` (JSON) | `input`, serialized (pretty-printed) |
//! | question id | `name` |
//! | Noul | `predicate`; `probability` → [`NoulAnswer::p_true`] |
//! | Choice | `choice`; options → `choices[].value` (strings), criteria → `description` |
//! | Score | `score`; levels → `levels[].label`, answered by level index |
//! | `refusal` answer | [`DecisionError::Refused`] naming the question (not retried; ends a fallback chain) |
//!
//! Instructions, criteria and levels that are JSON rather than text are sent
//! as serialized JSON strings (the API takes strings). A Noul's yes/no
//! criteria are appended to its instructions. The API returns no confidence
//! for a predicate, so a Noul answer's confidence is the crate's computed
//! `|2p - 1|` (see [`distribution_confidence`](super::distribution_confidence)).
//! Images (the API's `input_image` parts) are not supported: a request's
//! state is text or JSON.
//!
//! A response that arrived but could not be used — a refusal, an unusable
//! answer — was still billed: its `usage` is read first and recorded in
//! [`SessionStats::decision`](crate::SessionStats::decision) with the
//! failure (a 2xx body that is not even JSON counts as unpriced spend).
//!
//! Tested against mock servers only: no live OpenAI key was available when
//! this backend was written.

use super::answer::{Answer, ChoiceAnswer, DecisionUsage, Evaluation, NoulAnswer, ScoreAnswer};
use super::backend::{Capabilities, DecisionBackend};
use super::error::DecisionError;
use super::question::{Question, QuestionKind, Request};
use super::systemone::{post_json_billed, with_retries};
use super::{add_billed, billed_only};
use crate::retry::RetryConfig;
use serde_json::{json, Map, Value};
use std::sync::Mutex;

/// OpenAI's API base, `/v1` included (the convention of OpenAI's SDKs).
pub(crate) const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
/// The host that bills at OpenAI's list price.
const OPENAI_HOST: &str = "api.openai.com";
pub(crate) const OPENAI_API_KEY_ENV: &str = "OPENAI_API_KEY";

#[derive(Clone)]
enum Key {
    Fixed(String),
    /// Read at call time; unset or empty is an error.
    Env(String),
}

/// An HTTP client for OpenAI's Decisions API.
///
/// Cheap to clone. The key is read from `OPENAI_API_KEY` on every request
/// (so rotating it needs no rebuild) unless set with
/// [`with_api_key`](Self::with_api_key); it never appears in `Debug` output
/// or errors. Most callers want
/// [`DecisionModel::gpt_6_luna`](super::DecisionModel::gpt_6_luna).
///
/// Capabilities: every question kind, batching, and the same option and
/// level limits as the SystemOne presets (255 options, 10 levels, 64k tokens
/// per request, 32k for the state plus the longest question) — OpenAI's
/// schema publishes no limits of its own, so these are yoagent's defaults,
/// not OpenAI's; override them with [`with_capabilities`](Self::with_capabilities).
#[derive(Clone)]
pub struct OpenAiDecisionBackend {
    client: reqwest::Client,
    base_url: String,
    key: Key,
    retry: RetryConfig,
    capabilities: Capabilities,
    safety_identifier: Option<String>,
}

impl std::fmt::Debug for OpenAiDecisionBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let key = match &self.key {
            Key::Fixed(_) => "[redacted]".to_string(),
            Key::Env(var) => format!("${var}"),
        };
        f.debug_struct("OpenAiDecisionBackend")
            .field("endpoint", &self.endpoint_url())
            .field("key", &key)
            .field("retry", &self.retry)
            .field("capabilities", &self.capabilities)
            .finish_non_exhaustive()
    }
}

impl Default for OpenAiDecisionBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenAiDecisionBackend {
    /// OpenAI's hosted API (`https://api.openai.com/v1/decisions`), key from
    /// `OPENAI_API_KEY` at call time.
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: OPENAI_BASE_URL.into(),
            key: Key::Env(OPENAI_API_KEY_ENV.into()),
            retry: RetryConfig::default(),
            capabilities: Capabilities::systemone(),
            safety_identifier: None,
        }
    }

    /// Send this key (replaces the environment variable).
    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.key = Key::Fixed(key.into());
        self
    }

    /// Read the key from this environment variable at call time.
    pub fn with_api_key_env(mut self, var: impl Into<String>) -> Self {
        self.key = Key::Env(var.into());
        self
    }

    /// Use this API base, `/v1` included like OpenAI's SDKs (default
    /// `https://api.openai.com/v1`); requests go to `{base_url}/decisions`.
    /// A URL already ending in `/decisions` is used as is. Any host other
    /// than `api.openai.com` is unpriced.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Retry policy for 429 / 529 / transport failures (the same rules as
    /// [`SystemOneBackend::with_retry`](super::SystemOneBackend::with_retry)).
    /// The overall time limit is the model's
    /// [`with_timeout`](super::DecisionModel::with_timeout).
    pub fn with_retry(mut self, retry: RetryConfig) -> Self {
        self.retry = retry;
        self
    }

    /// Report these capabilities instead of the defaults.
    pub fn with_capabilities(mut self, capabilities: Capabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Send this `safety_identifier`: an opaque id for your end user, which
    /// OpenAI uses for abuse monitoring. Never the user's identity itself.
    pub fn with_safety_identifier(mut self, id: impl Into<String>) -> Self {
        self.safety_identifier = Some(id.into());
        self
    }

    /// The URL requests are sent to.
    pub fn endpoint_url(&self) -> String {
        let base = self.base_url.trim().trim_end_matches('/');
        if base.ends_with("/decisions") {
            base.to_string()
        } else {
            format!("{base}/decisions")
        }
    }

    /// Whether requests go to OpenAI's own host, so its list price applies.
    pub(crate) fn is_openai_host(&self) -> bool {
        let url = self.endpoint_url();
        let rest = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"))
            .unwrap_or(&url);
        rest.split(['/', ':', '?', '#'])
            .next()
            .unwrap_or("")
            .eq_ignore_ascii_case(OPENAI_HOST)
    }

    /// The key (trimmed) and the variable it came from, if any.
    fn key(&self) -> Result<(String, Option<&str>), DecisionError> {
        match &self.key {
            Key::Fixed(k) if k.trim().is_empty() => Err(DecisionError::MissingApiKey(
                "a non-empty key (the one given to with_api_key is empty)".into(),
            )),
            Key::Fixed(k) => Ok((k.trim().to_string(), None)),
            Key::Env(var) => match std::env::var(var) {
                Ok(v) if !v.trim().is_empty() => Ok((v.trim().to_string(), Some(var.as_str()))),
                Ok(_) => Err(DecisionError::MissingApiKey(format!(
                    "{var} (set, but empty)"
                ))),
                Err(_) => Err(DecisionError::MissingApiKey(var.clone())),
            },
        }
    }

    /// The request body: `model`, `input`, `questions` in request order,
    /// and `safety_identifier` when set.
    pub(crate) fn body(&self, request: &Request) -> Value {
        let mut body = Map::new();
        body.insert("model".into(), json!(request.model));
        body.insert("input".into(), json!(as_text(&request.state)));
        let questions: Vec<Value> = request
            .questions
            .iter()
            .map(|(id, q)| question_json(id, q))
            .collect();
        body.insert("questions".into(), Value::Array(questions));
        if let Some(id) = &self.safety_identifier {
            body.insert("safety_identifier".into(), json!(id));
        }
        Value::Object(body)
    }

    async fn send_once(
        &self,
        request: &Request,
        body: &[u8],
        billed: &Mutex<Option<Evaluation>>,
    ) -> Result<Evaluation, DecisionError> {
        let (key, var) = self.key()?;
        let value = post_json_billed(
            &self.client,
            &self.endpoint_url(),
            Some(&key),
            body,
            billed,
            &request.model,
        )
        .await
        .map_err(|e| match e {
            // Say which variable the rejected key came from.
            DecisionError::Http { status, body } if status == 401 || status == 403 => {
                let from = match var {
                    Some(var) => format!("key from ${var}"),
                    None => format!("key set with with_api_key, not ${OPENAI_API_KEY_ENV}"),
                };
                DecisionError::http(status, format!("{body} ({from})"))
            }
            e => e,
        })?;
        parse_response(&value, request).inspect_err(|_| {
            // Answered, so billed: keep what the response says it cost.
            add_billed(billed, &billed_by(&value, request));
        })
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
        let body = serde_json::to_vec(&self.body(request))
            .map_err(|e| DecisionError::Invalid(format!("request does not serialize: {e}")))?;
        with_retries(&self.retry, || self.send_once(request, &body, billed)).await
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl DecisionBackend for OpenAiDecisionBackend {
    fn capabilities(&self) -> Capabilities {
        self.capabilities.clone()
    }

    async fn evaluate(&self, request: &Request) -> Result<Evaluation, DecisionError> {
        self.evaluate_billed(request, &Mutex::new(None)).await
    }
}

/// A string as is; anything else as pretty-printed JSON (the API takes text).
fn as_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    }
}

fn question_json(id: &str, q: &Question) -> Value {
    let mut instructions = as_text(q.instructions());
    match q.kind() {
        QuestionKind::Noul => {
            if let Some((t, f)) = q.noul_criteria() {
                instructions.push_str(&format!(
                    "\n\nTrue when: {}\nFalse when: {}",
                    as_text(t),
                    as_text(f)
                ));
            }
            json!({"type": "predicate", "name": id, "instructions": instructions})
        }
        QuestionKind::Choice => {
            let choices: Vec<Value> = q
                .choice_criteria()
                .unwrap_or_default()
                .into_iter()
                .map(|(value, desc)| match desc {
                    Some(d) => json!({"value": value, "description": as_text(d)}),
                    None => json!({"value": value}),
                })
                .collect();
            json!({"type": "choice", "name": id, "instructions": instructions, "choices": choices})
        }
        QuestionKind::Score => {
            let levels: Vec<Value> = q
                .levels()
                .unwrap_or_default()
                .iter()
                .map(|l| json!({"label": as_text(l)}))
                .collect();
            json!({"type": "score", "name": id, "instructions": instructions, "levels": levels})
        }
    }
}

/// A number; `None` when absent or `null`; NaN when present but not a
/// number, so validation rejects it.
fn number(v: Option<&Value>) -> Option<f64> {
    v.filter(|v| !v.is_null())
        .map(|v| v.as_f64().unwrap_or(f64::NAN))
}

/// Parse a Decisions response against the request that produced it. Shape
/// only; values are validated centrally (`check_complete`).
///
/// Answers are matched to questions by `name`. An answer without a name is
/// matched by position — OpenAI documents that answers come back in
/// question order — but only when the response has exactly one answer per
/// question.
pub(crate) fn parse_response(body: &Value, request: &Request) -> Result<Evaluation, DecisionError> {
    let answers = body
        .get("answers")
        .and_then(Value::as_array)
        .ok_or_else(|| DecisionError::BadResponse("response has no `answers` array".into()))?;
    let positional = answers.len() == request.questions.len();
    let name_of = |a: &Value| a.get("name").and_then(Value::as_str).map(str::to_string);

    let mut eval = billed_by(body, request);
    for (i, (id, question)) in request.questions.iter().enumerate() {
        let raw = answers
            .iter()
            .find(|a| name_of(a).as_deref() == Some(id.as_str()))
            .or_else(|| {
                answers
                    .get(i)
                    .filter(|a| positional && name_of(a).is_none())
            })
            .ok_or_else(|| DecisionError::BadResponse(format!("answers.{id}: missing")))?;
        let answer = parse_answer(id, raw, question)?;
        eval = eval.with_answer(id.clone(), answer);
    }
    Ok(eval)
}

/// What a response body says it was billed for, answers aside: its model
/// and `usage`. Read before the answers, so a refusal or an unusable answer
/// still records its spend. No usage means the evaluation cannot be priced
/// (never a priced $0).
fn billed_by(body: &Value, request: &Request) -> Evaluation {
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .filter(|m| !m.is_empty())
        .unwrap_or(&request.model);
    let usage = body.get("usage").filter(|u| u.is_object());
    let tokens = |k: &str| usage.and_then(|u| u.get(k)).and_then(Value::as_u64);
    billed_only(
        model,
        DecisionUsage::new(
            tokens("input_tokens").unwrap_or(0),
            tokens("output_tokens").unwrap_or(0),
        ),
        tokens("input_tokens").is_some(),
    )
}

fn parse_answer(id: &str, raw: &Value, question: &Question) -> Result<Answer, DecisionError> {
    let missing =
        |field: &str| DecisionError::BadResponse(format!("answers.{id}.{field}: missing"));
    let kind = raw.get("type").and_then(Value::as_str).unwrap_or("");
    if kind == "refusal" {
        // The gate and the guard fail closed on any error; a refusal must
        // never read as an answer.
        return Err(DecisionError::refused(format!(
            "OpenAI declined to answer question `{id}` (refusal)"
        )));
    }
    let expected = match question.kind() {
        QuestionKind::Noul => "predicate",
        QuestionKind::Choice => "choice",
        QuestionKind::Score => "score",
    };
    if kind != expected {
        return Err(DecisionError::BadResponse(format!(
            "answers.{id}: type {kind:?} answers a {} question (expected {expected:?})",
            question.kind()
        )));
    }
    let confidence = number(raw.get("confidence"));
    let entries = || {
        raw.get("probabilities")
            .and_then(Value::as_array)
            .ok_or_else(|| missing("probabilities"))
    };
    match question.kind() {
        QuestionKind::Noul => {
            let p = number(raw.get("probability")).ok_or_else(|| missing("probability"))?;
            // No confidence is returned for a predicate: computed.
            Ok(NoulAnswer::new(p).into())
        }
        QuestionKind::Choice => {
            let options = question.options().unwrap_or_default();
            let mut by_option: Vec<Option<f64>> = vec![None; options.len()];
            for entry in entries()? {
                // We send string values only; a boolean (or anything else)
                // is not one of our options.
                let value = entry.get("value");
                let Some(at) = value
                    .and_then(Value::as_str)
                    .and_then(|v| options.iter().position(|o| *o == v))
                else {
                    return Err(DecisionError::BadResponse(format!(
                        "answers.{id}.probabilities: {} is not one of the options",
                        value.map_or("a missing value".to_string(), Value::to_string)
                    )));
                };
                if by_option[at].is_some() {
                    return Err(DecisionError::BadResponse(format!(
                        "answers.{id}.probabilities: two entries for {:?}",
                        options[at]
                    )));
                }
                by_option[at] = Some(number(entry.get("probability")).unwrap_or(f64::NAN));
            }
            // Option order, so an argmax tie resolves to the option listed
            // first; a missing option is left out and validation names it.
            let pairs: Vec<(String, f64)> = options
                .iter()
                .zip(&by_option)
                .filter_map(|(o, p)| p.map(|p| (o.to_string(), p)))
                .collect();
            if pairs.is_empty() {
                return Err(missing("probabilities"));
            }
            let mut a = ChoiceAnswer::new(pairs);
            match raw.get("choice") {
                Some(Value::String(choice)) => a = a.with_choice(choice.as_str()),
                Some(Value::Null) | None => {}
                Some(other) => {
                    return Err(DecisionError::BadResponse(format!(
                        "answers.{id}.choice: {other} is not one of the options"
                    )))
                }
            }
            if let Some(c) = confidence {
                a = a.with_confidence(c);
            }
            Ok(a.into())
        }
        QuestionKind::Score => {
            let levels = question.levels().unwrap_or_default();
            let n = levels.len();
            // A level the server left out stays NaN, which validation rejects.
            let mut probs = vec![f64::NAN; n];
            for entry in entries()? {
                let i =
                    entry
                        .get("value")
                        .and_then(Value::as_u64)
                        .and_then(|i| usize::try_from(i).ok())
                        .filter(|i| *i < n)
                        .ok_or_else(|| {
                            DecisionError::BadResponse(format!(
                            "answers.{id}.probabilities: value {} is not a level index below {n}",
                            entry.get("value").map_or("(missing)".into(), Value::to_string)
                        ))
                        })?;
                probs[i] = number(entry.get("probability")).unwrap_or(f64::NAN);
            }
            let legend: Vec<String> = levels.iter().map(as_text).collect();
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
    fn endpoint_and_host() {
        let b = OpenAiDecisionBackend::new();
        assert_eq!(b.endpoint_url(), "https://api.openai.com/v1/decisions");
        assert!(b.is_openai_host());
        let b = b.with_base_url("https://API.openai.com/v1/");
        assert_eq!(b.endpoint_url(), "https://API.openai.com/v1/decisions");
        assert!(b.is_openai_host());
        let b = b.with_base_url("https://proxy.example/v1/decisions");
        assert_eq!(b.endpoint_url(), "https://proxy.example/v1/decisions");
        assert!(!b.is_openai_host());
        assert!(!OpenAiDecisionBackend::new()
            .with_base_url("https://api.openai.com.evil.example/v1")
            .is_openai_host());
        let debug = format!(
            "{:?}",
            OpenAiDecisionBackend::new().with_api_key("sk-secret")
        );
        assert!(!debug.contains("sk-secret"), "{debug}");
        assert!(format!("{:?}", OpenAiDecisionBackend::new()).contains("$OPENAI_API_KEY"));
    }

    #[test]
    fn json_state_and_criteria_become_text() {
        let request = Request::new("gpt-6-luna", json!({"a": 1}))
            .question(
                "d",
                Question::noul_with_criteria("Destructive?", "Deletes data", "Reads only"),
            )
            .question(
                "j",
                Question::noul(json!({"question": "Is it?", "data": [1]})),
            );
        let body = OpenAiDecisionBackend::new().body(&request);
        assert_eq!(body["input"], "{\n  \"a\": 1\n}");
        assert_eq!(
            body["questions"][0]["instructions"],
            "Destructive?\n\nTrue when: Deletes data\nFalse when: Reads only"
        );
        let j: Value =
            serde_json::from_str(body["questions"][1]["instructions"].as_str().unwrap()).unwrap();
        assert_eq!(j, json!({"question": "Is it?", "data": [1]}));
        assert!(body.get("safety_identifier").is_none());
    }
}
