# Context Management

Long-running agents accumulate messages that exceed the model's context window. yo-agent handles this automatically with tiered compaction.

## Token Estimation

Fast estimation without external tokenizer dependencies:

```rust
pub fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(4)  // ~4 chars per token for English
}
```

Also available: `message_tokens(&AgentMessage)` and `total_tokens(&[AgentMessage])`.

## ContextConfig

```rust
pub struct ContextConfig {
    pub max_context_tokens: usize,      // Default: 100,000
    pub system_prompt_tokens: usize,    // Default: 4,000
    pub keep_recent: usize,             // Default: 10
    pub keep_first: usize,             // Default: 2
    pub tool_output_max_lines: usize,   // Default: 50
}
```

## Tiered Compaction

`compact_messages()` tries each level in order, stopping as soon as messages fit the budget:

### Level 1: Truncate Tool Outputs

Replaces long tool outputs with head + tail (keeping first N/2 and last N/2 lines). This is the cheapest — preserves conversation structure, typically saves 50-70% in coding sessions.

### Level 2: Summarize Old Turns

Keeps the last `keep_recent` messages in full detail. Older assistant messages are replaced with one-line summaries like `"[Summary] [Assistant used 3 tool(s)]"`, and their tool results are dropped.

### Level 3: Drop Middle Messages

Keeps `keep_first` messages from the start and `keep_recent` from the end, dropping everything in between. A marker message notes how many were removed.

## ExecutionLimits

Prevents runaway agents:

```rust
pub struct ExecutionLimits {
    pub max_turns: usize,              // Default: 50
    pub max_total_tokens: usize,       // Default: 1,000,000
    pub max_duration: Duration,        // Default: 600s (10 min)
}
```

When a limit is reached, the agent stops with a message like `"[Agent stopped: Max turns reached (50/50)]"`.

## Disabling Context Management

```rust
let agent = Agent::new(provider)
    .without_context_management();
```

This sets both `context_config` and `execution_limits` to `None`.
