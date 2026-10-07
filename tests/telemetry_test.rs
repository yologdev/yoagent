//! Tests that the loop emits the documented tracing spans with their fields.
//!
//! Attaches a capturing subscriber to the loop future via `with_subscriber`
//! and drives `agent_loop` directly in the current task — spans created in
//! separately-spawned tasks would not reach this scoped subscriber.

use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::instrument::WithSubscriber;
use tracing_subscriber::layer::SubscriberExt;
use yoagent::provider::mock::*;
use yoagent::provider::MockProvider;
use yoagent::*;

/// Layer that records every new span's name.
struct SpanCollector(Arc<Mutex<Vec<String>>>);

impl<S> tracing_subscriber::Layer<S> for SpanCollector
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _id: &tracing::span::Id,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        self.0
            .lock()
            .unwrap()
            .push(attrs.metadata().name().to_string());
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
        "echoes"
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

fn loop_config(provider: MockProvider) -> yoagent::agent_loop::AgentLoopConfig {
    {
        let mut config =
            yoagent::agent_loop::AgentLoopConfig::new(std::sync::Arc::new(provider), "mock");
        config.api_key = "test".into();
        config.retry_config = yoagent::RetryConfig::none();
        config
    }
}

#[tokio::test]
async fn loop_emits_agent_llm_and_tool_spans() {
    let spans = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(SpanCollector(spans.clone()));

    let provider = MockProvider::new(vec![
        MockResponse::ToolCalls(vec![MockToolCall {
            provider_metadata: None,
            name: "echo".into(),
            arguments: serde_json::json!({}),
        }]),
        MockResponse::Text("done".into()),
    ]);
    let config = loop_config(provider);

    let mut context = AgentContext {
        system_prompt: "test".into(),
        messages: Vec::new(),
        tools: vec![Box::new(EchoTool)],
    };
    let (tx, _rx) = mpsc::unbounded_channel();

    // with_subscriber attaches the subscriber across every poll of the
    // future (a plain `with_default(sub, || fut).await` would only cover
    // creating the future, not running it).
    agent_loop(
        vec![AgentMessage::Llm(Message::user("go"))],
        &mut context,
        &config,
        tx,
        CancellationToken::new(),
    )
    .with_subscriber(subscriber)
    .await;

    let names = spans.lock().unwrap().clone();
    assert!(
        names.contains(&"agent_loop".to_string()),
        "expected agent_loop span, got: {names:?}"
    );
    // Two turns → two llm_stream spans; one tool execution → one tool span.
    assert_eq!(
        names.iter().filter(|n| *n == "llm_stream").count(),
        2,
        "got: {names:?}"
    );
    assert_eq!(
        names.iter().filter(|n| *n == "tool").count(),
        1,
        "got: {names:?}"
    );
}

// ---------------------------------------------------------------------------
// Field values: tokens + cost recorded on llm_stream (not just span names)
// ---------------------------------------------------------------------------

/// Records (span_name, field_name, value_debug) for every record() call.
struct FieldCollector {
    names: Arc<Mutex<std::collections::HashMap<u64, String>>>,
    records: Arc<Mutex<Vec<(String, String, String)>>>,
}

struct FieldVisitor<'a> {
    span: String,
    out: &'a Mutex<Vec<(String, String, String)>>,
}

impl tracing::field::Visit for FieldVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.out.lock().unwrap().push((
            self.span.clone(),
            field.name().to_string(),
            format!("{value:?}"),
        ));
    }
}

impl<S> tracing_subscriber::Layer<S> for FieldCollector
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        self.names
            .lock()
            .unwrap()
            .insert(id.into_u64(), attrs.metadata().name().to_string());
    }
    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let span = self
            .names
            .lock()
            .unwrap()
            .get(&id.into_u64())
            .cloned()
            .unwrap_or_default();
        let mut visitor = FieldVisitor {
            span,
            out: &self.records,
        };
        values.record(&mut visitor);
    }
}

/// Provider returning fixed non-zero usage so token/cost fields are real.
struct UsageProvider;

#[async_trait::async_trait]
impl yoagent::provider::StreamProvider for UsageProvider {
    async fn stream(
        &self,
        _config: yoagent::provider::StreamConfig,
        tx: mpsc::UnboundedSender<yoagent::provider::StreamEvent>,
        _cancel: CancellationToken,
    ) -> Result<Message, yoagent::provider::ProviderError> {
        let msg = Message::assistant(
            vec![Content::Text { text: "ok".into() }],
            StopReason::Stop,
            "m",
            "mock",
            Usage {
                input: 1_000_000,
                output: 500_000,
                cache_read: 7,
                cache_write: 0,
                total_tokens: 1_500_007,
            },
        );
        let _ = tx.send(yoagent::provider::StreamEvent::Done {
            message: msg.clone(),
        });
        Ok(msg)
    }
}

/// Run one `UsageProvider` turn (1M in, 0.5M out, 7 cached) with the given
/// `ModelConfig::cost`, returning every `(span, field, value)` recorded.
async fn llm_stream_records(
    cost: Option<yoagent::provider::CostConfig>,
) -> Vec<(String, String, String)> {
    let names = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let records = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(FieldCollector {
        names: names.clone(),
        records: records.clone(),
    });

    let mut config = loop_config(MockProvider::text("unused"));
    config.provider = std::sync::Arc::new(UsageProvider);
    let mut mc = yoagent::provider::ModelConfig::mock();
    mc.cost = cost;
    config.model_config = Some(mc);

    let mut context = AgentContext {
        system_prompt: "t".into(),
        messages: Vec::new(),
        tools: Vec::new(),
    };
    let (tx, _rx) = mpsc::unbounded_channel();
    agent_loop(
        vec![AgentMessage::Llm(Message::user("go"))],
        &mut context,
        &config,
        tx,
        CancellationToken::new(),
    )
    .with_subscriber(subscriber)
    .await;

    let recs = records.lock().unwrap().clone();
    recs
}

fn llm_stream_field(recs: &[(String, String, String)], field: &str) -> Option<String> {
    recs.iter()
        .find(|(span, f, _)| span == "llm_stream" && f == field)
        .map(|(_, _, v)| v.clone())
}

#[tokio::test]
async fn llm_stream_records_tokens_and_cost() {
    let recs = llm_stream_records(Some(yoagent::provider::CostConfig::new(3.0, 15.0))).await;
    let get = |field: &str| -> String {
        llm_stream_field(&recs, field)
            .unwrap_or_else(|| panic!("field {field} not recorded; got {recs:?}"))
    };
    assert_eq!(get("tokens_in"), "1000000");
    assert_eq!(get("tokens_out"), "500000");
    assert_eq!(get("tokens_cached"), "7");
    // 1M in @ $3/M + 0.5M out @ $15/M = 10.5, plus 7 cached tokens: the
    // config sets no cache-read rate, so they bill at the $3/M input rate.
    let cost: f64 = get("cost_usd").parse().unwrap();
    assert!((cost - (10.5 + 7.0 * 3.0 / 1e6)).abs() < 1e-12, "{cost}");
    assert_eq!(get("error"), "false");
}

/// A free model (`Some`, all-zero rates) records a real `0` cost; an unpriced
/// one (`cost: None`) leaves the field empty. Unknown is never rendered as $0,
/// and free is never rendered as unknown.
#[tokio::test]
async fn llm_stream_cost_distinguishes_free_from_unpriced() {
    let free = llm_stream_records(Some(yoagent::provider::CostConfig::new(0.0, 0.0))).await;
    let cost = llm_stream_field(&free, "cost_usd")
        .unwrap_or_else(|| panic!("a free model must record cost_usd; got {free:?}"));
    assert_eq!(cost.parse::<f64>().unwrap(), 0.0);

    let unpriced = llm_stream_records(None).await;
    assert!(
        llm_stream_field(&unpriced, "tokens_in").is_some(),
        "{unpriced:?}"
    );
    assert_eq!(llm_stream_field(&unpriced, "cost_usd"), None);
}

/// Each new span's name with its parent's name.
type SpanParents = Arc<Mutex<Vec<(String, Option<String>)>>>;

/// Layer that records each new span's name with its parent's name.
struct ParentCollector(SpanParents);

impl<S> tracing_subscriber::Layer<S> for ParentCollector
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _id: &tracing::span::Id,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let parent = if let Some(id) = attrs.parent() {
            ctx.span(id).map(|s| s.name().to_string())
        } else if attrs.is_contextual() {
            ctx.lookup_current().map(|s| s.name().to_string())
        } else {
            None
        };
        self.0
            .lock()
            .unwrap()
            .push((attrs.metadata().name().to_string(), parent));
    }
}

/// An `Agent` run is spawned, yet its `agent_loop` span stays a child of the
/// caller's span, for the receiver and the sender methods alike.
#[tokio::test]
async fn an_agent_run_keeps_the_caller_s_span_as_parent() {
    use tracing::Instrument;
    let spans = Arc::new(Mutex::new(Vec::new()));
    // A thread-local default on the current-thread runtime also covers the
    // spawned run, which polls on this thread.
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(ParentCollector(spans.clone())),
    );

    let mut agent = Agent::from_provider(
        MockProvider::text("hi"),
        yoagent::provider::ModelConfig::mock(),
    );
    async {
        let mut rx = agent.prompt("one").await;
        while rx.recv().await.is_some() {}
        agent.finish().await;
    }
    .instrument(tracing::info_span!("request_a"))
    .await;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    agent
        .prompt_with_sender("two", tx)
        .instrument(tracing::info_span!("request_b"))
        .await;
    drain.await.unwrap();

    let loops: Vec<Option<String>> = spans
        .lock()
        .unwrap()
        .iter()
        .filter(|(name, _)| name == "agent_loop")
        .map(|(_, parent)| parent.clone())
        .collect();
    assert_eq!(
        loops,
        vec![Some("request_a".to_string()), Some("request_b".to_string())]
    );
}

// ---------------------------------------------------------------------------
// An `Agent` run (spawned) reaches a scoped subscriber
// ---------------------------------------------------------------------------

/// Layer that records every event's message text.
struct EventCollector(Arc<Mutex<Vec<String>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for EventCollector {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        struct Text<'a>(&'a mut String);
        impl tracing::field::Visit for Text<'_> {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    *self.0 = format!("{value:?}");
                }
            }
        }
        let mut text = String::new();
        event.record(&mut Text(&mut text));
        self.0.lock().unwrap().push(text);
    }
}

/// Logs from inside the run's task.
struct LoggingTool;

#[async_trait::async_trait]
impl AgentTool for LoggingTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn label(&self) -> &str {
        "Echo"
    }
    fn description(&self) -> &str {
        "echoes"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(
        &self,
        _params: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        tracing::info!("logging tool ran");
        Ok(ToolResult {
            content: vec![Content::Text { text: "ok".into() }],
            details: serde_json::Value::Null,
        })
    }
}

/// `Agent` runs its loop on a spawned task. On a multi-thread runtime that
/// task runs on a worker thread, never on the test's own thread, so a
/// subscriber set as this thread's default reaches the run only if the run
/// carries it along.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_run_reaches_a_scoped_subscriber_on_a_multi_thread_runtime() {
    let spans = Arc::new(Mutex::new(Vec::new()));
    let events = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry()
        .with(SpanCollector(spans.clone()))
        .with(EventCollector(events.clone()));
    let _guard = tracing::subscriber::set_default(subscriber);

    let provider = MockProvider::new(vec![
        MockResponse::ToolCalls(vec![MockToolCall {
            provider_metadata: None,
            name: "echo".into(),
            arguments: serde_json::json!({}),
        }]),
        MockResponse::Text("done".into()),
    ]);
    let mut agent = Agent::from_provider(provider, yoagent::provider::ModelConfig::mock())
        .with_api_key("test")
        .with_tools(vec![Box::new(LoggingTool)]);
    let (tx, _rx) = mpsc::unbounded_channel();
    agent.prompt_with_sender("go", tx).await;

    let names = spans.lock().unwrap().clone();
    assert!(
        names.contains(&"agent_loop".to_string()),
        "the run's spans reach the scoped subscriber: {names:?}"
    );
    assert_eq!(
        names.iter().filter(|n| *n == "llm_stream").count(),
        2,
        "{names:?}"
    );
    assert_eq!(
        names.iter().filter(|n| *n == "tool").count(),
        1,
        "{names:?}"
    );
    let logs = events.lock().unwrap().clone();
    assert!(
        logs.iter().any(|l| l.contains("logging tool ran")),
        "the run's logs reach the scoped subscriber: {logs:?}"
    );
}
