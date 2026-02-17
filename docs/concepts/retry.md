# Retry with Backoff

When an LLM provider returns a transient error — rate limit (HTTP 429) or network failure — yoagent automatically retries with exponential backoff and jitter. No configuration required; it works out of the box.

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
3. After `max_retries` attempts, the error propagates normally

## What gets retried

| Error Type | Retried? | Why |
|------------|----------|-----|
| `RateLimited` (429) | ✅ Yes | Temporary — provider will accept requests again soon |
| `Network` | ✅ Yes | Transient — connection resets, timeouts, DNS failures |
| `Auth` (401/403) | ❌ No | Permanent — wrong API key won't fix itself |
| `Api` (400, etc.) | ❌ No | Permanent — bad request won't change on retry |
| `Cancelled` | ❌ No | User-initiated — respect the cancellation |

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
let agent = Agent::new(provider);

// Custom — more retries, longer initial delay
let agent = Agent::new(provider)
    .with_retry_config(RetryConfig {
        max_retries: 5,
        initial_delay_ms: 2000,
        backoff_multiplier: 2.0,
        max_delay_ms: 60_000,
    });

// Disable retries entirely
let agent = Agent::new(provider)
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

When a provider returns `ProviderError::RateLimited { retry_after_ms: Some(5000) }`, yoagent uses that exact delay instead of the calculated backoff. This respects the provider's guidance — if Anthropic says "retry after 5 seconds", we wait 5 seconds, not our own estimate.

If no `retry_after_ms` is provided, the exponential backoff kicks in.

## Observability

Retry attempts are logged via `tracing` at the `WARN` level:

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
- **Cancellation is respected**: if the user cancels while waiting for a retry, the loop exits immediately.
- **No retry on API errors**: a malformed request will fail the same way every time. Retrying wastes time and tokens.
