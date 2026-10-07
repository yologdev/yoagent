//! Extension example: host policy over a whole delegation tree.
//!
//! Demonstrates:
//!   - `with_tree_extension`: the policy applies to this agent's runs and to
//!     every run they delegate to, at any depth; a child cannot remove it
//!   - `with_extension` for contrast: it sees this agent's own calls only
//!   - `RunContext::depth`, telling the parent's run (0) from a child's (1)
//!   - a delegation tool written by hand that passes the tree on with
//!     `Agent::delegated_from(&ctx)`, passes the parent's cancel down, and
//!     reports the child's spend with `ctx.report_delegated_run`
//!     (`SubAgentTool` does all three itself)
//!
//! Run (offline, scripted model; exits non-zero on a regression):
//!   cargo run --example extension_tree
//! Or against a real model (`DEEPSEEK_API_KEY` or `ANTHROPIC_API_KEY`):
//!   cargo run --example extension_tree -- --live

mod support;

use std::sync::{Arc, Mutex};
use support::*;
use yoagent::extension::*;
use yoagent::*;

type Log = Arc<Mutex<Vec<String>>>;

/// Writes only inside `/workspace/`, in every run of the tree.
struct WorkspaceOnly(Log);

/// One run's hooks: they know the run's delegation depth.
struct WorkspaceOnlyRun {
    depth: usize,
    log: Log,
}

#[async_trait::async_trait]
impl Extension for WorkspaceOnly {
    fn name(&self) -> &str {
        "workspace-only"
    }
    async fn start_run(&self, run: &RunContext<'_>) -> Result<Box<dyn RunHooks>, ExtensionError> {
        Ok(Box::new(WorkspaceOnlyRun {
            depth: run.depth,
            log: self.0.clone(),
        }))
    }
}

#[async_trait::async_trait]
impl RunHooks for WorkspaceOnlyRun {
    async fn before_tool(&self, call: &ToolCallRequest<'_>) -> ToolDecision {
        if call.tool_name != "write" {
            return ToolDecision::Allow;
        }
        let path = call.args["path"].as_str().unwrap_or_default();
        let allowed = path.starts_with("/workspace/");
        self.log.lock().unwrap().push(format!(
            "depth {}: write {path} -> {}",
            self.depth,
            if allowed { "allowed" } else { "denied" }
        ));
        if allowed {
            ToolDecision::Allow
        } else {
            ToolDecision::Deny(format!("{path} is outside /workspace"))
        }
    }
}

/// An ordinary extension: counts the calls it is shown.
#[derive(Clone)]
struct CountCalls(Arc<Mutex<usize>>);

#[async_trait::async_trait]
impl RunHooks for CountCalls {
    async fn before_tool(&self, _call: &ToolCallRequest<'_>) -> ToolDecision {
        *self.0.lock().unwrap() += 1;
        ToolDecision::Allow
    }
}

/// A pretend file writer.
struct Write;

#[async_trait::async_trait]
impl AgentTool for Write {
    fn name(&self) -> &str {
        "write"
    }
    fn label(&self) -> &str {
        "Write file"
    }
    fn description(&self) -> &str {
        "Write a file (simulated)"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "path": { "type": "string" }, "text": { "type": "string" } },
            "required": ["path"]
        })
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        _: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult {
            content: vec![Content::Text {
                text: format!("wrote {}", args["path"].as_str().unwrap_or_default()),
            }],
            details: serde_json::Value::Null,
        })
    }
}

/// A delegation tool written by hand: its child is a plain `Agent`.
struct AskReviewer;

#[async_trait::async_trait]
impl AgentTool for AskReviewer {
    fn name(&self) -> &str {
        "ask_reviewer"
    }
    fn label(&self) -> &str {
        "Ask reviewer"
    }
    fn description(&self) -> &str {
        "Ask a reviewer agent to look at the change and leave notes"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "question": { "type": "string" } },
            "required": ["question"]
        })
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let script = vec![
            call(
                "write",
                serde_json::json!({ "path": "/tmp/review-notes.md" }),
            ),
            answer("LGTM, but I could not save my notes."),
        ];
        let mut reviewer = new_agent(script)
            .with_system_prompt("You review changes. Save notes with `write`.")
            .with_tools(vec![Box::new(Write)])
            // The child becomes a delegated run of this call: the calling
            // run's tree extensions apply to it, at depth 1.
            .delegated_from(&ctx);
        let question = args["question"].as_str().unwrap_or("Review the change.");

        // `prompt` runs the child on its own task, so this tool can watch the
        // parent's cancel token meanwhile and pass a cancel down with
        // `abort()`; the child then ends `Aborted` and still sends `AgentEnd`.
        let mut rx = reviewer.prompt(question).await;
        let mut events = Vec::new();
        let mut cancelled = false;
        loop {
            tokio::select! {
                event = rx.recv() => match event {
                    Some(event) => events.push(event),
                    None => break,
                },
                _ = ctx.cancel.cancelled(), if !cancelled => {
                    reviewer.abort();
                    cancelled = true;
                }
            }
        }
        reviewer.finish().await;

        // Report the child's stats before returning, on every path (a
        // cancelled or failed delegation still spent money): this is how its
        // spend reaches the parent's `SessionStats::sub_agents`, and any
        // `Budget` the parent runs. `SubAgentTool` does the same.
        if let Some(AgentEvent::AgentEnd { stats, .. }) = events.last() {
            ctx.report_delegated_run(stats.clone());
        }
        if cancelled {
            return Err(ToolError::Cancelled);
        }
        Ok(ToolResult {
            content: vec![Content::Text {
                text: last_text(&events),
            }],
            details: serde_json::Value::Null,
        })
    }
}

#[tokio::main]
async fn main() {
    let log: Log = Arc::default();
    let parent_calls = Arc::new(Mutex::new(0));

    let fixer = sub_agent(
        "fixer",
        vec![
            call("write", serde_json::json!({ "path": "/etc/hosts" })),
            call(
                "write",
                serde_json::json!({ "path": "/workspace/fix.patch" }),
            ),
            answer("Patched in /workspace/fix.patch."),
        ],
    )
    .with_description("Writes fixes. Give it a task.")
    .with_tools(vec![Arc::new(Write)]);

    let script = vec![
        calls(&[
            ("write", serde_json::json!({ "path": "/workspace/PLAN.md" })),
            (
                "fixer",
                serde_json::json!({ "task": "Fix the failing build." }),
            ),
            (
                "ask_reviewer",
                serde_json::json!({ "question": "Is the plan sound?" }),
            ),
        ]),
        answer("Plan written, fix delegated, review requested."),
    ];
    let mut agent = new_agent(script)
        .with_system_prompt(
            "Write a plan to /workspace/PLAN.md, delegate the fix to `fixer`, \
             and ask the reviewer about the plan.",
        )
        .with_tools(vec![Box::new(Write), Box::new(AskReviewer)])
        .with_sub_agent(fixer)
        .with_tree_extension(WorkspaceOnly(log.clone()))
        .with_extension(ClonedHooks::new("count", CountCalls(parent_calls.clone())));

    let events = run(&mut agent, "Fix the build.").await;
    println!("answer: {}\n", last_text(&events));

    let mut decisions = log.lock().unwrap().clone();
    decisions.sort();
    for line in &decisions {
        println!("{line}");
    }
    let parent_calls = *parent_calls.lock().unwrap();
    println!("\nthe ordinary extension was shown {parent_calls} call(s)");

    check(
        decisions
            == [
                "depth 0: write /workspace/PLAN.md -> allowed",
                "depth 1: write /etc/hosts -> denied",
                "depth 1: write /tmp/review-notes.md -> denied",
                "depth 1: write /workspace/fix.patch -> allowed",
            ],
        "the tree policy judged the parent's write and both children's",
    );
    check(
        parent_calls == 3,
        "the ordinary extension saw only the parent's three calls",
    );

    let Some(AgentEvent::AgentEnd { stats, .. }) = events.last() else {
        panic!("the run ended without AgentEnd");
    };
    println!(
        "delegated runs in the parent's stats: {}",
        stats.sub_agents.runs
    );
    check(
        stats.sub_agents.runs == 2,
        "the parent's stats count both delegated runs (fixer and the hand-written reviewer)",
    );
}
