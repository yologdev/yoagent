//! Agent events out: the [`AgentEvent`] stream forwarded onto the rutis bus.
//!
//! Hand the sender from [`RutisBridge::event_sender`](crate::RutisBridge::event_sender)
//! (or [`event_sender_labeled`](crate::RutisBridge::event_sender_labeled)) to
//! a yoagent `*_with_sender` call. Each event is published with rutis `emit`
//! as an [`AgentEventEmitted`] — fire-and-forget. `emit` only queues a
//! dispatch and returns, so a slow or failing listener never slows the agent
//! (listener errors and panics go to the rutis `ErrorSink`).
//!
//! **Ordering.** rutis dispatches emits of one event key in emit order, one
//! listener after another in registration order, so every listener sees each
//! agent's events in the order that agent produced them. Events of several
//! agents on one bridge interleave; tell them apart by
//! [`AgentEventEmitted::label`].
//!
//! **Cost.** Every published event that has at least one listener spawns one
//! tokio task, and streaming produces one `MessageUpdate` event per text
//! delta — publishing is cheap per event, not free per run. Dispatches of
//! the event key form **one queue shared by every agent on the bus**: a slow
//! listener delays delivery of *all* agents' later events (never the agents
//! themselves), and a listener permanently slower than the agents builds an
//! unbounded backlog.
//!
//! **While the host is not running** events are dropped (logged at debug once
//! per event sender): rutis would accept the emit and deliver it to no one.

use std::sync::Arc;

use rutis::{BoxFuture, CordisError, Ctx, Event, EventKey, Listener};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use yoagent::AgentEvent;

/// One [`AgentEvent`], as published on the rutis bus.
#[derive(Debug, Clone)]
pub struct AgentEventEmitted {
    label: Option<Arc<str>>,
    event: AgentEvent,
}

impl Event for AgentEventEmitted {
    const NAME: &'static str = "yoagent/agent_event";
    type Value = ();
}

impl AgentEventEmitted {
    /// Wrap an event, without a label (for testing an observer).
    pub fn new(event: AgentEvent) -> Self {
        Self { label: None, event }
    }

    /// Wrap an event with a label (for testing an observer).
    pub fn labeled(label: impl Into<Arc<str>>, event: AgentEvent) -> Self {
        Self {
            label: Some(label.into()),
            event,
        }
    }

    /// The agent's event.
    pub fn event(&self) -> &AgentEvent {
        &self.event
    }

    /// The label the event sender attached, if any. Informational — whatever
    /// string the host chose; nothing checks it is unique, so it is not an
    /// identity.
    pub fn label(&self) -> Option<&str> {
        self.label.as_deref()
    }
}

/// Publish one event on `ctx`'s bus (fire-and-forget; never blocks).
/// Returns `false` when it was dropped (host not running, or refused).
fn emit_agent_event(ctx: &Ctx, event: AgentEventEmitted) -> bool {
    if crate::host::closed(ctx).is_some() {
        return false;
    }
    let key = EventKey::<AgentEventEmitted>::of();
    match ctx.events().emit(ctx, &key, Arc::new(event)) {
        Ok(()) => true,
        Err(error) => {
            tracing::debug!(%error, "agent event not published on the rutis bus");
            false
        }
    }
}

/// The forwarder behind [`RutisBridge::event_sender`](crate::RutisBridge::event_sender):
/// passes each event to `forward` first (a closed `forward` stops forwarding,
/// logged once, not publishing), then publishes it.
pub(crate) fn spawn_forwarder(
    ctx: &Ctx,
    label: Option<Arc<str>>,
    forward: Option<mpsc::UnboundedSender<AgentEvent>>,
) -> (mpsc::UnboundedSender<AgentEvent>, JoinHandle<()>) {
    let (tx, mut rx) = mpsc::unbounded_channel::<AgentEvent>();
    let handle = ctx.handle().clone();
    let ctx = ctx.clone();
    let task = handle.spawn(async move {
        let mut forward = forward;
        let mut dropped_logged = false;
        while let Some(event) = rx.recv().await {
            if let Some(out) = &forward {
                if out.send(event.clone()).is_err() {
                    tracing::debug!("forward receiver closed; still publishing on the bus");
                    forward = None;
                }
            }
            let wrapped = AgentEventEmitted {
                label: label.clone(),
                event,
            };
            if !emit_agent_event(&ctx, wrapped) && !dropped_logged {
                tracing::debug!("the plugin host is not running; dropping agent events");
                dropped_logged = true;
            }
        }
    });
    (tx, task)
}

/// Bus listener around an observer closure
/// ([`PluginCtxExt::on_agent_event`](crate::PluginCtxExt::on_agent_event)).
pub(crate) struct ObserverListener<F>(pub(crate) F);

impl<F> Listener<AgentEventEmitted> for ObserverListener<F>
where
    F: Fn(&AgentEventEmitted) + Send + Sync + 'static,
{
    fn call<'a>(
        &'a self,
        _ctx: &'a Ctx,
        e: &'a AgentEventEmitted,
    ) -> BoxFuture<'a, Result<Option<()>, CordisError>> {
        Box::pin(async move {
            (self.0)(e);
            Ok(None)
        })
    }
}
