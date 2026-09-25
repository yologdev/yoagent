# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

**yoagent** is a Rust library (crate) for building AI coding agents. It provides a core agent loop, multi-provider LLM streaming, built-in tools, MCP integration, and context management. Published to crates.io as `yoagent`.

## Build & Development Commands

```bash
cargo test --all-features            # Run all tests — what CI runs
cargo clippy --all-targets --all-features   # Lint — what CI runs (-Dwarnings)
cargo fmt                            # Auto-format code
cargo fmt -- --check                 # Check formatting (CI uses this)

cargo test <test_name>               # Run a single test by name
cargo test --test agent_test         # Run a specific test file
cargo run --example cli              # Run the interactive CLI example
cargo run --example basic            # Run the minimal example
```

**Pass `--all-features` when checking your work.** `openapi` and `gasp` are off
by default, and some targets exist only behind them: `gasp_emit` and
`llm_compaction_live` are both `required-features = ["gasp"]`, so plain
`cargo clippy --all-targets` skips them entirely and reports clean on code CI
will reject. This is not hypothetical — a breaking change to `CostConfig`
passed local clippy while `llm_compaction_live` no longer compiled.

CI (`RUSTFLAGS="-Dwarnings"`) treats all clippy warnings as errors. Integration tests in `tests/integration_anthropic.rs` require a live API key and are skipped by default.

## Architecture

### Core Loop Pattern

The central abstraction is a **stateless agent loop** (`agent_loop.rs`) driven by two traits:

- **`StreamProvider`** (`provider/traits.rs`) — streams LLM responses via SSE into an mpsc channel, returning a complete `Message`
- **`AgentTool`** (`types.rs`) — defines tool name/schema/execution; the primary extension point for custom tools

The loop: stream assistant response → extract tool calls → execute tools (parallel by default) → append results → repeat until `StopReason::Stop` with no follow-ups.

`agent_loop` and `agent_loop_continue` are **free functions**, not methods. The `Agent` struct (`agent.rs`) is an optional stateful wrapper that manages message history, tool registry, steering/follow-up queues, and provider selection. The `_with_sender` methods (`prompt_with_sender`, `prompt_messages_with_sender`, `continue_loop_with_sender`) accept a caller-provided `mpsc::UnboundedSender<AgentEvent>` for real-time event consumption on a separate task.

### Provider System

7 provider implementations behind `StreamProvider`. `Agent` holds a concrete provider directly; `ProviderRegistry` maps `ApiProtocol` → provider for registry-based dispatch (e.g. custom multi-protocol routers):

| Protocol | File | Covers |
|----------|------|--------|
| `AnthropicMessages` | `anthropic.rs` | Claude models |
| `OpenAiCompletions` | `openai_compat.rs` | OpenAI, Groq, Together, DeepSeek, Fireworks, Mistral, xAI, etc. (15+) |
| `OpenAiResponses` | `openai_responses.rs` | OpenAI Responses API (GPT-6 presets) |
| `AzureOpenAiResponses` | `azure_openai.rs` | Azure OpenAI v1 Responses (`{resource}/openai/v1/responses`) |
| `GoogleGenerativeAi` | `google.rs` | Gemini |
| `GoogleVertex` | `google_vertex.rs` | Vertex AI |
| `BedrockConverseStream` | `bedrock.rs` | Amazon Bedrock (ConverseStream) |

The Responses and Azure providers share one SSE parser (`responses_stream.rs`).

`ModelConfig` + quirk structs handle per-provider differences: `OpenAiCompat` (reasoning format, max_tokens field name, `max_reasoning_effort` ceiling — the ceiling is also read by Responses/Azure), `AnthropicCompat` (adaptive vs budget thinking, bearer auth, `native_structured_output`; `for_claude_id` infers them from an id), `GoogleCompat` (`thinkingLevel` vs `thinkingBudget`). `ThinkingLevel::Off` sends no thinking/effort field on any provider (DeepSeek's explicit `thinking: disabled` aside), so always-thinking models run at their default.

### Key Types

- **`Content`** — enum: `Text`, `Image`, `Thinking`, `ToolCall`
- **`Message`** — enum: `User`, `Assistant`, `ToolResult` — each variant carries its own fields
- **`AgentMessage`** — `Llm(Message)` | `Extension(ExtensionMessage)` — extension messages (`role`, `kind`, `data`) don't enter LLM context
- **`AgentEvent`** — full event stream emitted to callers: `AgentStart`, `TurnStart`, `MessageStart/Update/End`, `ToolExecutionStart/Update/End`, `ProgressMessage`, `InputRejected`, `TurnEnd`, `AgentEnd`
- **`StopReason`** — `Stop`, `Length`, `ToolUse`, `Error`, `Aborted`, `Refusal`

### Context Management (`context.rs`)

- **`ContextTracker`** — hybrid real-usage + estimation; the loop uses it to calibrate the compaction budget against real provider usage
- **`compact_messages()`** — tiered compaction: Level 1 (truncate tool outputs) → Level 2 (summarize old turns) → Level 3 (drop middle turns)
- **`ExecutionLimits`/`ExecutionTracker`** — max turns (50), max tokens (1M), max duration (10 min)

### Tool Execution (`agent_loop.rs`)

`ToolExecutionStrategy` controls concurrency:
- `Parallel` (default) — `futures::join_all` for all tool calls
- `Sequential` — one at a time, checks steering queue between each
- `Batched { size }` — concurrent within batch, steering check between batches

**Structured outputs** (`Agent::prompt_structured::<T>(text, schema)`): the schema is threaded per-call through `prompt_messages_internal` into `StreamConfig.output_schema` (`OutputSchema` in `provider/traits.rs`) — never stored on the Agent. Errors: `Provider` (API failure, or a refusal / content-filter stop — carries its explanation), `Parse { source, raw }`, `NoOutput`; only this call's messages are scanned. Anthropic uses native JSON outputs (`output_config.format`, no tool, thinking kept, tool choice `auto`) when `AnthropicCompat::native_structured_output` is set — every `claude_*` preset and OpenCode Claude ids from 4.5 set it — and otherwise falls back to tool-forcing (a synthetic tool + `tool_choice`; the loop's `unwrap_structured_tool_call` converts the forced call back to text **before** tool-call extraction, and runs only on the tool-forcing path — `structured_output_is_tool_forced` — so a user tool named like the schema still executes on the native path); OpenAI-compat uses `response_format: json_schema`; Gemini uses `responseSchema`. Responses/Azure/Vertex/Bedrock warn and ignore.

**Tool middleware** (`ToolMiddleware` in `types.rs`): async approve/deny/modify hooks gating every tool call, run in a chain at the single choke point (`execute_single_tool`) shared by all three strategies. `before_tool` takes a `#[non_exhaustive]` `ToolCallRequest` context struct (extensible without breaking implementors). `Deny(reason)` becomes an error tool result the LLM sees (loop continues); `Modify(args)` rewrites the call; a panicking middleware is contained as a denial. Installed via `Agent::with_tool_middleware` / `SubAgentTool::with_tool_middleware` / `AgentLoopConfig::tool_middleware`. Empty chain = allow all.

### OpenAPI Integration (`openapi/`, feature-gated)

Behind the `openapi` Cargo feature. `OpenApiToolAdapter` parses an OpenAPI 3.0 spec and creates one `AgentTool` per operation. Factory methods: `from_str`, `from_file`, `from_url`, `from_spec`. `OperationFilter` controls which operations become tools. Added to `Agent` via `with_openapi_file()` / `with_openapi_url()` / `with_openapi_spec()`.

### MCP Integration (`mcp/`)

`McpClient` communicates via `McpTransport` trait (stdio or HTTP). `McpToolAdapter` wraps MCP tools to implement `AgentTool`, making them transparent to the agent loop. Added via `Agent::with_mcp_server_stdio()` / `with_mcp_server_http()`.

### GASP Bridge (`gasp.rs`, feature-gated)

Behind the `gasp` Cargo feature (dep: `yoagent-state`, the GASP reference runtime). `GaspRecorder` consumes the `AgentEvent` stream (via `recording_sender`, which also tees to a forward sender) and maps it onto `yoagent_state`'s `YoAgentStateSink`: AgentStart→run.started, assistant MessageEnd→model.called/finished pair, ToolExecutionStart/End→tool.called/finished, AgentEnd→run.finished + one git commit per run at stream close (scaffolding committed at init so clones restore). Stale open runs are closed as "interrupted" on open; a dropped sender finishes the run with the derived outcome; InputRejected → "rejected". Recording failures stop recording but the forward tee keeps flowing (error surfaces only via the returned handle — await it); events are forwarded BEFORE recording. `Ok(None)` when no AgentStart arrived. Zero loop changes. CI job `gasp-conformance` emits a repo via `examples/gasp_emit.rs` and runs the gasp conformance checker (7 checks) against it.

### Session Trees (`session.rs`)

`Session` stores history as an id/parent_id tree: `append` advances the head, `seek`/`seek_checkpoint` move it, appending after a seek forks a new branch (never overwrites). `path_messages()` feeds a branch into `Agent::with_messages`; `append_new(agent.messages())` is the post-run sync. JSONL persistence (`to_jsonl`/`from_jsonl`, head = last line). Freestanding — no loop changes; maps to GASP's `transcripts/` tier.

### Shared State (`shared_state.rs`)

`SharedState` is a pluggable key-value store (`Arc<dyn SharedStateBackend>`) for sub-agent communication. It lets a parent store large artifacts once and have multiple sub-agents read/write by reference — no re-pasting into prompts.

- Two built-in backends: `MemoryBackend` (default, `HashMap` with 10MB cap) and `FileBackend` (one file per key, persistent)
- Custom backends implement the `SharedStateBackend` trait
- Opt-in via `SubAgentTool::with_shared_state(state)` — injects a `shared_state` tool and appends a state summary to the sub-agent's system prompt automatically
- Actions: `get`, `set`, `list`, `remove`
- Opt in on a parent via `Agent::with_shared_state(state)`, which also registers
  the `shared_state` tool for that run
- The loop reads it at one point only: `AgentLoopConfig::tool_output_sink`, used
  on the **append path** to stash the full text of a truncated tool result so the
  marker can name a retrievable key. Never on the compaction path — by then the
  middle is already gone, and re-deriving there would move marker bytes that the
  prefix cache depends on

### Construction API

The primary constructor is `Agent::from_config(ModelConfig)`: it selects the built-in provider from `config.api`, sets the model id from `config.id`, and resolves the API key from the provider-conventional env var (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `XAI_API_KEY`, …; see `provider::resolve_api_key`). This removes the provider↔config pairing footgun and the doubly-specified model id.

```rust
// provider auto-selected, key from XAI_API_KEY
let agent = Agent::from_config(ModelConfig::xai("grok-4.7", "Grok 4.7"));
```

Other constructors:
- `Agent::from_provider(provider, config)` — explicit provider (custom impls, test doubles). Pair with `ModelConfig::mock()` in tests.
- `Agent::from_config_with(&registry, config) -> Result<_, AgentBuildError>` — resolve against a custom `ProviderRegistry`.
- `Agent::set_model(config)` — switch model mid-session (re-resolves the env key; re-selects the provider only when it was registry-resolved, never clobbering an explicit one; explicit keys preserved).
- `Agent::new(provider)` + `with_model`/`with_model_config` — the original builder, still supported.

`SubAgentTool` mirrors these: `from_config`, `from_config_with`, `from_provider`.

**Sub-agent spend** is a separate bucket, never merged into the parent's own
figures: `SessionStats::sub_agents` (`SubAgentSpend { usage, cost_usd, runs }`),
summed over the whole delegation tree, each run priced at its own model's rates;
`total_usage()`/`total_cost_usd()` add the two. The child's `SessionStats`
reach the loop through `ToolContext::report_delegated_run` (public, so custom
delegation tools report too; must be called before `execute` returns) into a
private side channel — so a failed delegation, which returns `Err`, still
reports — and are folded in at `execute_single_tool`, which also attaches them
(combined, when one call reported several runs) to the `ToolExecutionEnd`
details (`SessionStats::from_sub_agent_result`). Every cost rollup —
`record_turn`, `SubAgentSpend::merge`, `total_cost_usd` — goes through one
`combine_cost` rule: non-zero usage with no cost is *unpriced* and sticky-`None`;
`None` with zero usage is "nothing spent" (`is_unpriced()` tells them apart).
`Agent` accumulates each run's `SessionStats`: `sub_agent_spend()`,
`total_cost_usd()` and `total_usage()` share that runs-since-construction-or-
`reset()` window, unaffected by clearing/replacing history, whereas
`session_cost_usd()` is history-derived and excludes sub-agents.

`AgentLoopConfig` also supports `turn_delay: Option<Duration>` — an inter-turn delay to throttle API calls for rate-limit-sensitive providers. Exposed on `SubAgentTool` via `with_turn_delay()`.

### Testing

All unit tests use `MockProvider` (`provider/mock.rs`) to simulate LLM responses without network. Test files are in `tests/` — `agent_test.rs`, `agent_loop_test.rs`, `tools_test.rs`. Follow the existing pattern of constructing a `MockProvider` with predetermined responses.

### Telemetry

The loop emits `tracing` spans: `agent_loop` → `llm_stream` per turn (records tokens_in/out/cached + cost_usd from `CostConfig` when configured) → `tool` per execution (records is_error). Futures are instrumented with `.instrument(span)` (never hold an entered guard across `.await`). OTel is app-side via `tracing-opentelemetry` — the library has no OTel dependency by design.

### Pricing (`provider/prices.rs`, `provider/prices/{global,fetch}.rs`, `provider/prices.json`)

Prices are data, not literals: `prices.json` (embedded via `include_str!`, schema 1, keyed provider → model id) is parsed into a validated `PriceTable` (plain data; every entry goes through `PriceTable::insert` → `PriceEntry::validate`). First-party constructors whose provider is in `PRICED_PROVIDERS` (`anthropic`, `openai`/`openai_responses`, `google`, `xai`, `groq`, `deepseek`, `mistral`, `zai`, `minimax`, `qwen`, `meta`) set `cost` via a private `priced()` lookup at construction (it debug-asserts membership); the named presets inherit it through `..Self::anthropic(..)` etc. Gateways/custom endpoints never look up (`None`).

The lookup reads the process-wide resolved table managed by the free functions in `prices::global` (`OnceLock<RwLock<Layers>>`): user layer (`install_override` → `OverrideReport { changes, reverted, inert, warnings }`, or `YOAGENT_PRICES` read once at first use, status via `env_override_status() -> EnvOverride`) > fetched layer (`install_fetched` / `install_fetched_with(table, InstallPolicy::AddOnly)`, returns `Vec<PriceChange>` against the previous built-in+fetched table, `shadowed` marks changes the user layer hides; logs built-in disagreements) > built-in. Each layer replaces whole entries; every install runs under one write lock. Constructors resolve at construction time: a `cost` set on a config wins over every layer, but `ModelConfig::reprice()` (repeats the constructor lookup, may clear; only for configs a pricing constructor built — private `#[serde(skip)] list_priced` marker — whose provider is still in `PRICED_PROVIDERS`; also `Agent::reprice`, `SubAgentTool::reprice`) and `with_prices(&table)` (never clears, applies to gateways) overwrite it.

Live sources (`prices/fetch.rs`), never fetched implicitly: `PriceTable::fetch` / `fetch_with(source, FetchOptions) -> FetchReport { table, skipped, ignored_fields }` from `ModelsDev`/`ModelsDevAt` (mapped; inexpressible models skipped, `alibaba`→`qwen`, `opencode`→`opencode-zen`), `YoagentMain` (lenient), `Url` (strict); `fetch_cached(source, path, CacheOptions) -> CachedPrices` (cache records the source URL; `PriceOrigin` is `Fetched{cache_write_error}` / `Cache{age}` / `StaleCache{age, fetch_error}` / `Builtin{fetch_error}`; `cache_problem` reports unusable caches). Hand-written input is strict (`PriceError::UnknownField`); `YoagentMain` and caches are lenient and return ignored field paths. `PriceError` is `Clone` (I/O and transport errors behind `Arc`).

Tests that mutate the global layers or assert on logs live in their own serialized binaries (`price_override_test`, `price_env_test`, `price_env_bad_test`, `price_env_unset_test`, `price_explicit_cost_test`, `price_fetch_test`); unit tests never read `YOAGENT_PRICES`. `tests/preset_prices_test.rs` pins every preset's full `CostConfig`; `tests/price_audit.rs` diffs every entry against models.dev (`--ignored`), and runs the same comparison offline against `tests/fixtures/models_dev_slice.json`. Change a price by editing the JSON entry (and its `verified`/`source`), never by adding a literal.

## Key Design Conventions

- Context overflow detection is centralized in `OVERFLOW_PHRASES` (`provider/traits.rs`) covering 15+ provider-specific error strings; both HTTP errors and SSE-embedded errors are classified. Rate limits are checked first (HTTP 429; structured SSE `type`/`code`/`status` such as `too_many_requests`, `no_capacity`, `rate_limit_exceeded`) so a capacity error whose text resembles an overflow phrase is retried, not compacted
- Tools return stdout/stderr even on failure so the LLM can self-correct
- Retry logic (`retry.rs`) uses exponential backoff with ±20% jitter; only retries `RateLimited` and `Network` errors
- The `skills.rs` module loads `<name>/SKILL.md` files with YAML frontmatter per the AgentSkills standard
