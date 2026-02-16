# OpenAI Compatible Provider

`OpenAiCompatProvider` implements the OpenAI Chat Completions API. One implementation covers OpenAI, xAI, Groq, Cerebras, OpenRouter, Mistral, DeepSeek, and any other compatible API.

## Usage

Requires a `ModelConfig` with `compat` flags set in `StreamConfig.model_config`:

```rust
use yo_agent::provider::{OpenAiCompatProvider, ModelConfig};

let agent = Agent::new(OpenAiCompatProvider)
    .with_model("gpt-4o")
    .with_api_key(std::env::var("OPENAI_API_KEY").unwrap());
```

## OpenAiCompat Quirk Flags

Different providers have behavioral differences even though they share the same API:

```rust
pub struct OpenAiCompat {
    pub supports_store: bool,
    pub supports_developer_role: bool,
    pub supports_reasoning_effort: bool,
    pub supports_usage_in_streaming: bool,
    pub max_tokens_field: MaxTokensField,       // MaxTokens or MaxCompletionTokens
    pub requires_tool_result_name: bool,
    pub requires_assistant_after_tool_result: bool,
    pub thinking_format: ThinkingFormat,        // OpenAi, Xai, or Qwen
}
```

## Provider Presets

| Provider | Constructor | Key Differences |
|----------|-------------|-----------------|
| OpenAI | `OpenAiCompat::openai()` | `developer` role, `max_completion_tokens`, `store`, `reasoning_effort` |
| xAI (Grok) | `OpenAiCompat::xai()` | `reasoning` field for thinking (not `reasoning_content`) |
| Groq | `OpenAiCompat::groq()` | Standard defaults |
| Cerebras | `OpenAiCompat::cerebras()` | Standard defaults |
| OpenRouter | `OpenAiCompat::openrouter()` | `max_completion_tokens` |
| Mistral | `OpenAiCompat::mistral()` | `max_tokens` field |
| DeepSeek | `OpenAiCompat::deepseek()` | `max_completion_tokens` |

## Adding a New Compatible Provider

1. Add a constructor to `OpenAiCompat`:

```rust
impl OpenAiCompat {
    pub fn my_provider() -> Self {
        Self {
            supports_usage_in_streaming: true,
            // set flags as needed...
            ..Default::default()
        }
    }
}
```

2. Create a `ModelConfig` that uses it:

```rust
let config = ModelConfig {
    id: "my-model".into(),
    name: "My Model".into(),
    api: ApiProtocol::OpenAiCompletions,
    provider: "my-provider".into(),
    base_url: "https://api.myprovider.com/v1".into(),
    compat: Some(OpenAiCompat::my_provider()),
    // ...
};
```

## Thinking/Reasoning

The `ThinkingFormat` enum controls how reasoning content is parsed from streams:

- `ThinkingFormat::OpenAi` — Uses `reasoning_content` field (DeepSeek, default)
- `ThinkingFormat::Xai` — Uses `reasoning` field (Grok)
- `ThinkingFormat::Qwen` — Uses `reasoning_content` field (Qwen)

## Auth

Uses `Authorization: Bearer {api_key}` header. Extra headers can be added via `ModelConfig.headers`.
