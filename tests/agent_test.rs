//! Tests for the Agent struct (stateful wrapper).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use yoagent::agent::Agent;
use yoagent::provider::mock::*;
use yoagent::provider::MockProvider;
use yoagent::*;

#[tokio::test]
async fn test_agent_simple_prompt() {
    let provider = MockProvider::text("Hello!");
    let mut agent = Agent::new(provider)
        .with_system_prompt("You are helpful.")
        .with_model("mock")
        .with_api_key("test");

    let mut rx = agent.prompt("Hi there").await;

    // Drain events
    let mut events = Vec::new();
    while let Some(e) = rx.recv().await {
        events.push(e);
    }

    agent.finish().await;
    assert!(!events.is_empty());
    assert_eq!(agent.messages().len(), 2); // user + assistant
}

#[tokio::test]
async fn test_agent_reset() {
    let provider = MockProvider::text("Hello!");
    let mut agent = Agent::new(provider)
        .with_system_prompt("test")
        .with_model("mock")
        .with_api_key("test");

    let mut rx = agent.prompt("Hi").await;
    while rx.recv().await.is_some() {}
    agent.finish().await;
    assert!(!agent.messages().is_empty());

    agent.reset().await;
    assert!(agent.messages().is_empty());
    assert!(!agent.is_streaming());
}

#[tokio::test]
async fn test_agent_with_tools() {
    struct EchoTool;

    #[async_trait::async_trait]
    impl AgentTool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn label(&self) -> &str {
            "Echo"
        }
        fn description(&self) -> &str {
            "Echoes input"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}}})
        }
        async fn execute(
            &self,
            params: serde_json::Value,
            _ctx: ToolContext,
        ) -> Result<ToolResult, ToolError> {
            let text = params["text"].as_str().unwrap_or("").to_string();
            Ok(ToolResult {
                content: vec![Content::Text { text }],
                details: serde_json::Value::Null,
            })
        }
    }

    let provider = MockProvider::new(vec![
        MockResponse::ToolCalls(vec![MockToolCall {
            provider_metadata: None,
            name: "echo".into(),
            arguments: serde_json::json!({"text": "hello"}),
        }]),
        MockResponse::Text("Echoed: hello".into()),
    ]);

    let mut agent = Agent::new(provider)
        .with_system_prompt("test")
        .with_model("mock")
        .with_api_key("test")
        .with_tools(vec![Box::new(EchoTool)]);

    let mut rx = agent.prompt("Echo hello").await;
    while rx.recv().await.is_some() {}
    agent.finish().await;

    // user + assistant(tool_call) + toolResult + assistant(text)
    assert_eq!(agent.messages().len(), 4);
}

#[tokio::test]
async fn test_agent_builder_pattern() {
    let provider = MockProvider::text("ok");
    let agent = Agent::new(provider)
        .with_system_prompt("sys")
        .with_model("test-model")
        .with_api_key("key123")
        .with_thinking(ThinkingLevel::Medium)
        .with_max_tokens(4096);

    assert_eq!(agent.system_prompt, "sys");
    assert_eq!(agent.model, "test-model");
    assert_eq!(agent.api_key, "key123");
    assert_eq!(agent.thinking_level, ThinkingLevel::Medium);
    assert_eq!(agent.max_tokens, Some(4096));
}

// ---------------------------------------------------------------------------
// State persistence tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_with_messages_builder() {
    let saved = vec![
        AgentMessage::Llm(Message::user("Hello")),
        AgentMessage::Llm(Message::assistant(
            vec![Content::Text {
                text: "Hi there!".into(),
            }],
            StopReason::Stop,
            "mock",
            "mock",
            Usage::default(),
        )),
    ];

    let provider = MockProvider::text("ok");
    let agent = Agent::new(provider)
        .with_model("mock")
        .with_api_key("test")
        .with_messages(saved.clone());

    assert_eq!(agent.messages().len(), 2);
    assert_eq!(*agent.messages(), saved[..]);
}

#[tokio::test]
async fn test_save_and_restore_messages() {
    let provider = MockProvider::text("Hello!");
    let mut agent = Agent::new(provider)
        .with_system_prompt("test")
        .with_model("mock")
        .with_api_key("test");

    let mut rx = agent.prompt("Hi").await;
    while rx.recv().await.is_some() {}
    agent.finish().await;
    let json = agent.save_messages().expect("save should succeed");

    // Create a fresh agent and restore
    let provider2 = MockProvider::text("ok");
    let mut agent2 = Agent::new(provider2)
        .with_system_prompt("test")
        .with_model("mock")
        .with_api_key("test");

    agent2
        .restore_messages(&json)
        .expect("restore should succeed");
    assert_eq!(agent.messages(), agent2.messages());
}

#[tokio::test]
async fn test_agent_continues_after_restore() {
    // First agent: prompt → get response → save
    let provider1 = MockProvider::text("First response");
    let mut agent1 = Agent::new(provider1)
        .with_system_prompt("test")
        .with_model("mock")
        .with_api_key("test");

    let mut rx = agent1.prompt("Hello").await;
    while rx.recv().await.is_some() {}
    agent1.finish().await;
    let json = agent1.save_messages().expect("save");

    // Second agent: restore → prompt again
    // The MockProvider will receive the full restored history + new prompt
    let provider2 = MockProvider::text("Second response");
    let mut agent2 = Agent::new(provider2)
        .with_system_prompt("test")
        .with_model("mock")
        .with_api_key("test");

    agent2.restore_messages(&json).expect("restore");
    let mut rx = agent2.prompt("Follow up").await;
    while rx.recv().await.is_some() {}
    agent2.finish().await;

    // Should have: original user + original assistant + follow-up user + new assistant
    assert_eq!(agent2.messages().len(), 4);
    assert_eq!(agent2.messages()[0].role(), "user");
    assert_eq!(agent2.messages()[1].role(), "assistant");
    assert_eq!(agent2.messages()[2].role(), "user");
    assert_eq!(agent2.messages()[3].role(), "assistant");
}

// ---------------------------------------------------------------------------
// Real-time streaming tests (prompt_with_sender / prompt_messages_with_sender)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_prompt_with_sender_streams_events() {
    let provider = MockProvider::text("Hello!");
    let mut agent = Agent::new(provider)
        .with_system_prompt("test")
        .with_model("mock")
        .with_api_key("test");

    let (tx, mut rx) = mpsc::unbounded_channel();
    let event_count = Arc::new(AtomicUsize::new(0));
    let count_clone = event_count.clone();

    let consumer = tokio::spawn(async move {
        while let Some(_event) = rx.recv().await {
            count_clone.fetch_add(1, Ordering::SeqCst);
        }
    });

    agent.prompt_with_sender("Hi there", tx).await;

    // tx is dropped when prompt_with_sender returns, so consumer will finish
    consumer.await.unwrap();

    assert!(event_count.load(Ordering::SeqCst) > 0);
    assert_eq!(agent.messages().len(), 2); // user + assistant
    assert!(!agent.is_streaming());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_prompt_with_sender_real_time_streaming() {
    let provider = MockProvider::text("Hello!");
    let mut agent = Agent::new(provider)
        .with_system_prompt("test")
        .with_model("mock")
        .with_api_key("test");

    let (tx, mut rx) = mpsc::unbounded_channel();
    let received_during = Arc::new(AtomicUsize::new(0));
    let received_clone = received_during.clone();

    // On a multi-threaded runtime, the consumer can run concurrently
    let consumer = tokio::spawn(async move {
        while let Some(_event) = rx.recv().await {
            received_clone.fetch_add(1, Ordering::SeqCst);
        }
    });

    agent.prompt_with_sender("Hello", tx).await;
    consumer.await.unwrap();

    // Events were consumed by the concurrent task
    assert!(received_during.load(Ordering::SeqCst) > 0);
    assert_eq!(agent.messages().len(), 2);
}

#[tokio::test]
async fn test_prompt_messages_with_sender() {
    let provider = MockProvider::text("Response");
    let mut agent = Agent::new(provider)
        .with_system_prompt("test")
        .with_model("mock")
        .with_api_key("test");

    let (tx, mut rx) = mpsc::unbounded_channel();

    let consumer = tokio::spawn(async move {
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        events
    });

    let msgs = vec![AgentMessage::Llm(Message::user("Hello"))];
    agent.prompt_messages_with_sender(msgs, tx).await;

    let events = consumer.await.unwrap();
    assert!(!events.is_empty());
    assert_eq!(agent.messages().len(), 2);
}

#[tokio::test]
async fn test_continue_loop_with_sender() {
    let provider = MockProvider::text("Continued response");
    let mut agent = Agent::new(provider)
        .with_system_prompt("test")
        .with_model("mock")
        .with_api_key("test");

    // First, add some messages to continue from (last must not be assistant)
    agent.append_message(AgentMessage::Llm(Message::user("Hello")));
    agent.append_message(AgentMessage::Llm(
        Message::assistant(
            vec![Content::Text { text: "Hi!".into() }],
            StopReason::Error,
            "mock",
            "mock",
            Usage::default(),
        )
        .with_error_message("rate limited"),
    ));
    agent.append_message(AgentMessage::Llm(Message::user("Please try again")));

    let (tx, mut rx) = mpsc::unbounded_channel();

    let consumer = tokio::spawn(async move {
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        events
    });

    agent.continue_loop_with_sender(tx).await;

    let events = consumer.await.unwrap();
    assert!(!events.is_empty());
    assert!(!agent.is_streaming());
}

#[tokio::test]
async fn test_prompt_with_sender_tools_restored() {
    struct DummyTool;

    #[async_trait::async_trait]
    impl AgentTool for DummyTool {
        fn name(&self) -> &str {
            "dummy"
        }
        fn label(&self) -> &str {
            "Dummy"
        }
        fn description(&self) -> &str {
            "A dummy tool"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(
            &self,
            _params: serde_json::Value,
            _ctx: ToolContext,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult {
                content: vec![Content::Text { text: "ok".into() }],
                details: serde_json::Value::Null,
            })
        }
    }

    let provider = MockProvider::text("Hello!");
    let mut agent = Agent::new(provider)
        .with_system_prompt("test")
        .with_model("mock")
        .with_api_key("test")
        .with_tools(vec![Box::new(DummyTool)]);

    let (tx, mut rx) = mpsc::unbounded_channel();
    let consumer = tokio::spawn(async move { while rx.recv().await.is_some() {} });

    agent.prompt_with_sender("Hi", tx).await;
    consumer.await.unwrap();

    // Tools should be restored after the call
    assert!(!agent.is_streaming());
    // Agent should still work for another prompt
    let mut rx2 = agent.prompt("Follow up").await;
    while rx2.recv().await.is_some() {}
    agent.finish().await;
    assert_eq!(agent.messages().len(), 4); // 2 from first + 2 from second
}

#[tokio::test]
async fn test_queue_inspection_and_take() {
    let provider = MockProvider::text("Hello!");
    let agent = Agent::new(provider)
        .with_system_prompt("test")
        .with_model("mock")
        .with_api_key("test");

    assert_eq!(agent.steering_queue_len(), 0);
    assert!(agent.steering_queue_snapshot().is_empty());

    agent.steer(AgentMessage::Llm(Message::user("stop")));
    agent.steer(AgentMessage::Llm(Message::user("use v2 instead")));
    agent.follow_up(AgentMessage::Llm(Message::user("then run tests")));

    // Inspection does not consume
    assert_eq!(agent.follow_up_queue_len(), 1);
    let snapshot = agent.steering_queue_snapshot();
    assert_eq!(snapshot.len(), 2);
    assert_eq!(agent.steering_queue_len(), 2, "snapshot must not drain");

    // Take drains atomically and returns in FIFO order
    let taken = agent.take_steering_queue();
    assert_eq!(taken.len(), 2);
    assert_eq!(agent.steering_queue_len(), 0);
    let AgentMessage::Llm(Message::User { content, .. }) = &taken[0] else {
        panic!("expected user message");
    };
    assert_eq!(
        content,
        &vec![Content::Text {
            text: "stop".into()
        }]
    );

    // Edit-and-requeue: drop the first entry, batch-requeue the survivor
    agent.steer_all(vec![taken[1].clone()]);
    assert_eq!(agent.steering_queue_len(), 1);
    let requeued = agent.steering_queue_snapshot();
    let AgentMessage::Llm(Message::User { content, .. }) = &requeued[0] else {
        panic!("expected user message");
    };
    assert_eq!(
        content,
        &vec![Content::Text {
            text: "use v2 instead".into()
        }]
    );

    // Follow-up variants
    let taken = agent.take_follow_up_queue();
    assert_eq!(taken.len(), 1);
    assert_eq!(agent.follow_up_queue_len(), 0);
    assert!(agent.follow_up_queue_snapshot().is_empty());
}

// ---------------------------------------------------------------------------
// session_cost_usd
// ---------------------------------------------------------------------------

fn assistant_with_usage(usage: Usage) -> AgentMessage {
    AgentMessage::Llm(Message::assistant(
        vec![Content::Text { text: "ok".into() }],
        StopReason::Stop,
        "id",
        "model",
        usage,
    ))
}

fn some_usage() -> Usage {
    Usage {
        input: 1_000_000,
        output: 500_000,
        cache_read: 0,
        cache_write: 0,
        total_tokens: 1_500_000,
    }
}

#[test]
fn session_cost_usd_none_without_model_config() {
    let agent =
        Agent::new(MockProvider::text("x")).with_messages(vec![assistant_with_usage(some_usage())]);
    assert_eq!(agent.session_cost_usd(), None);
}

#[test]
fn session_cost_usd_none_when_rates_unconfigured() {
    // ModelConfig::custom has all-zero cost rates: pricing is unknown, so
    // the answer must be None ("can't price"), not Some(0.0) ("free").
    let mc = yoagent::provider::ModelConfig::custom(
        yoagent::provider::ApiProtocol::OpenAiCompletions,
        "local",
        "http://localhost:8080/v1",
        "m",
        "M",
    );
    let agent = Agent::new(MockProvider::text("x"))
        .with_model_config(mc)
        .with_messages(vec![assistant_with_usage(some_usage())]);
    assert_eq!(agent.session_cost_usd(), None);
}

#[test]
fn session_cost_usd_sums_assistant_turns_only() {
    let mut mc = yoagent::provider::ModelConfig::custom(
        yoagent::provider::ApiProtocol::OpenAiCompletions,
        "local",
        "http://localhost:8080/v1",
        "m",
        "M",
    );
    mc.cost.input_per_million = 3.0;
    mc.cost.output_per_million = 15.0;
    let expected_per_turn = mc.cost.cost_usd(&some_usage());

    let agent = Agent::new(MockProvider::text("x"))
        .with_model_config(mc)
        .with_messages(vec![
            AgentMessage::Llm(Message::user("hi")),
            assistant_with_usage(some_usage()),
            assistant_with_usage(some_usage()),
        ]);
    let total = agent.session_cost_usd().expect("rates are configured");
    assert!(total > 0.0);
    assert!((total - 2.0 * expected_per_turn).abs() < 1e-9);
}

// ---------------------------------------------------------------------------
// API-key resolution: explicit key wins; env var is the fallback
// ---------------------------------------------------------------------------

/// Records the api_key each stream call receives.
struct KeyCapturingProvider {
    captured: Arc<std::sync::Mutex<String>>,
}

#[async_trait::async_trait]
impl yoagent::provider::StreamProvider for KeyCapturingProvider {
    async fn stream(
        &self,
        config: yoagent::provider::StreamConfig,
        tx: mpsc::UnboundedSender<yoagent::provider::StreamEvent>,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Message, yoagent::provider::ProviderError> {
        *self.captured.lock().unwrap() = config.api_key.clone();
        let msg = Message::assistant(
            vec![Content::Text { text: "ok".into() }],
            StopReason::Stop,
            "mock",
            "mock",
            Usage::default(),
        );
        let _ = tx.send(yoagent::provider::StreamEvent::Start);
        let _ = tx.send(yoagent::provider::StreamEvent::Done {
            message: msg.clone(),
        });
        Ok(msg)
    }
}

/// Run one prompt to completion so the provider records the resolved key.
async fn run_one_prompt(agent: &mut Agent) {
    let mut rx = agent.prompt("hi").await;
    while rx.recv().await.is_some() {}
    agent.finish().await;
}

#[tokio::test]
async fn test_explicit_api_key_wins_over_env() {
    // Each key-resolution test uses its own env var to stay race-free under
    // parallel test execution.
    std::env::set_var("ZAI_API_KEY", "env-key-should-lose");
    let captured = Arc::new(std::sync::Mutex::new(String::new()));
    let mut agent = Agent::new(KeyCapturingProvider {
        captured: captured.clone(),
    })
    .with_model("m")
    .with_api_key("explicit-key")
    .with_model_config(yoagent::provider::ModelConfig::custom(
        yoagent::provider::ApiProtocol::OpenAiCompletions,
        "zai",
        "http://localhost:8080/v1",
        "m",
        "M",
    ));
    run_one_prompt(&mut agent).await;
    assert_eq!(*captured.lock().unwrap(), "explicit-key");
}

#[tokio::test]
async fn test_env_var_fallback_resolves_api_key() {
    std::env::set_var("CEREBRAS_API_KEY", "cerebras-env-key");
    let captured = Arc::new(std::sync::Mutex::new(String::new()));
    let mut agent = Agent::new(KeyCapturingProvider {
        captured: captured.clone(),
    })
    .with_model("m")
    .with_model_config(yoagent::provider::ModelConfig::custom(
        yoagent::provider::ApiProtocol::OpenAiCompletions,
        "cerebras",
        "http://localhost:8080/v1",
        "m",
        "M",
    ));
    run_one_prompt(&mut agent).await;
    assert_eq!(*captured.lock().unwrap(), "cerebras-env-key");
}
