//! Responses-API stream parsing (#178), run through both
//! `OpenAiResponsesProvider` and `AzureOpenAiProvider`.
//!
//! The fixtures follow the event names and payload shapes of OpenAI's
//! generated types (`openai-python`, `src/openai/types/responses/`):
//! a function call starts with `response.output_item.added`
//! (`item.type == "function_call"`), argument deltas carry `output_index` and
//! `item_id`, and the complete arguments arrive in
//! `response.function_call_arguments.done` / `response.output_item.done`.
//! There is no `response.function_call_arguments.start` event; the previous
//! fixtures used one, which is how both providers passed tests while dropping
//! every real function call.

use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use yoagent::agent::Agent;
use yoagent::provider::{
    unparsed_tool_arguments, AzureOpenAiProvider, CostConfig, ModelConfig, OpenAiResponsesProvider,
    StreamConfig, StreamEvent, StreamProvider,
};
use yoagent::types::*;

// ---------------------------------------------------------------------------
// Fixture builders (real Responses event shapes)
// ---------------------------------------------------------------------------

/// Serialize `(event name, payload)` pairs as an SSE body, adding the `type`
/// and a running `sequence_number` the way the API does.
fn sse(events: Vec<(&str, Value)>) -> String {
    let mut out = String::new();
    for (seq, (name, mut data)) in events.into_iter().enumerate() {
        data["type"] = json!(name);
        data["sequence_number"] = json!(seq);
        out.push_str(&format!("event: {name}\ndata: {data}\n\n"));
    }
    out
}

fn created() -> (&'static str, Value) {
    (
        "response.created",
        json!({"response": {"id": "resp_1", "object": "response", "status": "in_progress", "output": []}}),
    )
}

fn completed(usage: Value) -> (&'static str, Value) {
    (
        "response.completed",
        json!({"response": {"id": "resp_1", "object": "response", "status": "completed", "usage": usage}}),
    )
}

fn plain_usage() -> Value {
    json!({
        "input_tokens": 20,
        "input_tokens_details": {"cached_tokens": 0, "cache_write_tokens": 0},
        "output_tokens": 10,
        "output_tokens_details": {"reasoning_tokens": 0},
        "total_tokens": 30
    })
}

fn fc_item(output_index: usize, n: u32, name: &str, arguments: &str, status: &str) -> Value {
    json!({
        "output_index": output_index,
        "item": {
            "type": "function_call",
            "id": format!("fc_{n}"),
            "call_id": format!("call_{n}"),
            "name": name,
            "arguments": arguments,
            "status": status
        }
    })
}

fn fc_added(output_index: usize, n: u32, name: &str) -> (&'static str, Value) {
    (
        "response.output_item.added",
        fc_item(output_index, n, name, "", "in_progress"),
    )
}

fn fc_delta(output_index: usize, n: u32, delta: &str) -> (&'static str, Value) {
    (
        "response.function_call_arguments.delta",
        json!({"item_id": format!("fc_{n}"), "output_index": output_index, "delta": delta}),
    )
}

fn fc_args_done(output_index: usize, n: u32, arguments: &str) -> (&'static str, Value) {
    (
        "response.function_call_arguments.done",
        json!({"item_id": format!("fc_{n}"), "output_index": output_index, "arguments": arguments}),
    )
}

fn fc_item_done(output_index: usize, n: u32, name: &str, arguments: &str) -> (&'static str, Value) {
    (
        "response.output_item.done",
        fc_item(output_index, n, name, arguments, "completed"),
    )
}

/// A complete assistant `message` output item streaming `chunks`.
fn message_item(output_index: usize, chunks: &[&str]) -> Vec<(&'static str, Value)> {
    let full: String = chunks.concat();
    let mut v = vec![
        (
            "response.output_item.added",
            json!({"output_index": output_index, "item": {"type": "message", "id": "msg_1", "status": "in_progress", "role": "assistant", "content": []}}),
        ),
        (
            "response.content_part.added",
            json!({"item_id": "msg_1", "output_index": output_index, "content_index": 0, "part": {"type": "output_text", "text": "", "annotations": []}}),
        ),
    ];
    for c in chunks {
        v.push((
            "response.output_text.delta",
            json!({"item_id": "msg_1", "output_index": output_index, "content_index": 0, "delta": c, "logprobs": []}),
        ));
    }
    v.push((
        "response.output_text.done",
        json!({"item_id": "msg_1", "output_index": output_index, "content_index": 0, "text": full, "logprobs": []}),
    ));
    v.push((
        "response.content_part.done",
        json!({"item_id": "msg_1", "output_index": output_index, "content_index": 0, "part": {"type": "output_text", "text": full, "annotations": []}}),
    ));
    v.push((
        "response.output_item.done",
        json!({"output_index": output_index, "item": {"type": "message", "id": "msg_1", "status": "completed", "role": "assistant", "content": [{"type": "output_text", "text": full, "annotations": []}]}}),
    ));
    v
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn text_fixture() -> String {
    let mut ev = vec![created()];
    ev.extend(message_item(0, &["Hello", " world"]));
    ev.push(completed(plain_usage()));
    sse(ev)
}

fn one_call_fixture() -> String {
    let args = r#"{"q":"rust"}"#;
    sse(vec![
        created(),
        fc_added(0, 1, "search"),
        fc_delta(0, 1, r#"{"q":"#),
        fc_delta(0, 1, r#""ru"#),
        fc_delta(0, 1, r#"st"}"#),
        fc_args_done(0, 1, args),
        fc_item_done(0, 1, "search", args),
        completed(plain_usage()),
    ])
}

fn parallel_calls_fixture() -> String {
    let a = r#"{"path":"a.txt"}"#;
    let b = r#"{"path":"b.txt"}"#;
    sse(vec![
        created(),
        fc_added(0, 1, "read_file"),
        fc_added(1, 2, "read_file"),
        // Interleaved: a "last buffer" parser would cross these over.
        fc_delta(0, 1, r#"{"path":"#),
        fc_delta(1, 2, r#"{"path":"#),
        fc_delta(1, 2, r#""b.txt"}"#),
        fc_delta(0, 1, r#""a.txt"}"#),
        fc_args_done(0, 1, a),
        fc_item_done(0, 1, "read_file", a),
        fc_args_done(1, 2, b),
        fc_item_done(1, 2, "read_file", b),
        completed(plain_usage()),
    ])
}

fn no_deltas_fixture() -> String {
    let args = r#"{"n":3}"#;
    sse(vec![
        created(),
        fc_added(0, 1, "roll"),
        fc_args_done(0, 1, args),
        fc_item_done(0, 1, "roll", args),
        completed(plain_usage()),
    ])
}

fn reasoning_fixture() -> String {
    let mut ev = vec![
        created(),
        (
            "response.output_item.added",
            json!({"output_index": 0, "item": {"type": "reasoning", "id": "rs_1", "summary": []}}),
        ),
        (
            "response.reasoning_summary_part.added",
            json!({"item_id": "rs_1", "output_index": 0, "summary_index": 0, "part": {"type": "summary_text", "text": ""}}),
        ),
        (
            "response.reasoning_summary_text.delta",
            json!({"item_id": "rs_1", "output_index": 0, "summary_index": 0, "delta": "Thinking about"}),
        ),
        (
            "response.reasoning_summary_text.delta",
            json!({"item_id": "rs_1", "output_index": 0, "summary_index": 0, "delta": " the task."}),
        ),
        (
            "response.reasoning_summary_text.done",
            json!({"item_id": "rs_1", "output_index": 0, "summary_index": 0, "text": "Thinking about the task."}),
        ),
        (
            "response.reasoning_summary_part.added",
            json!({"item_id": "rs_1", "output_index": 0, "summary_index": 1, "part": {"type": "summary_text", "text": ""}}),
        ),
        (
            "response.reasoning_summary_text.delta",
            json!({"item_id": "rs_1", "output_index": 0, "summary_index": 1, "delta": "Done."}),
        ),
        (
            "response.output_item.done",
            json!({"output_index": 0, "item": {"type": "reasoning", "id": "rs_1", "summary": [
                {"type": "summary_text", "text": "Thinking about the task."},
                {"type": "summary_text", "text": "Done."}
            ]}}),
        ),
    ];
    ev.extend(message_item(1, &["Answer"]));
    ev.push(completed(plain_usage()));
    sse(ev)
}

fn incomplete_mid_arguments_fixture() -> String {
    sse(vec![
        created(),
        fc_added(0, 1, "write_file"),
        fc_delta(0, 1, r#"{"path":"a.txt","content":"lo"#),
        (
            "response.incomplete",
            json!({"response": {
                "id": "resp_1", "object": "response", "status": "incomplete",
                "incomplete_details": {"reason": "max_output_tokens"},
                "usage": {"input_tokens": 5, "output_tokens": 1, "total_tokens": 6}
            }}),
        ),
    ])
}

fn cached_usage_fixture() -> String {
    let mut ev = vec![created()];
    ev.extend(message_item(0, &["ok"]));
    ev.push(completed(json!({
        "input_tokens": 10_000,
        "input_tokens_details": {"cached_tokens": 6_000, "cache_write_tokens": 3_000},
        "output_tokens": 500,
        "output_tokens_details": {"reasoning_tokens": 200},
        "total_tokens": 10_500
    })));
    sse(ev)
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum Which {
    Responses,
    Azure,
}

const BOTH: [Which; 2] = [Which::Responses, Which::Azure];

async fn run(which: Which, body: String) -> (Message, Vec<StreamEvent>) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(&server)
        .await;
    let mut mc = ModelConfig::openai_responses("gpt-5.5", "GPT-5.5");
    mc.base_url = server.uri();
    let mut config = StreamConfig::new("gpt-5.5", "test-key");
    config.system_prompt = "test".into();
    config.messages = vec![Message::user("hi")];
    config.model_config = Some(mc);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let result = match which {
        Which::Responses => {
            OpenAiResponsesProvider
                .stream(config, tx, CancellationToken::new())
                .await
        }
        Which::Azure => {
            AzureOpenAiProvider
                .stream(config, tx, CancellationToken::new())
                .await
        }
    };
    let message = result.unwrap_or_else(|e| panic!("{which:?}: stream failed: {e}"));
    let mut events = Vec::new();
    while let Ok(e) = rx.try_recv() {
        events.push(e);
    }
    (message, events)
}

fn parts(m: &Message) -> (&[Content], &StopReason, &Usage) {
    match m {
        Message::Assistant {
            content,
            stop_reason,
            usage,
            ..
        } => (content, stop_reason, usage),
        _ => panic!("expected assistant message"),
    }
}

fn tool_calls(content: &[Content]) -> Vec<(&str, &str, &Value)> {
    content
        .iter()
        .filter_map(|c| match c {
            Content::ToolCall {
                id,
                name,
                arguments,
                ..
            } => Some((id.as_str(), name.as_str(), arguments)),
            _ => None,
        })
        .collect()
}

/// Number of `ToolCallDelta` events for a content index. Equal to the number
/// of wire deltas only when each delta was routed live to an open call — a
/// parser that recovered the arguments from `.done` alone emits one.
fn delta_count(events: &[StreamEvent], content_index: usize) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, StreamEvent::ToolCallDelta { content_index: i, .. } if *i == content_index))
        .count()
}

/// Concatenated `ToolCallDelta` text per content index.
fn streamed_args(events: &[StreamEvent], content_index: usize) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCallDelta {
                content_index: i,
                delta,
            } if *i == content_index => Some(delta.as_str()),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn text_is_collected_once() {
    for which in BOTH {
        let (m, _) = run(which, text_fixture()).await;
        let (content, stop, usage) = parts(&m);
        assert_eq!(content.len(), 1, "{which:?}: {content:?}");
        assert!(
            matches!(&content[0], Content::Text { text } if text == "Hello world"),
            "{which:?}: deltas and the finished item must not both be appended: {content:?}"
        );
        assert_eq!(*stop, StopReason::Stop, "{which:?}");
        assert_eq!((usage.input, usage.output), (20, 10), "{which:?}");
    }
}

#[tokio::test]
async fn one_function_call_with_chunked_arguments() {
    for which in BOTH {
        let (m, events) = run(which, one_call_fixture()).await;
        let (content, stop, _) = parts(&m);
        let calls = tool_calls(content);
        assert_eq!(calls.len(), 1, "{which:?}: the call must not be dropped");
        assert_eq!(calls[0].0, "call_1", "{which:?}: id is the call_id");
        assert_eq!(calls[0].1, "search");
        assert_eq!(*calls[0].2, json!({"q": "rust"}));
        assert_eq!(*stop, StopReason::ToolUse, "{which:?}");

        assert!(events.iter().any(|e| matches!(e,
            StreamEvent::ToolCallStart { id, name, .. } if id == "call_1" && name == "search")));
        assert_eq!(streamed_args(&events, 0), r#"{"q":"rust"}"#, "{which:?}");
        assert_eq!(
            delta_count(&events, 0),
            3,
            "{which:?}: deltas streamed live"
        );
        assert!(events
            .iter()
            .any(|e| matches!(e, StreamEvent::ToolCallEnd { content_index: 0 })));
    }
}

#[tokio::test]
async fn parallel_calls_with_interleaved_deltas_route_by_output_index() {
    for which in BOTH {
        let (m, events) = run(which, parallel_calls_fixture()).await;
        let (content, stop, _) = parts(&m);
        let calls = tool_calls(content);
        assert_eq!(calls.len(), 2, "{which:?}");
        assert_eq!(calls[0].0, "call_1");
        assert_eq!(*calls[0].2, json!({"path": "a.txt"}), "{which:?}");
        assert_eq!(calls[1].0, "call_2");
        assert_eq!(*calls[1].2, json!({"path": "b.txt"}), "{which:?}");
        assert_eq!(*stop, StopReason::ToolUse);
        // The streamed deltas were routed to the right call, too.
        assert_eq!(
            streamed_args(&events, 0),
            r#"{"path":"a.txt"}"#,
            "{which:?}"
        );
        assert_eq!(
            streamed_args(&events, 1),
            r#"{"path":"b.txt"}"#,
            "{which:?}"
        );
        assert_eq!(delta_count(&events, 0), 2, "{which:?}");
        assert_eq!(delta_count(&events, 1), 2, "{which:?}");
    }
}

#[tokio::test]
async fn call_with_no_deltas_takes_arguments_from_done() {
    for which in BOTH {
        let (m, events) = run(which, no_deltas_fixture()).await;
        let (content, stop, _) = parts(&m);
        let calls = tool_calls(content);
        assert_eq!(calls.len(), 1, "{which:?}");
        assert_eq!(
            *calls[0].2,
            json!({"n": 3}),
            "{which:?}: `.done` is the source of truth, not an empty delta buffer"
        );
        assert_eq!(*stop, StopReason::ToolUse);
        assert_eq!(streamed_args(&events, 0), r#"{"n":3}"#, "{which:?}");
    }
}

#[tokio::test]
async fn reasoning_summary_becomes_thinking() {
    for which in BOTH {
        let (m, events) = run(which, reasoning_fixture()).await;
        let (content, stop, _) = parts(&m);
        assert_eq!(content.len(), 2, "{which:?}: {content:?}");
        assert!(
            matches!(&content[0], Content::Thinking { thinking, .. }
                if thinking == "Thinking about the task.\n\nDone."),
            "{which:?}: {content:?}"
        );
        assert!(matches!(&content[1], Content::Text { text } if text == "Answer"));
        assert_eq!(*stop, StopReason::Stop);
        assert!(events.iter().any(|e| matches!(
            e,
            StreamEvent::ThinkingDelta {
                content_index: 0,
                ..
            }
        )));
    }
}

#[tokio::test]
async fn incomplete_mid_arguments_keeps_length_and_marks_the_call() {
    for which in BOTH {
        let (m, _) = run(which, incomplete_mid_arguments_fixture()).await;
        let (content, stop, usage) = parts(&m);
        assert_eq!(
            *stop,
            StopReason::Length,
            "{which:?}: an open tool call must not overwrite the Length stop reason"
        );
        let calls = tool_calls(content);
        assert_eq!(
            calls.len(),
            1,
            "{which:?}: the cut-off call still reaches the loop, which answers it"
        );
        assert_eq!(
            unparsed_tool_arguments(calls[0].2),
            Some(r#"{"path":"a.txt","content":"lo"#),
            "{which:?}"
        );
        assert_eq!((usage.input, usage.output), (5, 1));
    }
}

#[tokio::test]
async fn usage_splits_cached_and_written_tokens_and_prices_each() {
    for which in BOTH {
        let (m, _) = run(which, cached_usage_fixture()).await;
        let (_, _, usage) = parts(&m);
        assert_eq!(
            usage.input, 1_000,
            "{which:?}: input is the uncached remainder"
        );
        assert_eq!(usage.cache_read, 6_000, "{which:?}");
        assert_eq!(usage.cache_write, 3_000, "{which:?}");
        assert_eq!(usage.output, 500);
        assert_eq!(usage.total_tokens, 10_500);

        // $1/M input, $10/M output, $0.10/M cache read, $1.25/M cache write.
        let cost = CostConfig::new(1.0, 10.0)
            .with_cache_read(0.1)
            .with_cache_write(1.25);
        let expected = (1_000.0 * 1.0 + 500.0 * 10.0 + 6_000.0 * 0.1 + 3_000.0 * 1.25) / 1e6;
        assert!(
            (cost.cost_usd(usage) - expected).abs() < 1e-12,
            "{which:?}: {} != {expected}",
            cost.cost_usd(usage)
        );
    }
}

/// `ModelConfig::openai_responses` + `Agent::from_config` resolves to the
/// Responses provider (it posts to `/responses`) with the `OPENAI_API_KEY`
/// key, and a streamed function call reaches the loop, which answers it.
#[tokio::test]
async fn from_config_openai_responses_runs_a_function_call_end_to_end() {
    // The only test in this binary that reads OPENAI_API_KEY.
    std::env::set_var("OPENAI_API_KEY", "openai-env-key");
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(header("authorization", "Bearer openai-env-key"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(one_call_fixture(), "text/event-stream"),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(header("authorization", "Bearer openai-env-key"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(text_fixture(), "text/event-stream"))
        .with_priority(2)
        .mount(&server)
        .await;

    let mut mc = ModelConfig::openai_responses("gpt-5.5", "GPT-5.5");
    mc.base_url = server.uri();
    let mut agent = Agent::from_config(mc);
    let mut rx = agent.prompt("search for rust").await;
    while rx.recv().await.is_some() {}
    agent.finish().await;

    let msgs: Vec<&Message> = agent
        .messages()
        .iter()
        .filter_map(|m| match m {
            AgentMessage::Llm(m) => Some(m),
            _ => None,
        })
        .collect();
    let called = msgs.iter().any(|m| match m {
        Message::Assistant { content, .. } => tool_calls(content)
            .iter()
            .any(|(id, _, args)| *id == "call_1" && **args == json!({"q": "rust"})),
        _ => false,
    });
    assert!(called, "the function call must reach history: {msgs:?}");
    assert!(
        msgs.iter().any(
            |m| matches!(m, Message::ToolResult { tool_call_id, .. } if tool_call_id == "call_1")
        ),
        "the loop must answer the call"
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

// ---------------------------------------------------------------------------
// Refusals, content filters and null usage
// ---------------------------------------------------------------------------

fn error_message(m: &Message) -> Option<&str> {
    match m {
        Message::Assistant { error_message, .. } => error_message.as_deref(),
        _ => panic!("expected assistant message"),
    }
}

fn text_of(content: &[Content]) -> String {
    content
        .iter()
        .filter_map(|c| match c {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn streamed_text(events: &[StreamEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect()
}

const REFUSAL: &str = "I'm sorry, but I can't help with that.";

/// A refusal the way the API streams it (`ResponseRefusalDeltaEvent`,
/// `ResponseRefusalDoneEvent`, and the `refusal` part repeated in
/// `content_part.done` and the finished `message` item).
fn streamed_refusal_fixture() -> String {
    let part = json!({"type": "refusal", "refusal": REFUSAL});
    sse(vec![
        created(),
        (
            "response.output_item.added",
            json!({"output_index": 0, "item": {"type": "message", "id": "msg_1", "status": "in_progress", "role": "assistant", "content": []}}),
        ),
        (
            "response.content_part.added",
            json!({"item_id": "msg_1", "output_index": 0, "content_index": 0, "part": {"type": "refusal", "refusal": ""}}),
        ),
        (
            "response.refusal.delta",
            json!({"item_id": "msg_1", "output_index": 0, "content_index": 0, "delta": "I'm sorry, but "}),
        ),
        (
            "response.refusal.delta",
            json!({"item_id": "msg_1", "output_index": 0, "content_index": 0, "delta": "I can't help with that."}),
        ),
        (
            "response.refusal.done",
            json!({"item_id": "msg_1", "output_index": 0, "content_index": 0, "refusal": REFUSAL}),
        ),
        (
            "response.content_part.done",
            json!({"item_id": "msg_1", "output_index": 0, "content_index": 0, "part": part}),
        ),
        (
            "response.output_item.done",
            json!({"output_index": 0, "item": {"type": "message", "id": "msg_1", "status": "completed", "role": "assistant", "content": [part]}}),
        ),
        completed(plain_usage()),
    ])
}

#[tokio::test]
async fn a_streamed_refusal_is_a_refusal_with_its_text_once() {
    for which in BOTH {
        let (m, events) = run(which, streamed_refusal_fixture()).await;
        let (content, stop, _) = parts(&m);
        assert_eq!(*stop, StopReason::Refusal, "{which:?}");
        // Kept as the turn's text, not duplicated by the three places the
        // complete refusal is repeated.
        assert_eq!(text_of(content), REFUSAL, "{which:?}");
        assert_eq!(streamed_text(&events), REFUSAL, "{which:?}");
    }
    let (m, _) = run(Which::Responses, streamed_refusal_fixture()).await;
    let msg = error_message(&m).expect("a refusal explains itself");
    assert!(msg.contains("refusal") && msg.contains(REFUSAL), "{msg}");
}

#[tokio::test]
async fn a_refusal_only_in_the_finished_item_is_still_a_refusal() {
    for which in BOTH {
        let body = sse(vec![
            created(),
            (
                "response.output_item.done",
                json!({"output_index": 0, "item": {"type": "message", "id": "msg_1", "status": "completed", "role": "assistant",
                       "content": [{"type": "refusal", "refusal": REFUSAL}]}}),
            ),
            completed(plain_usage()),
        ]);
        let (m, events) = run(which, body).await;
        let (content, stop, _) = parts(&m);
        assert_eq!(*stop, StopReason::Refusal, "{which:?}");
        assert_eq!(text_of(content), REFUSAL, "{which:?}");
        assert_eq!(streamed_text(&events), REFUSAL, "{which:?}");
    }
}

#[tokio::test]
async fn a_refusal_beside_a_function_call_is_not_masked_as_tool_use() {
    for which in BOTH {
        let body = sse(vec![
            created(),
            fc_added(0, 1, "read_file"),
            fc_item_done(0, 1, "read_file", r#"{"path":"a"}"#),
            (
                "response.refusal.done",
                json!({"item_id": "msg_1", "output_index": 1, "content_index": 0, "refusal": REFUSAL}),
            ),
            completed(plain_usage()),
        ]);
        let (m, _) = run(which, body).await;
        assert_eq!(*parts(&m).1, StopReason::Refusal, "{which:?}");
    }
}

#[tokio::test]
async fn ordinary_text_is_not_a_refusal() {
    // Near-miss / positive control for the refusal tests: the same message
    // shape with an `output_text` part stays `Stop` with no error message.
    for which in BOTH {
        let (m, _) = run(which, text_fixture()).await;
        assert_eq!(*parts(&m).1, StopReason::Stop, "{which:?}");
        assert_eq!(error_message(&m), None, "{which:?}");
    }
}

fn incomplete(event: &'static str, reason: &str) -> String {
    let mut ev = vec![created()];
    ev.extend(message_item(0, &["partial"]));
    ev.push((
        event,
        json!({"response": {
            "id": "resp_1", "object": "response", "status": "incomplete",
            "incomplete_details": {"reason": reason},
            "usage": {"input_tokens": 5, "output_tokens": 1, "total_tokens": 6}
        }}),
    ));
    sse(ev)
}

#[tokio::test]
async fn content_filter_is_a_refusal_and_max_output_tokens_stays_length() {
    // `IncompleteDetails.reason`: max_output_tokens | max_messages |
    // content_filter | steered. Both terminal shapes carry it.
    for which in BOTH {
        for event in ["response.incomplete", "response.completed"] {
            let (m, _) = run(which, incomplete(event, "content_filter")).await;
            let (content, stop, usage) = parts(&m);
            assert_eq!(*stop, StopReason::Refusal, "{which:?} {event}");
            assert_eq!(text_of(content), "partial", "{which:?} {event}");
            assert_eq!((usage.input, usage.output), (5, 1), "{which:?} {event}");

            for reason in ["max_output_tokens", "max_messages"] {
                let (m, _) = run(which, incomplete(event, reason)).await;
                assert_eq!(
                    *parts(&m).1,
                    StopReason::Length,
                    "{which:?} {event} {reason}"
                );
                if matches!(which, Which::Responses) {
                    assert_eq!(error_message(&m), None, "{event} {reason}");
                }
            }
        }
    }
    let (m, _) = run(
        Which::Responses,
        incomplete("response.incomplete", "content_filter"),
    )
    .await;
    let msg = error_message(&m).expect("a content-filter stop explains itself");
    assert!(msg.contains("content_filter"), "{msg}");
}

#[tokio::test]
async fn null_usage_counts_do_not_drop_the_terminal_event() {
    for which in BOTH {
        let mut ev = vec![created()];
        ev.extend(message_item(0, &["ok"]));
        ev.push((
            "response.incomplete",
            json!({"response": {
                "id": "resp_1", "object": "response", "status": "incomplete",
                "incomplete_details": {"reason": "content_filter"},
                "usage": {"input_tokens": 40, "input_tokens_details": {"cached_tokens": null, "cache_write_tokens": null},
                          "output_tokens": 7, "output_tokens_details": null, "total_tokens": null}
            }}),
        ));
        let (m, _) = run(which, sse(ev)).await;
        let (_, stop, usage) = parts(&m);
        // The usage survives, and so does the stop reason parsed alongside it.
        assert_eq!(
            (usage.input, usage.output, usage.cache_read),
            (40, 7, 0),
            "{which:?}"
        );
        assert_eq!(*stop, StopReason::Refusal, "{which:?}");
    }
}

/// Azure's mid-stream capacity error reads as a rate limit (retried with
/// backoff), not a context overflow (which would compact the history): its
/// message contains "exceeds the maximum", which used to match an overflow
/// phrase.
#[tokio::test]
async fn azure_no_capacity_mid_stream_is_rate_limited() {
    use yoagent::provider::ProviderError;
    let body = sse(vec![
        created(),
        (
            "error",
            json!({"error": {"type": "too_many_requests", "code": "no_capacity",
                   "message": "The request exceeds the maximum usage size allowed during peak load. Please retry later.",
                   "param": null}}),
        ),
    ]);
    for which in BOTH {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(body.clone(), "text/event-stream"),
            )
            .mount(&server)
            .await;
        let mut mc = ModelConfig::openai_responses("gpt-5.5", "GPT-5.5");
        mc.base_url = server.uri();
        let mut config = StreamConfig::new("gpt-5.5", "test-key");
        config.messages = vec![Message::user("hi")];
        config.model_config = Some(mc);
        let (tx, _rx) = mpsc::unbounded_channel();
        let result = match which {
            Which::Responses => {
                OpenAiResponsesProvider
                    .stream(config, tx, CancellationToken::new())
                    .await
            }
            Which::Azure => {
                AzureOpenAiProvider
                    .stream(config, tx, CancellationToken::new())
                    .await
            }
        };
        let err = result.expect_err("an error event fails the stream");
        assert!(
            matches!(err, ProviderError::RateLimited { .. }),
            "{which:?}: {err:?}"
        );
        assert!(!err.is_context_overflow(), "{which:?}");
    }
}
