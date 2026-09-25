//! OpenAI Responses API provider.
//!
//! This is the newer OpenAI API that uses a different event format
//! from Chat Completions. It has first-class support for reasoning items.

use super::model::{ModelConfig, OpenAiCompat};
use super::responses_stream::{Flow, ResponsesStreamState};
use super::traits::*;
use crate::types::*;
use async_trait::async_trait;
use futures::StreamExt;
use reqwest_eventsource::EventSource;
use tokio::sync::mpsc;
use tracing::{debug, warn};

pub struct OpenAiResponsesProvider;

#[async_trait]
impl StreamProvider for OpenAiResponsesProvider {
    fn protocol(&self) -> Option<crate::provider::ApiProtocol> {
        Some(crate::provider::ApiProtocol::OpenAiResponses)
    }

    async fn stream(
        &self,
        config: StreamConfig,
        tx: mpsc::UnboundedSender<StreamEvent>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Message, ProviderError> {
        if config.output_schema.is_some() {
            tracing::warn!(
                "structured outputs are not yet wired for the OpenAI Responses provider; output_schema will be ignored"
            );
        }
        let model_config = config
            .model_config
            .as_ref()
            .ok_or_else(|| ProviderError::Other("ModelConfig required".into()))?;

        let url = format!("{}/responses", model_config.base_url);
        let body = build_request_body(&config, model_config);
        debug!(
            "OpenAI Responses request: model={} url={}",
            config.model, url
        );

        let client = reqwest::Client::new();
        let mut request = client
            .post(&url)
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", config.api_key));

        for (k, v) in &model_config.headers {
            request = request.header(k, v);
        }

        let request = request.json(&body);
        let mut es =
            EventSource::new(request).map_err(|e| ProviderError::Network(e.to_string()))?;

        let mut state = ResponsesStreamState::new("OpenAI Responses");

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
                            warn!("OpenAI Responses SSE error: {}", provider_err);
                            return Err(provider_err);
                        }
                    }
                }
            }
        }

        let error_message = state.error_message();
        let (content, usage, stop_reason) = state.finish(&tx);

        let message = Message::Assistant {
            content,
            stop_reason,
            model: config.model.clone(),
            provider: model_config.provider.clone(),
            usage,
            timestamp: now_ms(),
            error_message,
        };

        let _ = tx.send(StreamEvent::Done {
            message: message.clone(),
        });
        Ok(message)
    }
}

fn build_request_body(config: &StreamConfig, model_config: &ModelConfig) -> serde_json::Value {
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
                    // Images present: build content array
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

    // The effort capability comes from `model_config.compat` (see
    // `OpenAiCompat::max_reasoning_effort`); `None` means a `high` ceiling,
    // which is what this provider always sent before. `Off` omits it.
    let default_compat = OpenAiCompat::default();
    let compat = model_config.compat.as_ref().unwrap_or(&default_compat);
    if let Some(effort) = compat.openai_reasoning_effort(config.thinking_level) {
        body["reasoning"] = serde_json::json!({"effort": effort});
    }

    if let Some(temp) = config.temperature {
        body["temperature"] = serde_json::json!(temp);
    }

    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::model::ReasoningEffortCeiling;

    const ALL_LEVELS: [ThinkingLevel; 7] = [
        ThinkingLevel::Off,
        ThinkingLevel::Minimal,
        ThinkingLevel::Low,
        ThinkingLevel::Medium,
        ThinkingLevel::High,
        ThinkingLevel::XHigh,
        ThinkingLevel::Max,
    ];

    fn body(mc: &ModelConfig, level: ThinkingLevel) -> serde_json::Value {
        let mut config = StreamConfig::new(mc.id.clone(), "key");
        config.messages = vec![Message::user("hi")];
        config.thinking_level = level;
        config.temperature = Some(0.5);
        config.model_config = Some(mc.clone());
        build_request_body(&config, mc)
    }

    fn effort(mc: &ModelConfig, level: ThinkingLevel) -> serde_json::Value {
        body(mc, level)["reasoning"]["effort"].clone()
    }

    #[test]
    fn without_compat_the_body_is_what_it_always_was() {
        // Near-miss guard: `openai_responses` carries `compat: None`, and a
        // Responses config built before this fix must send byte-identical
        // bodies — Off omits `reasoning`, XHigh/Max clamp to `high`.
        let mc = ModelConfig::openai_responses("gpt-5.5", "GPT-5.5");
        assert!(mc.compat.is_none());
        for level in ALL_LEVELS {
            let expected_effort = match level {
                ThinkingLevel::Off => None,
                ThinkingLevel::Minimal | ThinkingLevel::Low => Some("low"),
                ThinkingLevel::Medium => Some("medium"),
                _ => Some("high"),
            };
            let got = body(&mc, level);
            let mut expected = serde_json::json!({
                "model": "gpt-5.5",
                "stream": true,
                "input": [{"role": "user", "content": "hi"}],
                "temperature": 0.5,
            });
            if let Some(e) = expected_effort {
                expected["reasoning"] = serde_json::json!({"effort": e});
            }
            assert_eq!(
                serde_json::to_string(&got).unwrap(),
                serde_json::to_string(&expected).unwrap(),
                "{level:?}"
            );
        }
    }

    #[test]
    fn compat_ceiling_is_honoured() {
        // Positive control: the Responses builder used to ignore
        // `model_config.compat` entirely.
        let mut mc = ModelConfig::openai_responses("gpt-5.4", "GPT-5.4");
        mc.compat = Some(OpenAiCompat {
            max_reasoning_effort: ReasoningEffortCeiling::XHigh,
            ..Default::default()
        });
        assert_eq!(effort(&mc, ThinkingLevel::XHigh), "xhigh");
        assert_eq!(effort(&mc, ThinkingLevel::Max), "xhigh");
        assert_eq!(effort(&mc, ThinkingLevel::High), "high");
        assert!(body(&mc, ThinkingLevel::Off).get("reasoning").is_none());
    }

    #[test]
    fn gpt_6_astra_reaches_max_and_never_sends_none() {
        let mc = ModelConfig::gpt_6_astra();
        assert_eq!(effort(&mc, ThinkingLevel::Max), "max");
        assert_eq!(effort(&mc, ThinkingLevel::XHigh), "xhigh");
        assert_eq!(effort(&mc, ThinkingLevel::High), "high");
        assert_eq!(effort(&mc, ThinkingLevel::Medium), "medium");
        // GPT-6 has no `minimal`.
        assert_eq!(effort(&mc, ThinkingLevel::Minimal), "low");
        // `none` is an HTTP 400 on Astra: Off omits the effort instead.
        assert!(body(&mc, ThinkingLevel::Off).get("reasoning").is_none());
    }

    #[test]
    fn gpt_6_sol_and_luna_omit_effort_for_off() {
        for mc in [ModelConfig::gpt_6_sol(), ModelConfig::gpt_6_luna()] {
            assert!(body(&mc, ThinkingLevel::Off).get("reasoning").is_none());
            assert_eq!(effort(&mc, ThinkingLevel::Max), "max", "{}", mc.id);
            assert_eq!(effort(&mc, ThinkingLevel::XHigh), "xhigh", "{}", mc.id);
            assert_eq!(effort(&mc, ThinkingLevel::Low), "low", "{}", mc.id);
        }
    }

    #[test]
    fn off_bodies_are_byte_identical_to_the_uncapped_body_for_every_preset() {
        // `Off` means "send nothing": whatever the preset's ceiling, the body
        // equals the pre-#176 one (a compat-less config's body with the same
        // model id), with no `reasoning` key.
        for mc in [
            ModelConfig::gpt_6_astra(),
            ModelConfig::gpt_6_sol(),
            ModelConfig::gpt_6_luna(),
            ModelConfig::gpt_5_5(),
        ] {
            let mut legacy = mc.clone();
            legacy.compat = None;
            let got = body(&mc, ThinkingLevel::Off);
            assert!(got.get("reasoning").is_none(), "{}", mc.id);
            assert_eq!(
                serde_json::to_string(&got).unwrap(),
                serde_json::to_string(&body(&legacy, ThinkingLevel::Off)).unwrap(),
                "{}",
                mc.id
            );
        }
    }
}
