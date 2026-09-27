//! Decision models: typed questions in, calibrated probabilities out.
//!
//! A decision model evaluates a `state` (text or JSON) against typed
//! questions — **Noul** (yes/no), **Choice** (one of N options), **Score**
//! (an ordered scale) — and returns typed answers with probabilities, not
//! prose. They are fast (~100 ms) and cheap, which makes them useful for the
//! small judgments an agent makes constantly: is this request asking for an
//! action, which skill fits, does this tool call destroy data.
//!
//! "Noul" is deliberate: it is the SystemOne wire vocabulary (`"type":
//! "noul"`), shared by TypeSafe's API and self-hosted servers such as JevK5,
//! so the types read the same as the requests they produce.
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
//! let urgent = jev.noul("Help! My payouts have been failing for 3 days.",
//!                       "Does this convey urgency?").await?;
//! let team = jev.choice("My card was charged twice.",
//!                       "Which team should handle this?",
//!                       ["billing", "technical", "sales"]).await?;
//! println!("urgent: {:.2}, team: {} ({:.2})", urgent.p_true(), team.choice(), team.confidence());
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

mod advisory;
mod answer;
mod backend;
mod error;
mod gate;
mod mock;
mod question;
mod systemone;

pub use advisory::Advisory;
pub use answer::{
    distribution_confidence, Answer, ChoiceAnswer, DecisionUsage, Evaluation, NoulAnswer,
    ScoreAnswer,
};
pub use backend::{Capabilities, DecisionBackend};
pub use error::DecisionError;
pub use gate::ToolGate;
pub use mock::MockBackend;
pub use question::{Question, QuestionKind, Request};
pub use systemone::SystemOneBackend;

use crate::provider::CostConfig;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

/// The `prices.json` provider key for TypeSafe's models.
pub(crate) const TYPESAFE_PRICE_PROVIDER: &str = "typesafe";

/// Default overall timeout of one [`DecisionModel`] call, retries included.
pub(crate) const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

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
    /// TypeSafe's list prices from the resolved price table, by the reported
    /// model id — only while requests go to TypeSafe's own host.
    TypeSafeList,
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
    /// output is free) — while the base URL is TypeSafe's host; pointed
    /// elsewhere it is unpriced.
    pub fn jev() -> Self {
        Self {
            backend: Slot::SystemOne(SystemOneBackend::typesafe()),
            model: "jev-latest".into(),
            timeout: DEFAULT_TIMEOUT,
            pricing: Pricing::TypeSafeList,
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

    /// A self-hosted SystemOne-compatible server (JevK5, ...) at `base_url`:
    /// no key, [`local`](Capabilities::local) (the state stays with you),
    /// costs reported as $0. Model id `jev-latest` until
    /// [`with_model`](Self::with_model).
    pub fn local(base_url: impl Into<String>) -> Self {
        let backend = SystemOneBackend::new(base_url)
            .with_capabilities(Capabilities::new(QuestionKind::all()).with_local(true));
        Self {
            backend: Slot::SystemOne(backend),
            model: "jev-latest".into(),
            timeout: DEFAULT_TIMEOUT,
            pricing: Pricing::Fixed(CostConfig::new(0.0, 0.0)),
        }
    }

    /// Any backend — including a [`SystemOneBackend`] you configured —
    /// asking for `model`. Unpriced until [`with_cost`](Self::with_cost).
    pub fn from_backend(backend: impl DecisionBackend + 'static, model: impl Into<String>) -> Self {
        Self {
            backend: Slot::Custom(Arc::new(backend)),
            model: model.into(),
            timeout: DEFAULT_TIMEOUT,
            pricing: Pricing::Unpriced,
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
    /// variable. Only meaningful for the presets; ignored (with a warning)
    /// for [`from_backend`](Self::from_backend) models.
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

    /// Retry policy for rate limits and overload (presets only).
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

    /// The price of `usage` on the model that reported it.
    pub(crate) fn cost_usd(&self, model: &str, usage: &DecisionUsage) -> Option<f64> {
        let cost = match &self.pricing {
            Pricing::TypeSafeList => {
                let on_typesafe =
                    matches!(&self.backend, Slot::SystemOne(b) if b.is_typesafe_host());
                if !on_typesafe {
                    return None;
                }
                crate::provider::prices::global::resolved().cost(TYPESAFE_PRICE_PROVIDER, model)?
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

    /// Ask one yes/no question.
    pub async fn noul(
        &self,
        state: impl Into<Value>,
        instructions: impl Into<Value>,
    ) -> Result<NoulAnswer, DecisionError> {
        let eval = self.ask(state).noul("q", instructions).send().await?;
        eval.noul("q")
            .cloned()
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
    /// Validates the request against the backend's [`Capabilities`], applies
    /// the timeout, validates every answer (see [`DecisionBackend`]), and
    /// prices the result. Inside an agent run, the attempt and its spend are
    /// recorded in the run's [`SessionStats::decision`](crate::SessionStats::decision).
    pub async fn evaluate_request(&self, request: Request) -> Result<Evaluation, DecisionError> {
        let result = self.evaluate_unrecorded(request).await;
        crate::agent_loop::record_decision(|stats| match &result {
            Ok(eval) => stats.record_success(
                eval.usage.input_tokens,
                eval.usage.output_tokens,
                eval.cost_usd,
            ),
            Err(e) => stats.record_failure(matches!(e, DecisionError::Timeout(_))),
        });
        result
    }

    /// The cost of `eval` under this handle's pricing: an unpriced handle
    /// keeps whatever the backend reported; a priced one computes it from the
    /// reported usage, and is unpriced when the backend reported none.
    fn price(&self, eval: &Evaluation) -> Option<f64> {
        if self.pricing == Pricing::Unpriced {
            return eval.cost_usd;
        }
        if !eval.usage_reported {
            if !WARNED_NO_USAGE.swap(true, std::sync::atomic::Ordering::Relaxed) {
                tracing::warn!(
                    model = %eval.model,
                    "decision backend reported no usage; its evaluations are unpriced"
                );
            }
            return None;
        }
        self.cost_usd(&eval.model, &eval.usage)
    }

    async fn evaluate_unrecorded(&self, request: Request) -> Result<Evaluation, DecisionError> {
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
                let mut eval = backend.evaluate(&request).await?;
                check_complete(&request, &mut eval)?;
                Ok(eval)
            } else {
                let mut merged: Option<Evaluation> = None;
                for (id, q) in &request.questions {
                    let single = Request {
                        model: request.model.clone(),
                        state: request.state.clone(),
                        questions: vec![(id.clone(), q.clone())],
                    };
                    let step = match backend.evaluate(&single).await {
                        Ok(mut eval) => check_complete(&single, &mut eval).map(|()| eval),
                        Err(e) => Err(e),
                    };
                    let eval = match step {
                        Ok(eval) => eval,
                        Err(e) => {
                            // The questions already answered were billed.
                            if let Some(done) = &merged {
                                let cost = self.price(done);
                                crate::agent_loop::record_decision(|stats| {
                                    stats.record_billed(
                                        done.usage.input_tokens,
                                        done.usage.output_tokens,
                                        cost,
                                    )
                                });
                            }
                            return Err(e);
                        }
                    };
                    merged = Some(match merged {
                        None => eval,
                        Some(mut m) => {
                            m.usage.input_tokens += eval.usage.input_tokens;
                            m.usage.output_tokens += eval.usage.output_tokens;
                            m.usage_reported &= eval.usage_reported;
                            m.cost_usd = match (m.cost_usd, eval.cost_usd) {
                                (Some(a), Some(b)) => Some(a + b),
                                _ => None,
                            };
                            // `check_complete` left only the asked-for answer.
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
        eval.cost_usd = self.price(&eval);
        span.record("tokens_in", eval.usage.input_tokens);
        if let Some(c) = eval.cost_usd {
            span.record("cost_usd", c);
        }
        Ok(eval)
    }
}

/// Warned once per process that a backend reported no usage.
static WARNED_NO_USAGE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Every question must have a valid answer of its own type, and nothing
/// else survives: answers nobody asked for are dropped (so a backend cannot
/// smuggle one in to overwrite a validated answer when results are merged),
/// and the rest are put in request order. The one check every backend's
/// output passes through.
fn check_complete(request: &Request, eval: &mut Evaluation) -> Result<(), DecisionError> {
    let mut ordered = Vec::with_capacity(request.questions.len());
    for (id, q) in &request.questions {
        let Some(at) = eval.answers.iter().position(|(k, _)| k == id) else {
            return Err(DecisionError::BadResponse(format!("answers.{id}: missing")));
        };
        let (key, answer) = eval.answers.swap_remove(at);
        answer.validate(id, q)?;
        ordered.push((key, answer));
    }
    eval.answers = ordered;
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

type Hooks = Vec<Arc<dyn crate::TurnHook>>;
type Middleware = Vec<Arc<dyn crate::ToolMiddleware>>;

/// Add the decision integrations to a run's hooks and middleware: the
/// advisory hook when a model is set, and the tool gate — last in the chain,
/// so it judges the arguments every other middleware produced.
pub(crate) fn wire(
    advisory: Option<&Advisory>,
    gate: Option<&ToolGate>,
    skills: &crate::skills::SkillSet,
    mut hooks: Hooks,
    mut middleware: Middleware,
) -> (Hooks, Middleware) {
    if let Some(a) = advisory {
        hooks.push(Arc::new(advisory::Advisor::new(a.clone(), skills)));
    }
    if let Some(g) = gate {
        middleware.push(Arc::new(g.clone()));
    }
    (hooks, middleware)
}

#[cfg(test)]
mod pricing_tests {
    use super::*;

    #[test]
    fn jev_prices_by_reported_version_only_on_typesafe() {
        let usage = DecisionUsage::new(2_000_000, 500);
        let jev = DecisionModel {
            backend: Slot::SystemOne(
                SystemOneBackend::typesafe().with_base_url("https://api.typesafe.ai"),
            ),
            ..DecisionModel::jev()
        };
        let cost = jev.cost_usd("jev-1.13.0", &usage).unwrap();
        assert!((cost - 0.084).abs() < 1e-12, "{cost}");
        assert_eq!(jev.cost_usd("jev-latest", &usage), None, "aliases unpriced");
        assert_eq!(jev.cost_usd("jev-9.9.9", &usage), None, "unlisted unpriced");

        // The same pricing pointed at another host is unpriced.
        let proxied = DecisionModel {
            backend: Slot::SystemOne(
                SystemOneBackend::typesafe().with_base_url("https://proxy.example"),
            ),
            ..DecisionModel::jev()
        };
        assert_eq!(proxied.cost_usd("jev-1.13.0", &usage), None);

        assert_eq!(
            DecisionModel::jev_opencode().cost_usd("jev-1.13.0", &usage),
            None
        );
        assert_eq!(
            DecisionModel::local("http://localhost:1").cost_usd("x", &usage),
            Some(0.0)
        );
        let mock = DecisionModel::from_backend(MockBackend::neutral(), "m");
        assert_eq!(mock.cost_usd("m", &usage), None, "from_backend is unpriced");
        assert_eq!(
            mock.with_cost(Some(CostConfig::new(1.0, 0.0)))
                .cost_usd("m", &usage),
            Some(2.0)
        );
    }
}
