//! Tests for the core agent loop using MockProvider.

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use yoagent::agent_loop::{agent_loop, agent_loop_continue, AgentLoopConfig};
use yoagent::provider::mock::*;
use yoagent::provider::MockProvider;
use yoagent::*;

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
        cache_config: CacheConfig::default(),
        tool_execution: ToolExecutionStrategy::default(),
        retry_config: yoagent::RetryConfig::default(),
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
            _on_update: Option<ToolUpdateFn>,
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
            _on_update: Option<ToolUpdateFn>,
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

// ---------------------------------------------------------------------------
// Parallel tool execution tests
// ---------------------------------------------------------------------------

/// A tool that records execution timestamps to verify parallelism.
struct TimedTool {
    name: String,
    delay_ms: u64,
}

#[async_trait::async_trait]
impl AgentTool for TimedTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn label(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "Timed tool"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    async fn execute(
        &self,
        _id: &str,
        _params: serde_json::Value,
        _cancel: tokio_util::sync::CancellationToken,
        _on_update: Option<ToolUpdateFn>,
    ) -> Result<ToolResult, ToolError> {
        tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
        Ok(ToolResult {
            content: vec![Content::Text {
                text: format!("done:{}", self.name),
            }],
            details: serde_json::Value::Null,
        })
    }
}

#[tokio::test]
async fn test_parallel_tool_execution_faster_than_sequential() {
    // 3 tools each taking 50ms. Sequential = 150ms+, Parallel = ~50ms.
    let provider = MockProvider::new(vec![
        MockResponse::ToolCalls(vec![
            MockToolCall {
                name: "tool_a".into(),
                arguments: serde_json::json!({}),
            },
            MockToolCall {
                name: "tool_b".into(),
                arguments: serde_json::json!({}),
            },
            MockToolCall {
                name: "tool_c".into(),
                arguments: serde_json::json!({}),
            },
        ]),
        MockResponse::Text("All done.".into()),
    ]);

    let mut config = make_config(&provider);
    config.tool_execution = ToolExecutionStrategy::Parallel;

    let mut context = AgentContext {
        system_prompt: "test".into(),
        messages: Vec::new(),
        tools: vec![
            Box::new(TimedTool {
                name: "tool_a".into(),
                delay_ms: 50,
            }),
            Box::new(TimedTool {
                name: "tool_b".into(),
                delay_ms: 50,
            }),
            Box::new(TimedTool {
                name: "tool_c".into(),
                delay_ms: 50,
            }),
        ],
    };

    let prompt = AgentMessage::Llm(Message::user("Run all tools"));
    let (tx, rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();

    let start = std::time::Instant::now();
    let new_messages = agent_loop(vec![prompt], &mut context, &config, tx, cancel).await;
    let elapsed = start.elapsed();

    let events = collect_events(rx);

    // All 3 tool results should be present
    let tool_results: Vec<_> = new_messages
        .iter()
        .filter(|m| m.role() == "toolResult")
        .collect();
    assert_eq!(tool_results.len(), 3);

    // Should complete in roughly 50-100ms, not 150ms+
    assert!(
        elapsed.as_millis() < 130,
        "Parallel execution took {}ms, expected <130ms",
        elapsed.as_millis()
    );

    // Should have 3 ToolExecutionStart and 3 ToolExecutionEnd events
    let starts = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolExecutionStart { .. }))
        .count();
    let ends = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolExecutionEnd { .. }))
        .count();
    assert_eq!(starts, 3);
    assert_eq!(ends, 3);
}

#[tokio::test]
async fn test_sequential_tool_execution_is_slower() {
    let provider = MockProvider::new(vec![
        MockResponse::ToolCalls(vec![
            MockToolCall {
                name: "tool_a".into(),
                arguments: serde_json::json!({}),
            },
            MockToolCall {
                name: "tool_b".into(),
                arguments: serde_json::json!({}),
            },
        ]),
        MockResponse::Text("Done.".into()),
    ]);

    let mut config = make_config(&provider);
    config.tool_execution = ToolExecutionStrategy::Sequential;

    let mut context = AgentContext {
        system_prompt: "test".into(),
        messages: Vec::new(),
        tools: vec![
            Box::new(TimedTool {
                name: "tool_a".into(),
                delay_ms: 50,
            }),
            Box::new(TimedTool {
                name: "tool_b".into(),
                delay_ms: 50,
            }),
        ],
    };

    let prompt = AgentMessage::Llm(Message::user("Run tools"));
    let (tx, _rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();

    let start = std::time::Instant::now();
    let _new_messages = agent_loop(vec![prompt], &mut context, &config, tx, cancel).await;
    let elapsed = start.elapsed();

    // Sequential should take 100ms+ (2 × 50ms)
    assert!(
        elapsed.as_millis() >= 95,
        "Sequential execution took {}ms, expected >=95ms",
        elapsed.as_millis()
    );
}

#[tokio::test]
async fn test_batched_tool_execution() {
    // 4 tools, batch size 2: two batches of 2
    let provider = MockProvider::new(vec![
        MockResponse::ToolCalls(vec![
            MockToolCall {
                name: "tool_a".into(),
                arguments: serde_json::json!({}),
            },
            MockToolCall {
                name: "tool_b".into(),
                arguments: serde_json::json!({}),
            },
            MockToolCall {
                name: "tool_c".into(),
                arguments: serde_json::json!({}),
            },
            MockToolCall {
                name: "tool_d".into(),
                arguments: serde_json::json!({}),
            },
        ]),
        MockResponse::Text("All done.".into()),
    ]);

    let mut config = make_config(&provider);
    config.tool_execution = ToolExecutionStrategy::Batched { size: 2 };

    let mut context = AgentContext {
        system_prompt: "test".into(),
        messages: Vec::new(),
        tools: vec![
            Box::new(TimedTool {
                name: "tool_a".into(),
                delay_ms: 50,
            }),
            Box::new(TimedTool {
                name: "tool_b".into(),
                delay_ms: 50,
            }),
            Box::new(TimedTool {
                name: "tool_c".into(),
                delay_ms: 50,
            }),
            Box::new(TimedTool {
                name: "tool_d".into(),
                delay_ms: 50,
            }),
        ],
    };

    let prompt = AgentMessage::Llm(Message::user("Run all tools"));
    let (tx, rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();

    let start = std::time::Instant::now();
    let new_messages = agent_loop(vec![prompt], &mut context, &config, tx, cancel).await;
    let elapsed = start.elapsed();

    let _events = collect_events(rx);

    // All 4 results present
    let tool_results: Vec<_> = new_messages
        .iter()
        .filter(|m| m.role() == "toolResult")
        .collect();
    assert_eq!(tool_results.len(), 4);

    // 2 batches × 50ms = ~100ms (not 200ms sequential, not 50ms full parallel)
    assert!(
        elapsed.as_millis() >= 90 && elapsed.as_millis() < 160,
        "Batched execution took {}ms, expected 90-160ms",
        elapsed.as_millis()
    );
}

// ---------------------------------------------------------------------------
// Streaming tool output (on_update callback) tests
// ---------------------------------------------------------------------------

/// A tool that emits progress updates via on_update callback.
struct ProgressTool;

#[async_trait::async_trait]
impl AgentTool for ProgressTool {
    fn name(&self) -> &str {
        "progress_tool"
    }
    fn label(&self) -> &str {
        "Progress"
    }
    fn description(&self) -> &str {
        "A tool that streams progress"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({})
    }

    async fn execute(
        &self,
        _id: &str,
        _params: serde_json::Value,
        _cancel: tokio_util::sync::CancellationToken,
        on_update: Option<ToolUpdateFn>,
    ) -> Result<ToolResult, ToolError> {
        for i in 1..=3 {
            if let Some(ref cb) = on_update {
                cb(ToolResult {
                    content: vec![Content::Text {
                        text: format!("step {}/3", i),
                    }],
                    details: serde_json::Value::Null,
                });
            }
        }
        Ok(ToolResult {
            content: vec![Content::Text {
                text: "done".into(),
            }],
            details: serde_json::Value::Null,
        })
    }
}

#[tokio::test]
async fn test_tool_execution_update_events_emitted() {
    let provider = MockProvider::new(vec![
        MockResponse::ToolCalls(vec![MockToolCall {
            name: "progress_tool".into(),
            arguments: serde_json::json!({}),
        }]),
        MockResponse::Text("All done.".into()),
    ]);

    let config = make_config(&provider);

    let mut context = AgentContext {
        system_prompt: "test".into(),
        messages: Vec::new(),
        tools: vec![Box::new(ProgressTool)],
    };

    let prompt = AgentMessage::Llm(Message::user("go"));
    let (tx, rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();

    agent_loop(vec![prompt], &mut context, &config, tx, cancel).await;

    let events = collect_events(rx);

    let updates: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolExecutionUpdate { partial_result, .. } => {
                if let Some(Content::Text { text }) = partial_result.content.first() {
                    Some(text.clone())
                } else {
                    None
                }
            }
            _ => None,
        })
        .collect();

    assert_eq!(updates, vec!["step 1/3", "step 2/3", "step 3/3"]);
}

// ---------------------------------------------------------------------------
// Retry with backoff tests
// ---------------------------------------------------------------------------

/// A provider that fails N times with a given error, then delegates to a MockProvider.
struct FailThenSucceedProvider {
    fail_count: std::sync::atomic::AtomicUsize,
    max_failures: usize,
    error: ProviderError,
    inner: MockProvider,
}

use yoagent::provider::{ProviderError, StreamConfig, StreamEvent, StreamProvider};

#[async_trait::async_trait]
impl StreamProvider for FailThenSucceedProvider {
    async fn stream(
        &self,
        config: StreamConfig,
        tx: tokio::sync::mpsc::UnboundedSender<StreamEvent>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<yoagent::Message, ProviderError> {
        let attempt = self
            .fail_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if attempt < self.max_failures {
            return Err(match &self.error {
                ProviderError::RateLimited { retry_after_ms } => ProviderError::RateLimited {
                    retry_after_ms: *retry_after_ms,
                },
                ProviderError::Network(msg) => ProviderError::Network(msg.clone()),
                ProviderError::Auth(msg) => ProviderError::Auth(msg.clone()),
                other => ProviderError::Other(other.to_string()),
            });
        }
        self.inner.stream(config, tx, cancel).await
    }
}

#[tokio::test]
async fn test_retry_on_rate_limit_succeeds() {
    let provider = FailThenSucceedProvider {
        fail_count: std::sync::atomic::AtomicUsize::new(0),
        max_failures: 2,
        error: ProviderError::RateLimited {
            retry_after_ms: Some(10), // 10ms for fast tests
        },
        inner: MockProvider::text("Success after retries"),
    };

    let config = AgentLoopConfig {
        provider: &provider,
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
        cache_config: CacheConfig::default(),
        tool_execution: ToolExecutionStrategy::default(),
        retry_config: yoagent::RetryConfig {
            max_retries: 3,
            initial_delay_ms: 10,
            backoff_multiplier: 2.0,
            max_delay_ms: 100,
        },
    };

    let mut context = AgentContext {
        system_prompt: "test".into(),
        messages: Vec::new(),
        tools: Vec::new(),
    };

    let prompt = AgentMessage::Llm(Message::user("hi"));
    let (tx, rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();

    let new_messages = agent_loop(vec![prompt], &mut context, &config, tx, cancel).await;

    // Should have succeeded after 2 failures + 1 success
    assert_eq!(new_messages.len(), 2); // user + assistant
    let events = collect_events(rx);
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::AgentEnd { .. })));

    // Verify the provider was called 3 times (2 failures + 1 success)
    assert_eq!(
        provider
            .fail_count
            .load(std::sync::atomic::Ordering::SeqCst),
        3
    );
}

#[tokio::test]
async fn test_retry_exhausted_returns_error() {
    let provider = FailThenSucceedProvider {
        fail_count: std::sync::atomic::AtomicUsize::new(0),
        max_failures: 10, // more failures than retries
        error: ProviderError::Network("connection reset".into()),
        inner: MockProvider::text("never reached"),
    };

    let config = AgentLoopConfig {
        provider: &provider,
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
        cache_config: CacheConfig::default(),
        tool_execution: ToolExecutionStrategy::default(),
        retry_config: yoagent::RetryConfig {
            max_retries: 2,
            initial_delay_ms: 10,
            backoff_multiplier: 2.0,
            max_delay_ms: 100,
        },
    };

    let mut context = AgentContext {
        system_prompt: "test".into(),
        messages: Vec::new(),
        tools: Vec::new(),
    };

    let prompt = AgentMessage::Llm(Message::user("hi"));
    let (tx, _rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();

    let new_messages = agent_loop(vec![prompt], &mut context, &config, tx, cancel).await;

    // Should have an error message (StopReason::Error)
    let last = new_messages.last().unwrap();
    if let AgentMessage::Llm(Message::Assistant {
        stop_reason,
        error_message,
        ..
    }) = last
    {
        assert_eq!(*stop_reason, StopReason::Error);
        assert!(error_message.as_ref().unwrap().contains("connection reset"));
    } else {
        panic!("Expected error assistant message");
    }

    // 1 initial + 2 retries = 3 attempts
    assert_eq!(
        provider
            .fail_count
            .load(std::sync::atomic::Ordering::SeqCst),
        3
    );
}

#[tokio::test]
async fn test_no_retry_on_auth_error() {
    let provider = FailThenSucceedProvider {
        fail_count: std::sync::atomic::AtomicUsize::new(0),
        max_failures: 1,
        error: ProviderError::Auth("invalid key".into()),
        inner: MockProvider::text("never reached"),
    };

    let config = AgentLoopConfig {
        provider: &provider,
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
        cache_config: CacheConfig::default(),
        tool_execution: ToolExecutionStrategy::default(),
        retry_config: yoagent::RetryConfig::default(), // 3 retries, but auth is not retryable
    };

    let mut context = AgentContext {
        system_prompt: "test".into(),
        messages: Vec::new(),
        tools: Vec::new(),
    };

    let prompt = AgentMessage::Llm(Message::user("hi"));
    let (tx, _rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();

    agent_loop(vec![prompt], &mut context, &config, tx, cancel).await;

    // Should have been called exactly once — no retries for auth errors
    assert_eq!(
        provider
            .fail_count
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
}

#[tokio::test]
async fn test_retry_none_disables_retries() {
    let provider = FailThenSucceedProvider {
        fail_count: std::sync::atomic::AtomicUsize::new(0),
        max_failures: 1,
        error: ProviderError::RateLimited {
            retry_after_ms: None,
        },
        inner: MockProvider::text("never reached"),
    };

    let config = AgentLoopConfig {
        provider: &provider,
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
        cache_config: CacheConfig::default(),
        tool_execution: ToolExecutionStrategy::default(),
        retry_config: yoagent::RetryConfig::none(), // disabled
    };

    let mut context = AgentContext {
        system_prompt: "test".into(),
        messages: Vec::new(),
        tools: Vec::new(),
    };

    let prompt = AgentMessage::Llm(Message::user("hi"));
    let (tx, _rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();

    agent_loop(vec![prompt], &mut context, &config, tx, cancel).await;

    // Only 1 attempt — no retries
    assert_eq!(
        provider
            .fail_count
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
}

// ---------------------------------------------------------------------------
// Event streaming bug fix test
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_message_update_events_emitted_during_streaming() {
    // This test verifies the fix for: text deltas not emitted because
    // partial_message was None when deltas arrived (MessageStart was only
    // emitted on Done, after all deltas had already been processed).
    let provider = MockProvider::text("Hello, world!");
    let config = make_config(&provider);

    let mut context = AgentContext {
        system_prompt: "test".into(),
        messages: Vec::new(),
        tools: Vec::new(),
    };

    let prompt = AgentMessage::Llm(Message::user("hi"));
    let (tx, rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();

    agent_loop(vec![prompt], &mut context, &config, tx, cancel).await;

    let events = collect_events(rx);

    // Collect MessageUpdate text deltas
    let deltas: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::MessageUpdate {
                delta: StreamDelta::Text { delta },
                ..
            } => Some(delta.clone()),
            _ => None,
        })
        .collect();

    // Should have at least one text delta with "Hello, world!"
    assert!(
        !deltas.is_empty(),
        "Expected MessageUpdate events with text deltas, got none"
    );
    let full_text: String = deltas.into_iter().collect();
    assert_eq!(full_text, "Hello, world!");

    // Verify event ordering: MessageStart before MessageUpdate before MessageEnd
    let event_types: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::MessageStart { .. } => Some("Start"),
            AgentEvent::MessageUpdate { .. } => Some("Update"),
            AgentEvent::MessageEnd { .. } => Some("End"),
            _ => None,
        })
        .collect();

    // Should be: Start (user), End (user), Start (assistant), Update(s), End (assistant)
    // Find the assistant sequence
    let assistant_start = event_types.iter().rposition(|&e| e == "Start").unwrap();
    let assistant_end = event_types.iter().rposition(|&e| e == "End").unwrap();

    // All Updates should be between the last Start and last End
    for (i, &et) in event_types.iter().enumerate() {
        if et == "Update" {
            assert!(
                i > assistant_start && i < assistant_end,
                "MessageUpdate at index {} should be between MessageStart ({}) and MessageEnd ({})",
                i,
                assistant_start,
                assistant_end
            );
        }
    }
}
