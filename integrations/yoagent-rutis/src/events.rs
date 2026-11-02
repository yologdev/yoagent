//! Agent events for Rust observers: [`AgentEventEmitted`].
//!
//! Every event of a run that uses the bridge's extension is handed, as an
//! [`AgentEventEmitted`], to every observer a Rust plugin registered with
//! [`on_agent_event`](crate::PluginCtxExt::on_agent_event) — no event sender
//! to wire. Each observer has its own queue and task: the agent only queues
//! the event and goes on, so a slow observer never slows it; a panicking
//! observer is logged (`error!`) and called again for the next event.
//!
//! **In-process only.** The stream never goes on the rutis bus: rutis lets
//! any runtime session listen to any bus event name but `internal/…`
//! (`global`, past every emitter's filter) and emit any but `internal/…`
//! and `rutis/…` (rutis-bridge's `events/hub.rs`), so a bus event would
//! hand every run's prompts, answers and tool calls to every plugin — a
//! restricted runtime's included — and let any of them fake events to the
//! observers. The bridge keeps the observers itself, in its
//! [`Registry`](crate::Registry); they are removed when their plugin
//! unloads. Language plugins get events only through their handler's
//! opt-in (`options.events`, and `options.messages` for the conversation),
//! and a restricted one never does.
//!
//! **Ordering.** An observer is called with one event at a time, in the
//! order they were produced: each run's events in its order. Runs on one
//! bridge interleave; tell them apart by [`AgentEventEmitted::run_id`]
//! (unique) or [`AgentEventEmitted::label`] (the host's
//! `Agent::with_run_label`). An observer may see an event before or after
//! the run's own consumer does.
//!
//! **Cost.** With an observer registered, each event is cloned once and
//! shared by all of them; streaming produces one `MessageUpdate` event per
//! text delta. Nothing is cloned while no observer is registered. A stuck
//! observer's queue grows with every event until its plugin unloads.
//!
//! **While the host is not running** nothing is delivered.

use std::sync::{Arc, Mutex};

use serde::Serialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use yoagent::AgentEvent;

use crate::handler::RunInfo;
use crate::host::Host;

/// One [`AgentEvent`] of a run, as [`on_agent_event`](crate::PluginCtxExt::on_agent_event)
/// observers get it.
///
/// Serializes as `{run_id, label, depth, event}`, the event in yoagent's
/// JSON shape (to log it); it is not a rutis bus event (see the
/// [module docs](self)).
#[derive(Debug, Clone, Serialize)]
pub struct AgentEventEmitted {
    run_id: Arc<str>,
    label: Option<Arc<str>>,
    depth: usize,
    event: AgentEvent,
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

type Observer = Arc<dyn Fn(&AgentEventEmitted) + Send + Sync>;

/// The Rust observers of every run's events, kept by the [`Registry`](crate::Registry).
#[derive(Default)]
pub(crate) struct Observers {
    inner: Mutex<ObserversInner>,
}

#[derive(Default)]
struct ObserversInner {
    next: u64,
    /// In registration order: each observer's queue.
    queues: Vec<(u64, mpsc::UnboundedSender<Arc<AgentEventEmitted>>)>,
}

impl Observers {
    /// Add `observer`, called on a task of its own (spawned on the current
    /// Tokio runtime) until [`remove`](Self::remove) or `generation` is
    /// cancelled. Returns its id.
    pub(crate) fn add(&self, observer: Observer, generation: CancellationToken) -> u64 {
        let (tx, mut rx) = mpsc::unbounded_channel::<Arc<AgentEventEmitted>>();
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                if generation.is_cancelled() {
                    break;
                }
                let called =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| observer(&event)));
                if let Err(panic) = called {
                    let payload = panic
                        .downcast_ref::<&str>()
                        .map(|s| s.to_string())
                        .or_else(|| panic.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "a non-string panic".into());
                    tracing::error!(panic = %payload, "an agent event observer panicked");
                }
            }
        });
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let id = inner.next;
        inner.next += 1;
        inner.queues.push((id, tx));
        id
    }

    /// Remove the observer `id` (its task ends once its queue drains).
    pub(crate) fn remove(&self, id: u64) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.queues.retain(|(at, _)| *at != id);
    }

    /// Queue `event` of `run` for every observer (never blocks).
    fn deliver(&self, run: &RunInfo, event: &AgentEvent) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.queues.is_empty() {
            return;
        }
        let wrapped = Arc::new(AgentEventEmitted {
            run_id: run.run_id.as_str().into(),
            label: run.label.as_deref().map(Into::into),
            depth: run.depth,
            event: event.clone(),
        });
        // A closed queue: its task ended (its plugin's generation was
        // cancelled, or the runtime is gone): drop it.
        inner
            .queues
            .retain(|(_, tx)| tx.send(wrapped.clone()).is_ok());
    }
}

/// Hand one event of `run` to the Rust observers (never blocks).
pub(crate) fn publish(host: &Host, observers: &Observers, run: &RunInfo, event: &AgentEvent) {
    if host.is_closed() {
        return;
    }
    observers.deliver(run, event);
}
