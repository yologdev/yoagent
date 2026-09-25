//! Azure OpenAI provider.
//!
//! Uses the OpenAI Responses API format but with Azure-specific authentication
//! and URL patterns.
//!
//! Base URL format: `https://{resource}.openai.azure.com/openai/deployments/{deployment}`
//! Auth: `api-key` header or Azure AD Bearer token.

use super::responses_stream::{Flow, ResponsesStreamState};
use super::traits::*;
use crate::types::*;
use async_trait::async_trait;
use futures::StreamExt;
use reqwest_eventsource::EventSource;
use tokio::sync::mpsc;
use tracing::{debug, warn};

pub struct AzureOpenAiProvider;

#[async_trait]
impl StreamProvider for AzureOpenAiProvider {
    fn protocol(&self) -> Option<crate::provider::ApiProtocol> {
        Some(crate::provider::ApiProtocol::AzureOpenAiResponses)
    }

    async fn stream(
        &self,
        config: StreamConfig,
        tx: mpsc::UnboundedSender<StreamEvent>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Message, ProviderError> {
        if config.output_schema.is_some() {
            tracing::warn!(
                "structured outputs are not yet wired for the Azure OpenAI provider; output_schema will be ignored"
            );
        }
        let model_config = config
            .model_config
            .as_ref()
            .ok_or_else(|| ProviderError::Other("ModelConfig required".into()))?;

        // Azure uses the Responses API format
        let url = format!(
            "{}/responses?api-version=2025-01-01-preview",
            model_config.base_url
        );

        let body = build_azure_request_body(&config);
        debug!("Azure OpenAI request: model={} url={}", config.model, url);

        let client = reqwest::Client::new();
        let mut request = client
            .post(&url)
            .header("content-type", "application/json")
            .header("api-key", &config.api_key);

        for (k, v) in &model_config.headers {
            request = request.header(k, v);
        }

        let request = request.json(&body);
        let mut es =
            EventSource::new(request).map_err(|e| ProviderError::Network(e.to_string()))?;

        let mut state = ResponsesStreamState::new("Azure OpenAI");

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
                        Some(Ok(reqwest_eventsource::Event::Open)) => {}
                        Some(Ok(reqwest_eventsource::Event::Message(msg))) => {
                            if state.handle(&msg.event, &msg.data, &tx)? == Flow::Done {
                                break;
                            }
                        }
                        Some(Err(e)) => {
                            let provider_err = classify_eventsource_error(e).await;
                            warn!("Azure OpenAI SSE error: {}", provider_err);
                            return Err(provider_err);
                        }
                    }
                }
            }
        }

        let (content, usage, stop_reason) = state.finish(&tx);

        let message = Message::Assistant {
            content,
            stop_reason,
            model: config.model.clone(),
            provider: model_config.provider.clone(),
            usage,
            timestamp: now_ms(),
            error_message: None,
        };

        let _ = tx.send(StreamEvent::Done {
            message: message.clone(),
        });
        Ok(message)
    }
}

fn build_azure_request_body(config: &StreamConfig) -> serde_json::Value {
    // Same format as OpenAI Responses API
    let mut input: Vec<serde_json::Value> = Vec::new();

    for msg in &config.messages {
        match msg {
            Message::User { content, .. } => {
                // Build content array for user message (supports text + images)
                let user_content: Vec<serde_json::Value> = content
                    .iter()
                    .filter(|c| !matches!(c, Content::Text { text } if text.is_empty()))
                    .filter_map(|c| match c {
                        Content::Text { text } => Some(serde_json::json!({
                            "type": "input_text",
                            "text": text,
                        })),
                        Content::Image { data, mime_type } => Some(serde_json::json!({
                            "type": "input_image",
                            "image_url": format!("data:{};base64,{}", mime_type, data),
                        })),
                        _ => None,
                    })
                    .collect();

                if user_content.len() == 1 && user_content[0]["type"] == "input_text" {
                    // Simple text-only message can use shorthand format
                    input.push(serde_json::json!({
                        "role": "user",
                        "content": user_content[0]["text"].as_str().unwrap_or(""),
                    }));
                } else {
                    // Multi-modal content uses array format
                    input.push(serde_json::json!({
                        "role": "user",
                        "content": user_content,
                    }));
                }
            }
            Message::Assistant { content, .. } => {
                for c in content {
                    match c {
                        Content::Text { text } if text.is_empty() => {}
                        Content::Text { text } => {
                            input.push(serde_json::json!({
                                "type": "message",
                                "role": "assistant",
                                "content": [{"type": "output_text", "text": text}],
                            }));
                        }
                        Content::ToolCall {
                            id,
                            name,
                            arguments,
                            ..
                        } => {
                            input.push(serde_json::json!({
                                "type": "function_call",
                                "call_id": id,
                                "name": name,
                                "arguments": arguments.to_string(),
                            }));
                        }
                        _ => {}
                    }
                }
            }
            Message::ToolResult {
                tool_call_id,
                content,
                ..
            } => {
                let output_val = if content.iter().any(|c| matches!(c, Content::Image { .. })) {
                    let parts: Vec<serde_json::Value> = content
                        .iter()
                        .filter(|c| !matches!(c, Content::Text { text } if text.is_empty()))
                        .filter_map(|c| match c {
                            Content::Text { text } => Some(serde_json::json!({
                                "type": "input_text",
                                "text": text,
                            })),
                            Content::Image { data, mime_type } => Some(serde_json::json!({
                                "type": "input_image",
                                "image_url": format!("data:{};base64,{}", mime_type, data),
                            })),
                            _ => None,
                        })
                        .collect();
                    serde_json::json!(parts)
                } else {
                    let text = content
                        .iter()
                        .find_map(|c| match c {
                            Content::Text { text } => Some(text.clone()),
                            _ => None,
                        })
                        .unwrap_or_default();
                    serde_json::json!(text)
                };
                input.push(serde_json::json!({
                    "type": "function_call_output",
                    "call_id": tool_call_id,
                    "output": output_val,
                }));
            }
        }
    }

    let mut body = serde_json::json!({
        "model": config.model,
        "stream": true,
        "input": input,
    });

    if !config.system_prompt.is_empty() {
        body["instructions"] = serde_json::json!(config.system_prompt);
    }

    if let Some(max) = config.max_tokens {
        body["max_output_tokens"] = serde_json::json!(max);
    }

    if !config.tools.is_empty() {
        let tools: Vec<serde_json::Value> = config
            .tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                })
            })
            .collect();
        body["tools"] = serde_json::json!(tools);
    }

    if let Some(temp) = config.temperature {
        body["temperature"] = serde_json::json!(temp);
    }

    // Thinking: the Responses API's reasoning effort (same mapping as the
    // first-party OpenAI Responses provider).
    if config.thinking_level != ThinkingLevel::Off {
        let effort = match config.thinking_level {
            ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
            ThinkingLevel::Medium => "medium",
            // Clamped: `high` is the top rung this crate knows the provider
            // accepts, and an unknown effort string is rejected, not rounded.
            ThinkingLevel::High | ThinkingLevel::XHigh | ThinkingLevel::Max => "high",
            ThinkingLevel::Off => unreachable!(),
        };
        body["reasoning"] = serde_json::json!({"effort": effort});
    }

    body
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(level: ThinkingLevel) -> StreamConfig {
        StreamConfig {
            model: "gpt-5.5".into(),
            system_prompt: "".into(),
            messages: vec![Message::user("hi")],
            tools: vec![],
            thinking_level: level,
            api_key: "key".into(),
            max_tokens: None,
            temperature: None,
            model_config: None,
            cache_config: CacheConfig::default(),
            output_schema: None,
        }
    }

    #[test]
    fn thinking_level_sets_reasoning_effort() {
        let body = build_azure_request_body(&config(ThinkingLevel::Medium));
        assert_eq!(body["reasoning"]["effort"], "medium");
    }

    #[test]
    fn xhigh_and_max_clamp_to_high() {
        for level in [
            ThinkingLevel::XHigh,
            ThinkingLevel::Max,
            ThinkingLevel::High,
        ] {
            let body = build_azure_request_body(&config(level));
            assert_eq!(body["reasoning"]["effort"], "high");
        }
        let body = build_azure_request_body(&config(ThinkingLevel::Low));
        assert_eq!(body["reasoning"]["effort"], "low");
    }

    #[test]
    fn thinking_off_omits_reasoning() {
        let body = build_azure_request_body(&config(ThinkingLevel::Off));
        assert!(body["reasoning"].is_null());
    }
}
