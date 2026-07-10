//! Conversation session trees — branching history with checkpoints.
//!
//! A [`Session`] stores messages as a **tree**, not a list: every entry has an
//! `id` and a `parent_id`, the *head* points at the current branch tip, and
//! appending after a [`seek`](Session::seek) creates a new branch instead of
//! overwriting history. This is the primitive behind "edit that earlier
//! message and re-run", checkpoints, and conversation forking.
//!
//! Persistence is JSONL — one entry per line, append-friendly, diff-friendly.
//! (In GASP terms this is the shape of the `transcripts/` tier; the semantic
//! event log is a separate adapter.)
//!
//! # Typical flow
//!
//! ```no_run
//! use yoagent::{Agent, Session, provider::ModelConfig};
//!
//! # #[tokio::main]
//! # async fn main() {
//! let mut session = Session::new();
//!
//! // Run the agent, then record the new messages into the session.
//! let mut agent = Agent::from_config(ModelConfig::anthropic("claude-sonnet-5", "Sonnet 5"));
//! let mut rx = agent.prompt("hello").await;
//! while rx.recv().await.is_some() {}
//! agent.finish().await;
//! session.append_new(agent.messages());
//!
//! // Checkpoint, keep working...
//! session.checkpoint("after-hello").unwrap();
//!
//! // Later: fork from the checkpoint and try a different direction.
//! session.seek_checkpoint("after-hello").unwrap();
//! let mut agent = Agent::from_config(ModelConfig::anthropic("claude-sonnet-5", "Sonnet 5"))
//!     .with_messages(session.path_messages());
//! // ...prompt again; append_new() records the new branch
//! # }
//! ```

use crate::types::{now_ms, AgentMessage};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// One node in a session tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionEntry {
    /// Unique id within the session (`"e1"`, `"e2"`, ...).
    pub id: String,
    /// Parent entry id; `None` for a root.
    pub parent_id: Option<String>,
    /// The message this entry carries.
    pub message: AgentMessage,
    /// Creation time (ms since epoch).
    pub timestamp: u64,
    /// Checkpoint label, if this entry was labeled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Error from session operations.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SessionError {
    /// No entry with the given id exists in this session.
    #[error("unknown session entry: {0}")]
    UnknownEntry(String),
    /// No checkpoint with the given label exists in this session.
    #[error("unknown checkpoint label: {0}")]
    UnknownCheckpoint(String),
    /// A JSONL line failed to parse.
    #[error("failed to parse session line {line}: {error}")]
    Parse { line: usize, error: String },
    /// The session is empty where an entry was required.
    #[error("session is empty")]
    Empty,
}

/// A conversation history tree: append advances the head; seek + append forks.
#[derive(Debug, Clone, Default)]
pub struct Session {
    entries: Vec<SessionEntry>,
    head: Option<String>,
    next_seq: u64,
}

impl Session {
    /// Create an empty session.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a linear session from an existing flat history (e.g. an agent's
    /// messages). Head ends at the last message.
    pub fn from_messages(messages: &[AgentMessage]) -> Self {
        let mut s = Self::new();
        for m in messages {
            s.append(m.clone());
        }
        s
    }

    /// Append a message as a child of the current head and advance the head.
    /// After a [`seek`](Self::seek), this creates a new branch. Returns the
    /// new entry's id.
    pub fn append(&mut self, message: AgentMessage) -> String {
        self.next_seq += 1;
        let id = format!("e{}", self.next_seq);
        self.entries.push(SessionEntry {
            id: id.clone(),
            parent_id: self.head.clone(),
            message,
            timestamp: now_ms(),
            label: None,
        });
        self.head = Some(id.clone());
        id
    }

    /// Append every message of `full_history` beyond the current path length
    /// — the typical post-run sync from [`Agent::messages`](crate::Agent::messages).
    pub fn append_new(&mut self, full_history: &[AgentMessage]) {
        let known = self.path_ids().len();
        for m in full_history.iter().skip(known) {
            self.append(m.clone());
        }
    }

    /// Current head entry id, if any.
    pub fn head(&self) -> Option<&str> {
        self.head.as_deref()
    }

    /// Move the head to an existing entry (a fork point). The next
    /// [`append`](Self::append) starts a new branch from there; existing
    /// branches are never deleted.
    pub fn seek(&mut self, entry_id: &str) -> Result<(), SessionError> {
        if self.entry(entry_id).is_none() {
            return Err(SessionError::UnknownEntry(entry_id.to_string()));
        }
        self.head = Some(entry_id.to_string());
        Ok(())
    }

    /// Label the current head as a checkpoint.
    pub fn checkpoint(&mut self, label: impl Into<String>) -> Result<(), SessionError> {
        let head = self.head.clone().ok_or(SessionError::Empty)?;
        let label = label.into();
        let entry = self
            .entries
            .iter_mut()
            .find(|e| e.id == head)
            .expect("head always exists");
        entry.label = Some(label);
        Ok(())
    }

    /// Move the head to the entry labeled `label`.
    pub fn seek_checkpoint(&mut self, label: &str) -> Result<(), SessionError> {
        let id = self
            .entries
            .iter()
            .find(|e| e.label.as_deref() == Some(label))
            .map(|e| e.id.clone())
            .ok_or_else(|| SessionError::UnknownCheckpoint(label.to_string()))?;
        self.head = Some(id);
        Ok(())
    }

    /// The messages on the root→head path — what an agent should see when
    /// resuming this branch. Pair with
    /// [`Agent::with_messages`](crate::Agent::with_messages).
    pub fn path_messages(&self) -> Vec<AgentMessage> {
        self.path_ids()
            .iter()
            .map(|id| self.entry(id).expect("path ids exist").message.clone())
            .collect()
    }

    /// Entry ids on the root→head path.
    pub fn path_ids(&self) -> Vec<String> {
        let mut ids = Vec::new();
        let mut cursor = self.head.clone();
        while let Some(id) = cursor {
            cursor = self.entry(&id).and_then(|e| e.parent_id.clone());
            ids.push(id);
        }
        ids.reverse();
        ids
    }

    /// All entries, in insertion order (all branches).
    pub fn entries(&self) -> &[SessionEntry] {
        &self.entries
    }

    /// Look up an entry by id.
    pub fn entry(&self, id: &str) -> Option<&SessionEntry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// Ids of all leaf entries — one per branch.
    pub fn branch_tips(&self) -> Vec<&str> {
        let mut has_child: HashMap<&str, bool> = HashMap::new();
        for e in &self.entries {
            has_child.entry(e.id.as_str()).or_insert(false);
            if let Some(p) = &e.parent_id {
                has_child.insert(p.as_str(), true);
            }
        }
        self.entries
            .iter()
            .filter(|e| !has_child.get(e.id.as_str()).copied().unwrap_or(false))
            .map(|e| e.id.as_str())
            .collect()
    }

    /// Direct children of an entry.
    pub fn children(&self, id: &str) -> Vec<&SessionEntry> {
        self.entries
            .iter()
            .filter(|e| e.parent_id.as_deref() == Some(id))
            .collect()
    }

    /// Serialize to JSONL — one entry per line, insertion order. The head is
    /// restored as the **last line's entry** on load; seek before saving is
    /// not preserved (append after seeking and the new entry becomes both
    /// last line and head).
    pub fn to_jsonl(&self) -> String {
        self.entries
            .iter()
            .map(|e| serde_json::to_string(e).expect("session entries serialize"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Load from JSONL produced by [`to_jsonl`](Self::to_jsonl). Head is set
    /// to the last line's entry.
    pub fn from_jsonl(s: &str) -> Result<Self, SessionError> {
        let mut session = Self::new();
        for (i, line) in s.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let entry: SessionEntry =
                serde_json::from_str(line).map_err(|e| SessionError::Parse {
                    line: i + 1,
                    error: e.to_string(),
                })?;
            // Track the numeric suffix so future appends can't collide.
            if let Some(n) = entry
                .id
                .strip_prefix('e')
                .and_then(|n| n.parse::<u64>().ok())
            {
                session.next_seq = session.next_seq.max(n);
            }
            session.head = Some(entry.id.clone());
            session.entries.push(entry);
        }
        Ok(session)
    }
}
