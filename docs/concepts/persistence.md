# State Persistence

yoagent supports saving and restoring agent conversation state, enabling pause/resume workflows, state transfer between processes, and conversation checkpointing.

## Save and Restore

```rust
use yoagent::agent::Agent;

// After running some conversation turns...
let json = agent.save_messages()?;
std::fs::write("conversation.json", &json)?;

// Later, in a new process:
let json = std::fs::read_to_string("conversation.json")?;
let mut agent = Agent::new(provider)
    .with_system_prompt("You are helpful.")
    .with_model("claude-sonnet-4-20250514")
    .with_api_key(api_key);

agent.restore_messages(&json)?;

// Continue the conversation — the agent sees the full history
let rx = agent.prompt("Follow up question").await;
```

## Builder Initialization

For constructing an agent with pre-existing history:

```rust
let saved: Vec<AgentMessage> = serde_json::from_str(&json)?;
let agent = Agent::new(provider)
    .with_messages(saved)
    .with_system_prompt("...")
    .with_model("...");
```

## JSON Format

Messages serialize as a JSON array. Each message is tagged by role:

```json
[
  {
    "role": "user",
    "content": [{"type": "text", "text": "Hello"}],
    "timestamp": 1700000000000
  },
  {
    "role": "assistant",
    "content": [{"type": "text", "text": "Hi there!"}],
    "stopReason": "stop",
    "model": "claude-sonnet-4-20250514",
    "provider": "anthropic",
    "usage": {"input": 100, "output": 50, "cache_read": 0, "cache_write": 0, "total_tokens": 150},
    "timestamp": 1700000001000
  }
]
```

Extension messages use a nested structure:

```json
{
  "role": "extension",
  "kind": "status_update",
  "data": {"status": "running"}
}
```

## Context Tracking

`ContextTracker` and `ExecutionTracker` are runtime-only and not persisted. This is by design — both are created fresh each `agent_loop()` invocation and operate on whatever messages are in context at that point. Restoring messages and calling `prompt()` works correctly without any special recalculation.

## What's Serializable

| Type | Serialize | Deserialize | PartialEq |
|------|-----------|-------------|-----------|
| `Content` | Yes | Yes | Yes |
| `Message` | Yes | Yes | Yes |
| `AgentMessage` | Yes | Yes | Yes |
| `ExtensionMessage` | Yes | Yes | Yes |
| `Usage` | Yes | Yes | Yes |
| `StopReason` | Yes | Yes | Yes |
| `ToolResult` | Yes | Yes | Yes |
| `CacheConfig` | Yes | Yes | Yes |
| `ToolExecutionStrategy` | Yes | Yes | Yes |
| `ContextConfig` | Yes | Yes | No |
| `ExecutionLimits` | Yes | Yes | No |
