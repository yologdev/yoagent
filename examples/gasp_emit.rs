//! GASP emission example: run a (mock) agent and record it into a GASP agent
//! repo — the repo IS the agent's durable state.
//!
//! Run with: cargo run --example gasp_emit --features gasp -- [repo-path]
//!
//! The emitted repo passes the GASP conformance checker
//! (github.com/yologdev/gasp) — yoagent's CI verifies exactly that. Point it
//! at a path of your choice and inspect `state/events.jsonl` and `git log`
//! afterwards.

use yoagent::gasp::{GaspRecorder, GoalRef};
use yoagent::provider::mock::*;
use yoagent::provider::{MockProvider, ModelConfig};
use yoagent::*;

/// A tiny tool so the log shows tool-call pairs, not just model calls.
struct TouchTool;

#[async_trait::async_trait]
impl AgentTool for TouchTool {
    fn name(&self) -> &str {
        "touch"
    }
    fn label(&self) -> &str {
        "Touch"
    }
    fn description(&self) -> &str {
        "pretends to touch a file"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}})
    }
    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult {
            content: vec![Content::Text {
                text: format!("touched {}", params["path"]),
            }],
            details: serde_json::Value::Null,
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let repo = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/yoagent-gasp-demo".into());

    let recorder = GaspRecorder::init(
        &repo,
        "demo-agent",
        "worker-1",
        GoalRef::New {
            title: "demonstrate GASP emission".into(),
        },
    )
    .await?;

    // Mock agent: one tool call, then a final answer — deterministic, no key.
    let provider = MockProvider::new(vec![
        MockResponse::ToolCalls(vec![MockToolCall {
            provider_metadata: None,
            name: "touch".into(),
            arguments: serde_json::json!({"path": "hello.txt"}),
        }]),
        MockResponse::Text("Done: touched hello.txt".into()),
    ]);
    let mut agent =
        Agent::from_provider(provider, ModelConfig::mock()).with_tools(vec![Box::new(TouchTool)]);

    let (tx, record_handle) = recorder.recording_sender("touch hello.txt", None);
    agent.prompt_with_sender("touch hello.txt", tx).await;
    let run_id = record_handle.await??.expect("run recorded");

    println!("recorded run {run_id} into {repo}");
    println!("inspect:  cat {repo}/state/events.jsonl");
    println!("verify:   git clone https://github.com/yologdev/gasp");
    println!("          cd gasp/conformance-check && cargo run -q -- {repo}");
    Ok(())
}
