# Lifecycle Callbacks

yoagent provides three lifecycle callbacks that let you observe and control the agent loop without modifying its internals.

## Callbacks

### `before_turn`

Called before each LLM call. Receives the current message history and the turn number (0-indexed). Return `false` to end the run: no assistant message is produced for that turn, and `AgentEnd` still fires.

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

Called when a turn ends with `StopReason::Error` (after any retries are exhausted). Receives the error message string. It is not called for a cancelled run (`StopReason::Aborted`) or for a provider attempt that is retried.

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

The examples on this page use plain `#[async_trait::async_trait]`, which
builds on native targets only; for wasm32 use the `cfg_attr` pair in
[WebAssembly & Cloudflare Workers](../guides/wasm-workers.md#writing-tools-and-providers-for-both-targets).

**You own the timeout**: the loop awaits the filter as long as it takes.
A filter that panics is contained and treated as a `Reject` (fail closed):
the run ends with `AgentEvent::InputRejected` and the agent keeps its tools
and history.

For a raw loop, push `Arc::new(AsyncFilter::new(Moderation))` onto
`AgentLoopConfig::input_filters`; the loop awaits it through
`InputFilter::as_async`.

A sub-agent takes one too: `SubAgentTool::with_async_input_filter(Moderation)`
screens the task the parent model hands it. A `Warn` is appended to the task;
a `Reject` means the sub-agent never runs and the tool call fails with the
reason, which the parent's model sees.

Input filters see a run's **prompts** only: messages queued with
`Agent::steer` / `Agent::follow_up` enter the loop without passing through
them. With the `decision` feature, `Agent::with_input_guard` is a ready-made
async filter backed by a decision model (see
[Decision Models](decision-models.md#blocking-with_input_guard)).

## Turn Hooks

A `TurnHook` is awaited before **every LLM request** and may return one note
to append to that request's **latest user turn**. The note is transient —
never stored in history, never in the system prompt — and a hook returning
`None` leaves the request unchanged.

```rust
use yoagent::{TurnContext, TurnHook};

struct Reminder;

#[async_trait::async_trait]
impl TurnHook for Reminder {
    async fn before_turn(&self, turn: &TurnContext<'_>) -> Option<String> {
        let request = turn.user_request()?;
        request.contains("deploy").then(|| "Deploys need a changelog entry.".to_string())
    }
}

let agent = agent.with_turn_hook(Reminder);
```

The note is on the latest user message; everything before that message is
unchanged, so the provider's cached prefix up to it survives (during tool
turns, assistant and tool-result messages follow the note). On the next user
prompt the previous user message is sent without its note, so that last
exchange is **re-processed** — a cache miss from there, not a cache hit.
Keep a note stable within one request (derive it from the user's request,
and memoize) so its tool-calling turns cache too. Hooks
run once per provider call — a retried request runs them again. A panicking
hook is contained. `TurnContext::new(..)` builds a context to unit-test a
hook; `latest_user_text()` and `user_request()` skip the user-role messages
the loop injects itself (compaction summaries, limit notes, the loop nudge).

Turn hooks reach the loop by wrapping the provider: `Agent` does this per run,
`SubAgentTool::with_turn_hook(hook)` does it for a sub-agent's own requests,
and a raw loop wraps its own with
`TurnHookProvider::new(provider, vec![Arc::new(hook)])`.

`TurnContext::user_request_parts()` returns the same selection as
`user_request()` as a structured `UserRequestParts` (`latest`, `reply:
Option<ReplyContext { question, earlier_request }>`, `source:
UserRequestSource`, `run_prompts`). Prefer it to
parsing the prose of `user_request()`, whose labels and layout are not a
stable format.

## Using with `AgentLoopConfig`

For direct loop usage without the `Agent` wrapper:

```rust
use std::sync::Arc;
use yoagent::agent_loop::AgentLoopConfig;

let mut config = AgentLoopConfig::new(provider, "model");
config.before_turn = Some(Arc::new(|_msgs, turn| turn < 5));
config.after_turn = Some(Arc::new(|_msgs, _usage| { /* log */ }));
config.on_error = Some(Arc::new(|err| eprintln!("{}", err)));
```

## Callback Timing

```
Loop iteration:
  1. Inject pending messages (steering/follow-up)
  2. Check execution limits
  3. before_turn(messages, turn_number)  <-- return false to abort
  4. Compact context
  5. Stream LLM response (a retryable error closes the attempt with
     MessageEnd(Error) + ProviderRetry, waits, and re-requests; turn hooks
     run again for each attempt)
  6. Check for error/abort → on_error(message) only if StopReason::Error
     (not Aborted) → after_turn(messages, usage) on both
  7. Execute tool calls
  8. Track turn
  9. after_turn(messages, usage)
  10. Emit TurnEnd event
```
