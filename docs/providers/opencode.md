# OpenCode Zen & Go

[OpenCode Zen](https://opencode.ai/docs/zen) (pay-per-use) and [OpenCode Go](https://opencode.ai/docs/go) (subscription) are model gateways run by the OpenCode team. Both are supported through the `ModelConfig::opencode_zen()` and `ModelConfig::opencode_go()` presets.

The gateways serve different model families over different protocols. The presets select the protocol automatically from the model id:

| Gateway | Model family | Protocol | Pair with |
|---------|-------------|----------|-----------|
| Zen | `gpt-*` | OpenAI Responses | `OpenAiResponsesProvider` |
| Zen | `claude-*`, `qwen*` | Anthropic Messages | `AnthropicProvider` |
| Zen | DeepSeek, MiniMax, GLM, Kimi, ... | Chat Completions | `OpenAiCompatProvider` |
| Go | `qwen*`, `minimax-*` | Anthropic Messages | `AnthropicProvider` |
| Go | GLM, Kimi, DeepSeek, MiMo, ... | Chat Completions | `OpenAiCompatProvider` |

Gemini models on Zen are **not supported** — Zen serves them over a Google-native endpoint shape yoagent does not target. A `gemini-*` id falls through to Chat Completions (with a warning logged) and will fail at request time.

The routing table mirrors the Zen/Go endpoint docs as of mid-2026. OpenCode can change gateway-side routing at any time — if a model errors, verify its protocol against `{base}/models`.

## Usage

Because `Agent::new()` takes a concrete provider, pair the preset with the provider matching its protocol (check `config.api`):

```rust
use yoagent::provider::{AnthropicProvider, ModelConfig, OpenAiCompatProvider};
use yoagent::Agent;

let key = std::env::var("OPENCODE_API_KEY").unwrap();

// Chat-completions model (GLM, Kimi, DeepSeek, ...)
let config = ModelConfig::opencode_zen("glm-5.2");
let agent = Agent::new(OpenAiCompatProvider)
    .with_model(&config.id)
    .with_api_key(&key)
    .with_model_config(config);

// Claude/Qwen model — Anthropic Messages protocol
let config = ModelConfig::opencode_zen("claude-sonnet-5");
let agent = Agent::new(AnthropicProvider)
    .with_model(&config.id)
    .with_api_key(&key)
    .with_model_config(config);
```

For OpenCode Go, use `ModelConfig::opencode_go("kimi-k2.7-code")` — the base URL and protocol map differ, the usage pattern is identical.

## Authentication

Both gateways use `Authorization: Bearer {api_key}`. For the Anthropic-protocol models the presets set `AnthropicCompat { bearer_auth: true, .. }` so the Anthropic provider sends Bearer auth instead of its native `x-api-key` header.

Get an API key by signing in at [opencode.ai](https://opencode.ai) (Zen) or subscribing to Go.

## Defaults

The presets use conservative defaults (128K context window, 16K max output). Override the fields for models with larger limits:

```rust
let mut config = ModelConfig::opencode_zen("kimi-k2.7-code");
config.context_window = 256_000;
```

## Endpoints

- Zen: `https://opencode.ai/zen/v1/{chat/completions | messages | responses}`
- Go: `https://opencode.ai/zen/go/v1/{chat/completions | messages}`

Model list and metadata: `https://opencode.ai/zen/v1/models` and `https://opencode.ai/zen/go/v1/models`.
