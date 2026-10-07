//! Handlers: what a plugin registers with the bridge.
//!
//! A [`Handler`] is a name plus whichever hooks it implements. The hooks
//! mirror yoagent's [`RunHooks`](yoagent::RunHooks), with owned, plain-data
//! arguments ([`ToolCall`], [`Turn`], [`Input`], [`Stop`], each carrying the
//! [`RunInfo`]) — the same shapes a TypeScript or Python plugin receives as
//! JSON. Hooks are added with the `with_*` builder methods, in a synchronous
//! form for quick checks and an `_async` form for hooks that await.
//!
//! One handler may add several closures to the same hook; they run in the
//! order added, combined as across handlers (a `Deny` wins, a `Modify` feeds
//! the next closure, notes are joined, ...). See the
//! [extension docs](crate::extension) for the combination rules.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use serde::Serialize;
use serde_json::Value;
use yoagent::extension::{
    ExtensionError, InputDecision, RunOutcome, StopDecision, ToolOutput, TurnDecision,
};
use yoagent::{AgentEvent, AgentTool, ToolDecision};

/// The run a hook is called for.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct RunInfo {
    /// Unique per run (yoagent's `RunContext::run_id`).
    pub run_id: String,
    /// The host's label for the run (`Agent::with_run_label`); delegated
    /// runs keep their parent's. Informational, not an identity.
    pub label: Option<String>,
    /// 0 for a top-level run, 1 for a sub-agent's run, and so on.
    pub depth: usize,
    /// For a delegated run: the tool call (in the calling run) that started it.
    pub delegated_by: Option<String>,
    /// For a delegated run: the calling run's id.
    pub parent_run_id: Option<String>,
}

impl RunInfo {
    /// A top-level run with this id (for testing a handler).
    pub fn new(run_id: impl Into<String>) -> Self {
        Self {
            run_id: run_id.into(),
            ..Self::default()
        }
    }

    /// With the host's label.
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// At this delegation depth.
    pub fn with_depth(mut self, depth: usize) -> Self {
        self.depth = depth;
        self
    }

    pub(crate) fn from_context(run: &yoagent::extension::RunContext<'_>) -> Self {
        Self {
            run_id: run.run_id.to_string(),
            label: run.label.map(str::to_string),
            depth: run.depth,
            delegated_by: run.delegated_by.map(str::to_string),
            parent_run_id: run.parent_run_id.map(str::to_string),
        }
    }
}

/// A pending tool call, as `before_tool` and `after_tool` see it. Every tool
/// call of the run is shown — the agent's own tools too, not only plugin
/// tools.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct ToolCall {
    /// The tool the model wants to run.
    pub tool: String,
    /// The provider-assigned id of the call.
    pub call_id: String,
    /// The arguments as they stand: rewritten by earlier handlers (or by the
    /// host's own middleware and extensions), if any did.
    pub args: Value,
    /// What the user asked for (yoagent's `ToolCallRequest::user_request`;
    /// the prose is not a stable format).
    pub user_request: Option<String>,
    /// The text of the user's latest message.
    pub latest_user_text: Option<String>,
    /// The run (flattened into the JSON object).
    #[serde(flatten)]
    pub run: RunInfo,
}

impl ToolCall {
    /// A call, for testing a handler outside an agent.
    pub fn new(call_id: impl Into<String>, tool: impl Into<String>, args: Value) -> Self {
        Self {
            tool: tool.into(),
            call_id: call_id.into(),
            args,
            user_request: None,
            latest_user_text: None,
            run: RunInfo::default(),
        }
    }

    /// With what the user asked for.
    pub fn with_user_request(mut self, request: impl Into<String>) -> Self {
        self.user_request = Some(request.into());
        self
    }

    /// With the text of the user's latest message.
    pub fn with_latest_user_text(mut self, text: impl Into<String>) -> Self {
        self.latest_user_text = Some(text.into());
        self
    }

    /// In this run.
    pub fn with_run(mut self, run: RunInfo) -> Self {
        self.run = run;
        self
    }

    pub(crate) fn from_request(call: &yoagent::ToolCallRequest<'_>, run: &RunInfo) -> Self {
        Self {
            tool: call.tool_name.to_string(),
            call_id: call.tool_call_id.to_string(),
            args: call.args.clone(),
            user_request: call.user_request(),
            latest_user_text: call.latest_user_text(),
            run: run.clone(),
        }
    }
}

/// A model request about to be sent, as `before_model` sees it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct Turn {
    /// The model id.
    pub model: String,
    /// What the user asked for.
    pub user_request: Option<String>,
    /// The text of the user's latest message.
    pub latest_user_text: Option<String>,
    /// Names of the tools offered on this request.
    pub tools: Vec<String>,
    #[serde(flatten)]
    pub run: RunInfo,
}

impl Turn {
    /// A turn, for testing a handler.
    pub fn new(model: impl Into<String>, tools: Vec<String>) -> Self {
        Self {
            model: model.into(),
            user_request: None,
            latest_user_text: None,
            tools,
            run: RunInfo::default(),
        }
    }

    /// With what the user asked for.
    pub fn with_user_request(mut self, request: impl Into<String>) -> Self {
        self.user_request = Some(request.into());
        self
    }

    pub(crate) fn from_context(turn: &yoagent::TurnContext<'_>, run: &RunInfo) -> Self {
        Self {
            model: turn.model.to_string(),
            user_request: turn.user_request(),
            latest_user_text: turn.latest_user_text(),
            tools: turn.tools.iter().map(|t| t.name.clone()).collect(),
            run: run.clone(),
        }
    }
}

/// A prompted run's input, as `on_input` sees it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct Input {
    /// Every user text block of the prompts, joined by newlines.
    pub text: String,
    #[serde(flatten)]
    pub run: RunInfo,
}

impl Input {
    /// An input, for testing a handler.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            run: RunInfo::default(),
        }
    }
}

/// The model's final answer, as `on_stop` sees it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct Stop {
    /// The answer's text blocks, joined.
    pub answer: String,
    /// How many times extensions have continued this run already.
    pub continues: usize,
    #[serde(flatten)]
    pub run: RunInfo,
}

impl Stop {
    /// A stop, for testing a handler.
    pub fn new(answer: impl Into<String>, continues: usize) -> Self {
        Self {
            answer: answer.into(),
            continues,
            run: RunInfo::default(),
        }
    }
}

type HookFuture<T> = BoxFuture<'static, Result<T, ExtensionError>>;
type ToolsFn = Arc<dyn Fn(RunInfo) -> HookFuture<Vec<Arc<dyn AgentTool>>> + Send + Sync>;
type BeforeToolFn = Arc<dyn Fn(ToolCall) -> HookFuture<ToolDecision> + Send + Sync>;
type AfterToolFn = Arc<dyn Fn(ToolCall, ToolOutput) -> HookFuture<ToolOutput> + Send + Sync>;
type BeforeModelFn = Arc<dyn Fn(Turn) -> HookFuture<TurnDecision> + Send + Sync>;
type InputFn = Arc<dyn Fn(Input) -> HookFuture<InputDecision> + Send + Sync>;
type StopFn = Arc<dyn Fn(Stop) -> HookFuture<StopDecision> + Send + Sync>;
type FinishFn = Arc<dyn Fn(RunOutcome, RunInfo) -> HookFuture<()> + Send + Sync>;
type EventFn = Arc<dyn Fn(&RunInfo, &AgentEvent) + Send + Sync>;

/// What a plugin registers: a name plus the hooks it implements.
///
/// Register one from a plugin's `apply` with
/// [`PluginCtxExt::register_handler`](crate::PluginCtxExt::register_handler),
/// or wrap it in an [`AgentPlugin`](crate::AgentPlugin). It is cheap to
/// clone (every hook is shared).
///
/// ```
/// use yoagent::ToolDecision;
/// use yoagent_rutis::Handler;
///
/// let handler = Handler::new("no-shell").with_before_tool(|call| {
///     if call.tool == "bash" {
///         ToolDecision::Deny("shell access is disabled".into())
///     } else {
///         ToolDecision::Allow
///     }
/// });
/// assert_eq!(handler.name(), "no-shell");
/// ```
#[derive(Clone)]
pub struct Handler {
    name: String,
    tools: Vec<Arc<dyn AgentTool>>,
    tools_fns: Vec<ToolsFn>,
    before_tool: Vec<BeforeToolFn>,
    after_tool: Vec<AfterToolFn>,
    before_model: Vec<BeforeModelFn>,
    on_input: Vec<InputFn>,
    on_stop: Vec<StopFn>,
    finish: Vec<FinishFn>,
    on_event: Vec<EventFn>,
}

fn ready<T: Send + 'static>(value: T) -> HookFuture<T> {
    Box::pin(async move { Ok(value) })
}

impl Handler {
    /// A handler named `name`, with no hooks. Names are unique across
    /// plugins: registering a name a live handler holds is refused.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            tools: Vec::new(),
            tools_fns: Vec::new(),
            before_tool: Vec::new(),
            after_tool: Vec::new(),
            before_model: Vec::new(),
            on_input: Vec::new(),
            on_stop: Vec::new(),
            finish: Vec::new(),
            on_event: Vec::new(),
        }
    }

    /// The handler's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Offer a tool on every run that starts while the plugin is loaded.
    /// Tool names offered this way are unique across plugins (checked at
    /// registration).
    pub fn with_tool(self, tool: impl AgentTool + 'static) -> Self {
        self.with_tool_arc(Arc::new(tool))
    }

    /// [`with_tool`](Self::with_tool) for a shared tool.
    pub fn with_tool_arc(mut self, tool: Arc<dyn AgentTool>) -> Self {
        self.tools.push(tool);
        self
    }

    /// Decide the tools to offer per run (the `tools` hook). Names are not
    /// checked at registration: on a clash the earlier handler's tool wins
    /// (logged), and the agent's own tools win over every plugin tool.
    pub fn with_tools<F>(mut self, tools: F) -> Self
    where
        F: Fn(&RunInfo) -> Vec<Arc<dyn AgentTool>> + Send + Sync + 'static,
    {
        self.tools_fns.push(Arc::new(move |run| ready(tools(&run))));
        self
    }

    /// Judge every tool call (`before_tool`): `Allow`, `Deny(reason)` (the
    /// model sees the reason) or `Modify(args)` (later handlers and the tool
    /// see the new arguments).
    pub fn with_before_tool<F>(self, check: F) -> Self
    where
        F: Fn(&ToolCall) -> ToolDecision + Send + Sync + 'static,
    {
        self.with_before_tool_async(move |call| {
            let decision = check(&call);
            async move { Ok(decision) }
        })
    }

    /// [`with_before_tool`](Self::with_before_tool) that awaits (a lookup,
    /// an approval). An `Err` denies the call.
    pub fn with_before_tool_async<F, Fut>(mut self, check: F) -> Self
    where
        F: Fn(ToolCall) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<ToolDecision, ExtensionError>> + Send + 'static,
    {
        self.before_tool
            .push(Arc::new(move |call| Box::pin(check(call))));
        self
    }

    /// Edit a tool call's output before the model, the history and the
    /// run's consumers see it (`after_tool`; redaction). An `Err` withholds
    /// the result. Declare
    /// [`filters_tool_output`](crate::RutisExtension::filters_tool_output) on
    /// the host's extension so partial output is withheld too.
    pub fn with_after_tool<F>(self, filter: F) -> Self
    where
        F: Fn(&ToolCall, &mut ToolOutput) -> Result<(), ExtensionError> + Send + Sync + 'static,
    {
        let filter = Arc::new(filter);
        self.with_after_tool_async(move |call, mut output| {
            let result = filter(&call, &mut output);
            async move { result.map(|()| output) }
        })
    }

    /// [`with_after_tool`](Self::with_after_tool) that awaits; returns the
    /// output to pass on.
    pub fn with_after_tool_async<F, Fut>(mut self, filter: F) -> Self
    where
        F: Fn(ToolCall, ToolOutput) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<ToolOutput, ExtensionError>> + Send + 'static,
    {
        self.after_tool
            .push(Arc::new(move |call, output| Box::pin(filter(call, output))));
        self
    }

    /// Before each model request (`before_model`): `Continue`,
    /// `Note(text)` (appended to the request's latest user turn, never
    /// stored), `Stop(reason)` (end the run like an execution limit) or
    /// `Fail(reason)`.
    pub fn with_before_model<F>(self, decide: F) -> Self
    where
        F: Fn(&Turn) -> TurnDecision + Send + Sync + 'static,
    {
        self.with_before_model_async(move |turn| {
            let decision = decide(&turn);
            async move { Ok(decision) }
        })
    }

    /// [`with_before_model`](Self::with_before_model) that awaits.
    pub fn with_before_model_async<F, Fut>(mut self, decide: F) -> Self
    where
        F: Fn(Turn) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<TurnDecision, ExtensionError>> + Send + 'static,
    {
        self.before_model
            .push(Arc::new(move |turn| Box::pin(decide(turn))));
        self
    }

    /// Judge a prompted run's input (`on_input`): `Pass` or `Reject(reason)`.
    pub fn with_on_input<F>(self, check: F) -> Self
    where
        F: Fn(&Input) -> InputDecision + Send + Sync + 'static,
    {
        self.with_on_input_async(move |input| {
            let decision = check(&input);
            async move { Ok(decision) }
        })
    }

    /// [`with_on_input`](Self::with_on_input) that awaits. An `Err` rejects.
    pub fn with_on_input_async<F, Fut>(mut self, check: F) -> Self
    where
        F: Fn(Input) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<InputDecision, ExtensionError>> + Send + 'static,
    {
        self.on_input
            .push(Arc::new(move |input| Box::pin(check(input))));
        self
    }

    /// When the model ends its answer (`on_stop`; a verifier): `Accept`,
    /// `Continue(message)` (sent to the model, another turn runs) or
    /// `Fail(reason)`.
    pub fn with_on_stop<F>(self, verify: F) -> Self
    where
        F: Fn(&Stop) -> StopDecision + Send + Sync + 'static,
    {
        self.with_on_stop_async(move |stop| {
            let decision = verify(&stop);
            async move { Ok(decision) }
        })
    }

    /// [`with_on_stop`](Self::with_on_stop) that awaits (runs tests, say).
    pub fn with_on_stop_async<F, Fut>(mut self, verify: F) -> Self
    where
        F: Fn(Stop) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<StopDecision, ExtensionError>> + Send + 'static,
    {
        self.on_stop
            .push(Arc::new(move |stop| Box::pin(verify(stop))));
        self
    }

    /// When the run ends, however it ends (`finish`).
    pub fn with_finish<F>(self, done: F) -> Self
    where
        F: Fn(&RunOutcome, &RunInfo) + Send + Sync + 'static,
    {
        self.with_finish_async(move |outcome, run| {
            done(&outcome, &run);
            async { Ok(()) }
        })
    }

    /// [`with_finish`](Self::with_finish) that awaits (flushes an audit log).
    pub fn with_finish_async<F, Fut>(mut self, done: F) -> Self
    where
        F: Fn(RunOutcome, RunInfo) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), ExtensionError>> + Send + 'static,
    {
        self.finish
            .push(Arc::new(move |outcome, run| Box::pin(done(outcome, run))));
        self
    }

    /// Observe every event of the run, in order, synchronously from the
    /// run's event observer: it must not block. For an observer decoupled
    /// from the run, listen on the bus instead
    /// ([`PluginCtxExt::on_agent_event`](crate::PluginCtxExt::on_agent_event)).
    pub fn with_on_event<F>(mut self, observe: F) -> Self
    where
        F: Fn(&RunInfo, &AgentEvent) + Send + Sync + 'static,
    {
        self.on_event.push(Arc::new(observe));
        self
    }
}

/// The hooks a registered handler implements.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Hooks {
    pub(crate) tools: bool,
    pub(crate) before_tool: bool,
    pub(crate) after_tool: bool,
    pub(crate) before_model: bool,
    pub(crate) on_input: bool,
    pub(crate) on_stop: bool,
    pub(crate) finish: bool,
    pub(crate) on_event: bool,
}

impl Hooks {
    pub(crate) fn names(&self) -> Vec<&'static str> {
        [
            (self.tools, "tools"),
            (self.before_tool, "before_tool"),
            (self.after_tool, "after_tool"),
            (self.before_model, "before_model"),
            (self.on_input, "on_input"),
            (self.on_stop, "on_stop"),
            (self.finish, "finish"),
            (self.on_event, "on_event"),
        ]
        .into_iter()
        .filter_map(|(has, name)| has.then_some(name))
        .collect()
    }
}

/// Delivers one run's events to one handler; dropped when the
/// run ends.
pub(crate) trait EventSink: Send + Sync {
    /// Must not block.
    fn send(&self, event: &AgentEvent);
    /// Wait (bounded by the caller) until every event sent so far was
    /// delivered. More may follow: yoagent sends `AgentEnd` after `finish`.
    fn flush(&self) -> BoxFuture<'static, ()> {
        Box::pin(async {})
    }
    /// A delivery that failed since (for handlers that deliver later).
    fn failure(&self) -> Option<String> {
        None
    }
}

/// A registered handler, whatever implements it: a Rust [`Handler`], or a
/// language plugin's object behind a session.
#[async_trait::async_trait]
pub(crate) trait HandlerImpl: Send + Sync + 'static {
    fn hooks(&self) -> Hooks;
    /// Tools offered on every run, checked for unique names at registration.
    fn static_tools(&self) -> Vec<Arc<dyn AgentTool>>;
    async fn tools(&self, run: RunInfo) -> Result<Vec<Arc<dyn AgentTool>>, ExtensionError>;
    async fn before_tool(&self, call: ToolCall) -> Result<ToolDecision, ExtensionError>;
    async fn after_tool(
        &self,
        call: ToolCall,
        output: ToolOutput,
    ) -> Result<ToolOutput, ExtensionError>;
    async fn before_model(&self, turn: Turn) -> Result<TurnDecision, ExtensionError>;
    async fn on_input(&self, input: Input) -> Result<InputDecision, ExtensionError>;
    async fn on_stop(&self, stop: Stop) -> Result<StopDecision, ExtensionError>;
    async fn finish(&self, outcome: RunOutcome, run: RunInfo) -> Result<(), ExtensionError>;
    /// This run's event delivery, if the handler observes events.
    /// `limit` bounds each delivery, for handlers that deliver remotely.
    fn events(&self, run: &RunInfo, limit: Option<Duration>) -> Option<Box<dyn EventSink>>;
}

struct ClosureEvents {
    run: RunInfo,
    observers: Vec<EventFn>,
}

impl EventSink for ClosureEvents {
    fn send(&self, event: &AgentEvent) {
        for observe in &self.observers {
            observe(&self.run, event);
        }
    }
}

#[async_trait::async_trait]
impl HandlerImpl for Handler {
    fn hooks(&self) -> Hooks {
        Hooks {
            tools: !self.tools_fns.is_empty(),
            before_tool: !self.before_tool.is_empty(),
            after_tool: !self.after_tool.is_empty(),
            before_model: !self.before_model.is_empty(),
            on_input: !self.on_input.is_empty(),
            on_stop: !self.on_stop.is_empty(),
            finish: !self.finish.is_empty(),
            on_event: !self.on_event.is_empty(),
        }
    }

    fn static_tools(&self) -> Vec<Arc<dyn AgentTool>> {
        self.tools.clone()
    }

    async fn tools(&self, run: RunInfo) -> Result<Vec<Arc<dyn AgentTool>>, ExtensionError> {
        let mut tools = Vec::new();
        for f in &self.tools_fns {
            tools.extend(f(run.clone()).await?);
        }
        Ok(tools)
    }

    async fn before_tool(&self, mut call: ToolCall) -> Result<ToolDecision, ExtensionError> {
        let original = call.args.clone();
        for check in &self.before_tool {
            match check(call.clone()).await? {
                ToolDecision::Allow => {}
                ToolDecision::Modify(args) => call.args = args,
                ToolDecision::Deny(reason) => return Ok(ToolDecision::Deny(reason)),
            }
        }
        Ok(if call.args == original {
            ToolDecision::Allow
        } else {
            ToolDecision::Modify(call.args)
        })
    }

    async fn after_tool(
        &self,
        call: ToolCall,
        mut output: ToolOutput,
    ) -> Result<ToolOutput, ExtensionError> {
        for filter in &self.after_tool {
            output = filter(call.clone(), output).await?;
        }
        Ok(output)
    }

    async fn before_model(&self, turn: Turn) -> Result<TurnDecision, ExtensionError> {
        let mut notes = Vec::new();
        for decide in &self.before_model {
            match decide(turn.clone()).await? {
                TurnDecision::Note(note) => notes.push(note),
                TurnDecision::Continue => {}
                other => return Ok(other),
            }
        }
        Ok(join_notes(notes))
    }

    async fn on_input(&self, input: Input) -> Result<InputDecision, ExtensionError> {
        for check in &self.on_input {
            match check(input.clone()).await? {
                InputDecision::Pass => {}
                other => return Ok(other),
            }
        }
        Ok(InputDecision::Pass)
    }

    async fn on_stop(&self, stop: Stop) -> Result<StopDecision, ExtensionError> {
        let mut messages = Vec::new();
        for verify in &self.on_stop {
            match verify(stop.clone()).await? {
                StopDecision::Accept => {}
                StopDecision::Continue(message) => messages.push(message),
                other => return Ok(other),
            }
        }
        Ok(if messages.is_empty() {
            StopDecision::Accept
        } else {
            StopDecision::Continue(messages.join("\n"))
        })
    }

    async fn finish(&self, outcome: RunOutcome, run: RunInfo) -> Result<(), ExtensionError> {
        for done in &self.finish {
            done(outcome.clone(), run.clone()).await?;
        }
        Ok(())
    }

    fn events(&self, run: &RunInfo, _limit: Option<Duration>) -> Option<Box<dyn EventSink>> {
        (!self.on_event.is_empty()).then(|| {
            Box::new(ClosureEvents {
                run: run.clone(),
                observers: self.on_event.clone(),
            }) as Box<dyn EventSink>
        })
    }
}

/// Notes as one decision: none → `Continue`, else one note per line.
pub(crate) fn join_notes(notes: Vec<String>) -> TurnDecision {
    let notes: Vec<String> = notes.into_iter().filter(|n| !n.trim().is_empty()).collect();
    if notes.is_empty() {
        TurnDecision::Continue
    } else {
        TurnDecision::Note(notes.join("\n"))
    }
}
