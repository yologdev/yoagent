//! Plugin log lines land in the GASP record of the run they belong to.
//!
//! A stand-in for a rutis plugin writes `tracing` events the way the
//! yoagent-rutis bridge's `log` does (target `yoagent_rutis::plugin`, a
//! `run_id` field). With the recorder's extension on the agent and its layer
//! on the subscriber, the line of this run is recorded as an observation on
//! it; a line of another run, and one without a run, are not.
//!
//! Its own test binary: it installs a global subscriber, once.
#![cfg(feature = "gasp")]

use std::sync::Arc;
use tracing_subscriber::layer::SubscriberExt;
use yoagent::extension::{Extension, ExtensionError, RunContext, RunHooks, TurnDecision};
use yoagent::gasp::{GaspRecorder, GoalRef, PLUGIN_LOG_TARGET};
use yoagent::provider::mock::MockResponse;
use yoagent::provider::{MockProvider, ModelConfig};
use yoagent::types::TurnContext;
use yoagent::Agent;

/// Logs like a plugin through the bridge, once per run, at its first request.
struct PluginStandIn;

struct StandInRun {
    run_id: String,
}

#[async_trait::async_trait]
impl Extension for PluginStandIn {
    fn name(&self) -> &str {
        "plugin-stand-in"
    }
    async fn start_run(&self, run: &RunContext<'_>) -> Result<Box<dyn RunHooks>, ExtensionError> {
        Ok(Box::new(StandInRun {
            run_id: run.run_id.to_string(),
        }))
    }
}

#[async_trait::async_trait]
impl RunHooks for StandInRun {
    async fn before_model(&mut self, _turn: &TurnContext<'_>) -> TurnDecision {
        let run_id = self.run_id.as_str();
        tracing::warn!(target: PLUGIN_LOG_TARGET, run_id, "pi command /todos not available");
        tracing::error!(target: PLUGIN_LOG_TARGET, run_id = "someone-else", "another agent's line");
        tracing::warn!(target: PLUGIN_LOG_TARGET, run_id = "", "a line with no run");
        TurnDecision::Continue
    }
}

fn events(dir: &std::path::Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(dir.join("state/events.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn plugin_log_lines_are_recorded_on_their_runs() {
    for (k, v) in [
        ("GIT_AUTHOR_NAME", "yoagent-test"),
        ("GIT_AUTHOR_EMAIL", "test@yolog.dev"),
        ("GIT_COMMITTER_NAME", "yoagent-test"),
        ("GIT_COMMITTER_EMAIL", "test@yolog.dev"),
    ] {
        std::env::set_var(k, v);
    }
    let dir = tempfile::tempdir().unwrap();
    let recorder = GaspRecorder::init(
        dir.path(),
        "test-agent",
        "w1",
        GoalRef::New {
            title: "plugin logs".into(),
        },
    )
    .await
    .unwrap();
    tracing::subscriber::set_global_default(
        tracing_subscriber::registry().with(recorder.plugin_log_layer()),
    )
    .unwrap();

    let mut agent = Agent::from_provider(
        MockProvider::new(vec![
            MockResponse::Text("one".into()),
            MockResponse::Text("two".into()),
        ]),
        ModelConfig::mock(),
    )
    .with_extension(Arc::new(PluginStandIn))
    .with_extension(recorder.extension());

    // Run 1: its own line recorded on it; another run's and a run-less one not.
    let (tx1, handle1) = recorder.recording_sender("first", None);
    // Keep the first recording open while the second starts: its end must not
    // switch off the second's routing.
    let keep_open = tx1.clone();
    agent.prompt_with_sender("first", tx1).await;
    let (tx2, handle2) = recorder.recording_sender("second", None);
    drop(keep_open);
    let run1 = handle1.await.unwrap().unwrap().expect("run 1 recorded");
    agent.prompt_with_sender("second", tx2).await;
    let run2 = handle2.await.unwrap().unwrap().expect("run 2 recorded");

    let all = events(dir.path());
    let observations: Vec<&serde_json::Value> = all
        .iter()
        .filter(|e| e["kind"] == "observation.created")
        .map(|e| &e["payload"])
        .collect();
    assert_eq!(observations.len(), 2, "{observations:#?}");
    for (payload, run) in observations.iter().zip([&run1, &run2]) {
        assert_eq!(
            payload["summary"], "pi command /todos not available",
            "{payload}"
        );
        assert_eq!(payload["observed_in"], run.0.as_str(), "{payload}");
        assert_eq!(payload["metadata"]["source"], "plugin");
        assert_eq!(payload["metadata"]["level"], "warn");
    }
}
