//! Tool-call policy: a yoagent [`ToolMiddleware`] over a rutis `waterfall`.
//!
//! Every tool call dispatches one [`ToolCallEvent`] down the waterfall chain
//! (listeners in registration order, then a terminal that allows). Policies
//! gate **every** tool call of an attached agent — its own tools as well as
//! plugin tools.
//!
//! **Every policy must pass.** Any `Deny` wins, and the call runs only if the
//! chain reached its end:
//!
//! - **allow** — call `next` and return what it returns;
//! - **deny** — return a denial *without* calling `next` (later listeners
//!   never see the call — a rate counter after you is not bumped);
//! - **modify** — [`ToolCallEvent::set_args`], then call `next`: later
//!   listeners see the rewritten arguments, and the tool runs with them.
//!
//! # What the bridge enforces, and for whom
//!
//! Listeners registered through [`PluginCtxExt`](crate::PluginCtxExt) (and
//! [`AgentPlugin`](crate::AgentPlugin)) follow the rules above by
//! construction. A raw `WaterfallListener<ToolCallEvent>` receives the same
//! event and `next`, and could try to break them; the bridge checks what
//! rutis 0.5 lets it observe:
//!
//! - **Allow without `next`** (which, in rutis, skips every later listener):
//!   the chain never reached its terminal, so the call is denied.
//! - **Arguments changed after approval**: the arguments are frozen when the
//!   chain reaches its terminal — the tool runs with exactly what the last
//!   policy approved — and a later [`set_args`](ToolCallEvent::set_args) is
//!   ignored (it returns `false` and logs a warning).
//! - **A `Deny` overridden by an earlier listener** (one that calls `next`,
//!   receives a denial and returns `Allow`): every denial made by a bridge
//!   listener, or built by a raw listener with [`ToolCallEvent::deny`], is
//!   recorded on the event, and a recorded denial wins whatever the chain
//!   returns. **Not covered:** a raw listener that builds its denial as a
//!   plain [`ToolVerdict::deny`] and is overridden by another raw listener —
//!   rutis 0.5 passes verdicts between listeners only as return values, with
//!   no hook in between, so such a denial is invisible to the bridge. Raw
//!   listeners must deny through [`ToolCallEvent::deny`].
//!
//! # Failure modes
//!
//! **Fail closed:** a listener that returns an error or panics, a host that is
//! not running, or a chain that outlives its timeout (default
//! [`DEFAULT_POLICY_TIMEOUT`]) denies the call, with a reason the model sees —
//! matching yoagent's own middleware contract, where a panicking middleware is
//! contained as a denial. A timed-out chain is abandoned where it stands: a
//! listener that counted the call before it awaited has already counted it.
//!
//! **The empty-chain window.** No listener means allow — including *while a
//! policy plugin reloads* (restart, config update, dependency-driven eviction
//! drain the old listener before the new generation registers its own) and
//! *before it first reaches Active*. A host whose safety depends on a policy
//! plugin should call [`RutisBridge::require_policy`](crate::RutisBridge::require_policy):
//! then a call no policy judged is denied. That is a count of
//! [`mark_judged`](ToolCallEvent::mark_judged) calls — self-attestation by the
//! listeners, not enforcement.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use rutis::{BoxFuture, CordisError, Ctx, Event, EventKey, Terminal};
use serde_json::Value;
use yoagent::{ToolCallRequest, ToolDecision, ToolMiddleware};

use crate::host::{run_chain, Outcome};

/// Default bound on one tool call's policy chain (fail closed past it).
pub const DEFAULT_POLICY_TIMEOUT: Duration = Duration::from_secs(60);

const SKIPPED_REST: &str =
    "a plugin policy allowed the call without passing it on to the remaining policies";
const UNJUDGED: &str = "no plugin policy judged this call (none loaded, or a raw listener did \
                        not call `mark_judged`)";

/// One pending tool call, dispatched on the rutis bus as a `waterfall` event.
///
/// Fields are private so they can grow without breaking listeners; read them
/// through the getters.
#[derive(Debug)]
pub struct ToolCallEvent {
    tool_name: String,
    tool_call_id: String,
    original_args: Value,
    args: Mutex<Value>,
    user_request: Option<String>,
    latest_user_text: Option<String>,
    judged: AtomicUsize,
    /// The arguments as the chain's terminal saw them: set once, when the
    /// chain reaches its end — the approval point.
    approved: OnceLock<Value>,
    /// The first denial recorded on the chain, if any.
    denied: OnceLock<String>,
}

impl Event for ToolCallEvent {
    const NAME: &'static str = "yoagent/tool_call";
    type Value = ToolVerdict;
}

impl ToolCallEvent {
    /// Build an event by hand (for testing a policy outside an agent; see
    /// also [`with_user_request`](Self::with_user_request)).
    pub fn new(tool_call_id: impl Into<String>, tool_name: impl Into<String>, args: Value) -> Self {
        Self {
            tool_name: tool_name.into(),
            tool_call_id: tool_call_id.into(),
            original_args: args.clone(),
            args: Mutex::new(args),
            user_request: None,
            latest_user_text: None,
            judged: AtomicUsize::new(0),
            approved: OnceLock::new(),
            denied: OnceLock::new(),
        }
    }

    /// Set what the user asked for (testing).
    pub fn with_user_request(mut self, request: impl Into<String>) -> Self {
        self.user_request = Some(request.into());
        self
    }

    /// Set the text of the user's latest message (testing).
    pub fn with_latest_user_text(mut self, text: impl Into<String>) -> Self {
        self.latest_user_text = Some(text.into());
        self
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
    pub fn args(&self) -> Value {
        self.args.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The arguments the chain received (as the model — or an earlier yoagent
    /// middleware — provided them).
    pub fn original_args(&self) -> &Value {
        &self.original_args
    }

    /// Replace the arguments; later listeners and the tool see the new value.
    ///
    /// Only before approval: once the chain reached its end the arguments are
    /// frozen, and this returns `false` (and logs a warning) without changing
    /// anything.
    pub fn set_args(&self, args: Value) -> bool {
        let mut current = self.args.lock().unwrap_or_else(|e| e.into_inner());
        if self.approved.get().is_some() {
            tracing::warn!(
                tool = %self.tool_name,
                "a plugin policy tried to change the arguments after the call was approved; ignored"
            );
            return false;
        }
        *current = args;
        true
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

    /// Deny the call and record the denial on the event, so no earlier
    /// listener can turn it into an allow. Raw `WaterfallListener`s must deny
    /// through this (see the [module docs](self)); bridge listeners do it for
    /// you.
    pub fn deny(&self, reason: impl Into<String>) -> ToolVerdict {
        let reason = reason.into();
        let _ = self.denied.set(reason.clone());
        ToolVerdict::Deny(reason)
    }

    /// Record that a policy judged this call. Listeners registered through
    /// [`PluginCtxExt`](crate::PluginCtxExt) do this for you; a raw
    /// `WaterfallListener` must call it, or a bridge built with
    /// [`require_policy`](crate::RutisBridge::require_policy) denies the call
    /// as unjudged. It is **self-attestation**: the bridge trusts a listener
    /// that calls it.
    pub fn mark_judged(&self) {
        self.judged.fetch_add(1, Ordering::SeqCst);
    }

    /// How many policies judged this call so far.
    pub fn judged(&self) -> usize {
        self.judged.load(Ordering::SeqCst)
    }

    fn approve(&self) {
        let args = self.args.lock().unwrap_or_else(|e| e.into_inner());
        let _ = self.approved.set(args.clone());
    }
}

/// A listener's answer about a tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ToolVerdict {
    /// Let the call proceed. From a [`ToolPolicy`] or an
    /// [`on_tool_call`](crate::PluginCtxExt::on_tool_call) closure it means
    /// "I have no objection" — the bridge then passes the call on. From a raw
    /// `WaterfallListener` it is only valid as what `next` returned: an
    /// `Allow` that skipped the rest of the chain is treated as a denial.
    Allow,
    /// Block the call; the reason is returned to the model as an error result.
    Deny(String),
}

impl ToolVerdict {
    /// Shorthand for [`ToolVerdict::Deny`]. A raw `WaterfallListener` should
    /// use [`ToolCallEvent::deny`] instead, which also records the denial.
    pub fn deny(reason: impl Into<String>) -> Self {
        Self::Deny(reason.into())
    }
}

/// What [`RutisToolMiddleware::judge`] decided.
#[derive(Debug, Clone, PartialEq)]
pub struct Judgement {
    verdict: ToolVerdict,
    args: Value,
}

impl Judgement {
    /// Allow or deny (with the reason the model sees).
    pub fn verdict(&self) -> &ToolVerdict {
        &self.verdict
    }

    /// The arguments to run with: those approved when the chain reached its
    /// end; on a denial, the arguments as the chain received them.
    pub fn args(&self) -> &Value {
        &self.args
    }

    /// Whether the call may run.
    pub fn is_allowed(&self) -> bool {
        self.verdict == ToolVerdict::Allow
    }
}

/// The waterfall's end: nobody objected. Freezes the arguments.
struct AllowTerminal;

impl Terminal<ToolCallEvent> for AllowTerminal {
    fn call<'a>(
        &'a self,
        _ctx: &'a Ctx,
        e: &'a ToolCallEvent,
    ) -> BoxFuture<'a, Result<ToolVerdict, CordisError>> {
        e.approve();
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
    require_policy: bool,
}

impl RutisToolMiddleware {
    pub(crate) fn new(ctx: Ctx, timeout: Option<Duration>, require_policy: bool) -> Self {
        Self {
            ctx,
            timeout,
            require_policy,
        }
    }

    /// Run the chain for `event` and decide. Errors, panics, timeouts, a
    /// stopped host, a recorded denial, a short-circuited `Allow` and — with
    /// `require_policy` — an unjudged call all come back as denials.
    ///
    /// Takes the event by value: one event is judged once.
    pub async fn judge(&self, event: ToolCallEvent) -> Judgement {
        let key = EventKey::<ToolCallEvent>::of();
        let outcome = run_chain(&self.ctx, self.timeout, || {
            self.ctx
                .events()
                .waterfall(&self.ctx, &key, &event, AllowTerminal)
        })
        .await;
        let verdict = match outcome {
            Outcome::Closed(why) => deny_closed(&event, why.to_string()),
            Outcome::TimedOut(limit) => deny_closed(
                &event,
                format!("the plugin policy did not answer within {limit:?}"),
            ),
            Outcome::Panicked => deny_closed(&event, "a plugin policy panicked".into()),
            Outcome::Finished(Err(error)) => {
                deny_closed(&event, format!("a plugin policy failed: {error}"))
            }
            Outcome::Finished(Ok(verdict)) => self.check_allow(&event, verdict),
        };
        let args = match (&verdict, event.approved.get()) {
            (ToolVerdict::Allow, Some(approved)) => approved.clone(),
            _ => event.original_args.clone(),
        };
        Judgement { verdict, args }
    }

    /// The chain returned `verdict`; apply the checks rutis cannot enforce.
    fn check_allow(&self, event: &ToolCallEvent, verdict: ToolVerdict) -> ToolVerdict {
        if let Some(reason) = event.denied.get() {
            // A recorded denial wins, whatever an earlier listener returned.
            return ToolVerdict::Deny(reason.clone());
        }
        match verdict {
            ToolVerdict::Deny(reason) => ToolVerdict::Deny(reason),
            ToolVerdict::Allow if event.approved.get().is_none() => {
                deny_closed(event, SKIPPED_REST.into())
            }
            ToolVerdict::Allow if self.require_policy && event.judged() == 0 => {
                deny_closed(event, UNJUDGED.into())
            }
            ToolVerdict::Allow => ToolVerdict::Allow,
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
        let judgement = self.judge(ToolCallEvent::from_request(call)).await;
        match judgement.verdict {
            ToolVerdict::Deny(reason) => ToolDecision::Deny(reason),
            ToolVerdict::Allow if &judgement.args == call.args => ToolDecision::Allow,
            ToolVerdict::Allow => ToolDecision::Modify(judgement.args),
        }
    }
}

/// An async tool policy, for listeners that need to await (an approval
/// prompt, a lookup). Register with
/// [`PluginCtxExt::on_tool_call_async`](crate::PluginCtxExt::on_tool_call_async)
/// or [`AgentPlugin::with_tool_policy`](crate::AgentPlugin::with_tool_policy).
///
/// Return `Ok(Allow)` to pass the call on (after optionally
/// [`set_args`](ToolCallEvent::set_args)), `Ok(Deny(..))` to stop it; an
/// `Err` denies it too (fail closed). A policy that waits on a human should
/// run under a bridge whose policy timeout is raised or disabled
/// ([`RutisBridge::with_policy_timeout`](crate::RutisBridge::with_policy_timeout)).
///
/// This trait is open for implementation; any method added to it later will
/// come with a default body.
#[async_trait::async_trait]
pub trait ToolPolicy: Send + Sync + 'static {
    /// Judge one call.
    async fn check(&self, call: &ToolCallEvent) -> Result<ToolVerdict, CordisError>;
}

/// Waterfall listener around a [`ToolPolicy`]: marks the call judged, records
/// a denial on the event and returns without calling `next`, so later
/// policies never see a denied call.
pub(crate) struct PolicyListener<P>(pub(crate) P);

impl<P: ToolPolicy> rutis::WaterfallListener<ToolCallEvent> for PolicyListener<P> {
    fn call<'a>(
        &'a self,
        _ctx: &'a Ctx,
        e: &'a ToolCallEvent,
        next: rutis::Next<'a, ToolCallEvent>,
    ) -> BoxFuture<'a, Result<ToolVerdict, CordisError>> {
        Box::pin(async move {
            let verdict = self.0.check(e).await?;
            e.mark_judged();
            match verdict {
                ToolVerdict::Allow => next.call().await,
                ToolVerdict::Deny(reason) => Ok(e.deny(reason)),
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
