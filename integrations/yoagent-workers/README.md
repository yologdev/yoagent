# yoagent-workers

Run [yoagent](https://crates.io/crates/yoagent) on Cloudflare Workers through
the Worker's own bindings.

**Status:** 0.1.0, not on crates.io. It needs
`yoagent::decision::parse_systemone_response`, which ships in the yoagent
release after 0.23.0. Until then, depend on it by git.

yoagent itself builds for `wasm32-unknown-unknown` (`default-features = false`)
and reaches every LLM provider over the Worker's `fetch` (see the
[WebAssembly & Cloudflare Workers guide](https://yologdev.github.io/yoagent/guides/wasm-workers.html)).
This crate adds what only a Worker has: the objects the Workers runtime hands
it in `env`, already authenticated as your account, with no API token and no
trip over the public internet. yoagent's core never depends on it.

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
yoagent = { version = "0.23", default-features = false, features = ["decision"] }
yoagent-workers = { git = "https://github.com/yologdev/yoagent" }
```

```rust
// In the Worker's fetch handler (workers-rs):
let clef = yoagent_workers::ai::clef(env.ai("AI")?);
let urgent = clef
    .noul("Checkout fails for every customer.", "Is this urgent?")
    .await?;
```

`clef(..)` and `clef_flash(..)` take the binding as workers-rs's `Ai` or as
the raw `env.AI` `JsValue`, and return an ordinary `DecisionModel`: batched
questions, fallbacks (`.or(..)`), and inside an agent the advisory hints, tool
gate and input guard all work as with any other decision model. They are
priced from yoagent's price table (`cloudflare/clef` $0.24, `cloudflare/clef-flash`
$0.09 per million input tokens).

For another SystemOne model on Workers AI, or your own capabilities or price,
use the backend directly:

```rust
use yoagent::decision::DecisionModel;
use yoagent_workers::ai::{AiBackend, CLEF};

let model = DecisionModel::from_backend(AiBackend::new(env.ai("AI")?, CLEF), "clef");
```

**A binding belongs to one request.** Build the decision model, and the agent
that uses it, inside the handler from that request's `env`.

**Errors.** Whatever `env.AI.run` throws or rejects with (capacity, account
limits, a bad request) becomes `DecisionError::Backend` carrying its message.
It is not retried; a `.or(..)` fallback still applies. A value without a `run`
method is `DecisionError::Invalid`, before anything is called.

Outside a Worker, or with an API token instead of a binding, use yoagent's own
`DecisionModel::clef(account_id)`, which calls the Workers AI REST API.

## Testing

Everything here exists only on wasm32. The tests run under Node against fake
bindings (JavaScript objects whose `run` records its arguments):

```bash
cargo install wasm-bindgen-cli --version <the resolved wasm-bindgen> --locked
CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner \
  cargo test --target wasm32-unknown-unknown   # from this directory
```

Not tested against a real Workers AI binding (that needs `wrangler dev` and a
Cloudflare account).
