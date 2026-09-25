# OpenCode Zen & Go

[OpenCode Zen](https://opencode.ai/docs/zen) (pay-per-use) and [OpenCode Go](https://opencode.ai/docs/go) (subscription) are model gateways run by the OpenCode team. Both are supported through the `ModelConfig::opencode_zen()` and `ModelConfig::opencode_go()` presets.

The gateways serve different model families over different protocols. The presets select the protocol automatically from the model id:

| Gateway | Model family | Protocol | Pair with |
|---------|-------------|----------|-----------|
| Zen | `gpt-*`, `grok-*`, `muse-spark-*` | OpenAI Responses | `OpenAiResponsesProvider` |
| Zen | `claude-*`, `qwen*` | Anthropic Messages | `AnthropicProvider` |
| Zen | DeepSeek, MiniMax, GLM, Kimi, ... | Chat Completions | `OpenAiCompatProvider` |
| Go | `gpt-*`, `grok-*`, `muse-spark-*` | OpenAI Responses | `OpenAiResponsesProvider` |
| Go | `qwen*`, `minimax-*` | Anthropic Messages | `AnthropicProvider` |
| Go | GLM, Kimi, DeepSeek, MiMo, ... | Chat Completions | `OpenAiCompatProvider` |

Gemini models on Zen are **not supported** — Zen serves them over a Google-native endpoint shape yoagent does not target. A `gemini-*` id falls through to Chat Completions (with a warning logged) and will fail at request time.

Jev models (`jev-*`) are **not supported** either — Zen serves them on its `/systemone` evaluation endpoint. They fall through the same way, with a warning.

`claude-*` ids get the same compat flags as the matching `ModelConfig::claude_*` preset, inferred from the version in the id (`AnthropicCompat::for_claude_id`): budget-based thinking before Claude 4.6 (e.g. Haiku 4.5), adaptive thinking from 4.6, and native structured outputs from 4.5 — so `prompt_structured` works on models that reject forced tool choice, such as Opus 5.5 and Fable 5.1.

The routing table mirrors the Zen/Go endpoint docs as of September 2026. OpenCode can change gateway-side routing at any time — if a model errors, verify its protocol against `{base}/models`.

## Usage

`Agent::from_config` selects the built-in provider from the preset's protocol
(`config.api`) and resolves the key from `OPENCODE_API_KEY`, so the same call
works whichever model family you pick:

```rust
use yoagent::provider::ModelConfig;
use yoagent::Agent;

// Chat-completions model (GLM, Kimi, DeepSeek, ...)
let agent = Agent::from_config(ModelConfig::opencode_zen("glm-5.2"));

// Claude/Qwen model — Anthropic Messages protocol
let agent = Agent::from_config(ModelConfig::opencode_zen("claude-sonnet-5"));
```

For OpenCode Go, use `ModelConfig::opencode_go("kimi-k2.7-code")` — the base URL and protocol map differ, the usage pattern is identical.

## Authentication

Both gateways use `Authorization: Bearer {api_key}`. For the Anthropic-protocol models the presets set `AnthropicCompat::bearer_auth`, so the Anthropic provider sends Bearer auth instead of its native `x-api-key` header. To do the same for another Messages-protocol gateway:

```rust
use yoagent::provider::{AnthropicCompat, ModelConfig};

let mut config = ModelConfig::anthropic("claude-sonnet-5", "Claude Sonnet 5");
config.base_url = "https://gateway.example.com/v1".into();
config.anthropic = Some(AnthropicCompat::default().with_bearer_auth(true));
```

Get an API key by signing in at [opencode.ai](https://opencode.ai) (Zen) or subscribing to Go.

## Defaults

The presets use conservative defaults (128K context window, 16K max output). Override the fields for models with larger limits:

```rust
let mut config = ModelConfig::opencode_zen("kimi-k2.7-code");
config.context_window = 256_000;
```

## Endpoints

- Zen: `https://opencode.ai/zen/v1/{chat/completions | messages | responses}`
- Go: `https://opencode.ai/zen/go/v1/{chat/completions | messages | responses}`

Model list and metadata: `https://opencode.ai/zen/v1/models` and `https://opencode.ai/zen/go/v1/models`.
