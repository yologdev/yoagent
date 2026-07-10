//! Tests for the GASP bridge (feature `gasp`): event mapping, commit
//! behavior, goal reuse, and interrupted-run recovery.
#![cfg(feature = "gasp")]

use std::process::Command;
use yoagent::gasp::{GaspRecorder, GoalRef};
use yoagent::provider::mock::*;
use yoagent::provider::{MockProvider, ModelConfig};
use yoagent::*;

struct NoopTool;

#[async_trait::async_trait]
impl AgentTool for NoopTool {
    fn name(&self) -> &str {
        "noop"
    }
    fn label(&self) -> &str {
        "Noop"
    }
    fn description(&self) -> &str {
        "does nothing"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(
        &self,
        _params: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult {
            content: vec![Content::Text { text: "ok".into() }],
            details: serde_json::Value::Null,
        })
    }
}

fn tool_then_text_provider() -> MockProvider {
    MockProvider::new(vec![
        MockResponse::ToolCalls(vec![MockToolCall {
            provider_metadata: None,
            name: "noop".into(),
            arguments: serde_json::json!({}),
        }]),
        MockResponse::Text("done".into()),
    ])
}

fn event_kinds(repo: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(repo.join("state/events.jsonl"))
        .expect("events.jsonl exists")
        .lines()
        .map(|l| {
            serde_json::from_str::<serde_json::Value>(l).unwrap()["kind"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect()
}

#[tokio::test]
async fn records_a_full_run_with_expected_kinds_and_commit() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = GaspRecorder::init(
        dir.path(),
        "test-agent",
        "w1",
        GoalRef::New {
            title: "test goal".into(),
        },
    )
    .await
    .unwrap();

    let mut agent = Agent::from_provider(tool_then_text_provider(), ModelConfig::mock())
        .with_tools(vec![Box::new(NoopTool)]);
    let (tx, handle) = recorder.recording_sender("do the thing", None);
    agent.prompt_with_sender("do the thing", tx).await;
    let run_id = handle.await.unwrap().unwrap();

    let kinds = event_kinds(dir.path());
    // Semantic skeleton, in order (ops_applied lines interleave freely).
    let semantic: Vec<&str> = kinds
        .iter()
        .map(|s| s.as_str())
        .filter(|k| *k != "state.ops_applied")
        .collect();
    assert_eq!(
        semantic,
        vec![
            "goal.created",
            "run.started",
            "model.called",
            "model.finished",
            "tool.called",
            "tool.finished",
            "model.called",
            "model.finished",
            "run.finished",
        ],
        "full log: {kinds:?}"
    );

    // The run is committed (append-only history is what conformance walks).
    let log = Command::new("git")
        .args(["log", "--oneline"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&log.stdout);
    assert!(
        log.contains(&format!("run {run_id}")),
        "commit missing: {log}"
    );
    assert!(log.contains("completed"));
}

#[tokio::test]
async fn events_are_teed_to_the_forward_sender() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = GaspRecorder::init(
        dir.path(),
        "test-agent",
        "w1",
        GoalRef::New {
            title: "tee".into(),
        },
    )
    .await
    .unwrap();

    let (ui_tx, mut ui_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut agent = Agent::from_provider(MockProvider::text("hi"), ModelConfig::mock());
    let (tx, handle) = recorder.recording_sender("t", Some(ui_tx));
    agent.prompt_with_sender("t", tx).await;
    handle.await.unwrap().unwrap();

    let mut forwarded = 0;
    while ui_rx.try_recv().is_ok() {
        forwarded += 1;
    }
    assert!(forwarded > 0, "UI sender must receive the teed events");
}

#[tokio::test]
async fn goal_is_reused_across_runs_and_recorder_reopens() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = GaspRecorder::init(
        dir.path(),
        "test-agent",
        "w1",
        GoalRef::New {
            title: "persistent goal".into(),
        },
    )
    .await
    .unwrap();
    let goal = recorder.goal().clone();

    // Run 1.
    let mut agent = Agent::from_provider(MockProvider::text("one"), ModelConfig::mock());
    let (tx, handle) = recorder.recording_sender("run one", None);
    agent.prompt_with_sender("one", tx).await;
    handle.await.unwrap().unwrap();
    drop(recorder);

    // Reopen with the SAME goal — no new goal.created may appear.
    let recorder = GaspRecorder::open(
        dir.path().to_path_buf(),
        "test-agent",
        "w1",
        GoalRef::Existing(goal.clone()),
    )
    .await
    .unwrap();
    assert_eq!(recorder.goal(), &goal);

    let mut agent = Agent::from_provider(MockProvider::text("two"), ModelConfig::mock());
    let (tx, handle) = recorder.recording_sender("run two", None);
    agent.prompt_with_sender("two", tx).await;
    handle.await.unwrap().unwrap();

    let kinds = event_kinds(dir.path());
    assert_eq!(
        kinds.iter().filter(|k| *k == "goal.created").count(),
        1,
        "existing goal must be reused"
    );
    assert_eq!(kinds.iter().filter(|k| *k == "run.finished").count(), 2);
}

#[tokio::test]
async fn dropped_sender_without_agent_end_closes_run_as_interrupted() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = GaspRecorder::init(
        dir.path(),
        "test-agent",
        "w1",
        GoalRef::New {
            title: "crash".into(),
        },
    )
    .await
    .unwrap();

    // Simulate a crashed loop: send AgentStart, then drop the sender.
    let (tx, handle) = recorder.recording_sender("doomed", None);
    tx.send(AgentEvent::AgentStart).unwrap();
    drop(tx);
    handle.await.unwrap().unwrap();

    let kinds = event_kinds(dir.path());
    assert!(kinds.iter().any(|k| k == "run.finished"));
    let last_finish = std::fs::read_to_string(dir.path().join("state/events.jsonl"))
        .unwrap()
        .lines()
        .rfind(|l| l.contains("run.finished"))
        .unwrap()
        .to_string();
    assert!(last_finish.contains("interrupted"), "got: {last_finish}");
}
