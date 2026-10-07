//! Agent events on the rutis bus: [`AgentEventEmitted`].
//!
//! Every event of a run that uses the bridge's extension is published with
//! rutis `emit` as an [`AgentEventEmitted`], from the extension's `on_event`
//! — no event sender to wire. `emit` only queues a dispatch and returns (and
//! does nothing when no listener is registered), so a slow or failing
//! listener never slows the agent; listener errors and panics go to the
//! rutis `ErrorSink`.
//!
//! **Ordering.** rutis dispatches emits of one event key in emit order, one
//! listener after another in registration order, so every listener sees each
//! run's events in the order the run produced them. Runs on one bridge
//! interleave; tell them apart by [`AgentEventEmitted::run_id`] (unique) or
//! [`AgentEventEmitted::label`] (the host's `Agent::with_run_label`). A
//! listener may see an event before or after the run's own consumer does.
//!
//! **Cost.** Each event is cloned once to publish it while any listener is
//! registered, every published event spawns one tokio task, and streaming
//! produces one `MessageUpdate` event per text delta. Dispatches of the event
//! key form **one queue shared by every run on the bus**: a slow listener
//! delays delivery of *all* runs' later events (never the runs themselves),
//! and a listener permanently slower than the agents builds an unbounded
//! backlog.
//!
//! **While the host is not running** nothing is published.

use std::sync::Arc;

use rutis::{BoxFuture, CordisError, Ctx, Event, EventKey, Listener};
use yoagent::AgentEvent;

use crate::handler::RunInfo;

/// One [`AgentEvent`], as published on the rutis bus.
#[derive(Debug, Clone)]
pub struct AgentEventEmitted {
    run_id: Arc<str>,
    label: Option<Arc<str>>,
    depth: usize,
    event: AgentEvent,
}

impl Event for AgentEventEmitted {
    const NAME: &'static str = "yoagent/agent_event";
    type Value = ();
}

impl AgentEventEmitted {
    /// Wrap an event of run `run_id` (for testing an observer).
    pub fn new(run_id: impl Into<Arc<str>>, event: AgentEvent) -> Self {
        Self {
            run_id: run_id.into(),
            label: None,
            depth: 0,
            event,
        }
    }

    /// With the run's label (for testing an observer).
    pub fn with_label(mut self, label: impl Into<Arc<str>>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// The agent's event.
    pub fn event(&self) -> &AgentEvent {
        &self.event
    }

    /// The run that produced it (unique per run).
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// The host's label for the run (`Agent::with_run_label`), if any.
    /// Informational — nothing checks it is unique, so it is not an identity.
    pub fn label(&self) -> Option<&str> {
        self.label.as_deref()
    }

    /// The run's delegation depth: 0 for a top-level run, 1 for a
    /// sub-agent's run (with the extension installed as a tree extension).
    pub fn depth(&self) -> usize {
        self.depth
    }
}

/// Publish one event of `run` on `ctx`'s bus (fire-and-forget; never blocks).
pub(crate) fn publish(ctx: &Ctx, run: &RunInfo, event: &AgentEvent) {
    if crate::host::closed(ctx).is_some() {
        return;
    }
    let wrapped = AgentEventEmitted {
        run_id: run.run_id.as_str().into(),
        label: run.label.as_deref().map(Into::into),
        depth: run.depth,
        event: event.clone(),
    };
    let key = EventKey::<AgentEventEmitted>::of();
    if let Err(error) = ctx.events().emit(ctx, &key, Arc::new(wrapped)) {
        tracing::debug!(%error, "agent event not published on the rutis bus");
    }
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
