//! Input filtering: a yoagent [`AsyncInputFilter`] over a rutis `serial`.
//!
//! Each prompt dispatches one [`InputEvent`]. Listeners run in registration
//! order; the first that returns `Some(reason)` rejects the prompt (the run
//! ends with `AgentEvent::InputRejected { reason }`), `None` passes it on. No
//! listener means pass. **Fail closed**, like yoagent's own filters: a
//! listener error or panic (rutis's `serial` turns a panic into an error), or
//! a timeout, rejects.

use std::time::Duration;

use rutis::{BoxFuture, CordisError, Ctx, Event, EventKey, Listener};
use yoagent::{AsyncInputFilter, FilterResult};

/// A prompt about to reach the model, dispatched as a `serial` event.
/// The value a listener short-circuits with is the rejection reason.
#[derive(Debug)]
pub struct InputEvent {
    text: String,
}

impl Event for InputEvent {
    const NAME: &'static str = "yoagent/input";
    type Value = String;
}

impl InputEvent {
    /// Build an event by hand.
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }

    /// The prompt's text (every user text block, joined by newlines).
    pub fn text(&self) -> &str {
        &self.text
    }
}

/// yoagent [`AsyncInputFilter`] dispatching an [`InputEvent`] per prompt.
#[derive(Clone)]
pub struct RutisInputFilter {
    ctx: Ctx,
    timeout: Option<Duration>,
}

impl RutisInputFilter {
    /// Dispatch on `ctx`'s bus, rejecting after `timeout`.
    pub fn new(ctx: Ctx, timeout: Option<Duration>) -> Self {
        Self { ctx, timeout }
    }
}

#[async_trait::async_trait]
impl AsyncInputFilter for RutisInputFilter {
    async fn filter(&self, text: &str) -> FilterResult {
        let event = InputEvent::new(text);
        let key = EventKey::<InputEvent>::of();
        let dispatch = self.ctx.events().serial(&self.ctx, &key, &event);
        let outcome = match self.timeout {
            Some(limit) => match tokio::time::timeout(limit, dispatch).await {
                Ok(outcome) => outcome,
                Err(_) => {
                    return reject_closed(format!(
                        "the plugin input filter did not answer within {limit:?}"
                    ))
                }
            },
            None => dispatch.await,
        };
        match outcome {
            Ok(None) => FilterResult::Pass,
            Ok(Some(reason)) => FilterResult::Reject(reason),
            Err(error) => reject_closed(format!("a plugin input filter failed: {error}")),
        }
    }
}

fn reject_closed(why: String) -> FilterResult {
    tracing::warn!(%why, "rejecting the input (fail closed)");
    FilterResult::Reject(format!("input rejected: {why}"))
}

/// Serial listener around a check closure.
pub(crate) struct InputListener<F>(pub(crate) F);

impl<F> Listener<InputEvent> for InputListener<F>
where
    F: Fn(&InputEvent) -> Option<String> + Send + Sync + 'static,
{
    fn call<'a>(
        &'a self,
        _ctx: &'a Ctx,
        e: &'a InputEvent,
    ) -> BoxFuture<'a, Result<Option<String>, CordisError>> {
        let verdict = (self.0)(e);
        Box::pin(async move { Ok(verdict) })
    }
}
