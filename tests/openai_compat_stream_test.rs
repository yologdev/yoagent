//! Streaming tests for `OpenAiCompatProvider` against a local mock server.
//!
//! Covers the DONE-less close behavior (issue #76): some providers (MiniMax
//! confirmed in the field) close the SSE connection without the
//! OpenAI-standard `data: [DONE]` terminator, which surfaces as
//! `reqwest_eventsource::Error::StreamEnded`. After a `finish_reason` that
//! close is a completed response and must finish cleanly; before any
//! `finish_reason` it is genuine truncation and must stay an error.

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use yoagent::provider::{ModelConfig, OpenAiCompatProvider, StreamConfig, StreamProvider};
use yoagent::types::*;

fn chunk(json: &str) -> String {
    format!("data: {json}\n\n")
}

fn stream_config(base_url: &str) -> StreamConfig {
    let mut mc = ModelConfig::minimax("MiniMax-M2.7", "MiniMax M2.7");
    mc.base_url = base_url.to_string();
    let mut config = StreamConfig::new("MiniMax-M2.7", "test-key");
    config.system_prompt = "test".into();
    config.messages = vec![Message::user("hi")];
    config.max_tokens = Some(256);
    config.model_config = Some(mc);
    config
}

async fn run_stream(config: StreamConfig) -> Result<Message, yoagent::provider::ProviderError> {
    let (tx, _rx) = mpsc::unbounded_channel();
    OpenAiCompatProvider
        .stream(config, tx, CancellationToken::new())
        .await
}

/// DONE-less close AFTER finish_reason (MiniMax's normal ending): the
/// response is complete — clean finish with accumulated content, no error.
#[tokio::test]
async fn test_stream_ended_after_finish_reason_is_clean_eof() {
    let server = MockServer::start().await;
    let body = [
        chunk(r#"{"choices":[{"delta":{"content":"Hello"},"index":0}]}"#),
        chunk(r#"{"choices":[{"delta":{"content":" world"},"index":0}]}"#),
        chunk(r#"{"choices":[{"delta":{},"finish_reason":"stop","index":0}]}"#),
        // No `data: [DONE]` — the body just ends.
    ]
    .concat();
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(body, "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let msg = run_stream(stream_config(&server.uri()))
        .await
        .expect("DONE-less close after finish_reason must not be an error");

    let Message::Assistant {
        content,
        stop_reason,
        ..
    } = &msg
    else {
        panic!("expected assistant message");
    };
    assert_eq!(*stop_reason, StopReason::Stop);
    assert!(
        matches!(&content[0], Content::Text { text } if text == "Hello world"),
        "accumulated content must survive the DONE-less close: {content:?}"
    );
}

/// DONE-less close BEFORE any finish_reason: genuine mid-stream truncation —
/// must remain an error (retry semantics stay honest for real drops).
#[tokio::test]
async fn test_stream_ended_without_finish_reason_is_error() {
    let server = MockServer::start().await;
    let body = chunk(r#"{"choices":[{"delta":{"content":"Hel"},"index":0}]}"#);
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(body, "text/event-stream"),
        )
        .mount(&server)
        .await;

    let result = run_stream(stream_config(&server.uri())).await;
    assert!(
        result.is_err(),
        "truncation before finish_reason must stay an error, got {result:?}"
    );
}
