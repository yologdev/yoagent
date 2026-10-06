# Retry with Backoff

When an LLM provider returns a transient error — a rate limit (HTTP 429), an overload (503, 529) or a network failure — yoagent automatically retries with exponential backoff and jitter. No configuration required; it works out of the box.

## How it works

```
Request → Error? → Retryable? → Wait (backoff + jitter) → Retry → ...
                       ↓ No
                  Fail immediately
```

1. The agent loop calls the provider
2. If the provider returns a retryable error:
   - If a `retry-after` delay was provided (rate limits), use that
   - Otherwise, calculate delay: `initial_delay × multiplier^(attempt-1)` with ±20% jitter
   - Wait, then retry
3. After `max_retries` retries (`max_retries + 1` attempts), the error propagates normally

## What gets retried

| Error Type | Retried? | Why |
|------------|----------|-----|
| `RateLimited` (429, and 503 / 529 overloaded) | ✅ Yes | Temporary — provider will accept requests again soon |
| `Network` | ✅ Yes | Transient — connection resets, timeouts, DNS failures |
| `Auth` (401/403) | ❌ No | Permanent — wrong API key won't fix itself |
| `Api` (400, 500, 502, …) | ❌ No | Permanent — a bad request or a server bug fails the same way again |
| `Cancelled` | ❌ No | User-initiated — respect the cancellation |

HTTP 503 ("service unavailable") and 529 (Anthropic's "overloaded") are
`RateLimited` too: the provider is out of capacity for now, and any
`Retry-After` it sends is honoured. An in-stream overload is retried the same
way: Anthropic's `overloaded_error`, a `status` of `UNAVAILABLE`, or a numeric
`code` of 503 / 529. Other 5xx responses stay `Api`. `RateLimited` carries no
message, so the server's explanation is logged at `WARN` when the error is
classified.

HTTP 429 is always `RateLimited`, even when the body contains a phrase that
would otherwise read as a context overflow. A mid-stream error event is
`RateLimited` when its structured `type`, `code` or `status` is
`too_many_requests`, `no_capacity`, `rate_limit_exceeded`, `rate_limit_error`
or `rate_limit` (Azure OpenAI's peak-load `no_capacity` error is the
motivating case), and is checked before the overflow phrases, so a capacity
error is retried rather than compacting the context.

## Default configuration

```rust
RetryConfig {
    max_retries: 3,          // Up to 3 retry attempts
    initial_delay_ms: 1000,  // 1 second before first retry
    backoff_multiplier: 2.0, // Double the delay each attempt
    max_delay_ms: 30_000,    // Cap at 30 seconds
}
```

With defaults, the retry delays are approximately:
- Attempt 1: ~1s
- Attempt 2: ~2s
- Attempt 3: ~4s

(±20% jitter to avoid thundering herd when multiple agents hit the same provider)

## Configuration

### Using the Agent builder

```rust
use yoagent::agent::Agent;
use yoagent::retry::RetryConfig;

// Default — 3 retries, exponential backoff (recommended)
let agent = Agent::from_config(ModelConfig::anthropic("claude-sonnet-5", "Claude Sonnet 5"));

// Custom — more retries, longer initial delay
let agent = Agent::from_config(ModelConfig::anthropic("claude-sonnet-5", "Claude Sonnet 5"))
    .with_retry_config(RetryConfig {
        max_retries: 5,
        initial_delay_ms: 2000,
        backoff_multiplier: 2.0,
        max_delay_ms: 60_000,
    });

// Disable retries entirely
let agent = Agent::from_config(ModelConfig::anthropic("claude-sonnet-5", "Claude Sonnet 5"))
    .with_retry_config(RetryConfig::none());
```

### Using AgentLoopConfig directly

```rust
use yoagent::agent_loop::AgentLoopConfig;
use yoagent::retry::RetryConfig;

let config = AgentLoopConfig {
    // ...other fields...
    retry_config: RetryConfig {
        max_retries: 3,
        initial_delay_ms: 1000,
        backoff_multiplier: 2.0,
        max_delay_ms: 30_000,
    },
};
```

## Rate limit headers

When a provider returns `ProviderError::RateLimited { retry_after_ms: Some(5000) }`, yoagent uses that delay instead of the calculated backoff, capped at `max_delay_ms` so a bad header cannot stall the loop. This respects the provider's guidance — if Anthropic says "retry after 5 seconds", we wait 5 seconds, not our own estimate.

If no `retry_after_ms` is provided, the exponential backoff kicks in.

## Observability

### Events

Each retried attempt is visible on the event stream. If the attempt opened a
message, it is closed with a `MessageEnd` carrying `StopReason::Error` and an
`error_message` of the form `attempt N of M failed and will be retried: …`;
then `AgentEvent::ProviderRetry { attempt, max_attempts, error, delay_ms }`
follows (`attempt` is 1-based, `max_attempts` is `max_retries + 1`). An attempt
that failed before streaming anything gets only the `ProviderRetry`. The final
failure is never followed by `ProviderRetry`, and only the successful answer
(or the final error) enters history. See
[Messages & Events](messages-events.md#wire-format) for how a client should
read the sequence.

### Append-only consumers

A retried attempt may already have streamed part of its answer before it
failed. A UI can take that text back when it sees the error `MessageEnd` and
the `ProviderRetry` after it. A consumer writing to an **append-only sink**
(stdout on a pipe, a log file, a stream to a client) cannot: without help it
prints the partial text and then the full answer from the retry.

`yoagent::retry::retry_safe_events` wraps the receiver so that each provider
attempt's events are held back until the attempt succeeds:

```rust
use yoagent::retry::retry_safe_events;

let mut rx = retry_safe_events(agent.prompt("hello").await);
while let Some(event) = rx.recv().await {
    // Only the text of attempts that succeeded arrives here.
}
agent.finish().await;
```

- A successful attempt arrives whole: its `MessageStart`, every
  `MessageUpdate` and its `MessageEnd`, all at once when it finishes.
- A retried attempt disappears; only its `ProviderRetry` remains. That
  includes the last one when the run is aborted during the retry's backoff:
  the turn's `Aborted` message then arrives on its own, as a `MessageStart`
  and `MessageEnd` with no deltas.
- An attempt that fails for good arrives as its `MessageStart` and its
  `Error` or `Aborted` `MessageEnd`, without its deltas. The `MessageEnd`'s
  `content` still holds what the attempt produced, so print it from there if
  you want it. (A final failure that streamed nothing is still announced with
  a `MessageStart` and `MessageEnd`, so it arrives the same way.)
- An attempt that is left open (the stream ends, or a retry or another
  attempt starts before it ends) is dropped, never released, and logged.
- Every other event passes through at once, so it can overtake an attempt
  being held. Use one filter per agent's stream; a sender shared by several
  agents would mix their attempts.

It covers this agent's own assistant messages. A sub-agent's text reaches the
parent as `ToolExecutionUpdate`s, which pass through unfiltered, retried text
included.

The trade is latency. Text arrives once per attempt rather than token by
token, so use it for **non-interactive** output, where nobody watches the
tokens arrive. Keep the live stream when your UI can rewind. `RetrySafeEvents`
is the same filter without the spawned task (`push` each event, then
`finish`, which also resets it for another stream), for consumers that
already run their own loop over the events.

### Interactive terminals

A person watching a terminal is better served by live streaming than by
buffering, and a terminal cannot erase the partial text either. For that case,
stop the run instead of letting it retry once text has appeared. Watch for
text or thinking deltas, and call `agent.abort()` when a `ProviderRetry`
arrives after some:

```rust
let mut rx = agent.prompt("hello").await;
let mut streamed = false;
while let Some(event) = rx.recv().await {
    match &event {
        // Only text from the current attempt counts.
        AgentEvent::TurnStart | AgentEvent::MessageStart { .. } => streamed = false,
        AgentEvent::MessageUpdate { delta, .. } => {
            if let StreamDelta::Text { delta } | StreamDelta::Thinking { delta } = delta {
                streamed |= !delta.is_empty();
                print!("{delta}");
            }
        }
        AgentEvent::ProviderRetry { error, .. } if streamed => {
            eprintln!("\n[stopped: {error}]");
            agent.abort();
        }
        _ => {}
    }
}
agent.finish().await;
```

The backoff races cancellation, so an abort that lands during it ends the
turn at once and no retry request is sent. The turn's message (in `AgentEnd`
and in history) carries `StopReason::Aborted`. No attempt streamed it, so it
arrives on its own after the `ProviderRetry`, as a `MessageStart` and an
`Aborted` `MessageEnd` with no text. That holds when the consumer reacts within
the backoff, which by default is about a second. A very short backoff (a small
`Retry-After`, or a tiny `initial_delay_ms`) can let the retry start first;
the abort then cancels it in flight (possibly after the provider began
billing) and the turn ends `Aborted`, unless the retry already finished.

An error that arrives before any text still retries as usual, for example a
rate limit or a refused connection. The user sees the partial answer and the
error, and decides whether to try again.

### Logs

Retry attempts are also logged via `tracing` at the `WARN` level:

```
WARN Provider error (attempt 1/3), retrying in 1.1s: Rate limited, retry after 1000ms
WARN Provider error (attempt 2/3), retrying in 2.3s: Rate limited, retry after 2000ms
```

Subscribe to tracing events in your application to surface these in your UI:

```rust
use tracing_subscriber;

// Simple stderr logging
tracing_subscriber::fmt::init();

// Or filter to just retries
tracing_subscriber::fmt()
    .with_env_filter("yoagent::retry=warn")
    .init();
```

## Design notes

- **Retry lives in the agent loop**, not inside individual providers. One config controls all retry behavior.
- **Jitter** prevents thundering herd: when many agents hit a rate limit simultaneously, jitter spreads their retries so they don't all retry at the same instant.
- **Cancellation is respected**: cancelling during a retry's backoff ends the turn at once, with no further request. The turn's message carries `StopReason::Aborted`, and `on_error` is not called.
- **No retry on API errors**: a malformed request will fail the same way every time. Retrying wastes time and tokens.
