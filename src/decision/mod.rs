//! Decision models: typed questions in, calibrated probabilities out.
//!
//! A decision model evaluates a `state` (text or JSON) against typed
//! questions — **Noul** (yes/no), **Choice** (one of N options), **Score**
//! (an ordered scale) — and returns typed answers with probabilities, not
//! prose. They are fast (a few hundred ms per request: medians of 230–444 ms
//! for Jev and Clef in our measurements) and cheap, which makes them useful
//! for the small judgments an agent makes constantly: is this request asking
//! for an action, which skill fits, does this tool call destroy data.
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
//! self-hosted JevK5), [`LogprobBackend`] turns any OpenAI-compatible server
//! that returns logprobs into a decision model
//! ([`DecisionModel::logprobs`]), [`MockBackend`] scripts answers for tests,
//! and any other backend plugs in through [`DecisionModel::from_backend`].
//! Chain fallbacks with [`DecisionModel::or`]; measure a model on your own
//! labelled examples with [`calibrate()`].
//!
//! ```no_run
//! # async fn demo() {
//! use yoagent::decision::{calibrate, CalibrationExample, DecisionModel};
//!
//! // Hosted Jev, falling back to a local llama-server.
//! let model = DecisionModel::jev()
//!     .or(DecisionModel::logprobs("http://localhost:8080", "llama-3.1-8b-instruct"));
//! let q = "Does this ask to delete data?";
//! let report = calibrate(&model, vec![
//!     CalibrationExample::noul("rm -rf build/", q, true),
//!     CalibrationExample::noul("list src/", q, false),
//! ]).await;
//! println!("{report}");
//! # }
//! ```
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
//! (`Agent::with_decision_model`, `Agent::with_tool_gate`,
//! `Agent::with_input_guard`).

mod advisory;
mod answer;
mod backend;
mod calibrate;
mod error;
mod gate;
mod guard;
mod logprobs;
mod mock;
mod question;
mod systemone;

pub use advisory::Advisory;
pub use answer::{
    distribution_confidence, Answer, ChoiceAnswer, DecisionUsage, Evaluation, NoulAnswer,
    ScoreAnswer,
};
pub use backend::{Capabilities, DecisionBackend};
pub use calibrate::{
    calibrate, calibrate_with, CalibrationError, CalibrationExample, CalibrationOptions,
    CalibrationReport, Expected, ReliabilityBin, ThresholdPoint,
};
pub use error::{DecisionError, FallbackAttempt};
pub use gate::ToolGate;
pub use guard::InputGuard;
pub use logprobs::LogprobBackend;
pub use mock::MockBackend;
pub use question::{Question, QuestionKind, Request};
pub use systemone::{parse_systemone_response, SystemOneBackend};

use crate::provider::CostConfig;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

/// The `prices.json` provider key for TypeSafe's models.
pub(crate) const TYPESAFE_PRICE_PROVIDER: &str = "typesafe";

/// The `prices.json` provider key for Cloudflare Workers AI's models.
pub(crate) const CLOUDFLARE_PRICE_PROVIDER: &str = "cloudflare";

/// Default overall timeout of one [`DecisionModel`] call, retries included.
pub(crate) const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
enum Slot {
    SystemOne(SystemOneBackend),
    Logprobs(LogprobBackend),
    Custom(Arc<dyn DecisionBackend>),
}

impl Slot {
    fn get(&self) -> &dyn DecisionBackend {
        match self {
            Slot::SystemOne(b) => b,
            Slot::Logprobs(b) => b,
            Slot::Custom(b) => b.as_ref(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Pricing {
    /// TypeSafe's list prices from the resolved price table, by the reported
    /// model id — only while requests go to TypeSafe's own host.
    TypeSafeList,
    /// Workers AI's list prices from the resolved price table, by the model
    /// id (`clef`, `clef-flash`) — only while requests go to Cloudflare.
    CloudflareList,
    Fixed(CostConfig),
    Unpriced,
}

/// A decision model: a backend plus the model id to ask for. The handle
/// everything else takes.
///
/// Cheap to clone (the backend is shared). Choose one with a preset —
/// [`jev`](Self::jev), [`jev_opencode`](Self::jev_opencode),
/// [`jev_opencode_free`](Self::jev_opencode_free),
/// [`clef`](Self::clef), [`clef_flash`](Self::clef_flash), [`local`](Self::local),
/// [`logprobs`](Self::logprobs) — or [`from_backend`](Self::from_backend);
/// everything else has defaults. Chain fallbacks with [`or`](Self::or).
#[derive(Clone)]
pub struct DecisionModel {
    backend: Slot,
    model: String,
    /// One call's overall budget — the whole fallback chain's.
    timeout: Duration,
    pricing: Pricing,
    /// This member's own limit per attempt, inside the overall budget.
    attempt_timeout: Option<Duration>,
    /// Tried in order after this model fails (flat: members have none).
    fallbacks: Vec<DecisionModel>,
}

impl std::fmt::Debug for DecisionModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut d = f.debug_struct("DecisionModel");
        d.field("model", &self.model)
            .field("timeout", &self.timeout)
            .field("pricing", &self.pricing);
        if let Some(t) = self.attempt_timeout {
            d.field("attempt_timeout", &t);
        }
        match &self.backend {
            Slot::SystemOne(b) => d.field("backend", b),
            Slot::Logprobs(b) => d.field("backend", b),
            Slot::Custom(_) => d.field("backend", &"custom"),
        };
        if !self.fallbacks.is_empty() {
            d.field("fallbacks", &self.fallbacks);
        }
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
            attempt_timeout: None,
            fallbacks: Vec::new(),
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
            attempt_timeout: None,
            fallbacks: Vec::new(),
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

    /// Cloudflare's Clef (`@cf/cloudflare/clef`, 27B) on Workers AI, called
    /// through the REST API on Cloudflare account `account_id`. The API token
    /// (Workers AI Read and Edit) is read at call time from
    /// `CLOUDFLARE_API_TOKEN`, then `CLOUDFLARE_AUTH_TOKEN`, or set with
    /// [`with_api_key`](Self::with_api_key) — on wasm32 there is no process
    /// environment. Priced from the resolved price table ($0.24 per million
    /// input tokens by default) by the model id the response reports; that is
    /// the list price, while Cloudflare bills in neurons with a daily free
    /// allocation.
    ///
    /// Keep the preset's model id: Clef's input schema accepts only `clef`
    /// and `clef-flash`, each at its own URL, and
    /// [`with_model`](Self::with_model) changes the id but not the URL. For
    /// Clef Flash use [`clef_flash`](Self::clef_flash).
    ///
    /// ```no_run
    /// # async fn demo() -> Result<(), yoagent::decision::DecisionError> {
    /// use yoagent::decision::DecisionModel;
    /// let model = DecisionModel::clef("your-account-id");
    /// let urgent = model.noul("Checkout fails for every customer.", "Is this urgent?").await?;
    /// # Ok(()) }
    /// ```
    ///
    /// Inside a Cloudflare Worker, the `yoagent-workers` crate runs Clef
    /// through the Worker's AI binding instead, with no token.
    pub fn clef(account_id: impl AsRef<str>) -> Self {
        Self::workers_ai(account_id, "clef")
    }

    /// Cloudflare's Clef Flash (`@cf/cloudflare/clef-flash`, 9B, faster) on
    /// Workers AI; otherwise as [`clef`](Self::clef). $0.09 per million
    /// input tokens.
    pub fn clef_flash(account_id: impl AsRef<str>) -> Self {
        Self::workers_ai(account_id, "clef-flash")
    }

    fn workers_ai(account_id: impl AsRef<str>, model: &str) -> Self {
        let backend = SystemOneBackend::workers_ai(account_id, format!("@cf/cloudflare/{model}"));
        Self {
            backend: Slot::SystemOne(backend),
            model: model.into(),
            timeout: DEFAULT_TIMEOUT,
            pricing: Pricing::CloudflareList,
            attempt_timeout: None,
            fallbacks: Vec::new(),
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
            attempt_timeout: None,
            fallbacks: Vec::new(),
        }
    }

    /// Any backend — including a [`SystemOneBackend`] you configured —
    /// asking for `model`. Unpriced until [`with_cost`](Self::with_cost).
    pub fn from_backend(backend: impl DecisionBackend + 'static, model: impl Into<String>) -> Self {
        Self::from_arc(Arc::new(backend), model)
    }

    /// [`from_backend`](Self::from_backend) for a backend you already hold
    /// behind an `Arc` (shared with other handles, or chosen at runtime as
    /// `Arc<dyn DecisionBackend>`). Unpriced until
    /// [`with_cost`](Self::with_cost).
    pub fn from_arc(backend: Arc<dyn DecisionBackend>, model: impl Into<String>) -> Self {
        Self {
            backend: Slot::Custom(backend),
            model: model.into(),
            timeout: DEFAULT_TIMEOUT,
            pricing: Pricing::Unpriced,
            attempt_timeout: None,
            fallbacks: Vec::new(),
        }
    }

    /// Any OpenAI-compatible `/chat/completions` server that returns
    /// logprobs — llama.cpp's `llama-server`, vLLM, SGLang, LM Studio, or a
    /// hosted API — asking for `model`. Every question becomes a one-token
    /// completion whose label probabilities are read from `top_logprobs`
    /// (see [`LogprobBackend`]).
    ///
    /// ```no_run
    /// # async fn demo() -> Result<(), yoagent::decision::DecisionError> {
    /// use yoagent::decision::DecisionModel;
    /// // llama-server on this machine: local, $0, no key.
    /// // A non-thinking instruct model: the answer must be the first token.
    /// let model = DecisionModel::logprobs("http://localhost:8080", "llama-3.1-8b-instruct");
    /// let urgent = model.noul("Help! Payouts failing for 3 days.", "Is this urgent?").await?;
    /// # Ok(()) }
    /// ```
    ///
    /// - **Thinking must be off**: a reasoning model's first token is
    ///   `<think>`, and such responses are rejected as `BadResponse`. See
    ///   [`LogprobBackend::with_thinking_disabled`] and
    ///   [`from_logprob_backend`](Self::from_logprob_backend).
    /// - **No key** unless [`with_api_key`](Self::with_api_key); no
    ///   environment variable is read.
    /// - **Local and $0** when the host is loopback (`localhost`,
    ///   `127.0.0.0/8`, `::1`); any other host is not local and **unpriced**
    ///   until [`with_cost`](Self::with_cost). Usage is the sum of each
    ///   completion's `prompt_tokens` / `completion_tokens`.
    /// - **Calibration is approximate** compared with a trained decision
    ///   model: measure it with [`calibrate`](crate::decision::calibrate()) and
    ///   correct it with [`LogprobBackend::with_temperature`].
    /// - Choice is limited to 20 options (OpenAI's `top_logprobs` cap);
    ///   [`LogprobBackend::with_max_choice_options`] raises it to 26 for
    ///   servers that allow more.
    pub fn logprobs(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self::from_logprob_backend(LogprobBackend::new(base_url), model)
    }

    /// A [`logprobs`](Self::logprobs) model over a [`LogprobBackend`] you
    /// configured — temperature, Choice limit, `top_logprobs`, minimum label
    /// mass, thinking off. Keeps the logprobs conveniences:
    /// [`with_api_key`](Self::with_api_key) and
    /// [`with_retry`](Self::with_retry) apply, and a loopback host is $0.
    ///
    /// ```
    /// use yoagent::decision::{DecisionModel, LogprobBackend};
    /// let backend = LogprobBackend::new("http://localhost:8080")
    ///     .with_thinking_disabled()
    ///     .with_temperature(1.4); // e.g. from `calibrate`
    /// let model = DecisionModel::from_logprob_backend(backend, "qwen3-8b");
    /// ```
    pub fn from_logprob_backend(backend: LogprobBackend, model: impl Into<String>) -> Self {
        let pricing = if backend.is_local() {
            Pricing::Fixed(CostConfig::new(0.0, 0.0))
        } else {
            Pricing::Unpriced
        };
        Self {
            backend: Slot::Logprobs(backend),
            model: model.into(),
            timeout: DEFAULT_TIMEOUT,
            pricing,
            attempt_timeout: None,
            fallbacks: Vec::new(),
        }
    }

    /// Ask for this model id or alias — pin a version such as `jev-1.13.0`
    /// once you have tuned thresholds against it.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// Overall time limit for one call, retries included (default 30 s).
    /// With fallbacks ([`or`](Self::or)) it is **one budget for the whole
    /// chain**: a fallback gets only what is left.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Limit each attempt of this model to `timeout`, inside the overall
    /// [`with_timeout`](Self::with_timeout) budget — so in a fallback chain
    /// a slow primary (or one retrying a rate limit) leaves time for the
    /// fallback. When an attempt hits its own limit the chain moves on; when
    /// the overall budget runs out the call fails with
    /// [`DecisionError::Timeout`]. Default: none (an attempt may use the rest
    /// of the budget).
    ///
    /// ```
    /// # use std::time::Duration;
    /// use yoagent::decision::DecisionModel;
    /// let model = DecisionModel::jev()
    ///     .with_attempt_timeout(Duration::from_secs(2))
    ///     .or(DecisionModel::local("http://localhost:8000"));
    /// ```
    pub fn with_attempt_timeout(mut self, timeout: Duration) -> Self {
        self.attempt_timeout = Some(timeout);
        self
    }

    /// Fall back to `fallback` when this model fails. Chainable —
    /// `a.or(b).or(c)` tries `a`, then `b`, then `c` — and `fallback`'s own
    /// fallbacks are appended after it.
    ///
    /// ```
    /// use yoagent::decision::DecisionModel;
    /// // Hosted Jev; the local server when TypeSafe is down, rate limited,
    /// // or TYPESAFE_API_KEY is unset.
    /// let model = DecisionModel::jev().or(DecisionModel::local("http://localhost:8000"));
    /// ```
    ///
    /// - **Each member asks for its own model id** (the one it was built
    ///   with), not the primary's.
    /// - **Falls back on every error a member returns** — outages, rate
    ///   limits, missing keys, unusable answers, an unsupported question
    ///   kind, a member's own timeout, and a member's `Invalid` too (a 422 is
    ///   that server's own limit; another may accept the request).
    /// - **Each member is validated against its own capabilities** before it
    ///   is tried. A request structurally invalid everywhere (no questions, a
    ///   one-option Choice, ...) fails with `Invalid` at once, nothing sent;
    ///   one that only exceeds a member's limits (too many options or levels,
    ///   a question kind it lacks, its token limits) skips that member
    ///   without sending anything.
    /// - **One overall timeout** — this handle's
    ///   [`with_timeout`](Self::with_timeout) — covers the whole chain; the
    ///   members' own `with_timeout` is ignored, their
    ///   [`with_attempt_timeout`](Self::with_attempt_timeout) is not. The tool
    ///   gate's (5 s), the input guard's (3 s) and the advisory's (2 s) time
    ///   limits replace it the same way, so the gate and the guard stay
    ///   fail-closed within their budgets.
    /// - **When every member fails**, the error is
    ///   [`DecisionError::AllFailed`], listing each member as a
    ///   [`FallbackAttempt`] in order — or [`DecisionError::Timeout`] when
    ///   the overall budget ran out.
    /// - [`Evaluation::model`] names the member that answered; each member
    ///   prices its own answers. Every attempt sent is recorded in
    ///   [`SessionStats::decision`](crate::SessionStats::decision) (a primary
    ///   failure and a fallback success are two requests, one failure);
    ///   skipped members are not.
    /// - [`model`](Self::model), [`capabilities`](Self::capabilities) and
    ///   the builders (`with_api_key`, `with_retry`, `with_cost`, ...) are the
    ///   **primary's**: configure each member before chaining it.
    pub fn or(mut self, fallback: DecisionModel) -> Self {
        let mut fallback = fallback;
        let rest = std::mem::take(&mut fallback.fallbacks);
        self.fallbacks.push(fallback);
        self.fallbacks.extend(rest);
        self
    }

    /// The model ids of the fallbacks, in the order they are tried.
    pub fn fallback_models(&self) -> impl Iterator<Item = &str> + '_ {
        self.fallbacks.iter().map(|f| f.model.as_str())
    }

    /// Send this API key instead of reading the preset's environment
    /// variable (for [`logprobs`](Self::logprobs): send one at all). Only
    /// meaningful for the presets; ignored (with a warning) for
    /// [`from_backend`](Self::from_backend) models.
    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        match self.backend {
            Slot::SystemOne(b) => self.backend = Slot::SystemOne(b.with_api_key(key)),
            Slot::Logprobs(b) => self.backend = Slot::Logprobs(b.with_api_key(key)),
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
            Slot::Logprobs(b) => self.backend = Slot::Logprobs(b.with_retry(retry)),
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

    /// The model id or alias requests ask for — the primary's, when there
    /// are fallbacks ([`or`](Self::or)).
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The configured overall timeout.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// What the backend can answer — the primary's, when there are
    /// fallbacks ([`or`](Self::or)).
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
            Pricing::CloudflareList => {
                let on_cloudflare =
                    matches!(&self.backend, Slot::SystemOne(b) if b.is_cloudflare_host());
                if !on_cloudflare {
                    return None;
                }
                // The model may report itself by its catalog path.
                let id = model.strip_prefix("@cf/cloudflare/").unwrap_or(model);
                let cost =
                    crate::provider::prices::global::resolved().cost(CLOUDFLARE_PRICE_PROVIDER, id);
                if cost.is_none() && warn_once(format!("unlisted:{CLOUDFLARE_PRICE_PROVIDER}/{id}"))
                {
                    tracing::warn!(
                        model = %id,
                        "no price for {CLOUDFLARE_PRICE_PROVIDER}/{id} in the price table; \
                         its evaluations are unpriced"
                    );
                }
                cost?
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
    ///
    /// With fallbacks ([`or`](Self::or)), each member in turn — see `or` for
    /// the rules.
    pub async fn evaluate_request(&self, request: Request) -> Result<Evaluation, DecisionError> {
        if !self.fallbacks.is_empty() {
            return self.evaluate_chain(request).await;
        }
        let result = self
            .evaluate_unrecorded(request, self.attempt_limit(self.timeout))
            .await;
        record_attempt(&result);
        result
    }

    /// The time one attempt may take when `remaining` is left.
    fn attempt_limit(&self, remaining: Duration) -> Duration {
        self.attempt_timeout.map_or(remaining, |t| t.min(remaining))
    }

    /// Try the primary, then each fallback, within one overall budget.
    async fn evaluate_chain(&self, request: Request) -> Result<Evaluation, DecisionError> {
        // Invalid for every backend: fail now, like a single model would.
        if let Err(e) = request.validate(&Capabilities::unlimited()) {
            let result = Err(e);
            record_attempt(&result);
            return result;
        }
        let deadline = crate::rt::Instant::now() + self.timeout;
        let mut attempts: Vec<FallbackAttempt> = Vec::new();
        let mut sent = false;
        let members = std::iter::once(self).chain(self.fallbacks.iter());
        for (i, member) in members.enumerate() {
            let mut req = request.clone();
            if i > 0 {
                req.model = member.model.clone();
            }
            if let Err(e) = req.validate(&member.backend.get().capabilities()) {
                tracing::debug!(model = %req.model, "decision fallback: skipped a model: {e}");
                attempts.push(FallbackAttempt::new(req.model, e, false));
                continue;
            }
            let now = crate::rt::Instant::now();
            if now >= deadline {
                let result = Err(DecisionError::Timeout(self.timeout));
                if !sent {
                    record_attempt(&result);
                }
                return result;
            }
            let limit = member.attempt_limit(deadline - now);
            let model = req.model.clone();
            let result = member.evaluate_unrecorded(req, limit).await;
            sent = true;
            record_attempt(&result);
            match result {
                Ok(eval) => return Ok(eval),
                // The clock, not the error kind, says whether the overall
                // budget is spent: a backend may report a timeout of its own.
                Err(_) if crate::rt::Instant::now() >= deadline => {
                    return Err(DecisionError::Timeout(self.timeout))
                }
                // Every other error falls through — `Invalid` included: a
                // 422 is that server's limit, not necessarily the next one's.
                Err(e) => {
                    tracing::warn!(model = %model, "decision model failed; trying its fallback: {e}");
                    attempts.push(FallbackAttempt::new(model, e, true));
                }
            }
        }
        let result = Err(DecisionError::AllFailed { attempts });
        if !sent {
            record_attempt(&result);
        }
        result
    }

    /// The cost of `eval` under this handle's pricing: an unpriced handle
    /// keeps whatever the backend reported; a priced one computes it from the
    /// reported usage, and is unpriced when the backend reported none.
    fn price(&self, eval: &Evaluation) -> Option<f64> {
        match &self.pricing {
            Pricing::Unpriced => return eval.cost_usd,
            // Free (e.g. `local()`): $0 whatever the usage — missing usage
            // cannot make a free model's cost unknown.
            Pricing::Fixed(c) if is_free(c) => return Some(0.0),
            _ => {}
        }
        if !eval.usage_reported {
            if warn_once(format!("no-usage:{}", eval.model)) {
                tracing::warn!(
                    model = %eval.model,
                    "decision backend reported no usage; its evaluations are unpriced"
                );
            }
            return None;
        }
        self.cost_usd(&eval.model, &eval.usage)
    }

    /// A non-batching backend: one request per question, up to `concurrency`
    /// in flight, answers in request order. Every completed request adds its
    /// usage to `billed` (even if its answer is then rejected). After the
    /// first failure no new request starts, but those in flight are awaited,
    /// so their spend is recorded too; the error of the earliest failed
    /// question is returned.
    async fn evaluate_singles(
        &self,
        backend: &dyn DecisionBackend,
        request: &Request,
        concurrency: usize,
        billed: &std::sync::Mutex<Option<Evaluation>>,
    ) -> Result<Vec<Evaluation>, DecisionError> {
        use futures::StreamExt;
        // `Send` where work can move between threads; wasm32 has one thread
        // and its host futures are not `Send`.
        #[cfg(not(target_arch = "wasm32"))]
        type Single<'a> = std::pin::Pin<
            Box<
                dyn std::future::Future<Output = (usize, Result<Evaluation, DecisionError>)>
                    + Send
                    + 'a,
            >,
        >;
        #[cfg(target_arch = "wasm32")]
        type Single<'a> = std::pin::Pin<
            Box<dyn std::future::Future<Output = (usize, Result<Evaluation, DecisionError>)> + 'a>,
        >;
        // Built in a loop, not a closure: a closure over borrowed questions
        // trips the `Send` check of `async_trait`-boxed callers.
        let mut pending: Vec<Single<'_>> = Vec::with_capacity(request.questions.len());
        for (i, (id, q)) in request.questions.iter().enumerate() {
            pending.push(Box::pin(async move {
                let single = Request {
                    model: request.model.clone(),
                    state: request.state.clone(),
                    questions: vec![(id.clone(), q.clone())],
                };
                let result = match backend.evaluate(&single).await {
                    Ok(mut eval) => {
                        add_billed(billed, &eval);
                        check_complete(&single, &mut eval).map(|()| eval)
                    }
                    Err(e) => Err(e),
                };
                (i, result)
            }));
        }
        let mut queue = pending.into_iter();
        let mut running = futures::stream::FuturesUnordered::new();
        for next in queue.by_ref().take(concurrency.max(1)) {
            running.push(next);
        }
        let mut done: Vec<Option<Evaluation>> = request.questions.iter().map(|_| None).collect();
        let mut failed: Option<(usize, DecisionError)> = None;
        while let Some((i, result)) = running.next().await {
            match result {
                Ok(eval) => done[i] = Some(eval),
                Err(e) => {
                    let earlier = match &failed {
                        Some((j, _)) => i < *j,
                        None => true,
                    };
                    if earlier {
                        failed = Some((i, e));
                    }
                }
            }
            if failed.is_none() {
                if let Some(next) = queue.next() {
                    running.push(next);
                }
            }
        }
        if let Some((_, e)) = failed {
            return Err(e);
        }
        Ok(done.into_iter().flatten().collect())
    }

    /// One attempt on this model's own backend, within `limit`.
    async fn evaluate_unrecorded(
        &self,
        request: Request,
        limit: Duration,
    ) -> Result<Evaluation, DecisionError> {
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
        // What a non-batching request has already been billed for, kept
        // outside the future so a timeout (which drops the future) or a later
        // failure still records it.
        let billed: std::sync::Mutex<Option<Evaluation>> = std::sync::Mutex::new(None);
        let work = async {
            if caps.batching || request.questions.len() == 1 {
                let mut eval = backend.evaluate(&request).await?;
                check_complete(&request, &mut eval)?;
                Ok(eval)
            } else {
                let answers = self
                    .evaluate_singles(backend, &request, caps.max_concurrent_requests, &billed)
                    .await?;
                let mut merged: Option<Evaluation> = None;
                for eval in answers {
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
            crate::rt::timeout(limit, work.instrument(span.clone())).await
        };
        let result = match result {
            Ok(inner) => inner,
            Err(_) => Err(DecisionError::Timeout(limit)),
        };
        if result.is_err() {
            // A failed or timed-out non-batching request: record what the
            // questions answered so far were billed.
            let done = billed.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some(done) = done {
                let cost = self.price(&done);
                crate::agent_loop::record_decision(|stats| {
                    stats.record_billed(done.usage.input_tokens, done.usage.output_tokens, cost)
                });
            }
        }
        let mut eval = result?;
        eval.cost_usd = self.price(&eval);
        span.record("tokens_in", eval.usage.input_tokens);
        if let Some(c) = eval.cost_usd {
            span.record("cost_usd", c);
        }
        Ok(eval)
    }
}

/// Add what `eval` was billed to the running total of a split request.
fn add_billed(billed: &std::sync::Mutex<Option<Evaluation>>, eval: &Evaluation) {
    let mut total = billed.lock().unwrap_or_else(|e| e.into_inner());
    let step = Evaluation {
        answers: Vec::new(),
        ..eval.clone()
    };
    *total = Some(match total.take() {
        None => step,
        Some(mut t) => {
            t.usage.input_tokens += step.usage.input_tokens;
            t.usage.output_tokens += step.usage.output_tokens;
            t.usage_reported &= step.usage_reported;
            t.cost_usd = match (t.cost_usd, step.cost_usd) {
                (Some(a), Some(b)) => Some(a + b),
                _ => None,
            };
            t
        }
    });
}

/// Record one attempt's outcome in the enclosing run's decision stats.
fn record_attempt(result: &Result<Evaluation, DecisionError>) {
    crate::agent_loop::record_decision(|stats| match result {
        Ok(eval) => stats.record_success(
            eval.usage.input_tokens,
            eval.usage.output_tokens,
            eval.cost_usd,
        ),
        Err(e) => stats.record_failure(matches!(e, DecisionError::Timeout(_))),
    });
}

/// Whether every rate is zero (a model priced as free).
fn is_free(c: &CostConfig) -> bool {
    c.input_per_million == 0.0
        && c.output_per_million == 0.0
        && c.cache_read_per_million == 0.0
        && c.cache_write_per_million == 0.0
        && c.context_tiers.iter().all(|t| {
            t.input_per_million == 0.0
                && t.output_per_million == 0.0
                && t.cache_read_per_million == 0.0
                && t.cache_write_per_million == 0.0
        })
}

/// Whether `key` is new to this process: each warning that would repeat on
/// every evaluation (no usage, an unlisted price) is logged once per model,
/// and each deprecated use (`deprecated:` keys) once per process.
pub(crate) fn warn_once(key: String) -> bool {
    static WARNED: std::sync::Mutex<std::collections::BTreeSet<String>> =
        std::sync::Mutex::new(std::collections::BTreeSet::new());
    WARNED.lock().unwrap_or_else(|e| e.into_inner()).insert(key)
}

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

/// The decision features an agent enabled, as extensions (#241): the
/// advisor (if advisory is enabled), then the gate. They are appended after
/// the agent's own extensions, so the gate judges the final, possibly
/// modified call, after every middleware and extension; a fail-closed gate
/// can only deny, never allow what an earlier hook denied.
pub(crate) fn wire(
    advisory: Option<&Advisory>,
    gate: Option<&ToolGate>,
    skills: &crate::skills::SkillSet,
) -> Vec<Arc<dyn crate::Extension>> {
    let mut extensions: Vec<Arc<dyn crate::Extension>> = Vec::new();
    if let Some(a) = advisory {
        extensions.push(Arc::new(advisory::AdvisorExtension::new(
            advisory::Advisor::new(a.clone(), skills),
        )));
    }
    if let Some(g) = gate {
        extensions.push(Arc::new(g.clone()));
    }
    extensions
}

#[cfg(test)]
mod pricing_tests {
    use super::*;

    #[test]
    fn clef_prices_by_model_only_on_cloudflare() {
        let usage = DecisionUsage::new(2_000_000, 500);
        let clef = DecisionModel::clef("acct");
        let cost = clef.cost_usd("clef", &usage).unwrap();
        assert!((cost - 0.48).abs() < 1e-12, "{cost}");
        // Reported by its catalog path: the same price.
        let cost = clef.cost_usd("@cf/cloudflare/clef", &usage).unwrap();
        assert!((cost - 0.48).abs() < 1e-12, "{cost}");
        let flash = DecisionModel::clef_flash("acct");
        let cost = flash.cost_usd("clef-flash", &usage).unwrap();
        assert!((cost - 0.18).abs() < 1e-12, "{cost}");
        assert_eq!(clef.cost_usd("clef-9", &usage), None, "unlisted unpriced");

        // Any other host is unpriced: its bill is not Cloudflare's list price.
        let proxied = DecisionModel {
            backend: Slot::SystemOne(
                SystemOneBackend::workers_ai("acct", "x")
                    .with_endpoint_url("https://proxy.example/ai/run/@cf/cloudflare/clef"),
            ),
            ..DecisionModel::clef("acct")
        };
        assert_eq!(proxied.cost_usd("clef", &usage), None);
    }

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

    #[test]
    fn logprobs_are_free_only_on_loopback() {
        let usage = DecisionUsage::new(2_000_000, 500);
        for local in [
            "http://localhost:8080",
            "http://127.0.0.1:1/v1",
            "http://[::1]:9",
        ] {
            let m = DecisionModel::logprobs(local, "m");
            assert!(m.capabilities().local, "{local}");
            assert_eq!(m.cost_usd("m", &usage), Some(0.0), "{local}");
        }
        for remote in [
            "https://api.openai.com/v1",
            "http://10.1.2.3:8000",
            "http://gpu-box:8080",
        ] {
            let m = DecisionModel::logprobs(remote, "m");
            assert!(!m.capabilities().local, "{remote}");
            assert_eq!(m.cost_usd("m", &usage), None, "{remote}: unpriced");
            assert_eq!(
                m.with_cost(Some(CostConfig::new(1.0, 0.0)))
                    .cost_usd("m", &usage),
                Some(2.0)
            );
        }
    }
}
