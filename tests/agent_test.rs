//! Tests for the Agent struct (stateful wrapper).

use yoagent::agent::Agent;
use yoagent::provider::mock::*;
use yoagent::provider::MockProvider;
use yoagent::*;

#[tokio::test]
async fn test_agent_simple_prompt() {
    let provider = MockProvider::text("Hello!");
    let mut agent = Agent::new(provider)
        .with_system_prompt("You are helpful.")
        .with_model("mock")
        .with_api_key("test");

    let rx = agent.prompt("Hi there").await;

    // Drain events
    let mut events = Vec::new();
    let mut rx = rx;
    while let Ok(e) = rx.try_recv() {
        events.push(e);
    }

    assert!(!events.is_empty());
    assert_eq!(agent.messages().len(), 2); // user + assistant
}

#[tokio::test]
async fn test_agent_reset() {
    let provider = MockProvider::text("Hello!");
    let mut agent = Agent::new(provider)
        .with_system_prompt("test")
        .with_model("mock")
        .with_api_key("test");

    let _ = agent.prompt("Hi").await;
    assert!(!agent.messages().is_empty());

    agent.reset();
    assert!(agent.messages().is_empty());
    assert!(!agent.is_streaming());
}

#[tokio::test]
async fn test_agent_with_tools() {
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
            "Echoes input"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}}})
        }
        async fn execute(
            &self,
            _id: &str,
            params: serde_json::Value,
            _cancel: tokio_util::sync::CancellationToken,
            _on_update: Option<ToolUpdateFn>,
        ) -> Result<ToolResult, ToolError> {
            let text = params["text"].as_str().unwrap_or("").to_string();
            Ok(ToolResult {
                content: vec![Content::Text { text }],
                details: serde_json::Value::Null,
            })
        }
    }

    let provider = MockProvider::new(vec![
        MockResponse::ToolCalls(vec![MockToolCall {
            name: "echo".into(),
            arguments: serde_json::json!({"text": "hello"}),
        }]),
        MockResponse::Text("Echoed: hello".into()),
    ]);

    let mut agent = Agent::new(provider)
        .with_system_prompt("test")
        .with_model("mock")
        .with_api_key("test")
        .with_tools(vec![Box::new(EchoTool)]);

    let _ = agent.prompt("Echo hello").await;

    // user + assistant(tool_call) + toolResult + assistant(text)
    assert_eq!(agent.messages().len(), 4);
}

#[tokio::test]
async fn test_agent_builder_pattern() {
    let provider = MockProvider::text("ok");
    let agent = Agent::new(provider)
        .with_system_prompt("sys")
        .with_model("test-model")
        .with_api_key("key123")
        .with_thinking(ThinkingLevel::Medium)
        .with_max_tokens(4096);

    assert_eq!(agent.system_prompt, "sys");
    assert_eq!(agent.model, "test-model");
    assert_eq!(agent.api_key, "key123");
    assert_eq!(agent.thinking_level, ThinkingLevel::Medium);
    assert_eq!(agent.max_tokens, Some(4096));
}
