//! Extension example: redacting secrets from tool output.
//!
//! Demonstrates:
//!   - `after_tool` editing a call's `ToolOutput` before the model, the
//!     transcript and `ToolExecutionEnd` see it
//!   - why a redactor declares `filters_tool_output`: partial output
//!     (`ToolExecutionUpdate`) is sent while the tool runs, before
//!     `after_tool`, so the loop withholds it only while some installed
//!     extension says it filters output
//!
//! Run (offline, scripted model; exits non-zero on a regression):
//!   cargo run --example extension_redact
//! Or against a real model (`DEEPSEEK_API_KEY` or `ANTHROPIC_API_KEY`):
//!   cargo run --example extension_redact -- --live

mod support;

use support::*;
use yoagent::extension::*;
use yoagent::*;

const SECRET: &str = "sk-live-4f9a2c";

/// Replaces anything that looks like a live key.
#[derive(Clone)]
struct Redactor;

#[async_trait::async_trait]
impl RunHooks for Redactor {
    async fn after_tool(
        &self,
        _call: &ToolCallRequest<'_>,
        output: &mut ToolOutput,
    ) -> Result<(), ExtensionError> {
        for block in &mut output.result.content {
            if let Content::Text { text } = block {
                *text = redact(text);
            }
        }
        // Returning `Err` here would withhold the whole result instead (fail
        // closed): a redactor that fails must not leak.
        Ok(())
    }
}

/// Replace every `sk-live-…` token (up to the next whitespace).
fn redact(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("sk-live-") {
        out.push_str(&rest[..at]);
        out.push_str("[redacted]");
        let end = rest[at..]
            .find(char::is_whitespace)
            .map_or(rest.len(), |n| at + n);
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// Prints a pretend `.env` file, streaming its first line as partial output.
struct ReadEnv;

#[async_trait::async_trait]
impl AgentTool for ReadEnv {
    fn name(&self) -> &str {
        "read_env"
    }
    fn label(&self) -> &str {
        "Read .env"
    }
    fn description(&self) -> &str {
        "Show the project's environment file"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }
    async fn execute(
        &self,
        _params: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let first_line = format!("API_TOKEN={SECRET}");
        // Streamed to the consumer as `ToolExecutionUpdate`, unless withheld.
        if let Some(update) = &ctx.on_update {
            update(ToolResult {
                content: vec![Content::Text {
                    text: first_line.clone(),
                }],
                details: serde_json::Value::Null,
            });
        }
        Ok(ToolResult {
            content: vec![Content::Text {
                text: format!("{first_line}\nREGION=eu-west-1\n"),
            }],
            details: serde_json::Value::Null,
        })
    }
}

/// Runs the agent with `redactor` and reports what leaked where.
async fn demo(label: &str, redactor: ClonedHooks<Redactor>) -> (bool, bool, bool) {
    let script = vec![
        call("read_env", serde_json::json!({})),
        answer("The token is set and the region is eu-west-1."),
    ];
    let mut agent = new_agent(script)
        .with_tools(vec![Box::new(ReadEnv)])
        .with_extension(redactor);
    let events = run(
        &mut agent,
        "Which region does the .env configure? Use read_env.",
    )
    .await;

    let leaked_partial = events.iter().any(|e| {
        matches!(e, AgentEvent::ToolExecutionUpdate { partial_result, .. }
            if text_of(&partial_result.content).contains(SECRET))
    });
    let leaked_result = tool_results(&events)
        .iter()
        .any(|(_, text, _)| text.contains(SECRET));
    // What the model is shown on its next turn: the stored transcript.
    let leaked_transcript = format!("{:?}", agent.messages()).contains(SECRET);

    println!("{label}");
    for (_, text, _) in tool_results(&events) {
        println!("  tool result: {}", text.replace('\n', " | "));
    }
    println!("  secret in partial output: {leaked_partial}");
    println!("  secret in the final result / transcript: {leaked_result} / {leaked_transcript}");
    (leaked_partial, leaked_result, leaked_transcript)
}

#[tokio::main]
async fn main() {
    // A redactor that does not declare it: final results are clean, but the
    // partial output streamed while the tool ran was never filtered.
    let (partial, result, transcript) =
        demo("Undeclared redactor:", ClonedHooks::new("redact", Redactor)).await;
    check(
        partial && !result && !transcript,
        "an undeclared redactor cleans the result but not the partial output",
    );

    // Declared: the loop withholds partial output, so only the filtered
    // final result is ever sent.
    let (partial, result, transcript) = demo(
        "\nDeclared with .filters_tool_output():",
        ClonedHooks::new("redact", Redactor).filters_tool_output(),
    )
    .await;
    check(
        !partial && !result && !transcript,
        "a declared redactor leaks the secret nowhere",
    );
}
