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
//! never declares, so with another JavaScript row loaded an unprovided name
//! reads as one that row might announce; a name another row only declares
//! reads as provided; and a host service shared by name reads as missing.)

use std::time::Duration;

use rutis::{Ctx, FiberState};
use rutis_bridge::session::{host_key, HostDispatch};
use rutis_loader::{CheckReport, EntryStatus, Layer, Loader, Provider, RowStatus, Supply};

/// Where a row that is not running stands.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RowState {
    /// Waiting for the services it injects (rutis `Pending`).
    Waiting,
    /// Its plugin's `apply` is running.
    Loading,
    /// Its cleanup is running (it is being replaced, removed or disposed).
    Unloading,
    /// Not started where [`rutis_loader::Loader::get`] looks: a group above
    /// it is disabled or not running, the loader is not mounted, or the row
    /// is in an instanced group (its copies run per instance). Waited for,
    /// so reported only when the wait ends.
    Inactive,
    /// Its `apply` failed.
    Failed,
    /// Removed for good.
    Disposed,
    /// It cannot run: the row is invalid, its module does not resolve, its
    /// `disabled` expression fails, or its plugin refused its config.
    Unresolved,
    /// The row itself is disabled (one under a disabled group reads
    /// [`RowState::Inactive`]).
    Disabled,
    /// A copy inside an instance disposed itself.
    Stopped,
    /// No row has this id.
    NoSuchRow,
    /// A state this crate does not know (a newer rutis-loader): its text is
    /// diagnostic only, not stable — do not match on it.
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
    /// The error rutis reports for the row: its failed `apply`, why it
    /// cannot resolve, or a failed cleanup.
    pub error: Option<String>,
    /// For a [`RowState::Waiting`] row when the wait ended: the services it
    /// injects that do not exist in rutis — `Some` and empty when all of
    /// them exist. It omits names the host registered in the catalog under
    /// its own keys (not looked up here). `None` when not checked: another
    /// state, or the check could not tell (see `note`).
    pub missing: Option<Vec<String>>,
    /// Why `missing` is `None`, or may be incomplete, for a waiting row: the
    /// check timed out, refused the row, did not know what its plugin
    /// injects, or did not cover the row.
    pub note: Option<String>,
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
                if let Some(missing) = row.missing.as_ref().filter(|m| !m.is_empty()) {
                    text.push_str(&format!(": nothing provides {}", missing.join(", ")));
                }
                if let Some(error) = &row.error {
                    text.push_str(&format!(": {error}"));
                }
                if let Some(note) = &row.note {
                    text.push_str(&format!(" ({note})"));
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
/// `timeout`, plus up to 10 s for the final check.
///
/// A row that never will — failed, disposed, unresolved, disabled, stopped,
/// no such row, or a state this crate does not know — ends the wait at once;
/// the error then lists every row not running, those that may still run
/// included ([`RowState::may_run`]). Rows waiting, loading, unloading or
/// inactive are waited for. At the end of the wait, each row still waiting
/// is reported with the services it injects (by `layers`, the layers just
/// reconciled) that do not exist, looked up from `ctx` (the context the
/// loader runs under, usually the root) as shared services. Rows in an
/// instanced group (and the group's own row) always read inactive here —
/// their copies run per instance — so they cannot be waited for with this
/// function.
pub async fn wait_running<S: AsRef<str>>(
    loader: &Loader,
    ctx: &Ctx,
    layers: &[Layer],
    ids: &[S],
    timeout: Duration,
) -> Result<(), RowsNotRunning> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let mut rows: Vec<NotRunning> = ids
            .iter()
            .filter_map(|id| not_running(loader, id.as_ref()))
            .collect();
        if rows.iter().any(|row| !row.state.may_run()) {
            return Err(RowsNotRunning { rows });
        }
        if rows.is_empty() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            if rows.iter().any(|row| row.state == RowState::Waiting) {
                let report = tokio::time::timeout(CHECK_LIMIT, loader.check(layers)).await;
                for row in rows.iter_mut().filter(|r| r.state == RowState::Waiting) {
                    match &report {
                        Ok(report) => missing_for(row, report, ctx),
                        Err(_) => {
                            row.note = Some(format!(
                                "the check did not answer within {} s: missing services unknown",
                                CHECK_LIMIT.as_secs()
                            ))
                        }
                    }
                }
            }
            return Err(RowsNotRunning { rows });
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Fill a waiting row's `missing` (or `note`) from the check.
fn missing_for(row: &mut NotRunning, report: &CheckReport, ctx: &Ctx) {
    let Some(check) = report.rows.iter().find(|c| c.id == row.id) else {
        row.note = Some("not in the layers checked".into());
        return;
    };
    match &check.status {
        RowStatus::Checked {
            requires, declared, ..
        } => {
            row.missing = Some(
                requires
                    .iter()
                    .filter(|r| !from_host(&r.supply))
                    .filter(|r| {
                        ctx.get_as::<dyn HostDispatch>(host_key(&r.service))
                            .is_none()
                    })
                    .map(|r| r.service.clone())
                    .collect(),
            );
            if let Err(why) = declared {
                row.note = Some(format!(
                    "what its plugin injects is unknown, only the row's own inject was checked: {why}"
                ));
            }
        }
        RowStatus::Failed(error) => {
            row.note = Some(format!("the check refused the row: {error}"));
        }
        RowStatus::Disabled => row.note = Some("disabled in the layers checked".into()),
        _ => row.note = Some("the check could not tell".into()),
    }
}

/// Whether the host registered the service in the catalog (`register*`,
/// `readable*`, `export`): under its own key, which a lookup by name may not
/// find.
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
        missing: None,
        note: None,
    })
}
