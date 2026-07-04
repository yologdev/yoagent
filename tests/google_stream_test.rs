//! Behavioral tests for `GoogleProvider` against a local mock server.
//!
//! These port the PR #32 regression scenarios (Gemini thought-signature
//! round-trip and multi-turn function calling) from the key-gated
//! `integration_gemini.rs` tests to wiremock so they run in CI (issue #33).

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use yoagent::provider::{GoogleProvider, ModelConfig, StreamConfig, StreamProvider};
use yoagent::types::*;

const MODEL: &str = "gemini-2.5-flash";

fn sse(events: &[&str]) -> String {
    events
        .iter()
        .map(|data| format!("data: {}\r\n\r\n", data))
        .collect()
}

fn stream_config(base_url: &str, messages: Vec<Message>) -> StreamConfig {
    let mut mc = ModelConfig::google(MODEL, "Gemini 2.5 Flash");
    mc.base_url = base_url.to_string();
    StreamConfig {
        model: MODEL.into(),
        system_prompt: "test".into(),
        messages,
        tools: vec![],
        thinking_level: ThinkingLevel::Off,
        api_key: "test-key".into(),
        max_tokens: Some(256),
        temperature: None,
        model_config: Some(mc),
        cache_config: CacheConfig::default(),
    }
}

async fn run_stream(config: StreamConfig) -> Message {
    let (tx, _rx) = mpsc::unbounded_channel();
    GoogleProvider
        .stream(config, tx, CancellationToken::new())
        .await
        .expect("stream should succeed")
}

/// A streamed function call with a thoughtSignature must surface as a
/// ToolCall with the signature preserved in provider_metadata and a
/// synthetic id when Gemini sends none.
#[tokio::test]
async fn function_call_with_thought_signature_is_captured() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/v1beta/models/{}:streamGenerateContent",
            MODEL
        )))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            sse(&[
                r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"get_weather","args":{"city":"Paris"}},"thoughtSignature":"sig-abc"}],"role":"model"},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"totalTokenCount":15}}"#,
            ]),
            "text/event-stream",
        ))
        .mount(&server)
        .await;

    let message = run_stream(stream_config(
        &server.uri(),
        vec![Message::user("weather?")],
    ))
    .await;

    let Message::Assistant {
        content,
        stop_reason,
        ..
    } = &message
    else {
        panic!("expected assistant message");
    };
    assert_eq!(*stop_reason, StopReason::ToolUse);

    let Some(Content::ToolCall {
        id,
        name,
        arguments,
        provider_metadata,
        ..
    }) = content.first()
    else {
        panic!("expected a tool call, got {content:?}");
    };
    assert_eq!(name, "get_weather");
    assert_eq!(arguments["city"], "Paris");
    assert_eq!(id, "google-fc-0", "missing id must be synthesized");
    assert_eq!(
        provider_metadata
            .as_ref()
            .and_then(|m| m["thought_signature"].as_str()),
        Some("sig-abc"),
        "thought signature must be preserved in provider_metadata"
    );
}

/// Multi-turn: when the history contains a prior tool call carrying a
/// thought signature, the next request must echo the signature back to
/// Gemini and must NOT leak the synthetic `google-fc-` id.
#[tokio::test]
async fn thought_signature_round_trips_and_synthetic_id_is_stripped() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/v1beta/models/{}:streamGenerateContent",
            MODEL
        )))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            sse(&[
                r#"{"candidates":[{"content":{"parts":[{"text":"It is 22C in Paris."}],"role":"model"},"finishReason":"STOP","index":0}]}"#,
            ]),
            "text/event-stream",
        ))
        .expect(1)
        .mount(&server)
        .await;

    let history = vec![
        Message::user("weather in Paris?"),
        Message::Assistant {
            content: vec![Content::tool_call_with_metadata(
                "google-fc-0",
                "get_weather",
                serde_json::json!({"city": "Paris"}),
                serde_json::json!({"thought_signature": "sig-abc"}),
            )],
            stop_reason: StopReason::ToolUse,
            model: MODEL.into(),
            provider: "google".into(),
            usage: Usage::default(),
            timestamp: 1,
            error_message: None,
        },
        Message::ToolResult {
            tool_call_id: "google-fc-0".into(),
            tool_name: "get_weather".into(),
            content: vec![Content::Text { text: "22C".into() }],
            is_error: false,
            timestamp: 2,
        },
    ];

    let message = run_stream(stream_config(&server.uri(), history)).await;

    // The follow-up turn parses normally
    let Message::Assistant { content, .. } = &message else {
        panic!("expected assistant message");
    };
    assert!(matches!(
        content.first(),
        Some(Content::Text { text }) if text.contains("22C")
    ));

    // Inspect the actual request body sent to the gateway
    let requests = server.received_requests().await.expect("recording enabled");
    assert_eq!(requests.len(), 1);
    let body: serde_json::Value = requests[0].body_json().expect("json body");
    let raw = serde_json::to_string(&body).unwrap();
    assert!(
        raw.contains("sig-abc"),
        "thought signature must be echoed back to Gemini, body: {raw}"
    );
    assert!(
        !raw.contains("google-fc-0"),
        "synthetic tool-call id must not be sent to Gemini, body: {raw}"
    );
}
