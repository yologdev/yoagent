//! Streaming tests for `AnthropicProvider` against a local mock server.
//!
//! These cover the response-parsing and auth behavior that unit tests on
//! `build_request_body` can't reach: stop-reason mapping from SSE events and
//! the request headers actually sent on the wire.

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};
use yoagent::provider::{
    AnthropicCompat, AnthropicProvider, ModelConfig, StreamConfig, StreamProvider,
};
use yoagent::types::*;

/// Matcher: the request must NOT carry the given header.
struct HeaderAbsent(&'static str);

impl wiremock::Match for HeaderAbsent {
    fn matches(&self, request: &Request) -> bool {
        !request.headers.contains_key(self.0)
    }
}

/// Canned SSE body for a stream that ends with the given stop_reason and no
/// content blocks (the shape of a pre-output refusal).
fn sse_empty_with_stop(stop_reason: &str) -> String {
    format!(
        "event: message_start\n\
         data: {{\"type\":\"message_start\",\"message\":{{\"usage\":{{\"input_tokens\":10,\"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":0}}}}}}\n\n\
         event: message_delta\n\
         data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"{stop_reason}\"}},\"usage\":{{\"output_tokens\":0}}}}\n\n\
         event: message_stop\n\
         data: {{\"type\":\"message_stop\"}}\n\n"
    )
}

fn stream_config(base_url: &str, anthropic: Option<AnthropicCompat>) -> StreamConfig {
    let mut mc = ModelConfig::anthropic("claude-sonnet-5", "Claude Sonnet 5");
    mc.base_url = base_url.to_string();
    mc.anthropic = anthropic;
    StreamConfig {
        model: "claude-sonnet-5".into(),
        system_prompt: "test".into(),
        messages: vec![Message::user("hi")],
        tools: vec![],
        thinking_level: ThinkingLevel::Off,
        api_key: "test-key".into(),
        max_tokens: Some(256),
        temperature: None,
        model_config: Some(mc),
        cache_config: CacheConfig::default(),
    }
}

async fn run_stream(config: StreamConfig) -> Result<Message, yoagent::provider::ProviderError> {
    let (tx, _rx) = mpsc::unbounded_channel();
    AnthropicProvider
        .stream(config, tx, CancellationToken::new())
        .await
}

#[tokio::test]
async fn refusal_stop_reason_maps_to_refusal_with_error_message() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(sse_empty_with_stop("refusal"), "text/event-stream"),
        )
        .mount(&server)
        .await;

    let message = run_stream(stream_config(&server.uri(), None))
        .await
        .expect("stream should succeed");

    let Message::Assistant {
        stop_reason,
        error_message,
        ..
    } = &message
    else {
        panic!("expected assistant message");
    };
    assert_eq!(*stop_reason, StopReason::Refusal);
    assert!(
        error_message.as_deref().unwrap_or("").contains("refusal"),
        "error_message should explain the refusal, got {error_message:?}"
    );
}

#[tokio::test]
async fn context_window_exceeded_maps_to_overflow_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            sse_empty_with_stop("model_context_window_exceeded"),
            "text/event-stream",
        ))
        .mount(&server)
        .await;

    let message = run_stream(stream_config(&server.uri(), None))
        .await
        .expect("stream should succeed");

    assert!(
        message.is_context_overflow(),
        "in-stream overflow must trigger the documented recovery hook"
    );
}

#[tokio::test]
async fn bearer_auth_sends_authorization_and_no_x_api_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .and(header("authorization", "Bearer test-key"))
        .and(HeaderAbsent("x-api-key"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(sse_empty_with_stop("end_turn"), "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let config = stream_config(
        &server.uri(),
        Some(AnthropicCompat {
            adaptive_thinking: true,
            bearer_auth: true,
        }),
    );
    run_stream(config).await.expect("stream should succeed");
    // Mock expectation (`expect(1)`) verifies the headers on drop.
}

#[tokio::test]
async fn default_auth_sends_x_api_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .and(header("x-api-key", "test-key"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(sse_empty_with_stop("end_turn"), "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;

    run_stream(stream_config(&server.uri(), None))
        .await
        .expect("stream should succeed");
}

#[tokio::test]
async fn user_authorization_header_suppresses_x_api_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .and(header("authorization", "Bearer custom-token"))
        .and(HeaderAbsent("x-api-key"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(sse_empty_with_stop("end_turn"), "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let mut config = stream_config(&server.uri(), None);
    if let Some(mc) = &mut config.model_config {
        mc.headers
            .insert("Authorization".into(), "Bearer custom-token".into());
    }
    run_stream(config).await.expect("stream should succeed");
}

#[tokio::test]
async fn rate_limit_carries_retry_after_from_header() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "7")
                .set_body_string(
                    r#"{"type":"error","error":{"type":"rate_limit_error","message":"rate limited"}}"#,
                ),
        )
        .mount(&server)
        .await;

    let err = run_stream(stream_config(&server.uri(), None))
        .await
        .expect_err("429 must surface as an error");

    match err {
        yoagent::provider::ProviderError::RateLimited { retry_after_ms } => {
            assert_eq!(retry_after_ms, Some(7000));
        }
        other => panic!("expected RateLimited, got: {:?}", other),
    }
}
