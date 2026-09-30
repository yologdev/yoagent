//! Input filtering: a yoagent [`AsyncInputFilter`] over a rutis `serial`.
//!
//! Each prompt dispatches one [`InputEvent`]. Listeners run in registration
//! order; the first that returns `Some(reason)` rejects the prompt (the run
//! ends with `AgentEvent::InputRejected { reason }`), `None` passes it on. No
//! listener means pass — including while an input-filter plugin reloads or
//! before it first loads: unlike tool policy, input filtering has no
//! `require_*` counterpart. **Fail closed**, like yoagent's own filters: a
//! listener error or panic (rutis's `serial` turns a panic into an error), a
//! host that is not running, or a chain past its timeout (default
//! [`DEFAULT_INPUT_TIMEOUT`]) rejects.

use std::time::Duration;

use rutis::{BoxFuture, CordisError, Ctx, Event, EventKey, Listener};
use yoagent::{AsyncInputFilter, FilterResult};

use crate::host::{run_chain, Outcome};

/// Default bound on one prompt's input-filter chain (fail closed past it).
pub const DEFAULT_INPUT_TIMEOUT: Duration = Duration::from_secs(30);

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
///
/// Built by [`RutisBridge::input_filter`](crate::RutisBridge::input_filter).
#[derive(Clone)]
pub struct RutisInputFilter {
    ctx: Ctx,
    timeout: Option<Duration>,
}

impl RutisInputFilter {
    pub(crate) fn new(ctx: Ctx, timeout: Option<Duration>) -> Self {
        Self { ctx, timeout }
    }
}

#[async_trait::async_trait]
impl AsyncInputFilter for RutisInputFilter {
    async fn filter(&self, text: &str) -> FilterResult {
        let event = InputEvent::new(text);
        let key = EventKey::<InputEvent>::of();
        let outcome = run_chain(&self.ctx, self.timeout, || {
            self.ctx.events().serial(&self.ctx, &key, &event)
        })
        .await;
        match outcome {
            Outcome::Closed(why) => reject_closed(why.to_string()),
            Outcome::TimedOut(limit) => reject_closed(format!(
                "the plugin input filter did not answer within {limit:?}"
            )),
            // rutis's `serial` already turns a listener panic into an error;
            // this arm is the backstop.
            Outcome::Panicked => reject_closed("a plugin input filter panicked".into()),
            Outcome::Finished(Err(error)) => {
                reject_closed(format!("a plugin input filter failed: {error}"))
            }
            Outcome::Finished(Ok(Some(reason))) => FilterResult::Reject(reason),
            // The host can shut down between the liveness check and the
            // dispatch (the chain then ran empty and "passed"): check again.
            Outcome::Finished(Ok(None)) => match crate::host::closed(&self.ctx) {
                Some(why) => reject_closed(why.to_string()),
                None => FilterResult::Pass,
            },
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
