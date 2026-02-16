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
let agent = Agent::new(provider)
    .with_system_prompt("You are helpful.")
    .with_model("claude-sonnet-4-20250514")
    .with_api_key("sk-...")
    .with_tools(default_tools())
    .with_thinking(ThinkingLevel::Medium)
    .with_max_tokens(4096)
    .with_context_config(ContextConfig::default())
    .with_execution_limits(ExecutionLimits::default());
```

### Prompting

```rust
// Text prompt
let rx: UnboundedReceiver<AgentEvent> = agent.prompt("Hello").await;

// Message prompt
let rx = agent.prompt_messages(vec![msg]).await;

// Continue from current state
let rx = agent.continue_loop().await;
```

### Steering & Follow-Ups

```rust
// Interrupt agent mid-execution
agent.steer(AgentMessage::Llm(Message::user("Stop, do this instead")));

// Queue work for after agent finishes
agent.follow_up(AgentMessage::Llm(Message::user("Also do this")));

// Queue delivery modes
agent.set_steering_mode(QueueMode::All);       // Deliver all at once
agent.set_follow_up_mode(QueueMode::OneAtATime); // One per turn
```

### State Management

```rust
agent.messages()          // &[AgentMessage]
agent.is_streaming()      // bool
agent.abort()             // Cancel current run
agent.reset()             // Clear all state
agent.clear_messages()    // Clear message history
agent.append_message(msg) // Add a message
agent.replace_messages(msgs) // Replace all messages
agent.set_tools(tools)    // Replace tools
```

## Re-exports

The crate re-exports key types from `lib.rs`:

```rust
pub use agent::Agent;
pub use agent_loop::{agent_loop, agent_loop_continue};
pub use types::*;  // Message, Content, AgentMessage, AgentEvent, etc.
```
