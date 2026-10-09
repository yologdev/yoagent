//! The frontend protocol: what a UI sends and receives, whatever carries it —
//! a rutis plugin call (a terminal UI) or a WebSocket (a browser).
//!
//! JSON, internally tagged by `type`, field names in camelCase, the same
//! conventions as yoagent's [`AgentEvent`]. The enums are `#[non_exhaustive]`:
//! a frontend must skip a `type` (or a question `kind`) it does not know.

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
#[non_exhaustive]
pub enum ClientMessage {
    /// Start a run with this prompt; while one runs, it is queued and runs next.
    Prompt { text: String },
    /// Guidance for the run in progress, picked up between tool batches;
    /// when idle, starts a run like `Prompt`.
    Steer { text: String },
    /// Queued after the run in progress, in the same run; when idle, starts
    /// a run like `Prompt`.
    FollowUp { text: String },
    /// Stop the run in progress.
    Abort,
    /// Stop any run in progress, drop queued prompts, and forget the
    /// conversation.
    Reset,
    /// The answer to a [`ServerMessage::UiRequest`]. Ignored unless it fits
    /// the question ([`UiRequest::accepts`]).
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
#[non_exhaustive]
pub enum ServerMessage {
    /// The first message on a connection. A broadcast already reflected in
    /// it (`running`, an open question) may still arrive after it: frontends
    /// tolerate the repeat.
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
    RunStart { run: u64, prompt: String },
    /// An event of the run. Streamed text arrives coalesced: consecutive
    /// text (or thinking) deltas are merged, flushed at most 30 ms later.
    Event { run: u64, event: Box<AgentEvent> },
    /// The run ended — always sent once per `RunStart`, whatever happened
    /// to the events before it (an agent task that failed included).
    RunEnd {
        run: u64,
        /// How it ended.
        outcome: RunOutcome,
        /// What went wrong, for every outcome but `completed`.
        error: Option<String>,
        stats: Box<SessionStats>,
        /// The run's whole cost — its own turns, sub-agents, decision
        /// models and summaries — or `None` when part of it is unpriced.
        total_cost_usd: Option<f64>,
    },
    /// A plugin asks the user something. Answer with
    /// [`ClientMessage::UiResponse`]; the first fitting answer from any
    /// frontend wins.
    UiRequest { id: u64, request: UiRequest },
    /// A request is over (answered, withdrawn or timed out): close its dialog.
    UiResolved { id: u64, reason: ResolveReason },
    /// Something the user should know that is not part of a run's events (a
    /// failed agent task, a message the session refused).
    Notice { level: NoticeLevel, message: String },
    /// A browser component was offered or withdrawn: the full list.
    UiPlugins { ui_plugins: Vec<UiPlugin> },
    /// The session ended: nothing more will arrive, nothing sent is heard.
    Closed,
}

/// How a run ended ([`ServerMessage::RunEnd`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum RunOutcome {
    Completed,
    /// Stopped by `abort`, `reset` or `quit`.
    Aborted,
    /// Its input was rejected (an input check).
    Rejected,
    /// The model or the agent failed.
    Error,
}

/// Why a question closed ([`ServerMessage::UiResolved`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum ResolveReason {
    /// A frontend answered.
    Answered,
    /// Nobody answered in time: the asker got the safe default.
    TimedOut,
    /// The asker gave up (its run stopped): the safe default.
    Withdrawn,
}

/// How much a [`ServerMessage::Notice`] matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum NoticeLevel {
    Info,
    Warning,
    Error,
}

/// A question waiting for an answer, as listed in [`ServerMessage::Hello`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingUiRequest {
    pub id: u64,
    pub request: UiRequest,
}

/// What a plugin asks the user, and the answer each kind takes
/// ([`UiRequest::accepts`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
#[non_exhaustive]
pub enum UiRequest {
    /// Answer: `true` or `false`.
    Confirm {
        title: String,
        #[serde(default)]
        message: String,
    },
    /// Answer: one of `options`, or `null` (no choice).
    Select { title: String, options: Vec<String> },
    /// Answer: a string, or `null` (no text).
    Input {
        title: String,
        #[serde(default)]
        placeholder: String,
        /// Text the field starts with (an editor's prefill).
        #[serde(default)]
        value: String,
    },
    /// Shown, not answered.
    Notify {
        message: String,
        #[serde(default)]
        level: String,
    },
}

impl UiRequest {
    /// The answer when no frontend is attached, none answered in time, or
    /// the asker gave up: the safe one (`false`, no choice, no text).
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

    /// The title (a notification's message), for logs and notices.
    pub fn title(&self) -> &str {
        match self {
            UiRequest::Confirm { title, .. }
            | UiRequest::Select { title, .. }
            | UiRequest::Input { title, .. } => title,
            UiRequest::Notify { message, .. } => message,
        }
    }

    /// Whether `answer` fits this question: a boolean for `confirm`, one of
    /// the options or `null` for `select`, a string or `null` for `input`.
    /// (The string `"false"` is not a `confirm` answer.)
    pub fn accepts(&self, answer: &Json) -> bool {
        match (self, answer) {
            (UiRequest::Confirm { .. }, Json::Bool(_)) => true,
            (UiRequest::Select { .. } | UiRequest::Input { .. }, Json::Null) => true,
            (UiRequest::Select { options, .. }, Json::String(choice)) => options.contains(choice),
            (UiRequest::Input { .. }, Json::String(_)) => true,
            _ => false,
        }
    }
}

/// A browser component a plugin offers: an ES module the browser frontend
/// loads from `/ui-plugins/<name>.js`. UI plugins are trusted code: they run
/// in the page with its privileges.
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
    /// Set by the session each time the plugin is offered (a value sent
    /// with the offer is ignored): a frontend reloads a module whose
    /// version changed.
    #[serde(default, skip_deserializing)]
    pub version: u64,
}
