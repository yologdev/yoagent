//! Agent events out: the [`AgentEvent`] stream forwarded onto the rutis bus.
//!
//! Each event is published with rutis `emit` as an [`AgentEventEmitted`] —
//! fire-and-forget. `emit` only queues a dispatch and returns, so a slow or
//! failing listener never slows the agent (listener errors and panics go to
//! the rutis `ErrorSink`).
//!
//! **Ordering.** rutis dispatches emits of one event key in emit order, one
//! listener after another in registration order, so every listener sees each
//! agent's events in the order that agent produced them. Events of several
//! agents on one bridge interleave; tell them apart by
//! [`AgentEventEmitted::label`] (see [`event_sender_labeled`]).
//!
//! **Cost.** Every published event that has at least one listener spawns one
//! tokio task, and streaming produces one `MessageUpdate` event per text
//! delta — publishing is cheap per event, not free per run. Dispatches of
//! the event key form **one queue shared by every agent on the bus**: a slow
//! listener delays delivery of *all* agents' later events (never the agents
//! themselves), and a listener permanently slower than the agents builds an
//! unbounded backlog.
//!
//! **After the host shut down** events are dropped (logged at debug once per
//! sender): rutis would accept the emit and deliver it to no one.

use std::sync::Arc;

use rutis::{Ctx, Event, EventKey};
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
    /// Wrap an event for publishing, without a label.
    pub fn new(event: AgentEvent) -> Self {
        Self { label: None, event }
    }

    /// Wrap an event for publishing, labelled with the agent (or run) it
    /// came from.
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

    /// The label the sender attached, if any.
    pub fn label(&self) -> Option<&str> {
        self.label.as_deref()
    }
}

/// Publish one event on `ctx`'s bus (fire-and-forget; never blocks).
///
/// Returns `false` when it was dropped because the host shut down (or rutis
/// refused it).
pub fn emit_agent_event(ctx: &Ctx, event: AgentEventEmitted) -> bool {
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

/// A sender to hand to `Agent::prompt_with_sender` (and the other
/// `*_with_sender` methods) that publishes every event on the bus and, when
/// `forward` is given, also passes it on — **before** publishing, so your own
/// consumer is never behind the bus.
///
/// The returned task ends when every clone of the sender is dropped (the
/// agent drops its copy when the run ends); await it to know the last event
/// was handed over. A closed `forward` receiver stops forwarding (logged at
/// debug once) but not publishing.
pub fn event_sender(
    ctx: &Ctx,
    forward: Option<mpsc::UnboundedSender<AgentEvent>>,
) -> (mpsc::UnboundedSender<AgentEvent>, JoinHandle<()>) {
    spawn_forwarder(ctx, None, forward)
}

/// [`event_sender`] whose events carry `label` ([`AgentEventEmitted::label`]),
/// so listeners can tell several agents on one bridge apart.
pub fn event_sender_labeled(
    ctx: &Ctx,
    label: impl Into<Arc<str>>,
    forward: Option<mpsc::UnboundedSender<AgentEvent>>,
) -> (mpsc::UnboundedSender<AgentEvent>, JoinHandle<()>) {
    spawn_forwarder(ctx, Some(label.into()), forward)
}

fn spawn_forwarder(
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
                tracing::debug!("the plugin host has shut down; dropping agent events");
                dropped_logged = true;
            }
        }
    });
    (tx, task)
}
