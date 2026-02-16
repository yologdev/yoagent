//! Anthropic Claude provider (Messages API with streaming)

use crate::types::*;
use super::traits::*;
use async_trait::async_trait;
use futures::StreamExt;
use reqwest_eventsource::{Event, EventSource};
use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::{debug, warn};

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2023-06-01";

pub struct AnthropicProvider;

#[async_trait]
impl StreamProvider for AnthropicProvider {
    async fn stream(
        &self,
        config: StreamConfig,
        tx: mpsc::UnboundedSender<StreamEvent>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Message, ProviderError> {
        let body = build_request_body(&config);
        debug!("Anthropic request: model={}", config.model);

        let client = reqwest::Client::new();
        let request = client
            .post(API_URL)
            .header("x-api-key", &config.api_key)
            .header("anthropic-version", API_VERSION)
            .header("content-type", "application/json")
            .json(&body);

        let mut es = EventSource::new(request)
            .map_err(|e| ProviderError::Network(e.to_string()))?;

        let mut content: Vec<Content> = Vec::new();
        let mut usage = Usage::default();
        let mut stop_reason = StopReason::Stop;

        let _ = tx.send(StreamEvent::Start);

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    es.close();
                    return Err(ProviderError::Cancelled);
                }
                event = es.next() => {
                    match event {
                        None => break,
                        Some(Ok(Event::Open)) => {}
                        Some(Ok(Event::Message(msg))) => {
                            match msg.event.as_str() {
                                "message_start" => {
                                    if let Ok(data) = serde_json::from_str::<AnthropicMessageStart>(&msg.data) {
                                        usage.input = data.message.usage.input_tokens;
                                    }
                                }
                                "content_block_start" => {
                                    if let Ok(data) = serde_json::from_str::<AnthropicContentBlockStart>(&msg.data) {
                                        let idx = data.index as usize;
                                        match data.content_block {
                                            AnthropicContentBlock::Text { .. } => {
                                                while content.len() <= idx {
                                                    content.push(Content::Text { text: String::new() });
                                                }
                                            }
                                            AnthropicContentBlock::Thinking { .. } => {
                                                while content.len() <= idx {
                                                    content.push(Content::Thinking { thinking: String::new(), signature: None });
                                                }
                                            }
                                            AnthropicContentBlock::ToolUse { id, name, .. } => {
                                                while content.len() <= idx {
                                                    content.push(Content::ToolCall {
                                                        id: id.clone(),
                                                        name: name.clone(),
                                                        arguments: serde_json::Value::Object(Default::default()),
                                                    });
                                                }
                                                let _ = tx.send(StreamEvent::ToolCallStart {
                                                    content_index: idx,
                                                    id,
                                                    name,
                                                });
                                            }
                                        }
                                    }
                                }
                                "content_block_delta" => {
                                    if let Ok(data) = serde_json::from_str::<AnthropicContentBlockDelta>(&msg.data) {
                                        let idx = data.index as usize;
                                        match data.delta {
                                            AnthropicDelta::TextDelta { text } => {
                                                if let Some(Content::Text { text: ref mut t }) = content.get_mut(idx) {
                                                    t.push_str(&text);
                                                }
                                                let _ = tx.send(StreamEvent::TextDelta {
                                                    content_index: idx,
                                                    delta: text,
                                                });
                                            }
                                            AnthropicDelta::ThinkingDelta { thinking } => {
                                                if let Some(Content::Thinking { thinking: ref mut t, .. }) = content.get_mut(idx) {
                                                    t.push_str(&thinking);
                                                }
                                                let _ = tx.send(StreamEvent::ThinkingDelta {
                                                    content_index: idx,
                                                    delta: thinking,
                                                });
                                            }
                                            AnthropicDelta::InputJsonDelta { partial_json } => {
                                                let _ = tx.send(StreamEvent::ToolCallDelta {
                                                    content_index: idx,
                                                    delta: partial_json,
                                                });
                                            }
                                            AnthropicDelta::SignatureDelta { signature } => {
                                                if let Some(Content::Thinking { signature: ref mut s, .. }) = content.get_mut(idx) {
                                                    *s = Some(signature);
                                                }
                                            }
                                        }
                                    }
                                }
                                "content_block_stop" => {
                                    if let Ok(data) = serde_json::from_str::<serde_json::Value>(&msg.data) {
                                        let idx = data["index"].as_u64().unwrap_or(0) as usize;
                                        // Parse accumulated JSON for tool calls
                                        if let Some(Content::ToolCall { arguments: _, .. }) = content.get_mut(idx) {
                                            // Arguments will be accumulated from deltas
                                        }
                                        let _ = tx.send(StreamEvent::ToolCallEnd { content_index: idx });
                                    }
                                }
                                "message_delta" => {
                                    if let Ok(data) = serde_json::from_str::<AnthropicMessageDelta>(&msg.data) {
                                        stop_reason = match data.delta.stop_reason.as_deref() {
                                            Some("tool_use") => StopReason::ToolUse,
                                            Some("max_tokens") => StopReason::Length,
                                            _ => StopReason::Stop,
                                        };
                                        usage.output = data.usage.output_tokens;
                                    }
                                }
                                "message_stop" => break,
                                "ping" => {}
                                "error" => {
                                    warn!("Anthropic stream error: {}", msg.data);
                                    let err_msg = Message::Assistant {
                                        content: vec![Content::Text { text: String::new() }],
                                        stop_reason: StopReason::Error,
                                        model: config.model.clone(),
                                        provider: "anthropic".into(),
                                        usage: usage.clone(),
                                        timestamp: now_ms(),
                                        error_message: Some(msg.data),
                                    };
                                    let _ = tx.send(StreamEvent::Error { message: err_msg.clone() });
                                    return Ok(err_msg);
                                }
                                other => {
                                    debug!("Unknown Anthropic event: {}", other);
                                }
                            }
                        }
                        Some(Err(e)) => {
                            let err_str = e.to_string();
                            warn!("SSE error: {}", err_str);
                            let err_msg = Message::Assistant {
                                content: vec![Content::Text { text: String::new() }],
                                stop_reason: StopReason::Error,
                                model: config.model.clone(),
                                provider: "anthropic".into(),
                                usage: usage.clone(),
                                timestamp: now_ms(),
                                error_message: Some(err_str),
                            };
                            let _ = tx.send(StreamEvent::Error { message: err_msg.clone() });
                            return Ok(err_msg);
                        }
                    }
                }
            }
        }

        let has_tool_calls = content.iter().any(|c| matches!(c, Content::ToolCall { .. }));
        if has_tool_calls {
            stop_reason = StopReason::ToolUse;
        }

        let message = Message::Assistant {
            content,
            stop_reason,
            model: config.model.clone(),
            provider: "anthropic".into(),
            usage,
            timestamp: now_ms(),
            error_message: None,
        };

        let _ = tx.send(StreamEvent::Done { message: message.clone() });
        Ok(message)
    }
}

// ---------------------------------------------------------------------------
// Anthropic API request/response types
// ---------------------------------------------------------------------------

fn build_request_body(config: &StreamConfig) -> serde_json::Value {
    let mut messages: Vec<serde_json::Value> = Vec::new();

    for msg in &config.messages {
        match msg {
            Message::User { content, .. } => {
                messages.push(serde_json::json!({
                    "role": "user",
                    "content": content_to_anthropic(content),
                }));
            }
            Message::Assistant { content, .. } => {
                messages.push(serde_json::json!({
                    "role": "assistant",
                    "content": content_to_anthropic(content),
                }));
            }
            Message::ToolResult { tool_call_id, content, is_error, .. } => {
                let text = content.iter().find_map(|c| match c {
                    Content::Text { text } => Some(text.clone()),
                    _ => None,
                }).unwrap_or_default();

                messages.push(serde_json::json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": tool_call_id,
                        "content": text,
                        "is_error": is_error,
                    }],
                }));
            }
        }
    }

    let mut body = serde_json::json!({
        "model": config.model,
        "max_tokens": config.max_tokens.unwrap_or(8192),
        "stream": true,
        "messages": messages,
    });

    if !config.system_prompt.is_empty() {
        body["system"] = serde_json::json!(config.system_prompt);
    }

    if !config.tools.is_empty() {
        let tools: Vec<serde_json::Value> = config.tools.iter().map(|t| {
            serde_json::json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.parameters,
            })
        }).collect();
        body["tools"] = serde_json::json!(tools);
    }

    if config.thinking_level != ThinkingLevel::Off {
        let budget = match config.thinking_level {
            ThinkingLevel::Minimal => 128,
            ThinkingLevel::Low => 512,
            ThinkingLevel::Medium => 2048,
            ThinkingLevel::High => 8192,
            ThinkingLevel::Off => 0,
        };
        body["thinking"] = serde_json::json!({
            "type": "enabled",
            "budget_tokens": budget,
        });
    }

    if let Some(temp) = config.temperature {
        body["temperature"] = serde_json::json!(temp);
    }

    body
}

fn content_to_anthropic(content: &[Content]) -> Vec<serde_json::Value> {
    content.iter().filter_map(|c| match c {
        Content::Text { text } => Some(serde_json::json!({"type": "text", "text": text})),
        Content::Image { data, mime_type } => Some(serde_json::json!({
            "type": "image",
            "source": {"type": "base64", "media_type": mime_type, "data": data},
        })),
        Content::Thinking { thinking, signature } => Some(serde_json::json!({
            "type": "thinking",
            "thinking": thinking,
            "signature": signature.as_deref().unwrap_or(""),
        })),
        Content::ToolCall { id, name, arguments } => Some(serde_json::json!({
            "type": "tool_use",
            "id": id,
            "name": name,
            "input": arguments,
        })),
    }).collect()
}

// Anthropic SSE event types
#[derive(Deserialize)]
struct AnthropicMessageStart {
    message: AnthropicMessageInfo,
}

#[derive(Deserialize)]
struct AnthropicMessageInfo {
    usage: AnthropicUsage,
}

#[derive(Deserialize)]
struct AnthropicUsage {
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

#[derive(Deserialize)]
struct AnthropicContentBlockStart {
    index: u64,
    content_block: AnthropicContentBlock,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum AnthropicContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "thinking")]
    Thinking { thinking: String },
    #[serde(rename = "tool_use")]
    ToolUse { id: String, name: String },
}

#[derive(Deserialize)]
struct AnthropicContentBlockDelta {
    index: u64,
    delta: AnthropicDelta,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum AnthropicDelta {
    #[serde(rename = "text_delta")]
    TextDelta { text: String },
    #[serde(rename = "thinking_delta")]
    ThinkingDelta { thinking: String },
    #[serde(rename = "input_json_delta")]
    InputJsonDelta { partial_json: String },
    #[serde(rename = "signature_delta")]
    SignatureDelta { signature: String },
}

#[derive(Deserialize)]
struct AnthropicMessageDelta {
    delta: AnthropicMessageDeltaInner,
    usage: AnthropicUsage,
}

#[derive(Deserialize)]
struct AnthropicMessageDeltaInner {
    stop_reason: Option<String>,
}
