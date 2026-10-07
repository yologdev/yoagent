//! Request side of the Responses API, shared by
//! [`OpenAiResponsesProvider`](super::OpenAiResponsesProvider) and
//! [`AzureOpenAiProvider`](super::AzureOpenAiProvider): the request body
//! (message conversion, tools, reasoning effort, caching, encrypted reasoning)
//! and the read loop that turns the event stream into a [`Message`].
//!
//! The two providers differ only in the URL, the auth header and Azure's
//! legacy deployment override of `model`; everything here is the same API.
//!
//! Field shapes follow OpenAI's generated types (`openai-python`,
//! `src/openai/types/responses/response_create_params.py` and
//! `response_reasoning_item_param.py`):
//!
//! - `prompt_cache_key` (string) routes requests sharing a prefix to the same
//!   cache. Azure documents the same field ("You don't need a specific API
//!   version to use `prompt_cache_key`").
//! - `include: ["reasoning.encrypted_content"]` "includes an encrypted version
//!   of reasoning tokens in reasoning item outputs. This enables reasoning
//!   items to be used in multi-turn conversations when using the Responses API
//!   statelessly" — which is how yoagent uses it: it resends the whole history
//!   and never uses `previous_response_id`. In stateless mode (`store: false`
//!   or Zero Data Retention) the API returns `encrypted_content` by default
//!   and still accepts the `include` value; with the default `store: true` it
//!   is what asks for it.
//! - A reasoning input item is `{"type": "reasoning", "id", "summary",
//!   "encrypted_content"}` (`id` and `summary` required). OpenAI: "If the
//!   model calls multiple functions consecutively, you should pass back all
//!   reasoning items, function call items, and function call output items,
//!   since the last `user` message."
//! - A replayed reasoning item must be followed by its paired output item,
//!   identified by `id` ("Item 'rs_…' of type 'reasoning' was provided
//!   without its required following item"). So the `function_call` and
//!   `message` items that followed a replayed reasoning item carry the `id`
//!   (`fc_…` / `msg_…`) the same API gave them, recorded in the reasoning
//!   item's stored JSON. Items with no replayed reasoning before them are
//!   sent without ids, as before.
//! - A non-reasoning model rejects reasoning input items, so neither
//!   `include` nor any reasoning item is sent to one (the same test for
//!   both), and the paired items then go without ids.

use super::model::{ApiProtocol, OpenAiCompat};
use super::responses_stream::{Flow, ResponsesStreamState, CALL_IDS_KEY, MESSAGE_ID_KEY};
use super::traits::*;
use crate::types::*;
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::HashMap;
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// The `include` value that asks for encrypted reasoning.
pub(crate) const INCLUDE_ENCRYPTED_REASONING: &str = "reasoning.encrypted_content";

/// Build the Responses request body. `protocol` is the API the request goes
/// to: only encrypted reasoning that came from that same protocol is
/// replayed (OpenAI's and Azure's are separate services).
pub(crate) fn build_request_body(config: &StreamConfig, protocol: ApiProtocol) -> Value {
    // The effort capability comes from `ModelConfig::compat` (see
    // `OpenAiCompat::max_reasoning_effort`); `None` means a `high` ceiling.
    // `Off` omits it.
    let default_compat = OpenAiCompat::default();
    let compat = config
        .model_config
        .as_ref()
        .and_then(|m| m.compat.as_ref())
        .unwrap_or(&default_compat);
    let effort = compat.openai_reasoning_effort(&config.model, config.thinking_level);

    // Encrypted reasoning — asked for, and replayed from history — only for
    // a reasoning model: declared by `ModelConfig::reasoning`, or implied by
    // sending a reasoning effort (which a non-reasoning model rejects
    // anyway). A model without reasoning has nothing to return for it and
    // rejects reasoning input items ("Encrypted content is not supported
    // with this model"), so neither is sent there — e.g. after
    // `Agent::set_model` from a reasoning model to `gpt-4.1` on the same API.
    let reasoning_model =
        config.model_config.as_ref().is_some_and(|m| m.reasoning) || effort.is_some();
    // Replay is keyed on the target protocol; `None` replays nothing.
    let replay = reasoning_model.then_some(protocol);

    let mut input: Vec<Value> = Vec::new();

    for msg in &config.messages {
        match msg {
            Message::User { content, .. } => {
                let user_content = input_parts(content);
                if user_content.len() == 1 && user_content[0]["type"] == "input_text" {
                    // Simple text-only message can use shorthand format
                    input.push(json!({
                        "role": "user",
                        "content": user_content[0]["text"].as_str().unwrap_or(""),
                    }));
                } else {
                    // Multi-modal content uses array format
                    input.push(json!({
                        "role": "user",
                        "content": user_content,
                    }));
                }
            }
            Message::Assistant { content, .. } => assistant_items(content, replay, &mut input),
            Message::ToolResult {
                tool_call_id,
                content,
                ..
            } => {
                let output_val = if content.iter().any(|c| matches!(c, Content::Image { .. })) {
                    json!(input_parts(content))
                } else {
                    let text = content
                        .iter()
                        .find_map(|c| match c {
                            Content::Text { text } => Some(text.clone()),
                            _ => None,
                        })
                        .unwrap_or_default();
                    json!(text)
                };
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": tool_call_id,
                    "output": output_val,
                }));
            }
        }
    }

    let mut body = json!({
        "model": config.model,
        "stream": true,
        "input": input,
    });

    if !config.system_prompt.is_empty() {
        body["instructions"] = json!(config.system_prompt);
    }

    if let Some(max) = config.max_tokens {
        body["max_output_tokens"] = json!(max);
    }

    if !config.tools.is_empty() {
        let tools: Vec<Value> = config
            .tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                })
            })
            .collect();
        body["tools"] = json!(tools);
    }

    if let Some(effort) = effort {
        body["reasoning"] = json!({"effort": effort});
    }

    // Encrypted reasoning, for replay on the next turn (see `reasoning_model`).
    if reasoning_model {
        body["include"] = json!([INCLUDE_ENCRYPTED_REASONING]);
    }

    // Prompt caching. The Responses API caches prefixes automatically; the
    // key only routes one conversation's requests to the same cache. It is
    // `CacheConfig::session_key` when set, else derived from the system
    // prompt (see `StreamConfig::cache_session_key`); none when caching
    // hints are off. Not gated on a compat flag as on Chat Completions: the
    // field is part of the Responses API itself.
    if let Some(key) = config.cache_session_key() {
        body["prompt_cache_key"] = json!(key);
    }

    if let Some(temp) = config.temperature {
        body["temperature"] = json!(temp);
    }

    body
}

/// Text and images as Responses `input_text` / `input_image` parts.
fn input_parts(content: &[Content]) -> Vec<Value> {
    content
        .iter()
        .filter(|c| !matches!(c, Content::Text { text } if text.is_empty()))
        .filter_map(|c| match c {
            Content::Text { text } => Some(json!({
                "type": "input_text",
                "text": text,
            })),
            Content::Image { data, mime_type } => Some(json!({
                "type": "input_image",
                "image_url": format!("data:{};base64,{}", mime_type, data),
            })),
            _ => None,
        })
        .collect()
}

/// One assistant message as Responses input items, in content order.
///
/// A reasoning item is replayed in place, before the output items that
/// followed it — but only when one did: a reasoning item is the model's
/// reasoning *for* the next output item, and a trailing one (a turn cut off
/// after reasoning, or whose only output was empty text) has nothing to
/// attach to, so it is dropped rather than risk a 400.
///
/// After a replayed reasoning item, the output items that followed it carry
/// the ids the same API gave them, read from that reasoning item's stored
/// JSON (`call_ids`, `message_id`; see [`ResponsesStreamState`]), so the
/// reasoning item arrives with its paired item. Nothing else gets an id.
///
/// `replay` is the protocol whose encrypted reasoning may be replayed, or
/// `None` when the target model takes no reasoning items: then every
/// reasoning item is skipped and its paired items go without ids, the same
/// as after another API's reasoning.
fn assistant_items(content: &[Content], replay: Option<ApiProtocol>, input: &mut Vec<Value>) {
    let mut pending_reasoning: Vec<Value> = Vec::new();
    // Ids recorded by the reasoning items replayed so far in this message.
    let mut call_ids: HashMap<String, String> = HashMap::new();
    // The id of the first message after the last replayed reasoning item.
    let mut message_id: Option<String> = None;
    for c in content {
        match c {
            Content::Thinking {
                redacted: Some(_), ..
            } => {
                let Some(target) = replay else {
                    debug!("Responses: target model takes no reasoning items; not replayed");
                    continue;
                };
                match c.redacted_for(target).and_then(reasoning_item) {
                    Some(replayed) => {
                        pending_reasoning.push(replayed.item);
                        call_ids.extend(replayed.call_ids);
                        message_id = replayed.message_id;
                    }
                    None => debug!(
                        "Responses ({target}): skipping encrypted reasoning from another \
                         provider (or unreadable)"
                    ),
                }
            }
            Content::Text { text } if text.is_empty() => {}
            Content::Text { text } => {
                input.append(&mut pending_reasoning);
                let mut item = json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": text}],
                });
                if let Some(id) = message_id.take() {
                    item["id"] = json!(id);
                }
                input.push(item);
            }
            Content::ToolCall {
                id,
                name,
                arguments,
                ..
            } => {
                input.append(&mut pending_reasoning);
                let mut item = json!({
                    "type": "function_call",
                    "call_id": id,
                    "name": name,
                    "arguments": arguments.to_string(),
                });
                if let Some(item_id) = call_ids.get(id) {
                    item["id"] = json!(item_id);
                }
                input.push(item);
            }
            // Plain reasoning text (a summary without encrypted content, or
            // another provider's thinking) is not an input the API takes.
            _ => {}
        }
    }
    if !pending_reasoning.is_empty() {
        debug!(
            "Responses: {} trailing reasoning item(s) with no output after them; not replayed",
            pending_reasoning.len()
        );
    }
}

/// A stored reasoning item read back for replay.
struct ReplayedReasoning {
    /// The `reasoning` input item.
    item: Value,
    /// `call_id` → output item id of the function calls that followed it.
    call_ids: HashMap<String, String>,
    /// The id of the first message that followed it.
    message_id: Option<String>,
}

/// The stored reasoning item (see [`ResponsesStreamState`]) as an input item,
/// with the ids of the output items that followed it. `None` if it is not
/// the JSON object this crate wrote.
fn reasoning_item(data: &str) -> Option<ReplayedReasoning> {
    let stored: Value = serde_json::from_str(data).ok()?;
    let id = stored.get("id")?.as_str()?;
    let encrypted = stored.get("encrypted_content")?.as_str()?;
    let summary = match stored.get("summary") {
        Some(Value::Array(parts)) => Value::Array(parts.clone()),
        _ => json!([]),
    };
    let call_ids = stored
        .get(CALL_IDS_KEY)
        .and_then(Value::as_object)
        .map(|ids| {
            ids.iter()
                .filter_map(|(call_id, id)| {
                    let id = id.as_str().filter(|id| !id.is_empty())?;
                    Some((call_id.clone(), id.to_string()))
                })
                .collect()
        })
        .unwrap_or_default();
    let message_id = stored
        .get(MESSAGE_ID_KEY)
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string);
    Some(ReplayedReasoning {
        item: json!({
            "type": "reasoning",
            "id": id,
            "summary": summary,
            "encrypted_content": encrypted,
        }),
        call_ids,
        message_id,
    })
}

/// Send `request`, read the Responses event stream and return the assistant
/// message. `label` names the provider in log lines; `protocol` tags the
/// encrypted reasoning the response carries, so it is replayed only there.
pub(crate) async fn stream_response(
    request: reqwest::RequestBuilder,
    label: &'static str,
    protocol: ApiProtocol,
    config: &StreamConfig,
    provider: &str,
    tx: mpsc::UnboundedSender<StreamEvent>,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<Message, ProviderError> {
    let mut es = super::sse::open_event_source(request)?;
    let mut state = ResponsesStreamState::new(label, protocol);

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
                        warn!("{label} SSE error: {provider_err}");
                        return Err(provider_err);
                    }
                }
            }
        }
    }

    // Read before `finish`, which consumes the refusal state.
    let error_message = state.error_message();
    let (content, usage, stop_reason) = state.finish(&tx);

    let message = Message::Assistant {
        content,
        stop_reason,
        model: config.model.clone(),
        provider: provider.to_string(),
        usage,
        timestamp: now_ms(),
        error_message,
    };

    let _ = tx.send(StreamEvent::Done {
        message: message.clone(),
    });
    Ok(message)
}
