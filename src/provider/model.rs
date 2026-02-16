//! Model configuration and provider compatibility flags.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Which API protocol a model uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiProtocol {
    AnthropicMessages,
    OpenAiCompletions,
    OpenAiResponses,
    AzureOpenAiResponses,
    GoogleGenerativeAi,
    GoogleVertex,
    BedrockConverseStream,
}

impl std::fmt::Display for ApiProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AnthropicMessages => write!(f, "anthropic_messages"),
            Self::OpenAiCompletions => write!(f, "openai_completions"),
            Self::OpenAiResponses => write!(f, "openai_responses"),
            Self::AzureOpenAiResponses => write!(f, "azure_openai_responses"),
            Self::GoogleGenerativeAi => write!(f, "google_generative_ai"),
            Self::GoogleVertex => write!(f, "google_vertex"),
            Self::BedrockConverseStream => write!(f, "bedrock_converse_stream"),
        }
    }
}

/// Cost per million tokens (input/output).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostConfig {
    pub input_per_million: f64,
    pub output_per_million: f64,
    #[serde(default)]
    pub cache_read_per_million: f64,
    #[serde(default)]
    pub cache_write_per_million: f64,
}

impl Default for CostConfig {
    fn default() -> Self {
        Self {
            input_per_million: 0.0,
            output_per_million: 0.0,
            cache_read_per_million: 0.0,
            cache_write_per_million: 0.0,
        }
    }
}

/// How a provider handles the `max_tokens` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MaxTokensField {
    #[default]
    MaxTokens,
    MaxCompletionTokens,
}

/// How a provider formats thinking/reasoning output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingFormat {
    #[default]
    OpenAi,
    Xai,
    Qwen,
}

/// Compatibility flags for OpenAI-compatible providers.
/// Different providers have different quirks even though they share the same base API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiCompat {
    /// Supports the `store` parameter for conversation persistence.
    pub supports_store: bool,
    /// Supports `developer` role (system-level instructions).
    pub supports_developer_role: bool,
    /// Supports `reasoning_effort` parameter.
    pub supports_reasoning_effort: bool,
    /// Includes usage data in streaming responses.
    pub supports_usage_in_streaming: bool,
    /// Which field name to use for max tokens.
    pub max_tokens_field: MaxTokensField,
    /// Tool results must include a `name` field.
    pub requires_tool_result_name: bool,
    /// Must insert an assistant message after tool results.
    pub requires_assistant_after_tool_result: bool,
    /// How thinking/reasoning content is formatted in streaming.
    pub thinking_format: ThinkingFormat,
}

impl Default for OpenAiCompat {
    fn default() -> Self {
        Self {
            supports_store: false,
            supports_developer_role: false,
            supports_reasoning_effort: false,
            supports_usage_in_streaming: true,
            max_tokens_field: MaxTokensField::MaxTokens,
            requires_tool_result_name: false,
            requires_assistant_after_tool_result: false,
            thinking_format: ThinkingFormat::OpenAi,
        }
    }
}

impl OpenAiCompat {
    /// Compat flags for native OpenAI.
    pub fn openai() -> Self {
        Self {
            supports_store: true,
            supports_developer_role: true,
            supports_reasoning_effort: true,
            supports_usage_in_streaming: true,
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
            ..Default::default()
        }
    }

    /// Compat flags for xAI (Grok).
    pub fn xai() -> Self {
        Self {
            supports_usage_in_streaming: true,
            thinking_format: ThinkingFormat::Xai,
            ..Default::default()
        }
    }

    /// Compat flags for Groq.
    pub fn groq() -> Self {
        Self {
            supports_usage_in_streaming: true,
            ..Default::default()
        }
    }

    /// Compat flags for Cerebras.
    pub fn cerebras() -> Self {
        Self::default()
    }

    /// Compat flags for OpenRouter.
    pub fn openrouter() -> Self {
        Self {
            supports_usage_in_streaming: true,
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
            ..Default::default()
        }
    }

    /// Compat flags for Mistral.
    pub fn mistral() -> Self {
        Self {
            supports_usage_in_streaming: true,
            max_tokens_field: MaxTokensField::MaxTokens,
            ..Default::default()
        }
    }

    /// Compat flags for DeepSeek.
    pub fn deepseek() -> Self {
        Self {
            supports_usage_in_streaming: true,
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
            ..Default::default()
        }
    }
}

/// Full model configuration. Knows everything needed to make API calls.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    /// Model identifier sent to the API (e.g. "gpt-4o", "claude-sonnet-4-20250514").
    pub id: String,
    /// Human-friendly name.
    pub name: String,
    /// Which API protocol to use.
    pub api: ApiProtocol,
    /// Provider name (e.g. "openai", "anthropic", "xai").
    pub provider: String,
    /// Base URL for API requests (without trailing slash).
    pub base_url: String,
    /// Whether this model supports reasoning/thinking.
    pub reasoning: bool,
    /// Context window size in tokens.
    pub context_window: u32,
    /// Default max output tokens.
    pub max_tokens: u32,
    /// Cost configuration.
    #[serde(default)]
    pub cost: CostConfig,
    /// Additional headers to send with requests.
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// OpenAI-compat quirk flags (only for OpenAiCompletions protocol).
    #[serde(default)]
    pub compat: Option<OpenAiCompat>,
}

impl ModelConfig {
    /// Create a new Anthropic model config.
    pub fn anthropic(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            api: ApiProtocol::AnthropicMessages,
            provider: "anthropic".into(),
            base_url: "https://api.anthropic.com".into(),
            reasoning: false,
            context_window: 200_000,
            max_tokens: 8192,
            cost: CostConfig::default(),
            headers: HashMap::new(),
            compat: None,
        }
    }

    /// Create a new OpenAI model config.
    pub fn openai(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            api: ApiProtocol::OpenAiCompletions,
            provider: "openai".into(),
            base_url: "https://api.openai.com/v1".into(),
            reasoning: false,
            context_window: 128_000,
            max_tokens: 4096,
            cost: CostConfig::default(),
            headers: HashMap::new(),
            compat: Some(OpenAiCompat::openai()),
        }
    }

    /// Create a new Google Generative AI (Gemini) model config.
    pub fn google(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            api: ApiProtocol::GoogleGenerativeAi,
            provider: "google".into(),
            base_url: "https://generativelanguage.googleapis.com".into(),
            reasoning: false,
            context_window: 1_000_000,
            max_tokens: 8192,
            cost: CostConfig::default(),
            headers: HashMap::new(),
            compat: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_config_anthropic() {
        let config = ModelConfig::anthropic("claude-sonnet-4-20250514", "Claude Sonnet 4");
        assert_eq!(config.api, ApiProtocol::AnthropicMessages);
        assert_eq!(config.provider, "anthropic");
        assert!(config.compat.is_none());
    }

    #[test]
    fn test_model_config_openai() {
        let config = ModelConfig::openai("gpt-4o", "GPT-4o");
        assert_eq!(config.api, ApiProtocol::OpenAiCompletions);
        let compat = config.compat.unwrap();
        assert!(compat.supports_store);
        assert!(compat.supports_developer_role);
        assert_eq!(compat.max_tokens_field, MaxTokensField::MaxCompletionTokens);
    }

    #[test]
    fn test_openai_compat_variants() {
        let xai = OpenAiCompat::xai();
        assert_eq!(xai.thinking_format, ThinkingFormat::Xai);
        assert!(!xai.supports_store);

        let groq = OpenAiCompat::groq();
        assert!(groq.supports_usage_in_streaming);
        assert!(!groq.supports_store);

        let deepseek = OpenAiCompat::deepseek();
        assert_eq!(
            deepseek.max_tokens_field,
            MaxTokensField::MaxCompletionTokens
        );
    }

    #[test]
    fn test_api_protocol_display() {
        assert_eq!(
            ApiProtocol::AnthropicMessages.to_string(),
            "anthropic_messages"
        );
        assert_eq!(
            ApiProtocol::OpenAiCompletions.to_string(),
            "openai_completions"
        );
        assert_eq!(
            ApiProtocol::GoogleGenerativeAi.to_string(),
            "google_generative_ai"
        );
    }

    #[test]
    fn test_cost_config_default() {
        let cost = CostConfig::default();
        assert_eq!(cost.input_per_million, 0.0);
        assert_eq!(cost.output_per_million, 0.0);
    }
}
