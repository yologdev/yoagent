//! A reasoning effort clamped to a model's declared ceiling is never silent:
//! one `WARN` per model and clamp, naming the model, however many requests
//! repeat it (#198: replacing `warn_effort_clamp` with `()` went unnoticed).
//!
//! Its own test binary: it installs the process's global subscriber, once —
//! a scoped one races other tests that hit the same `warn!` callsite.

use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::layer::SubscriberExt;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};
use yoagent::provider::model::ReasoningEffortCeiling;
use yoagent::provider::{ModelConfig, OpenAiCompatProvider, StreamConfig, StreamProvider};
use yoagent::types::*;

/// The `model` field of every `WARN` event, process-wide.
#[derive(Clone, Default)]
struct Warns(Arc<Mutex<Vec<String>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Warns {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        if *event.metadata().level() != tracing::Level::WARN {
            return;
        }
        struct Model(Option<String>);
        impl tracing::field::Visit for Model {
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                if field.name() == "model" {
                    self.0 = Some(value.to_string());
                }
            }
            fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
        }
        let mut model = Model(None);
        event.record(&mut model);
        if let Some(model) = model.0 {
            self.0.lock().unwrap().push(model);
        }
    }
}

fn chat_sse() -> String {
    [
        json!({"choices": [{"index": 0, "delta": {"content": "ok"}, "finish_reason": null}]}),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
    ]
    .iter()
    .map(|e| format!("data: {e}\n\n"))
    .chain(std::iter::once("data: [DONE]\n\n".to_string()))
    .collect()
}

/// One Chat Completions request at `level`; the `reasoning_effort` it sent.
async fn send(server: &MockServer, mc: &ModelConfig, level: ThinkingLevel) -> Value {
    let mut config = StreamConfig::new(mc.id.clone(), "test-key");
    config.messages = vec![Message::user("hi")];
    config.thinking_level = level;
    config.model_config = Some(mc.clone());
    let (tx, _rx) = mpsc::unbounded_channel();
    OpenAiCompatProvider
        .stream(config, tx, CancellationToken::new())
        .await
        .expect("the request succeeds");
    let requests = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests.last().unwrap().body).unwrap();
    body["reasoning_effort"].clone()
}

#[tokio::test]
async fn a_clamped_effort_warns_once_naming_the_model() {
    let warns = Warns::default();
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(warns.clone()))
        .expect("the only subscriber in this binary");

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(chat_sse(), "text/event-stream"))
        .mount(&server)
        .await;
    let mut mc = ModelConfig::openai("clamp-log-test-model", "Clamp");
    mc.base_url = server.uri();
    mc.reasoning = true;
    mc.compat
        .as_mut()
        .expect("openai compat")
        .max_reasoning_effort = ReasoningEffortCeiling::High;

    for _ in 0..3 {
        assert_eq!(send(&server, &mc, ThinkingLevel::XHigh).await, "high");
    }
    assert_eq!(send(&server, &mc, ThinkingLevel::High).await, "high");
    assert_eq!(
        *warns.0.lock().unwrap(),
        ["clamp-log-test-model"],
        "three clamped requests warn once, naming the model; an unclamped one not at all"
    );
}
