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
//! of one session, and between agents sharing one extension (an
//! `Arc<dyn Extension>` is an extension too). State that should span runs
//! lives in the `Extension` itself, behind its own synchronization.
//!
//! # Lifecycle
//!
//! | Hook | When | Several extensions |
//! | --- | --- | --- |
//! | [`RunHooks::tools`] | Once per run, at its start | Static tools win, then the earlier extension; sorted by name |
//! | [`RunHooks::on_input`] | On a prompted run's input, after the input filters | First `Reject` wins |
//! | [`RunHooks::before_model`] | Before each model request (not repeated for a retried attempt) | Notes appended in order; the first `Stop`, or a required extension's `Fail`, ends the run |
//! | [`RunHooks::before_tool`] | Before each tool call, after the [`ToolMiddleware`] chain | `Deny` wins, `Modify` feeds the next |
//! | [`RunHooks::after_tool`] | After each call that ran (errors and panics included), before truncation and `ToolExecutionEnd` | In order, each sees the previous edit |
//! | [`RunHooks::on_stop`] | When the model ends with `StopReason::Stop` and nothing is queued | A required extension's `Fail` ends the run; otherwise every `Continue` is sent, one line each |
//! | [`RunHooks::on_event`] | For every `AgentEvent` of the run | All, in order |
//! | [`RunHooks::finish`] | When the run ends, however it ends | All |
//!
//! `tools`, `on_input`, `before_model`, `on_stop` and `finish` take
//! `&mut self` and run one at a time. `before_tool`, `after_tool` and
//! `on_event` take `&self`: the calls of one response may be judged
//! concurrently, so state they change needs interior mutability.
//!
//! Every hook watches the run's cancellation: a hook still awaiting when the
//! run is cancelled is abandoned (the run then ends cancelled before any
//! model request; a pending `before_tool` denies, a pending `after_tool`
//! withholds the result). `finish` gets [`FINISH_TIMEOUT`].
//!
//! `on_event` is called from the run's event observer, which waits for any
//! `&mut self` hook of the same extension that is running: a slow
//! `before_model` delays the events behind it (it never reorders them).
//!
//! # Failures
//!
//! A failure is a panic, an `Err`, a `Fail` decision, or `start_run` failing.
//! [`ExtensionMode`] decides what it means:
//!
//! - **Advisory** (default): logged, and that hook is skipped.
//! - **Required**: the run fails. Its last message is an assistant message
//!   with `StopReason::Error` and an `error_message` starting with
//!   [`EXTENSION_FAILED_PREFIX`]. Tool calls not started yet are answered
//!   with an error and never run.
//!
//! Hooks that guard a single call fail closed in both modes, without failing
//! the run: a failing `on_input` rejects the input, a failing `before_tool`
//! denies the call. A failing `after_tool` replaces the result with an error
//! naming the extension (a redactor that fails must not leak), and a required
//! one also fails the run.
//!
//! Panics are contained with `catch_unwind`, which does nothing where panics
//! abort (wasm32 builds, `panic = "abort"`).
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
//! the agent they are installed on. A custom delegation tool passes the tree
//! on with [`AgentLoopConfig::delegated_from`](crate::agent_loop::AgentLoopConfig::delegated_from)
//! or [`Agent::delegated_from`](crate::Agent::delegated_from).

use crate::provider::ToolDefinition;
use crate::rt::{MaybeSend, MaybeSync};
use crate::types::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Prefix of the `error_message` a run ends with when a required extension
/// fails: `[Extension failed: <name>] <reason>`.
pub const EXTENSION_FAILED_PREFIX: &str = "[Extension failed:";

/// Prefix of the user message a [`StopDecision::Continue`] appends:
/// `[Extension message: <name>] <message>`. Recognized as loop-injected
/// ([`is_loop_injected`]), so it is never taken for the user's own request.
pub const EXTENSION_MESSAGE_PREFIX: &str = "[Extension message: ";

/// How many times per run [`RunHooks::on_stop`] may continue the run, unless
/// [`AgentLoopConfig::max_stop_continues`](crate::agent_loop::AgentLoopConfig::max_stop_continues)
/// says otherwise.
pub const DEFAULT_MAX_STOP_CONTINUES: usize = 3;

/// How long [`RunHooks::finish`] may take before it is abandoned.
pub const FINISH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// What a failure inside an extension means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
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
    /// An error with this message.
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
    /// [`Agent::with_run_label`](crate::Agent::with_run_label) (a session id,
    /// say). yoagent does not interpret it. Delegated runs keep their
    /// parent's.
    pub label: Option<&'a str>,
    /// The run's prompts (for a delegated run: its task). Empty for a
    /// `continue_loop` run.
    pub prompts: &'a [Message],
    /// 0 for a top-level run, 1 for a sub-agent's run, and so on.
    pub depth: usize,
    /// For a delegated run: the id of the tool call that started it (in the
    /// calling run).
    pub delegated_by: Option<&'a str>,
    /// For a delegated run whose caller has extensions: the caller's
    /// `run_id`. With `delegated_by`, it names the delegation uniquely.
    pub parent_run_id: Option<&'a str>,
    /// In `start_run`: this extension came from the calling run's tree
    /// ([`Agent::with_tree_extension`](crate::Agent::with_tree_extension)),
    /// so the calling run has it too.
    pub inherited: bool,
    /// The run's cancellation token.
    pub cancel: &'a CancellationToken,
    /// What the run spends outside its model turns (decision models,
    /// compaction summaries), for [`Budget`]. `None` outside a loop.
    pub(crate) spend: Option<&'a Arc<RunSpend>>,
}

impl<'a> RunContext<'a> {
    /// A context for a top-level run with no label, for testing extensions.
    pub fn new(run_id: &'a str, prompts: &'a [Message], cancel: &'a CancellationToken) -> Self {
        Self {
            run_id,
            label: None,
            prompts,
            depth: 0,
            delegated_by: None,
            parent_run_id: None,
            inherited: false,
            cancel,
            spend: None,
        }
    }

    /// With the host's label.
    pub fn with_label(mut self, label: &'a str) -> Self {
        self.label = Some(label);
        self
    }

    /// At this delegation depth.
    pub fn with_depth(mut self, depth: usize) -> Self {
        self.depth = depth;
        self
    }

    /// Started by this tool call of the calling run `parent_run_id`.
    pub fn with_delegated_by(mut self, parent_run_id: &'a str, call_id: &'a str) -> Self {
        self.parent_run_id = Some(parent_run_id);
        self.delegated_by = Some(call_id);
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
    /// Every user text block of the prompts, joined by newlines: what the
    /// input filters see (their warnings are not included).
    pub text: &'a str,
    /// The prompts, after the input filters' warnings were appended.
    pub prompts: &'a [AgentMessage],
}

impl<'a> InputContext<'a> {
    /// A context for testing an input check.
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
    /// A context for testing a verifier.
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
    /// [`EXTENSION_MESSAGE_PREFIX`]; when several extensions continue, each
    /// gets a line) and run another turn. Capped per run.
    Continue(String),
    /// The answer is not acceptable.
    Fail(String),
}

/// A tool call's output, as [`RunHooks::after_tool`] sees and may edit it.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ToolOutput {
    /// What the model will see.
    pub result: ToolResult,
    /// Whether the call failed (the tool returned an error or panicked).
    pub is_error: bool,
}

impl ToolOutput {
    /// An output, for testing `after_tool`.
    pub fn new(result: ToolResult, is_error: bool) -> Self {
        Self { result, is_error }
    }
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RunEnd {
    /// The model finished its answer (or stopped otherwise on its own: a
    /// refusal, a length cut-off; see [`RunOutcome::stop_reason`]).
    Completed,
    /// The run was stopped before finishing: an execution limit, loop
    /// detection, `on_before_turn`, or an extension's `Stop`. The reason is
    /// the stop marker's text.
    Stopped { reason: String },
    /// The input was rejected before the run started.
    Rejected { reason: String },
    /// The run was cancelled.
    Cancelled,
    /// The run failed: a provider error, or a required extension
    /// (`extension` names it).
    Failed {
        error: String,
        extension: Option<String>,
    },
}

/// How a run ended, for [`RunHooks::finish`].
#[derive(Debug, Clone)]
pub struct RunOutcome {
    end: RunEnd,
    stop_reason: Option<StopReason>,
}

impl RunOutcome {
    /// An outcome, for testing `finish`.
    pub fn new(end: RunEnd, stop_reason: Option<StopReason>) -> Self {
        Self { end, stop_reason }
    }

    /// How the run ended.
    pub fn end(&self) -> &RunEnd {
        &self.end
    }

    /// The stop reason of the run's last assistant message, if it has one.
    pub fn stop_reason(&self) -> Option<&StopReason> {
        self.stop_reason.as_ref()
    }
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
    /// Such an extension that cannot start fails the run whatever its mode:
    /// running without it would let unfiltered output through.
    fn filters_tool_output(&self) -> bool {
        false
    }

    /// Whether this extension's `before_tool` must judge a call again when a
    /// later extension rewrote its arguments (a policy). The second verdict
    /// sees the final arguments, and a `Deny` there wins; a `Modify` returned
    /// on the recheck is ignored. So `before_tool` may be called twice for one
    /// call: a hook that counts calls should not rely on once.
    fn rechecks_modified_calls(&self) -> bool {
        false
    }

    /// The hooks for one run. Called at the start of every run.
    async fn start_run(&self, run: &RunContext<'_>) -> Result<Box<dyn RunHooks>, ExtensionError>;
}

/// A shared extension is an extension: install one `Arc` on several agents,
/// keep a handle to read its state, or pass a run's tree extensions on.
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl<T: Extension + ?Sized> Extension for Arc<T> {
    fn name(&self) -> &str {
        (**self).name()
    }
    fn mode(&self) -> ExtensionMode {
        (**self).mode()
    }
    fn filters_tool_output(&self) -> bool {
        (**self).filters_tool_output()
    }
    fn rechecks_modified_calls(&self) -> bool {
        (**self).rechecks_modified_calls()
    }
    async fn start_run(&self, run: &RunContext<'_>) -> Result<Box<dyn RunHooks>, ExtensionError> {
        (**self).start_run(run).await
    }
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

    /// Before a tool call runs. Takes `&self`: under parallel tool
    /// execution, the calls of one response are judged concurrently (a slow
    /// policy must not serialize them), so state it changes needs interior
    /// mutability.
    async fn before_tool(&self, _call: &ToolCallRequest<'_>) -> ToolDecision {
        ToolDecision::Allow
    }

    /// After a tool call ran; may edit its output. Takes `&self` and may run
    /// concurrently for parallel calls, like [`before_tool`](Self::before_tool).
    async fn after_tool(
        &self,
        _call: &ToolCallRequest<'_>,
        _output: &mut ToolOutput,
    ) -> Result<(), ExtensionError> {
        Ok(())
    }

    /// When the model ends its answer. Not called for an answer cut off at
    /// the output limit (`StopReason::Length`).
    async fn on_stop(&mut self, _stop: &StopContext<'_>) -> StopDecision {
        StopDecision::Accept
    }

    /// Observe an event of the run. Called for every `AgentEvent`, in order,
    /// before the event reaches the run's consumer. Must not block. The loop
    /// waits for observation to catch up before each turn, before tools run
    /// and before `finish`, so what it records is current there.
    fn on_event(&self, _event: &AgentEvent) {}

    /// A failure these hooks recorded but could not raise where it happened:
    /// from `on_event` (which must not block, and whose panic would switch
    /// off this extension's observation for the run), or from work of their
    /// own running in the background. Return it once; the loop takes it.
    ///
    /// The loop asks whenever it acts on failures: before each turn, before
    /// a response's tools run, before `on_stop`, and when the run ends
    /// however it ends (a limit, a cancel, an answer cut off). A required
    /// extension's failure then fails the run, and tool calls not started yet
    /// are not run; an advisory one's is logged.
    fn take_failure(&self) -> Option<String> {
        None
    }

    /// When the run ends. Not called if the run's future is dropped, so
    /// correctness must not depend on it. Bounded by [`FINISH_TIMEOUT`].
    async fn finish(&mut self, _outcome: &RunOutcome) {}
}

/// An extension whose hooks are cloned for every run: per-run state starts
/// from `hooks` each time.
pub struct ClonedHooks<H> {
    name: String,
    mode: ExtensionMode,
    filters_tool_output: bool,
    rechecks_modified_calls: bool,
    hooks: H,
}

impl<H: RunHooks + Clone + 'static> ClonedHooks<H> {
    /// An advisory extension named `name`.
    pub fn new(name: impl Into<String>, hooks: H) -> Self {
        Self {
            name: name.into(),
            mode: ExtensionMode::Advisory,
            filters_tool_output: false,
            rechecks_modified_calls: false,
            hooks,
        }
    }

    /// Make it required: its failures fail the run.
    pub fn required(mut self) -> Self {
        self.mode = ExtensionMode::Required;
        self
    }

    /// Declare that it filters tool output ([`Extension::filters_tool_output`]).
    pub fn filters_tool_output(mut self) -> Self {
        self.filters_tool_output = true;
        self
    }

    /// Declare that it rechecks rewritten calls
    /// ([`Extension::rechecks_modified_calls`]).
    pub fn rechecks_modified_calls(mut self) -> Self {
        self.rechecks_modified_calls = true;
        self
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl<H: RunHooks + Clone + 'static> Extension for ClonedHooks<H> {
    fn name(&self) -> &str {
        &self.name
    }

    fn mode(&self) -> ExtensionMode {
        self.mode
    }

    fn filters_tool_output(&self) -> bool {
        self.filters_tool_output
    }

    fn rechecks_modified_calls(&self) -> bool {
        self.rechecks_modified_calls
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

/// The extension named in a failed run's `error_message`, if an extension
/// failed it.
pub(crate) fn failed_extension(error: &str) -> Option<String> {
    let rest = error.strip_prefix(EXTENSION_FAILED_PREFIX)?.trim_start();
    rest.split_once(']').map(|(name, _)| name.to_string())
}

struct Active {
    name: String,
    mode: ExtensionMode,
    rechecks: bool,
    /// Installed by a parent (a tree extension): its tools are not offered.
    inherited: bool,
    /// `on_event` panicked once: it is not called again this run.
    events_off: AtomicBool,
    /// Read-locked for the `&self` hooks (`before_tool`, `after_tool`,
    /// `on_event`), write-locked for the rest.
    hooks: tokio::sync::RwLock<Box<dyn RunHooks>>,
}

/// A request to the event observer: acknowledge once every event sent before
/// it has been observed.
pub(crate) type FlushRequest = tokio::sync::oneshot::Sender<()>;

/// What a hook did: finished, failed (panicked), or was abandoned because the
/// run was cancelled.
enum Hooked<T> {
    Done(T),
    Panicked(String),
    Cancelled,
}

/// The extensions of one run, started, in dispatch order.
pub(crate) struct ActiveExtensions {
    run_id: String,
    label: Option<String>,
    prompts: Vec<Message>,
    depth: usize,
    delegated_by: Option<String>,
    parent_run_id: Option<String>,
    cancel: CancellationToken,
    active: Vec<Active>,
    filters_output: bool,
    /// A required extension's failure that could not end the run where it
    /// happened (in `tools`, `after_tool`, `on_event`); acted on at the next
    /// boundary, and blocking tool calls until then.
    failure: std::sync::Mutex<Option<Failure>>,
    /// The event observer's flush channel, set when one is running.
    flush: std::sync::OnceLock<tokio::sync::mpsc::UnboundedSender<FlushRequest>>,
    /// The run has been failed: later failures are only logged.
    failed_run: AtomicBool,
    /// The run's spend outside its model turns.
    spend: Arc<RunSpend>,
}

/// What a run spent outside its own model turns — decision-model
/// evaluations and compaction summaries recorded into the loop's scope —
/// kept so [`Budget`] can count it. Each piece keeps the price it was
/// recorded with; a piece that had none is kept as usage, priced by the
/// reader.
#[derive(Debug, Default)]
pub(crate) struct RunSpend(std::sync::Mutex<RunSpendInner>);

#[derive(Debug, Default)]
struct RunSpendInner {
    priced_usd: f64,
    unpriced: Usage,
}

impl RunSpend {
    /// Record one piece of spend; `cost_usd: None` = unpriced.
    pub(crate) fn add(&self, usage: &Usage, cost_usd: Option<f64>) {
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        match cost_usd {
            Some(usd) => inner.priced_usd += usd.max(0.0),
            None => {
                let u = &mut inner.unpriced;
                u.input = u.input.saturating_add(usage.input);
                u.output = u.output.saturating_add(usage.output);
                u.cache_read = u.cache_read.saturating_add(usage.cache_read);
                u.cache_write = u.cache_write.saturating_add(usage.cache_write);
            }
        }
    }

    /// Dollars so far, the unpriced pieces priced with `fallback`.
    fn usd_at(&self, fallback: &crate::provider::CostConfig) -> f64 {
        let inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        inner.priced_usd + fallback.cost_usd(&inner.unpriced)
    }
}

/// Await `fut`, containing a panic as `Err(payload text)`.
async fn guarded<T>(fut: impl std::future::Future<Output = T>) -> Result<T, String> {
    use futures::FutureExt;
    std::panic::AssertUnwindSafe(fut)
        .catch_unwind()
        .await
        .map_err(|payload| format!("panicked: {}", crate::tool_source::panic_message(&*payload)))
}

/// Why `before_model` ended the run before the request.
pub(crate) enum ModelHalt {
    Stop(String),
    Fail(Failure),
}

/// What `on_stop` decided across the extensions.
pub(crate) enum StopGate {
    Accept,
    Continue {
        /// Every extension that asked, with its message, in order.
        messages: Vec<(String, String)>,
        /// The first required extension that asked (did not accept).
        required: Option<String>,
    },
    Fail(Failure),
}

/// What identifies a run to its extensions.
pub(crate) struct RunInfo<'a> {
    pub(crate) label: Option<&'a str>,
    pub(crate) prompts: &'a [Message],
    pub(crate) depth: usize,
    pub(crate) delegated_by: Option<&'a str>,
    pub(crate) parent_run_id: Option<&'a str>,
    pub(crate) cancel: &'a CancellationToken,
}

impl ActiveExtensions {
    /// No extensions.
    pub(crate) fn none() -> Self {
        Self {
            run_id: String::new(),
            label: None,
            prompts: Vec::new(),
            depth: 0,
            delegated_by: None,
            parent_run_id: None,
            cancel: CancellationToken::new(),
            active: Vec::new(),
            filters_output: false,
            failure: std::sync::Mutex::new(None),
            flush: std::sync::OnceLock::new(),
            failed_run: AtomicBool::new(false),
            spend: Arc::default(),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.active.is_empty()
    }

    pub(crate) fn run_id(&self) -> &str {
        &self.run_id
    }

    /// The run's context, for hooks.
    pub(crate) fn run_context(&self) -> RunContext<'_> {
        RunContext {
            run_id: &self.run_id,
            label: self.label.as_deref(),
            prompts: &self.prompts,
            depth: self.depth,
            delegated_by: self.delegated_by.as_deref(),
            parent_run_id: self.parent_run_id.as_deref(),
            inherited: false,
            cancel: &self.cancel,
            spend: Some(&self.spend),
        }
    }

    /// What the run spends outside its model turns; the loop records into
    /// it (decision models, compaction summaries).
    pub(crate) fn spend(&self) -> Arc<RunSpend> {
        Arc::clone(&self.spend)
    }

    /// Await a hook, contained and abandoned if the run is cancelled.
    async fn hook<T>(&self, fut: impl std::future::Future<Output = T>) -> Hooked<T> {
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => Hooked::Cancelled,
            out = guarded(fut) => match out {
                Ok(v) => Hooked::Done(v),
                Err(panic) => Hooked::Panicked(panic),
            },
        }
    }

    /// Start every extension for a run: `inherited` first (from a parent's
    /// tree), then `own`. Returns the extensions that started, and the
    /// failure that must end the run, if any: a required extension, or one
    /// that filters tool output, could not start. The started ones are kept
    /// either way, so they still get `finish`.
    pub(crate) async fn start(
        inherited: &[Arc<dyn Extension>],
        own: &[Arc<dyn Extension>],
        info: RunInfo<'_>,
    ) -> (Self, Option<Failure>) {
        let mut exts = Self {
            run_id: uuid::Uuid::new_v4().to_string(),
            label: info.label.map(String::from),
            prompts: info.prompts.to_vec(),
            depth: info.depth,
            delegated_by: info.delegated_by.map(String::from),
            parent_run_id: info.parent_run_id.map(String::from),
            cancel: info.cancel.clone(),
            ..Self::none()
        };
        let mut failure = None;
        let all = inherited
            .iter()
            .map(|e| (e, true))
            .chain(own.iter().map(|e| (e, false)));
        for (ext, inherited) in all {
            // Even naming an extension runs its code: a panic there fails
            // the run (its mode is unknowable).
            let meta = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                (
                    ext.name().to_string(),
                    ext.mode(),
                    ext.filters_tool_output(),
                    ext.rechecks_modified_calls(),
                )
            }));
            let Ok((name, mode, filters, rechecks)) = meta else {
                tracing::error!(run_id = %exts.run_id, "an extension panicked describing itself; failing the run");
                failure.get_or_insert(Failure {
                    name: "<unnamed>".into(),
                    reason: "panicked describing itself".into(),
                });
                continue;
            };
            let run = RunContext {
                inherited,
                ..exts.run_context()
            };
            let reason = match exts.hook(ext.start_run(&run)).await {
                Hooked::Done(Ok(hooks)) => {
                    exts.filters_output |= filters;
                    exts.active.push(Active {
                        name,
                        mode,
                        rechecks,
                        inherited,
                        events_off: AtomicBool::new(false),
                        hooks: tokio::sync::RwLock::new(hooks),
                    });
                    continue;
                }
                // A cancelled run ends anyway; the extension is not needed.
                Hooked::Cancelled => continue,
                Hooked::Done(Err(e)) => e.to_string(),
                Hooked::Panicked(panic) => panic,
            };
            if mode == ExtensionMode::Required || filters {
                tracing::error!(run_id = %exts.run_id, extension = %name, "extension could not start; failing the run: {reason}");
                failure.get_or_insert(Failure {
                    name,
                    reason: format!("could not start: {reason}"),
                });
            } else {
                tracing::warn!(run_id = %exts.run_id, extension = %name, "extension could not start; it sits this run out: {reason}");
            }
        }
        (exts, failure)
    }

    /// The run is being failed: from now on, failures are only logged.
    pub(crate) fn latch_failed(&self) {
        self.failed_run.store(true, Ordering::SeqCst);
    }

    /// Record a hook failure: a required extension's fails the run (at the
    /// next boundary), an advisory one's is logged.
    fn failed(&self, a: &Active, hook: &str, reason: String) {
        if self.failed_run.load(Ordering::SeqCst) {
            tracing::error!(run_id = %self.run_id, extension = %a.name, hook, "extension failed after the run failed: {reason}");
            return;
        }
        if a.mode == ExtensionMode::Required {
            tracing::error!(run_id = %self.run_id, extension = %a.name, hook, "required extension failed: {reason}");
            let mut slot = self.failure.lock().unwrap_or_else(|e| e.into_inner());
            slot.get_or_insert(Failure {
                name: a.name.clone(),
                reason: format!("{hook} failed: {reason}"),
            });
        } else {
            tracing::warn!(run_id = %self.run_id, extension = %a.name, hook, "extension hook failed: {reason}");
        }
    }

    /// Connect the event observer's flush channel.
    pub(crate) fn connect_observer(&self, flush: tokio::sync::mpsc::UnboundedSender<FlushRequest>) {
        let _ = self.flush.set(flush);
    }

    /// Wait until every event sent so far has been observed, so `on_event`
    /// state and failures are current.
    pub(crate) async fn sync_events(&self) {
        if let Some(flush) = self.flush.get() {
            let (ack, done) = tokio::sync::oneshot::channel();
            let acked = flush.send(ack).is_ok() && done.await.is_ok();
            if !acked {
                tracing::error!(run_id = %self.run_id, "the extension event observer is gone; events are no longer observed");
            }
        }
        // With observation current, collect what the hooks recorded.
        for a in &self.active {
            let hooks = a.hooks.read().await;
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| hooks.take_failure())) {
                Ok(None) => {}
                Ok(Some(reason)) => self.failed(a, "take_failure", reason),
                Err(payload) => {
                    let why = crate::tool_source::panic_message(&*payload);
                    self.failed(a, "take_failure", format!("panicked: {why}"));
                }
            }
        }
    }

    /// The failure recorded so far, if any, once every sent event has been
    /// observed. Each failure is returned once, and none after the run was
    /// failed.
    pub(crate) async fn settle(&self) -> Option<Failure> {
        if self.is_empty() {
            return None;
        }
        self.sync_events().await;
        self.take_failure()
    }

    /// The failure recorded so far, without waiting for events.
    pub(crate) fn take_failure(&self) -> Option<Failure> {
        if self.failed_run.load(Ordering::SeqCst) {
            return None;
        }
        self.failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    /// Whether a required extension's failure is pending: tool calls are not
    /// started then.
    pub(crate) fn has_failure(&self) -> bool {
        !self.failed_run.load(Ordering::SeqCst)
            && self
                .failure
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_some()
    }

    pub(crate) fn filters_tool_output(&self) -> bool {
        self.filters_output
    }

    /// Every extension's tools for the run (not inherited ones').
    pub(crate) async fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        let run = self.run_context();
        let mut tools = Vec::new();
        for a in self.active.iter().filter(|a| !a.inherited) {
            let mut hooks = a.hooks.write().await;
            match self.hook(hooks.tools(&run)).await {
                Hooked::Done(t) => tools.extend(t),
                Hooked::Panicked(panic) => self.failed(a, "tools", panic),
                Hooked::Cancelled => break,
            }
        }
        tools
    }

    /// `Err(reason)` if an extension rejects the input. A failing hook
    /// rejects (fail closed); a cancelled one lets the run start, and it ends
    /// cancelled before any model request.
    pub(crate) async fn on_input(&self, input: &InputContext<'_>) -> Result<(), String> {
        for a in &self.active {
            let mut hooks = a.hooks.write().await;
            match self.hook(hooks.on_input(input)).await {
                Hooked::Done(InputDecision::Pass) => {}
                Hooked::Done(InputDecision::Reject(reason)) => return Err(reason),
                // Not a rejection: the run starts and ends cancelled at its
                // first check, before any model request.
                Hooked::Cancelled => return Ok(()),
                Hooked::Panicked(panic) => {
                    if a.mode == ExtensionMode::Required {
                        tracing::error!(run_id = %self.run_id, extension = %a.name, "on_input failed; rejecting the input: {panic}");
                    } else {
                        tracing::warn!(run_id = %self.run_id, extension = %a.name, "on_input failed; rejecting the input: {panic}");
                    }
                    return Err(format!("extension '{}' failed to check the input", a.name));
                }
            }
        }
        Ok(())
    }

    /// The notes for the request, or why the run ends before it. A
    /// cancelled run gets the notes so far: the request then ends aborted.
    pub(crate) async fn before_model(
        &self,
        turn: &TurnContext<'_>,
    ) -> Result<Vec<String>, ModelHalt> {
        let mut notes = Vec::new();
        for a in &self.active {
            let mut hooks = a.hooks.write().await;
            let decision = match self.hook(hooks.before_model(turn)).await {
                Hooked::Done(d) => d,
                Hooked::Panicked(panic) => TurnDecision::Fail(panic),
                Hooked::Cancelled => return Ok(notes),
            };
            match decision {
                TurnDecision::Continue => {}
                TurnDecision::Note(note) => {
                    if !note.trim().is_empty() {
                        notes.push(note);
                    }
                }
                TurnDecision::Stop(reason) => return Err(ModelHalt::Stop(reason)),
                TurnDecision::Fail(reason) => match a.mode {
                    ExtensionMode::Required => {
                        tracing::error!(run_id = %self.run_id, extension = %a.name, "required extension failed in before_model: {reason}");
                        return Err(ModelHalt::Fail(Failure {
                            name: a.name.clone(),
                            reason: format!("before_model failed: {reason}"),
                        }));
                    }
                    _ => {
                        tracing::warn!(run_id = %self.run_id, extension = %a.name, "before_model failed: {reason}")
                    }
                },
            }
        }
        Ok(notes)
    }

    /// One `before_tool` verdict, under a read lock.
    async fn judge(&self, a: &Active, call: &ToolCallRequest<'_>) -> Result<ToolDecision, String> {
        let hooks = a.hooks.read().await;
        match self.hook(hooks.before_tool(call)).await {
            Hooked::Done(decision) => Ok(decision),
            Hooked::Cancelled => Err("the run was cancelled".into()),
            Hooked::Panicked(panic) => {
                tracing::warn!(
                    run_id = %self.run_id,
                    extension = %a.name,
                    tool_call_id = call.tool_call_id,
                    "before_tool failed; denying the call: {panic}"
                );
                Err(format!("extension '{}' failed to check the call", a.name))
            }
        }
    }

    /// The final arguments, or `Err(reason)` if the call is denied. A failing
    /// or cancelled hook denies (fail closed).
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
            match self.judge(a, &request).await? {
                ToolDecision::Allow => {}
                ToolDecision::Modify(new_args) => {
                    args = new_args;
                    last_modifier = Some(i);
                }
                ToolDecision::Deny(reason) => return Err(reason),
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
                match self.judge(a, &request).await? {
                    ToolDecision::Deny(reason) => return Err(reason),
                    ToolDecision::Allow => {}
                    ToolDecision::Modify(_) => tracing::debug!(
                        run_id = %self.run_id,
                        extension = %a.name,
                        "a Modify on a recheck is ignored; the call runs with the arguments it approved"
                    ),
                }
            }
        }
        Ok(args)
    }

    /// Run `after_tool` over a call's output. A failing or cancelled hook
    /// replaces the result with an error naming the extension (fail closed);
    /// a required extension's failure also fails the run.
    pub(crate) async fn after_tool(&self, call: &ToolCallRequest<'_>, output: &mut ToolOutput) {
        for a in &self.active {
            let outcome = {
                let hooks = a.hooks.read().await;
                self.hook(hooks.after_tool(call, output)).await
            };
            let reason = match outcome {
                Hooked::Done(Ok(())) => continue,
                Hooked::Done(Err(e)) => Some(e.to_string()),
                Hooked::Panicked(panic) => Some(panic),
                Hooked::Cancelled => None,
            };
            // Withheld either way (a redactor may not have run), but say why:
            // a cancelled run is not an extension's fault.
            let text = if reason.is_none() {
                format!(
                    "Tool result withheld: the run was cancelled before extension '{}' \
                     finished processing it.",
                    a.name
                )
            } else {
                format!(
                    "Tool result withheld: extension '{}' did not finish processing it.",
                    a.name
                )
            };
            output.result = ToolResult {
                content: vec![Content::Text { text }],
                details: serde_json::Value::Null,
            };
            output.is_error = true;
            match reason {
                Some(reason) => self.failed(a, "after_tool", reason),
                // Cancelled: the run ends anyway; withholding is enough.
                None => return,
            }
        }
    }

    pub(crate) async fn on_stop(&self, stop: &StopContext<'_>) -> StopGate {
        let mut messages: Vec<(String, String)> = Vec::new();
        let mut required: Option<String> = None;
        for a in &self.active {
            let decision = {
                let mut hooks = a.hooks.write().await;
                self.hook(hooks.on_stop(stop)).await
            };
            let reason = match decision {
                Hooked::Done(StopDecision::Accept) => continue,
                Hooked::Done(StopDecision::Continue(message)) => {
                    if a.mode == ExtensionMode::Required {
                        required.get_or_insert_with(|| a.name.clone());
                    }
                    messages.push((a.name.clone(), message));
                    continue;
                }
                // A cancelled run ends here anyway.
                Hooked::Cancelled => return StopGate::Accept,
                Hooked::Done(StopDecision::Fail(reason)) => reason,
                Hooked::Panicked(panic) => panic,
            };
            match a.mode {
                ExtensionMode::Required => {
                    tracing::error!(run_id = %self.run_id, extension = %a.name, "required extension failed in on_stop: {reason}");
                    return StopGate::Fail(Failure {
                        name: a.name.clone(),
                        reason: format!("on_stop failed: {reason}"),
                    });
                }
                _ => {
                    tracing::warn!(run_id = %self.run_id, extension = %a.name, "on_stop failed: {reason}")
                }
            }
        }
        if messages.is_empty() {
            StopGate::Accept
        } else {
            StopGate::Continue { messages, required }
        }
    }

    /// Show an event to every extension's `on_event`. One that panics is not
    /// called again this run.
    pub(crate) async fn on_event(&self, event: &AgentEvent) {
        for a in &self.active {
            if a.events_off.load(Ordering::Relaxed) {
                continue;
            }
            let hooks = a.hooks.read().await;
            if let Err(payload) =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| hooks.on_event(event)))
            {
                a.events_off.store(true, Ordering::Relaxed);
                let why = crate::tool_source::panic_message(&*payload);
                self.failed(
                    a,
                    "on_event",
                    format!("panicked: {why} (not called again this run)"),
                );
            }
        }
    }

    pub(crate) async fn finish(&self, outcome: &RunOutcome) {
        // Every event sent so far is observed before the hooks see the end.
        self.sync_events().await;
        for a in &self.active {
            let mut hooks = a.hooks.write().await;
            let result = crate::rt::timeout(FINISH_TIMEOUT, guarded(hooks.finish(outcome))).await;
            let problem = match result {
                Ok(Ok(())) => continue,
                Ok(Err(panic)) => panic,
                Err(_) => format!("did not finish within {FINISH_TIMEOUT:?}"),
            };
            // Too late to change the outcome: log it.
            match a.mode {
                ExtensionMode::Required => {
                    tracing::error!(run_id = %self.run_id, extension = %a.name, "required extension's finish failed: {problem}")
                }
                _ => {
                    tracing::warn!(run_id = %self.run_id, extension = %a.name, "finish failed: {problem}")
                }
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

// ---------------------------------------------------------------------------
// Budget
// ---------------------------------------------------------------------------

/// A spending limit in dollars: an [`Extension`] that ends a run with
/// `[Agent stopped: budget …]` before a model request once the limit is
/// reached. A request already sent can take spend past the limit; the check
/// is before each request, not during one.
///
/// Spend is each assistant message's usage priced with the given
/// [`CostConfig`](crate::provider::CostConfig), as the message is observed;
/// plus the run's own requests outside its turns — an
/// [`LlmCompaction`](crate::LlmCompaction) summary and decision-model
/// evaluations (the tool gate, input guard and advisor; feature
/// `decision`) — each at its own model's price, or this one if it has none,
/// counted before each request and when the run ends; plus what a sub-agent
/// reports spending, all of the above included (its own price, or this one
/// if it has none). That is the spend
/// [`SessionStats::total_cost_usd`](crate::SessionStats::total_cost_usd)
/// adds up, except that it never stays unknown: an unpriced part is priced
/// at this budget's rates. By default the limit is per run, sub-agents included. With
/// [`across_runs`](Self::across_runs) it is one total for every run the
/// extension serves: a session's runs, or, installed with
/// [`Agent::with_tree_extension`](crate::Agent::with_tree_extension), every
/// run of a delegation tree (each run's own messages counted once). A per-run
/// budget installed as a tree extension gives every run of the tree its own
/// limit. Every message of the tree is priced at the one rate given. A
/// provider attempt that fails mid-stream reports no usage, so input tokens a
/// provider billed for it are not counted.
///
/// ```
/// # use yoagent::extension::Budget;
/// # use yoagent::provider::ModelConfig;
/// // Nothing is priced until the process opts in.
/// yoagent::provider::prices::enable_bundled();
/// let model = ModelConfig::claude_sonnet_5();
/// // `None` when the model has no price: an unpriced budget would be no limit.
/// let budget = Budget::for_model(2.0, &model).expect("a priced model");
/// ```
pub struct Budget {
    name: String,
    max_usd: f64,
    cost: crate::provider::CostConfig,
    across_runs: bool,
    /// The total when `across_runs`.
    total: Arc<std::sync::Mutex<f64>>,
    /// The tool calls whose delegated run this budget served (as a tree
    /// extension): that child's spend is counted by the child's own run, not
    /// again from the parent's tool result.
    children: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
}

impl Budget {
    /// At most `max_usd` per run, priced with `cost`.
    ///
    /// # Panics
    ///
    /// When `max_usd` is negative or NaN: a limit that never stops, or stops
    /// before anything, is a setup mistake. `f64::INFINITY` is allowed.
    pub fn usd(max_usd: f64, cost: crate::provider::CostConfig) -> Self {
        assert!(
            max_usd >= 0.0,
            "Budget::usd: the limit must be zero or more, got {max_usd}"
        );
        Self {
            name: "budget".into(),
            max_usd,
            cost,
            across_runs: false,
            total: Arc::default(),
            children: Arc::default(),
        }
    }

    /// At most `max_usd` per run, priced at `model`'s rates; `None` when the
    /// model has no price.
    pub fn for_model(max_usd: f64, model: &crate::provider::ModelConfig) -> Option<Self> {
        model.cost.clone().map(|cost| Self::usd(max_usd, cost))
    }

    /// One total for every run this extension serves, instead of one per run.
    pub fn across_runs(mut self) -> Self {
        self.across_runs = true;
        self
    }

    /// Name it (default `"budget"`), to tell several budgets apart in logs
    /// and stop messages.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Dollars spent so far across runs: `None` for a per-run budget, whose
    /// totals end with their runs. Keep an `Arc<Budget>` (an extension too)
    /// to read it after installing.
    pub fn spent_usd(&self) -> Option<f64> {
        self.across_runs
            .then(|| *self.total.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

struct BudgetRun {
    run_id: String,
    name: String,
    max_usd: f64,
    cost: crate::provider::CostConfig,
    /// The run's own spend, or the shared total when `across_runs`.
    spent: Arc<std::sync::Mutex<f64>>,
    children: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// The run's spend outside its turns (decision, compaction), and how
    /// much of it is already in `spent`.
    extra: Option<Arc<RunSpend>>,
    extra_counted: f64,
}

impl BudgetRun {
    fn add(&self, usd: f64) {
        if usd > 0.0 {
            *self.spent.lock().unwrap_or_else(|e| e.into_inner()) += usd;
        }
    }

    /// Count what the run spent outside its turns since the last call.
    fn count_extra(&mut self) {
        if let Some(extra) = &self.extra {
            let now = extra.usd_at(&self.cost);
            if now > self.extra_counted {
                self.add(now - self.extra_counted);
                self.extra_counted = now;
            }
        }
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl Extension for Budget {
    fn name(&self) -> &str {
        &self.name
    }

    async fn start_run(&self, run: &RunContext<'_>) -> Result<Box<dyn RunHooks>, ExtensionError> {
        // Only a budget inherited from the calling run's tree is also there to
        // skip the child's reported spend; any other key would never be
        // removed.
        if let (true, Some(parent), Some(call_id)) =
            (run.inherited, run.parent_run_id, run.delegated_by)
        {
            self.children
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(format!("{parent}/{call_id}"));
        }
        Ok(Box::new(BudgetRun {
            run_id: run.run_id.to_string(),
            name: self.name.clone(),
            max_usd: self.max_usd,
            cost: self.cost.clone(),
            spent: if self.across_runs {
                self.total.clone()
            } else {
                Arc::default()
            },
            children: self.children.clone(),
            extra: run.spend.cloned(),
            extra_counted: 0.0,
        }))
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl RunHooks for BudgetRun {
    async fn before_model(&mut self, _turn: &TurnContext<'_>) -> TurnDecision {
        self.count_extra();
        let spent = *self.spent.lock().unwrap_or_else(|e| e.into_inner());
        if spent >= self.max_usd {
            TurnDecision::Stop(format!(
                "{} of ${:.2} spent (${spent:.4})",
                self.name, self.max_usd
            ))
        } else {
            TurnDecision::Continue
        }
    }

    fn on_event(&self, event: &AgentEvent) {
        match event {
            AgentEvent::MessageEnd {
                message: AgentMessage::Llm(Message::Assistant { usage, .. }),
            } => self.add(self.cost.cost_usd(usage)),
            // A sub-agent's spend, unless this budget served that child run
            // too (a tree budget counts the child's messages there).
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                result,
                ..
            } => {
                let served = self
                    .children
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&format!("{}/{tool_call_id}", self.run_id));
                if served {
                    return;
                }
                if let Some(child) = SessionStats::from_sub_agent_result(result) {
                    let usd = child
                        .total_cost_usd()
                        .unwrap_or_else(|| self.cost.cost_usd(&child.total_usage()));
                    self.add(usd);
                }
            }
            _ => {}
        }
    }

    async fn finish(&mut self, _outcome: &RunOutcome) {
        // Spend after the last request (a gate judging the last calls, a
        // summary that finished late) still counts toward a shared total.
        self.count_extra();
    }
}
