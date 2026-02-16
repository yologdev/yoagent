# Messages & Events

## Message Types

### `Message`

The core LLM message type, tagged by role:

```rust
pub enum Message {
    User {
        content: Vec<Content>,
        timestamp: u64,
    },
    Assistant {
        content: Vec<Content>,
        stop_reason: StopReason,
        model: String,
        provider: String,
        usage: Usage,
        timestamp: u64,
        error_message: Option<String>,
    },
    ToolResult {
        tool_call_id: String,
        tool_name: String,
        content: Vec<Content>,
        is_error: bool,
        timestamp: u64,
    },
}
```

Create user messages easily:

```rust
let msg = Message::user("Hello, world!");
```

### `AgentMessage`

Wraps `Message` with support for extension messages (UI-only, notifications, etc.):

```rust
pub enum AgentMessage {
    Llm(Message),
    Extension {
        role: String,
        data: serde_json::Value,
    },
}
```

Use `as_llm()` to extract the `Message` if it's an LLM message. The default `convert_to_llm` function filters out `Extension` messages before sending to the provider.

## Content

Each message contains `Vec<Content>`:

```rust
pub enum Content {
    Text { text: String },
    Image { data: String, mime_type: String },
    Thinking { thinking: String, signature: Option<String> },
    ToolCall { id: String, name: String, arguments: serde_json::Value },
}
```

An assistant message can contain multiple content blocks — e.g., thinking + text + tool calls.

## StopReason

```rust
pub enum StopReason {
    Stop,       // Natural completion
    Length,     // Hit max tokens
    ToolUse,    // Wants to call tools
    Error,      // Provider error
    Aborted,    // Cancelled by user
}
```

## Usage

Token usage from the provider:

```rust
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub total_tokens: u64,
}
```

## AgentEvent

Events emitted during the agent loop for real-time UI updates:

| Event | When |
|-------|------|
| `AgentStart` | Loop begins |
| `AgentEnd { messages }` | Loop finishes, all new messages |
| `TurnStart` | New LLM call starting |
| `TurnEnd { message, tool_results }` | LLM call + tool execution complete |
| `MessageStart { message }` | A message is available |
| `MessageUpdate { message, delta }` | Streaming delta arrived |
| `MessageEnd { message }` | Message finalized |
| `ToolExecutionStart { tool_call_id, tool_name, args }` | Tool about to run |
| `ToolExecutionUpdate { tool_call_id, tool_name, partial_result }` | Tool progress |
| `ToolExecutionEnd { tool_call_id, tool_name, result, is_error }` | Tool finished |

## StreamDelta

Deltas within `MessageUpdate`:

```rust
pub enum StreamDelta {
    Text { delta: String },
    Thinking { delta: String },
    ToolCallDelta { delta: String },
}
```
