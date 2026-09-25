//! OpenAI Chat Completions compatible provider.
//!
//! One implementation covers OpenAI, xAI, Groq, Cerebras, OpenRouter,
//! Mistral, DeepSeek, MiniMax, HuggingFace, Kimi, and any other provider
//! that implements the OpenAI Chat Completions API.
//!
//! Behavioral differences are handled via `OpenAiCompat` flags in ModelConfig.

use super::model::{MaxTokensField, ModelConfig, OpenAiCompat, ThinkingFormat};
use super::tool_args::finalize_tool_arguments;
use super::traits::*;
use crate::types::*;
use async_trait::async_trait;
use futures::StreamExt;
use reqwest_eventsource::EventSource;
use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::{debug, warn};

pub struct OpenAiCompatProvider;

#[async_trait]
impl StreamProvider for OpenAiCompatProvider {
    fn protocol(&self) -> Option<crate::provider::ApiProtocol> {
        Some(crate::provider::ApiProtocol::OpenAiCompletions)
    }

    async fn stream(
        &self,
        config: StreamConfig,
        tx: mpsc::UnboundedSender<StreamEvent>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Message, ProviderError> {
        let model_config = config.model_config.as_ref().ok_or_else(|| {
            ProviderError::Other("ModelConfig required for OpenAI provider".into())
        })?;
        let compat = model_config.compat.as_ref().cloned().unwrap_or_default();

        let base_url = &model_config.base_url;
        let url = format!("{}/chat/completions", base_url);

        let body = build_request_body(&config, model_config, &compat);
        debug!("OpenAI compat request: model={} url={}", config.model, url);

        let client = reqwest::Client::new();
        let mut request = client
            .post(&url)
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", config.api_key));

        // Add any extra headers from model config
        for (k, v) in &model_config.headers {
            request = request.header(k, v);
        }

        let request = request.json(&body);

        let mut es =
            EventSource::new(request).map_err(|e| ProviderError::Network(e.to_string()))?;

        let mut content: Vec<Content> = Vec::new();
        let mut usage = Usage::default();
        let mut stop_reason = StopReason::Stop;
        let mut saw_finish_reason = false;
        let mut tool_call_buffers: Vec<ToolCallBuffer> = Vec::new();

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
                            if msg.data == "[DONE]" {
                                break;
                            }

                            let chunk: OpenAiChunk = match serde_json::from_str(&msg.data) {
                                Ok(c) => c,
                                Err(e) => {
                                    debug!("Failed to parse OpenAI chunk: {} data={}", e, &msg.data);
                                    continue;
                                }
                            };

                            // Process usage
                            if let Some(u) = &chunk.usage {
                                usage = usage_from_openai(u);
                            }

                            for choice in &chunk.choices {
                                let delta = &choice.delta;

                                // Handle reasoning/thinking content
                                let reasoning = match compat.thinking_format {
                                    ThinkingFormat::Xai => delta.reasoning.as_deref(),
                                    _ => delta.reasoning_content.as_deref(),
                                };
                                if let Some(reasoning_text) = reasoning {
                                    // Find or create thinking block
                                    let thinking_idx = content.iter().position(|c| matches!(c, Content::Thinking { .. }));
                                    let idx = match thinking_idx {
                                        Some(i) => i,
                                        None => {
                                            content.push(Content::Thinking { thinking: String::new(), signature: None });
                                            content.len() - 1
                                        }
                                    };
                                    if let Some(Content::Thinking { thinking, .. }) = content.get_mut(idx) {
                                        thinking.push_str(reasoning_text);
                                    }
                                    let _ = tx.send(StreamEvent::ThinkingDelta {
                                        content_index: idx,
                                        delta: reasoning_text.to_string(),
                                    });
                                }

                                // Handle text content
                                if let Some(text) = &delta.content {
                                    let text_idx = content.iter().position(|c| matches!(c, Content::Text { .. }));
                                    let idx = match text_idx {
                                        Some(i) => i,
                                        None => {
                                            content.push(Content::Text { text: String::new() });
                                            content.len() - 1
                                        }
                                    };
                                    if let Some(Content::Text { text: t }) = content.get_mut(idx) {
                                        t.push_str(text);
                                    }
                                    let _ = tx.send(StreamEvent::TextDelta {
                                        content_index: idx,
                                        delta: text.clone(),
                                    });
                                }

                                // Handle tool calls
                                if let Some(tool_calls) = &delta.tool_calls {
                                    for tc in tool_calls {
                                        let tc_index = tc.index as usize;
                                        while tool_call_buffers.len() <= tc_index {
                                            tool_call_buffers.push(ToolCallBuffer::default());
                                        }
                                        let buf = &mut tool_call_buffers[tc_index];
                                        if let Some(id) = &tc.id {
                                            buf.id = id.clone();
                                        }
                                        if let Some(f) = &tc.function {
                                            if let Some(name) = &f.name {
                                                buf.name.clone_from(name);
                                                let _ = tx.send(StreamEvent::ToolCallStart {
                                                    content_index: content.len() + tc_index,
                                                    id: buf.id.clone(),
                                                    name: name.clone(),
                                                });
                                            }
                                            if let Some(args) = &f.arguments {
                                                buf.arguments.push_str(args);
                                                let _ = tx.send(StreamEvent::ToolCallDelta {
                                                    content_index: content.len() + tc_index,
                                                    delta: args.clone(),
                                                });
                                            }
                                        }
                                    }
                                }

                                // Handle finish reason
                                if let Some(reason) = &choice.finish_reason {
                                    saw_finish_reason = true;
                                    stop_reason = match reason.as_str() {
                                        "stop" => StopReason::Stop,
                                        "length" => StopReason::Length,
                                        "tool_calls" => StopReason::ToolUse,
                                        _ => StopReason::Stop,
                                    };
                                }
                            }
                        }
                        // Some providers (e.g. MiniMax) close the connection
                        // without the OpenAI-standard `data: [DONE]` terminator.
                        // If a finish_reason was already received, the response
                        // is complete — treat as clean EOF. (This eventsource
                        // surfaces a body close as StreamEnded; an I/O failure
                        // mid-body surfaces as Transport instead.) A
                        // StreamEnded with NO finish_reason is truncation and
                        // stays an error — a retryable one since #83, since a
                        // well-framed body carrying a truncated payload is
                        // usually a transient gateway fault.
                        Some(Err(reqwest_eventsource::Error::StreamEnded)) if saw_finish_reason => {
                            debug!("provider closed stream without [DONE] after finish_reason");
                            break;
                        }
                        Some(Err(e)) => {
                            let provider_err = classify_eventsource_error(e).await;
                            warn!("OpenAI SSE error: {}", provider_err);
                            return Err(provider_err);
                        }
                    }
                }
            }
        }

        // Finalize tool calls
        for buf in &tool_call_buffers {
            let args = finalize_tool_arguments(&buf.name, &buf.arguments);
            content.push(Content::ToolCall {
                provider_metadata: None,
                id: buf.id.clone(),
                name: buf.name.clone(),
                arguments: args,
            });
            let _ = tx.send(StreamEvent::ToolCallEnd {
                content_index: content.len() - 1,
            });
        }

        // Tool calls make this a ToolUse turn — unless the output hit the
        // token limit. `finish_reason: "length"` mid-arguments is how a call
        // ends up with unparsed arguments, and Length is the signal a caller
        // needs to see; the loop still answers every tool call either way.
        if !tool_call_buffers.is_empty() && stop_reason != StopReason::Length {
            stop_reason = StopReason::ToolUse;
        }

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

#[derive(Default)]
struct ToolCallBuffer {
    id: String,
    name: String,
    arguments: String,
}

fn build_request_body(
    config: &StreamConfig,
    model_config: &ModelConfig,
    compat: &OpenAiCompat,
) -> serde_json::Value {
    let mut messages: Vec<serde_json::Value> = Vec::new();

    // System prompt
    if !config.system_prompt.is_empty() {
        let role = if compat.supports_developer_role {
            "developer"
        } else {
            "system"
        };
        messages.push(serde_json::json!({
            "role": role,
            "content": config.system_prompt,
        }));
    }

    // DeepSeek thinking mode with tools: every earlier assistant turn's
    // reasoning must go back as `reasoning_content`, or the request is a 400.
    // Without tools DeepSeek ignores it, so it is only sent when it matters.
    let replay_reasoning = compat.replays_reasoning_content && !config.tools.is_empty();

    for msg in &config.messages {
        if !matches!(msg, Message::ToolResult { .. } | Message::Assistant { .. }) {
            maybe_insert_assistant_after_tool_results(&mut messages, compat);
        }

        match msg {
            Message::User { content, .. } => {
                messages.push(serde_json::json!({
                    "role": "user",
                    "content": content_to_openai(content),
                }));
            }
            Message::Assistant { content, .. } => {
                let mut parts: Vec<serde_json::Value> = Vec::new();
                let mut tool_calls: Vec<serde_json::Value> = Vec::new();
                let mut reasoning = String::new();

                for c in content {
                    match c {
                        Content::Thinking { thinking, .. } if replay_reasoning => {
                            reasoning.push_str(thinking);
                        }
                        Content::Text { text } if text.is_empty() => {}
                        Content::Text { text } => {
                            parts.push(serde_json::json!({"type": "text", "text": text}));
                        }
                        Content::ToolCall {
                            id,
                            name,
                            arguments,
                            ..
                        } => {
                            tool_calls.push(serde_json::json!({
                                "id": id,
                                "type": "function",
                                "function": {"name": name, "arguments": arguments.to_string()},
                            }));
                        }
                        _ => {}
                    }
                }

                let mut msg_obj = serde_json::json!({"role": "assistant"});
                if !parts.is_empty() {
                    msg_obj["content"] = serde_json::json!(parts);
                }
                if !reasoning.is_empty() {
                    msg_obj["reasoning_content"] = serde_json::json!(reasoning);
                }
                if !tool_calls.is_empty() {
                    msg_obj["tool_calls"] = serde_json::json!(tool_calls);
                }
                messages.push(msg_obj);
            }
            Message::ToolResult {
                tool_call_id,
                tool_name,
                content,
                ..
            } => {
                let content_val = if content.iter().any(|c| matches!(c, Content::Image { .. })) {
                    // Images present: use array format for multimodal tool results
                    content_to_openai(content)
                } else {
                    // Text-only: use plain string for maximum compat
                    let text = content
                        .iter()
                        .find_map(|c| match c {
                            Content::Text { text } => Some(text.clone()),
                            _ => None,
                        })
                        .unwrap_or_default();
                    serde_json::json!(text)
                };

                let mut msg_obj = serde_json::json!({
                    "role": "tool",
                    "tool_call_id": tool_call_id,
                    "content": content_val,
                });
                if compat.requires_tool_result_name {
                    msg_obj["name"] = serde_json::json!(tool_name);
                }
                messages.push(msg_obj);
            }
        }
    }
    maybe_insert_assistant_after_tool_results(&mut messages, compat);

    let max_tokens_val = config.max_tokens.unwrap_or(model_config.max_tokens);
    let mut body = serde_json::json!({
        "model": config.model,
        "stream": true,
        "stream_options": {"include_usage": true},
        "messages": messages,
    });

    match compat.max_tokens_field {
        MaxTokensField::MaxCompletionTokens => {
            body["max_completion_tokens"] = serde_json::json!(max_tokens_val);
        }
        MaxTokensField::MaxTokens => {
            body["max_tokens"] = serde_json::json!(max_tokens_val);
        }
    }

    if compat.supports_thinking_control {
        let thinking_type = if config.thinking_level == ThinkingLevel::Off {
            "disabled"
        } else {
            "enabled"
        };
        body["thinking"] = serde_json::json!({ "type": thinking_type });
    }

    if !config.tools.is_empty() {
        let tools: Vec<serde_json::Value> = config
            .tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    }
                })
            })
            .collect();
        body["tools"] = serde_json::json!(tools);
    }

    // Prompt caching. OpenAI caches prefixes automatically once they exceed
    // ~1024 tokens, so there are no breakpoints to place — `prompt_cache_key`
    // only routes requests from one conversation toward the same cache. Gated
    // on the compat flag because the field is OpenAI's: a strict compat server
    // that validates unknown keys would reject the request outright, and the
    // providers that cache automatically were never reading it anyway.
    if compat.supports_prompt_cache_key {
        if let Some(key) = config.cache_session_key() {
            body["prompt_cache_key"] = serde_json::json!(key);
        }
    } else if config.cache_config.session_key.is_some() && config.cache_config.hints_enabled() {
        // A *derived* key going unsent is a missed optimization and stays
        // quiet. An *explicitly configured* one going unsent is a user
        // instruction being discarded — someone isolating tenants gets exactly
        // the sharing they were preventing. Matches the convention stated on
        // `StreamConfig::output_schema` and honoured by five providers.
        warn!(
            "CacheConfig::session_key is set, but provider '{}' does not accept \
             prompt_cache_key; the key is ignored and requests will not be routed \
             by session",
            model_config.provider
        );
    }

    // Structured outputs: native json_schema response format.
    if let Some(schema) = &config.output_schema {
        body["response_format"] = serde_json::json!({
            "type": "json_schema",
            "json_schema": {
                "name": schema.name,
                "schema": schema.schema,
                "strict": true,
            },
        });
    }

    if compat.supports_reasoning_effort {
        let effort = if compat.supports_thinking_control {
            // DeepSeek: `Off` is already `thinking: disabled` above.
            (config.thinking_level != ThinkingLevel::Off)
                .then(|| deepseek_reasoning_effort(config.thinking_level))
        } else {
            compat.openai_reasoning_effort(config.thinking_level)
        };
        if let Some(effort) = effort {
            body["reasoning_effort"] = serde_json::json!(effort);
        }
    }

    if let Some(temp) = config.temperature {
        body["temperature"] = serde_json::json!(temp);
    }

    body
}

/// `reasoning_effort` on the DeepSeek ladder, for a thinking-enabled request.
///
/// DeepSeek-style providers ([`OpenAiCompat::supports_thinking_control`])
/// take `low`/`high`/`max`
/// (<https://api-docs.deepseek.com/guides/thinking_mode>; `Off` is expressed
/// as `thinking: disabled`, not as an effort value). `XHigh` is sent as
/// `high`, matching DeepSeek's own documented mapping of a requested `xhigh`,
/// so it never silently selects the most expensive rung; only `Max` sends
/// `max`. [`OpenAiCompat::max_reasoning_effort`] is not consulted here.
///
/// Every other provider goes through
/// [`OpenAiCompat::openai_reasoning_effort`], which caps `XHigh`/`Max` at the
/// declared [`OpenAiCompat::max_reasoning_effort`] (default `high`, since most
/// providers reject an unknown string rather than rounding it).
fn deepseek_reasoning_effort(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High | ThinkingLevel::XHigh => "high",
        ThinkingLevel::Max => "max",
        ThinkingLevel::Off => unreachable!(),
    }
}

fn maybe_insert_assistant_after_tool_results(
    messages: &mut Vec<serde_json::Value>,
    compat: &OpenAiCompat,
) {
    if !compat.requires_assistant_after_tool_result {
        return;
    }

    let last_is_tool = messages
        .last()
        .and_then(|m| m.get("role"))
        .and_then(|role| role.as_str())
        == Some("tool");
    if last_is_tool {
        messages.push(serde_json::json!({
            "role": "assistant",
            "content": "",
        }));
    }
}

fn content_to_openai(content: &[Content]) -> serde_json::Value {
    if content.len() == 1 {
        if let Content::Text { text } = &content[0] {
            if !text.is_empty() {
                return serde_json::json!(text);
            }
        }
    }
    let parts: Vec<serde_json::Value> = content
        .iter()
        .filter(|c| !matches!(c, Content::Text { text } if text.is_empty()))
        .filter_map(|c| match c {
            Content::Text { text } => Some(serde_json::json!({"type": "text", "text": text})),
            Content::Image { data, mime_type } => Some(serde_json::json!({
                "type": "image_url",
                "image_url": {"url": format!("data:{};base64,{}", mime_type, data)},
            })),
            _ => None,
        })
        .collect();
    serde_json::json!(parts)
}

// OpenAI streaming response types
#[derive(Deserialize)]
struct OpenAiChunk {
    #[serde(default)]
    choices: Vec<OpenAiChoice>,
    #[serde(default)]
    usage: Option<OpenAiUsage>,
}

#[derive(Deserialize)]
struct OpenAiChoice {
    delta: OpenAiDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct OpenAiDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OpenAiToolCallDelta>>,
}

#[derive(Deserialize)]
struct OpenAiToolCallDelta {
    #[serde(default)]
    index: u32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<OpenAiFunctionDelta>,
}

#[derive(Deserialize)]
struct OpenAiFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

/// A token count that some servers send as explicit `null`.
///
/// `#[serde(default)]` covers only a *missing* key; `null` in a `u64` field
/// fails the whole payload — on Chat Completions the chunk carrying the usage
/// (often with the `finish_reason`), on Responses the terminal event.
pub(crate) fn null_as_zero<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    Ok(Option::<u64>::deserialize(d)?.unwrap_or(0))
}

#[derive(Deserialize)]
struct OpenAiUsage {
    #[serde(default, deserialize_with = "null_as_zero")]
    prompt_tokens: u64,
    #[serde(default, deserialize_with = "null_as_zero")]
    completion_tokens: u64,
    #[serde(default, deserialize_with = "null_as_zero")]
    total_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<OpenAiPromptTokensDetails>,
    #[serde(default)]
    prompt_cache_hit_tokens: Option<u64>,
    #[serde(default)]
    prompt_cache_miss_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct OpenAiPromptTokensDetails {
    #[serde(default, deserialize_with = "null_as_zero")]
    cached_tokens: u64,
    /// Prompt tokens written to the cache on this request (openai-python
    /// `PromptTokensDetails.cache_write_tokens`). Billed at a premium on
    /// GPT-5.6 and later.
    #[serde(default, deserialize_with = "null_as_zero")]
    cache_write_tokens: u64,
}

/// Maps a streamed usage chunk onto [`Usage`].
///
/// `prompt_tokens` includes both cached and cache-written tokens — OpenAI's
/// prompt-caching guide computes ordinary input as
/// `input_tokens - cached_tokens - cache_write_tokens`
/// (<https://developers.openai.com/api/docs/guides/prompt-caching>) — so `input`
/// excludes both, matching how the other providers split `Usage`. DeepSeek
/// reports the split directly as `prompt_cache_hit_tokens` /
/// `prompt_cache_miss_tokens`, which take precedence.
fn usage_from_openai(u: &OpenAiUsage) -> Usage {
    let details = u.prompt_tokens_details.as_ref();
    let cache_read = u
        .prompt_cache_hit_tokens
        .or_else(|| details.map(|d| d.cached_tokens))
        .unwrap_or(0);
    let cache_write = details.map(|d| d.cache_write_tokens).unwrap_or(0);
    Usage {
        input: u.prompt_cache_miss_tokens.unwrap_or_else(|| {
            u.prompt_tokens
                .saturating_sub(cache_read)
                .saturating_sub(cache_write)
        }),
        output: u.completion_tokens,
        cache_read,
        cache_write,
        total_tokens: u.total_tokens,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::model::{ModelConfig, ReasoningEffortCeiling};

    #[test]
    fn structured_output_sets_json_schema_response_format() {
        let mc = ModelConfig::openai("gpt-5.5", "GPT-5.5");
        let config = StreamConfig {
            model: "gpt-5.5".into(),
            system_prompt: "".into(),
            messages: vec![Message::user("Hello")],
            tools: vec![],
            thinking_level: ThinkingLevel::Off,
            api_key: "test".into(),
            max_tokens: None,
            temperature: None,
            model_config: Some(mc.clone()),
            cache_config: CacheConfig::default(),
            output_schema: Some(crate::provider::OutputSchema::new(
                "structured_output",
                serde_json::json!({"type": "object"}),
            )),
        };
        let body = build_request_body(&config, &mc, &OpenAiCompat::openai());
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(
            body["response_format"]["json_schema"]["name"],
            "structured_output"
        );
        assert_eq!(body["response_format"]["json_schema"]["strict"], true);
        assert_eq!(
            body["response_format"]["json_schema"]["schema"]["type"],
            "object"
        );
    }

    #[test]
    fn test_build_request_body_basic() {
        let model_config = ModelConfig::openai("gpt-4o", "GPT-4o");
        let config = StreamConfig {
            model: "gpt-4o".into(),
            system_prompt: "You are helpful.".into(),
            messages: vec![Message::user("Hello")],
            tools: vec![],
            thinking_level: ThinkingLevel::Off,
            api_key: "test".into(),
            max_tokens: None,
            temperature: None,
            model_config: Some(model_config.clone()),
            cache_config: CacheConfig::default(),
            output_schema: None,
        };

        let body = build_request_body(&config, &model_config, &OpenAiCompat::openai());
        assert_eq!(body["model"], "gpt-4o");
        assert!(body["stream"].as_bool().unwrap());
        // Developer role for OpenAI
        assert_eq!(body["messages"][0]["role"], "developer");
        assert_eq!(body["messages"][1]["role"], "user");
        // max_completion_tokens for OpenAI
        assert!(body["max_completion_tokens"].is_number());
    }

    #[test]
    fn test_build_request_body_with_tools() {
        let model_config = ModelConfig::openai("gpt-4o", "GPT-4o");
        let compat = OpenAiCompat::openai();
        let config = StreamConfig {
            model: "gpt-4o".into(),
            system_prompt: String::new(),
            messages: vec![Message::user("List files")],
            tools: vec![ToolDefinition {
                name: "bash".into(),
                description: "Run a command".into(),
                parameters: serde_json::json!({"type": "object"}),
            }],
            thinking_level: ThinkingLevel::Off,
            api_key: "test".into(),
            max_tokens: Some(1024),
            temperature: Some(0.5),
            model_config: Some(model_config.clone()),
            cache_config: CacheConfig::default(),
            output_schema: None,
        };

        let body = build_request_body(&config, &model_config, &compat);
        assert!(body["tools"].is_array());
        assert_eq!(body["tools"][0]["function"]["name"], "bash");
        assert_eq!(body["temperature"], 0.5);
    }

    #[test]
    fn test_build_request_body_deepseek_off_uses_current_api_shape() {
        let model_config = ModelConfig::deepseek("deepseek-v4-flash", "DeepSeek V4 Flash");
        let compat = model_config.compat.as_ref().unwrap().clone();
        let config = StreamConfig {
            model: "deepseek-v4-flash".into(),
            system_prompt: "You are helpful.".into(),
            messages: vec![Message::user("Hello")],
            tools: vec![],
            thinking_level: ThinkingLevel::Off,
            api_key: "test".into(),
            max_tokens: Some(1024),
            temperature: None,
            model_config: Some(model_config.clone()),
            cache_config: CacheConfig::default(),
            output_schema: None,
        };

        let body = build_request_body(&config, &model_config, &compat);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["max_tokens"], 1024);
        assert!(body.get("max_completion_tokens").is_none());
        assert_eq!(body["thinking"]["type"], "disabled");
        assert!(body.get("reasoning_effort").is_none());
        // Anthropic's breakpoint markers never belong on this path.
        assert!(!body.to_string().contains("cache_control"));
        // Nor does OpenAI's routing key: DeepSeek caches automatically and
        // does not read it, so `supports_prompt_cache_key` stays off. Caching
        // is enabled here — this asserts the compat gate, not the master
        // switch.
        assert!(config.cache_config.enabled);
        assert!(!compat.supports_prompt_cache_key);
        assert!(body.get("prompt_cache_key").is_none());
    }

    fn assistant(text: &str) -> Message {
        Message::assistant(
            vec![Content::Text { text: text.into() }],
            StopReason::Stop,
            "gpt-5.5",
            "openai",
            Usage::default(),
        )
    }

    /// Build a body against native OpenAI, which is the only compat provider
    /// with `supports_prompt_cache_key` on.
    fn openai_body(cache_config: CacheConfig, messages: Vec<Message>) -> serde_json::Value {
        let model_config = ModelConfig::openai("gpt-5.5", "GPT-5.5");
        let compat = model_config.compat.as_ref().unwrap().clone();
        let mut config = StreamConfig::new("gpt-5.5", "test");
        config.system_prompt = "You are helpful.".into();
        config.messages = messages;
        config.model_config = Some(model_config.clone());
        config.cache_config = cache_config;
        build_request_body(&config, &model_config, &compat)
    }

    #[test]
    fn prompt_cache_key_is_sent_when_caching_is_enabled() {
        let body = openai_body(CacheConfig::default(), vec![Message::user("Hello")]);
        let key = body["prompt_cache_key"]
            .as_str()
            .expect("enabled caching must send a routing key");
        assert!(key.starts_with("yo-"), "unexpected key shape: {key}");
    }

    #[test]
    fn prompt_cache_key_is_absent_when_caching_is_off() {
        for cfg in [
            CacheConfig::disabled(),
            CacheConfig {
                strategy: CacheStrategy::Disabled,
                ..CacheConfig::default()
            },
        ] {
            let body = openai_body(cfg, vec![Message::user("Hello")]);
            assert!(
                body.get("prompt_cache_key").is_none(),
                "disabled caching must send no routing key"
            );
        }
    }

    #[test]
    fn explicit_session_key_wins_over_derivation() {
        let body = openai_body(
            CacheConfig::default().with_session_key("tenant-42/session-7"),
            vec![Message::user("Hello")],
        );
        assert_eq!(body["prompt_cache_key"], "tenant-42/session-7");
    }

    /// The point of keying on the *head* rather than the whole message list:
    /// a key that changed every turn would route each request to a fresh cache
    /// and defeat the feature it exists to serve.
    #[test]
    fn derived_key_is_stable_as_the_conversation_grows() {
        let turn_1 = openai_body(CacheConfig::default(), vec![Message::user("Hello")]);
        let turn_5 = openai_body(
            CacheConfig::default(),
            vec![
                Message::user("Hello"),
                assistant("Hi"),
                Message::user("Second"),
                assistant("Sure"),
                Message::user("Third"),
            ],
        );
        assert_eq!(turn_1["prompt_cache_key"], turn_5["prompt_cache_key"]);
    }

    #[test]
    fn key_survives_a_compaction_marker_replacing_the_head() {
        let normal = openai_body(
            CacheConfig::default(),
            vec![Message::user("Deploy the API")],
        );
        // What `compact_messages` leaves at index 0 once it drops the head.
        // Referencing the constant rather than copying its text, so changing
        // the marker cannot silently stop covering this scenario.
        let compacted = openai_body(
            CacheConfig::default(),
            vec![
                Message::user(crate::context::COMPACTION_MARKER),
                Message::user("Later turn"),
            ],
        );
        assert_eq!(normal["prompt_cache_key"], compacted["prompt_cache_key"]);
    }

    /// `prompt_cache_key` must reach **only** native OpenAI. The failure this
    /// guards is the one the gate exists for: a strict compat server rejects
    /// the entire request over an unknown field. Nothing else pins this — a
    /// future preset written as `..OpenAiCompat::openai()` (the pattern
    /// `gpt_5_5` already uses) would switch it on silently.
    #[test]
    fn prompt_cache_key_reaches_only_native_openai() {
        let must_not_send = [
            ModelConfig::deepseek("deepseek-v4-flash", "DeepSeek"),
            ModelConfig::groq("llama-3.3-70b", "Llama"),
            ModelConfig::xai("grok-4-1-fast", "Grok"),
            ModelConfig::mistral("mistral-large", "Mistral"),
            ModelConfig::zai("glm-4", "Z.ai"),
            ModelConfig::qwen("qwen-max", "Qwen"),
            ModelConfig::minimax("abab-6", "MiniMax"),
            ModelConfig::meta("llama-4", "Meta"),
        ];

        for model_config in must_not_send {
            let compat = model_config.compat.as_ref().cloned().unwrap_or_default();
            assert!(
                !compat.supports_prompt_cache_key,
                "{} must not advertise prompt_cache_key support",
                model_config.provider
            );

            let mut config = StreamConfig::new(model_config.id.clone(), "test");
            config.system_prompt = "You are helpful.".into();
            config.messages = vec![Message::user("Hello")];
            config.model_config = Some(model_config.clone());

            let body = build_request_body(&config, &model_config, &compat);
            assert!(
                body.get("prompt_cache_key").is_none(),
                "{} must not receive prompt_cache_key",
                model_config.provider
            );
        }

        // ...and the one that must.
        let openai = ModelConfig::openai("gpt-5.5", "GPT-5.5");
        assert!(openai.compat.as_ref().unwrap().supports_prompt_cache_key);
    }

    /// An explicit key set against a provider that cannot carry it is a user
    /// instruction being discarded; the request goes out without it and the
    /// call site warns rather than dropping it silently.
    #[test]
    fn ungated_provider_sends_no_key_even_when_set_explicitly() {
        let model_config = ModelConfig::deepseek("deepseek-v4-flash", "DeepSeek V4 Flash");
        let compat = model_config.compat.as_ref().unwrap().clone();
        let mut config = StreamConfig::new("deepseek-v4-flash", "test");
        config.system_prompt = "You are helpful.".into();
        config.messages = vec![Message::user("Hello")];
        config.model_config = Some(model_config.clone());
        config.cache_config = CacheConfig::default().with_session_key("tenant-42");

        let body = build_request_body(&config, &model_config, &compat);
        assert!(body.get("prompt_cache_key").is_none());
    }

    #[test]
    fn test_build_request_body_deepseek_thinking_enabled() {
        let model_config = ModelConfig::deepseek("deepseek-v4-pro", "DeepSeek V4 Pro");
        let compat = model_config.compat.as_ref().unwrap().clone();
        let config = StreamConfig {
            model: "deepseek-v4-pro".into(),
            system_prompt: String::new(),
            messages: vec![Message::user("Solve this")],
            tools: vec![],
            thinking_level: ThinkingLevel::High,
            api_key: "test".into(),
            max_tokens: None,
            temperature: None,
            model_config: Some(model_config.clone()),
            cache_config: CacheConfig::default(),
            output_schema: None,
        };

        let body = build_request_body(&config, &model_config, &compat);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["max_tokens"], 384_000);
    }

    fn thinking_config(
        model_config: &ModelConfig,
        level: ThinkingLevel,
    ) -> (StreamConfig, OpenAiCompat) {
        let config = StreamConfig {
            model: model_config.id.clone(),
            system_prompt: String::new(),
            messages: vec![Message::user("Solve this")],
            tools: vec![],
            thinking_level: level,
            api_key: "test".into(),
            max_tokens: None,
            temperature: None,
            model_config: Some(model_config.clone()),
            cache_config: CacheConfig::default(),
            output_schema: None,
        };
        (config, model_config.compat.as_ref().unwrap().clone())
    }

    fn effort_for(model_config: &ModelConfig, level: ThinkingLevel) -> serde_json::Value {
        let (config, compat) = thinking_config(model_config, level);
        build_request_body(&config, model_config, &compat)["reasoning_effort"].clone()
    }

    #[test]
    fn test_deepseek_max_reaches_the_max_rung_and_xhigh_does_not() {
        // DeepSeek's reasoning_effort ladder is low/high/max (api-docs.deepseek.com
        // /guides/thinking_mode). Only Max selects max; XHigh goes out as high,
        // which is what DeepSeek itself maps a requested xhigh to.
        let deepseek = ModelConfig::deepseek("deepseek-v4-pro", "DeepSeek V4 Pro");
        assert_eq!(effort_for(&deepseek, ThinkingLevel::Max), "max");
        assert_eq!(effort_for(&deepseek, ThinkingLevel::XHigh), "high");
        let (config, compat) = thinking_config(&deepseek, ThinkingLevel::Max);
        let body = build_request_body(&config, &deepseek, &compat);
        assert_eq!(body["thinking"]["type"], "enabled");
    }

    #[test]
    fn test_deepseek_lower_levels_are_unchanged() {
        // Near-miss guard: only `Max` sends `max`. High stays `high`
        // (the old flag approach sent High as `max`); Medium goes out as
        // `medium`, which DeepSeek resolves server-side.
        let deepseek = ModelConfig::deepseek("deepseek-v4-pro", "DeepSeek V4 Pro");
        assert_eq!(effort_for(&deepseek, ThinkingLevel::High), "high");
        assert_eq!(effort_for(&deepseek, ThinkingLevel::Medium), "medium");
        assert_eq!(effort_for(&deepseek, ThinkingLevel::Low), "low");
        assert_eq!(effort_for(&deepseek, ThinkingLevel::Minimal), "low");
    }

    const ALL_LEVELS: [ThinkingLevel; 7] = [
        ThinkingLevel::Off,
        ThinkingLevel::Minimal,
        ThinkingLevel::Low,
        ThinkingLevel::Medium,
        ThinkingLevel::High,
        ThinkingLevel::XHigh,
        ThinkingLevel::Max,
    ];

    fn with_effort_caps(
        mut model_config: ModelConfig,
        ceiling: ReasoningEffortCeiling,
    ) -> ModelConfig {
        let compat = model_config.compat.as_mut().unwrap();
        compat.max_reasoning_effort = ceiling;
        model_config
    }

    #[test]
    fn test_effort_ceiling_xhigh_sends_xhigh_for_xhigh_and_max() {
        // Positive control for the #176 fix: the clamp is lifted exactly as
        // far as the declared ceiling.
        let mc = with_effort_caps(
            ModelConfig::openai("gpt-5.4", "GPT-5.4"),
            ReasoningEffortCeiling::XHigh,
        );
        assert_eq!(effort_for(&mc, ThinkingLevel::XHigh), "xhigh");
        assert_eq!(effort_for(&mc, ThinkingLevel::Max), "xhigh");
        assert_eq!(effort_for(&mc, ThinkingLevel::High), "high");
        assert_eq!(effort_for(&mc, ThinkingLevel::Medium), "medium");
        assert_eq!(effort_for(&mc, ThinkingLevel::Minimal), "low");
    }

    #[test]
    fn test_effort_ceiling_max_sends_max_only_for_max() {
        let mc = with_effort_caps(
            ModelConfig::openai("gpt-5.6", "GPT-5.6"),
            ReasoningEffortCeiling::Max,
        );
        assert_eq!(effort_for(&mc, ThinkingLevel::Max), "max");
        assert_eq!(effort_for(&mc, ThinkingLevel::XHigh), "xhigh");
        assert_eq!(effort_for(&mc, ThinkingLevel::High), "high");
        assert_eq!(effort_for(&mc, ThinkingLevel::Low), "low");
    }

    #[test]
    fn test_off_omits_reasoning_effort_on_every_ceiling() {
        // `Off` sends nothing — the model runs at its own default — whatever
        // the ceiling, including the presets whose models have a `none` rung.
        for mc in [
            ModelConfig::gpt_5_5(),
            with_effort_caps(
                ModelConfig::openai("gpt-5.6", "GPT-5.6"),
                ReasoningEffortCeiling::Max,
            ),
            ModelConfig::openai("gpt-5", "GPT-5"),
            ModelConfig::xai("grok-4.7", "Grok 4.7"),
            ModelConfig::groq("llama", "Llama"),
        ] {
            let (config, compat) = thinking_config(&mc, ThinkingLevel::Off);
            let body = build_request_body(&config, &mc, &compat);
            assert!(body.get("reasoning_effort").is_none(), "{}", mc.id);
            // Positive control: the same config does send an effort when
            // thinking is on, so the omission above is Off's doing.
            if compat.supports_reasoning_effort {
                assert_eq!(effort_for(&mc, ThinkingLevel::Low), "low", "{}", mc.id);
            }
        }
    }

    #[test]
    fn test_gpt_5_5_preset_reaches_xhigh_and_omits_effort_for_off() {
        // gpt-5.5: low/medium/high/xhigh, no max.
        let gpt = ModelConfig::gpt_5_5();
        assert!(effort_for(&gpt, ThinkingLevel::Off).is_null());
        assert_eq!(effort_for(&gpt, ThinkingLevel::XHigh), "xhigh");
        assert_eq!(effort_for(&gpt, ThinkingLevel::Max), "xhigh");
        assert_eq!(effort_for(&gpt, ThinkingLevel::High), "high");
        assert_eq!(effort_for(&gpt, ThinkingLevel::Medium), "medium");
        assert_eq!(effort_for(&gpt, ThinkingLevel::Low), "low");
    }

    #[test]
    fn test_deepseek_ignores_the_openai_effort_caps() {
        // DeepSeek's ladder is its own: even with an OpenAI ceiling
        // declared on it, Off stays `thinking: disabled` with no effort,
        // XHigh stays `high` and Max stays `max`.
        let deepseek = with_effort_caps(
            ModelConfig::deepseek("deepseek-v4-pro", "DeepSeek V4 Pro"),
            ReasoningEffortCeiling::XHigh,
        );
        assert_eq!(effort_for(&deepseek, ThinkingLevel::XHigh), "high");
        assert_eq!(effort_for(&deepseek, ThinkingLevel::Max), "max");
        let (config, compat) = thinking_config(&deepseek, ThinkingLevel::Off);
        let body = build_request_body(&config, &deepseek, &compat);
        assert!(body.get("reasoning_effort").is_none());
        assert_eq!(body["thinking"]["type"], "disabled");
    }

    /// The mapping every provider used before `max_reasoning_effort`
    /// existed, transcribed from the removed code.
    fn legacy_effort(level: ThinkingLevel, compat: &OpenAiCompat) -> Option<&'static str> {
        if level == ThinkingLevel::Off || !compat.supports_reasoning_effort {
            return None;
        }
        Some(match level {
            ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
            ThinkingLevel::Medium => "medium",
            ThinkingLevel::Max if compat.supports_thinking_control => "max",
            _ => "high",
        })
    }

    #[test]
    fn test_presets_without_effort_caps_send_byte_identical_bodies() {
        // Near-miss guard for #176: the capability is opt-in. Every preset
        // that did not gain a ceiling must produce exactly
        // the body it produced before — compared as whole bodies against a
        // copy with the legacy effort spliced in, so a stray field or a
        // moved key fails too.
        let unchanged = [
            ModelConfig::openai("gpt-5.5", "GPT-5.5"),
            ModelConfig::openai("o3", "o3"),
            ModelConfig::meta("muse-spark-1.2", "Muse Spark 1.2"),
            ModelConfig::deepseek("deepseek-v4-pro", "DeepSeek V4 Pro"),
            ModelConfig::groq("llama-3.3-70b-versatile", "Llama"),
            ModelConfig::qwen("qwen3.6-plus", "Qwen"),
            ModelConfig::zai("glm-5", "GLM-5"),
            ModelConfig::minimax("MiniMax-M1", "M1"),
            ModelConfig::mistral("mistral-large-latest", "Mistral"),
            ModelConfig::ollama("http://localhost:11434/v1", "m"),
            ModelConfig::local("http://localhost:1234/v1", "m"),
            ModelConfig::openai_compat("http://h/v1", "m", "p", OpenAiCompat::openrouter()),
            ModelConfig::openai_compat("http://h/v1", "m", "p", OpenAiCompat::cerebras()),
        ];
        for mc in &unchanged {
            let compat = mc.compat.as_ref().unwrap();
            assert_eq!(compat.max_reasoning_effort, ReasoningEffortCeiling::High);
            for level in ALL_LEVELS {
                let (config, compat) = thinking_config(mc, level);
                let body = build_request_body(&config, mc, &compat);
                let mut expected = body.clone();
                expected.as_object_mut().unwrap().remove("reasoning_effort");
                if let Some(e) = legacy_effort(level, &compat) {
                    expected["reasoning_effort"] = serde_json::json!(e);
                }
                assert_eq!(
                    serde_json::to_string(&body).unwrap(),
                    serde_json::to_string(&expected).unwrap(),
                    "{} at {level:?}",
                    mc.id
                );
            }
        }
    }

    #[test]
    fn test_off_bodies_are_byte_identical_to_pre_176_for_capped_presets() {
        // The presets that did gain a ceiling: at `Off` their body is exactly
        // the pre-#176 one — the body the same config builds when it takes no
        // effort at all, so no `reasoning_effort` key and nothing else moved.
        for mc in [
            ModelConfig::gpt_5_5(),
            ModelConfig::xai("grok-4.7", "Grok 4.7"),
            with_effort_caps(
                ModelConfig::openai("gpt-6-sol", "GPT-6 Sol"),
                ReasoningEffortCeiling::Max,
            ),
        ] {
            let (config, compat) = thinking_config(&mc, ThinkingLevel::Off);
            let body = build_request_body(&config, &mc, &compat);
            let mut legacy_compat = compat.clone();
            legacy_compat.supports_reasoning_effort = false;
            let mut legacy_mc = mc.clone();
            legacy_mc.compat = Some(legacy_compat.clone());
            let legacy = build_request_body(&config, &legacy_mc, &legacy_compat);
            assert!(legacy_effort(ThinkingLevel::Off, &compat).is_none());
            assert_eq!(
                serde_json::to_string(&body).unwrap(),
                serde_json::to_string(&legacy).unwrap(),
                "{}",
                mc.id
            );
        }
    }

    #[test]
    fn test_effort_caps_deserialize_to_legacy_defaults() {
        // A compat persisted before the fields existed keeps its old behaviour.
        let mut v = serde_json::to_value(OpenAiCompat::openai()).unwrap();
        let obj = v.as_object_mut().unwrap();
        assert_eq!(obj.remove("max_reasoning_effort").unwrap(), "high");
        assert!(obj.get("supports_effort_none").is_none());
        let back: OpenAiCompat = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(back.max_reasoning_effort, ReasoningEffortCeiling::High);
        // A compat persisted by a pre-release build that still carried
        // `supports_effort_none` loads; the key is ignored.
        v["supports_effort_none"] = serde_json::json!(true);
        let back: OpenAiCompat = serde_json::from_value(v).unwrap();
        assert_eq!(back.max_reasoning_effort, ReasoningEffortCeiling::High);
        // The wire names of the rungs.
        for (ceiling, name) in [
            (ReasoningEffortCeiling::High, "high"),
            (ReasoningEffortCeiling::XHigh, "xhigh"),
            (ReasoningEffortCeiling::Max, "max"),
        ] {
            assert_eq!(serde_json::to_value(ceiling).unwrap(), name);
        }
    }

    #[test]
    fn test_openai_xhigh_and_max_clamp_to_high() {
        // A provider without DeepSeek-style thinking control tops out at
        // `high` and rejects unknown strings, so the upper rungs clamp.
        let openai = ModelConfig::openai("gpt-5.5", "GPT-5.5");
        assert_eq!(effort_for(&openai, ThinkingLevel::XHigh), "high");
        assert_eq!(effort_for(&openai, ThinkingLevel::Max), "high");
        assert_eq!(effort_for(&openai, ThinkingLevel::High), "high");
        assert_eq!(effort_for(&openai, ThinkingLevel::Medium), "medium");
        assert_eq!(effort_for(&openai, ThinkingLevel::Low), "low");
    }

    #[test]
    fn test_build_request_body_qwen_uses_max_tokens_and_streaming_usage() {
        let model_config = ModelConfig::qwen("qwen3.6-plus", "Qwen 3.6 Plus");
        let compat = model_config.compat.as_ref().unwrap().clone();
        let config = StreamConfig {
            model: "qwen3.6-plus".into(),
            system_prompt: "You are helpful.".into(),
            messages: vec![Message::user("Hello")],
            tools: vec![],
            thinking_level: ThinkingLevel::High,
            api_key: "test".into(),
            max_tokens: Some(2048),
            temperature: None,
            model_config: Some(model_config.clone()),
            cache_config: CacheConfig::default(),
            output_schema: None,
        };

        let body = build_request_body(&config, &model_config, &compat);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["max_tokens"], 2048);
        assert!(body.get("max_completion_tokens").is_none());
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn test_build_request_body_qwen_tools_use_openai_shape() {
        let model_config = ModelConfig::qwen("qwen3-coder-plus", "Qwen 3 Coder Plus");
        let compat = model_config.compat.as_ref().unwrap().clone();
        let config = StreamConfig {
            model: "qwen3-coder-plus".into(),
            system_prompt: String::new(),
            messages: vec![Message::user("List files")],
            tools: vec![ToolDefinition {
                name: "list_files".into(),
                description: "List files".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"}
                    }
                }),
            }],
            thinking_level: ThinkingLevel::Off,
            api_key: "test".into(),
            max_tokens: None,
            temperature: None,
            model_config: Some(model_config.clone()),
            cache_config: CacheConfig::default(),
            output_schema: None,
        };

        let body = build_request_body(&config, &model_config, &compat);
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "list_files");
        assert_eq!(
            body["tools"][0]["function"]["parameters"]["properties"]["path"]["type"],
            "string"
        );
    }

    #[test]
    fn test_deepseek_usage_cache_fields_parse() {
        let chunk: OpenAiChunk = serde_json::from_value(serde_json::json!({
            "choices": [],
            "usage": {
                "prompt_tokens": 100,
                "prompt_cache_hit_tokens": 70,
                "prompt_cache_miss_tokens": 30,
                "completion_tokens": 10,
                "total_tokens": 110
            }
        }))
        .unwrap();

        let usage = usage_from_openai(&chunk.usage.unwrap());
        assert_eq!(usage.input, 30);
        assert_eq!(usage.cache_read, 70);
        assert_eq!(usage.cache_write, 0);
        assert_eq!(usage.output, 10);
        assert_eq!(usage.total_tokens, 110);
    }

    #[test]
    fn test_usage_tolerates_explicit_nulls() {
        // One `null` count used to fail the whole chunk, dropping the usage
        // and any finish_reason riding on it.
        let chunk: OpenAiChunk = serde_json::from_str(
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":100,"completion_tokens":null,"total_tokens":null,
                         "prompt_tokens_details":{"cached_tokens":null,"cache_write_tokens":null}}}"#,
        )
        .unwrap();
        assert_eq!(chunk.choices[0].finish_reason.as_deref(), Some("stop"));
        let usage = usage_from_openai(chunk.usage.as_ref().unwrap());
        assert_eq!((usage.input, usage.output, usage.cache_read), (100, 0, 0));
    }

    #[test]
    fn test_usage_cache_write_tokens_parsed_and_excluded_from_input() {
        // prompt_tokens includes both the cached and the cache-written tokens
        // (OpenAI's prompt-caching guide: ordinary input =
        // input_tokens - cached_tokens - cache_write_tokens).
        let chunk: OpenAiChunk = serde_json::from_value(serde_json::json!({
            "choices": [],
            "usage": {
                "prompt_tokens": 15000,
                "completion_tokens": 200,
                "total_tokens": 15200,
                "prompt_tokens_details": {
                    "cached_tokens": 12000,
                    "cache_write_tokens": 2500
                }
            }
        }))
        .unwrap();
        let usage = usage_from_openai(&chunk.usage.unwrap());
        assert_eq!(usage.cache_read, 12000);
        assert_eq!(usage.cache_write, 2500);
        assert_eq!(usage.input, 500);
        assert_eq!(usage.output, 200);
        assert_eq!(usage.total_tokens, 15200);
    }

    #[test]
    fn test_usage_without_cache_write_tokens_is_unchanged() {
        // Positive control: a chunk with only cached_tokens splits as before.
        let chunk: OpenAiChunk = serde_json::from_value(serde_json::json!({
            "choices": [],
            "usage": {
                "prompt_tokens": 1000,
                "completion_tokens": 5,
                "total_tokens": 1005,
                "prompt_tokens_details": {"cached_tokens": 768}
            }
        }))
        .unwrap();
        let usage = usage_from_openai(&chunk.usage.unwrap());
        assert_eq!(usage.input, 232);
        assert_eq!(usage.cache_read, 768);
        assert_eq!(usage.cache_write, 0);
    }

    #[test]
    fn test_content_to_openai_simple_text() {
        let content = vec![Content::Text {
            text: "hello".into(),
        }];
        let result = content_to_openai(&content);
        assert_eq!(result, "hello");
    }

    #[test]
    fn test_content_to_openai_filters_empty_text() {
        let content = vec![
            Content::Text { text: "".into() },
            Content::Text {
                text: "hello".into(),
            },
            Content::Text { text: "".into() },
        ];
        let result = content_to_openai(&content);
        let parts = result.as_array().unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["text"], "hello");
    }

    #[test]
    fn test_content_to_openai_single_empty_text_filtered() {
        let content = vec![Content::Text { text: "".into() }];
        let result = content_to_openai(&content);
        let parts = result.as_array().unwrap();
        assert!(parts.is_empty());
    }

    #[test]
    fn test_content_to_openai_multipart() {
        let content = vec![
            Content::Text {
                text: "look at this".into(),
            },
            Content::Image {
                data: "abc".into(),
                mime_type: "image/png".into(),
            },
        ];
        let result = content_to_openai(&content);
        assert!(result.is_array());
        assert_eq!(result[0]["type"], "text");
        assert_eq!(result[1]["type"], "image_url");
    }

    #[test]
    fn test_tool_result_with_image() {
        let model_config = ModelConfig::openai("gpt-4o", "GPT-4o");
        let compat = OpenAiCompat::openai();
        let config = StreamConfig {
            model: "gpt-4o".into(),
            system_prompt: String::new(),
            messages: vec![
                Message::Assistant {
                    content: vec![Content::ToolCall {
                        provider_metadata: None,
                        id: "call-1".into(),
                        name: "read_file".into(),
                        arguments: serde_json::json!({"path": "img.png"}),
                    }],
                    stop_reason: StopReason::ToolUse,
                    model: "test".into(),
                    provider: "test".into(),
                    usage: Usage::default(),
                    timestamp: 0,
                    error_message: None,
                },
                Message::ToolResult {
                    tool_call_id: "call-1".into(),
                    tool_name: "read_file".into(),
                    content: vec![Content::Image {
                        data: "aW1hZ2VkYXRh".into(),
                        mime_type: "image/png".into(),
                    }],
                    is_error: false,
                    timestamp: 0,
                },
            ],
            tools: vec![],
            thinking_level: ThinkingLevel::Off,
            api_key: "test".into(),
            max_tokens: None,
            temperature: None,
            model_config: Some(model_config.clone()),
            cache_config: CacheConfig::default(),
            output_schema: None,
        };

        let body = build_request_body(&config, &model_config, &compat);
        let msgs = body["messages"].as_array().unwrap();
        // tool result is the last message (after system + assistant)
        let tool_msg = msgs.last().unwrap();
        assert_eq!(tool_msg["role"], "tool");
        // content should be an array with image_url
        let content = tool_msg["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "image_url");
        assert!(content[0]["image_url"]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,"));
    }

    #[test]
    fn test_tool_result_text_only_uses_string() {
        let model_config = ModelConfig::openai("gpt-4o", "GPT-4o");
        let compat = OpenAiCompat::openai();
        let config = StreamConfig {
            model: "gpt-4o".into(),
            system_prompt: String::new(),
            messages: vec![Message::ToolResult {
                tool_call_id: "call-1".into(),
                tool_name: "bash".into(),
                content: vec![Content::Text {
                    text: "hello".into(),
                }],
                is_error: false,
                timestamp: 0,
            }],
            tools: vec![],
            thinking_level: ThinkingLevel::Off,
            api_key: "test".into(),
            max_tokens: None,
            temperature: None,
            model_config: Some(model_config.clone()),
            cache_config: CacheConfig::default(),
            output_schema: None,
        };

        let body = build_request_body(&config, &model_config, &compat);
        let msgs = body["messages"].as_array().unwrap();
        let tool_msg = msgs.last().unwrap();
        // Text-only: content should be a plain string
        assert_eq!(tool_msg["content"], "hello");
    }

    #[test]
    fn test_ollama_inserts_assistant_after_tool_result_run() {
        let model_config = ModelConfig::ollama("http://localhost:11434/v1", "llama3.1:8b");
        let compat = model_config.compat.as_ref().unwrap().clone();
        let config = StreamConfig {
            model: "llama3.1:8b".into(),
            system_prompt: String::new(),
            messages: vec![
                Message::Assistant {
                    content: vec![Content::ToolCall {
                        provider_metadata: None,
                        id: "call-1".into(),
                        name: "bash".into(),
                        arguments: serde_json::json!({"cmd": "ls"}),
                    }],
                    stop_reason: StopReason::ToolUse,
                    model: "test".into(),
                    provider: "test".into(),
                    usage: Usage::default(),
                    timestamp: 0,
                    error_message: None,
                },
                Message::ToolResult {
                    tool_call_id: "call-1".into(),
                    tool_name: "bash".into(),
                    content: vec![Content::Text {
                        text: "a.txt\nb.txt".into(),
                    }],
                    is_error: false,
                    timestamp: 0,
                },
                Message::User {
                    content: vec![Content::Text {
                        text: "which is largest?".into(),
                    }],
                    timestamp: 0,
                },
            ],
            tools: vec![],
            thinking_level: ThinkingLevel::Off,
            api_key: "test".into(),
            max_tokens: None,
            temperature: None,
            model_config: Some(model_config.clone()),
            cache_config: CacheConfig::default(),
            output_schema: None,
        };

        let body = build_request_body(&config, &model_config, &compat);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "assistant");
        assert_eq!(msgs[1]["role"], "tool");
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[2]["content"], "");
        assert_eq!(msgs[3]["role"], "user");
    }

    #[test]
    fn test_ollama_inserts_one_assistant_after_multiple_tool_results() {
        let model_config = ModelConfig::ollama("http://localhost:11434/v1", "qwen2.5-coder:7b");
        let compat = model_config.compat.as_ref().unwrap().clone();
        let config = StreamConfig {
            model: "qwen2.5-coder:7b".into(),
            system_prompt: String::new(),
            messages: vec![
                Message::ToolResult {
                    tool_call_id: "call-1".into(),
                    tool_name: "read_file".into(),
                    content: vec![Content::Text { text: "a".into() }],
                    is_error: false,
                    timestamp: 0,
                },
                Message::ToolResult {
                    tool_call_id: "call-2".into(),
                    tool_name: "read_file".into(),
                    content: vec![Content::Text { text: "b".into() }],
                    is_error: false,
                    timestamp: 0,
                },
            ],
            tools: vec![],
            thinking_level: ThinkingLevel::Off,
            api_key: "test".into(),
            max_tokens: None,
            temperature: None,
            model_config: Some(model_config.clone()),
            cache_config: CacheConfig::default(),
            output_schema: None,
        };

        let body = build_request_body(&config, &model_config, &compat);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["role"], "tool");
        assert_eq!(msgs[1]["role"], "tool");
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[2]["content"], "");
    }

    #[test]
    fn test_ollama_does_not_insert_assistant_before_existing_assistant() {
        let model_config = ModelConfig::ollama("http://localhost:11434/v1", "llama3.1:8b");
        let compat = model_config.compat.as_ref().unwrap().clone();
        let config = StreamConfig {
            model: "llama3.1:8b".into(),
            system_prompt: String::new(),
            messages: vec![
                Message::ToolResult {
                    tool_call_id: "call-1".into(),
                    tool_name: "read_file".into(),
                    content: vec![Content::Text { text: "a".into() }],
                    is_error: false,
                    timestamp: 0,
                },
                Message::Assistant {
                    content: vec![Content::Text {
                        text: "The file contains a.".into(),
                    }],
                    stop_reason: StopReason::Stop,
                    model: "test".into(),
                    provider: "test".into(),
                    usage: Usage::default(),
                    timestamp: 0,
                    error_message: None,
                },
                Message::User {
                    content: vec![Content::Text {
                        text: "thanks".into(),
                    }],
                    timestamp: 0,
                },
            ],
            tools: vec![],
            thinking_level: ThinkingLevel::Off,
            api_key: "test".into(),
            max_tokens: None,
            temperature: None,
            model_config: Some(model_config.clone()),
            cache_config: CacheConfig::default(),
            output_schema: None,
        };

        let body = build_request_body(&config, &model_config, &compat);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["role"], "tool");
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["content"][0]["text"], "The file contains a.");
        assert_eq!(msgs[2]["role"], "user");
    }

    fn thinking_tool_history() -> Vec<Message> {
        vec![
            Message::user("What's the weather in Hangzhou?"),
            Message::assistant(
                vec![
                    Content::thinking("Need the date first."),
                    Content::tool_call("call-1", "get_date", serde_json::json!({})),
                ],
                StopReason::ToolUse,
                "deepseek-v4-pro",
                "deepseek",
                Usage::default(),
            ),
            Message::ToolResult {
                tool_call_id: "call-1".into(),
                tool_name: "get_date".into(),
                content: vec![Content::Text {
                    text: "2026-09-25".into(),
                }],
                is_error: false,
                timestamp: 0,
            },
            Message::assistant(
                vec![
                    Content::thinking("Have the date. "),
                    Content::thinking("Answer now."),
                    Content::Text {
                        text: "Cloudy, 7-13C.".into(),
                    },
                ],
                StopReason::Stop,
                "deepseek-v4-pro",
                "deepseek",
                Usage::default(),
            ),
            Message::user("And tomorrow?"),
        ]
    }

    fn history_body(
        model_config: &ModelConfig,
        messages: Vec<Message>,
        with_tools: bool,
    ) -> serde_json::Value {
        let compat = model_config.compat.as_ref().unwrap().clone();
        let config = StreamConfig {
            model: model_config.id.clone(),
            system_prompt: String::new(),
            messages,
            tools: if with_tools {
                vec![ToolDefinition {
                    name: "get_date".into(),
                    description: "Get the current date".into(),
                    parameters: serde_json::json!({"type": "object"}),
                }]
            } else {
                vec![]
            },
            thinking_level: ThinkingLevel::High,
            api_key: "test".into(),
            max_tokens: None,
            temperature: None,
            model_config: Some(model_config.clone()),
            cache_config: CacheConfig::default(),
            output_schema: None,
        };
        build_request_body(&config, model_config, &compat)
    }

    #[test]
    fn test_deepseek_replays_reasoning_content_with_tools() {
        // api-docs.deepseek.com/guides/thinking_mode: with tools, the
        // reasoning_content of all previous turns must be passed back, even
        // turns without a tool call, or the API returns 400.
        let deepseek = ModelConfig::deepseek("deepseek-v4-pro", "DeepSeek V4 Pro");
        let body = history_body(&deepseek, thinking_tool_history(), true);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["reasoning_content"], "Need the date first.");
        assert_eq!(msgs[1]["tool_calls"][0]["function"]["name"], "get_date");
        // The turn without a tool call carries its reasoning too, joined.
        assert_eq!(msgs[3]["role"], "assistant");
        assert_eq!(msgs[3]["reasoning_content"], "Have the date. Answer now.");
        assert_eq!(msgs[3]["content"][0]["text"], "Cloudy, 7-13C.");
        // Never on user or tool messages.
        assert!(msgs[0].get("reasoning_content").is_none());
        assert!(msgs[2].get("reasoning_content").is_none());
        assert!(msgs[4].get("reasoning_content").is_none());
    }

    #[test]
    fn test_deepseek_omits_reasoning_content_without_tools() {
        // Without tools DeepSeek ignores reasoning_content, so it is not sent.
        let deepseek = ModelConfig::deepseek("deepseek-v4-pro", "DeepSeek V4 Pro");
        let body = history_body(&deepseek, thinking_tool_history(), false);
        assert!(!body.to_string().contains("reasoning_content"));
    }

    #[test]
    fn test_other_providers_never_send_reasoning_content() {
        // Near-miss: the same history with tools, on providers without the
        // flag. The body must be byte-identical to the one built from a
        // history with the thinking blocks removed, i.e. what was sent before.
        let mut stripped = thinking_tool_history();
        for m in &mut stripped {
            if let Message::Assistant { content, .. } = m {
                content.retain(|c| !matches!(c, Content::Thinking { .. }));
            }
        }
        for mc in [
            ModelConfig::openai("gpt-5.5", "GPT-5.5"),
            ModelConfig::xai("grok-4.7", "Grok 4.7"),
            ModelConfig::qwen("qwen3.6-plus", "Qwen 3.6 Plus"),
            ModelConfig::groq("llama-3.3-70b-versatile", "Llama 3.3 70B"),
        ] {
            let body = history_body(&mc, thinking_tool_history(), true);
            assert!(
                !body.to_string().contains("reasoning_content"),
                "{} sent reasoning_content",
                mc.provider
            );
            let baseline = history_body(&mc, stripped.clone(), true);
            assert_eq!(body.to_string(), baseline.to_string(), "{}", mc.provider);
        }
    }

    #[test]
    fn test_replays_reasoning_content_is_deepseek_only_among_presets() {
        assert!(OpenAiCompat::deepseek().replays_reasoning_content);
        for compat in [
            OpenAiCompat::default(),
            OpenAiCompat::openai(),
            OpenAiCompat::meta(),
            OpenAiCompat::xai(),
            OpenAiCompat::groq(),
            OpenAiCompat::cerebras(),
            OpenAiCompat::openrouter(),
            OpenAiCompat::mistral(),
            OpenAiCompat::zai(),
            OpenAiCompat::minimax(),
            OpenAiCompat::qwen(),
        ] {
            assert!(!compat.replays_reasoning_content);
        }
        // A config persisted before the flag existed still deserializes.
        let mut v = serde_json::to_value(OpenAiCompat::deepseek()).unwrap();
        v.as_object_mut()
            .unwrap()
            .remove("replays_reasoning_content");
        let back: OpenAiCompat = serde_json::from_value(v).unwrap();
        assert!(!back.replays_reasoning_content);
    }

    #[test]
    fn test_xai_sends_reasoning_effort() {
        // docs.x.ai reasoning guide: grok-4.5/4.6/4.7 take reasoning_effort
        // low/medium/high, plus xhigh on 4.6+; older models treat xhigh as
        // high rather than rejecting it, so the preset's ceiling is XHigh.
        let xai = ModelConfig::xai("grok-4.7", "Grok 4.7");
        assert_eq!(effort_for(&xai, ThinkingLevel::Minimal), "low");
        assert_eq!(effort_for(&xai, ThinkingLevel::Low), "low");
        assert_eq!(effort_for(&xai, ThinkingLevel::Medium), "medium");
        assert_eq!(effort_for(&xai, ThinkingLevel::High), "high");
        assert_eq!(effort_for(&xai, ThinkingLevel::XHigh), "xhigh");
        // xAI has no `max` rung: Max stops at the ceiling.
        assert_eq!(effort_for(&xai, ThinkingLevel::Max), "xhigh");
        let (config, compat) = thinking_config(&xai, ThinkingLevel::High);
        let body = build_request_body(&config, &xai, &compat);
        // xAI has no DeepSeek-style thinking toggle.
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn test_xai_off_omits_reasoning_effort() {
        // Reasoning cannot be disabled on xAI: Off sends nothing and the model
        // runs at its default (high). No invented "none" value.
        let xai = ModelConfig::xai("grok-4.7", "Grok 4.7");
        let (config, compat) = thinking_config(&xai, ThinkingLevel::Off);
        let body = build_request_body(&config, &xai, &compat);
        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("thinking").is_none());
    }
}
