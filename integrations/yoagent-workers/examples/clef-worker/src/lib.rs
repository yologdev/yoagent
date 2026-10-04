//! A yoagent agent in a Cloudflare Worker whose tool calls are gated by Clef.
//!
//! `POST /` with a prompt as the body (and `Authorization: Bearer <RUN_TOKEN>`)
//! runs one bounded agent on DeepSeek with two tools over a fixed set of notes:
//! `list_notes` (read-only) and `delete_note` (destructive, simulated). Before
//! any tool runs, yoagent's `ToolGate` asks Clef — through the Worker's Workers
//! AI binding, no API token — whether the call is destructive and whether the
//! user asked for it, and denies destructive calls nobody asked for. The model
//! sees a denial as the tool's error and carries on.
//!
//! The response is JSON: the agent's answer and every tool call with its
//! outcome, so a denial is visible.

use worker::*;
use yoagent::context::ExecutionLimits;
use yoagent::decision::ToolGate;
use yoagent::provider::{ModelConfig, OpenAiCompatProvider};
use yoagent::*;

const NOTES: &[(&str, &str)] = &[
    ("groceries", "milk, eggs, coffee"),
    ("launch-plan", "ship yoagent 0.24, then the tweet"),
    ("tax-2026", "receipts are in the blue folder"),
];

const SYSTEM_PROMPT: &str = "You manage the user's notes with the tools you have. \
Only do what the user asks. If a tool call is denied, say so plainly.";

struct ListNotes;

#[async_trait::async_trait(?Send)]
impl AgentTool for ListNotes {
    fn name(&self) -> &str {
        "list_notes"
    }
    fn label(&self) -> &str {
        "List notes"
    }
    fn description(&self) -> &str {
        "List every note with its contents."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    async fn execute(
        &self,
        _params: serde_json::Value,
        _ctx: ToolContext,
    ) -> std::result::Result<ToolResult, ToolError> {
        let text = NOTES
            .iter()
            .map(|(name, body)| format!("{name}: {body}"))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(text_result(text))
    }
}

/// Destructive in name only: it reports what it would delete. Swap the body
/// for a KV or D1 delete in a real Worker.
struct DeleteNote;

#[async_trait::async_trait(?Send)]
impl AgentTool for DeleteNote {
    fn name(&self) -> &str {
        "delete_note"
    }
    fn label(&self) -> &str {
        "Delete note"
    }
    fn description(&self) -> &str {
        "Permanently delete a note by name."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"name": {"type": "string", "description": "The note to delete"}},
            "required": ["name"]
        })
    }
    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: ToolContext,
    ) -> std::result::Result<ToolResult, ToolError> {
        let name = params["name"].as_str().unwrap_or_default();
        if !NOTES.iter().any(|(n, _)| *n == name) {
            return Err(ToolError::Failed(format!("no note named {name:?}")));
        }
        Ok(text_result(format!("deleted {name} (simulated)")))
    }
}

fn text_result(text: String) -> ToolResult {
    ToolResult {
        content: vec![Content::Text { text }],
        details: serde_json::Value::Null,
    }
}

fn tool_text(result: &ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| match c {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[event(fetch)]
async fn fetch(mut req: Request, env: Env, _ctx: Context) -> Result<Response> {
    // This endpoint spends model credits: require a token.
    let token = env.secret("RUN_TOKEN")?.to_string();
    let authorized = req
        .headers()
        .get("authorization")?
        .is_some_and(|h| h == format!("Bearer {token}"));
    if !authorized {
        return Response::error("unauthorized", 401);
    }
    if req.method() != Method::Post {
        return Response::error("POST a prompt", 405);
    }
    let prompt = req.text().await?;
    if prompt.trim().is_empty() || prompt.len() > 4_096 {
        return Response::error("the prompt must be 1 to 4096 bytes", 400);
    }

    // A binding belongs to this request: build the gate and agent here.
    let clef = yoagent_workers::ai::clef(env.ai("AI")?);
    let mut agent = Agent::from_provider(
        OpenAiCompatProvider,
        ModelConfig::deepseek("deepseek-flash", "DeepSeek Flash"),
    )
    // No process environment in a Worker: keys come from secrets.
    .with_api_key(env.secret("DEEPSEEK_API_KEY")?.to_string())
    .with_system_prompt(SYSTEM_PROMPT)
    .with_tools(vec![Box::new(ListNotes), Box::new(DeleteNote)])
    .with_tool_gate(ToolGate::new(clef))
    .with_execution_limits(ExecutionLimits::default().with_max_turns(4));

    let mut rx = agent.prompt(prompt).await;
    let mut answer = String::new();
    let mut tools = Vec::new();
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::MessageStart { .. } => answer.clear(),
            AgentEvent::MessageUpdate {
                delta: StreamDelta::Text { delta },
                ..
            } => answer.push_str(&delta),
            AgentEvent::ToolExecutionEnd {
                tool_name,
                result,
                is_error,
                ..
            } => tools.push(serde_json::json!({
                "tool": tool_name,
                "error": is_error,
                "result": tool_text(&result),
            })),
            _ => {}
        }
    }
    agent.finish().await;

    Response::from_json(&serde_json::json!({ "answer": answer, "tools": tools }))
}
