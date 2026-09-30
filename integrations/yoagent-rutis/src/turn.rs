//! Turn notes: a yoagent [`TurnHook`] over a rutis `waterfall`.
//!
//! Before every LLM request the bridge dispatches one [`TurnEvent`]. Each
//! listener may [`add_note`](TurnEvent::add_note) and then calls `next`; the
//! notes are joined (one per line, in listener order) and appended to the
//! request's latest user turn by yoagent — transient, never stored in
//! history. Turn notes are advisory, so this hook **fails open**: a listener
//! that errors or panics, or a chain past its timeout (default
//! [`DEFAULT_TURN_TIMEOUT`]), keeps the notes added so far and the request
//! proceeds; the failure is logged. A host that is not running adds no notes
//! (warned once per hook).
//!
//! Notes are recomputed for every request (never accumulated across turns).
//! A raw `WaterfallListener` that does not call `next` drops the notes every
//! later listener would have added; the [`PluginCtxExt::on_turn`](crate::PluginCtxExt::on_turn)
//! helper always calls it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rutis::{BoxFuture, CordisError, Ctx, Event, EventKey, Next, Terminal, WaterfallListener};
use yoagent::{TurnContext, TurnHook};

use crate::host::{run_chain, Outcome};

/// Default bound on one request's turn-note chain (notes so far are kept).
pub const DEFAULT_TURN_TIMEOUT: Duration = Duration::from_secs(5);

/// The request about to be sent, dispatched as a `waterfall` event.
#[derive(Debug)]
pub struct TurnEvent {
    model: String,
    user_request: Option<String>,
    latest_user_text: Option<String>,
    tool_names: Vec<String>,
    notes: Mutex<Vec<String>>,
}

impl Event for TurnEvent {
    const NAME: &'static str = "yoagent/turn";
    type Value = ();
}

impl TurnEvent {
    /// Build an event by hand (for testing a listener outside an agent).
    pub fn new(model: impl Into<String>, tool_names: Vec<String>) -> Self {
        Self {
            model: model.into(),
            user_request: None,
            latest_user_text: None,
            tool_names,
            notes: Mutex::new(Vec::new()),
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

    fn from_turn(turn: &TurnContext<'_>) -> Self {
        let mut event = Self::new(
            turn.model,
            turn.tools.iter().map(|t| t.name.clone()).collect(),
        );
        event.user_request = turn.user_request();
        event.latest_user_text = turn.latest_user_text();
        event
    }

    /// The model id of the request.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// What the user asked for (see yoagent's `TurnContext::user_request`).
    pub fn user_request(&self) -> Option<&str> {
        self.user_request.as_deref()
    }

    /// The text of the user's latest message.
    pub fn latest_user_text(&self) -> Option<&str> {
        self.latest_user_text.as_deref()
    }

    /// Names of the tools offered on this request.
    pub fn tool_names(&self) -> &[String] {
        &self.tool_names
    }

    /// Add a note to this request's latest user turn.
    pub fn add_note(&self, note: impl Into<String>) {
        self.notes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(note.into());
    }

    /// The notes added so far.
    pub fn notes(&self) -> Vec<String> {
        self.notes.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

struct DoneTerminal;

impl Terminal<TurnEvent> for DoneTerminal {
    fn call<'a>(
        &'a self,
        _ctx: &'a Ctx,
        _e: &'a TurnEvent,
    ) -> BoxFuture<'a, Result<(), CordisError>> {
        Box::pin(async { Ok(()) })
    }
}

/// yoagent [`TurnHook`] dispatching a [`TurnEvent`] per request.
///
/// Built by [`RutisBridge::turn_hook`](crate::RutisBridge::turn_hook).
#[derive(Clone)]
pub struct RutisTurnHook {
    ctx: Ctx,
    timeout: Option<Duration>,
    /// Warn about a stopped host once, not on every turn.
    warned_closed: Arc<AtomicBool>,
}

impl RutisTurnHook {
    pub(crate) fn new(ctx: Ctx, timeout: Option<Duration>) -> Self {
        Self {
            ctx,
            timeout,
            warned_closed: Arc::new(AtomicBool::new(false)),
        }
    }
}

#[async_trait::async_trait]
impl TurnHook for RutisTurnHook {
    async fn before_turn(&self, turn: &TurnContext<'_>) -> Option<String> {
        let event = TurnEvent::from_turn(turn);
        let key = EventKey::<TurnEvent>::of();
        let outcome = run_chain(&self.ctx, self.timeout, || {
            self.ctx
                .events()
                .waterfall(&self.ctx, &key, &event, DoneTerminal)
        })
        .await;
        match outcome {
            Outcome::Closed(why) => {
                if !self.warned_closed.swap(true, Ordering::Relaxed) {
                    tracing::warn!(why, "skipping plugin turn notes");
                }
                return None;
            }
            Outcome::TimedOut(limit) => {
                tracing::warn!(
                    ?limit,
                    "the plugin turn hooks timed out; keeping earlier notes"
                )
            }
            Outcome::Panicked => {
                tracing::warn!("a plugin turn hook panicked; keeping earlier notes")
            }
            Outcome::Finished(Err(error)) => {
                tracing::warn!(%error, "a plugin turn hook failed; keeping earlier notes")
            }
            Outcome::Finished(Ok(())) => {}
        }
        let notes = event.notes();
        (!notes.is_empty()).then(|| notes.join("\n"))
    }
}

/// Waterfall listener around a note closure.
pub(crate) struct NoteListener<F>(pub(crate) F);

impl<F> WaterfallListener<TurnEvent> for NoteListener<F>
where
    F: Fn(&TurnEvent) -> Option<String> + Send + Sync + 'static,
{
    fn call<'a>(
        &'a self,
        _ctx: &'a Ctx,
        e: &'a TurnEvent,
        next: Next<'a, TurnEvent>,
    ) -> BoxFuture<'a, Result<(), CordisError>> {
        Box::pin(async move {
            if let Some(note) = (self.0)(e) {
                e.add_note(note);
            }
            next.call().await
        })
    }
}
