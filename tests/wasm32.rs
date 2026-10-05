//! Runs on `wasm32-unknown-unknown` under Node (wasm-bindgen-test):
//!
//! ```bash
//! cargo install wasm-bindgen-cli --version <locked wasm-bindgen> --locked
//! CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner \
//!   cargo test --target wasm32-unknown-unknown --no-default-features --test wasm32
//! ```
//!
//! Clippy cannot see the failures this guards against: code that compiles for
//! wasm32 but panics or hangs there (a `std::time` clock read, a Tokio timer,
//! a task that never runs on the host executor). So the `rt` shims are
//! exercised directly, and whole agent runs go through the same spawn, timer
//! and clock paths a Worker uses.
#![cfg(target_arch = "wasm32")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wasm_bindgen_test::wasm_bindgen_test;
use yoagent::context::ExecutionLimits;
use yoagent::provider::mock::{MockResponse, MockToolCall};
use yoagent::provider::{
    MockProvider, ModelConfig, ProviderError, StreamConfig, StreamEvent, StreamProvider,
};
use yoagent::retry::RetryConfig;
use yoagent::rt;
use yoagent::*;

// ---------------------------------------------------------------------------
// rt shims
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
async fn sleep_waits_on_the_host_timer() {
    let start = rt::Instant::now();
    rt::sleep(Duration::from_millis(30)).await;
    // setTimeout may fire a little early or late; it must not resolve at once.
    assert!(
        start.elapsed() >= Duration::from_millis(20),
        "slept {:?}",
        start.elapsed()
    );
}

#[wasm_bindgen_test]
async fn timeout_returns_the_value_or_elapsed() {
    let fast = rt::timeout(Duration::from_secs(5), async { 7 }).await;
    assert_eq!(fast.ok(), Some(7));

    // The future is polled before the timer, so the two checks above and
    // below would pass with a timer that fires at once: also check timing.
    let waited = rt::timeout(Duration::from_secs(1), async {
        rt::sleep(Duration::from_millis(5)).await;
        8
    })
    .await;
    assert_eq!(waited.ok(), Some(8), "the timer fired before its deadline");

    let start = rt::Instant::now();
    let slow = rt::timeout(Duration::from_millis(30), futures::future::pending::<()>()).await;
    assert!(slow.is_err(), "a pending future must time out");
    assert!(
        start.elapsed() >= Duration::from_millis(20),
        "timed out after {:?}",
        start.elapsed()
    );
}

#[wasm_bindgen_test]
async fn spawn_runs_on_the_host_executor_and_returns_its_value() {
    let handle = rt::spawn(async {
        rt::sleep(Duration::from_millis(1)).await;
        "done"
    });
    assert_eq!(handle.await.unwrap(), "done");
}

#[wasm_bindgen_test]
async fn an_aborted_task_reports_cancellation() {
    let handle = rt::spawn(futures::future::pending::<()>());
    handle.abort();
    let err = handle.await.expect_err("an aborted task yields no value");
    assert!(err.is_cancelled());
}

#[wasm_bindgen_test]
async fn instant_reads_the_host_clock_and_advances() {
    // `std::time::Instant::now()` panics on wasm32; `rt::Instant` must not.
    let a = rt::Instant::now();
    rt::sleep(Duration::from_millis(5)).await;
    assert!(rt::Instant::now() > a);
}

// ---------------------------------------------------------------------------
// Agent runs
// ---------------------------------------------------------------------------

struct Echo {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait(?Send)]
impl AgentTool for Echo {
    fn name(&self) -> &str {
        "echo"
    }
    fn label(&self) -> &str {
        "Echo"
    }
    fn description(&self) -> &str {
        "Echo the input"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}}})
    }
    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let text = params["text"].as_str().unwrap_or_default().to_string();
        Ok(ToolResult {
            content: vec![Content::Text { text }],
            details: serde_json::Value::Null,
        })
    }
}

async fn drain(mut rx: mpsc::UnboundedReceiver<AgentEvent>) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    while let Some(e) = rx.recv().await {
        events.push(e);
    }
    events
}

fn final_text(events: &[AgentEvent]) -> String {
    events
        .iter()
        .rev()
        .find_map(|e| match e {
            AgentEvent::MessageEnd {
                message: AgentMessage::Llm(Message::Assistant { content, .. }),
            } => content.iter().find_map(|c| match c {
                Content::Text { text } if !text.is_empty() => Some(text.clone()),
                _ => None,
            }),
            _ => None,
        })
        .unwrap_or_default()
}

/// A full run: the loop is spawned on the host executor, the tool runs, its
/// result reaches the model, and the execution limits read the host clock.
#[wasm_bindgen_test]
async fn an_agent_runs_a_tool_turn_and_answers() {
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = MockProvider::new(vec![
        MockResponse::ToolCalls(vec![MockToolCall {
            name: "echo".into(),
            arguments: serde_json::json!({"text": "from the tool"}),
            provider_metadata: None,
        }]),
        MockResponse::Text("the tool said: from the tool".into()),
    ]);
    let mut agent = Agent::from_provider(provider, ModelConfig::mock())
        .with_tools(vec![Box::new(Echo {
            calls: calls.clone(),
        })])
        .with_execution_limits(
            ExecutionLimits::default()
                .with_max_turns(4)
                .with_max_duration(Duration::from_secs(30)),
        );
    let events = drain(agent.prompt("use the tool").await).await;
    agent.finish().await;

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::ToolExecutionEnd {
            is_error: false,
            ..
        }
    )));
    assert_eq!(final_text(&events), "the tool said: from the tool");
    assert!(matches!(events.last(), Some(AgentEvent::AgentEnd { .. })));
    // Message timestamps come from `web-time`'s SystemTime, not std's.
    let stamped = agent.messages().iter().any(
        |m| matches!(m, AgentMessage::Llm(Message::Assistant { timestamp, .. }) if *timestamp > 0),
    );
    assert!(stamped, "assistant messages carry a wall-clock timestamp");
}

/// Fails its first attempt with a retryable error after streaming a
/// partial delta, then answers.
struct FlakyOnce {
    attempts: AtomicUsize,
    inner: MockProvider,
}

#[async_trait::async_trait(?Send)]
impl StreamProvider for FlakyOnce {
    async fn stream(
        &self,
        config: StreamConfig,
        tx: mpsc::UnboundedSender<StreamEvent>,
        cancel: CancellationToken,
    ) -> Result<Message, ProviderError> {
        if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            let _ = tx.send(StreamEvent::Start);
            let _ = tx.send(StreamEvent::TextDelta {
                content_index: 0,
                delta: "partial".into(),
            });
            return Err(ProviderError::Network("stream cut".into()));
        }
        self.inner.stream(config, tx, cancel).await
    }
}

/// The retry path on the host: the backoff is a real `setTimeout`, the
/// failed attempt is closed and marked with `ProviderRetry`, and the retry
/// answers.
#[wasm_bindgen_test]
async fn a_failed_attempt_is_retried_on_the_host_timer() {
    let provider = FlakyOnce {
        attempts: AtomicUsize::new(0),
        inner: MockProvider::text("recovered"),
    };
    let mut agent =
        Agent::from_provider(provider, ModelConfig::mock()).with_retry_config(RetryConfig {
            max_retries: 2,
            initial_delay_ms: 20,
            backoff_multiplier: 1.0,
            max_delay_ms: 20,
        });
    let started = rt::Instant::now();
    let events = drain(agent.prompt("hi").await).await;
    agent.finish().await;

    let retries: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ProviderRetry {
                attempt,
                max_attempts,
                ..
            } => Some((*attempt, *max_attempts)),
            _ => None,
        })
        .collect();
    assert_eq!(retries, [(1, 3)]);
    assert_eq!(final_text(&events), "recovered");
    // The backoff (20 ms ± 20% jitter) was actually waited out.
    assert!(
        started.elapsed() >= Duration::from_millis(10),
        "{:?}",
        started.elapsed()
    );
}

/// `retry_safe_events` runs its filter on a task spawned on the host
/// executor: the retried attempt's "partial" never reaches the consumer.
#[wasm_bindgen_test]
async fn retry_safe_events_drops_the_retried_text_on_the_host() {
    let provider = FlakyOnce {
        attempts: AtomicUsize::new(0),
        inner: MockProvider::text("recovered"),
    };
    let mut agent =
        Agent::from_provider(provider, ModelConfig::mock()).with_retry_config(RetryConfig {
            max_retries: 2,
            initial_delay_ms: 20,
            backoff_multiplier: 1.0,
            max_delay_ms: 20,
        });
    let events = drain(yoagent::retry::retry_safe_events(agent.prompt("hi").await)).await;
    agent.finish().await;

    let streamed: String = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::MessageUpdate {
                delta: StreamDelta::Text { delta },
                ..
            } => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(streamed, "recovered");
    assert_eq!(final_text(&events), "recovered");
    assert!(matches!(events.last(), Some(AgentEvent::AgentEnd { .. })));
}
