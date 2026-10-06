//! Pins the order in which the existing hooks fire, interleaved with the
//! events, on every path through a run: a plain answer, a tool call, a
//! refusal, a provider error, a cancellation during streaming, a cancellation
//! while tools run, and a turn limit.
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
}

impl Recorder {
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
// Scripted provider and hooks
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Step {
    /// A plain answer.
    Text,
    /// One call to the `probe` tool.
    Tool,
    /// A refused response carrying a tool call.
    RefuseWithTool,
    /// A failed response (`StopReason::Error`), as an SSE-embedded error arrives.
    Error,
    /// Cancel the run while streaming.
    Cancel,
    /// Cancel the run, then return a tool call (cancelled while tools run).
    CancelThenTool,
}

struct Scripted {
    steps: Mutex<Vec<Step>>,
    rec: Arc<Recorder>,
}

fn tool_call_message(stop_reason: StopReason) -> Message {
    Message::assistant(
        vec![Content::tool_call("call-1", "probe", serde_json::json!({}))],
        stop_reason,
        "mock",
        "mock",
        Usage::default(),
    )
}

fn send_tool_call(tx: &mpsc::UnboundedSender<StreamEvent>) {
    let _ = tx.send(StreamEvent::ToolCallStart {
        content_index: 0,
        id: "call-1".into(),
        name: "probe".into(),
    });
    let _ = tx.send(StreamEvent::ToolCallEnd { content_index: 0 });
}

#[async_trait::async_trait]
impl StreamProvider for Scripted {
    async fn stream(
        &self,
        _config: StreamConfig,
        tx: mpsc::UnboundedSender<StreamEvent>,
        cancel: CancellationToken,
    ) -> Result<Message, ProviderError> {
        self.rec.hook("provider.stream");
        let step = self.steps.lock().unwrap().remove(0);
        if let Step::Cancel = step {
            cancel.cancel();
            return Err(ProviderError::Cancelled);
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
            Step::Tool => {
                send_tool_call(&tx);
                tool_call_message(StopReason::ToolUse)
            }
            Step::RefuseWithTool => {
                send_tool_call(&tx);
                tool_call_message(StopReason::Refusal)
            }
            Step::Error => {
                Message::assistant(vec![], StopReason::Error, "mock", "mock", Usage::default())
                    .with_error_message("upstream failed")
            }
            Step::CancelThenTool => {
                send_tool_call(&tx);
                cancel.cancel();
                tool_call_message(StopReason::ToolUse)
            }
            Step::Cancel => unreachable!(),
        };
        let _ = tx.send(StreamEvent::Done {
            message: message.clone(),
        });
        Ok(message)
    }
}

struct Probe(Arc<Recorder>);

#[async_trait::async_trait]
impl AgentTool for Probe {
    fn name(&self) -> &str {
        "probe"
    }
    fn label(&self) -> &str {
        "Probe"
    }
    fn description(&self) -> &str {
        "Records that it ran"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    async fn execute(
        &self,
        _params: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        self.0.hook("tool.execute probe");
        Ok(ToolResult {
            content: vec![Content::Text { text: "ok".into() }],
            details: serde_json::Value::Null,
        })
    }
}

struct SyncFilter(Arc<Recorder>, bool);
impl InputFilter for SyncFilter {
    fn filter(&self, _text: &str) -> FilterResult {
        self.0.hook("input_filter.sync");
        if self.1 {
            FilterResult::Reject("no".into())
        } else {
            FilterResult::Pass
        }
    }
}

struct AsyncFilterHook(Arc<Recorder>);
#[async_trait::async_trait]
impl AsyncInputFilter for AsyncFilterHook {
    async fn filter(&self, _text: &str) -> FilterResult {
        self.0.hook("input_filter.async");
        FilterResult::Pass
    }
}

struct Hook(Arc<Recorder>);
#[async_trait::async_trait]
impl TurnHook for Hook {
    async fn before_turn(&self, _turn: &TurnContext<'_>) -> Option<String> {
        self.0.hook("turn_hook");
        None
    }
}

struct Middleware(Arc<Recorder>);
#[async_trait::async_trait]
impl ToolMiddleware for Middleware {
    async fn before_tool(&self, call: &ToolCallRequest<'_>) -> ToolDecision {
        self.0.hook(format!("middleware {}", call.tool_name));
        ToolDecision::Allow
    }
}

struct Source(Arc<Recorder>);
#[async_trait::async_trait]
impl ToolSource for Source {
    async fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        self.0.hook("tool_source");
        vec![]
    }
}

/// Run one prompt with every hook installed, and return the timeline.
async fn run(steps: &[Step], limits: Option<ExecutionLimits>) -> Vec<String> {
    run_with(steps, limits, false).await
}

async fn run_with(steps: &[Step], limits: Option<ExecutionLimits>, reject: bool) -> Vec<String> {
    let rec = Arc::new(Recorder::default());
    let provider = Scripted {
        steps: Mutex::new(steps.to_vec()),
        rec: rec.clone(),
    };
    let (r1, r2, r3) = (rec.clone(), rec.clone(), rec.clone());
    let mut agent = Agent::from_provider(provider, ModelConfig::mock())
        .with_tools(vec![Box::new(Probe(rec.clone()))])
        .with_tool_source(Source(rec.clone()))
        .with_input_filter(SyncFilter(rec.clone(), reject))
        .with_async_input_filter(AsyncFilterHook(rec.clone()))
        .with_turn_hook(Hook(rec.clone()))
        .with_tool_middleware(Middleware(rec.clone()))
        .on_before_turn(move |_, turn| {
            r1.hook(format!("on_before_turn {turn}"));
            true
        })
        .on_after_turn(move |_, _| r2.hook("on_after_turn"))
        .on_error(move |e| r3.hook(format!("on_error {e:?}")));
    if let Some(limits) = limits {
        agent = agent.with_execution_limits(limits);
    }
    let (tx, rx) = mpsc::unbounded_channel();
    *rec.rx.lock().unwrap() = Some(rx);
    agent.prompt_with_sender("go", tx).await;
    rec.timeline()
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
        run(&[Step::Text], None).await,
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
        run(&[Step::Tool, Step::Text], None).await,
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
        middleware probe
        event:ToolExecutionStart probe
        tool.execute probe
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
        run(&[Step::RefuseWithTool], None).await,
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
async fn provider_error() {
    assert_timeline(
        run(&[Step::Error], None).await,
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
async fn cancelled_while_streaming() {
    // Current behaviour, kept as is here: the aborted assistant message is
    // in the final history but gets no `MessageStart` / `MessageEnd`.
    assert_timeline(
        run(&[Step::Cancel], None).await,
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
        on_after_turn
        event:TurnEnd tool_results=0
        event:AgentEnd [user, assistant/Aborted]
        "#,
    );
}

#[tokio::test]
async fn cancelled_while_tools_run() {
    // Current behaviour, kept as is here: the run was cancelled before its
    // tools started, and the tool still ran.
    assert_timeline(
        run(&[Step::CancelThenTool, Step::Text], None).await,
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
        middleware probe
        event:ToolExecutionStart probe
        tool.execute probe
        event:ToolExecutionEnd probe error=false
        event:MessageStart toolResult/error=false
        event:MessageEnd toolResult/error=false
        on_after_turn
        event:TurnEnd tool_results=1
        event:MessageStart user "[Agent stopped: cancelled]"
        event:MessageEnd user "[Agent stopped: cancelled]"
        event:AgentEnd [user, assistant/ToolUse, toolResult/error=false, user "[Agent stopped: cancelled]"]
        "#,
    );
}

#[tokio::test]
async fn turn_limit() {
    // Current behaviour, kept as is here: the limit is checked after
    // `TurnStart`, so that `TurnStart` has no `TurnEnd`.
    let limits = ExecutionLimits::default().with_max_turns(1);
    assert_timeline(
        run(&[Step::Tool, Step::Text], Some(limits)).await,
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
        middleware probe
        event:ToolExecutionStart probe
        tool.execute probe
        event:ToolExecutionEnd probe error=false
        event:MessageStart toolResult/error=false
        event:MessageEnd toolResult/error=false
        on_after_turn
        event:TurnEnd tool_results=1
        event:TurnStart
        event:MessageStart user "[Agent stopped: Max turns reached (1/1)]"
        event:MessageEnd user "[Agent stopped: Max turns reached (1/1)]"
        event:AgentEnd [user, assistant/ToolUse, toolResult/error=false, user "[Agent stopped: Max turns reached (1/1)]"]
        "#,
    );
}

#[tokio::test]
async fn input_rejected() {
    assert_timeline(
        run_with(&[Step::Text], None, true).await,
        r#"
        tool_source
        event:AgentStart
        input_filter.sync
        event:InputRejected
        event:AgentEnd []
        "#,
    );
}
