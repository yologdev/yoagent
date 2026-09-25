//! Every priced preset's full `CostConfig`, pinned.
//!
//! Written against the 0.19.0 literals *before* prices moved into
//! `src/provider/prices.json`, so a green run proves the data file reproduces
//! each preset exactly — every rate, every tier, bit for bit (`f64` equality,
//! not a tolerance). Change a price here only together with the data file and
//! the vendor page that justifies it.
//!
//! Each test first drops the user layer (a developer's `YOAGENT_PRICES`),
//! so presets resolve against the built-in data alone.

use yoagent::provider::{ContextTier, CostConfig, ModelConfig};

fn pinned() -> Vec<(&'static str, ModelConfig, CostConfig)> {
    let flat = |i, o, r, w| CostConfig::new(i, o).with_cache_read(r).with_cache_write(w);
    let tiered = |base: CostConfig, i, o, r, w| {
        base.with_context_tier(
            ContextTier::new(272_000, i, o)
                .with_cache_read(r)
                .with_cache_write(w),
        )
    };
    vec![
        (
            "claude_fable_5",
            ModelConfig::claude_fable_5(),
            flat(10.0, 50.0, 1.0, 12.5),
        ),
        (
            "claude_fable_5_1",
            ModelConfig::claude_fable_5_1(),
            flat(10.0, 50.0, 0.25, 12.5),
        ),
        (
            "claude_opus_5_5",
            ModelConfig::claude_opus_5_5(),
            flat(4.0, 20.0, 0.2, 5.0),
        ),
        (
            "claude_opus_5",
            ModelConfig::claude_opus_5(),
            flat(5.0, 25.0, 0.5, 6.25),
        ),
        (
            "claude_opus_4_8",
            ModelConfig::claude_opus_4_8(),
            flat(5.0, 25.0, 0.5, 6.25),
        ),
        (
            "claude_sonnet_5",
            ModelConfig::claude_sonnet_5(),
            flat(2.0, 10.0, 0.2, 2.5),
        ),
        (
            "claude_haiku_4_5",
            ModelConfig::claude_haiku_4_5(),
            flat(1.0, 5.0, 0.1, 1.25),
        ),
        (
            "gpt_5_5",
            ModelConfig::gpt_5_5(),
            tiered(flat(5.0, 30.0, 0.5, 5.0), 10.0, 45.0, 1.0, 10.0),
        ),
        (
            "gpt_6_astra",
            ModelConfig::gpt_6_astra(),
            tiered(flat(10.0, 50.0, 1.0, 12.5), 20.0, 75.0, 2.0, 25.0),
        ),
        (
            "gpt_6_sol",
            ModelConfig::gpt_6_sol(),
            tiered(flat(2.0, 10.0, 0.2, 2.5), 4.0, 15.0, 0.4, 5.0),
        ),
        (
            "gpt_6_luna",
            ModelConfig::gpt_6_luna(),
            tiered(flat(0.1, 0.5, 0.01, 0.125), 0.2, 0.75, 0.02, 0.25),
        ),
        (
            "meta(muse-spark-1.1)",
            ModelConfig::meta("muse-spark-1.1", "Muse Spark 1.1"),
            flat(1.25, 4.25, 0.15, 1.25),
        ),
        (
            "meta(muse-spark-1.2)",
            ModelConfig::meta("muse-spark-1.2", "Muse Spark 1.2"),
            flat(1.25, 4.25, 0.15, 1.25),
        ),
    ]
}

#[test]
fn every_preset_cost_config_is_pinned() {
    // List prices only: a developer's YOAGENT_PRICES must not change them.
    yoagent::provider::prices::global::clear_override();
    for (name, config, expected) in pinned() {
        assert_eq!(
            config.cost.as_ref(),
            Some(&expected),
            "{name}: the resolved CostConfig changed"
        );
    }
}

/// Positive control: the pin is exact, so a one-ULP change is caught.
#[test]
fn the_pin_detects_a_one_ulp_change() {
    yoagent::provider::prices::global::clear_override();
    let (_, config, expected) = pinned().remove(0);
    let mut nudged = config.cost.unwrap();
    nudged.input_per_million = f64::from_bits(nudged.input_per_million.to_bits() + 1);
    assert_ne!(nudged, expected);
    let mut detiered = pinned().remove(7).1.cost.unwrap();
    detiered.context_tiers[0].cache_read_per_million = 0.5;
    assert_ne!(Some(detiered), pinned().remove(7).1.cost);
}
