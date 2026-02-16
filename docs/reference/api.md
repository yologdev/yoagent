# API Reference

## Top-Level Functions

### `agent_loop()`

```rust
pub async fn agent_loop(
    prompts: Vec<AgentMessage>,
    context: &mut AgentContext,
    config: &AgentLoopConfig<'_>,
    tx: mpsc::UnboundedSender<AgentEvent>,
    cancel: CancellationToken,
) -> Vec<AgentMessage>
```

Start an agent loop with new prompt messages. Returns all messages generated during the run.

### `agent_loop_continue()`

```rust
pub async fn agent_loop_continue(
    context: &mut AgentContext,
    config: &AgentLoopConfig<'_>,
    tx: mpsc::UnboundedSender<AgentEvent>,
    cancel: CancellationToken,
) -> Vec<AgentMessage>
```

Resume from existing context. The last message must not be an assistant message.

### `default_tools()`

```rust
pub fn default_tools() -> Vec<Box<dyn AgentTool>>
```

Returns: `BashTool`, `ReadFileTool`, `WriteFileTool`, `EditFileTool`, `ListFilesTool`, `SearchTool`.

## Agent Struct

High-level stateful wrapper around the agent loop.

### Construction

```rust
let agent = Agent::new(provider);
```

| Signature | Description |
|-----------|-------------|
| `Agent::new(provider: impl StreamProvider + 'static) -> Self` | Create a new agent with the given provider |

### Builder Methods

All return `Self` for chaining.

| Method | Description |
|--------|-------------|
| `with_system_prompt(prompt: impl Into<String>) -> Self` | Set the system prompt |
| `with_model(model: impl Into<String>) -> Self` | Set the model identifier |
| `with_api_key(key: impl Into<String>) -> Self` | Set the API key |
| `with_thinking(level: ThinkingLevel) -> Self` | Set thinking level (`Off`, `Minimal`, `Low`, `Medium`, `High`) |
| `with_tools(tools: Vec<Box<dyn AgentTool>>) -> Self` | Set tools |
| `with_max_tokens(max: u32) -> Self` | Set max output tokens |
| `with_context_config(config: ContextConfig) -> Self` | Set context compaction config |
| `with_execution_limits(limits: ExecutionLimits) -> Self` | Set execution limits (max turns, tokens, duration) |
| `without_context_management() -> Self` | Disable automatic context compaction and execution limits |
| `async with_mcp_server_stdio(command, args, env) -> Result<Self, McpError>` | Connect to MCP server via stdio and add its tools |
| `async with_mcp_server_http(url) -> Result<Self, McpError>` | Connect to MCP server via HTTP and add its tools |

### Prompting

| Method | Description |
|--------|-------------|
| `async prompt(text: impl Into<String>) -> UnboundedReceiver<AgentEvent>` | Send a text prompt, returns event stream |
| `async prompt_messages(messages: Vec<AgentMessage>) -> UnboundedReceiver<AgentEvent>` | Send messages as prompt |
| `async continue_loop() -> UnboundedReceiver<AgentEvent>` | Resume from current context (for retries) |

### State Access

| Method | Description |
|--------|-------------|
| `messages() -> &[AgentMessage]` | Get the full message history |
| `is_streaming() -> bool` | Whether the agent is currently running |

### State Mutation

| Method | Description |
|--------|-------------|
| `set_tools(tools: Vec<Box<dyn AgentTool>>)` | Replace the tool set |
| `clear_messages()` | Clear all messages |
| `append_message(msg: AgentMessage)` | Add a message to history |
| `replace_messages(msgs: Vec<AgentMessage>)` | Replace all messages |

### Steering & Follow-Up Queues

| Method | Description |
|--------|-------------|
| `steer(msg: AgentMessage)` | Queue a steering message (interrupts mid-tool-execution) |
| `follow_up(msg: AgentMessage)` | Queue a follow-up message (processed after agent finishes) |
| `clear_steering_queue()` | Clear pending steering messages |
| `clear_follow_up_queue()` | Clear pending follow-up messages |
| `clear_all_queues()` | Clear both queues |
| `set_steering_mode(mode: QueueMode)` | Set delivery mode: `OneAtATime` or `All` |
| `set_follow_up_mode(mode: QueueMode)` | Set delivery mode: `OneAtATime` or `All` |

### Control

| Method | Description |
|--------|-------------|
| `abort()` | Cancel the current run via `CancellationToken` |
| `reset()` | Clear all state (messages, queues, streaming flag) |

## Re-exports

The crate re-exports key types from `lib.rs`:

```rust
pub use agent::Agent;
pub use agent_loop::{agent_loop, agent_loop_continue};
pub use types::*;  // Message, Content, AgentMessage, AgentEvent, etc.
```
