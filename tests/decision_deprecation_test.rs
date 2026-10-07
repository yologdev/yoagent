//! The deprecated ways to install a `ToolGate` (as a `ToolMiddleware`) and an
//! `InputGuard` (as an `AsyncInputFilter`) warn once per process, and the
//! supported ones (`with_tool_gate`, `with_input_guard`) never do.
//!
//! Its own test binary with a single test: the warnings are recorded
//! process-wide, so their order across tests would decide the outcome.

use serde_json::json;
use std::sync::{Arc, Mutex};
use tracing_subscriber::layer::SubscriberExt;
use yoagent::decision::*;
use yoagent::provider::mock::*;
use yoagent::provider::{MockProvider, ModelConfig};
use yoagent::*;

/// Captures every event's message and fields on this thread.
#[derive(Clone, Default)]
struct CapturedLogs(Arc<Mutex<Vec<String>>>);

impl CapturedLogs {
    fn deprecations(&self) -> Vec<String> {
        let logs = self.0.lock().unwrap();
        logs.iter()
            .filter(|l| l.contains("deprecated"))
            .cloned()
            .collect()
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CapturedLogs {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        struct Fields(String);
        impl tracing::field::Visit for Fields {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0.push_str(&format!(" {}={value:?}", field.name()));
            }
        }
        let mut fields = Fields(String::new());
        event.record(&mut fields);
        self.0
            .lock()
            .unwrap()
            .push(format!("{}:{}", event.metadata().level(), fields.0));
    }
}

/// Answers every question with `p`.
fn answering(p: f64) -> MockBackend {
    MockBackend::from_fn(move |req| {
        let mut eval = Evaluation::new("jev-test", DecisionUsage::default());
        for (id, _) in &req.questions {
            eval = eval.with_answer(id.clone(), NoulAnswer::new(p));
        }
        Ok(eval)
    })
}

struct Echo;

#[async_trait::async_trait]
impl AgentTool for Echo {
    fn name(&self) -> &str {
        "echo"
    }
    fn label(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "Echoes."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({"type": "object"})
    }
    async fn execute(&self, _: serde_json::Value, _: ToolContext) -> Result<ToolResult, ToolError> {
        Ok(ToolResult {
            content: vec![Content::Text { text: "ok".into() }],
            details: serde_json::Value::Null,
        })
    }
}

#[tokio::test]
async fn deprecated_installs_warn_once_and_supported_ones_never() {
    let logs = CapturedLogs::default();
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(logs.clone()));

    // (a) The supported installs: both consulted, no deprecation warning.
    let gate_backend = answering(0.95);
    let guard_backend = answering(0.0);
    let provider = MockProvider::new(vec![
        MockResponse::ToolCalls(vec![MockToolCall {
            provider_metadata: None,
            name: "echo".into(),
            arguments: json!({}),
        }]),
        MockResponse::Text("done".into()),
    ]);
    let mut agent = Agent::from_provider(provider, ModelConfig::mock())
        .with_tools(vec![Box::new(Echo)])
        .with_tool_gate(ToolGate::new(DecisionModel::from_backend(
            gate_backend.clone(),
            "jev-test",
        )))
        .with_input_guard(InputGuard::new(DecisionModel::from_backend(
            guard_backend.clone(),
            "jev-test",
        )));
    let mut rx = agent.prompt("echo something").await;
    while rx.recv().await.is_some() {}
    agent.finish().await;
    assert_eq!(
        guard_backend.request_count(),
        1,
        "the guard screened the prompt"
    );
    assert_eq!(gate_backend.request_count(), 1, "the gate judged the call");
    assert_eq!(logs.deprecations(), Vec::<String>::new());

    // (b) The deprecated impls: one warning each. This also shows (a) was not
    // vacuous: the once-per-process keys were still unspent there.
    let args = json!({});
    let prompts = [Message::user("echo something")];
    let call = ToolCallRequest::new("call-1", "echo", &args).with_run_prompts(&prompts);
    let gate = ToolGate::new(DecisionModel::from_backend(answering(0.95), "jev-test"));
    let guard = InputGuard::new(DecisionModel::from_backend(answering(0.0), "jev-test"));
    ToolMiddleware::before_tool(&gate, &call).await;
    AsyncInputFilter::filter(&guard, "hello").await;
    let warned = logs.deprecations();
    assert_eq!(warned.len(), 2, "{warned:?}");
    assert!(warned
        .iter()
        .any(|l| l.starts_with("WARN") && l.contains("ToolGate::decide")));
    assert!(warned
        .iter()
        .any(|l| l.starts_with("WARN") && l.contains("InputGuard::screen")));

    // (c) Once per process: a second use, even of another gate, adds nothing.
    let other = ToolGate::new(DecisionModel::from_backend(answering(0.95), "jev-test"));
    ToolMiddleware::before_tool(&other, &call).await;
    AsyncInputFilter::filter(&guard, "hello").await;
    assert_eq!(logs.deprecations().len(), 2);
}
