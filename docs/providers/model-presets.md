# Model Presets

yoagent's first-class model presets are the `ModelConfig::*` constructors. A preset sets provider routing, base URL, context metadata, default output limits, and provider compatibility flags.

Use a preset when the provider is listed here. Use a custom `ModelConfig` when you need a compatible provider or endpoint that does not have a constructor yet.

## First-Class Constructors

| Constructor | Provider | Protocol | Default Base URL | Context | Default Max Output |
|-------------|----------|----------|------------------|---------|--------------------|
| `ModelConfig::anthropic(id, name)` | Anthropic | `AnthropicMessages` | `https://api.anthropic.com/v1` | 200K | 16,000 |
| `ModelConfig::claude_fable_5()` | Anthropic | `AnthropicMessages` | `https://api.anthropic.com/v1` | 1M | 64,000 |
| `ModelConfig::claude_opus_4_8()` | Anthropic | `AnthropicMessages` | `https://api.anthropic.com/v1` | 1M | 64,000 |
| `ModelConfig::claude_sonnet_5()` | Anthropic | `AnthropicMessages` | `https://api.anthropic.com/v1` | 1M | 64,000 |
| `ModelConfig::claude_haiku_4_5()` | Anthropic | `AnthropicMessages` | `https://api.anthropic.com/v1` | 200K | 32,000 |
| `ModelConfig::openai(id, name)` | OpenAI | `OpenAiCompletions` | `https://api.openai.com/v1` | 128K | 4,096 |
| `ModelConfig::gpt_5_5()` | OpenAI | `OpenAiCompletions` | `https://api.openai.com/v1` | 1M | 64,000 |
| `ModelConfig::opencode_zen(model_id)` | OpenCode Zen | by model family | `https://opencode.ai/zen/v1` | 128K | 16,000 |
| `ModelConfig::opencode_go(model_id)` | OpenCode Go | by model family | `https://opencode.ai/zen/go/v1` | 128K | 16,000 |
| `ModelConfig::google(id, name)` | Google Gemini | `GoogleGenerativeAi` | `https://generativelanguage.googleapis.com` | 1M | 8,192 |
| `ModelConfig::xai(id, name)` | xAI | `OpenAiCompletions` | `https://api.x.ai/v1` | 131,072 | 4,096 |
| `ModelConfig::groq(id, name)` | Groq | `OpenAiCompletions` | `https://api.groq.com/openai/v1` | 128K | 4,096 |
| `ModelConfig::deepseek(id, name)` | DeepSeek | `OpenAiCompletions` | `https://api.deepseek.com` | 1M | 384K |
| `ModelConfig::mistral(id, name)` | Mistral | `OpenAiCompletions` | `https://api.mistral.ai/v1` | 128K | 4,096 |
| `ModelConfig::minimax(id, name)` | MiniMax | `OpenAiCompletions` | `https://api.minimaxi.chat/v1` | 1M | 4,096 |
| `ModelConfig::zai(id, name)` | Z.ai | `OpenAiCompletions` | `https://api.z.ai/api/paas/v4` | 128K | 4,096 |
| `ModelConfig::qwen(id, name)` | Qwen / DashScope | `OpenAiCompletions` | `https://dashscope-intl.aliyuncs.com/compatible-mode/v1` | 128K | 4,096 |
| `ModelConfig::ollama(base_url, model_id)` | Ollama | `OpenAiCompletions` | caller provided | 128K | 4,096 |
| `ModelConfig::openai_compat(base_url, model_id, provider, compat)` | Custom compatible server | `OpenAiCompletions` | caller provided | 128K | 4,096 |
| `ModelConfig::local(base_url, model_id)` | Local compatible server | `OpenAiCompletions` | caller provided | 128K | 4,096 |

The constructors do not validate model IDs. They send the `id` you pass through to the provider, which lets you use newly released model IDs before yoagent updates its examples.

The named presets (`claude_fable_5`, `claude_opus_4_8`, `claude_sonnet_5`, `claude_haiku_4_5`, `gpt_5_5`) also fill in real `CostConfig` pricing. The OpenCode presets select the API protocol from the model id — see [OpenCode Zen & Go](opencode.md).

## OpenAI-Compatible Presets

These constructors all use `OpenAiCompatProvider`:

```rust
use yoagent::provider::ModelConfig;

let agent = Agent::from_config(ModelConfig::deepseek(
    "deepseek-v4-flash",
    "DeepSeek V4 Flash",
));
```

OpenAI-compatible presets also set `OpenAiCompat` flags for provider-specific API differences, such as `max_tokens` vs. `max_completion_tokens`, reasoning fields, tool result formatting, and streaming usage support. See [OpenAI Compatible](openai-compat.md) for the full quirk-flag list.

## Ollama Models

Use `ModelConfig::ollama` for Ollama's OpenAI-compatible endpoint:

```rust
let llama = ModelConfig::ollama("http://localhost:11434/v1", "llama3.1:8b");
```

Ollama remains separate from `ModelConfig::local(...)` because some Ollama-served models need an assistant message after tool results, while other local OpenAI-compatible servers may not. The Ollama preset enables that transcript workaround; the generic local preset stays neutral.

## Qwen Models

Use `ModelConfig::qwen` for hosted Qwen / DashScope:

```rust
let qwen = ModelConfig::qwen("qwen3.6-plus", "Qwen 3.6 Plus");
```

The default base URL is the international DashScope endpoint. For other regions, override `base_url` after construction:

```rust
let mut qwen = ModelConfig::qwen("qwen3.6-plus", "Qwen 3.6 Plus");
qwen.base_url = "https://dashscope.aliyuncs.com/compatible-mode/v1".into();
```

Region endpoints:

- International/Singapore: `https://dashscope-intl.aliyuncs.com/compatible-mode/v1`
- China/Beijing: `https://dashscope.aliyuncs.com/compatible-mode/v1`
- US/Virginia: `https://dashscope-us.aliyuncs.com/compatible-mode/v1`

For locally deployed Qwen, keep the local endpoint and opt into Qwen's model-family compat flags:

```rust
let qwen_local = ModelConfig::openai_compat(
    "http://localhost:1234/v1",
    "qwen3-local",
    "qwen",
    OpenAiCompat::qwen(),
);
```

If a local serving layer also has its own quirks, combine the compat flags explicitly. For example, Qwen served by Ollama may need both Qwen reasoning parsing and Ollama's tool-result transcript workaround:

```rust
let mut compat = OpenAiCompat::qwen();
compat.requires_assistant_after_tool_result = true;

let qwen_ollama = ModelConfig::openai_compat(
    "http://localhost:11434/v1",
    "qwen2.5-coder:7b",
    "ollama",
    compat,
);
```

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
let chat_agent = Agent::from_config(ModelConfig::deepseek("deepseek-chat", "DeepSeek Chat"))
    .with_thinking(ThinkingLevel::Off);

let reasoner_agent = Agent::from_config(ModelConfig::deepseek("deepseek-reasoner", "DeepSeek Reasoner"))
    .with_thinking(ThinkingLevel::High);
```

Older DeepSeek reasoning models had stricter feature limits than the current V4 API. In particular, historical `deepseek-reasoner` documentation did not support function calling. If you need tools, prefer current V4 model IDs unless you have tested the legacy alias for your workflow.

## Compat Flags Without Constructors

`OpenAiCompat` also has quirk presets such as `OpenAiCompat::cerebras()` and `OpenAiCompat::openrouter()`. Those are compatibility profiles, not full `ModelConfig` constructors. To use them, call `ModelConfig::openai_compat(...)` with the provider name, base URL, and `compat` value you need.
