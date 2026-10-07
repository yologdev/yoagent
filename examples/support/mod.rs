//! Shared by the `extension_*` examples (not an example itself).
//!
//! Every example runs offline by default on a scripted `MockProvider`, and
//! then checks its own result: a regression makes `cargo run --example …`
//! exit non-zero, which is how CI keeps the examples honest.
//!
//! Pass `--live` to drive a real model instead (`DEEPSEEK_API_KEY`, else
//! `ANTHROPIC_API_KEY`). A live model decides for itself what to call, so
//! the checks are skipped then and the example only prints what happened.
//! Live mode is opt-in rather than keyed off the environment alone, so a
//! machine that happens to export a key never spends money by surprise.
#![allow(dead_code)] // each example uses a different subset

use std::sync::{Arc, OnceLock};
use tokio::sync::mpsc;
use yoagent::provider::mock::{MockResponse, MockToolCall};
use yoagent::provider::{MockProvider, ModelConfig};
use yoagent::sub_agent::SubAgentTool;
use yoagent::*;

/// The live model, when `--live` was passed and a key is set.
///
/// Decided once per process, so a missing key is reported once.
pub fn live_model() -> Option<ModelConfig> {
    static LIVE: OnceLock<Option<ModelConfig>> = OnceLock::new();
    LIVE.get_or_init(|| {
        if !std::env::args().any(|a| a == "--live") {
            return None;
        }
        let has = |var: &str| std::env::var(var).is_ok_and(|v| !v.trim().is_empty());
        if has("DEEPSEEK_API_KEY") {
            Some(ModelConfig::deepseek("deepseek-flash", "DeepSeek Flash"))
        } else if has("ANTHROPIC_API_KEY") {
            Some(ModelConfig::claude_haiku_4_5())
        } else {
            eprintln!("--live needs DEEPSEEK_API_KEY or ANTHROPIC_API_KEY; running offline");
            None
        }
    })
    .clone()
}

pub fn is_live() -> bool {
    live_model().is_some()
}

/// An agent on the live model, or on `script` (one response per turn).
pub fn new_agent(script: Vec<MockResponse>) -> Agent {
    match live_model() {
        // The key is read from the provider's env var; never printed.
        Some(model) => Agent::from_config(model),
        None => Agent::from_provider(MockProvider::new(script), ModelConfig::mock()),
    }
}

/// A sub-agent on the live model, or on `script`.
pub fn sub_agent(name: &str, script: Vec<MockResponse>) -> SubAgentTool {
    match live_model() {
        Some(model) => SubAgentTool::from_config(name, model),
        None => SubAgentTool::from_provider(
            name,
            Arc::new(MockProvider::new(script)),
            ModelConfig::mock(),
        ),
    }
}

/// A scripted turn calling one tool.
pub fn call(name: &str, args: serde_json::Value) -> MockResponse {
    calls(&[(name, args)])
}

/// A scripted turn calling several tools at once.
pub fn calls(list: &[(&str, serde_json::Value)]) -> MockResponse {
    MockResponse::ToolCalls(
        list.iter()
            .map(|(name, args)| MockToolCall {
                name: name.to_string(),
                arguments: args.clone(),
                provider_metadata: None,
            })
            .collect(),
    )
}

/// A scripted final answer.
pub fn answer(text: &str) -> MockResponse {
    MockResponse::Text(text.into())
}

/// Run one prompt to the end and return its events.
pub async fn run(agent: &mut Agent, prompt: &str) -> Vec<AgentEvent> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    agent.prompt_with_sender(prompt, tx).await;
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    events
}

/// The text blocks of `content`, joined.
pub fn text_of(content: &[Content]) -> String {
    content
        .iter()
        .filter_map(|c| match c {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// Every tool result of a run: `(tool, text, is_error)`.
pub fn tool_results(events: &[AgentEvent]) -> Vec<(String, String, bool)> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolExecutionEnd {
                tool_name,
                result,
                is_error,
                ..
            } => Some((tool_name.clone(), text_of(&result.content), *is_error)),
            _ => None,
        })
        .collect()
}

/// The text of the run's last message (an answer, or a stop marker).
pub fn last_text(events: &[AgentEvent]) -> String {
    let Some(AgentEvent::AgentEnd { messages, .. }) = events.last() else {
        return String::new();
    };
    match messages.last() {
        Some(AgentMessage::Llm(Message::Assistant { content, .. }))
        | Some(AgentMessage::Llm(Message::User { content, .. })) => text_of(content),
        _ => String::new(),
    }
}

/// Assert `ok` offline; live, only say it was not checked.
pub fn check(ok: bool, what: &str) {
    if is_live() {
        println!("  (live run: not checking that {what})");
    } else {
        assert!(ok, "regression: expected {what}");
        println!("  ok: {what}");
    }
}
