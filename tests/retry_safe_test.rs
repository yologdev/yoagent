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
    assert_eq!(count_retries(&events), 2);
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

/// The interactive-terminal pattern from `docs/concepts/retry.md`, as
/// written there: abort when a retry follows text streamed in this turn.
async fn run_terminal_pattern(agent: &mut Agent, prompt: &str) -> Vec<AgentEvent> {
    let mut rx = agent.prompt(prompt).await;
    let mut streamed = false;
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        match &event {
            AgentEvent::TurnStart | AgentEvent::MessageStart { .. } => streamed = false,
            AgentEvent::MessageUpdate {
                delta: StreamDelta::Text { delta } | StreamDelta::Thinking { delta },
                ..
            } => streamed |= !delta.is_empty(),
            AgentEvent::ProviderRetry { .. } if streamed => agent.abort(),
            _ => {}
        }
        events.push(event);
    }
    agent.finish().await;
    events
}

fn ends_aborted(events: &[AgentEvent]) -> bool {
    let Some(AgentEvent::AgentEnd { messages, .. }) = events.last() else {
        panic!("the run must end with AgentEnd");
    };
    matches!(
        messages.last(),
        Some(AgentMessage::Llm(Message::Assistant {
            stop_reason: StopReason::Aborted,
            ..
        }))
    )
}

/// The abort lands during the backoff (200 ms here, far longer than the
/// consumer takes to react): the turn ends `Aborted` and the retry request is
/// never sent.
#[tokio::test]
async fn aborting_on_a_retry_after_text_sends_no_retry_request() {
    let provider = std::sync::Arc::new(FailsAfterText {
        attempts: AtomicUsize::new(0),
        fail_first: 1,
        inner: MockProvider::text("PONG"),
    });
    let mut agent = Agent::from_provider(ArcProvider(provider.clone()), ModelConfig::mock())
        .with_retry_config(RetryConfig {
            max_retries: 2,
            initial_delay_ms: 200,
            backoff_multiplier: 1.0,
            max_delay_ms: 200,
        });
    let events = run_terminal_pattern(&mut agent, "ping").await;

    assert_eq!(provider.attempts.load(Ordering::SeqCst), 1, "no retry sent");
    assert_eq!(streamed_text(&events), "PARTIAL_");
    // The failed attempt's error end, then the turn's aborted message: no new
    // attempt streamed it, so it is announced on its own (#243).
    assert_eq!(
        assistant_ends(&events),
        vec![StopReason::Error, StopReason::Aborted]
    );
    assert!(ends_aborted(&events));
}

/// Answers turn 1 with streamed text and a tool call, then fails turn 2's
/// first attempt *before* sending `Start` — as Google, Vertex and Bedrock do
/// for a 429 — then answers.
struct TextThenEarlyFailure {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl StreamProvider for TextThenEarlyFailure {
    async fn stream(
        &self,
        _config: StreamConfig,
        tx: mpsc::UnboundedSender<StreamEvent>,
        _cancel: CancellationToken,
    ) -> Result<Message, ProviderError> {
        let (content, stop) = match self.calls.fetch_add(1, Ordering::SeqCst) {
            0 => (
                vec![
                    Content::Text {
                        text: "Let me echo.".into(),
                    },
                    Content::tool_call("c1", "echo", serde_json::json!({"text": "hi"})),
                ],
                StopReason::ToolUse,
            ),
            1 => {
                return Err(ProviderError::RateLimited {
                    retry_after_ms: None,
                })
            }
            _ => (
                vec![Content::Text {
                    text: "done".into(),
                }],
                StopReason::Stop,
            ),
        };
        let _ = tx.send(StreamEvent::Start);
        if let Some(Content::Text { text }) = content.first() {
            let _ = tx.send(StreamEvent::TextDelta {
                content_index: 0,
                delta: text.clone(),
            });
        }
        let message = Message::assistant(content, stop, "mock", "mock", Usage::default());
        let _ = tx.send(StreamEvent::Done {
            message: message.clone(),
        });
        Ok(message)
    }
}

/// Text from an earlier turn must not make the pattern abort a retry that
/// came before any text of its own. Within one run the tool result's
/// `MessageStart` already resets the flag; the pattern's `TurnStart` reset
/// also covers a consumer that keeps the flag across runs (a `continue_loop`
/// starts a turn with no new message).
#[tokio::test]
async fn the_terminal_pattern_lets_an_early_failure_retry() {
    let mut agent = Agent::from_provider(
        TextThenEarlyFailure {
            calls: AtomicUsize::new(0),
        },
        ModelConfig::mock(),
    )
    .with_tools(vec![Box::new(EchoTool)])
    .with_retry_config(fast_retries());
    let events = run_terminal_pattern(&mut agent, "echo hi").await;

    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::ProviderRetry { .. })));
    assert!(!ends_aborted(&events), "an early failure must retry");
    assert_eq!(streamed_text(&events), "Let me echo.done");
}

/// With the filter, an abort during the backoff releases only the turn's
/// aborted message: the failed attempt goes with its `ProviderRetry`, no new
/// attempt starts, and the aborted message is announced on its own (#243).
#[tokio::test]
async fn the_filter_and_an_abort_release_only_the_aborted_message() {
    let mut agent = Agent::from_provider(
        FailsAfterText {
            attempts: AtomicUsize::new(0),
            fail_first: 1,
            inner: MockProvider::text("PONG"),
        },
        ModelConfig::mock(),
    )
    .with_retry_config(RetryConfig {
        max_retries: 2,
        initial_delay_ms: 200,
        backoff_multiplier: 1.0,
        max_delay_ms: 200,
    });
    let mut rx = retry_safe_events(agent.prompt("ping").await);
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        if matches!(event, AgentEvent::ProviderRetry { .. }) {
            agent.abort();
        }
        events.push(event);
    }
    agent.finish().await;

    assert_eq!(streamed_text(&events), "");
    assert_eq!(count_starts(&events), 1);
    assert_eq!(assistant_ends(&events), vec![StopReason::Aborted]);
    assert!(ends_aborted(&events));
}

/// Several retries in a row, then success: only the answer's text and the
/// retry markers reach the consumer.
#[tokio::test]
async fn two_retries_then_success_leave_only_the_answer() {
    let mut agent = Agent::from_provider(
        FailsAfterText {
            attempts: AtomicUsize::new(0),
            fail_first: 2,
            inner: MockProvider::text("PONG"),
        },
        ModelConfig::mock(),
    )
    .with_retry_config(fast_retries());
    let events = drain(retry_safe_events(agent.prompt("ping").await)).await;
    agent.finish().await;

    assert_eq!(streamed_text(&events), "PONG");
    assert_eq!(count_retries(&events), 2);
    assert_eq!(assistant_ends(&events), vec![StopReason::Stop]);
}

fn count_retries(events: &[AgentEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ProviderRetry { .. }))
        .count()
}

fn assistant_event(start: bool, stop: StopReason) -> AgentEvent {
    let message: AgentMessage = Message::assistant(vec![], stop, "m", "p", Usage::default())
        .with_timestamp(0)
        .into();
    if start {
        AgentEvent::MessageStart { message }
    } else {
        AgentEvent::MessageEnd { message }
    }
}

/// The adapter releases a pending final failure when its input closes, and
/// drops an attempt the input closed inside.
#[tokio::test]
async fn the_adapter_flushes_when_its_input_closes() {
    let delta = AgentEvent::MessageUpdate {
        message: Message::assistant(vec![], StopReason::Stop, "m", "p", Usage::default())
            .with_timestamp(0)
            .into(),
        delta: StreamDelta::Text { delta: "x".into() },
    };

    let (tx, rx) = mpsc::unbounded_channel();
    let out = retry_safe_events(rx);
    tx.send(assistant_event(true, StopReason::Stop)).unwrap();
    tx.send(delta.clone()).unwrap();
    tx.send(assistant_event(false, StopReason::Error)).unwrap();
    drop(tx);
    assert_eq!(
        drain(out).await,
        vec![
            assistant_event(true, StopReason::Stop),
            assistant_event(false, StopReason::Error)
        ]
    );

    let (tx, rx) = mpsc::unbounded_channel();
    let out = retry_safe_events(rx);
    tx.send(assistant_event(true, StopReason::Stop)).unwrap();
    tx.send(delta).unwrap();
    drop(tx);
    assert!(drain(out).await.is_empty());
}

/// Dropping the filtered receiver must not stall the run.
#[tokio::test]
async fn dropping_the_filtered_receiver_lets_the_run_finish() {
    let mut agent = Agent::from_provider(MockProvider::text("PONG"), ModelConfig::mock());
    let mut rx = retry_safe_events(agent.prompt("ping").await);
    let _ = rx.recv().await;
    drop(rx);
    tokio::time::timeout(std::time::Duration::from_secs(5), agent.finish())
        .await
        .expect("the run must finish without a consumer");
    assert!(agent.messages().iter().any(|m| matches!(
        m,
        AgentMessage::Llm(Message::Assistant { content, .. })
            if matches!(content.first(), Some(Content::Text { text }) if text == "PONG")
    )));
}

/// A provider that returns `Ok` without sending `Done`: the loop closes the
/// message it opened, so the filter releases the answer instead of holding
/// it forever.
struct NoDone;

#[async_trait::async_trait]
impl StreamProvider for NoDone {
    async fn stream(
        &self,
        _config: StreamConfig,
        tx: mpsc::UnboundedSender<StreamEvent>,
        _cancel: CancellationToken,
    ) -> Result<Message, ProviderError> {
        let _ = tx.send(StreamEvent::Start);
        let _ = tx.send(StreamEvent::TextDelta {
            content_index: 0,
            delta: "PONG".into(),
        });
        Ok(Message::assistant(
            vec![Content::Text {
                text: "PONG".into(),
            }],
            StopReason::Stop,
            "mock",
            "mock",
            Usage::default(),
        ))
    }
}

#[tokio::test]
async fn an_answer_without_done_is_closed_and_released() {
    let mut agent = Agent::from_provider(NoDone, ModelConfig::mock());
    let raw = drain(agent.prompt("ping").await).await;
    agent.finish().await;
    assert_eq!(assistant_ends(&raw), vec![StopReason::Stop]);

    let mut agent = Agent::from_provider(NoDone, ModelConfig::mock());
    let events = drain(retry_safe_events(agent.prompt("ping").await)).await;
    agent.finish().await;
    assert_eq!(streamed_text(&events), "PONG");
    assert_eq!(assistant_ends(&events), vec![StopReason::Stop]);
}

/// Shares one `FailsAfterText` so the test can read its attempt count.
struct ArcProvider(std::sync::Arc<FailsAfterText>);

#[async_trait::async_trait]
impl StreamProvider for ArcProvider {
    async fn stream(
        &self,
        config: StreamConfig,
        tx: mpsc::UnboundedSender<StreamEvent>,
        cancel: CancellationToken,
    ) -> Result<Message, ProviderError> {
        self.0.stream(config, tx, cancel).await
    }
}
