//! Behavioral tests for `GoogleProvider` against a local mock server.
//!
//! These give CI coverage for the PR #32 regression scenarios (Gemini
//! thought-signature round-trip and multi-turn function calling), previously
//! exercised only by the key-gated `integration_gemini.rs` live tests
//! (issue #33).

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{header, method, path, query_param_is_missing};
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
    let mut config = StreamConfig::new(MODEL, "test-key");
    config.system_prompt = "test".into();
    config.messages = messages;
    config.max_tokens = Some(256);
    config.model_config = Some(mc);
    config
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

/// Gemini's promptTokenCount INCLUDES cachedContentTokenCount; the mapping
/// must keep `input` as the uncached remainder so `input + cache_read`
/// doesn't double-count cached tokens.
#[tokio::test]
async fn cached_tokens_are_not_double_counted_in_usage() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/v1beta/models/{}:streamGenerateContent",
            MODEL
        )))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            sse(&[
                r#"{"candidates":[{"content":{"parts":[{"text":"hi"}],"role":"model"},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":100,"cachedContentTokenCount":80,"candidatesTokenCount":5,"totalTokenCount":105}}"#,
            ]),
            "text/event-stream",
        ))
        .mount(&server)
        .await;

    let message = run_stream(stream_config(&server.uri(), vec![Message::user("hi")])).await;

    let Message::Assistant { usage, .. } = &message else {
        panic!("expected assistant message");
    };
    assert_eq!(usage.input, 20, "input must exclude cached tokens");
    assert_eq!(usage.cache_read, 80);
    assert_eq!(usage.output, 5);
    assert_eq!(usage.input + usage.cache_read + usage.output, 105);
}

/// Thought-summary parts (thinkingConfig.includeThoughts) must stream as
/// ThinkingDelta and land as Content::Thinking — separate from the answer.
#[tokio::test]
async fn thought_parts_map_to_thinking_content() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/v1beta/models/{}:streamGenerateContent",
            MODEL
        )))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            sse(&[
                r#"{"candidates":[{"content":{"parts":[{"text":"Considering the options...","thought":true}],"role":"model"},"index":0}]}"#,
                r#"{"candidates":[{"content":{"parts":[{"text":"The answer is 4."}],"role":"model"},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"totalTokenCount":15}}"#,
            ]),
            "text/event-stream",
        ))
        .mount(&server)
        .await;

    let message = run_stream(stream_config(&server.uri(), vec![Message::user("2+2?")])).await;

    let Message::Assistant { content, .. } = &message else {
        panic!("expected assistant message");
    };
    let thinking = content
        .iter()
        .find_map(|c| match c {
            Content::Thinking { thinking, .. } => Some(thinking.clone()),
            _ => None,
        })
        .expect("thought part must become Thinking content");
    assert!(thinking.contains("Considering the options"));
    let text = content
        .iter()
        .find_map(|c| match c {
            Content::Text { text } => Some(text.clone()),
            _ => None,
        })
        .expect("answer text");
    assert_eq!(text, "The answer is 4.");
}

/// The API key travels in `x-goog-api-key`, never in the URL, where reqwest's
/// error text (logs, retry events, transcripts) would carry it.
#[tokio::test]
async fn the_api_key_is_sent_as_a_header_not_in_the_url() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/v1beta/models/{}:streamGenerateContent",
            MODEL
        )))
        .and(header("x-goog-api-key", "test-key"))
        .and(query_param_is_missing("key"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            sse(&[
                r#"{"candidates":[{"content":{"parts":[{"text":"hi"}],"role":"model"},"finishReason":"STOP","index":0}]}"#,
            ]),
            "text/event-stream",
        ))
        .expect(1)
        .mount(&server)
        .await;

    run_stream(stream_config(&server.uri(), vec![Message::user("hi")])).await;
}

/// A transport failure's error text does not contain the key.
#[tokio::test]
async fn a_network_error_does_not_leak_the_key() {
    // Bind a port and release it, so nothing listens there: the connection
    // is refused.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let config = stream_config(
        &format!("http://127.0.0.1:{port}"),
        vec![Message::user("hi")],
    );
    let (tx, _rx) = mpsc::unbounded_channel();
    let err = GoogleProvider
        .stream(config, tx, CancellationToken::new())
        .await
        .expect_err("nothing is listening");
    assert!(!err.to_string().contains("test-key"), "{err}");
}

fn ok_body() -> String {
    sse(&[
        r#"{"candidates":[{"content":{"parts":[{"text":"hi"}],"role":"model"},"finishReason":"STOP","index":0}]}"#,
    ])
}

/// A key read from a CRLF `.env` keeps working: it is trimmed before it
/// becomes a header (the URL parser used to strip the CR/LF).
#[tokio::test]
async fn a_key_with_a_trailing_newline_is_trimmed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header("x-goog-api-key", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(ok_body(), "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;

    let mut config = stream_config(&server.uri(), vec![Message::user("hi")]);
    config.api_key = "test-key\r\n".into();
    run_stream(config).await;
}

/// A caller that authenticates through `ModelConfig.headers` gets exactly
/// its own header — the built-in one is not appended next to it.
#[tokio::test]
async fn a_key_in_the_custom_headers_is_sent_once() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(|req: &wiremock::Request| {
            let values: Vec<_> = req.headers.get_all("x-goog-api-key").iter().collect();
            values.len() == 1 && values[0] == "from-headers"
        })
        .respond_with(ResponseTemplate::new(200).set_body_raw(ok_body(), "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;

    let mut config = stream_config(&server.uri(), vec![Message::user("hi")]);
    config.api_key = "from-config".into();
    if let Some(mc) = config.model_config.as_mut() {
        mc.headers
            .insert("X-Goog-Api-Key".into(), "from-headers".into());
    }
    run_stream(config).await;
}

/// A key that cannot be a header value fails before sending, and says so
/// without echoing the key.
#[tokio::test]
async fn an_invalid_key_is_an_auth_error_and_nothing_is_sent() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let mut config = stream_config(&server.uri(), vec![Message::user("hi")]);
    config.api_key = "bad\u{1}key".into();
    let (tx, _rx) = mpsc::unbounded_channel();
    let err = GoogleProvider
        .stream(config, tx, CancellationToken::new())
        .await
        .expect_err("an invalid header value must be refused");
    assert!(
        matches!(err, yoagent::provider::ProviderError::Auth(_)),
        "{err:?}"
    );
    assert!(err.to_string().contains("GEMINI_API_KEY"), "{err}");
    assert!(!err.to_string().contains("bad"), "{err}");
}

/// An `Authorization` header (a proxy's own, say) does not stop the key from
/// being sent: only a caller-supplied `x-goog-api-key` replaces it.
#[tokio::test]
async fn an_authorization_header_keeps_the_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header("x-goog-api-key", "test-key"))
        .and(header("authorization", "Bearer proxy"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(ok_body(), "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;

    let mut config = stream_config(&server.uri(), vec![Message::user("hi")]);
    if let Some(mc) = config.model_config.as_mut() {
        mc.headers
            .insert("Authorization".into(), "Bearer proxy".into());
    }
    run_stream(config).await;
}

async fn gemini_result(body: String) -> Result<Message, yoagent::provider::ProviderError> {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(&server)
        .await;
    let (tx, _rx) = mpsc::unbounded_channel();
    GoogleProvider
        .stream(
            stream_config(&server.uri(), vec![Message::user("hi")]),
            tx,
            CancellationToken::new(),
        )
        .await
}

/// A stream cut off before any `finishReason` is a retryable error, never an
/// `Ok` with whatever text arrived.
#[tokio::test]
async fn a_stream_without_a_finish_reason_is_a_retryable_error() {
    let err = gemini_result(sse(&[
        r#"{"candidates":[{"content":{"parts":[{"text":"Partial"}],"role":"model"},"index":0}]}"#,
    ]))
    .await
    .expect_err("a cut-off stream is not a finished answer");
    assert!(
        matches!(err, yoagent::provider::ProviderError::Network(_)),
        "{err:?}"
    );
}

/// A blocked prompt (`promptFeedback.blockReason`, no candidates) is a
/// refusal that says why — it used to be an empty `Ok`/`Stop`.
#[tokio::test]
async fn a_blocked_prompt_is_a_refusal() {
    let msg = gemini_result(sse(&[
        r#"{"promptFeedback":{"blockReason":"PROHIBITED_CONTENT"},"usageMetadata":{"promptTokenCount":5,"totalTokenCount":5}}"#,
    ]))
    .await
    .unwrap();
    let Message::Assistant {
        stop_reason,
        error_message,
        ..
    } = &msg
    else {
        panic!("expected assistant message")
    };
    assert_eq!(*stop_reason, StopReason::Refusal);
    assert!(
        error_message
            .as_deref()
            .is_some_and(|m| m.contains("PROHIBITED_CONTENT")),
        "{error_message:?}"
    );
}

/// `MALFORMED_FUNCTION_CALL` is an error with a message, not a normal stop.
#[tokio::test]
async fn a_malformed_function_call_is_an_error() {
    let msg = gemini_result(sse(&[
        r#"{"candidates":[{"content":{"role":"model"},"finishReason":"MALFORMED_FUNCTION_CALL","index":0}]}"#,
    ]))
    .await
    .unwrap();
    let Message::Assistant {
        stop_reason,
        error_message,
        ..
    } = &msg
    else {
        panic!("expected assistant message")
    };
    assert_eq!(*stop_reason, StopReason::Error);
    assert!(error_message.is_some());
}

/// Vertex shares Gemini's parser: a cut-off stream and an in-stream error
/// are errors there too (its old copy returned both as `Ok`).
#[tokio::test]
async fn vertex_uses_the_gemini_parser() {
    use yoagent::provider::GoogleVertexProvider;
    for (body, what) in [
        (
            sse(&[
                r#"{"candidates":[{"content":{"parts":[{"text":"Part"}],"role":"model"},"index":0}]}"#,
            ]),
            "cut off",
        ),
        (
            sse(&[r#"{"error":{"code":500,"message":"backend error","status":"INTERNAL"}}"#]),
            "in-stream error",
        ),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
            .mount(&server)
            .await;
        let mut mc = ModelConfig::google(MODEL, "Gemini");
        mc.base_url = server.uri();
        let mut config = StreamConfig::new(MODEL, "token");
        config.messages = vec![Message::user("hi")];
        config.model_config = Some(mc);
        let (tx, _rx) = mpsc::unbounded_channel();
        let result = GoogleVertexProvider
            .stream(config, tx, CancellationToken::new())
            .await;
        assert!(result.is_err(), "{what}: {result:?}");
    }
}
