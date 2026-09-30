//! Agent events out: the [`AgentEvent`] stream forwarded onto the rutis bus.
//!
//! Each event is published with rutis `emit` as an [`AgentEventEmitted`] —
//! fire-and-forget. `emit` only queues a dispatch task and returns, so a slow
//! or failing listener never slows the agent (listener errors and panics go
//! to the rutis `ErrorSink`).
//!
//! **Ordering.** rutis dispatches emits of one event key in emit order, one
//! listener after another in registration order, so every listener sees the
//! agent's events in the order the agent produced them. A slow listener
//! delays later *dispatches* (they queue behind it), not the agent. The
//! queue is unbounded: a listener that is permanently slower than the agent
//! accumulates a backlog.

use std::sync::Arc;

use rutis::{Ctx, Event, EventKey};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use yoagent::AgentEvent;

/// One [`AgentEvent`], as published on the rutis bus.
#[derive(Debug, Clone)]
pub struct AgentEventEmitted {
    event: AgentEvent,
}

impl Event for AgentEventEmitted {
    const NAME: &'static str = "yoagent/agent_event";
    type Value = ();
}

impl AgentEventEmitted {
    /// Wrap an event for publishing.
    pub fn new(event: AgentEvent) -> Self {
        Self { event }
    }

    /// The agent's event.
    pub fn event(&self) -> &AgentEvent {
        &self.event
    }
}

/// Publish one event on `ctx`'s bus (fire-and-forget; never blocks).
///
/// A refused publish (the root was shut down) is logged at debug and dropped.
pub fn emit_agent_event(ctx: &Ctx, event: AgentEvent) {
    let key = EventKey::<AgentEventEmitted>::of();
    if let Err(error) = ctx
        .events()
        .emit(ctx, &key, Arc::new(AgentEventEmitted::new(event)))
    {
        tracing::debug!(%error, "agent event not published on the rutis bus");
    }
}

/// A sender to hand to `Agent::prompt_with_sender` (and the other
/// `*_with_sender` methods) that publishes every event on the bus and, when
/// `forward` is given, also passes it on — **before** publishing, so your own
/// consumer is never behind the bus.
///
/// The returned task ends when every clone of the sender is dropped (the
/// agent drops its copy when the run ends); await it to know the last event
/// was handed over. A closed `forward` receiver stops forwarding but not
/// publishing.
pub fn event_sender(
    ctx: &Ctx,
    forward: Option<mpsc::UnboundedSender<AgentEvent>>,
) -> (mpsc::UnboundedSender<AgentEvent>, JoinHandle<()>) {
    let (tx, mut rx) = mpsc::unbounded_channel::<AgentEvent>();
    let handle = ctx.handle().clone();
    let ctx = ctx.clone();
    let task = handle.spawn(async move {
        let mut forward = forward;
        while let Some(event) = rx.recv().await {
            if let Some(out) = &forward {
                if out.send(event.clone()).is_err() {
                    forward = None;
                }
            }
            emit_agent_event(&ctx, event);
        }
    });
    (tx, task)
}
