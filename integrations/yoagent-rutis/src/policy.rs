//! Tool-call policy: a yoagent [`ToolMiddleware`] over a rutis `waterfall`.
//!
//! Every tool call dispatches one [`ToolCallEvent`] down the waterfall chain
//! (listeners in registration order, then a terminal that allows). A listener
//! may:
//!
//! - **allow** — call `next` and return what it returns;
//! - **deny** — return [`ToolVerdict::Deny`] without calling `next` (the rest
//!   of the chain never sees the call);
//! - **modify** — [`ToolCallEvent::set_args`], then call `next`: later
//!   listeners see the rewritten arguments, and the tool runs with them.
//!
//! No listener means allow. **Fail closed:** a listener that returns an error
//! or panics, a dispatch rutis refuses (e.g. the root was shut down), or a
//! chain that outlives the optional timeout denies the call, with a reason
//! the model sees. This matches yoagent's own middleware contract, where a
//! panicking middleware is contained as a denial.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::FutureExt;
use rutis::{BoxFuture, CordisError, Ctx, Event, EventKey, Terminal};
use yoagent::{ToolCallRequest, ToolDecision, ToolMiddleware};

/// One pending tool call, dispatched on the rutis bus as a `waterfall` event.
///
/// Fields are private so they can grow without breaking listeners; read them
/// through the getters.
#[derive(Debug)]
pub struct ToolCallEvent {
    tool_name: String,
    tool_call_id: String,
    original_args: serde_json::Value,
    args: Mutex<serde_json::Value>,
    user_request: Option<String>,
    latest_user_text: Option<String>,
}

impl Event for ToolCallEvent {
    const NAME: &'static str = "yoagent/tool_call";
    type Value = ToolVerdict;
}

impl ToolCallEvent {
    /// Build an event by hand (for testing a policy outside an agent).
    pub fn new(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        args: serde_json::Value,
    ) -> Self {
        Self {
            tool_name: tool_name.into(),
            tool_call_id: tool_call_id.into(),
            original_args: args.clone(),
            args: Mutex::new(args),
            user_request: None,
            latest_user_text: None,
        }
    }

    fn from_request(call: &ToolCallRequest<'_>) -> Self {
        let mut event = Self::new(call.tool_call_id, call.tool_name, call.args.clone());
        event.user_request = call.user_request();
        event.latest_user_text = call.latest_user_text();
        event
    }

    /// Name of the tool the model wants to run.
    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    /// Provider-assigned id of this call.
    pub fn tool_call_id(&self) -> &str {
        &self.tool_call_id
    }

    /// The arguments as they stand now (rewritten by earlier listeners, if
    /// any did).
    pub fn args(&self) -> serde_json::Value {
        self.args.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The arguments the chain received (as the model — or an earlier yoagent
    /// middleware — provided them).
    pub fn original_args(&self) -> &serde_json::Value {
        &self.original_args
    }

    /// Replace the arguments. Later listeners and the tool see the new value.
    pub fn set_args(&self, args: serde_json::Value) {
        *self.args.lock().unwrap_or_else(|e| e.into_inner()) = args;
    }

    /// What the user asked for; see yoagent's
    /// [`ToolCallRequest::user_request`] (the prose is not a stable format).
    pub fn user_request(&self) -> Option<&str> {
        self.user_request.as_deref()
    }

    /// The text of the user's latest message; see
    /// [`ToolCallRequest::latest_user_text`].
    pub fn latest_user_text(&self) -> Option<&str> {
        self.latest_user_text.as_deref()
    }
}

/// A listener's answer about a tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolVerdict {
    /// Let the call proceed (with the arguments as they stand).
    Allow,
    /// Block the call; the reason is returned to the model as an error result.
    Deny(String),
}

impl ToolVerdict {
    /// Shorthand for [`ToolVerdict::Deny`].
    pub fn deny(reason: impl Into<String>) -> Self {
        Self::Deny(reason.into())
    }
}

/// The waterfall's end: nobody objected.
pub(crate) struct AllowTerminal;

impl Terminal<ToolCallEvent> for AllowTerminal {
    fn call<'a>(
        &'a self,
        _ctx: &'a Ctx,
        _e: &'a ToolCallEvent,
    ) -> BoxFuture<'a, Result<ToolVerdict, CordisError>> {
        Box::pin(async { Ok(ToolVerdict::Allow) })
    }
}

/// yoagent [`ToolMiddleware`] dispatching each call as a [`ToolCallEvent`].
///
/// Built by [`RutisBridge::tool_middleware`](crate::RutisBridge::tool_middleware).
#[derive(Clone)]
pub struct RutisToolMiddleware {
    ctx: Ctx,
    timeout: Option<Duration>,
}

impl RutisToolMiddleware {
    /// Dispatch on `ctx`'s bus; deny any call whose chain takes longer than
    /// `timeout` (`None`: wait as long as it takes).
    pub fn new(ctx: Ctx, timeout: Option<Duration>) -> Self {
        Self { ctx, timeout }
    }

    /// Run the chain for `event` and return the raw verdict (errors, panics
    /// and timeouts already turned into denials).
    pub async fn judge(&self, event: &ToolCallEvent) -> ToolVerdict {
        let key = EventKey::<ToolCallEvent>::of();
        let chain = self
            .ctx
            .events()
            .waterfall(&self.ctx, &key, event, AllowTerminal);
        // rutis propagates a waterfall listener's panic to the dispatcher:
        // contain it here so the reason names the plugin chain.
        let guarded = std::panic::AssertUnwindSafe(chain).catch_unwind();
        let outcome = match self.timeout {
            Some(limit) => match tokio::time::timeout(limit, guarded).await {
                Ok(outcome) => outcome,
                Err(_) => {
                    return deny_closed(
                        event,
                        format!("the plugin policy did not answer within {limit:?}"),
                    )
                }
            },
            None => guarded.await,
        };
        match outcome {
            Ok(Ok(verdict)) => verdict,
            Ok(Err(error)) => deny_closed(event, format!("a plugin policy failed: {error}")),
            Err(_) => deny_closed(event, "a plugin policy panicked".into()),
        }
    }
}

fn deny_closed(event: &ToolCallEvent, why: String) -> ToolVerdict {
    tracing::warn!(tool = event.tool_name(), %why, "denying the tool call (fail closed)");
    ToolVerdict::Deny(why)
}

#[async_trait::async_trait]
impl ToolMiddleware for RutisToolMiddleware {
    async fn before_tool(&self, call: &ToolCallRequest<'_>) -> ToolDecision {
        let event = ToolCallEvent::from_request(call);
        match self.judge(&event).await {
            ToolVerdict::Deny(reason) => ToolDecision::Deny(reason),
            ToolVerdict::Allow => {
                let args = event.args();
                if &args == event.original_args() {
                    ToolDecision::Allow
                } else {
                    ToolDecision::Modify(args)
                }
            }
        }
    }
}

/// An async tool policy, for listeners that need to await (an approval
/// prompt, a lookup). Register with
/// [`PluginCtxExt::on_tool_call_async`](crate::PluginCtxExt::on_tool_call_async).
///
/// Return `Ok(Allow)` to pass the call on (after optionally
/// [`set_args`](ToolCallEvent::set_args)), `Ok(Deny(..))` to stop it; an
/// `Err` denies it too (fail closed).
#[async_trait::async_trait]
pub trait ToolPolicy: Send + Sync + 'static {
    /// Judge one call.
    async fn check(&self, call: &ToolCallEvent) -> Result<ToolVerdict, CordisError>;
}

/// Waterfall listener around a [`ToolPolicy`].
pub(crate) struct PolicyListener<P>(pub(crate) Arc<P>);

impl<P: ToolPolicy> rutis::WaterfallListener<ToolCallEvent> for PolicyListener<P> {
    fn call<'a>(
        &'a self,
        _ctx: &'a Ctx,
        e: &'a ToolCallEvent,
        next: rutis::Next<'a, ToolCallEvent>,
    ) -> BoxFuture<'a, Result<ToolVerdict, CordisError>> {
        Box::pin(async move {
            match self.0.check(e).await? {
                ToolVerdict::Allow => next.call().await,
                deny => Ok(deny),
            }
        })
    }
}

/// A synchronous closure policy.
pub(crate) struct FnPolicy<F>(pub(crate) F);

#[async_trait::async_trait]
impl<F> ToolPolicy for FnPolicy<F>
where
    F: Fn(&ToolCallEvent) -> ToolVerdict + Send + Sync + 'static,
{
    async fn check(&self, call: &ToolCallEvent) -> Result<ToolVerdict, CordisError> {
        Ok((self.0)(call))
    }
}
