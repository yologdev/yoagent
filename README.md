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
├── types.rs            # Core types: Message, AgentMessage, AgentEvent, AgentTool trait
├── agent_loop.rs       # The core loop (agent_loop + agent_loop_continue)
├── agent.rs            # Stateful Agent with steering/follow-up queues
├── context.rs          # Context management, token estimation, smart truncation
├── tools/
│   ├── bash.rs         # Shell execution (timeout, output truncation, deny patterns)
│   ├── file.rs         # Read/write files (line numbers, path restrictions)
│   ├── edit.rs         # Surgical search/replace editing
│   ├── list.rs         # Directory listing via find
│   └── search.rs       # Pattern search via ripgrep/grep
└── provider/
    ├── traits.rs           # StreamProvider trait, StreamEvent, ProviderError
    ├── model.rs            # ModelConfig, ApiProtocol, CostConfig, OpenAiCompat
    ├── registry.rs         # ProviderRegistry — dispatch by API protocol
    ├── anthropic.rs        # Anthropic Messages API (Claude)
    ├── openai_compat.rs    # OpenAI Chat Completions (OpenAI, xAI, Groq, Mistral, etc.)
    ├── openai_responses.rs # OpenAI Responses API
    ├── azure_openai.rs     # Azure OpenAI
    ├── google.rs           # Google Generative AI (Gemini)
    ├── google_vertex.rs    # Google Vertex AI
    ├── bedrock.rs          # Amazon Bedrock (ConverseStream)
    ├── sse.rs              # Shared SSE parsing utility
    └── mock.rs             # Mock provider for testing
```

## Providers

yo-agent supports **7 API protocols** covering **20+ providers** through a modular registry:

| Protocol | Providers |
|---|---|
| Anthropic Messages | Anthropic (Claude) |
| OpenAI Completions | OpenAI, xAI, Groq, Cerebras, OpenRouter, Mistral, MiniMax, HuggingFace, Kimi, DeepSeek |
| OpenAI Responses | OpenAI (newer API) |
| Azure OpenAI | Azure OpenAI |
| Google Generative AI | Google Gemini |
| Google Vertex | Google Vertex AI |
| Bedrock ConverseStream | Amazon Bedrock |

OpenAI-compatible providers share one implementation with per-provider quirk flags (`OpenAiCompat`) for differences in auth, reasoning format, tool handling, etc.

```rust
use yo_agent::provider::{ModelConfig, ApiProtocol, ProviderRegistry};

// Anthropic
let model = ModelConfig::anthropic("claude-sonnet-4-20250514");

// Any OpenAI-compatible provider
let model = ModelConfig::openai_compat("groq", "llama-3.3-70b", "https://api.groq.com/openai/v1");

// Google Gemini
let model = ModelConfig::google("gemini-2.5-pro");

// Registry dispatches to the right provider
let registry = ProviderRegistry::default();
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
