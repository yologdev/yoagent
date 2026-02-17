# yoagent: What We Learned From pi-agent-core — and Where We Went Further

*February 17, 2026*

Today we released [yoagent v0.1.0](https://crates.io/crates/yoagent) — an agent loop library in Rust, inspired by Mario Zechner's excellent [pi-agent-core](https://github.com/badlogic/pi-mono/tree/main/packages/agent). This post explains what we share, where we diverge, and why we think Rust changes the game for agent infrastructure.

## The Shared Philosophy

Both libraries agree on the fundamentals:

- **The loop is the product.** No planning layers, no RAG, no multi-agent orchestration. Just: prompt → LLM → tools → repeat.
- **Everything is observable.** Both emit fine-grained events: `AgentStart`, `TurnStart`, `MessageUpdate`, `ToolExecutionStart/End`, `AgentEnd`.
- **Custom messages.** Both support app-specific message types that don't pollute the LLM context (pi-agent-core uses declaration merging, yoagent uses `AgentMessage::Extension`).
- **Steering & follow-up.** Both let you interrupt a running agent or queue work for after it finishes.
- **Streaming tool output.** Both pass an `on_update` callback into tool execution for progress reporting.

If you've used pi-agent-core, yoagent will feel familiar. That's intentional.

## What yoagent Adds

### 1. Parallel Tool Execution

pi-agent-core executes tool calls **sequentially**, checking steering between each:

```typescript
// pi-agent-core: one tool at a time
for (const toolCall of toolCalls) {
    result = await tool.execute(...);
    // check steering
}
```

yoagent gives you three strategies:

| Strategy | Behavior |
|----------|----------|
| `Sequential` | Same as pi-agent-core — one at a time |
| **`Parallel`** (default) | All tools run concurrently via `futures::join_all` |
| `Batched { size: N }` | Groups of N with steering checks between batches |

When the LLM says "read file A, read file B, run a search" — that's three independent operations. Running them in parallel cuts latency from 150ms to ~50ms. This is the default in yoagent because most tool calls are independent.

### 2. Multi-Provider Architecture (Built-in)

pi-agent-core delegates provider support to its sibling package `@mariozechner/pi-ai`. The agent itself is provider-agnostic — which is clean, but means you need a separate dependency for any LLM access.

yoagent ships with **7 API protocols and 20+ providers** built in:

| Protocol | Providers |
|----------|-----------|
| Anthropic Messages | Claude |
| OpenAI Chat Completions | OpenAI, xAI, Groq, Cerebras, OpenRouter, Mistral, DeepSeek, ... |
| OpenAI Responses | OpenAI (newer API) |
| Azure OpenAI | Azure |
| Google Generative AI | Gemini |
| Google Vertex | Vertex AI |
| Bedrock ConverseStream | Amazon Bedrock |

One crate, zero extra dependencies for LLM access. The OpenAI-compatible implementation uses per-provider quirk flags to handle differences in auth, reasoning format, and tool handling — so adding a new compatible provider is just a `ModelConfig` with a base URL.

### 3. Automatic Retry with Backoff

pi-agent-core has a `maxRetryDelayMs` option, but the retry logic lives in the provider layer (`pi-ai`), not the agent loop.

yoagent has retry built into the loop itself:

```rust
RetryConfig {
    max_retries: 3,
    initial_delay_ms: 1000,
    backoff_multiplier: 2.0,
    max_delay_ms: 30_000,
}
```

- Retries rate limits (429) and network errors automatically
- Respects `retry-after` headers from the provider
- ±20% jitter to avoid thundering herd
- `RetryConfig::none()` to disable
- Cancellation respected during retry waits

This is enabled by default. You don't configure it, and it just works.

### 4. Built-in Tools

pi-agent-core is **intentionally tool-free** — it's a pure loop library. Tools are your problem.

yoagent ships with 6 production-ready tools:

- **`bash`** — Shell execution with timeout, output truncation, command deny patterns (blocks `rm -rf /`, fork bombs), and optional confirmation callbacks
- **`read_file`** — File reading with line numbers, byte-range support, path restrictions
- **`write_file`** — File writing with auto-mkdir, path restrictions
- **`edit_file`** — Surgical search/replace with fuzzy match error hints (like Claude Code's edit tool)
- **`list_files`** — Directory exploration via `find`
- **`search`** — Pattern search via ripgrep/grep with context lines

These are the same tools that power real coding agents. You can use them as-is, extend them, or ignore them entirely and bring your own.

### 5. Context Management

pi-agent-core has `transformContext` — a hook where you can plug in your own context management.

yoagent also has `transformContext`, but additionally ships with built-in context management:

- **Token estimation** for messages
- **Smart truncation** — keep first + last messages, drop middle
- **Execution limits** — max turns (50), max tokens (1M), max duration (10min), all configurable
- Prevents runaway loops and context window overflow without any configuration

### 6. Prompt Caching (Anthropic)

yoagent has built-in support for Anthropic's prompt caching:

- Automatic cache breakpoint placement (system prompt, tool definitions, conversation prefix)
- ~4-5x cost savings on long conversations
- Zero configuration — enabled by default
- `Usage::cache_hit_rate()` helper for monitoring

pi-agent-core leaves caching to the provider layer.

### 7. MCP Client Support

yoagent includes an MCP (Model Context Protocol) client:

- stdio and HTTP transports
- `McpToolAdapter` wraps any MCP tool as a native `AgentTool`
- One-line integration: `agent.with_mcp_server_stdio("npx", &["-y", "server-name"])`

### 8. Rust: Performance and Safety

This isn't a feature — it's a foundation. pi-agent-core is TypeScript. yoagent is Rust.

**What this means in practice:**

- **Single binary deployment.** No Node.js, no npm, no `node_modules`. Compile and ship.
- **Memory safety.** The `CancellationToken` pattern prevents use-after-cancel bugs that are easy to hit with AbortController in async JS.
- **True parallelism.** `futures::join_all` runs tool calls on actual threads, not JavaScript's cooperative concurrency.
- **Type-driven correctness.** `ProviderError::is_retryable()`, `ToolExecutionStrategy`, `CacheConfig` — these are enums, not strings. The compiler catches mistakes.
- **Predictable performance.** No garbage collector pauses. Important when you're streaming deltas at low latency.

## What pi-agent-core Does Better

Credit where it's due:

- **Maturity.** pi-agent-core is at v0.52 with battle-tested production usage (it powers Claude Code's agent infrastructure). yoagent is v0.1.
- **Declaration merging for custom messages.** TypeScript's structural typing makes custom message types more ergonomic than Rust's enum approach.
- **Streaming architecture.** pi-agent-core uses `for await (const event of response)` which is more natural for progressive streaming. yoagent collects all events after the provider call completes (we fixed the delta bug, but the architecture differs).
- **Dynamic API key resolution.** `getApiKey` per-call is elegant for rotating OAuth tokens (e.g., GitHub Copilot). yoagent has a static API key per agent.
- **Ecosystem.** pi-agent-core lives in a monorepo with `pi-ai` (the provider layer), proxy servers, and tools. It's part of a larger system.

## Side-by-Side

| Feature | pi-agent-core | yoagent |
|---------|--------------|---------|
| Language | TypeScript | Rust |
| Version | 0.52 | 0.1 |
| Core loop | ✅ | ✅ (ported) |
| Events stream | ✅ | ✅ |
| Steering/follow-up | ✅ | ✅ |
| Custom messages | ✅ (declaration merging) | ✅ (Extension variant) |
| Streaming tool output | ✅ (on_update callback) | ✅ (on_update callback) |
| Tool execution | Sequential only | **Parallel, Sequential, Batched** |
| Built-in providers | ❌ (separate package) | **7 protocols, 20+ providers** |
| Built-in tools | ❌ | **6 tools (bash, files, search)** |
| Retry with backoff | Provider-level | **Loop-level, automatic** |
| Context management | Hook only | **Built-in token estimation + truncation** |
| Execution limits | ❌ | **Max turns, tokens, duration** |
| Prompt caching | Provider-level | **Built-in (Anthropic)** |
| MCP support | ❌ | **stdio + HTTP** |
| Tests | Vitest | **77 tests + 3 integration** |
| Package size | ~12KB (loop only) | 95KB (batteries included) |
| Deployment | Needs Node.js | Single binary |

## Who Should Use What

**Use pi-agent-core if:**
- You're building in TypeScript/Node.js
- You want maximum flexibility and minimal opinions
- You're already in the pi-mono ecosystem
- You need production-proven stability

**Use yoagent if:**
- You want batteries included — providers, tools, retry, caching out of the box
- You're building in Rust or want single-binary deployment
- Performance matters (parallel tools, no GC)
- You want to get a working coding agent in 200 lines

## Getting Started

```bash
cargo add yoagent
```

```rust
use yoagent::agent::Agent;
use yoagent::provider::AnthropicProvider;
use yoagent::tools::default_tools;

let mut agent = Agent::new(AnthropicProvider)
    .with_system_prompt("You are a coding assistant.")
    .with_model("claude-sonnet-4-20250514")
    .with_api_key(api_key)
    .with_tools(default_tools());

let mut rx = agent.prompt("Fix the failing test in src/main.rs").await;
```

Or run the 210-line interactive CLI: `cargo run --example cli`

---

[GitHub](https://github.com/yologdev/yoagent) · [Docs](https://yologdev.github.io/yoagent/) · [crates.io](https://crates.io/crates/yoagent)

*yoagent is inspired by pi-agent-core. Thanks to [Mario Zechner](https://github.com/badlogic) for the brilliant design that proved a thin loop + good model is all you need.*
