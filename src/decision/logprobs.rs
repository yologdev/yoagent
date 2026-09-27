//! [`LogprobBackend`]: decisions from any OpenAI-compatible
//! `/chat/completions` server that returns logprobs.
//!
//! Each question becomes one completion of **one token** at temperature 0:
//! the prompt presents the state, the question and a label per answer, and
//! the answer is read from the first token's `top_logprobs` — never from the
//! generated text.
//!
//! | Question | Labels |
//! |----------|--------|
//! | Noul     | `A` = yes, `B` = no |
//! | Choice   | `A`, `B`, `C`, ... — the options in order (at most 26) |
//! | Score    | `0` ... `9` — the levels, lowest first |
//!
//! Reading the answer: every top-K token is trimmed of whitespace and
//! upper-cased (so `" A"` and `"a"` both count as `A`), the probability of
//! tokens that map to the same label is summed, the optional temperature is
//! applied to the resulting log-probabilities, and a softmax over the labels
//! that appeared gives the distribution; a label outside the top K gets
//! probability 0. No label in the top K is [`DecisionError::BadResponse`].
//!
//! **Calibration is approximate.** A general LLM's next-token probability
//! for a label is not a calibrated probability the way a trained decision
//! model's is; it is often overconfident. Measure it on your own data with
//! [`calibrate`](super::calibrate()) and apply the suggested
//! [`with_temperature`](LogprobBackend::with_temperature).

use super::answer::{Answer, ChoiceAnswer, DecisionUsage, Evaluation, NoulAnswer, ScoreAnswer};
use super::backend::{Capabilities, DecisionBackend};
use super::error::DecisionError;
use super::question::{Question, QuestionKind, Request};
use super::systemone::{post_json, with_retries};
use crate::retry::RetryConfig;
use serde_json::{json, Value};

/// Most `top_logprobs` OpenAI accepts; the default K and Choice limit.
const DEFAULT_TOP_LOGPROBS: usize = 20;
/// Letters available as Choice labels.
const MAX_LETTER_LABELS: usize = 26;
/// Questions of one request evaluated at once (one HTTP request each).
pub(crate) const MAX_CONCURRENT_QUESTIONS: usize = 8;

/// A decision backend over an OpenAI-compatible `/chat/completions` endpoint
/// that returns logprobs — llama.cpp's `llama-server`, vLLM, SGLang,
/// LM Studio, or a hosted API.
///
/// Usually built for you by
/// [`DecisionModel::logprobs`](super::DecisionModel::logprobs); build one
/// directly to change its limits, then wrap it with
/// [`DecisionModel::from_backend`](super::DecisionModel::from_backend)
/// (which is unpriced until `with_cost`).
///
/// - **Every question type**; Choice up to 20 options by default (OpenAI caps
///   `top_logprobs` at 20; raise it to 26 with
///   [`with_max_choice_options`](Self::with_max_choice_options) for servers
///   that allow more), Score up to 10 levels.
/// - **Batching:** a request with several questions fans out one HTTP request
///   per question, at most 8 at a time, and returns one [`Evaluation`] with
///   the usage summed.
/// - **Local** ([`Capabilities::local`]) only when the base URL's host is
///   loopback (`localhost`, `127.0.0.0/8`, `::1`).
/// - **Key:** none unless [`with_api_key`](Self::with_api_key); no
///   environment variable is ever read.
/// - Rate limits (429/529) and transport failures are retried like
///   [`SystemOneBackend`](super::SystemOneBackend)'s, honouring
///   `retry-after`; 422 is [`DecisionError::Invalid`]; any other non-success
///   status is [`DecisionError::Http`].
///
/// See the [module docs](self) for how answers are read, and why their
/// calibration is approximate.
#[derive(Clone)]
pub struct LogprobBackend {
    client: reqwest::Client,
    base_url: String,
    key: Option<String>,
    retry: RetryConfig,
    temperature: f64,
    top_logprobs: usize,
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
            capabilities: Capabilities::new(QuestionKind::all())
                .with_max_choice_options(DEFAULT_TOP_LOGPROBS)
                .with_max_score_levels(10)
                .with_batching(true)
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
    /// least this many `top_logprobs`, so raise it only for servers that
    /// allow more than 20 (llama.cpp, vLLM with `--max-logprobs`).
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

    /// Ask for this many `top_logprobs` (default 20; never fewer than the
    /// question's labels). Lower it for a server with a smaller cap.
    ///
    /// Panics on 0.
    pub fn with_top_logprobs(mut self, k: usize) -> Self {
        assert!(k > 0, "top_logprobs must be at least 1");
        self.top_logprobs = k;
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
        let body = json!({
            "model": request.model,
            "messages": [{"role": "user", "content": prompt(&request.state, question, &labels)}],
            "max_tokens": 1,
            "temperature": 0,
            "logprobs": true,
            "top_logprobs": self.top_logprobs.max(labels.len()),
        });
        let body = serde_json::to_vec(&body)
            .map_err(|e| DecisionError::Invalid(format!("request does not serialize: {e}")))?;
        let url = self.endpoint_url();
        let value = with_retries(&self.retry, || {
            post_json(&self.client, &url, self.key.as_deref(), &body)
        })
        .await?;

        let probs = label_distribution(&value, &labels, self.temperature)
            .map_err(|e| DecisionError::BadResponse(format!("answers.{id}: {e}")))?;
        let answer: Answer = match question.kind() {
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
            _ => NoulAnswer::new(probs[0]).into(),
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

#[async_trait::async_trait]
impl DecisionBackend for LogprobBackend {
    fn capabilities(&self) -> Capabilities {
        self.capabilities.clone()
    }

    async fn evaluate(&self, request: &Request) -> Result<Evaluation, DecisionError> {
        use futures::{StreamExt, TryStreamExt};
        type Asked = (String, (Answer, String, DecisionUsage, bool));
        type Pending<'a> = std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Asked, DecisionError>> + Send + 'a>,
        >;
        // Built in a loop, not a closure: a closure over borrowed questions
        // trips the `Send` check of `async_trait`'s boxed future.
        let mut pending: Vec<Pending<'_>> = Vec::with_capacity(request.questions.len());
        for (id, q) in &request.questions {
            pending.push(Box::pin(async move {
                self.ask_one(request, id, q).await.map(|r| (id.clone(), r))
            }));
        }
        // In order, at most MAX_CONCURRENT_QUESTIONS in flight; the first
        // error stops the rest.
        let results: Vec<Asked> = futures::stream::iter(pending)
            .buffered(MAX_CONCURRENT_QUESTIONS)
            .try_collect()
            .await?;

        let mut eval: Option<Evaluation> = None;
        let mut input = 0u64;
        let mut output = 0u64;
        let mut reported = true;
        for (id, (answer, model, usage, usage_reported)) in results {
            input += usage.input_tokens;
            output += usage.output_tokens;
            reported &= usage_reported;
            let e = eval.take().unwrap_or_else(|| Evaluation::new(model, usage));
            eval = Some(e.with_answer(id, answer));
        }
        let mut eval = eval.ok_or_else(|| DecisionError::Invalid("questions: empty".into()))?;
        eval.usage = DecisionUsage::new(input, output);
        eval.usage_reported = reported;
        Ok(eval)
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
        QuestionKind::Choice => letters(question.options().map_or(0, |o| o.len())),
        QuestionKind::Score => (0..question.levels().map_or(0, <[_]>::len))
            .map(|i| i.to_string())
            .collect(),
        _ => letters(2),
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
        _ => {
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
/// `top_logprobs`: tokens trimmed and upper-cased, duplicates summed,
/// `temperature` applied to the log-probabilities, softmax over the labels
/// present (absent labels get 0).
pub(crate) fn label_distribution(
    body: &Value,
    labels: &[String],
    temperature: f64,
) -> Result<Vec<f64>, String> {
    let top = body
        .pointer("/choices/0/logprobs/content/0/top_logprobs")
        .and_then(Value::as_array)
        .ok_or("the response has no choices[0].logprobs.content[0].top_logprobs")?;
    let mut mass = vec![0.0f64; labels.len()];
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
        let token = token.trim().to_ascii_uppercase();
        if let Some(i) = labels.iter().position(|l| *l == token) {
            mass[i] += logprob.exp();
        }
    }
    let logits: Vec<Option<f64>> = mass
        .iter()
        .map(|&m| (m > 0.0 && m.is_finite()).then(|| m.ln() / temperature))
        .collect();
    let max = logits
        .iter()
        .flatten()
        .copied()
        .fold(f64::NEG_INFINITY, f64::max);
    if !max.is_finite() {
        return Err(format!(
            "none of the labels {} is among the top {} tokens",
            labels.join(", "),
            top.len()
        ));
    }
    let weights: Vec<f64> = logits
        .iter()
        .map(|l| l.map_or(0.0, |l| (l - max).exp()))
        .collect();
    let total: f64 = weights.iter().sum();
    Ok(weights.into_iter().map(|w| w / total).collect())
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

    #[test]
    fn labels_are_normalised_and_summed() {
        // " A" and "a" are both A; B once.
        let body = top(&[(" A", 0.5f64.ln()), ("a", 0.2f64.ln()), ("B", 0.3f64.ln())]);
        let p = label_distribution(&body, &ab(), 1.0).unwrap();
        assert!((p[0] - 0.7).abs() < 1e-9, "{p:?}");
        assert!((p[1] - 0.3).abs() < 1e-9, "{p:?}");
    }

    #[test]
    fn a_missing_label_is_renormalised_to_zero() {
        let body = top(&[("A", 0.6f64.ln()), ("The", 0.3f64.ln())]);
        let p = label_distribution(&body, &ab(), 1.0).unwrap();
        assert_eq!(p, vec![1.0, 0.0]);
    }

    #[test]
    fn no_label_is_an_error() {
        let body = top(&[("The", -0.1), ("Yes", -2.0)]);
        let e = label_distribution(&body, &ab(), 1.0).unwrap_err();
        assert!(e.contains("none of the labels"), "{e}");
        assert!(label_distribution(&json!({"choices": []}), &ab(), 1.0).is_err());
    }

    #[test]
    fn temperature_softens_and_sharpens() {
        let body = top(&[("A", 0.8f64.ln()), ("B", 0.2f64.ln())]);
        let p1 = label_distribution(&body, &ab(), 1.0).unwrap()[0];
        let p2 = label_distribution(&body, &ab(), 2.0).unwrap()[0];
        let p05 = label_distribution(&body, &ab(), 0.5).unwrap()[0];
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
}
