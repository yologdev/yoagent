# Changelog

All notable changes to `yoagent` are documented here. The format loosely
follows [Keep a Changelog](https://keepachangelog.com/), and the project
adheres to [Semantic Versioning](https://semver.org/).

## Unreleased

### Added

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
