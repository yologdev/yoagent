# Architecture Overview

## Layered Design

yoagent is organized as three conceptual layers within a single crate. Dependencies flow strictly downward — upper layers use lower layers, never the reverse.

```
┌─────────────────────────────────────────────┐
│  Layer 3: Orchestration   (partly planned)   │
│  Delegation today; work modes planned        │
├─────────────────────────────────────────────┤
│  Layer 2: Agent + Providers                  │
│  Concrete providers, tools, retry, caching,  │
│  context management, MCP                     │
├─────────────────────────────────────────────┤
│  Layer 1: Core Loop                          │
│  agent_loop, types, traits                   │
│  Provider-agnostic. Tool-agnostic.           │
└─────────────────────────────────────────────┘
```

### Layer 1: Core Loop

The pure agent loop. No opinions about LLMs, no built-in tools. Just the control flow.

**Modules:** `types.rs`, `agent_loop.rs`, `provider/traits.rs`

**Owns:**
- `agent_loop()` / `agent_loop_continue()` — the loop itself
- `AgentTool` trait — interface tools must implement
- `StreamProvider` trait — interface providers must implement
- `AgentMessage`, `AgentEvent`, `StreamDelta` — message & event types
- `AgentContext` — system prompt + messages + tools
- Tool execution strategies (parallel/sequential/batched)
- Streaming tool output (`ToolUpdateFn`)
- Steering & follow-up message injection

**Does not own:** Any concrete provider or tool implementation.

### Layer 2: Agent + Providers

Batteries-included single-agent layer. Most users interact with this.

**Modules:** `agent.rs`, `context.rs`, `retry.rs`, `provider/*.rs`, `tools/*.rs`, `mcp/*.rs`

**Adds on top of Layer 1:**
- Concrete providers — Anthropic, OpenAI-compat, OpenAI Responses, Azure OpenAI, Google Gemini, Vertex, Bedrock (7 protocols)
- Provider registry — dispatch by API protocol
- Prompt caching — automatic cache breakpoint placement
- Retry with backoff — exponential, jitter, respects retry-after
- Context management — token estimation, smart truncation, execution limits
- Built-in tools — bash, read_file, write_file, edit_file, list_files, search (feature `native`)
- MCP client — stdio (feature `native`) + HTTP transports, tool adapter
- `ToolSource` — per-run tool sources, consulted at the start of every run
- `rt` — runtime shims (Tokio on native targets, the host executor on wasm32)
- `Agent` struct — stateful builder wrapping it all together

### Layer 3: Orchestration (partly planned)

Delegation exists today as `SubAgentTool` (a tool that runs a child loop with its own model, tools and limits; nestable) with `SharedState` for passing artifacts by reference. A higher-level `Orchestrator` with work modes is not implemented.

**Planned capabilities:**
- `Orchestrator` struct — spawn, delegate, and coordinate multiple agents
- Work modes:
  - **Interactive** — multi-turn, human in the loop (current default)
  - **Autonomous** — runs to completion without input (background tasks, CI)
  - **Pipeline** — input → output, chainable (scan → fix → verify)
  - **Supervisor** — delegates to other agents, synthesizes results
- Fan-out — same task to multiple agents (different providers for diversity)
- Pipeline chaining — output of agent A feeds input of agent B
- Agent communication through the orchestrator event bus

**Why not yet:** Multi-agent orchestration adds complexity. The single-agent loop handles 95% of use cases. Layer 3 will be built when a concrete use case drives it, not speculatively.

---

## Module Layout

```
yoagent/
├── src/
│   ├── lib.rs                  # Public re-exports
│   │
│   │── Layer 1: Core Loop ─────────────────────
│   ├── types.rs                # Message, Content, AgentTool, AgentEvent
│   ├── agent_loop.rs           # Core loop: prompt → LLM → tools → repeat
│   │
│   ├── rt.rs                   # spawn/sleep/timeout/Instant, MaybeSend/MaybeSync
│   ├── tool_source.rs          # ToolSource (tools resolved per run)
│   │
│   │── Layer 2: Agent + Providers ─────────────
│   ├── agent.rs                # Agent struct (stateful wrapper)
│   ├── context.rs              # Token estimation, compaction, limits
│   ├── llm_compaction.rs       # LlmCompaction (background LLM summaries)
│   ├── retry.rs                # Retry with exponential backoff
│   ├── sub_agent.rs            # SubAgentTool (delegation to a child loop)
│   ├── shared_state.rs         # SharedState + backends
│   ├── session.rs              # Session trees, JSONL persistence
│   ├── skills.rs               # AgentSkills SKILL.md loading
│   ├── gasp.rs                 # GASP run recording (feature gasp)
│   ├── provider/
│   │   ├── traits.rs           # StreamProvider trait, StreamEvent, ProviderError
│   │   ├── model.rs            # ModelConfig, ApiProtocol, OpenAiCompat
│   │   ├── registry.rs         # ProviderRegistry (protocol → provider)
│   │   ├── anthropic.rs        # Anthropic Messages API
│   │   ├── openai_compat.rs    # OpenAI Chat Completions (15+ providers)
│   │   ├── openai_responses.rs # OpenAI Responses API
│   │   ├── azure_openai.rs     # Azure OpenAI
│   │   ├── responses_stream.rs # SSE parser shared by Responses and Azure
│   │   ├── google.rs           # Google Generative AI
│   │   ├── google_vertex.rs    # Google Vertex AI
│   │   ├── bedrock.rs          # AWS Bedrock ConverseStream
│   │   ├── eventstream.rs      # AWS binary eventstream decoder
│   │   ├── sigv4.rs            # SigV4 request signing
│   │   ├── turn_hook.rs        # TurnHookProvider
│   │   ├── tool_args.rs        # Tool-argument parsing
│   │   ├── prices.rs           # PriceTable (+ prices/global.rs, prices/fetch.rs)
│   │   ├── prices.json         # Built-in price data
│   │   ├── mock.rs             # Mock provider for testing
│   │   └── sse.rs              # SSE utilities
│   ├── tools/
│   │   ├── bash.rs             # BashTool (native)
│   │   ├── file.rs             # ReadFileTool, WriteFileTool (native)
│   │   ├── edit.rs             # EditFileTool (native)
│   │   ├── list.rs             # ListFilesTool (native)
│   │   ├── search.rs           # SearchTool (native)
│   │   ├── sandbox.rs          # PathSandbox
│   │   └── shared_state_tool.rs # SharedStateTool
│   ├── mcp/
│   │   ├── client.rs           # MCP client (stdio on native, HTTP everywhere)
│   │   ├── tool_adapter.rs     # McpToolAdapter (MCP tool → AgentTool)
│   │   ├── transport.rs        # Transport implementations
│   │   └── types.rs            # MCP protocol types
│   ├── openapi/                # OpenAPI 3.0 → tools (feature openapi)
│   └── decision/               # Decision models (feature decision)
```

## Data Flow

```
                    ┌─────────────┐
                    │   Caller    │
                    └──────┬──────┘
                           │ prompt / prompt_messages
                    ┌──────▼──────┐
                    │    Agent    │  Layer 2: stateful wrapper
                    │  (agent.rs) │  Manages queues, tools, state
                    └──────┬──────┘
                           │
                    ┌──────▼──────┐
                    │ agent_loop  │  Layer 1: core loop
                    │             │  Prompt → LLM → Tools → Repeat
                    └──┬───────┬──┘
                       │       │
              ┌────────▼──┐ ┌──▼────────┐
              │ Provider  │ │   Tools   │  Layer 2: implementations
              │ .stream() │ │ .execute()│
              └────────┬──┘ └──┬────────┘
                       │       │
              ┌────────▼──┐ ┌──▼────────┐
              │ LLM API   │ │ OS / FS   │
              │ (HTTP)    │ │ (shell)   │
              └───────────┘ └───────────┘

Events flow back via mpsc::UnboundedSender<AgentEvent>
```

## How Providers Plug In

1. Implement `StreamProvider` trait (Layer 1 interface)
2. Register with `ProviderRegistry` under an `ApiProtocol` (Layer 2)
3. Set `ModelConfig.api` to match that protocol
4. The registry dispatches `stream()` calls to the right provider

Each provider translates between yoagent's `Message`/`Content` types and the provider's native API format. All providers emit `StreamEvent`s through the channel for real-time updates.

## How Tools Plug In

1. Implement `AgentTool` trait (Layer 1 interface)
2. Add to the tools vec (via `default_tools()` or custom)
3. The agent loop converts tools to `ToolDefinition` (name, description, schema) for the LLM
4. When the LLM returns `Content::ToolCall`, the loop finds the matching tool and calls `execute()`
5. Results are wrapped in `Message::ToolResult` and added to context

Tools receive a `CancellationToken` child token — they should check it for cooperative cancellation during long operations.

## Design Principles

- **Layers are conceptual, not physical.** One crate, clean module boundaries. Feature flags only gate host capabilities (`native`) and optional integrations (`openapi`, `gasp`, `decision`).
- **Dependencies flow down.** Layer 1 never imports from Layer 2. Layer 2 never imports from Layer 3.
- **Layer 1 is stable.** The core loop and traits change rarely. New features are added in Layer 2 or 3.
- **Build what's needed.** Layer 3's `Orchestrator` is designed but not implemented. It will be built when a use case demands it, not speculatively.
- **Simple over clever.** A straightforward loop with good defaults beats an elegant abstraction nobody can debug.
