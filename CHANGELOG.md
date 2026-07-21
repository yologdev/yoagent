# Changelog

All notable changes to `yoagent` are documented here. The format loosely
follows [Keep a Changelog](https://keepachangelog.com/), and the project
adheres to [Semantic Versioning](https://semver.org/).

## 0.13.1

### Fixed

- **openai_compat: DONE-less SSE close after `finish_reason` is now a clean
  EOF** (#76). Providers that close the connection without the
  OpenAI-standard `data: [DONE]` terminator (MiniMax confirmed in the field)
  no longer return `ProviderError::Other("Stream ended")` after the complete
  response already streamed. A `StreamEnded` with no `finish_reason` remains
  an error — genuine truncation still surfaces (network-level drops retry;
  deliberate server closes fail fast).

## 0.13.0

### Added

- **Serializable event stream** — `AgentEvent` and `StreamDelta` now derive
  `Serialize`, `Deserialize`, and `PartialEq`, so external frontends
  (websocket fanout servers, TypeScript clients, JSONL pipes) can consume
  the agent's event stream as JSON. The wire format is internally tagged
  camelCase — `{"type":"toolExecutionEnd","toolCallId":...,"isError":false}`
  — and is a **frozen public contract** guarded by snapshot tests: variant
  tags, field names, and the tagging scheme will not change in minor
  releases. Additive only: no variant, field, or signature changes.

### Changed

- **Message payload serialization normalized to camelCase** — the five
  remaining snake_case fields in serialized messages now match the rest of
  the wire format: `usage.cacheRead`/`cacheWrite`/`totalTokens`,
  `errorMessage`, and `providerMetadata`. Session files and `save_messages`
  blobs written by older versions still load (`serde` aliases accept the old
  names). Files **written** by 0.13 load in older versions *without error*,
  but the renamed fields are silently dropped there: cache/total token
  counts read as 0, and `errorMessage`/`providerMetadata` — including
  Gemini thought signatures — are lost. Don't round-trip session files
  through yoagent < 0.13. The full nested payload shape (message, content
  blocks, usage) is frozen by an exact-JSON snapshot test.
- `serde` minimum version is now `1.0.177` (the release that added
  `rename_all_fields`, July 2023); no practical impact.

## 0.12.0

### Added

- **Meta Model API (Muse Spark)** — `ModelConfig::meta("muse-spark-1.1",
  ...)` preset for Meta's OpenAI-compatible endpoint (US-only public preview
  at launch): 1M context, 128K output, launch pricing pre-configured
  ($1.25/$4.25 per M, $0.15/M cached input). `reasoning_effort` is wired
  (`ThinkingLevel` tunes it; note Meta's server default is `medium`). Key
  resolves from `META_API_KEY`, then Meta's documented `MODEL_API_KEY`. Also
  available in the CLI example via `--provider meta`.

- **GASP bridge** (feature `gasp`) — `gasp::GaspRecorder` records agent runs
  into a [GASP](https://github.com/yologdev/gasp) agent repo via
  `yoagent-state`: append-only `state/events.jsonl` (goal/run/model/tool
  events), one git commit per run (scaffolding committed at init so `git
  clone` restores a complete agent), stale/interrupted runs closed safely,
  events teed to your UI **before** recording (a recording failure never
  blinds the UI; the error surfaces via the returned handle). Redaction hook
  via `with_summarizer` — summaries of tool inputs/outputs are persisted to
  a shareable repo. yoagent is now a **tested** GASP-conformant runtime: CI
  emits a repo and runs the protocol's 7-check suite against a **fresh
  clone** (the actual restore operation). New `gasp_emit` example and docs
  page.

## 0.11.0

### Fixed (pre-release review of the items below)

- `prompt_structured` now surfaces provider failures as
  `StructuredPromptError::Provider` (previously laundered into
  `Parse { raw: "" }`), scans only messages produced by the current call
  (never stale history), and threads the schema per-call so a dropped/timed-
  out future can't leave the agent stuck in schema-forcing mode.
- Bedrock replays `Content::Thinking` blocks (with signatures) on subsequent
  requests — previously captured and then dropped, breaking multi-turn
  thinking + tool use with a ValidationException.
- Anthropic structured outputs disable extended thinking for that request
  (forced tool choice + thinking is an API-level conflict) with a warning.
- Vertex now round-trips Gemini thought signatures on function calls (parity
  with the Gemini API provider).
- `Session::append_new` verifies the history still extends the session path
  and returns `HistoryDiverged` instead of silently corrupting the tree (the
  usual cause: context compaction); `from_jsonl` validates ids and parents
  (duplicates/dangling/cycles rejected); `seek_checkpoint` is latest-wins.
- A panicking `ToolMiddleware` is contained as a denial instead of killing
  the loop task (which stripped the agent of its tools).
- Middleware denials are logged (`tracing::warn!`) so operators see them.
- `ToolMiddleware::before_tool` takes a `ToolCallRequest` context struct
  (extensible without breaking implementors); `StreamConfig` and
  `OutputSchema` are `#[non_exhaustive]` with constructors.

### Added

- **Telemetry** — `tracing` spans: `agent_loop` (model), `llm_stream` per
  turn (tokens in/out/cached + `cost_usd` when pricing is configured), and
  `tool` per execution (name, `is_error`). Bridge to OpenTelemetry
  app-side with `tracing-opentelemetry`; zero overhead with no subscriber.
  New `telemetry` example and docs page.

- **Cross-provider thinking (7/7)** — `thinking_level` is now honored by
  every protocol: Gemini and Vertex send `thinkingConfig` (with thought
  summaries streamed back as `Content::Thinking`), Bedrock sends
  Anthropic-style `additionalModelRequestFields.thinking` (reasoning deltas
  and signatures streamed back), Azure sends Responses-style reasoning
  effort. The "not yet wired" warnings are gone.

- **Session trees** — `Session`: branching conversation history with
  `append`/`seek`/`checkpoint`, fork-preserving edits, `path_messages()` for
  branch resume, and JSONL persistence. The pi-style id/parent_id tree; maps
  to GASP's `transcripts/` tier.

- **Structured outputs** — `Agent::prompt_structured::<T>(text, schema)`
  returns a typed, schema-validated reply. Enforcement is native per
  provider: Anthropic (forced tool call, unwrapped by the loop),
  OpenAI-compatible (`response_format: json_schema, strict`), Gemini
  (`responseSchema`). Providers without support log a warning. New
  `OutputSchema` type on `StreamConfig`/`AgentLoopConfig`; new
  `StructuredPromptError` with the raw text preserved on parse failure.

- **Tool middleware (permissions)** — `ToolMiddleware`, an async
  approve/deny/modify hook gating every tool call, installed via
  `Agent::with_tool_middleware` / `SubAgentTool::with_tool_middleware` /
  `AgentLoopConfig::tool_middleware`. `Deny(reason)` becomes an error tool
  result the LLM can adapt to (the loop continues); `Modify(args)` rewrites
  the call. Empty chain = allow all (no behavior change).

## 0.10.0

The headline change is a **config-first construction API**. You now build an
agent from a single `ModelConfig` — the provider, model id, context window, and
pricing all come from one place, and the API key is resolved from the
provider-conventional environment variable.

### Added

- `Agent::from_config(ModelConfig)` — the new primary constructor. Selects the
  built-in provider for `config.api` and resolves the API key from the
  provider-conventional env var (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`,
  `XAI_API_KEY`, …; see `provider::resolve_api_key`).
- `Agent::from_provider(provider, ModelConfig)` — explicit provider (custom
  `StreamProvider` impls and test doubles). Pair with `ModelConfig::mock()`.
- `Agent::from_config_with(&registry, ModelConfig) -> Result<Agent, AgentBuildError>`
  — resolve the provider from a caller-supplied `ProviderRegistry`.
- `Agent::set_model(ModelConfig)` — switch model mid-session. Re-resolves the
  env key; re-selects the provider only when it was registry-resolved (an
  explicitly-supplied provider is never silently replaced).
- `SubAgentTool::from_config`, `from_config_with`, and `from_provider` mirror
  the above.
- `ModelConfig::mock()` — a throwaway config for tests (use only with
  `from_provider`).
- `AgentBuildError` (exported) — the error type for the fallible
  `from_config_with` path.
- `ProviderRegistry::resolve(&ApiProtocol) -> Option<Arc<dyn StreamProvider>>`
  and `StreamProvider::protocol() -> Option<ApiProtocol>`.
- Automatic env-var API-key resolution and a `with_temperature()` builder
  (from the 0.9.x adoption-funnel work, now the default construction path).

### Deprecated

The following are deprecated since 0.10.0 and will be **removed in 1.0**. They
still work; you'll get a compiler warning pointing at the replacement:

- `Agent::new`, `Agent::with_model`, `Agent::with_model_config`
- `SubAgentTool::new`, `SubAgentTool::with_model`, `SubAgentTool::with_model_config`

### Migration

The old builder made you pair a provider with a matching config by hand and
pass the model id twice. The new one takes a single config:

```rust
// before (0.9): provider and config paired manually; model id passed twice
let agent = Agent::new(OpenAiCompatProvider)
    .with_model_config(ModelConfig::zai("glm-4.7", "GLM 4.7"))
    .with_model("glm-4.7")
    .with_api_key(key);

// after (0.10): provider inferred from config.api; key from ZAI_API_KEY
let agent = Agent::from_config(ModelConfig::zai("glm-4.7", "GLM 4.7"));
```

Per constructor:

| Before | After |
|---|---|
| `Agent::new(AnthropicProvider).with_model("m").with_api_key(k)` | `Agent::from_config(ModelConfig::anthropic("m", "Name")).with_api_key(k)` (drop `with_api_key` to use `ANTHROPIC_API_KEY`) |
| `Agent::new(P).with_model_config(cfg).with_model(cfg.id)` | `Agent::from_config(cfg)` |
| `Agent::new(customProvider).with_model("m")` | `Agent::from_provider(customProvider, cfg)` |
| `Agent::new(MockProvider::text("hi")).with_model("mock")` | `Agent::from_provider(MockProvider::text("hi"), ModelConfig::mock())` |
| `SubAgentTool::new(name, provider).with_model_config(cfg)` | `SubAgentTool::from_config(name, cfg)` or `from_provider(name, provider, cfg)` |

`with_api_key` is **not** deprecated — keep it wherever you want to pass a key
explicitly instead of via the environment.

### Fixed

- Google/Vertex usage no longer double-counts cached tokens.
- `Retry-After` is clamped to `max_delay_ms`.
- Compaction budget calibration subtracts measured overhead instead of scaling
  by a ratio (the old formula could collapse the budget toward zero).
- `session_cost_usd()` returns `None` for unpriced models instead of `0.0`.
- Missing API keys now log a warning naming the env var to set.
