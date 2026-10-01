//! Shared SSE (Server-Sent Events) parsing utilities.
//!
//! Factored out from the Anthropic provider so all providers can reuse
//! the same streaming infrastructure.

use futures::StreamExt;
use reqwest_eventsource::{Event, EventSource};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::debug;

/// Open an SSE stream for a provider request.
///
/// The stream's own reconnect is disabled (`retry::Never`). Providers already
/// stop at the first stream error and leave retries to the agent loop, so the
/// library's reconnect never took effect — it only scheduled a timer. That
/// timer reads `std::time::Instant`, which panics on wasm32. A reconnect
/// would also re-send a completion request, which must not happen behind the
/// loop's retry accounting.
pub(crate) fn open_event_source(
    request: reqwest::RequestBuilder,
) -> Result<EventSource, crate::provider::ProviderError> {
    let mut es = EventSource::new(request)
        .map_err(|e| crate::provider::ProviderError::Network(e.to_string()))?;
    es.set_retry_policy(Box::new(reqwest_eventsource::retry::Never));
    Ok(es)
}

/// A parsed SSE event with event type and data.
#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event: String,
    pub data: String,
}

/// Drives an EventSource, sending parsed events through a channel.
/// Returns when the stream ends, errors, or is cancelled.
///
/// The caller receives `SseEvent`s and can parse them according to
/// provider-specific formats.
pub async fn drive_sse(
    mut es: EventSource,
    tx: mpsc::UnboundedSender<SseEvent>,
    cancel: CancellationToken,
) -> Result<(), String> {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                es.close();
                return Err("cancelled".into());
            }
            event = es.next() => {
                match event {
                    None => return Ok(()),
                    Some(Ok(Event::Open)) => {
                        debug!("SSE connection opened");
                    }
                    Some(Ok(Event::Message(msg))) => {
                        if tx.send(SseEvent {
                            event: msg.event,
                            data: msg.data,
                        }).is_err() {
                            es.close();
                            return Ok(());
                        }
                    }
                    Some(Err(e)) => {
                        es.close();
                        return Err(e.to_string());
                    }
                }
            }
        }
    }
}
