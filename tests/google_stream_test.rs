//! Behavioral tests for `GoogleProvider` against a local mock server.
//!
//! These give CI coverage for the PR #32 regression scenarios (Gemini
//! thought-signature round-trip and multi-turn function calling), previously
//! exercised only by the key-gated `integration_gemini.rs` live tests
//! (issue #33).

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

    let Message::Assistant { usage, .. } = &message else {
        unreachable!()
    };
    assert_eq!((usage.input, usage.output, usage.total_tokens), (10, 5, 15));
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
        Message::assistant(
            vec![Content::tool_call_with_metadata(
                "google-fc-0",
                "get_weather",
                serde_json::json!({"city": "Paris"}),
                serde_json::json!({"thought_signature": "sig-abc"}),
            )],
            StopReason::ToolUse,
            MODEL,
            "google",
            Usage::default(),
        ),
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
    // Structural: the signature must sit on the functionCall part of the
    // assistant turn (contents[1]), not merely appear somewhere in the body.
    assert_eq!(
        body["contents"][1]["parts"][0]["thoughtSignature"], "sig-abc",
        "thought signature must be echoed on the functionCall part, body: {body}"
    );
    let raw = serde_json::to_string(&body).unwrap();
    assert!(
        !raw.contains("google-fc-0"),
        "synthetic tool-call id must not be sent to Gemini, body: {raw}"
    );
}

/// A part carrying BOTH empty text and a functionCall must still produce the
/// tool call (the old loop `continue`d past it while Gemini was thinking).
#[tokio::test]
async fn empty_text_part_does_not_swallow_function_call() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/v1beta/models/{}:streamGenerateContent",
            MODEL
        )))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            sse(&[
                r#"{"candidates":[{"content":{"parts":[{"text":"","functionCall":{"name":"get_weather","args":{"city":"Oslo"}}}],"role":"model"},"finishReason":"STOP","index":0}]}"#,
            ]),
            "text/event-stream",
        ))
        .mount(&server)
        .await;

    let message = run_stream(stream_config(&server.uri(), vec![Message::user("hi")])).await;

    let Message::Assistant {
        content,
        stop_reason,
        ..
    } = &message
    else {
        panic!("expected assistant message");
    };
    assert_eq!(*stop_reason, StopReason::ToolUse);
    assert!(
        matches!(content.first(), Some(Content::ToolCall { name, .. }) if name == "get_weather"),
        "functionCall in an empty-text part must not be dropped, got {content:?}"
    );
}

/// Text deltas across multiple SSE events accumulate into ONE Content::Text.
#[tokio::test]
async fn text_deltas_accumulate_across_events() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/v1beta/models/{}:streamGenerateContent",
            MODEL
        )))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            sse(&[
                r#"{"candidates":[{"content":{"parts":[{"text":"Hello, "}],"role":"model"},"index":0}]}"#,
                r#"{"candidates":[{"content":{"parts":[{"text":"world!"}],"role":"model"},"finishReason":"STOP","index":0}]}"#,
            ]),
            "text/event-stream",
        ))
        .mount(&server)
        .await;

    let message = run_stream(stream_config(&server.uri(), vec![Message::user("hi")])).await;

    let Message::Assistant { content, .. } = &message else {
        panic!("expected assistant message");
    };
    assert_eq!(content.len(), 1, "deltas must merge into one text block");
    assert!(matches!(content.first(), Some(Content::Text { text }) if text == "Hello, world!"));
}

/// A mid-stream {"error": ...} payload must fail the stream, not vanish
/// into an empty chunk and a fake successful turn.
#[tokio::test]
async fn in_stream_error_payload_fails_the_stream() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/v1beta/models/{}:streamGenerateContent",
            MODEL
        )))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            sse(&[
                r#"{"error":{"code":429,"message":"Resource has been exhausted","status":"RESOURCE_EXHAUSTED"}}"#,
            ]),
            "text/event-stream",
        ))
        .mount(&server)
        .await;

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let result = yoagent::provider::GoogleProvider
        .stream(
            stream_config(&server.uri(), vec![Message::user("hi")]),
            tx,
            CancellationToken::new(),
        )
        .await;

    let err = result.expect_err("in-stream error must surface as Err");
    assert!(
        err.to_string().contains("RESOURCE_EXHAUSTED"),
        "error should carry the provider payload, got: {err}"
    );
}

/// A SAFETY finish reason maps to StopReason::Refusal with an explanation.
#[tokio::test]
async fn safety_finish_reason_maps_to_refusal() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/v1beta/models/{}:streamGenerateContent",
            MODEL
        )))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            sse(&[
                r#"{"candidates":[{"content":{"parts":[],"role":"model"},"finishReason":"SAFETY","index":0}]}"#,
            ]),
            "text/event-stream",
        ))
        .mount(&server)
        .await;

    let message = run_stream(stream_config(&server.uri(), vec![Message::user("hi")])).await;

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
        error_message.as_deref().unwrap_or("").contains("SAFETY"),
        "error_message should explain the block, got {error_message:?}"
    );
}
