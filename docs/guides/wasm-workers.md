# WebAssembly & Cloudflare Workers

yoagent builds for `wasm32-unknown-unknown`, so the agent loop can run inside a
JavaScript host such as a Cloudflare Worker. Turn off default features, which
removes what needs an operating system:

```toml
[dependencies]
yoagent = { version = "0.23", default-features = false }
```

```bash
rustup target add wasm32-unknown-unknown
cargo build --target wasm32-unknown-unknown
```

0.23 is the first release with the `native` feature; 0.22 and earlier do not
build for wasm32.

> **Upgrading from 0.22 with `default-features = false`?** That line did
> nothing before 0.23, because there were no default features. It now turns
> `native` off, which removes the built-in tools and, more subtly, reqwest's
> default TLS: the build succeeds but every HTTPS provider call fails at
> runtime. Native users who set it add `features = ["native"]`.

## What is available

| On wasm32 | Not on wasm32 |
|-----------|--------------------------------------------|
| The agent loop, `Agent`, retries, execution limits | `BashTool`, `ReadFileTool`, `WriteFileTool`, `EditFileTool`, `ListFilesTool`, `SearchTool`, `default_tools()` |
| All providers, over the host's `fetch` | `StdioTransport` / `with_mcp_server_stdio` |
| `AgentTool`s you write; HTTP MCP (`with_mcp_server_http`) | `FileBackend` for `SharedState` |
| `SharedState` (in memory, or your own `SharedStateBackend`), sub-agents | `SkillSet` loading (compiles, but finds no directories: there is no filesystem) |
| The `decision` feature | `PriceTable::fetch_cached` (its disk cache) |
| Compaction: `LlmCompaction` runs only its deterministic tiers and never summarises | reqwest's default features (rustls TLS, HTTP/2, system proxy settings, charset decoding); SOCKS proxies |
| | The `openapi` and `gasp` features (they need the filesystem and a full Tokio) |

Everything in the right-hand column except `SkillSet` and the `openapi` /
`gasp` features is gated by the `native` feature, which is on by default, so
a dependency that keeps default features sees no difference.

Only `wasm32-unknown-unknown` is supported. WASI targets are not.

## API keys

There are no environment variables on wasm32, so `Agent::from_config` never
finds a key there. Pass it explicitly, for example from a Worker secret:

```rust,ignore
let key = env.secret("API_KEY")?.to_string();
let agent = Agent::from_config(config).with_api_key(key);
```

The same holds everywhere yoagent would read the environment (see
[API keys](../providers/overview.md#api-keys)):

- **Amazon Bedrock** reads `AWS_*` variables when the key is empty, which
  fails here with an `Auth` error. Pass `access_key:secret[:session_token]`
  (SigV4) or a Bedrock API key with `with_api_key`, and use a regional
  `base_url` (`https://bedrock-runtime.<region>.amazonaws.com`): the signing
  region cannot fall back to `AWS_REGION`.
- **Decision models**: `DecisionModel::jev()` and the OpenCode presets find no
  key; pass one with `.with_api_key(..)`.
- **Prices**: `YOAGENT_PRICES` is never set; install an override with
  `prices::global::install_override(..)`.

## Writing tools and providers for both targets

`AgentTool`, `StreamProvider`, `McpTransport`, `CompactionStrategy`,
`ToolSource`, `SharedStateBackend`, `TurnHook`, `ToolMiddleware`,
`InputFilter`, `AsyncInputFilter` and `DecisionBackend` require
`yoagent::rt::MaybeSend + MaybeSync`. On native targets that is exactly
`Send + Sync`. On wasm32 it is nothing, because the host is single-threaded and
its futures (for example `fetch`) are not `Send`. Use `async_trait`'s
non-`Send` form on wasm32 only:

```rust
use yoagent::{AgentTool, ToolContext, ToolError, ToolResult};

struct Lookup;

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl AgentTool for Lookup {
    fn name(&self) -> &str { "lookup" }
    fn label(&self) -> &str { "Lookup" }
    fn description(&self) -> &str { "Look up a value" }
    fn parameters_schema(&self) -> serde_json::Value { serde_json::json!({"type": "object"}) }
    async fn execute(&self, _params: serde_json::Value, _ctx: ToolContext) -> Result<ToolResult, ToolError> {
        Ok(ToolResult { content: vec![], details: serde_json::Value::Null })
    }
}
```

A custom `StreamProvider` must drop every clone of its event sender before
`stream` returns: the loop waits for that channel to close before it retries
or ends the turn.

## Tasks and timers

There is no Tokio runtime inside a Worker. `yoagent::rt` provides `spawn`,
`sleep`, `timeout`, `JoinHandle` and `Instant` (plus the `JoinError` and
`Elapsed` error types) for both targets: on native
targets they *are* Tokio's, and on wasm32 they use the host executor
(`wasm_bindgen_futures::spawn_local`), `setTimeout` and `performance.now()`.
Use them instead of `tokio::spawn` / `tokio::time` in code that must run on
both. Wall-clock reads go through [`web-time`](https://docs.rs/web-time),
because `std::time::Instant::now()` and `SystemTime::now()` panic on wasm32.

## Cloudflare Workers notes

- With [workers-rs](https://github.com/cloudflare/workers-rs), drive the agent
  from the `fetch` handler: build the `Agent`, call `prompt`, and drain the
  event receiver before responding. The run lives as long as that request, so
  keep it bounded with `ExecutionLimits`. Durable, long-running work belongs
  in a Durable Object.
- Inside a request, the Worker's clocks (`performance.now()`, which `web-time`
  reads, and `Date.now()`) advance only when the Worker does I/O, so
  `ExecutionLimits::max_duration` is coarse there.
- Keep the `target_features` custom section when stripping release builds
  (`strip = "debuginfo"`, not `strip = true`). wasm-bindgen reads it to create
  the externref table that workers-rs panic recovery requires.
