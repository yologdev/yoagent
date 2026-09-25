//! A bad `YOAGENT_PRICES` file must not panic: it is logged and ignored, and
//! every constructor keeps the built-in prices.
//!
//! Its own test binary with a single test, because the variable is read
//! once, on first use of the process-wide table.

use std::sync::{Arc, Mutex};
use tracing_subscriber::layer::SubscriberExt;
use yoagent::provider::prices::global::{self, EnvOverride};
use yoagent::provider::{ModelConfig, PriceError, PriceTable};

/// Captures every event's message and fields on this thread.
#[derive(Clone, Default)]
struct CapturedLogs(Arc<Mutex<Vec<String>>>);

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

#[test]
fn a_bad_env_file_is_logged_and_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("prices.json");
    // Valid JSON, invalid prices: a negative rate.
    std::fs::write(
        &path,
        r#"{"schema": 1, "providers": {"anthropic": {"claude-opus-5": {"input": -5, "output": 25}}}}"#,
    )
    .unwrap();
    std::env::set_var("YOAGENT_PRICES", &path);

    let logs = CapturedLogs::default();
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(logs.clone()));

    // First use reads the variable. No panic, built-in rates.
    let opus = ModelConfig::claude_opus_5();
    assert_eq!(
        opus.cost,
        PriceTable::builtin().cost("anthropic", "claude-opus-5")
    );
    assert_eq!(global::resolved(), PriceTable::builtin());

    // Positive control: the rejection was reported, naming the file and why.
    let logs = logs.0.lock().unwrap();
    let warning = logs
        .iter()
        .find(|l| l.starts_with("WARN") && l.contains("YOAGENT_PRICES"))
        .unwrap_or_else(|| panic!("no warning about the bad file; logs: {logs:?}"));
    assert!(warning.contains("prices.json"), "{warning}");
    assert!(warning.contains("non-negative"), "{warning}");
    drop(logs);

    // The host can see it too, and fail fast if it wants.
    match global::env_override_status() {
        EnvOverride::Rejected {
            path: rejected,
            error,
            ..
        } => {
            assert_eq!(rejected, path);
            assert!(matches!(error, PriceError::InvalidRate { .. }), "{error}");
        }
        other => panic!("expected Rejected, got {other:?}"),
    }
    assert!(matches!(
        global::load_env_override(),
        Err(PriceError::InvalidRate { .. })
    ));
    // A typo'd field in a hand-written file is UnknownField, strictly.
    std::fs::write(
        &path,
        r#"{"schema": 1, "providers": {"anthropic": {"claude-opus-5": {"input": 5, "output": 25, "cache_reed": 0.5}}}}"#,
    )
    .unwrap();
    assert!(matches!(
        global::load_env_override(),
        Err(PriceError::UnknownField { .. })
    ));
    std::env::remove_var("YOAGENT_PRICES");
    assert!(global::load_env_override().unwrap().is_none());
}
