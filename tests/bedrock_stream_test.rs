//! Streaming tests for `BedrockProvider` against a local mock server (#174).
//!
//! ConverseStream answers with binary `application/vnd.amazon.eventstream`
//! frames. These tests build real frames — prelude, headers, JSON payload and
//! both CRC-32s — shaped after the AWS Bedrock Runtime API reference, and
//! serve them over HTTP. They are mocks of the documented format, not
//! recordings of a live endpoint.
//!
//! The frame encoder and CRC below are written independently of the crate's
//! decoder, so the two cross-check each other.

use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use yoagent::agent::Agent;
use yoagent::provider::{
    ApiProtocol, BedrockProvider, ModelConfig, ProviderError, StreamConfig, StreamEvent,
    StreamProvider,
};
use yoagent::*;

const MODEL: &str = "anthropic.claude-test";
const EVENTSTREAM: &str = "application/vnd.amazon.eventstream";

// ---------------------------------------------------------------------------
// Frame encoding
// ---------------------------------------------------------------------------

/// Bitwise CRC-32 (IEEE, reflected 0xEDB88320) — deliberately not the
/// crate's table-driven version.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// One frame with string-typed headers.
fn frame(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
    let mut h = Vec::new();
    for (name, value) in headers {
        h.push(name.len() as u8);
        h.extend_from_slice(name.as_bytes());
        h.push(7);
        h.extend_from_slice(&(value.len() as u16).to_be_bytes());
        h.extend_from_slice(value.as_bytes());
    }
    let total = (16 + h.len() + payload.len()) as u32;
    let mut out = Vec::new();
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&(h.len() as u32).to_be_bytes());
    let c = crc32(&out);
    out.extend_from_slice(&c.to_be_bytes());
    out.extend_from_slice(&h);
    out.extend_from_slice(payload);
    let c = crc32(&out);
    out.extend_from_slice(&c.to_be_bytes());
    out
}

/// An event frame, as ConverseStream sends it: the payload is the event
/// structure itself, padded with a `p` field as Bedrock does.
fn event(event_type: &str, mut payload: Value) -> Vec<u8> {
    payload["p"] = json!("abcdefghijklmnopqrstuvwxyzABCDEF");
    frame(
        &[
            (":event-type", event_type),
            (":content-type", "application/json"),
            (":message-type", "event"),
        ],
        payload.to_string().as_bytes(),
    )
}

fn exception(kind: &str, message: &str) -> Vec<u8> {
    frame(
        &[
            (":exception-type", kind),
            (":content-type", "application/json"),
            (":message-type", "exception"),
        ],
        json!({ "message": message }).to_string().as_bytes(),
    )
}

fn message_start() -> Vec<u8> {
    event("messageStart", json!({"role": "assistant"}))
}
fn text(index: u64, t: &str) -> Vec<u8> {
    event(
        "contentBlockDelta",
        json!({"contentBlockIndex": index, "delta": {"text": t}}),
    )
}
fn tool_start(index: u64, id: &str, name: &str) -> Vec<u8> {
    event(
        "contentBlockStart",
        json!({"contentBlockIndex": index, "start": {"toolUse": {"toolUseId": id, "name": name}}}),
    )
}
fn tool_input(index: u64, input: &str) -> Vec<u8> {
    event(
        "contentBlockDelta",
        json!({"contentBlockIndex": index, "delta": {"toolUse": {"input": input}}}),
    )
}
fn reasoning(index: u64, delta: Value) -> Vec<u8> {
    event(
        "contentBlockDelta",
        json!({"contentBlockIndex": index, "delta": {"reasoningContent": delta}}),
    )
}
fn block_stop(index: u64) -> Vec<u8> {
    event("contentBlockStop", json!({"contentBlockIndex": index}))
}
fn message_stop(reason: &str) -> Vec<u8> {
    event("messageStop", json!({"stopReason": reason}))
}
fn metadata(usage: Value) -> Vec<u8> {
    event(
        "metadata",
        json!({"usage": usage, "metrics": {"latencyMs": 42}}),
    )
}
fn small_usage() -> Vec<u8> {
    metadata(json!({"inputTokens": 3, "outputTokens": 2, "totalTokens": 5}))
}

/// A complete plain-text response.
fn text_response(t: &str) -> Vec<u8> {
    [
        message_start(),
        text(0, t),
        block_stop(0),
        message_stop("end_turn"),
        small_usage(),
    ]
    .concat()
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn model_config(base_url: &str) -> ModelConfig {
    ModelConfig::custom(
        ApiProtocol::BedrockConverseStream,
        "bedrock",
        base_url,
        MODEL,
        "Test model",
    )
}

fn stream_config(base_url: &str) -> StreamConfig {
    let mut config = StreamConfig::new(MODEL, "access:secret");
    config.messages = vec![Message::user("hi")];
    config.model_config = Some(model_config(base_url));
    config
}

async fn serve(body: Vec<u8>) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/model/{MODEL}/converse-stream")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, EVENTSTREAM))
        .mount(&server)
        .await;
    server
}

async fn run_with_events(base_url: &str) -> (Result<Message, ProviderError>, Vec<StreamEvent>) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let result = BedrockProvider
        .stream(stream_config(base_url), tx, CancellationToken::new())
        .await;
    let mut events = Vec::new();
    while let Ok(e) = rx.try_recv() {
        events.push(e);
    }
    (result, events)
}

async fn run(body: Vec<u8>) -> Result<Message, ProviderError> {
    let server = serve(body).await;
    run_with_events(&server.uri()).await.0
}

fn parts(msg: &Message) -> (&Vec<Content>, StopReason, &Usage, Option<&str>) {
    match msg {
        Message::Assistant {
            content,
            stop_reason,
            usage,
            error_message,
            ..
        } => (
            content,
            stop_reason.clone(),
            usage,
            error_message.as_deref(),
        ),
        other => panic!("expected an assistant message, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Text, usage, stop reasons
// ---------------------------------------------------------------------------

#[tokio::test]
async fn text_across_frames_with_usage_including_cache() {
    let body = [
        message_start(),
        text(0, "Hel"),
        text(0, "lo, "),
        text(0, "wörld"),
        block_stop(0),
        message_stop("end_turn"),
        metadata(json!({
            "inputTokens": 11, "outputTokens": 7, "totalTokens": 918,
            "cacheReadInputTokens": 800, "cacheWriteInputTokens": 100
        })),
    ]
    .concat();
    let server = serve(body).await;
    let (result, events) = run_with_events(&server.uri()).await;
    let msg = result.expect("a complete response");
    let (content, stop, usage, err) = parts(&msg);
    assert_eq!(stop, StopReason::Stop);
    assert_eq!(err, None);
    assert_eq!(content.len(), 1);
    assert!(matches!(&content[0], Content::Text { text } if text == "Hello, wörld"));
    assert_eq!(
        *usage,
        Usage {
            input: 11,
            output: 7,
            cache_read: 800,
            cache_write: 100,
            total_tokens: 918,
        }
    );
    let deltas: Vec<(usize, String)> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta {
                content_index,
                delta,
            } => Some((*content_index, delta.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        deltas,
        vec![
            (0, "Hel".to_string()),
            (0, "lo, ".to_string()),
            (0, "wörld".to_string())
        ]
    );
    assert!(matches!(events.first(), Some(StreamEvent::Start)));
    assert!(matches!(events.last(), Some(StreamEvent::Done { .. })));
}

#[tokio::test]
async fn separate_text_blocks_stay_separate() {
    let body = [
        message_start(),
        text(0, "first"),
        block_stop(0),
        text(1, "second"),
        block_stop(1),
        message_stop("end_turn"),
        small_usage(),
    ]
    .concat();
    let msg = run(body).await.unwrap();
    let (content, ..) = parts(&msg);
    assert_eq!(content.len(), 2);
    assert!(matches!(&content[0], Content::Text { text } if text == "first"));
    assert!(matches!(&content[1], Content::Text { text } if text == "second"));
}

#[tokio::test]
async fn max_tokens_is_length() {
    let body = [
        message_start(),
        text(0, "cut o"),
        block_stop(0),
        message_stop("max_tokens"),
        small_usage(),
    ]
    .concat();
    let msg = run(body).await.unwrap();
    assert_eq!(parts(&msg).1, StopReason::Length);
}

#[tokio::test]
async fn guardrail_and_content_filter_are_refusals() {
    for reason in ["guardrail_intervened", "content_filtered"] {
        let body = [
            message_start(),
            text(0, "Sorry."),
            block_stop(0),
            message_stop(reason),
            small_usage(),
        ]
        .concat();
        let msg = run(body).await.unwrap();
        let (content, stop, _, err) = parts(&msg);
        assert_eq!(stop, StopReason::Refusal, "{reason}");
        assert!(err.unwrap().contains(reason), "{err:?}");
        assert!(matches!(&content[0], Content::Text { text } if text == "Sorry."));
    }
}

#[tokio::test]
async fn context_window_exceeded_stop_is_an_overflow() {
    let body = [
        message_start(),
        message_stop("model_context_window_exceeded"),
        small_usage(),
    ]
    .concat();
    let msg = run(body).await.unwrap();
    assert_eq!(parts(&msg).1, StopReason::Error);
    assert!(msg.is_context_overflow());
}

// ---------------------------------------------------------------------------
// Tool calls
// ---------------------------------------------------------------------------

/// Records the arguments of every execution.
struct Recorder(Arc<Mutex<Vec<Value>>>);

#[async_trait::async_trait]
impl AgentTool for Recorder {
    fn name(&self) -> &str {
        "read_file"
    }
    fn label(&self) -> &str {
        "Read file"
    }
    fn description(&self) -> &str {
        "Read a file"
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {"path": {"type": "string"}, "limit": {"type": "integer"}}})
    }
    async fn execute(&self, params: Value, _ctx: ToolContext) -> Result<ToolResult, ToolError> {
        self.0.lock().unwrap().push(params);
        Ok(ToolResult {
            content: vec![Content::Text {
                text: "file contents".into(),
            }],
            details: Value::Null,
        })
    }
}

/// Serve `first` to the first request and `rest` to every later one; run
/// one prompt through the agent loop with the recording tool.
async fn run_agent(first: Vec<u8>, rest: Vec<u8>) -> (Vec<Value>, Vec<AgentMessage>, MockServer) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(first, EVENTSTREAM))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(rest, EVENTSTREAM))
        .with_priority(2)
        .mount(&server)
        .await;
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut agent = Agent::from_provider(BedrockProvider, model_config(&server.uri()))
        .with_api_key("access:secret")
        .with_tools(vec![Box::new(Recorder(calls.clone()))]);
    let mut rx = agent.prompt("read it").await;
    while rx.recv().await.is_some() {}
    agent.finish().await;
    let ran = calls.lock().unwrap().clone();
    (ran, agent.messages().to_vec(), server)
}

fn tool_results(messages: &[AgentMessage]) -> Vec<(bool, String)> {
    messages
        .iter()
        .filter_map(|m| match m {
            AgentMessage::Llm(Message::ToolResult {
                content, is_error, ..
            }) => Some((
                *is_error,
                content
                    .iter()
                    .filter_map(|c| match c {
                        Content::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>(),
            )),
            _ => None,
        })
        .collect()
}

/// The #174 headline: `toolUse.input` arrives in pieces and the tool runs
/// with the whole parsed object — not `{}`.
#[tokio::test]
async fn chunked_tool_input_runs_with_the_full_arguments() {
    let first = [
        message_start(),
        text(0, "Reading."),
        block_stop(0),
        tool_start(1, "tooluse_1", "read_file"),
        tool_input(1, ""),
        tool_input(1, "{\"pa"),
        tool_input(1, "th\": \"src/ma"),
        tool_input(1, "in.rs\", \"limit\""),
        tool_input(1, ": 10}"),
        block_stop(1),
        message_stop("tool_use"),
        small_usage(),
    ]
    .concat();
    let (ran, messages, _server) = run_agent(first, text_response("Done.")).await;
    assert_eq!(ran, vec![json!({"path": "src/main.rs", "limit": 10})]);
    let results = tool_results(&messages);
    assert_eq!(results, vec![(false, "file contents".to_string())]);
}

/// Two tool calls whose deltas interleave by `contentBlockIndex`: each
/// input goes to its own block, and every event carries that block's
/// content index.
#[tokio::test]
async fn interleaved_tool_calls_accumulate_by_block_index() {
    let body = [
        message_start(),
        tool_start(0, "a", "read_file"),
        tool_start(1, "b", "read_file"),
        tool_input(1, "{\"path\":"),
        tool_input(0, "{\"path\":"),
        tool_input(1, "\"b.txt\"}"),
        tool_input(0, "\"a.txt\"}"),
        block_stop(1),
        block_stop(0),
        message_stop("tool_use"),
        small_usage(),
    ]
    .concat();
    let server = serve(body).await;
    let (result, events) = run_with_events(&server.uri()).await;
    let msg = result.unwrap();
    let (content, stop, ..) = parts(&msg);
    assert_eq!(stop, StopReason::ToolUse);
    let calls: Vec<(&str, &Value)> = content
        .iter()
        .map(|c| match c {
            Content::ToolCall { id, arguments, .. } => (id.as_str(), arguments),
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    assert_eq!(
        calls,
        vec![
            ("a", &json!({"path": "a.txt"})),
            ("b", &json!({"path": "b.txt"}))
        ]
    );
    let deltas: Vec<(usize, String)> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCallDelta {
                content_index,
                delta,
            } => Some((*content_index, delta.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        deltas,
        vec![
            (1, "{\"path\":".into()),
            (0, "{\"path\":".into()),
            (1, "\"b.txt\"}".into()),
            (0, "\"a.txt\"}".into()),
        ]
    );
    let ends: Vec<usize> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCallEnd { content_index } => Some(*content_index),
            _ => None,
        })
        .collect();
    assert_eq!(ends, vec![1, 0]);
}

/// A tool with no parameters: a closed block with no input is `{}`, and
/// the tool runs.
#[tokio::test]
async fn closed_block_without_input_is_a_zero_argument_call() {
    let body = [
        message_start(),
        tool_start(0, "z", "read_file"),
        block_stop(0),
        message_stop("tool_use"),
        small_usage(),
    ]
    .concat();
    let msg = run(body).await.unwrap();
    assert!(
        matches!(&parts(&msg).0[0], Content::ToolCall { arguments, .. } if *arguments == json!({}))
    );
}

/// #167: input cut off at the token limit is never run on `{}` or on its
/// fragment — the call carries the unparsed marker, the loop answers it
/// with an error result, and the turn reports `Length`.
#[tokio::test]
async fn truncated_tool_input_is_answered_with_an_error_not_run() {
    let first = [
        message_start(),
        tool_start(0, "tooluse_1", "read_file"),
        tool_input(0, "{\"path\": \"src/ma"),
        block_stop(0),
        message_stop("max_tokens"),
        small_usage(),
    ]
    .concat();
    let msg = run(first.clone()).await.unwrap();
    let (content, stop, ..) = parts(&msg);
    assert_eq!(stop, StopReason::Length);
    let Content::ToolCall { arguments, .. } = &content[0] else {
        panic!("{content:?}")
    };
    assert_eq!(
        yoagent::provider::unparsed_tool_arguments(arguments),
        Some("{\"path\": \"src/ma")
    );

    let (ran, messages, _server) = run_agent(first, text_response("unused")).await;
    assert!(ran.is_empty(), "the tool must not run: {ran:?}");
    let results = tool_results(&messages);
    assert_eq!(results.len(), 1);
    assert!(results[0].0, "must be an error result: {results:?}");
}

/// A block that never got its `contentBlockStop` is not a runnable call,
/// even with no input at all — "nothing arrived" and "no parameters" cannot
/// be told apart.
#[tokio::test]
async fn unclosed_tool_block_is_never_runnable() {
    for input in [None, Some("{\"path\": \"a.txt\"}")] {
        let mut frames = vec![message_start(), tool_start(0, "t", "read_file")];
        if let Some(i) = input {
            frames.push(tool_input(0, i));
        }
        frames.push(message_stop("tool_use"));
        frames.push(small_usage());
        let first = frames.concat();

        let msg = run(first.clone()).await.unwrap();
        let Content::ToolCall { arguments, .. } = &parts(&msg).0[0] else {
            panic!()
        };
        assert!(
            yoagent::provider::unparsed_tool_arguments(arguments).is_some(),
            "{input:?}: {arguments}"
        );

        let (ran, messages, _server) = run_agent(first, text_response("ok")).await;
        assert!(ran.is_empty(), "{input:?}: tool ran with {ran:?}");
        assert!(tool_results(&messages)[0].0);
    }
}

// ---------------------------------------------------------------------------
// Reasoning
// ---------------------------------------------------------------------------

/// Reasoning text and signature accumulate into one thinking block, and the
/// next request replays it — text and signature unmodified, as the API
/// reference requires.
#[tokio::test]
async fn reasoning_with_signature_is_accumulated_and_replayed() {
    let first = [
        message_start(),
        reasoning(0, json!({"text": "Let me "})),
        reasoning(0, json!({"text": "think."})),
        reasoning(0, json!({"signature": "c2lnLTE="})),
        block_stop(0),
        tool_start(1, "t1", "read_file"),
        tool_input(1, "{\"path\":\"x\"}"),
        block_stop(1),
        message_stop("tool_use"),
        small_usage(),
    ]
    .concat();

    let msg = run(first.clone()).await.unwrap();
    let (content, ..) = parts(&msg);
    assert_eq!(
        content[0],
        Content::thinking_signed("Let me think.", "c2lnLTE=")
    );

    let (ran, _, server) = run_agent(first, text_response("done")).await;
    assert_eq!(ran, vec![json!({"path": "x"})]);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    let body: Value = serde_json::from_slice(&requests[1].body).unwrap();
    let assistant = &body["messages"][1];
    assert_eq!(assistant["role"], "assistant");
    assert_eq!(
        assistant["content"][0]["reasoningContent"]["reasoningText"],
        json!({"text": "Let me think.", "signature": "c2lnLTE="})
    );
    assert_eq!(
        assistant["content"][1]["toolUse"]["input"],
        json!({"path": "x"})
    );
}

// ---------------------------------------------------------------------------
// Framing over the network
// ---------------------------------------------------------------------------

/// Serve one response over a raw socket as HTTP/1.1 chunked transfer, one
/// chunk per piece with a pause between, so the client sees the body in
/// those pieces. `complete = false` drops the connection without the final
/// chunk.
async fn serve_chunked(pieces: Vec<Vec<u8>>, complete: bool) -> ChunkedServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        // Read the request: headers, then Content-Length bytes of body.
        let mut req = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = sock.read(&mut buf).await.unwrap();
            req.extend_from_slice(&buf[..n]);
            if let Some(end) = req.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&req[..end]).to_ascii_lowercase();
                let len: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .map(|v| v.trim().parse().unwrap())
                    .unwrap_or(0);
                if req.len() >= end + 4 + len {
                    break;
                }
            }
            if n == 0 {
                break;
            }
        }
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: {EVENTSTREAM}\r\ntransfer-encoding: chunked\r\n\r\n"
        );
        sock.write_all(head.as_bytes()).await.unwrap();
        for piece in pieces {
            let mut chunk = format!("{:x}\r\n", piece.len()).into_bytes();
            chunk.extend_from_slice(&piece);
            chunk.extend_from_slice(b"\r\n");
            sock.write_all(&chunk).await.unwrap();
            sock.flush().await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        if complete {
            sock.write_all(b"0\r\n\r\n").await.unwrap();
        }
        sock.shutdown().await.ok();
    });
    ChunkedServer {
        base: format!("http://{addr}"),
        handle,
    }
}

struct ChunkedServer {
    base: String,
    handle: tokio::task::JoinHandle<()>,
}

impl ChunkedServer {
    /// Wait for the server task, so a panic in it surfaces as itself and a
    /// hung accept cannot hang the suite.
    async fn join(self) {
        tokio::time::timeout(std::time::Duration::from_secs(10), self.handle)
            .await
            .expect("raw-socket test server hung")
            .expect("raw-socket test server panicked");
    }
}

/// Run one request against a raw-socket server and join it.
async fn run_chunked(pieces: Vec<Vec<u8>>, complete: bool) -> Result<Message, ProviderError> {
    let server = serve_chunked(pieces, complete).await;
    let (result, _) = run_with_events(&server.base).await;
    server.join().await;
    result
}

fn split_at_offsets(body: &[u8], offsets: &[usize]) -> Vec<Vec<u8>> {
    let mut pieces = Vec::new();
    let mut start = 0;
    for &o in offsets {
        pieces.push(body[start..o].to_vec());
        start = o;
    }
    pieces.push(body[start..].to_vec());
    pieces
}

/// Smoke test of the real HTTP path: the body arrives in chunks that split
/// frames inside a prelude, inside headers and between the bytes of a
/// multi-byte character, and still decodes. (A split character is harmless
/// by construction — payloads are only decoded from complete frames; the
/// proof that *every* split point works is the every-chunk-size unit tests
/// in `bedrock.rs` and `eventstream.rs`.)
#[tokio::test]
async fn frames_split_across_network_chunks_at_awkward_offsets() {
    let frames = [
        message_start(),
        text(0, "日本語のテキスト"),
        tool_start(1, "t", "read_file"),
        tool_input(1, "{\"path\":\"ü.txt\"}"),
        block_stop(1),
        message_stop("tool_use"),
        small_usage(),
    ];
    let body = frames.concat();
    let f0 = frames[0].len();
    // Inside the second frame's prelude (5 bytes in), inside its headers
    // (20 bytes in), inside the first 3-byte character of its payload, and
    // one byte into the third frame's CRC-covered prelude.
    let utf8 = body[f0..]
        .windows(3)
        .position(|w| w == "日".as_bytes())
        .unwrap()
        + f0;
    let offsets = [
        f0 + 5,
        f0 + 20,
        utf8 + 1,
        utf8 + 2,
        f0 + frames[1].len() + 1,
    ];
    let msg = run_chunked(split_at_offsets(&body, &offsets), true)
        .await
        .unwrap();
    let (content, stop, ..) = parts(&msg);
    assert_eq!(stop, StopReason::ToolUse);
    assert!(
        matches!(&content[0], Content::Text { text } if text == "日本語のテキスト"),
        "{content:?}"
    );
    assert!(
        matches!(&content[1], Content::ToolCall { arguments, .. } if *arguments == json!({"path": "ü.txt"}))
    );
}

/// The connection drops mid-frame after real content arrived: an error,
/// never the partial content as a finished turn.
#[tokio::test]
async fn connection_dropped_mid_stream_is_an_error() {
    let body = [
        message_start(),
        text(0, "partial answer"),
        message_stop("end_turn"),
    ]
    .concat();
    let cut = body.len() - 10;
    let result = run_chunked(vec![body[..cut].to_vec()], false).await;
    assert!(
        matches!(result, Err(ProviderError::Network(_))),
        "{result:?}"
    );
}

// ---------------------------------------------------------------------------
// Errors: never an empty successful turn
// ---------------------------------------------------------------------------

#[tokio::test]
async fn crc_mismatch_is_an_error() {
    let mut body = text_response("hello");
    // Flip a byte inside the second frame's JSON payload (10 bytes before
    // its trailing CRC); its message CRC no longer matches.
    let end_of_second = message_start().len() + text(0, "hello").len();
    body[end_of_second - 10] ^= 0x01;
    let result = run(body).await;
    let err = result.expect_err("a corrupted frame must fail the turn");
    assert!(err.to_string().contains("checksum"), "{err}");
}

#[tokio::test]
async fn truncated_frame_at_end_of_body_is_an_error() {
    let body = text_response("hello");
    let result = run(body[..body.len() - 3].to_vec()).await;
    assert!(
        matches!(result, Err(ProviderError::Network(_))),
        "{result:?}"
    );
}

#[tokio::test]
async fn stream_without_message_stop_is_an_error() {
    let body = [message_start(), text(0, "Hello"), block_stop(0)].concat();
    let result = run(body).await;
    assert!(
        matches!(result, Err(ProviderError::Network(ref m)) if m.contains("messageStop")),
        "{result:?}"
    );
}

#[tokio::test]
async fn empty_body_is_an_error_not_an_empty_turn() {
    let result = run(Vec::new()).await;
    assert!(
        matches!(result, Err(ProviderError::Network(_))),
        "{result:?}"
    );
}

/// What #174 reported: a JSON-lines body (what the old parser expected) is
/// not an event stream and must fail loudly instead of parsing to nothing.
#[tokio::test]
async fn json_lines_body_is_an_error() {
    let body = b"{\"contentBlockDelta\":{\"delta\":{\"text\":\"hi\"},\"contentBlockIndex\":0}}\n{\"messageStop\":{\"stopReason\":\"end_turn\"}}\n".to_vec();
    let result = run(body).await;
    // Rejected by the framing layer: the first 12 bytes are not a prelude
    // with a matching CRC.
    match result {
        Err(ProviderError::Other(m)) => assert!(m.contains("prelude checksum"), "{m}"),
        other => panic!("expected a framing error, got {other:?}"),
    }
}

#[tokio::test]
async fn throttling_exception_is_a_retryable_rate_limit() {
    let body = [
        message_start(),
        exception(
            "throttlingException",
            "Too many tokens, please wait before trying again.",
        ),
    ]
    .concat();
    let result = run(body).await;
    assert!(
        matches!(result, Err(ProviderError::RateLimited { .. })),
        "{result:?}"
    );
}

#[tokio::test]
async fn validation_exception_with_overflow_message_is_context_overflow() {
    let body = exception(
        "validationException",
        "Input is too long for requested model.",
    );
    let result = run(body).await;
    assert!(
        matches!(result, Err(ref e) if e.is_context_overflow()),
        "{result:?}"
    );
}

#[tokio::test]
async fn model_stream_error_exception_mid_stream_is_an_error() {
    let body = [
        message_start(),
        text(0, "Half an ans"),
        exception("modelStreamErrorException", "The model stream failed."),
    ]
    .concat();
    let result = run(body).await;
    match result {
        Err(ProviderError::Api(m)) => assert!(m.contains("modelStreamErrorException"), "{m}"),
        other => panic!("expected an API error, got {other:?}"),
    }
}

#[tokio::test]
async fn http_errors_are_classified() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(400)
                .insert_header(
                    "x-amzn-ErrorType",
                    "ValidationException:http://internal.amazon.com/coral/com.amazon.bedrock/",
                )
                .set_body_json(json!({"message": "Input is too long for requested model."})),
        )
        .mount(&server)
        .await;
    let (result, _) = run_with_events(&server.uri()).await;
    assert!(
        matches!(result, Err(ref e) if e.is_context_overflow()),
        "{result:?}"
    );

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "2")
                .set_body_json(json!({"message": "Too many requests"})),
        )
        .mount(&server)
        .await;
    let (result, _) = run_with_events(&server.uri()).await;
    assert!(
        matches!(
            result,
            Err(ProviderError::RateLimited {
                retry_after_ms: Some(2000)
            })
        ),
        "{result:?}"
    );
}

#[tokio::test]
async fn json_success_body_is_an_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"output": {"message": {"content": [{"text": "hi"}]}}})),
        )
        .mount(&server)
        .await;
    let (result, _) = run_with_events(&server.uri()).await;
    assert!(matches!(result, Err(ProviderError::Api(_))), "{result:?}");
}

// ---------------------------------------------------------------------------
// Review round: redacted reasoning, completeness, dropped content, protocol
// ---------------------------------------------------------------------------

/// A reasoning block that is only `redactedContent` is kept as a redacted
/// thinking block and replayed as `redactedContent` — never as an empty
/// `reasoningText` with an empty signature, which Bedrock rejects.
#[tokio::test]
async fn redacted_reasoning_is_kept_and_replayed() {
    // Two deltas: the bytes are concatenated, not the base64 strings.
    let first = [
        message_start(),
        reasoning(0, json!({"redactedContent": "AAEC"})), // 00 01 02
        reasoning(0, json!({"redactedContent": "AwQ="})), // 03 04
        block_stop(0),
        text(1, "Reading."),
        block_stop(1),
        tool_start(2, "t1", "read_file"),
        tool_input(2, "{\"path\":\"x\"}"),
        block_stop(2),
        message_stop("tool_use"),
        small_usage(),
    ]
    .concat();

    let msg = run(first.clone()).await.unwrap();
    let (content, ..) = parts(&msg);
    assert_eq!(content[0], Content::thinking_redacted("AAECAwQ="));
    // The text next to it keeps its own block and index.
    assert!(matches!(&content[1], Content::Text { text } if text == "Reading."));

    let (ran, _, server) = run_agent(first, text_response("done")).await;
    assert_eq!(ran, vec![json!({"path": "x"})]);
    let requests = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[1].body).unwrap();
    let replayed = &body["messages"][1]["content"];
    assert_eq!(
        replayed[0],
        json!({"reasoningContent": {"redactedContent": "AAECAwQ="}})
    );
    assert_eq!(replayed[1], json!({"text": "Reading."}));
    assert!(
        !body.to_string().contains("\"signature\":\"\""),
        "no empty signature may be sent: {body}"
    );
}

/// Signature deltas are appended (a delta union member, like text).
#[tokio::test]
async fn signature_deltas_are_appended() {
    let body = [
        message_start(),
        reasoning(0, json!({"text": "hmm"})),
        reasoning(0, json!({"signature": "sig-a"})),
        reasoning(0, json!({"signature": "sig-b"})),
        block_stop(0),
        text(1, "ok"),
        block_stop(1),
        message_stop("end_turn"),
        small_usage(),
    ]
    .concat();
    let msg = run(body).await.unwrap();
    assert_eq!(
        parts(&msg).0[0],
        Content::thinking_signed("hmm", "sig-asig-b")
    );
}

/// Once `messageStop` and `metadata` have arrived the response is complete:
/// a connection that then drops (cleanly or mid-frame) keeps it rather than
/// failing into a re-billed retry.
#[tokio::test]
async fn drop_after_a_complete_response_keeps_it() {
    let complete = text_response("all here");
    // Drop without the terminating chunk.
    let msg = run_chunked(vec![complete.clone()], false).await.unwrap();
    assert!(matches!(&parts(&msg).0[0], Content::Text { text } if text == "all here"));
    // Half a trailing frame, then a clean end of body.
    let mut body = complete.clone();
    body.extend_from_slice(&text(0, "extra")[..10]);
    let msg = run(body).await.unwrap();
    assert_eq!(parts(&msg).1, StopReason::Stop);
    // Half a trailing frame, then a dropped connection.
    let mut body = complete;
    body.extend_from_slice(&text(0, "extra")[..10]);
    let msg = run_chunked(vec![body], false).await.unwrap();
    assert_eq!(parts(&msg).2.total_tokens, 5);
}

/// `messageStop` alone is not complete: a drop before `metadata` is still an
/// error (usage would be unknown).
#[tokio::test]
async fn drop_before_metadata_is_an_error() {
    let mut body = [
        message_start(),
        text(0, "hi"),
        block_stop(0),
        message_stop("end_turn"),
    ]
    .concat();
    body.extend_from_slice(&small_usage()[..10]);
    let result = run_chunked(vec![body], false).await;
    assert!(
        matches!(result, Err(ProviderError::Network(_))),
        "{result:?}"
    );
}

/// A stream that ends cleanly after `messageStop` without `metadata` is a
/// finished turn with zero usage, as the other providers report it.
#[tokio::test]
async fn message_stop_without_metadata_reports_zero_usage() {
    let body = [
        message_start(),
        text(0, "hi"),
        block_stop(0),
        message_stop("end_turn"),
    ]
    .concat();
    let msg = run(body).await.unwrap();
    assert_eq!(*parts(&msg).2, Usage::default());
}

/// The last `metadata` event wins.
#[tokio::test]
async fn last_metadata_wins() {
    let body = [
        message_start(),
        text(0, "hi"),
        message_stop("end_turn"),
        metadata(json!({"inputTokens": 1, "outputTokens": 1, "totalTokens": 2})),
        metadata(json!({"inputTokens": 9, "outputTokens": 4, "totalTokens": 13})),
    ]
    .concat();
    let msg = run(body).await.unwrap();
    assert_eq!(parts(&msg).2.input, 9);
    assert_eq!(parts(&msg).2.total_tokens, 13);
}

/// Fields the provider does not need may be missing without failing a
/// finished turn.
#[tokio::test]
async fn unneeded_fields_may_be_missing() {
    let body = [
        event("messageStart", json!({})),
        text(0, "hi"),
        message_stop("end_turn"),
        metadata(json!({"inputTokens": 2, "outputTokens": 3})),
    ]
    .concat();
    let msg = run(body).await.unwrap();
    assert_eq!(parts(&msg).2.input, 2);
    assert_eq!(parts(&msg).2.total_tokens, 0);
}

/// A server-side tool (`type: server_tool_use`) runs on AWS's side: it is
/// not surfaced as a tool call and nothing executes locally; the text block
/// after it keeps index 0.
#[tokio::test]
async fn server_side_tool_use_is_not_executed() {
    let first = [
        message_start(),
        event(
            "contentBlockStart",
            json!({"contentBlockIndex": 0, "start": {"toolUse": {"toolUseId": "s1", "name": "read_file", "type": "server_tool_use"}}}),
        ),
        tool_input(0, "{\"path\":\"secret\"}"),
        block_stop(0),
        event(
            "contentBlockStart",
            json!({"contentBlockIndex": 1, "start": {"toolResult": {"toolUseId": "s1"}}}),
        ),
        block_stop(1),
        text(2, "The answer."),
        block_stop(2),
        message_stop("end_turn"),
        small_usage(),
    ]
    .concat();
    let msg = run(first.clone()).await.unwrap();
    let (content, stop, ..) = parts(&msg);
    assert_eq!(stop, StopReason::Stop);
    assert_eq!(content.len(), 1, "{content:?}");
    assert!(matches!(&content[0], Content::Text { text } if text == "The answer."));

    let (ran, messages, _server) = run_agent(first, text_response("unused")).await;
    assert!(ran.is_empty(), "{ran:?}");
    assert!(tool_results(&messages).is_empty());
}

/// A refusal or an error stop after a complete tool block keeps its verdict
/// and the tool is not run.
#[tokio::test]
async fn refusal_or_error_after_a_tool_block_is_not_run() {
    for (reason, expected) in [
        ("guardrail_intervened", StopReason::Refusal),
        ("malformed_tool_use", StopReason::Error),
    ] {
        let first = [
            message_start(),
            tool_start(0, "t", "read_file"),
            tool_input(0, "{\"path\":\"a\"}"),
            block_stop(0),
            message_stop(reason),
            small_usage(),
        ]
        .concat();
        let msg = run(first.clone()).await.unwrap();
        assert_eq!(parts(&msg).1, expected, "{reason}");
        let (ran, _, _server) = run_agent(first, text_response("unused")).await;
        assert!(ran.is_empty(), "{reason}: tool ran with {ran:?}");
    }
}

/// An unmodeled error frame (`:message-type: error`, name in `:error-code`).
#[tokio::test]
async fn error_frame_with_throttling_code_is_a_rate_limit() {
    let body = [
        message_start(),
        text(0, "partial"),
        frame(
            &[
                (":message-type", "error"),
                (":error-code", "ThrottlingException"),
                (":error-message", "Rate exceeded"),
            ],
            b"",
        ),
    ]
    .concat();
    let result = run(body).await;
    assert!(
        matches!(result, Err(ProviderError::RateLimited { .. })),
        "{result:?}"
    );
}

/// Protocol violations are errors, not guesses.
#[tokio::test]
async fn protocol_violations_are_errors() {
    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "delta after stop",
            [message_start(), text(0, "a"), block_stop(0), text(0, "b")].concat(),
        ),
        (
            "duplicate start",
            [
                message_start(),
                tool_start(0, "a", "read_file"),
                tool_start(0, "b", "read_file"),
            ]
            .concat(),
        ),
        (
            "tool delta without start",
            [message_start(), tool_input(0, "{}")].concat(),
        ),
        (
            "kind mismatch",
            [
                message_start(),
                text(0, "a"),
                reasoning(0, json!({"text": "b"})),
            ]
            .concat(),
        ),
    ];
    for (name, mut body) in cases {
        body.extend(message_stop("end_turn"));
        body.extend(small_usage());
        let result = run(body).await;
        assert!(
            matches!(result, Err(ProviderError::Other(ref m)) if m.contains("Bedrock stream")),
            "{name}: {result:?}"
        );
    }
}

/// An unclosed tool block still closes its lifecycle with `ToolCallEnd`.
#[tokio::test]
async fn unclosed_tool_block_emits_tool_call_end() {
    let body = [
        message_start(),
        text(0, "x"),
        tool_start(1, "t", "read_file"),
        tool_input(1, "{\"pa"),
        message_stop("max_tokens"),
        small_usage(),
    ]
    .concat();
    let server = serve(body).await;
    let (result, events) = run_with_events(&server.uri()).await;
    result.unwrap();
    let ends: Vec<usize> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCallEnd { content_index } => Some(*content_index),
            _ => None,
        })
        .collect();
    assert_eq!(ends, vec![1]);
}

/// Forward compatibility: an unknown event type, an unknown `start` member
/// and a not-surfaced delta member do not fail the turn or shift indices,
/// and a block whose start was unrecognised is typed by its first delta.
#[tokio::test]
async fn unknown_members_and_events_are_skipped() {
    let body = [
        message_start(),
        event("someFutureEvent", json!({"data": 1})),
        event(
            "contentBlockStart",
            json!({"contentBlockIndex": 0, "start": {"futureThing": {"a": 1}}}),
        ),
        text(0, "typed by its delta"),
        block_stop(0),
        event(
            "contentBlockStart",
            json!({"contentBlockIndex": 1, "start": {"image": {"format": "png"}}}),
        ),
        event(
            "contentBlockDelta",
            json!({"contentBlockIndex": 1, "delta": {"image": {"source": {}}}}),
        ),
        block_stop(1),
        event(
            "contentBlockDelta",
            json!({"contentBlockIndex": 2, "delta": {"citation": {"title": "x"}}}),
        ),
        text(3, "after"),
        block_stop(3),
        message_stop("end_turn"),
        small_usage(),
    ]
    .concat();
    let server = serve(body).await;
    let (result, events) = run_with_events(&server.uri()).await;
    let msg = result.unwrap();
    let (content, ..) = parts(&msg);
    assert_eq!(content.len(), 2, "{content:?}");
    assert!(matches!(&content[0], Content::Text { text } if text == "typed by its delta"));
    assert!(matches!(&content[1], Content::Text { text } if text == "after"));
    let indices: Vec<usize> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta { content_index, .. } => Some(*content_index),
            _ => None,
        })
        .collect();
    assert_eq!(indices, vec![0, 1]);
}

/// `x-amzn-ErrorType` names the error in the message.
#[tokio::test]
async fn http_error_names_the_aws_error_type() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(403)
                .insert_header("x-amzn-ErrorType", "AccessDeniedException:http://x/")
                .set_body_json(json!({"message": "not allowed"})),
        )
        .mount(&server)
        .await;
    let (result, _) = run_with_events(&server.uri()).await;
    match result {
        Err(ProviderError::Auth(m)) => assert!(
            m.contains("AccessDeniedException") && m.contains("not allowed"),
            "{m}"
        ),
        other => panic!("expected Auth, got {other:?}"),
    }
}

/// A 200 with some other declared content type reports its body.
#[tokio::test]
async fn non_eventstream_content_type_reports_the_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw("<html>gateway says no</html>", "text/html"),
        )
        .mount(&server)
        .await;
    let (result, _) = run_with_events(&server.uri()).await;
    match result {
        Err(ProviderError::Api(m)) => assert!(
            m.contains("text/html") && m.contains("gateway says no"),
            "{m}"
        ),
        other => panic!("expected Api, got {other:?}"),
    }
}
