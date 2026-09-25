//! Opt-in live price sources: models.dev, this crate's own checked file on
//! GitHub, or any URL serving the `prices.json` format.
//!
//! Nothing here runs unless the caller asks, and nothing here installs a
//! table: pass the result to [`global::install_fetched`](super::global::install_fetched)
//! (process-wide, above the built-in data and below any user override) or
//! [`ModelConfig::with_prices`](crate::provider::ModelConfig::with_prices)
//! (one config).

use super::{builtin_ref, PriceEntry, PriceError, PriceTable, Strictness};
use crate::provider::model::{ContextTier, CostConfig};
use serde_json::{Map, Value};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// How long a fetch waits for a source before giving up, unless
/// [`FetchOptions`] or [`CacheOptions`] say otherwise.
pub const DEFAULT_FETCH_TIMEOUT: Duration = Duration::from_secs(10);

const MODELS_DEV_URL: &str = "https://models.dev/api.json";
const YOAGENT_MAIN_URL: &str =
    "https://raw.githubusercontent.com/yologdev/yoagent/main/src/provider/prices.json";

/// Marks a file written by [`PriceTable::fetch_cached`], and its version.
const CACHE_MARKER: &str = "yoagent_price_cache";

/// Where [`PriceTable::fetch`] gets prices from.
///
/// # Trust
///
/// A table installed with the default policy overrides the built-in data for
/// every model it lists, so pick a source you trust for the models you use:
///
/// - [`YoagentMain`](Self::YoagentMain) is this crate's own `prices.json` on
///   the `main` branch: the same format, the same review and the same price
///   audit as a release, without waiting for one.
/// - [`ModelsDev`](Self::ModelsDev) is [models.dev](https://models.dev), a
///   community-maintained database covering far more models. It is **not
///   authoritative** and has been provably wrong before (Claude context-tier
///   data, DeepSeek V4 Pro's price). Installing it logs the built-in models
///   where it disagrees (the first few, and a count) and returns them all;
///   the fetched rates win unless you install with
///   [`InstallPolicy::AddOnly`](super::global::InstallPolicy::AddOnly).
/// - [`Url`](Self::Url) is yours: typically a hand-maintained file, so it is
///   parsed **strictly** — an unknown field is [`PriceError::UnknownField`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PriceSource {
    /// <https://models.dev/api.json>, mapped into this crate's format.
    ModelsDev,
    /// A models.dev-format document at another URL (a mirror, a pinned
    /// snapshot, a test server).
    ModelsDevAt(String),
    /// This crate's checked `src/provider/prices.json` on GitHub `main`, so
    /// a price fix merged to `main` reaches you without a release. Parsed
    /// leniently: a metadata field a newer release added is ignored, logged
    /// and returned in [`FetchReport::ignored_fields`]. If `main` moves to a
    /// newer schema — a field that changes billing was added — the fetch
    /// fails with [`PriceError::UnsupportedSchema`] and nothing changes.
    YoagentMain,
    /// Any URL serving this crate's `prices.json` format, parsed strictly.
    Url(String),
}

impl PriceSource {
    /// The URL this source is fetched from.
    pub fn url(&self) -> &str {
        match self {
            Self::ModelsDev => MODELS_DEV_URL,
            Self::ModelsDevAt(url) | Self::Url(url) => url,
            Self::YoagentMain => YOAGENT_MAIN_URL,
        }
    }
}

/// Options for [`PriceTable::fetch_with`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct FetchOptions {
    /// How long the whole request, body included, may take. Default
    /// [`DEFAULT_FETCH_TIMEOUT`].
    pub timeout: Duration,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_FETCH_TIMEOUT,
        }
    }
}

impl FetchOptions {
    /// The defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// A fetched table and what the fetch left out.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct FetchReport {
    pub table: PriceTable,
    /// Models a models.dev source listed but could not be mapped, and why
    /// (always empty for this crate's format).
    pub skipped: Vec<SkippedModel>,
    /// Dotted paths of fields a lenient parse ignored
    /// ([`PriceSource::YoagentMain`], or a cache file); always empty for
    /// strict and models.dev sources.
    pub ignored_fields: Vec<String>,
}

/// A model a models.dev document listed but [`PriceTable`] could not
/// express, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SkippedModel {
    /// This crate's provider name (after the renames `alibaba` → `qwen` and
    /// `opencode` → `opencode-zen`).
    pub provider: String,
    pub model: String,
    pub reason: String,
}

/// Options for [`PriceTable::fetch_cached`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct CacheOptions {
    /// A cache younger than this is used without a request. Default: one
    /// day.
    pub max_age: Duration,
    /// When the fetch fails, an expired cache no older than this is used
    /// instead of the built-in data. Default: seven days. `Duration::MAX`
    /// accepts a cache of any age, including one whose age is unknown.
    pub max_stale: Duration,
    /// The fetch timeout. Default [`DEFAULT_FETCH_TIMEOUT`].
    pub timeout: Duration,
}

impl Default for CacheOptions {
    fn default() -> Self {
        Self {
            max_age: Duration::from_secs(24 * 3600),
            max_stale: Duration::from_secs(7 * 24 * 3600),
            timeout: DEFAULT_FETCH_TIMEOUT,
        }
    }
}

impl CacheOptions {
    /// The defaults: refetch after a day, fall back to a cache at most a
    /// week old, time out after [`DEFAULT_FETCH_TIMEOUT`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Set [`max_age`](Self::max_age).
    pub fn with_max_age(mut self, max_age: Duration) -> Self {
        self.max_age = max_age;
        self
    }

    /// Set [`max_stale`](Self::max_stale).
    pub fn with_max_stale(mut self, max_stale: Duration) -> Self {
        self.max_stale = max_stale;
        self
    }

    /// Set [`timeout`](Self::timeout).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// Where the table from [`PriceTable::fetch_cached`] came from — with the
/// data that only makes sense for that origin.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum PriceOrigin {
    /// Fetched just now. The cache was rewritten, unless
    /// `cache_write_error` says otherwise (the table is still good).
    #[non_exhaustive]
    Fetched {
        cache_write_error: Option<PriceError>,
    },
    /// The cache, younger than `max_age`. No request was made.
    #[non_exhaustive]
    Cache { age: Duration },
    /// The fetch failed, so an expired cache no older than `max_stale` was
    /// used. `age` is `None` when it is unknown (a modification time in the
    /// future, or none on this platform), which only `max_stale ==
    /// Duration::MAX` accepts.
    #[non_exhaustive]
    StaleCache {
        age: Option<Duration>,
        fetch_error: PriceError,
    },
    /// The fetch failed and there was no usable cache: the built-in data.
    #[non_exhaustive]
    Builtin { fetch_error: PriceError },
}

impl PriceOrigin {
    /// Whether this is [`PriceOrigin::Builtin`]: installing the table would
    /// change nothing.
    pub fn is_builtin(&self) -> bool {
        matches!(self, Self::Builtin { .. })
    }

    /// The fetch error behind a [`StaleCache`](Self::StaleCache) or
    /// [`Builtin`](Self::Builtin) fallback.
    pub fn fetch_error(&self) -> Option<&PriceError> {
        match self {
            Self::StaleCache { fetch_error, .. } | Self::Builtin { fetch_error } => {
                Some(fetch_error)
            }
            _ => None,
        }
    }
}

/// Why [`PriceTable::fetch_cached`] could not use the cache file it found.
/// Logged at `warn` too; the file is treated as absent.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum CacheProblem {
    /// The file exists but could not be read.
    #[non_exhaustive]
    Unreadable { error: PriceError },
    /// The file is not a valid price cache.
    #[non_exhaustive]
    Invalid { error: PriceError },
    /// The file caches a different source (`cached` is its URL, or `None`
    /// if it does not say). Use one cache path per source.
    #[non_exhaustive]
    SourceMismatch {
        cached: Option<String>,
        expected: String,
    },
}

/// The result of [`PriceTable::fetch_cached`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct CachedPrices {
    /// The table to use. Built-in data when
    /// [`origin`](Self::origin) is [`PriceOrigin::Builtin`].
    pub table: PriceTable,
    pub origin: PriceOrigin,
    /// Models the fetch skipped (models.dev sources, [`PriceOrigin::Fetched`]
    /// only; a cache holds the mapped table).
    pub skipped: Vec<SkippedModel>,
    /// Fields a lenient parse ignored — in the fetched document, or in the
    /// cache file used.
    pub ignored_fields: Vec<String>,
    /// A cache file that existed but could not be used.
    pub cache_problem: Option<CacheProblem>,
}

impl PriceTable {
    /// Fetch a table from `source` with the default [`FetchOptions`].
    ///
    /// Never called implicitly, and not installed: see the
    /// [`global`](super::global) module. Mind the trust caveats on
    /// [`PriceSource`].
    pub async fn fetch(source: &PriceSource) -> Result<PriceTable, PriceError> {
        Self::fetch_with(source, FetchOptions::default())
            .await
            .map(|report| report.table)
    }

    /// Fetch a table from `source`, also reporting what the fetch left out
    /// (models.dev models it could not map, fields a lenient parse ignored).
    pub async fn fetch_with(
        source: &PriceSource,
        options: FetchOptions,
    ) -> Result<FetchReport, PriceError> {
        let url = source.url().to_string();
        let timeout = options.timeout;
        let request_error = |e: reqwest::Error| {
            if e.is_timeout() {
                PriceError::Timeout {
                    url: url.clone(),
                    timeout,
                }
            } else {
                PriceError::Request {
                    url: url.clone(),
                    source: Arc::new(e),
                }
            }
        };
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(request_error)?;
        let resp = client.get(&url).send().await.map_err(request_error)?;
        let status = resp.status();
        if !status.is_success() {
            return Err(PriceError::Http {
                url,
                status: status.as_u16(),
            });
        }
        let body = resp.text().await.map_err(request_error)?;
        match source {
            PriceSource::ModelsDev | PriceSource::ModelsDevAt(_) => {
                let value: Value =
                    serde_json::from_str(&body).map_err(|e| PriceError::json(Some(&url), e))?;
                Self::from_models_dev_value(&value, &url)
            }
            PriceSource::YoagentMain | PriceSource::Url(_) => {
                let strictness = if matches!(source, PriceSource::Url(_)) {
                    Strictness::Strict
                } else {
                    Strictness::Lenient
                };
                let (table, ignored_fields) = Self::parse(&body, strictness, Some(&url))?;
                Ok(FetchReport {
                    table,
                    skipped: Vec::new(),
                    ignored_fields,
                })
            }
        }
    }

    /// Map a models.dev `api.json` document into a table.
    ///
    /// Entries are keyed by models.dev's provider key, with two renames to
    /// this crate's provider names: `alibaba` → `qwen` and `opencode` →
    /// `opencode-zen`. `cost.input` / `output` / `cache_read` /
    /// `cache_write` map directly; context tiers come from the `tiers` array
    /// (`{"tier": {"type": "context", "size": N}, ...rates}`), or, when that
    /// is absent, from the older `context_over_200k` object as one tier above
    /// 200,000 prompt tokens.
    ///
    /// A model is **skipped**, not approximated, when its cost carries
    /// structure a [`CostConfig`] cannot express: an unknown key, a
    /// non-zero `reasoning` rate different from `output`, a non-context
    /// tier, or rates that fail validation. Audio rates (`input_audio`,
    /// `output_audio`) are ignored — this crate sends text. A skipped model
    /// that the built-in data lists is named in a `warn` log. A document from
    /// which no model maps is an error, so a changed envelope cannot install
    /// an empty table.
    pub fn from_models_dev_json(json: &str) -> Result<PriceTable, PriceError> {
        Self::from_models_dev_json_report(json).map(|report| report.table)
    }

    /// [`from_models_dev_json`](Self::from_models_dev_json), also reporting
    /// every model it skipped and why (including models models.dev lists
    /// with no cost at all).
    pub fn from_models_dev_json_report(json: &str) -> Result<FetchReport, PriceError> {
        let value: Value = serde_json::from_str(json).map_err(|e| PriceError::json(None, e))?;
        Self::from_models_dev_value(&value, MODELS_DEV_URL)
    }

    fn from_models_dev_value(db: &Value, source: &str) -> Result<FetchReport, PriceError> {
        let models_dev_error = |reason: String| PriceError::ModelsDev {
            url: source.to_string(),
            reason,
        };
        let providers = db
            .as_object()
            .ok_or_else(|| models_dev_error("the document is not a JSON object".into()))?;
        let mut table = PriceTable::new();
        let mut skipped = Vec::new();
        for (provider_key, provider) in providers {
            let Some(models) = provider.get("models").and_then(Value::as_object) else {
                continue;
            };
            let provider = models_dev_provider(provider_key);
            for (model, info) in models {
                let mut skip = |reason: String| {
                    skipped.push(SkippedModel {
                        provider: provider.to_string(),
                        model: model.clone(),
                        reason,
                    })
                };
                let Some(cost) = info.get("cost") else {
                    skip("models.dev lists no cost".into());
                    continue;
                };
                let cost = match map_models_dev_cost(cost) {
                    Ok(cost) => cost,
                    Err(reason) => {
                        skip(reason);
                        continue;
                    }
                };
                let entry = PriceEntry::new(cost).with_source(source);
                if let Err(e) = table.insert(provider, model.as_str(), entry) {
                    skip(e.to_string());
                }
            }
        }
        let builtin = builtin_ref();
        let dropped: Vec<String> = skipped
            .iter()
            .filter(|s| builtin.entry(&s.provider, &s.model).is_some())
            .map(|s| format!("{}/{} ({})", s.provider, s.model, s.reason))
            .collect();
        if !dropped.is_empty() {
            tracing::warn!(
                "yoagent prices: models.dev data for built-in model(s) could not be mapped \
                 and was skipped; they keep their built-in price: {}",
                dropped.join("; ")
            );
        }
        tracing::debug!(
            mapped = table.len(),
            skipped = skipped.len(),
            "yoagent prices: mapped models.dev data"
        );
        if table.is_empty() {
            return Err(models_dev_error(format!(
                "no model carried a usable price ({} providers, {} models skipped); \
                 the schema may have changed",
                providers.len(),
                skipped.len()
            )));
        }
        Ok(FetchReport {
            table,
            skipped,
            ignored_fields: Vec::new(),
        })
    }

    /// Fetch `source` through a cache file at `cache_path`, never failing.
    ///
    /// 1. A cache of this source younger than `max_age` (by modification
    ///    time) is used without a request — [`PriceOrigin::Cache`].
    /// 2. Otherwise `source` is fetched and the cache rewritten (in this
    ///    crate's format, recording the source URL) — [`PriceOrigin::Fetched`].
    ///    A failed cache write is logged and reported in the origin; the
    ///    fetched table is still returned.
    /// 3. If the fetch fails, an expired cache no older than `max_stale` is
    ///    used — [`PriceOrigin::StaleCache`] — and failing that the built-in
    ///    data — [`PriceOrigin::Builtin`]. The fetch error is logged and in
    ///    the origin.
    ///
    /// A cache whose age is unknown (modification time in the future, or
    /// none on this platform) is never fresh, and a stale fallback only when
    /// `max_stale` is `Duration::MAX`. A cache file that exists but is
    /// unreadable, invalid, or caches a different source is logged, ignored
    /// and reported in [`CachedPrices::cache_problem`]; only a missing file
    /// is silent. The cache is parsed leniently.
    ///
    /// The result is not installed — decide from it, and install **before**
    /// building configs (or [`reprice`](crate::provider::ModelConfig::reprice)
    /// the ones you hold):
    ///
    /// ```no_run
    /// # use std::time::Duration;
    /// # use yoagent::provider::{CacheOptions, PriceSource, PriceTable};
    /// # use yoagent::provider::prices::global;
    /// # async fn run() {
    /// let prices = PriceTable::fetch_cached(
    ///     &PriceSource::YoagentMain,
    ///     "/var/cache/myapp/yoagent-prices.json",
    ///     CacheOptions::new(), // refetch daily; offline, accept a week-old cache
    /// )
    /// .await;
    /// if let Some(e) = prices.origin.fetch_error() {
    ///     eprintln!("price refresh failed ({e}); using {:?}", prices.origin);
    /// }
    /// if !prices.origin.is_builtin() {
    ///     let changes = global::install_fetched(prices.table);
    ///     eprintln!("{} model prices changed", changes.len());
    /// }
    /// // ...now build configs.
    /// # }
    /// ```
    pub async fn fetch_cached(
        source: &PriceSource,
        cache_path: impl AsRef<Path>,
        options: CacheOptions,
    ) -> CachedPrices {
        let path = cache_path.as_ref();
        let url = source.url();
        let (cached, cache_problem) = match read_cache(path, url).await {
            CacheRead::Missing => (None, None),
            CacheRead::Problem(problem) => (None, Some(problem)),
            CacheRead::Found(cached) => (Some(cached), None),
        };
        if let Some(Cached {
            table,
            age: Some(age),
            ignored,
        }) = &cached
        {
            if *age < options.max_age {
                return CachedPrices {
                    table: table.clone(),
                    origin: PriceOrigin::Cache { age: *age },
                    skipped: Vec::new(),
                    ignored_fields: ignored.clone(),
                    cache_problem,
                };
            }
        }
        let fetched =
            Self::fetch_with(source, FetchOptions::new().with_timeout(options.timeout)).await;
        match fetched {
            Ok(report) => {
                let cache_write_error = write_cache(path, url, &report.table).await.err();
                CachedPrices {
                    table: report.table,
                    origin: PriceOrigin::Fetched { cache_write_error },
                    skipped: report.skipped,
                    ignored_fields: report.ignored_fields,
                    cache_problem,
                }
            }
            Err(fetch_error) => {
                let usable = cached.filter(|c| match c.age {
                    Some(age) => age <= options.max_stale,
                    None => options.max_stale == Duration::MAX,
                });
                tracing::warn!(
                    url,
                    error = %fetch_error,
                    "yoagent prices: fetch failed; falling back to {}",
                    if usable.is_some() { "the expired cache" } else { "built-in prices" }
                );
                match usable {
                    Some(c) => CachedPrices {
                        table: c.table,
                        origin: PriceOrigin::StaleCache {
                            age: c.age,
                            fetch_error,
                        },
                        skipped: Vec::new(),
                        ignored_fields: c.ignored,
                        cache_problem,
                    },
                    None => CachedPrices {
                        table: PriceTable::builtin(),
                        origin: PriceOrigin::Builtin { fetch_error },
                        skipped: Vec::new(),
                        ignored_fields: Vec::new(),
                        cache_problem,
                    },
                }
            }
        }
    }
}

/// This crate's provider name for a models.dev provider key.
fn models_dev_provider(key: &str) -> &str {
    match key {
        "alibaba" => "qwen",
        "opencode" => "opencode-zen",
        other => other,
    }
}

struct Cached {
    table: PriceTable,
    /// `None` when unknown.
    age: Option<Duration>,
    ignored: Vec<String>,
}

enum CacheRead {
    Missing,
    Problem(CacheProblem),
    Found(Cached),
}

/// Read the cache at `path`, which must cache `url`. Every problem but a
/// missing file is logged.
async fn read_cache(path: &Path, url: &str) -> CacheRead {
    let problem = |p: CacheProblem| {
        tracing::warn!(
            path = %path.display(),
            problem = ?p,
            "yoagent prices: ignoring the price cache"
        );
        CacheRead::Problem(p)
    };
    let io_error = |e: std::io::Error| PriceError::Io {
        path: path.to_path_buf(),
        source: Arc::new(e),
    };
    let meta = match tokio::fs::metadata(path).await {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return CacheRead::Missing,
        Err(e) => {
            return problem(CacheProblem::Unreadable { error: io_error(e) });
        }
    };
    // A modification time in the future (clock skew, a copied file) or none
    // at all: the age is unknown, so the cache is treated as expired.
    let age = meta
        .modified()
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok());
    let text = match tokio::fs::read_to_string(path).await {
        Ok(text) => text,
        Err(e) => return problem(CacheProblem::Unreadable { error: io_error(e) }),
    };
    let origin = path.display().to_string();
    let invalid = |error: PriceError| problem(CacheProblem::Invalid { error });
    let mut value: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => return invalid(PriceError::json(Some(&origin), e)),
    };
    if value.get(CACHE_MARKER).and_then(Value::as_u64) != Some(1) {
        return invalid(PriceError::Json {
            origin: Some(origin),
            message: format!("not a yoagent price cache (no `{CACHE_MARKER}: 1`)"),
        });
    }
    let cached_url = value.get("source").and_then(Value::as_str);
    if cached_url != Some(url) {
        return problem(CacheProblem::SourceMismatch {
            cached: cached_url.map(str::to_string),
            expected: url.to_string(),
        });
    }
    let prices = value
        .as_object_mut()
        .and_then(|o| o.remove("prices"))
        .unwrap_or(Value::Null);
    match PriceTable::parse_value(prices, Strictness::Lenient, Some(&origin)) {
        Ok((table, ignored)) => CacheRead::Found(Cached {
            table,
            age,
            ignored,
        }),
        Err(e) => invalid(e),
    }
}

/// Write the cache through a temporary file and a rename, so a concurrent
/// reader never sees half a file. A failure is logged and returned.
async fn write_cache(path: &Path, url: &str, table: &PriceTable) -> Result<(), PriceError> {
    let body = serde_json::json!({
        CACHE_MARKER: 1,
        "source": url,
        "prices": table.to_wire(),
    });
    let result = async {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            tokio::fs::create_dir_all(dir).await?;
        }
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        tokio::fs::write(&tmp, body.to_string()).await?;
        tokio::fs::rename(&tmp, path).await
    }
    .await;
    result.map_err(|source| {
        tracing::warn!(
            path = %path.display(),
            error = %source,
            "yoagent prices: could not write the price cache"
        );
        PriceError::Io {
            path: path.to_path_buf(),
            source: Arc::new(source),
        }
    })
}

/// One models.dev `cost` object as a [`CostConfig`], or why it cannot be
/// expressed as one.
fn map_models_dev_cost(cost: &Value) -> Result<CostConfig, String> {
    let obj = cost.as_object().ok_or("cost is not an object")?;
    let (input, output, cache_read, cache_write) = map_band(obj, &["tiers", "context_over_200k"])?;
    let mut config = CostConfig::new(input, output)
        .with_cache_read(cache_read)
        .with_cache_write(cache_write);
    let tier = |above: u64, (i, o, r, w): (f64, f64, f64, f64)| {
        ContextTier::new(above, i, o)
            .with_cache_read(r)
            .with_cache_write(w)
    };
    if let Some(tiers) = obj.get("tiers") {
        for t in tiers.as_array().ok_or("`tiers` is not an array")? {
            let t = t.as_object().ok_or("a tier is not an object")?;
            let spec = t
                .get("tier")
                .and_then(Value::as_object)
                .ok_or("a tier has no `tier` spec")?;
            let kind = spec.get("type").and_then(Value::as_str);
            if kind != Some("context") {
                return Err(format!("unsupported tier type {kind:?}"));
            }
            if let Some(k) = spec.keys().find(|k| *k != "type" && *k != "size") {
                return Err(format!("unknown tier spec key `{k}`"));
            }
            let size = spec
                .get("size")
                .and_then(Value::as_f64)
                .ok_or("a tier has no numeric size")?;
            if size < 1.0 || size.fract() != 0.0 || size > u64::MAX as f64 {
                return Err(format!("tier size {size} is not a positive integer"));
            }
            // Pushed in document order; validation rejects an unsorted list.
            config
                .context_tiers
                .push(tier(size as u64, map_band(t, &["tier"])?));
        }
    } else if let Some(over) = obj.get("context_over_200k") {
        let over = over
            .as_object()
            .ok_or("`context_over_200k` is not an object")?;
        config
            .context_tiers
            .push(tier(200_000, map_band(over, &[])?));
    }
    Ok(config)
}

/// One band's `(input, output, cache_read, cache_write)`. `structural` names
/// the keys the caller handles itself.
fn map_band(obj: &Map<String, Value>, structural: &[&str]) -> Result<(f64, f64, f64, f64), String> {
    for key in obj.keys() {
        match key.as_str() {
            "input" | "output" | "cache_read" | "cache_write" | "reasoning" => {}
            // Audio is priced separately and this crate sends text.
            "input_audio" | "output_audio" => {}
            k if structural.contains(&k) => {}
            other => return Err(format!("unknown cost key `{other}`")),
        }
    }
    let rate = |key: &str| -> Result<f64, String> {
        match obj.get(key) {
            None | Some(Value::Null) => Err(format!("no `{key}` rate")),
            Some(v) => v
                .as_f64()
                .ok_or_else(|| format!("`{key}` is not a number: {v}")),
        }
    };
    let optional = |key: &str| -> Result<f64, String> {
        match obj.get(key) {
            None | Some(Value::Null) => Ok(0.0),
            Some(_) => rate(key),
        }
    };
    let (input, output) = (rate("input")?, rate("output")?);
    let (cache_read, cache_write) = (optional("cache_read")?, optional("cache_write")?);
    // Reasoning tokens bill as output here, so a separate reasoning rate is
    // expressible only when it is the output rate, or `0` (not separately
    // priced — models.dev's encoding for reasoning included in output).
    let reasoning = optional("reasoning")?;
    if reasoning != output && reasoning != 0.0 {
        return Err(format!(
            "a separate reasoning rate ({reasoning}) differs from output ({output})"
        ));
    }
    Ok((input, output, cache_read, cache_write))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(cost: serde_json::Value) -> Option<CostConfig> {
        map_models_dev_cost(&cost).ok()
    }

    #[test]
    fn maps_flat_and_tiered_costs() {
        let flat =
            map(serde_json::json!({"input": 4, "output": 20, "cache_read": 0.2, "cache_write": 5}));
        assert_eq!(
            flat,
            Some(
                CostConfig::new(4.0, 20.0)
                    .with_cache_read(0.2)
                    .with_cache_write(5.0)
            )
        );
        let tiered = map(serde_json::json!({
            "input": 5, "output": 30, "cache_read": 0.5,
            "tiers": [{"input": 10, "output": 45, "cache_read": 1,
                       "tier": {"type": "context", "size": 272000}}],
            "context_over_200k": {"input": 10, "output": 45, "cache_read": 1}
        }))
        .unwrap();
        assert_eq!(tiered.context_tiers.len(), 1);
        assert_eq!(tiered.context_tiers[0].above_prompt_tokens, 272_000);
        assert_eq!(tiered.context_tiers[0].cache_read_per_million, 1.0);
        // The legacy encoding alone is one tier above 200K.
        let legacy = map(serde_json::json!({
            "input": 1.25, "output": 10,
            "context_over_200k": {"input": 2.5, "output": 15}
        }))
        .unwrap();
        assert_eq!(legacy.context_tiers[0].above_prompt_tokens, 200_000);
        assert_eq!(legacy.context_tiers[0].input_per_million, 2.5);
    }

    #[test]
    fn skips_what_it_cannot_express() {
        // Reasoning equal to output is fine; different is not.
        assert!(map(serde_json::json!({"input": 1, "output": 2, "reasoning": 2})).is_some());
        assert!(map(serde_json::json!({"input": 1, "output": 2, "reasoning": 3})).is_none());
        assert!(map(serde_json::json!({"input": 1, "output": 2, "per_request": 0.01})).is_none());
        assert!(map(serde_json::json!({"input": 1})).is_none());
        assert!(map(serde_json::json!({"input": "1", "output": 2})).is_none());
        let other_tier = serde_json::json!({"input": 1, "output": 2,
            "tiers": [{"input": 2, "output": 3, "tier": {"type": "time", "size": 5}}]});
        assert!(map(other_tier).is_none());
        // Audio rates are ignored.
        assert!(map(serde_json::json!({"input": 1, "output": 2, "input_audio": 9})).is_some());
    }

    #[test]
    fn effective_rates_treat_zero_cache_as_input() {
        use crate::provider::prices::effective;
        let implicit = CostConfig::new(5.0, 30.0).with_cache_read(0.5);
        let explicit = implicit.clone().with_cache_write(5.0);
        assert_eq!(effective(&implicit), effective(&explicit));
        assert_ne!(
            effective(&implicit),
            effective(&explicit.with_cache_write(6.0))
        );
    }
}
