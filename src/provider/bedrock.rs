//! Amazon Bedrock ConverseStream provider.
//!
//! **Authentication.** Requests are *not* SigV4-signed: this crate has no
//! SigV4 implementation. Headers in `ModelConfig.headers` are sent as given
//! (pre-computed auth headers, or an IAM proxy that signs for you); without an
//! `authorization` header the provider sends `Authorization: Bearer {api_key}`.
//! The `api_key` must currently be formatted as
//! `{access_key_id}:{secret_access_key}` (with optional `:{session_token}`).
//! The `base_url` in ModelConfig should be the Bedrock endpoint, e.g.
//! `https://bedrock-runtime.us-east-1.amazonaws.com`.
//!
//! **Response stream.** ConverseStream answers with binary
//! `application/vnd.amazon.eventstream` frames (decoded by the crate-private
//! `provider::eventstream` module), not JSON lines. The event type is
//! the `:event-type` header of each frame, and the JSON payload is the event
//! structure itself — `{"contentBlockIndex":0,"delta":{"text":"Hi"}}` for a
//! `contentBlockDelta`, with no wrapper key. The shapes below follow the AWS
//! Bedrock Runtime API reference (`ConverseStreamOutput` and the event types
//! it lists). Tested against mock frames built to that format, not against a
//! live endpoint.

use super::eventstream::{Frame, FrameDecoder, FrameError};
use super::tool_args::finalize_tool_arguments;
use super::traits::*;
use crate::provider::UNPARSED_ARGUMENTS_KEY;
use crate::types::*;
use async_trait::async_trait;
use futures::{Stream, StreamExt};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use tokio::sync::mpsc;
use tracing::{debug, warn};

pub struct BedrockProvider;

#[async_trait]
impl StreamProvider for BedrockProvider {
    fn protocol(&self) -> Option<crate::provider::ApiProtocol> {
        Some(crate::provider::ApiProtocol::BedrockConverseStream)
    }

    async fn stream(
        &self,
        config: StreamConfig,
        tx: mpsc::UnboundedSender<StreamEvent>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Message, ProviderError> {
        if config.output_schema.is_some() {
            tracing::warn!(
                "structured outputs are not yet wired for the Amazon Bedrock provider; output_schema will be ignored"
            );
        }
        let model_config = config
            .model_config
            .as_ref()
            .ok_or_else(|| ProviderError::Other("ModelConfig required".into()))?;

        let base_url = &model_config.base_url;
        let url = format!("{}/model/{}/converse-stream", base_url, config.model);

        let body = build_bedrock_body(&config);
        debug!("Bedrock request: model={} url={}", config.model, url);

        // Parse AWS credentials from api_key
        let parts: Vec<&str> = config.api_key.splitn(3, ':').collect();
        if parts.len() < 2 {
            return Err(ProviderError::Auth(
                "Bedrock api_key must be 'access_key:secret_key[:session_token]'".into(),
            ));
        }

        let client = reqwest::Client::new();
        let mut request = client
            .post(&url)
            .header("content-type", "application/json")
            .header("accept", "application/vnd.amazon.eventstream");

        // Add AWS auth headers. In a real implementation, this would use SigV4.
        // For now, we support a simplified auth model where the caller provides
        // pre-computed auth headers via model_config.headers, or uses an IAM proxy.
        for (k, v) in &model_config.headers {
            request = request.header(k, v);
        }

        // If no auth headers provided, try basic Bearer auth as fallback
        // (works with some Bedrock proxy configurations)
        if !model_config.headers.contains_key("authorization") {
            request = request.header("authorization", format!("Bearer {}", config.api_key));
        }

        let response = tokio::select! {
            _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
            r = request.json(&body).send() => r.map_err(|e| ProviderError::Network(e.to_string()))?,
        };

        if !response.status().is_success() {
            return Err(http_error(response).await);
        }

        // A 200 that declares some other content type (JSON from a proxy, an
        // HTML error page, an endpoint that is not ConverseStream) is not an
        // event stream. Report its body rather than a checksum error from
        // trying to frame it. With no content type at all, try to decode.
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .map(|v| String::from_utf8_lossy(v.as_bytes()).to_ascii_lowercase());
        if let Some(content_type) = content_type {
            if !content_type.contains("application/vnd.amazon.eventstream") {
                let body = read_body(response).await;
                return Err(ProviderError::Api(format!(
                    "Bedrock returned `{content_type}` instead of an event stream: {}",
                    truncate_for_error(&body)
                )));
            }
        }

        let _ = tx.send(StreamEvent::Start);

        let outcome = read_converse_stream(response.bytes_stream(), &tx, &cancel).await?;

        let message = Message::Assistant {
            content: outcome.content,
            stop_reason: outcome.stop_reason,
            model: config.model.clone(),
            provider: model_config.provider.clone(),
            usage: outcome.usage,
            timestamp: now_ms(),
            error_message: outcome.error_message,
        };

        let _ = tx.send(StreamEvent::Done {
            message: message.clone(),
        });
        Ok(message)
    }
}

/// Classify a non-2xx ConverseStream response. The body is AWS's JSON error
/// (`{"message": "..."}`) and the error name is in `x-amzn-ErrorType`
/// (`ThrottlingException:http://...`). A 429 is a retryable rate limit; a
/// validation error whose message is a context-overflow phrase ("Input is too
/// long for requested model") is an overflow.
async fn http_error(response: reqwest::Response) -> ProviderError {
    let status = response.status();
    let retry_after_ms = parse_retry_after(response.headers());
    let kind = response
        .headers()
        .get("x-amzn-errortype")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(':').next())
        .map(str::to_string);
    let body = read_body(response).await;
    let message = error_message_of(body.as_bytes());
    let text = match kind {
        Some(kind) => format!("Bedrock error {status} ({kind}): {message}"),
        None => format!("Bedrock error {status}: {message}"),
    };
    ProviderError::classify_with_retry_after(status.as_u16(), &text, retry_after_ms)
}

/// The response body as text; a failure to read it is reported in its place
/// rather than turning into an empty message.
async fn read_body(response: reqwest::Response) -> String {
    match response.text().await {
        Ok(body) => body,
        Err(e) => format!("<failed to read the response body: {e}>"),
    }
}

/// The `message` of an AWS JSON error body, or the body itself when it has
/// none (or is not JSON).
fn error_message_of(payload: &[u8]) -> String {
    let text = String::from_utf8_lossy(payload);
    serde_json::from_slice::<serde_json::Value>(payload)
        .ok()
        .and_then(|v| {
            ["message", "Message"]
                .iter()
                .find_map(|k| v.get(*k).and_then(|m| m.as_str()).map(str::to_string))
        })
        .unwrap_or_else(|| text.into_owned())
}

/// Map an in-stream exception (`:message-type: exception`) onto a
/// [`ProviderError`] through the HTTP status AWS documents for it in
/// `ConverseStreamOutput`, so it classifies exactly like the same error
/// returned as an HTTP response: `throttlingException` (429) is a retryable
/// rate limit, `validationException` (400) carrying an overflow phrase is a
/// context overflow, the rest are API errors.
fn exception_error(kind: &str, message: &str) -> ProviderError {
    let status = match kind.to_ascii_lowercase().as_str() {
        "throttlingexception" => 429,
        "validationexception" => 400,
        "accessdeniedexception" => 403,
        "modelstreamerrorexception" | "modelerrorexception" => 424,
        // The 5xx exceptions stay `Api` (not retried), as an HTTP 5xx does
        // everywhere else in this crate: only `RateLimited` and `Network`
        // are retryable.
        "internalserverexception" => 500,
        "serviceunavailableexception" => 503,
        _ => 0,
    };
    ProviderError::classify(status, &format!("Bedrock {kind}: {message}"))
}

fn truncate_for_error(s: &str) -> String {
    const MAX: usize = 300;
    if s.chars().count() <= MAX {
        return s.to_string();
    }
    let head: String = s.chars().take(MAX).collect();
    format!("{head}\u{2026} ({} bytes total)", s.len())
}

/// What one ConverseStream response assembled into.
struct StreamOutcome {
    content: Vec<Content>,
    usage: Usage,
    stop_reason: StopReason,
    error_message: Option<String>,
}

/// Drive the frame decoder and the event state machine over a byte stream.
///
/// Generic over the chunk and error types so tests can feed arbitrary chunk
/// boundaries; the provider passes `reqwest`'s `bytes_stream()`.
async fn read_converse_stream<S, B, E>(
    stream: S,
    tx: &mpsc::UnboundedSender<StreamEvent>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<StreamOutcome, ProviderError>
where
    S: Stream<Item = Result<B, E>>,
    B: AsRef<[u8]>,
    E: std::fmt::Display,
{
    let mut stream = std::pin::pin!(stream);
    let mut decoder = FrameDecoder::new();
    let mut state = ConverseStreamState::default();

    loop {
        let chunk = tokio::select! {
            _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
            chunk = stream.next() => chunk,
        };
        match chunk {
            None => break,
            Some(Err(e)) => {
                // Once `messageStop` and `metadata` have both arrived the
                // response is whole — `metadata` is the last event AWS sends.
                // Failing now would make the turn retryable and re-bill a
                // finished response (the rule `classify_eventsource_error`
                // documents for SSE providers). Before that, a dropped
                // connection is a transport failure and the partial content
                // is discarded.
                if state.is_complete() {
                    warn!(
                        "Bedrock stream transport error after a complete response; keeping it: {e}"
                    );
                    return state.finish(tx);
                }
                warn!("Bedrock stream transport error: {e}");
                return Err(ProviderError::Network(format!(
                    "Bedrock stream interrupted: {e}"
                )));
            }
            Some(Ok(bytes)) => {
                decoder.push(bytes.as_ref());
                while let Some(frame) = decoder.next_frame().map_err(frame_error)? {
                    state.handle_frame(&frame, tx)?;
                }
            }
        }
    }
    if let Err(e) = decoder.finish() {
        // Same rule for a body that ends inside a trailing frame.
        if state.is_complete() && matches!(e, FrameError::Truncated { .. }) {
            warn!("Bedrock stream: {e} after a complete response; keeping it");
        } else {
            return Err(frame_error(e));
        }
    }
    state.finish(tx)
}

/// A body that ends inside a frame is truncation (retryable, like any other
/// dropped stream); a checksum or structure error means the body is not an
/// intact event stream, which retrying the same endpoint will not fix.
fn frame_error(e: FrameError) -> ProviderError {
    warn!("Bedrock event stream: {e}");
    match e {
        FrameError::Truncated { .. } => ProviderError::Network(e.to_string()),
        _ => ProviderError::Other(format!("Bedrock {e}")),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Text,
    Thinking,
    Tool,
    /// A block this provider does not surface (image, server-side tool use
    /// and its result); its deltas are skipped.
    Ignored,
}

#[derive(Debug)]
struct Block {
    kind: BlockKind,
    /// Position in `content` (unused for `Ignored`).
    content_index: usize,
    /// Tool blocks: the name, and the `toolUse.input` text accumulated so far.
    name: String,
    input: String,
    /// Thinking blocks: decoded `redactedContent` bytes accumulated so far.
    redacted: Vec<u8>,
    closed: bool,
}

impl Block {
    fn new(kind: BlockKind, content_index: usize, name: String) -> Self {
        Block {
            kind,
            content_index,
            name,
            input: String::new(),
            redacted: Vec::new(),
            closed: false,
        }
    }
}

/// Accumulates one ConverseStream response, keyed by `contentBlockIndex`.
///
/// Tool calls are pushed into `content` with the unparsed-arguments marker
/// (`{"__partial_json": ""}`) as a placeholder, which the agent loop never
/// runs; only `contentBlockStop` replaces it with the parsed arguments. A
/// path that skips finalization therefore fails closed instead of running
/// the tool on `{}`.
#[derive(Default)]
struct ConverseStreamState {
    content: Vec<Content>,
    blocks: BTreeMap<u64, Block>,
    stop: Option<(StopReason, Option<String>)>,
    usage: Option<Usage>,
    events: usize,
    /// Keys of warnings already logged, so dropped content warns once per
    /// block (or per unknown event type), not once per delta.
    warned: BTreeSet<String>,
}

fn parse_payload<'a, T: Deserialize<'a>>(
    event: &str,
    payload: &'a [u8],
) -> Result<T, ProviderError> {
    serde_json::from_slice(payload).map_err(|e| {
        ProviderError::Other(format!(
            "Bedrock `{event}` payload does not match the documented shape ({e}): {}",
            truncate_for_error(&String::from_utf8_lossy(payload))
        ))
    })
}

fn protocol_error(msg: String) -> ProviderError {
    warn!("Bedrock stream: {msg}");
    ProviderError::Other(format!("Bedrock stream: {msg}"))
}

fn unfinalized_arguments(raw: &str) -> serde_json::Value {
    serde_json::json!({ UNPARSED_ARGUMENTS_KEY: raw })
}

fn member_names(other: &BTreeMap<String, serde_json::Value>) -> String {
    other.keys().cloned().collect::<Vec<_>>().join(", ")
}

impl ConverseStreamState {
    /// `messageStop` and `metadata` both arrived: nothing else is expected.
    fn is_complete(&self) -> bool {
        self.stop.is_some() && self.usage.is_some()
    }

    /// Log `message` once per `key`.
    fn warn_once(&mut self, key: String, message: impl FnOnce() -> String) {
        if self.warned.insert(key) {
            warn!("Bedrock: {}", message());
        }
    }

    fn handle_frame(
        &mut self,
        frame: &Frame,
        tx: &mpsc::UnboundedSender<StreamEvent>,
    ) -> Result<(), ProviderError> {
        let message_type = frame
            .header_str(":message-type")
            .ok_or_else(|| protocol_error("frame without a `:message-type` header".into()))?;
        match message_type {
            "event" => {
                let event = frame.header_str(":event-type").ok_or_else(|| {
                    protocol_error("event frame without an `:event-type` header".into())
                })?;
                self.events += 1;
                self.handle_event(event, &frame.payload, tx)
            }
            // A modeled error (`throttlingException`, `validationException`,
            // ...): the name is in `:exception-type`, the payload is
            // `{"message": "..."}`.
            "exception" => {
                let kind = frame.header_str(":exception-type").unwrap_or("exception");
                let err = exception_error(kind, &error_message_of(&frame.payload));
                warn!("Bedrock stream exception: {err}");
                Err(err)
            }
            // An unmodeled error: name and text travel in headers.
            "error" => {
                let code = frame.header_str(":error-code").unwrap_or("error");
                let message = frame.header_str(":error-message").unwrap_or("");
                let err = exception_error(code, message);
                warn!("Bedrock stream error: {err}");
                Err(err)
            }
            other => Err(protocol_error(format!("unknown `:message-type` `{other}`"))),
        }
    }

    fn handle_event(
        &mut self,
        event: &str,
        payload: &[u8],
        tx: &mpsc::UnboundedSender<StreamEvent>,
    ) -> Result<(), ProviderError> {
        match event {
            "messageStart" => {
                let e: MessageStartEvent = parse_payload(event, payload)?;
                debug!("Bedrock messageStart role={}", e.role);
            }
            "contentBlockStart" => {
                let e: ContentBlockStartEvent = parse_payload(event, payload)?;
                self.block_start(e, tx)?;
            }
            "contentBlockDelta" => {
                let e: ContentBlockDeltaEvent = parse_payload(event, payload)?;
                self.block_delta(e, tx)?;
            }
            "contentBlockStop" => {
                let e: ContentBlockStopEvent = parse_payload(event, payload)?;
                self.block_stop(e.content_block_index, tx);
            }
            "messageStop" => {
                let e: MessageStopEvent = parse_payload(event, payload)?;
                self.stop = Some(map_stop_reason(&e.stop_reason));
            }
            "metadata" => {
                let e: MetadataEvent = parse_payload(event, payload)?;
                match e.usage {
                    Some(u) => self.usage = Some(u.into_usage()),
                    None => warn!("Bedrock metadata event without usage"),
                }
            }
            // An event type added after this was written may carry content
            // that is now being dropped — say so, once per type.
            other => {
                let other = other.to_string();
                self.warn_once(format!("event:{other}"), || {
                    format!("ignoring unknown ConverseStream event `{other}`")
                });
            }
        }
        Ok(())
    }

    fn block_start(
        &mut self,
        e: ContentBlockStartEvent,
        tx: &mpsc::UnboundedSender<StreamEvent>,
    ) -> Result<(), ProviderError> {
        let index = e.content_block_index;
        if self.blocks.contains_key(&index) {
            return Err(protocol_error(format!(
                "contentBlockStart for block {index}, which already started"
            )));
        }
        let start = e.start;
        let block = match start.tool_use {
            // A server-side tool runs on AWS's side and its result streams
            // back; it is not a call for the agent loop to execute.
            Some(t) if t.kind.as_deref() == Some("server_tool_use") => {
                warn!(
                    "Bedrock: server-side tool `{}` (block {index}) is not surfaced",
                    t.name
                );
                Block::new(BlockKind::Ignored, 0, t.name)
            }
            Some(t) => {
                let content_index = self.content.len();
                self.content.push(Content::ToolCall {
                    provider_metadata: None,
                    id: t.tool_use_id.clone(),
                    name: t.name.clone(),
                    arguments: unfinalized_arguments(""),
                });
                let _ = tx.send(StreamEvent::ToolCallStart {
                    content_index,
                    id: t.tool_use_id,
                    name: t.name.clone(),
                });
                Block::new(BlockKind::Tool, content_index, t.name)
            }
            None if start.image.is_some() || start.tool_result.is_some() => {
                let what = if start.image.is_some() {
                    "image"
                } else {
                    "toolResult"
                };
                warn!("Bedrock: {what} block {index} is not surfaced");
                Block::new(BlockKind::Ignored, 0, String::new())
            }
            // A start member this provider does not know: keep no block, so
            // the first delta types it rather than its content being dropped.
            None => {
                warn!(
                    "Bedrock: contentBlockStart for block {index} has no known member ({}); \
                     the block will be typed by its first delta",
                    member_names(&start.other)
                );
                return Ok(());
            }
        };
        self.blocks.insert(index, block);
        Ok(())
    }

    /// The block for `index`, created as `kind` if this is its first event
    /// (text and reasoning blocks have no `contentBlockStart`). `None` for a
    /// block this provider does not surface.
    fn block_for(
        &mut self,
        index: u64,
        kind: BlockKind,
    ) -> Result<Option<&mut Block>, ProviderError> {
        if !self.blocks.contains_key(&index) {
            let content_index = self.content.len();
            match kind {
                BlockKind::Text => self.content.push(Content::Text {
                    text: String::new(),
                }),
                BlockKind::Thinking => self.content.push(Content::thinking(String::new())),
                // A tool delta needs the id and name its start carried.
                BlockKind::Tool => {
                    return Err(protocol_error(format!(
                        "toolUse delta for block {index} without a contentBlockStart"
                    )))
                }
                BlockKind::Ignored => {}
            }
            self.blocks
                .insert(index, Block::new(kind, content_index, String::new()));
        }
        let block = self.blocks.get_mut(&index).expect("inserted above");
        if block.kind == BlockKind::Ignored {
            return Ok(None);
        }
        if block.kind != kind {
            return Err(protocol_error(format!(
                "block {index} is a {:?} block but received a {kind:?} delta",
                block.kind
            )));
        }
        if block.closed {
            return Err(protocol_error(format!(
                "delta for block {index} after its contentBlockStop"
            )));
        }
        Ok(Some(block))
    }

    fn block_delta(
        &mut self,
        e: ContentBlockDeltaEvent,
        tx: &mpsc::UnboundedSender<StreamEvent>,
    ) -> Result<(), ProviderError> {
        let index = e.content_block_index;
        let delta = e.delta;
        let known =
            delta.text.is_some() || delta.tool_use.is_some() || delta.reasoning_content.is_some();
        if let Some(text) = delta.text {
            if let Some(block) = self.block_for(index, BlockKind::Text)? {
                let ci = block.content_index;
                if let Some(Content::Text { text: t }) = self.content.get_mut(ci) {
                    t.push_str(&text);
                }
                let _ = tx.send(StreamEvent::TextDelta {
                    content_index: ci,
                    delta: text,
                });
            }
        }
        if let Some(tool) = delta.tool_use {
            if let Some(block) = self.block_for(index, BlockKind::Tool)? {
                block.input.push_str(&tool.input);
                let _ = tx.send(StreamEvent::ToolCallDelta {
                    content_index: block.content_index,
                    delta: tool.input,
                });
            }
        }
        if let Some(reasoning) = delta.reasoning_content {
            self.reasoning_delta(index, reasoning, tx)?;
        }
        // Union members this provider does not surface (`citation`,
        // `image`, `toolResult`, anything added later) and an empty delta.
        if !delta.other.is_empty() {
            let names = member_names(&delta.other);
            self.warn_once(format!("delta:{index}:{names}"), || {
                format!("dropping `{names}` content in block {index} (not surfaced)")
            });
        } else if !known {
            self.warn_once(format!("delta:{index}:empty"), || {
                format!("contentBlockDelta for block {index} has no member")
            });
        }
        Ok(())
    }

    fn reasoning_delta(
        &mut self,
        index: u64,
        reasoning: ReasoningContentBlockDelta,
        tx: &mpsc::UnboundedSender<StreamEvent>,
    ) -> Result<(), ProviderError> {
        let Some(block) = self.block_for(index, BlockKind::Thinking)? else {
            return Ok(());
        };
        let ci = block.content_index;
        if let Some(data) = &reasoning.redacted_content {
            use base64::Engine as _;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data)
                .map_err(|e| {
                    protocol_error(format!(
                        "redactedContent in block {index} is not base64 ({e})"
                    ))
                })?;
            block.redacted.extend_from_slice(&bytes);
        }
        // Re-encoded only when this delta carried redacted data.
        let redacted = reasoning.redacted_content.is_some().then(|| {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD.encode(&block.redacted)
        });
        if let Some(Content::Thinking {
            thinking,
            signature,
            redacted: slot,
        }) = self.content.get_mut(ci)
        {
            if let Some(text) = reasoning.text {
                thinking.push_str(&text);
                let _ = tx.send(StreamEvent::ThinkingDelta {
                    content_index: ci,
                    delta: text,
                });
            }
            // A signature is a delta like any other member of this union:
            // pieces are appended. The API reference does not say it arrives
            // in one piece, and appending is the only reading under which a
            // split signature survives; a single delta is the same either way.
            if let Some(sig) = reasoning.signature {
                signature.get_or_insert_with(String::new).push_str(&sig);
            }
            if redacted.is_some() {
                *slot = redacted;
            }
        }
        Ok(())
    }

    fn block_stop(&mut self, index: u64, tx: &mpsc::UnboundedSender<StreamEvent>) {
        let Some(block) = self.blocks.get_mut(&index) else {
            debug!("Bedrock: contentBlockStop for unseen block {index}");
            return;
        };
        if block.closed {
            debug!("Bedrock: duplicate contentBlockStop for block {index}");
            return;
        }
        block.closed = true;
        if block.kind == BlockKind::Tool {
            let args = finalize_tool_arguments(&block.name, &block.input);
            if let Some(Content::ToolCall { arguments, .. }) =
                self.content.get_mut(block.content_index)
            {
                *arguments = args;
            }
            let _ = tx.send(StreamEvent::ToolCallEnd {
                content_index: block.content_index,
            });
        }
    }

    fn finish(
        self,
        tx: &mpsc::UnboundedSender<StreamEvent>,
    ) -> Result<StreamOutcome, ProviderError> {
        let Some((mut stop_reason, error_message)) = self.stop else {
            // No messageStop: the connection closed early, or the body held no
            // events at all. Either way this is not a finished response, and
            // must never read as an empty successful turn.
            let msg = format!(
                "Bedrock stream ended without a messageStop event ({} events received)",
                self.events
            );
            warn!("{msg}");
            return Err(ProviderError::Network(msg));
        };
        let mut content = self.content;
        let mut has_tool_calls = false;
        for block in self.blocks.values() {
            if block.kind != BlockKind::Tool {
                continue;
            }
            has_tool_calls = true;
            if !block.closed {
                // No contentBlockStop: the input may be cut short, and an
                // empty buffer cannot be told apart from a tool with no
                // parameters. Keep the marker so the call is answered with an
                // error, never run.
                warn!(
                    tool = %block.name,
                    "Bedrock tool call block was never closed; the call will not be run"
                );
                if let Some(Content::ToolCall { arguments, .. }) =
                    content.get_mut(block.content_index)
                {
                    *arguments = unfinalized_arguments(&block.input);
                }
                let _ = tx.send(StreamEvent::ToolCallEnd {
                    content_index: block.content_index,
                });
            }
        }
        if stop_reason == StopReason::ToolUse {
            let runnable = content.iter().any(|c| {
                matches!(c, Content::ToolCall { arguments, .. }
                    if crate::provider::unparsed_tool_arguments(arguments).is_none())
            });
            if !runnable {
                warn!("Bedrock: stopReason is tool_use but the response has no runnable tool call");
            }
        }
        // Same rule as the OpenAI-shaped providers: tool calls make this a
        // ToolUse turn, unless it hit the token limit (how a call ends up with
        // unparsed arguments), was refused, or failed.
        if has_tool_calls
            && !matches!(
                stop_reason,
                StopReason::Length | StopReason::Refusal | StopReason::Error
            )
        {
            stop_reason = StopReason::ToolUse;
        }
        // A stream that ends without usage reports zero tokens, as the other
        // providers do when their usage chunk never arrives: `Usage` has no
        // "unknown" state. The warning carries a structured `usage_missing`
        // field and is emitted inside the loop's `llm_stream` span, so
        // tracing/OTel consumers can find these turns.
        let usage = self.usage.unwrap_or_else(|| {
            warn!(
                usage_missing = true,
                "Bedrock stream carried no metadata usage; reporting zero tokens"
            );
            Usage::default()
        });
        Ok(StreamOutcome {
            content,
            usage,
            stop_reason,
            error_message,
        })
    }
}

/// Map a `MessageStopEvent.stopReason` onto [`StopReason`], with a diagnosis
/// for the terminal ones. Every documented value is listed ("Valid Values:
/// end_turn | tool_use | max_tokens | stop_sequence | guardrail_intervened |
/// content_filtered | malformed_model_output | malformed_tool_use |
/// model_context_window_exceeded").
fn map_stop_reason(reason: &str) -> (StopReason, Option<String>) {
    match reason {
        "end_turn" | "stop_sequence" => (StopReason::Stop, None),
        "tool_use" => (StopReason::ToolUse, None),
        "max_tokens" => (StopReason::Length, None),
        "guardrail_intervened" => {
            warn!("Bedrock: a guardrail intervened (stopReason=guardrail_intervened)");
            (
                StopReason::Refusal,
                Some("Response blocked by an Amazon Bedrock guardrail (stopReason: guardrail_intervened)".into()),
            )
        }
        "content_filtered" => {
            warn!("Bedrock: response stopped by the content filter (stopReason=content_filtered)");
            (
                StopReason::Refusal,
                Some(
                    "Response stopped by the content filter (stopReason: content_filtered)".into(),
                ),
            )
        }
        // Same Error + overflow-phrase shape as the Anthropic provider, so
        // `Message::is_context_overflow()` and compaction keep working.
        "model_context_window_exceeded" => {
            warn!("Bedrock: context window exceeded mid-stream");
            (
                StopReason::Error,
                Some("model_context_window_exceeded".into()),
            )
        }
        "malformed_model_output" | "malformed_tool_use" => {
            warn!("Bedrock: the model produced malformed output (stopReason={reason})");
            (
                StopReason::Error,
                Some(format!(
                    "Bedrock stopped the response: the model produced malformed output (stopReason: {reason})"
                )),
            )
        }
        other => {
            warn!("unrecognized Bedrock stopReason '{other}'; treating it as a normal stop");
            (StopReason::Stop, None)
        }
    }
}

/// Budget for Bedrock's Anthropic-style thinking per level — the legacy
/// Anthropic budget table, shared so the two cannot drift. Unlike the
/// first-party path, Bedrock does not raise `maxTokens` above the budget.
fn bedrock_thinking_budget(level: ThinkingLevel) -> u32 {
    super::anthropic::legacy_thinking_budget(level)
}

fn build_bedrock_body(config: &StreamConfig) -> serde_json::Value {
    let mut messages: Vec<serde_json::Value> = Vec::new();
    // Claude verifies replayed reasoning against its signature, so unsigned
    // reasoning (from another provider, after a model switch) cannot go back
    // to it. Bedrock's other reasoning models do not sign at all, and their
    // reasoning is replayed without one.
    let signed_reasoning_only = config.model.to_ascii_lowercase().contains("claude");

    for msg in &config.messages {
        match msg {
            Message::User { content, .. } => {
                let blocks = content_to_bedrock(content, signed_reasoning_only);
                messages.push(serde_json::json!({"role": "user", "content": blocks}));
            }
            Message::Assistant { content, .. } => {
                let blocks = content_to_bedrock(content, signed_reasoning_only);
                messages.push(serde_json::json!({"role": "assistant", "content": blocks}));
            }
            Message::ToolResult {
                tool_call_id,
                content,
                is_error,
                ..
            } => {
                // Build content blocks for tool result (text + images)
                let tool_content: Vec<serde_json::Value> = content
                    .iter()
                    .filter(|c| !matches!(c, Content::Text { text } if text.is_empty()))
                    .filter_map(|c| match c {
                        Content::Text { text } => Some(serde_json::json!({"text": text})),
                        Content::Image { data, mime_type } => Some(serde_json::json!({
                            "image": {
                                "format": mime_type.split('/').nth(1).unwrap_or("png"),
                                "source": {"bytes": data},
                            }
                        })),
                        _ => None,
                    })
                    .collect();

                let tool_content = if tool_content.is_empty() {
                    vec![serde_json::json!({"text": ""})]
                } else {
                    tool_content
                };

                messages.push(serde_json::json!({
                    "role": "user",
                    "content": [{
                        "toolResult": {
                            "toolUseId": tool_call_id,
                            "content": tool_content,
                            "status": if *is_error { "error" } else { "success" },
                        }
                    }],
                }));
            }
        }
    }

    let mut body = serde_json::json!({"messages": messages});

    if !config.system_prompt.is_empty() {
        body["system"] = serde_json::json!([{"text": config.system_prompt}]);
    }

    let mut inference_config = serde_json::json!({});
    if let Some(max) = config.max_tokens {
        inference_config["maxTokens"] = serde_json::json!(max);
    }
    if let Some(temp) = config.temperature {
        inference_config["temperature"] = serde_json::json!(temp);
    }
    if inference_config != serde_json::json!({}) {
        body["inferenceConfig"] = inference_config;
    }

    // Thinking: Claude models on Bedrock take Anthropic's budget-based
    // thinking via additionalModelRequestFields (same budgets as the
    // pre-adaptive Anthropic path).
    if config.thinking_level != ThinkingLevel::Off {
        body["additionalModelRequestFields"] = serde_json::json!({
            "thinking": {
                "type": "enabled",
                "budget_tokens": bedrock_thinking_budget(config.thinking_level),
            }
        });
    }

    if !config.tools.is_empty() {
        let tools: Vec<serde_json::Value> = config
            .tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "toolSpec": {
                        "name": t.name,
                        "description": t.description,
                        "inputSchema": {"json": t.parameters},
                    }
                })
            })
            .collect();
        body["toolConfig"] = serde_json::json!({"tools": tools});
    }

    body
}

fn content_to_bedrock(content: &[Content], signed_reasoning_only: bool) -> Vec<serde_json::Value> {
    content
        .iter()
        .filter(|c| !matches!(c, Content::Text { text } if text.is_empty()))
        .filter_map(|c| match c {
            Content::Text { text } => Some(serde_json::json!({"text": text})),
            Content::Image { data, mime_type } => Some(serde_json::json!({
                "image": {
                    "format": mime_type.split('/').nth(1).unwrap_or("png"),
                    "source": {"bytes": data},
                }
            })),
            Content::ToolCall {
                id,
                name,
                arguments,
                ..
            } => Some(serde_json::json!({
                "toolUse": {"toolUseId": id, "name": name, "input": arguments},
            })),
            // Replay reasoning blocks: Anthropic-on-Bedrock requires the
            // thinking block to accompany a replayed assistant message in
            // multi-turn tool use, unmodified ("include the text and its
            // signature unmodified" — ReasoningTextBlock). The block is a
            // union (ReasoningContentBlock: `reasoningText` | `redactedContent`).
            Content::Thinking {
                redacted: Some(data),
                ..
            } => Some(serde_json::json!({
                "reasoningContent": {"redactedContent": data}
            })),
            Content::Thinking {
                thinking,
                signature: Some(signature),
                ..
            } if !signature.is_empty() => Some(serde_json::json!({
                "reasoningContent": {
                    "reasoningText": {"text": thinking, "signature": signature}
                }
            })),
            // Unsigned reasoning. `signature` is optional in
            // ReasoningTextBlock ("Required: No"), and Bedrock's non-Claude
            // reasoning models never sign, so it is sent without one — never
            // as `signature: ""`. Claude would reject it, so for a Claude
            // model it is skipped; that only happens after a switch from
            // another provider, so it logs at debug, not once per turn.
            Content::Thinking { thinking, .. } if !thinking.is_empty() => {
                if signed_reasoning_only {
                    debug!("Bedrock: not replaying unsigned reasoning to a Claude model");
                    None
                } else {
                    Some(serde_json::json!({
                        "reasoningContent": {"reasoningText": {"text": thinking}}
                    }))
                }
            }
            // Nothing to replay: no text, no signature, no redacted data.
            Content::Thinking { .. } => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// ConverseStream event payloads.
//
// Each struct is the JSON payload of the frame whose `:event-type` header
// names it, as documented in the Amazon Bedrock Runtime API reference
// (docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_<Type>.html).
// Unknown fields are ignored: Bedrock pads payloads with an extra `p` field,
// and new optional members appear over time. Inside the two content unions
// (`ContentBlockStart`, `ContentBlockDelta`) unknown members are collected
// instead, so content this provider does not surface (`image`, `toolResult`,
// `citation`, anything added later) is dropped with a warning, not silently.
// ---------------------------------------------------------------------------

/// `MessageStartEvent`: "role — The role for the message. Valid Values: user
/// | assistant | system. Required: Yes".
/// Only logged, so a proxy that omits it does not fail the turn.
#[derive(Deserialize)]
struct MessageStartEvent {
    #[serde(default)]
    role: String,
}

/// `ContentBlockStartEvent`: "contentBlockIndex — The index for a content
/// block start event. Required: Yes"; "start — Start information about a
/// content block start event. Type: ContentBlockStart object ... a Union".
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContentBlockStartEvent {
    content_block_index: u64,
    start: ContentBlockStart,
}

/// `ContentBlockStart` (union): `toolUse` | `image` | `toolResult`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContentBlockStart {
    #[serde(default)]
    tool_use: Option<ToolUseBlockStart>,
    #[serde(default)]
    image: Option<serde_json::Value>,
    #[serde(default)]
    tool_result: Option<serde_json::Value>,
    /// Any other member, named in a warning.
    #[serde(flatten)]
    other: BTreeMap<String, serde_json::Value>,
}

/// `ToolUseBlockStart`: `name` and `toolUseId` required; optional `type`
/// ("Valid Values: server_tool_use").
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolUseBlockStart {
    tool_use_id: String,
    name: String,
    #[serde(default, rename = "type")]
    kind: Option<String>,
}

/// `ContentBlockDeltaEvent`: "contentBlockIndex — The block index for a
/// content block delta event. Required: Yes"; "delta — The delta for a
/// content block delta event. Type: ContentBlockDelta object ... a Union".
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContentBlockDeltaEvent {
    content_block_index: u64,
    delta: ContentBlockDelta,
}

/// `ContentBlockDelta` (union): `text` (String) | `toolUse`
/// (`ToolUseBlockDelta`) | `reasoningContent` (`ReasoningContentBlockDelta`)
/// | `citation` | `image` | `toolResult`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContentBlockDelta {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    tool_use: Option<ToolUseBlockDelta>,
    #[serde(default)]
    reasoning_content: Option<ReasoningContentBlockDelta>,
    /// `citation`, `image`, `toolResult` or a member added later: not
    /// surfaced, named in a warning.
    #[serde(flatten)]
    other: BTreeMap<String, serde_json::Value>,
}

/// `ToolUseBlockDelta`: "input — The input for a requested tool. Type:
/// String. Required: Yes" — a fragment of the argument JSON text.
#[derive(Deserialize)]
struct ToolUseBlockDelta {
    input: String,
}

/// `ReasoningContentBlockDelta` (union): `text` | `signature` ("If you pass a
/// reasoning block back to the API in a multi-turn conversation, include the
/// text and its signature unmodified") | `redactedContent` (base64).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReasoningContentBlockDelta {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    signature: Option<String>,
    #[serde(default)]
    redacted_content: Option<String>,
}

/// `ContentBlockStopEvent`: "contentBlockIndex — The index for a content
/// block. Required: Yes".
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContentBlockStopEvent {
    content_block_index: u64,
}

/// `MessageStopEvent`: "stopReason — The reason why the model stopped
/// generating output. Type: String ... Required: Yes";
/// `additionalModelResponseFields` (JSON, optional) is not used.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MessageStopEvent {
    stop_reason: String,
}

/// `ConverseStreamMetadataEvent`: `usage` (`TokenUsage`) and `metrics`
/// (`ConverseStreamMetrics`, `latencyMs`) are documented as required; only
/// usage is read, and its absence is tolerated (with a warning) rather than
/// failing an otherwise finished response.
#[derive(Deserialize)]
struct MetadataEvent {
    #[serde(default)]
    usage: Option<TokenUsage>,
}

/// `TokenUsage`: `inputTokens`, `outputTokens`, `totalTokens` required;
/// `cacheReadInputTokens` ("The number of input tokens read from the cache
/// for the request") and `cacheWriteInputTokens` ("... written to the cache
/// ...") optional.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TokenUsage {
    input_tokens: u64,
    output_tokens: u64,
    /// Copied through when present; a proxy that omits it gets 0 rather
    /// than a failed turn.
    #[serde(default)]
    total_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    cache_write_input_tokens: Option<u64>,
}

impl TokenUsage {
    fn into_usage(self) -> Usage {
        Usage {
            input: self.input_tokens,
            output: self.output_tokens,
            cache_read: self.cache_read_input_tokens.unwrap_or(0),
            cache_write: self.cache_write_input_tokens.unwrap_or(0),
            total_tokens: self.total_tokens,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_blocks_are_replayed_with_signature() {
        // Anthropic-on-Bedrock rejects replayed assistant messages whose
        // thinking block was dropped — pin that we serialize it back.
        let mut config = StreamConfig::new("anthropic.claude-sonnet", "a:b");
        config.messages = vec![
            Message::user("go"),
            Message::assistant(
                vec![
                    Content::thinking_signed("chain of thought", "sig-1"),
                    Content::Text {
                        text: "answer".into(),
                    },
                ],
                StopReason::Stop,
                "m",
                "bedrock",
                Usage::default(),
            ),
        ];
        let body = build_bedrock_body(&config);
        let assistant_content = body["messages"][1]["content"].as_array().unwrap();
        let reasoning = assistant_content
            .iter()
            .find(|b| b.get("reasoningContent").is_some())
            .expect("thinking block must be replayed");
        assert_eq!(
            reasoning["reasoningContent"]["reasoningText"]["text"],
            "chain of thought"
        );
        assert_eq!(
            reasoning["reasoningContent"]["reasoningText"]["signature"],
            "sig-1"
        );
    }

    #[test]
    fn thinking_level_sets_additional_model_request_fields() {
        let config = StreamConfig {
            model: "anthropic.claude-sonnet".into(),
            system_prompt: "".into(),
            messages: vec![Message::user("hi")],
            tools: vec![],
            thinking_level: ThinkingLevel::High,
            api_key: "a:b".into(),
            max_tokens: Some(1024),
            temperature: None,
            model_config: None,
            cache_config: CacheConfig::default(),
            output_schema: None,
        };
        let body = build_bedrock_body(&config);
        let thinking = &body["additionalModelRequestFields"]["thinking"];
        assert_eq!(thinking["type"], "enabled");
        assert_eq!(thinking["budget_tokens"], 8192);
    }

    #[test]
    fn xhigh_and_max_use_the_legacy_anthropic_budgets() {
        for (level, budget) in [(ThinkingLevel::XHigh, 16_384), (ThinkingLevel::Max, 30_720)] {
            let config = StreamConfig {
                model: "anthropic.claude-sonnet".into(),
                system_prompt: "".into(),
                messages: vec![Message::user("hi")],
                tools: vec![],
                thinking_level: level,
                api_key: "a:b".into(),
                max_tokens: Some(64_000),
                temperature: None,
                model_config: None,
                cache_config: CacheConfig::default(),
                output_schema: None,
            };
            let body = build_bedrock_body(&config);
            assert_eq!(
                body["additionalModelRequestFields"]["thinking"]["budget_tokens"],
                budget
            );
        }
    }

    #[test]
    fn thinking_off_omits_additional_fields() {
        let config = StreamConfig {
            model: "anthropic.claude-sonnet".into(),
            system_prompt: "".into(),
            messages: vec![Message::user("hi")],
            tools: vec![],
            thinking_level: ThinkingLevel::Off,
            api_key: "a:b".into(),
            max_tokens: Some(1024),
            temperature: None,
            model_config: None,
            cache_config: CacheConfig::default(),
            output_schema: None,
        };
        let body = build_bedrock_body(&config);
        assert!(body["additionalModelRequestFields"].is_null());
    }

    #[test]
    fn test_build_bedrock_body() {
        let config = StreamConfig {
            model: "anthropic.claude-3-sonnet-20240229-v1:0".into(),
            system_prompt: "Be helpful".into(),
            messages: vec![Message::user("Hello")],
            tools: vec![],
            thinking_level: ThinkingLevel::Off,
            api_key: "key:secret".into(),
            max_tokens: Some(1024),
            temperature: None,
            model_config: None,
            cache_config: CacheConfig::default(),
            output_schema: None,
        };

        let body = build_bedrock_body(&config);
        assert!(body["messages"].is_array());
        assert_eq!(body["messages"][0]["role"], "user");
        assert!(body["system"].is_array());
        assert_eq!(body["inferenceConfig"]["maxTokens"], 1024);
    }

    #[test]
    fn test_content_to_bedrock_filters_empty_text() {
        let content = vec![
            Content::Text { text: "".into() },
            Content::Text {
                text: "hello".into(),
            },
            Content::Text { text: "".into() },
        ];
        let blocks = content_to_bedrock(&content, false);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["text"], "hello");
    }

    #[test]
    fn test_content_to_bedrock() {
        let content = vec![
            Content::Text {
                text: "hello".into(),
            },
            Content::ToolCall {
                provider_metadata: None,
                id: "tc-1".into(),
                name: "bash".into(),
                arguments: serde_json::json!({"command": "ls"}),
            },
        ];
        let blocks = content_to_bedrock(&content, false);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["text"], "hello");
        assert_eq!(blocks[1]["toolUse"]["name"], "bash");
    }

    // -- Response stream ---------------------------------------------------

    use super::super::eventstream::{encode_frame, HeaderValue};

    fn event(event_type: &str, payload: serde_json::Value) -> Vec<u8> {
        encode_frame(
            &[
                (":message-type", HeaderValue::String("event".into())),
                (":event-type", HeaderValue::String(event_type.into())),
                (
                    ":content-type",
                    HeaderValue::String("application/json".into()),
                ),
            ],
            payload.to_string().as_bytes(),
        )
    }

    /// A text + tool-call response whose text has multi-byte characters.
    fn full_response() -> Vec<u8> {
        use serde_json::json;
        [
            event("messageStart", json!({"role": "assistant"})),
            event(
                "contentBlockDelta",
                json!({"contentBlockIndex": 0, "delta": {"text": "héllo 世界 🌍"}}),
            ),
            event("contentBlockStop", json!({"contentBlockIndex": 0})),
            event(
                "contentBlockStart",
                json!({"contentBlockIndex": 1, "start": {"toolUse": {"toolUseId": "t1", "name": "read"}}}),
            ),
            event(
                "contentBlockDelta",
                json!({"contentBlockIndex": 1, "delta": {"toolUse": {"input": "{\"path\":"}}}),
            ),
            event(
                "contentBlockDelta",
                json!({"contentBlockIndex": 1, "delta": {"toolUse": {"input": "\"ß.txt\"}"}}}),
            ),
            event("contentBlockStop", json!({"contentBlockIndex": 1})),
            event("messageStop", json!({"stopReason": "tool_use"})),
            event(
                "metadata",
                json!({"usage": {"inputTokens": 5, "outputTokens": 7, "totalTokens": 12}, "metrics": {"latencyMs": 3}}),
            ),
        ]
        .concat()
    }

    async fn read_chunks(
        chunks: Vec<Result<Vec<u8>, String>>,
    ) -> Result<StreamOutcome, ProviderError> {
        let (tx, _rx) = mpsc::unbounded_channel();
        read_converse_stream(
            futures::stream::iter(chunks),
            &tx,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
    }

    /// The provider's byte-stream path buffers across network chunks: every
    /// chunk size from 1 byte up splits frames inside the prelude, inside
    /// headers and inside multi-byte UTF-8 characters, and the result is the
    /// same as one chunk.
    #[tokio::test]
    async fn byte_stream_is_buffered_across_arbitrary_chunks() {
        let body = full_response();
        for size in 1..=body.len() {
            let chunks = body.chunks(size).map(|c| Ok(c.to_vec())).collect();
            let out = read_chunks(chunks)
                .await
                .unwrap_or_else(|e| panic!("chunk size {size}: {e}"));
            assert_eq!(out.stop_reason, StopReason::ToolUse, "chunk size {size}");
            assert!(
                matches!(&out.content[0], Content::Text { text } if text == "héllo 世界 🌍"),
                "chunk size {size}: {:?}",
                out.content
            );
            assert!(
                matches!(&out.content[1], Content::ToolCall { arguments, .. }
                    if *arguments == serde_json::json!({"path": "ß.txt"})),
                "chunk size {size}: {:?}",
                out.content
            );
            assert_eq!(out.usage.total_tokens, 12);
        }
    }

    #[tokio::test]
    async fn transport_error_mid_stream_is_a_network_error() {
        let body = full_response();
        let (head, _) = body.split_at(body.len() / 2);
        let result = read_chunks(vec![Ok(head.to_vec()), Err("connection reset".into())]).await;
        assert!(
            matches!(result, Err(ProviderError::Network(_))),
            "{:?}",
            result.err()
        );
    }

    /// Cancellation wins over a stream that has stalled mid-response.
    #[tokio::test]
    async fn cancellation_interrupts_a_stalled_stream() {
        use futures::StreamExt as _;
        let first: Vec<Result<Vec<u8>, String>> = vec![Ok(event(
            "messageStart",
            serde_json::json!({"role": "assistant"}),
        ))];
        let stream = futures::stream::iter(first).chain(futures::stream::pending());
        let (tx, _rx) = mpsc::unbounded_channel();
        let cancel = tokio_util::sync::CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            trigger.cancel();
        });
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            read_converse_stream(stream, &tx, &cancel),
        )
        .await
        .expect("cancellation must end the read");
        assert!(matches!(result, Err(ProviderError::Cancelled)));
    }

    #[test]
    fn reasoning_replay_rules() {
        let content = [
            Content::thinking("unsigned reasoning"),
            Content::thinking(""),
            Content::thinking_redacted("AAEC"),
            Content::thinking_signed("real", "sig"),
            Content::Text { text: "t".into() },
        ];
        let redacted = serde_json::json!({"reasoningContent": {"redactedContent": "AAEC"}});
        let signed = serde_json::json!(
            {"reasoningContent": {"reasoningText": {"text": "real", "signature": "sig"}}}
        );
        let text = serde_json::json!({"text": "t"});

        // A model that does not sign gets unsigned reasoning back, with no
        // signature key at all (never `""`); an empty block is dropped.
        assert_eq!(
            content_to_bedrock(&content, false),
            vec![
                serde_json::json!(
                    {"reasoningContent": {"reasoningText": {"text": "unsigned reasoning"}}}
                ),
                redacted.clone(),
                signed.clone(),
                text.clone(),
            ]
        );
        // Claude verifies signatures: unsigned reasoning is not replayed.
        assert_eq!(
            content_to_bedrock(&content, true),
            vec![redacted, signed, text]
        );
    }

    #[test]
    fn claude_model_ids_replay_signed_reasoning_only() {
        let unsigned = || Message::Assistant {
            content: vec![
                Content::thinking("mine"),
                Content::Text { text: "a".into() },
            ],
            stop_reason: StopReason::Stop,
            model: String::new(),
            provider: String::new(),
            usage: Usage::default(),
            timestamp: 0,
            error_message: None,
        };
        let body_for = |model: &str| {
            build_bedrock_body(&StreamConfig {
                model: model.into(),
                system_prompt: String::new(),
                messages: vec![unsigned()],
                tools: vec![],
                thinking_level: ThinkingLevel::Off,
                api_key: "key:secret".into(),
                max_tokens: Some(1024),
                temperature: None,
                model_config: None,
                cache_config: CacheConfig::default(),
                output_schema: None,
            })
        };
        let reasoning_blocks = |body: serde_json::Value| {
            body["messages"][0]["content"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|b| b.get("reasoningContent").is_some())
                .count()
        };
        assert_eq!(
            reasoning_blocks(body_for("us.anthropic.claude-sonnet-5-v1:0")),
            0
        );
        assert_eq!(reasoning_blocks(body_for("openai.gpt-oss-120b-1:0")), 1);
    }

    #[tokio::test]
    async fn zero_bytes_is_an_error_not_an_empty_turn() {
        let result = read_chunks(vec![]).await;
        assert!(matches!(result, Err(ProviderError::Network(_))));
    }

    #[test]
    fn exceptions_classify_like_their_http_status() {
        assert!(matches!(
            exception_error("throttlingException", "Too many requests"),
            ProviderError::RateLimited { .. }
        ));
        assert!(exception_error(
            "validationException",
            "Input is too long for requested model."
        )
        .is_context_overflow());
        assert!(matches!(
            exception_error("validationException", "bad field"),
            ProviderError::Api(_)
        ));
        assert!(matches!(
            exception_error("modelStreamErrorException", "boom"),
            ProviderError::Api(_)
        ));
    }

    #[test]
    fn every_documented_stop_reason_is_mapped() {
        use StopReason::*;
        for (reason, expected) in [
            ("end_turn", Stop),
            ("stop_sequence", Stop),
            ("tool_use", ToolUse),
            ("max_tokens", Length),
            ("guardrail_intervened", Refusal),
            ("content_filtered", Refusal),
            ("malformed_model_output", Error),
            ("malformed_tool_use", Error),
            ("model_context_window_exceeded", Error),
        ] {
            assert_eq!(map_stop_reason(reason).0, expected, "{reason}");
        }
        let (_, msg) = map_stop_reason("model_context_window_exceeded");
        assert!(crate::provider::traits::is_context_overflow_message(
            &msg.unwrap()
        ));
    }
}
