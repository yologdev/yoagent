//! The bridge as one yoagent [`Extension`]: [`RutisExtension`].
//!
//! The host installs it with `Agent::with_extension` (this agent's runs), or
//! with `with_tree_extension` so plugin policy also covers every sub-agent
//! run (yoagent does not offer a tree extension's tools to child runs).
//! [`start_run`](Extension::start_run) snapshots the registered handlers:
//! a plugin loading, unloading or reloading during a run does not change the
//! handlers that run uses (see [`registry`](crate::registry) for what a
//! handler whose plugin unloaded does).
//!
//! # How handlers combine
//!
//! Handlers run in registration order (a reloaded plugin registers again,
//! after the others).
//!
//! | Hook | Combination | A handler that errors, panics or times out |
//! | --- | --- | --- |
//! | `tools` | Static tools, then each `tools` hook; on a name clash the earlier wins | Contributes nothing (a required extension fails the run) |
//! | `on_input` | The first `Reject` wins | Rejects the input |
//! | `before_model` | Notes joined, one per line; the first `Stop` ends the run | Skipped (a required extension fails the run) |
//! | `before_tool` | A `Deny` wins; a `Modify` feeds the next handler | Denies the call |
//! | `after_tool` | Each sees the previous handler's edit | Withholds the result (a required extension fails the run) |
//! | `on_stop` | Every `Continue` message is sent, joined | Skipped (a required extension fails the run) |
//! | `on_event` | All, in order, synchronously | That handler is switched off for the run; the others and bus publishing go on (a required extension fails the run, see below) |
//! | `finish` | All, concurrently | Logged |
//!
//! A handler whose plugin unloaded is not a failure: it is skipped where the
//! table says "skipped", and a hook that must not pass unjudged still fails
//! closed — `before_tool` denies, `on_input` rejects, `after_tool` withholds
//! the result (without failing a required run).
//!
//! A required extension's `tools` or `on_event` failure is recorded, not
//! raised: from then on the run's tool calls are denied and their results
//! withheld, and yoagent takes it at its next boundary
//! ([`RunHooks::take_failure`]): before a turn or a response's tools,
//! before `on_stop`, or when the run ends however it ends, and fails the
//! run. Only a failure on the run's very last event (`AgentEnd`) comes too
//! late; it is logged at `error` level.
//!
//! Required or not is the host's choice ([`RutisExtension::required`]), not
//! a plugin's. Each hook call is bounded by a timeout (see
//! [`RutisExtension::with_policy_timeout`]) and, through yoagent, abandoned
//! when the run is cancelled.
//!
//! # A host that is not running
//!
//! When the bridge's rutis context has shut down (or the root was disposed
//! or restarted since the bridge was installed),
//! a run starting then denies every tool call, rejects its input and gets no
//! notes, and a run already going denies its later tool calls. Every plugin
//! was unloaded with it, so an empty registry must not read as "no policy
//! objected".

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::FutureExt;
use yoagent::extension::{
    ExtensionError, InputContext, InputDecision, RunContext, RunEnd, RunOutcome, StopContext,
    StopDecision, ToolOutput, TurnDecision,
};
use yoagent::{
    AgentEvent, AgentTool, Content, Extension, ExtensionMode, Message, RunHooks, ToolCallRequest,
    ToolDecision, TurnContext,
};

use crate::handler::{
    join_notes, llm_messages, EventSink, Input, Messages, Offer, RunInfo, Stop, ToolCall, Turn,
};
use crate::host::{Host, NOT_RUNNING};
use crate::late::{Late, LateRun, LateTools};
use crate::registry::{Outcome, Registered, Registry, ToolOwner};

/// Default bound on one handler's `before_tool`, `after_tool` and `on_stop`
/// call (the tool call is denied / the result withheld / the handler skipped
/// past it).
pub const DEFAULT_POLICY_TIMEOUT: Duration = Duration::from_secs(60);

/// Default bound on one handler's `on_input` call (the input is rejected
/// past it).
pub const DEFAULT_INPUT_TIMEOUT: Duration = Duration::from_secs(30);

/// Default bound on one handler's `before_model`, `tools` and `finish` call
/// (the handler is skipped past it).
pub const DEFAULT_TURN_TIMEOUT: Duration = Duration::from_secs(5);

const UNJUDGED: &str = "no plugin policy is loaded to judge this call";

/// The bridge's plugins as one yoagent [`Extension`].
///
/// Built by [`RutisBridge::extension`](crate::RutisBridge::extension); cheap
/// to clone. The builder methods are the host's decisions — a plugin cannot
/// make itself required, or declare that it filters output.
#[derive(Clone)]
pub struct RutisExtension {
    host: Host,
    registry: Arc<Registry>,
    name: String,
    mode: ExtensionMode,
    filters_tool_output: bool,
    rechecks_modified_calls: bool,
    require_policy: bool,
    timeouts: Timeouts,
    /// Shared with this extension's [`LateTools`] (and its clones).
    late: Arc<Late>,
}

#[derive(Clone, Copy)]
pub(crate) struct Timeouts {
    pub(crate) policy: Option<Duration>,
    pub(crate) input: Option<Duration>,
    pub(crate) turn: Option<Duration>,
}

impl RutisExtension {
    pub(crate) fn new(host: Host, registry: Arc<Registry>) -> Self {
        Self {
            host,
            registry,
            name: "rutis".into(),
            mode: ExtensionMode::Advisory,
            filters_tool_output: false,
            rechecks_modified_calls: false,
            require_policy: false,
            timeouts: Timeouts {
                policy: Some(DEFAULT_POLICY_TIMEOUT),
                input: Some(DEFAULT_INPUT_TIMEOUT),
                turn: Some(DEFAULT_TURN_TIMEOUT),
            },
            late: Arc::default(),
        }
    }

    /// Name — or leave out — the tools of **restricted** handlers (see the
    /// [`registry`](crate::registry) docs): `name(handler, tool)` gives the
    /// name to offer `tool` of the handler named `handler` under, or `None`
    /// to leave it out (without this: `<handler>__<tool>`). The plugin is
    /// still called with its own name. For a host that offers only the
    /// tools its user approved, or names them its way — apart from the
    /// agent's own and its sources' tools, which win a clash. Applies to
    /// [`late_tools`](Self::late_tools) too. Other handlers' tools keep
    /// their names.
    pub fn with_restricted_tools(
        self,
        name: impl Fn(&str, &str) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        *self.late.names.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(name));
        self
    }

    /// A yoagent `ToolSource` offering the tools of restricted handlers that
    /// register while one of this extension's runs is going, from the run's
    /// next turn: see [`LateTools`]. Install it on the agent this extension
    /// is installed on.
    pub fn late_tools(&self) -> LateTools {
        if !self
            .late
            .watched
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            self.registry.watch_late(&self.late);
        }
        LateTools {
            late: self.late.clone(),
            registry: self.registry.clone(),
        }
    }

    /// Name the extension in yoagent's logs and failure messages (default
    /// `"rutis"`).
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Make plugin failures fail the run: a handler whose `tools`,
    /// `before_model`, `after_tool`, `on_stop` or `on_event` fails ends the
    /// run with yoagent's `[Extension failed: ...]` error (a `tools` or
    /// `on_event` failure at yoagent's next boundary, the run's end included;
    /// one on `AgentEnd` is only logged). By default they are logged and the
    /// handler is skipped. A failing `before_tool` denies the call and a
    /// failing `on_input` rejects the input either way. A handler whose
    /// plugin unloaded mid-run never fails the run.
    pub fn required(mut self) -> Self {
        self.mode = ExtensionMode::Required;
        self
    }

    /// Declare that plugins filter tool output (redaction in `after_tool`):
    /// yoagent then withholds partial tool output, so only the filtered
    /// result is sent. Only if you trust your plugins to redact.
    pub fn filters_tool_output(mut self) -> Self {
        self.filters_tool_output = true;
        self
    }

    /// Have plugin policy judge a call again when an extension installed
    /// after the bridge rewrote its arguments (when the bridge carries host
    /// policy). Handlers' `before_tool` may then run twice for one call.
    pub fn rechecks_modified_calls(mut self) -> Self {
        self.rechecks_modified_calls = true;
        self
    }

    /// Deny every tool call of a run that starts with no plugin policy (no
    /// handler with `before_tool`) registered — the agent's own tools
    /// included.
    ///
    /// By default such a run allows every call. That covers the window
    /// while a policy plugin reloads (restart, config update,
    /// dependency-driven eviction: the old handler is removed before the new
    /// generation registers) and before it first becomes active. Use this
    /// when a policy plugin is load-bearing. Input checks have no
    /// counterpart.
    pub fn require_policy(mut self) -> Self {
        self.require_policy = true;
        self
    }

    /// Set the same bound on every hook.
    pub fn with_timeout(self, limit: Duration) -> Self {
        self.with_policy_timeout(Some(limit))
            .with_input_timeout(Some(limit))
            .with_turn_timeout(Some(limit))
    }

    /// Bound each handler's `before_tool`, `after_tool` and `on_stop` call
    /// (default [`DEFAULT_POLICY_TIMEOUT`]). `None`: no bound beyond the
    /// run's cancellation (for a policy that waits on a human).
    pub fn with_policy_timeout(mut self, limit: Option<Duration>) -> Self {
        self.timeouts.policy = limit;
        self
    }

    /// Bound each handler's `on_input` call (default
    /// [`DEFAULT_INPUT_TIMEOUT`]). `None`: no bound.
    pub fn with_input_timeout(mut self, limit: Option<Duration>) -> Self {
        self.timeouts.input = limit;
        self
    }

    /// Bound each handler's `before_model`, `tools` and `finish` call
    /// (default [`DEFAULT_TURN_TIMEOUT`]). `None`: no bound. yoagent bounds
    /// `finish` as a whole at its own `FINISH_TIMEOUT` anyway.
    pub fn with_turn_timeout(mut self, limit: Option<Duration>) -> Self {
        self.timeouts.turn = limit;
        self
    }
}

#[async_trait::async_trait]
impl Extension for RutisExtension {
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

    async fn start_run(&self, run: &RunContext<'_>) -> Result<Box<dyn RunHooks>, ExtensionError> {
        let info = RunInfo::from_context(run);
        let closed = self.host.is_closed();
        // Tracked before the snapshot: a restricted handler registering in
        // between is in the snapshot or pending, never in neither.
        let late = self
            .late
            .started(&info, run.inherited, self.timeouts)
            .map(|run| (self.late.clone(), run));
        let handlers = if closed {
            tracing::warn!(run_id = %info.run_id, "{NOT_RUNNING}: tool calls are denied and input rejected");
            Vec::new()
        } else {
            self.registry.snapshot()
        };
        if let Some((_, run)) = &late {
            run.snapshot(&handlers);
        }
        let events = handlers
            .iter()
            .filter(|h| h.hooks.on_event)
            .filter_map(|h| {
                h.handler
                    .events(&info, self.timeouts.turn)
                    .map(|sink| Observer {
                        handler: h.clone(),
                        sink,
                        off: AtomicBool::new(false),
                        got_end: AtomicBool::new(false),
                    })
            })
            .collect();
        // A restricted handler's `before_tool` judges only its own tools: not
        // a policy.
        let unjudged = self.require_policy
            && !handlers
                .iter()
                .any(|h| h.hooks.before_tool && !h.restricted);
        Ok(Box::new(RunState {
            registry: self.registry.clone(),
            offered: Arc::default(),
            recorded: false,
            host: self.host.clone(),
            run: info,
            required: self.mode == ExtensionMode::Required,
            closed,
            unjudged,
            timeouts: self.timeouts,
            names: self
                .late
                .names
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            handlers,
            events,
            late,
            failure: Mutex::new(None),
        }))
    }
}

impl Drop for RunState {
    fn drop(&mut self) {
        // Late tools record offers for the run too.
        if self.recorded || self.late.is_some() {
            self.registry.forget_offers(&self.run.run_id);
        }
        if let Some((late, _)) = &self.late {
            late.ended(&self.run.run_id);
        }
    }
}

/// One handler's event delivery for the run.
struct Observer {
    handler: Registered,
    sink: Box<dyn EventSink>,
    /// Panicked once: not called again this run.
    off: AtomicBool,
    /// The run's end reached it — its plugin was loaded when the run's
    /// `finish` began and, with a `finish` hook, when that hook was called:
    /// it gets the run's `AgentEnd` even if the plugin has begun unloading
    /// since. Set by `finish` (yoagent always calls it before `AgentEnd`).
    got_end: AtomicBool,
}

/// One run's hooks over the snapshot of handlers.
struct RunState {
    registry: Arc<Registry>,
    /// Which handler offered each plugin tool of the run (set by `tools`,
    /// also in the registry until the run's hooks are dropped).
    offered: Arc<HashMap<String, ToolOwner>>,
    /// This run's `tools` made the registry's record of `offered`.
    recorded: bool,
    host: Host,
    run: RunInfo,
    required: bool,
    closed: bool,
    unjudged: bool,
    timeouts: Timeouts,
    /// The host's naming of restricted handlers' tools.
    names: Option<crate::late::Names>,
    handlers: Vec<Registered>,
    events: Vec<Observer>,
    /// The run as late (restricted) handlers join it, when it is the
    /// extension's own and late tools are in use.
    late: Option<(Arc<Late>, Arc<LateRun>)>,
    /// A required failure not yet handed to yoagent: one in `before_model` /
    /// `on_stop` (returned as `Fail` right away), or one in `tools` /
    /// `on_event`, which have no way to fail the run themselves. While it is
    /// pending, tool calls are denied and results withheld; the next
    /// `before_model` or `on_stop` returns it as `Fail`.
    failure: Mutex<Option<String>>,
}

/// Why a handler's hook gave no answer.
pub(crate) enum Missed {
    /// Its plugin unloaded before the call, or during it.
    Unavailable(String),
    /// It errored, panicked or timed out.
    Failed(String),
}

impl Missed {
    pub(crate) fn reason(&self) -> &str {
        match self {
            Missed::Unavailable(r) | Missed::Failed(r) => r,
        }
    }
}

/// Text of a panic payload.
fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".into())
}

/// Call one handler's hook: unavailable if its plugin is gone, contained if
/// it panics, abandoned past `limit` or when its plugin unloads mid-call.
pub(crate) async fn call<T>(
    h: &Registered,
    hook: &'static str,
    limit: Option<Duration>,
    fut: impl std::future::Future<Output = Result<T, ExtensionError>>,
) -> Result<T, Missed> {
    if !h.gate.is_available() {
        return Err(Missed::Unavailable(format!(
            "plugin handler `{}` is no longer available: its plugin unloaded",
            h.name
        )));
    }
    let timer = h.time(hook);
    let result = call_timed(h, hook, limit, fut).await;
    timer.done(Outcome::answered(result.is_ok()));
    result
}

async fn call_timed<T>(
    h: &Registered,
    hook: &'static str,
    limit: Option<Duration>,
    fut: impl std::future::Future<Output = Result<T, ExtensionError>>,
) -> Result<T, Missed> {
    let guarded = AssertUnwindSafe(fut).catch_unwind();
    let raced = async {
        tokio::select! {
            out = guarded => Some(out),
            _ = h.gate.gone() => None,
        }
    };
    let out = match limit {
        Some(limit) => match tokio::time::timeout(limit, raced).await {
            Ok(out) => out,
            Err(_) => {
                return Err(Missed::Failed(format!(
                    "plugin handler `{}` did not answer `{hook}` within {limit:?}",
                    h.name
                )))
            }
        },
        None => raced.await,
    };
    match out {
        None => Err(Missed::Unavailable(format!(
            "plugin handler `{}`: its plugin unloaded during `{hook}`",
            h.name
        ))),
        Some(Err(payload)) => Err(Missed::Failed(format!(
            "plugin handler `{}` panicked in `{hook}`: {}",
            h.name,
            panic_text(&*payload)
        ))),
        Some(Ok(Err(error))) => Err(Missed::Failed(format!(
            "plugin handler `{}` failed in `{hook}`: {error}",
            h.name
        ))),
        Some(Ok(Ok(value))) => Ok(value),
    }
}

impl RunState {
    /// A handler of a skippable hook failed: log it, and remember it when
    /// the extension is required.
    fn skipped(&self, missed: &Missed) {
        match missed {
            Missed::Unavailable(why) => {
                tracing::debug!(run_id = %self.run.run_id, "skipping a handler: {why}")
            }
            Missed::Failed(why) if self.required => {
                tracing::error!(run_id = %self.run.run_id, "{why}");
                self.record(why.clone());
            }
            Missed::Failed(why) => tracing::warn!(run_id = %self.run.run_id, "{why}; skipped"),
        }
    }

    /// Pick up a delivery failure of a handler that delivers events later
    /// (a language plugin): its `on_event` is switched off, and a required
    /// failure is recorded, to deny tool calls and fail the run at the next
    /// model request or stop.
    fn check_sinks(&self) {
        for observer in &self.events {
            if observer.off.load(Ordering::Relaxed) {
                continue;
            }
            if let Some(why) = observer.sink.failure() {
                observer.off.store(true, Ordering::Relaxed);
                self.skipped(&Missed::Failed(why));
            }
        }
    }

    /// Which handler offered the run `tool`: at its start, or late.
    fn owner_of(&self, tool: &str) -> Option<ToolOwner> {
        self.offered
            .get(tool)
            .cloned()
            .or_else(|| self.late.as_ref().and_then(|(_, late)| late.offered(tool)))
    }

    /// The restricted handlers that joined the run late.
    fn joined(&self) -> Vec<Registered> {
        self.late
            .as_ref()
            .map(|(_, late)| late.joined())
            .unwrap_or_default()
    }

    /// The pending required failure, without taking it.
    fn pending(&self) -> Option<String> {
        self.check_sinks();
        self.failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn record(&self, why: String) {
        self.failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert(why);
    }

    fn take_failure(&self) -> Option<String> {
        self.check_sinks();
        self.failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    fn with(&self, pick: fn(&Registered) -> bool) -> impl Iterator<Item = &Registered> {
        self.handlers.iter().filter(move |h| pick(h))
    }
}

/// Whether any of these handlers asked for the run's conversation (then it
/// is copied once for the call).
fn wants_messages(handlers: &[&Registered]) -> bool {
    handlers.iter().any(|h| h.hooks.messages)
}

/// The event as `h` sees it: the conversation only if it asked.
fn for_handler<T: Clone>(
    h: &Registered,
    event: &T,
    messages: impl FnOnce(&mut T) -> &mut Option<Messages>,
) -> T {
    let mut event = event.clone();
    if !h.hooks.messages {
        *messages(&mut event) = None;
    }
    event
}

fn answer_text(answer: &Message) -> String {
    match answer {
        Message::Assistant { content, .. } => content
            .iter()
            .filter_map(|c| match c {
                Content::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

#[async_trait::async_trait]
impl RunHooks for RunState {
    async fn tools(&mut self, _run: &RunContext<'_>) -> Vec<Arc<dyn AgentTool>> {
        let mut tools: Vec<Arc<dyn AgentTool>> = Vec::new();
        let mut offered = HashMap::new();
        // Restricted handlers last: their tools never take a name a trusted
        // handler's tool has.
        let order: Vec<&Registered> = self
            .handlers
            .iter()
            .filter(|h| !h.restricted)
            .chain(self.handlers.iter().filter(|h| h.restricted))
            .collect();
        for h in order {
            if !h.gate.is_available() {
                tracing::debug!(run_id = %self.run.run_id, handler = %h.name, "its plugin unloaded; offering none of its tools");
                continue;
            }
            let mut offers: Vec<Offer> = h
                .handler
                .static_tools()
                .into_iter()
                .map(|tool| Offer { tool, source: None })
                .collect();
            if h.hooks.tools {
                match call(
                    h,
                    "tools",
                    self.timeouts.turn,
                    h.handler.tools(self.run.clone()),
                )
                .await
                {
                    Ok(dynamic) => offers.extend(dynamic),
                    // A restricted handler offers nothing then; it never
                    // fails the run (tools only).
                    Err(missed) if h.restricted => {
                        tracing::warn!(run_id = %self.run.run_id, handler = %h.name, "a restricted handler's tools: {}; none offered", missed.reason())
                    }
                    // Not raised as a panic: recorded, and handed to yoagent
                    // at the next decision point (`before_model` comes next).
                    Err(missed) => self.skipped(&missed),
                }
            }
            for Offer { tool, source } in offers {
                let own = tool.name().to_string();
                let Some(name) = crate::late::offered_name(&self.names, h, &own) else {
                    tracing::debug!(run_id = %self.run.run_id, handler = %h.name, tool = %own, "left out by the host");
                    continue;
                };
                if offered.contains_key(&name) {
                    tracing::warn!(
                        run_id = %self.run.run_id,
                        tool = %name,
                        handler = %h.name,
                        "a plugin tool's name is taken by an earlier handler's tool; dropped for this run"
                    );
                    continue;
                }
                let (owner, tool) = crate::late::live(
                    h,
                    tool,
                    source,
                    name.clone(),
                    &self.run,
                    self.timeouts.policy,
                );
                offered.insert(name, owner);
                tools.push(tool);
            }
        }
        (self.offered, self.recorded) = self.registry.record_offers(&self.run.run_id, offered);
        if let Some((_, late)) = &self.late {
            late.taken
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend(self.offered.keys().cloned());
        }
        tools
    }

    async fn on_input(&mut self, input: &InputContext<'_>) -> InputDecision {
        if self.closed {
            return InputDecision::Reject(format!("input rejected: {NOT_RUNNING}"));
        }
        let event = Input {
            text: input.text.to_string(),
            run: self.run.clone(),
        };
        for h in self.with(|h| h.hooks.on_input) {
            let decision = call(
                h,
                "on_input",
                self.timeouts.input,
                h.handler.on_input(event.clone()),
            )
            .await;
            match decision {
                Ok(InputDecision::Pass) => {}
                Ok(InputDecision::Reject(reason)) => return InputDecision::Reject(reason),
                Ok(other) => return other,
                Err(missed) => {
                    tracing::warn!(run_id = %self.run.run_id, "rejecting the input (fail closed): {}", missed.reason());
                    return InputDecision::Reject(format!("input rejected: {}", missed.reason()));
                }
            }
        }
        InputDecision::Pass
    }

    async fn before_model(&mut self, turn: &TurnContext<'_>) -> TurnDecision {
        // Every tool the request offers — the agent's, its sources', every
        // extension's: a late tool may not take one of their names (as a
        // tool source it would win over an extension's tool).
        if let Some((_, late)) = &self.late {
            late.taken
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend(turn.tools.iter().map(|t| t.name.clone()));
        }
        if let Some(why) = self.take_failure() {
            return TurnDecision::Fail(why);
        }
        let handlers: Vec<&Registered> = self.with(|h| h.hooks.before_model).collect();
        let event = Turn::from_context(turn, &self.run, wants_messages(&handlers));
        let mut notes = Vec::new();
        for h in handlers {
            let decision = call(
                h,
                "before_model",
                self.timeouts.turn,
                h.handler
                    .before_model(for_handler(h, &event, |e| &mut e.messages)),
            )
            .await;
            match decision {
                Ok(TurnDecision::Continue) => {}
                Ok(TurnDecision::Note(note)) => notes.push(note),
                Ok(TurnDecision::Stop(reason)) => return TurnDecision::Stop(reason),
                Ok(TurnDecision::Fail(reason)) => {
                    let missed = Missed::Failed(format!(
                        "plugin handler `{}` failed in `before_model`: {reason}",
                        h.name
                    ));
                    self.skipped(&missed);
                }
                Ok(other) => return other,
                Err(missed) => self.skipped(&missed),
            }
            if let Some(why) = self.take_failure() {
                return TurnDecision::Fail(why);
            }
        }
        join_notes(notes)
    }

    async fn before_tool(&self, request: &ToolCallRequest<'_>) -> ToolDecision {
        // Checked per call too: the host may stop mid-run, taking every
        // plugin (and so every policy) with it.
        if self.closed || self.host.is_closed() {
            return ToolDecision::Deny(NOT_RUNNING.into());
        }
        if let Some(why) = self.pending() {
            return ToolDecision::Deny(format!("a required plugin handler failed this run: {why}"));
        }
        if self.unjudged {
            tracing::warn!(run_id = %self.run.run_id, tool = request.tool_name, "{UNJUDGED}; denying (require_policy)");
            return ToolDecision::Deny(UNJUDGED.into());
        }
        // A restricted handler judges its own tool's calls inside the tool
        // (see `LiveTool`), never here: here it could be handed a call of a
        // tool that only shares a name with one of its own.
        let handlers: Vec<&Registered> = self
            .with(|h| h.hooks.before_tool && !h.restricted)
            .collect();
        let mut event = ToolCall::from_request(
            request,
            &self.run,
            wants_messages(&handlers),
            self.owner_of(request.tool_name),
        );
        for h in handlers {
            let decision = call(
                h,
                "before_tool",
                self.timeouts.policy,
                h.handler
                    .before_tool(for_handler(h, &event, |e| &mut e.messages)),
            )
            .await;
            match decision {
                Ok(ToolDecision::Allow) => {}
                Ok(ToolDecision::Modify(args)) => event.args = args,
                Ok(ToolDecision::Deny(reason)) => return ToolDecision::Deny(reason),
                Err(missed) => {
                    tracing::warn!(run_id = %self.run.run_id, tool = request.tool_name, "denying the tool call (fail closed): {}", missed.reason());
                    return ToolDecision::Deny(missed.reason().to_string());
                }
            }
        }
        if &event.args == request.args {
            ToolDecision::Allow
        } else {
            ToolDecision::Modify(event.args)
        }
    }

    async fn after_tool(
        &self,
        request: &ToolCallRequest<'_>,
        output: &mut ToolOutput,
    ) -> Result<(), ExtensionError> {
        if let Some(why) = self.pending() {
            return Err(ExtensionError::new(why));
        }
        let event =
            ToolCall::from_request(request, &self.run, false, self.owner_of(request.tool_name));
        for h in self.with(|h| h.hooks.after_tool) {
            let edited = call(
                h,
                "after_tool",
                self.timeouts.policy,
                h.handler.after_tool(event.clone(), output.clone()),
            )
            .await;
            match edited {
                Ok(edited) => *output = edited,
                // The plugin went (a reload, say): not a failure, so a
                // required run goes on, but the result is not let through
                // unfiltered either.
                Err(Missed::Unavailable(why)) => {
                    tracing::warn!(run_id = %self.run.run_id, tool = request.tool_name, "withholding the result: {why}");
                    *output = ToolOutput::new(
                        yoagent::ToolResult {
                            content: vec![Content::Text {
                                text: format!("Tool result withheld: {why}."),
                            }],
                            details: serde_json::Value::Null,
                        },
                        true,
                    );
                    return Ok(());
                }
                // yoagent withholds the result (and fails a required run).
                Err(Missed::Failed(why)) => return Err(ExtensionError::new(why)),
            }
        }
        Ok(())
    }

    async fn on_stop(&mut self, stop: &StopContext<'_>) -> StopDecision {
        if let Some(why) = self.take_failure() {
            return StopDecision::Fail(why);
        }
        let handlers: Vec<&Registered> = self.with(|h| h.hooks.on_stop).collect();
        let event = Stop {
            answer: answer_text(stop.answer),
            continues: stop.continues,
            messages: wants_messages(&handlers).then(|| llm_messages(stop.messages.iter())),
            run: self.run.clone(),
        };
        let mut messages = Vec::new();
        for h in handlers {
            let decision = call(
                h,
                "on_stop",
                self.timeouts.policy,
                h.handler
                    .on_stop(for_handler(h, &event, |e| &mut e.messages)),
            )
            .await;
            match decision {
                Ok(StopDecision::Accept) => {}
                Ok(StopDecision::Continue(message)) => messages.push(message),
                Ok(StopDecision::Fail(reason)) => {
                    let missed = Missed::Failed(format!(
                        "plugin handler `{}` failed in `on_stop`: {reason}",
                        h.name
                    ));
                    self.skipped(&missed);
                }
                Ok(other) => return other,
                Err(missed) => self.skipped(&missed),
            }
            if let Some(why) = self.take_failure() {
                return StopDecision::Fail(why);
            }
        }
        if messages.is_empty() {
            StopDecision::Accept
        } else {
            StopDecision::Continue(messages.join("\n"))
        }
    }

    /// Never unwinds: yoagent would switch the whole extension's `on_event`
    /// off for the run, silencing bus publishing and every other handler. A
    /// failing handler is switched off alone and its failure recorded.
    /// Hand a recorded `tools` / `on_event` failure to yoagent, which asks
    /// at every boundary, the run's end included.
    fn take_failure(&self) -> Option<String> {
        if self.required {
            RunState::take_failure(self)
        } else {
            None
        }
    }

    fn on_event(&self, event: &AgentEvent) {
        if !self.closed {
            crate::events::publish(&self.host, &self.run, event);
        }
        // `AgentEnd` comes after `finish`: a handler the run's end reached
        // gets it even if its plugin has begun unloading since, so the run's
        // end is not lost to the race (#374).
        let last = matches!(event, AgentEvent::AgentEnd { .. });
        for observer in &self.events {
            let live = if last {
                observer.got_end.load(Ordering::Relaxed)
            } else {
                observer.handler.gate.is_available()
            };
            if observer.off.load(Ordering::Relaxed) || !live {
                continue;
            }
            let timer = observer.handler.time("on_event");
            let sent = std::panic::catch_unwind(AssertUnwindSafe(|| {
                if last {
                    observer.sink.send_last(event, &observer.handler.gate)
                } else {
                    observer.sink.send(event)
                }
            }));
            let failed = match sent {
                // A Rust handler panicked.
                Err(payload) => {
                    timer.done(Outcome::Failed);
                    Some(format!(
                        "plugin handler `{}` panicked in `on_event`: {} (not called again this run)",
                        observer.handler.name,
                        panic_text(&*payload)
                    ))
                }
                // A language plugin's delivery failed (its message says how).
                Ok(taken) => {
                    if taken {
                        timer.done(Outcome::Ok);
                    } else {
                        timer.discard();
                    }
                    observer.sink.failure()
                }
            };
            if let Some(why) = failed {
                observer.off.store(true, Ordering::Relaxed);
                self.skipped(&Missed::Failed(why));
            }
        }
    }

    async fn finish(&mut self, outcome: &RunOutcome) {
        let this = &*self;
        // Whom the run's `AgentEnd`, sent after this, goes to (see
        // `on_event`): for now every handler whose plugin is loaded, refined
        // below for those with a `finish` hook. Stored first, so it holds
        // if yoagent drops this future (it bounds `finish`).
        for observer in &this.events {
            let live = observer.handler.gate.is_available();
            observer.got_end.store(live, Ordering::Relaxed);
        }
        // Every event sent so far reaches its handler before `finish` does.
        let closing = this.events.iter().map(|o| async move {
            let flush = o.sink.flush();
            if let Some(limit) = this.timeouts.turn {
                if tokio::time::timeout(limit, flush).await.is_err() {
                    tracing::warn!(run_id = %this.run.run_id, handler = %o.handler.name, "event delivery did not drain within {limit:?}");
                }
            } else {
                flush.await;
            }
        });
        futures::future::join_all(closing).await;
        // After the flush, so a language plugin's late delivery failure counts.
        if let Some(why) = this.take_failure() {
            // Handed over through `after_tool` (an `Err`), the run already
            // failed on it. Otherwise no later decision point came (see the
            // module docs): too late to fail the run, so it is logged. One on
            // `AgentEnd`, sent after `finish`, is logged where it happens.
            let failed = matches!(
                outcome.end(),
                RunEnd::Failed {
                    extension: Some(_),
                    ..
                }
            );
            if !failed {
                tracing::error!(run_id = %this.run.run_id, "a required plugin handler failed with no later point to fail the run: {why}");
            }
        }
        let joined = this.joined();
        let finishing = this
            .with(|h| h.hooks.finish)
            .chain(joined.iter().filter(|h| h.hooks.finish))
            .map(|h| async move {
            // Sent to a loaded plugin, it learns that the run ended — even if
            // the call fails or the plugin unloads during it.
            let dispatched = h.gate.is_available();
            for observer in this.events.iter() {
                if Arc::ptr_eq(&observer.handler.handler, &h.handler) {
                    observer.got_end.store(dispatched, Ordering::Relaxed);
                }
            }
            let done = call(
                h,
                "finish",
                this.timeouts.turn,
                h.handler.finish(outcome.clone(), this.run.clone()),
            )
            .await;
            match done {
                Ok(()) => {}
                Err(Missed::Unavailable(why)) => {
                    tracing::debug!(run_id = %this.run.run_id, "skipping a handler's finish: {why}")
                }
                Err(Missed::Failed(why)) => tracing::warn!(run_id = %this.run.run_id, "{why}"),
            }
        });
        futures::future::join_all(finishing).await;
    }
}
