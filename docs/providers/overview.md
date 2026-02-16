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
pub struct ModelConfig {
    pub id: String,              // e.g. "gpt-4o"
    pub name: String,            // e.g. "GPT-4o"
    pub api: ApiProtocol,        // Which provider to use
    pub provider: String,        // e.g. "openai"
    pub base_url: String,        // API endpoint
    pub reasoning: bool,         // Supports thinking/reasoning
    pub context_window: u32,     // Context size in tokens
    pub max_tokens: u32,         // Default max output
    pub cost: CostConfig,        // Pricing per million tokens
    pub headers: HashMap<String, String>,  // Extra headers
    pub compat: Option<OpenAiCompat>,      // Quirk flags
}
```

Convenience constructors:

```rust
let anthropic = ModelConfig::anthropic("claude-sonnet-4-20250514", "Claude Sonnet 4");
let openai = ModelConfig::openai("gpt-4o", "GPT-4o");
let google = ModelConfig::google("gemini-2.0-flash", "Gemini 2.0 Flash");
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

## StreamProvider Trait

```rust
#[async_trait]
pub trait StreamProvider: Send + Sync {
    async fn stream(
        &self,
        config: StreamConfig,
        tx: mpsc::UnboundedSender<StreamEvent>,
        cancel: CancellationToken,
    ) -> Result<Message, ProviderError>;
}
```

All providers receive a `StreamConfig`, emit `StreamEvent`s through the channel, and return the final `Message`.
