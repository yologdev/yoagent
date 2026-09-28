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
RUSTDOCFLAGS="-Dwarnings" cargo doc --no-deps --all-features   # Doc links — what CI runs

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

Anthropic SSE (`anthropic.rs`): block `index` → position in `content` via a map, never padding, so a block type it does not surface (server tool blocks, `fallback`, anything new) leaves no placeholder and later blocks keep their deltas; unknown block/delta types `warn_once` per type by name, unparseable start/delta events are logged. `redacted_thinking` (`data` arrives whole at `content_block_start`, no delta type) → `Content::thinking_redacted(ApiProtocol::AnthropicMessages, data)` → replayed as `{"type":"redacted_thinking","data"}` in place (#197).

Bedrock is the one non-SSE provider: ConverseStream answers with binary `application/vnd.amazon.eventstream` frames, decoded by the private `provider/eventstream.rs` (bytes buffered across chunks; prelude + message CRC-32 verified with an in-house table, no dependency; all ten header types; checksum/malformed → `Other`, a body ending mid-frame → `Network`; pinned by aws-sdk-go's published test vectors). `bedrock.rs` dispatches on the `:message-type` / `:event-type` / `:exception-type` headers — payloads are the event structs themselves (no wrapper key), shapes per the AWS Bedrock Runtime API reference. Blocks accumulate per `contentBlockIndex`; a tool call is pushed with the `__partial_json` marker and only `contentBlockStop` finalizes it (`finalize_tool_arguments`), so an unclosed block is never runnable. Exceptions classify via their documented HTTP status (`throttlingException` → `RateLimited`; 5xx stay `Api`). No `messageStop` (incl. empty body) or a transport error → `Network`, never an empty `Ok` — except once `messageStop` **and** `metadata` arrived: a later drop/truncation keeps the response (no re-billed retry). `messageStop` without `metadata` → zero usage + `warn!(usage_missing = true)` (same zero as other providers). Unsurfaced content (image/citation/toolResult, server_tool_use, unknown union members via `#[serde(flatten)] other`, unknown event types) is dropped with a once-per-block warning; an unknown `contentBlockStart` member creates no block, so the first delta types it. Reasoning replay: signed → `reasoningText`, `redactedContent` kept in `Content::Thinking::redacted` (base64, bytes concatenated across deltas; `redacted_protocol = BedrockConverseStream`) → `redactedContent`, only if it came from Bedrock (another API's redacted data is skipped, logged at debug); unsigned → `reasoningText {text}` with no signature key, except to a model id containing `claude` (signature-verifying), where it is skipped at debug (never `signature: ""`); the Anthropic provider skips Bedrock's redacted blocks. Signature deltas are appended. Tested only with mock frames (`tests/bedrock_stream_test.rs`). Auth (`resolve_auth`, first match wins): an `authorization` key in `ModelConfig.headers` (any case) → sent alone; non-empty `api_key` → no `:` = Bedrock API key (`Bearer`; a bare `AKIA…`/`ASIA…` id is refused), `access:secret[:token]` = SigV4; empty `api_key` → env at request time, `AWS_BEARER_TOKEN_BEDROCK` then `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`(/`AWS_SESSION_TOKEN`); none → `Auth` before sending. `resolve_api_key("bedrock")` also prefers `AWS_BEARER_TOKEN_BEDROCK`. SigV4 is the private `provider/sigv4.rs` (sha2 + hmac, no AWS SDK; pinned to aws-c-auth's `aws-sig-v4-test-suite` vectors): signing name `bedrock`, region from the host (`bedrock-runtime[-fips].<region>.amazonaws.com[.cn]`, VPC endpoints) > `AWS_REGION` > `AWS_DEFAULT_REGION` > `Auth` error; signs `host`, `content-type`, `x-amz-date`, `x-amz-content-sha256`, `x-amz-security-token` over the exact body bytes sent (serialize once — never `.json()`). The model id is percent-encoded on the wire (`:` → `%3A`, as the SDKs send it) and encoded again in the canonical URI; `tests/bedrock_auth_test.rs` (env-mutating, serialized by a lock) recomputes each signature from what wiremock received. Never run against a live endpoint.

`ModelConfig` + quirk structs handle per-provider differences: `OpenAiCompat` (reasoning format, max_tokens field name, `max_reasoning_effort` ceiling — the ceiling is also read by Responses/Azure), `AnthropicCompat` (adaptive vs budget thinking, bearer auth, `native_structured_output`; `for_claude_id` infers them from an id), `GoogleCompat` (`thinkingLevel` vs `thinkingBudget`). `ThinkingLevel::Off` sends no thinking/effort field on any provider (DeepSeek's explicit `thinking: disabled` aside), so always-thinking models run at their default.

### Key Types

- **`Content`** — enum: `Text`, `Image`, `Thinking` (`thinking`, `signature`, `redacted` + `redacted_protocol: Option<ApiProtocol>` — provider-encrypted reasoning and the API that produced it; replayed only to that protocol (`Content::redacted_for`, crate-private), `None` = pre-field data, sent nowhere; `#[non_exhaustive]`, build with `Content::thinking*`, `thinking_redacted(protocol, data)`), `ToolCall`
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

**Tool middleware** (`ToolMiddleware` in `types.rs`): async approve/deny/modify hooks gating every tool call, run in a chain at the single choke point (`execute_single_tool`) shared by all three strategies. `before_tool` takes a `#[non_exhaustive]` `ToolCallRequest` context struct (extensible without breaking implementors). `Deny(reason)` becomes an error tool result the LLM sees (loop continues); `Modify(args)` rewrites the call; a panicking middleware is contained as a denial. Installed via `Agent::with_tool_middleware` / `SubAgentTool::with_tool_middleware` / `AgentLoopConfig::tool_middleware`. Empty chain = allow all. `ToolCallRequest::new(id, name, &args)` + `with_messages` / `with_run_prompts` build one outside the loop (unit-testing middleware, `ToolGate` included). `user_request()` is built from `user_request_parts()` (`UserRequestParts { latest: String, reply: Option<ReplyContext { question, earlier_request }>, source: UserRequestSource::{Conversation, RunPrompts}, run_prompts: Vec<String> }`, all `#[non_exhaustive]`, also on `TurnContext`); the prose is documented as unstable — policies should read the parts. `SubAgentTool` mirrors `with_turn_hook` / `with_async_input_filter`; a sub-agent task rejected by a filter fails the tool call (detected via the `InputRejected` event, drained from the channel when there is no forwarder).

### OpenAPI Integration (`openapi/`, feature-gated)

Behind the `openapi` Cargo feature. `OpenApiToolAdapter` parses an OpenAPI 3.0 spec and creates one `AgentTool` per operation. Factory methods: `from_str`, `from_file`, `from_url`, `from_spec`. `OperationFilter` controls which operations become tools. Added to `Agent` via `with_openapi_file()` / `with_openapi_url()` / `with_openapi_spec()`.

### MCP Integration (`mcp/`)

`McpClient` communicates via `McpTransport` trait (stdio or HTTP). `McpToolAdapter` wraps MCP tools to implement `AgentTool`, making them transparent to the agent loop. Added via `Agent::with_mcp_server_stdio()` / `with_mcp_server_http()`.

### GASP Bridge (`gasp.rs`, feature-gated)

Behind the `gasp` Cargo feature (dep: `yoagent-state`, the GASP reference runtime). `GaspRecorder` consumes the `AgentEvent` stream (via `recording_sender`, which also tees to a forward sender) and maps it onto `yoagent_state`'s `YoAgentStateSink`: AgentStart→run.started, assistant MessageEnd→model.called/finished pair, ToolExecutionStart/End→tool.called/finished, AgentEnd→run.finished + one git commit per run at stream close (scaffolding committed at init so clones restore). Stale open runs are closed as "interrupted" on open; a dropped sender finishes the run with the derived outcome; InputRejected → "rejected". Recording failures stop recording but the forward tee keeps flowing (error surfaces only via the returned handle — await it); events are forwarded BEFORE recording. `Ok(None)` when no AgentStart arrived. Zero loop changes. CI job `gasp-conformance` emits a repo via `examples/gasp_emit.rs` and runs the gasp conformance checker (7 checks) against it.

### Decision Models (`decision/`, feature-gated)

Behind the `decision` Cargo feature (no deps; off by default; nothing is sent until a `DecisionModel` is constructed and used — an env key alone enables nothing, proven by `tests/decision_env_test.rs` against a wiremock `expect(0)`). Typed `Question`s (`Noul`/`Choice`/`Score`; "Noul" is SystemOne wire vocabulary) about a JSON/text `state` → `Evaluation` (private fields; getters `model()`, `usage()`, `cost_usd()`, `answers()` in request order; `new`/`with_answer`/`with_cost_usd` for backends); answers have private fields + getters (`NoulAnswer::p_true()`, `ChoiceAnswer` in option order, `ScoreAnswer`). `DecisionModel` (cheap clone) wraps a `DecisionBackend` trait (`capabilities()`, `evaluate(&Request)`): `SystemOneBackend` (HTTP `POST /v1/systemone`; lenient parsing; 429/529 retried via `RetryConfig`, `retry-after` wins) and `MockBackend`. Presets `jev()` (`TYPESAFE_API_KEY`/`TYPESAFE_BASE_URL`, read at call time), `jev_opencode()`, `jev_opencode_free()` (`OPENCODE_API_KEY`), `local(url)` (sets `Capabilities::local` = self-hosted), `logprobs(url, model)` (`LogprobBackend`, see below), `from_backend` / `from_arc`. Requests validated against `Capabilities` (2–255 options, 2–10 levels, 64k/32k token estimates); **answers validated centrally in `check_complete` for every backend** (kind, finite probs/confidence in [0,1], a probability for every Choice option / Score level, sums within `max(0.02, n × 0.005)` of 1 for n options/levels (TypeSafe rounds to 2 dp), choice ∈ options, score in range) → `BadResponse`; answers nobody asked for are dropped and the rest reordered to request order (so a non-batching merge cannot be overwritten). `DecisionError` is `Clone` without `PartialEq`; `Backend`/`Transport` are struct variants with an optional `Arc<dyn Error>` source. Missing confidence = `(n*p_max-1)/(n-1)`. Pricing: `jev()` uses `prices.json` `typesafe/<reported id>` only when the base URL is TypeSafe's host; gateways and `from_backend` unpriced; `local()` $0; an unpriced handle keeps a backend-reported cost; a response without usage is unpriced under a non-zero price, while a free handle (`local()`, all-zero rates) stays $0.

Agent integration (`decision::wire` from `Agent::build_config` / `SubAgentTool`): `with_decision_model` → advisory only (crate-private `Advisor` `TurnHook`: one batched request per user request, memoized; skill hint via Choice+Noul at need ≥0.3 / confidence ≥0.5, tool hint at ≥40 tools; 2 s; failures warn and add nothing; does nothing without skills and with <40 tools). `with_tool_gate(ToolGate)` → `ToolMiddleware` appended last; Nouls destructive/requested, deny when destructive ≥0.5 and requested <0.7; state `{user_request, tool_call}` with long strings head+tail-truncated and >12k chars denied; every comparison denies on NaN/missing; threshold setters and check-id collisions panic; **fails closed**. Not a security boundary. Spend: every `evaluate_request` inside a loop records into the `tokio::task_local!` `LoopScope` (`agent_loop::record_decision`), opened in `agent_loop_with_stats` **before the input filters** (a rejected run's `AgentEnd` carries the spend) and in `agent_loop_continue_with_stats` → `SessionStats::decision` (`DecisionStats`, with `unpriced` counting evaluations of unknown cost), child runs folded in (not into `sub_agents`); counted in `total_cost_usd`; a non-batching request that fails or times out partway records the usage already billed (accumulated outside the timed-out future).

Phase 3–4 (`decision/logprobs.rs`, `calibrate.rs`, `guard.rs`; fallback and the split path in `mod.rs`). **`LogprobBackend`** (`DecisionModel::logprobs(url, model)`, or `from_logprob_backend(backend, model)` to keep the logprobs slot — `with_api_key`/`with_retry`/loopback $0 — with custom settings; there is no `DecisionModel::with_temperature`): any OpenAI-compatible `/chat/completions` with logprobs; one request per question (`max_tokens:1`, `temperature:0`, `logprobs:true`, `top_logprobs:max(K=20, labels)`, `extra_body` deep-merged (nested objects key by key) and inserted under the core fields; `with_thinking_disabled` = `chat_template_kwargs.enable_thinking=false`, not default because OpenAI rejects unknown fields), single user message; labels A/B (Noul, A = yes), A.. (Choice, ≤26), 0–9 (Score); `KINDS` lists the three kinds explicitly and every match is exhaustive (a future kind → `Unsupported`). `label_distribution`: trim + upper-case, sum duplicates; no label or label mass < `min_label_mass` (0.5) → `BadResponse` (a `<think>` first token or a word answer is noise, not a confident answer — the gate/guard then fail closed); an absent label gets `min(smallest reported p, 1 − Σreported)` clamped to `[1e-9, 1]`; temperature on ln(mass), softmax, then an absent label is re-floored at 1e-9 after scaling (a tiny temperature would underflow it), so absence never gives 0/1. Reports `batching:false` + `Capabilities::max_concurrent_requests = 8` (new field, default 1): `DecisionModel::evaluate_singles` splits the request into boxed futures built in a loop (a closure trips the `Send` HRTB check), runs them via `FuturesUnordered` up to the limit, stops starting new ones after the first failure but awaits those in flight, adds each completed one to `billed` (so errors and timeouts record the spend of completed questions), returns the earliest failed question's error. `local` only for a loopback host (`is_loopback_url`: localhost/`*.localhost`/127/8/::1; `0.0.0.0` is *not*, which the unix-only remote-pricing wiremock test relies on); loopback → `Pricing::Fixed(0)`, else unpriced. HTTP shared with SystemOne via `systemone::{post_json, with_retries, status_error}` (429/529/transport retried; other 5xx is `Http`; a reqwest `is_builder()` error — malformed URL — is `Invalid`). **Fallback** `DecisionModel::or`: `fallbacks: Vec<DecisionModel>` (flattened; each member sends its own model id), `evaluate_chain` — structural validation against `Capabilities::unlimited()` first (→ `Invalid`, no member tried), then per member: capability-invalid → skipped unsent (`FallbackAttempt { was_sent: false }`, not recorded); any member error, `Invalid` included, falls through; one `tokio::time::Instant` deadline from the head's `timeout` (members' `timeout` ignored) judged by the clock after each failure (a backend's own `Timeout` does not end the chain), per-attempt limit `min(remaining, attempt_timeout)`; deadline passed → `Timeout(head timeout)`; exhaustion → `DecisionError::AllFailed { attempts: Vec<FallbackAttempt> }` (getters `model`/`error`/`was_sent`; `is_retryable` if any member's error was). Each sent attempt is recorded (`record_attempt`); nothing sent → one failure. `model()`/`capabilities()`/builders are the primary's. **`calibrate` / `calibrate_with`** (`CalibrationOptions` private fields + getters): one request per example, `buffer_unordered(concurrency)`; top-label ECE (10 bins), Brier binary for Noul / multi-class otherwise, Noul F1 and target-precision thresholds over observed `p_true`, temperature by NLL grid `1.05^k, k∈[-33,33]` (ties → closest to 1) where only p>0 entries are rescaled and only the true outcome's rescaled p is floored; `accuracy`/`brier`/`ece` are `Option` (None when nothing evaluated); `models` counts per `Evaluation::model` + `mixed_models()`; `errors: Vec<CalibrationError>`. **`InputGuard`** (`Agent`/`SubAgentTool::with_input_guard` = `assert_has_checks()` (panics on no checks) + `with_async_input_filter`): built-ins `injection` / `harmful` at 0.8, stored as a `defaults` flag + `custom` list + `thresholds` overrides, resolved lazily in `checks()` (a custom check with a built-in id replaces it in place and survives `without_default_checks`, whatever the call order); state `{input}` with the **whole** text (no truncation); over `max_input_chars` (32k) → not sent, failure policy; 3 s, fail-closed (`with_fail_open` opt-out); empty text → pass; no checks used directly → Reject; steering/follow-ups are not filtered (loop unchanged). Tests: `decision_logprobs_test` (wiremock, incl. split-request spend via an input guard), `decision_fallback_test`, `decision_calibrate_test`, `decision_guard_test` (MockBackend).

The generic hooks it rides on need no feature: `ToolCallRequest::messages` / `run_prompts` + `latest_user_text()` / `user_request()` (shared helper `types::user_request_of`: never looks back past a compaction boundary (`COMPACTION_MARKER`, `SUMMARY_MARKER`, `[Summary]`) nor past the latest real user message even when it has no text (image-only) — that message's text, else the run's prompts (kept in the loop's task-local `LoopScope`, fed from the run's prompts and injected steering/follow-ups, so compaction cannot remove them), else `None` → the gate denies; skips loop-injected user messages via the public predicate `is_loop_injected` (crate root), built from `context::{SUMMARY_PREFIX, COMPACTION_MARKER}` and `agent_loop::LOOP_NUDGE_PREFIX` (crate-private) plus the public `llm_compaction::SUMMARY_MARKER`, `agent_loop::{AGENT_STOPPED_PREFIX, LOOP_ABORT_PREFIX}`; includes the previous assistant text + earlier request only when the reply is <40 chars AND the assistant text ends with a question outside code), `AsyncInputFilter` (provided `InputFilter::as_async` + `AsyncFilter::new`; panics contained → Reject), and `TurnHook` applied by wrapping the provider in `TurnHookProvider`, which appends notes to the outgoing request's **latest user turn** (system prompt and history prefix untouched; never stored). `AgentLoopConfig` unchanged.

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

**Mutation testing** (`cargo-mutants`, #163) runs weekly, not per-PR, via `.github/workflows/mutants.yml` (16 round-robin shards with `CARGO_INCREMENTAL=1` on the mutants step (rust-cache exports 0, which made every mutant a from-scratch rebuild), pinned version in the workflow's `CARGO_MUTANTS_VERSION`; survivors are a report on the run summary, never a red X; a failing unmutated baseline is). Scope, features, the `mutants` profile (`Cargo.toml`: opt-level 1, no debuginfo) and the documented acceptable exclusions live in `.cargo/mutants.toml`. `docs/evals/mutation-baseline.md` holds the baseline and the triaged survivors — check a missed mutant against it before treating it as news, and add newly-accepted equivalents there (or to `exclude_re` with a comment saying why). Locally: never export `CARGO_TARGET_DIR` for it; each `-j` job does its own cold build (~2–3 GB, 12+ min) in `$TMPDIR`; `-f` does not narrow a run when the config sets `examine_globs` (copy the config and change the globs, or use `--shard k/n --sharding round-robin` for a sample).

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
