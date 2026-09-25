//! An empty `YOAGENT_PRICES` counts as unset.
//!
//! Its own test binary with a single test: the variable is read once, when
//! the process-wide table is first touched.

use yoagent::provider::prices::global::{self, EnvOverride};
use yoagent::provider::{ModelConfig, PriceTable};

#[test]
fn an_empty_env_var_is_unset() {
    std::env::set_var("YOAGENT_PRICES", "");
    assert!(matches!(global::env_override_status(), EnvOverride::Unset));
    assert_eq!(global::resolved(), PriceTable::builtin());
    assert_eq!(
        ModelConfig::claude_sonnet_5().cost,
        PriceTable::builtin().cost("anthropic", "claude-sonnet-5")
    );
    assert!(global::load_env_override().unwrap().is_none());
    std::env::remove_var("YOAGENT_PRICES");
    assert!(global::load_env_override().unwrap().is_none());
}
