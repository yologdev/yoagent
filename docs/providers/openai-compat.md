# OpenAI Compatible Provider

`OpenAiCompatProvider` implements the OpenAI Chat Completions API. One implementation covers OpenAI, xAI, Groq, Cerebras, OpenRouter, Mistral, DeepSeek, MiniMax, Z.ai, Qwen, Ollama, and any other compatible API.

For the first-class `ModelConfig::*` constructors and default model metadata, see [Model Presets](model-presets.md).

## Usage

Requires a `ModelConfig` with `compat` flags set in `StreamConfig.model_config`:

```rust
use yoagent::provider::ModelConfig;

let agent = Agent::from_config(ModelConfig::openai("gpt-5.5", "GPT-5.5"));
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
    pub supports_prompt_cache_key: bool,
    pub replays_reasoning_content: bool,
    pub max_reasoning_effort: ReasoningEffortCeiling, // High (default), XHigh, Max
    pub supports_effort_none: bool,             // Off sends effort "none"
}
```

`OpenAiCompat` is `#[non_exhaustive]`: start from a preset or
`Default::default()` and set fields.

## Provider Presets

| Provider | Constructor | Key Differences |
|----------|-------------|-----------------|
| OpenAI | `OpenAiCompat::openai()` | `developer` role, `max_completion_tokens`, `store`, `reasoning_effort` |
| xAI (Grok) | `OpenAiCompat::xai()` | `reasoning` field for thinking (not `reasoning_content`); effort ceiling `xhigh` |
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

With `supports_reasoning_effort`, `ThinkingLevel` becomes `reasoning_effort`.
How far up the ladder it goes is the model's declared
`max_reasoning_effort` (a `ReasoningEffortCeiling`):

| Level | Ceiling `High` (default) | Ceiling `XHigh` | Ceiling `Max` | DeepSeek-style (`supports_thinking_control`) |
|-------|------|------|------|------|
| `Off` | omitted, or `none`¹ | omitted, or `none`¹ | omitted, or `none`¹ | omitted; `thinking: disabled` |
| `Minimal`, `Low` | `low` | `low` | `low` | `low` |
| `Medium` | `medium` | `medium` | `medium` | `medium` (DeepSeek rounds it up to `high`) |
| `High` | `high` | `high` | `high` | `high` |
| `XHigh` | `high` (clamped) | `xhigh` | `xhigh` | `high` (clamped) |
| `Max` | `high` (clamped) | `xhigh` (clamped) | `max` | `max` |

¹ `none` when `supports_effort_none` is set. Omitting the effort does not turn
reasoning off on an OpenAI reasoning model — it runs at the model's default,
`medium` — so where the model has a `none` rung, `Off` must send it.

A model rejects an effort string it does not know rather than rounding it,
which is why the ceiling is declared per model and never guessed from the id.
Per OpenAI's model pages and Azure's reasoning guide (2026-09-25): `max` exists
on GPT-5.6 and GPT-6; `xhigh` on those plus GPT-5.5, GPT-5.4, gpt-5.2 and
gpt-5.1-codex-max; gpt-5 and gpt-5.1 top out at `high`. `none` exists on
gpt-5.1 and later except GPT-6 Astra. The `gpt_5_5()` preset declares `XHigh`
and `none`; `OpenAiCompat::xai()` declares `XHigh`, because xAI treats `xhigh`
as `high` on Grok models without the rung instead of rejecting it. For a
model with no preset:

```rust
use yoagent::provider::{ModelConfig, ReasoningEffortCeiling};

let mut config = ModelConfig::openai("gpt-5.4", "GPT-5.4");
let compat = config.compat.as_mut().unwrap();
compat.max_reasoning_effort = ReasoningEffortCeiling::XHigh;
compat.supports_effort_none = true;
```

The DeepSeek-style column needs both `supports_thinking_control` and
`supports_reasoning_effort`, and ignores `max_reasoning_effort` and
`supports_effort_none`. DeepSeek's `reasoning_effort` accepts
`low`/`high`/`max`
([DeepSeek thinking-mode docs](https://api-docs.deepseek.com/guides/thinking_mode)).
DeepSeek itself maps a requested `xhigh` to `high`, so this crate sends `XHigh`
as `high` there too and only `Max` selects DeepSeek's `max` rung.

The OpenAI Responses and Azure OpenAI providers apply the same ceiling and
`none` columns, read from `ModelConfig::compat`.

The `ThinkingFormat` enum controls how reasoning content is parsed from streams:

- `ThinkingFormat::OpenAi` — Uses `reasoning_content` field (DeepSeek, default)
- `ThinkingFormat::Xai` — Uses `reasoning` field (Grok)
- `ThinkingFormat::Qwen` — Uses `reasoning_content` field (Qwen)

## Local Servers (LM Studio, Ollama, llama.cpp, vLLM)

Use `ModelConfig::ollama()` for Ollama, or `ModelConfig::local()` for any other local OpenAI-compatible server. No API key required:

```rust
use yoagent::agent::Agent;
use yoagent::provider::ModelConfig;

// The `local` provider resolves to an empty API key automatically — none needed.
let agent = Agent::from_config(ModelConfig::local("http://localhost:1234/v1", "my-model"));
```

For Ollama:

```rust
let agent = Agent::from_config(ModelConfig::ollama("http://localhost:11434/v1", "llama3.1:8b"));
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
use yoagent::provider::{ModelConfig, OpenAiCompat};

let mut config = ModelConfig::openai_compat(
    "https://api.githubcopilot.com",
    "gpt-5.5",
    "copilot",
    OpenAiCompat::openai(),
);
// Copilot fingerprints clients via these headers; they are required.
config.headers.insert("Copilot-Integration-Id".into(), "vscode-chat".into());
config.headers.insert("Editor-Version".into(), "Neovim/0.10.0".into());

let agent = Agent::from_config(config)
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
