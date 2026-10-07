//! Pins the order in which the existing hooks fire, interleaved with the
//! events, on the `prompt` and `continue_loop` paths: a plain answer, a tool
//! call, a refusal, provider errors (in the response, and before any output),
//! a retried request, cancellations, a turn limit, a rejected input, hook
//! chains, a sourced tool and a turn hook's note.
//!
//! The `Extension` contract (#241) re-routes the trait hooks (input filters,
//! turn hooks, tool middleware, tool sources) through one dispatch path and
//! leaves the closures (`on_before_turn`, `on_after_turn`, `on_error`) where
//! they are. These tests must pass unchanged across that refactor: a
//! difference here is a behaviour change for every existing user.
//!
//! Each hook records how many events had been sent when it fired, so the
//! timeline below is exact, not a race between the hook log and a consumer
//! draining the channel.
//!
//! In the timelines, the `MessageStart` of an assistant message shows
//! `assistant/Stop`: that is the placeholder the stream starts with, before
//! the real stop reason is known at `MessageEnd`.

use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use yoagent::context::ExecutionLimits;
use yoagent::provider::{ModelConfig, ProviderError, StreamConfig, StreamEvent, StreamProvider};
use yoagent::*;

// ---------------------------------------------------------------------------
// Recorder: one timeline of hook calls and events
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Recorder {
    /// `(events sent so far, hook)`.
    hooks: Mutex<Vec<(usize, String)>>,
    /// The run's event channel, left undrained until the run ends so its
    /// length is the number of events sent.
    rx: Mutex<Option<mpsc::UnboundedReceiver<AgentEvent>>>,
    /// The run's cancel token, as the provider receives it, so a hook or a
    /// tool can cancel the run.
    cancel: Mutex<Option<CancellationToken>>,
}

impl Recorder {
    fn cancel_run(&self) {
        self.cancel
            .lock()
            .unwrap()
            .as_ref()
            .expect("a provider call happened")
            .cancel();
    }

    fn hook(&self, name: impl Into<String>) {
        let sent = self.rx.lock().unwrap().as_ref().map_or(0, |rx| rx.len());
        self.hooks.lock().unwrap().push((sent, name.into()));
    }

    /// The hooks and events, in the order they happened.
    fn timeline(&self) -> Vec<String> {
        let mut rx = self.rx.lock().unwrap().take().expect("run started");
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(describe(&event));
        }
        let hooks = self.hooks.lock().unwrap();
        let mut out = Vec::new();
        let mut h = hooks.iter().peekable();
        for (i, event) in events.into_iter().enumerate() {
            while let Some((_, name)) = h.next_if(|(sent, _)| *sent <= i) {
                out.push(name.clone());
            }
            out.push(event);
        }
        out.extend(h.map(|(_, name)| name.clone()));
        // Deltas say nothing about ordering; keep the first of a run of them.
        out.dedup_by(|a, b| a == "event:MessageUpdate" && b == "event:MessageUpdate");
        out
    }
}

fn describe_message(message: &AgentMessage) -> String {
    let text_of = |content: &[Content]| {
        content.iter().find_map(|c| match c {
            Content::Text { text } => Some(text.clone()),
            _ => None,
        })
    };
    let marker = |content: &[Content]| match text_of(content) {
        Some(t) if t.starts_with(agent_loop::AGENT_STOPPED_PREFIX) => format!(" {t:?}"),
        _ => String::new(),
    };
    match message.as_llm() {
        Some(Message::User { content, .. }) => format!("user{}", marker(content)),
        Some(Message::Assistant {
            content,
            stop_reason,
            ..
        }) => format!("assistant/{stop_reason:?}{}", marker(content)),
        Some(Message::ToolResult { is_error, .. }) => format!("toolResult/error={is_error}"),
        None => format!("extension/{}", message.role()),
    }
}

fn describe(event: &AgentEvent) -> String {
    match event {
        AgentEvent::MessageStart { message } => {
            format!("event:MessageStart {}", describe_message(message))
        }
        AgentEvent::MessageUpdate { .. } => "event:MessageUpdate".into(),
        AgentEvent::MessageEnd { message } => {
            format!("event:MessageEnd {}", describe_message(message))
        }
        AgentEvent::ToolExecutionStart { tool_name, .. } => {
            format!("event:ToolExecutionStart {tool_name}")
        }
        AgentEvent::ToolExecutionEnd {
            tool_name,
            is_error,
            ..
        } => format!("event:ToolExecutionEnd {tool_name} error={is_error}"),
        AgentEvent::AgentEnd { messages, .. } => {
            let messages: Vec<String> = messages.iter().map(describe_message).collect();
            format!("event:AgentEnd [{}]", messages.join(", "))
        }
        AgentEvent::TurnEnd { tool_results, .. } => {
            format!("event:TurnEnd tool_results={}", tool_results.len())
        }
        other => {
            // The variant name, from its Debug form.
            let debug = format!("{other:?}");
            let name = debug
                .split(|c: char| !c.is_alphanumeric())
                .next()
                .unwrap_or_default();
            format!("event:{name}")
        }
    }
}

// ---------------------------------------------------------------------------
// Scripted provider, tools and hooks
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Step {
    /// A plain answer.
    Text,
    /// One call to the named tool.
    Tool(&'static str),
    /// A refused response carrying a tool call.
    RefuseWithTool,
    /// A failed response (`StopReason::Error`), as an SSE-embedded error arrives.
    Error,
    /// A non-retryable provider error before any output.
    Fail,
    /// A retryable provider error before any output.
    Retryable,
    /// Cancel the run before any output.
    Cancel,
    /// Start streaming text, then cancel the run.
    StartThenCancel,
    /// Cancel the run, then return a tool call: the cancel lands before tools start.
    CancelThenTool,
    /// Return an answer without streaming any event.
    Silent,
    /// Stream `Done` without `Start`.
    DoneOnly,
    /// Call these tools, in this order, in one response.
    Tools(&'static [&'static str]),
}

struct Scripted {
    steps: Mutex<Vec<Step>>,
    rec: Arc<Recorder>,
    /// Record the text of the latest user message each request carries.
    show_last_user: bool,
}

fn tool_call_message(name: &str, stop_reason: StopReason) -> Message {
    tool_calls_message(&[name], stop_reason)
}

fn tool_calls_message(names: &[&str], stop_reason: StopReason) -> Message {
    Message::assistant(
        names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                Content::tool_call(format!("call-{}", i + 1), *name, serde_json::json!({}))
            })
            .collect(),
        stop_reason,
        "mock",
        "mock",
        Usage::default(),
    )
}

fn send_tool_call(tx: &mpsc::UnboundedSender<StreamEvent>, name: &str) {
    send_tool_calls(tx, &[name]);
}

fn send_tool_calls(tx: &mpsc::UnboundedSender<StreamEvent>, names: &[&str]) {
    for (i, name) in names.iter().enumerate() {
        let _ = tx.send(StreamEvent::ToolCallStart {
            content_index: i,
            id: format!("call-{}", i + 1),
            name: (*name).into(),
        });
        let _ = tx.send(StreamEvent::ToolCallEnd { content_index: i });
    }
}

fn last_user_text(messages: &[Message]) -> String {
    messages
        .iter()
        .rev()
        .find_map(|m| match m {
            Message::User { content, .. } => Some(
                content
                    .iter()
                    .filter_map(|c| match c {
                        Content::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" | "),
            ),
            _ => None,
        })
        .unwrap_or_default()
}

#[async_trait::async_trait]
impl StreamProvider for Scripted {
    async fn stream(
        &self,
        config: StreamConfig,
        tx: mpsc::UnboundedSender<StreamEvent>,
        cancel: CancellationToken,
    ) -> Result<Message, ProviderError> {
        if self.show_last_user {
            self.rec.hook(format!(
                "provider.stream last_user={:?}",
                last_user_text(&config.messages)
            ));
        } else {
            self.rec.hook("provider.stream");
        }
        *self.rec.cancel.lock().unwrap() = Some(cancel.clone());
        let step = self.steps.lock().unwrap().remove(0);
        match step {
            Step::Cancel => {
                cancel.cancel();
                return Err(ProviderError::Cancelled);
            }
            Step::Fail => return Err(ProviderError::Api("bad request".into())),
            Step::Retryable => return Err(ProviderError::Network("connection reset".into())),
            Step::DoneOnly => {
                let message = Message::assistant(
                    vec![Content::Text {
                        text: "done".into(),
                    }],
                    StopReason::Stop,
                    "mock",
                    "mock",
                    Usage::default(),
                );
                let _ = tx.send(StreamEvent::Done {
                    message: message.clone(),
                });
                return Ok(message);
            }
            Step::Silent => {
                return Ok(Message::assistant(
                    vec![Content::Text {
                        text: "done".into(),
                    }],
                    StopReason::Stop,
                    "mock",
                    "mock",
                    Usage::default(),
                ))
            }
            _ => {}
        }
        let _ = tx.send(StreamEvent::Start);
        let message = match step {
            Step::Text => {
                let _ = tx.send(StreamEvent::TextDelta {
                    content_index: 0,
                    delta: "done".into(),
                });
                Message::assistant(
                    vec![Content::Text {
                        text: "done".into(),
                    }],
                    StopReason::Stop,
                    "mock",
                    "mock",
                    Usage::default(),
                )
            }
            Step::Tool(name) => {
                send_tool_call(&tx, name);
                tool_call_message(name, StopReason::ToolUse)
            }
            Step::Tools(names) => {
                send_tool_calls(&tx, names);
                tool_calls_message(names, StopReason::ToolUse)
            }
            Step::RefuseWithTool => {
                send_tool_call(&tx, "probe");
                tool_call_message("probe", StopReason::Refusal)
            }
            Step::Error => {
                Message::assistant(vec![], StopReason::Error, "mock", "mock", Usage::default())
                    .with_error_message("upstream failed")
            }
            Step::StartThenCancel => {
                let _ = tx.send(StreamEvent::TextDelta {
                    content_index: 0,
                    delta: "partial".into(),
                });
                cancel.cancel();
                return Err(ProviderError::Cancelled);
            }
            Step::CancelThenTool => {
                send_tool_call(&tx, "probe");
                cancel.cancel();
                tool_call_message("probe", StopReason::ToolUse)
            }
            Step::Cancel | Step::Fail | Step::Retryable | Step::Silent | Step::DoneOnly => {
                unreachable!()
            }
        };
        let _ = tx.send(StreamEvent::Done {
            message: message.clone(),
        });
        Ok(message)
    }
}

struct Probe {
    name: &'static str,
    rec: Arc<Recorder>,
    /// Cancel the run while executing.
    cancels: bool,
}

#[async_trait::async_trait]
impl AgentTool for Probe {
    fn name(&self) -> &str {
        self.name
    }
    fn label(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "Records that it ran"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        self.rec
            .hook(format!("tool.execute {} args={params}", self.name));
        if self.cancels {
            self.rec.cancel_run();
        }
        Ok(ToolResult {
            content: vec![Content::Text { text: "ok".into() }],
            details: serde_json::Value::Null,
        })
    }
}

/// What an input filter returns.
#[derive(Clone, Copy)]
enum Outcome {
    Pass,
    Warn,
    Reject,
}

impl Outcome {
    fn result(self) -> FilterResult {
        match self {
            Outcome::Pass => FilterResult::Pass,
            Outcome::Warn => FilterResult::Warn("careful".into()),
            Outcome::Reject => FilterResult::Reject("no".into()),
        }
    }
}

/// An input filter, sync or async, in installation order.
#[derive(Clone, Copy)]
enum Filter {
    Sync(Outcome),
    Async(Outcome),
}

struct SyncFilter(Arc<Recorder>, Outcome);
impl InputFilter for SyncFilter {
    fn filter(&self, _text: &str) -> FilterResult {
        self.0.hook("input_filter.sync");
        self.1.result()
    }
}

struct AsyncFilterHook(Arc<Recorder>, Outcome);
#[async_trait::async_trait]
impl AsyncInputFilter for AsyncFilterHook {
    async fn filter(&self, _text: &str) -> FilterResult {
        self.0.hook("input_filter.async");
        self.1.result()
    }
}

struct Hook(Arc<Recorder>, Option<&'static str>);
#[async_trait::async_trait]
impl TurnHook for Hook {
    async fn before_turn(&self, _turn: &TurnContext<'_>) -> Option<String> {
        self.0.hook("turn_hook");
        self.1.map(String::from)
    }
}

/// What a middleware decides.
#[derive(Clone)]
enum Verdict {
    Allow,
    Modify(serde_json::Value),
    Deny,
    /// Cancel the run, then allow: the run is cancelled while middleware
    /// decides.
    CancelThenAllow,
}

struct Middleware(Arc<Recorder>, usize, Verdict);
#[async_trait::async_trait]
impl ToolMiddleware for Middleware {
    async fn before_tool(&self, call: &ToolCallRequest<'_>) -> ToolDecision {
        self.0.hook(format!(
            "middleware{} {} args={}",
            self.1, call.tool_name, call.args
        ));
        match &self.2 {
            Verdict::Allow => ToolDecision::Allow,
            Verdict::Modify(args) => ToolDecision::Modify(args.clone()),
            Verdict::Deny => ToolDecision::Deny("not allowed".into()),
            Verdict::CancelThenAllow => {
                self.0.cancel_run();
                ToolDecision::Allow
            }
        }
    }
}

/// A tool source; with `Some(name)` it offers a tool of that name.
struct Source(Arc<Recorder>, Option<&'static str>);
#[async_trait::async_trait]
impl ToolSource for Source {
    async fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        self.0.hook("tool_source");
        match self.1 {
            Some(name) => vec![Arc::new(Probe {
                name,
                rec: self.0.clone(),
                cancels: false,
            })],
            None => vec![],
        }
    }
}

/// One run with every hook installed. The defaults are the common case; each
/// test changes what it is about.
struct Rig {
    steps: Vec<Step>,
    limits: Option<ExecutionLimits>,
    filters: Vec<Filter>,
    middlewares: Vec<Verdict>,
    sourced_tool: Option<&'static str>,
    note: Option<&'static str>,
    show_last_user: bool,
    /// Continue from a history holding this user message, instead of prompting.
    continue_from: Option<&'static str>,
    /// `on_before_turn` returns `false` on this turn.
    stop_at_turn: Option<usize>,
    strategy: Option<ToolExecutionStrategy>,
}

fn rig(steps: &[Step]) -> Rig {
    Rig {
        steps: steps.to_vec(),
        limits: None,
        filters: vec![Filter::Sync(Outcome::Pass), Filter::Async(Outcome::Pass)],
        middlewares: vec![Verdict::Allow],
        sourced_tool: None,
        note: None,
        show_last_user: false,
        continue_from: None,
        stop_at_turn: None,
        strategy: None,
    }
}

impl Rig {
    async fn run(self) -> Vec<String> {
        let rec = Arc::new(Recorder::default());
        let provider = Scripted {
            steps: Mutex::new(self.steps),
            rec: rec.clone(),
            show_last_user: self.show_last_user,
        };
        let (r1, r2, r3) = (rec.clone(), rec.clone(), rec.clone());
        let stop_at_turn = self.stop_at_turn;
        let mut agent = Agent::from_provider(provider, ModelConfig::mock())
            .with_tools(vec![
                Box::new(Probe {
                    name: "probe",
                    rec: rec.clone(),
                    cancels: false,
                }),
                Box::new(Probe {
                    name: "canceller",
                    rec: rec.clone(),
                    cancels: true,
                }),
            ])
            .with_tool_source(Source(rec.clone(), self.sourced_tool))
            .with_turn_hook(Hook(rec.clone(), self.note))
            .with_retry_config(yoagent::RetryConfig {
                max_retries: 2,
                initial_delay_ms: 1,
                backoff_multiplier: 1.0,
                max_delay_ms: 1,
            })
            .on_before_turn(move |_, turn| {
                r1.hook(format!("on_before_turn {turn}"));
                stop_at_turn != Some(turn)
            })
            .on_after_turn(move |_, _| r2.hook("on_after_turn"))
            .on_error(move |e| r3.hook(format!("on_error {e:?}")));
        for filter in self.filters {
            agent = match filter {
                Filter::Sync(o) => agent.with_input_filter(SyncFilter(rec.clone(), o)),
                Filter::Async(o) => agent.with_async_input_filter(AsyncFilterHook(rec.clone(), o)),
            };
        }
        for (i, verdict) in self.middlewares.into_iter().enumerate() {
            agent = agent.with_tool_middleware(Middleware(rec.clone(), i, verdict));
        }
        if let Some(limits) = self.limits {
            agent = agent.with_execution_limits(limits);
        }
        if let Some(strategy) = self.strategy {
            agent = agent.with_tool_execution(strategy);
        }
        let (tx, rx) = mpsc::unbounded_channel();
        *rec.rx.lock().unwrap() = Some(rx);
        match self.continue_from {
            Some(text) => {
                agent = agent.with_messages(vec![AgentMessage::Llm(Message::user(text))]);
                agent.continue_loop_with_sender(tx).await;
            }
            None => agent.prompt_with_sender("go", tx).await,
        }
        rec.timeline()
    }
}

fn lines(expected: &str) -> Vec<String> {
    expected
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

fn assert_timeline(actual: Vec<String>, expected: &str) {
    let expected = lines(expected);
    assert_eq!(
        actual,
        expected,
        "\nactual timeline:\n{}\n",
        actual.join("\n")
    );
}

// ---------------------------------------------------------------------------
// The pinned orders
// ---------------------------------------------------------------------------

#[tokio::test]
async fn plain_answer() {
    assert_timeline(
        rig(&[Step::Text]).run().await,
        r#"
        tool_source
        event:AgentStart
        input_filter.sync
        input_filter.async
        event:TurnStart
        event:MessageStart user
        event:MessageEnd user
        on_before_turn 0
        turn_hook
        provider.stream
        event:MessageStart assistant/Stop
        event:MessageUpdate
        event:MessageEnd assistant/Stop
        on_after_turn
        event:TurnEnd tool_results=0
        event:AgentEnd [user, assistant/Stop]
        "#,
    );
}

#[tokio::test]
async fn tool_call_then_answer() {
    assert_timeline(
        rig(&[Step::Tool("probe"), Step::Text]).run().await,
        r#"
        tool_source
        event:AgentStart
        input_filter.sync
        input_filter.async
        event:TurnStart
        event:MessageStart user
        event:MessageEnd user
        on_before_turn 0
        turn_hook
        provider.stream
        event:MessageStart assistant/Stop
        event:MessageEnd assistant/ToolUse
        middleware0 probe args={}
        event:ToolExecutionStart probe
        tool.execute probe args={}
        event:ToolExecutionEnd probe error=false
        event:MessageStart toolResult/error=false
        event:MessageEnd toolResult/error=false
        on_after_turn
        event:TurnEnd tool_results=1
        event:TurnStart
        on_before_turn 1
        turn_hook
        provider.stream
        event:MessageStart assistant/Stop
        event:MessageUpdate
        event:MessageEnd assistant/Stop
        on_after_turn
        event:TurnEnd tool_results=0
        event:AgentEnd [user, assistant/ToolUse, toolResult/error=false, assistant/Stop]
        "#,
    );
}

#[tokio::test]
async fn refusal_with_a_tool_call() {
    assert_timeline(
        rig(&[Step::RefuseWithTool]).run().await,
        r#"
        tool_source
        event:AgentStart
        input_filter.sync
        input_filter.async
        event:TurnStart
        event:MessageStart user
        event:MessageEnd user
        on_before_turn 0
        turn_hook
        provider.stream
        event:MessageStart assistant/Stop
        event:MessageEnd assistant/Refusal
        event:ToolExecutionStart probe
        event:ToolExecutionEnd probe error=true
        event:MessageStart toolResult/error=true
        event:MessageEnd toolResult/error=true
        on_after_turn
        event:TurnEnd tool_results=1
        event:AgentEnd [user, assistant/Refusal, toolResult/error=true]
        "#,
    );
}

#[tokio::test]
async fn provider_error_in_the_response() {
    assert_timeline(
        rig(&[Step::Error]).run().await,
        r#"
        tool_source
        event:AgentStart
        input_filter.sync
        input_filter.async
        event:TurnStart
        event:MessageStart user
        event:MessageEnd user
        on_before_turn 0
        turn_hook
        provider.stream
        event:MessageStart assistant/Stop
        event:MessageEnd assistant/Error
        on_error "upstream failed"
        on_after_turn
        event:TurnEnd tool_results=0
        event:AgentEnd [user, assistant/Error]
        "#,
    );
}

#[tokio::test]
async fn provider_error_before_any_output() {
    // Nothing had streamed, but the failed message is appended to the
    // history, so it is announced (#243).
    assert_timeline(
        rig(&[Step::Fail]).run().await,
        r#"
        tool_source
        event:AgentStart
        input_filter.sync
        input_filter.async
        event:TurnStart
        event:MessageStart user
        event:MessageEnd user
        on_before_turn 0
        turn_hook
        provider.stream
        event:MessageStart assistant/Error
        event:MessageEnd assistant/Error
        on_error "API error: bad request"
        on_after_turn
        event:TurnEnd tool_results=0
        event:AgentEnd [user, assistant/Error]
        "#,
    );
}

/// The turn hook wraps the provider inside the retry loop, so it runs once
/// per attempt, not once per turn.
#[tokio::test]
async fn retried_request() {
    assert_timeline(
        rig(&[Step::Retryable, Step::Text]).run().await,
        r#"
        tool_source
        event:AgentStart
        input_filter.sync
        input_filter.async
        event:TurnStart
        event:MessageStart user
        event:MessageEnd user
        on_before_turn 0
        turn_hook
        provider.stream
        event:ProviderRetry
        turn_hook
        provider.stream
        event:MessageStart assistant/Stop
        event:MessageUpdate
        event:MessageEnd assistant/Stop
        on_after_turn
        event:TurnEnd tool_results=0
        event:AgentEnd [user, assistant/Stop]
        "#,
    );
}

#[tokio::test]
async fn cancelled_before_any_output() {
    // Nothing had streamed, but the aborted message is appended to the
    // history, so it is announced (#243).
    assert_timeline(
        rig(&[Step::Cancel]).run().await,
        r#"
        tool_source
        event:AgentStart
        input_filter.sync
        input_filter.async
        event:TurnStart
        event:MessageStart user
        event:MessageEnd user
        on_before_turn 0
        turn_hook
        provider.stream
        event:MessageStart assistant/Aborted
        event:MessageEnd assistant/Aborted
        on_after_turn
        event:TurnEnd tool_results=0
        event:AgentEnd [user, assistant/Aborted]
        "#,
    );
}

#[tokio::test]
async fn cancelled_while_streaming() {
    assert_timeline(
        rig(&[Step::StartThenCancel]).run().await,
        r#"
        tool_source
        event:AgentStart
        input_filter.sync
        input_filter.async
        event:TurnStart
        event:MessageStart user
        event:MessageEnd user
        on_before_turn 0
        turn_hook
        provider.stream
        event:MessageStart assistant/Stop
        event:MessageUpdate
        event:MessageEnd assistant/Aborted
        on_after_turn
        event:TurnEnd tool_results=0
        event:AgentEnd [user, assistant/Aborted]
        "#,
    );
}

#[tokio::test]
async fn cancelled_before_tools_start() {
    // The run was cancelled before its tools started: the call is answered
    // with an error and never runs, and the run ends with the cancel marker
    // (#243).
    assert_timeline(
        rig(&[Step::CancelThenTool, Step::Text]).run().await,
        r#"
        tool_source
        event:AgentStart
        input_filter.sync
        input_filter.async
        event:TurnStart
        event:MessageStart user
        event:MessageEnd user
        on_before_turn 0
        turn_hook
        provider.stream
        event:MessageStart assistant/Stop
        event:MessageEnd assistant/ToolUse
        event:ToolExecutionStart probe
        event:ToolExecutionEnd probe error=true
        event:MessageStart toolResult/error=true
        event:MessageEnd toolResult/error=true
        on_after_turn
        event:TurnEnd tool_results=1
        event:MessageStart user "[Agent stopped: cancelled]"
        event:MessageEnd user "[Agent stopped: cancelled]"
        event:AgentEnd [user, assistant/ToolUse, toolResult/error=true, user "[Agent stopped: cancelled]"]
        "#,
    );
}

#[tokio::test]
async fn turn_limit() {
    // The limit stops the turn before its model request: the turn runs
    // neither `on_before_turn` nor `on_after_turn`, but its `TurnStart` is
    // still paired with a `TurnEnd` (#243).
    let mut r = rig(&[Step::Tool("probe"), Step::Text]);
    r.limits = Some(ExecutionLimits::default().with_max_turns(1));
    assert_timeline(
        r.run().await,
        r#"
        tool_source
        event:AgentStart
        input_filter.sync
        input_filter.async
        event:TurnStart
        event:MessageStart user
        event:MessageEnd user
        on_before_turn 0
        turn_hook
        provider.stream
        event:MessageStart assistant/Stop
        event:MessageEnd assistant/ToolUse
        middleware0 probe args={}
        event:ToolExecutionStart probe
        tool.execute probe args={}
        event:ToolExecutionEnd probe error=false
        event:MessageStart toolResult/error=false
        event:MessageEnd toolResult/error=false
        on_after_turn
        event:TurnEnd tool_results=1
        event:TurnStart
        event:MessageStart user "[Agent stopped: Max turns reached (1/1)]"
        event:MessageEnd user "[Agent stopped: Max turns reached (1/1)]"
        event:TurnEnd tool_results=0
        event:AgentEnd [user, assistant/ToolUse, toolResult/error=false, user "[Agent stopped: Max turns reached (1/1)]"]
        "#,
    );
}

#[tokio::test]
async fn input_rejected() {
    let mut r = rig(&[Step::Text]);
    r.filters = vec![Filter::Sync(Outcome::Reject), Filter::Async(Outcome::Pass)];
    assert_timeline(
        r.run().await,
        r#"
        tool_source
        event:AgentStart
        input_filter.sync
        event:InputRejected
        event:AgentEnd []
        "#,
    );
}

/// Filters run in installation order (one list, sync and async mixed), and a
/// `Warn` lets the prompt through.
#[tokio::test]
async fn input_filter_chain() {
    let mut r = rig(&[Step::Text]);
    r.filters = vec![Filter::Async(Outcome::Warn), Filter::Sync(Outcome::Pass)];
    r.show_last_user = true;
    assert_timeline(
        r.run().await,
        r#"
        tool_source
        event:AgentStart
        input_filter.async
        input_filter.sync
        event:TurnStart
        event:MessageStart user
        event:MessageEnd user
        on_before_turn 0
        turn_hook
        provider.stream last_user="go | [Warning: careful]"
        event:MessageStart assistant/Stop
        event:MessageUpdate
        event:MessageEnd assistant/Stop
        on_after_turn
        event:TurnEnd tool_results=0
        event:AgentEnd [user, assistant/Stop]
        "#,
    );
}

/// Middleware runs in installation order; a `Modify` is what the next one and
/// the tool see.
#[tokio::test]
async fn middleware_chain_modify() {
    let mut r = rig(&[Step::Tool("probe"), Step::Text]);
    r.middlewares = vec![
        Verdict::Modify(serde_json::json!({"path": "safe"})),
        Verdict::Allow,
    ];
    assert_timeline(
        r.run().await,
        r#"
        tool_source
        event:AgentStart
        input_filter.sync
        input_filter.async
        event:TurnStart
        event:MessageStart user
        event:MessageEnd user
        on_before_turn 0
        turn_hook
        provider.stream
        event:MessageStart assistant/Stop
        event:MessageEnd assistant/ToolUse
        middleware0 probe args={}
        middleware1 probe args={"path":"safe"}
        event:ToolExecutionStart probe
        tool.execute probe args={"path":"safe"}
        event:ToolExecutionEnd probe error=false
        event:MessageStart toolResult/error=false
        event:MessageEnd toolResult/error=false
        on_after_turn
        event:TurnEnd tool_results=1
        event:TurnStart
        on_before_turn 1
        turn_hook
        provider.stream
        event:MessageStart assistant/Stop
        event:MessageUpdate
        event:MessageEnd assistant/Stop
        on_after_turn
        event:TurnEnd tool_results=0
        event:AgentEnd [user, assistant/ToolUse, toolResult/error=false, assistant/Stop]
        "#,
    );
}

/// A denial stops the chain and the tool.
#[tokio::test]
async fn middleware_deny() {
    let mut r = rig(&[Step::Tool("probe"), Step::Text]);
    r.middlewares = vec![Verdict::Deny, Verdict::Allow];
    assert_timeline(
        r.run().await,
        r#"
        tool_source
        event:AgentStart
        input_filter.sync
        input_filter.async
        event:TurnStart
        event:MessageStart user
        event:MessageEnd user
        on_before_turn 0
        turn_hook
        provider.stream
        event:MessageStart assistant/Stop
        event:MessageEnd assistant/ToolUse
        middleware0 probe args={}
        event:ToolExecutionStart probe
        event:ToolExecutionEnd probe error=true
        event:MessageStart toolResult/error=true
        event:MessageEnd toolResult/error=true
        on_after_turn
        event:TurnEnd tool_results=1
        event:TurnStart
        on_before_turn 1
        turn_hook
        provider.stream
        event:MessageStart assistant/Stop
        event:MessageUpdate
        event:MessageEnd assistant/Stop
        on_after_turn
        event:TurnEnd tool_results=0
        event:AgentEnd [user, assistant/ToolUse, toolResult/error=true, assistant/Stop]
        "#,
    );
}

/// A tool from a tool source goes through middleware and runs like a static one.
#[tokio::test]
async fn sourced_tool() {
    let mut r = rig(&[Step::Tool("sourced"), Step::Text]);
    r.sourced_tool = Some("sourced");
    assert_timeline(
        r.run().await,
        r#"
        tool_source
        event:AgentStart
        input_filter.sync
        input_filter.async
        event:TurnStart
        event:MessageStart user
        event:MessageEnd user
        on_before_turn 0
        turn_hook
        provider.stream
        event:MessageStart assistant/Stop
        event:MessageEnd assistant/ToolUse
        middleware0 sourced args={}
        event:ToolExecutionStart sourced
        tool.execute sourced args={}
        event:ToolExecutionEnd sourced error=false
        event:MessageStart toolResult/error=false
        event:MessageEnd toolResult/error=false
        on_after_turn
        event:TurnEnd tool_results=1
        event:TurnStart
        on_before_turn 1
        turn_hook
        provider.stream
        event:MessageStart assistant/Stop
        event:MessageUpdate
        event:MessageEnd assistant/Stop
        on_after_turn
        event:TurnEnd tool_results=0
        event:AgentEnd [user, assistant/ToolUse, toolResult/error=false, assistant/Stop]
        "#,
    );
}

/// A turn hook's note is appended to the latest user message of the request.
#[tokio::test]
async fn turn_hook_note() {
    let mut r = rig(&[Step::Tool("probe"), Step::Text]);
    r.note = Some("a note");
    r.show_last_user = true;
    assert_timeline(
        r.run().await,
        r#"
        tool_source
        event:AgentStart
        input_filter.sync
        input_filter.async
        event:TurnStart
        event:MessageStart user
        event:MessageEnd user
        on_before_turn 0
        turn_hook
        provider.stream last_user="go | a note"
        event:MessageStart assistant/Stop
        event:MessageEnd assistant/ToolUse
        middleware0 probe args={}
        event:ToolExecutionStart probe
        tool.execute probe args={}
        event:ToolExecutionEnd probe error=false
        event:MessageStart toolResult/error=false
        event:MessageEnd toolResult/error=false
        on_after_turn
        event:TurnEnd tool_results=1
        event:TurnStart
        on_before_turn 1
        turn_hook
        provider.stream last_user="go | a note"
        event:MessageStart assistant/Stop
        event:MessageUpdate
        event:MessageEnd assistant/Stop
        on_after_turn
        event:TurnEnd tool_results=0
        event:AgentEnd [user, assistant/ToolUse, toolResult/error=false, assistant/Stop]
        "#,
    );
}

/// `continue_loop` runs no input filters.
#[tokio::test]
async fn continue_loop_path() {
    let mut r = rig(&[Step::Text]);
    r.continue_from = Some("seeded");
    assert_timeline(
        r.run().await,
        r#"
        tool_source
        event:AgentStart
        event:TurnStart
        on_before_turn 0
        turn_hook
        provider.stream
        event:MessageStart assistant/Stop
        event:MessageUpdate
        event:MessageEnd assistant/Stop
        on_after_turn
        event:TurnEnd tool_results=0
        event:AgentEnd [assistant/Stop]
        "#,
    );
}

/// `on_before_turn` returning `false` ends the run; its turn is still closed
/// (#243).
#[tokio::test]
async fn stopped_by_on_before_turn() {
    let mut r = rig(&[Step::Tool("probe"), Step::Text]);
    r.stop_at_turn = Some(1);
    assert_timeline(
        r.run().await,
        r#"
        tool_source
        event:AgentStart
        input_filter.sync
        input_filter.async
        event:TurnStart
        event:MessageStart user
        event:MessageEnd user
        on_before_turn 0
        turn_hook
        provider.stream
        event:MessageStart assistant/Stop
        event:MessageEnd assistant/ToolUse
        middleware0 probe args={}
        event:ToolExecutionStart probe
        tool.execute probe args={}
        event:ToolExecutionEnd probe error=false
        event:MessageStart toolResult/error=false
        event:MessageEnd toolResult/error=false
        on_after_turn
        event:TurnEnd tool_results=1
        event:TurnStart
        on_before_turn 1
        event:TurnEnd tool_results=0
        event:AgentEnd [user, assistant/ToolUse, toolResult/error=false]
        "#,
    );
}

/// A provider that returns its message without streaming anything: the
/// message is announced anyway (#243).
#[tokio::test]
async fn provider_streams_nothing() {
    assert_timeline(
        rig(&[Step::Silent]).run().await,
        r#"
        tool_source
        event:AgentStart
        input_filter.sync
        input_filter.async
        event:TurnStart
        event:MessageStart user
        event:MessageEnd user
        on_before_turn 0
        turn_hook
        provider.stream
        event:MessageStart assistant/Stop
        event:MessageEnd assistant/Stop
        on_after_turn
        event:TurnEnd tool_results=0
        event:AgentEnd [user, assistant/Stop]
        "#,
    );
}

/// The run is cancelled while middleware decides: the call that middleware
/// then allows is still not run (#243).
#[tokio::test]
async fn cancelled_while_middleware_decides() {
    let mut r = rig(&[Step::Tool("probe"), Step::Text]);
    r.middlewares = vec![Verdict::CancelThenAllow, Verdict::Allow];
    assert_timeline(
        r.run().await,
        r#"
        tool_source
        event:AgentStart
        input_filter.sync
        input_filter.async
        event:TurnStart
        event:MessageStart user
        event:MessageEnd user
        on_before_turn 0
        turn_hook
        provider.stream
        event:MessageStart assistant/Stop
        event:MessageEnd assistant/ToolUse
        middleware0 probe args={}
        middleware1 probe args={}
        event:ToolExecutionStart probe
        event:ToolExecutionEnd probe error=true
        event:MessageStart toolResult/error=true
        event:MessageEnd toolResult/error=true
        on_after_turn
        event:TurnEnd tool_results=1
        event:MessageStart user "[Agent stopped: cancelled]"
        event:MessageEnd user "[Agent stopped: cancelled]"
        event:AgentEnd [user, assistant/ToolUse, toolResult/error=true, user "[Agent stopped: cancelled]"]
        "#,
    );
}

/// Sequential tools: the first cancels the run, the second is not run, and
/// the run ends with the cancel marker once (#243).
#[tokio::test]
async fn sequential_tools_after_a_cancel() {
    let mut r = rig(&[Step::Tools(&["canceller", "probe"]), Step::Text]);
    r.strategy = Some(ToolExecutionStrategy::Sequential);
    assert_timeline(
        r.run().await,
        r#"
        tool_source
        event:AgentStart
        input_filter.sync
        input_filter.async
        event:TurnStart
        event:MessageStart user
        event:MessageEnd user
        on_before_turn 0
        turn_hook
        provider.stream
        event:MessageStart assistant/Stop
        event:MessageEnd assistant/ToolUse
        middleware0 canceller args={}
        event:ToolExecutionStart canceller
        tool.execute canceller args={}
        event:ToolExecutionEnd canceller error=false
        event:MessageStart toolResult/error=false
        event:MessageEnd toolResult/error=false
        event:ToolExecutionStart probe
        event:ToolExecutionEnd probe error=true
        event:MessageStart toolResult/error=true
        event:MessageEnd toolResult/error=true
        on_after_turn
        event:TurnEnd tool_results=2
        event:MessageStart user "[Agent stopped: cancelled]"
        event:MessageEnd user "[Agent stopped: cancelled]"
        event:AgentEnd [user, assistant/ToolUse, toolResult/error=false, toolResult/error=true, user "[Agent stopped: cancelled]"]
        "#,
    );
}

/// Batches of one: the cancel in the first batch stops the second (#243).
#[tokio::test]
async fn batched_tools_after_a_cancel() {
    let mut r = rig(&[Step::Tools(&["canceller", "probe"]), Step::Text]);
    r.strategy = Some(ToolExecutionStrategy::Batched { size: 1 });
    assert_timeline(
        r.run().await,
        r#"
        tool_source
        event:AgentStart
        input_filter.sync
        input_filter.async
        event:TurnStart
        event:MessageStart user
        event:MessageEnd user
        on_before_turn 0
        turn_hook
        provider.stream
        event:MessageStart assistant/Stop
        event:MessageEnd assistant/ToolUse
        middleware0 canceller args={}
        event:ToolExecutionStart canceller
        tool.execute canceller args={}
        event:ToolExecutionEnd canceller error=false
        event:MessageStart toolResult/error=false
        event:MessageEnd toolResult/error=false
        event:ToolExecutionStart probe
        event:ToolExecutionEnd probe error=true
        event:MessageStart toolResult/error=true
        event:MessageEnd toolResult/error=true
        on_after_turn
        event:TurnEnd tool_results=2
        event:MessageStart user "[Agent stopped: cancelled]"
        event:MessageEnd user "[Agent stopped: cancelled]"
        event:AgentEnd [user, assistant/ToolUse, toolResult/error=false, toolResult/error=true, user "[Agent stopped: cancelled]"]
        "#,
    );
}

/// A provider that sends `Done` without `Start`: the message is announced
/// with both events (#243).
#[tokio::test]
async fn provider_sends_done_without_start() {
    assert_timeline(
        rig(&[Step::DoneOnly]).run().await,
        r#"
        tool_source
        event:AgentStart
        input_filter.sync
        input_filter.async
        event:TurnStart
        event:MessageStart user
        event:MessageEnd user
        on_before_turn 0
        turn_hook
        provider.stream
        event:MessageStart assistant/Stop
        event:MessageEnd assistant/Stop
        on_after_turn
        event:TurnEnd tool_results=0
        event:AgentEnd [user, assistant/Stop]
        "#,
    );
}
