//! Google Generative AI (Gemini) provider.
//!
//! Uses the `streamGenerateContent` endpoint with SSE streaming.
//! API key is passed as a query parameter.
//!
//! # Prompt caching: implicit only, deliberately
//!
//! This provider sends no cache directives and ignores [`CacheStrategy`].
//! Gemini caches implicitly on its own, and the `cachedContentTokenCount` this
//! module reads back into [`Usage::cache_read`] reports the result of that
//! automatic behaviour — it is telemetry, not an acknowledgement of anything
//! the client asked for.
//!
//! Gemini's *explicit* caching is a separate resource with a different
//! lifecycle: you create a `CachedContent` object, receive a handle, reference
//! it by name on later requests, and manage its TTL and its own billing line.
//! That does not fit behind [`CacheStrategy`], which describes where to place
//! markers inside a single request. Wiring it here would mean either
//! misrepresenting a stateful resource as a per-request flag, or silently
//! creating server-side objects with a lifetime the caller cannot see.
//!
//! It is worth doing as its own API — with explicit create/reference/expire
//! surface — and it is not worth pretending the current enum can express it.
//! See yologdev/yoagent#123.

use super::traits::*;
use crate::types::*;
use async_trait::async_trait;
use futures::StreamExt;
use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::{debug, warn};

pub struct GoogleProvider;

#[async_trait]
impl StreamProvider for GoogleProvider {
    fn protocol(&self) -> Option<crate::provider::ApiProtocol> {
        Some(crate::provider::ApiProtocol::GoogleGenerativeAi)
    }

    async fn stream(
        &self,
        config: StreamConfig,
        tx: mpsc::UnboundedSender<StreamEvent>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Message, ProviderError> {
        let model_config = config
            .model_config
            .as_ref()
            .ok_or_else(|| ProviderError::Other("ModelConfig required".into()))?;

        let base_url = &model_config.base_url;
        let url = format!(
            "{}/v1beta/models/{}:streamGenerateContent?alt=sse&key={}",
            base_url, config.model, config.api_key
        );

        let body = build_request_body(&config);
        debug!("Google GenAI request: model={}", config.model);

        let client = reqwest::Client::new();
        let mut request = client.post(&url).header("content-type", "application/json");

        for (k, v) in &model_config.headers {
            request = request.header(k, v);
        }

        // Google streams JSON chunks separated by newlines, not SSE.
        // With alt=sse, it does use SSE format.
        let response = request
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Network(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(ProviderError::classify(
                status.as_u16(),
                &format!("Google API error {}: {}", status, body),
            ));
        }

        let mut content: Vec<Content> = Vec::new();
        let mut usage = Usage::default();
        let mut stop_reason = StopReason::Stop;
        let mut error_message: Option<String> = None;

        let _ = tx.send(StreamEvent::Start);

        // Parse SSE stream
        let mut stream = response.bytes_stream();
        let mut buffer = String::new();

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    return Err(ProviderError::Cancelled);
                }
                chunk = stream.next() => {
                    match chunk {
                        None => break,
                        Some(Err(e)) => {
                            // Match the other providers: a transport failure is an
                            // error (and retryable), not a silently truncated turn.
                            let provider_err = ProviderError::Network(e.to_string());
                            warn!("Google stream error: {}", provider_err);
                            return Err(provider_err);
                        }
                        Some(Ok(bytes)) => {
                            buffer.push_str(&String::from_utf8_lossy(&bytes));

                            // Process complete SSE events (handle both \n\n and \r\n\r\n)
                            while let Some(data) = next_sse_data(&mut buffer) {
                                if data.is_empty() {
                                    continue;
                                }

                                // Google reports mid-stream failures as
                                // {"error": {...}} payloads, which would otherwise
                                // deserialize into an empty chunk and vanish.
                                if is_error_payload(&data) {
                                    let provider_err = classify_sse_error_event(&data);
                                    warn!("Google in-stream error: {}", provider_err);
                                    return Err(provider_err);
                                }

                                let chunk: GoogleChunk = match serde_json::from_str(&data) {
                                    Ok(c) => c,
                                    Err(e) => {
                                        warn!("Failed to parse Google chunk: {}", e);
                                        continue;
                                    }
                                };

                                // Process candidates
                                for candidate in &chunk.candidates.unwrap_or_default() {
                                    if let Some(c) = &candidate.content {
                                        for part in &c.parts {
                                            if let Some(text) = part_text(part) {
                                                if part.thought.unwrap_or(false) {
                                                    // Thought summary part → Thinking content.
                                                    let think_idx = content.iter().position(|c| matches!(c, Content::Thinking { .. }));
                                                    let idx = match think_idx {
                                                        Some(i) => i,
                                                        None => {
                                                            content.push(Content::thinking(String::new()));
                                                            content.len() - 1
                                                        }
                                                    };
                                                    if let Some(Content::Thinking { thinking, .. }) = content.get_mut(idx) {
                                                        thinking.push_str(text);
                                                    }
                                                    let _ = tx.send(StreamEvent::ThinkingDelta {
                                                        content_index: idx,
                                                        delta: text.to_string(),
                                                    });
                                                    continue;
                                                }
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
                                                    delta: text.to_string(),
                                                });
                                            }
                                            if let Some(fc) = &part.function_call {
                                                let id = fc.id.clone().unwrap_or_else(|| format!("google-fc-{}", content.len()));
                                                let args = fc.args.clone().unwrap_or(serde_json::Value::Object(Default::default()));
                                                let metadata = part.thought_signature.as_ref().map(|sig| {
                                                    serde_json::json!({"thought_signature": sig})
                                                });
                                                let idx = content.len();
                                                content.push(Content::ToolCall {
                                                    id: id.clone(),
                                                    name: fc.name.clone(),
                                                    arguments: args,
                                                    provider_metadata: metadata,
                                                });
                                                let _ = tx.send(StreamEvent::ToolCallStart {
                                                    content_index: idx,
                                                    id,
                                                    name: fc.name.clone(),
                                                });
                                                let _ = tx.send(StreamEvent::ToolCallEnd { content_index: idx });
                                                stop_reason = StopReason::ToolUse;
                                            }
                                        }
                                    }
                                    if let Some(reason) = &candidate.finish_reason {
                                        // Don't override ToolUse -- Gemini returns "STOP"
                                        // even when it emits function calls
                                        if stop_reason != StopReason::ToolUse {
                                            stop_reason = match reason.as_str() {
                                                "STOP" => StopReason::Stop,
                                                "MAX_TOKENS" | "RECITATION" => StopReason::Length,
                                                "SAFETY" | "PROHIBITED_CONTENT" | "BLOCKLIST"
                                                | "SPII" => {
                                                    warn!(
                                                        "Gemini blocked the response (finishReason={})",
                                                        reason
                                                    );
                                                    error_message = Some(format!(
                                                        "Response blocked by Gemini safety filters (finishReason: {})",
                                                        reason
                                                    ));
                                                    StopReason::Refusal
                                                }
                                                _ => StopReason::Stop,
                                            };
                                        }
                                    }
                                }

                                // Process usage
                                if let Some(u) = &chunk.usage_metadata {
                                    // promptTokenCount includes cached tokens;
                                    // keep `input` as the uncached remainder so
                                    // downstream sums don't double-count.
                                    usage.input = u
                                        .prompt_token_count
                                        .unwrap_or(0)
                                        .saturating_sub(u.cached_content_token_count.unwrap_or(0));
                                    usage.output = u.candidates_token_count.unwrap_or(0);
                                    usage.total_tokens = u.total_token_count.unwrap_or(0);
                                    usage.cache_read = u.cached_content_token_count.unwrap_or(0);
                                }
                            }
                        }
                    }
                }
            }
        }

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

/// Pop the next complete SSE event from `buffer` and return its `data:`
/// payload (empty string when the event carries no data line). Handles both
/// `\n\n` and `\r\n\r\n` event separators, splitting at whichever occurs
/// first. Returns `None` until a complete event is buffered. Only the first
/// `data:` line of an event is returned.
fn next_sse_data(buffer: &mut String) -> Option<String> {
    let lf = buffer.find("\n\n");
    let crlf = buffer.find("\r\n\r\n");
    let (pos, sep_len) = match (lf, crlf) {
        (Some(l), Some(c)) if c < l => (c, 4),
        (Some(l), _) => (l, 2),
        (None, Some(c)) => (c, 4),
        (None, None) => return None,
    };
    let event_str = buffer[..pos].to_string();
    *buffer = buffer[pos + sep_len..].to_string();
    let data = event_str
        .lines()
        .map(|l| l.trim_end_matches('\r'))
        .find(|l| l.starts_with("data: "))
        .map(|l| l[6..].to_string())
        .unwrap_or_default();
    Some(data)
}

/// Whether an SSE data payload is a Google error envelope
/// (`{"error": {...}}`) rather than a content chunk.
fn is_error_payload(data: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(data)
        .map(|v| v.get("error").is_some())
        .unwrap_or(false)
}

/// Non-empty text of a part. Gemini streams empty text parts while thinking;
/// those must be skipped.
fn part_text(part: &GooglePart) -> Option<&str> {
    part.text.as_deref().filter(|t| !t.is_empty())
}

/// Token budget for Gemini 2.x's `thinkingConfig.thinkingBudget` per level.
/// Only reached through [`gemini_thinking_config`], which Vertex AI shares.
pub(crate) fn gemini_thinking_budget(level: ThinkingLevel) -> u32 {
    match level {
        ThinkingLevel::Off => 0,
        ThinkingLevel::Minimal | ThinkingLevel::Low => 1024,
        ThinkingLevel::Medium => 8192,
        // Clamped: 24,576 is the documented thinkingBudget maximum of Gemini
        // 2.5 Flash and Flash-Lite; 2.5 Pro's is 32,768
        // (cloud.google.com/vertex-ai/generative-ai/docs/thinking). There is
        // no per-model table here, so the crate stays within the smallest
        // documented maximum rather than send an out-of-range budget.
        ThinkingLevel::High | ThinkingLevel::XHigh | ThinkingLevel::Max => 24576,
    }
}

/// Which `thinkingConfig` field a Gemini model takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GeminiThinkingParam {
    /// `thinkingBudget` — Gemini 2.x, and any id the version rule cannot read
    /// (Gemini 3 still accepts a budget for backward compatibility).
    Budget,
    /// `thinkingLevel` — Gemini 3 and later, restricted to the rungs the
    /// model documents as accepted.
    Level(GeminiRungs),
    /// No `thinkingConfig` at all — Gemini 3+ TTS models, which neither
    /// thinking guide lists as supporting thinking.
    Omit,
}

/// The `thinkingLevel` values a Gemini 3+ model accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GeminiRungs {
    /// `MINIMAL`, `LOW`, `MEDIUM`, `HIGH` — 3.6 / 3.5 Flash, 3.x Flash-Lite,
    /// 3 Flash.
    All,
    /// `LOW`, `MEDIUM`, `HIGH` — 3.8 / 3.7 Flash, 3.1 Pro, and every 3.x+
    /// model not documented as accepting `MINIMAL`.
    NoMinimal,
    /// `MINIMAL`, `HIGH` — 3.1 Flash Image and 3.1 Flash-Lite Image.
    MinimalHigh,
    /// `HIGH` only — 3 Pro Image.
    HighOnly,
}

/// Reads the Gemini generation off a model id.
///
/// Google's rule is version-based: `thinkingLevel` is "Recommended for Gemini
/// 3 or later models. Use with earlier models results in an error." (REST
/// reference, `ThinkingConfig`). So the version number decides, not a model
/// list. Accepts bare ids (`gemini-3.8-flash`), `models/…` names, Vertex
/// resource paths (`projects/…/publishers/google/models/gemini-3.1-pro-preview`)
/// and `@version` suffixes.
///
/// `MINIMAL` per the Gemini API thinking guide's level table: rejected by 3.8
/// and 3.7 Flash, not supported by 3.1 Pro; accepted by 3.6 / 3.5 Flash, every
/// 3.x Flash-Lite and 3 Flash. Unlisted 3.x+ models (Pro variants, future
/// Flash releases) are treated as not accepting it, so the worst case is
/// `LOW` where `MINIMAL` would have worked, never a 400.
///
/// Image models, per the Vertex AI thinking table ("Supported thinking_level
/// values"): Gemini 3.1 Flash Image and 3.1 Flash-Lite Image take
/// "MINIMAL , HIGH"; Gemini 3 Pro Image takes "HIGH" only. Every 3.x+
/// `-image` id is read that way (`-pro…-image` → `HIGH` only, any other →
/// `MINIMAL`/`HIGH`).
///
/// TTS models (`gemini-3.8-flash-tts`, `gemini-3.8-flash-lite-tts`,
/// `gemini-3.1-flash-tts-preview`) appear in neither guide's list of models
/// that support thinking, so they get no `thinkingConfig`.
///
/// Aliases: `gemini-flash-latest` (3.5 Flash since 2026-05-19) and
/// `gemini-pro-latest` (3 Pro preview since 2026-01-21) point at Gemini 3
/// per the API changelog; they are hot-swapped, so `MINIMAL` is not assumed.
/// `gemini-flash-lite-latest` has no documented target, so it keeps the
/// budget, which every generation accepts.
fn gemini_thinking_param(model: &str) -> GeminiThinkingParam {
    let name = model.rsplit('/').next().unwrap_or(model);
    let name = name.split('@').next().unwrap_or(name).to_ascii_lowercase();
    let Some(rest) = name.strip_prefix("gemini-") else {
        return GeminiThinkingParam::Budget;
    };
    if rest == "flash-latest" || rest == "pro-latest" {
        return GeminiThinkingParam::Level(GeminiRungs::NoMinimal);
    }

    let major_len = rest.bytes().take_while(u8::is_ascii_digit).count();
    let Ok(major) = rest[..major_len].parse::<u32>() else {
        return GeminiThinkingParam::Budget;
    };
    if major < 3 {
        return GeminiThinkingParam::Budget;
    }
    let after_major = &rest[major_len..];
    let (minor, variant) = match after_major.strip_prefix('.') {
        Some(tail) => {
            let len = tail.bytes().take_while(u8::is_ascii_digit).count();
            (tail[..len].parse::<u32>().ok(), &tail[len..])
        }
        None => (Some(0), after_major),
    };

    if variant.contains("-tts") {
        return GeminiThinkingParam::Omit;
    }
    let rungs = if variant.contains("-image") {
        if variant.starts_with("-pro") {
            GeminiRungs::HighOnly
        } else {
            GeminiRungs::MinimalHigh
        }
    } else if major == 3
        && (variant.starts_with("-flash-lite")
            || (variant.starts_with("-flash") && matches!(minor, Some(0 | 5 | 6))))
    {
        GeminiRungs::All
    } else {
        GeminiRungs::NoMinimal
    };
    GeminiThinkingParam::Level(rungs)
}

/// The `thinkingLevel` string for a non-`Off` [`ThinkingLevel`] on a Gemini
/// 3+ model: the requested rung, or the nearest one the model accepts.
///
/// `Minimal` → `LOW` where `MINIMAL` is not accepted. On the `MINIMAL`/`HIGH`
/// image models, `Low` goes down to `MINIMAL` (both are the "minimize latency
/// and cost" end) and `Medium` goes up to `HIGH` (a request for some thinking
/// gets thinking). `Off` never reaches here in practice: it omits
/// `thinkingConfig`, and is mapped like `Minimal` only to keep the match total.
fn gemini_thinking_level(level: ThinkingLevel, rungs: GeminiRungs) -> &'static str {
    use GeminiRungs::*;
    use ThinkingLevel::*;
    match (rungs, level) {
        (HighOnly, _) => "HIGH",
        (MinimalHigh, Off | Minimal | Low) => "MINIMAL",
        (MinimalHigh, _) => "HIGH",
        (All, Off | Minimal) => "MINIMAL",
        (_, Off | Minimal | Low) => "LOW",
        (_, Medium) => "MEDIUM",
        (_, High | XHigh | Max) => "HIGH",
    }
}

/// The `generationConfig.thinkingConfig` object for a request, or `None` to
/// omit it. Shared by the Gemini API and Vertex AI providers so the two
/// cannot drift.
///
/// Exactly one of `thinkingLevel` / `thinkingBudget` is ever sent: Gemini 3
/// rejects a request carrying both, and pre-3 models reject `thinkingLevel`.
/// The choice comes from [`GoogleCompat::thinking_level`] when set, else
/// from the model id (see `gemini_thinking_param`).
///
/// `ThinkingLevel::Off` omits `thinkingConfig` on every model. On Gemini 3
/// that does **not** disable thinking — the model runs at its own default
/// level (3.1 Pro `HIGH`, 3.5–3.8 Flash `MEDIUM`, Flash-Lite `MINIMAL`);
/// `ThinkingLevel::Minimal` is the way to ask for the least thinking.
///
/// [`GoogleCompat::thinking_level`]: super::GoogleCompat::thinking_level
pub(crate) fn gemini_thinking_config(config: &StreamConfig) -> Option<serde_json::Value> {
    if config.thinking_level == ThinkingLevel::Off {
        return None;
    }
    let forced = config
        .model_config
        .as_ref()
        .and_then(|mc| mc.google.as_ref())
        .and_then(|g| g.thinking_level);
    let param = match (forced, gemini_thinking_param(&config.model)) {
        (Some(false), _) => GeminiThinkingParam::Budget,
        (Some(true), GeminiThinkingParam::Budget) => {
            GeminiThinkingParam::Level(GeminiRungs::NoMinimal)
        }
        (_, inferred) => inferred,
    };

    match param {
        // Gemini 2.x: the payload this crate has always sent — budget scales
        // with the level and includeThoughts streams thought summaries back
        // as thought parts.
        GeminiThinkingParam::Budget => Some(serde_json::json!({
            "thinkingBudget": gemini_thinking_budget(config.thinking_level),
            "includeThoughts": true,
        })),
        GeminiThinkingParam::Level(rungs) => Some(serde_json::json!({
            "thinkingLevel": gemini_thinking_level(config.thinking_level, rungs),
            "includeThoughts": true,
        })),
        GeminiThinkingParam::Omit => None,
    }
}

fn build_request_body(config: &StreamConfig) -> serde_json::Value {
    let mut contents: Vec<serde_json::Value> = Vec::new();

    for msg in &config.messages {
        match msg {
            Message::User { content, .. } => {
                let parts = content_to_google_parts(content);
                contents.push(serde_json::json!({
                    "role": "user",
                    "parts": parts,
                }));
            }
            Message::Assistant { content, .. } => {
                let parts = content_to_google_parts(content);
                contents.push(serde_json::json!({
                    "role": "model",
                    "parts": parts,
                }));
            }
            Message::ToolResult {
                tool_call_id,
                tool_name,
                content,
                ..
            } => {
                let text = content
                    .iter()
                    .find_map(|c| match c {
                        Content::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();

                let mut fr = serde_json::json!({
                    "name": tool_name,
                    "response": {"result": text},
                });
                if !tool_call_id.is_empty() && !tool_call_id.starts_with("google-fc-") {
                    fr["id"] = serde_json::json!(tool_call_id);
                }
                let mut parts = vec![serde_json::json!({"functionResponse": fr})];

                // Append image parts if present
                for c in content {
                    if let Content::Image { data, mime_type } = c {
                        parts.push(serde_json::json!({
                            "inlineData": {"mimeType": mime_type, "data": data},
                        }));
                    }
                }

                contents.push(serde_json::json!({
                    "role": "user",
                    "parts": parts,
                }));
            }
        }
    }

    let mut body = serde_json::json!({
        "contents": contents,
    });

    if !config.system_prompt.is_empty() {
        body["systemInstruction"] = serde_json::json!({
            "parts": [{"text": config.system_prompt}],
        });
    }

    let mut generation_config = serde_json::json!({});
    if let Some(max) = config.max_tokens {
        generation_config["maxOutputTokens"] = serde_json::json!(max);
    }
    if let Some(temp) = config.temperature {
        generation_config["temperature"] = serde_json::json!(temp);
    }
    // Structured outputs: native responseSchema (Gemini's OpenAPI-style
    // schema dialect — pass the caller's schema through as given).
    if let Some(schema) = &config.output_schema {
        generation_config["responseMimeType"] = serde_json::json!("application/json");
        generation_config["responseSchema"] = schema.schema.clone();
    }

    // Thinking: thinkingLevel on Gemini 3+, thinkingBudget on 2.x.
    if let Some(thinking) = gemini_thinking_config(config) {
        generation_config["thinkingConfig"] = thinking;
    }

    if generation_config != serde_json::json!({}) {
        body["generationConfig"] = generation_config;
    }

    if !config.tools.is_empty() {
        let declarations: Vec<serde_json::Value> = config
            .tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                })
            })
            .collect();
        body["tools"] = serde_json::json!([{
            "functionDeclarations": declarations,
        }]);
    }

    body
}

fn content_to_google_parts(content: &[Content]) -> Vec<serde_json::Value> {
    content
        .iter()
        .filter(|c| !matches!(c, Content::Text { text } if text.is_empty()))
        .filter_map(|c| match c {
            Content::Text { text } => Some(serde_json::json!({"text": text})),
            Content::Image { data, mime_type } => Some(serde_json::json!({
                "inlineData": {"mimeType": mime_type, "data": data},
            })),
            Content::ToolCall {
                id,
                name,
                arguments,
                provider_metadata,
            } => {
                let mut fc = serde_json::json!({"name": name, "args": arguments});
                if !id.is_empty() && !id.starts_with("google-fc-") {
                    fc["id"] = serde_json::json!(id);
                }
                let mut part = serde_json::json!({"functionCall": fc});
                if let Some(sig) = provider_metadata
                    .as_ref()
                    .and_then(|m| m.get("thought_signature"))
                    .and_then(|v| v.as_str())
                {
                    part["thoughtSignature"] = serde_json::json!(sig);
                }
                Some(part)
            }
            Content::Thinking { .. } => None,
        })
        .collect()
}

// Google API response types
#[derive(Deserialize)]
struct GoogleChunk {
    #[serde(default)]
    candidates: Option<Vec<GoogleCandidate>>,
    #[serde(default, rename = "usageMetadata")]
    usage_metadata: Option<GoogleUsageMetadata>,
}

#[derive(Deserialize)]
struct GoogleCandidate {
    #[serde(default)]
    content: Option<GoogleContent>,
    #[serde(default, rename = "finishReason")]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct GoogleContent {
    #[serde(default)]
    parts: Vec<GooglePart>,
}

#[derive(Deserialize)]
struct GooglePart {
    #[serde(default)]
    text: Option<String>,
    /// True when this part is a thought summary (thinkingConfig.includeThoughts).
    #[serde(default)]
    thought: Option<bool>,
    #[serde(default, rename = "functionCall")]
    function_call: Option<GoogleFunctionCall>,
    #[serde(default, rename = "thoughtSignature")]
    thought_signature: Option<String>,
}

#[derive(Deserialize)]
struct GoogleFunctionCall {
    name: String,
    #[serde(default)]
    args: Option<serde_json::Value>,
    #[serde(default)]
    id: Option<String>,
}

#[derive(Deserialize)]
struct GoogleUsageMetadata {
    #[serde(default, rename = "promptTokenCount")]
    prompt_token_count: Option<u64>,
    #[serde(default, rename = "candidatesTokenCount")]
    candidates_token_count: Option<u64>,
    #[serde(default, rename = "totalTokenCount")]
    total_token_count: Option<u64>,
    #[serde(default, rename = "cachedContentTokenCount")]
    cached_content_token_count: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{GoogleCompat, ModelConfig};

    #[test]
    fn thinking_blocks_are_dropped_on_replay() {
        // Gemini does not accept echoed thought summaries; signatures ride on
        // functionCall parts instead. Pin the drop so it stays deliberate.
        let mut config = StreamConfig::new("gemini-2.5-pro", "k");
        config.messages = vec![
            Message::user("go"),
            Message::assistant(
                vec![
                    Content::thinking("thought"),
                    Content::Text {
                        text: "answer".into(),
                    },
                ],
                StopReason::Stop,
                "m",
                "google",
                Usage::default(),
            ),
        ];
        let body = build_request_body(&config);
        let parts = body["contents"][1]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 1, "only the text part is replayed");
        assert_eq!(parts[0]["text"], "answer");
    }

    #[test]
    fn thinking_level_sets_thinking_config() {
        let config = StreamConfig {
            model: "gemini-2.5-pro".into(),
            system_prompt: "".into(),
            messages: vec![Message::user("hi")],
            tools: vec![],
            thinking_level: ThinkingLevel::Medium,
            api_key: "test".into(),
            max_tokens: None,
            temperature: None,
            model_config: None,
            cache_config: CacheConfig::default(),
            output_schema: None,
        };
        let body = build_request_body(&config);
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            8192
        );
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["includeThoughts"],
            true
        );
    }

    #[test]
    fn thinking_off_omits_thinking_config() {
        let config = StreamConfig {
            model: "gemini-2.5-pro".into(),
            system_prompt: "".into(),
            messages: vec![Message::user("hi")],
            tools: vec![],
            thinking_level: ThinkingLevel::Off,
            api_key: "test".into(),
            max_tokens: None,
            temperature: None,
            model_config: None,
            cache_config: CacheConfig::default(),
            output_schema: None,
        };
        let body = build_request_body(&config);
        assert!(body["generationConfig"]["thinkingConfig"].is_null());
    }

    #[test]
    fn structured_output_sets_response_schema() {
        let config = StreamConfig {
            model: "gemini-2.5-pro".into(),
            system_prompt: "".into(),
            messages: vec![Message::user("Hello")],
            tools: vec![],
            thinking_level: ThinkingLevel::Off,
            api_key: "test".into(),
            max_tokens: None,
            temperature: None,
            model_config: None,
            cache_config: CacheConfig::default(),
            output_schema: Some(crate::provider::OutputSchema::new(
                "structured_output",
                serde_json::json!({"type": "object", "properties": {"x": {"type": "number"}}}),
            )),
        };
        let body = build_request_body(&config);
        assert_eq!(
            body["generationConfig"]["responseMimeType"],
            "application/json"
        );
        assert_eq!(
            body["generationConfig"]["responseSchema"]["properties"]["x"]["type"],
            "number"
        );
    }

    #[test]
    fn test_build_google_request() {
        let config = StreamConfig {
            model: "gemini-2.0-flash".into(),
            system_prompt: "Be helpful".into(),
            messages: vec![Message::user("Hello")],
            tools: vec![],
            thinking_level: ThinkingLevel::Off,
            api_key: "test".into(),
            max_tokens: Some(1024),
            temperature: Some(0.7),
            model_config: None,
            cache_config: CacheConfig::default(),
            output_schema: None,
        };

        let body = build_request_body(&config);
        assert!(body["contents"].is_array());
        assert_eq!(body["contents"][0]["role"], "user");
        assert!(body["systemInstruction"].is_object());
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 1024);
        let temp = body["generationConfig"]["temperature"].as_f64().unwrap();
        assert!((temp - 0.7).abs() < 0.01);
    }

    #[test]
    fn test_content_to_google_parts_text() {
        let content = vec![Content::Text {
            text: "hello".into(),
        }];
        let parts = content_to_google_parts(&content);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["text"], "hello");
    }

    #[test]
    fn test_content_to_google_parts_filters_empty_text() {
        let content = vec![
            Content::Text { text: "".into() },
            Content::Text {
                text: "hello".into(),
            },
            Content::Text { text: "".into() },
        ];
        let parts = content_to_google_parts(&content);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["text"], "hello");
    }

    #[test]
    fn test_content_to_google_parts_tool_call() {
        let content = vec![Content::ToolCall {
            id: "tc-1".into(),
            name: "bash".into(),
            arguments: serde_json::json!({"command": "ls"}),
            provider_metadata: None,
        }];
        let parts = content_to_google_parts(&content);
        assert_eq!(parts[0]["functionCall"]["name"], "bash");
    }

    #[test]
    fn test_parse_chunk_with_function_call_and_thought_signature() {
        let data = r#"{"candidates": [{"content": {"parts": [{"functionCall": {"name": "bash", "args": {"command": "echo hi"}, "id": "abc123"}, "thoughtSignature": "SIG_DATA"}], "role": "model"}, "finishReason": "STOP", "index": 0}], "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 5, "totalTokenCount": 15}}"#;

        let chunk: GoogleChunk = serde_json::from_str(data).unwrap();
        let candidates = chunk.candidates.unwrap();
        assert_eq!(candidates.len(), 1);

        let parts = &candidates[0].content.as_ref().unwrap().parts;
        assert_eq!(parts.len(), 1);

        let fc = parts[0].function_call.as_ref().unwrap();
        assert_eq!(fc.name, "bash");
        assert_eq!(fc.id.as_deref(), Some("abc123"));
        assert_eq!(fc.args.as_ref().unwrap()["command"], "echo hi");

        assert_eq!(parts[0].thought_signature.as_deref(), Some("SIG_DATA"));
    }

    #[test]
    fn test_parse_chunk_with_empty_text() {
        // Gemini sends empty text parts during thinking -- part_text (used by
        // the streaming loop) must skip them and keep non-empty ones.
        let data = r#"{"candidates": [{"content": {"parts": [{"text": ""}, {"text": "Hello"}], "role": "model"}, "index": 0}]}"#;

        let chunk: GoogleChunk = serde_json::from_str(data).unwrap();
        let candidates = chunk.candidates.unwrap();
        let parts = &candidates[0].content.as_ref().unwrap().parts;
        assert_eq!(part_text(&parts[0]), None, "empty text parts are skipped");
        assert_eq!(part_text(&parts[1]), Some("Hello"));
    }

    #[test]
    fn test_parse_chunk_with_crlf_sse() {
        // Full pipeline: next_sse_data (the production splitter) on a CRLF
        // stream, then chunk parsing.
        let mut buf = "data: {\"candidates\": [{\"content\": {\"parts\": [{\"text\": \"Blue\"}], \"role\": \"model\"}, \"finishReason\": \"STOP\", \"index\": 0}]}\r\n\r\n".to_string();

        let data = next_sse_data(&mut buf).expect("complete CRLF event");
        assert!(buf.is_empty(), "event consumed from buffer");
        assert_eq!(next_sse_data(&mut buf), None);

        let chunk: GoogleChunk = serde_json::from_str(&data).unwrap();
        let candidates = chunk.candidates.unwrap();
        let text = &candidates[0].content.as_ref().unwrap().parts[0].text;
        assert_eq!(text.as_deref(), Some("Blue"));
    }

    #[test]
    fn test_next_sse_data_partial_events_stay_buffered() {
        let mut buf = "data: {\"a\":1}\n\ndata: partial".to_string();
        assert_eq!(next_sse_data(&mut buf).as_deref(), Some("{\"a\":1}"));
        assert_eq!(next_sse_data(&mut buf), None, "incomplete event waits");
        assert_eq!(buf, "data: partial");
        buf.push_str("\n\n");
        assert_eq!(next_sse_data(&mut buf).as_deref(), Some("partial"));
    }

    #[test]
    fn test_next_sse_data_consumes_events_without_data_lines() {
        // SSE comments/keepalives and leading separators must be CONSUMED
        // (returning Some("")), never None — returning None would wedge the
        // buffer and drop every subsequent event.
        let mut buf = ": keepalive\n\n\n\ndata: x\n\n".to_string();
        assert_eq!(next_sse_data(&mut buf).as_deref(), Some(""));
        assert_eq!(next_sse_data(&mut buf).as_deref(), Some(""));
        assert_eq!(next_sse_data(&mut buf).as_deref(), Some("x"));
        assert_eq!(next_sse_data(&mut buf), None);
    }

    #[test]
    fn test_is_error_payload() {
        assert!(is_error_payload(
            r#"{"error": {"code": 429, "status": "RESOURCE_EXHAUSTED"}}"#
        ));
        assert!(!is_error_payload(r#"{"candidates": []}"#));
        assert!(!is_error_payload("not json"));
    }

    #[test]
    fn test_next_sse_data_splits_earliest_separator_first() {
        // A CRLF-separated event earlier in the buffer must split before a
        // later LF separator (the old inline logic preferred the LF match and
        // merged the two events).
        let mut buf = "data: one\r\n\r\ndata: two\n\n".to_string();
        assert_eq!(next_sse_data(&mut buf).as_deref(), Some("one"));
        assert_eq!(next_sse_data(&mut buf).as_deref(), Some("two"));
        assert_eq!(next_sse_data(&mut buf), None);
    }

    #[test]
    fn test_thought_signature_round_trip() {
        let content = vec![Content::ToolCall {
            id: "abc123".into(),
            name: "bash".into(),
            arguments: serde_json::json!({"command": "echo hi"}),
            provider_metadata: Some(serde_json::json!({"thought_signature": "SIG_DATA"})),
        }];

        let parts = content_to_google_parts(&content);
        assert_eq!(parts.len(), 1);

        assert_eq!(parts[0]["functionCall"]["name"], "bash");
        assert_eq!(parts[0]["functionCall"]["id"], "abc123");
        assert_eq!(parts[0]["functionCall"]["args"]["command"], "echo hi");
        assert_eq!(parts[0]["thoughtSignature"], "SIG_DATA");
    }

    #[test]
    fn test_tool_call_without_thought_signature() {
        // Synthetic IDs (google-fc-*) should not be sent to Gemini
        let content = vec![Content::ToolCall {
            id: "google-fc-0".into(),
            name: "bash".into(),
            arguments: serde_json::json!({"command": "ls"}),
            provider_metadata: None,
        }];

        let parts = content_to_google_parts(&content);
        assert!(parts[0]["functionCall"].get("id").is_none());
        assert!(parts[0].get("thoughtSignature").is_none());
    }

    #[test]
    fn test_function_response_includes_id() {
        let config = StreamConfig {
            model: "gemini-2.5-flash".into(),
            system_prompt: "".into(),
            messages: vec![
                Message::Assistant {
                    content: vec![Content::ToolCall {
                        id: "abc123".into(),
                        name: "bash".into(),
                        arguments: serde_json::json!({"command": "echo hi"}),
                        provider_metadata: None,
                    }],
                    stop_reason: StopReason::ToolUse,
                    model: "test".into(),
                    provider: "test".into(),
                    usage: Usage::default(),
                    timestamp: 0,
                    error_message: None,
                },
                Message::ToolResult {
                    tool_call_id: "abc123".into(),
                    tool_name: "bash".into(),
                    content: vec![Content::Text { text: "hi".into() }],
                    is_error: false,
                    timestamp: 0,
                },
            ],
            tools: vec![],
            thinking_level: ThinkingLevel::Off,
            api_key: "test".into(),
            max_tokens: None,
            temperature: None,
            model_config: None,
            cache_config: CacheConfig::default(),
            output_schema: None,
        };

        let body = build_request_body(&config);
        let msgs = body["contents"].as_array().unwrap();
        let tool_result = &msgs[1]["parts"][0]["functionResponse"];
        assert_eq!(tool_result["name"], "bash");
        assert_eq!(tool_result["id"], "abc123");
        assert_eq!(tool_result["response"]["result"], "hi");
    }

    #[test]
    fn test_function_response_synthetic_id_omitted() {
        let config = StreamConfig {
            model: "gemini-2.5-flash".into(),
            system_prompt: "".into(),
            messages: vec![
                Message::Assistant {
                    content: vec![Content::ToolCall {
                        id: "google-fc-0".into(),
                        name: "bash".into(),
                        arguments: serde_json::json!({"command": "ls"}),
                        provider_metadata: None,
                    }],
                    stop_reason: StopReason::ToolUse,
                    model: "test".into(),
                    provider: "test".into(),
                    usage: Usage::default(),
                    timestamp: 0,
                    error_message: None,
                },
                Message::ToolResult {
                    tool_call_id: "google-fc-0".into(),
                    tool_name: "bash".into(),
                    content: vec![Content::Text {
                        text: "output".into(),
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
            model_config: None,
            cache_config: CacheConfig::default(),
            output_schema: None,
        };

        let body = build_request_body(&config);
        let msgs = body["contents"].as_array().unwrap();
        let tool_result = &msgs[1]["parts"][0]["functionResponse"];
        assert!(
            tool_result.get("id").is_none(),
            "Synthetic ID should not be included"
        );
    }

    // --- thinkingLevel (Gemini 3+) vs thinkingBudget (2.x) -----------------

    const ALL_LEVELS: [ThinkingLevel; 7] = [
        ThinkingLevel::Off,
        ThinkingLevel::Minimal,
        ThinkingLevel::Low,
        ThinkingLevel::Medium,
        ThinkingLevel::High,
        ThinkingLevel::XHigh,
        ThinkingLevel::Max,
    ];

    fn thinking_config(model: &str, level: ThinkingLevel) -> serde_json::Value {
        let mut config = StreamConfig::new(model, "k");
        config.messages = vec![Message::user("hi")];
        config.thinking_level = level;
        build_request_body(&config)["generationConfig"]["thinkingConfig"].clone()
    }

    fn thinking_config_with(
        model: &str,
        level: ThinkingLevel,
        compat: GoogleCompat,
    ) -> serde_json::Value {
        let mut config = StreamConfig::new(model, "k");
        config.messages = vec![Message::user("hi")];
        config.thinking_level = level;
        let mut mc = ModelConfig::google(model, "test");
        mc.google = Some(compat);
        config.model_config = Some(mc);
        build_request_body(&config)["generationConfig"]["thinkingConfig"].clone()
    }

    /// The payload every release before this one sent, for every level.
    fn legacy_budget_payload(level: ThinkingLevel) -> serde_json::Value {
        let budget = match level {
            ThinkingLevel::Off => return serde_json::Value::Null,
            ThinkingLevel::Minimal | ThinkingLevel::Low => 1024,
            ThinkingLevel::Medium => 8192,
            _ => 24576,
        };
        serde_json::json!({"thinkingBudget": budget, "includeThoughts": true})
    }

    #[test]
    fn gemini_2x_payload_is_unchanged_budget() {
        // Near-misses: every id here must keep the pre-3 payload byte for byte.
        for model in [
            "gemini-2.5-pro",
            "gemini-2.5-flash",
            "gemini-2.5-flash-lite",
            "gemini-2.5-flash-preview-09-2025",
            "gemini-2.0-flash",
            "models/gemini-2.5-flash",
            "projects/p/locations/us-central1/publishers/google/models/gemini-2.5-pro",
            "gemini-2.5-pro@001",
            "gemini-flash-lite-latest",
            "gemini-exp-1206",
            "gemma-3-27b-it",
        ] {
            for level in ALL_LEVELS {
                let got = thinking_config(model, level);
                assert_eq!(got, legacy_budget_payload(level), "{model} {level:?}");
                assert_eq!(
                    serde_json::to_string(&got).unwrap(),
                    serde_json::to_string(&legacy_budget_payload(level)).unwrap(),
                    "{model} {level:?}: byte-identical"
                );
            }
        }
    }

    #[test]
    fn gemini_3_8_flash_sends_level_and_clamps_minimal_to_low() {
        // 3.8 / 3.7 Flash reject MINIMAL ("Not supported (error)").
        for model in [
            "gemini-3.8-flash",
            "gemini-3.7-flash",
            "gemini-3.8-flash-cyber",
        ] {
            let expect = [
                None,        // Off: omitted, the model runs at its default
                Some("LOW"), // Minimal clamped
                Some("LOW"),
                Some("MEDIUM"),
                Some("HIGH"),
                Some("HIGH"), // XHigh clamped
                Some("HIGH"), // Max clamped
            ];
            for (level, want) in ALL_LEVELS.into_iter().zip(expect) {
                let got = thinking_config(model, level);
                match want {
                    None => assert!(got.is_null(), "{model} {level:?}: {got}"),
                    Some(want) => assert_eq!(
                        got,
                        serde_json::json!({"thinkingLevel": want, "includeThoughts": true}),
                        "{model} {level:?}: level only, never both"
                    ),
                }
            }
        }
    }

    #[test]
    fn gemini_3_flash_lite_and_3_5_flash_pass_minimal_through() {
        for model in [
            "gemini-3.5-flash-lite",
            "gemini-3.1-flash-lite",
            "gemini-3.5-flash",
            "gemini-3.6-flash",
            "gemini-3-flash-preview",
        ] {
            assert_eq!(
                thinking_config(model, ThinkingLevel::Minimal),
                serde_json::json!({"thinkingLevel": "MINIMAL", "includeThoughts": true}),
                "{model}"
            );
            assert_eq!(
                thinking_config(model, ThinkingLevel::Low)["thinkingLevel"],
                "LOW"
            );
        }
    }

    #[test]
    fn gemini_3_pro_has_no_minimal() {
        for model in [
            "gemini-3.1-pro-preview",
            "gemini-3-pro-preview",
            "gemini-pro-latest",
            "gemini-flash-latest", // hot-swapped alias: MINIMAL not assumed
        ] {
            assert_eq!(
                thinking_config(model, ThinkingLevel::Minimal)["thinkingLevel"],
                "LOW",
                "{model}"
            );
            assert_eq!(
                thinking_config(model, ThinkingLevel::Max)["thinkingLevel"],
                "HIGH",
                "{model}"
            );
        }
    }

    #[test]
    fn off_omits_thinking_config_on_every_gemini_generation() {
        // User decision: Off sends nothing. On Gemini 3 the model then runs
        // at its own default level (it cannot be switched off).
        for model in [
            "gemini-3.8-flash",
            "gemini-3.5-flash-lite",
            "gemini-3.1-pro-preview",
            "gemini-3-flash-preview",
            "gemini-flash-latest",
            "projects/p/locations/global/publishers/google/models/gemini-3.8-flash",
            "gemini-3.1-flash-image",
            "gemini-3-pro-image",
            "gemini-3.8-flash-tts",
            "gemini-2.5-flash",
        ] {
            let mut config = StreamConfig::new(model, "k");
            config.messages = vec![Message::user("hi")];
            config.thinking_level = ThinkingLevel::Off;
            let body = build_request_body(&config);
            assert!(
                body["generationConfig"].get("thinkingConfig").is_none(),
                "{model}: {body}"
            );
        }
        // Forcing thinkingLevel does not bring Off back either.
        assert!(thinking_config_with(
            "my-gemini-alias",
            ThinkingLevel::Off,
            GoogleCompat::force_thinking_level()
        )
        .is_null());
    }

    #[test]
    fn positive_control_off_revert_only_touches_off() {
        // The same 3.x ids still get a thinkingConfig one rung up, so the
        // omission above is Off-specific, not a broken 3.x path.
        for model in [
            "gemini-3.8-flash",
            "gemini-3.5-flash-lite",
            "gemini-3.1-pro-preview",
        ] {
            assert!(
                thinking_config(model, ThinkingLevel::Minimal)
                    .get("thinkingLevel")
                    .is_some(),
                "{model}"
            );
        }
    }

    #[test]
    fn gemini_image_models_get_only_their_accepted_levels() {
        // Vertex thinking table: 3.1 Flash Image and 3.1 Flash-Lite Image take
        // "MINIMAL , HIGH"; 3 Pro Image takes "HIGH" only.
        for model in [
            "gemini-3.1-flash-image",
            "gemini-3.1-flash-image-preview",
            "gemini-3.1-flash-lite-image",
            "projects/p/locations/global/publishers/google/models/gemini-3.1-flash-image",
        ] {
            let expect = [
                None,
                Some("MINIMAL"),
                Some("MINIMAL"), // Low: down to the low-cost end
                Some("HIGH"),    // Medium: up, a request for thinking gets it
                Some("HIGH"),
                Some("HIGH"),
                Some("HIGH"),
            ];
            for (level, want) in ALL_LEVELS.into_iter().zip(expect) {
                let got = thinking_config(model, level);
                match want {
                    None => assert!(got.is_null(), "{model} {level:?}"),
                    Some(want) => assert_eq!(
                        got,
                        serde_json::json!({"thinkingLevel": want, "includeThoughts": true}),
                        "{model} {level:?}"
                    ),
                }
            }
        }
        for model in ["gemini-3-pro-image", "gemini-3-pro-image-preview"] {
            assert!(thinking_config(model, ThinkingLevel::Off).is_null());
            for level in ALL_LEVELS.into_iter().skip(1) {
                assert_eq!(
                    thinking_config(model, level),
                    serde_json::json!({"thinkingLevel": "HIGH", "includeThoughts": true}),
                    "{model} {level:?}"
                );
            }
        }
        // 2.5 Flash Image stays on the legacy budget path, byte for byte.
        for level in ALL_LEVELS {
            assert_eq!(
                thinking_config("gemini-2.5-flash-image", level),
                legacy_budget_payload(level)
            );
        }
    }

    #[test]
    fn gemini_3_tts_models_get_no_thinking_config() {
        for model in [
            "gemini-3.8-flash-tts",
            "gemini-3.8-flash-lite-tts",
            "gemini-3.1-flash-tts-preview",
        ] {
            for level in ALL_LEVELS {
                assert!(thinking_config(model, level).is_null(), "{model} {level:?}");
            }
        }
        // Positive control: the non-TTS sibling does get one.
        assert_eq!(
            thinking_config("gemini-3.8-flash", ThinkingLevel::Low)["thinkingLevel"],
            "LOW"
        );
        // 2.x TTS ids keep the pre-3 payload unchanged.
        assert_eq!(
            thinking_config("gemini-2.5-flash-preview-tts", ThinkingLevel::Low),
            legacy_budget_payload(ThinkingLevel::Low)
        );
    }

    #[test]
    fn gemini_3_ids_are_read_through_prefixes_and_paths() {
        for model in [
            "gemini-3",
            "models/gemini-3.8-flash",
            "GEMINI-3.8-FLASH",
            "projects/p/locations/global/publishers/google/models/gemini-3.8-flash",
            "gemini-3.8-flash@001",
            "gemini-4-flash", // later generations inherit the version rule
        ] {
            let got = thinking_config(model, ThinkingLevel::Medium);
            assert_eq!(
                got,
                serde_json::json!({"thinkingLevel": "MEDIUM", "includeThoughts": true}),
                "{model}"
            );
        }
    }

    #[test]
    fn positive_control_2_5_and_3_x_payloads_differ() {
        // Guards against a helper that always returns the same shape.
        let old = thinking_config("gemini-2.5-flash", ThinkingLevel::High);
        let new = thinking_config("gemini-3.5-flash", ThinkingLevel::High);
        assert_ne!(old, new);
        assert_eq!(old["thinkingBudget"], 24576);
        assert_eq!(new["thinkingLevel"], "HIGH");
    }

    #[test]
    fn google_compat_overrides_the_model_id_rule() {
        // Force level on an id the rule cannot read (a proxy alias).
        let got = thinking_config_with(
            "my-gemini-alias",
            ThinkingLevel::Minimal,
            GoogleCompat::force_thinking_level(),
        );
        assert_eq!(
            got,
            serde_json::json!({"thinkingLevel": "LOW", "includeThoughts": true})
        );
        // Forcing level on a recognised 3.x id keeps its MINIMAL knowledge.
        let got = thinking_config_with(
            "gemini-3.5-flash-lite",
            ThinkingLevel::Minimal,
            GoogleCompat::force_thinking_level(),
        );
        assert_eq!(got["thinkingLevel"], "MINIMAL");
        // Force budget on a 3.x id: the legacy payload, exactly.
        for level in ALL_LEVELS {
            let got = thinking_config_with(
                "gemini-3.8-flash",
                level,
                GoogleCompat::force_thinking_budget(),
            );
            assert_eq!(got, legacy_budget_payload(level), "{level:?}");
        }
        // Default compat == no compat.
        assert_eq!(
            thinking_config_with(
                "gemini-3.8-flash",
                ThinkingLevel::High,
                GoogleCompat::default()
            ),
            thinking_config("gemini-3.8-flash", ThinkingLevel::High)
        );
    }

    #[test]
    fn google_compat_serde_default_and_round_trip() {
        let compat: GoogleCompat = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(compat, GoogleCompat::default());
        let mut mc = ModelConfig::google("gemini-2.5-flash", "G");
        let before = serde_json::to_value(&mc).unwrap();
        assert!(
            before.get("google").is_none(),
            "None is omitted on serialize"
        );
        mc.google = Some(GoogleCompat::force_thinking_level());
        let back: ModelConfig = serde_json::from_value(serde_json::to_value(&mc).unwrap()).unwrap();
        assert_eq!(back.google, Some(GoogleCompat::force_thinking_level()));
    }
}
