# Lifecycle Callbacks

yoagent provides three lifecycle callbacks that let you observe and control the agent loop without modifying its internals.

## Callbacks

### `before_turn`

Called before each LLM call. Receives the current message history and the turn number (0-indexed). Return `false` to abort the loop.

```rust
let agent = Agent::new(provider)
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

let agent = Agent::new(provider)
    .on_after_turn(move |_messages, usage| {
        let mut cost = cost_tracker.lock().unwrap();
        *cost += usage.input + usage.output;
        println!("Cumulative tokens: {}", *cost);
    });
```

### `on_error`

Called when the LLM returns a `StopReason::Error`. Receives the error message string.

```rust
let agent = Agent::new(provider)
    .on_error(|err| {
        eprintln!("LLM error: {}", err);
        // Log to monitoring, send alert, etc.
    });
```

## Combining Callbacks

All callbacks are optional and independent:

```rust
let agent = Agent::new(provider)
    .on_before_turn(|_msgs, turn| turn < 20)
    .on_after_turn(|msgs, usage| {
        println!("Messages: {}, Tokens: {}/{}", msgs.len(), usage.input, usage.output);
    })
    .on_error(|err| eprintln!("Error: {}", err));
```

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
  7. Execute tool calls
  8. Track turn
  9. after_turn(messages, usage)
  10. Emit TurnEnd event
```
