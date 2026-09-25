//! A response that ends as `StopReason::Refusal` — the model declined, or a
//! content filter stopped it — is terminal: its tool calls are answered with
//! error results but never executed, and the run ends without another LLM
//! turn. Driven through the real Chat Completions and Responses providers
//! against a local mock server, so the stop reason comes from the wire shape
//! rather than a hand-built message.

use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};
use yoagent::agent::{Agent, StructuredPromptError};
use yoagent::agent_loop::{agent_loop, AgentLoopConfig};
use yoagent::context::ExecutionLimits;
use yoagent::provider::{
    ModelConfig, OpenAiCompatProvider, OpenAiResponsesProvider, StreamProvider,
};
use yoagent::*;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn chunk(json: &str) -> String {
    format!("data: {json}\n\n")
}

/// Chat Completions: a complete tool call, then the given finish reason.
fn chat_tool_call(finish_reason: &str) -> String {
    [
        chunk(
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"search","arguments":"{\"q\":\"x\"}"}}]}}]}"#,
        ),
        chunk(&format!(
            r#"{{"choices":[{{"index":0,"delta":{{}},"finish_reason":"{finish_reason}"}}]}}"#
        )),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat()
}

fn chat_text(text: &str) -> String {
    [
        chunk(&format!(
            r#"{{"choices":[{{"index":0,"delta":{{"content":"{text}"}}}}]}}"#
        )),
        chunk(r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat()
}

/// Serialize `(event name, payload)` pairs as a Responses SSE body.
fn sse(events: Vec<(&str, Value)>) -> String {
    let mut out = String::new();
    for (seq, (name, mut data)) in events.into_iter().enumerate() {
        data["type"] = json!(name);
        data["sequence_number"] = json!(seq);
        out.push_str(&format!("event: {name}\ndata: {data}\n\n"));
    }
    out
}

/// Responses: a complete function call, plus a refusal part when `refuse`.
fn responses_tool_call(refuse: bool) -> String {
    let fc = |status: &str| {
        json!({"output_index": 0, "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1",
               "name": "search", "arguments": if status == "completed" { r#"{"q":"x"}"# } else { "" },
               "status": status}})
    };
    let mut events = vec![
        (
            "response.created",
            json!({"response": {"id": "resp_1", "object": "response", "status": "in_progress", "output": []}}),
        ),
        ("response.output_item.added", fc("in_progress")),
        ("response.output_item.done", fc("completed")),
    ];
    if refuse {
        let part = json!({"type": "refusal", "refusal": "I can't help with that."});
        events.push((
            "response.output_item.done",
            json!({"output_index": 1, "item": {"type": "message", "id": "msg_1", "status": "completed",
                   "role": "assistant", "content": [part]}}),
        ));
    }
    events.push((
        "response.completed",
        json!({"response": {"id": "resp_1", "object": "response", "status": "completed",
               "usage": {"input_tokens": 5, "output_tokens": 3, "total_tokens": 8}}}),
    ));
    sse(events)
}

fn responses_text(text: &str) -> String {
    sse(vec![
        (
            "response.output_text.delta",
            json!({"output_index": 0, "item_id": "msg_1", "content_index": 0, "delta": text}),
        ),
        (
            "response.completed",
            json!({"response": {"id": "resp_2", "object": "response", "status": "completed",
                   "usage": {"input_tokens": 5, "output_tokens": 1, "total_tokens": 6}}}),
        ),
    ])
}

/// Serve `first` once, then `rest` for every later request.
async fn server(first: String, rest: String) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(first, "text/event-stream"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(rest, "text/event-stream"))
        .with_priority(2)
        .mount(&server)
        .await;
    server
}

// ---------------------------------------------------------------------------
// Loop harness
// ---------------------------------------------------------------------------

/// A tool that counts how often it actually ran.
struct Search(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl AgentTool for Search {
    fn name(&self) -> &str {
        "search"
    }
    fn label(&self) -> &str {
        "Search"
    }
    fn description(&self) -> &str {
        "Search"
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {"q": {"type": "string"}}})
    }
    async fn execute(&self, _params: Value, _ctx: ToolContext) -> Result<ToolResult, ToolError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult {
            content: vec![Content::Text {
                text: "found".into(),
            }],
            details: Value::Null,
        })
    }
}

#[derive(Clone, Copy, Debug)]
enum Api {
    Chat,
    Responses,
}

struct Run {
    new_messages: Vec<AgentMessage>,
    events: Vec<AgentEvent>,
    tool_runs: usize,
    requests: usize,
    follow_up_polls: usize,
}

async fn run(api: Api, server: &MockServer, strategy: ToolExecutionStrategy) -> Run {
    let (provider, mut mc): (Arc<dyn StreamProvider>, ModelConfig) = match api {
        Api::Chat => (
            Arc::new(OpenAiCompatProvider),
            ModelConfig::openai("gpt-5.5", "GPT-5.5"),
        ),
        Api::Responses => (
            Arc::new(OpenAiResponsesProvider),
            ModelConfig::openai_responses("gpt-5.5", "GPT-5.5"),
        ),
    };
    mc.base_url = server.uri();
    let follow_up_polls = Arc::new(AtomicUsize::new(0));
    let polls = follow_up_polls.clone();
    let config = AgentLoopConfig {
        provider,
        model: "gpt-5.5".into(),
        api_key: "test".into(),
        thinking_level: ThinkingLevel::Off,
        max_tokens: None,
        temperature: None,
        model_config: Some(mc),
        convert_to_llm: None,
        transform_context: None,
        get_steering_messages: None,
        // Counts polls without ever queueing anything, so a positive control
        // still ends; the refusal path must not reach the poll at all.
        get_follow_up_messages: Some(Box::new(move || {
            polls.fetch_add(1, Ordering::SeqCst);
            Vec::new()
        })),
        context_config: None,
        compaction_strategy: None,
        // Loop detection and turn accounting live on the tracker; keep it on.
        execution_limits: Some(ExecutionLimits::default()),
        cache_config: CacheConfig::default(),
        tool_output_sink: None,
        output_schema: None,
        tool_execution: strategy,
        retry_config: yoagent::RetryConfig::none(),
        before_turn: None,
        after_turn: None,
        on_error: None,
        input_filters: vec![],
        tool_middleware: vec![],
        turn_delay: None,
    };
    let tool_runs = Arc::new(AtomicUsize::new(0));
    let mut context = AgentContext {
        system_prompt: "test".into(),
        messages: Vec::new(),
        tools: vec![Box::new(Search(tool_runs.clone()))],
    };
    let (tx, mut rx) = mpsc::unbounded_channel();
    let new_messages = agent_loop(
        vec![AgentMessage::Llm(Message::user("search for x"))],
        &mut context,
        &config,
        tx,
        CancellationToken::new(),
    )
    .await;
    let mut events = Vec::new();
    while let Ok(e) = rx.try_recv() {
        events.push(e);
    }
    Run {
        new_messages,
        events,
        tool_runs: tool_runs.load(Ordering::SeqCst),
        requests: server.received_requests().await.unwrap().len(),
        follow_up_polls: follow_up_polls.load(Ordering::SeqCst),
    }
}

const STRATEGIES: [ToolExecutionStrategy; 3] = [
    ToolExecutionStrategy::Parallel,
    ToolExecutionStrategy::Sequential,
    ToolExecutionStrategy::Batched { size: 1 },
];

fn roles(msgs: &[AgentMessage]) -> Vec<&str> {
    msgs.iter().map(|m| m.role()).collect()
}

/// Everything a refused turn must leave behind.
fn assert_refused_turn(r: &Run, label: &str) {
    assert_eq!(r.tool_runs, 0, "{label}: the refused tool call ran");
    assert_eq!(r.requests, 1, "{label}: a refusal must end the run");
    assert_eq!(
        r.follow_up_polls, 0,
        "{label}: follow-ups stay queued, as after Error/Aborted"
    );
    assert_eq!(
        roles(&r.new_messages),
        ["user", "assistant", "toolResult"],
        "{label}"
    );
    let AgentMessage::Llm(Message::Assistant { stop_reason, .. }) = &r.new_messages[1] else {
        panic!("{label}: expected assistant");
    };
    assert_eq!(*stop_reason, StopReason::Refusal, "{label}");
    let AgentMessage::Llm(Message::ToolResult {
        tool_call_id,
        is_error,
        content,
        ..
    }) = &r.new_messages[2]
    else {
        panic!("{label}: expected tool result");
    };
    assert_eq!(tool_call_id, "call_1", "{label}");
    assert!(*is_error, "{label}");
    assert!(
        matches!(&content[0], Content::Text { text } if text.contains("not run") && text.contains("refusal")),
        "{label}: {content:?}"
    );

    // Events: the call is paired Start/End (error), the TurnEnd carries the
    // result, and AgentEnd closes the run.
    let starts = r
        .events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolExecutionStart { tool_call_id, .. } if tool_call_id == "call_1"))
        .count();
    let ends: Vec<bool> = r
        .events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                is_error,
                ..
            } if tool_call_id == "call_1" => Some(*is_error),
            _ => None,
        })
        .collect();
    assert_eq!((starts, ends), (1, vec![true]), "{label}");
    let turn_ends: Vec<usize> = r
        .events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TurnEnd { tool_results, .. } => Some(tool_results.len()),
            _ => None,
        })
        .collect();
    assert_eq!(turn_ends, [1], "{label}");
    assert!(
        matches!(r.events.last(), Some(AgentEvent::AgentEnd { messages, .. }) if messages.len() == 3),
        "{label}"
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn chat_content_filter_after_a_complete_tool_call_does_not_run_it() {
    for strategy in STRATEGIES {
        let s = server(chat_tool_call("content_filter"), chat_text("again")).await;
        let r = run(Api::Chat, &s, strategy.clone()).await;
        assert_refused_turn(&r, &format!("chat {strategy:?}"));
    }
}

#[tokio::test]
async fn responses_refusal_beside_a_function_call_does_not_run_it() {
    for strategy in STRATEGIES {
        let s = server(responses_tool_call(true), responses_text("again")).await;
        let r = run(Api::Responses, &s, strategy.clone()).await;
        assert_refused_turn(&r, &format!("responses {strategy:?}"));
    }
}

/// Positive control: the same tool call without the refusal runs, and the
/// loop takes its next turn — so the tests above fail for the right reason.
#[tokio::test]
async fn positive_control_the_same_tool_call_runs_without_a_refusal() {
    for (api, first, rest) in [
        (Api::Chat, chat_tool_call("tool_calls"), chat_text("done")),
        (
            Api::Responses,
            responses_tool_call(false),
            responses_text("done"),
        ),
    ] {
        for strategy in STRATEGIES {
            let s = server(first.clone(), rest.clone()).await;
            let r = run(api, &s, strategy.clone()).await;
            let label = format!("{api:?} {strategy:?}");
            assert_eq!(r.tool_runs, 1, "{label}");
            assert_eq!(r.requests, 2, "{label}");
            assert_eq!(r.follow_up_polls, 1, "{label}");
            assert_eq!(
                roles(&r.new_messages),
                ["user", "assistant", "toolResult", "assistant"],
                "{label}"
            );
        }
    }
}

/// `prompt_structured` reports a refusal as a provider-side failure carrying
/// the explanation, not as a parse failure over the refusal text.
#[tokio::test]
async fn prompt_structured_reports_a_refusal_with_its_explanation() {
    let body = [
        chunk(r#"{"choices":[{"index":0,"delta":{"refusal":"I can't help with that."}}]}"#),
        chunk(r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    let s = server(body, chat_text("unused")).await;
    let mut mc = ModelConfig::openai("gpt-5.5", "GPT-5.5");
    mc.base_url = s.uri();
    let mut agent = Agent::from_provider(OpenAiCompatProvider, mc).with_api_key("test");
    let err = agent
        .prompt_structured::<Value>("give me json", json!({"type": "object"}))
        .await
        .expect_err("a refusal is not structured output");
    match err {
        StructuredPromptError::Provider { message } => {
            assert!(
                message.contains("refusal") && message.contains("I can't help with that."),
                "{message}"
            );
        }
        other => panic!("expected Provider, got {other:?}"),
    }

    // Positive control: a JSON reply on the same path parses.
    let s = server(chat_text(r#"{\"ok\":true}"#), chat_text("unused")).await;
    let mut mc = ModelConfig::openai("gpt-5.5", "GPT-5.5");
    mc.base_url = s.uri();
    let mut agent = Agent::from_provider(OpenAiCompatProvider, mc).with_api_key("test");
    let v: Value = agent
        .prompt_structured("give me json", json!({"type": "object"}))
        .await
        .unwrap();
    assert_eq!(v, json!({"ok": true}));
}
