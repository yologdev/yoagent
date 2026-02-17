# Context Management

Long-running agents accumulate messages that exceed the model's context window. yoagent provides token tracking, overflow detection, tiered compaction, and execution limits.

## Token Estimation

Fast estimation without external tokenizer dependencies:

```rust
use yoagent::context::{estimate_tokens, message_tokens, total_tokens};

estimate_tokens("Hello world");          // ~3 tokens (chars / 4)
message_tokens(&agent_message);          // estimate for a single message
total_tokens(&messages);                 // estimate for all messages
```

## Context Tracking

`ContextTracker` combines real token counts from provider responses with estimation for new messages — more accurate than pure estimation:

```rust
use yoagent::context::ContextTracker;

let mut tracker = ContextTracker::new();

// After each assistant response, record the real usage:
tracker.record_usage(&assistant_usage, message_index);

// Get current context size (real usage + estimated trailing):
let tokens = tracker.estimate_context_tokens(agent.messages());

// After compaction, reset the tracker:
tracker.reset();
```

When no usage data is available, it falls back to chars/4 estimation.

## Context Overflow Detection

When the context exceeds a model's window, providers return overflow errors. yoagent detects these automatically across all major providers.

### HTTP-level detection

Providers that check before streaming (Google, Bedrock, Vertex) return `ProviderError::ContextOverflow`:

```rust
use yoagent::provider::ProviderError;

match agent.prompt("...").await {
    // The loop already handles this — but you can also match it:
    Err(ProviderError::ContextOverflow { message }) => {
        // Compact and retry
    }
    _ => {}
}
```

`ProviderError::classify()` auto-detects overflow from error messages covering Anthropic, OpenAI, Google, AWS Bedrock, xAI, Groq, OpenRouter, llama.cpp, LM Studio, MiniMax, Kimi, GitHub Copilot, and generic patterns.

### Message-level detection

SSE-based providers (Anthropic, OpenAI) return overflow as a `StopReason::Error` message. Check with:

```rust
if message.is_context_overflow() {
    // Compact and retry
}
```

### Handling overflow in your application

yoagent provides the detection and building blocks. Your application wires the compaction strategy:

```rust
// Proactive: check before each prompt
let tokens = tracker.estimate_context_tokens(agent.messages());
if tokens > context_window - reserve {
    let compacted = compact_messages(agent.messages().to_vec(), &config);
    agent.replace_messages(compacted);
}

// Reactive: catch overflow errors
// ... on ContextOverflow or message.is_context_overflow():
//   compact, then retry with agent.continue_loop()
```

For LLM-based summarization (asking the model to summarize old messages), implement that in your application layer — yoagent provides `replace_messages()` and `compact_messages()` as building blocks.

## ContextConfig

```rust
pub struct ContextConfig {
    pub max_context_tokens: usize,      // Default: 100,000
    pub system_prompt_tokens: usize,    // Default: 4,000
    pub keep_recent: usize,             // Default: 10
    pub keep_first: usize,              // Default: 2
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
