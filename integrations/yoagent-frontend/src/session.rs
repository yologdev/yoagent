//! The session: one agent, any number of frontends.
//!
//! [`Session`] is the handle frontends and plugins use (cheap to clone);
//! [`Driver`] owns the command queue and runs the agent. Every
//! [`ServerMessage`] goes to every connected frontend over its own unbounded
//! channel — in order, none dropped — so `RunEnd` arrives even when a slow
//! frontend fell behind on streamed text.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value as Json;
use tokio::sync::{mpsc, oneshot};
use yoagent::{Agent, AgentEvent, AgentMessage, Message, SessionStats, StopReason, StreamDelta};

use crate::protocol::{
    ClientMessage, NoticeLevel, PendingUiRequest, ResolveReason, RunOutcome, ServerMessage,
    UiPlugin, UiRequest,
};

/// Flush period for merged text deltas: a held delta waits at most this long.
const COALESCE: Duration = Duration::from_millis(30);

/// How long a plugin's question waits for an answer before the safe default.
pub const UI_TIMEOUT: Duration = Duration::from_secs(300);

/// How long a withdrawal for a question not asked yet is remembered: the
/// asker's `withdraw` can overtake its own `request` on the way to the host.
const EARLY_WITHDRAWAL: Duration = Duration::from_secs(60);

#[derive(Default)]
struct UiState {
    next_id: u64,
    /// Open questions: who waits for the answer, and the question (replayed
    /// to frontends that connect while it is open).
    pending: HashMap<u64, (oneshot::Sender<Json>, UiRequest)>,
    /// Asker-chosen keys of open questions, for [`Session::withdraw`].
    keys: HashMap<String, u64>,
    /// Keys withdrawn before their question was asked.
    early: HashMap<String, Instant>,
}

struct Inner {
    clients: Mutex<HashMap<u64, mpsc::UnboundedSender<ServerMessage>>>,
    next_client: AtomicU64,
    ui: Mutex<UiState>,
    /// Offered browser components, with their module source.
    ui_plugins: Mutex<Vec<(UiPlugin, Arc<str>)>>,
    plugin_versions: AtomicU64,
    running: AtomicBool,
    /// The driver ended (or failed): nothing sent is heard any more.
    closed: AtomicBool,
}

/// The session handle.
#[derive(Clone)]
pub struct Session {
    inner: Arc<Inner>,
    /// Only the handles hold it: once they are all gone, the driver ends.
    commands: mpsc::UnboundedSender<ClientMessage>,
}

/// A frontend's connection: its id and the messages for it. Dropping
/// `messages` detaches the frontend at the next broadcast.
pub struct Connection {
    pub id: u64,
    pub messages: mpsc::UnboundedReceiver<ServerMessage>,
}

/// Runs the agent for a [`Session`]; see [`Driver::run`].
pub struct Driver {
    /// Closes the session when the driver goes, run or not.
    ending: Ending,
    commands: mpsc::UnboundedReceiver<ClientMessage>,
    accept_quit: bool,
}

impl Session {
    /// A session and its driver. With `accept_quit`, a frontend's
    /// [`ClientMessage::Quit`] ends the session (a terminal UI); without, it
    /// is ignored (a server with browser frontends coming and going).
    pub fn new(accept_quit: bool) -> (Session, Driver) {
        let (tx, rx) = mpsc::unbounded_channel();
        let session = Session {
            inner: Arc::new(Inner {
                clients: Mutex::default(),
                next_client: AtomicU64::new(1),
                ui: Mutex::default(),
                ui_plugins: Mutex::default(),
                plugin_versions: AtomicU64::new(0),
                running: AtomicBool::new(false),
                closed: AtomicBool::new(false),
            }),
            commands: tx,
        };
        let driver = Driver {
            ending: Ending {
                inner: session.inner.clone(),
                open_run: None,
            },
            commands: rx,
            accept_quit,
        };
        (session, driver)
    }

    /// Attach a frontend. Its first message is [`ServerMessage::Hello`]
    /// (then [`ServerMessage::Closed`] at once, if the session already ended).
    pub fn connect(&self) -> Connection {
        let id = self.inner.next_client.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::unbounded_channel();
        // Under the clients lock, every later broadcast reaches this frontend
        // after its Hello; one already reflected in Hello (`running`, an open
        // question) may still arrive after it, and frontends tolerate that.
        let mut clients = self.inner.clients.lock().unwrap();
        let mut ui_requests: Vec<PendingUiRequest> = {
            let ui = self.inner.ui.lock().unwrap();
            ui.pending
                .iter()
                .map(|(id, (_, request))| PendingUiRequest {
                    id: *id,
                    request: request.clone(),
                })
                .collect()
        };
        ui_requests.sort_by_key(|r| r.id);
        let _ = tx.send(ServerMessage::Hello {
            running: self.inner.running.load(Ordering::SeqCst),
            ui_plugins: self.ui_plugins(),
            ui_requests,
        });
        if self.inner.closed.load(Ordering::SeqCst) {
            let _ = tx.send(ServerMessage::Closed);
        }
        clients.insert(id, tx);
        Connection { id, messages: rx }
    }

    /// Detach a frontend.
    pub fn disconnect(&self, id: u64) {
        self.inner.clients.lock().unwrap().remove(&id);
    }

    /// Number of attached frontends.
    pub fn frontends(&self) -> usize {
        self.inner.clients.lock().unwrap().len()
    }

    /// Whether the session ended: the driver stopped, nothing sent is heard.
    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::SeqCst)
    }

    /// A message from a frontend. Returns `false` when the session ended (the
    /// message is not heard) — a frontend should tell its user.
    ///
    /// An answer that does not fit its question ([`UiRequest::accepts`]) is
    /// ignored: the question stays open for another answer (or its timeout).
    pub fn send(&self, message: ClientMessage) -> bool {
        if self.is_closed() {
            tracing::warn!("a frontend message after the session ended was dropped");
            return false;
        }
        match message {
            ClientMessage::UiResponse { id, value } => {
                let mut ui = self.inner.ui.lock().unwrap();
                match ui.pending.get(&id) {
                    Some((_, request)) if !request.accepts(&value) => {
                        tracing::warn!(id, answer = %value, "an answer that does not fit its question was ignored");
                    }
                    Some(_) => {
                        if let Some((waiter, _)) = ui.pending.remove(&id) {
                            let _ = waiter.send(value);
                        }
                    }
                    None => tracing::debug!(id, "an answer to a question already closed"),
                }
                true
            }
            other => self.commands.send(other).is_ok(),
        }
    }

    /// Tell every frontend something outside a run's events.
    pub fn notice(&self, level: NoticeLevel, message: impl Into<String>) {
        self.broadcast(ServerMessage::Notice {
            level,
            message: message.into(),
        });
    }

    /// Send one frontend a message (a reply to something only it sent).
    pub(crate) fn send_to(&self, id: u64, message: ServerMessage) {
        if let Some(tx) = self.inner.clients.lock().unwrap().get(&id) {
            let _ = tx.send(message);
        }
    }

    /// Ask the user through whichever frontend answers first. With no
    /// frontend attached, or no fitting answer within `timeout`, the safe
    /// default ([`UiRequest::default_answer`]). A `notify` is shown and
    /// returns at once.
    ///
    /// Cancel-safe: dropping the future (the plugin's call was cancelled)
    /// withdraws the question, and frontends close its dialog.
    pub async fn ask(&self, request: UiRequest, timeout: Duration) -> Json {
        self.ask_keyed(request, timeout, None).await
    }

    /// [`Session::ask`], the question findable by `key` for
    /// [`Session::withdraw`] — for an asker that cannot drop this future
    /// when it gives up (a plugin in another process). Keys must be unique
    /// (a UUID); a key already in use is ignored.
    pub async fn ask_keyed(
        &self,
        request: UiRequest,
        timeout: Duration,
        key: Option<String>,
    ) -> Json {
        let default = request.default_answer();
        let expects = request.expects_answer();
        let (tx, rx) = oneshot::channel();
        // Before the `ui` lock: `connect` takes `clients` then `ui`, so
        // taking them the other way round could deadlock.
        let attached = self.frontends() > 0;
        let (id, key) = {
            let mut ui = self.inner.ui.lock().unwrap();
            ui.early.retain(|_, at| at.elapsed() < EARLY_WITHDRAWAL);
            if let Some(key) = &key {
                if ui.early.remove(key).is_some() {
                    tracing::debug!(%key, "a question withdrawn before it was asked");
                    return default;
                }
            }
            if !attached {
                drop(ui);
                tracing::info!(
                    title = request.title(),
                    "no frontend to ask: the safe default"
                );
                return default;
            }
            ui.next_id += 1;
            let id = ui.next_id;
            let mut owned_key = None;
            if expects {
                ui.pending.insert(id, (tx, request.clone()));
                if let Some(key) = key {
                    if ui.keys.contains_key(&key) {
                        tracing::warn!(%key, "a question key already in use: this one cannot be withdrawn by key");
                    } else {
                        ui.keys.insert(key.clone(), id);
                        owned_key = Some(key);
                    }
                }
            }
            (id, owned_key)
        };
        let title = request.title().to_owned();
        self.broadcast(ServerMessage::UiRequest { id, request });
        if !expects {
            return Json::Null;
        }
        /// Closes the question however `ask` ends — answered, timed out,
        /// withdrawn by key, or dropped — and tells the frontends why.
        struct Close<'a> {
            session: &'a Session,
            id: u64,
            key: Option<String>,
            reason: ResolveReason,
        }
        impl Drop for Close<'_> {
            fn drop(&mut self) {
                {
                    let mut ui = self.session.inner.ui.lock().unwrap();
                    ui.pending.remove(&self.id);
                    if let Some(key) = &self.key {
                        if ui.keys.get(key) == Some(&self.id) {
                            ui.keys.remove(key);
                        }
                    }
                }
                self.session.broadcast(ServerMessage::UiResolved {
                    id: self.id,
                    reason: self.reason,
                });
            }
        }
        let mut close = Close {
            session: self,
            id,
            key,
            reason: ResolveReason::Withdrawn,
        };
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(value)) => {
                close.reason = ResolveReason::Answered;
                value
            }
            Ok(Err(_)) => default,
            Err(_) => {
                close.reason = ResolveReason::TimedOut;
                tracing::info!(%title, "no answer in time: the safe default");
                default
            }
        }
    }

    /// Withdraw the open question asked with `key`: its asker gets the safe
    /// default, frontends close it. A key not asked yet is remembered for a
    /// minute, so a withdrawal that overtakes its question still counts.
    pub fn withdraw(&self, key: &str) {
        let mut ui = self.inner.ui.lock().unwrap();
        ui.early.retain(|_, at| at.elapsed() < EARLY_WITHDRAWAL);
        match ui.keys.remove(key) {
            // Dropping the waiter ends `ask` with the default; its guard then
            // tells the frontends.
            Some(id) => {
                ui.pending.remove(&id);
            }
            None => {
                ui.early.insert(key.to_owned(), Instant::now());
            }
        }
    }

    /// Offer a browser component, returning the version it got; withdrawn
    /// with [`Session::remove_ui_plugin`]. Offering one under a name already
    /// taken replaces it. A plugin without a name is refused (`None`).
    pub fn add_ui_plugin(&self, mut plugin: UiPlugin, module: impl Into<Arc<str>>) -> Option<u64> {
        if plugin.name.trim().is_empty() {
            tracing::warn!("a UI plugin without a name was refused");
            return None;
        }
        let version = self.inner.plugin_versions.fetch_add(1, Ordering::Relaxed) + 1;
        plugin.version = version;
        {
            let mut plugins = self.inner.ui_plugins.lock().unwrap();
            plugins.retain(|(p, _)| p.name != plugin.name);
            plugins.push((plugin, module.into()));
        }
        self.broadcast(ServerMessage::UiPlugins {
            ui_plugins: self.ui_plugins(),
        });
        Some(version)
    }

    /// Withdraw a browser component. With `version`, only that offer: a
    /// plugin's own disposer must not remove an offer that replaced it.
    pub fn remove_ui_plugin(&self, name: &str, version: Option<u64>) {
        let removed = {
            let mut plugins = self.inner.ui_plugins.lock().unwrap();
            let before = plugins.len();
            plugins.retain(|(p, _)| p.name != name || version.is_some_and(|v| v != p.version));
            plugins.len() != before
        };
        if removed {
            self.broadcast(ServerMessage::UiPlugins {
                ui_plugins: self.ui_plugins(),
            });
        }
    }

    pub fn ui_plugins(&self) -> Vec<UiPlugin> {
        let plugins = self.inner.ui_plugins.lock().unwrap();
        plugins.iter().map(|(p, _)| p.clone()).collect()
    }

    /// A component's module source, to serve to a browser.
    pub fn ui_plugin_module(&self, name: &str) -> Option<Arc<str>> {
        let plugins = self.inner.ui_plugins.lock().unwrap();
        plugins
            .iter()
            .find(|(p, _)| p.name == name)
            .map(|(_, m)| m.clone())
    }

    fn broadcast(&self, message: ServerMessage) {
        self.inner.broadcast(message);
    }
}

impl Inner {
    fn broadcast(&self, message: ServerMessage) {
        self.clients
            .lock()
            .unwrap()
            .retain(|_, tx| tx.send(message.clone()).is_ok());
    }
}

/// However the driver goes — [`Driver::run`] returning, its task failing, or
/// a driver never run — a run in progress gets its `RunEnd` and every
/// frontend `Closed`.
struct Ending {
    inner: Arc<Inner>,
    open_run: Option<u64>,
}

impl Drop for Ending {
    fn drop(&mut self) {
        if let Some(run) = self.open_run.take() {
            self.inner.running.store(false, Ordering::SeqCst);
            self.inner.broadcast(ServerMessage::RunEnd {
                run,
                outcome: RunOutcome::Error,
                error: Some("the session stopped during the run".into()),
                stats: Box::default(),
                total_cost_usd: None,
            });
        }
        // Under the clients lock: a frontend connecting now gets `Closed`
        // either from `connect` or from this broadcast, never both.
        let mut clients = self.inner.clients.lock().unwrap();
        self.inner.closed.store(true, Ordering::SeqCst);
        clients.retain(|_, tx| tx.send(ServerMessage::Closed).is_ok());
    }
}

impl Driver {
    /// Run the agent until a frontend quits (with `accept_quit`) or every
    /// [`Session`] handle is gone. A prompt sent during a run is queued and
    /// runs next; steering and follow-ups go to the run in progress (or start
    /// one when idle). Returns the agent with its history.
    pub async fn run(mut self, mut agent: Agent) -> Agent {
        let inner = self.ending.inner.clone();
        let mut queue: VecDeque<String> = VecDeque::new();
        let mut run = 0u64;
        'session: loop {
            let prompt = match queue.pop_front() {
                Some(text) => text,
                None => match self.commands.recv().await {
                    Some(
                        ClientMessage::Prompt { text }
                        | ClientMessage::Steer { text }
                        | ClientMessage::FollowUp { text },
                    ) => text,
                    Some(ClientMessage::Reset) => {
                        agent.clear_messages();
                        continue;
                    }
                    Some(ClientMessage::Quit) if self.accept_quit => break,
                    Some(_) => continue,
                    None => break,
                },
            };
            run += 1;
            inner.running.store(true, Ordering::SeqCst);
            self.ending.open_run = Some(run);
            inner.broadcast(ServerMessage::RunStart {
                run,
                prompt: prompt.clone(),
            });
            let mut events = agent.prompt(prompt).await;
            let mut text = Coalesced::default();
            let mut end: Option<(SessionStats, Option<(RunOutcome, String)>)> = None;
            let mut rejected = None;
            let mut aborted = false;
            let mut reset = false;
            let mut quit = false;
            let mut tick = tokio::time::interval(COALESCE);
            loop {
                tokio::select! {
                    event = events.recv() => {
                        let Some(event) = event else { break };
                        match &event {
                            AgentEvent::AgentEnd { messages, stats, .. } => {
                                end = Some((stats.clone(), run_error(messages)));
                            }
                            AgentEvent::InputRejected { reason } => {
                                rejected = Some(format!("input rejected: {reason}"));
                            }
                            _ => {}
                        }
                        for event in text.push(event) {
                            inner.broadcast(ServerMessage::Event { run, event: Box::new(event) });
                        }
                    }
                    _ = tick.tick() => {
                        if let Some(event) = text.flush() {
                            inner.broadcast(ServerMessage::Event { run, event: Box::new(event) });
                        }
                    }
                    Some(command) = self.commands.recv() => match command {
                        ClientMessage::Prompt { text } => queue.push_back(text),
                        ClientMessage::Steer { text } => agent.steer(user(text)),
                        ClientMessage::FollowUp { text } => agent.follow_up(user(text)),
                        ClientMessage::Abort => {
                            agent.abort();
                            aborted = true;
                        }
                        // Start over: stop this run, drop what was queued,
                        // and forget the conversation once the run ends.
                        ClientMessage::Reset => {
                            queue.clear();
                            agent.abort();
                            aborted = true;
                            reset = true;
                        }
                        ClientMessage::Quit if self.accept_quit => {
                            agent.abort();
                            aborted = true;
                            quit = true;
                        }
                        _ => {}
                    },
                }
            }
            if let Some(event) = text.flush() {
                inner.broadcast(ServerMessage::Event {
                    run,
                    event: Box::new(event),
                });
            }
            agent.finish().await;
            if reset {
                agent.clear_messages();
            }
            let saw_end = end.is_some();
            let (stats, last) = end.unwrap_or_default();
            let (outcome, error) = match (rejected, saw_end, last) {
                (Some(reason), _, _) => (RunOutcome::Rejected, Some(reason)),
                (None, false, _) => {
                    // The agent's task failed: the events stopped without an
                    // AgentEnd (yoagent logs the panic).
                    inner.broadcast(ServerMessage::Notice {
                        level: NoticeLevel::Error,
                        message: "The agent's run failed unexpectedly (see the host's logs); later runs may be missing tools.".into(),
                    });
                    (
                        RunOutcome::Error,
                        Some("the run ended unexpectedly: the agent task failed".into()),
                    )
                }
                (None, true, _) if aborted => (RunOutcome::Aborted, Some("aborted".into())),
                (None, true, Some((outcome, message))) => (outcome, Some(message)),
                (None, true, None) => (RunOutcome::Completed, None),
            };
            let total_cost_usd = stats.total_cost_usd();
            inner.running.store(false, Ordering::SeqCst);
            self.ending.open_run = None;
            inner.broadcast(ServerMessage::RunEnd {
                run,
                outcome,
                error,
                stats: Box::new(stats),
                total_cost_usd,
            });
            if quit {
                break 'session;
            }
        }
        drop(self);
        agent
    }
}

fn user(text: String) -> AgentMessage {
    AgentMessage::Llm(Message::user(text))
}

/// How the run ended, from its last assistant message only (an earlier
/// message's outcome says nothing about how the run ended).
fn run_error(messages: &[AgentMessage]) -> Option<(RunOutcome, String)> {
    let last = messages.iter().rev().find_map(|m| match m {
        AgentMessage::Llm(Message::Assistant {
            stop_reason,
            error_message,
            ..
        }) => Some((stop_reason, error_message)),
        _ => None,
    })?;
    match last {
        (StopReason::Error, message) => Some((
            RunOutcome::Error,
            message.clone().unwrap_or_else(|| "error".into()),
        )),
        (StopReason::Aborted, _) => Some((RunOutcome::Aborted, "aborted".into())),
        _ => None,
    }
}

/// Merges consecutive text (and thinking) deltas into one `MessageUpdate`.
#[derive(Default)]
struct Coalesced {
    held: Option<AgentEvent>,
}

impl Coalesced {
    /// The events to send now, in order.
    fn push(&mut self, event: AgentEvent) -> Vec<AgentEvent> {
        if let (
            Some(AgentEvent::MessageUpdate {
                delta: held_delta,
                message: held_message,
            }),
            AgentEvent::MessageUpdate { delta, message },
        ) = (&mut self.held, &event)
        {
            let merged = match (held_delta, delta) {
                (StreamDelta::Text { delta: a }, StreamDelta::Text { delta: b })
                | (StreamDelta::Thinking { delta: a }, StreamDelta::Thinking { delta: b }) => {
                    a.push_str(b);
                    true
                }
                _ => false,
            };
            if merged {
                *held_message = message.clone();
                return Vec::new();
            }
        }
        let mut out: Vec<AgentEvent> = self.held.take().into_iter().collect();
        if matches!(
            event,
            AgentEvent::MessageUpdate {
                delta: StreamDelta::Text { .. } | StreamDelta::Thinking { .. },
                ..
            }
        ) {
            self.held = Some(event);
        } else {
            out.push(event);
        }
        out
    }

    fn flush(&mut self) -> Option<AgentEvent> {
        self.held.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(delta: &str) -> AgentEvent {
        AgentEvent::MessageUpdate {
            message: AgentMessage::Llm(Message::user("")),
            delta: StreamDelta::Text {
                delta: delta.into(),
            },
        }
    }

    fn delta_of(event: &AgentEvent) -> &str {
        match event {
            AgentEvent::MessageUpdate {
                delta: StreamDelta::Text { delta },
                ..
            } => delta,
            _ => panic!("a text update"),
        }
    }

    #[test]
    fn consecutive_text_deltas_merge_and_other_events_flush_them_first() {
        let mut c = Coalesced::default();
        assert!(c.push(text("Hel")).is_empty());
        assert!(c.push(text("lo")).is_empty());
        let out = c.push(AgentEvent::TurnStart);
        assert_eq!(out.len(), 2, "the merged text, then the event, in order");
        assert_eq!(delta_of(&out[0]), "Hello");
        assert!(matches!(out[1], AgentEvent::TurnStart));
        assert!(c.flush().is_none());
    }

    #[test]
    fn thinking_merges_apart_from_text() {
        let thinking = |d: &str| AgentEvent::MessageUpdate {
            message: AgentMessage::Llm(Message::user("")),
            delta: StreamDelta::Thinking { delta: d.into() },
        };
        let mut c = Coalesced::default();
        assert!(c.push(thinking("hm")).is_empty());
        assert!(c.push(thinking("m")).is_empty());
        let out = c.push(text("A"));
        assert_eq!(out.len(), 1, "the merged thinking; the text is held");
        assert!(matches!(
            &out[0],
            AgentEvent::MessageUpdate { delta: StreamDelta::Thinking { delta }, .. } if delta == "hmm"
        ));
        assert_eq!(delta_of(&c.flush().unwrap()), "A");
    }

    #[test]
    fn a_held_delta_is_flushed_on_the_tick() {
        let mut c = Coalesced::default();
        c.push(text("a"));
        assert_eq!(delta_of(&c.flush().unwrap()), "a");
        assert!(c.flush().is_none());
    }
}
