//! Nothing is priced until the process opts in (0.25).
//!
//! Its own test binary with a single test: it asserts on the process-wide
//! price table before anything installs a layer.

use yoagent::extension::Budget;
use yoagent::provider::mock::MockResponse;
use yoagent::provider::prices::{self, global};
use yoagent::provider::{MockProvider, ModelConfig, PriceTable};
use yoagent::types::Usage;
use yoagent::Agent;

fn usage() -> Usage {
    Usage {
        input: 1_000_000,
        output: 500_000,
        cache_read: 0,
        cache_write: 0,
        total_tokens: 1_500_000,
    }
}

#[tokio::test]
async fn nothing_is_priced_until_the_process_opts_in() {
    // A developer's YOAGENT_PRICES would be an opt-in of its own.
    global::clear_override();

    assert!(!global::pricing_enabled());
    assert!(!global::is_bundled_enabled());
    assert!(global::resolved().is_empty());
    for config in [
        ModelConfig::claude_sonnet_5(),
        ModelConfig::gpt_5_5(),
        ModelConfig::anthropic("claude-opus-5", "Opus"),
        ModelConfig::deepseek("deepseek-v4-pro", "DeepSeek"),
    ] {
        assert!(config.cost.is_none(), "{} is priced", config.id);
    }
    assert!(Budget::for_model(1.0, &ModelConfig::claude_sonnet_5()).is_none());

    // A run on a default-built preset reports its spend as unpriced.
    let early = ModelConfig::claude_sonnet_5();
    let provider = MockProvider::new(vec![MockResponse::TextWithUsage("ok".into(), usage())]);
    let mut agent = Agent::from_provider(provider, early.clone());
    let mut rx = agent.prompt("hi").await;
    while rx.recv().await.is_some() {}
    agent.finish().await;
    assert_eq!(agent.session_cost_usd(), None);
    assert_eq!(agent.total_cost_usd(), None);

    // The offline opt-in prices configs built afterwards, and says what
    // changed: every snapshot entry, from unpriced.
    let changes = prices::enable_bundled();
    assert_eq!(changes.len(), PriceTable::builtin().len());
    assert!(changes.iter().all(|c| c.before.is_none()));
    assert!(global::pricing_enabled() && global::is_bundled_enabled());
    assert_eq!(global::resolved(), PriceTable::builtin());
    assert_eq!(
        ModelConfig::claude_sonnet_5().cost,
        PriceTable::builtin().cost("anthropic", "claude-sonnet-5")
    );
    assert!(Budget::for_model(1.0, &ModelConfig::claude_sonnet_5()).is_some());
    // Idempotent.
    assert!(prices::enable_bundled().is_empty());

    // A config built before the opt-in keeps `None` until repriced.
    assert!(early.cost.is_none());
    assert_eq!(
        early.reprice().cost,
        PriceTable::builtin().cost("anthropic", "claude-sonnet-5")
    );

    // And off again.
    global::clear_bundled();
    assert!(!global::pricing_enabled());
    assert!(ModelConfig::claude_sonnet_5().cost.is_none());
}
