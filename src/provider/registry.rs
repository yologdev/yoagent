//! Provider registry — maps ApiProtocol to StreamProvider implementations.

use super::model::{ApiProtocol, ModelConfig};
use super::traits::*;
use crate::types::*;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Registry of all available stream providers, keyed by API protocol.
pub struct ProviderRegistry {
    providers: HashMap<ApiProtocol, Arc<dyn StreamProvider>>,
}

impl ProviderRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            providers: HashMap::new(),
        }
    }

    /// Register a provider for a given protocol.
    pub fn register(&mut self, protocol: ApiProtocol, provider: impl StreamProvider + 'static) {
        self.providers.insert(protocol, Arc::new(provider));
    }

    /// Resolve the provider for a protocol as a shared handle.
    ///
    /// Unlike [`get`](Self::get), this hands out an owned `Arc` that can be
    /// stored (e.g. by [`Agent::from_config`](crate::Agent::from_config)),
    /// so callers don't have to borrow the registry for the provider's
    /// lifetime.
    pub fn resolve(&self, protocol: &ApiProtocol) -> Option<Arc<dyn StreamProvider>> {
        self.providers.get(protocol).cloned()
    }

    /// Borrow the provider for a protocol for a transient call. Prefer
    /// [`resolve`](Self::resolve) when you need to keep the handle.
    pub fn get(&self, protocol: &ApiProtocol) -> Option<&dyn StreamProvider> {
        self.providers.get(protocol).map(|p| p.as_ref())
    }

    /// Check if a protocol is registered.
    pub fn has(&self, protocol: &ApiProtocol) -> bool {
        self.providers.contains_key(protocol)
    }

    /// List all registered protocols.
    pub fn protocols(&self) -> Vec<ApiProtocol> {
        self.providers.keys().copied().collect()
    }

    /// Stream using the appropriate provider for the model's API protocol.
    pub async fn stream(
        &self,
        model: &ModelConfig,
        config: StreamConfig,
        tx: mpsc::UnboundedSender<StreamEvent>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Message, ProviderError> {
        let provider = self.providers.get(&model.api).ok_or_else(|| {
            ProviderError::Other(format!(
                "No provider registered for protocol: {}",
                model.api
            ))
        })?;

        provider.stream(config, tx, cancel).await
    }
}

impl Default for ProviderRegistry {
    /// Create a registry with all built-in providers registered.
    fn default() -> Self {
        use crate::provider::{
            AnthropicProvider, AzureOpenAiProvider, BedrockProvider, GoogleProvider,
            GoogleVertexProvider, OpenAiCompatProvider, OpenAiResponsesProvider,
        };

        let mut registry = Self::new();
        registry.register(ApiProtocol::AnthropicMessages, AnthropicProvider);
        registry.register(ApiProtocol::OpenAiCompletions, OpenAiCompatProvider);
        registry.register(ApiProtocol::OpenAiResponses, OpenAiResponsesProvider);
        registry.register(ApiProtocol::GoogleGenerativeAi, GoogleProvider);
        registry.register(ApiProtocol::GoogleVertex, GoogleVertexProvider);
        registry.register(ApiProtocol::BedrockConverseStream, BedrockProvider);
        registry.register(ApiProtocol::AzureOpenAiResponses, AzureOpenAiProvider);

        registry
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_registry_has_all_providers() {
        let registry = ProviderRegistry::default();

        assert!(registry.has(&ApiProtocol::AnthropicMessages));
        assert!(registry.has(&ApiProtocol::OpenAiCompletions));
        assert!(registry.has(&ApiProtocol::OpenAiResponses));
        assert!(registry.has(&ApiProtocol::GoogleGenerativeAi));
        assert!(registry.has(&ApiProtocol::GoogleVertex));
        assert!(registry.has(&ApiProtocol::BedrockConverseStream));
        assert!(registry.has(&ApiProtocol::AzureOpenAiResponses));
    }

    #[test]
    fn test_registry_protocols() {
        let registry = ProviderRegistry::default();
        let protocols = registry.protocols();
        assert_eq!(protocols.len(), 7);
    }

    #[test]
    fn test_custom_registry() {
        let mut registry = ProviderRegistry::new();
        assert!(!registry.has(&ApiProtocol::AnthropicMessages));

        registry.register(
            ApiProtocol::AnthropicMessages,
            crate::provider::AnthropicProvider,
        );
        assert!(registry.has(&ApiProtocol::AnthropicMessages));
    }
}

/// Resolve an API key from the conventional environment variable(s) for a
/// provider name (the `ModelConfig.provider` string).
///
/// Used as a fallback when no explicit key is set on the agent — an explicit
/// `with_api_key` always wins. Conventions:
///
/// | provider | env var(s), first match wins |
/// |---|---|
/// | `anthropic` | `ANTHROPIC_API_KEY` |
/// | `openai` | `OPENAI_API_KEY` |
/// | `google` | `GEMINI_API_KEY`, `GOOGLE_API_KEY` |
/// | `xai` / `groq` / `deepseek` / `mistral` / `zai` / `minimax` / `openrouter` / `cerebras` | `<PROVIDER>_API_KEY` |
/// | `qwen` | `DASHSCOPE_API_KEY` |
/// | `meta` | `META_API_KEY`, then `MODEL_API_KEY` |
/// | `opencode-zen` / `opencode-go` | `OPENCODE_API_KEY` |
/// | `azure` | `AZURE_OPENAI_API_KEY` |
/// | `bedrock` | `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY` (+ `AWS_SESSION_TOKEN`), composed as `access:secret[:token]` |
/// | `vertex` | none — pass a short-lived OAuth token via `with_api_key` |
/// | `local` / `ollama` | no key needed (empty) |
/// | anything else | `YOAGENT_API_KEY`, then `API_KEY` |
pub fn resolve_api_key(provider: &str) -> Option<String> {
    use std::env::var;
    let first = |names: &[&str]| {
        names.iter().find_map(|n| {
            var(n).ok().inspect(|_| {
                tracing::debug!("resolved API key for provider '{}' from ${}", provider, n)
            })
        })
    };
    match provider {
        "anthropic" => first(&["ANTHROPIC_API_KEY"]),
        "openai" => first(&["OPENAI_API_KEY"]),
        "google" => first(&["GEMINI_API_KEY", "GOOGLE_API_KEY"]),
        "xai" => first(&["XAI_API_KEY"]),
        "groq" => first(&["GROQ_API_KEY"]),
        "deepseek" => first(&["DEEPSEEK_API_KEY"]),
        "mistral" => first(&["MISTRAL_API_KEY"]),
        "zai" => first(&["ZAI_API_KEY"]),
        "minimax" => first(&["MINIMAX_API_KEY"]),
        // Meta's docs name the generic MODEL_API_KEY; prefer the unambiguous
        // META_API_KEY, accept the official one.
        "meta" => first(&["META_API_KEY", "MODEL_API_KEY"]),
        "openrouter" => first(&["OPENROUTER_API_KEY"]),
        "cerebras" => first(&["CEREBRAS_API_KEY"]),
        "qwen" => first(&["DASHSCOPE_API_KEY"]),
        "opencode-zen" | "opencode-go" => first(&["OPENCODE_API_KEY"]),
        "azure" => first(&["AZURE_OPENAI_API_KEY"]),
        "bedrock" => {
            let access = var("AWS_ACCESS_KEY_ID").ok()?;
            let secret = var("AWS_SECRET_ACCESS_KEY").ok()?;
            Some(match var("AWS_SESSION_TOKEN") {
                Ok(token) => format!("{}:{}:{}", access, secret, token),
                Err(_) => format!("{}:{}", access, secret),
            })
        }
        "vertex" => None,
        "local" | "ollama" => Some(String::new()),
        _ => first(&["YOAGENT_API_KEY", "API_KEY"]),
    }
}

/// Like [`resolve_api_key`], but warns (via `tracing`) when resolution fails
/// for a provider that needs a key, then falls back to an empty string so
/// the provider returns a clear authentication error instead of the failure
/// being invisible until the first request.
pub(crate) fn resolve_api_key_or_warn(provider: &str) -> String {
    match resolve_api_key(provider) {
        Some(key) => key,
        None => {
            tracing::warn!(
                "no API key found for provider '{}': {}; requests will fail \
                 with an authentication error",
                provider,
                api_key_env_hint(provider)
            );
            String::new()
        }
    }
}

/// Remedy fragment for the missing-key warning, matching the
/// [`resolve_api_key`] table.
fn api_key_env_hint(provider: &str) -> &'static str {
    match provider {
        "anthropic" => "set ANTHROPIC_API_KEY or call .with_api_key(...)",
        "openai" => "set OPENAI_API_KEY or call .with_api_key(...)",
        "google" => "set GEMINI_API_KEY (or GOOGLE_API_KEY) or call .with_api_key(...)",
        "xai" => "set XAI_API_KEY or call .with_api_key(...)",
        "groq" => "set GROQ_API_KEY or call .with_api_key(...)",
        "deepseek" => "set DEEPSEEK_API_KEY or call .with_api_key(...)",
        "mistral" => "set MISTRAL_API_KEY or call .with_api_key(...)",
        "zai" => "set ZAI_API_KEY or call .with_api_key(...)",
        "minimax" => "set MINIMAX_API_KEY or call .with_api_key(...)",
        "meta" => "set META_API_KEY (or MODEL_API_KEY) or call .with_api_key(...)",
        "openrouter" => "set OPENROUTER_API_KEY or call .with_api_key(...)",
        "cerebras" => "set CEREBRAS_API_KEY or call .with_api_key(...)",
        "qwen" => "set DASHSCOPE_API_KEY or call .with_api_key(...)",
        "opencode-zen" | "opencode-go" => "set OPENCODE_API_KEY or call .with_api_key(...)",
        "azure" => "set AZURE_OPENAI_API_KEY or call .with_api_key(...)",
        "bedrock" => {
            "set AWS_ACCESS_KEY_ID + AWS_SECRET_ACCESS_KEY (+ AWS_SESSION_TOKEN) \
             or call .with_api_key(\"access:secret[:token]\")"
        }
        "vertex" => "pass a short-lived OAuth token via .with_api_key(...)",
        _ => "set YOAGENT_API_KEY (or API_KEY) or call .with_api_key(...)",
    }
}

#[cfg(test)]
mod resolve_key_tests {
    use super::resolve_api_key;

    #[test]
    fn test_deterministic_branches() {
        // local/ollama need no key
        assert_eq!(resolve_api_key("local").as_deref(), Some(""));
        assert_eq!(resolve_api_key("ollama").as_deref(), Some(""));
        // vertex expects an explicit OAuth token — never env-resolved
        assert_eq!(resolve_api_key("vertex"), None);
    }

    #[test]
    fn test_meta_env_resolution() {
        // Own vars, no other test touches them.
        std::env::set_var("MODEL_API_KEY", "official-var");
        assert_eq!(resolve_api_key("meta").as_deref(), Some("official-var"));
        // The unambiguous var wins when both are set.
        std::env::set_var("META_API_KEY", "preferred-var");
        assert_eq!(resolve_api_key("meta").as_deref(), Some("preferred-var"));
        std::env::remove_var("META_API_KEY");
        std::env::remove_var("MODEL_API_KEY");
    }

    #[test]
    fn test_env_resolution() {
        // Use a provider name unique to this test to avoid env races with
        // parallel tests.
        std::env::set_var("YOAGENT_API_KEY", "from-generic-fallback");
        assert_eq!(
            resolve_api_key("some-unknown-gateway").as_deref(),
            Some("from-generic-fallback")
        );
        std::env::remove_var("YOAGENT_API_KEY");
    }
}

#[cfg(test)]
mod registry_tests {
    use super::*;

    /// Every `ApiProtocol` variant. The wildcard-free `match` is a compile
    /// forcing-function: adding a variant fails to build here until it's added
    /// to the list (and, in turn, registered in `default()` below), so the
    /// "default registry is complete" invariant behind `from_config`'s unwrap
    /// cannot silently rot.
    fn all_protocols() -> Vec<ApiProtocol> {
        fn _assert_exhaustive(p: ApiProtocol) {
            match p {
                ApiProtocol::AnthropicMessages
                | ApiProtocol::OpenAiCompletions
                | ApiProtocol::OpenAiResponses
                | ApiProtocol::AzureOpenAiResponses
                | ApiProtocol::GoogleGenerativeAi
                | ApiProtocol::GoogleVertex
                | ApiProtocol::BedrockConverseStream => {}
            }
        }
        vec![
            ApiProtocol::AnthropicMessages,
            ApiProtocol::OpenAiCompletions,
            ApiProtocol::OpenAiResponses,
            ApiProtocol::AzureOpenAiResponses,
            ApiProtocol::GoogleGenerativeAi,
            ApiProtocol::GoogleVertex,
            ApiProtocol::BedrockConverseStream,
        ]
    }

    /// The default registry must resolve a provider for every protocol, so
    /// `Agent::from_config` (which unwraps against it) can never panic.
    #[test]
    fn default_registry_covers_all_protocols() {
        let registry = ProviderRegistry::default();
        for api in all_protocols() {
            assert!(
                registry.resolve(&api).is_some(),
                "default registry missing a provider for {api}"
            );
        }
    }

    /// Each protocol must resolve to a provider that actually *speaks* that
    /// protocol — guards against a mis-wired `register()` line (e.g. mapping
    /// `GoogleVertex` to the Anthropic provider) that a mere is-some check
    /// would miss.
    #[test]
    fn default_registry_maps_each_protocol_to_its_own_provider() {
        let registry = ProviderRegistry::default();
        for api in all_protocols() {
            let provider = registry.resolve(&api).expect("registered");
            assert_eq!(
                provider.protocol(),
                Some(api),
                "protocol {api} resolved to a provider that reports {:?}",
                provider.protocol()
            );
        }
    }

    #[test]
    fn empty_registry_resolves_nothing() {
        assert!(ProviderRegistry::new()
            .resolve(&ApiProtocol::AnthropicMessages)
            .is_none());
    }
}
