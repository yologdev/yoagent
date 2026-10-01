# WebAssembly & Cloudflare Workers

yoagent builds for `wasm32-unknown-unknown`, so the agent loop can run inside a
JavaScript host such as a Cloudflare Worker. Turn off default features, which
removes what needs an operating system:

```toml
[dependencies]
yoagent = { version = "0.22", default-features = false }
```

```bash
rustup target add wasm32-unknown-unknown
cargo build --target wasm32-unknown-unknown
```

## What is available

| On wasm32 | Not on wasm32 (needs the `native` feature) |
|-----------|--------------------------------------------|
| The agent loop, `Agent`, retries, execution limits | `BashTool`, `ReadFileTool`, `WriteFileTool`, `EditFileTool`, `ListFilesTool`, `SearchTool`, `default_tools()` |
| All providers, over the host's `fetch` | `StdioTransport` / `with_mcp_server_stdio` |
| `AgentTool`s you write; HTTP MCP (`with_mcp_server_http`) | `FileBackend` for `SharedState` |
| `SharedState` (in memory), sub-agents, skills from bytes | `PriceTable::fetch_cached` (its disk cache) |
| Compaction — `LlmCompaction` uses its deterministic tiers | SOCKS proxies; reqwest's default TLS stack |

The `native` feature is on by default, so native users see no difference.

## Writing tools and providers for both targets

`AgentTool`, `StreamProvider`, `McpTransport` and `CompactionStrategy` require
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

## Tasks and timers

There is no Tokio runtime inside a Worker. `yoagent::rt` provides `spawn`,
`sleep`, `timeout` and `JoinHandle` for both targets: on native targets they
*are* Tokio's, and on wasm32 they use the host executor
(`wasm_bindgen_futures::spawn_local`) and `setTimeout`. Use them instead of
`tokio::spawn` / `tokio::time` in code that must run on both. Wall-clock reads
go through [`web-time`](https://docs.rs/web-time), because
`std::time::Instant::now()` panics on wasm32.

## Cloudflare Workers notes

- With [workers-rs](https://github.com/cloudflare/workers-rs), drive the agent
  from the `fetch` handler: build the `Agent`, call `prompt`, and drain the
  event receiver before responding. The run lives as long as that request, so
  keep it bounded with `ExecutionLimits`. Durable, long-running work belongs
  in a Durable Object.
- Inside a request, `Date.now()` advances only when the Worker does I/O, so
  `ExecutionLimits::max_duration` is coarse there.
- Keep the `target_features` custom section when stripping release builds
  (`strip = "debuginfo"`, not `strip = true`). wasm-bindgen reads it to create
  the externref table that workers-rs panic recovery requires.
