//! The live opt-ins: `prices::enable_live` / `enable_live_cached` install a
//! fetched table over the bundled snapshot, or fall back to the snapshot and
//! say so.
//!
//! Every test resets the process-wide layers, so they serialize on `LOCK`.

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use yoagent::provider::prices::{self, global};
use yoagent::provider::{CacheOptions, ModelConfig, PriceOrigin, PriceSource, PriceTable};

static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Serialize, and start from nothing priced.
async fn reset() -> tokio::sync::MutexGuard<'static, ()> {
    let guard = LOCK.lock().await;
    global::clear_override();
    global::clear_fetched();
    global::clear_bundled();
    guard
}

const FETCHED: &str = r#"{"schema": 1, "providers": {"anthropic": {"claude-sonnet-5": {"input": 1.5, "output": 7.5}}}}"#;

async fn serve(status: u16, body: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/prices.json"))
        .respond_with(ResponseTemplate::new(status).set_body_string(body))
        .mount(&server)
        .await;
    server
}

fn source(server: &MockServer) -> PriceSource {
    PriceSource::Url(format!("{}/prices.json", server.uri()))
}

fn input(config: ModelConfig) -> f64 {
    config.cost.expect("priced").input_per_million
}

fn bundled(id: &str) -> Option<yoagent::provider::CostConfig> {
    PriceTable::builtin().cost("anthropic", id)
}

#[tokio::test]
async fn a_successful_fetch_is_installed_over_the_bundled_snapshot() {
    let _g = reset().await;
    let server = serve(200, FETCHED).await;
    assert!(ModelConfig::claude_sonnet_5().cost.is_none());

    let live = prices::enable_live(&source(&server)).await;
    assert!(
        matches!(live.origin, PriceOrigin::Fetched { .. }),
        "{:?}",
        live.origin
    );
    assert!(!live.fell_back());
    assert!(global::is_bundled_enabled());
    // Fetched where it lists a model, the snapshot elsewhere.
    assert_eq!(input(ModelConfig::claude_sonnet_5()), 1.5);
    assert_eq!(ModelConfig::claude_opus_5().cost, bundled("claude-opus-5"));
    // Reported against the unpriced state before the call.
    assert_eq!(live.changes.len(), PriceTable::builtin().len());
    let sonnet = live
        .changes
        .iter()
        .find(|c| c.model == "claude-sonnet-5")
        .unwrap();
    assert!(sonnet.before.is_none());
    assert_eq!(sonnet.after.as_ref().unwrap().input_per_million, 1.5);
}

#[tokio::test]
async fn a_failed_fetch_falls_back_to_the_bundled_snapshot_and_says_so() {
    let _g = reset().await;
    let server = serve(500, "down").await;

    let live = prices::enable_live(&source(&server)).await;
    assert!(live.fell_back());
    assert!(live.origin.fetch_error().is_some(), "{:?}", live.origin);
    assert!(global::is_bundled_enabled());
    assert_eq!(global::resolved(), PriceTable::builtin());
    assert_eq!(
        ModelConfig::claude_sonnet_5().cost,
        bundled("claude-sonnet-5")
    );
    assert_eq!(live.changes.len(), PriceTable::builtin().len());
}

#[tokio::test]
async fn a_fresh_cache_is_installed_without_a_request() {
    let _g = reset().await;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string(FETCHED))
        .expect(0)
        .mount(&server)
        .await;
    let source = source(&server);
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("prices-cache.json");
    let prices: serde_json::Value = serde_json::from_str(FETCHED).unwrap();
    std::fs::write(
        &cache,
        serde_json::json!({"yoagent_price_cache": 1, "source": source.url(), "prices": prices})
            .to_string(),
    )
    .unwrap();

    let live = prices::enable_live_cached(&source, &cache, CacheOptions::new()).await;
    assert!(
        matches!(live.origin, PriceOrigin::Cache { .. }),
        "{:?}",
        live.origin
    );
    assert_eq!(input(ModelConfig::claude_sonnet_5()), 1.5);
    assert_eq!(ModelConfig::claude_opus_5().cost, bundled("claude-opus-5"));
}

#[tokio::test]
async fn a_failed_cached_fetch_without_a_cache_falls_back() {
    let _g = reset().await;
    let server = serve(503, "busy").await;
    let dir = tempfile::tempdir().unwrap();

    let live = prices::enable_live_cached(
        &source(&server),
        dir.path().join("missing.json"),
        CacheOptions::new(),
    )
    .await;
    assert!(live.fell_back());
    assert!(live.origin.fetch_error().is_some());
    assert_eq!(global::resolved(), PriceTable::builtin());
}

/// A user layer stays on top of a live install.
#[tokio::test]
async fn a_user_layer_still_wins_over_live_prices() {
    let _g = reset().await;
    let _ = global::install_override(
        PriceTable::from_json_str(
            r#"{"schema": 1, "providers": {"anthropic": {"claude-sonnet-5": {"input": 1.8, "output": 9.0}}}}"#,
        )
        .unwrap(),
    );
    // Alone, the user layer prices exactly what it lists.
    assert_eq!(input(ModelConfig::claude_sonnet_5()), 1.8);
    assert!(ModelConfig::claude_opus_5().cost.is_none());

    let server = serve(200, FETCHED).await;
    let live = prices::enable_live(&source(&server)).await;
    assert_eq!(input(ModelConfig::claude_sonnet_5()), 1.8);
    assert_eq!(ModelConfig::claude_opus_5().cost, bundled("claude-opus-5"));
    // What constructors bill for sonnet did not change, so it is not listed.
    assert!(live.changes.iter().all(|c| c.model != "claude-sonnet-5"));
}
