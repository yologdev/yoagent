//! [`LogprobBackend`]: decisions from any OpenAI-compatible
//! `/chat/completions` server that returns logprobs. See the type's docs for
//! how answers are read and why their calibration is approximate.

use super::answer::{Answer, ChoiceAnswer, DecisionUsage, Evaluation, NoulAnswer, ScoreAnswer};
use super::backend::{Capabilities, DecisionBackend};
use super::error::DecisionError;
use super::question::{Question, QuestionKind, Request};
use super::systemone::{post_json, with_retries};
use crate::retry::RetryConfig;
use serde_json::{json, Map, Value};

/// Most `top_logprobs` OpenAI accepts; the default K and Choice limit.
const DEFAULT_TOP_LOGPROBS: usize = 20;
/// Letters available as Choice labels.
const MAX_LETTER_LABELS: usize = 26;
/// Questions of one request evaluated at once (one HTTP request each).
const MAX_CONCURRENT_QUESTIONS: usize = 8;
/// Default minimum share of the first-token probability the labels must
/// cover.
const DEFAULT_MIN_LABEL_MASS: f64 = 0.5;
/// The smallest probability an absent label is given, so absence alone
/// never yields exactly 0 or 1.
const ABSENT_FLOOR: f64 = 1e-9;
/// The question kinds this backend answers — listed, never `all()`, so a
/// future kind is `Unsupported` rather than answered as something else.
const KINDS: &[QuestionKind] = &[
    QuestionKind::Noul,
    QuestionKind::Choice,
    QuestionKind::Score,
];

/// A decision backend over an OpenAI-compatible `/chat/completions` endpoint
/// that returns logprobs — llama.cpp's `llama-server`, vLLM, SGLang,
/// LM Studio, or a hosted API.
///
/// Usually built for you by
/// [`DecisionModel::logprobs`](super::DecisionModel::logprobs); build one
/// directly to change its settings and wrap it with
/// [`DecisionModel::from_logprob_backend`](super::DecisionModel::from_logprob_backend),
/// which keeps the same conveniences (`with_api_key`, `with_retry`, $0 on a
/// loopback host). Wrapping it with
/// [`from_backend`](super::DecisionModel::from_backend) works too, but loses
/// them: that model is unpriced and owns its key and retries.
///
/// - **Question types:** Noul, Choice and Score. Choice up to 20 options by
///   default (OpenAI caps `top_logprobs` at 20; raise it to 26 with
///   [`with_max_choice_options`](Self::with_max_choice_options) for servers
///   that allow more), Score up to 10 levels.
/// - **One HTTP request per question.** It reports `batching: false` with 8
///   concurrent requests, so a [`DecisionModel`](super::DecisionModel) sends
///   a request's questions concurrently and records the usage of those that
///   completed even when another fails or the call times out.
/// - **Local** ([`Capabilities::local`]) only when the base URL's host is
///   loopback (`localhost`, `127.0.0.0/8`, `::1`).
/// - **Key:** none unless [`with_api_key`](Self::with_api_key); no
///   environment variable is ever read.
/// - Rate limits (429/529) and transport failures are retried like
///   [`SystemOneBackend`](super::SystemOneBackend)'s, honouring
///   `retry-after`; 422 and a malformed URL are [`DecisionError::Invalid`];
///   any other non-success status is [`DecisionError::Http`].
///
/// **Thinking must be off.** The answer must be the very first token. A
/// reasoning model that starts with `<think>` (Qwen3 does by default), or
/// one that answers in words, puts little probability on the labels — and
/// is rejected (below) rather than read as an answer. Use a non-thinking
/// model, start llama-server with `--reasoning off`, or send
/// `chat_template_kwargs: {"enable_thinking": false}` with
/// [`with_thinking_disabled`](Self::with_thinking_disabled) (llama.cpp,
/// vLLM and SGLang accept it; OpenAI's API rejects unknown fields, so it is
/// not sent by default).
///
/// **How answers are read.** Each question is one completion of one token
/// at temperature 0 (`max_tokens: 1`, `logprobs: true`,
/// `top_logprobs: max(configured K, the question's label count)`). The
/// prompt presents the state, the question and a label per answer — `A` =
/// yes / `B` = no for a Noul, `A`, `B`, ... for a Choice's options in order,
/// `0`–`9` for a Score's levels. Then, from the first position's
/// `top_logprobs`:
///
/// 1. every token is trimmed and upper-cased (`" A"` and `"a"` both count
///    as `A`), and the probabilities of tokens mapping to one label are
///    summed;
/// 2. no label at all, or labels covering less than the minimum share of
///    the probability (default 0.5,
///    [`with_min_label_mass`](Self::with_min_label_mass)), is
///    [`DecisionError::BadResponse`] — the model was not answering with a
///    label;
/// 3. a label outside the top K is given `min(smallest reported
///    probability, 1 − total reported probability)` — an upper bound on its
///    real probability — so absence alone never yields exactly 0 or 1;
/// 4. the temperature is applied to the log-probabilities and a softmax
///    gives the distribution; an absent label keeps a floor of 1e-9 after
///    scaling, so even a very small temperature cannot make it 0.
///
/// **Calibration is approximate.** A general LLM's next-token probability
/// for a label is not a calibrated probability the way a trained decision
/// model's is; it is often overconfident. Measure it with
/// [`calibrate`](super::calibrate()) and correct it with
/// [`with_temperature`](Self::with_temperature).
#[derive(Clone)]
pub struct LogprobBackend {
    client: reqwest::Client,
    base_url: String,
    key: Option<String>,
    retry: RetryConfig,
    temperature: f64,
    top_logprobs: usize,
    min_label_mass: f64,
    extra_body: Map<String, Value>,
    capabilities: Capabilities,
}

impl std::fmt::Debug for LogprobBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogprobBackend")
            .field("endpoint", &self.endpoint_url())
            .field("key", &self.key.as_ref().map(|_| "[redacted]"))
            .field("retry", &self.retry)
            .field("temperature", &self.temperature)
            .field("top_logprobs", &self.top_logprobs)
            .field("min_label_mass", &self.min_label_mass)
            .field("extra_body", &self.extra_body)
            .field("capabilities", &self.capabilities)
            .finish_non_exhaustive()
    }
}

impl LogprobBackend {
    /// A backend for the server at `base_url`, sending no key. `base_url`
    /// may be the host (`http://localhost:8080`, which gets `/v1`), a base
    /// ending in `/v1` or any other path (`https://api.groq.com/openai/v1`),
    /// or the full `.../chat/completions` URL.
    pub fn new(base_url: impl Into<String>) -> Self {
        let base_url = base_url.into();
        let local = is_loopback_url(&base_url);
        Self {
            client: reqwest::Client::new(),
            base_url,
            key: None,
            retry: RetryConfig::default(),
            temperature: 1.0,
            top_logprobs: DEFAULT_TOP_LOGPROBS,
            min_label_mass: DEFAULT_MIN_LABEL_MASS,
            extra_body: Map::new(),
            capabilities: Capabilities::new(KINDS)
                .with_max_choice_options(DEFAULT_TOP_LOGPROBS)
                .with_max_score_levels(10)
                .with_batching(false)
                .with_max_concurrent_requests(MAX_CONCURRENT_QUESTIONS)
                .with_local(local),
        }
    }

    /// Send this key as a bearer token.
    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.key = Some(key.into());
        self
    }

    /// Retry policy for 429 / 529 / transport failures (default
    /// [`RetryConfig::default`]).
    pub fn with_retry(mut self, retry: RetryConfig) -> Self {
        self.retry = retry;
        self
    }

    /// Temperature-scale the label log-probabilities before normalising
    /// (default 1.0, no scaling): above 1 softens an overconfident model,
    /// below 1 sharpens an underconfident one. Not the sampling temperature
    /// (requests always sample at 0). Choose it with
    /// [`calibrate`](super::calibrate()).
    ///
    /// Panics unless `t` is finite and positive.
    pub fn with_temperature(mut self, t: f64) -> Self {
        assert!(
            t.is_finite() && t > 0.0,
            "logprob temperature must be finite and positive, got {t}"
        );
        self.temperature = t;
        self
    }

    /// Most options one Choice may have (default 20). Requests ask for at
    /// least as many `top_logprobs` as the question has labels, so raise it
    /// only for servers that allow more than 20 (llama.cpp, vLLM with
    /// `--max-logprobs`).
    ///
    /// Panics outside `2..=26` (options are labelled with letters).
    pub fn with_max_choice_options(mut self, n: usize) -> Self {
        assert!(
            (2..=MAX_LETTER_LABELS).contains(&n),
            "logprob backend Choice limit must be 2..=26, got {n}"
        );
        self.capabilities.max_choice_options = n;
        self
    }

    /// Ask for this many `top_logprobs` (default 20). Requests ask for
    /// `max(k, the question's label count)`. Lower it for a server with a
    /// smaller cap.
    ///
    /// Panics on 0.
    pub fn with_top_logprobs(mut self, k: usize) -> Self {
        assert!(k > 0, "top_logprobs must be at least 1");
        self.top_logprobs = k;
        self
    }

    /// The share of the first token's probability the labels must cover
    /// for an answer to be read (default 0.5). Below it the response is
    /// [`DecisionError::BadResponse`]: the model was thinking, or answering
    /// in words, not choosing a label.
    ///
    /// Panics outside `(0, 1]`.
    pub fn with_min_label_mass(mut self, share: f64) -> Self {
        assert!(
            share > 0.0 && share <= 1.0,
            "minimum label mass must be in (0, 1], got {share}"
        );
        self.min_label_mass = share;
        self
    }

    /// Send `chat_template_kwargs: {"enable_thinking": false}`, which turns
    /// thinking off for templates that support it (Qwen3 and others) on
    /// llama.cpp, vLLM and SGLang. OpenAI's API rejects the field.
    pub fn with_thinking_disabled(self) -> Self {
        self.with_extra_body(json!({"chat_template_kwargs": {"enable_thinking": false}}))
    }

    /// Merge these fields into every request body — server-specific
    /// settings such as `chat_template_kwargs` or `reasoning_effort`. The
    /// fields the backend relies on (`model`, `messages`, `max_tokens`,
    /// `temperature`, `logprobs`, `top_logprobs`) cannot be overridden.
    ///
    /// The merge is deep: nested objects are merged key by key (so
    /// `with_thinking_disabled()` followed by
    /// `with_extra_body(json!({"chat_template_kwargs": {"x": 1}}))` keeps
    /// `enable_thinking: false`); any other value replaces the earlier one.
    ///
    /// Panics unless `fields` is a JSON object.
    pub fn with_extra_body(mut self, fields: Value) -> Self {
        let Value::Object(map) = fields else {
            panic!("with_extra_body takes a JSON object");
        };
        deep_merge(&mut self.extra_body, map);
        self
    }

    /// The configured scaling temperature.
    pub fn temperature(&self) -> f64 {
        self.temperature
    }

    /// Whether the base URL's host is loopback.
    pub fn is_local(&self) -> bool {
        self.capabilities.local
    }

    /// The URL requests are sent to.
    pub fn endpoint_url(&self) -> String {
        let base = self.base_url.trim().trim_end_matches('/');
        if base.ends_with("/chat/completions") {
            return base.to_string();
        }
        let rest = base.split_once("://").map_or(base, |(_, r)| r);
        if rest.contains('/') {
            format!("{base}/chat/completions")
        } else {
            format!("{base}/v1/chat/completions")
        }
    }

    /// One question, one completion.
    async fn ask_one(
        &self,
        request: &Request,
        id: &str,
        question: &Question,
    ) -> Result<(Answer, String, DecisionUsage, bool), DecisionError> {
        let labels = labels_for(question);
        let mut body = self.extra_body.clone();
        for (k, v) in [
            ("model", json!(request.model)),
            (
                "messages",
                json!([{"role": "user", "content": prompt(&request.state, question, &labels)}]),
            ),
            ("max_tokens", json!(1)),
            ("temperature", json!(0)),
            ("logprobs", json!(true)),
            ("top_logprobs", json!(self.top_logprobs.max(labels.len()))),
        ] {
            body.insert(k.to_string(), v);
        }
        let body = serde_json::to_vec(&body)
            .map_err(|e| DecisionError::Invalid(format!("request does not serialize: {e}")))?;
        let url = self.endpoint_url();
        let value = with_retries(&self.retry, || {
            post_json(&self.client, &url, self.key.as_deref(), &body)
        })
        .await?;

        let probs = label_distribution(&value, &labels, self.temperature, self.min_label_mass)
            .map_err(|e| DecisionError::BadResponse(format!("answers.{id}: {e}")))?;
        let answer: Answer = match question.kind() {
            QuestionKind::Noul => NoulAnswer::new(probs[0]).into(),
            QuestionKind::Choice => {
                let options = question.options().unwrap_or_default();
                ChoiceAnswer::new(options.into_iter().zip(probs)).into()
            }
            QuestionKind::Score => {
                let legend = question
                    .levels()
                    .unwrap_or_default()
                    .iter()
                    .map(render)
                    .collect();
                ScoreAnswer::new(legend, probs).into()
            }
        };

        let model = value
            .get("model")
            .and_then(Value::as_str)
            .filter(|m| !m.is_empty())
            .unwrap_or(&request.model)
            .to_string();
        let usage = value.get("usage").filter(|u| u.is_object());
        let tokens = |k: &str| usage.and_then(|u| u.get(k)).and_then(Value::as_u64);
        let reported = tokens("prompt_tokens").is_some();
        let usage = DecisionUsage::new(
            tokens("prompt_tokens").unwrap_or(0),
            tokens("completion_tokens").unwrap_or(0),
        );
        Ok((answer, model, usage, reported))
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl DecisionBackend for LogprobBackend {
    fn capabilities(&self) -> Capabilities {
        self.capabilities.clone()
    }

    /// Answers each question with its own completion, one after another.
    /// (A [`DecisionModel`](super::DecisionModel) splits a request into
    /// single questions and sends them concurrently itself.)
    async fn evaluate(&self, request: &Request) -> Result<Evaluation, DecisionError> {
        let mut eval: Option<Evaluation> = None;
        let (mut input, mut output, mut reported) = (0u64, 0u64, true);
        for (id, q) in &request.questions {
            if !KINDS.contains(&q.kind()) {
                return Err(DecisionError::Unsupported(format!(
                    "questions.{id}: {} questions are not supported by the logprob backend",
                    q.kind()
                )));
            }
            let (answer, model, usage, usage_reported) = self.ask_one(request, id, q).await?;
            input += usage.input_tokens;
            output += usage.output_tokens;
            reported &= usage_reported;
            let e = eval.take().unwrap_or_else(|| Evaluation::new(model, usage));
            eval = Some(e.with_answer(id.clone(), answer));
        }
        let mut eval = eval.ok_or_else(|| DecisionError::Invalid("questions: empty".into()))?;
        eval.usage = DecisionUsage::new(input, output);
        eval.usage_reported = reported;
        Ok(eval)
    }
}

/// Merge `from` into `into`: objects key by key, recursively; anything else
/// replaces.
fn deep_merge(into: &mut Map<String, Value>, from: Map<String, Value>) {
    for (key, value) in from {
        match (into.get_mut(&key), value) {
            (Some(Value::Object(existing)), Value::Object(new)) => deep_merge(existing, new),
            (_, value) => {
                into.insert(key, value);
            }
        }
    }
}

/// Whether `url`'s host is loopback: `localhost` (or a `*.localhost` name),
/// `127.0.0.0/8`, or `::1`.
pub(crate) fn is_loopback_url(url: &str) -> bool {
    let rest = url.trim();
    let rest = rest.split_once("://").map_or(rest, |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = if let Some(v6) = authority.strip_prefix('[') {
        v6.split(']').next().unwrap_or("")
    } else {
        authority.split(':').next().unwrap_or("")
    };
    if host.eq_ignore_ascii_case("localhost") || host.to_ascii_lowercase().ends_with(".localhost") {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

/// The labels for `question`, one per answer, in answer order.
fn labels_for(question: &Question) -> Vec<String> {
    let letters = |n: usize| -> Vec<String> {
        (0..n)
            .map(|i| char::from(b'A' + (i % MAX_LETTER_LABELS) as u8).to_string())
            .collect()
    };
    match question.kind() {
        QuestionKind::Noul => letters(2),
        QuestionKind::Choice => letters(question.options().map_or(0, |o| o.len())),
        QuestionKind::Score => (0..question.levels().map_or(0, <[_]>::len))
            .map(|i| i.to_string())
            .collect(),
    }
}

/// Text as is; anything else as JSON.
fn render(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    }
}

/// The single-label prompt for one question.
fn prompt(state: &Value, question: &Question, labels: &[String]) -> String {
    let mut lines: Vec<String> = Vec::new();
    match question.kind() {
        QuestionKind::Noul => {
            let (yes, no) = match question.noul_criteria() {
                Some((t, f)) => (
                    format!("Yes — {}", render(t)),
                    format!("No — {}", render(f)),
                ),
                None => ("Yes".to_string(), "No".to_string()),
            };
            lines.push(format!("{}: {yes}", labels[0]));
            lines.push(format!("{}: {no}", labels[1]));
        }
        QuestionKind::Choice => {
            for (label, (option, desc)) in labels
                .iter()
                .zip(question.choice_criteria().unwrap_or_default())
            {
                match desc {
                    Some(d) if !d.is_null() => {
                        lines.push(format!("{label}: {option} — {}", render(d)))
                    }
                    _ => lines.push(format!("{label}: {option}")),
                }
            }
        }
        QuestionKind::Score => {
            for (label, level) in labels.iter().zip(question.levels().unwrap_or_default()) {
                lines.push(format!("{label}: {}", render(level)));
            }
        }
    }
    let scale = if question.kind() == QuestionKind::Score {
        " (the levels are ordered, lowest first)"
    } else {
        ""
    };
    format!(
        "Read the state, then answer the question with a single label.\n\n\
         <state>\n{}\n</state>\n\n\
         Question: {}\n\n\
         Labels{scale}:\n{}\n\n\
         Reply with exactly one label — one of {} — and nothing else.",
        render(state),
        render(question.instructions()),
        lines.join("\n"),
        labels.join(", "),
    )
}

/// The distribution over `labels` read from the first generated token's
/// `top_logprobs` (see [`LogprobBackend`] for the rules).
pub(crate) fn label_distribution(
    body: &Value,
    labels: &[String],
    temperature: f64,
    min_label_mass: f64,
) -> Result<Vec<f64>, String> {
    let top = body
        .pointer("/choices/0/logprobs/content/0/top_logprobs")
        .and_then(Value::as_array)
        .ok_or("the response has no choices[0].logprobs.content[0].top_logprobs")?;
    let mut mass = vec![0.0f64; labels.len()];
    let mut reported_total = 0.0f64;
    let mut smallest = f64::INFINITY;
    for entry in top {
        let Some(token) = entry.get("token").and_then(Value::as_str) else {
            continue;
        };
        let Some(logprob) = entry.get("logprob").and_then(Value::as_f64) else {
            continue;
        };
        if logprob.is_nan() {
            continue;
        }
        let p = logprob.exp();
        reported_total += p;
        if p > 0.0 {
            smallest = smallest.min(p);
        }
        let token = token.trim().to_ascii_uppercase();
        if let Some(i) = labels.iter().position(|l| *l == token) {
            mass[i] += p;
        }
    }
    let label_mass: f64 = mass.iter().sum();
    if label_mass <= 0.0 {
        return Err(format!(
            "none of the labels {} is among the top {} tokens",
            labels.join(", "),
            top.len()
        ));
    }
    if label_mass < min_label_mass {
        return Err(format!(
            "labels cover only {label_mass:.3} of the first-token probability (minimum \
             {min_label_mass}); the model is not answering with a label — is thinking on?"
        ));
    }
    // An absent label's probability is below every reported one and within
    // what the reported tokens leave over.
    let absent = smallest.min(1.0 - reported_total).clamp(ABSENT_FLOOR, 1.0);
    let logits: Vec<f64> = mass
        .iter()
        .map(|&m| if m > 0.0 { m } else { absent }.ln() / temperature)
        .collect();
    let max = logits.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let weights: Vec<f64> = logits.iter().map(|l| (l - max).exp()).collect();
    let total: f64 = weights.iter().sum();
    let mut probs: Vec<f64> = weights.into_iter().map(|w| w / total).collect();
    // A small temperature can underflow an absent label's share to 0: keep
    // the floor after scaling, so absence alone never yields 0 or 1.
    let mut floored = false;
    for (p, m) in probs.iter_mut().zip(&mass) {
        if *m <= 0.0 && *p < ABSENT_FLOOR {
            *p = ABSENT_FLOOR;
            floored = true;
        }
    }
    if floored {
        let total: f64 = probs.iter().sum();
        probs.iter_mut().for_each(|p| *p /= total);
    }
    Ok(probs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn top(tokens: &[(&str, f64)]) -> Value {
        let list: Vec<Value> = tokens
            .iter()
            .map(|(t, lp)| json!({"token": t, "logprob": lp}))
            .collect();
        json!({"choices": [{"logprobs": {"content": [{"token": "A", "logprob": 0.0, "top_logprobs": list}]}}]})
    }

    fn ab() -> Vec<String> {
        vec!["A".into(), "B".into()]
    }

    fn dist(tokens: &[(&str, f64)], t: f64) -> Result<Vec<f64>, String> {
        let probs: Vec<(&str, f64)> = tokens.iter().map(|(k, p)| (*k, p.ln())).collect();
        label_distribution(&top(&probs), &ab(), t, DEFAULT_MIN_LABEL_MASS)
    }

    #[test]
    fn labels_are_normalised_and_summed() {
        // " A" and "a" are both A; B once.
        let p = dist(&[(" A", 0.5), ("a", 0.2), ("B", 0.3)], 1.0).unwrap();
        assert!((p[0] - 0.7).abs() < 1e-9, "{p:?}");
        assert!((p[1] - 0.3).abs() < 1e-9, "{p:?}");
    }

    #[test]
    fn a_missing_label_is_bounded_not_zero() {
        // B is absent: it gets min(smallest reported 0.3, 1 - 0.9) = 0.1.
        let p = dist(&[("A", 0.6), ("The", 0.3)], 1.0).unwrap();
        assert!((p[0] - 0.6 / 0.7).abs() < 1e-9, "{p:?}");
        assert!((p[1] - 0.1 / 0.7).abs() < 1e-9, "{p:?}");
        // Even when the reported tokens cover everything, never exactly 1.
        let p = dist(&[("A", 1.0)], 1.0).unwrap();
        assert!(p[0] < 1.0 && p[1] > 0.0, "{p:?}");
    }

    #[test]
    fn no_label_is_an_error() {
        let e = dist(&[("The", 0.9), ("Yes", 0.1)], 1.0).unwrap_err();
        assert!(e.contains("none of the labels"), "{e}");
        let e = label_distribution(&json!({"choices": []}), &ab(), 1.0, 0.5).unwrap_err();
        assert!(e.contains("top_logprobs"), "{e}");
    }

    #[test]
    fn too_little_label_mass_is_an_error() {
        // A thinking model: `<think>` takes nearly everything.
        let body = top(&[("<think>", -0.0001), ("A", -12.0), ("B", -14.0)]);
        let e = label_distribution(&body, &ab(), 1.0, 0.5).unwrap_err();
        assert!(e.contains("labels cover only"), "{e}");
        // A model answering in words.
        let e = dist(&[("No", 0.9), ("A", 0.05), ("B", 0.01)], 1.0).unwrap_err();
        assert!(e.contains("labels cover only"), "{e}");
        // Positive control: the same shape with the labels dominant.
        let p = dist(&[("B", 0.9), ("No", 0.05), ("A", 0.05)], 1.0).unwrap();
        assert!((p[1] - 0.9 / 0.95).abs() < 1e-9, "{p:?}");
        // A lower floor accepts the word answer's labels.
        let words = top(&[
            ("No", 0.9f64.ln()),
            ("A", 0.05f64.ln()),
            ("B", 0.01f64.ln()),
        ]);
        assert!(label_distribution(&words, &ab(), 1.0, 0.05).is_ok());
    }

    #[test]
    fn a_tiny_temperature_keeps_absent_labels_off_zero() {
        // B absent; at t = 0.01 its bounded 0.1 share would underflow.
        let p = dist(&[("A", 0.6), ("The", 0.3)], 0.01).unwrap();
        assert!(p[1] > 0.0 && p[0] < 1.0, "{p:?}");
        assert!((p.iter().sum::<f64>() - 1.0).abs() < 1e-12);
        // Positive control: at t = 1 no floor is needed.
        let p = dist(&[("A", 0.6), ("The", 0.3)], 1.0).unwrap();
        assert!((p[1] - 0.1 / 0.7).abs() < 1e-9, "{p:?}");
    }

    #[test]
    fn extra_body_merges_deeply() {
        let b = LogprobBackend::new("http://localhost:1")
            .with_thinking_disabled()
            .with_extra_body(json!({"chat_template_kwargs": {"custom": 1}, "top_k": 5}))
            .with_extra_body(json!({"top_k": 7}));
        assert_eq!(
            Value::Object(b.extra_body.clone()),
            json!({"chat_template_kwargs": {"enable_thinking": false, "custom": 1}, "top_k": 7})
        );
        // A non-object value replaces.
        let b = b.with_extra_body(json!({"chat_template_kwargs": null}));
        assert_eq!(b.extra_body["chat_template_kwargs"], Value::Null);
    }

    #[test]
    fn temperature_softens_and_sharpens() {
        let p1 = dist(&[("A", 0.8), ("B", 0.2)], 1.0).unwrap()[0];
        let p2 = dist(&[("A", 0.8), ("B", 0.2)], 2.0).unwrap()[0];
        let p05 = dist(&[("A", 0.8), ("B", 0.2)], 0.5).unwrap()[0];
        assert!((p1 - 0.8).abs() < 1e-9);
        // 0.8^(1/2) / (0.8^(1/2) + 0.2^(1/2)) = 2/3
        assert!((p2 - 2.0 / 3.0).abs() < 1e-9, "{p2}");
        // 0.64 / (0.64 + 0.04)
        assert!((p05 - 0.64 / 0.68).abs() < 1e-9, "{p05}");
    }

    #[test]
    fn loopback_detection() {
        for local in [
            "http://localhost:8080",
            "http://LOCALHOST/v1",
            "http://127.0.0.1:8000/v1",
            "http://127.1.2.3",
            "http://[::1]:8080/v1",
            "http://api.localhost:1",
            "localhost:8080",
        ] {
            assert!(is_loopback_url(local), "{local}");
        }
        for remote in [
            "https://api.openai.com/v1",
            "http://10.0.0.5:8080",
            "http://0.0.0.0:8080",
            "http://localhost.evil.example",
            "http://127.0.0.1.evil.example",
            "http://user@example.com",
        ] {
            assert!(!is_loopback_url(remote), "{remote}");
        }
    }

    #[test]
    fn endpoint_forms() {
        let e = |u: &str| LogprobBackend::new(u).endpoint_url();
        assert_eq!(
            e("http://localhost:8080"),
            "http://localhost:8080/v1/chat/completions"
        );
        assert_eq!(
            e("http://localhost:8080/v1/"),
            "http://localhost:8080/v1/chat/completions"
        );
        assert_eq!(
            e("https://api.groq.com/openai/v1"),
            "https://api.groq.com/openai/v1/chat/completions"
        );
        assert_eq!(
            e("http://h/v1/chat/completions"),
            "http://h/v1/chat/completions"
        );
    }

    #[test]
    fn labels_by_kind() {
        assert_eq!(labels_for(&Question::noul("q")), ["A", "B"]);
        assert_eq!(
            labels_for(&Question::choice("q", ["x", "y", "z"])),
            ["A", "B", "C"]
        );
        assert_eq!(
            labels_for(&Question::score("q", ["lo", "mid", "hi"])),
            ["0", "1", "2"]
        );
    }

    #[test]
    fn only_the_three_kinds_are_listed() {
        let caps = LogprobBackend::new("http://localhost:1").capabilities();
        assert_eq!(caps.question_kinds, KINDS);
        assert!(!caps.batching);
        assert_eq!(caps.max_concurrent_requests, 8);
    }
}
