//! Model prices as data: the built-in `prices.json`, [`PriceTable`], runtime
//! overrides ([`global`]) and opt-in live sources ([`PriceSource`]).
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
//! metadata (`note`, `source`, `verified` as `YYYY-MM-DD`,
//! `cache_write_at_input`, `absent_upstream`). A cache rate that is omitted or
//! `0` bills at the band's input rate, as for [`CostConfig`].
//!
//! # Format evolution
//!
//! - A field that **changes what is billed** (a new rate, a new kind of tier)
//!   must bump `schema`. An older release then rejects the file with
//!   [`PriceError::UnsupportedSchema`] instead of billing without the field.
//! - A **metadata-only** field (like `note`) must **not** bump `schema`, so
//!   older releases keep reading this crate's published file. They parse
//!   [`PriceSource::YoagentMain`] and the [`fetch_cached`](PriceTable::fetch_cached)
//!   cache leniently: unknown fields are ignored, logged at `warn` and
//!   returned to the caller. Everything else — [`PriceTable::from_json_str`],
//!   [`PriceTable::from_path`], `YOAGENT_PRICES`, [`PriceSource::Url`] — is
//!   usually maintained by hand and is strict: an unknown field is
//!   [`PriceError::UnknownField`].
//!
//! A test pins the field set to [`PRICE_SCHEMA_VERSION`], so changing it
//! without deciding which of the two it is fails CI.
//!
//! # Which constructors look prices up
//!
//! The first-party constructors whose provider is in [`PRICED_PROVIDERS`]
//! ([`ModelConfig::anthropic`], [`ModelConfig::openai`],
//! [`ModelConfig::google`], …, and the named presets built on them) look
//! their `(provider, id)` up when they build a config and get `Some` for a
//! listed model, `None` otherwise. Gateways and custom endpoints
//! ([`ModelConfig::custom`], [`ModelConfig::openai_compat`],
//! [`ModelConfig::local`], [`ModelConfig::ollama`], the OpenCode gateways)
//! never look up: what they bill is not the vendor's list price.
//!
//! # Runtime overrides and precedence
//!
//! Constructors read a process-wide **resolved table** ([`global`]), built in
//! layers, highest first:
//!
//! 1. the **user layer**: [`global::install_override`], or on first use the
//!    file named by the `YOAGENT_PRICES` environment variable;
//! 2. the **fetched layer**: [`global::install_fetched`], opt-in, from a live
//!    [`PriceSource`] (see its trust caveats) — nothing is fetched unless you
//!    call [`PriceTable::fetch`] or [`PriceTable::fetch_cached`];
//! 3. the built-in `prices.json`.
//!
//! Each layer replaces whole entries per `(provider, id)`. **Constructors
//! resolve when they run**, so install prices before building configs, or
//! re-price existing ones with [`ModelConfig::reprice`] (or
//! `Agent::reprice` / `SubAgentTool::reprice`).
//!
//! A `cost` you set on a config yourself wins over every process-wide layer,
//! because constructors are the only thing that read them — but
//! [`ModelConfig::reprice`] and [`ModelConfig::with_prices`] overwrite it.
//!
//! See `docs/concepts/pricing.md` for the full guide.
//!
//! [`ModelConfig::reprice`]: crate::provider::ModelConfig::reprice
//! [`ModelConfig::with_prices`]: crate::provider::ModelConfig::with_prices
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
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

mod fetch;
pub mod global;
pub use fetch::{
    CacheOptions, CacheProblem, CachedPrices, FetchOptions, FetchReport, PriceOrigin, PriceSource,
    SkippedModel, DEFAULT_FETCH_TIMEOUT,
};

/// The only `schema` value this release reads.
pub const PRICE_SCHEMA_VERSION: u32 = 1;

/// Environment variable naming a price file for the user layer, read once
/// when the process-wide table is first used. Unset or empty means no file.
pub const PRICES_ENV_VAR: &str = "YOAGENT_PRICES";

/// The `ModelConfig::provider` values whose constructors look prices up:
/// `anthropic`, `openai` (set by both the `openai` and `openai_responses`
/// constructors), `google`, `xai`, `groq`, `deepseek`, `mistral`, `zai`,
/// `minimax`, `qwen`, `meta`. Entries for any other provider are read only by
/// [`ModelConfig::with_prices`](crate::provider::ModelConfig::with_prices).
pub const PRICED_PROVIDERS: &[&str] = &[
    "anthropic",
    "openai",
    "google",
    "xai",
    "groq",
    "deepseek",
    "mistral",
    "zai",
    "minimax",
    "qwen",
    "meta",
];

/// The built-in data, embedded at compile time.
const BUILTIN_JSON: &str = include_str!("prices.json");

/// Why price data was rejected, or could not be fetched.
///
/// `Clone`, so one error can be both logged and returned; the underlying I/O
/// and HTTP errors are shared behind `Arc`.
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum PriceError {
    /// Malformed JSON, a missing required field (`input`, `output`,
    /// `above_prompt_tokens`), or a value of the wrong type. `origin` is the
    /// file path or URL, when there is one.
    #[error(
        "invalid price JSON{}: {message}",
        .origin.as_deref().map(|o| format!(" from {o}")).unwrap_or_default()
    )]
    #[non_exhaustive]
    Json {
        origin: Option<String>,
        message: String,
    },
    /// A price file (or cache file) could not be read or written.
    #[error("price file {}: {source}", .path.display())]
    #[non_exhaustive]
    Io {
        path: PathBuf,
        #[source]
        source: Arc<std::io::Error>,
    },
    /// `schema` is missing (`found: None`) or a version this release does
    /// not read.
    #[error(
        "unsupported price schema {}; this yoagent reads schema {PRICE_SCHEMA_VERSION}",
        .found.as_deref().unwrap_or("(missing)")
    )]
    #[non_exhaustive]
    UnsupportedSchema { found: Option<String> },
    /// A field this release does not know, in input parsed strictly. Either
    /// a typo, or data written for a newer yoagent that added a field —
    /// upgrade, or remove the field.
    #[error(
        "unknown price field `{field}`: a typo, or data written for a newer yoagent \
         (upgrade yoagent, or remove the field)"
    )]
    #[non_exhaustive]
    UnknownField {
        /// Dotted path of the first unknown field, e.g.
        /// `providers.anthropic.claude-opus-5.cache_reed`.
        field: String,
    },
    /// A rate is negative or not finite.
    #[error("{provider}/{model}: {field} is {value}; rates must be finite and non-negative")]
    #[non_exhaustive]
    InvalidRate {
        provider: String,
        model: String,
        field: String,
        value: f64,
    },
    /// Context tier thresholds are not strictly ascending.
    #[error("{provider}/{model}: tier thresholds must be strictly ascending, got {thresholds:?}")]
    #[non_exhaustive]
    TiersNotAscending {
        provider: String,
        model: String,
        thresholds: Vec<u64>,
    },
    /// Any other inconsistency inside one entry (an empty provider or model
    /// id, a malformed `verified` date, a tier at 0, a
    /// `cache_write_at_input` flag the rates contradict).
    #[error("{provider}/{model}: {reason}")]
    #[non_exhaustive]
    InvalidEntry {
        provider: String,
        model: String,
        reason: String,
    },
    /// A price source answered with a non-success HTTP status.
    #[error("GET {url} returned HTTP {status}")]
    #[non_exhaustive]
    Http { url: String, status: u16 },
    /// A price source did not answer within the timeout.
    #[error("GET {url} timed out after {timeout:?}")]
    #[non_exhaustive]
    Timeout {
        url: String,
        timeout: std::time::Duration,
    },
    /// A price source could not be reached, or its body not read.
    #[error("GET {url} failed: {source}")]
    #[non_exhaustive]
    Request {
        url: String,
        /// The transport error, opaque so no HTTP client type is part of
        /// this crate's API.
        #[source]
        source: Arc<dyn std::error::Error + Send + Sync>,
    },
    /// models.dev data this crate cannot map — its envelope changed, or no
    /// model in it carried a usable price.
    #[error("cannot map models.dev data from {url}: {reason}")]
    #[non_exhaustive]
    ModelsDev { url: String, reason: String },
}

impl PriceError {
    fn json(origin: Option<&str>, e: serde_json::Error) -> Self {
        PriceError::Json {
            origin: origin.map(str::to_string),
            message: e.to_string(),
        }
    }
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
    /// The date (`YYYY-MM-DD`, validated) the rates were last checked
    /// against `source`.
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

    /// Record why models.dev lacks this model, and when that was checked.
    pub fn with_absent_upstream(mut self, why: impl Into<String>) -> Self {
        self.absent_upstream = Some(why.into());
        self
    }

    /// Check this entry as it would be checked when stored under
    /// `(provider, model)`: non-empty ids, finite non-negative rates, tier
    /// thresholds above 0 and strictly ascending, a `YYYY-MM-DD` `verified`
    /// date, and a `cache_write_at_input` flag the rates agree with.
    /// [`PriceTable::insert`] and every parser call this.
    pub fn validate(&self, provider: &str, model: &str) -> Result<(), PriceError> {
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
        if let Some(date) = &self.verified {
            if !is_iso_date(date) {
                return Err(entry_err(format!(
                    "verified is {date:?}; expected a YYYY-MM-DD date"
                )));
            }
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

/// `YYYY-MM-DD` with a plausible month and day.
fn is_iso_date(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return false;
    }
    let digits = |r: std::ops::Range<usize>| {
        b[r.clone()]
            .iter()
            .all(u8::is_ascii_digit)
            .then(|| s[r].parse::<u32>().ok())
            .flatten()
    };
    matches!(
        (digits(0..4), digits(5..7), digits(8..10)),
        (Some(_), Some(1..=12), Some(1..=31))
    )
}

/// Prices keyed by provider, then model id.
///
/// Parse one with [`from_json_str`](Self::from_json_str) or
/// [`from_path`](Self::from_path), start from [`builtin`](Self::builtin), and
/// combine with [`layered`](Self::layered). Every way in goes through
/// [`insert`](Self::insert), which calls [`PriceEntry::validate`]; `schema`
/// must be [`PRICE_SCHEMA_VERSION`]. [`from_json_str`](Self::from_json_str)
/// and [`from_path`](Self::from_path) reject unknown fields as
/// [`PriceError::UnknownField`], so a misspelled `"cache_reed"` fails loudly
/// instead of billing at the input rate.
///
/// A `PriceTable` is plain data. The process-wide tables constructors read
/// are managed by [`global`].
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

    /// Parse and validate a table in this crate's JSON format, **strictly**:
    /// an unknown field is [`PriceError::UnknownField`].
    pub fn from_json_str(json: &str) -> Result<PriceTable, PriceError> {
        Self::parse(json, Strictness::Strict, None).map(|(t, _)| t)
    }

    /// Read, strictly parse and validate a table file. Errors name the path.
    pub fn from_path(path: impl AsRef<Path>) -> Result<PriceTable, PriceError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|source| PriceError::Io {
            path: path.to_path_buf(),
            source: Arc::new(source),
        })?;
        let origin = path.display().to_string();
        Self::parse(&text, Strictness::Strict, Some(&origin)).map(|(t, _)| t)
    }

    /// Parse `json`; with [`Strictness::Lenient`], also return the dotted
    /// paths of the fields it ignored.
    pub(crate) fn parse(
        json: &str,
        strictness: Strictness,
        origin: Option<&str>,
    ) -> Result<(PriceTable, Vec<String>), PriceError> {
        let value: serde_json::Value =
            serde_json::from_str(json).map_err(|e| PriceError::json(origin, e))?;
        Self::parse_value(value, strictness, origin)
    }

    pub(crate) fn parse_value(
        value: serde_json::Value,
        strictness: Strictness,
        origin: Option<&str>,
    ) -> Result<(PriceTable, Vec<String>), PriceError> {
        match value.get("schema") {
            Some(v) if v.as_u64() == Some(u64::from(PRICE_SCHEMA_VERSION)) => {}
            found => {
                return Err(PriceError::UnsupportedSchema {
                    found: found.map(|v| v.to_string()),
                })
            }
        }
        let unknown = unknown_fields(&value);
        if let (Strictness::Strict, Some(first)) = (strictness, unknown.first()) {
            return Err(PriceError::UnknownField {
                field: first.clone(),
            });
        }
        if !unknown.is_empty() {
            tracing::warn!(
                origin = origin.unwrap_or("<string>"),
                ?unknown,
                "yoagent prices: ignoring fields this release does not know \
                 (metadata a newer yoagent added?)"
            );
        }
        let wire: FileWire =
            serde_json::from_value(value).map_err(|e| PriceError::json(origin, e))?;
        let mut table = PriceTable {
            entries: BTreeMap::new(),
            comment: wire.comment,
        };
        for (provider, models) in wire.providers {
            for (model, entry) in models {
                table.insert(&provider, &model, entry.into())?;
            }
        }
        Ok((table, unknown))
    }

    /// Serialize to this crate's JSON format (pretty-printed, sorted), which
    /// [`from_json_str`](Self::from_json_str) reads back to an equal table.
    pub fn to_json(&self) -> String {
        // Only strings, finite numbers and maps: serialization cannot fail.
        serde_json::to_string_pretty(&self.to_wire()).expect("a validated price table serializes")
    }

    fn to_wire(&self) -> FileWire {
        FileWire {
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
        }
    }

    /// Add or replace one model's entry, validating it with
    /// [`PriceEntry::validate`]. Returns the entry it replaced. The one way
    /// entries get into a table.
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

    /// Every model in `self` whose billed rates differ from `base`, or that
    /// `base` does not list. Models only `base` lists are not reported (use
    /// this to preview what layering `self` over `base` would change).
    /// Nothing is installed.
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
                    after: Some(entry.cost.clone()),
                    shadowed: false,
                })
            })
            .collect()
    }
}

/// Every model whose billed price differs between `before` and `after`,
/// including models only one of them lists.
pub(crate) fn diff_tables(before: &PriceTable, after: &PriceTable) -> Vec<PriceChange> {
    let keys: BTreeSet<(&str, &str)> = before
        .iter()
        .chain(after.iter())
        .map(|(p, m, _)| (p, m))
        .collect();
    keys.into_iter()
        .filter_map(|(p, m)| {
            let (b, a) = (before.cost(p, m), after.cost(p, m));
            let same = match (&b, &a) {
                (Some(b), Some(a)) => effective(b) == effective(a),
                (None, None) => true,
                _ => false,
            };
            (!same).then(|| PriceChange {
                provider: p.to_string(),
                model: m.to_string(),
                before: b,
                after: a,
                shadowed: false,
            })
        })
        .collect()
}

/// One model whose price differs between two states (see
/// [`PriceTable::changes_from`] and the [`global`] install functions).
///
/// Rates are compared as billed: a cache rate of `0` counts as the band's
/// input rate, so writing a no-premium cache rate out explicitly is not a
/// change.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct PriceChange {
    pub provider: String,
    pub model: String,
    /// The earlier rates; `None` when the model was not priced.
    pub before: Option<CostConfig>,
    /// The new rates; `None` when the model is no longer priced.
    pub after: Option<CostConfig>,
    /// The change happened in a layer the user layer overrides for this
    /// model, so it does not affect what constructors bill (yet: it will if
    /// the override is cleared).
    pub shadowed: bool,
}

impl std::fmt::Display for PriceChange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}: ", self.provider, self.model)?;
        match (&self.before, &self.after) {
            (None, None) => write!(f, "unpriced")?,
            (None, Some(after)) => write!(f, "new ({})", describe(after))?,
            (Some(before), None) => write!(f, "no longer priced (was {})", describe(before))?,
            (Some(before), Some(after)) => {
                let (b, a) = (effective(before), effective(after));
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
                        describe_tiers(&after.context_tiers)
                    ));
                }
                write!(f, "{}", parts.join(", "))?;
            }
        }
        if self.shadowed {
            write!(f, " [shadowed by the user layer]")?;
        }
        Ok(())
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

/// The parsed built-in table. The data is checked by this crate's tests, so
/// a parse failure is a build defect, not a runtime condition.
fn builtin_ref() -> &'static PriceTable {
    static BUILTIN: OnceLock<PriceTable> = OnceLock::new();
    BUILTIN.get_or_init(|| {
        PriceTable::from_json_str(BUILTIN_JSON)
            .unwrap_or_else(|e| panic!("yoagent's built-in prices.json is invalid: {e}"))
    })
}

// ---- wire format ---------------------------------------------------------

fn is_zero(v: &f64) -> bool {
    *v == 0.0
}

fn is_false(v: &bool) -> bool {
    !*v
}

/// How a parse treats a field it does not know.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Strictness {
    /// [`PriceError::UnknownField`]: input people maintain by hand.
    Strict,
    /// Ignored, logged and returned: this crate's published file and caches
    /// of it, which a newer release may have written.
    Lenient,
}

/// The field names of schema [`PRICE_SCHEMA_VERSION`]. Changing any of these
/// sets is a format change; `wire_fields_are_pinned_to_the_schema_version`
/// makes it a deliberate one (see the module docs for when it needs a bump).
const FILE_FIELDS: &[&str] = &["schema", "comment", "providers"];
const ENTRY_FIELDS: &[&str] = &[
    "input",
    "output",
    "cache_read",
    "cache_write",
    "tiers",
    "cache_write_at_input",
    "note",
    "source",
    "verified",
    "absent_upstream",
];
const TIER_FIELDS: &[&str] = &[
    "above_prompt_tokens",
    "input",
    "output",
    "cache_read",
    "cache_write",
];

/// Dotted paths of every field in `value` that the schema does not define.
/// Shapes that are not objects are left to serde to reject.
fn unknown_fields(value: &serde_json::Value) -> Vec<String> {
    fn extra(
        obj: &serde_json::Map<String, serde_json::Value>,
        known: &[&str],
        at: &str,
        out: &mut Vec<String>,
    ) {
        for key in obj.keys() {
            if !known.contains(&key.as_str()) {
                out.push(if at.is_empty() {
                    key.clone()
                } else {
                    format!("{at}.{key}")
                });
            }
        }
    }
    let mut out = Vec::new();
    let Some(top) = value.as_object() else {
        return out;
    };
    extra(top, FILE_FIELDS, "", &mut out);
    let providers = top.get("providers").and_then(|p| p.as_object());
    for (provider, models) in providers.into_iter().flatten() {
        for (model, entry) in models.as_object().into_iter().flatten() {
            let Some(entry) = entry.as_object() else {
                continue;
            };
            let at = format!("providers.{provider}.{model}");
            extra(entry, ENTRY_FIELDS, &at, &mut out);
            let tiers = entry.get("tiers").and_then(|t| t.as_array());
            for (i, tier) in tiers.into_iter().flatten().enumerate() {
                if let Some(tier) = tier.as_object() {
                    extra(tier, TIER_FIELDS, &format!("{at}.tiers[{i}]"), &mut out);
                }
            }
        }
    }
    out
}

#[derive(Serialize, Deserialize)]
struct FileWire {
    schema: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    comment: Option<String>,
    #[serde(default)]
    providers: BTreeMap<String, BTreeMap<String, EntryWire>>,
}

#[derive(Serialize, Deserialize)]
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

    fn lenient(json: &str) -> Result<(PriceTable, Vec<String>), PriceError> {
        PriceTable::parse(json, Strictness::Lenient, None)
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
        assert!(
            matches!(e, PriceError::UnsupportedSchema { ref found } if found.as_deref() == Some("2")),
            "{e}"
        );
        let e = PriceTable::from_json_str(r#"{"providers": {}}"#).unwrap_err();
        assert!(
            matches!(e, PriceError::UnsupportedSchema { found: None }),
            "{e}"
        );
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
    fn empty_ids_are_invalid_entries() {
        let entry = PriceEntry::new(CostConfig::new(1.0, 2.0));
        for (p, m) in [("", "m"), ("p", "")] {
            assert!(matches!(
                PriceTable::new().insert(p, m, entry.clone()).unwrap_err(),
                PriceError::InvalidEntry { .. }
            ));
        }
        let e = PriceTable::from_json_str(
            r#"{"schema": 1, "providers": {"p": {"": {"input": 1, "output": 2}}}}"#,
        )
        .unwrap_err();
        assert!(matches!(e, PriceError::InvalidEntry { .. }), "{e}");
        // Positive control.
        assert!(PriceTable::new().insert("p", "m", entry).is_ok());
    }

    #[test]
    fn verified_must_be_an_iso_date() {
        for bad in [
            "2026-9-25",
            "25/09/2026",
            "2026-13-01",
            "2026-09-32",
            "yesterday",
            "2026-09-2x",
        ] {
            let e = PriceTable::from_json_str(&one(&format!(
                r#"{{"input": 1, "output": 2, "verified": "{bad}"}}"#
            )))
            .unwrap_err();
            assert!(matches!(e, PriceError::InvalidEntry { .. }), "{bad}: {e}");
        }
        assert!(PriceTable::from_json_str(&one(
            r#"{"input": 1, "output": 2, "verified": "2026-09-25"}"#
        ))
        .is_ok());
        let entry = PriceEntry::new(CostConfig::new(1.0, 2.0)).with_verified("soon");
        assert!(entry.validate("p", "m").is_err());
        assert!(entry.with_verified("2026-01-31").validate("p", "m").is_ok());
    }

    #[test]
    fn unknown_fields_are_rejected_strictly() {
        let e = PriceTable::from_json_str(&one(r#"{"input": 1, "output": 2, "cache_reed": 0.1}"#))
            .unwrap_err();
        assert!(
            matches!(e, PriceError::UnknownField { ref field } if field == "providers.p.m.cache_reed"),
            "{e}"
        );
        // At every level: top, entry, tier.
        let top = r#"{"schema": 1, "providers": {}, "generated": "x"}"#;
        assert!(matches!(
            PriceTable::from_json_str(top).unwrap_err(),
            PriceError::UnknownField { .. }
        ));
        let tier = one(
            r#"{"input": 1, "output": 2, "tiers": [{"above_prompt_tokens": 9, "input": 2, "output": 3, "kind": "x"}]}"#,
        );
        assert!(matches!(
            PriceTable::from_json_str(&tier).unwrap_err(),
            PriceError::UnknownField { ref field } if field.ends_with("tiers[0].kind")
        ));
        // A missing required field is corrupt data, not an unknown field.
        assert!(matches!(
            PriceTable::from_json_str(&one(r#"{"input": 1}"#)).unwrap_err(),
            PriceError::Json { .. }
        ));
    }

    #[test]
    fn unknown_fields_are_returned_leniently() {
        let json = one(r#"{"input": 1, "output": 2, "deprecated": true}"#);
        // Positive control: strict rejects exactly this document.
        assert!(PriceTable::from_json_str(&json).is_err());
        let (t, ignored) = lenient(&json).unwrap();
        assert_eq!(t.cost("p", "m"), Some(CostConfig::new(1.0, 2.0)));
        assert_eq!(ignored, ["providers.p.m.deprecated"]);
        // Lenient still enforces the schema version and validation.
        assert!(matches!(
            lenient(r#"{"schema": 2, "providers": {}}"#).unwrap_err(),
            PriceError::UnsupportedSchema { .. }
        ));
        assert!(lenient(&one(r#"{"input": -1, "output": 2}"#)).is_err());
    }

    /// The format contract: a field that changes billing bumps `schema`; a
    /// metadata-only field does not. Changing the field set therefore needs
    /// a decision, and this test forces it: add the new set under a new
    /// version (billing field — also bump `PRICE_SCHEMA_VERSION`), or extend
    /// the current version's set (metadata only — older releases ignore it
    /// in this crate's published file).
    #[test]
    fn wire_fields_are_pinned_to_the_schema_version() {
        type Fields = (
            &'static [&'static str],
            &'static [&'static str],
            &'static [&'static str],
        );
        let pinned: &[(u32, Fields)] = &[(
            1,
            (
                &["schema", "comment", "providers"],
                &[
                    "input",
                    "output",
                    "cache_read",
                    "cache_write",
                    "tiers",
                    "cache_write_at_input",
                    "note",
                    "source",
                    "verified",
                    "absent_upstream",
                ],
                &[
                    "above_prompt_tokens",
                    "input",
                    "output",
                    "cache_read",
                    "cache_write",
                ],
            ),
        )];
        let (_, (file, entry, tier)) = pinned
            .iter()
            .find(|(v, _)| *v == PRICE_SCHEMA_VERSION)
            .expect("pin the field set of the new PRICE_SCHEMA_VERSION here");
        let sorted = |s: &[&str]| {
            let mut v: Vec<String> = s.iter().map(|x| x.to_string()).collect();
            v.sort();
            v
        };
        assert_eq!(sorted(FILE_FIELDS), sorted(file));
        assert_eq!(sorted(ENTRY_FIELDS), sorted(entry));
        assert_eq!(sorted(TIER_FIELDS), sorted(tier));

        // The consts must be exactly what the wire structs read and write.
        // Built as struct literals with no `..`, so a field added to a wire
        // struct fails to compile here until it is populated — and then
        // fails the key comparison until it is in the consts.
        let wire = FileWire {
            schema: 1,
            comment: Some("c".into()),
            providers: BTreeMap::from([(
                "p".to_string(),
                BTreeMap::from([(
                    "m".to_string(),
                    EntryWire {
                        input: 1.0,
                        output: 2.0,
                        cache_read: 0.1,
                        cache_write: 1.0,
                        tiers: vec![TierWire {
                            above_prompt_tokens: 10,
                            input: 2.0,
                            output: 3.0,
                            cache_read: 0.2,
                            cache_write: 2.0,
                        }],
                        cache_write_at_input: true,
                        note: Some("n".into()),
                        source: Some("s".into()),
                        verified: Some("2026-01-01".into()),
                        absent_upstream: Some("a".into()),
                    },
                )]),
            )]),
        };
        let v = serde_json::to_value(&wire).unwrap();
        let keys = |v: &serde_json::Value| {
            let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
            k.sort();
            k
        };
        assert_eq!(keys(&v), sorted(FILE_FIELDS));
        let e = &v["providers"]["p"]["m"];
        assert_eq!(keys(e), sorted(ENTRY_FIELDS));
        assert_eq!(keys(&e["tiers"][0]), sorted(TIER_FIELDS));

        // And a document with every field parses strictly and round-trips
        // every one of them: nothing in the consts is dropped on the floor.
        let text = v.to_string();
        let table = PriceTable::from_json_str(&text).unwrap();
        assert_eq!(serde_json::to_value(table.to_wire()).unwrap(), v);
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
        let sonnet = t.cost("anthropic", "claude-sonnet-5").unwrap();
        assert_eq!(sonnet.input_per_million, 1.8);
        // The override's entry replaces the whole entry, not single fields.
        assert_eq!(sonnet.cache_read_per_million, 0.0);
        assert_eq!(
            t.cost("anthropic", "claude-opus-5"),
            base.cost("anthropic", "claude-opus-5")
        );
        assert!(t.cost("acme", "rocket-1").is_some());
        assert_eq!(t.len(), base.len() + 1);
    }

    #[test]
    fn diff_reports_added_removed_and_changed() {
        let a = PriceTable::from_json_str(
            r#"{"schema": 1, "providers": {"p": {
                "same": {"input": 1, "output": 2, "cache_write": 1},
                "changed": {"input": 1, "output": 2},
                "gone": {"input": 1, "output": 2}}}}"#,
        )
        .unwrap();
        let b = PriceTable::from_json_str(
            r#"{"schema": 1, "providers": {"p": {
                "same": {"input": 1, "output": 2},
                "changed": {"input": 3, "output": 2},
                "new": {"input": 1, "output": 2}}}}"#,
        )
        .unwrap();
        let d = diff_tables(&a, &b);
        let shown: Vec<String> = d.iter().map(ToString::to_string).collect();
        assert_eq!(d.len(), 3, "{shown:?}");
        assert!(
            shown.contains(
                &"p/changed: input 1 -> 3, cache_read 1 -> 3, cache_write 1 -> 3".to_string()
            ),
            "{shown:?}"
        );
        assert!(shown
            .iter()
            .any(|s| s.starts_with("p/gone: no longer priced")));
        assert!(shown.iter().any(|s| s.starts_with("p/new: new")));
    }

    #[test]
    fn io_and_json_errors_name_their_origin() {
        let e = PriceTable::from_path("/nonexistent/prices.json").unwrap_err();
        assert!(matches!(e, PriceError::Io { .. }));
        assert!(e.to_string().contains("/nonexistent/prices.json"), "{e}");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, "{not json").unwrap();
        let e = PriceTable::from_path(&path).unwrap_err();
        assert!(matches!(e, PriceError::Json { .. }));
        assert!(e.to_string().contains("bad.json"), "{e}");
        // Errors are Clone.
        let copy = e.clone();
        assert_eq!(copy.to_string(), e.to_string());
    }
}
