//! [`ToolGate`]: a [`ToolMiddleware`] that asks a decision model whether a
//! tool call is destructive and whether the user asked for it. Blocking, so
//! strictly opt-in, and fail-closed.

use super::advisory::truncate;
use super::question::Question;
use super::DecisionModel;
use crate::types::{ToolCallRequest, ToolDecision, ToolMiddleware};
use serde_json::{json, Value};
use std::time::Duration;

/// Most characters of the user's request sent as state.
const MAX_REQUEST_CHARS: usize = 8_000;
/// Most characters of a call's serialized arguments sent as state.
const MAX_ARGS_CHARS: usize = 8_000;

/// Default question: does the call destroy or irreversibly change something?
pub const DEFAULT_DESTRUCTIVE_QUESTION: &str = "Would executing `tool_call` delete, overwrite, or \
     irreversibly change data or external state — files, databases, remote systems, or messages \
     sent to other people?";

/// Default question: is the call what the user asked for?
pub const DEFAULT_REQUESTED_QUESTION: &str = "Is `tool_call` something the user asked for in \
     `user_request`, or a direct step toward what they asked for?";

/// A tool gate: per tool call, one decision request with two Nouls —
/// *destructive* ("does this call delete, overwrite or irreversibly change
/// data or external state?") and *requested* ("is this call what the user
/// asked for?") — plus any extra checks you add.
///
/// The call is **denied** when it looks destructive
/// (`p_destructive >= destructive_threshold`, default 0.5) **and** is not
/// clearly requested (`p_requested < requested_threshold`, default 0.7), or
/// when any extra check reaches its threshold. Everything else is allowed.
/// A denial reaches the model as an error tool result with the reason, so it
/// can ask the user instead.
///
/// **Fails closed.** If the decision model errors or does not answer within
/// the timeout (default 5 s), the call is denied with a reason saying so.
///
/// **Defence in depth, not a security boundary.** The state the gate
/// evaluates — the user's message and the call's arguments — can carry
/// injected text written to steer the decision model, and decision models do
/// not treat their state as hostile. Keep real sandboxing and permissions
/// underneath.
///
/// Install with [`Agent::with_tool_gate`](crate::Agent::with_tool_gate) (the
/// agent's decision model, these defaults), or configure one and pass it to
/// [`Agent::with_tool_gate_config`](crate::Agent::with_tool_gate_config).
/// It is a plain [`ToolMiddleware`], so [`SubAgentTool::with_tool_middleware`](crate::SubAgentTool::with_tool_middleware)
/// and raw loops take it too.
#[derive(Debug, Clone)]
pub struct ToolGate {
    model: DecisionModel,
    timeout: Duration,
    destructive_question: String,
    requested_question: String,
    destructive_threshold: f64,
    requested_threshold: f64,
    checks: Vec<(String, String, f64)>,
}

impl ToolGate {
    /// A gate with the default questions, thresholds and timeout.
    pub fn new(model: DecisionModel) -> Self {
        Self {
            model,
            timeout: Duration::from_secs(5),
            destructive_question: DEFAULT_DESTRUCTIVE_QUESTION.into(),
            requested_question: DEFAULT_REQUESTED_QUESTION.into(),
            destructive_threshold: 0.5,
            requested_threshold: 0.7,
            checks: Vec::new(),
        }
    }

    /// Deny when `p_destructive >= destructive` and `p_requested < requested`.
    pub fn with_thresholds(mut self, destructive: f64, requested: f64) -> Self {
        self.destructive_threshold = destructive;
        self.requested_threshold = requested;
        self
    }

    /// Replace the destructive question. The state names `tool_call` and
    /// `user_request`; refer to them in backticks.
    pub fn with_destructive_question(mut self, question: impl Into<String>) -> Self {
        self.destructive_question = question.into();
        self
    }

    /// Replace the requested question.
    pub fn with_requested_question(mut self, question: impl Into<String>) -> Self {
        self.requested_question = question.into();
        self
    }

    /// Add a Noul asked on every call; the call is denied when its answer is
    /// at least `deny_at_or_above`.
    pub fn with_check(
        mut self,
        id: impl Into<String>,
        question: impl Into<String>,
        deny_at_or_above: f64,
    ) -> Self {
        self.checks
            .push((id.into(), question.into(), deny_at_or_above));
        self
    }

    /// Time limit per call (default 5 s); on expiry the call is denied.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The state the gate evaluates for a call.
    fn state(call: &ToolCallRequest<'_>) -> Value {
        let args = serde_json::to_string(call.args).unwrap_or_default();
        let arguments = if args.chars().count() > MAX_ARGS_CHARS {
            Value::String(truncate(&args, MAX_ARGS_CHARS))
        } else {
            call.args.clone()
        };
        json!({
            "user_request": truncate(&call.latest_user_text().unwrap_or_default(), MAX_REQUEST_CHARS),
            "tool_call": { "tool": call.tool_name, "arguments": arguments },
        })
    }

    async fn decide(&self, call: &ToolCallRequest<'_>) -> ToolDecision {
        let mut questions = vec![
            (
                "destructive".to_string(),
                Question::noul_with_criteria(
                    self.destructive_question.as_str(),
                    "The call removes or replaces existing data, or changes something outside \
                     the conversation that cannot easily be undone.",
                    "The call only reads, lists or searches, or creates something new without \
                     replacing anything.",
                ),
            ),
            (
                "requested".to_string(),
                Question::noul(self.requested_question.as_str()),
            ),
        ];
        for (id, q, _) in &self.checks {
            questions.push((id.clone(), Question::noul(q.as_str())));
        }

        let result = tokio::time::timeout(
            self.timeout,
            self.model.evaluate(Self::state(call), questions),
        )
        .await;
        let eval = match result {
            Ok(Ok(eval)) => eval,
            Ok(Err(e)) => {
                tracing::warn!(tool = call.tool_name, "tool gate denied a call: {e}");
                return ToolDecision::Deny(format!(
                    "Tool gate: the decision model could not be consulted ({e}), so this call \
                     was not run. The gate fails closed; ask the user how to proceed."
                ));
            }
            Err(_) => {
                tracing::warn!(tool = call.tool_name, "tool gate denied a call: timed out");
                return ToolDecision::Deny(format!(
                    "Tool gate: the decision model did not answer within {:.1}s, so this call \
                     was not run. The gate fails closed; ask the user how to proceed.",
                    self.timeout.as_secs_f64()
                ));
            }
        };

        for (id, q, deny_at) in &self.checks {
            let p = eval.p_true(id).unwrap_or(1.0);
            if p >= *deny_at {
                return ToolDecision::Deny(format!(
                    "Tool gate: check `{id}` (\"{q}\") answered {p:.2}, at or above {deny_at:.2}. \
                     Ask the user to confirm before retrying."
                ));
            }
        }
        // A missing answer cannot happen past `evaluate`'s completeness
        // check; defaulting to the denying side keeps it closed regardless.
        let destructive = eval.p_true("destructive").unwrap_or(1.0);
        let requested = eval.p_true("requested").unwrap_or(0.0);
        if destructive >= self.destructive_threshold && requested < self.requested_threshold {
            return ToolDecision::Deny(format!(
                "Tool gate: this call looks destructive or irreversible (p={destructive:.2}) and \
                 not clearly what the user asked for (p={requested:.2}). Ask the user to \
                 confirm before retrying."
            ));
        }
        ToolDecision::Allow
    }
}

#[async_trait::async_trait]
impl ToolMiddleware for ToolGate {
    async fn before_tool(&self, call: &ToolCallRequest<'_>) -> ToolDecision {
        self.decide(call).await
    }
}

/// The gate an agent gets from `with_tool_gate()` without a decision model:
/// every call denied, saying why.
pub(crate) struct UnconfiguredGate;

#[async_trait::async_trait]
impl ToolMiddleware for UnconfiguredGate {
    async fn before_tool(&self, _call: &ToolCallRequest<'_>) -> ToolDecision {
        ToolDecision::Deny(
            "Tool gate: enabled, but no decision model is configured (call \
             with_decision_model), so no tool call can be approved. The gate fails closed."
                .into(),
        )
    }
}

/// How an agent's tool gate is configured.
#[derive(Debug, Clone)]
pub(crate) enum GateSetting {
    /// `with_tool_gate()`: defaults, on the agent's decision model.
    Default,
    /// `with_tool_gate_config(gate)`.
    Custom(Box<ToolGate>),
}
