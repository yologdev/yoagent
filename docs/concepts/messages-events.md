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
    Extension(ExtensionMessage),
}

pub struct ExtensionMessage {
    pub role: String,
    pub kind: String,
    pub data: serde_json::Value,
}
```

Create extension messages with the convenience constructor:

```rust
let ext = ExtensionMessage::new("status_update", serde_json::json!({"status": "running"}));
let msg = AgentMessage::Extension(ext);
```

The `kind` field categorizes the extension (e.g., `"status_update"`, `"ui_event"`, `"notification"`). Use `as_llm()` to extract the `Message` if it's an LLM message. The default `convert_to_llm` function filters out `Extension` messages before sending to the provider.

All core message types implement `Serialize`, `Deserialize`, `Clone`, and `PartialEq`, enabling state persistence and test assertions.

## Content

Each message contains `Vec<Content>`:

```rust
pub enum Content {
    Text { text: String },
    Image { data: String, mime_type: String },
    Thinking {
        thinking: String,
        signature: Option<String>,
        redacted: Option<String>, // provider-encrypted reasoning (Anthropic `redacted_thinking`, Bedrock `redactedContent`)
        redacted_protocol: Option<ApiProtocol>, // the API that produced `redacted`
    },
    ToolCall {
        id: String,
        name: String,
        arguments: serde_json::Value,
        provider_metadata: Option<serde_json::Value>, // e.g. Gemini thought signatures
    },
}
```

An assistant message can contain multiple content blocks — e.g., thinking + text + tool calls.

Redacted (encrypted) reasoning is opaque. It is sent back unmodified, and only
to the API protocol recorded in `redacted_protocol`; every other provider skips
it. A block without a recorded protocol (a session saved before the field
existed) is sent nowhere. Both fields are omitted from JSON when `None`.

`Content` is `#[non_exhaustive]` (match with a wildcard arm), and the `ToolCall` and `Thinking` variants are separately `#[non_exhaustive]` — construct them via `Content::tool_call()` / `tool_call_with_metadata()` / `thinking()` / `thinking_signed()` / `thinking_redacted()`. `Message::Assistant` is likewise `#[non_exhaustive]`; custom providers construct it via `Message::assistant()`.

## StopReason

```rust
pub enum StopReason {
    Stop,       // Natural completion
    Length,     // Hit max tokens
    ToolUse,    // Wants to call tools
    Error,      // Provider error
    Aborted,    // Run cancelled during an LLM call or a retry's backoff (on_error is not called)
    Refusal,    // Declined by the provider's safety system
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
| `AgentEnd { messages, stats }` | Loop finishes: all new messages plus the run's `SessionStats` |
| `TurnStart` | New LLM call starting |
| `TurnEnd { message, tool_results }` | LLM call + tool execution complete. Every `TurnStart` gets one: a turn stopped before its LLM call (an execution limit, or `on_before_turn` returning `false`) ends with the history's last message and no tool results |
| `MessageStart { message }` | A message is available |
| `MessageUpdate { message, delta }` | Streaming delta arrived |
| `MessageEnd { message }` | Message finalized |
| `ToolExecutionStart { tool_call_id, tool_name, args }` | Tool about to run |
| `ToolExecutionUpdate { tool_call_id, tool_name, partial_result }` | Tool progress |
| `ToolExecutionEnd { tool_call_id, tool_name, result, is_error }` | Tool finished |
| `ProgressMessage { tool_call_id, tool_name, text }` | User-facing progress text from a tool |
| `InputRejected { reason }` | Input filter rejected the user's message |
| `ProviderRetry { attempt, max_attempts, error, delay_ms }` | A provider attempt failed with a retryable error; the next starts after `delay_ms` |
| `LoopDetected { tool_name, repetitions, aborted }` | Identical tool calls tripped loop detection (`aborted`: the run stopped) |
| `ContextCompacted { method, messages_before, messages_after, tokens_before, tokens_after, summary }` | History was compacted before a turn (sent by `LlmCompaction` to the sender given to its `with_event_sender`, not by the loop itself) |

### Wire format

`AgentEvent` and `StreamDelta` serialize as internally-tagged camelCase JSON, so
external frontends (a websocket fanout server, a TypeScript client, a JSONL
pipe) can consume the event stream directly:

```json
{"type":"messageUpdate","message":{...},"delta":{"type":"text","delta":"hi"}}
{"type":"toolExecutionEnd","toolCallId":"tc_1","toolName":"bash","result":{...},"isError":false}
```

This shape is a **public contract** frozen by snapshot tests — variant tags,
field names, and the tagging scheme won't change in minor releases.

Streaming semantics: clients accumulate text from each `MessageUpdate`'s
`delta`; the `message` field during streaming is an empty-content
placeholder (the complete message arrives as a new value in `MessageEnd`).
Reset accumulation on each `MessageStart`.

Retries: when a provider attempt fails with a retryable error, every event it
produced is delivered, then the attempt is closed and marked:

```text
messageStart → messageUpdate… → messageEnd (stopReason "error",
  errorMessage "attempt 1 of 4 failed and will be retried: …") → providerRetry
messageStart → messageUpdate… → messageEnd (the retry's result)
```

Discard the partial text of a message that ends with `stopReason: "error"`.
That `messageEnd` is **not** the turn's result when `providerRetry` follows
it, so a client that reports failures should look at the next event first. An
attempt that failed before streaming anything has no `messageStart`, so it
gets only the `providerRetry`. A final failure is closed with the error the
turn returns and is never followed by `providerRetry`; one that streamed
nothing (or a run cancelled before any output) is still announced with a
`messageStart` and `messageEnd`, so every message in the history has its
events. A run cancelled during
an LLM call ends with `stopReason: "aborted"`, including one cancelled during
a retry's backoff. Cancelling between turns or while tools run ends the run
without a new assistant message. A tool call the run has not started yet when
it is cancelled is answered with an error result ("the run was cancelled")
and never runs; a tool already running sees the cancel through its
`ToolContext`. It appends a user message
`[Agent stopped: cancelled]` (`agent_loop::CANCELLED_MARKER`, emitted as
`messageStart` / `messageEnd`) when the run had produced an assistant message,
so a last assistant message of `ToolUse` does not read as a normal stop. A
sub-agent cancelled that way fails its delegation.

A Rust consumer that cannot discard text it already wrote (stdout, a log) can
wrap its receiver in `retry::retry_safe_events`, which removes retried
attempts before they reach it. See
[Retry: append-only consumers](retry.md#append-only-consumers).

A client that misses events entirely (e.g. a lagged websocket subscriber)
resyncs from the next `MessageEnd` without replay.

## StreamDelta

Deltas within `MessageUpdate`:

```rust
pub enum StreamDelta {
    Text { delta: String },
    Thinking { delta: String },
    ToolCallDelta { delta: String },
}
```

## Agent State

The `Agent` struct provides access to its current state:

```rust
// Check if the agent is currently streaming a response
if agent.is_streaming() {
    // Use steer() or follow_up() instead of prompt()
    agent.steer(AgentMessage::Llm(Message::user("New instruction")));
}

// Access the full message history
let messages: &[AgentMessage] = agent.messages();

// Check the last message
if let Some(last) = messages.last() {
    println!("Last message role: {}", last.role());
}
```

The `is_streaming()` flag is `true` between `prompt()`/`continue_loop()` call and completion. While streaming, calling `prompt()` will panic — use `steer()` or `follow_up()` instead.
