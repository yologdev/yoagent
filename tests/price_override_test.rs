//! The process-wide price layers: user overrides over the built-in data, and
//! an explicit `config.cost` over both.
//!
//! These tests mutate process-wide state, so they live in their own test
//! binary and serialize on `LOCK`; each restores the layers it touched.

use std::sync::{Arc, Mutex, MutexGuard};
use tracing_subscriber::layer::SubscriberExt;
use yoagent::provider::{CostConfig, ModelConfig, PriceChange, PriceTable};

static LOCK: Mutex<()> = Mutex::new(());

/// Serialize, and start from built-in data only.
fn exclusive() -> MutexGuard<'static, ()> {
    let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    PriceTable::clear_override();
    guard
}

fn table(json: &str) -> PriceTable {
    PriceTable::from_json_str(json).unwrap()
}

fn sonnet_override() -> PriceTable {
    table(
        r#"{"schema": 1, "providers": {
            "anthropic": {"claude-sonnet-5": {"input": 1.8, "output": 9.0, "cache_read": 0.18, "cache_write": 2.25}},
            "deepseek": {"deepseek-flash": {"input": 0.3, "output": 1.2, "cache_read": 0.006}}}}"#,
    )
}

#[test]
fn user_override_beats_builtin_per_model() {
    let _g = exclusive();
    let builtin_sonnet = ModelConfig::claude_sonnet_5().cost;
    assert_eq!(builtin_sonnet.as_ref().unwrap().input_per_million, 2.0);
    // Positive control: unlisted before the override.
    assert!(ModelConfig::deepseek("deepseek-flash", "D").cost.is_none());

    let _ = PriceTable::install_override(sonnet_override());

    let sonnet = ModelConfig::claude_sonnet_5().cost.unwrap();
    assert_eq!(
        sonnet,
        CostConfig::new(1.8, 9.0)
            .with_cache_read(0.18)
            .with_cache_write(2.25)
    );
    // The generic constructor reads the same layer.
    assert_eq!(
        ModelConfig::anthropic("claude-sonnet-5", "S").cost,
        Some(sonnet)
    );
    // A partial override leaves everything else on the built-in data.
    assert_eq!(
        ModelConfig::claude_opus_5().cost,
        PriceTable::builtin().cost("anthropic", "claude-opus-5")
    );
    // And can price a model the built-in data does not know.
    assert_eq!(
        ModelConfig::deepseek("deepseek-flash", "D")
            .cost
            .unwrap()
            .input_per_million,
        0.3
    );
    // Gateways still never look up, whatever the table says.
    assert!(ModelConfig::opencode_zen("claude-sonnet-5").cost.is_none());
    assert_eq!(
        PriceTable::resolved()
            .cost("anthropic", "claude-sonnet-5")
            .unwrap()
            .input_per_million,
        1.8
    );

    PriceTable::clear_override();
    assert_eq!(ModelConfig::claude_sonnet_5().cost, builtin_sonnet);
    assert!(ModelConfig::deepseek("deepseek-flash", "D").cost.is_none());
}

#[test]
fn explicit_cost_beats_every_layer() {
    let _g = exclusive();
    let _ = PriceTable::install_override(sonnet_override());
    let mut config = ModelConfig::claude_sonnet_5();
    config.cost = Some(CostConfig::new(7.0, 7.0));
    assert_eq!(config.cost, Some(CostConfig::new(7.0, 7.0)));
    PriceTable::clear_override();
}

/// Constructors resolve when they run: a config built before the override
/// keeps its price; `with_prices(&PriceTable::resolved())` re-prices it.
#[test]
fn configs_resolve_at_construction() {
    let _g = exclusive();
    let before = ModelConfig::claude_sonnet_5();
    let _ = PriceTable::install_override(sonnet_override());
    assert_eq!(before.cost.as_ref().unwrap().input_per_million, 2.0);
    let repriced = before.with_prices(&PriceTable::resolved());
    assert_eq!(repriced.cost.unwrap().input_per_million, 1.8);
    PriceTable::clear_override();
}

#[test]
fn install_override_replaces_the_previous_override() {
    let _g = exclusive();
    let _ = PriceTable::install_override(sonnet_override());
    let _ = PriceTable::install_override(table(
        r#"{"schema": 1, "providers": {"anthropic": {"claude-opus-5": {"input": 1, "output": 2}}}}"#,
    ));
    // The first override is gone, not merged.
    assert_eq!(
        ModelConfig::claude_sonnet_5()
            .cost
            .unwrap()
            .input_per_million,
        2.0
    );
    assert!(ModelConfig::deepseek("deepseek-flash", "D").cost.is_none());
    assert_eq!(
        ModelConfig::claude_opus_5().cost.unwrap().input_per_million,
        1.0
    );
    PriceTable::clear_override();
}

/// Captures every event's level and fields on this thread.
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

fn install_capturing(t: PriceTable) -> (Vec<PriceChange>, Vec<String>) {
    let logs = CapturedLogs::default();
    let changes = {
        let _log =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(logs.clone()));
        PriceTable::install_override(t)
    };
    let warnings = logs
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|l| l.starts_with("WARN"))
        .cloned()
        .collect();
    (changes, warnings)
}

/// `install_override` returns what it changed against the layers below.
#[test]
fn install_override_returns_its_changes() {
    let _g = exclusive();
    let (changes, _) = install_capturing(sonnet_override());
    let mut names: Vec<String> = changes
        .iter()
        .map(|c| format!("{}/{}:{}", c.provider, c.model, c.before.is_some()))
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "anthropic/claude-sonnet-5:true",
            "deepseek/deepseek-flash:false"
        ]
    );
    // Restating a built-in price as billed is not a change.
    let (changes, _) = install_capturing(table(
        r#"{"schema": 1, "providers": {"anthropic": {"claude-haiku-4-5":
            {"input": 1, "output": 5, "cache_read": 0.1, "cache_write": 1.25}}}}"#,
    ));
    assert!(changes.is_empty(), "{changes:?}");
    PriceTable::clear_override();
}

/// Whole-entry replacement stays, but its silent failure modes are logged.
#[test]
fn risky_overrides_are_warned_about() {
    let _g = exclusive();
    // gpt-5.5 without its tier or cache rates; a provider no constructor
    // looks up.
    let (_, warnings) = install_capturing(table(
        r#"{"schema": 1, "providers": {
            "openai": {"gpt-5.5": {"input": 5, "output": 30}},
            "openrouter": {"x/y": {"input": 1, "output": 2}}}}"#,
    ));
    let all = warnings.join("\n");
    assert!(
        all.contains("openai/gpt-5.5 drops the 1 context tier"),
        "{all}"
    );
    assert!(
        all.contains("openai/gpt-5.5 leaves cache_read unset"),
        "{all}"
    );
    assert!(
        all.contains("openai/gpt-5.5 leaves cache_write unset"),
        "{all}"
    );
    assert!(all.contains("\"openrouter\""), "{all}");
    assert_eq!(warnings.len(), 4, "{all}");

    // Positive control: a complete override of a looked-up provider is quiet.
    let (_, warnings) = install_capturing(sonnet_override());
    assert!(warnings.is_empty(), "{warnings:?}");
    PriceTable::clear_override();
}

/// `reprice` repeats the constructor's lookup — including clearing a price
/// that is no longer listed — and only for configs a first-party
/// constructor priced.
#[test]
fn reprice_repeats_the_constructor_lookup() {
    let _g = exclusive();
    let _ = PriceTable::install_override(sonnet_override());
    let flash = ModelConfig::deepseek("deepseek-flash", "D");
    let sonnet = ModelConfig::claude_sonnet_5();
    let generic = ModelConfig::anthropic("claude-sonnet-5", "S");
    let gateway =
        ModelConfig::opencode_zen("claude-sonnet-5").with_prices(&sonnet_override_for_gateway());
    assert!(flash.cost.is_some());
    assert!(gateway.cost.is_some());

    PriceTable::clear_override();
    // Listed before, not now: cleared (with_prices would never do this).
    assert!(flash.clone().reprice().cost.is_none());
    assert!(flash.with_prices(&PriceTable::resolved()).cost.is_some());
    // Back to the built-in price.
    let builtin = PriceTable::builtin().cost("anthropic", "claude-sonnet-5");
    assert_eq!(sonnet.reprice().cost, builtin);
    assert_eq!(generic.reprice().cost, builtin);
    // A gateway was not priced by its constructor: left alone.
    assert!(gateway.clone().reprice().cost.is_some());
    // Nor is a deserialized config (the marker is not persisted).
    let json = serde_json::to_string(&ModelConfig::claude_opus_5()).unwrap();
    let mut loaded: ModelConfig = serde_json::from_str(&json).unwrap();
    loaded.cost = Some(CostConfig::new(1.0, 1.0));
    assert_eq!(loaded.reprice().cost, Some(CostConfig::new(1.0, 1.0)));
}

fn sonnet_override_for_gateway() -> PriceTable {
    table(
        r#"{"schema": 1, "providers": {"opencode-zen": {"claude-sonnet-5": {"input": 2, "output": 10}}}}"#,
    )
}
