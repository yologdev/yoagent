# Lifecycle Callbacks

yoagent provides three lifecycle callbacks that let you observe and control the agent loop without modifying its internals.

## Callbacks

### `before_turn`

Called before each LLM call. Receives the current message history and the turn number (0-indexed). Return `false` to abort the loop.

```rust
let agent = Agent::from_config(ModelConfig::anthropic("claude-sonnet-5", "Claude Sonnet 5"))
    .on_before_turn(|messages, turn| {
        println!("Turn {} starting with {} messages", turn, messages.len());
        turn < 10 // Stop after 10 turns
    });
```

### `after_turn`

Called after each LLM response and tool execution. Receives the updated message history and the turn's token usage.

```rust
use std::sync::{Arc, Mutex};

let total_cost = Arc::new(Mutex::new(0u64));
let cost_tracker = total_cost.clone();

let agent = Agent::from_config(ModelConfig::anthropic("claude-sonnet-5", "Claude Sonnet 5"))
    .on_after_turn(move |_messages, usage| {
        let mut cost = cost_tracker.lock().unwrap();
        *cost += usage.input + usage.output;
        println!("Cumulative tokens: {}", *cost);
    });
```

### `on_error`

Called when the LLM returns a `StopReason::Error`. Receives the error message string.

```rust
let agent = Agent::from_config(ModelConfig::anthropic("claude-sonnet-5", "Claude Sonnet 5"))
    .on_error(|err| {
        eprintln!("LLM error: {}", err);
        // Log to monitoring, send alert, etc.
    });
```

## Combining Callbacks

All callbacks are optional and independent:

```rust
let agent = Agent::from_config(ModelConfig::anthropic("claude-sonnet-5", "Claude Sonnet 5"))
    .on_before_turn(|_msgs, turn| turn < 20)
    .on_after_turn(|msgs, usage| {
        println!("Messages: {}, Tokens: {}/{}", msgs.len(), usage.input, usage.output);
    })
    .on_error(|err| eprintln!("Error: {}", err));
```

## Async Input Filters

`InputFilter` is synchronous. A filter that awaits — a moderation API, a
classifier — implements `AsyncInputFilter` instead; it runs in the same
ordered list, with the same `Pass` / `Warn` / `Reject` semantics:

```rust
use yoagent::{AsyncInputFilter, FilterResult};

struct Moderation;

#[async_trait::async_trait]
impl AsyncInputFilter for Moderation {
    async fn filter(&self, text: &str) -> FilterResult {
        // call your moderation endpoint; bound its latency yourself
        FilterResult::Pass
    }
}

let agent = agent.with_async_input_filter(Moderation);
```

For a raw loop, push `Arc::new(AsyncFilter(Moderation))` onto
`AgentLoopConfig::input_filters`; the loop awaits it through
`InputFilter::as_async`.

## Turn Hooks

A `TurnHook` is awaited before **every LLM request** and may return one line
to append to that request's system prompt. The line is transient — never
stored in history — and a hook returning `None` leaves the request unchanged.

```rust
use yoagent::{TurnContext, TurnHook};

struct Reminder;

#[async_trait::async_trait]
impl TurnHook for Reminder {
    async fn before_turn(&self, turn: &TurnContext<'_>) -> Option<String> {
        let request = turn.latest_user_text()?;
        request.contains("deploy").then(|| "Deploys need a changelog entry.".to_string())
    }
}

let agent = agent.with_turn_hook(Reminder);
```

The system prompt precedes the conversation, so a line that changes between
turns invalidates the provider's prompt cache from there on; keep it stable
(derive it from the user's request, and memoize). Hooks run once per provider
call — a retried request runs them again. A panicking hook is contained.

Turn hooks reach the loop by wrapping the provider: `Agent` does this per run,
and a raw loop wraps its own with
`TurnHookProvider::new(provider, vec![Arc::new(hook)])`.

## Using with `AgentLoopConfig`

For direct loop usage without the `Agent` wrapper:

```rust
use std::sync::Arc;
use yoagent::agent_loop::AgentLoopConfig;

let config = AgentLoopConfig {
    before_turn: Some(Arc::new(|_msgs, turn| turn < 5)),
    after_turn: Some(Arc::new(|_msgs, _usage| { /* log */ })),
    on_error: Some(Arc::new(|err| eprintln!("{}", err))),
    // ... other fields
};
```

## Callback Timing

```
Loop iteration:
  1. Inject pending messages (steering/follow-up)
  2. Check execution limits
  3. before_turn(messages, turn_number)  <-- return false to abort
  4. Compact context
  5. Stream LLM response
  6. Check for error/abort → on_error(message) if StopReason::Error
     → after_turn(messages, usage) even on error/abort
  7. Execute tool calls
  8. Track turn
  9. after_turn(messages, usage)
  10. Emit TurnEnd event
```
