//! Waiting for plugin rows to run, with what keeps a row waiting.
//!
//! The bridge never loads plugins; a host does, with
//! [rutis-loader](https://docs.rs/rutis-loader). Its `reconcile` counts a row
//! still waiting for a service as settled, and since rutis 0.12
//! (`ServiceCatalog::new()` shares every name) a row injecting a service
//! nobody provides waits for good — reading only `reconcile`'s failures, a
//! plugin that never starts looks like a success. [`wait_running`] waits
//! until each row runs, fails at once on a row that never will, and when the
//! wait ends names, per row still waiting, the services it injects that do
//! not exist: [`Loader::check`] asks the runtimes what each plugin injects,
//! and each name is looked up in rutis as a shared service. (`check` alone
//! cannot say: since rutis 0.12 a JavaScript row may provide services it
//! never declares, so an unprovided name reads as one it might announce, and
//! a name another row only declares reads as provided.)

use std::time::Duration;

use rutis::{Ctx, FiberState};
use rutis_bridge::session::{host_key, HostDispatch};
use rutis_loader::{EntryStatus, Layer, Loader, Provider, RowStatus, Supply};

/// Where a row that is not running stands.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RowState {
    /// Waiting for the services it injects (rutis `Pending`).
    Waiting,
    /// Its plugin's `apply` is running.
    Loading,
    /// Its cleanup is running (it was running and is being replaced).
    Unloading,
    /// A group above it is not running.
    Inactive,
    /// Its `apply` failed.
    Failed,
    /// Removed for good.
    Disposed,
    /// It cannot run: the row is invalid or its module does not resolve.
    Unresolved,
    /// The row, or a group above it, is disabled.
    Disabled,
    /// A copy inside an instance disposed itself.
    Stopped,
    /// No row has this id.
    NoSuchRow,
    /// A state this crate does not know (a newer rutis-loader).
    Other(String),
}

impl RowState {
    /// Whether the row may still come to run: waiting, loading, unloading
    /// or inactive.
    pub fn may_run(&self) -> bool {
        matches!(
            self,
            RowState::Waiting | RowState::Loading | RowState::Unloading | RowState::Inactive
        )
    }
}

impl std::fmt::Display for RowState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            RowState::Waiting => "waiting",
            RowState::Loading => "loading",
            RowState::Unloading => "unloading",
            RowState::Inactive => "inactive",
            RowState::Failed => "failed",
            RowState::Disposed => "disposed",
            RowState::Unresolved => "unresolved",
            RowState::Disabled => "disabled",
            RowState::Stopped => "stopped",
            RowState::NoSuchRow => "no such row",
            RowState::Other(other) => other,
        };
        f.write_str(name)
    }
}

/// A row that is not running.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct NotRunning {
    /// The row's id.
    pub id: String,
    pub state: RowState,
    /// Why it failed or cannot resolve, when it did.
    pub error: Option<String>,
    /// For a [`RowState::Waiting`] row when the wait ended: the services it
    /// injects that do not exist in rutis. Empty when all of them exist,
    /// when the row waits for a service the host registered in the catalog
    /// as a typed Rust service (not looked up here: its key is the type's),
    /// or when `check` did not answer in time.
    pub missing: Vec<String>,
}

/// Rows that did not run ([`wait_running`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RowsNotRunning {
    pub rows: Vec<NotRunning>,
}

impl std::fmt::Display for RowsNotRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let rows: Vec<String> = self
            .rows
            .iter()
            .map(|row| {
                let mut text = format!("{} ({})", row.id, row.state);
                if !row.missing.is_empty() {
                    text.push_str(&format!(": nothing provides {}", row.missing.join(", ")));
                }
                if let Some(error) = &row.error {
                    text.push_str(&format!(": {error}"));
                }
                text
            })
            .collect();
        write!(f, "plugin rows not running: {}", rows.join("; "))
    }
}

impl std::error::Error for RowsNotRunning {}

/// How long the final [`Loader::check`] may take: it asks runtimes to
/// describe their plugins, and a hung runtime must not hold the wait.
const CHECK_LIMIT: Duration = Duration::from_secs(10);

/// Wait until every row in `ids` runs (its fiber is active), at most
/// `timeout`.
///
/// A row that never will — failed, disposed, unresolved, disabled, stopped,
/// or no such row — ends the wait at once. Rows waiting, loading, unloading
/// or inactive are waited for. At the end of the wait, each row still
/// waiting is reported with the services it injects (by `layers`, the layers
/// just reconciled) that do not exist, looked up from `ctx` (the context the
/// loader runs under, usually the root) as shared services. An instanced
/// group's own row always reads inactive: pass the rows inside it.
pub async fn wait_running(
    loader: &Loader,
    ctx: &Ctx,
    layers: &[Layer],
    ids: &[&str],
    timeout: Duration,
) -> Result<(), RowsNotRunning> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let rows: Vec<NotRunning> = ids
            .iter()
            .filter_map(|id| not_running(loader, id))
            .collect();
        let ended: Vec<NotRunning> = rows
            .iter()
            .filter(|row| !row.state.may_run())
            .cloned()
            .collect();
        if !ended.is_empty() {
            return Err(RowsNotRunning { rows: ended });
        }
        if rows.is_empty() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            let mut rows = rows;
            if rows.iter().any(|row| row.state == RowState::Waiting) {
                if let Ok(report) = tokio::time::timeout(CHECK_LIMIT, loader.check(layers)).await {
                    for row in rows.iter_mut().filter(|r| r.state == RowState::Waiting) {
                        let Some(check) = report.rows.iter().find(|c| c.id == row.id) else {
                            continue;
                        };
                        if let RowStatus::Checked { requires, .. } = &check.status {
                            row.missing = requires
                                .iter()
                                .filter(|r| !from_host(&r.supply))
                                .filter(|r| {
                                    ctx.get_as::<dyn HostDispatch>(host_key(&r.service))
                                        .is_none()
                                })
                                .map(|r| r.service.clone())
                                .collect();
                        }
                    }
                }
            }
            return Err(RowsNotRunning { rows });
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Whether the host registered the service in the catalog (a typed Rust
/// service is under its type's key, which a lookup by name cannot find).
fn from_host(supply: &Supply) -> bool {
    matches!(supply, Supply::Provided(providers) if providers.contains(&Provider::Host))
}

/// The row `id`, unless it runs.
fn not_running(loader: &Loader, id: &str) -> Option<NotRunning> {
    let (state, error) = match loader.get(id).map(|info| info.status) {
        Some(EntryStatus::Running(s)) => {
            let state = match s.state {
                FiberState::Active => return None,
                FiberState::Pending => RowState::Waiting,
                FiberState::Loading => RowState::Loading,
                FiberState::Unloading => RowState::Unloading,
                FiberState::Failed => RowState::Failed,
                FiberState::Disposed => RowState::Disposed,
            };
            (state, s.error.map(|e| e.to_string()))
        }
        Some(EntryStatus::Unresolved(e)) => (RowState::Unresolved, Some(e.to_string())),
        Some(EntryStatus::Inactive) => (RowState::Inactive, None),
        Some(EntryStatus::Disabled) => (RowState::Disabled, None),
        Some(EntryStatus::Stopped) => (RowState::Stopped, None),
        Some(other) => (RowState::Other(format!("{other:?}")), None),
        None => (RowState::NoSuchRow, None),
    };
    Some(NotRunning {
        id: id.to_owned(),
        state,
        error,
        missing: Vec::new(),
    })
}
