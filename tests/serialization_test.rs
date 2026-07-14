//! Serde round-trip tests for core types.

use yoagent::*;

fn roundtrip<T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug>(
    value: &T,
) {
    let json = serde_json::to_string(value).expect("serialize");
    let back: T = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(*value, back);
}

// ---------------------------------------------------------------------------
// Message variants
// ---------------------------------------------------------------------------

#[test]
fn test_message_user_roundtrip() {
    let msg = Message::User {
        content: vec![Content::Text {
            text: "Hello".into(),
        }],
        timestamp: 123456,
    };
    roundtrip(&msg);
}

#[test]
fn test_message_assistant_roundtrip() {
    let msg = Message::assistant(
        vec![
            Content::Text {
                text: "Hi there".into(),
            },
            Content::tool_call("tc-1", "read_file", serde_json::json!({"path": "foo.rs"})),
        ],
        StopReason::ToolUse,
        "claude-sonnet",
        "anthropic",
        Usage {
            input: 100,
            output: 50,
            cache_read: 10,
            cache_write: 5,
            total_tokens: 165,
        },
    )
    .with_timestamp(789);
    roundtrip(&msg);
}

#[test]
fn test_message_tool_result_roundtrip() {
    let msg = Message::ToolResult {
        tool_call_id: "tc-1".into(),
        tool_name: "bash".into(),
        content: vec![Content::Text {
            text: "exit code 0".into(),
        }],
        is_error: false,
        timestamp: 999,
    };
    roundtrip(&msg);
}

// ---------------------------------------------------------------------------
// AgentMessage
// ---------------------------------------------------------------------------

#[test]
fn test_agent_message_roundtrip() {
    let am = AgentMessage::Llm(Message::user("test prompt"));
    roundtrip(&am);
}

#[test]
fn test_extension_message_roundtrip() {
    let ext = ExtensionMessage::new("status_update", serde_json::json!({"status": "running"}));
    roundtrip(&ext);

    let am = AgentMessage::Extension(ext);
    roundtrip(&am);
}

// ---------------------------------------------------------------------------
// Content variants
// ---------------------------------------------------------------------------

#[test]
fn test_content_variants_roundtrip() {
    roundtrip(&Content::Text {
        text: "hello".into(),
    });
    roundtrip(&Content::Image {
        data: "base64data".into(),
        mime_type: "image/png".into(),
    });
    roundtrip(&Content::thinking_signed("let me think...", "sig123"));
    roundtrip(&Content::thinking("unsigned thought"));
    roundtrip(&Content::tool_call(
        "tc-1",
        "bash",
        serde_json::json!({"command": "ls"}),
    ));
}

// ---------------------------------------------------------------------------
// Full conversation
// ---------------------------------------------------------------------------

#[test]
fn test_full_conversation_roundtrip() {
    let conversation: Vec<AgentMessage> = vec![
        AgentMessage::Llm(Message::user("Read the file")),
        AgentMessage::Llm(
            Message::assistant(
                vec![Content::tool_call(
                    "tc-1",
                    "read_file",
                    serde_json::json!({"path": "main.rs"}),
                )],
                StopReason::ToolUse,
                "mock",
                "mock",
                Usage::default(),
            )
            .with_timestamp(100),
        ),
        AgentMessage::Llm(Message::ToolResult {
            tool_call_id: "tc-1".into(),
            tool_name: "read_file".into(),
            content: vec![Content::Text {
                text: "fn main() {}".into(),
            }],
            is_error: false,
            timestamp: 200,
        }),
        AgentMessage::Llm(
            Message::assistant(
                vec![Content::Text {
                    text: "The file contains a main function.".into(),
                }],
                StopReason::Stop,
                "mock",
                "mock",
                Usage::default(),
            )
            .with_timestamp(300),
        ),
        AgentMessage::Extension(ExtensionMessage::new(
            "ui_event",
            serde_json::json!({"action": "scroll"}),
        )),
    ];

    let json = serde_json::to_string(&conversation).expect("serialize");
    let back: Vec<AgentMessage> = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(conversation, back);
}

// ---------------------------------------------------------------------------
// Config types
// ---------------------------------------------------------------------------

#[test]
fn test_execution_limits_roundtrip() {
    use yoagent::context::ExecutionLimits;
    let limits = ExecutionLimits {
        max_turns: 25,
        max_total_tokens: 500_000,
        max_duration: std::time::Duration::from_secs(300),
    };
    let json = serde_json::to_string(&limits).expect("serialize");
    let back: ExecutionLimits = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(limits.max_turns, back.max_turns);
    assert_eq!(limits.max_total_tokens, back.max_total_tokens);
    assert_eq!(limits.max_duration, back.max_duration);
}

#[test]
fn test_tool_execution_strategy_roundtrip() {
    roundtrip(&ToolExecutionStrategy::Sequential);
    roundtrip(&ToolExecutionStrategy::Parallel);
    roundtrip(&ToolExecutionStrategy::Batched { size: 4 });
}

#[test]
fn test_refusal_stop_reason_round_trip() {
    use yoagent::types::*;

    let message = Message::assistant(
        vec![],
        StopReason::Refusal,
        "claude-fable-5",
        "anthropic",
        Usage::default(),
    )
    .with_timestamp(1)
    .with_error_message("declined");

    let json = serde_json::to_string(&message).unwrap();
    let back: Message = serde_json::from_str(&json).unwrap();
    assert_eq!(message, back);
    assert_eq!(StopReason::Refusal.to_string(), "refusal");
}

#[test]
fn test_constructors_pin_fields() {
    // The non_exhaustive variants make these the mandated construction path
    // for downstream crates — pin every field explicitly.
    let tc = Content::tool_call("id-1", "bash", serde_json::json!({"cmd": "ls"}));
    let Content::ToolCall {
        id,
        name,
        arguments,
        provider_metadata,
        ..
    } = &tc
    else {
        panic!("expected tool call");
    };
    assert_eq!(id, "id-1");
    assert_eq!(name, "bash");
    assert_eq!(arguments["cmd"], "ls");
    assert!(provider_metadata.is_none());

    let think = Content::thinking_signed("hmm", "sig");
    let Content::Thinking {
        thinking,
        signature,
        ..
    } = &think
    else {
        panic!("expected thinking");
    };
    assert_eq!(thinking, "hmm");
    assert_eq!(signature.as_deref(), Some("sig"));

    let msg = Message::assistant(
        vec![Content::Text { text: "hi".into() }],
        StopReason::Stop,
        "model-x",
        "provider-y",
        Usage::default(),
    )
    .with_timestamp(42)
    .with_error_message("oops");
    let Message::Assistant {
        model,
        provider,
        timestamp,
        error_message,
        stop_reason,
        ..
    } = &msg
    else {
        panic!("expected assistant");
    };
    assert_eq!(model, "model-x");
    assert_eq!(provider, "provider-y");
    assert_eq!(*timestamp, 42);
    assert_eq!(error_message.as_deref(), Some("oops"));
    assert_eq!(*stop_reason, StopReason::Stop);
}

#[test]
fn test_tool_call_with_metadata_roundtrip() {
    // Thought signatures must survive session persistence.
    roundtrip(&Content::tool_call_with_metadata(
        "tc-9",
        "get_weather",
        serde_json::json!({"city": "Paris"}),
        serde_json::json!({"thought_signature": "sig-xyz"}),
    ));
}

// ---------------------------------------------------------------------------
// AgentEvent wire format (public contract — see the doc comment on AgentEvent)
// ---------------------------------------------------------------------------

fn sample_assistant() -> AgentMessage {
    AgentMessage::Llm(Message::assistant(
        vec![Content::Text { text: "hi".into() }],
        StopReason::Stop,
        "claude-sonnet",
        "anthropic",
        Usage::default(),
    ))
}

fn sample_tool_result() -> ToolResult {
    ToolResult {
        content: vec![Content::Text {
            text: "exit code 0".into(),
        }],
        details: serde_json::json!({"exit_code": 0}),
    }
}

/// One value of every `AgentEvent` variant.
fn all_agent_events() -> Vec<AgentEvent> {
    vec![
        AgentEvent::AgentStart,
        AgentEvent::AgentEnd {
            messages: vec![sample_assistant()],
        },
        AgentEvent::TurnStart,
        AgentEvent::TurnEnd {
            message: sample_assistant(),
            tool_results: vec![Message::ToolResult {
                tool_call_id: "tc-1".into(),
                tool_name: "bash".into(),
                content: vec![Content::Text { text: "ok".into() }],
                is_error: false,
                timestamp: 7,
            }],
        },
        AgentEvent::MessageStart {
            message: sample_assistant(),
        },
        AgentEvent::MessageUpdate {
            message: sample_assistant(),
            delta: StreamDelta::Text { delta: "hi".into() },
        },
        AgentEvent::MessageEnd {
            message: sample_assistant(),
        },
        AgentEvent::ToolExecutionStart {
            tool_call_id: "tc-1".into(),
            tool_name: "bash".into(),
            args: serde_json::json!({"command": "ls"}),
        },
        AgentEvent::ToolExecutionUpdate {
            tool_call_id: "tc-1".into(),
            tool_name: "bash".into(),
            partial_result: sample_tool_result(),
        },
        AgentEvent::ToolExecutionEnd {
            tool_call_id: "tc-1".into(),
            tool_name: "bash".into(),
            result: sample_tool_result(),
            is_error: false,
        },
        AgentEvent::ProgressMessage {
            tool_call_id: "tc-1".into(),
            tool_name: "bash".into(),
            text: "50% done".into(),
        },
        AgentEvent::InputRejected {
            reason: "injection detected".into(),
        },
    ]
}

#[test]
fn test_agent_event_every_variant_roundtrips() {
    for event in all_agent_events() {
        roundtrip(&event);
    }
}

#[test]
fn test_stream_delta_every_variant_roundtrips() {
    roundtrip(&StreamDelta::Text { delta: "a".into() });
    roundtrip(&StreamDelta::Thinking { delta: "b".into() });
    roundtrip(&StreamDelta::ToolCallDelta { delta: "c".into() });
}

/// Freezes the `"type"` discriminant of every variant. A tag change here is a
/// breaking change for wire clients — do not update this list casually.
#[test]
fn test_agent_event_type_tags_are_frozen() {
    let expected = [
        "agentStart",
        "agentEnd",
        "turnStart",
        "turnEnd",
        "messageStart",
        "messageUpdate",
        "messageEnd",
        "toolExecutionStart",
        "toolExecutionUpdate",
        "toolExecutionEnd",
        "progressMessage",
        "inputRejected",
    ];
    let events = all_agent_events();
    assert_eq!(events.len(), expected.len());
    for (event, tag) in events.iter().zip(expected) {
        let v: serde_json::Value = serde_json::to_value(event).expect("serialize");
        assert_eq!(v["type"], *tag, "tag drifted for {event:?}");
    }
}

/// Shape snapshot: camelCase field names on the wire (`rename_all_fields`).
#[test]
fn test_agent_event_fields_are_camel_case() {
    let end = AgentEvent::ToolExecutionEnd {
        tool_call_id: "tc-1".into(),
        tool_name: "bash".into(),
        result: sample_tool_result(),
        is_error: true,
    };
    let v = serde_json::to_value(&end).expect("serialize");
    assert_eq!(v["toolCallId"], "tc-1");
    assert_eq!(v["toolName"], "bash");
    assert_eq!(v["isError"], true);
    assert!(
        v.get("tool_call_id").is_none(),
        "snake_case leaked onto the wire"
    );

    let update = AgentEvent::MessageUpdate {
        message: sample_assistant(),
        delta: StreamDelta::Text { delta: "hi".into() },
    };
    let v = serde_json::to_value(&update).expect("serialize");
    assert_eq!(v["type"], "messageUpdate");
    assert_eq!(v["delta"]["type"], "text");
    assert_eq!(v["delta"]["delta"], "hi");
    assert!(v.get("message").is_some());

    let rejected = AgentEvent::InputRejected {
        reason: "nope".into(),
    };
    let v = serde_json::to_value(&rejected).expect("serialize");
    assert_eq!(v["reason"], "nope");
}

/// Unit variants carry only the tag: `{"type":"agentStart"}`.
#[test]
fn test_agent_event_unit_variant_shape() {
    let json = serde_json::to_string(&AgentEvent::AgentStart).expect("serialize");
    assert_eq!(json, r#"{"type":"agentStart"}"#);
    let json = serde_json::to_string(&AgentEvent::TurnStart).expect("serialize");
    assert_eq!(json, r#"{"type":"turnStart"}"#);
}

/// A wire client's inbound path: parse an event from a raw JSON line.
#[test]
fn test_agent_event_deserializes_from_raw_json() {
    let line = r#"{"type":"toolExecutionStart","toolCallId":"tc-9","toolName":"read","args":{"path":"a.rs"}}"#;
    let event: AgentEvent = serde_json::from_str(line).expect("deserialize");
    let AgentEvent::ToolExecutionStart {
        tool_call_id,
        tool_name,
        args,
    } = event
    else {
        panic!("expected ToolExecutionStart");
    };
    assert_eq!(tool_call_id, "tc-9");
    assert_eq!(tool_name, "read");
    assert_eq!(args["path"], "a.rs");
}
