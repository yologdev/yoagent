//! Opt-in live price sources: models.dev mapping, our-format URLs, timeouts
//! and the cache helper. Nothing here installs a layer (see
//! `price_override_test` for that).
//!
//! Every test serializes on `LOCK`: some assert on `tracing` output, and a
//! callsite first hit on another thread while a test's scoped subscriber is
//! being set up can miss the event.

use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing_subscriber::layer::SubscriberExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use yoagent::provider::{
    CacheOptions, CacheProblem, CostConfig, FetchOptions, ModelConfig, PriceError, PriceOrigin,
    PriceSource, PriceTable,
};

/// A slice of the real <https://models.dev/api.json> (2026-09-25): flat and
/// tiered entries, the legacy `context_over_200k` mirror, a `reasoning` rate
/// equal to output (DeepSeek), one that differs (skipped), a model without a
/// cost (skipped), and `alibaba` (our `qwen`).
const MODELS_DEV: &str = include_str!("fixtures/models_dev_slice.json");

static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn exclusive() -> tokio::sync::MutexGuard<'static, ()> {
    LOCK.blocking_lock()
}

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    LOCK.lock().await
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

fn url_source(server: &MockServer) -> PriceSource {
    PriceSource::Url(format!("{}/prices.json", server.uri()))
}

const FETCHED: &str = r#"{"schema": 1, "providers": {"anthropic": {"claude-sonnet-5": {"input": 1.5, "output": 7.5}}}}"#;

/// A cache file as `fetch_cached` writes it: the table, and the source URL.
fn write_cache(path: &std::path::Path, source: &PriceSource, table_json: &str) {
    let prices: serde_json::Value = serde_json::from_str(table_json).unwrap();
    let body =
        serde_json::json!({"yoagent_price_cache": 1, "source": source.url(), "prices": prices});
    std::fs::write(path, body.to_string()).unwrap();
}

fn set_mtime(path: &std::path::Path, when: std::time::SystemTime) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(when)
        .unwrap();
}

const DAY: Duration = Duration::from_secs(24 * 3600);

fn opts(max_age: Duration, max_stale: Duration) -> CacheOptions {
    CacheOptions::new()
        .with_max_age(max_age)
        .with_max_stale(max_stale)
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

impl CapturedLogs {
    fn warns(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.starts_with("WARN"))
            .cloned()
            .collect()
    }
}

#[test]
fn models_dev_fixture_maps_into_our_schema() {
    let _g = exclusive();
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
    let _g = exclusive();
    for body in ["[]", "{}", r#"{"anthropic": {"data": {}}}"#, "null"] {
        let e = PriceTable::from_models_dev_json(body).unwrap_err();
        assert!(matches!(e, PriceError::ModelsDev { .. }), "{body}: {e}");
    }
    assert!(matches!(
        PriceTable::from_models_dev_json("{not json").unwrap_err(),
        PriceError::Json { .. }
    ));
}

#[test]
fn the_models_dev_report_says_what_was_skipped_and_why() {
    let _g = exclusive();
    let report = PriceTable::from_models_dev_json_report(MODELS_DEV).unwrap();
    assert_eq!(report.table.len(), 11);
    let reason = |p: &str, m: &str| {
        report
            .skipped
            .iter()
            .find(|s| s.provider == p && s.model == m)
            .map(|s| s.reason.clone())
            .unwrap_or_else(|| panic!("{p}/{m} not reported: {:?}", report.skipped))
    };
    assert!(reason("openrouter", "perplexity/sonar-deep-research").contains("reasoning"));
    assert!(reason("poe", "cerebras/qwen3-32b-cs").contains("no cost"));
    assert_eq!(report.skipped.len(), 2);
    assert!(report.ignored_fields.is_empty());
}

/// models.dev's `opencode` is this crate's `opencode-zen`, as `alibaba` is
/// `qwen`.
#[test]
fn models_dev_provider_keys_are_renamed() {
    let _g = exclusive();
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
    let _g = exclusive();
    let doc = r#"{"anthropic": {"models": {
        "claude-opus-5": {"cost": {"input": 5, "output": 25, "per_request": 0.01}},
        "claude-sonnet-5": {"cost": {"input": 2, "output": 10}}}}}"#;
    let logs = CapturedLogs::default();
    let report = {
        let _log =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(logs.clone()));
        PriceTable::from_models_dev_json_report(doc).unwrap()
    };
    assert_eq!(report.table.len(), 1);
    assert_eq!(report.skipped.len(), 1);
    let warns = logs.warns();
    assert_eq!(warns.len(), 1, "{warns:?}");
    assert!(warns[0].contains("anthropic/claude-opus-5"), "{warns:?}");
    assert!(warns[0].contains("per_request"), "{warns:?}");

    // Positive control: a model the built-in data does not list is skipped
    // quietly (it is still in the report).
    let quiet = CapturedLogs::default();
    let report = {
        let _log =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(quiet.clone()));
        PriceTable::from_models_dev_json_report(
            r#"{"acme": {"models": {"a": {"cost": {"input": 1, "output": 2, "per_request": 1}},
                                   "b": {"cost": {"input": 1, "output": 2}}}}}"#,
        )
        .unwrap()
    };
    assert_eq!(report.skipped.len(), 1);
    assert!(quiet.warns().is_empty());
}

#[tokio::test]
async fn fetches_models_dev_format_from_a_url() {
    let _g = serial().await;
    let server = serve("/api.json", ok(MODELS_DEV)).await;
    let source = PriceSource::ModelsDevAt(format!("{}/api.json", server.uri()));
    let report = PriceTable::fetch_with(&source, FetchOptions::new())
        .await
        .unwrap();
    assert_eq!(report.table, {
        // Same mapping, but entries name the URL they came from.
        let mut expected = PriceTable::new();
        for (p, m, e) in PriceTable::from_models_dev_json(MODELS_DEV).unwrap().iter() {
            let entry =
                yoagent::provider::PriceEntry::new(e.cost.clone()).with_source(source.url());
            expected.insert(p, m, entry).unwrap();
        }
        expected
    });
    assert_eq!(report.skipped.len(), 2);
}

/// Item 2: a `Url` source is usually hand-maintained, so it is strict.
#[tokio::test]
async fn url_sources_are_parsed_strictly() {
    let _g = serial().await;
    let server = serve("/prices.json", ok(FETCHED)).await;
    let t = PriceTable::fetch(&url_source(&server)).await.unwrap();
    assert_eq!(t, PriceTable::from_json_str(FETCHED).unwrap());

    let typo = r#"{"schema": 1, "providers": {"anthropic": {"claude-sonnet-5": {"input": 1.5, "output": 7.5, "cache_reed": 0.15}}}}"#;
    let server = serve("/prices.json", ok(typo)).await;
    let e = PriceTable::fetch(&url_source(&server)).await.unwrap_err();
    assert!(
        matches!(e, PriceError::UnknownField { ref field, .. } if field.ends_with("cache_reed")),
        "{e}"
    );
    // Validated like any other table, and errors name the URL.
    let bad = serve("/prices.json", ok(r#"{"schema": 2, "providers": {}}"#)).await;
    let e = PriceTable::fetch(&url_source(&bad)).await.unwrap_err();
    assert!(matches!(e, PriceError::UnsupportedSchema { .. }), "{e}");
    let junk = serve("/prices.json", ok("<html>")).await;
    let e = PriceTable::fetch(&url_source(&junk)).await.unwrap_err();
    assert!(matches!(e, PriceError::Json { .. }), "{e}");
    assert!(e.to_string().contains(&junk.uri()), "{e}");
}

#[tokio::test]
async fn http_errors_timeouts_and_refused_connections_are_reported() {
    let _g = serial().await;
    let down = serve("/prices.json", ResponseTemplate::new(503)).await;
    let e = PriceTable::fetch(&url_source(&down)).await.unwrap_err();
    assert!(matches!(e, PriceError::Http { status: 503, .. }), "{e}");
    assert!(e.to_string().contains(&down.uri()), "{e}");

    let slow = serve(
        "/prices.json",
        ok(FETCHED).set_delay(Duration::from_secs(5)),
    )
    .await;
    let started = std::time::Instant::now();
    let e = PriceTable::fetch_with(
        &url_source(&slow),
        FetchOptions::new().with_timeout(Duration::from_millis(200)),
    )
    .await
    .unwrap_err();
    assert!(matches!(e, PriceError::Timeout { .. }), "{e}");
    assert!(started.elapsed() < Duration::from_secs(4));

    // A port nobody listens on: connection refused is a Request error, with
    // the transport error kept as its (opaque) source.
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let e = PriceTable::fetch(&PriceSource::Url(format!("http://127.0.0.1:{port}/p.json")))
        .await
        .unwrap_err();
    assert!(matches!(e, PriceError::Request { .. }), "{e}");
    assert!(std::error::Error::source(&e).is_some());
}

#[test]
fn source_urls() {
    let _g = exclusive();
    assert_eq!(PriceSource::ModelsDev.url(), "https://models.dev/api.json");
    assert!(PriceSource::YoagentMain
        .url()
        .starts_with("https://raw.githubusercontent.com/yologdev/yoagent/main/"));
}

/// The file `PriceSource::YoagentMain` serves is this crate's own data file:
/// the path after `/main/` in the URL must exist here and parse.
#[test]
fn the_yoagent_main_url_names_the_checked_in_file() {
    let _g = exclusive();
    let url = PriceSource::YoagentMain.url();
    let (_, rel) = url.split_once("/main/").expect("a /main/ URL");
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    assert!(path.exists(), "{} does not exist", path.display());
    assert_eq!(PriceTable::from_path(&path).unwrap(), PriceTable::builtin());
}

#[tokio::test]
async fn a_fresh_cache_is_used_without_a_request() {
    let _g = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("prices.json");
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ok("{}"))
        .expect(0)
        .mount(&server)
        .await;
    let source = url_source(&server);
    write_cache(&cache, &source, FETCHED);
    set_mtime(
        &cache,
        std::time::SystemTime::now() - Duration::from_secs(60),
    );

    let got = PriceTable::fetch_cached(&source, &cache, opts(DAY, DAY)).await;
    let PriceOrigin::Cache { age, .. } = got.origin else {
        panic!("expected Cache, got {:?}", got.origin);
    };
    assert!(
        age >= Duration::from_secs(60) && age < Duration::from_secs(120),
        "{age:?}"
    );
    assert_eq!(got.table, PriceTable::from_json_str(FETCHED).unwrap());
    assert!(got.cache_problem.is_none());
}

#[tokio::test]
async fn an_expired_cache_is_refetched_and_rewritten() {
    let _g = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("nested/dir/prices.json");
    std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
    let server = serve("/api.json", ok(MODELS_DEV)).await;
    let source = PriceSource::ModelsDevAt(format!("{}/api.json", server.uri()));
    write_cache(&cache, &source, r#"{"schema": 1, "providers": {}}"#);

    let got = PriceTable::fetch_cached(&source, &cache, opts(Duration::ZERO, DAY)).await;
    assert!(
        matches!(
            got.origin,
            PriceOrigin::Fetched {
                cache_write_error: None,
                ..
            }
        ),
        "{:?}",
        got.origin
    );
    assert_eq!(got.table.len(), 11);
    // The models.dev skip list reaches the caller.
    assert_eq!(got.skipped.len(), 2);
    // The cache now holds the mapped table and serves it, fresh.
    let again = PriceTable::fetch_cached(&source, &cache, opts(DAY, DAY)).await;
    assert!(matches!(again.origin, PriceOrigin::Cache { .. }));
    assert_eq!(again.table, got.table);

    // And a missing cache directory is created.
    let fresh = dir.path().join("new/prices.json");
    let got = PriceTable::fetch_cached(&source, &fresh, opts(Duration::ZERO, DAY)).await;
    assert!(matches!(got.origin, PriceOrigin::Fetched { .. }));
    assert!(fresh.exists());
}

#[tokio::test]
async fn offline_falls_back_to_the_stale_cache_then_builtin() {
    let _g = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("prices.json");
    let down = serve("/prices.json", ResponseTemplate::new(500)).await;
    let source = url_source(&down);

    // No cache: built-in, with the fetch error.
    let got = PriceTable::fetch_cached(&source, &cache, opts(Duration::ZERO, Duration::MAX)).await;
    assert!(
        matches!(
            got.origin,
            PriceOrigin::Builtin {
                fetch_error: PriceError::Http { status: 500, .. },
                ..
            }
        ),
        "{:?}",
        got.origin
    );
    assert_eq!(got.table, PriceTable::builtin());
    assert!(!cache.exists(), "a failed fetch must not write a cache");

    // An expired cache beats built-in.
    write_cache(&cache, &source, FETCHED);
    let got = PriceTable::fetch_cached(&source, &cache, opts(Duration::ZERO, Duration::MAX)).await;
    assert!(
        matches!(
            got.origin,
            PriceOrigin::StaleCache {
                age: Some(_),
                fetch_error: PriceError::Http { status: 500, .. },
                ..
            }
        ),
        "{:?}",
        got.origin
    );
    assert_eq!(got.table, PriceTable::from_json_str(FETCHED).unwrap());

    // Too old for `max_stale`: not used, built-in instead.
    set_mtime(&cache, std::time::SystemTime::now() - 10 * DAY);
    let got = PriceTable::fetch_cached(&source, &cache, opts(Duration::ZERO, 7 * DAY)).await;
    assert!(got.origin.is_builtin(), "{:?}", got.origin);
    assert!(got.origin.fetch_error().is_some());
    // Positive control: within the bound, the same file is used.
    let got = PriceTable::fetch_cached(&source, &cache, opts(Duration::ZERO, 14 * DAY)).await;
    let PriceOrigin::StaleCache { age: Some(age), .. } = got.origin else {
        panic!("expected StaleCache, got {:?}", got.origin);
    };
    assert!(age >= 10 * DAY && age < 11 * DAY, "{age:?}");

    // A corrupt cache is reported, not fatal.
    std::fs::write(&cache, "not json").unwrap();
    let got = PriceTable::fetch_cached(&source, &cache, opts(DAY, Duration::MAX)).await;
    assert!(got.origin.is_builtin());
    assert!(
        matches!(got.cache_problem, Some(CacheProblem::Invalid { .. })),
        "{:?}",
        got.cache_problem
    );
}

/// A fetch that times out falls back like any other failure.
#[tokio::test]
async fn a_timed_out_fetch_falls_back_to_the_cache() {
    let _g = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("prices.json");
    let slow = serve(
        "/prices.json",
        ok(FETCHED).set_delay(Duration::from_secs(5)),
    )
    .await;
    let source = url_source(&slow);
    write_cache(&cache, &source, FETCHED);
    let got = PriceTable::fetch_cached(
        &source,
        &cache,
        opts(Duration::ZERO, DAY).with_timeout(Duration::from_millis(200)),
    )
    .await;
    assert!(
        matches!(
            got.origin,
            PriceOrigin::StaleCache {
                fetch_error: PriceError::Timeout { .. },
                ..
            }
        ),
        "{:?}",
        got.origin
    );
}

/// A modification time in the future is an unknown age: never fresh, and a
/// stale fallback only when `max_stale` is unbounded.
#[tokio::test]
async fn a_future_mtime_is_treated_as_expired() {
    let _g = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("prices.json");
    let year = 365 * DAY;
    let future = std::time::SystemTime::now() + year;

    // With a year of max_age, a "fresh" reading would skip the request; it
    // must be refetched instead.
    let server = serve("/prices.json", ok(FETCHED)).await;
    let source = url_source(&server);
    write_cache(&cache, &source, FETCHED);
    set_mtime(&cache, future);
    let got = PriceTable::fetch_cached(&source, &cache, opts(year, year)).await;
    assert!(matches!(got.origin, PriceOrigin::Fetched { .. }));

    let down = serve("/prices.json", ResponseTemplate::new(500)).await;
    let source = url_source(&down);
    write_cache(&cache, &source, FETCHED);
    set_mtime(&cache, future);
    let got = PriceTable::fetch_cached(&source, &cache, opts(year, year)).await;
    assert!(got.origin.is_builtin());
    let got = PriceTable::fetch_cached(&source, &cache, opts(year, Duration::MAX)).await;
    assert!(
        matches!(got.origin, PriceOrigin::StaleCache { age: None, .. }),
        "{:?}",
        got.origin
    );
}

/// Item 5: a cache of a different source is a miss, reported and logged.
#[tokio::test]
async fn a_cache_of_another_source_is_a_miss() {
    let _g = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("prices.json");
    let server = serve("/prices.json", ok(FETCHED)).await;
    let source = url_source(&server);
    let other = PriceSource::Url("https://example.invalid/other.json".into());
    write_cache(
        &cache,
        &other,
        r#"{"schema": 1, "providers": {"anthropic": {"claude-sonnet-5": {"input": 9, "output": 9}}}}"#,
    );
    let logs = CapturedLogs::default();
    let got = {
        let _log =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(logs.clone()));
        PriceTable::fetch_cached(&source, &cache, opts(DAY, DAY)).await
    };
    // Not served from the cache, even though it is fresh.
    assert!(
        matches!(got.origin, PriceOrigin::Fetched { .. }),
        "{:?}",
        got.origin
    );
    assert_eq!(got.table, PriceTable::from_json_str(FETCHED).unwrap());
    match &got.cache_problem {
        Some(CacheProblem::SourceMismatch {
            cached, expected, ..
        }) => {
            assert_eq!(cached.as_deref(), Some(other.url()));
            assert_eq!(expected, source.url());
        }
        other => panic!("expected SourceMismatch, got {other:?}"),
    }
    assert!(logs
        .warns()
        .iter()
        .any(|w| w.contains("ignoring the price cache")));
    // Positive control: the rewritten cache (now of this source) is a hit.
    let again = PriceTable::fetch_cached(&source, &cache, opts(DAY, DAY)).await;
    assert!(matches!(again.origin, PriceOrigin::Cache { .. }));
    assert!(again.cache_problem.is_none());
}

/// A failed cache write does not lose the fetched table, and is reported.
#[tokio::test]
async fn a_failed_cache_write_is_reported() {
    let _g = serial().await;
    let dir = tempfile::tempdir().unwrap();
    // The cache's parent is a file, so the directory cannot be created.
    let blocker = dir.path().join("not-a-dir");
    std::fs::write(&blocker, "x").unwrap();
    let cache = blocker.join("prices.json");
    let server = serve("/prices.json", ok(FETCHED)).await;
    let source = url_source(&server);
    let got = PriceTable::fetch_cached(&source, &cache, opts(Duration::ZERO, DAY)).await;
    assert_eq!(got.table, PriceTable::from_json_str(FETCHED).unwrap());
    assert!(
        matches!(
            got.origin,
            PriceOrigin::Fetched {
                cache_write_error: Some(PriceError::Io { .. }),
                ..
            }
        ),
        "{:?}",
        got.origin
    );
    // Positive control: a writable path reports no error.
    let fine = dir.path().join("ok/prices.json");
    let got = PriceTable::fetch_cached(&source, &fine, opts(Duration::ZERO, DAY)).await;
    assert!(
        matches!(
            got.origin,
            PriceOrigin::Fetched {
                cache_write_error: None,
                ..
            }
        ),
        "{:?}",
        got.origin
    );
}

/// The cache is parsed leniently (it may have been written by a newer
/// release), and the fields it ignored are returned.
#[tokio::test]
async fn a_cache_is_parsed_leniently_and_reports_ignored_fields() {
    let _g = serial().await;
    let newer = r#"{"schema": 1, "generated_at": "2027-01-01", "providers": {"anthropic": {
        "claude-sonnet-5": {"input": 1.5, "output": 7.5, "deprecated": false}}}}"#;
    // Positive control: hand-written input rejects exactly this document.
    assert!(matches!(
        PriceTable::from_json_str(newer).unwrap_err(),
        PriceError::UnknownField { .. }
    ));
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("prices.json");
    let source = PriceSource::Url("https://example.invalid/prices.json".into());
    write_cache(&cache, &source, newer);
    let got = PriceTable::fetch_cached(&source, &cache, opts(DAY, DAY)).await;
    assert!(
        matches!(got.origin, PriceOrigin::Cache { .. }),
        "{:?}",
        got.origin
    );
    assert_eq!(
        got.table.cost("anthropic", "claude-sonnet-5"),
        Some(CostConfig::new(1.5, 7.5))
    );
    let mut ignored = got.ignored_fields.clone();
    ignored.sort();
    assert_eq!(
        ignored,
        [
            "generated_at",
            "providers.anthropic.claude-sonnet-5.deprecated"
        ]
    );
}

/// A cache that exists but cannot be read is reported and logged, not
/// silently skipped (only a missing cache is silent).
#[tokio::test]
async fn an_unreadable_cache_is_reported() {
    let _g = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let down = serve("/prices.json", ResponseTemplate::new(500)).await;
    let source = url_source(&down);
    let run = |cache: std::path::PathBuf| {
        let source = source.clone();
        async move {
            let logs = CapturedLogs::default();
            let _log =
                tracing::subscriber::set_default(tracing_subscriber::registry().with(logs.clone()));
            let got = PriceTable::fetch_cached(&source, &cache, opts(DAY, DAY)).await;
            assert!(got.origin.is_builtin());
            let logged = logs
                .warns()
                .iter()
                .filter(|l| l.contains("ignoring the price cache"))
                .count();
            (got.cache_problem, logged)
        }
    };
    // A directory where the file should be: the read fails, and says so.
    let as_dir = dir.path().join("cache-dir");
    std::fs::create_dir(&as_dir).unwrap();
    let (problem, logged) = run(as_dir).await;
    assert!(
        matches!(problem, Some(CacheProblem::Unreadable { .. })),
        "{problem:?}"
    );
    assert_eq!(logged, 1);
    // Positive control: a missing cache is not worth a warning.
    let (problem, logged) = run(dir.path().join("missing.json")).await;
    assert!(problem.is_none());
    assert_eq!(logged, 0);
}
