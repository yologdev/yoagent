//! Opening a provider's SSE (Server-Sent Events) stream, shared by every
//! SSE provider. Crate-private: each provider reads the stream itself.

use reqwest_eventsource::EventSource;

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

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A stream that ends or fails must not be re-requested behind the agent
    /// loop's back: with the library's default policy, the server below sees
    /// repeated re-sends of the same completion within a second.
    #[tokio::test]
    async fn a_failed_stream_is_never_reopened() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                // `set_body_raw`, not `set_body_string`: the latter makes it
                // text/plain, which the event source refuses without retrying.
                ResponseTemplate::new(200).set_body_raw("data: one\n\n", "text/event-stream"),
            )
            .mount(&server)
            .await;
        let request = reqwest::Client::new().post(server.uri());
        let mut es = open_event_source(request).unwrap();
        let mut saw_data = false;
        let drained = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            // Consume everything, errors included, until the source gives up.
            while let Some(event) = es.next().await {
                if let Ok(reqwest_eventsource::Event::Message(m)) = event {
                    saw_data |= m.data == "one";
                }
            }
        })
        .await;
        assert!(saw_data, "the stream was not read (wrong content type?)");
        assert!(drained.is_ok(), "the event source kept reconnecting");
        let requests = server.received_requests().await.unwrap_or_default();
        assert_eq!(requests.len(), 1, "the completion was re-sent");
    }
}
