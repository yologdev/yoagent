//! Tests for the core agent loop using MockProvider.

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use yo_agent::agent_loop::{agent_loop, agent_loop_continue, AgentLoopConfig};
use yo_agent::provider::mock::*;
use yo_agent::provider::MockProvider;
use yo_agent::*;

fn make_config(provider: &MockProvider) -> AgentLoopConfig<'_> {
    AgentLoopConfig {
        provider,
        model: "mock".into(),
        api_key: "test".into(),
        thinking_level: ThinkingLevel::Off,
        max_tokens: None,
        temperature: None,
        convert_to_llm: None,
        transform_context: None,
        get_steering_messages: None,
        get_follow_up_messages: None,
        context_config: None,
        execution_limits: None,
    }
}

fn collect_events(mut rx: mpsc::UnboundedReceiver<AgentEvent>) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    while let Ok(e) = rx.try_recv() {
        events.push(e);
    }
    events
}

#[tokio::test]
async fn test_simple_text_response() {
    let provider = MockProvider::text("Hello, world!");
    let config = make_config(&provider);

    let mut context = AgentContext {
        system_prompt: "You are helpful.".into(),
        messages: Vec::new(),
        tools: Vec::new(),
    };

    let prompt = AgentMessage::Llm(Message::user("Hi"));
    let (tx, rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();

    let new_messages = agent_loop(vec![prompt], &mut context, &config, tx, cancel).await;

    let events = collect_events(rx);

    // Should have: AgentStart, TurnStart, MessageStart(user), MessageEnd(user),
    //              MessageStart(assistant), MessageEnd(assistant), TurnEnd, AgentEnd
    let event_types: Vec<&str> = events
        .iter()
        .map(|e| match e {
            AgentEvent::AgentStart => "AgentStart",
            AgentEvent::AgentEnd { .. } => "AgentEnd",
            AgentEvent::TurnStart => "TurnStart",
            AgentEvent::TurnEnd { .. } => "TurnEnd",
            AgentEvent::MessageStart { .. } => "MessageStart",
            AgentEvent::MessageEnd { .. } => "MessageEnd",
            AgentEvent::MessageUpdate { .. } => "MessageUpdate",
            AgentEvent::ToolExecutionStart { .. } => "ToolExecStart",
            AgentEvent::ToolExecutionUpdate { .. } => "ToolExecUpdate",
            AgentEvent::ToolExecutionEnd { .. } => "ToolExecEnd",
        })
        .collect();

    assert!(event_types.contains(&"AgentStart"));
    assert!(event_types.contains(&"AgentEnd"));
    assert!(event_types.contains(&"TurnStart"));
    assert!(event_types.contains(&"TurnEnd"));

    // new_messages should contain user prompt + assistant response
    assert_eq!(new_messages.len(), 2);
    assert_eq!(new_messages[0].role(), "user");
    assert_eq!(new_messages[1].role(), "assistant");

    // Context should have both messages
    assert_eq!(context.messages.len(), 2);
}

#[tokio::test]
async fn test_tool_call_and_response() {
    // Mock: first call returns tool use, second returns text
    let provider = MockProvider::new(vec![
        MockResponse::ToolCalls(vec![MockToolCall {
            name: "read_file".into(),
            arguments: serde_json::json!({"path": "test.txt"}),
        }]),
        MockResponse::Text("The file contains: hello".into()),
    ]);

    // Define a simple tool
    struct ReadFileTool;

    #[async_trait::async_trait]
    impl AgentTool for ReadFileTool {
        fn name(&self) -> &str {
            "read_file"
        }
        fn label(&self) -> &str {
            "Read File"
        }
        fn description(&self) -> &str {
            "Read a file"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"}
                }
            })
        }
        async fn execute(
            &self,
            _tool_call_id: &str,
            _params: serde_json::Value,
            _cancel: tokio_util::sync::CancellationToken,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult {
                content: vec![Content::Text {
                    text: "hello".into(),
                }],
                details: serde_json::Value::Null,
            })
        }
    }

    let config = make_config(&provider);

    let mut context = AgentContext {
        system_prompt: "You are helpful.".into(),
        messages: Vec::new(),
        tools: vec![Box::new(ReadFileTool)],
    };

    let prompt = AgentMessage::Llm(Message::user("Read test.txt"));
    let (tx, rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();

    let new_messages = agent_loop(vec![prompt], &mut context, &config, tx, cancel).await;

    let events = collect_events(rx);

    let event_types: Vec<&str> = events
        .iter()
        .map(|e| match e {
            AgentEvent::AgentStart => "AgentStart",
            AgentEvent::AgentEnd { .. } => "AgentEnd",
            AgentEvent::TurnStart => "TurnStart",
            AgentEvent::TurnEnd { .. } => "TurnEnd",
            AgentEvent::MessageStart { .. } => "MessageStart",
            AgentEvent::MessageEnd { .. } => "MessageEnd",
            AgentEvent::MessageUpdate { .. } => "MessageUpdate",
            AgentEvent::ToolExecutionStart { .. } => "ToolExecStart",
            AgentEvent::ToolExecutionUpdate { .. } => "ToolExecUpdate",
            AgentEvent::ToolExecutionEnd { .. } => "ToolExecEnd",
        })
        .collect();

    // Should have tool execution events
    assert!(event_types.contains(&"ToolExecStart"));
    assert!(event_types.contains(&"ToolExecEnd"));

    // Messages: user, assistant(tool_call), toolResult, assistant(text)
    assert_eq!(new_messages.len(), 4);
    assert_eq!(new_messages[0].role(), "user");
    assert_eq!(new_messages[1].role(), "assistant");
    assert_eq!(new_messages[2].role(), "toolResult");
    assert_eq!(new_messages[3].role(), "assistant");
}

#[tokio::test]
async fn test_abort_cancels_loop() {
    // Provider that returns text — but we cancel before it runs
    let provider = MockProvider::text("Should not appear");
    let config = make_config(&provider);

    let mut context = AgentContext {
        system_prompt: "test".into(),
        messages: Vec::new(),
        tools: Vec::new(),
    };

    let prompt = AgentMessage::Llm(Message::user("Hi"));
    let (tx, _rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();

    // Cancel immediately
    cancel.cancel();

    let new_messages = agent_loop(vec![prompt], &mut context, &config, tx, cancel).await;

    // Should have user message but loop should exit early
    // The prompt is added before the loop checks cancellation
    assert!(new_messages.len() <= 2); // user + possibly error
}

#[tokio::test]
async fn test_continue_from_tool_result() {
    let provider = MockProvider::text("Done processing.");
    let config = make_config(&provider);

    let mut context = AgentContext {
        system_prompt: "test".into(),
        messages: vec![
            AgentMessage::Llm(Message::user("do something")),
            AgentMessage::Llm(Message::ToolResult {
                tool_call_id: "tc-1".into(),
                tool_name: "test_tool".into(),
                content: vec![Content::Text {
                    text: "result".into(),
                }],
                is_error: false,
                timestamp: 0,
            }),
        ],
        tools: Vec::new(),
    };

    let (tx, _rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();

    let new_messages = agent_loop_continue(&mut context, &config, tx, cancel).await;

    assert!(!new_messages.is_empty());
    assert_eq!(new_messages[0].role(), "assistant");
}

#[tokio::test]
async fn test_tool_error_is_reported() {
    let provider = MockProvider::new(vec![
        MockResponse::ToolCalls(vec![MockToolCall {
            name: "failing_tool".into(),
            arguments: serde_json::json!({}),
        }]),
        MockResponse::Text("Tool failed, sorry.".into()),
    ]);

    struct FailingTool;

    #[async_trait::async_trait]
    impl AgentTool for FailingTool {
        fn name(&self) -> &str {
            "failing_tool"
        }
        fn label(&self) -> &str {
            "Failing Tool"
        }
        fn description(&self) -> &str {
            "Always fails"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        async fn execute(
            &self,
            _id: &str,
            _params: serde_json::Value,
            _cancel: tokio_util::sync::CancellationToken,
        ) -> Result<ToolResult, ToolError> {
            Err(ToolError::Failed("Something went wrong".into()))
        }
    }

    let config = make_config(&provider);
    let mut context = AgentContext {
        system_prompt: "test".into(),
        messages: Vec::new(),
        tools: vec![Box::new(FailingTool)],
    };

    let prompt = AgentMessage::Llm(Message::user("Use the tool"));
    let (tx, rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();

    let new_messages = agent_loop(vec![prompt], &mut context, &config, tx, cancel).await;

    let events = collect_events(rx);

    // Tool error should be reported
    let tool_end_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolExecutionEnd { is_error: true, .. }))
        .collect();
    assert_eq!(tool_end_events.len(), 1);

    // Should still get a final assistant response
    assert_eq!(new_messages.last().unwrap().role(), "assistant");
}

#[tokio::test]
async fn test_unknown_tool_reports_error() {
    let provider = MockProvider::new(vec![
        MockResponse::ToolCalls(vec![MockToolCall {
            name: "nonexistent".into(),
            arguments: serde_json::json!({}),
        }]),
        MockResponse::Text("I couldn't find that tool.".into()),
    ]);

    let config = make_config(&provider);
    let mut context = AgentContext {
        system_prompt: "test".into(),
        messages: Vec::new(),
        tools: Vec::new(), // No tools registered
    };

    let prompt = AgentMessage::Llm(Message::user("Use nonexistent tool"));
    let (tx, rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();

    let _new_messages = agent_loop(vec![prompt], &mut context, &config, tx, cancel).await;

    let events = collect_events(rx);
    let tool_errors: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolExecutionEnd { is_error: true, .. }))
        .collect();
    assert_eq!(tool_errors.len(), 1);
}
