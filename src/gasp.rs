//! GASP bridge — record agent runs into a [GASP](https://github.com/yologdev/gasp)
//! agent repo (feature `gasp`).
//!
//! GASP ("the repo is the agent") keeps an agent's durable self in a git
//! repository: an append-only semantic event log (`state/events.jsonl`) that
//! folds into a typed goal/run/model/tool graph, with restore = `clone +
//! replay`. This module is the bridge between yoagent's [`AgentEvent`] stream
//! and the [`yoagent_state`] reference runtime — **zero agent-loop changes**;
//! the recorder is just another consumer of the event stream.
//!
//! ```no_run
//! use yoagent::{Agent, gasp::{GaspRecorder, GoalRef}, provider::ModelConfig};
//!
//! # #[tokio::main]
//! # async fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let recorder = GaspRecorder::init(
//!     "./my-agent-repo",
//!     "my-agent",
//!     "worker-1",
//!     GoalRef::New { title: "ship the feature".into() },
//! )
//! .await?;
//!
//! let mut agent = Agent::from_config(ModelConfig::anthropic("claude-sonnet-5", "Sonnet 5"));
//! let (tx, record_handle) = recorder.recording_sender("implement the parser", None);
//! agent.prompt_with_sender("implement the parser", tx).await;
//! let run_id = record_handle.await??; // events appended + committed
//! # let _ = run_id; Ok(())
//! # }
//! ```
//!
//! The semantic log records **summaries** (task, model, tool names, outcome)
//! — full conversations belong in GASP's cold `transcripts/` tier, which is
//! exactly the shape of [`Session::to_jsonl`](crate::Session::to_jsonl).

use crate::types::*;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use yoagent_state::{
    ActorRef, GitEventStore, Goal, YoAgentModelCalled, YoAgentModelFinished, YoAgentRunFinished,
    YoAgentRunStarted, YoAgentState, YoAgentStateAdapter, YoAgentStateSink, YoAgentToolCalled,
    YoAgentToolFinished,
};
pub use yoagent_state::{GoalId, RunId, StateError};

/// Which GASP goal recorded runs chain to.
#[derive(Debug, Clone)]
pub enum GoalRef {
    /// Use an existing goal (persist the id in your app's config).
    Existing(GoalId),
    /// Create a new goal with this title when the recorder opens.
    New { title: String },
}

/// Records an agent's [`AgentEvent`] stream into a GASP agent repo.
///
/// One recorder = one writer (`worker_id` names it in the repo lease) and one
/// goal; each `recording_sender` call records one run. Events are appended to
/// `state/events.jsonl` as they arrive and committed when the run closes, so
/// the git history stays append-only (GASP conformance check 4).
pub struct GaspRecorder {
    state: YoAgentState<GitEventStore>,
    store: GitEventStore,
    actor: ActorRef,
    goal: GoalId,
}

impl GaspRecorder {
    /// Initialize a fresh agent repo at `root` (git init + minimal manifest)
    /// and open a recorder on it.
    pub async fn init(
        root: impl AsRef<std::path::Path>,
        agent_id: &str,
        worker_id: &str,
        goal: GoalRef,
    ) -> Result<Self, StateError> {
        let store = yoagent_state::init_agent_repo(root, agent_id, worker_id)?;
        Self::with_store(store, agent_id, goal).await
    }

    /// Open a recorder on an existing GASP agent repo.
    pub async fn open(
        root: impl Into<std::path::PathBuf>,
        agent_id: &str,
        worker_id: &str,
        goal: GoalRef,
    ) -> Result<Self, StateError> {
        let store = GitEventStore::open(root, worker_id)?;
        Self::with_store(store, agent_id, goal).await
    }

    async fn with_store(
        store: GitEventStore,
        agent_id: &str,
        goal: GoalRef,
    ) -> Result<Self, StateError> {
        let actor = ActorRef::agent(agent_id);
        let state = YoAgentState::load(store.clone()).await?;

        // A run left open by a crashed/killed process would make the next
        // record_run_started fail — close it explicitly instead.
        if let Some(stale) = state.resume_open_run().await? {
            tracing::warn!(run = %stale, "closing stale open run as interrupted");
            state
                .record_run_finished(actor.clone(), stale, "interrupted")
                .await?;
        }

        let goal = match goal {
            GoalRef::Existing(id) => id,
            GoalRef::New { title } => {
                let id = GoalId::generate();
                state
                    .record_goal(Goal::new(id.clone(), title.clone(), title, actor.clone()))
                    .await?;
                id
            }
        };

        Ok(Self {
            state,
            store,
            actor,
            goal,
        })
    }

    /// The goal this recorder chains runs to (persist it to reuse across
    /// processes via [`GoalRef::Existing`]).
    pub fn goal(&self) -> &GoalId {
        &self.goal
    }

    /// Returns a sender to pass to
    /// [`Agent::prompt_with_sender`](crate::Agent::prompt_with_sender) (or
    /// the raw loop) and a handle that resolves to the recorded [`RunId`]
    /// once the run closes and is committed.
    ///
    /// Every event is optionally forwarded to `forward` (your UI) — the
    /// recorder tees, it doesn't consume exclusively.
    pub fn recording_sender(
        &self,
        task: impl Into<String>,
        forward: Option<mpsc::UnboundedSender<AgentEvent>>,
    ) -> (
        mpsc::UnboundedSender<AgentEvent>,
        JoinHandle<Result<RunId, StateError>>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let sink = YoAgentStateAdapter::new(self.state.clone(), self.actor.clone());
        let store = self.store.clone();
        let goal = self.goal.clone();
        let task = task.into();
        let handle = tokio::spawn(consume(sink, store, goal, task, rx, forward));
        (tx, handle)
    }
}

/// Map the event stream onto the GASP sink. Runs in its own task; ends when
/// the sender side is dropped (the loop finished).
async fn consume(
    sink: YoAgentStateAdapter<GitEventStore>,
    store: GitEventStore,
    goal: GoalId,
    task: String,
    mut rx: mpsc::UnboundedReceiver<AgentEvent>,
    forward: Option<mpsc::UnboundedSender<AgentEvent>>,
) -> Result<RunId, StateError> {
    let run_id = RunId::generate();
    let mut started = false;
    let mut finished = false;
    let mut turn: usize = 0;
    let mut outcome = "interrupted".to_string();

    while let Some(event) = rx.recv().await {
        match &event {
            AgentEvent::AgentStart => {
                sink.on_run_started(YoAgentRunStarted {
                    run_id: run_id.clone(),
                    task: task.clone(),
                    metadata: serde_json::json!({}),
                })
                .await?;
                started = true;
            }
            AgentEvent::MessageEnd {
                message:
                    AgentMessage::Llm(Message::Assistant {
                        content,
                        model,
                        stop_reason,
                        ..
                    }),
            } if started => {
                turn += 1;
                sink.on_model_called(YoAgentModelCalled {
                    run_id: run_id.clone(),
                    model: model.clone(),
                    prompt_summary: if turn == 1 {
                        summarize(&task)
                    } else {
                        format!("turn {turn}")
                    },
                })
                .await?;
                let text = content
                    .iter()
                    .find_map(|c| match c {
                        Content::Text { text } if !text.is_empty() => Some(text.as_str()),
                        _ => None,
                    })
                    .unwrap_or("(no text)");
                sink.on_model_finished(YoAgentModelFinished {
                    run_id: run_id.clone(),
                    model: model.clone(),
                    output_summary: summarize(text),
                })
                .await?;
                outcome = outcome_for(stop_reason).to_string();
            }
            AgentEvent::ToolExecutionStart {
                tool_name, args, ..
            } if started => {
                sink.on_tool_called(YoAgentToolCalled {
                    run_id: run_id.clone(),
                    tool: tool_name.clone(),
                    input_summary: summarize(&args.to_string()),
                })
                .await?;
            }
            AgentEvent::ToolExecutionEnd {
                tool_name,
                result,
                is_error,
                ..
            } if started => {
                let text = result
                    .content
                    .iter()
                    .find_map(|c| match c {
                        Content::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .unwrap_or("(no output)");
                sink.on_tool_finished(YoAgentToolFinished {
                    run_id: run_id.clone(),
                    tool: tool_name.clone(),
                    output_summary: summarize(text),
                    success: !is_error,
                })
                .await?;
            }
            AgentEvent::AgentEnd { .. } if started && !finished => {
                sink.on_run_finished(YoAgentRunFinished {
                    run_id: run_id.clone(),
                    outcome: outcome.clone(),
                    metadata: serde_json::json!({}),
                })
                .await?;
                finished = true;
            }
            _ => {}
        }
        if let Some(fwd) = &forward {
            let _ = fwd.send(event);
        }
    }

    // Loop dropped the sender without AgentEnd (abort, filter reject, panic):
    // close the run so the log never carries an open run across processes.
    if started && !finished {
        sink.on_run_finished(YoAgentRunFinished {
            run_id: run_id.clone(),
            outcome: "interrupted".into(),
            metadata: serde_json::json!({}),
        })
        .await?;
    }

    if started {
        store.commit_run(&run_id, &goal, &outcome, &[])?;
    }
    Ok(run_id)
}

/// One-line, bounded summary for semantic events — full content belongs in
/// the transcripts tier, not the event log.
fn summarize(text: &str) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= 200 {
        one_line
    } else {
        let truncated: String = one_line.chars().take(200).collect();
        format!("{truncated}…")
    }
}

fn outcome_for(stop_reason: &StopReason) -> &'static str {
    match stop_reason {
        StopReason::Stop | StopReason::ToolUse => "completed",
        StopReason::Length => "truncated",
        StopReason::Error => "error",
        StopReason::Aborted => "aborted",
        StopReason::Refusal => "refused",
    }
}
