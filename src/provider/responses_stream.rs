//! Shared SSE parser for the OpenAI Responses event stream, used by both
//! [`OpenAiResponsesProvider`](super::OpenAiResponsesProvider) and
//! [`AzureOpenAiProvider`](super::AzureOpenAiProvider) (Azure serves the same
//! Responses API with the same event names).
//!
//! Event names and payload shapes follow OpenAI's generated types
//! (`openai-python`, `src/openai/types/responses/response_*_event.py`):
//!
//! - A function call **starts** with `response.output_item.added` whose
//!   `item.type == "function_call"` (carrying `call_id`, `name`, `id`). There
//!   is no `response.function_call_arguments.start` event.
//! - `response.function_call_arguments.delta` carries `output_index` and
//!   `item_id`, so parallel calls are routed to their own buffer rather than
//!   to "the last one".
//! - `response.function_call_arguments.done` and `response.output_item.done`
//!   carry the complete `arguments`; they are the source of truth, which also
//!   covers servers that send no deltas at all.
//! - Reasoning arrives as `response.reasoning_summary_text.delta` (summaries,
//!   what hosted OpenAI reasoning models stream) and
//!   `response.reasoning_text.delta` (raw reasoning content).
//! - `response.completed` / `response.incomplete` carry the final `usage`,
//!   including `input_tokens_details.cached_tokens` and `.cache_write_tokens`.

use super::tool_args::finalize_tool_arguments;
use super::traits::{classify_sse_error_event, ProviderError, StreamEvent};
use crate::types::{Content, StopReason, Usage};
use serde::Deserialize;
use std::collections::HashMap;
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// What the caller's read loop should do after an event.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Flow {
    Continue,
    /// A terminal event (`response.completed` / `response.incomplete`) arrived.
    Done,
}

/// One in-flight function call. Its `Content::ToolCall` slot is reserved in
/// `content` when the call starts, so content order follows `output_index`.
struct CallSlot {
    content_index: usize,
    output_index: Option<usize>,
    item_id: Option<String>,
    call_id: String,
    name: String,
    arguments: String,
    ended: bool,
}

/// Which kind of reasoning text a thinking block collects. Summaries and raw
/// reasoning text for the same item go to separate blocks.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum ReasoningKind {
    Summary,
    Text,
}

/// Accumulates a Responses stream into assistant content, usage and a stop
/// reason. Feed it every SSE message with [`handle`](Self::handle), then call
/// [`finish`](Self::finish).
pub(crate) struct ResponsesStreamState {
    label: &'static str,
    content: Vec<Content>,
    /// output_index (None when a server omits it) → content index of its text block.
    text_slots: HashMap<Option<usize>, usize>,
    /// (output_index, kind) → (content index, last part index seen).
    thinking_slots: HashMap<(Option<usize>, ReasoningKind), (usize, Option<u64>)>,
    calls: Vec<CallSlot>,
    usage: Usage,
    stop_reason: StopReason,
}

impl ResponsesStreamState {
    /// `label` names the provider in log lines.
    pub(crate) fn new(label: &'static str) -> Self {
        Self {
            label,
            content: Vec::new(),
            text_slots: HashMap::new(),
            thinking_slots: HashMap::new(),
            calls: Vec::new(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
        }
    }

    /// Process one SSE message. `event` is the SSE `event:` field; when a
    /// server sends only `data:` lines (the field then defaults to
    /// `"message"`), the event name is taken from the payload's `type`.
    pub(crate) fn handle(
        &mut self,
        event: &str,
        data: &str,
        tx: &mpsc::UnboundedSender<StreamEvent>,
    ) -> Result<Flow, ProviderError> {
        let owned_type;
        let event = if event.is_empty() || event == "message" {
            owned_type = serde_json::from_str::<TypeOnly>(data)
                .ok()
                .and_then(|t| t.kind)
                .unwrap_or_default();
            owned_type.as_str()
        } else {
            event
        };

        match event {
            "response.output_text.delta" => {
                let Some(ev) = self.parse::<TextDelta>(event, data) else {
                    return Ok(Flow::Continue);
                };
                let idx = self.text_slot(ev.output_index);
                if let Some(Content::Text { text }) = self.content.get_mut(idx) {
                    text.push_str(&ev.delta);
                }
                let _ = tx.send(StreamEvent::TextDelta {
                    content_index: idx,
                    delta: ev.delta,
                });
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let Some(ev) = self.parse::<ReasoningDelta>(event, data) else {
                    return Ok(Flow::Continue);
                };
                let (kind, part) = if event == "response.reasoning_summary_text.delta" {
                    (ReasoningKind::Summary, ev.summary_index)
                } else {
                    (ReasoningKind::Text, ev.content_index)
                };
                self.thinking_delta(ev.output_index, kind, part, ev.delta, tx);
            }
            "response.output_item.added" => {
                let Some(ev) = self.parse::<OutputItemEvent>(event, data) else {
                    return Ok(Flow::Continue);
                };
                if ev.item.kind.as_deref() == Some("function_call") {
                    self.open_call(ev.output_index, &ev.item, tx);
                }
            }
            "response.function_call_arguments.delta" => {
                let Some(ev) = self.parse::<ArgumentsDelta>(event, data) else {
                    return Ok(Flow::Continue);
                };
                match self.find_call(ev.output_index, ev.item_id.as_deref()) {
                    Some(i) => {
                        let slot = &mut self.calls[i];
                        slot.arguments.push_str(&ev.delta);
                        let _ = tx.send(StreamEvent::ToolCallDelta {
                            content_index: slot.content_index,
                            delta: ev.delta,
                        });
                    }
                    None => warn!(
                        output_index = ?ev.output_index,
                        item_id = ?ev.item_id,
                        "{}: function_call_arguments.delta for an unknown output item; \
                         {} bytes of tool arguments dropped",
                        self.label,
                        ev.delta.len()
                    ),
                }
            }
            "response.function_call_arguments.done" => {
                let Some(ev) = self.parse::<ArgumentsDone>(event, data) else {
                    return Ok(Flow::Continue);
                };
                match self.find_call(ev.output_index, ev.item_id.as_deref()) {
                    Some(i) => self.set_final_arguments(i, ev.arguments, tx),
                    None => warn!(
                        output_index = ?ev.output_index,
                        item_id = ?ev.item_id,
                        "{}: function_call_arguments.done for an unknown output item; \
                         the call is dropped",
                        self.label
                    ),
                }
            }
            "response.output_item.done" => {
                let Some(ev) = self.parse::<OutputItemEvent>(event, data) else {
                    return Ok(Flow::Continue);
                };
                match ev.item.kind.as_deref() {
                    Some("function_call") => {
                        let i = match self.find_call(ev.output_index, ev.item.id.as_deref()) {
                            Some(i) => i,
                            // A server that skipped `output_item.added`.
                            None => self.open_call(ev.output_index, &ev.item, tx),
                        };
                        let slot = &mut self.calls[i];
                        if slot.call_id.is_empty() {
                            slot.call_id = ev.item.call_id.clone().unwrap_or_default();
                        }
                        if slot.name.is_empty() {
                            slot.name = ev.item.name.clone().unwrap_or_default();
                        }
                        if let Some(args) = ev.item.arguments {
                            self.set_final_arguments(i, args, tx);
                        }
                        self.end_call(i, tx);
                    }
                    Some("message") => {
                        // A server that sent the text only in the finished item.
                        if !self.text_slots.contains_key(&ev.output_index) {
                            let text: String = ev
                                .item
                                .content
                                .iter()
                                .filter(|p| p.kind.as_deref() == Some("output_text"))
                                .filter_map(|p| p.text.as_deref())
                                .collect();
                            if !text.is_empty() {
                                let idx = self.text_slot(ev.output_index);
                                if let Some(Content::Text { text: t }) = self.content.get_mut(idx) {
                                    t.push_str(&text);
                                }
                                let _ = tx.send(StreamEvent::TextDelta {
                                    content_index: idx,
                                    delta: text,
                                });
                            }
                        }
                    }
                    _ => {}
                }
            }
            "response.completed" => {
                if let Some(resp) = self
                    .parse::<ResponseEvent>(event, data)
                    .and_then(|e| e.response)
                {
                    if let Some(u) = resp.usage {
                        self.usage = u.into_usage();
                    }
                    if resp.status.as_deref() == Some("incomplete") {
                        self.stop_reason = StopReason::Length;
                    }
                }
                return Ok(Flow::Done);
            }
            // Terminal events other than `response.completed`. Without these
            // arms the loop never breaks, the body closes, and the resulting
            // StreamEnded is retryable (#83) — re-running an already-billed
            // generation that would fail the same way again.
            "response.incomplete" => {
                if let Some(u) = self
                    .parse::<ResponseEvent>(event, data)
                    .and_then(|e| e.response)
                    .and_then(|r| r.usage)
                {
                    self.usage = u.into_usage();
                }
                self.stop_reason = StopReason::Length;
                return Ok(Flow::Done);
            }
            "response.failed" | "error" => {
                let err = classify_sse_error_event(data);
                warn!("{} error: {}", self.label, err);
                return Err(err);
            }
            _ => debug!("{}: unhandled Responses event: {}", self.label, event),
        }
        Ok(Flow::Continue)
    }

    /// Finalize tool calls and return `(content, usage, stop_reason)`.
    pub(crate) fn finish(
        mut self,
        tx: &mpsc::UnboundedSender<StreamEvent>,
    ) -> (Vec<Content>, Usage, StopReason) {
        for i in 0..self.calls.len() {
            self.end_call(i, tx);
        }
        for slot in &self.calls {
            let args = finalize_tool_arguments(&slot.name, &slot.arguments);
            self.content[slot.content_index] = Content::ToolCall {
                provider_metadata: None,
                id: slot.call_id.clone(),
                name: slot.name.clone(),
                arguments: args,
            };
        }

        // Tool calls make this a ToolUse turn — unless the response was
        // incomplete (token limit), which is how a call ends up with unparsed
        // arguments. Length is kept so callers see it; the loop still answers
        // every tool call either way.
        let mut stop_reason = self.stop_reason;
        if stop_reason != StopReason::Length && !self.calls.is_empty() {
            stop_reason = StopReason::ToolUse;
        }
        (self.content, self.usage, stop_reason)
    }

    fn parse<T: for<'de> Deserialize<'de>>(&self, event: &str, data: &str) -> Option<T> {
        match serde_json::from_str(data) {
            Ok(v) => Some(v),
            Err(e) => {
                warn!("{}: could not parse {} payload: {}", self.label, event, e);
                None
            }
        }
    }

    fn text_slot(&mut self, output_index: Option<usize>) -> usize {
        if let Some(&idx) = self.text_slots.get(&output_index) {
            return idx;
        }
        self.content.push(Content::Text {
            text: String::new(),
        });
        let idx = self.content.len() - 1;
        self.text_slots.insert(output_index, idx);
        idx
    }

    fn thinking_delta(
        &mut self,
        output_index: Option<usize>,
        kind: ReasoningKind,
        part: Option<u64>,
        delta: String,
        tx: &mpsc::UnboundedSender<StreamEvent>,
    ) {
        let key = (output_index, kind);
        let (idx, sep) = match self.thinking_slots.get_mut(&key) {
            Some((idx, last_part)) => {
                // A new summary part (or reasoning content part) starts a new
                // paragraph rather than running into the previous one.
                let sep = part.is_some() && *last_part != part;
                *last_part = part;
                (*idx, sep)
            }
            None => {
                self.content.push(Content::Thinking {
                    thinking: String::new(),
                    signature: None,
                });
                let idx = self.content.len() - 1;
                self.thinking_slots.insert(key, (idx, part));
                (idx, false)
            }
        };
        let delta = if sep { format!("\n\n{delta}") } else { delta };
        if let Some(Content::Thinking { thinking, .. }) = self.content.get_mut(idx) {
            thinking.push_str(&delta);
        }
        let _ = tx.send(StreamEvent::ThinkingDelta {
            content_index: idx,
            delta,
        });
    }

    fn open_call(
        &mut self,
        output_index: Option<usize>,
        item: &OutputItem,
        tx: &mpsc::UnboundedSender<StreamEvent>,
    ) -> usize {
        let call_id = item.call_id.clone().unwrap_or_default();
        let name = item.name.clone().unwrap_or_default();
        // Placeholder, replaced in `finish` once the arguments are final.
        self.content.push(Content::ToolCall {
            provider_metadata: None,
            id: call_id.clone(),
            name: name.clone(),
            arguments: serde_json::Value::Null,
        });
        let content_index = self.content.len() - 1;
        let _ = tx.send(StreamEvent::ToolCallStart {
            content_index,
            id: call_id.clone(),
            name: name.clone(),
        });
        let arguments = item.arguments.clone().unwrap_or_default();
        if !arguments.is_empty() {
            let _ = tx.send(StreamEvent::ToolCallDelta {
                content_index,
                delta: arguments.clone(),
            });
        }
        self.calls.push(CallSlot {
            content_index,
            output_index,
            item_id: item.id.clone(),
            call_id,
            name,
            arguments,
            ended: false,
        });
        self.calls.len() - 1
    }

    /// Route by `output_index` first, then by item id.
    fn find_call(&self, output_index: Option<usize>, item_id: Option<&str>) -> Option<usize> {
        if let Some(oi) = output_index {
            if let Some(i) = self.calls.iter().position(|c| c.output_index == Some(oi)) {
                return Some(i);
            }
        }
        let id = item_id?;
        self.calls
            .iter()
            .position(|c| c.item_id.as_deref() == Some(id))
    }

    /// The complete argument text from a `.done` event replaces whatever the
    /// deltas accumulated. If it extends what was streamed (including the
    /// no-deltas case), the missing suffix is emitted as one more delta so
    /// event consumers see the whole text.
    fn set_final_arguments(
        &mut self,
        i: usize,
        arguments: String,
        tx: &mpsc::UnboundedSender<StreamEvent>,
    ) {
        let label = self.label;
        let slot = &mut self.calls[i];
        if slot.arguments == arguments {
            return;
        }
        if let Some(rest) = arguments.strip_prefix(slot.arguments.as_str()) {
            let _ = tx.send(StreamEvent::ToolCallDelta {
                content_index: slot.content_index,
                delta: rest.to_string(),
            });
        } else {
            debug!(
                "{}: final arguments for call {} differ from the streamed deltas; using the final text",
                label, slot.call_id
            );
        }
        slot.arguments = arguments;
    }

    fn end_call(&mut self, i: usize, tx: &mpsc::UnboundedSender<StreamEvent>) {
        let slot = &mut self.calls[i];
        if !slot.ended {
            slot.ended = true;
            let _ = tx.send(StreamEvent::ToolCallEnd {
                content_index: slot.content_index,
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Wire types. Every field is optional/defaulted so a missing field degrades to
// a warning or a fallback route instead of dropping the whole event.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct TypeOnly {
    #[serde(rename = "type", default)]
    kind: Option<String>,
}

#[derive(Deserialize)]
struct TextDelta {
    delta: String,
    #[serde(default)]
    output_index: Option<usize>,
}

#[derive(Deserialize)]
struct ReasoningDelta {
    delta: String,
    #[serde(default)]
    output_index: Option<usize>,
    /// `response.reasoning_summary_text.delta`
    #[serde(default)]
    summary_index: Option<u64>,
    /// `response.reasoning_text.delta`
    #[serde(default)]
    content_index: Option<u64>,
}

#[derive(Deserialize)]
struct ArgumentsDelta {
    delta: String,
    #[serde(default)]
    output_index: Option<usize>,
    #[serde(default)]
    item_id: Option<String>,
}

#[derive(Deserialize)]
struct ArgumentsDone {
    arguments: String,
    #[serde(default)]
    output_index: Option<usize>,
    #[serde(default)]
    item_id: Option<String>,
}

#[derive(Deserialize)]
struct OutputItemEvent {
    #[serde(default)]
    output_index: Option<usize>,
    item: OutputItem,
}

#[derive(Deserialize)]
struct OutputItem {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    call_id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
    /// `message` items: output parts.
    #[serde(default)]
    content: Vec<OutputPart>,
}

#[derive(Deserialize)]
struct OutputPart {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Deserialize)]
struct ResponseEvent {
    #[serde(default)]
    response: Option<ResponseData>,
}

#[derive(Deserialize)]
struct ResponseData {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    usage: Option<ResponseUsage>,
}

#[derive(Deserialize)]
struct ResponseUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    total_tokens: u64,
    #[serde(default)]
    input_tokens_details: Option<InputTokensDetails>,
}

#[derive(Deserialize, Default)]
struct InputTokensDetails {
    #[serde(default)]
    cached_tokens: u64,
    #[serde(default)]
    cache_write_tokens: u64,
}

impl ResponseUsage {
    /// `input_tokens` is the whole prompt, cache hits and cache writes
    /// included. [`Usage::input`] is the *uncached* remainder — the same
    /// convention as the Anthropic and OpenAI-compatible providers — so each
    /// bucket is priced at its own rate by `CostConfig::cost_usd`, which still
    /// sums all three to pick a context tier.
    fn into_usage(self) -> Usage {
        let d = self.input_tokens_details.unwrap_or_default();
        Usage {
            input: self
                .input_tokens
                .saturating_sub(d.cached_tokens)
                .saturating_sub(d.cache_write_tokens),
            output: self.output_tokens,
            cache_read: d.cached_tokens,
            cache_write: d.cache_write_tokens,
            total_tokens: self.total_tokens,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_splits_cache_read_and_write_out_of_input() {
        let u: ResponseUsage = serde_json::from_str(
            r#"{"input_tokens":1000,"input_tokens_details":{"cached_tokens":600,"cache_write_tokens":300},
                "output_tokens":50,"output_tokens_details":{"reasoning_tokens":10},"total_tokens":1050}"#,
        )
        .unwrap();
        let u = u.into_usage();
        assert_eq!(u.input, 100);
        assert_eq!(u.cache_read, 600);
        assert_eq!(u.cache_write, 300);
        assert_eq!(u.output, 50);
        assert_eq!(u.total_tokens, 1050);
    }

    #[test]
    fn usage_without_details_is_all_uncached() {
        let u: ResponseUsage =
            serde_json::from_str(r#"{"input_tokens":5,"output_tokens":1,"total_tokens":6}"#)
                .unwrap();
        let u = u.into_usage();
        assert_eq!((u.input, u.cache_read, u.cache_write), (5, 0, 0));
    }

    #[test]
    fn data_only_messages_take_the_event_name_from_type() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut s = ResponsesStreamState::new("test");
        s.handle(
            "message",
            r#"{"type":"response.output_text.delta","output_index":0,"delta":"hi"}"#,
            &tx,
        )
        .unwrap();
        let (content, _, _) = s.finish(&tx);
        assert!(matches!(&content[0], Content::Text { text } if text == "hi"));
    }
}
