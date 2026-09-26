//! [`TurnHookProvider`]: runs [`TurnHook`]s before each request.

use super::traits::{ProviderError, StreamConfig, StreamEvent, StreamProvider};
use crate::types::{Message, TurnContext, TurnHook};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Wraps a provider so every request first runs the given [`TurnHook`]s and
/// appends the lines they return to that request's system prompt.
///
/// This is how turn hooks reach the loop without a new
/// [`AgentLoopConfig`](crate::agent_loop::AgentLoopConfig) field:
/// [`Agent::with_turn_hook`](crate::Agent::with_turn_hook) wraps its provider
/// for each run, and a raw-loop caller wraps theirs:
///
/// ```ignore
/// let provider = Arc::new(TurnHookProvider::new(provider, vec![Arc::new(my_hook)]));
/// let config = AgentLoopConfig { provider, /* ... */ };
/// ```
///
/// With no hooks, or hooks that all return `None`, the request reaches the
/// inner provider unchanged.
pub struct TurnHookProvider {
    inner: Arc<dyn StreamProvider>,
    hooks: Vec<Arc<dyn TurnHook>>,
}

impl TurnHookProvider {
    pub fn new(inner: Arc<dyn StreamProvider>, hooks: Vec<Arc<dyn TurnHook>>) -> Self {
        Self { inner, hooks }
    }

    /// Run the hooks against `config` and return the lines they added.
    async fn lines(&self, config: &StreamConfig) -> Vec<String> {
        let turn = TurnContext {
            system_prompt: &config.system_prompt,
            messages: &config.messages,
            tools: &config.tools,
            model: &config.model,
        };
        let mut lines = Vec::new();
        for hook in &self.hooks {
            use futures::FutureExt;
            match std::panic::AssertUnwindSafe(hook.before_turn(&turn))
                .catch_unwind()
                .await
            {
                Ok(Some(line)) if !line.trim().is_empty() => lines.push(line),
                Ok(_) => {}
                Err(_) => tracing::warn!("turn hook panicked; adding nothing for this turn"),
            }
        }
        lines
    }
}

#[async_trait::async_trait]
impl StreamProvider for TurnHookProvider {
    async fn stream(
        &self,
        mut config: StreamConfig,
        tx: mpsc::UnboundedSender<StreamEvent>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Message, ProviderError> {
        let lines = self.lines(&config).await;
        if !lines.is_empty() {
            let extra = lines.join("\n");
            config.system_prompt = if config.system_prompt.is_empty() {
                extra
            } else {
                format!("{}\n\n{extra}", config.system_prompt)
            };
        }
        self.inner.stream(config, tx, cancel).await
    }

    fn protocol(&self) -> Option<super::ApiProtocol> {
        self.inner.protocol()
    }
}
