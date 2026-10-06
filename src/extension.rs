//! Extensions: one plug-in contract for the agent lifecycle (#241).
//!
//! An [`Extension`] packages a feature (a budget, a policy, redaction, an
//! audit log, a verifier) as one object that hooks into the loop at every
//! point it needs. yoagent defines the contract and calls it; implementations
//! live outside the crate. Loading code at runtime, discovery, manifests and
//! sandboxes belong to hosts, which end up calling
//! [`Agent::with_extension`](crate::Agent::with_extension).
//!
//! Each run gets fresh hooks: [`Extension::start_run`] returns a
//! [`RunHooks`] for that run alone, so state for one run (a run's spend, a
//! verifier's attempts) is isolated between concurrent runs, between the runs
//! of one session, and between agents sharing one `Arc<dyn Extension>`. State
//! that should span runs lives in the `Extension` itself, behind its own
//! synchronization.
//!
//! # Lifecycle
//!
//! | Hook | When | Several extensions |
//! | --- | --- | --- |
//! | [`RunHooks::tools`] | Once per run, at its start | Static tools win, then the earlier extension; sorted by name |
//! | [`RunHooks::on_input`] | On a prompted run's input, after the input filters | First `Reject` wins |
//! | [`RunHooks::before_model`] | Before each model request (not repeated for a retried attempt) | Notes appended in order; first `Stop` / `Fail` ends the run |
//! | [`RunHooks::before_tool`] | Before each tool call, after the [`ToolMiddleware`] chain | `Deny` wins, `Modify` feeds the next |
//! | [`RunHooks::after_tool`] | After each call that ran (errors and panics included), before truncation and `ToolExecutionEnd` | In order, each sees the previous edit |
//! | [`RunHooks::on_stop`] | When the model ends with `StopReason::Stop` and nothing is queued | First `Fail` wins, else first `Continue` |
//! | [`Extension::on_event`] | For every `AgentEvent` of the run | All, in order |
//! | [`RunHooks::finish`] | When the run ends, however it ends | All |
//!
//! # Failures
//!
//! A failure is a panic, an `Err`, a `Fail` decision, or `start_run` failing.
//! [`ExtensionMode`] decides what it means:
//!
//! - **Advisory** (default): logged, and that hook is skipped.
//! - **Required**: the run fails. Its last message is an assistant message
//!   with `StopReason::Error` and an `error_message` starting with
//!   [`EXTENSION_FAILED_PREFIX`].
//!
//! Hooks that guard a single call fail closed in both modes: a failing
//! `on_input` rejects the input, a failing `before_tool` denies the call, and
//! a failing `after_tool` replaces the result with an error naming the
//! extension (a redactor that fails must not leak).
//!
//! # What a hook cannot take back
//!
//! Streamed model text reaches consumers as it arrives: no hook can revoke
//! it, and `on_stop` runs after the final answer has streamed. The run's
//! outcome is `AgentEnd`. Partial tool output (`ToolExecutionUpdate`,
//! `ProgressMessage`) is sent while a tool runs, before `after_tool`; while
//! any installed extension returns `true` from
//! [`Extension::filters_tool_output`], the loop withholds it, so only the
//! filtered result is sent. Tool arguments and the model's own text are not
//! filtered.
//!
//! # Sub-agents
//!
//! [`Agent::with_tree_extension`](crate::Agent::with_tree_extension) installs
//! an extension on every run of the delegation tree, at any depth, ahead of
//! the child's own extensions; a child cannot remove it. Its `tools` are not
//! offered to child runs: tools are delegated explicitly. Ordinary extensions
//! ([`Agent::with_extension`](crate::Agent::with_extension)) apply only to
//! the agent they are installed on.

use crate::provider::ToolDefinition;
use crate::rt::{MaybeSend, MaybeSync};
use crate::types::*;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Prefix of the `error_message` a run ends with when a required extension
/// fails: `[Extension failed: <name>] <reason>`.
pub const EXTENSION_FAILED_PREFIX: &str = "[Extension failed:";

/// Prefix of the user message an [`StopDecision::Continue`] appends:
/// `[Extension <name>] <message>`. Recognized as loop-injected
/// ([`is_loop_injected`]), so it is never taken for
/// the user's own request.
pub const EXTENSION_MESSAGE_PREFIX: &str = "[Extension ";

/// How many times per run [`RunHooks::on_stop`] may continue the run, unless
/// [`AgentLoopConfig::max_stop_continues`](crate::agent_loop::AgentLoopConfig::max_stop_continues)
/// says otherwise.
pub const DEFAULT_MAX_STOP_CONTINUES: usize = 3;

/// What a failure inside an extension means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExtensionMode {
    /// A failure is logged and the hook is skipped.
    #[default]
    Advisory,
    /// A failure fails the run.
    Required,
}

/// An extension's error.
#[derive(Debug, Clone)]
pub struct ExtensionError(String);

impl ExtensionError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl std::fmt::Display for ExtensionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ExtensionError {}

impl From<String> for ExtensionError {
    fn from(message: String) -> Self {
        Self(message)
    }
}

impl From<&str> for ExtensionError {
    fn from(message: &str) -> Self {
        Self(message.to_string())
    }
}

/// The run a hook belongs to.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct RunContext<'a> {
    /// Unique per run, generated by the loop.
    pub run_id: &'a str,
    /// The host's label for the run, set with
    /// [`Agent::with_run_label`](crate::Agent::with_run_label) (yo puts its
    /// session id here). yoagent does not interpret it.
    pub label: Option<&'a str>,
    /// The run's prompts (for a delegated run: its task). Empty for a
    /// `continue_loop` run.
    pub prompts: &'a [Message],
    /// 0 for a top-level run, 1 for a sub-agent's run, and so on.
    pub depth: usize,
    /// The run's cancellation token.
    pub cancel: &'a CancellationToken,
}

impl<'a> RunContext<'a> {
    /// A context for a top-level run with no label, for testing extensions.
    pub fn new(run_id: &'a str, prompts: &'a [Message], cancel: &'a CancellationToken) -> Self {
        Self {
            run_id,
            label: None,
            prompts,
            depth: 0,
            cancel,
        }
    }

    pub fn with_label(mut self, label: &'a str) -> Self {
        self.label = Some(label);
        self
    }

    pub fn with_depth(mut self, depth: usize) -> Self {
        self.depth = depth;
        self
    }

    /// Whether this run is a delegation (a sub-agent's run).
    pub fn is_delegation(&self) -> bool {
        self.depth > 0
    }
}

/// A prompted run's input, as [`RunHooks::on_input`] sees it.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct InputContext<'a> {
    /// Every user text block of the prompts, joined by newlines (what the
    /// input filters see).
    pub text: &'a str,
    /// The prompts, after the input filters' warnings were appended.
    pub prompts: &'a [AgentMessage],
}

impl<'a> InputContext<'a> {
    pub fn new(text: &'a str, prompts: &'a [AgentMessage]) -> Self {
        Self { text, prompts }
    }
}

/// [`RunHooks::on_input`]'s verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum InputDecision {
    Pass,
    /// Reject the input: the run ends with `AgentEvent::InputRejected`.
    Reject(String),
}

/// [`RunHooks::before_model`]'s verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TurnDecision {
    Continue,
    /// Append this note to the request's latest user turn (not stored).
    Note(String),
    /// End the run before this request, like an execution limit: a stop
    /// marker (`[Agent stopped: <reason>]`), partial success.
    Stop(String),
    /// Fail the run (a required extension) or skip the hook (advisory).
    Fail(String),
}

/// The model's final answer, as [`RunHooks::on_stop`] sees it.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct StopContext<'a> {
    /// The answer: the last assistant message.
    pub answer: &'a Message,
    /// The conversation so far.
    pub messages: &'a [AgentMessage],
    /// How many times extensions have continued this run already.
    pub continues: usize,
}

impl<'a> StopContext<'a> {
    pub fn new(answer: &'a Message, messages: &'a [AgentMessage], continues: usize) -> Self {
        Self {
            answer,
            messages,
            continues,
        }
    }
}

/// [`RunHooks::on_stop`]'s verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum StopDecision {
    Accept,
    /// Keep going: append this as a user message (prefixed with
    /// [`EXTENSION_MESSAGE_PREFIX`]) and run another turn. Capped per run.
    Continue(String),
    /// The answer is not acceptable.
    Fail(String),
}

/// How a run ended, for [`RunHooks::finish`].
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct RunOutcome {
    /// The stop reason of the run's last assistant message, if it has one.
    pub stop_reason: Option<StopReason>,
    /// The input was rejected before the run started.
    pub rejected: bool,
    /// The run was cancelled.
    pub cancelled: bool,
    /// The `error_message` of a failed run's last assistant message.
    pub error: Option<String>,
}

/// One plug-in for the agent lifecycle. See the [module docs](self).
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
pub trait Extension: MaybeSend + MaybeSync {
    /// Names the extension in logs and in failure messages.
    fn name(&self) -> &str;

    /// What a failure means. Read once per run.
    fn mode(&self) -> ExtensionMode {
        ExtensionMode::Advisory
    }

    /// Whether this extension filters tool output (redaction). While any
    /// installed extension returns `true`, partial tool output is withheld.
    fn filters_tool_output(&self) -> bool {
        false
    }

    /// Whether this extension's `before_tool` must judge a call again when a
    /// later extension rewrote its arguments (a policy). The second verdict
    /// sees the final arguments, and a `Deny` there wins.
    fn rechecks_modified_calls(&self) -> bool {
        false
    }

    /// The hooks for one run. Called at the start of every run.
    async fn start_run(&self, run: &RunContext<'_>) -> Result<Box<dyn RunHooks>, ExtensionError>;

    /// Observe an event of a run. Called for every `AgentEvent`, in order,
    /// before the event reaches the run's consumer. Must not block.
    fn on_event(&self, _run_id: &str, _event: &AgentEvent) {}
}

/// The hooks of one run. Every method has a no-op default.
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
pub trait RunHooks: MaybeSend + MaybeSync {
    /// Tools to offer for this run. Called once, at the start of the run.
    async fn tools(&mut self, _run: &RunContext<'_>) -> Vec<Arc<dyn AgentTool>> {
        Vec::new()
    }

    /// Judge a prompted run's input. Not called by `continue_loop`.
    async fn on_input(&mut self, _input: &InputContext<'_>) -> InputDecision {
        InputDecision::Pass
    }

    /// Before each model request.
    async fn before_model(&mut self, _turn: &TurnContext<'_>) -> TurnDecision {
        TurnDecision::Continue
    }

    /// Before a tool call runs.
    async fn before_tool(&mut self, _call: &ToolCallRequest<'_>) -> ToolDecision {
        ToolDecision::Allow
    }

    /// After a tool call ran; may edit its result.
    async fn after_tool(
        &mut self,
        _call: &ToolCallRequest<'_>,
        _result: &mut ToolResult,
    ) -> Result<(), ExtensionError> {
        Ok(())
    }

    /// When the model ends its answer.
    async fn on_stop(&mut self, _stop: &StopContext<'_>) -> StopDecision {
        StopDecision::Accept
    }

    /// When the run ends. Not called if the run's future is dropped, so
    /// correctness must not depend on it.
    async fn finish(&mut self, _outcome: &RunOutcome) {}
}

/// An extension with no state for a run: every run gets a clone of `hooks`.
pub struct Stateless<H> {
    name: String,
    mode: ExtensionMode,
    hooks: H,
}

impl<H: RunHooks + Clone + 'static> Stateless<H> {
    pub fn new(name: impl Into<String>, hooks: H) -> Self {
        Self {
            name: name.into(),
            mode: ExtensionMode::Advisory,
            hooks,
        }
    }

    pub fn required(mut self) -> Self {
        self.mode = ExtensionMode::Required;
        self
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl<H: RunHooks + Clone + 'static> Extension for Stateless<H> {
    fn name(&self) -> &str {
        &self.name
    }

    fn mode(&self) -> ExtensionMode {
        self.mode
    }

    async fn start_run(&self, _run: &RunContext<'_>) -> Result<Box<dyn RunHooks>, ExtensionError> {
        Ok(Box::new(self.hooks.clone()))
    }
}

// ---------------------------------------------------------------------------
// Dispatch (crate-private)
// ---------------------------------------------------------------------------

/// A required extension's failure: the run ends with it.
#[derive(Debug, Clone)]
pub(crate) struct Failure {
    pub(crate) name: String,
    pub(crate) reason: String,
}

impl Failure {
    /// The failed run's `error_message`.
    pub(crate) fn message(&self) -> String {
        format!("{EXTENSION_FAILED_PREFIX} {}] {}", self.name, self.reason)
    }
}

struct Active {
    ext: Arc<dyn Extension>,
    name: String,
    mode: ExtensionMode,
    rechecks: bool,
    /// Installed by a parent (a tree extension): its tools are not offered.
    inherited: bool,
    hooks: tokio::sync::Mutex<Box<dyn RunHooks>>,
}

/// The extensions of one run, started, in dispatch order.
pub(crate) struct ActiveExtensions {
    run_id: String,
    active: Vec<Active>,
    filters_output: bool,
    /// A required extension's failure that could not end the run where it
    /// happened (in `after_tool`, `on_event`); checked at the next boundary.
    failure: std::sync::Mutex<Option<Failure>>,
}

/// Await `fut`, containing a panic as `Err(payload text)`.
async fn guarded<T>(fut: impl std::future::Future<Output = T>) -> Result<T, String> {
    use futures::FutureExt;
    std::panic::AssertUnwindSafe(fut)
        .catch_unwind()
        .await
        .map_err(|payload| format!("panicked: {}", crate::tool_source::panic_message(&*payload)))
}

/// What `before_model` decided across the extensions.
pub(crate) enum ModelGate {
    Proceed(Vec<String>),
    Stop(String),
    Fail(Failure),
}

/// What `on_stop` decided across the extensions.
pub(crate) enum StopGate {
    Accept,
    Continue { name: String, message: String },
    Fail(Failure),
}

impl ActiveExtensions {
    /// No extensions.
    pub(crate) fn none() -> Self {
        Self {
            run_id: String::new(),
            active: Vec::new(),
            filters_output: false,
            failure: std::sync::Mutex::new(None),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.active.is_empty()
    }

    pub(crate) fn run_id(&self) -> &str {
        &self.run_id
    }

    /// Start every extension for a run: `inherited` first (from a parent's
    /// tree), then `own`.
    pub(crate) async fn start(
        inherited: &[Arc<dyn Extension>],
        own: &[Arc<dyn Extension>],
        run: &RunContext<'_>,
    ) -> Result<Self, Failure> {
        let mut active = Vec::new();
        let all = inherited
            .iter()
            .map(|e| (e, true))
            .chain(own.iter().map(|e| (e, false)));
        for (ext, inherited) in all {
            let name = ext.name().to_string();
            let mode = ext.mode();
            match guarded(async { ext.start_run(run).await }).await {
                Ok(Ok(hooks)) => active.push(Active {
                    ext: ext.clone(),
                    name,
                    mode,
                    rechecks: ext.rechecks_modified_calls(),
                    inherited,
                    hooks: tokio::sync::Mutex::new(hooks),
                }),
                Ok(Err(e)) => Self::sit_out(&name, mode, e.to_string())?,
                Err(panic) => Self::sit_out(&name, mode, panic)?,
            }
        }
        let filters_output = active.iter().any(|a| a.ext.filters_tool_output());
        Ok(Self {
            run_id: run.run_id.to_string(),
            active,
            filters_output,
            failure: std::sync::Mutex::new(None),
        })
    }

    /// A `start_run` failure: an advisory extension sits the run out, a
    /// required one fails it.
    fn sit_out(name: &str, mode: ExtensionMode, reason: String) -> Result<(), Failure> {
        match mode {
            ExtensionMode::Required => Err(Failure {
                name: name.to_string(),
                reason: format!("could not start: {reason}"),
            }),
            ExtensionMode::Advisory => {
                tracing::warn!(
                    extension = name,
                    "extension could not start; it sits this run out: {reason}"
                );
                Ok(())
            }
        }
    }

    /// Record a hook failure: a required extension's fails the run (at the
    /// next boundary), an advisory one's is logged.
    fn failed(&self, a: &Active, hook: &str, reason: String) {
        tracing::warn!(extension = %a.name, hook, "extension hook failed: {reason}");
        if a.mode == ExtensionMode::Required {
            let mut slot = self.failure.lock().unwrap_or_else(|e| e.into_inner());
            slot.get_or_insert(Failure {
                name: a.name.clone(),
                reason: format!("{hook} failed: {reason}"),
            });
        }
    }

    /// The failure recorded since the last check, if any.
    pub(crate) fn take_failure(&self) -> Option<Failure> {
        self.failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    pub(crate) fn filters_tool_output(&self) -> bool {
        self.filters_output
    }

    /// Every extension's tools for the run (not inherited ones').
    pub(crate) async fn tools(&self, run: &RunContext<'_>) -> Vec<Arc<dyn AgentTool>> {
        let mut tools = Vec::new();
        for a in self.active.iter().filter(|a| !a.inherited) {
            let mut hooks = a.hooks.lock().await;
            match guarded(hooks.tools(run)).await {
                Ok(t) => tools.extend(t),
                Err(panic) => self.failed(a, "tools", panic),
            }
        }
        tools
    }

    /// `Err(reason)` if an extension rejects the input. A failing hook
    /// rejects (fail closed).
    pub(crate) async fn on_input(&self, input: &InputContext<'_>) -> Result<(), String> {
        for a in &self.active {
            let mut hooks = a.hooks.lock().await;
            match guarded(hooks.on_input(input)).await {
                Ok(InputDecision::Pass) => {}
                Ok(InputDecision::Reject(reason)) => return Err(reason),
                Err(panic) => {
                    tracing::warn!(extension = %a.name, "on_input failed; rejecting the input: {panic}");
                    return Err(format!("extension '{}' failed to check the input", a.name));
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn before_model(&self, turn: &TurnContext<'_>) -> ModelGate {
        let mut notes = Vec::new();
        for a in &self.active {
            let mut hooks = a.hooks.lock().await;
            let decision = match guarded(hooks.before_model(turn)).await {
                Ok(d) => d,
                Err(panic) => TurnDecision::Fail(panic),
            };
            match decision {
                TurnDecision::Continue => {}
                TurnDecision::Note(note) => {
                    if !note.trim().is_empty() {
                        notes.push(note);
                    }
                }
                TurnDecision::Stop(reason) => return ModelGate::Stop(reason),
                TurnDecision::Fail(reason) => match a.mode {
                    ExtensionMode::Required => {
                        return ModelGate::Fail(Failure {
                            name: a.name.clone(),
                            reason: format!("before_model failed: {reason}"),
                        })
                    }
                    ExtensionMode::Advisory => {
                        tracing::warn!(extension = %a.name, "before_model failed: {reason}")
                    }
                },
            }
        }
        ModelGate::Proceed(notes)
    }

    /// The final arguments, or `Err(reason)` if the call is denied. A failing
    /// hook denies (fail closed).
    pub(crate) async fn before_tool(
        &self,
        call: ToolCallRequest<'_>,
    ) -> Result<serde_json::Value, String> {
        let mut args = call.args.clone();
        // Index of the last extension that rewrote the arguments.
        let mut last_modifier: Option<usize> = None;
        for (i, a) in self.active.iter().enumerate() {
            let request = ToolCallRequest {
                args: &args,
                ..call
            };
            let decision = {
                let mut hooks = a.hooks.lock().await;
                guarded(hooks.before_tool(&request)).await
            };
            match decision {
                Ok(ToolDecision::Allow) => {}
                Ok(ToolDecision::Modify(new_args)) => {
                    args = new_args;
                    last_modifier = Some(i);
                }
                Ok(ToolDecision::Deny(reason)) => return Err(reason),
                Err(panic) => {
                    tracing::warn!(extension = %a.name, "before_tool failed; denying the call: {panic}");
                    return Err(format!("extension '{}' failed to check the call", a.name));
                }
            }
        }
        // A policy judged arguments a later extension then rewrote: judge the
        // final ones again.
        if let Some(last) = last_modifier {
            for a in self.active[..last].iter().filter(|a| a.rechecks) {
                let request = ToolCallRequest {
                    args: &args,
                    ..call
                };
                let decision = {
                    let mut hooks = a.hooks.lock().await;
                    guarded(hooks.before_tool(&request)).await
                };
                match decision {
                    Ok(ToolDecision::Deny(reason)) => return Err(reason),
                    Ok(_) => {}
                    Err(panic) => {
                        tracing::warn!(extension = %a.name, "before_tool failed; denying the call: {panic}");
                        return Err(format!("extension '{}' failed to check the call", a.name));
                    }
                }
            }
        }
        Ok(args)
    }

    /// Run `after_tool` over a result. A failing hook replaces the result
    /// with an error naming the extension (fail closed) and, for a required
    /// extension, fails the run.
    pub(crate) async fn after_tool(
        &self,
        call: &ToolCallRequest<'_>,
        result: &mut ToolResult,
        is_error: &mut bool,
    ) {
        for a in &self.active {
            let outcome = {
                let mut hooks = a.hooks.lock().await;
                guarded(hooks.after_tool(call, result)).await
            };
            let reason = match outcome {
                Ok(Ok(())) => continue,
                Ok(Err(e)) => e.to_string(),
                Err(panic) => panic,
            };
            *result = ToolResult {
                content: vec![Content::Text {
                    text: format!(
                        "Tool result withheld: extension '{}' failed to process it.",
                        a.name
                    ),
                }],
                details: serde_json::Value::Null,
            };
            *is_error = true;
            self.failed(a, "after_tool", reason);
        }
    }

    pub(crate) async fn on_stop(&self, stop: &StopContext<'_>) -> StopGate {
        let mut verdict = StopGate::Accept;
        for a in &self.active {
            let decision = {
                let mut hooks = a.hooks.lock().await;
                guarded(hooks.on_stop(stop)).await
            };
            match decision {
                Ok(StopDecision::Accept) => {}
                Ok(StopDecision::Continue(message)) => {
                    if matches!(verdict, StopGate::Accept) {
                        verdict = StopGate::Continue {
                            name: a.name.clone(),
                            message,
                        };
                    }
                }
                Ok(StopDecision::Fail(reason)) | Err(reason) => match a.mode {
                    ExtensionMode::Required => {
                        return StopGate::Fail(Failure {
                            name: a.name.clone(),
                            reason,
                        })
                    }
                    ExtensionMode::Advisory => {
                        tracing::warn!(extension = %a.name, "on_stop failed: {reason}")
                    }
                },
            }
        }
        verdict
    }

    /// The required extensions whose `on_stop` continued the run: when the
    /// continue cap is reached, the run fails if any of them asked.
    pub(crate) fn is_required(&self, name: &str) -> bool {
        self.active
            .iter()
            .any(|a| a.name == name && a.mode == ExtensionMode::Required)
    }

    pub(crate) fn on_event(&self, event: &AgentEvent) {
        for a in &self.active {
            use std::panic::{catch_unwind, AssertUnwindSafe};
            if let Err(payload) =
                catch_unwind(AssertUnwindSafe(|| a.ext.on_event(&self.run_id, event)))
            {
                let why = crate::tool_source::panic_message(&*payload);
                self.failed(a, "on_event", format!("panicked: {why}"));
            }
        }
    }

    pub(crate) async fn finish(&self, outcome: &RunOutcome) {
        for a in &self.active {
            let mut hooks = a.hooks.lock().await;
            if let Err(panic) = guarded(hooks.finish(outcome)).await {
                tracing::warn!(extension = %a.name, "finish failed: {panic}");
            }
        }
    }
}

/// The tool definitions a turn offers (for [`TurnContext`]).
pub(crate) fn tool_definitions(tools: &[Box<dyn AgentTool>]) -> Vec<ToolDefinition> {
    tools
        .iter()
        .map(|t| ToolDefinition {
            name: t.name().to_string(),
            description: t.description().to_string(),
            parameters: t.parameters_schema(),
        })
        .collect()
}
