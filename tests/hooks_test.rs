//! The policy-engine hooks: conversation context on `ToolCallRequest`, async
//! input filters, and per-turn hooks that add a transient system-prompt line.

use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use yoagent::agent_loop::{agent_loop, AgentLoopConfig};
use yoagent::provider::mock::*;
use yoagent::provider::{
    MockProvider, ModelConfig, ProviderError, StreamConfig, StreamEvent, StreamProvider,
    TurnHookProvider,
};
use yoagent::*;

/// Records the system prompt and tool count of every request, then delegates.
struct Recording {
    inner: MockProvider,
    seen: Arc<Mutex<Vec<(String, usize)>>>,
}

#[async_trait::async_trait]
impl StreamProvider for Recording {
    async fn stream(
        &self,
        config: StreamConfig,
        tx: mpsc::UnboundedSender<StreamEvent>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Message, ProviderError> {
        self.seen
            .lock()
            .unwrap()
            .push((config.system_prompt.clone(), config.tools.len()));
        self.inner.stream(config, tx, cancel).await
    }
}

type Seen = Arc<Mutex<Vec<(String, usize)>>>;

fn recording(inner: MockProvider) -> (Recording, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    (
        Recording {
            inner,
            seen: seen.clone(),
        },
        seen,
    )
}

struct EchoTool;

#[async_trait::async_trait]
impl AgentTool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn label(&self) -> &str {
        "Echo"
    }
    fn description(&self) -> &str {
        "Echoes"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(
        &self,
        _params: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult {
            content: vec![Content::Text { text: "ok".into() }],
            details: serde_json::Value::Null,
        })
    }
}

fn tool_then_text() -> MockProvider {
    MockProvider::new(vec![
        MockResponse::ToolCalls(vec![MockToolCall {
            provider_metadata: None,
            name: "echo".into(),
            arguments: serde_json::json!({"x": 1}),
        }]),
        MockResponse::Text("done".into()),
    ])
}

async fn run(mut agent: Agent, prompt: &str) -> (Agent, Vec<AgentEvent>) {
    let mut rx = agent.prompt(prompt).await;
    let mut events = Vec::new();
    while let Some(e) = rx.recv().await {
        events.push(e);
    }
    agent.finish().await;
    (agent, events)
}

// ---------------------------------------------------------------------------
// ToolCallRequest carries the conversation
// ---------------------------------------------------------------------------

/// (latest user text, message count) per call.
type HistoryLog = Arc<Mutex<Vec<(Option<String>, usize)>>>;

struct SeesHistory(HistoryLog);

#[async_trait::async_trait]
impl ToolMiddleware for SeesHistory {
    async fn before_tool(&self, call: &ToolCallRequest<'_>) -> ToolDecision {
        self.0
            .lock()
            .unwrap()
            .push((call.latest_user_text(), call.messages.len()));
        ToolDecision::Allow
    }
}

#[tokio::test]
async fn middleware_sees_the_conversation_and_latest_user_text() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let agent = Agent::from_provider(tool_then_text(), ModelConfig::mock())
        .with_tools(vec![Box::new(EchoTool)])
        .with_tool_middleware(SeesHistory(log.clone()));
    let (_agent, _) = run(agent, "delete the temp file").await;
    let log = log.lock().unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].0.as_deref(), Some("delete the temp file"));
    // The user prompt and the assistant message carrying the call.
    assert_eq!(log[0].1, 2);
}

// ---------------------------------------------------------------------------
// Async input filters
// ---------------------------------------------------------------------------

struct SlowModeration;

#[async_trait::async_trait]
impl AsyncInputFilter for SlowModeration {
    async fn filter(&self, text: &str) -> FilterResult {
        tokio::task::yield_now().await;
        if text.contains("forbidden") {
            FilterResult::Reject("moderation said no".into())
        } else if text.contains("iffy") {
            FilterResult::Warn("flagged by moderation".into())
        } else {
            FilterResult::Pass
        }
    }
}

#[tokio::test]
async fn async_input_filter_rejects_before_the_model_is_called() {
    let (provider, seen) = recording(MockProvider::text("should not run"));
    let agent =
        Agent::from_provider(provider, ModelConfig::mock()).with_async_input_filter(SlowModeration);
    let (agent, events) = run(agent, "this is forbidden").await;
    assert!(events.iter().any(
        |e| matches!(e, AgentEvent::InputRejected { reason } if reason == "moderation said no")
    ));
    assert!(seen.lock().unwrap().is_empty(), "no LLM request");
    assert!(agent.messages().is_empty());
}

#[tokio::test]
async fn async_input_filter_warns_and_passes() {
    let agent = Agent::from_provider(MockProvider::text("fine"), ModelConfig::mock())
        .with_async_input_filter(SlowModeration);
    let (agent, _) = run(agent, "an iffy request").await;
    let Some(AgentMessage::Llm(Message::User { content, .. })) = agent.messages().first() else {
        panic!("user message first");
    };
    assert!(content.iter().any(
        |c| matches!(c, Content::Text { text } if text == "[Warning: flagged by moderation]")
    ));
    // Positive control: a clean prompt carries no warning.
    let agent = Agent::from_provider(MockProvider::text("fine"), ModelConfig::mock())
        .with_async_input_filter(SlowModeration);
    let (agent, _) = run(agent, "a clean request").await;
    let Some(AgentMessage::Llm(Message::User { content, .. })) = agent.messages().first() else {
        panic!("user message first");
    };
    assert_eq!(content.len(), 1);
}

struct PassSync;
impl InputFilter for PassSync {
    fn filter(&self, _text: &str) -> FilterResult {
        FilterResult::Warn("sync ran".into())
    }
}

#[tokio::test]
async fn sync_and_async_filters_share_one_ordered_list() {
    let agent = Agent::from_provider(MockProvider::text("fine"), ModelConfig::mock())
        .with_input_filter(PassSync)
        .with_async_input_filter(SlowModeration);
    let (agent, _) = run(agent, "an iffy request").await;
    let Some(AgentMessage::Llm(Message::User { content, .. })) = agent.messages().first() else {
        panic!("user message first");
    };
    let Content::Text { text } = content.last().unwrap() else {
        panic!("text")
    };
    assert_eq!(
        text,
        "[Warning: sync ran]\n[Warning: flagged by moderation]"
    );
}

#[test]
fn an_async_filter_called_synchronously_fails_closed() {
    let f = AsyncFilter(SlowModeration);
    assert!(matches!(
        InputFilter::filter(&f, "anything"),
        FilterResult::Reject(_)
    ));
    assert!(f.as_async().is_some());
    assert!(PassSync.as_async().is_none());
}

// ---------------------------------------------------------------------------
// Turn hooks
// ---------------------------------------------------------------------------

struct Line(Option<&'static str>, Arc<Mutex<Vec<Option<String>>>>);

#[async_trait::async_trait]
impl TurnHook for Line {
    async fn before_turn(&self, turn: &TurnContext<'_>) -> Option<String> {
        self.1.lock().unwrap().push(turn.latest_user_text());
        self.0.map(str::to_string)
    }
}

struct Panics;

#[async_trait::async_trait]
impl TurnHook for Panics {
    async fn before_turn(&self, _turn: &TurnContext<'_>) -> Option<String> {
        panic!("hook bug")
    }
}

#[tokio::test]
async fn turn_hook_adds_a_transient_system_line_every_turn() {
    let (provider, seen) = recording(tool_then_text());
    let calls = Arc::new(Mutex::new(Vec::new()));
    let agent = Agent::from_provider(provider, ModelConfig::mock())
        .with_system_prompt("Base prompt.")
        .with_tools(vec![Box::new(EchoTool)])
        .with_turn_hook(Line(Some("Hint A."), calls.clone()))
        .with_turn_hook(Panics)
        .with_turn_hook(Line(None, calls.clone()))
        .with_turn_hook(Line(Some("Hint B."), calls.clone()));
    let (agent, _) = run(agent, "hello").await;

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2, "two turns");
    for (prompt, _) in seen.iter() {
        assert_eq!(prompt, "Base prompt.\n\nHint A.\nHint B.");
    }
    // Each hook ran once per turn and saw the user's text.
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 6);
    assert!(calls.iter().all(|c| c.as_deref() == Some("hello")));
    // Transient: never stored.
    assert_eq!(agent.system_prompt, "Base prompt.");
    for m in agent.messages() {
        let json = serde_json::to_string(m).unwrap();
        assert!(!json.contains("Hint A."), "{json}");
    }
}

#[tokio::test]
async fn a_hook_returning_none_leaves_the_request_unchanged() {
    let (provider, seen) = recording(MockProvider::text("hi"));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let agent = Agent::from_provider(provider, ModelConfig::mock())
        .with_system_prompt("Base prompt.")
        .with_turn_hook(Line(None, calls.clone()));
    run(agent, "hello").await;
    assert_eq!(seen.lock().unwrap()[0].0, "Base prompt.");
    assert_eq!(calls.lock().unwrap().len(), 1, "the hook did run");
}

#[tokio::test]
async fn raw_loop_callers_wrap_the_provider() {
    let (provider, seen) = recording(MockProvider::text("hi"));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let hooked = TurnHookProvider::new(
        Arc::new(provider),
        vec![Arc::new(Line(Some("Hint."), calls.clone()))],
    );
    let config = AgentLoopConfig {
        provider: Arc::new(hooked),
        model: "mock".into(),
        api_key: String::new(),
        thinking_level: ThinkingLevel::Off,
        max_tokens: None,
        temperature: None,
        model_config: None,
        convert_to_llm: None,
        transform_context: None,
        get_steering_messages: None,
        get_follow_up_messages: None,
        context_config: None,
        compaction_strategy: None,
        execution_limits: None,
        cache_config: CacheConfig::default(),
        tool_output_sink: None,
        tool_execution: ToolExecutionStrategy::default(),
        tool_middleware: vec![],
        output_schema: None,
        retry_config: yoagent::RetryConfig::default(),
        before_turn: None,
        after_turn: None,
        on_error: None,
        input_filters: vec![Arc::new(AsyncFilter(SlowModeration))],
        turn_delay: None,
    };
    let mut context = AgentContext {
        system_prompt: String::new(),
        messages: vec![],
        tools: vec![],
    };
    let (tx, _rx) = mpsc::unbounded_channel();
    agent_loop(
        vec![AgentMessage::Llm(Message::user("hello"))],
        &mut context,
        &config,
        tx,
        tokio_util::sync::CancellationToken::new(),
    )
    .await;
    assert_eq!(
        seen.lock().unwrap()[0].0,
        "Hint.",
        "empty base: just the line"
    );
    assert!(context.system_prompt.is_empty());
}
