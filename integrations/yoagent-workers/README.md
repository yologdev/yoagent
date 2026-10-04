# yoagent-workers

Run [yoagent](https://crates.io/crates/yoagent) on Cloudflare Workers through
the Worker's own bindings.

Requires yoagent 0.24 or later (the first release with
`decision::parse_systemone_response`).

yoagent itself builds for `wasm32-unknown-unknown` (`default-features = false`)
and reaches every LLM provider over the Worker's `fetch` (see the
[WebAssembly & Cloudflare Workers guide](https://yologdev.github.io/yoagent/guides/wasm-workers.html)).
This crate adds what only a Worker has: the objects the Workers runtime hands
it in `env`, already authenticated as your account, so no API token is needed
(and, per Cloudflare, bindings are faster and less restricted than the REST
API). yoagent's core never depends on it.

| Module | Binding | Gives you |
|---|---|---|
| `ai` | Workers AI (`env.AI`) | Cloudflare's Clef decision models as a yoagent `DecisionModel` |

## Workers AI decision models: Clef

```toml
# wrangler.toml
[ai]
binding = "AI"
```

```toml
# Cargo.toml
[dependencies]
yoagent = { version = "0.24", default-features = false, features = ["decision"] }
yoagent-workers = "0.1"
```

Take both from the same source: crates.io for both as above, or git for both.
A crates.io `yoagent` next to a git `yoagent-workers` is two `yoagent` crates
whose types do not match.

```rust
// In a workers-rs fetch handler. `worker::Error` has no conversion from
// `DecisionError`, hence the `map_err`.
let clef = yoagent_workers::ai::clef(env.ai("AI")?);
let urgent = clef
    .noul("Checkout fails for every customer.", "Is this urgent?")
    .await
    .map_err(|e| worker::Error::RustError(e.to_string()))?;
```

`clef(..)` and `clef_flash(..)` take the binding as workers-rs's `Ai` or as
the raw `env.AI` `JsValue`, and return an ordinary `DecisionModel`: batched
questions and fallbacks (`.or(..)`) work, and it can be handed to an agent's
advisory hints, tool gate or input guard like any other decision model (the
tool gate has run live on the binding path, in the `clef-worker` example; the
advisory hints and input guard have not). They are priced at the
`prices.json` rate the price table holds when they are built (`cloudflare/clef`
$0.24, `cloudflare/clef-flash` $0.09 per million input tokens by default; a
later price override does not reprice them). That is the list price;
Cloudflare bills in neurons with a daily free allocation.

For another SystemOne model on Workers AI, or your own capabilities, retry
policy or price, use the backend directly:

```rust
use yoagent::decision::DecisionModel;
use yoagent_workers::ai::{AiBackend, CLEF};

let model = DecisionModel::from_backend(AiBackend::new(env.ai("AI")?, CLEF), "clef");
```

**A binding belongs to one request.** Build the decision model, and the agent
that uses it, inside the handler from that request's `env`.

**Errors.** What `env.AI.run` throws or rejects with is read for a leading
Workers AI error code, and classified like the REST API's:

| Binding error | `DecisionError` | Retried |
|---|---|---|
| `3040` out of capacity | `RateLimited` (429) | yes, `AiBackend::with_retry` (default 3, from 1 s) |
| `3036` daily free allocation used up | `Http` (429) | no |
| anything else | `Backend`, with the binding's message | no |

How the binding words its errors is not documented by Cloudflare, so the code
match is best effort. A value without a `run` method is `Invalid`, before
anything is called; a state over Clef's token limits is `Invalid` before the
binding runs. A `.or(..)` fallback applies to every failure.

Outside a Worker, or with an API token instead of a binding, use yoagent's own
`DecisionModel::clef(account_id)`, which calls the Workers AI REST API.

## Example

[`examples/clef-worker`](examples/clef-worker/) is a complete Worker, managed
with the Cloudflare CLI (`cf`): a yoagent agent on DeepSeek with two tools over
a set of notes, every tool call gated by Clef (`ToolGate`) through `env.AI`, so
a call Clef judges destructive and not requested is denied (a guardrail, not a
security boundary). About 392 KiB gzipped. It is type-checked and linted in CI,
and was run live once with `cf dev` against the real Workers AI binding
(2026-10-04): Clef allowed the read and the requested delete; see its README
for what was and was not observed.

## Testing

Everything here exists only on wasm32. The tests run under Node against fake
bindings (JavaScript objects whose `run` records its arguments):

```bash
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version <the resolved wasm-bindgen> --locked
CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner \
  cargo test --target wasm32-unknown-unknown   # from this directory
```

Live: `clef(..)` has run against a real Workers AI binding once, through the
`clef-worker` example under `cf dev` (2026-10-04). `clef_flash(..)` and the
error-code mapping (3040, 3036) have only run against the fakes above.
