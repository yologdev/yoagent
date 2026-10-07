//! Live test of the OpenAI Responses provider: a tool-call conversation whose
//! continuation replays the encrypted reasoning item, then a follow-up user
//! turn on the same history.
//!
//! Run with:
//!
//! ```text
//! OPENAI_API_KEY=... cargo test --test integration_openai_responses -- --ignored --nocapture
//! ```
//!
//! `#[ignore]`d, so CI and a plain `cargo test` never run it. Without
//! `OPENAI_API_KEY` it prints a skip line and passes. The key is read by
//! `Agent::from_config` and is never printed. The model defaults to
//! `gpt-6-luna` (the cheapest GPT-6 preset); set `YOAGENT_RESPONSES_MODEL` to
//! another Responses model id to override it. Cost: three short requests.
//!
//! What a pass shows that the mock tests cannot: OpenAI returns
//! `encrypted_content` for the `include` this crate sends (with `store` left
//! at its default), and accepts the replayed reasoning item, the function
//! call and `prompt_cache_key` without a 400.

use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use yoagent::agent::Agent;
use yoagent::provider::{ApiProtocol, ModelConfig};
use yoagent::types::*;

/// Returns a fixed forecast and records its arguments.
struct Weather(Arc<Mutex<Vec<Value>>>);

#[async_trait::async_trait]
impl AgentTool for Weather {
    fn name(&self) -> &str {
        "get_weather"
    }
    fn label(&self) -> &str {
        "Weather"
    }
    fn description(&self) -> &str {
        "Get today's weather for a city."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"city": {"type": "string"}},
            "required": ["city"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, params: Value, _ctx: ToolContext) -> Result<ToolResult, ToolError> {
        self.0.lock().unwrap().push(params);
        Ok(ToolResult {
            content: vec![Content::Text {
                text: "Sunny, 23 degrees Celsius, wind 4 km/h.".into(),
            }],
            details: Value::Null,
        })
    }
}

fn model_config() -> ModelConfig {
    match std::env::var("YOAGENT_RESPONSES_MODEL") {
        Ok(id) if !id.trim().is_empty() => {
            let mut mc = ModelConfig::openai_responses(id.trim(), id.trim());
            mc.compat = ModelConfig::gpt_6_luna().compat;
            mc
        }
        _ => ModelConfig::gpt_6_luna(),
    }
}

fn assistant_messages(agent: &Agent) -> Vec<&Message> {
    agent
        .messages()
        .iter()
        .filter_map(|m| match m {
            AgentMessage::Llm(m @ Message::Assistant { .. }) => Some(m),
            _ => None,
        })
        .collect()
}

fn text_of(m: &Message) -> String {
    match m {
        Message::Assistant { content, .. } => content
            .iter()
            .filter_map(|c| match c {
                Content::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect(),
        _ => String::new(),
    }
}

#[tokio::test]
#[ignore]
async fn tool_call_conversation_replays_encrypted_reasoning() {
    if std::env::var("OPENAI_API_KEY").map_or(true, |k| k.trim().is_empty()) {
        eprintln!("skipped: OPENAI_API_KEY is not set");
        return;
    }
    let mc = model_config();
    let model = mc.id.clone();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut agent = Agent::from_config(mc)
        .with_system_prompt(
            "You are a weather assistant. Always call get_weather before answering \
             a weather question. Answer in one short sentence.",
        )
        .with_thinking(ThinkingLevel::Low)
        .with_max_tokens(2048)
        .with_tools(vec![Box::new(Weather(calls.clone()))]);

    // Turn 1: tool call, then the continuation that replays the reasoning.
    let mut rx = agent.prompt("What's the weather in Lisbon today?").await;
    while rx.recv().await.is_some() {}
    agent.finish().await;

    let assistants = assistant_messages(&agent);
    for m in &assistants {
        if let Message::Assistant {
            stop_reason,
            error_message,
            usage,
            ..
        } = m
        {
            eprintln!(
                "[{model}] stop={stop_reason:?} input={} cache_read={} output={} error={error_message:?}",
                usage.input, usage.cache_read, usage.output
            );
            assert_ne!(
                *stop_reason,
                StopReason::Error,
                "request failed: {error_message:?}"
            );
        }
    }
    assert_eq!(
        calls.lock().unwrap().len(),
        1,
        "the model must call get_weather once"
    );
    let Some(Message::Assistant { content, .. }) = assistants.first() else {
        panic!("no assistant message")
    };
    let encrypted = content
        .iter()
        .filter(|c| {
            matches!(
                c,
                Content::Thinking {
                    redacted: Some(_),
                    redacted_protocol: Some(ApiProtocol::OpenAiResponses),
                    ..
                }
            )
        })
        .count();
    eprintln!("[{model}] reasoning items kept for replay on the tool-call turn: {encrypted}");
    assert!(
        encrypted > 0,
        "no encrypted reasoning came back for include=[reasoning.encrypted_content]: {content:?}"
    );
    assert!(
        assistants.len() >= 2,
        "the continuation after the tool call must have run"
    );
    let answer = text_of(assistants.last().unwrap());
    eprintln!("[{model}] answer: {answer}");
    assert!(answer.contains("23"), "the answer must use the tool result");

    // Turn 2: a follow-up on the same history (earlier reasoning replayed
    // again, now behind a later user message).
    let mut rx = agent.prompt("And is it windy there?").await;
    while rx.recv().await.is_some() {}
    agent.finish().await;
    let last = assistant_messages(&agent).last().copied().cloned().unwrap();
    if let Message::Assistant {
        stop_reason,
        error_message,
        ..
    } = &last
    {
        assert_ne!(
            *stop_reason,
            StopReason::Error,
            "follow-up failed: {error_message:?}"
        );
    }
    eprintln!("[{model}] follow-up: {}", text_of(&last));
    assert!(!text_of(&last).is_empty());
}
