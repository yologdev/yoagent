//! Plugin log lines land in the GASP record of the run they belong to.
//!
//! A stand-in for a rutis plugin writes `tracing` events the way the
//! yoagent-rutis bridge's `log` does (target `yoagent_rutis::plugin`, a
//! `run_id` field). With the recorder's extension on the agent and its layer
//! on the subscriber, the line of this run is recorded as an observation on
//! it; a line of another run, and one without a run, are not.
//!
//! Its own test binary with a single test: it installs a global subscriber.
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

#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_log_line_is_recorded_on_its_run() {
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
        MockProvider::new(vec![MockResponse::Text("done".into())]),
        ModelConfig::mock(),
    )
    .with_extension(Arc::new(PluginStandIn))
    .with_extension(recorder.extension());
    let (tx, handle) = recorder.recording_sender("go", None);
    agent.prompt_with_sender("go", tx).await;
    let run_id = handle.await.unwrap().unwrap().expect("run recorded");

    let events: Vec<serde_json::Value> =
        std::fs::read_to_string(dir.path().join("state/events.jsonl"))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
    let observations: Vec<&serde_json::Value> = events
        .iter()
        .filter(|e| e["kind"] == "observation.created")
        .collect();
    assert_eq!(observations.len(), 1, "{observations:#?}");
    let payload = &observations[0]["payload"];
    assert_eq!(
        payload["summary"], "pi command /todos not available",
        "{payload}"
    );
    assert_eq!(payload["observed_in"], run_id.0.as_str(), "{payload}");
    assert_eq!(payload["metadata"]["source"], "plugin");
    assert_eq!(payload["metadata"]["level"], "warn");
}
