# Providers Overview

yoagent supports multiple LLM providers through the `StreamProvider` trait and `ApiProtocol` dispatch.

## Supported Protocols

| Protocol | Provider Struct | API Format |
|----------|----------------|------------|
| `AnthropicMessages` | `AnthropicProvider` | Anthropic Messages API |
| `OpenAiCompletions` | `OpenAiCompatProvider` | OpenAI Chat Completions |
| `OpenAiResponses` | `OpenAiResponsesProvider` | OpenAI Responses API |
| `AzureOpenAiResponses` | `AzureOpenAiProvider` | Azure OpenAI Responses |
| `GoogleGenerativeAi` | `GoogleProvider` | Google Gemini API |
| `GoogleVertex` | `GoogleVertexProvider` | Google Vertex AI |
| `BedrockConverseStream` | `BedrockProvider` | AWS Bedrock ConverseStream |

On native targets HTTPS comes from reqwest's TLS, which the default `native`
feature enables. With `default-features = false` and no `features = ["native"]`
the crate still builds, but every HTTPS provider call fails at runtime (as a
retryable network error). On `wasm32-unknown-unknown` all seven providers run
over the host's `fetch`; see [WebAssembly & Cloudflare Workers](../guides/wasm-workers.md).

## ApiProtocol Enum

```rust
pub enum ApiProtocol {
    AnthropicMessages,
    OpenAiCompletions,
    OpenAiResponses,
    AzureOpenAiResponses,
    GoogleGenerativeAi,
    GoogleVertex,
    BedrockConverseStream,
}
```

## ModelConfig

Full configuration for a model, including provider routing:

```rust
#[non_exhaustive]  // build with a constructor, then set fields
pub struct ModelConfig {
    pub id: String,              // e.g. "gpt-5.5"
    pub name: String,            // e.g. "GPT-5.5"
    pub api: ApiProtocol,        // Which provider to use
    pub provider: String,        // e.g. "openai"
    pub base_url: String,        // API endpoint
    pub reasoning: bool,         // Supports thinking/reasoning
    pub context_window: u32,     // Context size in tokens
    pub max_tokens: u32,         // Default max output
    pub cost: Option<CostConfig>, // Pricing per million tokens; None = unknown
    pub headers: HashMap<String, String>,  // Extra headers
    pub compat: Option<OpenAiCompat>,      // OpenAI quirk flags (Chat Completions; effort ceiling also read by Responses/Azure)
    pub anthropic: Option<AnthropicCompat>, // Anthropic Messages quirk flags (thinking mode, bearer auth, native structured output)
    pub google: Option<GoogleCompat>,      // Gemini quirk flags (thinkingLevel vs thinkingBudget override)
}
```

First-class model presets are documented in [Model Presets](model-presets.md). Convenience constructors:

```rust
let anthropic = ModelConfig::anthropic("claude-sonnet-5", "Claude Sonnet 5");
let openai = ModelConfig::openai("gpt-5.5", "GPT-5.5");
let responses = ModelConfig::openai_responses("gpt-5.5", "GPT-5.5");
let google = ModelConfig::google("gemini-3.8-flash", "Gemini 3.8 Flash");
let xai = ModelConfig::xai("grok-4.7", "Grok 4.7");
let groq = ModelConfig::groq("openai/gpt-oss-120b", "GPT-OSS 120B");
let deepseek = ModelConfig::deepseek("deepseek-flash", "DeepSeek Flash");
let mistral = ModelConfig::mistral("mistral-large-latest", "Mistral Large");
let minimax = ModelConfig::minimax("MiniMax-M3", "MiniMax M3");
let zai = ModelConfig::zai("glm-4.7", "GLM 4.7");
let qwen = ModelConfig::qwen("qwen3.6-plus", "Qwen 3.6 Plus");
let ollama = ModelConfig::ollama("http://localhost:11434/v1", "llama3.1:8b");
let local = ModelConfig::local("http://localhost:1234/v1", "my-model");
// Any protocol, any endpoint (Azure, Bedrock, Vertex, gateways):
let bedrock = ModelConfig::custom(
    ApiProtocol::BedrockConverseStream,
    "bedrock",
    "https://bedrock-runtime.us-east-1.amazonaws.com",
    "anthropic.claude-opus-4-8",
    "Claude Opus 4.8",
);
```

## ProviderRegistry

Maps `ApiProtocol` → `StreamProvider`. The default registry includes all built-in providers:

```rust
let registry = ProviderRegistry::default();

// Use it to stream with any model
let result = registry.stream(&model_config, stream_config, tx, cancel).await?;
```

Custom registries:

```rust
let mut registry = ProviderRegistry::new();
registry.register(ApiProtocol::AnthropicMessages, AnthropicProvider);
```

## API keys

`Agent::from_config` (also `SubAgentTool::from_config` and `set_model`)
resolves the key from the environment variable named for `ModelConfig.provider`
(`provider::resolve_api_key`). An explicit `with_api_key` always wins.

| `provider` | variable(s), first match wins |
|---|---|
| `anthropic` | `ANTHROPIC_API_KEY` |
| `openai` | `OPENAI_API_KEY` |
| `google` | `GEMINI_API_KEY`, `GOOGLE_API_KEY` |
| `xai`, `groq`, `deepseek`, `mistral`, `zai`, `minimax`, `openrouter`, `cerebras` | `<PROVIDER>_API_KEY` |
| `qwen` | `DASHSCOPE_API_KEY` |
| `meta` | `META_API_KEY`, `MODEL_API_KEY` |
| `opencode-zen`, `opencode-go` | `OPENCODE_API_KEY` |
| `azure` | `AZURE_OPENAI_API_KEY` |
| any `BedrockConverseStream` config | none here: the provider reads `AWS_BEARER_TOKEN_BEDROCK`, or `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY` (+ `AWS_SESSION_TOKEN`), per request ([Bedrock](bedrock.md)) |
| `vertex` | none: pass an OAuth access token with `with_api_key` ([Vertex](google.md#google-vertex-ai)) |
| `local`, `ollama` | none needed (empty key) |
| anything else | `YOAGENT_API_KEY`, `API_KEY` |

**wasm32 (Cloudflare Workers):** there are no environment variables, so none of
the above is found. Pass every key with `with_api_key` (see
[WebAssembly & Cloudflare Workers](../guides/wasm-workers.md)).

## StreamProvider Trait

```rust
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
pub trait StreamProvider: rt::MaybeSend + rt::MaybeSync {
    async fn stream(
        &self,
        config: StreamConfig,
        tx: mpsc::UnboundedSender<StreamEvent>,
        cancel: CancellationToken,
    ) -> Result<Message, ProviderError>;

    /// The protocol this provider speaks; `None` (the default) for test
    /// doubles and multi-protocol adapters.
    fn protocol(&self) -> Option<ApiProtocol> { None }
}
```

All providers receive a `StreamConfig`, emit `StreamEvent`s through the
channel, and return the final `Message`. `rt::MaybeSend + rt::MaybeSync` is
exactly `Send + Sync` on native targets, so a plain `#[async_trait]` impl works
there; a provider that also builds for wasm32 uses the two `cfg_attr` lines.

**Drop every clone of `tx` before `stream` returns.** The agent loop forwards an
attempt's events until the channel closes, and only then decides whether to
retry or end the turn; a sender kept alive afterwards (in a spawned task, say)
makes the loop wait.

Return `ProviderError::RateLimited` or `ProviderError::Network` for failures
worth retrying; the loop does the retrying. Each retried attempt is closed with
a `MessageEnd` (`StopReason::Error`) and followed by `AgentEvent::ProviderRetry`
(see [Retry](../concepts/retry.md)). Return `ProviderError::Cancelled` when
`cancel` fires; the turn then ends with `StopReason::Aborted`.

## OpenAPI Tool Adapter

In addition to LLM providers, yoagent can auto-generate tools from any OpenAPI 3.0 spec. This is a tool integration (not a provider), but it complements the provider system by letting agents call external APIs.

Enable with `features = ["openapi"]`. See the [OpenAPI Tools guide](../guides/openapi.md) for details.
