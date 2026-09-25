//! Opt-in live price sources: models.dev, this crate's own checked file on
//! GitHub, or any URL serving the `prices.json` format.
//!
//! Nothing here runs unless the caller asks. A fetched table becomes the
//! **fetched layer** only through [`PriceTable::install_fetched`], which sits
//! above the built-in data and below any user override, and logs every
//! disagreement with the built-in data.

use super::{builtin_ref, write_layers, PriceError, PriceTable};
use crate::provider::model::{ContextTier, CostConfig};
use serde_json::{Map, Value};
use std::path::Path;
use std::time::{Duration, SystemTime};

/// How long [`PriceTable::fetch`] and [`PriceTable::fetch_cached`] wait for a
/// source before giving up.
pub const DEFAULT_FETCH_TIMEOUT: Duration = Duration::from_secs(10);

const MODELS_DEV_URL: &str = "https://models.dev/api.json";
const YOAGENT_MAIN_URL: &str =
    "https://raw.githubusercontent.com/yologdev/yoagent/main/src/provider/prices.json";

/// How many differing entries [`PriceTable::install_fetched`] spells out in
/// its log line; the rest are counted.
const LOGGED_CHANGES: usize = 5;

/// Where [`PriceTable::fetch`] gets prices from.
///
/// # Trust
///
/// A fetched table overrides the built-in data for every model it lists, so
/// pick a source you trust for the models you use:
///
/// - [`YoagentMain`](Self::YoagentMain) is this crate's own `prices.json` on
///   the `main` branch: the same format, the same review and the same price
///   audit as a release, without waiting for one.
/// - [`ModelsDev`](Self::ModelsDev) is [models.dev](https://models.dev), a
///   community-maintained database covering far more models. It is **not
///   authoritative** and has been provably wrong before (Claude context-tier
///   data, DeepSeek V4 Pro's price). Installing it logs every model where it
///   disagrees with the built-in data, but the fetched rates win.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PriceSource {
    /// <https://models.dev/api.json>, mapped into this crate's format.
    ModelsDev,
    /// A models.dev-format document at another URL (a mirror, a pinned
    /// snapshot, a test server).
    ModelsDevAt(String),
    /// This crate's checked `src/provider/prices.json` on GitHub `main`, so
    /// a price fix merged to `main` reaches you without a release. If `main`
    /// moves to a newer schema than this release reads — a field that
    /// changes billing was added — the fetch fails with
    /// [`PriceError::UnsupportedSchema`] and nothing changes. Metadata-only
    /// fields do not bump the schema, and this release ignores them.
    YoagentMain,
    /// Any URL serving this crate's `prices.json` format.
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

    fn is_models_dev(&self) -> bool {
        matches!(self, Self::ModelsDev | Self::ModelsDevAt(_))
    }
}

/// Where the table from [`PriceTable::fetch_cached`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PriceOrigin {
    /// Fetched just now; the cache was refreshed.
    Fetched,
    /// The cache, still within `max_age`. No request was made.
    Cache,
    /// The fetch failed, so an expired cache was used.
    StaleCache,
    /// The fetch failed and there was no usable cache: the built-in data.
    Builtin,
}

/// The result of [`PriceTable::fetch_cached`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct CachedPrices {
    pub table: PriceTable,
    pub origin: PriceOrigin,
}

/// One model where a table differs from a base table (see
/// [`PriceTable::changes_from`]).
///
/// Rates are compared as billed: a cache rate of `0` counts as the band's
/// input rate, so writing a no-premium cache rate out explicitly is not a
/// change.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct PriceChange {
    pub provider: String,
    pub model: String,
    /// The base table's rates; `None` when the base does not list the model.
    pub before: Option<CostConfig>,
    pub after: CostConfig,
}

impl std::fmt::Display for PriceChange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}: ", self.provider, self.model)?;
        let Some(before) = &self.before else {
            return write!(f, "new ({})", describe(&self.after));
        };
        let (b, a) = (effective(before), effective(&self.after));
        let mut parts = Vec::new();
        for (name, x, y) in [
            ("input", b.input_per_million, a.input_per_million),
            ("output", b.output_per_million, a.output_per_million),
            (
                "cache_read",
                b.cache_read_per_million,
                a.cache_read_per_million,
            ),
            (
                "cache_write",
                b.cache_write_per_million,
                a.cache_write_per_million,
            ),
        ] {
            if x != y {
                parts.push(format!("{name} {x} -> {y}"));
            }
        }
        if b.context_tiers != a.context_tiers {
            parts.push(format!(
                "tiers {} -> {}",
                describe_tiers(&before.context_tiers),
                describe_tiers(&self.after.context_tiers)
            ));
        }
        write!(f, "{}", parts.join(", "))
    }
}

fn describe(c: &CostConfig) -> String {
    let mut s = format!(
        "input {}, output {}, cache_read {}, cache_write {}",
        c.input_per_million,
        c.output_per_million,
        c.cache_read_per_million,
        c.cache_write_per_million
    );
    if !c.context_tiers.is_empty() {
        s.push_str(&format!(", tiers {}", describe_tiers(&c.context_tiers)));
    }
    s
}

fn describe_tiers(tiers: &[ContextTier]) -> String {
    let items: Vec<String> = tiers
        .iter()
        .map(|t| {
            format!(
                ">{}: {}/{}/{}/{}",
                t.above_prompt_tokens,
                t.input_per_million,
                t.output_per_million,
                t.cache_read_per_million,
                t.cache_write_per_million
            )
        })
        .collect();
    format!("[{}]", items.join("; "))
}

/// `c` as billed: every zero cache rate replaced by its band's input rate.
fn effective(c: &CostConfig) -> CostConfig {
    let or = |rate: f64, input: f64| if rate == 0.0 { input } else { rate };
    let mut e = c.clone();
    e.cache_read_per_million = or(c.cache_read_per_million, c.input_per_million);
    e.cache_write_per_million = or(c.cache_write_per_million, c.input_per_million);
    for t in &mut e.context_tiers {
        t.cache_read_per_million = or(t.cache_read_per_million, t.input_per_million);
        t.cache_write_per_million = or(t.cache_write_per_million, t.input_per_million);
    }
    e
}

impl PriceTable {
    /// Fetch a table from `source`, with [`DEFAULT_FETCH_TIMEOUT`].
    ///
    /// Never called implicitly. The result is not installed: pass it to
    /// [`install_fetched`](Self::install_fetched) (process-wide) or
    /// [`ModelConfig::with_prices`](crate::provider::ModelConfig::with_prices)
    /// (one config). Mind the trust caveats on [`PriceSource`].
    pub async fn fetch(source: &PriceSource) -> Result<PriceTable, PriceError> {
        Self::fetch_with_timeout(source, DEFAULT_FETCH_TIMEOUT).await
    }

    /// [`fetch`](Self::fetch) with an explicit timeout covering the whole
    /// request, body included.
    pub async fn fetch_with_timeout(
        source: &PriceSource,
        timeout: Duration,
    ) -> Result<PriceTable, PriceError> {
        let url = source.url().to_string();
        let request_error = |e: reqwest::Error| {
            if e.is_timeout() {
                PriceError::Timeout {
                    url: url.clone(),
                    timeout,
                }
            } else {
                PriceError::Request {
                    url: url.clone(),
                    source: e,
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
        if source.is_models_dev() {
            Self::from_models_dev_value(&serde_json::from_str(&body)?, &url)
        } else {
            Self::from_json_str_lenient(&body)
        }
    }

    /// Map a models.dev `api.json` document into a table.
    ///
    /// Entries are keyed by models.dev's provider key, except `alibaba`,
    /// which is this crate's `qwen`. `cost.input` / `output` / `cache_read` /
    /// `cache_write` map directly; context tiers come from the `tiers` array
    /// (`{"tier": {"type": "context", "size": N}, ...rates}`), or, when that
    /// is absent, from the older `context_over_200k` object as one tier above
    /// 200,000 prompt tokens.
    ///
    /// A model is **skipped**, not approximated, when its cost carries
    /// structure a [`CostConfig`] cannot express: an unknown key, a
    /// non-zero `reasoning` rate different from `output`, a non-context tier, or rates
    /// that fail validation. Audio rates (`input_audio`, `output_audio`) are
    /// ignored — this crate sends text. A document from which no model maps
    /// is an error, so a changed envelope cannot install an empty table.
    pub fn from_models_dev_json(json: &str) -> Result<PriceTable, PriceError> {
        Self::from_models_dev_value(&serde_json::from_str(json)?, MODELS_DEV_URL)
    }

    fn from_models_dev_value(db: &Value, source: &str) -> Result<PriceTable, PriceError> {
        let providers = db
            .as_object()
            .ok_or_else(|| PriceError::ModelsDev("the document is not a JSON object".into()))?;
        let mut table = PriceTable::new();
        let (mut skipped, mut unpriced) = (0usize, 0usize);
        for (provider_key, provider) in providers {
            let Some(models) = provider.get("models").and_then(Value::as_object) else {
                continue;
            };
            let provider = match provider_key.as_str() {
                "alibaba" => "qwen",
                other => other,
            };
            for (model, info) in models {
                let Some(cost) = info.get("cost") else {
                    unpriced += 1;
                    continue;
                };
                let Some(cost) = map_models_dev_cost(cost) else {
                    skipped += 1;
                    continue;
                };
                let entry = super::PriceEntry::new(cost).with_source(source);
                if table.insert(provider, model.as_str(), entry).is_err() {
                    skipped += 1;
                }
            }
        }
        tracing::debug!(
            mapped = table.len(),
            skipped,
            unpriced,
            "yoagent prices: mapped models.dev data"
        );
        if table.is_empty() {
            return Err(PriceError::ModelsDev(format!(
                "no model carried a usable price ({} providers, {skipped} models skipped); \
                 the schema may have changed",
                providers.len()
            )));
        }
        Ok(table)
    }

    /// Every model in `self` whose billed rates differ from `base`, or that
    /// `base` does not list. Models only `base` lists are not reported —
    /// layering never removes an entry.
    pub fn changes_from(&self, base: &PriceTable) -> Vec<PriceChange> {
        self.iter()
            .filter_map(|(provider, model, entry)| {
                let before = base.entry(provider, model).map(|b| &b.cost);
                if before.is_some_and(|b| effective(b) == effective(&entry.cost)) {
                    return None;
                }
                Some(PriceChange {
                    provider: provider.to_string(),
                    model: model.to_string(),
                    before: before.cloned(),
                    after: entry.cost.clone(),
                })
            })
            .collect()
    }

    /// Install `table` as the process-wide **fetched layer**: above the
    /// built-in data, below any user override
    /// ([`install_override`](Self::install_override)). Replaces a previous
    /// fetched layer. Affects configs built afterwards.
    ///
    /// Disagreements are made visible rather than silent: every built-in
    /// model the fetched table prices differently is logged with
    /// `tracing::warn!` (the count and the first few differences), and new
    /// models are counted at `info`. The returned list holds all of them
    /// ([`PriceChange::before`] is `None` for a new model).
    pub fn install_fetched(table: PriceTable) -> Vec<PriceChange> {
        let changes = table.changes_from(builtin_ref());
        let differing: Vec<&PriceChange> = changes.iter().filter(|c| c.before.is_some()).collect();
        let added = changes.len() - differing.len();
        if !differing.is_empty() {
            let first: Vec<String> = differing
                .iter()
                .take(LOGGED_CHANGES)
                .map(|c| c.to_string())
                .collect();
            tracing::warn!(
                differing = differing.len(),
                "yoagent prices: the fetched table disagrees with the built-in data on {} \
                 model(s) and now takes precedence for them: {}{}",
                differing.len(),
                first.join("; "),
                if differing.len() > LOGGED_CHANGES {
                    format!("; and {} more", differing.len() - LOGGED_CHANGES)
                } else {
                    String::new()
                }
            );
        }
        tracing::info!(
            entries = table.len(),
            differing = differing.len(),
            added,
            "yoagent prices: installed fetched prices"
        );
        let mut layers = write_layers();
        layers.fetched = Some(table);
        layers.rebuild();
        drop(layers);
        changes
    }

    /// Remove the fetched layer.
    pub fn clear_fetched() {
        let mut layers = write_layers();
        layers.fetched = None;
        layers.rebuild();
    }

    /// Fetch `source` through a cache file at `cache_path`, never failing.
    ///
    /// 1. A cache younger than `max_age` (by modification time) is returned
    ///    without a request — [`PriceOrigin::Cache`].
    /// 2. Otherwise `source` is fetched; on success the cache is rewritten
    ///    (in this crate's format, whatever the source) —
    ///    [`PriceOrigin::Fetched`].
    /// 3. If the fetch fails, an expired cache is used —
    ///    [`PriceOrigin::StaleCache`] — and failing that the built-in data —
    ///    [`PriceOrigin::Builtin`]. Failures are logged with `tracing::warn!`.
    ///
    /// Use one cache path per source. The result is not installed:
    ///
    /// ```no_run
    /// # use std::time::Duration;
    /// # use yoagent::provider::{PriceOrigin, PriceSource, PriceTable};
    /// # async fn run() {
    /// let prices = PriceTable::fetch_cached(
    ///     &PriceSource::YoagentMain,
    ///     "/var/cache/myapp/yoagent-prices.json",
    ///     Duration::from_secs(24 * 3600),
    /// )
    /// .await;
    /// if prices.origin != PriceOrigin::Builtin {
    ///     PriceTable::install_fetched(prices.table);
    /// }
    /// # }
    /// ```
    pub async fn fetch_cached(
        source: &PriceSource,
        cache_path: impl AsRef<Path>,
        max_age: Duration,
    ) -> CachedPrices {
        let path = cache_path.as_ref();
        let cached = read_cache(path).await;
        if let Some((table, age)) = &cached {
            if *age < max_age {
                return CachedPrices {
                    table: table.clone(),
                    origin: PriceOrigin::Cache,
                };
            }
        }
        match Self::fetch(source).await {
            Ok(table) => {
                write_cache(path, &table).await;
                CachedPrices {
                    table,
                    origin: PriceOrigin::Fetched,
                }
            }
            Err(e) => {
                tracing::warn!(
                    url = source.url(),
                    error = %e,
                    "yoagent prices: fetch failed; falling back to {}",
                    if cached.is_some() { "the expired cache" } else { "built-in prices" }
                );
                match cached {
                    Some((table, _)) => CachedPrices {
                        table,
                        origin: PriceOrigin::StaleCache,
                    },
                    None => CachedPrices {
                        table: PriceTable::builtin(),
                        origin: PriceOrigin::Builtin,
                    },
                }
            }
        }
    }
}

/// The cached table and its age, if the cache exists and parses. A corrupt
/// cache is logged and treated as absent.
async fn read_cache(path: &Path) -> Option<(PriceTable, Duration)> {
    let meta = tokio::fs::metadata(path).await.ok()?;
    let age = match meta.modified() {
        // A modification time in the future reads as brand new.
        Ok(modified) => SystemTime::now()
            .duration_since(modified)
            .unwrap_or(Duration::ZERO),
        // No modification time on this platform: always refetch first.
        Err(_) => Duration::MAX,
    };
    let text = tokio::fs::read_to_string(path).await.ok()?;
    match PriceTable::from_json_str_lenient(&text) {
        Ok(table) => Some((table, age)),
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "yoagent prices: ignoring an unreadable price cache"
            );
            None
        }
    }
}

/// Write the cache through a temporary file and a rename, so a concurrent
/// reader never sees half a file. Failure is logged, not returned: the
/// fetched table is still good.
async fn write_cache(path: &Path, table: &PriceTable) {
    let result = async {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            tokio::fs::create_dir_all(dir).await?;
        }
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        tokio::fs::write(&tmp, table.to_json()).await?;
        tokio::fs::rename(&tmp, path).await
    }
    .await;
    if let Err(e) = result {
        tracing::warn!(
            path = %path.display(),
            error = %e,
            "yoagent prices: could not write the price cache"
        );
    }
}

/// One models.dev `cost` object as a [`CostConfig`], or `None` when it
/// carries structure a `CostConfig` cannot express.
fn map_models_dev_cost(cost: &Value) -> Option<CostConfig> {
    let obj = cost.as_object()?;
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
        for t in tiers.as_array()? {
            let t = t.as_object()?;
            let spec = t.get("tier")?.as_object()?;
            if spec.get("type")?.as_str()? != "context"
                || spec.keys().any(|k| k != "type" && k != "size")
            {
                return None;
            }
            let size = spec.get("size")?.as_f64()?;
            if size < 1.0 || size.fract() != 0.0 || size > u64::MAX as f64 {
                return None;
            }
            // Pushed in document order; validation rejects an unsorted list.
            config
                .context_tiers
                .push(tier(size as u64, map_band(t, &["tier"])?));
        }
    } else if let Some(over) = obj.get("context_over_200k") {
        config
            .context_tiers
            .push(tier(200_000, map_band(over.as_object()?, &[])?));
    }
    Some(config)
}

/// One band's `(input, output, cache_read, cache_write)`. `structural` names
/// the keys the caller handles itself.
fn map_band(obj: &Map<String, Value>, structural: &[&str]) -> Option<(f64, f64, f64, f64)> {
    for key in obj.keys() {
        match key.as_str() {
            "input" | "output" | "cache_read" | "cache_write" | "reasoning" => {}
            // Audio is priced separately and this crate sends text.
            "input_audio" | "output_audio" => {}
            k if structural.contains(&k) => {}
            _ => return None,
        }
    }
    let input = obj.get("input")?.as_f64()?;
    let output = obj.get("output")?.as_f64()?;
    let optional = |key: &str| match obj.get(key) {
        None | Some(Value::Null) => Some(0.0),
        Some(v) => v.as_f64(),
    };
    let cache_read = optional("cache_read")?;
    let cache_write = optional("cache_write")?;
    // Reasoning tokens bill as output here, so a separate reasoning rate is
    // expressible only when it is the output rate, or `0` (not separately
    // priced — models.dev's encoding for reasoning included in output).
    if let Some(reasoning) = obj.get("reasoning") {
        let reasoning = reasoning.as_f64()?;
        if reasoning != output && reasoning != 0.0 {
            return None;
        }
    }
    Some((input, output, cache_read, cache_write))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(cost: serde_json::Value) -> Option<CostConfig> {
        map_models_dev_cost(&cost)
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
        let implicit = CostConfig::new(5.0, 30.0).with_cache_read(0.5);
        let explicit = implicit.clone().with_cache_write(5.0);
        assert_eq!(effective(&implicit), effective(&explicit));
        assert_ne!(
            effective(&implicit),
            effective(&explicit.with_cache_write(6.0))
        );
    }
}
