//! `YOAGENT_PRICES` names a partial override file, read once on first use.
//!
//! Its own test binary with a single test: the variable is read when the
//! process-wide table is first touched, so nothing may touch it earlier.

use yoagent::provider::prices::global::{self, EnvOverride};
use yoagent::provider::{ModelConfig, PriceError, PriceTable};

#[test]
fn env_var_file_overrides_what_it_lists_and_is_read_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("prices.json");
    std::fs::write(
        &path,
        r#"{"schema": 1, "providers": {
            "anthropic": {"claude-haiku-4-5": {"input": 0.8, "output": 4.0}},
            "xai": {"grok-4.7": {"input": 3.0, "output": 15.0, "cache_read": 0.75}}}}"#,
    )
    .unwrap();
    std::env::set_var("YOAGENT_PRICES", &path);

    // Listed: overridden, whole entry.
    let haiku = ModelConfig::claude_haiku_4_5().cost.unwrap();
    assert_eq!(haiku.input_per_million, 0.8);
    assert_eq!(haiku.cache_read_per_million, 0.0);
    assert_eq!(
        ModelConfig::xai("grok-4.7", "Grok")
            .cost
            .unwrap()
            .output_per_million,
        15.0
    );
    // Unlisted: built-in.
    assert_eq!(
        ModelConfig::claude_opus_5().cost,
        PriceTable::builtin().cost("anthropic", "claude-opus-5")
    );

    // The outcome is visible to the host, with the same warnings
    // install_override would report: haiku's entry drops two cache rates.
    match global::env_override_status() {
        EnvOverride::Loaded {
            path: loaded,
            entries,
            warnings,
            ..
        } => {
            assert_eq!(loaded, path);
            assert_eq!(entries, 2);
            assert_eq!(warnings.len(), 2, "{warnings:?}");
            assert!(warnings
                .iter()
                .all(|w| w.starts_with("anthropic/claude-haiku-4-5 leaves cache_")));
        }
        other => panic!("expected Loaded, got {other:?}"),
    }

    // Read once: pointing the variable elsewhere changes nothing.
    std::env::set_var("YOAGENT_PRICES", dir.path().join("missing.json"));
    assert_eq!(
        ModelConfig::claude_haiku_4_5()
            .cost
            .unwrap()
            .input_per_million,
        0.8
    );

    // load_env_override reads now, strictly, without installing.
    assert!(matches!(
        global::load_env_override(),
        Err(PriceError::Io { .. })
    ));

    // install_override replaces the env-loaded layer, says so, and reports
    // the models that revert.
    let report = global::install_override(PriceTable::new());
    assert!(report.changes.is_empty());
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("replaces the user layer loaded from YOAGENT_PRICES")),
        "{:?}",
        report.warnings
    );
    let mut reverted: Vec<String> = report
        .reverted
        .iter()
        .map(|c| format!("{}/{}:{}", c.provider, c.model, c.after.is_some()))
        .collect();
    reverted.sort();
    assert_eq!(
        reverted,
        ["anthropic/claude-haiku-4-5:true", "xai/grok-4.7:false"]
    );
    assert_eq!(
        ModelConfig::claude_haiku_4_5().cost,
        PriceTable::builtin().cost("anthropic", "claude-haiku-4-5")
    );
}
