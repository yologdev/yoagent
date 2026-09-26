//! [`ToolGate`]: a [`ToolMiddleware`] that asks a decision model whether a
//! tool call is destructive and whether the user asked for it. Blocking, so
//! strictly opt-in, and fail-closed.

use super::advisory::{assert_threshold, truncate_middle};
use super::question::Question;
use super::DecisionModel;
use crate::types::{ToolCallRequest, ToolDecision, ToolMiddleware};
use serde_json::{json, Value};
use std::time::Duration;

/// Most characters of the user's request sent as state (head and tail kept).
const MAX_REQUEST_CHARS: usize = 8_000;
/// A string argument longer than this is shortened to head and tail.
const MAX_STRING_CHARS: usize = 2_000;
/// Most characters of the arguments, serialized, after shortening. Larger
/// calls are denied: the gate will not approve what it could not read.
const MAX_ARGS_CHARS: usize = 12_000;

const DESTRUCTIVE_ID: &str = "destructive";
const REQUESTED_ID: &str = "requested";

const DEFAULT_DESTRUCTIVE_QUESTION: &str = "Would executing `tool_call` delete, overwrite, or \
     irreversibly change data or external state — files, databases, remote systems, or messages \
     sent to other people?";

const DEFAULT_REQUESTED_QUESTION: &str = "Is `tool_call` something the user asked for in \
     `user_request`, or a direct step toward what they asked for?";

/// A tool gate: per tool call, one decision request with two Nouls —
/// *destructive* ("Would executing `tool_call` delete, overwrite, or
/// irreversibly change data or external state — files, databases, remote
/// systems, or messages sent to other people?") and *requested* ("Is
/// `tool_call` something the user asked for in `user_request`, or a direct
/// step toward what they asked for?") — plus any checks you add.
///
/// The call is **denied** when it looks destructive
/// (`p_destructive >= 0.5`) **and** is not clearly requested
/// (`p_requested < 0.7`), or when any added check reaches its threshold.
/// Everything else is allowed. A denial reaches the model as an error tool
/// result with the reason, so it can ask the user instead.
///
/// The state is `{"user_request": .., "tool_call": {"tool": .., "arguments": ..}}`.
/// `user_request` is [`ToolCallRequest::user_request`]: the user's latest
/// message after the most recent compaction boundary — with, when it is a
/// short reply to an assistant question, that question and the earlier
/// request, so a confirmation ("yes, go ahead") carries what it confirms —
/// else the run's own prompts. When none of these exists (compaction left
/// no user message and the run has no prompt), the call is denied and the
/// reason asks for the request to be restated. Small argument
/// values are sent in full; a string over 2,000 characters is shortened to
/// its head and tail around an explicit `[truncated N chars]` marker, and a
/// call whose arguments are still over 12,000 characters is denied.
///
/// **Fails closed.** A decision-model error or timeout (default 5 s), a
/// missing or malformed answer, and a non-finite probability all deny.
///
/// **Scope.**
/// - Install it **last**: middleware after it could modify the arguments
///   after they were approved. [`Agent::with_tool_gate`](crate::Agent::with_tool_gate)
///   does this for you.
/// - It gates the agent it is installed on. Calls made *inside* a
///   [`SubAgentTool`](crate::SubAgentTool) are not covered by the parent's
///   gate; give the sub-agent its own (`SubAgentTool::with_tool_gate`) —
///   and there `user_request` is the task text the parent model wrote, not
///   the human's words.
///
/// **Defence in depth, not a security boundary.** The user's message and
/// the call's arguments can carry text written to steer the decision model,
/// which does not treat its state as hostile. Keep real sandboxing and
/// permissions underneath.
///
/// **This widens in the fail-open direction.** Including the assistant's
/// question and the earlier request in `user_request` makes more calls
/// count as requested — and the assistant's text can itself be steered by
/// injected content (a tool result that makes the model *ask* "Shall I
/// delete everything?" turns a user's "yes" into apparent consent).
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

    /// A call counts as destructive from this probability (default 0.5).
    /// Panics outside `[0, 1]` (NaN included).
    pub fn with_destructive_threshold(mut self, p: f64) -> Self {
        assert_threshold("destructive threshold", p);
        self.destructive_threshold = p;
        self
    }

    /// A call counts as requested from this probability (default 0.7).
    /// Panics outside `[0, 1]` (NaN included).
    pub fn with_requested_threshold(mut self, p: f64) -> Self {
        assert_threshold("requested threshold", p);
        self.requested_threshold = p;
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
    ///
    /// Panics on an empty id, an id already used (including the built-in
    /// `destructive` and `requested`), or a threshold outside `[0, 1]`.
    pub fn with_check(
        mut self,
        id: impl Into<String>,
        question: impl Into<String>,
        deny_at_or_above: f64,
    ) -> Self {
        let id = id.into();
        assert!(
            !id.trim().is_empty(),
            "a tool gate check id must not be empty"
        );
        assert!(
            id != DESTRUCTIVE_ID && id != REQUESTED_ID,
            "tool gate check id {id:?} collides with a built-in question"
        );
        assert!(
            self.checks.iter().all(|(existing, _, _)| *existing != id),
            "tool gate check id {id:?} is used twice"
        );
        assert_threshold("check threshold", deny_at_or_above);
        self.checks.push((id, question.into(), deny_at_or_above));
        self
    }

    /// Time limit per call (default 5 s); on expiry the call is denied.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    async fn decide(&self, call: &ToolCallRequest<'_>) -> ToolDecision {
        let arguments = shorten(call.args);
        let size = serde_json::to_string(&arguments).map_or(usize::MAX, |s| s.chars().count());
        if size > MAX_ARGS_CHARS {
            tracing::warn!(
                tool = call.tool_name,
                size,
                "tool gate denied a call: arguments too large"
            );
            return ToolDecision::Deny(format!(
                "Tool gate: this call's arguments are too large to evaluate ({size} characters \
                 after shortening long strings; the limit is {MAX_ARGS_CHARS}), so it was not run. \
                 The gate fails closed; split the work into smaller calls or ask the user."
            ));
        }
        let Some(user_request) = call.user_request() else {
            tracing::warn!(
                tool = call.tool_name,
                "tool gate denied a call: no user request to judge against"
            );
            return ToolDecision::Deny(
                "Tool gate: the conversation no longer shows what the user asked for (history \
                 was compacted and this run has no prompt), so this call was not run. The gate \
                 fails closed; ask the user to restate the request."
                    .into(),
            );
        };
        let state = json!({
            "user_request": truncate_middle(&user_request, MAX_REQUEST_CHARS),
            "tool_call": { "tool": call.tool_name, "arguments": arguments },
        });

        let mut questions = vec![
            (
                DESTRUCTIVE_ID.to_string(),
                Question::noul_with_criteria(
                    self.destructive_question.as_str(),
                    "The call removes or replaces existing data, or changes something outside \
                     the conversation that cannot easily be undone.",
                    "The call only reads, lists or searches, or creates something new without \
                     replacing anything.",
                ),
            ),
            (
                REQUESTED_ID.to_string(),
                Question::noul(self.requested_question.as_str()),
            ),
        ];
        for (id, q, _) in &self.checks {
            questions.push((id.clone(), Question::noul(q.as_str())));
        }

        let model = self.model.clone().with_timeout(self.timeout);
        let eval = match model.evaluate(state, questions).await {
            Ok(eval) => eval,
            Err(e) => {
                tracing::warn!(tool = call.tool_name, "tool gate denied a call: {e}");
                return ToolDecision::Deny(format!(
                    "Tool gate: the decision model could not be consulted ({e}), so this call \
                     was not run. The gate fails closed; ask the user how to proceed."
                ));
            }
        };

        // Every comparison below denies on a missing or non-finite answer.
        // `evaluate` already rejects both; this keeps the gate closed even if
        // that check ever regresses.
        let p = |id: &str| eval.p_true(id).filter(|p| p.is_finite());
        for (id, q, deny_at) in &self.checks {
            let denied = match p(id) {
                Some(v) => at_or_above(v, *deny_at),
                None => true,
            };
            if denied {
                return ToolDecision::Deny(format!(
                    "Tool gate: check `{id}` (\"{q}\") answered {}, at or above {deny_at:.2}. \
                     Ask the user to confirm before retrying.",
                    fmt_p(p(id))
                ));
            }
        }
        let (destructive, requested) = (p(DESTRUCTIVE_ID), p(REQUESTED_ID));
        let looks_destructive = match destructive {
            Some(d) => at_or_above(d, self.destructive_threshold),
            None => true,
        };
        let clearly_requested = match requested {
            Some(r) => r >= self.requested_threshold,
            None => false,
        };
        if destructive.is_none() || requested.is_none() || (looks_destructive && !clearly_requested)
        {
            return ToolDecision::Deny(format!(
                "Tool gate: this call looks destructive or irreversible (p={}) and not clearly \
                 what the user asked for (p={}). Ask the user to confirm before retrying.",
                fmt_p(destructive),
                fmt_p(requested)
            ));
        }
        ToolDecision::Allow
    }
}

/// `v >= t`, except that an unordered comparison (NaN) counts as reaching the
/// threshold — the denying side.
fn at_or_above(v: f64, t: f64) -> bool {
    !matches!(v.partial_cmp(&t), Some(std::cmp::Ordering::Less))
}

fn fmt_p(p: Option<f64>) -> String {
    p.map_or_else(|| "unknown".into(), |p| format!("{p:.2}"))
}

/// The arguments as the gate shows them: every value kept, except strings
/// longer than `MAX_STRING_CHARS`, which keep their head and tail around an
/// explicit `[truncated N chars]` marker — so a command's last line, and
/// every short field such as a path, stays visible.
fn shorten(v: &Value) -> Value {
    match v {
        Value::String(s) if s.chars().count() > MAX_STRING_CHARS => {
            Value::String(truncate_middle(s, MAX_STRING_CHARS))
        }
        Value::Array(items) => Value::Array(items.iter().map(shorten).collect()),
        Value::Object(map) => {
            Value::Object(map.iter().map(|(k, v)| (k.clone(), shorten(v))).collect())
        }
        other => other.clone(),
    }
}

#[async_trait::async_trait]
impl ToolMiddleware for ToolGate {
    async fn before_tool(&self, call: &ToolCallRequest<'_>) -> ToolDecision {
        self.decide(call).await
    }
}
