//! A config whose context window and output limit are a provider's generic
//! defaults (no preset, nothing set) is warned about once per model, when it
//! becomes an agent's or a sub-agent's.
//!
//! Its own test binary with a single test: the warnings are recorded
//! process-wide, so their order across tests would decide the outcome.

use std::sync::{Arc, Mutex};
use tracing_subscriber::layer::SubscriberExt;
use yoagent::provider::{MockProvider, ModelConfig};
use yoagent::sub_agent::SubAgentTool;
use yoagent::Agent;

/// Captures every warning's message and fields on this thread.
#[derive(Clone, Default)]
struct CapturedLogs(Arc<Mutex<Vec<String>>>);

impl CapturedLogs {
    /// The default-limits warnings since the last call.
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.lock().unwrap())
            .into_iter()
            .filter(|l| l.contains("generic defaults"))
            .collect()
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CapturedLogs {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        if *event.metadata().level() != tracing::Level::WARN {
            return;
        }
        struct Fields(String);
        impl tracing::field::Visit for Fields {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0.push_str(&format!(" {}={value:?}", field.name()));
            }
        }
        let mut fields = Fields(String::new());
        event.record(&mut fields);
        self.0.lock().unwrap().push(fields.0);
    }
}

fn agent(config: ModelConfig) -> Agent {
    Agent::from_provider(MockProvider::text("ok"), config)
}

#[test]
fn default_limits_are_warned_about_once_per_model() {
    let logs = CapturedLogs::default();
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(logs.clone()));

    // An unknown OpenAI-compatible id at the constructor's defaults.
    let _a = agent(ModelConfig::openai("my-finetune", "Mine"));
    let warned = logs.take();
    assert_eq!(warned.len(), 1, "{warned:?}");
    assert!(
        warned[0].contains("model openai/my-finetune: the context window (128000) and output limit (4096) are openai's generic defaults"),
        "{warned:?}"
    );
    assert!(
        warned[0].contains("set `context_window` and `max_tokens`"),
        "{warned:?}"
    );

    // Once per model: not again, whether by an agent or a sub-agent.
    let _b = agent(ModelConfig::openai("my-finetune", "Mine"));
    let _c = SubAgentTool::from_provider(
        "helper",
        Arc::new(MockProvider::text("ok")),
        ModelConfig::openai("my-finetune", "Mine"),
    );
    assert_eq!(logs.take(), Vec::<String>::new());

    // A preset, limits set by the caller, a local server: nothing.
    let _d = agent(ModelConfig::gpt_5_5());
    let mut set = ModelConfig::openai("other-finetune", "Other");
    set.context_window = 32_768;
    set.max_tokens = 8_192;
    let _e = agent(set);
    let _f = agent(ModelConfig::local("http://localhost:1234/v1", "llama"));
    assert_eq!(logs.take(), Vec::<String>::new());

    // A preset id built generically points at the preset.
    let _g = SubAgentTool::from_provider(
        "helper",
        Arc::new(MockProvider::text("ok")),
        ModelConfig::openai("gpt-5.5", "GPT-5.5"),
    );
    let warned = logs.take();
    assert_eq!(warned.len(), 1, "{warned:?}");
    assert!(
        warned[0].contains("ModelConfig::preset(\"openai\", \"gpt-5.5\") has the model's limits"),
        "{warned:?}"
    );

    // Switching models checks the new one.
    let mut h = agent(ModelConfig::gpt_5_5());
    h.set_model(ModelConfig::xai("grok-unknown", "Grok"));
    let warned = logs.take();
    assert_eq!(warned.len(), 1, "{warned:?}");
    assert!(warned[0].contains("model xai/grok-unknown"), "{warned:?}");
}
