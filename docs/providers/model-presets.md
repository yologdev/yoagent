# Model Presets

yoagent's first-class model presets are the `ModelConfig::*` constructors. A preset sets provider routing, base URL, context metadata, default output limits, and provider compatibility flags.

Use a preset when the provider is listed here. Use a custom `ModelConfig` when you need a compatible provider or endpoint that does not have a constructor yet.

## First-Class Constructors

| Constructor | Provider | Protocol | Default Base URL | Context | Default Max Output |
|-------------|----------|----------|------------------|---------|--------------------|
| `ModelConfig::anthropic(id, name)` | Anthropic | `AnthropicMessages` | `https://api.anthropic.com` | 200K | 8,192 |
| `ModelConfig::openai(id, name)` | OpenAI | `OpenAiCompletions` | `https://api.openai.com/v1` | 128K | 4,096 |
| `ModelConfig::google(id, name)` | Google Gemini | `GoogleGenerativeAi` | `https://generativelanguage.googleapis.com` | 1M | 8,192 |
| `ModelConfig::xai(id, name)` | xAI | `OpenAiCompletions` | `https://api.x.ai/v1` | 131,072 | 4,096 |
| `ModelConfig::groq(id, name)` | Groq | `OpenAiCompletions` | `https://api.groq.com/openai/v1` | 128K | 4,096 |
| `ModelConfig::deepseek(id, name)` | DeepSeek | `OpenAiCompletions` | `https://api.deepseek.com` | 1M | 384K |
| `ModelConfig::mistral(id, name)` | Mistral | `OpenAiCompletions` | `https://api.mistral.ai/v1` | 128K | 4,096 |
| `ModelConfig::minimax(id, name)` | MiniMax | `OpenAiCompletions` | `https://api.minimaxi.chat/v1` | 1M | 4,096 |
| `ModelConfig::zai(id, name)` | Z.ai | `OpenAiCompletions` | `https://api.z.ai/api/paas/v4` | 128K | 4,096 |
| `ModelConfig::local(base_url, model_id)` | Local compatible server | `OpenAiCompletions` | caller provided | 128K | 4,096 |

The constructors do not validate model IDs. They send the `id` you pass through to the provider, which lets you use newly released model IDs before yoagent updates its examples.

## OpenAI-Compatible Presets

These constructors all use `OpenAiCompatProvider`:

```rust
use yoagent::provider::{ModelConfig, OpenAiCompatProvider};

let agent = Agent::new(OpenAiCompatProvider)
    .with_model_config(ModelConfig::deepseek(
        "deepseek-v4-flash",
        "DeepSeek V4 Flash",
    ))
    .with_model("deepseek-v4-flash")
    .with_api_key(std::env::var("DEEPSEEK_API_KEY").unwrap());
```

OpenAI-compatible presets also set `OpenAiCompat` flags for provider-specific API differences, such as `max_tokens` vs. `max_completion_tokens`, reasoning fields, tool result formatting, and streaming usage support. See [OpenAI Compatible](openai-compat.md) for the full quirk-flag list.

## DeepSeek Models

Use the current DeepSeek API model IDs by default:

```rust
let flash = ModelConfig::deepseek("deepseek-v4-flash", "DeepSeek V4 Flash");
let pro = ModelConfig::deepseek("deepseek-v4-pro", "DeepSeek V4 Pro");
```

Legacy DeepSeek aliases still work because `ModelConfig::deepseek` passes the model ID through unchanged:

```rust
let chat = ModelConfig::deepseek("deepseek-chat", "DeepSeek Chat");
let reasoner = ModelConfig::deepseek("deepseek-reasoner", "DeepSeek Reasoner");
```

DeepSeek documents `deepseek-chat` and `deepseek-reasoner` as compatibility aliases scheduled for deprecation on 2026-07-24. In DeepSeek's current API, `deepseek-chat` maps to the non-thinking mode of `deepseek-v4-flash`, while `deepseek-reasoner` maps to the thinking mode of `deepseek-v4-flash`.

yoagent also sends DeepSeek's current request shape:

- `max_tokens`, not `max_completion_tokens`
- `thinking: { "type": "enabled" | "disabled" }`
- `reasoning_effort` when `ThinkingLevel` is not `Off`
- DeepSeek cache hit/miss usage fields when present

For legacy aliases, set `ThinkingLevel` to match the alias behavior:

```rust
let chat_agent = Agent::new(OpenAiCompatProvider)
    .with_model_config(ModelConfig::deepseek("deepseek-chat", "DeepSeek Chat"))
    .with_model("deepseek-chat")
    .with_thinking_level(ThinkingLevel::Off);

let reasoner_agent = Agent::new(OpenAiCompatProvider)
    .with_model_config(ModelConfig::deepseek("deepseek-reasoner", "DeepSeek Reasoner"))
    .with_model("deepseek-reasoner")
    .with_thinking_level(ThinkingLevel::High);
```

Older DeepSeek reasoning models had stricter feature limits than the current V4 API. In particular, historical `deepseek-reasoner` documentation did not support function calling. If you need tools, prefer current V4 model IDs unless you have tested the legacy alias for your workflow.

## Compat Flags Without Constructors

`OpenAiCompat` also has quirk presets such as `OpenAiCompat::cerebras()` and `OpenAiCompat::openrouter()`. Those are compatibility profiles, not full `ModelConfig` constructors. To use them, build a custom `ModelConfig` with the provider name, base URL, protocol, and `compat` value you need.
