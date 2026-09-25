//! `YOAGENT_PRICES` names a partial override file, read once on first use.
//!
//! Its own test binary with a single test: the variable is read when the
//! process-wide table is first touched, so nothing may touch it earlier.

use yoagent::provider::{ModelConfig, PriceTable};

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

    // The outcome is visible to the host, not just logged.
    assert!(matches!(PriceTable::env_override_status(), Some(Ok(2))));

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
        PriceTable::load_env_override(),
        Err(yoagent::provider::PriceError::Io { .. })
    ));

    // install_override replaces the env-loaded layer.
    PriceTable::install_override(PriceTable::new());
    assert_eq!(
        ModelConfig::claude_haiku_4_5().cost,
        PriceTable::builtin().cost("anthropic", "claude-haiku-4-5")
    );
}
