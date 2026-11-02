//! Agent events on the rutis bus: [`AgentEventEmitted`].
//!
//! Every event of a run that uses the bridge's extension is published with
//! rutis `emit` as an [`AgentEventEmitted`], from the extension's `on_event`
//! — no event sender to wire. `emit` starts the listeners' calls and
//! returns without waiting for them, so a slow or failing listener never
//! slows the agent; listener errors and panics go to the rutis `ErrorSink`.
//!
//! **Ordering.** rutis starts the calls of one event name's emits in emit
//! order at every listener (rutis 0.13+), but runs them concurrently: a
//! listener that does its work as its call starts — an
//! [`on_agent_event`](crate::PluginCtxExt::on_agent_event) observer — sees
//! each run's events in the order the run produced them; one that awaits
//! before acting may finish them in another order. Runs on one bridge
//! interleave; tell them apart by [`AgentEventEmitted::run_id`] (unique) or
//! [`AgentEventEmitted::label`] (the host's `Agent::with_run_label`). A
//! listener may see an event before or after the run's own consumer does.
//!
//! **Cost.** Each event is cloned once to publish it (whether or not a
//! listener is registered: rutis does not say), and streaming produces one
//! `MessageUpdate` event per text delta. A slow listener never slows the
//! runs; a listener in another process costs one cross-process call per
//! event.
//!
//! **While the host is not running** nothing is published.

use std::sync::Arc;

use rutis::Event;
use serde::{Deserialize, Serialize};
use yoagent::AgentEvent;

use crate::handler::RunInfo;
use crate::host::Host;

/// One [`AgentEvent`], as published on the rutis bus.
///
/// Serde (every rutis event is, since 0.13): it reaches listeners in other
/// processes and languages too, as `{run_id, label, depth, event}` with the
/// event in yoagent's JSON shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
pub(crate) fn publish(host: &Host, run: &RunInfo, event: &AgentEvent) {
    if host.is_closed() {
        return;
    }
    let ctx = host.ctx();
    let wrapped = AgentEventEmitted {
        run_id: run.run_id.as_str().into(),
        label: run.label.as_deref().map(Into::into),
        depth: run.depth,
        event: event.clone(),
    };
    if let Err(error) = ctx.emit(wrapped) {
        tracing::debug!(%error, "agent event not published on the rutis bus");
    }
}
