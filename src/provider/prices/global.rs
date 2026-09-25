//! The process-wide price tables constructors read, and the only functions
//! that change them.
//!
//! These are free functions in their own module, not methods on
//! [`PriceTable`], so a call reads as what it is — a change to process-wide
//! state — and never as an operation on a table value:
//!
//! ```
//! use yoagent::provider::prices::global;
//! # use yoagent::provider::{ModelConfig, PriceTable};
//! # global::clear_override(); // ignore a developer's YOAGENT_PRICES
//! let report = global::install_override(PriceTable::from_json_str(
//!     r#"{"schema": 1, "providers": {"anthropic": {"claude-sonnet-5":
//!         {"input": 1.8, "output": 9.0, "cache_read": 0.18, "cache_write": 2.25}}}}"#,
//! )?);
//! assert_eq!(report.changes.len(), 1);
//! let config = ModelConfig::claude_sonnet_5(); // built after the install
//! assert_eq!(config.cost.unwrap().input_per_million, 1.8);
//! # global::clear_override();
//! # Ok::<(), yoagent::provider::PriceError>(())
//! ```
//!
//! # Layers
//!
//! The **resolved table** is the built-in data, with the **fetched layer**
//! ([`install_fetched`]) over it and the **user layer** ([`install_override`],
//! or the `YOAGENT_PRICES` file) over that. Each layer replaces whole entries
//! per `(provider, id)`.
//!
//! Constructors read the resolved table **when they run**. Install prices
//! first and build configs afterwards, or re-price configs you already hold
//! with [`ModelConfig::reprice`](crate::provider::ModelConfig::reprice),
//! `Agent::reprice` or `SubAgentTool::reprice`.
//!
//! # What the install functions report
//!
//! Every install happens under one lock and returns what it changed, as
//! [`PriceChange`]s computed as billed (a zero cache rate counts as the input
//! rate):
//!
//! - [`install_override`] compares the resolved table before and after the
//!   call. Its [`OverrideReport`] splits that into the overridden models
//!   (`changes`) and models that revert because a previous override no
//!   longer lists them (`reverted`).
//! - [`install_fetched`] / [`install_fetched_with`] compare the built-in plus
//!   fetched layers before and after the call. A change to a model the user
//!   layer overrides is still reported, marked
//!   [`shadowed`](PriceChange::shadowed): it does not change what is billed.

use super::{builtin_ref, diff_tables, PriceChange, PriceError, PriceTable};
use super::{PRICED_PROVIDERS, PRICES_ENV_VAR};
use crate::provider::model::CostConfig;
use std::path::PathBuf;
use std::sync::{OnceLock, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// How many disagreements with the built-in data [`install_fetched_with`]
/// spells out in its log line; the rest are counted.
const LOGGED_CHANGES: usize = 5;

/// Which entries of a fetched table [`install_fetched_with`] installs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum InstallPolicy {
    /// Replace the fetched layer with the table: every entry overrides the
    /// built-in data where both list a model. What [`install_fetched`] does.
    /// Replacing a non-empty fetched layer is logged at `warn`.
    #[default]
    ReplaceAll,
    /// Merge into the current fetched layer only the models that no lower
    /// layer — the built-in data or the current fetched layer — lists:
    /// extend coverage, never override a price already in effect.
    AddOnly,
}

/// What became of `YOAGENT_PRICES` (see [`env_override_status`]).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum EnvOverride {
    /// The variable was unset or empty.
    Unset,
    /// The file loaded as the user layer.
    #[non_exhaustive]
    Loaded {
        path: PathBuf,
        entries: usize,
        /// The same warnings [`install_override`] reports (entries no
        /// constructor reads, dropped tiers, newly unset cache rates).
        warnings: Vec<String>,
    },
    /// The file was rejected: logged at `warn` and ignored, so no user layer
    /// was installed from it.
    #[non_exhaustive]
    Rejected { path: PathBuf, error: PriceError },
}

/// What [`install_override`] did.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
#[must_use = "the report is how you see what the override changed and what it warned about"]
pub struct OverrideReport {
    /// Models the new override prices differently from the resolved table
    /// before the call. Excludes `inert` entries.
    pub changes: Vec<PriceChange>,
    /// Models whose price changed because a previous user layer listed them
    /// and the new one does not: they revert to the fetched or built-in
    /// price, or become unpriced. Unlike `changes`, this can include
    /// entries for providers no constructor prices (an inert entry of the
    /// previous override); those never affected billing.
    pub reverted: Vec<PriceChange>,
    /// `provider/model` of entries whose provider is not in
    /// [`PRICED_PROVIDERS`]: no constructor reads
    /// them; only [`ModelConfig::with_prices`](crate::provider::ModelConfig::with_prices) does.
    pub inert: Vec<String>,
    /// Everything that was also logged at `warn`: inert entries, dropped
    /// tiers, cache rates left unset where the replaced entry set one, and
    /// replacing an existing user layer.
    pub warnings: Vec<String>,
}

/// The user layer and where it came from.
struct UserLayer {
    table: PriceTable,
    /// Set when it was loaded from `YOAGENT_PRICES`.
    from_env: Option<PathBuf>,
}

struct Layers {
    fetched: Option<PriceTable>,
    user: Option<UserLayer>,
    /// `builtin`, then `fetched`, then `user`, rebuilt on every change so a
    /// lookup is one map read.
    resolved: PriceTable,
}

impl Layers {
    /// Everything below the user layer.
    fn lower(&self) -> PriceTable {
        match &self.fetched {
            Some(fetched) => builtin_ref().layered(fetched),
            None => builtin_ref().clone(),
        }
    }

    fn rebuild(&mut self) {
        let lower = self.lower();
        self.resolved = match &self.user {
            Some(user) => lower.layered(&user.table),
            None => lower,
        };
    }
}

static ENV_STATUS: OnceLock<EnvOverride> = OnceLock::new();

fn layers() -> &'static RwLock<Layers> {
    static LAYERS: OnceLock<RwLock<Layers>> = OnceLock::new();
    LAYERS.get_or_init(|| {
        let (user, status) = env_layer();
        let _ = ENV_STATUS.set(status);
        let mut layers = Layers {
            fetched: None,
            user,
            resolved: PriceTable::default(),
        };
        layers.rebuild();
        RwLock::new(layers)
    })
}

fn read_layers() -> RwLockReadGuard<'static, Layers> {
    // A panic while holding the lock cannot leave `Layers` half-written
    // (every write replaces whole fields), so a poisoned lock is still valid.
    layers().read().unwrap_or_else(PoisonError::into_inner)
}

fn write_layers() -> RwLockWriteGuard<'static, Layers> {
    layers().write().unwrap_or_else(PoisonError::into_inner)
}

/// The `YOAGENT_PRICES` path the layer initialiser uses. Never read in this
/// crate's own unit tests, so a developer's override cannot change what they
/// assert (integration tests run with the real variable).
fn env_path() -> Option<PathBuf> {
    #[cfg(test)]
    {
        None
    }
    #[cfg(not(test))]
    {
        std::env::var_os(PRICES_ENV_VAR)
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
    }
}

/// The user layer named by `YOAGENT_PRICES`, and its status. A bad file is
/// logged and ignored — a typo in an environment variable must not take down
/// every constructor in the process — and reported through
/// [`env_override_status`].
fn env_layer() -> (Option<UserLayer>, EnvOverride) {
    let Some(path) = env_path() else {
        return (None, EnvOverride::Unset);
    };
    match PriceTable::from_path(&path) {
        Ok(table) => {
            let warnings = override_warnings(&table, builtin_ref());
            tracing::info!(
                path = %path.display(),
                entries = table.len(),
                "yoagent prices: loaded {PRICES_ENV_VAR} override"
            );
            for warning in &warnings {
                tracing::warn!("yoagent prices: {PRICES_ENV_VAR}: {warning}");
            }
            let status = EnvOverride::Loaded {
                path: path.clone(),
                entries: table.len(),
                warnings,
            };
            let layer = UserLayer {
                table,
                from_env: Some(path),
            };
            (Some(layer), status)
        }
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "yoagent prices: ignoring {PRICES_ENV_VAR}; no user layer installed"
            );
            (None, EnvOverride::Rejected { path, error })
        }
    }
}

/// The mistakes an override can make silently, as messages.
fn override_warnings(table: &PriceTable, lower: &PriceTable) -> Vec<String> {
    let mut out = Vec::new();
    let inert = inert_entries(table);
    if !inert.is_empty() {
        out.push(format!(
            "{inert:?} name provider(s) no constructor looks up — only \
             ModelConfig::with_prices reads them (constructors look up {PRICED_PROVIDERS:?})"
        ));
    }
    for (provider, model, entry) in table.iter() {
        let Some(old) = lower.entry(provider, model) else {
            continue;
        };
        let (old, new): (&CostConfig, &CostConfig) = (&old.cost, &entry.cost);
        if !old.context_tiers.is_empty() && new.context_tiers.is_empty() {
            out.push(format!(
                "{provider}/{model} drops the {} context tier(s) of the entry it replaces; \
                 every request now bills at the base rates",
                old.context_tiers.len()
            ));
        }
        for (name, was, now) in [
            (
                "cache_read",
                old.cache_read_per_million,
                new.cache_read_per_million,
            ),
            (
                "cache_write",
                old.cache_write_per_million,
                new.cache_write_per_million,
            ),
        ] {
            if was != 0.0 && now == 0.0 {
                out.push(format!(
                    "{provider}/{model} leaves {name} unset, so it bills at the input rate \
                     ({}); the entry it replaces set {was}",
                    new.input_per_million
                ));
            }
        }
    }
    out
}

fn inert_entries(table: &PriceTable) -> Vec<String> {
    table
        .iter()
        .filter(|(p, _, _)| !PRICED_PROVIDERS.contains(p))
        .map(|(p, m, _)| format!("{p}/{m}"))
        .collect()
}

/// Install `table` as the **user layer**, replacing any previous one
/// (including one loaded from `YOAGENT_PRICES`, which is not re-read).
///
/// Its entries take precedence over the fetched layer and the built-in data
/// per `(provider, id)`; models it does not list keep their lower-layer
/// price. Affects configs built **after** this call.
///
/// The whole operation holds one lock. The [`OverrideReport`] compares the
/// resolved table before and after it. An entry replaces the lower entry
/// **whole**, so the mistakes that makes easy are reported in
/// [`OverrideReport::warnings`] and logged at `warn`: see there.
pub fn install_override(table: PriceTable) -> OverrideReport {
    let mut layers = write_layers();
    let lower = layers.lower();
    let mut warnings = override_warnings(&table, &lower);
    let inert = inert_entries(&table);
    if let Some(old) = &layers.user {
        warnings.push(match &old.from_env {
            Some(path) => format!(
                "replaces the user layer loaded from {PRICES_ENV_VAR} ({}, {} entries)",
                path.display(),
                old.table.len()
            ),
            None => format!(
                "replaces a previously installed override of {} entries",
                old.table.len()
            ),
        });
    }
    let before = layers.resolved.clone();
    layers.user = Some(UserLayer {
        table,
        from_env: None,
    });
    layers.rebuild();
    let diff = diff_tables(&before, &layers.resolved);
    let (mut changes, mut reverted) = (Vec::new(), Vec::new());
    let user = &layers.user.as_ref().expect("just installed").table;
    for change in diff {
        let listed = user.entry(&change.provider, &change.model).is_some();
        if !listed {
            reverted.push(change);
        } else if PRICED_PROVIDERS.contains(&change.provider.as_str()) {
            changes.push(change);
        }
    }
    drop(layers);
    for warning in &warnings {
        tracing::warn!("yoagent prices: override: {warning}");
    }
    OverrideReport {
        changes,
        reverted,
        inert,
        warnings,
    }
}

/// Remove the user layer, including one loaded from `YOAGENT_PRICES` (which
/// is not re-read).
pub fn clear_override() {
    let mut layers = write_layers();
    layers.user = None;
    layers.rebuild();
}

/// Install `table` as the **fetched layer**, replacing the current one:
/// [`install_fetched_with`] with [`InstallPolicy::ReplaceAll`].
///
/// Returns the changes to the built-in-plus-fetched table (see the
/// [module docs](self)). Separately, every built-in model the new layer
/// prices differently from the built-in data is logged at `warn` — the count
/// and the first few — so trusting a fetched source is never silent.
///
/// Affects configs built **after** this call: fetch and install first, then
/// build configs, or [`reprice`](crate::provider::ModelConfig::reprice)
/// existing ones.
#[must_use = "the returned changes are how you see where the fetched prices differ"]
pub fn install_fetched(table: PriceTable) -> Vec<PriceChange> {
    install_fetched_with(table, InstallPolicy::ReplaceAll)
}

/// [`install_fetched`] under an explicit [`InstallPolicy`].
///
/// ```no_run
/// # use yoagent::provider::{PriceSource, PriceTable};
/// # use yoagent::provider::prices::global::{self, InstallPolicy};
/// # async fn run() -> Result<(), yoagent::provider::PriceError> {
/// // Extend coverage with models.dev without overriding any price in effect.
/// let fetched = PriceTable::fetch(&PriceSource::ModelsDev).await?;
/// let added = global::install_fetched_with(fetched, InstallPolicy::AddOnly);
/// eprintln!("models.dev priced {} more models", added.len());
/// # Ok(()) }
/// ```
#[must_use = "the returned changes are how you see where the fetched prices differ"]
pub fn install_fetched_with(table: PriceTable, policy: InstallPolicy) -> Vec<PriceChange> {
    let mut warnings = Vec::new();
    let mut layers = write_layers();
    let before = layers.lower();
    let fetched = match policy {
        InstallPolicy::ReplaceAll => {
            if let Some(old) = layers.fetched.as_ref().filter(|t| !t.is_empty()) {
                warnings.push(format!(
                    "replaces a fetched layer of {} entries; models only it listed revert",
                    old.len()
                ));
            }
            table
        }
        InstallPolicy::AddOnly => {
            let mut merged = layers.fetched.clone().unwrap_or_default();
            for (provider, model, entry) in table.iter() {
                if before.entry(provider, model).is_none() {
                    if let Err(e) = merged.insert(provider, model, entry.clone()) {
                        warnings.push(format!("skipped {provider}/{model}: {e}"));
                    }
                }
            }
            merged
        }
    };
    let disagreements: Vec<PriceChange> = fetched
        .changes_from(builtin_ref())
        .into_iter()
        .filter(|c| c.before.is_some())
        .collect();
    layers.fetched = Some(fetched);
    layers.rebuild();
    let mut changes = diff_tables(&before, &layers.lower());
    if let Some(user) = &layers.user {
        for change in &mut changes {
            change.shadowed = user.table.entry(&change.provider, &change.model).is_some();
        }
    }
    drop(layers);

    for warning in &warnings {
        tracing::warn!("yoagent prices: install_fetched: {warning}");
    }
    if !disagreements.is_empty() {
        let first: Vec<String> = disagreements
            .iter()
            .take(LOGGED_CHANGES)
            .map(ToString::to_string)
            .collect();
        tracing::warn!(
            differing = disagreements.len(),
            "yoagent prices: the fetched table disagrees with the built-in data on {} \
             model(s) and takes precedence for them: {}{}",
            disagreements.len(),
            first.join("; "),
            if disagreements.len() > LOGGED_CHANGES {
                format!("; and {} more", disagreements.len() - LOGGED_CHANGES)
            } else {
                String::new()
            }
        );
    }
    tracing::info!(
        changed = changes.len(),
        ?policy,
        "yoagent prices: installed fetched prices"
    );
    changes
}

/// Remove the fetched layer.
pub fn clear_fetched() {
    let mut layers = write_layers();
    layers.fetched = None;
    layers.rebuild();
}

/// A snapshot of the resolved table — what a constructor called now would
/// use.
pub fn resolved() -> PriceTable {
    read_layers().resolved.clone()
}

/// What became of `YOAGENT_PRICES` when the process-wide table was first
/// used (calling this uses it, reading the variable if nothing has yet).
///
/// A host that would rather fail than run on prices it did not ask for
/// checks this at startup, or calls [`load_env_override`] itself.
pub fn env_override_status() -> EnvOverride {
    let _ = layers();
    ENV_STATUS.get().cloned().unwrap_or(EnvOverride::Unset)
}

/// Read and strictly parse the file named by `YOAGENT_PRICES` now, without
/// installing it: `Ok(None)` when the variable is unset or empty. For a host
/// that wants to fail fast on a bad file rather than have it logged and
/// ignored.
pub fn load_env_override() -> Result<Option<PriceTable>, PriceError> {
    match std::env::var_os(PRICES_ENV_VAR) {
        Some(path) if !path.is_empty() => PriceTable::from_path(path).map(Some),
        _ => Ok(None),
    }
}

/// The rates a first-party constructor gets for `(provider, id)`.
pub(crate) fn resolved_cost(provider: &str, id: &str) -> Option<CostConfig> {
    read_layers().resolved.cost(provider, id)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// This crate's unit tests never read `YOAGENT_PRICES` (no environment
    /// is mutated here: the integration binaries `price_env*_test` are the
    /// positive controls that it is read outside unit tests).
    #[test]
    fn unit_tests_do_not_read_the_env_override() {
        assert!(env_path().is_none());
        assert!(matches!(env_override_status(), EnvOverride::Unset));
    }
}
