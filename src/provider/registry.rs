//! Provider registry — maps ApiProtocol to StreamProvider implementations.

use super::model::{ApiProtocol, ModelConfig};
use super::traits::*;
use crate::types::*;
use std::collections::HashMap;
use tokio::sync::mpsc;

/// Registry of all available stream providers, keyed by API protocol.
pub struct ProviderRegistry {
    providers: HashMap<ApiProtocol, Box<dyn StreamProvider>>,
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
        self.providers.insert(protocol, Box::new(provider));
    }

    /// Get a provider for a given protocol.
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
