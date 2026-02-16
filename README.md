# yo-agent 🦀

Simple, effective agent loop with tool execution and event streaming in Rust. Inspired by [pi-agent-core](https://github.com/badlogic/pi-mono/tree/main/packages/agent).

## Philosophy

The loop is the product. No over-engineered planning/reflection/RAG layers — just:

```
prompt → LLM stream → tool execution → loop if tool calls → done
```

Everything is observable via events. Custom message types let apps add UI-only messages without polluting the LLM context.

## Quick Start

```rust
use yo_agent::agent::Agent;
use yo_agent::provider::AnthropicProvider;
use yo_agent::*;

#[tokio::main]
async fn main() {
    let mut agent = Agent::new(AnthropicProvider)
        .with_system_prompt("You are a helpful assistant.")
        .with_model("claude-sonnet-4-20250514")
        .with_api_key(std::env::var("ANTHROPIC_API_KEY").unwrap());

    let mut rx = agent.prompt("Hello!").await;

    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::MessageUpdate { delta: StreamDelta::Text { delta }, .. } => {
                print!("{}", delta);
            }
            _ => {}
        }
    }
}
```

## Architecture

```
src/
├── types.rs          # Core types: Message, AgentMessage, AgentEvent, AgentTool trait
├── agent_loop.rs     # The core loop (agent_loop + agent_loop_continue)
├── agent.rs          # Stateful Agent with steering/follow-up queues
└── provider/
    ├── traits.rs     # StreamProvider trait
    ├── anthropic.rs  # Anthropic Claude (streaming)
    └── mock.rs       # Mock provider for testing
```

## Key Concepts

### AgentMessage vs Message

`Message` is what LLMs understand (user/assistant/toolResult). `AgentMessage` wraps this and adds an `Extension` variant for app-specific messages (UI notifications, artifacts, etc.) that live in conversation history but aren't sent to the model.

### Event Flow

```
agent_loop("Hello")
├─ AgentStart
├─ TurnStart
├─ MessageStart   (user prompt)
├─ MessageEnd     (user prompt)
├─ MessageStart   (assistant)
├─ MessageUpdate  (streaming deltas)
├─ MessageEnd     (assistant complete)
├─ TurnEnd
└─ AgentEnd
```

### Steering & Follow-up

- **Steering**: Interrupt the agent mid-tool-execution. Remaining tools are skipped.
- **Follow-up**: Queue work for after the agent finishes its current task.

```rust
// While agent is running tools
agent.steer(AgentMessage::Llm(Message::user("Stop! Do this instead.")));

// After agent finishes
agent.follow_up(AgentMessage::Llm(Message::user("Also summarize the result.")));
```

## Tools

Implement the `AgentTool` trait:

```rust
use yo_agent::*;

struct ReadFile;

#[async_trait::async_trait]
impl AgentTool for ReadFile {
    fn name(&self) -> &str { "read_file" }
    fn label(&self) -> &str { "Read File" }
    fn description(&self) -> &str { "Read a file's contents" }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path"}
            },
            "required": ["path"]
        })
    }
    async fn execute(
        &self,
        _id: &str,
        params: serde_json::Value,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        let path = params["path"].as_str().ok_or(ToolError::InvalidArgs("missing path".into()))?;
        let content = tokio::fs::read_to_string(path).await
            .map_err(|e| ToolError::Failed(e.to_string()))?;
        Ok(ToolResult {
            content: vec![Content::Text { text: content }],
            details: serde_json::Value::Null,
        })
    }
}
```

## Testing

Uses `MockProvider` for tests — no API keys needed:

```rust
use yo_agent::provider::mock::*;
use yo_agent::provider::MockProvider;

let provider = MockProvider::new(vec![
    MockResponse::ToolCalls(vec![MockToolCall {
        name: "read_file".into(),
        arguments: serde_json::json!({"path": "test.txt"}),
    }]),
    MockResponse::Text("File contents: hello".into()),
]);

let mut agent = Agent::new(provider)
    .with_system_prompt("test")
    .with_model("mock")
    .with_api_key("test");
```

## License

MIT
