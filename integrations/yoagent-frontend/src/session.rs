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

use crate::protocol::{ClientMessage, ServerMessage, UiPlugin, UiRequest};

/// How long streamed text is held to merge deltas before it is sent.
const COALESCE: Duration = Duration::from_millis(30);

/// How long a plugin's question waits for an answer before the safe default.
pub const UI_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Default)]
struct UiState {
    next_id: u64,
    pending: HashMap<u64, oneshot::Sender<Json>>,
}

struct Inner {
    clients: Mutex<HashMap<u64, mpsc::UnboundedSender<ServerMessage>>>,
    next_client: AtomicU64,
    ui: Mutex<UiState>,
    /// Offered browser components, with their module source.
    ui_plugins: Mutex<Vec<(UiPlugin, Arc<str>)>>,
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
        let _ = tx.send(ServerMessage::Hello {
            running: self.inner.running.load(Ordering::SeqCst),
            ui_plugins: self.ui_plugins(),
        });
        self.inner.clients.lock().unwrap().insert(id, tx);
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
                if let Some(waiter) = waiter {
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
    pub async fn ask(&self, request: UiRequest, timeout: Duration) -> Json {
        let default = request.default_answer();
        if self.frontends() == 0 {
            return default;
        }
        let (tx, rx) = oneshot::channel();
        let id = {
            let mut ui = self.inner.ui.lock().unwrap();
            ui.next_id += 1;
            let id = ui.next_id;
            if request.expects_answer() {
                ui.pending.insert(id, tx);
            }
            id
        };
        let expects = request.expects_answer();
        self.broadcast(ServerMessage::UiRequest { id, request });
        if !expects {
            return Json::Null;
        }
        let answer = tokio::time::timeout(timeout, rx).await;
        self.inner.ui.lock().unwrap().pending.remove(&id);
        self.broadcast(ServerMessage::UiResolved { id });
        match answer {
            Ok(Ok(value)) => value,
            _ => default,
        }
    }

    /// Offer a browser component; withdrawn with [`Session::remove_ui_plugin`].
    /// Offering one under a name already taken replaces it.
    pub fn add_ui_plugin(&self, plugin: UiPlugin, module: impl Into<Arc<str>>) {
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
            let mut quit = false;
            let mut tick = tokio::time::interval(COALESCE);
            loop {
                tokio::select! {
                    event = events.recv() => {
                        let Some(event) = event else { break };
                        if let AgentEvent::AgentEnd { messages, stats: s, .. } = &event {
                            stats = s.clone();
                            error = run_error(messages);
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
                        ClientMessage::Abort => agent.abort(),
                        ClientMessage::Reset => queue.clear(),
                        ClientMessage::Quit if self.accept_quit => {
                            agent.abort();
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

/// The run's error, from its last assistant message.
fn run_error(messages: &[AgentMessage]) -> Option<String> {
    messages.iter().rev().find_map(|m| match m {
        AgentMessage::Llm(Message::Assistant {
            stop_reason,
            error_message,
            ..
        }) => match stop_reason {
            StopReason::Error => Some(error_message.clone().unwrap_or_else(|| "error".into())),
            StopReason::Aborted => Some("aborted".into()),
            _ => None,
        },
        _ => None,
    })
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
