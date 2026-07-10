//! Tests that the loop emits the documented tracing spans with their fields.
//!
//! Attaches a capturing subscriber to the loop future via `with_subscriber`
//! and drives `agent_loop` directly in the current task — spans created in
//! separately-spawned tasks would not reach this scoped subscriber.

use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::instrument::WithSubscriber;
use tracing_subscriber::layer::SubscriberExt;
use yoagent::provider::mock::*;
use yoagent::provider::MockProvider;
use yoagent::*;

/// Layer that records every new span's name.
struct SpanCollector(Arc<Mutex<Vec<String>>>);

impl<S> tracing_subscriber::Layer<S> for SpanCollector
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _id: &tracing::span::Id,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        self.0
            .lock()
            .unwrap()
            .push(attrs.metadata().name().to_string());
    }
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
        "echoes"
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

fn loop_config(provider: MockProvider) -> yoagent::agent_loop::AgentLoopConfig {
    yoagent::agent_loop::AgentLoopConfig {
        provider: std::sync::Arc::new(provider),
        model: "mock".into(),
        api_key: "test".into(),
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
        tool_execution: ToolExecutionStrategy::default(),
        tool_middleware: vec![],
        output_schema: None,
        retry_config: yoagent::RetryConfig::none(),
        before_turn: None,
        after_turn: None,
        on_error: None,
        input_filters: vec![],
        turn_delay: None,
    }
}

#[tokio::test]
async fn loop_emits_agent_llm_and_tool_spans() {
    let spans = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(SpanCollector(spans.clone()));

    let provider = MockProvider::new(vec![
        MockResponse::ToolCalls(vec![MockToolCall {
            provider_metadata: None,
            name: "echo".into(),
            arguments: serde_json::json!({}),
        }]),
        MockResponse::Text("done".into()),
    ]);
    let config = loop_config(provider);

    let mut context = AgentContext {
        system_prompt: "test".into(),
        messages: Vec::new(),
        tools: vec![Box::new(EchoTool)],
    };
    let (tx, _rx) = mpsc::unbounded_channel();

    // with_subscriber attaches the subscriber across every poll of the
    // future (a plain `with_default(sub, || fut).await` would only cover
    // creating the future, not running it).
    agent_loop(
        vec![AgentMessage::Llm(Message::user("go"))],
        &mut context,
        &config,
        tx,
        CancellationToken::new(),
    )
    .with_subscriber(subscriber)
    .await;

    let names = spans.lock().unwrap().clone();
    assert!(
        names.contains(&"agent_loop".to_string()),
        "expected agent_loop span, got: {names:?}"
    );
    // Two turns → two llm_stream spans; one tool execution → one tool span.
    assert_eq!(
        names.iter().filter(|n| *n == "llm_stream").count(),
        2,
        "got: {names:?}"
    );
    assert_eq!(
        names.iter().filter(|n| *n == "tool").count(),
        1,
        "got: {names:?}"
    );
}
