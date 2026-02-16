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
use yoagent::types::*;
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
use yoagent::tools::default_tools;

let mut tools = default_tools();
tools.push(Box::new(WeatherTool));
let agent = Agent::new(provider).with_tools(tools);
```

## Error Handling

**Return `Err(ToolError)` on failure, not `Ok` with error text.** When a tool returns `Err`, the agent loop converts it to a `Message::ToolResult` with `is_error: true` and sends it to the LLM. The LLM sees the error and can self-correct — retry with different arguments, try a different approach, or explain the failure to the user.

```rust
async fn execute(&self, _id: &str, params: serde_json::Value, _cancel: CancellationToken) -> Result<ToolResult, ToolError> {
    let path = params["path"].as_str()
        .ok_or(ToolError::InvalidArgs("missing 'path'".into()))?;

    let content = std::fs::read_to_string(path)
        .map_err(|e| ToolError::Failed(format!("Cannot read {}: {}", path, e)))?;

    Ok(ToolResult {
        content: vec![Content::Text { text: content }],
        details: serde_json::Value::Null,
    })
}
```

**Exception: BashTool.** The built-in `BashTool` returns `Ok` even on non-zero exit codes, with both stdout and stderr in the result. This is intentional — the LLM needs to see the actual error output (compilation errors, test failures, etc.) to diagnose and fix issues. Only truly exceptional failures (e.g., command not found, cancellation) return `Err`.

## Tool Execution Flow

1. LLM returns `Content::ToolCall` blocks in its response
2. Agent loop emits `ToolExecutionStart` for each
3. Tool's `execute()` is called with parsed arguments
4. Result (or error) is wrapped in `Message::ToolResult`
5. `ToolExecutionEnd` is emitted
6. All tool results are added to context
7. Loop continues with another LLM call

Tools execute **sequentially** (not in parallel). Between tools, steering messages are checked — if a user interrupts, remaining tools are skipped.
