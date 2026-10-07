//! Extension example: an audit log.
//!
//! Demonstrates:
//!   - `on_event`: every `AgentEvent` of the run, in order, before the
//!     consumer gets it. It takes `&self` and must not block, so the hooks
//!     buffer records behind a `Mutex`
//!   - `finish`: called once however the run ends (here: completed, and
//!     rejected by another extension's `on_input`), with every event already
//!     observed; it writes the run's records as JSON lines to stdout
//!   - `RunContext`: the run's id and the host's label (`with_run_label`)
//!
//! Run (offline, scripted model; exits non-zero on a regression):
//!   cargo run --example extension_audit
//! Or against a real model (`DEEPSEEK_API_KEY` or `ANTHROPIC_API_KEY`):
//!   cargo run --example extension_audit -- --live

mod support;

use std::sync::{Arc, Mutex};
use support::*;
use yoagent::extension::*;
use yoagent::*;

/// Every record written, for the checks at the end (a real sink would be a
/// file or a log pipeline).
type Sink = Arc<Mutex<Vec<serde_json::Value>>>;

struct Audit(Sink);

struct AuditRun {
    run_id: String,
    label: Option<String>,
    records: Mutex<Vec<serde_json::Value>>,
    sink: Sink,
}

#[async_trait::async_trait]
impl Extension for Audit {
    fn name(&self) -> &str {
        "audit"
    }
    async fn start_run(&self, run: &RunContext<'_>) -> Result<Box<dyn RunHooks>, ExtensionError> {
        Ok(Box::new(AuditRun {
            run_id: run.run_id.to_string(),
            label: run.label.map(String::from),
            records: Mutex::default(),
            sink: self.0.clone(),
        }))
    }
}

#[async_trait::async_trait]
impl RunHooks for AuditRun {
    fn on_event(&self, event: &AgentEvent) {
        let record = match event {
            AgentEvent::ToolExecutionStart {
                tool_name, args, ..
            } => {
                serde_json::json!({ "event": "tool_call", "tool": tool_name, "args": args })
            }
            AgentEvent::ToolExecutionEnd {
                tool_name,
                is_error,
                ..
            } => serde_json::json!({ "event": "tool_done", "tool": tool_name, "error": is_error }),
            AgentEvent::MessageEnd {
                message: AgentMessage::Llm(Message::Assistant { usage, model, .. }),
            } => serde_json::json!({
                "event": "model", "model": model, "input": usage.input, "output": usage.output
            }),
            AgentEvent::InputRejected { reason } => {
                serde_json::json!({ "event": "rejected", "reason": reason })
            }
            _ => return,
        };
        self.records.lock().unwrap().push(record);
    }

    async fn finish(&mut self, outcome: &RunOutcome) {
        let end = match outcome.end() {
            RunEnd::Completed => "completed".to_string(),
            RunEnd::Stopped { reason } => format!("stopped: {reason}"),
            RunEnd::Rejected { reason } => format!("rejected: {reason}"),
            RunEnd::Cancelled => "cancelled".to_string(),
            RunEnd::Failed { error, .. } => format!("failed: {error}"),
            // `RunEnd` is `#[non_exhaustive]`.
            other => format!("{other:?}"),
        };
        let mut records = std::mem::take(&mut *self.records.lock().unwrap());
        records.push(serde_json::json!({ "event": "end", "outcome": end }));
        for mut record in records {
            record["run"] = self.run_id[..8].into();
            record["label"] = self.label.clone().into();
            println!("{record}");
            self.sink.lock().unwrap().push(record);
        }
    }
}

/// Another extension: rejects prompts that carry a password.
#[derive(Clone)]
struct NoPasswords;

#[async_trait::async_trait]
impl RunHooks for NoPasswords {
    async fn on_input(&mut self, input: &InputContext<'_>) -> InputDecision {
        if input.text.contains("password=") {
            InputDecision::Reject("the prompt contains a password".into())
        } else {
            InputDecision::Pass
        }
    }
}

/// A pretend clock.
struct Now;

#[async_trait::async_trait]
impl AgentTool for Now {
    fn name(&self) -> &str {
        "now"
    }
    fn label(&self) -> &str {
        "Now"
    }
    fn description(&self) -> &str {
        "The current time (simulated)"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }
    async fn execute(&self, _: serde_json::Value, _: ToolContext) -> Result<ToolResult, ToolError> {
        Ok(ToolResult {
            content: vec![Content::Text {
                text: "2026-10-07T09:00:00Z".into(),
            }],
            details: serde_json::Value::Null,
        })
    }
}

#[tokio::main]
async fn main() {
    let sink: Sink = Arc::default();
    let script = vec![
        call("now", serde_json::json!({})),
        answer("It is 09:00 UTC."),
    ];
    let mut agent = new_agent(script)
        .with_tools(vec![Box::new(Now)])
        .with_run_label("session-42")
        .with_extension(Audit(sink.clone()))
        .with_extension(ClonedHooks::new("no-passwords", NoPasswords));

    run(&mut agent, "What time is it? Use the `now` tool.").await;
    run(&mut agent, "Log in with password=hunter2").await;

    let sink = sink.lock().unwrap();
    let kinds: Vec<&str> = sink.iter().filter_map(|r| r["event"].as_str()).collect();
    let ends: Vec<&str> = sink.iter().filter_map(|r| r["outcome"].as_str()).collect();
    let runs: std::collections::HashSet<&str> =
        sink.iter().filter_map(|r| r["run"].as_str()).collect();

    println!();
    check(
        kinds
            == [
                "model",
                "tool_call",
                "tool_done",
                "model",
                "end",
                "rejected",
                "end",
            ],
        "the first run's model calls and tool call were logged in order, then the rejection",
    );
    check(
        ends == ["completed", "rejected: the prompt contains a password"],
        "finish reported how each run ended",
    );
    check(
        runs.len() == 2 && sink.iter().all(|r| r["label"] == "session-42"),
        "each run has its own id and carries the host's label",
    );
}
