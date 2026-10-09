//! The session: one agent, any number of frontends.
//!
//! [`Session`] is the handle frontends and plugins use (cheap to clone);
//! [`Driver`] owns the command queue and runs the agent. Every
//! [`ServerMessage`] goes to every connected frontend over its own unbounded
//! channel — in order, none dropped — so `RunEnded` arrives even when a slow
//! frontend fell behind on streamed text.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value as Json;
use tokio::sync::{mpsc, oneshot};
use yoagent::{Agent, AgentEvent, AgentMessage, Message, SessionStats, StopReason, StreamDelta};

use crate::protocol::{ClientMessage, PendingUiRequest, ServerMessage, UiPlugin, UiRequest};

/// How long streamed text is held to merge deltas before it is sent.
const COALESCE: Duration = Duration::from_millis(30);

/// How long a plugin's question waits for an answer before the safe default.
pub const UI_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Default)]
struct UiState {
    next_id: u64,
    /// Open questions: who waits for the answer, and the question (replayed
    /// to frontends that connect while it is open).
    pending: HashMap<u64, (oneshot::Sender<Json>, UiRequest)>,
    /// Asker-chosen keys of open questions, for [`Session::withdraw`].
    keys: HashMap<String, u64>,
}

struct Inner {
    clients: Mutex<HashMap<u64, mpsc::UnboundedSender<ServerMessage>>>,
    next_client: AtomicU64,
    ui: Mutex<UiState>,
    /// Offered browser components, with their module source.
    ui_plugins: Mutex<Vec<(UiPlugin, Arc<str>)>>,
    plugin_versions: AtomicU64,
    running: AtomicBool,
}

/// The session handle.
#[derive(Clone)]
pub struct Session {
    inner: Arc<Inner>,
    /// Only the handles hold it: once they are all gone, the driver ends.
    commands: mpsc::UnboundedSender<ClientMessage>,
}

/// A frontend's connection: its id and the messages for it.
pub struct Connection {
    pub id: u64,
    pub messages: mpsc::UnboundedReceiver<ServerMessage>,
}

/// Runs the agent for a [`Session`]; see [`Driver::run`].
pub struct Driver {
    inner: Arc<Inner>,
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
            }),
            commands: tx,
        };
        let driver = Driver {
            inner: session.inner.clone(),
            commands: rx,
            accept_quit,
        };
        (session, driver)
    }

    /// Attach a frontend. Its first message is [`ServerMessage::Hello`].
    pub fn connect(&self) -> Connection {
        let id = self.inner.next_client.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::unbounded_channel();
        // Under the clients lock: a run boundary broadcast either reaches
        // this frontend after its Hello or is already reflected in it.
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

    /// A message from a frontend.
    pub fn send(&self, message: ClientMessage) {
        match message {
            ClientMessage::UiResponse { id, value } => {
                let waiter = self.inner.ui.lock().unwrap().pending.remove(&id);
                if let Some((waiter, _)) = waiter {
                    let _ = waiter.send(value);
                }
            }
            other => {
                let _ = self.commands.send(other);
            }
        }
    }

    /// Ask the user through whichever frontend answers first. With no
    /// frontend attached, or no answer within `timeout`, the safe default
    /// ([`UiRequest::default_answer`]). A `notify` is shown and returns at once.
    ///
    /// Cancel-safe: dropping the future (the plugin's call was cancelled)
    /// withdraws the question, and frontends close its dialog.
    pub async fn ask(&self, request: UiRequest, timeout: Duration) -> Json {
        self.ask_keyed(request, timeout, None).await
    }

    /// [`Session::ask`], the question findable by `key` for
    /// [`Session::withdraw`] — for an asker that cannot drop this future
    /// when it gives up (a plugin in another process).
    pub async fn ask_keyed(
        &self,
        request: UiRequest,
        timeout: Duration,
        key: Option<String>,
    ) -> Json {
        let default = request.default_answer();
        if self.frontends() == 0 {
            return default;
        }
        let expects = request.expects_answer();
        let (tx, rx) = oneshot::channel();
        let id = {
            let mut ui = self.inner.ui.lock().unwrap();
            ui.next_id += 1;
            let id = ui.next_id;
            if expects {
                ui.pending.insert(id, (tx, request.clone()));
                if let Some(key) = key.clone() {
                    ui.keys.insert(key, id);
                }
            }
            id
        };
        self.broadcast(ServerMessage::UiRequest { id, request });
        if !expects {
            return Json::Null;
        }
        /// Withdraws the question however `ask` ends: answered, timed out,
        /// or dropped.
        struct Withdraw<'a> {
            session: &'a Session,
            id: u64,
            key: Option<String>,
        }
        impl Drop for Withdraw<'_> {
            fn drop(&mut self) {
                {
                    let mut ui = self.session.inner.ui.lock().unwrap();
                    ui.pending.remove(&self.id);
                    if let Some(key) = &self.key {
                        ui.keys.remove(key);
                    }
                }
                self.session
                    .broadcast(ServerMessage::UiResolved { id: self.id });
            }
        }
        let _withdraw = Withdraw {
            session: self,
            id,
            key,
        };
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(value)) => value,
            _ => default,
        }
    }

    /// Withdraw the open question asked with `key`: its asker gets the safe
    /// default, frontends close it. Unknown keys are ignored.
    pub fn withdraw(&self, key: &str) {
        let mut ui = self.inner.ui.lock().unwrap();
        if let Some(id) = ui.keys.remove(key) {
            // Dropping the waiter ends `ask` with the default; its guard
            // then tells the frontends.
            ui.pending.remove(&id);
        }
    }

    /// Offer a browser component; withdrawn with [`Session::remove_ui_plugin`].
    /// Offering one under a name already taken replaces it.
    pub fn add_ui_plugin(&self, mut plugin: UiPlugin, module: impl Into<Arc<str>>) {
        plugin.version = self.inner.plugin_versions.fetch_add(1, Ordering::Relaxed) + 1;
        {
            let mut plugins = self.inner.ui_plugins.lock().unwrap();
            plugins.retain(|(p, _)| p.name != plugin.name);
            plugins.push((plugin, module.into()));
        }
        self.broadcast(ServerMessage::UiPlugins {
            ui_plugins: self.ui_plugins(),
        });
    }

    pub fn remove_ui_plugin(&self, name: &str) {
        self.inner
            .ui_plugins
            .lock()
            .unwrap()
            .retain(|(p, _)| p.name != name);
        self.broadcast(ServerMessage::UiPlugins {
            ui_plugins: self.ui_plugins(),
        });
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

impl Driver {
    /// Run the agent until a frontend quits (with `accept_quit`) or every
    /// [`Session`] handle is gone. A prompt sent during a run is queued and
    /// runs next; steering and follow-ups go to the run in progress (or start
    /// one when idle). Returns the agent with its history.
    pub async fn run(mut self, mut agent: Agent) -> Agent {
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
            self.inner.running.store(true, Ordering::SeqCst);
            self.inner.broadcast(ServerMessage::RunStarted {
                run,
                prompt: prompt.clone(),
            });
            let mut events = agent.prompt(prompt).await;
            let mut text = Coalesced::default();
            let mut stats = SessionStats::default();
            let mut error = None;
            let mut aborted = false;
            let mut reset = false;
            let mut quit = false;
            let mut tick = tokio::time::interval(COALESCE);
            loop {
                tokio::select! {
                    event = events.recv() => {
                        let Some(event) = event else { break };
                        match &event {
                            AgentEvent::AgentEnd { messages, stats: s, .. } => {
                                stats = s.clone();
                                error = error.take().or_else(|| run_error(messages));
                            }
                            AgentEvent::InputRejected { reason } => {
                                error = Some(format!("input rejected: {reason}"));
                            }
                            _ => {}
                        }
                        for event in text.push(event) {
                            self.inner.broadcast(ServerMessage::Event { run, event: Box::new(event) });
                        }
                    }
                    _ = tick.tick() => {
                        if let Some(event) = text.flush() {
                            self.inner.broadcast(ServerMessage::Event { run, event: Box::new(event) });
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
                self.inner.broadcast(ServerMessage::Event {
                    run,
                    event: Box::new(event),
                });
            }
            agent.finish().await;
            if reset {
                agent.clear_messages();
            }
            if aborted {
                error = error.or_else(|| Some("aborted".into()));
            }
            self.inner.running.store(false, Ordering::SeqCst);
            self.inner.broadcast(ServerMessage::RunEnded {
                run,
                stats: Box::new(stats),
                error,
            });
            if quit {
                break 'session;
            }
        }
        self.inner.broadcast(ServerMessage::Closed);
        agent
    }
}

fn user(text: String) -> AgentMessage {
    AgentMessage::Llm(Message::user(text))
}

/// The run's error, from its last assistant message only (an earlier
/// message's outcome says nothing about how the run ended).
fn run_error(messages: &[AgentMessage]) -> Option<String> {
    let last = messages.iter().rev().find_map(|m| match m {
        AgentMessage::Llm(Message::Assistant {
            stop_reason,
            error_message,
            ..
        }) => Some((stop_reason, error_message)),
        _ => None,
    })?;
    match last {
        (StopReason::Error, message) => Some(message.clone().unwrap_or_else(|| "error".into())),
        (StopReason::Aborted, _) => Some("aborted".into()),
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
    fn a_held_delta_is_flushed_on_the_tick() {
        let mut c = Coalesced::default();
        c.push(text("a"));
        assert_eq!(delta_of(&c.flush().unwrap()), "a");
        assert!(c.flush().is_none());
    }
}
