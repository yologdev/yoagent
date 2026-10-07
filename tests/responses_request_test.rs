//! What the Responses providers (`OpenAiResponsesProvider` and
//! `AzureOpenAiProvider`, which share one request builder) put on the wire:
//! `prompt_cache_key`, `include: ["reasoning.encrypted_content"]`, and
//! encrypted reasoning items replayed in place on the next turn — only to the
//! API that produced them.
//!
//! Every assertion compares the exact request JSON the mock server received.

use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};
use yoagent::agent::Agent;
use yoagent::provider::{
    ApiProtocol, AzureOpenAiProvider, ModelConfig, OpenAiResponsesProvider, StreamConfig,
    StreamProvider,
};
use yoagent::types::*;

// ---------------------------------------------------------------------------
// Fixtures (event shapes per openai-python `types/responses/`)
// ---------------------------------------------------------------------------

fn sse(events: Vec<Value>) -> String {
    events
        .into_iter()
        .enumerate()
        .map(|(seq, mut e)| {
            e["sequence_number"] = json!(seq);
            format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap())
        })
        .collect()
}

fn completed() -> Value {
    json!({"type": "response.completed", "response": {"id": "resp_1", "status": "completed",
        "usage": {"input_tokens": 20, "output_tokens": 10, "total_tokens": 30}}})
}

/// Reasoning (with a streamed summary and encrypted content), then a
/// function call: what a reasoning model sends when it calls a tool.
fn reasoning_then_call_fixture() -> String {
    let args = r#"{"path":"a.txt"}"#;
    sse(vec![
        json!({"type": "response.created", "response": {"id": "resp_1", "status": "in_progress", "output": []}}),
        json!({"type": "response.output_item.added", "output_index": 0,
               "item": {"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "partial"}}),
        json!({"type": "response.reasoning_summary_text.delta", "item_id": "rs_1", "output_index": 0,
               "summary_index": 0, "delta": "Need the file."}),
        json!({"type": "response.output_item.done", "output_index": 0,
               "item": {"type": "reasoning", "id": "rs_1",
                        "summary": [{"type": "summary_text", "text": "Need the file."}],
                        "encrypted_content": "gAAAA-enc-1", "status": "completed"}}),
        json!({"type": "response.output_item.added", "output_index": 1,
               "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1",
                        "name": "read_file", "arguments": "", "status": "in_progress"}}),
        json!({"type": "response.function_call_arguments.done", "item_id": "fc_1",
               "output_index": 1, "arguments": args}),
        json!({"type": "response.output_item.done", "output_index": 1,
               "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1",
                        "name": "read_file", "arguments": args, "status": "completed"}}),
        completed(),
    ])
}

fn text_fixture() -> String {
    sse(vec![
        json!({"type": "response.output_text.delta", "output_index": 0, "delta": "done"}),
        completed(),
    ])
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

impl Which {
    fn protocol(self) -> ApiProtocol {
        match self {
            Which::Responses => ApiProtocol::OpenAiResponses,
            Which::Azure => ApiProtocol::AzureOpenAiResponses,
        }
    }

    fn path(self) -> &'static str {
        match self {
            Which::Responses => "/responses",
            Which::Azure => "/openai/v1/responses",
        }
    }

    /// A reasoning-model config pointing at `uri`.
    fn model_config(self, uri: &str) -> ModelConfig {
        match self {
            Which::Responses => {
                let mut mc = ModelConfig::openai_responses("gpt-5.5", "GPT-5.5");
                mc.base_url = uri.to_string();
                mc
            }
            Which::Azure => {
                let mut mc = ModelConfig::custom(
                    ApiProtocol::AzureOpenAiResponses,
                    "azure",
                    format!("{uri}/openai/v1"),
                    "gpt-5.5",
                    "GPT-5.5",
                );
                mc.reasoning = true;
                mc
            }
        }
    }

    async fn stream(self, config: StreamConfig) -> Message {
        let (tx, _rx) = mpsc::unbounded_channel();
        let result = match self {
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
        result.unwrap_or_else(|e| panic!("{self:?}: stream failed: {e}"))
    }
}

/// Stream one request with `messages` and return (the request body the server
/// saw, the assistant message).
async fn one_request(
    which: Which,
    mc: Option<ModelConfig>,
    messages: Vec<Message>,
    fixture: String,
    tune: impl FnOnce(&mut StreamConfig),
) -> (Value, Message) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(which.path()))
        .respond_with(ResponseTemplate::new(200).set_body_raw(fixture, "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;
    let mc = mc
        .map(|mut mc| {
            mc.base_url = match which {
                Which::Responses => server.uri(),
                Which::Azure => format!("{}/openai/v1", server.uri()),
            };
            mc
        })
        .unwrap_or_else(|| which.model_config(&server.uri()));
    let mut config = StreamConfig::new(mc.id.clone(), "test-key");
    config.system_prompt = "You are a coding agent.".into();
    config.messages = messages;
    config.model_config = Some(mc);
    tune(&mut config);
    let message = which.stream(config).await;
    let requests = server.received_requests().await.unwrap();
    (body(&requests[0]), message)
}

fn body(req: &Request) -> Value {
    serde_json::from_slice(&req.body).unwrap()
}

fn assistant(content: Vec<Content>) -> Message {
    Message::assistant(
        content,
        StopReason::ToolUse,
        "gpt-5.5",
        "test",
        Usage::default(),
    )
}

fn tool_result(id: &str, text: &str) -> Message {
    Message::ToolResult {
        tool_call_id: id.into(),
        tool_name: "read_file".into(),
        content: vec![Content::Text { text: text.into() }],
        is_error: false,
        timestamp: 0,
    }
}

// ---------------------------------------------------------------------------
// The two-turn exchange, request JSON pinned exactly
// ---------------------------------------------------------------------------

/// Turn 1 asks for encrypted reasoning and sends the cache key; its reasoning
/// item comes back on a thinking block; turn 2 replays that item, unmodified
/// and before the function call it led to, followed by the call's output.
#[tokio::test]
async fn encrypted_reasoning_is_requested_and_replayed_in_place_next_turn() {
    for which in BOTH {
        let with_key = |c: &mut StreamConfig| {
            c.cache_config = CacheConfig::default().with_session_key("session-42");
        };
        let user = Message::user("read a.txt");
        let (first, reply) = one_request(
            which,
            None,
            vec![user.clone()],
            reasoning_then_call_fixture(),
            with_key,
        )
        .await;
        assert_eq!(
            first,
            json!({
                "model": "gpt-5.5",
                "stream": true,
                "instructions": "You are a coding agent.",
                "input": [{"role": "user", "content": "read a.txt"}],
                "include": ["reasoning.encrypted_content"],
                "prompt_cache_key": "session-42",
            }),
            "{which:?}"
        );

        // The reply: one thinking block carrying the item, then the call.
        let Message::Assistant { content, .. } = &reply else {
            panic!("{which:?}: {reply:?}")
        };
        assert_eq!(content.len(), 2, "{which:?}: {content:?}");
        let Content::Thinking {
            thinking,
            redacted: Some(stored),
            redacted_protocol,
            ..
        } = &content[0]
        else {
            panic!("{which:?}: no encrypted reasoning: {content:?}")
        };
        assert_eq!(thinking, "Need the file.");
        assert_eq!(*redacted_protocol, Some(which.protocol()), "{which:?}");
        // The finished item's content, not the partial copy in `added`.
        assert_eq!(
            serde_json::from_str::<Value>(stored).unwrap(),
            json!({"id": "rs_1", "encrypted_content": "gAAAA-enc-1",
                   "summary": [{"type": "summary_text", "text": "Need the file."}]})
        );

        let (second, _) = one_request(
            which,
            None,
            vec![user, reply, tool_result("call_1", "hello")],
            text_fixture(),
            with_key,
        )
        .await;
        assert_eq!(
            second["input"],
            json!([
                {"role": "user", "content": "read a.txt"},
                {"type": "reasoning", "id": "rs_1", "encrypted_content": "gAAAA-enc-1",
                 "summary": [{"type": "summary_text", "text": "Need the file."}]},
                {"type": "function_call", "call_id": "call_1", "name": "read_file",
                 "arguments": "{\"path\":\"a.txt\"}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "hello"},
            ]),
            "{which:?}"
        );
        assert_eq!(second["prompt_cache_key"], "session-42", "{which:?}");
        assert_eq!(
            second["include"],
            json!(["reasoning.encrypted_content"]),
            "{which:?}"
        );
    }
}

/// Encrypted reasoning from another API — Anthropic's `redacted_thinking`,
/// Bedrock's `redactedContent`, the other Responses service, or data of
/// unknown origin — is never sent. OpenAI's and Azure's encrypted reasoning
/// are separate services' data, so they do not cross either.
#[tokio::test]
async fn encrypted_reasoning_from_another_protocol_is_not_replayed() {
    let item = json!({"id": "rs_9", "summary": [], "encrypted_content": "enc"}).to_string();
    for which in BOTH {
        let other_responses = match which {
            Which::Responses => ApiProtocol::AzureOpenAiResponses,
            Which::Azure => ApiProtocol::OpenAiResponses,
        };
        let mut unknown_origin = Content::thinking("old");
        if let Content::Thinking { redacted, .. } = &mut unknown_origin {
            *redacted = Some(item.clone());
        }
        let turn = assistant(vec![
            Content::thinking_redacted(ApiProtocol::AnthropicMessages, item.clone()),
            Content::thinking_redacted(ApiProtocol::BedrockConverseStream, item.clone()),
            Content::thinking_redacted(other_responses, item.clone()),
            unknown_origin,
            Content::thinking_signed("plain summary", "sig"),
            Content::tool_call("call_1", "read_file", json!({"path": "a"})),
        ]);
        let (sent, _) = one_request(
            which,
            None,
            vec![Message::user("go"), turn, tool_result("call_1", "x")],
            text_fixture(),
            |_| {},
        )
        .await;
        assert_eq!(
            sent["input"],
            json!([
                {"role": "user", "content": "go"},
                {"type": "function_call", "call_id": "call_1", "name": "read_file",
                 "arguments": "{\"path\":\"a\"}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "x"},
            ]),
            "{which:?}"
        );
    }
}

/// Positive control for the test above: the same block, tagged with the
/// target's own protocol, is replayed — and a reasoning item with nothing
/// after it in its turn is not.
#[tokio::test]
async fn own_reasoning_is_replayed_but_a_trailing_item_is_dropped() {
    let item = |id: &str| json!({"id": id, "summary": [], "encrypted_content": "enc"}).to_string();
    for which in BOTH {
        let turn_1 = assistant(vec![
            Content::thinking_redacted(which.protocol(), item("rs_a")),
            Content::Text {
                text: "Sure.".into(),
            },
        ]);
        let turn_2 = assistant(vec![
            Content::Text {
                text: "Partial".into(),
            },
            Content::thinking_redacted(which.protocol(), item("rs_b")),
        ]);
        let (sent, _) = one_request(
            which,
            None,
            vec![
                Message::user("one"),
                turn_1,
                Message::user("two"),
                turn_2,
                Message::user("three"),
            ],
            text_fixture(),
            |_| {},
        )
        .await;
        assert_eq!(
            sent["input"],
            json!([
                {"role": "user", "content": "one"},
                {"type": "reasoning", "id": "rs_a", "summary": [], "encrypted_content": "enc"},
                {"type": "message", "role": "assistant",
                 "content": [{"type": "output_text", "text": "Sure."}]},
                {"role": "user", "content": "two"},
                {"type": "message", "role": "assistant",
                 "content": [{"type": "output_text", "text": "Partial"}]},
                {"role": "user", "content": "three"},
            ]),
            "{which:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// prompt_cache_key and include gating
// ---------------------------------------------------------------------------

/// Without an explicit key one is derived from the system prompt (stable
/// across a session's turns); caching hints off sends none.
#[tokio::test]
async fn prompt_cache_key_is_derived_and_absent_when_caching_is_off() {
    for which in BOTH {
        let (a, _) = one_request(
            which,
            None,
            vec![Message::user("a")],
            text_fixture(),
            |_| {},
        )
        .await;
        let (b, _) = one_request(
            which,
            None,
            vec![Message::user("a"), Message::user("b")],
            text_fixture(),
            |_| {},
        )
        .await;
        let key = a["prompt_cache_key"].as_str().expect("derived key");
        assert!(key.starts_with("yo-"), "{which:?}: {key}");
        assert_eq!(a["prompt_cache_key"], b["prompt_cache_key"], "{which:?}");

        for off in [CacheConfig::disabled(), {
            let mut c = CacheConfig::default().with_session_key("ignored");
            c.strategy = CacheStrategy::Disabled;
            c
        }] {
            let (sent, _) =
                one_request(which, None, vec![Message::user("a")], text_fixture(), |c| {
                    c.cache_config = off.clone()
                })
                .await;
            assert!(sent.get("prompt_cache_key").is_none(), "{which:?}: {sent}");
        }
    }
}

/// `include` goes only to a reasoning model: declared by
/// `ModelConfig::reasoning`, or implied by a reasoning effort being sent.
/// A `custom` Azure deployment (reasoning: false) with thinking off sends
/// neither `reasoning` nor `include`.
#[tokio::test]
async fn include_is_sent_only_for_reasoning_models() {
    for which in BOTH {
        let mut plain = which.model_config("http://unused");
        plain.reasoning = false;

        let (off, _) = one_request(
            which,
            Some(plain.clone()),
            vec![Message::user("a")],
            text_fixture(),
            |_| {},
        )
        .await;
        assert!(off.get("include").is_none(), "{which:?}: {off}");
        assert!(off.get("reasoning").is_none(), "{which:?}: {off}");

        let (effort, _) = one_request(
            which,
            Some(plain),
            vec![Message::user("a")],
            text_fixture(),
            |c| c.thinking_level = ThinkingLevel::Medium,
        )
        .await;
        assert_eq!(
            effort["reasoning"],
            json!({"effort": "medium"}),
            "{which:?}"
        );
        assert_eq!(
            effort["include"],
            json!(["reasoning.encrypted_content"]),
            "{which:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Through the agent loop
// ---------------------------------------------------------------------------

struct ReadFile(Arc<Mutex<Vec<Value>>>);

#[async_trait::async_trait]
impl AgentTool for ReadFile {
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
        json!({"type": "object", "properties": {"path": {"type": "string"}}})
    }
    async fn execute(&self, params: Value, _ctx: ToolContext) -> Result<ToolResult, ToolError> {
        self.0.lock().unwrap().push(params);
        Ok(ToolResult {
            content: vec![Content::Text {
                text: "hello".into(),
            }],
            details: Value::Null,
        })
    }
}

/// The agent loop's continuation after a tool call carries the reasoning item
/// back, in place, with the same cache key as the first request.
#[tokio::test]
async fn agent_loop_replays_the_reasoning_item_after_a_tool_call() {
    for which in BOTH {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(which.path()))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(reasoning_then_call_fixture(), "text/event-stream"),
            )
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(which.path()))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(text_fixture(), "text/event-stream"),
            )
            .with_priority(2)
            .mount(&server)
            .await;

        let calls = Arc::new(Mutex::new(Vec::new()));
        let mc = which.model_config(&server.uri());
        let mut agent = match which {
            Which::Responses => Agent::from_provider(OpenAiResponsesProvider, mc),
            Which::Azure => Agent::from_provider(AzureOpenAiProvider, mc),
        }
        .with_api_key("test-key")
        .with_system_prompt("You are a coding agent.")
        .with_tools(vec![Box::new(ReadFile(calls.clone()))]);
        let mut rx = agent.prompt("read a.txt").await;
        while rx.recv().await.is_some() {}
        agent.finish().await;

        assert_eq!(*calls.lock().unwrap(), vec![json!({"path": "a.txt"})]);
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2, "{which:?}");
        let (first, second) = (body(&requests[0]), body(&requests[1]));
        let input = second["input"].as_array().unwrap();
        let kinds: Vec<&str> = input
            .iter()
            .map(|i| i["type"].as_str().unwrap_or("user"))
            .collect();
        assert_eq!(
            kinds,
            ["user", "reasoning", "function_call", "function_call_output"],
            "{which:?}: {input:?}"
        );
        assert_eq!(input[1]["encrypted_content"], "gAAAA-enc-1", "{which:?}");
        assert!(first["prompt_cache_key"].is_string(), "{which:?}");
        assert_eq!(first["prompt_cache_key"], second["prompt_cache_key"]);
    }
}
