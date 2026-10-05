//! The examples in `docs/guides/testing.md`, compiled and run, so the guide
//! cannot drift from the API. Keep the two in sync.

use yoagent::provider::mock::{MockResponse, MockToolCall};
use yoagent::provider::{MockProvider, ModelConfig};
use yoagent::*;

// --- 1. A scripted run --------------------------------------------------

struct Echo;

#[async_trait::async_trait]
impl AgentTool for Echo {
    fn name(&self) -> &str {
        "echo"
    }
    fn label(&self) -> &str {
        "Echo"
    }
    fn description(&self) -> &str {
        "Echoes its input"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}}})
    }
    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult {
            content: vec![Content::Text {
                text: params["text"].as_str().unwrap_or("").to_string(),
            }],
            details: serde_json::Value::Null,
        })
    }
}

#[tokio::test]
async fn the_agent_calls_the_tool_then_answers() {
    // Each response is one model turn, in order.
    let provider = MockProvider::new(vec![
        MockResponse::ToolCalls(vec![MockToolCall {
            name: "echo".into(),
            arguments: serde_json::json!({"text": "hi"}),
            provider_metadata: None,
        }]),
        MockResponse::Text("The tool said hi.".into()),
    ]);
    let mut agent =
        Agent::from_provider(provider, ModelConfig::mock()).with_tools(vec![Box::new(Echo)]);

    let mut rx = agent.prompt("say hi through the tool").await;
    let mut tool_output = None;
    while let Some(event) = rx.recv().await {
        if let AgentEvent::ToolExecutionEnd { result, .. } = event {
            tool_output = result.content.first().cloned();
        }
    }
    agent.finish().await;

    assert!(matches!(tool_output, Some(Content::Text { text }) if text == "hi"));
    let last = agent.messages().last().cloned();
    assert!(matches!(
        last,
        Some(AgentMessage::Llm(Message::Assistant { content, .. }))
            if matches!(content.first(), Some(Content::Text { text }) if text == "The tool said hi.")
    ));
}

// --- 2. A middleware, without an agent -----------------------------------

struct NoRm;

#[async_trait::async_trait]
impl ToolMiddleware for NoRm {
    async fn before_tool(&self, call: &ToolCallRequest<'_>) -> ToolDecision {
        let command = call.args["command"].as_str().unwrap_or("");
        if call.tool_name == "bash" && command.contains("rm ") {
            ToolDecision::Deny("no deleting".into())
        } else {
            ToolDecision::Allow
        }
    }
}

#[tokio::test]
async fn the_middleware_denies_rm() {
    let args = serde_json::json!({"command": "rm -rf build"});
    let call = ToolCallRequest::new("call-1", "bash", &args);
    assert!(matches!(
        NoRm.before_tool(&call).await,
        ToolDecision::Deny(_)
    ));

    let args = serde_json::json!({"command": "ls"});
    let call = ToolCallRequest::new("call-2", "bash", &args);
    assert!(matches!(NoRm.before_tool(&call).await, ToolDecision::Allow));
}

// --- 3. Abort ------------------------------------------------------------

#[tokio::test]
async fn an_abort_ends_the_run() {
    let mut agent = Agent::from_provider(MockProvider::text("hello"), ModelConfig::mock());
    let mut rx = agent.prompt("hi").await;
    agent.abort();
    while rx.recv().await.is_some() {}
    agent.finish().await; // the agent is usable again
    assert!(!agent.is_streaming());
}
