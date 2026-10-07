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
//! | `on_event` | All, in order, synchronously | Switched off for the run (a required extension fails the run) |
//! | `finish` | All, concurrently | Logged |
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

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::FutureExt;
use yoagent::extension::{
    ExtensionError, InputContext, InputDecision, RunContext, RunOutcome, StopContext, StopDecision,
    ToolOutput, TurnDecision,
};
use yoagent::{
    AgentEvent, AgentTool, Content, Extension, ExtensionMode, Message, RunHooks, ToolCallRequest,
    ToolDecision, TurnContext,
};

use crate::handler::{join_notes, EventSink, Input, RunInfo, Stop, ToolCall, Turn};
use crate::host::{Host, NOT_RUNNING};
use crate::registry::{LiveTool, Registered, Registry};

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
}

#[derive(Clone, Copy)]
struct Timeouts {
    policy: Option<Duration>,
    input: Option<Duration>,
    turn: Option<Duration>,
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
    /// run with yoagent's `[Extension failed: ...]` error. By default they
    /// are logged and the handler is skipped. A failing `before_tool` denies
    /// the call and a failing `on_input` rejects the input either way.
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
        let handlers = if closed {
            tracing::warn!(run_id = %info.run_id, "{NOT_RUNNING}: tool calls are denied and input rejected");
            Vec::new()
        } else {
            self.registry.snapshot()
        };
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
                    })
            })
            .collect();
        let unjudged = self.require_policy && !handlers.iter().any(|h| h.hooks.before_tool);
        Ok(Box::new(RunState {
            host: self.host.clone(),
            run: info,
            required: self.mode == ExtensionMode::Required,
            closed,
            unjudged,
            timeouts: self.timeouts,
            handlers,
            events,
            failure: Mutex::new(None),
        }))
    }
}

/// One handler's event delivery for the run.
struct Observer {
    handler: Registered,
    sink: Box<dyn EventSink>,
    /// Panicked once: not called again this run.
    off: AtomicBool,
}

/// One run's hooks over the snapshot of handlers.
struct RunState {
    host: Host,
    run: RunInfo,
    required: bool,
    closed: bool,
    unjudged: bool,
    timeouts: Timeouts,
    handlers: Vec<Registered>,
    events: Vec<Observer>,
    /// A required failure recorded where it could not end the run: one in
    /// `before_model` / `on_stop` (returned as `Fail` right away), or, in a
    /// build where panics abort, one in `tools` / `on_event` (see
    /// [`RunState::escalate`]). Tool calls are denied while it is pending.
    failure: Mutex<Option<String>>,
}

/// Why a handler's hook gave no answer.
enum Missed {
    /// Its plugin unloaded before the call, or during it.
    Unavailable(String),
    /// It errored, panicked or timed out.
    Failed(String),
}

impl Missed {
    fn reason(&self) -> &str {
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
async fn call<T>(
    h: &Registered,
    hook: &str,
    limit: Option<Duration>,
    fut: impl std::future::Future<Output = Result<T, ExtensionError>>,
) -> Result<T, Missed> {
    if !h.gate.is_available() {
        return Err(Missed::Unavailable(format!(
            "plugin handler `{}` is no longer available: its plugin unloaded",
            h.name
        )));
    }
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

    /// A handler of `tools` or `on_event` failed. yoagent learns of a failure
    /// there only from a panic, so a required one is handed over as one (a
    /// `resume_unwind`, which does not run the panic hook): yoagent then
    /// stops tool calls not started yet and fails the run at its next
    /// boundary, whatever that is. Where panics abort, it is recorded
    /// instead and acted on at the next tool call, model request or stop.
    fn escalate(&self, missed: Missed) {
        match missed {
            Missed::Failed(why) if self.required => {
                tracing::error!(run_id = %self.run.run_id, "{why}");
                #[cfg(panic = "unwind")]
                std::panic::resume_unwind(Box::new(why));
                #[cfg(not(panic = "unwind"))]
                self.record(why);
            }
            other => self.skipped(&other),
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
        let mut taken = std::collections::HashSet::new();
        let mut offer = |tool: Arc<dyn AgentTool>, h: &Registered, tools: &mut Vec<_>| {
            if taken.insert(tool.name().to_string()) {
                tools.push(Arc::new(LiveTool {
                    inner: tool,
                    gate: h.gate.clone(),
                }) as Arc<dyn AgentTool>);
            } else {
                tracing::warn!(
                    run_id = %self.run.run_id,
                    tool = tool.name(),
                    handler = %h.name,
                    "a plugin tool's name is taken by an earlier handler's tool; dropped for this run"
                );
            }
        };
        for h in &self.handlers {
            if !h.gate.is_available() {
                tracing::debug!(run_id = %self.run.run_id, handler = %h.name, "its plugin unloaded; offering none of its tools");
                continue;
            }
            for tool in h.handler.static_tools() {
                offer(tool, h, &mut tools);
            }
            if h.hooks.tools {
                match call(
                    h,
                    "tools",
                    self.timeouts.turn,
                    h.handler.tools(self.run.clone()),
                )
                .await
                {
                    Ok(dynamic) => {
                        for tool in dynamic {
                            offer(tool, h, &mut tools);
                        }
                    }
                    Err(missed) => self.escalate(missed),
                }
            }
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
        if let Some(why) = self.take_failure() {
            return TurnDecision::Fail(why);
        }
        let event = Turn::from_context(turn, &self.run);
        let mut notes = Vec::new();
        for h in self.with(|h| h.hooks.before_model) {
            let decision = call(
                h,
                "before_model",
                self.timeouts.turn,
                h.handler.before_model(event.clone()),
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
        let mut event = ToolCall::from_request(request, &self.run);
        for h in self.with(|h| h.hooks.before_tool) {
            let decision = call(
                h,
                "before_tool",
                self.timeouts.policy,
                h.handler.before_tool(event.clone()),
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
        let event = ToolCall::from_request(request, &self.run);
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
                // yoagent withholds the result (and fails a required run).
                Err(missed) => return Err(ExtensionError::new(missed.reason())),
            }
        }
        Ok(())
    }

    async fn on_stop(&mut self, stop: &StopContext<'_>) -> StopDecision {
        if let Some(why) = self.take_failure() {
            return StopDecision::Fail(why);
        }
        let event = Stop {
            answer: answer_text(stop.answer),
            continues: stop.continues,
            run: self.run.clone(),
        };
        let mut messages = Vec::new();
        for h in self.with(|h| h.hooks.on_stop) {
            let decision = call(
                h,
                "on_stop",
                self.timeouts.policy,
                h.handler.on_stop(event.clone()),
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

    fn on_event(&self, event: &AgentEvent) {
        if !self.closed {
            crate::events::publish(&self.host, &self.run, event);
        }
        for observer in &self.events {
            if observer.off.load(Ordering::Relaxed) || !observer.handler.gate.is_available() {
                continue;
            }
            let sent = std::panic::catch_unwind(AssertUnwindSafe(|| observer.sink.send(event)));
            if let Err(payload) = sent {
                observer.off.store(true, Ordering::Relaxed);
                let missed = Missed::Failed(format!(
                    "plugin handler `{}` panicked in `on_event`: {} (not called again this run)",
                    observer.handler.name,
                    panic_text(&*payload)
                ));
                self.escalate(missed);
            }
        }
    }

    async fn finish(&mut self, outcome: &RunOutcome) {
        if let Some(why) = self.take_failure() {
            // Only where panics abort: yoagent could not be told in time.
            tracing::error!(run_id = %self.run.run_id, "a required plugin handler failed after the run's last decision point: {why}");
        }
        let this = &*self;
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
        let finishing = this.with(|h| h.hooks.finish).map(|h| async move {
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
