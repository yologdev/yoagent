//! The [`DecisionBackend`] trait: what integrations depend on, never a vendor.

use super::answer::Evaluation;
use super::error::DecisionError;
use super::question::{QuestionKind, Request};

/// What a backend can answer. Checked client-side before a request is sent.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Capabilities {
    /// Question types the backend answers. Anything else is
    /// [`DecisionError::Unsupported`] — never emulated.
    pub question_kinds: Vec<QuestionKind>,
    /// Most options one Choice may have.
    pub max_choice_options: usize,
    /// Most levels one Score may have (at least 2 are always required).
    pub max_score_levels: usize,
    /// Whether one request may carry many questions. When `false`,
    /// [`DecisionModel`](super::DecisionModel) sends one request per question
    /// and merges the answers (summing usage).
    pub batching: bool,
    /// Self-hosted: the state does not go to a third party. Informational;
    /// it does not affect pricing.
    pub local: bool,
    /// Request token limit (state plus every question), when known.
    pub max_request_tokens: Option<usize>,
    /// Limit on the state plus the single longest question, when known.
    pub max_state_and_question_tokens: Option<usize>,
}

impl Capabilities {
    /// A backend answering `kinds`, batching, with SystemOne's option/level
    /// limits (255 / 10), not local, no token limits.
    pub fn new(kinds: &[QuestionKind]) -> Self {
        Self {
            question_kinds: kinds.to_vec(),
            max_choice_options: 255,
            max_score_levels: 10,
            batching: true,
            local: false,
            max_request_tokens: None,
            max_state_and_question_tokens: None,
        }
    }

    /// TypeSafe's hosted SystemOne API as documented for Jev 1.13: every
    /// question type, 255 options, 10 levels, batching, 64k tokens per
    /// request and 32k for the state plus the longest question.
    pub(crate) fn systemone() -> Self {
        Self::new(QuestionKind::all()).with_token_limits(Some(64_000), Some(32_000))
    }

    /// No limits at all: what every request must satisfy whatever the
    /// backend (the structural checks).
    pub(crate) fn unlimited() -> Self {
        Self::new(QuestionKind::all())
            .with_max_choice_options(usize::MAX)
            .with_max_score_levels(usize::MAX)
    }

    /// Most options one Choice may have.
    pub fn with_max_choice_options(mut self, n: usize) -> Self {
        self.max_choice_options = n;
        self
    }

    /// Most levels one Score may have.
    pub fn with_max_score_levels(mut self, n: usize) -> Self {
        self.max_score_levels = n;
        self
    }

    /// Whether one request may carry many questions.
    pub fn with_batching(mut self, batching: bool) -> Self {
        self.batching = batching;
        self
    }

    /// Whether the backend is self-hosted (the state stays with you).
    pub fn with_local(mut self, local: bool) -> Self {
        self.local = local;
        self
    }

    /// The request token limit and the limit on the state plus the longest
    /// question (`None` = unknown; not checked client-side).
    pub fn with_token_limits(
        mut self,
        request: Option<usize>,
        state_and_question: Option<usize>,
    ) -> Self {
        self.max_request_tokens = request;
        self.max_state_and_question_tokens = state_and_question;
        self
    }

    /// Whether `kind` is answered.
    pub fn supports(&self, kind: QuestionKind) -> bool {
        self.question_kinds.contains(&kind)
    }
}

/// A decision-model backend: evaluates a [`Request`] into an [`Evaluation`].
///
/// Built in: [`SystemOneBackend`](super::SystemOneBackend) (HTTP, TypeSafe's
/// API and compatible servers) and [`MockBackend`](super::MockBackend)
/// (tests). Implement it to plug in anything else; wrap it with
/// [`DecisionModel::from_backend`](super::DecisionModel::from_backend).
///
/// [`DecisionModel`](super::DecisionModel) validates each request against
/// [`capabilities`](Self::capabilities) before calling
/// [`evaluate`](Self::evaluate), and validates every answer afterwards —
/// present, of the question's type, probabilities and confidences finite and
/// in `[0, 1]`, choices among the options, one probability per Score level —
/// so a backend need not repeat either check. Answers nobody asked for are
/// dropped.
///
/// **Cost.** A handle with its own pricing (a preset, or
/// [`with_cost`](super::DecisionModel::with_cost)) computes the cost from
/// the usage you report, replacing any you set; an unpriced handle
/// (`from_backend` without `with_cost`) keeps the cost you set with
/// [`Evaluation::with_cost_usd`]. Report usage whenever you know it: a
/// priced handle treats an evaluation without reported usage as unpriced.
///
/// Report your own failures with
/// [`DecisionError::backend`] / [`backend_with_source`](DecisionError::backend_with_source).
#[async_trait::async_trait]
pub trait DecisionBackend: Send + Sync {
    /// What this backend can answer.
    fn capabilities(&self) -> Capabilities;

    /// Evaluate one request.
    async fn evaluate(&self, request: &Request) -> Result<Evaluation, DecisionError>;
}
