//! The process-wide price layers (`prices::global`): the user layer, the
//! fetched layer, what each install reports, and `reprice`.
//!
//! Every test mutates process-wide state or asserts on `tracing` output, so
//! all of them serialize on `LOCK` and start from built-in data only.

use std::sync::{Arc, Mutex, MutexGuard};
use tracing_subscriber::layer::SubscriberExt;
use yoagent::provider::prices::global::{self, InstallPolicy, OverrideReport};
use yoagent::provider::{CostConfig, ModelConfig, PriceChange, PriceTable};

static LOCK: Mutex<()> = Mutex::new(());

fn exclusive() -> MutexGuard<'static, ()> {
    let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    global::clear_override();
    global::clear_fetched();
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

fn input(config: ModelConfig) -> f64 {
    config.cost.expect("priced").input_per_million
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

/// Run `f` with `tracing` captured; return its result and the `WARN` lines.
fn warns_of<T>(f: impl FnOnce() -> T) -> (T, Vec<String>) {
    let logs = CapturedLogs::default();
    let out = {
        let _log =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(logs.clone()));
        f()
    };
    let warns = logs
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|l| l.starts_with("WARN"))
        .cloned()
        .collect();
    (out, warns)
}

fn names(changes: &[PriceChange]) -> Vec<String> {
    let mut v: Vec<String> = changes
        .iter()
        .map(|c| format!("{}/{}", c.provider, c.model))
        .collect();
    v.sort();
    v
}

#[test]
fn user_override_beats_builtin_per_model() {
    let _g = exclusive();
    let builtin_sonnet = ModelConfig::claude_sonnet_5().cost;
    assert_eq!(builtin_sonnet.as_ref().unwrap().input_per_million, 2.0);
    // Positive control: unlisted before the override.
    assert!(ModelConfig::deepseek("deepseek-flash", "D").cost.is_none());

    let _ = global::install_override(sonnet_override());

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
    assert_eq!(input(ModelConfig::deepseek("deepseek-flash", "D")), 0.3);
    // Gateways still never look up, whatever the table says.
    assert!(ModelConfig::opencode_zen("claude-sonnet-5").cost.is_none());
    assert_eq!(
        global::resolved()
            .cost("anthropic", "claude-sonnet-5")
            .unwrap()
            .input_per_million,
        1.8
    );

    global::clear_override();
    assert_eq!(ModelConfig::claude_sonnet_5().cost, builtin_sonnet);
    assert!(ModelConfig::deepseek("deepseek-flash", "D").cost.is_none());
}

/// A cost set on a config is not touched by later installs (only
/// constructors read the layers)...
#[test]
fn an_explicit_cost_wins_over_the_layers() {
    let _g = exclusive();
    let mut config = ModelConfig::claude_sonnet_5();
    config.cost = Some(CostConfig::new(7.0, 7.0));
    let _ = global::install_override(sonnet_override());
    let _ = global::install_fetched(sonnet_override());
    assert_eq!(config.cost, Some(CostConfig::new(7.0, 7.0)));
    // ...but reprice, as documented, replaces it on a constructor-priced
    // config. Positive control for the "explicit cost wins" claim's limits.
    assert_eq!(input(config.reprice()), 1.8);
}

/// Constructors resolve when they run: a config built before the override
/// keeps its price until `reprice`.
#[test]
fn configs_resolve_at_construction() {
    let _g = exclusive();
    let before = ModelConfig::claude_sonnet_5();
    let _ = global::install_override(sonnet_override());
    assert_eq!(before.cost.as_ref().unwrap().input_per_million, 2.0);
    assert_eq!(input(before.reprice()), 1.8);
}

#[test]
fn install_override_reports_changes_reverts_and_inert_entries() {
    let _g = exclusive();
    let (report, _) = warns_of(|| {
        global::install_override(table(
            r#"{"schema": 1, "providers": {
                "anthropic": {"claude-sonnet-5": {"input": 1.8, "output": 9.0, "cache_read": 0.18, "cache_write": 2.25}},
                "deepseek": {"deepseek-flash": {"input": 0.3, "output": 1.2}},
                "openrouter": {"x/y": {"input": 1, "output": 2}}}}"#,
        ))
    });
    assert_eq!(
        names(&report.changes),
        ["anthropic/claude-sonnet-5", "deepseek/deepseek-flash"]
    );
    // An inert entry is reported, never as a change.
    assert_eq!(report.inert, ["openrouter/x/y"]);
    let sonnet = report
        .changes
        .iter()
        .find(|c| c.model == "claude-sonnet-5")
        .unwrap();
    assert_eq!(sonnet.before.as_ref().unwrap().input_per_million, 2.0);
    assert!(report.reverted.is_empty());

    // Replacing it: deepseek-flash reverts to unpriced, sonnet reverts to
    // its built-in price, and the replacement is warned about.
    let (report, warns) = warns_of(|| {
        global::install_override(table(
            r#"{"schema": 1, "providers": {"anthropic": {"claude-opus-5":
                {"input": 5, "output": 25, "cache_read": 0.5, "cache_write": 6.25}}}}"#,
        ))
    });
    // Restating a built-in price is not a change.
    assert!(report.changes.is_empty(), "{:?}", report.changes);
    assert_eq!(
        names(&report.reverted),
        [
            "anthropic/claude-sonnet-5",
            "deepseek/deepseek-flash",
            "openrouter/x/y"
        ]
    );
    let flash = report
        .reverted
        .iter()
        .find(|c| c.model == "deepseek-flash")
        .unwrap();
    assert!(flash.after.is_none());
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("replaces a previously installed override of 3 entries")),
        "{:?}",
        report.warnings
    );
    assert_eq!(warns.len(), report.warnings.len(), "{warns:?}");
}

/// Whole-entry replacement stays, but its silent failure modes are warned
/// about — and returned.
#[test]
fn risky_overrides_are_warned_about() {
    let _g = exclusive();
    let (report, warns): (OverrideReport, _) = warns_of(|| {
        global::install_override(table(
            r#"{"schema": 1, "providers": {
                "openai": {"gpt-5.5": {"input": 5, "output": 30}},
                "openrouter": {"x/y": {"input": 1, "output": 2}}}}"#,
        ))
    });
    let all = report.warnings.join("\n");
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
    assert!(all.contains("openrouter/x/y"), "{all}");
    assert_eq!(report.warnings.len(), 4, "{all}");
    assert_eq!(warns.len(), 4, "{warns:?}");

    // Positive control: a complete override of a looked-up provider is quiet.
    global::clear_override();
    let (report, warns) = warns_of(|| global::install_override(sonnet_override()));
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    assert!(warns.is_empty(), "{warns:?}");
}

/// Item 18: override changes are computed against the resolved table, which
/// includes the fetched layer — not against the built-in data alone.
#[test]
fn an_override_after_a_fetch_reports_against_the_fetched_price() {
    let _g = exclusive();
    let _ = global::install_fetched(table(
        r#"{"schema": 1, "providers": {
            "anthropic": {"claude-sonnet-5": {"input": 1.5, "output": 7.5}},
            "deepseek": {"deepseek-flash": {"input": 0.15, "output": 0.6,
                "tiers": [{"above_prompt_tokens": 128000, "input": 0.3, "output": 1.2}]}}}}"#,
    ));
    let builtin = PriceTable::builtin();
    let (report, _) = warns_of(|| {
        global::install_override(table(&format!(
            r#"{{"schema": 1, "providers": {{
                "anthropic": {{"claude-sonnet-5": {}}},
                "deepseek": {{"deepseek-flash": {{"input": 0.15, "output": 0.6}}}}}}}}"#,
            serde_json::to_string(&serde_json::json!({
                "input": 2.0, "output": 10.0, "cache_read": 0.2, "cache_write": 2.5
            }))
            .unwrap()
        )))
    });
    // Restoring sonnet to exactly its built-in rates is still a change: the
    // fetched layer had it at 1.5. (Against the built-in data alone it would
    // look like no change at all.)
    let sonnet: Vec<&PriceChange> = report
        .changes
        .iter()
        .filter(|c| c.model == "claude-sonnet-5")
        .collect();
    assert_eq!(sonnet.len(), 1, "{:?}", report.changes);
    assert_eq!(sonnet[0].before.as_ref().unwrap().input_per_million, 1.5);
    assert_eq!(
        sonnet[0].after,
        builtin.cost("anthropic", "claude-sonnet-5")
    );
    // Only the fetched layer had deepseek-flash's tier; dropping it warns.
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("deepseek/deepseek-flash drops the 1 context tier")),
        "{:?}",
        report.warnings
    );
}

#[test]
fn install_fetched_reports_against_the_previous_state_and_marks_shadowed() {
    let _g = exclusive();
    let first = table(
        r#"{"schema": 1, "providers": {
            "anthropic": {"claude-sonnet-5": {"input": 1.5, "output": 7.5}},
            "deepseek": {"deepseek-flash": {"input": 0.15, "output": 0.6}}}}"#,
    );
    let changes = global::install_fetched(first);
    assert_eq!(
        names(&changes),
        ["anthropic/claude-sonnet-5", "deepseek/deepseek-flash"]
    );
    assert!(changes.iter().all(|c| !c.shadowed));

    // The user layer overrides sonnet; a new fetched layer that drops
    // deepseek-flash and moves sonnet again reports both — the revert, and
    // sonnet's change marked as shadowed (it bills at the override).
    let _ = global::install_override(table(
        r#"{"schema": 1, "providers": {"anthropic": {"claude-sonnet-5": {"input": 1.0, "output": 5.0}}}}"#,
    ));
    let (changes, warns) = warns_of(|| {
        global::install_fetched(table(
            r#"{"schema": 1, "providers": {"anthropic": {"claude-sonnet-5": {"input": 1.2, "output": 6.0}}}}"#,
        ))
    });
    assert_eq!(
        names(&changes),
        ["anthropic/claude-sonnet-5", "deepseek/deepseek-flash"]
    );
    let sonnet = changes
        .iter()
        .find(|c| c.model == "claude-sonnet-5")
        .unwrap();
    assert!(sonnet.shadowed);
    assert_eq!(sonnet.before.as_ref().unwrap().input_per_million, 1.5);
    assert!(sonnet.to_string().ends_with("[shadowed by the user layer]"));
    let flash = changes
        .iter()
        .find(|c| c.model == "deepseek-flash")
        .unwrap();
    assert!(!flash.shadowed);
    assert!(flash.after.is_none(), "reverted to unpriced");
    // Replacing a non-empty fetched layer is warned about.
    assert!(
        warns
            .iter()
            .any(|w| w.contains("replaces a fetched layer of 2 entries")),
        "{warns:?}"
    );
    assert_eq!(input(ModelConfig::claude_sonnet_5()), 1.0);
}

/// `AddOnly` merges into the fetched layer only models no lower layer
/// prices, and never replaces a previous layer.
#[test]
fn add_only_merges_and_never_overrides() {
    let _g = exclusive();
    let _ = global::install_fetched(table(
        r#"{"schema": 1, "providers": {"deepseek": {"deepseek-flash": {"input": 0.15, "output": 0.6}}}}"#,
    ));
    let (changes, warns) = warns_of(|| {
        global::install_fetched_with(
            table(
                r#"{"schema": 1, "providers": {
                    "anthropic": {"claude-sonnet-5": {"input": 1.5, "output": 7.5}},
                    "deepseek": {"deepseek-flash": {"input": 9, "output": 9},
                                 "deepseek-v4-pro": {"input": 0.435, "output": 0.87}}}}"#,
            ),
            InstallPolicy::AddOnly,
        )
    });
    // Only the model neither the built-in data nor the current fetched layer
    // listed.
    assert_eq!(names(&changes), ["deepseek/deepseek-v4-pro"]);
    assert!(warns.is_empty(), "{warns:?}");
    assert_eq!(
        ModelConfig::claude_sonnet_5().cost,
        PriceTable::builtin().cost("anthropic", "claude-sonnet-5")
    );
    // The earlier fetched entry survived (merged, not replaced).
    assert_eq!(input(ModelConfig::deepseek("deepseek-flash", "D")), 0.15);
    assert_eq!(input(ModelConfig::deepseek("deepseek-v4-pro", "D")), 0.435);

    // Positive control: the default policy replaces the layer and overrides.
    let changes = global::install_fetched_with(
        table(
            r#"{"schema": 1, "providers": {"anthropic": {"claude-sonnet-5": {"input": 1.5, "output": 7.5}}}}"#,
        ),
        InstallPolicy::default(),
    );
    assert_eq!(
        names(&changes),
        [
            "anthropic/claude-sonnet-5",
            "deepseek/deepseek-flash",
            "deepseek/deepseek-v4-pro"
        ]
    );
    assert_eq!(input(ModelConfig::claude_sonnet_5()), 1.5);
    assert!(ModelConfig::deepseek("deepseek-flash", "D").cost.is_none());
}

/// Disagreements with the built-in data are logged: the first five, then
/// a count of the rest.
#[test]
fn fetched_disagreements_are_logged_and_truncated() {
    let _g = exclusive();
    let mut fetched = PriceTable::new();
    let builtin = PriceTable::builtin();
    let seven: Vec<(String, String)> = builtin
        .iter()
        .take(7)
        .map(|(p, m, _)| (p.to_string(), m.to_string()))
        .collect();
    for (p, m) in &seven {
        let mut entry = builtin.entry(p, m).unwrap().clone();
        entry.cost.output_per_million += 1.0;
        fetched.insert(p, m, entry).unwrap();
    }
    let (changes, warns) = warns_of(|| global::install_fetched(fetched));
    assert_eq!(changes.len(), 7);
    let log = warns
        .iter()
        .find(|w| w.contains("disagrees with the built-in data"))
        .expect("the disagreement is logged");
    assert!(log.contains("on 7 model(s)"), "{log}");
    assert!(log.contains("and 2 more"), "{log}");

    // Positive control: an agreeing table logs no disagreement.
    global::clear_fetched();
    let (_, warns) = warns_of(|| global::install_fetched(PriceTable::builtin()));
    assert!(warns.is_empty(), "{warns:?}");
}

/// Precedence end to end: user > fetched > built-in.
#[test]
fn precedence_user_over_fetched_over_builtin() {
    let _g = exclusive();
    let builtin_opus = ModelConfig::claude_opus_5().cost;
    let _ = global::install_fetched(table(
        r#"{"schema": 1, "providers": {"anthropic": {
            "claude-sonnet-5": {"input": 1.5, "output": 7.5},
            "claude-haiku-4-5": {"input": 0.5, "output": 2.5}}}}"#,
    ));
    assert_eq!(input(ModelConfig::claude_sonnet_5()), 1.5);
    assert_eq!(ModelConfig::claude_opus_5().cost, builtin_opus);
    let _ = global::install_override(table(
        r#"{"schema": 1, "providers": {"anthropic": {"claude-sonnet-5": {"input": 1.0, "output": 5.0}}}}"#,
    ));
    assert_eq!(input(ModelConfig::claude_sonnet_5()), 1.0);
    assert_eq!(input(ModelConfig::claude_haiku_4_5()), 0.5);
    global::clear_override();
    assert_eq!(input(ModelConfig::claude_sonnet_5()), 1.5);
    global::clear_fetched();
    assert_eq!(
        ModelConfig::claude_sonnet_5().cost,
        PriceTable::builtin().cost("anthropic", "claude-sonnet-5")
    );
}

/// `reprice` repeats the constructor lookup — including clearing a price
/// that is no longer listed — and only for configs a first-party constructor
/// priced whose provider constructors still look up.
#[test]
fn reprice_repeats_the_constructor_lookup() {
    let _g = exclusive();
    let _ = global::install_override(sonnet_override());
    let flash = ModelConfig::deepseek("deepseek-flash", "D");
    let sonnet = ModelConfig::claude_sonnet_5();
    let generic = ModelConfig::anthropic("claude-sonnet-5", "S");
    let gateway_table = table(
        r#"{"schema": 1, "providers": {"opencode-zen": {"claude-sonnet-5": {"input": 2, "output": 10}}}}"#,
    );
    let gateway = ModelConfig::opencode_zen("claude-sonnet-5").with_prices(&gateway_table);
    assert!(flash.cost.is_some());
    assert!(gateway.cost.is_some());

    global::clear_override();
    // Listed before, not now: cleared (with_prices would never do this).
    assert!(flash.clone().reprice().cost.is_none());
    assert!(flash.with_prices(&global::resolved()).cost.is_some());
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

/// Item 1: a first-party config whose provider was changed is left alone —
/// no panic (the constructor's debug assertion is not on this path) and no
/// lookup under a provider constructors do not read. Run in debug by
/// `cargo test` and in release by `cargo test --release`.
#[test]
fn reprice_ignores_a_config_whose_provider_changed() {
    let _g = exclusive();
    let _ = global::install_override(table(
        r#"{"schema": 1, "providers": {"openrouter": {"claude-sonnet-5": {"input": 9, "output": 9}}}}"#,
    ));
    let mut config = ModelConfig::claude_sonnet_5();
    config.provider = "openrouter".into();
    config.cost = Some(CostConfig::new(3.0, 3.0));
    let repriced = config.reprice();
    assert_eq!(repriced.cost, Some(CostConfig::new(3.0, 3.0)));
    // Positive control: with its provider restored it reprices.
    let mut restored = repriced;
    restored.provider = "anthropic".into();
    assert_eq!(
        restored.reprice().cost,
        PriceTable::builtin().cost("anthropic", "claude-sonnet-5")
    );
}
