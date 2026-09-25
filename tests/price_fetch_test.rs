//! Opt-in live price sources: models.dev mapping, our-format URLs, timeouts,
//! the cache helper, and the fetched layer's place in the precedence order.
//!
//! Some tests install process-wide layers, so they serialize on `LOCK` and
//! restore what they touched.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tracing_subscriber::layer::SubscriberExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use yoagent::provider::{
    CostConfig, FetchPolicy, ModelConfig, PriceError, PriceOrigin, PriceSource, PriceTable,
};

/// A slice of the real <https://models.dev/api.json> (2026-09-25): flat and
/// tiered entries, the legacy `context_over_200k` mirror, a `reasoning` rate
/// equal to output (DeepSeek), one that differs (skipped), a model without a
/// cost (skipped), and `alibaba` (our `qwen`).
const MODELS_DEV: &str = include_str!("fixtures/models_dev_slice.json");

static LOCK: Mutex<()> = Mutex::new(());

fn exclusive() -> MutexGuard<'static, ()> {
    let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    PriceTable::clear_override();
    PriceTable::clear_fetched();
    guard
}

async fn serve(route: &str, template: ResponseTemplate) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(route))
        .respond_with(template)
        .mount(&server)
        .await;
    server
}

fn ok(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_string(body)
}

#[test]
fn models_dev_fixture_maps_into_our_schema() {
    let t = PriceTable::from_models_dev_json(MODELS_DEV).unwrap();

    // Flat, with all four rates.
    assert_eq!(
        t.cost("anthropic", "claude-opus-5-5"),
        Some(
            CostConfig::new(4.0, 20.0)
                .with_cache_read(0.2)
                .with_cache_write(5.0)
        )
    );
    // Tiered from the `tiers` array (the mirror is not a second tier).
    let gpt = t.cost("openai", "gpt-5.5").unwrap();
    assert_eq!(gpt.context_tiers.len(), 1);
    assert_eq!(gpt.context_tiers[0].above_prompt_tokens, 272_000);
    assert_eq!(gpt.context_tiers[0].input_per_million, 10.0);
    assert_eq!(gpt.context_tiers[0].cache_read_per_million, 1.0);
    // models.dev omits gpt-5.5's cache_write: 0, which bills at input.
    assert_eq!(gpt.cache_write_per_million, 0.0);
    let gemini = t.cost("google", "gemini-2.5-pro").unwrap();
    assert_eq!(gemini.context_tiers[0].above_prompt_tokens, 200_000);
    // reasoning == output is expressible.
    assert_eq!(
        t.cost("deepseek", "deepseek-v4-pro")
            .unwrap()
            .output_per_million,
        0.87
    );
    // alibaba is our qwen.
    assert!(t.cost("qwen", "qwen-flash").is_some());
    assert!(t.cost("alibaba", "qwen-flash").is_none());
    // Skipped: a separate reasoning rate, and no cost at all.
    assert!(t
        .cost("openrouter", "perplexity/sonar-deep-research")
        .is_none());
    assert!(t.cost("poe", "cerebras/qwen3-32b-cs").is_none());
    assert_eq!(t.len(), 11);

    // Against the built-in data: every overlapping model agrees as billed
    // (gpt-5.5's implicit cache_write equals its explicit input-rate one), so
    // only new models are reported.
    let changes = t.changes_from(&PriceTable::builtin());
    assert!(
        changes.iter().all(|c| c.before.is_none()),
        "{:?}",
        changes
            .iter()
            .filter(|c| c.before.is_some())
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    );
    assert!(changes
        .iter()
        .any(|c| c.provider == "deepseek" && c.model == "deepseek-flash"));
}

#[test]
fn a_changed_models_dev_envelope_is_an_error_not_an_empty_table() {
    for body in ["[]", "{}", r#"{"anthropic": {"data": {}}}"#, "null"] {
        let e = PriceTable::from_models_dev_json(body).unwrap_err();
        assert!(matches!(e, PriceError::ModelsDev(_)), "{body}: {e}");
    }
}

#[tokio::test]
async fn fetches_models_dev_format_from_a_url() {
    let server = serve("/api.json", ok(MODELS_DEV)).await;
    let source = PriceSource::ModelsDevAt(format!("{}/api.json", server.uri()));
    let t = PriceTable::fetch(&source).await.unwrap();
    assert_eq!(t, {
        // Same mapping, but entries name the URL they came from.
        let mut expected = PriceTable::new();
        for (p, m, e) in PriceTable::from_models_dev_json(MODELS_DEV).unwrap().iter() {
            let entry =
                yoagent::provider::PriceEntry::new(e.cost.clone()).with_source(source.url());
            expected.insert(p, m, entry).unwrap();
        }
        expected
    });
}

#[tokio::test]
async fn fetches_our_format_from_a_url() {
    let body = r#"{"schema": 1, "providers": {"anthropic": {"claude-sonnet-5": {"input": 1.5, "output": 7.5}}}}"#;
    let server = serve("/prices.json", ok(body)).await;
    let t = PriceTable::fetch(&PriceSource::Url(format!("{}/prices.json", server.uri())))
        .await
        .unwrap();
    assert_eq!(t, PriceTable::from_json_str(body).unwrap());
    // Our-format sources are validated like any other table.
    let bad = serve("/prices.json", ok(r#"{"schema": 2, "providers": {}}"#)).await;
    let e = PriceTable::fetch(&PriceSource::Url(format!("{}/prices.json", bad.uri())))
        .await
        .unwrap_err();
    assert!(matches!(e, PriceError::UnsupportedSchema { .. }), "{e}");
}

#[tokio::test]
async fn http_errors_and_timeouts_are_reported() {
    let down = serve("/prices.json", ResponseTemplate::new(503)).await;
    let e = PriceTable::fetch(&PriceSource::Url(format!("{}/prices.json", down.uri())))
        .await
        .unwrap_err();
    assert!(matches!(e, PriceError::Http { status: 503, .. }), "{e}");

    let slow = serve(
        "/prices.json",
        ok(r#"{"schema": 1, "providers": {}}"#).set_delay(Duration::from_secs(5)),
    )
    .await;
    let started = std::time::Instant::now();
    let e = PriceTable::fetch_with_timeout(
        &PriceSource::Url(format!("{}/prices.json", slow.uri())),
        Duration::from_millis(200),
    )
    .await
    .unwrap_err();
    assert!(matches!(e, PriceError::Timeout { .. }), "{e}");
    assert!(started.elapsed() < Duration::from_secs(4));
}

#[test]
fn source_urls() {
    assert_eq!(PriceSource::ModelsDev.url(), "https://models.dev/api.json");
    assert_eq!(
        PriceSource::YoagentMain.url(),
        "https://raw.githubusercontent.com/yologdev/yoagent/main/src/provider/prices.json"
    );
}

/// The file `PriceSource::YoagentMain` serves is this crate's own data file,
/// so it must parse as its format.
#[test]
fn the_checked_in_file_is_what_yoagent_main_serves() {
    let text = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/provider/prices.json"),
    )
    .unwrap();
    assert_eq!(
        PriceTable::from_json_str(&text).unwrap(),
        PriceTable::builtin()
    );
}

const FETCHED: &str = r#"{"schema": 1, "providers": {"anthropic": {"claude-sonnet-5": {"input": 1.5, "output": 7.5}}}}"#;

#[tokio::test]
async fn a_fresh_cache_is_used_without_a_request() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("prices.json");
    let cached = PriceTable::from_json_str(FETCHED).unwrap();
    std::fs::write(&cache, cached.to_json()).unwrap();

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ok("{}"))
        .expect(0)
        .mount(&server)
        .await;
    let source = PriceSource::Url(format!("{}/prices.json", server.uri()));
    let got =
        PriceTable::fetch_cached(&source, &cache, Duration::from_secs(3600), Duration::MAX).await;
    assert_eq!(got.origin, PriceOrigin::Cache);
    assert_eq!(got.table, cached);
}

#[tokio::test]
async fn an_expired_cache_is_refetched_and_rewritten() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("nested/dir/prices.json");
    std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
    std::fs::write(&cache, PriceTable::new().to_json()).unwrap();

    let server = serve("/api.json", ok(MODELS_DEV)).await;
    let source = PriceSource::ModelsDevAt(format!("{}/api.json", server.uri()));
    let got = PriceTable::fetch_cached(&source, &cache, Duration::ZERO, Duration::MAX).await;
    assert_eq!(got.origin, PriceOrigin::Fetched);
    assert_eq!(got.table.len(), 11);
    // The cache now holds the mapped table, in our format.
    assert_eq!(PriceTable::from_path(&cache).unwrap(), got.table);

    // And a missing cache directory is created.
    let fresh = dir.path().join("new/prices.json");
    let got = PriceTable::fetch_cached(&source, &fresh, Duration::ZERO, Duration::MAX).await;
    assert_eq!(got.origin, PriceOrigin::Fetched);
    assert!(fresh.exists());
}

#[tokio::test]
async fn offline_falls_back_to_the_stale_cache_then_builtin() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("prices.json");
    let down = serve("/prices.json", ResponseTemplate::new(500)).await;
    let source = PriceSource::Url(format!("{}/prices.json", down.uri()));

    // No cache: built-in.
    let got = PriceTable::fetch_cached(&source, &cache, Duration::ZERO, Duration::MAX).await;
    assert_eq!(got.origin, PriceOrigin::Builtin);
    assert_eq!(got.table, PriceTable::builtin());
    assert!(!cache.exists(), "a failed fetch must not write a cache");

    // An expired cache beats built-in.
    let stale = PriceTable::from_json_str(FETCHED).unwrap();
    std::fs::write(&cache, stale.to_json()).unwrap();
    let got = PriceTable::fetch_cached(&source, &cache, Duration::ZERO, Duration::MAX).await;
    assert_eq!(got.origin, PriceOrigin::StaleCache);
    assert_eq!(got.table, stale);
    assert!(got.age.is_some());
    // The fetch error behind the fallback is returned, not just logged.
    assert!(
        matches!(
            got.error.as_deref(),
            Some(PriceError::Http { status: 500, .. })
        ),
        "{:?}",
        got.error
    );

    // Too old for `max_stale`: not used, built-in instead.
    let old = std::time::SystemTime::now() - Duration::from_secs(10 * 24 * 3600);
    std::fs::File::options()
        .write(true)
        .open(&cache)
        .unwrap()
        .set_modified(old)
        .unwrap();
    let week = Duration::from_secs(7 * 24 * 3600);
    let got = PriceTable::fetch_cached(&source, &cache, Duration::ZERO, week).await;
    assert_eq!(got.origin, PriceOrigin::Builtin);
    assert!(got.error.is_some());
    // Positive control: within the bound, the same file is used.
    let got = PriceTable::fetch_cached(&source, &cache, Duration::ZERO, 2 * week).await;
    assert_eq!(got.origin, PriceOrigin::StaleCache);
    let age = got.age.unwrap();
    assert!(
        age >= Duration::from_secs(10 * 24 * 3600) && age < 2 * week,
        "{age:?}"
    );

    // A corrupt cache is ignored, not fatal.
    std::fs::write(&cache, "not json").unwrap();
    let got =
        PriceTable::fetch_cached(&source, &cache, Duration::from_secs(3600), Duration::MAX).await;
    assert_eq!(got.origin, PriceOrigin::Builtin);
}

/// A modification time in the future is an unknown age: never fresh, and a
/// stale fallback only when `max_stale` is unbounded.
#[tokio::test]
async fn a_future_mtime_is_treated_as_expired() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("prices.json");
    std::fs::write(
        &cache,
        PriceTable::from_json_str(FETCHED).unwrap().to_json(),
    )
    .unwrap();
    let future = std::time::SystemTime::now() + Duration::from_secs(365 * 24 * 3600);
    std::fs::File::options()
        .write(true)
        .open(&cache)
        .unwrap()
        .set_modified(future)
        .unwrap();

    // With a year of max_age, a "fresh" reading would skip the request; it
    // must be refetched instead.
    let server = serve("/prices.json", ok(FETCHED)).await;
    let source = PriceSource::Url(format!("{}/prices.json", server.uri()));
    let year = Duration::from_secs(365 * 24 * 3600);
    let got = PriceTable::fetch_cached(&source, &cache, year, year).await;
    assert_eq!(got.origin, PriceOrigin::Fetched);

    std::fs::File::options()
        .write(true)
        .open(&cache)
        .unwrap()
        .set_modified(future)
        .unwrap();
    let down = serve("/prices.json", ResponseTemplate::new(500)).await;
    let source = PriceSource::Url(format!("{}/prices.json", down.uri()));
    let got = PriceTable::fetch_cached(&source, &cache, year, year).await;
    assert_eq!(got.origin, PriceOrigin::Builtin);
    let got = PriceTable::fetch_cached(&source, &cache, year, Duration::MAX).await;
    assert_eq!(got.origin, PriceOrigin::StaleCache);
    assert_eq!(got.age, None);
}

/// A failed cache write does not lose the fetched table, and is reported.
#[tokio::test]
async fn a_failed_cache_write_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    // The cache's parent is a file, so the directory cannot be created.
    let blocker = dir.path().join("not-a-dir");
    std::fs::write(&blocker, "x").unwrap();
    let cache = blocker.join("prices.json");
    let server = serve("/prices.json", ok(FETCHED)).await;
    let source = PriceSource::Url(format!("{}/prices.json", server.uri()));
    let got = PriceTable::fetch_cached(&source, &cache, Duration::ZERO, Duration::MAX).await;
    assert_eq!(got.origin, PriceOrigin::Fetched);
    assert_eq!(got.table, PriceTable::from_json_str(FETCHED).unwrap());
    assert!(
        matches!(got.error.as_deref(), Some(PriceError::Io { .. })),
        "{:?}",
        got.error
    );
    // Positive control: a writable path reports no error.
    let fine = dir.path().join("ok/prices.json");
    let got = PriceTable::fetch_cached(&source, &fine, Duration::ZERO, Duration::MAX).await;
    assert!(got.error.is_none(), "{:?}", got.error);
}

#[test]
fn the_models_dev_report_says_what_was_skipped_and_why() {
    let (table, skipped) = PriceTable::from_models_dev_json_report(MODELS_DEV).unwrap();
    assert_eq!(table.len(), 11);
    let reason = |p: &str, m: &str| {
        skipped
            .iter()
            .find(|s| s.provider == p && s.model == m)
            .map(|s| s.reason.clone())
            .unwrap_or_else(|| panic!("{p}/{m} not reported: {skipped:?}"))
    };
    assert!(
        reason("openrouter", "perplexity/sonar-deep-research").contains("reasoning"),
        "{skipped:?}"
    );
    assert!(reason("poe", "cerebras/qwen3-32b-cs").contains("no cost"));
    assert_eq!(skipped.len(), 2);
}

/// models.dev's `opencode` is this crate's `opencode-zen`, as `alibaba` is
/// `qwen`.
#[test]
fn models_dev_provider_keys_are_renamed() {
    let doc = r#"{
        "opencode": {"models": {"claude-sonnet-5": {"cost": {"input": 2, "output": 10}}}},
        "alibaba": {"models": {"qwen-flash": {"cost": {"input": 0.05, "output": 0.4}}}}
    }"#;
    let t = PriceTable::from_models_dev_json(doc).unwrap();
    assert!(t.cost("opencode-zen", "claude-sonnet-5").is_some());
    assert!(t.cost("opencode", "claude-sonnet-5").is_none());
    assert!(t.cost("qwen", "qwen-flash").is_some());
    // And the gateway constructor uses that name, so `with_prices` finds it.
    assert_eq!(
        ModelConfig::opencode_zen("claude-sonnet-5").provider,
        "opencode-zen"
    );
    assert!(ModelConfig::opencode_zen("claude-sonnet-5")
        .with_prices(&t)
        .cost
        .is_some());
}

/// A built-in model models.dev describes in a way this crate cannot map is
/// named in a warning — it silently keeps its built-in price otherwise.
#[test]
fn skipping_a_builtin_model_is_named_in_a_warning() {
    let doc = r#"{"anthropic": {"models": {
        "claude-opus-5": {"cost": {"input": 5, "output": 25, "per_request": 0.01}},
        "claude-sonnet-5": {"cost": {"input": 2, "output": 10}}}}}"#;
    let logs = CapturedLogs::default();
    let (table, skipped) = {
        let _log =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(logs.clone()));
        PriceTable::from_models_dev_json_report(doc).unwrap()
    };
    assert_eq!(table.len(), 1);
    assert_eq!(skipped.len(), 1);
    let warning = logs
        .0
        .lock()
        .unwrap()
        .iter()
        .find(|l| l.starts_with("WARN"))
        .cloned()
        .expect("a skipped built-in model must be named");
    assert!(warning.contains("anthropic/claude-opus-5"), "{warning}");
    assert!(warning.contains("per_request"), "{warning}");

    // Positive control: a model the built-in data does not list is skipped
    // quietly (it is still in the report).
    let quiet = CapturedLogs::default();
    let (_, skipped) = {
        let _log =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(quiet.clone()));
        PriceTable::from_models_dev_json_report(
            r#"{"acme": {"models": {"a": {"cost": {"input": 1, "output": 2, "per_request": 1}},
                                   "b": {"cost": {"input": 1, "output": 2}}}}}"#,
        )
        .unwrap()
    };
    assert_eq!(skipped.len(), 1);
    assert!(!quiet
        .0
        .lock()
        .unwrap()
        .iter()
        .any(|l| l.starts_with("WARN")));
}

/// `AddOnly` extends coverage without overriding a built-in price.
#[test]
fn add_only_policy_never_overrides_builtin() {
    let _g = exclusive();
    let fetched = PriceTable::from_json_str(
        r#"{"schema": 1, "providers": {
            "anthropic": {"claude-sonnet-5": {"input": 1.5, "output": 7.5}},
            "deepseek": {"deepseek-flash": {"input": 0.15, "output": 0.6}}}}"#,
    )
    .unwrap();
    let changes = PriceTable::install_fetched_with(fetched.clone(), FetchPolicy::AddOnly);
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].model, "deepseek-flash");
    assert!(changes[0].before.is_none());
    assert_eq!(
        ModelConfig::claude_sonnet_5().cost,
        PriceTable::builtin().cost("anthropic", "claude-sonnet-5")
    );
    assert!(ModelConfig::deepseek("deepseek-flash", "D").cost.is_some());

    // Positive control: the default policy does override.
    let changes = PriceTable::install_fetched_with(fetched, FetchPolicy::default());
    assert_eq!(changes.len(), 2);
    assert_eq!(
        ModelConfig::claude_sonnet_5()
            .cost
            .unwrap()
            .input_per_million,
        1.5
    );
    PriceTable::clear_fetched();
}

/// Captures every event's level, message and fields on this thread.
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

/// user > fetched > built-in, and an explicit `config.cost` over all three.
#[test]
fn precedence_user_over_fetched_over_builtin() {
    let _g = exclusive();
    let builtin_opus = ModelConfig::claude_opus_5().cost;

    let fetched = PriceTable::from_json_str(
        r#"{"schema": 1, "providers": {"anthropic": {
            "claude-sonnet-5": {"input": 1.5, "output": 7.5},
            "claude-haiku-4-5": {"input": 0.5, "output": 2.5}}}}"#,
    )
    .unwrap();
    let logs = CapturedLogs::default();
    let changes = {
        let _log =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(logs.clone()));
        PriceTable::install_fetched(fetched)
    };
    // Disagreements are visible: returned and logged at warn.
    assert_eq!(changes.len(), 2);
    assert!(changes.iter().all(|c| c.before.is_some()));
    let warning = logs
        .0
        .lock()
        .unwrap()
        .iter()
        .find(|l| l.starts_with("WARN"))
        .cloned()
        .expect("a disagreement with the built-in data must be logged");
    assert!(warning.contains("2 model(s)"), "{warning}");
    assert!(
        warning.contains("claude-sonnet-5: input 2 -> 1.5"),
        "{warning}"
    );

    // Fetched beats built-in; unlisted models keep the built-in price.
    assert_eq!(
        ModelConfig::claude_sonnet_5()
            .cost
            .unwrap()
            .input_per_million,
        1.5
    );
    assert_eq!(ModelConfig::claude_opus_5().cost, builtin_opus);

    // User beats fetched, for what it lists.
    PriceTable::install_override(
        PriceTable::from_json_str(
            r#"{"schema": 1, "providers": {"anthropic": {"claude-sonnet-5": {"input": 1.0, "output": 5.0}}}}"#,
        )
        .unwrap(),
    );
    assert_eq!(
        ModelConfig::claude_sonnet_5()
            .cost
            .unwrap()
            .input_per_million,
        1.0
    );
    assert_eq!(
        ModelConfig::claude_haiku_4_5()
            .cost
            .unwrap()
            .input_per_million,
        0.5
    );

    // Explicit beats everything.
    let mut config = ModelConfig::claude_sonnet_5();
    config.cost = Some(CostConfig::new(9.0, 9.0));
    assert_eq!(config.cost.unwrap().input_per_million, 9.0);

    // Clearing the override exposes the fetched layer again; clearing that,
    // the built-in data.
    PriceTable::clear_override();
    assert_eq!(
        ModelConfig::claude_sonnet_5()
            .cost
            .unwrap()
            .input_per_million,
        1.5
    );
    PriceTable::clear_fetched();
    assert_eq!(
        ModelConfig::claude_sonnet_5().cost,
        PriceTable::builtin().cost("anthropic", "claude-sonnet-5")
    );
}

/// Positive control for the log: a fetched table that agrees with the
/// built-in data (as billed) raises no warning.
#[test]
fn an_agreeing_fetch_logs_no_warning() {
    let _g = exclusive();
    let agreeing = PriceTable::from_models_dev_json(MODELS_DEV).unwrap();
    let logs = CapturedLogs::default();
    let changes = {
        let _log =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(logs.clone()));
        PriceTable::install_fetched(agreeing)
    };
    assert!(changes.iter().all(|c| c.before.is_none()));
    assert!(
        !logs.0.lock().unwrap().iter().any(|l| l.starts_with("WARN")),
        "{:?}",
        logs.0.lock().unwrap()
    );
    // The new models it brings are priced now.
    assert!(ModelConfig::deepseek("deepseek-flash", "D").cost.is_some());
    PriceTable::clear_fetched();
    assert!(ModelConfig::deepseek("deepseek-flash", "D").cost.is_none());
}

/// Remote and cached data are parsed leniently: a metadata field a newer
/// yoagent added (without a schema bump) must not break an older client.
#[tokio::test]
async fn remote_and_cached_tables_ignore_unknown_fields() {
    let newer = r#"{"schema": 1, "generated_at": "2027-01-01", "providers": {"anthropic": {
        "claude-sonnet-5": {"input": 1.5, "output": 7.5, "deprecated": false}}}}"#;
    // Positive control: hand-written input rejects exactly this document.
    assert!(matches!(
        PriceTable::from_json_str(newer).unwrap_err(),
        PriceError::NewerFormat { .. }
    ));

    let server = serve("/prices.json", ok(newer)).await;
    let source = PriceSource::Url(format!("{}/prices.json", server.uri()));
    let t = PriceTable::fetch(&source).await.unwrap();
    assert_eq!(
        t.cost("anthropic", "claude-sonnet-5"),
        Some(CostConfig::new(1.5, 7.5))
    );

    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("prices.json");
    std::fs::write(&cache, newer).unwrap();
    let got =
        PriceTable::fetch_cached(&source, &cache, Duration::from_secs(3600), Duration::MAX).await;
    assert_eq!(got.origin, PriceOrigin::Cache);
    assert_eq!(got.table, t);
}

async fn cache_warnings(source: &PriceSource, cache: &std::path::Path) -> usize {
    let logs = CapturedLogs::default();
    let _log = tracing::subscriber::set_default(tracing_subscriber::registry().with(logs.clone()));
    let got = PriceTable::fetch_cached(source, cache, Duration::MAX, Duration::MAX).await;
    assert_eq!(got.origin, PriceOrigin::Builtin);
    let n = logs
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|l| l.contains("ignoring the price cache"))
        .count();
    n
}

/// A cache that exists but cannot be read is logged, not silently skipped
/// (only a missing cache is silent).
#[tokio::test]
async fn an_unreadable_cache_is_logged() {
    let dir = tempfile::tempdir().unwrap();
    let down = serve("/prices.json", ResponseTemplate::new(500)).await;
    let source = PriceSource::Url(format!("{}/prices.json", down.uri()));
    // A directory where the file should be: the read fails, and says so.
    let as_dir = dir.path().join("cache-dir");
    std::fs::create_dir(&as_dir).unwrap();
    assert_eq!(cache_warnings(&source, &as_dir).await, 1);
    // Positive control: a missing cache is not worth a warning.
    assert_eq!(
        cache_warnings(&source, &dir.path().join("missing.json")).await,
        0
    );
}
