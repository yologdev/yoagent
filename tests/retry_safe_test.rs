//! `retry::retry_safe_events` / `RetrySafeEvents` (#218): a consumer writing
//! to an append-only sink must never see the text of an attempt the loop
//! retried, and must see everything else exactly as the loop sent it.

use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use yoagent::provider::mock::{MockResponse, MockToolCall};
use yoagent::provider::{
    MockProvider, ModelConfig, ProviderError, StreamConfig, StreamEvent, StreamProvider,
};
use yoagent::retry::{retry_safe_events, RetryConfig, RetrySafeEvents};
use yoagent::*;

/// Streams "PARTIAL_" and then fails with a retryable error on the first
/// `fail_first` attempts; answers from `inner` after that.
struct FailsAfterText {
    attempts: AtomicUsize,
    fail_first: usize,
    inner: MockProvider,
}

#[async_trait::async_trait]
impl StreamProvider for FailsAfterText {
    async fn stream(
        &self,
        config: StreamConfig,
        tx: mpsc::UnboundedSender<StreamEvent>,
        cancel: CancellationToken,
    ) -> Result<Message, ProviderError> {
        if self.attempts.fetch_add(1, Ordering::SeqCst) < self.fail_first {
            let _ = tx.send(StreamEvent::Start);
            let _ = tx.send(StreamEvent::TextDelta {
                content_index: 0,
                delta: "PARTIAL_".into(),
            });
            return Err(ProviderError::Network("stream cut".into()));
        }
        self.inner.stream(config, tx, cancel).await
    }
}

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
        Ok(ToolResult {
            content: vec![Content::Text {
                text: params["text"].as_str().unwrap_or("").to_string(),
            }],
            details: serde_json::Value::Null,
        })
    }
}

fn fast_retries() -> RetryConfig {
    RetryConfig {
        max_retries: 2,
        initial_delay_ms: 1,
        backoff_multiplier: 1.0,
        max_delay_ms: 1,
    }
}

async fn drain(mut rx: mpsc::UnboundedReceiver<AgentEvent>) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    events
}

/// Everything a consumer would print, from the text deltas alone.
fn streamed_text(events: &[AgentEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::MessageUpdate {
                delta: StreamDelta::Text { delta },
                ..
            } => Some(delta.as_str()),
            _ => None,
        })
        .collect()
}

fn assistant_ends(events: &[AgentEvent]) -> Vec<StopReason> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::MessageEnd {
                message: AgentMessage::Llm(Message::Assistant { stop_reason, .. }),
            } => Some(stop_reason.clone()),
            _ => None,
        })
        .collect()
}

fn count_starts(events: &[AgentEvent]) -> usize {
    events
        .iter()
        .filter(|e| {
            matches!(
                e,
                AgentEvent::MessageStart {
                    message: AgentMessage::Llm(Message::Assistant { .. })
                }
            )
        })
        .count()
}

#[tokio::test]
async fn a_retried_attempt_leaves_no_text() {
    let provider = FailsAfterText {
        attempts: AtomicUsize::new(0),
        fail_first: 1,
        inner: MockProvider::text("PONG"),
    };
    let mut agent =
        Agent::from_provider(provider, ModelConfig::mock()).with_retry_config(fast_retries());
    let events = drain(retry_safe_events(agent.prompt("ping").await)).await;
    agent.finish().await;

    // Without the filter a consumer prints "PARTIAL_PONG".
    assert_eq!(streamed_text(&events), "PONG");
    // The failed attempt is gone entirely; its retry marker is not.
    assert_eq!(assistant_ends(&events), vec![StopReason::Stop]);
    assert_eq!(count_starts(&events), 1);
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ProviderRetry { .. }))
            .count(),
        1
    );
    assert!(matches!(events.last(), Some(AgentEvent::AgentEnd { .. })));
}

#[tokio::test]
async fn a_final_failure_keeps_its_end_but_not_its_text() {
    let provider = FailsAfterText {
        attempts: AtomicUsize::new(0),
        fail_first: usize::MAX,
        inner: MockProvider::text("never"),
    };
    let mut agent =
        Agent::from_provider(provider, ModelConfig::mock()).with_retry_config(fast_retries());
    let events = drain(retry_safe_events(agent.prompt("ping").await)).await;
    agent.finish().await;

    assert_eq!(streamed_text(&events), "");
    // Two retried attempts vanish; the last one is reported as the error it is.
    assert_eq!(assistant_ends(&events), vec![StopReason::Error]);
    assert_eq!(count_starts(&events), 1);
    assert!(matches!(events.last(), Some(AgentEvent::AgentEnd { .. })));
}

#[tokio::test]
async fn without_a_retry_the_stream_is_unchanged() {
    let responses = || {
        vec![
            MockResponse::ToolCalls(vec![MockToolCall {
                provider_metadata: None,
                name: "echo".into(),
                arguments: serde_json::json!({"text": "hello"}),
            }]),
            MockResponse::Text("Echoed: hello".into()),
        ]
    };
    let run = |filtered: bool| async move {
        let mut agent = Agent::from_provider(MockProvider::new(responses()), ModelConfig::mock())
            .with_tools(vec![Box::new(EchoTool)]);
        let rx = agent.prompt("echo hello").await;
        let rx = if filtered { retry_safe_events(rx) } else { rx };
        let events = drain(rx).await;
        agent.finish().await;
        events
    };
    let plain = run(false).await;
    let filtered = run(true).await;

    assert!(streamed_text(&plain).contains("Echoed: hello"));
    // Timestamps and ids differ between two runs, so compare the shape.
    let shape = |events: &[AgentEvent]| -> Vec<String> {
        events
            .iter()
            .map(|e| {
                format!("{e:?}")
                    .split([' ', '{', '('])
                    .next()
                    .unwrap()
                    .to_string()
            })
            .collect()
    };
    assert_eq!(shape(&filtered), shape(&plain));
    assert_eq!(streamed_text(&filtered), streamed_text(&plain));
    assert_eq!(assistant_ends(&filtered), assistant_ends(&plain));
}

#[tokio::test]
async fn the_filter_by_hand_matches_the_receiver_adapter() {
    let provider = FailsAfterText {
        attempts: AtomicUsize::new(0),
        fail_first: 1,
        inner: MockProvider::text("PONG"),
    };
    let mut agent =
        Agent::from_provider(provider, ModelConfig::mock()).with_retry_config(fast_retries());
    let raw = drain(agent.prompt("ping").await).await;
    agent.finish().await;
    assert_eq!(
        streamed_text(&raw),
        "PARTIAL_PONG",
        "the hazard this guards"
    );

    let mut filter = RetrySafeEvents::new();
    let mut out: Vec<AgentEvent> = Vec::new();
    for event in raw.iter().cloned() {
        out.extend(filter.push(event));
    }
    out.extend(filter.finish());
    assert_eq!(streamed_text(&out), "PONG");
    // Exactly the failed attempt's start, delta and error end are dropped.
    assert_eq!(out.len(), raw.len() - 3);
}
