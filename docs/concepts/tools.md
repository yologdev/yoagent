# Tools

## The AgentTool Trait

Every tool implements `AgentTool`:

```rust
#[async_trait]
pub trait AgentTool: Send + Sync {
    fn name(&self) -> &str;
    fn label(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> serde_json::Value;
    async fn execute(
        &self,
        tool_call_id: &str,
        params: serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError>;
}
```

| Method | Purpose |
|--------|---------|
| `name()` | Unique ID sent to LLM (e.g., `"bash"`) |
| `label()` | Human-readable name for UI (e.g., `"Run Command"`) |
| `description()` | Tells the LLM what the tool does |
| `parameters_schema()` | JSON Schema for the tool's parameters |
| `execute()` | Runs the tool, returns `ToolResult` or `ToolError` |

## ToolResult

```rust
pub struct ToolResult {
    pub content: Vec<Content>,
    pub details: serde_json::Value,
}
```

The `content` is sent back to the LLM. The `details` field holds metadata (not sent to the LLM) for UI/logging.

## ToolError

```rust
pub enum ToolError {
    Failed(String),
    NotFound(String),
    InvalidArgs(String),
    Cancelled,
}
```

Errors are converted to `ToolResult` with `is_error: true` and sent back to the LLM so it can recover.

## Implementing a Custom Tool

```rust
use yo_agent::types::*;
use async_trait::async_trait;

pub struct WeatherTool;

#[async_trait]
impl AgentTool for WeatherTool {
    fn name(&self) -> &str { "get_weather" }
    fn label(&self) -> &str { "Weather" }
    fn description(&self) -> &str {
        "Get current weather for a city."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "city": {
                    "type": "string",
                    "description": "City name"
                }
            },
            "required": ["city"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        let city = params["city"].as_str()
            .ok_or(ToolError::InvalidArgs("missing city".into()))?;

        // Call weather API...
        Ok(ToolResult {
            content: vec![Content::Text {
                text: format!("Weather in {}: 72°F, sunny", city),
            }],
            details: serde_json::Value::Null,
        })
    }
}
```

Register custom tools alongside defaults:

```rust
use yo_agent::tools::default_tools;

let mut tools = default_tools();
tools.push(Box::new(WeatherTool));
let agent = Agent::new(provider).with_tools(tools);
```

## Tool Execution Flow

1. LLM returns `Content::ToolCall` blocks in its response
2. Agent loop emits `ToolExecutionStart` for each
3. Tool's `execute()` is called with parsed arguments
4. Result (or error) is wrapped in `Message::ToolResult`
5. `ToolExecutionEnd` is emitted
6. All tool results are added to context
7. Loop continues with another LLM call

Tools execute **sequentially** (not in parallel). Between tools, steering messages are checked — if a user interrupts, remaining tools are skipped.
