//! Decision models: typed questions in, calibrated probabilities out.
//!
//! A decision model evaluates a `state` (text or JSON) against typed
//! questions — **Noul** (yes/no), **Choice** (one of N options), **Score**
//! (an ordered scale) — and returns typed answers with probabilities, not
//! prose. They are fast (~100 ms) and cheap, which makes them useful for the
//! small judgments an agent makes constantly: is this request asking for an
//! action, which skill fits, does this tool call destroy data.
//!
//! Behind the `decision` Cargo feature, off by default. Nothing is ever sent
//! until you construct a [`DecisionModel`] and use it — an API key in the
//! environment enables nothing on its own.
//!
//! ```no_run
//! # async fn demo() -> Result<(), yoagent::decision::DecisionError> {
//! use yoagent::decision::DecisionModel;
//!
//! let jev = DecisionModel::jev(); // TypeSafe Jev; key from TYPESAFE_API_KEY
//! let p = jev.noul("Help! My payouts have been failing for 3 days.",
//!                  "Does this convey urgency?").await?;
//! let team = jev.choice("My card was charged twice.",
//!                       "Which team should handle this?",
//!                       ["billing", "technical", "sales"]).await?;
//! println!("urgent: {p:.2}, team: {} ({:.2})", team.choice, team.confidence);
//! # Ok(()) }
//! ```
//!
//! Several questions about one state go in one request:
//!
//! ```no_run
//! # async fn demo() -> Result<(), yoagent::decision::DecisionError> {
//! # let jev = yoagent::decision::DecisionModel::jev();
//! let eval = jev
//!     .ask("Help! My payouts have been failing for 3 days.")
//!     .noul("urgent", "Does this convey urgency?")
//!     .choice("team", "Which team should handle this?", ["billing", "technical"])
//!     .score("mood", "How frustrated is the customer?", ["Calm", "Frustrated", "Very angry"])
//!     .send()
//!     .await?;
//! let urgent = eval.p_true("urgent").unwrap_or(0.0);
//! # Ok(()) }
//! ```
//!
//! Integrations depend on the [`DecisionBackend`] trait, never on a vendor:
//! [`SystemOneBackend`] speaks the SystemOne HTTP API (TypeSafe, OpenCode Zen,
//! self-hosted JevK5), [`MockBackend`] scripts answers for tests, and any
//! other backend plugs in through [`DecisionModel::from_backend`].
//!
//! # Limits you should know
//!
//! - **Hosted backends see the state.** Whatever you evaluate is sent to the
//!   vendor. [`DecisionModel::local`] keeps it on your machine.
//! - **Thresholds are per model.** A threshold tuned on `jev-1.13.0` is not
//!   portable to the next release; pin a version with
//!   [`with_model`](DecisionModel::with_model) once you tune one.
//! - **Adversarial content can steer answers.** State is data to the model,
//!   not an instruction channel it defends. Never treat a decision model as a
//!   security boundary.
//! - Known weak spots (Jev 1.13): literal reading, arithmetic, counting and
//!   dates, and large states full of irrelevant detail.
//!
//! See the book chapter *Decision models* for the agent integrations
//! (`Agent::with_decision_model`, `Agent::with_tool_gate`).

mod answer;
mod backend;
mod error;
mod mock;
mod question;
mod systemone;

pub use answer::{
    distribution_confidence, Answer, ChoiceAnswer, DecisionUsage, Evaluation, NoulAnswer,
    ScoreAnswer,
};
pub use backend::{Capabilities, DecisionBackend};
pub use error::DecisionError;
pub use mock::MockBackend;
pub use question::{Question, QuestionKind, Request};
pub use systemone::{
    ModelInfo, SystemOneBackend, OPENCODE_API_KEY_ENV, OPENCODE_ZEN_BASE_URL, TYPESAFE_API_KEY_ENV,
    TYPESAFE_BASE_URL, TYPESAFE_BASE_URL_ENV,
};

use crate::provider::CostConfig;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

/// The `prices.json` provider key for TypeSafe's models.
pub const TYPESAFE_PRICE_PROVIDER: &str = "typesafe";

/// Default overall timeout of one [`DecisionModel`] call, retries included.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
enum Slot {
    SystemOne(SystemOneBackend),
    Custom(Arc<dyn DecisionBackend>),
}

impl Slot {
    fn get(&self) -> &dyn DecisionBackend {
        match self {
            Slot::SystemOne(b) => b,
            Slot::Custom(b) => b.as_ref(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Pricing {
    /// Look the answering model up in the resolved price table under this
    /// provider key.
    Table(&'static str),
    Fixed(CostConfig),
    Unpriced,
}

/// A decision model: a backend plus the model id to ask for. The handle
/// everything else takes.
///
/// Cheap to clone (the backend is shared). Choose one with a preset —
/// [`jev`](Self::jev), [`jev_opencode`](Self::jev_opencode),
/// [`jev_opencode_free`](Self::jev_opencode_free), [`local`](Self::local) —
/// or [`from_backend`](Self::from_backend); everything else has defaults.
#[derive(Clone)]
pub struct DecisionModel {
    backend: Slot,
    model: String,
    timeout: Duration,
    pricing: Pricing,
}

impl std::fmt::Debug for DecisionModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut d = f.debug_struct("DecisionModel");
        d.field("model", &self.model)
            .field("timeout", &self.timeout)
            .field("pricing", &self.pricing);
        match &self.backend {
            Slot::SystemOne(b) => d.field("backend", b),
            Slot::Custom(_) => d.field("backend", &"custom"),
        };
        d.finish()
    }
}

impl DecisionModel {
    /// TypeSafe's Jev (`jev-latest`), key from `TYPESAFE_API_KEY` and base
    /// URL from `TYPESAFE_BASE_URL` (default `https://api.typesafe.ai`), both
    /// read at call time. Priced from the built-in table by the versioned id
    /// the API reports ($0.042 per million input tokens for `jev-1.13.0`;
    /// output is free).
    pub fn jev() -> Self {
        Self {
            backend: Slot::SystemOne(SystemOneBackend::typesafe()),
            model: "jev-latest".into(),
            timeout: DEFAULT_TIMEOUT,
            pricing: Pricing::Table(TYPESAFE_PRICE_PROVIDER),
        }
    }

    /// Jev through OpenCode Zen (`jev-1.13`), key from `OPENCODE_API_KEY`.
    /// Unpriced: a gateway's bill is not the vendor's list price.
    pub fn jev_opencode() -> Self {
        Self {
            backend: Slot::SystemOne(SystemOneBackend::opencode_zen()),
            model: "jev-1.13".into(),
            timeout: DEFAULT_TIMEOUT,
            pricing: Pricing::Unpriced,
        }
    }

    /// Jev's free tier on OpenCode Zen (`jev-1.13-free`), key from
    /// `OPENCODE_API_KEY`. Unpriced like [`jev_opencode`](Self::jev_opencode);
    /// set [`with_cost`](Self::with_cost) if you want it reported as $0.
    pub fn jev_opencode_free() -> Self {
        Self {
            model: "jev-1.13-free".into(),
            ..Self::jev_opencode()
        }
    }

    /// A self-hosted SystemOne-compatible server (JevK5, ...) at `base_url`.
    /// No key; model id `jev-latest` until [`with_model`](Self::with_model);
    /// costs reported as $0.
    pub fn local(base_url: impl Into<String>) -> Self {
        Self {
            backend: Slot::SystemOne(SystemOneBackend::new(base_url)),
            model: "jev-latest".into(),
            timeout: DEFAULT_TIMEOUT,
            pricing: Pricing::Fixed(CostConfig::new(0.0, 0.0)),
        }
    }

    /// Any backend, asking for `model`. Unpriced until
    /// [`with_cost`](Self::with_cost), except that a
    /// [`local`](Capabilities::local) backend reports $0.
    pub fn from_backend(backend: impl DecisionBackend + 'static, model: impl Into<String>) -> Self {
        let local = backend.capabilities().local;
        Self {
            backend: Slot::Custom(Arc::new(backend)),
            model: model.into(),
            timeout: DEFAULT_TIMEOUT,
            pricing: if local {
                Pricing::Fixed(CostConfig::new(0.0, 0.0))
            } else {
                Pricing::Unpriced
            },
        }
    }

    /// A configured [`SystemOneBackend`] (custom retry, client, capabilities),
    /// priced like [`jev`](Self::jev) when it is hosted and $0 when local.
    pub fn from_systemone(backend: SystemOneBackend, model: impl Into<String>) -> Self {
        let local = backend.capabilities().local;
        Self {
            backend: Slot::SystemOne(backend),
            model: model.into(),
            timeout: DEFAULT_TIMEOUT,
            pricing: if local {
                Pricing::Fixed(CostConfig::new(0.0, 0.0))
            } else {
                Pricing::Table(TYPESAFE_PRICE_PROVIDER)
            },
        }
    }

    /// Ask for this model id or alias — pin a version such as `jev-1.13.0`
    /// once you have tuned thresholds against it.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// Overall time limit for one call, retries included (default 30 s).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Send this API key instead of reading the preset's environment
    /// variable. Only meaningful for SystemOne backends; ignored (with a
    /// warning) for a custom backend.
    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        match self.backend {
            Slot::SystemOne(b) => self.backend = Slot::SystemOne(b.with_api_key(key)),
            Slot::Custom(_) => {
                tracing::warn!(
                    "DecisionModel::with_api_key ignored: custom backends own their keys"
                )
            }
        }
        self
    }

    /// Retry policy for rate limits and overload (SystemOne backends only).
    pub fn with_retry(mut self, retry: crate::retry::RetryConfig) -> Self {
        match self.backend {
            Slot::SystemOne(b) => self.backend = Slot::SystemOne(b.with_retry(retry)),
            Slot::Custom(_) => {
                tracing::warn!(
                    "DecisionModel::with_retry ignored: custom backends own their retries"
                )
            }
        }
        self
    }

    /// Price every evaluation at these rates (`None` = unpriced), instead of
    /// the preset's pricing.
    pub fn with_cost(mut self, cost: Option<CostConfig>) -> Self {
        self.pricing = match cost {
            Some(c) => Pricing::Fixed(c),
            None => Pricing::Unpriced,
        };
        self
    }

    /// The model id or alias requests ask for.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The configured overall timeout.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// What the backend can answer.
    pub fn capabilities(&self) -> Capabilities {
        self.backend.get().capabilities()
    }

    /// The price of `usage` on the model that reported it: the table entry
    /// for a versioned id (an alias or an unlisted version is unpriced), the
    /// fixed rates, or `None`.
    pub fn cost_usd(&self, model: &str, usage: &DecisionUsage) -> Option<f64> {
        let cost = match &self.pricing {
            Pricing::Table(provider) => {
                crate::provider::prices::global::resolved().cost(provider, model)?
            }
            Pricing::Fixed(c) => c.clone(),
            Pricing::Unpriced => return None,
        };
        Some(cost.cost_usd(&usage.to_usage()))
    }

    /// Start a batched request about `state` (a string, or JSON via
    /// `serde_json::json!`).
    pub fn ask(&self, state: impl Into<Value>) -> Ask<'_> {
        Ask {
            model: self,
            state: state.into(),
            questions: Vec::new(),
        }
    }

    /// Probability that the answer to a yes/no question is yes.
    pub async fn noul(
        &self,
        state: impl Into<Value>,
        instructions: impl Into<Value>,
    ) -> Result<f64, DecisionError> {
        let eval = self.ask(state).noul("q", instructions).send().await?;
        eval.p_true("q")
            .ok_or_else(|| DecisionError::BadResponse("no noul answer".into()))
    }

    /// Pick one of `options`.
    pub async fn choice<I, S>(
        &self,
        state: impl Into<Value>,
        instructions: impl Into<Value>,
        options: I,
    ) -> Result<ChoiceAnswer, DecisionError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let eval = self
            .ask(state)
            .choice("q", instructions, options)
            .send()
            .await?;
        eval.choice("q")
            .cloned()
            .ok_or_else(|| DecisionError::BadResponse("no choice answer".into()))
    }

    /// Rate on `levels`, lowest first.
    pub async fn score<I, S>(
        &self,
        state: impl Into<Value>,
        instructions: impl Into<Value>,
        levels: I,
    ) -> Result<ScoreAnswer, DecisionError>
    where
        I: IntoIterator<Item = S>,
        S: Into<Value>,
    {
        let eval = self
            .ask(state)
            .score("q", instructions, levels)
            .send()
            .await?;
        eval.score("q")
            .cloned()
            .ok_or_else(|| DecisionError::BadResponse("no score answer".into()))
    }

    /// Evaluate `questions` about `state` in one call (split per question when
    /// the backend does not batch).
    pub async fn evaluate(
        &self,
        state: impl Into<Value>,
        questions: Vec<(String, Question)>,
    ) -> Result<Evaluation, DecisionError> {
        let mut request = Request::new(self.model.clone(), state);
        request.questions = questions;
        self.evaluate_request(request).await
    }

    /// Evaluate a prepared [`Request`] (its `model` is used as is).
    ///
    /// Validates against the backend's [`Capabilities`], applies the timeout,
    /// checks every question got an answer of its type, and prices the
    /// result.
    pub async fn evaluate_request(&self, request: Request) -> Result<Evaluation, DecisionError> {
        let backend = self.backend.get();
        let caps = backend.capabilities();
        request.validate(&caps)?;

        let span = tracing::debug_span!(
            "decision",
            model = %request.model,
            questions = request.questions.len(),
            tokens_in = tracing::field::Empty,
            cost_usd = tracing::field::Empty,
        );
        let work = async {
            if caps.batching || request.questions.len() == 1 {
                let eval = backend.evaluate(&request).await?;
                check_complete(&request, &eval)?;
                Ok(eval)
            } else {
                let mut merged: Option<Evaluation> = None;
                for (id, q) in &request.questions {
                    let single = Request {
                        model: request.model.clone(),
                        state: request.state.clone(),
                        questions: vec![(id.clone(), q.clone())],
                    };
                    let eval = backend.evaluate(&single).await?;
                    check_complete(&single, &eval)?;
                    merged = Some(match merged {
                        None => eval,
                        Some(mut m) => {
                            m.usage.input_tokens += eval.usage.input_tokens;
                            m.usage.output_tokens += eval.usage.output_tokens;
                            m.answers.extend(eval.answers);
                            m
                        }
                    });
                }
                merged.ok_or_else(|| DecisionError::Invalid("questions: empty".into()))
            }
        };
        let result = {
            use tracing::Instrument;
            tokio::time::timeout(self.timeout, work.instrument(span.clone())).await
        };
        let mut eval = result.map_err(|_| DecisionError::Timeout(self.timeout))??;
        eval.cost_usd = self.cost_usd(&eval.model, &eval.usage);
        span.record("tokens_in", eval.usage.input_tokens);
        if let Some(c) = eval.cost_usd {
            span.record("cost_usd", c);
        }
        Ok(eval)
    }
}

/// Every question must have an answer of its own type.
fn check_complete(request: &Request, eval: &Evaluation) -> Result<(), DecisionError> {
    for (id, q) in &request.questions {
        match eval.get(id) {
            None => return Err(DecisionError::BadResponse(format!("answers.{id}: missing"))),
            Some(a) if a.kind() != q.kind() => {
                return Err(DecisionError::BadResponse(format!(
                    "answers.{id}: a {} answer to a {} question",
                    a.kind(),
                    q.kind()
                )))
            }
            Some(_) => {}
        }
    }
    Ok(())
}

/// A batched request under construction; see [`DecisionModel::ask`].
#[must_use = "a request does nothing until .send().await"]
pub struct Ask<'a> {
    model: &'a DecisionModel,
    state: Value,
    questions: Vec<(String, Question)>,
}

impl Ask<'_> {
    /// Add a yes/no question under `id`.
    pub fn noul(self, id: impl Into<String>, instructions: impl Into<Value>) -> Self {
        self.question(id, Question::noul(instructions))
    }

    /// Add a choice under `id`.
    pub fn choice<I, S>(
        self,
        id: impl Into<String>,
        instructions: impl Into<Value>,
        options: I,
    ) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.question(id, Question::choice(instructions, options))
    }

    /// Add a score under `id`.
    pub fn score<I, S>(
        self,
        id: impl Into<String>,
        instructions: impl Into<Value>,
        levels: I,
    ) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<Value>,
    {
        self.question(id, Question::score(instructions, levels))
    }

    /// Add any [`Question`] under `id`.
    pub fn question(mut self, id: impl Into<String>, question: Question) -> Self {
        self.questions.push((id.into(), question));
        self
    }

    /// Send the request.
    pub async fn send(self) -> Result<Evaluation, DecisionError> {
        self.model.evaluate(self.state, self.questions).await
    }
}
