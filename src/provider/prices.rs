//! Model prices as data: the built-in `prices.json` and [`PriceTable`].
//!
//! Every price this crate knows lives in `src/provider/prices.json`, embedded
//! at compile time, keyed by [`ModelConfig::provider`] then [`ModelConfig::id`]:
//!
//! ```json
//! {
//!   "schema": 1,
//!   "providers": {
//!     "anthropic": {
//!       "claude-opus-5-5": {
//!         "input": 4.0, "output": 20.0, "cache_read": 0.2, "cache_write": 5.0,
//!         "source": "https://platform.claude.com/docs/en/about-claude/pricing",
//!         "verified": "2026-09-25"
//!       }
//!     }
//!   }
//! }
//! ```
//!
//! Rates are USD per million tokens. An entry may add `"tiers"` (context
//! tiers: `above_prompt_tokens` plus the four rates, strictly ascending) and
//! metadata (`note`, `source`, `verified`, `cache_write_at_input`,
//! `absent_upstream`). A cache rate that is omitted or `0` bills at the band's
//! input rate, as for [`CostConfig`].
//!
//! The first-party constructors ([`ModelConfig::anthropic`],
//! [`ModelConfig::openai`], [`ModelConfig::google`], …) look their
//! `(provider, id)` up when they build a config and get `Some` for a listed
//! model, `None` otherwise. Gateways and custom endpoints
//! ([`ModelConfig::custom`], [`ModelConfig::openai_compat`],
//! [`ModelConfig::local`], [`ModelConfig::ollama`], the OpenCode gateways)
//! never look up: what they bill is not the vendor's list price.
//!
//! # Runtime overrides and precedence
//!
//! Constructors read a process-wide **resolved table**, built in layers
//! (highest first):
//!
//! 1. an explicit `config.cost` you set after construction — it is a plain
//!    field, so it always wins;
//! 2. the **user layer**: [`PriceTable::install_override`], or on first use
//!    the file named by the `YOAGENT_PRICES` environment variable;
//! 3. the **fetched layer**: [`PriceTable::install_fetched`], opt-in, from a
//!    live [`PriceSource`] (see its trust caveats) — never fetched unless you
//!    call [`PriceTable::fetch`] or [`PriceTable::fetch_cached`];
//! 4. the built-in `prices.json`.
//!
//! Each layer replaces whole entries per `(provider, id)`; a partial override
//! file overrides exactly the models it lists. **Constructors resolve when
//! they run**, so install overrides before building configs — a config built
//! earlier keeps the price it was built with (re-price it with
//! [`ModelConfig::with_prices`] and [`PriceTable::resolved`]).
//!
//! See `docs/concepts/pricing.md` for the format, precedence and trust
//! caveats.
//!
//! [`ModelConfig::with_prices`]: crate::provider::ModelConfig::with_prices
//!
//! [`ModelConfig::provider`]: crate::provider::ModelConfig::provider
//! [`ModelConfig::id`]: crate::provider::ModelConfig::id
//! [`ModelConfig::anthropic`]: crate::provider::ModelConfig::anthropic
//! [`ModelConfig::openai`]: crate::provider::ModelConfig::openai
//! [`ModelConfig::google`]: crate::provider::ModelConfig::google
//! [`ModelConfig::custom`]: crate::provider::ModelConfig::custom
//! [`ModelConfig::openai_compat`]: crate::provider::ModelConfig::openai_compat
//! [`ModelConfig::local`]: crate::provider::ModelConfig::local
//! [`ModelConfig::ollama`]: crate::provider::ModelConfig::ollama

use super::model::{ContextTier, CostConfig};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, PoisonError, RwLock};

mod fetch;
pub use fetch::{CachedPrices, PriceChange, PriceOrigin, PriceSource, DEFAULT_FETCH_TIMEOUT};

/// The only `schema` value this release reads.
pub const PRICE_SCHEMA_VERSION: u32 = 1;

/// Environment variable naming a price file for the user layer, read once
/// when the process-wide table is first used.
pub const PRICES_ENV_VAR: &str = "YOAGENT_PRICES";

/// The built-in data, embedded at compile time.
const BUILTIN_JSON: &str = include_str!("prices.json");

/// Why price data was rejected.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PriceError {
    /// Not JSON, or JSON that does not match the schema (a missing rate, a
    /// misspelled or unknown field, a string where a number belongs).
    #[error("price data does not match the price schema: {0}")]
    Json(#[from] serde_json::Error),
    /// The file could not be read.
    #[error("cannot read price file {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// `schema` is missing or is a version this release does not read.
    #[error("unsupported price schema {found}; this yoagent reads schema {PRICE_SCHEMA_VERSION}")]
    UnsupportedSchema {
        /// The `schema` value as found, or `missing`.
        found: String,
    },
    /// A rate is negative or not finite.
    #[error("{provider}/{model}: {field} is {value}; rates must be finite and non-negative")]
    InvalidRate {
        provider: String,
        model: String,
        field: String,
        value: f64,
    },
    /// Context tier thresholds are not strictly ascending.
    #[error("{provider}/{model}: tier thresholds must be strictly ascending, got {thresholds:?}")]
    TiersNotAscending {
        provider: String,
        model: String,
        thresholds: Vec<u64>,
    },
    /// Any other inconsistency inside one entry.
    #[error("{provider}/{model}: {reason}")]
    InvalidEntry {
        provider: String,
        model: String,
        reason: String,
    },
    /// A price source answered with a non-success HTTP status.
    #[error("GET {url} returned HTTP {status}")]
    Http { url: String, status: u16 },
    /// A price source did not answer within the timeout.
    #[error("GET {url} timed out after {timeout:?}")]
    Timeout {
        url: String,
        timeout: std::time::Duration,
    },
    /// A price source could not be reached or its body not read.
    #[error("GET {url} failed: {source}")]
    Request {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    /// models.dev data this crate cannot map — its envelope changed, or no
    /// model in it carried a usable price.
    #[error("cannot map models.dev data: {0}")]
    ModelsDev(String),
}

/// One model's price and where it came from.
///
/// Built from data by [`PriceTable`], or in code with [`PriceEntry::new`] and
/// the `with_*` methods. Marked `#[non_exhaustive]` so metadata can grow; the
/// fields are public for reading.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct PriceEntry {
    /// The rates a [`ModelConfig`](crate::provider::ModelConfig) gets.
    pub cost: CostConfig,
    /// The vendor charges nothing extra for cache writes and the entry
    /// states `cache_write` explicitly as the input rate. Validated: every
    /// band's `cache_write` must equal its `input`. The price audit uses it to
    /// accept an entry whose `cache_write` models.dev does not list.
    pub cache_write_at_input: bool,
    /// A caveat — a rate the vendor page does not itself state, a tier that is
    /// deliberately not modelled.
    pub note: Option<String>,
    /// The vendor page the rates were checked against. The authority when
    /// this data and any other source disagree.
    pub source: Option<String>,
    /// The date (`YYYY-MM-DD`) the rates were last checked against `source`.
    pub verified: Option<String>,
    /// Set only when models.dev genuinely lacks this model: why, and the date
    /// that was checked. The price audit otherwise treats absence as drift.
    pub absent_upstream: Option<String>,
}

impl PriceEntry {
    /// An entry with these rates and no metadata.
    pub fn new(cost: CostConfig) -> Self {
        Self {
            cost,
            cache_write_at_input: false,
            note: None,
            source: None,
            verified: None,
            absent_upstream: None,
        }
    }

    /// Attach a caveat.
    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }

    /// Record the vendor page the rates came from.
    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    /// Record when the rates were checked (`YYYY-MM-DD`).
    pub fn with_verified(mut self, date: impl Into<String>) -> Self {
        self.verified = Some(date.into());
        self
    }

    /// Declare that cache writes bill at the input rate (see
    /// [`cache_write_at_input`](Self::cache_write_at_input)).
    pub fn with_cache_write_at_input(mut self, on: bool) -> Self {
        self.cache_write_at_input = on;
        self
    }

    fn validate(&self, provider: &str, model: &str) -> Result<(), PriceError> {
        let entry_err = |reason: String| PriceError::InvalidEntry {
            provider: provider.to_string(),
            model: model.to_string(),
            reason,
        };
        if provider.is_empty() || model.is_empty() {
            return Err(entry_err("provider and model id must be non-empty".into()));
        }
        let c = &self.cost;
        let mut rates: Vec<(String, f64)> = vec![
            ("input".into(), c.input_per_million),
            ("output".into(), c.output_per_million),
            ("cache_read".into(), c.cache_read_per_million),
            ("cache_write".into(), c.cache_write_per_million),
        ];
        for (i, t) in c.context_tiers.iter().enumerate() {
            rates.push((format!("tiers[{i}].input"), t.input_per_million));
            rates.push((format!("tiers[{i}].output"), t.output_per_million));
            rates.push((format!("tiers[{i}].cache_read"), t.cache_read_per_million));
            rates.push((format!("tiers[{i}].cache_write"), t.cache_write_per_million));
        }
        for (field, value) in rates {
            if !value.is_finite() || value < 0.0 {
                return Err(PriceError::InvalidRate {
                    provider: provider.to_string(),
                    model: model.to_string(),
                    field,
                    value,
                });
            }
        }
        let thresholds: Vec<u64> = c
            .context_tiers
            .iter()
            .map(|t| t.above_prompt_tokens)
            .collect();
        if thresholds.first() == Some(&0) {
            return Err(entry_err(
                "a tier above 0 prompt tokens applies to every request; set the base rates instead"
                    .into(),
            ));
        }
        if thresholds.windows(2).any(|w| w[0] >= w[1]) {
            return Err(PriceError::TiersNotAscending {
                provider: provider.to_string(),
                model: model.to_string(),
                thresholds,
            });
        }
        if self.cache_write_at_input {
            let base = (c.input_per_million, c.cache_write_per_million);
            let bands = std::iter::once(base).chain(
                c.context_tiers
                    .iter()
                    .map(|t| (t.input_per_million, t.cache_write_per_million)),
            );
            for (input, cache_write) in bands {
                if cache_write != input {
                    return Err(entry_err(format!(
                        "cache_write_at_input is set but a band has cache_write {cache_write} \
                         and input {input}; state the input rate explicitly"
                    )));
                }
            }
        }
        Ok(())
    }
}

/// Prices keyed by provider, then model id.
///
/// Parse one with [`from_json_str`](Self::from_json_str) or
/// [`from_path`](Self::from_path), start from [`builtin`](Self::builtin), and
/// combine with [`layered`](Self::layered). Every way in validates: rates must
/// be finite and non-negative, tier thresholds strictly ascending, and
/// `schema` must be [`PRICE_SCHEMA_VERSION`]. Unknown fields are rejected, so
/// a misspelled `"cache_reed"` fails loudly instead of billing at the input
/// rate.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PriceTable {
    entries: BTreeMap<String, BTreeMap<String, PriceEntry>>,
    comment: Option<String>,
}

impl PriceTable {
    /// An empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// The data compiled into this release (`src/provider/prices.json`).
    pub fn builtin() -> PriceTable {
        builtin_ref().clone()
    }

    /// Parse and validate a table in this crate's JSON format.
    pub fn from_json_str(json: &str) -> Result<PriceTable, PriceError> {
        let value: serde_json::Value = serde_json::from_str(json)?;
        match value.get("schema") {
            Some(v) if v.as_u64() == Some(u64::from(PRICE_SCHEMA_VERSION)) => {}
            Some(v) => {
                return Err(PriceError::UnsupportedSchema {
                    found: v.to_string(),
                })
            }
            None => {
                return Err(PriceError::UnsupportedSchema {
                    found: "missing".into(),
                })
            }
        }
        let wire: FileWire = serde_json::from_value(value)?;
        let mut table = PriceTable {
            entries: BTreeMap::new(),
            comment: wire.comment,
        };
        for (provider, models) in wire.providers {
            for (model, entry) in models {
                table.insert(&provider, &model, entry.into())?;
            }
        }
        Ok(table)
    }

    /// Read, parse and validate a table file.
    pub fn from_path(path: impl AsRef<Path>) -> Result<PriceTable, PriceError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|source| PriceError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_json_str(&text)
    }

    /// Serialize to this crate's JSON format (pretty-printed, sorted), which
    /// [`from_json_str`](Self::from_json_str) reads back to an equal table.
    pub fn to_json(&self) -> String {
        let wire = FileWire {
            schema: PRICE_SCHEMA_VERSION,
            comment: self.comment.clone(),
            providers: self
                .entries
                .iter()
                .map(|(p, models)| {
                    let models = models
                        .iter()
                        .map(|(m, e)| (m.clone(), EntryWire::from(e)))
                        .collect();
                    (p.clone(), models)
                })
                .collect(),
        };
        // Only strings, finite numbers and maps: serialization cannot fail.
        serde_json::to_string_pretty(&wire).expect("a validated price table serializes")
    }

    /// Add or replace one model's entry, validating it. Returns the entry it
    /// replaced.
    pub fn insert(
        &mut self,
        provider: impl Into<String>,
        model: impl Into<String>,
        entry: PriceEntry,
    ) -> Result<Option<PriceEntry>, PriceError> {
        let (provider, model) = (provider.into(), model.into());
        entry.validate(&provider, &model)?;
        Ok(self
            .entries
            .entry(provider)
            .or_default()
            .insert(model, entry))
    }

    /// `self` with every entry of `over` replacing the entry for the same
    /// `(provider, id)`. Entries `over` does not list are kept, so a partial
    /// table overrides exactly what it contains.
    pub fn layered(&self, over: &PriceTable) -> PriceTable {
        let mut out = self.clone();
        for (provider, models) in &over.entries {
            let slot = out.entries.entry(provider.clone()).or_default();
            for (model, entry) in models {
                slot.insert(model.clone(), entry.clone());
            }
        }
        out
    }

    /// The entry for `(provider, id)`, with its metadata.
    pub fn entry(&self, provider: &str, id: &str) -> Option<&PriceEntry> {
        self.entries.get(provider)?.get(id)
    }

    /// The rates for `(provider, id)`, or `None` when the table does not list
    /// the model.
    pub fn cost(&self, provider: &str, id: &str) -> Option<CostConfig> {
        self.entry(provider, id).map(|e| e.cost.clone())
    }

    /// Every entry as `(provider, id, entry)`, sorted by provider then id.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str, &PriceEntry)> + '_ {
        self.entries
            .iter()
            .flat_map(|(p, models)| models.iter().map(move |(m, e)| (p.as_str(), m.as_str(), e)))
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.entries.values().map(BTreeMap::len).sum()
    }

    /// Whether the table lists no model.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The parsed built-in table. The data is checked by this crate's tests, so
/// a parse failure is a build defect, not a runtime condition.
fn builtin_ref() -> &'static PriceTable {
    static BUILTIN: OnceLock<PriceTable> = OnceLock::new();
    BUILTIN.get_or_init(|| {
        PriceTable::from_json_str(BUILTIN_JSON)
            .unwrap_or_else(|e| panic!("yoagent's built-in prices.json is invalid: {e}"))
    })
}

/// The process-wide layers and their resolution.
struct Layers {
    fetched: Option<PriceTable>,
    user: Option<PriceTable>,
    /// `builtin`, then `fetched`, then `user`, rebuilt on every install so a
    /// lookup is one map read.
    resolved: PriceTable,
}

impl Layers {
    fn rebuild(&mut self) {
        let mut table = builtin_ref().clone();
        for layer in [&self.fetched, &self.user].into_iter().flatten() {
            table = table.layered(layer);
        }
        self.resolved = table;
    }
}

fn layers() -> &'static RwLock<Layers> {
    static LAYERS: OnceLock<RwLock<Layers>> = OnceLock::new();
    LAYERS.get_or_init(|| {
        let mut layers = Layers {
            fetched: None,
            user: env_override(),
            resolved: PriceTable::default(),
        };
        layers.rebuild();
        RwLock::new(layers)
    })
}

/// The user layer named by [`PRICES_ENV_VAR`], if set and valid. A bad file
/// is logged and ignored: a typo in an environment variable must not take
/// down every constructor in the process.
fn env_override() -> Option<PriceTable> {
    let path = std::env::var_os(PRICES_ENV_VAR)?;
    if path.is_empty() {
        return None;
    }
    match PriceTable::from_path(&path) {
        Ok(table) => {
            tracing::info!(
                path = %Path::new(&path).display(),
                entries = table.len(),
                "yoagent prices: loaded {PRICES_ENV_VAR} override"
            );
            Some(table)
        }
        Err(e) => {
            tracing::warn!(
                path = %Path::new(&path).display(),
                error = %e,
                "yoagent prices: ignoring {PRICES_ENV_VAR}; using built-in prices"
            );
            None
        }
    }
}

fn read_layers() -> std::sync::RwLockReadGuard<'static, Layers> {
    // A panic while holding the lock cannot leave `Layers` half-written
    // (every write replaces whole fields), so a poisoned lock is still valid.
    layers().read().unwrap_or_else(PoisonError::into_inner)
}

fn write_layers() -> std::sync::RwLockWriteGuard<'static, Layers> {
    layers().write().unwrap_or_else(PoisonError::into_inner)
}

impl PriceTable {
    /// Install `table` as the process-wide **user layer**, replacing any
    /// previous override (including one loaded from `YOAGENT_PRICES`).
    ///
    /// Its entries take precedence over the fetched layer and the built-in data per
    /// `(provider, id)`; models it does not list keep their lower-layer price.
    /// Affects configs built **after** this call — constructors resolve when
    /// they run.
    ///
    /// ```
    /// # use yoagent::provider::{ModelConfig, PriceTable};
    /// let mine = PriceTable::from_json_str(r#"{"schema": 1, "providers": {
    ///     "deepseek": {"deepseek-flash": {"input": 0.3, "output": 1.2, "cache_read": 0.006}}}}"#)?;
    /// PriceTable::install_override(mine);
    /// let config = ModelConfig::deepseek("deepseek-flash", "DeepSeek Flash");
    /// assert_eq!(config.cost.unwrap().input_per_million, 0.3);
    /// # PriceTable::clear_override();
    /// # Ok::<(), yoagent::provider::PriceError>(())
    /// ```
    pub fn install_override(table: PriceTable) {
        let mut layers = write_layers();
        layers.user = Some(table);
        layers.rebuild();
    }

    /// Remove the user layer, including one loaded from `YOAGENT_PRICES`
    /// (which is not re-read).
    pub fn clear_override() {
        let mut layers = write_layers();
        layers.user = None;
        layers.rebuild();
    }

    /// A snapshot of the process-wide resolved table — what a constructor
    /// called now would use.
    pub fn resolved() -> PriceTable {
        read_layers().resolved.clone()
    }
}

/// The rates a first-party constructor gets for `(provider, id)`, from the
/// process-wide resolved table.
pub(crate) fn resolved_cost(provider: &str, id: &str) -> Option<CostConfig> {
    read_layers().resolved.cost(provider, id)
}

// ---- wire format ---------------------------------------------------------

fn is_zero(v: &f64) -> bool {
    *v == 0.0
}

fn is_false(v: &bool) -> bool {
    !*v
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileWire {
    schema: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    comment: Option<String>,
    #[serde(default)]
    providers: BTreeMap<String, BTreeMap<String, EntryWire>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EntryWire {
    input: f64,
    output: f64,
    #[serde(default, skip_serializing_if = "is_zero")]
    cache_read: f64,
    #[serde(default, skip_serializing_if = "is_zero")]
    cache_write: f64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tiers: Vec<TierWire>,
    #[serde(default, skip_serializing_if = "is_false")]
    cache_write_at_input: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    verified: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    absent_upstream: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TierWire {
    above_prompt_tokens: u64,
    input: f64,
    output: f64,
    #[serde(default, skip_serializing_if = "is_zero")]
    cache_read: f64,
    #[serde(default, skip_serializing_if = "is_zero")]
    cache_write: f64,
}

impl From<EntryWire> for PriceEntry {
    fn from(w: EntryWire) -> Self {
        let mut cost = CostConfig::new(w.input, w.output)
            .with_cache_read(w.cache_read)
            .with_cache_write(w.cache_write);
        // Assigned in file order, not through `with_context_tier` (which
        // sorts), so validation sees — and rejects — an unsorted file.
        cost.context_tiers = w
            .tiers
            .into_iter()
            .map(|t| ContextTier {
                above_prompt_tokens: t.above_prompt_tokens,
                input_per_million: t.input,
                output_per_million: t.output,
                cache_read_per_million: t.cache_read,
                cache_write_per_million: t.cache_write,
            })
            .collect();
        PriceEntry {
            cost,
            cache_write_at_input: w.cache_write_at_input,
            note: w.note,
            source: w.source,
            verified: w.verified,
            absent_upstream: w.absent_upstream,
        }
    }
}

impl From<&PriceEntry> for EntryWire {
    fn from(e: &PriceEntry) -> Self {
        let c = &e.cost;
        EntryWire {
            input: c.input_per_million,
            output: c.output_per_million,
            cache_read: c.cache_read_per_million,
            cache_write: c.cache_write_per_million,
            tiers: c
                .context_tiers
                .iter()
                .map(|t| TierWire {
                    above_prompt_tokens: t.above_prompt_tokens,
                    input: t.input_per_million,
                    output: t.output_per_million,
                    cache_read: t.cache_read_per_million,
                    cache_write: t.cache_write_per_million,
                })
                .collect(),
            cache_write_at_input: e.cache_write_at_input,
            note: e.note.clone(),
            source: e.source.clone(),
            verified: e.verified.clone(),
            absent_upstream: e.absent_upstream.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(entry: &str) -> String {
        format!(r#"{{"schema": 1, "providers": {{"p": {{"m": {entry}}}}}}}"#)
    }

    #[test]
    fn builtin_parses_and_round_trips() {
        let t = PriceTable::builtin();
        assert!(t.len() >= 13, "built-in table has {} entries", t.len());
        let back = PriceTable::from_json_str(&t.to_json()).unwrap();
        assert_eq!(back, t);
    }

    #[test]
    fn schema_version_is_checked() {
        let e = PriceTable::from_json_str(r#"{"schema": 2, "providers": {}}"#).unwrap_err();
        assert!(matches!(e, PriceError::UnsupportedSchema { .. }), "{e}");
        let e = PriceTable::from_json_str(r#"{"providers": {}}"#).unwrap_err();
        assert!(matches!(e, PriceError::UnsupportedSchema { .. }), "{e}");
        // Positive control.
        assert!(PriceTable::from_json_str(r#"{"schema": 1, "providers": {}}"#).is_ok());
    }

    #[test]
    fn negative_rates_are_rejected() {
        let e = PriceTable::from_json_str(&one(r#"{"input": -1, "output": 2}"#)).unwrap_err();
        assert!(
            matches!(e, PriceError::InvalidRate { ref field, .. } if field == "input"),
            "{e}"
        );
        let e = PriceTable::from_json_str(&one(
            r#"{"input": 1, "output": 2, "tiers": [{"above_prompt_tokens": 10, "input": 2, "output": 3, "cache_read": -0.1}]}"#,
        ))
        .unwrap_err();
        assert!(
            matches!(e, PriceError::InvalidRate { ref field, .. } if field == "tiers[0].cache_read"),
            "{e}"
        );
        assert!(PriceTable::from_json_str(&one(r#"{"input": 0, "output": 0}"#)).is_ok());
    }

    #[test]
    fn tiers_must_ascend() {
        let tiers = |a: u64, b: u64| {
            one(&format!(
                r#"{{"input": 1, "output": 2, "tiers": [
                    {{"above_prompt_tokens": {a}, "input": 2, "output": 3}},
                    {{"above_prompt_tokens": {b}, "input": 3, "output": 4}}]}}"#
            ))
        };
        for (a, b) in [(200, 100), (100, 100)] {
            let e = PriceTable::from_json_str(&tiers(a, b)).unwrap_err();
            assert!(matches!(e, PriceError::TiersNotAscending { .. }), "{e}");
        }
        let t = PriceTable::from_json_str(&tiers(100, 200)).unwrap();
        assert_eq!(t.cost("p", "m").unwrap().context_tiers.len(), 2);
        let zero = one(
            r#"{"input": 1, "output": 2, "tiers": [{"above_prompt_tokens": 0, "input": 2, "output": 3}]}"#,
        );
        assert!(PriceTable::from_json_str(&zero).is_err());
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let e = PriceTable::from_json_str(&one(r#"{"input": 1, "output": 2, "cache_reed": 0.1}"#))
            .unwrap_err();
        assert!(matches!(e, PriceError::Json(_)), "{e}");
        assert!(PriceTable::from_json_str(&one(r#"{"input": 1}"#)).is_err());
    }

    #[test]
    fn cache_write_at_input_is_validated() {
        let ok =
            one(r#"{"input": 5, "output": 30, "cache_write": 5, "cache_write_at_input": true}"#);
        assert!(PriceTable::from_json_str(&ok).is_ok());
        let bad =
            one(r#"{"input": 5, "output": 30, "cache_write": 6, "cache_write_at_input": true}"#);
        assert!(matches!(
            PriceTable::from_json_str(&bad).unwrap_err(),
            PriceError::InvalidEntry { .. }
        ));
        let tier_bad = one(
            r#"{"input": 5, "output": 30, "cache_write": 5, "cache_write_at_input": true,
                "tiers": [{"above_prompt_tokens": 10, "input": 10, "output": 45, "cache_write": 5}]}"#,
        );
        assert!(PriceTable::from_json_str(&tier_bad).is_err());
    }

    #[test]
    fn layered_replaces_per_model_and_keeps_the_rest() {
        let base = PriceTable::builtin();
        let over = PriceTable::from_json_str(
            r#"{"schema": 1, "providers": {
                "anthropic": {"claude-sonnet-5": {"input": 1.8, "output": 9}},
                "acme": {"rocket-1": {"input": 1, "output": 2}}}}"#,
        )
        .unwrap();
        let t = base.layered(&over);
        assert_eq!(
            t.cost("anthropic", "claude-sonnet-5")
                .unwrap()
                .input_per_million,
            1.8
        );
        // The override's entry replaces the whole entry, not single fields.
        assert_eq!(
            t.cost("anthropic", "claude-sonnet-5")
                .unwrap()
                .cache_read_per_million,
            0.0
        );
        assert_eq!(
            t.cost("anthropic", "claude-opus-5"),
            base.cost("anthropic", "claude-opus-5")
        );
        assert!(t.cost("acme", "rocket-1").is_some());
        assert_eq!(t.len(), base.len() + 1);
    }

    #[test]
    fn io_errors_name_the_path() {
        let e = PriceTable::from_path("/nonexistent/prices.json").unwrap_err();
        assert!(e.to_string().contains("/nonexistent/prices.json"), "{e}");
    }
}
