//! The frontend protocol: what a UI sends and receives, whatever carries it —
//! a rutis plugin call (a terminal UI) or a WebSocket (a browser).
//!
//! JSON, internally tagged by `type`, field names in camelCase, the same
//! conventions as yoagent's [`AgentEvent`].

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use yoagent::{AgentEvent, SessionStats};

/// From a frontend to the session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ClientMessage {
    /// Start a run with this prompt; while one runs, it is queued and runs next.
    Prompt { text: String },
    /// Guidance for the run in progress, picked up between tool batches.
    Steer { text: String },
    /// Queued after the run in progress, in the same run.
    FollowUp { text: String },
    /// Stop the run in progress.
    Abort,
    /// Forget the conversation.
    Reset,
    /// The answer to a [`ServerMessage::UiRequest`].
    UiResponse { id: u64, value: Json },
    /// End the session (a terminal UI quitting). A host may ignore it.
    Quit,
}

/// From the session to a frontend. Every message is delivered to every
/// connected frontend, in order, none dropped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ServerMessage {
    /// The first message on a connection.
    Hello {
        /// Whether a run is in progress (a browser that connects mid-run).
        running: bool,
        /// Browser components plugins offer ([`UiPlugin`]).
        ui_plugins: Vec<UiPlugin>,
        /// Questions still waiting for an answer, oldest first: a frontend
        /// that connects (or reloads) while one is open can still answer it.
        ui_requests: Vec<PendingUiRequest>,
    },
    /// A run started. `run` numbers runs within the session.
    RunStarted { run: u64, prompt: String },
    /// An event of the run. Streamed text arrives coalesced: consecutive
    /// text deltas are merged before they are sent.
    Event { run: u64, event: Box<AgentEvent> },
    /// The run ended — always sent once per `RunStarted`, whatever happened
    /// to the events before it.
    RunEnded {
        run: u64,
        stats: Box<SessionStats>,
        /// Set when the run ended on an error or was aborted.
        error: Option<String>,
    },
    /// A plugin asks the user something. Answer with
    /// [`ClientMessage::UiResponse`]; the first answer from any frontend wins.
    UiRequest { id: u64, request: UiRequest },
    /// A request was answered (here or elsewhere) or timed out: close its dialog.
    UiResolved { id: u64 },
    /// A browser component was offered or withdrawn: the full list.
    UiPlugins { ui_plugins: Vec<UiPlugin> },
    /// The session ends.
    Closed,
}

/// A question waiting for an answer, as listed in [`ServerMessage::Hello`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingUiRequest {
    pub id: u64,
    pub request: UiRequest,
}

/// What a plugin asks the user. The answer's shape depends on the kind:
/// `confirm` → `true`/`false`, `select` → the chosen option (string) or
/// `null`, `input` → the text or `null`; `notify` needs none.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum UiRequest {
    Confirm {
        title: String,
        #[serde(default)]
        message: String,
    },
    Select {
        title: String,
        options: Vec<String>,
    },
    Input {
        title: String,
        #[serde(default)]
        placeholder: String,
        /// Text the field starts with (an editor's prefill).
        #[serde(default)]
        value: String,
    },
    Notify {
        message: String,
        #[serde(default)]
        level: String,
    },
}

impl UiRequest {
    /// The answer when no frontend is attached or none answered in time:
    /// the safe one (`false` for a confirmation, no choice, no text).
    pub fn default_answer(&self) -> Json {
        match self {
            UiRequest::Confirm { .. } => Json::Bool(false),
            _ => Json::Null,
        }
    }

    /// Whether the request waits for an answer.
    pub fn expects_answer(&self) -> bool {
        !matches!(self, UiRequest::Notify { .. })
    }
}

/// A browser component a plugin offers: an ES module the browser frontend
/// loads from `/ui-plugins/<name>.js`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiPlugin {
    pub name: String,
    /// Tool names whose results the component renders (`renderTool`).
    #[serde(default)]
    pub tools: Vec<String>,
    /// Whether it adds a side panel (`mountPanel`).
    #[serde(default)]
    pub panel: bool,
    /// Set by the session, new each time the plugin is offered: a frontend
    /// reloads a module whose version changed.
    #[serde(default)]
    pub version: u64,
}
