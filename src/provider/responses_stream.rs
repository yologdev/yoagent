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
//! - The finished reasoning item arrives in `response.output_item.done`
//!   (`item.type == "reasoning"`, with `id`, `summary` and, when requested,
//!   `encrypted_content`; the copy in `output_item.added` "may be
//!   incomplete"). An item carrying `encrypted_content` is kept on its
//!   thinking block as redacted data for the stream's protocol (a JSON object
//!   `{id, summary, encrypted_content}`), so the next request can replay it
//!   in place; see `responses_request.rs`. A summary sent only in the
//!   finished item becomes the block's text.
//! - A replayed reasoning item must be followed by its paired output item
//!   ("Item 'rs_…' of type 'reasoning' was provided without its required
//!   following item"), so the ids of the output items that followed it in
//!   the response (up to the next reasoning item, stored or not) are kept in
//!   the same stored JSON: `call_ids` maps each
//!   `function_call`'s `call_id` to its item `id` (`fc_…`), and `message_id`
//!   is the id (`msg_…`) of the first `message` after it. They ride on the
//!   reasoning item, so they carry its protocol tag and are replayed only
//!   where it is (`Content::ToolCall::provider_metadata` stays Gemini's).
//! - `response.completed` / `response.incomplete` carry the final `usage`,
//!   including `input_tokens_details.cached_tokens` and `.cache_write_tokens`.
//!   A usage count sent as explicit `null` reads as 0 rather than failing the
//!   whole terminal event (which would drop the usage and the stop reason).
//! - A refusal streams as `response.refusal.delta` / `response.refusal.done`
//!   (`ResponseRefusalDeltaEvent` / `ResponseRefusalDoneEvent`), and appears as
//!   a `{"type": "refusal", "refusal": …}` content part
//!   (`ResponseOutputRefusal`) in `response.content_part.done` and in the
//!   finished `message` item. It becomes [`StopReason::Refusal`], its text is
//!   kept as the turn's text, and [`error_message`](ResponsesStreamState::error_message)
//!   explains it — the same shape as the Anthropic and Gemini refusals.
//! - `incomplete_details.reason` (`Response.IncompleteDetails`) is one of
//!   `max_output_tokens`, `max_messages`, `content_filter`, `steered`.
//!   `content_filter` is a [`StopReason::Refusal`]; every other reason stays
//!   [`StopReason::Length`].

use super::model::ApiProtocol;
use super::openai_compat::null_as_zero;
use super::tool_args::finalize_tool_arguments;
use super::traits::{classify_sse_error_event, ProviderError, StreamEvent};
use crate::types::{Content, StopReason, Usage};
use serde::Deserialize;
use std::collections::HashMap;
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// Key, in a stored reasoning item, of the `call_id` → item `id` map of the
/// function calls that followed it.
pub(crate) const CALL_IDS_KEY: &str = "call_ids";

/// Key, in a stored reasoning item, of the id of the first `message` item
/// that followed it.
pub(crate) const MESSAGE_ID_KEY: &str = "message_id";

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
    /// The API this stream came from; tags its encrypted reasoning.
    protocol: ApiProtocol,
    content: Vec<Content>,
    /// output_index (None when a server omits it) → content index of its text block.
    text_slots: HashMap<Option<usize>, usize>,
    /// (output_index, kind) → (content index, last part index seen).
    thinking_slots: HashMap<(Option<usize>, ReasoningKind), (usize, Option<u64>)>,
    calls: Vec<CallSlot>,
    /// Content indices of the thinking blocks that store a reasoning item, in
    /// content order.
    reasoning_blocks: Vec<usize>,
    /// Where every finished reasoning item starts in content order, stored or
    /// not: its thinking block's index, or, for one with no block, the index
    /// the next item takes. Output items after it belong to it, not to an
    /// earlier stored reasoning item. Paired with the item's output_index.
    reasoning_bounds: Vec<(usize, Option<usize>)>,
    /// output_index → the `message` item id sent for it.
    message_ids: HashMap<Option<usize>, String>,
    usage: Usage,
    stop_reason: StopReason,
    /// output_index → the refusal text received for it so far. Present once
    /// any refusal signal arrived for that output item.
    refusals: HashMap<Option<usize>, String>,
    /// Why the response was stopped by a content filter, when it was.
    filtered: Option<String>,
}

impl ResponsesStreamState {
    /// `label` names the provider in log lines; `protocol` is the API the
    /// stream came from, recorded on its encrypted reasoning.
    pub(crate) fn new(label: &'static str, protocol: ApiProtocol) -> Self {
        Self {
            label,
            protocol,
            content: Vec::new(),
            text_slots: HashMap::new(),
            thinking_slots: HashMap::new(),
            calls: Vec::new(),
            reasoning_blocks: Vec::new(),
            reasoning_bounds: Vec::new(),
            message_ids: HashMap::new(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            refusals: HashMap::new(),
            filtered: None,
        }
    }

    /// The explanation to put on the assistant message's `error_message`:
    /// set for a refusal or a content-filter stop, `None` otherwise. Read it
    /// before [`finish`](Self::finish), which consumes the state.
    pub(crate) fn error_message(&self) -> Option<String> {
        if let Some(reason) = &self.filtered {
            return Some(format!(
                "Response stopped by the content filter (incomplete_details.reason: {reason})"
            ));
        }
        if self.refusals.is_empty() {
            return None;
        }
        let mut keys: Vec<_> = self.refusals.keys().copied().collect();
        keys.sort();
        let text: Vec<&str> = keys
            .iter()
            .map(|k| self.refusals[k].as_str())
            .filter(|t| !t.is_empty())
            .collect();
        Some(if text.is_empty() {
            "Request declined by the model (refusal)".to_string()
        } else {
            format!(
                "Request declined by the model (refusal): {}",
                text.join("\n\n")
            )
        })
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
            "response.refusal.delta" => {
                let Some(ev) = self.parse::<RefusalDelta>(event, data) else {
                    return Ok(Flow::Continue);
                };
                self.refusal_delta(ev.output_index, ev.delta, tx);
            }
            "response.refusal.done" => {
                let Some(ev) = self.parse::<RefusalDone>(event, data) else {
                    return Ok(Flow::Continue);
                };
                self.refusal_done(ev.output_index, ev.refusal, tx);
            }
            "response.content_part.done" => {
                let Some(ev) = self.parse::<ContentPartEvent>(event, data) else {
                    return Ok(Flow::Continue);
                };
                if ev.part.kind.as_deref() == Some("refusal") {
                    let text = ev.part.refusal.unwrap_or_default();
                    self.refusal_done(ev.output_index, text, tx);
                }
            }
            "response.output_item.added" => {
                let Some(ev) = self.parse::<OutputItemEvent>(event, data) else {
                    return Ok(Flow::Continue);
                };
                match ev.item.kind.as_deref() {
                    Some("function_call") => {
                        self.open_call(ev.output_index, &ev.item, tx);
                    }
                    Some("message") => self.message_id(ev.output_index, ev.item.id.as_deref()),
                    _ => {}
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
                        if slot.item_id.is_none() {
                            slot.item_id = ev.item.id.clone();
                        }
                        if let Some(args) = ev.item.arguments {
                            self.set_final_arguments(i, args, tx);
                        }
                        self.end_call(i, tx);
                    }
                    Some("reasoning") => self.reasoning_done(ev.output_index, ev.item, tx),
                    Some("message") => {
                        self.message_id(ev.output_index, ev.item.id.as_deref());
                        // Checked before any refusal below opens a text slot.
                        let streamed_text = self.text_slots.contains_key(&ev.output_index);
                        // A refusal part: authoritative even when nothing was
                        // streamed for it (`refusal_done` dedups streamed text).
                        let refusal: Option<String> = ev
                            .item
                            .content
                            .iter()
                            .filter(|p| p.kind.as_deref() == Some("refusal"))
                            .map(|p| p.refusal.clone().unwrap_or_default())
                            .reduce(|a, b| a + &b);
                        if let Some(refusal) = refusal {
                            self.refusal_done(ev.output_index, refusal, tx);
                        }
                        // A server that sent the text only in the finished item.
                        if streamed_text {
                            return Ok(Flow::Continue);
                        }
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
                        self.incomplete(resp.incomplete_details);
                    }
                }
                return Ok(Flow::Done);
            }
            // Terminal events other than `response.completed`. Without these
            // arms the loop never breaks, the body closes, and the resulting
            // StreamEnded is retryable (#83) — re-running an already-billed
            // generation that would fail the same way again.
            "response.incomplete" => {
                let resp = self
                    .parse::<ResponseEvent>(event, data)
                    .and_then(|e| e.response);
                let details = match resp {
                    Some(r) => {
                        if let Some(u) = r.usage {
                            self.usage = u.into_usage();
                        }
                        r.incomplete_details
                    }
                    None => None,
                };
                self.incomplete(details);
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

        self.record_following_ids();

        // A refusal (streamed, or in a finished item) is the most specific
        // verdict, as on the Anthropic provider: nothing overrides it.
        if !self.refusals.is_empty() {
            warn!("{}: the model declined the request (refusal)", self.label);
            self.stop_reason = StopReason::Refusal;
        }

        // Tool calls make this a ToolUse turn — unless the response was
        // incomplete (token limit), which is how a call ends up with unparsed
        // arguments, or refused. Length is kept so callers see it; the loop
        // still answers every tool call either way.
        let mut stop_reason = self.stop_reason;
        if !matches!(stop_reason, StopReason::Length | StopReason::Refusal)
            && !self.calls.is_empty()
        {
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

    /// Record why a response ended incomplete. `content_filter` is a refusal;
    /// every other reason (`max_output_tokens`, `max_messages`, `steered`, or
    /// none given) is reported as `Length`, as before.
    fn incomplete(&mut self, details: Option<IncompleteDetails>) {
        let reason = details.and_then(|d| d.reason);
        if reason.as_deref() == Some("content_filter") {
            warn!(
                "{}: response stopped by the content filter (incomplete_details.reason=content_filter)",
                self.label
            );
            self.filtered = reason;
            self.stop_reason = StopReason::Refusal;
        } else {
            self.stop_reason = StopReason::Length;
        }
    }

    /// Streamed refusal text: kept as the turn's text (the model's own
    /// explanation) and recorded for the stop reason.
    fn refusal_delta(
        &mut self,
        output_index: Option<usize>,
        delta: String,
        tx: &mpsc::UnboundedSender<StreamEvent>,
    ) {
        self.refusals
            .entry(output_index)
            .or_default()
            .push_str(&delta);
        self.push_text(output_index, delta, tx);
    }

    /// The complete refusal text. Emits only what the deltas have not already
    /// delivered, so a server that sends both — or `refusal.done` and then the
    /// same part again in `content_part.done` / `output_item.done` — does not
    /// duplicate it.
    fn refusal_done(
        &mut self,
        output_index: Option<usize>,
        refusal: String,
        tx: &mpsc::UnboundedSender<StreamEvent>,
    ) {
        let seen = self.refusals.entry(output_index).or_default();
        if *seen == refusal {
            return;
        }
        let Some(rest) = refusal.strip_prefix(seen.as_str()).map(str::to_string) else {
            debug!(
                "{}: final refusal text differs from the streamed deltas; keeping the streamed text",
                self.label
            );
            return;
        };
        *seen = refusal;
        if !rest.is_empty() {
            self.push_text(output_index, rest, tx);
        }
    }

    fn push_text(
        &mut self,
        output_index: Option<usize>,
        delta: String,
        tx: &mpsc::UnboundedSender<StreamEvent>,
    ) {
        let idx = self.text_slot(output_index);
        if let Some(Content::Text { text }) = self.content.get_mut(idx) {
            text.push_str(&delta);
        }
        let _ = tx.send(StreamEvent::TextDelta {
            content_index: idx,
            delta,
        });
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
                self.content.push(Content::thinking(String::new()));
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

    /// A finished reasoning item. Its `encrypted_content`, when present, is
    /// kept on the item's thinking block (the summary block if one was
    /// streamed, else the raw-text block, else a new block) so it can be
    /// replayed on the next request.
    fn reasoning_done(
        &mut self,
        output_index: Option<usize>,
        item: OutputItem,
        tx: &mpsc::UnboundedSender<StreamEvent>,
    ) {
        let existing = [ReasoningKind::Summary, ReasoningKind::Text]
            .iter()
            .find_map(|kind| self.thinking_slots.get(&(output_index, *kind)))
            .map(|(idx, _)| *idx);
        let summary_text: Vec<&str> = item
            .summary
            .iter()
            .filter_map(|p| p.get("text").and_then(serde_json::Value::as_str))
            .filter(|t| !t.is_empty())
            .collect();
        let encrypted = item.encrypted_content.filter(|e| !e.is_empty());
        let idx = match existing {
            Some(idx) => idx,
            // A server that sent the summary only in the finished item.
            None if !summary_text.is_empty() => {
                let text = summary_text.join("\n\n");
                self.content.push(Content::thinking(text.clone()));
                let idx = self.content.len() - 1;
                self.thinking_slots
                    .insert((output_index, ReasoningKind::Summary), (idx, None));
                let _ = tx.send(StreamEvent::ThinkingDelta {
                    content_index: idx,
                    delta: text,
                });
                idx
            }
            // Nothing streamed, no summary: a block only to carry the
            // encrypted reasoning.
            None if encrypted.is_some() => {
                self.content.push(Content::thinking(String::new()));
                self.content.len() - 1
            }
            None => {
                // Nothing to keep, but the items after it are its own.
                self.reasoning_bounds
                    .push((self.content.len(), output_index));
                return;
            }
        };
        self.reasoning_bounds.push((idx, output_index));
        let (Some(encrypted), Some(id)) = (encrypted, item.id) else {
            debug!(
                "{}: reasoning item without encrypted_content (or id); it will not be replayed",
                self.label
            );
            return;
        };
        let stored = serde_json::json!({
            "id": id,
            "summary": item.summary,
            "encrypted_content": encrypted,
        });
        if let Some(Content::Thinking {
            redacted,
            redacted_protocol,
            ..
        }) = self.content.get_mut(idx)
        {
            *redacted = Some(stored.to_string());
            *redacted_protocol = Some(self.protocol);
            if !self.reasoning_blocks.contains(&idx) {
                self.reasoning_blocks.push(idx);
                self.reasoning_blocks.sort_unstable();
            }
        }
    }

    fn message_id(&mut self, output_index: Option<usize>, id: Option<&str>) {
        if let Some(id) = id.filter(|id| !id.is_empty()) {
            self.message_ids.insert(output_index, id.to_string());
        }
    }

    /// Add to each stored reasoning item the ids of the output items that
    /// followed it (up to the next reasoning item of any kind — stored or
    /// not, finished or only streamed): `call_ids` for its function calls,
    /// `message_id` for its first message. Content order is arrival order,
    /// which follows `output_index`.
    ///
    /// The range must end at *every* reasoning item: items after one that is
    /// not replayed (no encrypted content) are paired with it, and filing
    /// them under an earlier item would replay them with ids but without
    /// their own reasoning item, which the API rejects.
    fn record_following_ids(&mut self) {
        let mut bounds = self.reasoning_bounds.clone();
        // Reasoning that streamed text but whose finished item never came.
        bounds.extend(
            self.thinking_slots
                .iter()
                .map(|((oi, _), (idx, _))| (*idx, *oi)),
        );
        for &r in &self.reasoning_blocks {
            // The item's own other block (raw text beside its summary) is
            // not a boundary.
            let own = bounds.iter().find(|(p, _)| *p == r).and_then(|(_, oi)| *oi);
            let end = bounds
                .iter()
                .filter(|(p, oi)| *p > r && (own.is_none() || *oi != own))
                .map(|(p, _)| *p)
                .min()
                .unwrap_or(self.content.len());
            let in_region = |i: usize| i > r && i < end;
            let call_ids: serde_json::Map<String, serde_json::Value> = self
                .calls
                .iter()
                .filter(|c| in_region(c.content_index) && !c.call_id.is_empty())
                .filter_map(|c| {
                    let id = c.item_id.as_deref().filter(|id| !id.is_empty())?;
                    Some((c.call_id.clone(), serde_json::Value::from(id)))
                })
                .collect();
            let message_id = self
                .text_slots
                .iter()
                .filter(|(_, &i)| in_region(i))
                .filter_map(|(oi, &i)| self.message_ids.get(oi).map(|id| (i, id)))
                .min_by_key(|(i, _)| *i)
                .map(|(_, id)| id.clone());
            if call_ids.is_empty() && message_id.is_none() {
                continue;
            }
            let Some(Content::Thinking {
                redacted: Some(stored),
                ..
            }) = self.content.get_mut(r)
            else {
                continue;
            };
            let Ok(serde_json::Value::Object(mut item)) = serde_json::from_str(stored) else {
                continue;
            };
            if !call_ids.is_empty() {
                item.insert(CALL_IDS_KEY.into(), serde_json::Value::Object(call_ids));
            }
            if let Some(id) = message_id {
                item.insert(MESSAGE_ID_KEY.into(), serde_json::Value::from(id));
            }
            *stored = serde_json::Value::Object(item).to_string();
        }
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
struct RefusalDelta {
    delta: String,
    #[serde(default)]
    output_index: Option<usize>,
}

#[derive(Deserialize)]
struct RefusalDone {
    #[serde(default)]
    refusal: String,
    #[serde(default)]
    output_index: Option<usize>,
}

#[derive(Deserialize)]
struct ContentPartEvent {
    #[serde(default)]
    output_index: Option<usize>,
    part: OutputPart,
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
    #[serde(default, deserialize_with = "null_as_default")]
    content: Vec<OutputPart>,
    /// `reasoning` items: `{"type": "summary_text", "text"}` parts, kept as
    /// sent for replay.
    #[serde(default, deserialize_with = "null_as_default")]
    summary: Vec<serde_json::Value>,
    /// `reasoning` items: the encrypted reasoning, when requested.
    #[serde(default)]
    encrypted_content: Option<String>,
}

/// A field sent as explicit `null` reads as its default, like a missing one.
fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Deserialize)]
struct OutputPart {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    text: Option<String>,
    /// `refusal` parts.
    #[serde(default)]
    refusal: Option<String>,
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
    #[serde(default)]
    incomplete_details: Option<IncompleteDetails>,
}

#[derive(Deserialize)]
struct IncompleteDetails {
    #[serde(default)]
    reason: Option<String>,
}

// Counts use `null_as_zero`: `#[serde(default)]` covers a missing key only,
// and one explicit `null` would otherwise fail the whole terminal event.
#[derive(Deserialize)]
struct ResponseUsage {
    #[serde(default, deserialize_with = "null_as_zero")]
    input_tokens: u64,
    #[serde(default, deserialize_with = "null_as_zero")]
    output_tokens: u64,
    #[serde(default, deserialize_with = "null_as_zero")]
    total_tokens: u64,
    #[serde(default)]
    input_tokens_details: Option<InputTokensDetails>,
}

#[derive(Deserialize, Default)]
struct InputTokensDetails {
    #[serde(default, deserialize_with = "null_as_zero")]
    cached_tokens: u64,
    #[serde(default, deserialize_with = "null_as_zero")]
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
    fn usage_tolerates_explicit_nulls() {
        let u: ResponseUsage = serde_json::from_str(
            r#"{"input_tokens":10,"input_tokens_details":{"cached_tokens":4,"cache_write_tokens":null},
                "output_tokens":null,"output_tokens_details":null,"total_tokens":null}"#,
        )
        .unwrap();
        let u = u.into_usage();
        assert_eq!((u.input, u.cache_read, u.cache_write), (6, 4, 0));
        assert_eq!((u.output, u.total_tokens), (0, 0));
        let u: ResponseUsage =
            serde_json::from_str(r#"{"input_tokens":3,"input_tokens_details":null}"#).unwrap();
        assert_eq!(u.into_usage().input, 3);
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
        let mut s = ResponsesStreamState::new("test", ApiProtocol::OpenAiResponses);
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
