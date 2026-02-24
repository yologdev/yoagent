# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

**yoagent** is a Rust library (crate) for building AI coding agents. It provides a core agent loop, multi-provider LLM streaming, built-in tools, MCP integration, and context management. Published to crates.io as `yoagent`.

## Build & Development Commands

```bash
cargo build                          # Build the library
cargo test                           # Run all unit tests
cargo test <test_name>               # Run a single test by name
cargo test --test agent_test         # Run a specific test file
cargo fmt                            # Auto-format code
cargo fmt -- --check                 # Check formatting (CI uses this)
cargo clippy --all-targets           # Lint (CI runs with -Dwarnings)
cargo run --example cli              # Run the interactive CLI example
cargo run --example basic            # Run the minimal example
```

CI (`RUSTFLAGS="-Dwarnings"`) treats all clippy warnings as errors. Integration tests in `tests/integration_anthropic.rs` require a live API key and are skipped by default.

## Architecture

### Core Loop Pattern

The central abstraction is a **stateless agent loop** (`agent_loop.rs`) driven by two traits:

- **`StreamProvider`** (`provider/traits.rs`) — streams LLM responses via SSE into an mpsc channel, returning a complete `Message`
- **`AgentTool`** (`types.rs`) — defines tool name/schema/execution; the primary extension point for custom tools

The loop: stream assistant response → extract tool calls → execute tools (parallel by default) → append results → repeat until `StopReason::Stop` with no follow-ups.

`agent_loop` and `agent_loop_continue` are **free functions**, not methods. The `Agent` struct (`agent.rs`) is an optional stateful wrapper that manages message history, tool registry, steering/follow-up queues, and provider selection.

### Provider System

7 provider implementations behind `StreamProvider`, dispatched by `ApiProtocol` enum via `ProviderRegistry`:

| Protocol | File | Covers |
|----------|------|--------|
| `Anthropic` | `anthropic.rs` | Claude models |
| `OpenAiCompat` | `openai_compat.rs` | OpenAI, Groq, Together, DeepSeek, Fireworks, Mistral, xAI, etc. (15+) |
| `OpenAiResponses` | `openai_responses.rs` | OpenAI Responses API |
| `AzureOpenAi` | `azure_openai.rs` | Azure OpenAI |
| `Google` | `google.rs` | Gemini |
| `GoogleVertex` | `google_vertex.rs` | Vertex AI |
| `Bedrock` | `bedrock.rs` | Amazon Bedrock (ConverseStream) |

`ModelConfig` + `OpenAiCompat` flags handle per-provider quirks (auth style, reasoning format, max_tokens field name, etc.).

### Key Types

- **`Content`** — enum: `Text`, `Image`, `Thinking`, `ToolCall`
- **`Message`** — enum: `User`, `Assistant`, `ToolResult` — each variant carries its own fields
- **`AgentMessage`** — `Llm(Message)` | `Extension(ExtensionMessage)` — extension messages (`role`, `kind`, `data`) don't enter LLM context
- **`AgentEvent`** — full event stream emitted to callers: `AgentStart`, `TurnStart`, `MessageStart/Update/End`, `ToolExecutionStart/Update/End`, `ProgressMessage`, `TurnEnd`, `AgentEnd`
- **`StopReason`** — `Stop`, `Length`, `ToolUse`, `Error`, `Aborted`

### Context Management (`context.rs`)

- **`ContextTracker`** — hybrid real-usage + estimation for token tracking
- **`compact_messages()`** — tiered compaction: Level 1 (truncate tool outputs) → Level 2 (summarize old turns) → Level 3 (drop middle turns)
- **`ExecutionLimits`/`ExecutionTracker`** — max turns (50), max tokens (1M), max duration (10 min)

### Tool Execution (`agent_loop.rs`)

`ToolExecutionStrategy` controls concurrency:
- `Parallel` (default) — `futures::join_all` for all tool calls
- `Sequential` — one at a time, checks steering queue between each
- `Batched { size }` — concurrent within batch, steering check between batches

### MCP Integration (`mcp/`)

`McpClient` communicates via `McpTransport` trait (stdio or HTTP). `McpToolAdapter` wraps MCP tools to implement `AgentTool`, making them transparent to the agent loop. Added via `Agent::with_mcp_server_stdio()` / `with_mcp_server_http()`.

### Testing

All unit tests use `MockProvider` (`provider/mock.rs`) to simulate LLM responses without network. Test files are in `tests/` — `agent_test.rs`, `agent_loop_test.rs`, `tools_test.rs`. Follow the existing pattern of constructing a `MockProvider` with predetermined responses.

## Key Design Conventions

- Context overflow detection is centralized in `OVERFLOW_PHRASES` (`provider/traits.rs`) covering 15+ provider-specific error strings; both HTTP errors and SSE-embedded errors are classified
- Tools return stdout/stderr even on failure so the LLM can self-correct
- Retry logic (`retry.rs`) uses exponential backoff with ±20% jitter; only retries `RateLimited` and `Network` errors
- The `skills.rs` module loads `<name>/SKILL.md` files with YAML frontmatter per the AgentSkills standard
