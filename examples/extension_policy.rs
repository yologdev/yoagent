//! Extension example: a tool policy that denies and rewrites calls.
//!
//! Demonstrates:
//!   - `before_tool` returning `Allow`, `Modify` and `Deny`
//!   - `ClonedHooks` (the same hooks, cloned for every run)
//!   - `rechecks_modified_calls`: a later extension rewrote a call the policy
//!     had approved, so the policy judges the final arguments again
//!
//! Run (offline, scripted model; exits non-zero on a regression):
//!   cargo run --example extension_policy
//! Or against a real model (`DEEPSEEK_API_KEY` or `ANTHROPIC_API_KEY`):
//!   cargo run --example extension_policy -- --live

mod support;

use std::sync::{Arc, Mutex};
use support::*;
use yoagent::extension::*;
use yoagent::*;

/// The policy. Installed first, so it sees the model's own arguments.
#[derive(Clone)]
struct ShellPolicy;

#[async_trait::async_trait]
impl RunHooks for ShellPolicy {
    // `&self`: the calls of one response are judged concurrently.
    async fn before_tool(&self, call: &ToolCallRequest<'_>) -> ToolDecision {
        let cmd = call.args["cmd"].as_str().unwrap_or_default();
        if cmd.contains("rm -rf") {
            // The model sees this reason as the tool's error result.
            ToolDecision::Deny("rm -rf is not allowed in this workspace".into())
        } else if cmd.starts_with("git push") && !cmd.contains("--dry-run") {
            // Rewrite instead of refusing: the call runs with these arguments.
            ToolDecision::Modify(serde_json::json!({ "cmd": format!("{cmd} --dry-run") }))
        } else {
            ToolDecision::Allow
        }
    }
}

/// Expands a project's command aliases. Installed after the policy, so
/// without the recheck the policy would never see what an alias expands to.
#[derive(Clone)]
struct Aliases;

#[async_trait::async_trait]
impl RunHooks for Aliases {
    async fn before_tool(&self, call: &ToolCallRequest<'_>) -> ToolDecision {
        match call.args["cmd"].as_str() {
            Some("clean") => ToolDecision::Modify(serde_json::json!({ "cmd": "rm -rf ./build" })),
            _ => ToolDecision::Allow,
        }
    }
}

/// A pretend shell: records what it was asked to run, runs nothing.
struct Shell(Arc<Mutex<Vec<String>>>);

#[async_trait::async_trait]
impl AgentTool for Shell {
    fn name(&self) -> &str {
        "shell"
    }
    fn label(&self) -> &str {
        "Shell"
    }
    fn description(&self) -> &str {
        "Run a shell command (simulated)"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "cmd": { "type": "string" } },
            "required": ["cmd"]
        })
    }
    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let cmd = params["cmd"].as_str().unwrap_or_default().to_string();
        self.0.lock().unwrap().push(cmd.clone());
        Ok(ToolResult {
            content: vec![Content::Text {
                text: format!("$ {cmd}\n(simulated: ok)"),
            }],
            details: serde_json::Value::Null,
        })
    }
}

#[tokio::main]
async fn main() {
    let ran = Arc::new(Mutex::new(Vec::new()));

    // The scripted model asks for four commands in one response.
    let script = vec![
        calls(&[
            ("shell", serde_json::json!({ "cmd": "ls" })),
            (
                "shell",
                serde_json::json!({ "cmd": "git push origin main" }),
            ),
            ("shell", serde_json::json!({ "cmd": "rm -rf /" })),
            ("shell", serde_json::json!({ "cmd": "clean" })),
        ]),
        answer("Listed the files and dry-ran the push; two commands were refused."),
    ];

    let mut agent = new_agent(script)
        .with_system_prompt("You operate a shell through the `shell` tool.")
        .with_tools(vec![Box::new(Shell(ran.clone()))])
        // Extensions run in installation order: policy first, aliases second.
        .with_extension(ClonedHooks::new("shell-policy", ShellPolicy).rechecks_modified_calls())
        .with_extension(ClonedHooks::new("aliases", Aliases));

    let events = run(
        &mut agent,
        "Run `ls`, `git push origin main`, `rm -rf /` and `clean` with the shell tool.",
    )
    .await;

    for (_, text, is_error) in tool_results(&events) {
        let mark = if is_error { "denied" } else { "ran   " };
        println!("{mark} {}", text.lines().next().unwrap_or_default());
    }
    println!("answer: {}\n", last_text(&events));

    let ran = ran.lock().unwrap().clone();
    let denials = tool_results(&events)
        .into_iter()
        .filter(|(_, text, is_error)| *is_error && text.contains("rm -rf is not allowed"))
        .count();
    check(
        ran == ["ls", "git push origin main --dry-run"],
        "only `ls` and the dry-run push reached the shell",
    );
    check(
        denials == 2,
        "`rm -rf /` was denied, and so was `clean` once it expanded to `rm -rf ./build`",
    );
}
