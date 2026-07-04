# OpenAI Compatible Provider

`OpenAiCompatProvider` implements the OpenAI Chat Completions API. One implementation covers OpenAI, xAI, Groq, Cerebras, OpenRouter, Mistral, DeepSeek, MiniMax, Z.ai, Qwen, Ollama, and any other compatible API.

For the first-class `ModelConfig::*` constructors and default model metadata, see [Model Presets](model-presets.md).

## Usage

Requires a `ModelConfig` with `compat` flags set in `StreamConfig.model_config`:

```rust
use yoagent::provider::{OpenAiCompatProvider, ModelConfig};

let agent = Agent::new(OpenAiCompatProvider)
    .with_model("gpt-5.5")
    .with_api_key(std::env::var("OPENAI_API_KEY").unwrap());
```

## OpenAiCompat Quirk Flags

Different providers have behavioral differences even though they share the same API:

```rust
pub struct OpenAiCompat {
    pub supports_store: bool,
    pub supports_developer_role: bool,
    pub supports_reasoning_effort: bool,
    pub supports_thinking_control: bool,
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
| DeepSeek | `OpenAiCompat::deepseek()` | `max_tokens`, `thinking`, `reasoning_effort`, 1M context window |
| MiniMax | `OpenAiCompat::minimax()` | Standard defaults, 1M context window |
| Z.ai (Zhipu) | `OpenAiCompat::zai()` | Standard defaults |
| Qwen | `OpenAiCompat::qwen()` | Qwen reasoning content format, `max_tokens`, streaming usage |
| Ollama | `OpenAiCompat::ollama()` | Inserts an empty assistant message after tool result runs |

`OpenAiCompat` presets are lower-level quirk flags. A provider is first-class when it also has a `ModelConfig::*` constructor; see [Model Presets](model-presets.md).

DeepSeek context caching is automatic on DeepSeek's side. yoagent does not send
`cache_control` markers for DeepSeek, but it does parse DeepSeek's
`prompt_cache_hit_tokens` and `prompt_cache_miss_tokens` usage fields into
`Usage.cache_read` and `Usage.input`.

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
let config = ModelConfig::openai_compat(
    "https://api.myprovider.com/v1",
    "my-model",
    "my-provider",
    OpenAiCompat::my_provider(),
);
```

## Thinking/Reasoning

The `ThinkingFormat` enum controls how reasoning content is parsed from streams:

- `ThinkingFormat::OpenAi` — Uses `reasoning_content` field (DeepSeek, default)
- `ThinkingFormat::Xai` — Uses `reasoning` field (Grok)
- `ThinkingFormat::Qwen` — Uses `reasoning_content` field (Qwen)

## Local Servers (LM Studio, Ollama, llama.cpp, vLLM)

Use `ModelConfig::ollama()` for Ollama, or `ModelConfig::local()` for any other local OpenAI-compatible server. No API key required:

```rust
use yoagent::agent::Agent;
use yoagent::provider::{OpenAiCompatProvider, ModelConfig};

let agent = Agent::new(OpenAiCompatProvider)
    .with_model_config(ModelConfig::local("http://localhost:1234/v1", "my-model"))
    .with_model("my-model")
    .with_api_key(""); // empty string OK for local
```

For Ollama:

```rust
let agent = Agent::new(OpenAiCompatProvider)
    .with_model_config(ModelConfig::ollama("http://localhost:11434/v1", "llama3.1:8b"))
    .with_model("llama3.1:8b")
    .with_api_key("");
```

Or via the CLI example:

```bash
cargo run --example cli -- --api-url http://localhost:1234/v1 --model my-model
```

For locally deployed open-source model families, keep the local endpoint and choose the model-family compat profile:

```rust
let qwen_local = ModelConfig::openai_compat(
    "http://localhost:1234/v1",
    "qwen3-local",
    "qwen",
    OpenAiCompat::qwen(),
);
```

Serving-layer quirks and model-family quirks can be combined because `OpenAiCompat` fields are public:

```rust
let mut compat = OpenAiCompat::qwen();
compat.requires_assistant_after_tool_result = true;

let qwen_on_ollama = ModelConfig::openai_compat(
    "http://localhost:11434/v1",
    "qwen2.5-coder:7b",
    "ollama",
    compat,
);
```

## GitHub Copilot (bring-your-own-token)

> **Terms of service.** `api.githubcopilot.com` is intended for use through official
> GitHub Copilot editor integrations. Accessing it from a third-party agent is against
> GitHub's Copilot terms of service and may result in token revocation or account
> suspension. yoagent does **not** ship a first-class Copilot preset for this reason.
> The configuration below is documented only for users who understand and accept that
> risk. Use at your own discretion.

Copilot's chat endpoint is OpenAI Chat Completions–shaped, so it works with
`OpenAiCompatProvider` given the right base URL, integration headers, and a valid
Copilot token as the API key:

```rust
use yoagent::agent::Agent;
use yoagent::provider::{OpenAiCompatProvider, ModelConfig, OpenAiCompat};

let mut config = ModelConfig::openai_compat(
    "https://api.githubcopilot.com",
    "gpt-5.5",
    "copilot",
    OpenAiCompat::openai(),
);
// Copilot fingerprints clients via these headers; they are required.
config.headers.insert("Copilot-Integration-Id".into(), "vscode-chat".into());
config.headers.insert("Editor-Version".into(), "Neovim/0.10.0".into());

let agent = Agent::new(OpenAiCompatProvider)
    .with_model_config(config)
    .with_model("gpt-5.5")
    .with_api_key(copilot_token); // see below
```

**The API key is a short-lived Copilot token, not your GitHub token.** You obtain it by
exchanging a GitHub OAuth token (from the device-login flow, or from the local Copilot
config under `~/.config/github-copilot/`) at
`https://api.github.com/copilot_internal/v2/token`. That token **expires after ~25–30
minutes**.

yoagent has no built-in credential refresh — `api_key` is static for the life of the
provider (`Authorization: Bearer {api_key}`). For anything longer than a single short
turn, **you** must exchange and refresh the token yourself and rebuild the agent's config
with a fresh token before it expires; otherwise long runs will fail with `401`.

## Auth

Uses `Authorization: Bearer {api_key}` header. Extra headers can be added via `ModelConfig.headers`.
