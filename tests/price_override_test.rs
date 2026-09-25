//! The process-wide price layers: user overrides over the built-in data, and
//! an explicit `config.cost` over both.
//!
//! These tests mutate process-wide state, so they live in their own test
//! binary and serialize on `LOCK`; each restores the layers it touched.

use std::sync::{Mutex, MutexGuard};
use yoagent::provider::{CostConfig, ModelConfig, PriceTable};

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

    PriceTable::install_override(sonnet_override());

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
    PriceTable::install_override(sonnet_override());
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
    PriceTable::install_override(sonnet_override());
    assert_eq!(before.cost.as_ref().unwrap().input_per_million, 2.0);
    let repriced = before.with_prices(&PriceTable::resolved());
    assert_eq!(repriced.cost.unwrap().input_per_million, 1.8);
    PriceTable::clear_override();
}

#[test]
fn install_override_replaces_the_previous_override() {
    let _g = exclusive();
    PriceTable::install_override(sonnet_override());
    PriceTable::install_override(table(
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
