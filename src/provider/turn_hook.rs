//! [`TurnHookProvider`]: runs [`TurnHook`]s before each request.

use super::traits::{ProviderError, StreamConfig, StreamEvent, StreamProvider};
use crate::types::{Content, Message, TurnContext, TurnHook};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Wraps a provider so every request first runs the given [`TurnHook`]s and
/// appends the notes they return to that request's latest user turn.
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
/// The system prompt and every earlier message are never touched. With no
/// hooks, hooks that all return `None`, or a request with no user message,
/// the request reaches the inner provider unchanged.
pub struct TurnHookProvider {
    inner: Arc<dyn StreamProvider>,
    hooks: Vec<Arc<dyn TurnHook>>,
}

impl TurnHookProvider {
    /// Wrap `inner` so `hooks` run, in order, before each of its requests.
    pub fn new(inner: Arc<dyn StreamProvider>, hooks: Vec<Arc<dyn TurnHook>>) -> Self {
        Self { inner, hooks }
    }

    /// Run the hooks against `config` and return the notes they added.
    async fn notes(&self, config: &StreamConfig) -> Vec<String> {
        let prompts = crate::agent_loop::run_prompts();
        let turn = TurnContext::new(
            &config.system_prompt,
            &config.messages,
            &config.tools,
            &config.model,
        )
        .with_run_prompts(&prompts);
        let mut notes = Vec::new();
        for hook in &self.hooks {
            use futures::FutureExt;
            match std::panic::AssertUnwindSafe(hook.before_turn(&turn))
                .catch_unwind()
                .await
            {
                Ok(Some(note)) if !note.trim().is_empty() => notes.push(note),
                Ok(_) => {}
                Err(_) => tracing::warn!("turn hook panicked; adding nothing for this turn"),
            }
        }
        notes
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl StreamProvider for TurnHookProvider {
    async fn stream(
        &self,
        mut config: StreamConfig,
        tx: mpsc::UnboundedSender<StreamEvent>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Message, ProviderError> {
        let notes = self.notes(&config).await;
        if !notes.is_empty() {
            let latest_user = config.messages.iter_mut().rev().find_map(|m| match m {
                Message::User { content, .. } => Some(content),
                _ => None,
            });
            match latest_user {
                Some(content) => content.push(Content::Text {
                    text: notes.join("\n"),
                }),
                None => tracing::debug!("turn hook note dropped: the request has no user turn"),
            }
        }
        self.inner.stream(config, tx, cancel).await
    }

    fn protocol(&self) -> Option<super::ApiProtocol> {
        self.inner.protocol()
    }
}
