//! Waiting for plugin rows to run, with what keeps a row waiting.
//!
//! The bridge never loads plugins; a host does, with
//! [rutis-loader](https://docs.rs/rutis-loader). Its `reconcile` counts a row
//! still waiting for a service as settled, and a row injecting a service
//! nobody provides waits for good — reading only `reconcile`'s failures, a
//! plugin that never starts looks like a success. [`wait_running`] waits
//! until each row runs, fails at once on a row that never will, and when the
//! wait ends names, per row still waiting, the services it waits for and why
//! each is not there, as rutis-loader says it ([`Loader::waits`],
//! [`Loader::status`]: what `rutis-host status` prints).

use std::time::Duration;

use rutis::host::FiberState;
use rutis_loader::report::{EntryStatus, WaitReason};
use rutis_loader::Loader;

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
    /// waits for, by name, as rutis-loader reports them ([`Loader::waits`]);
    /// empty when it waits for its own runtime alone, or rutis cannot say
    /// which. `None` for another state.
    pub missing: Option<Vec<String>>,
    /// For a [`RowState::Waiting`] row when the wait ended: rutis-loader's
    /// line on what it waits for and why each is not there
    /// (`waiting for db (no row provides it)`, `waiting for runtime node
    /// (starting)`), as `rutis-host status` prints it.
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
                if let Some(note) = &row.note {
                    text.push_str(&format!(": {note}"));
                } else if let Some(missing) = row.missing.as_ref().filter(|m| !m.is_empty()) {
                    text.push_str(&format!(": waiting for {}", missing.join(", ")));
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

/// Wait until every row in `ids` runs (its fiber is active), at most
/// `timeout`.
///
/// A row that never will — failed, disposed, unresolved, disabled, stopped,
/// no such row, or a state this crate does not know — ends the wait at once;
/// the error then lists every row not running, those that may still run
/// included ([`RowState::may_run`]). Rows waiting, loading, unloading or
/// inactive are waited for. At the end of the wait, each row still waiting
/// is reported with the services it waits for and why each is not there
/// ([`NotRunning::missing`], [`NotRunning::note`]). Rows in an instanced
/// group (and the group's own row) always read inactive here — their copies
/// run per instance — so they cannot be waited for with this function.
pub async fn wait_running<S: AsRef<str>>(
    loader: &Loader,
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
            waits_for(loader, &mut rows);
            return Err(RowsNotRunning { rows });
        }
        // Polled: a row comes to run through its own fiber, a provider's,
        // a runtime or a group around it, and no one event says them all.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Fill each waiting row's `missing` and `note` from rutis-loader.
fn waits_for(loader: &Loader, rows: &mut [NotRunning]) {
    let waiting: Vec<_> = rows
        .iter()
        .filter(|row| row.state == RowState::Waiting)
        .filter_map(|row| loader.get(&row.id))
        .collect();
    if waiting.is_empty() {
        return;
    }
    let waits = loader.waits_of(&waiting.iter().collect::<Vec<_>>());
    let status = loader.status();
    for (entry, waits) in waiting.iter().zip(waits) {
        let Some(row) = rows.iter_mut().find(|row| row.id == entry.id) else {
            continue;
        };
        row.missing = Some(
            waits
                .into_iter()
                .filter(|wait| !matches!(wait.reason, WaitReason::OwnRuntime))
                .map(|wait| wait.service)
                .collect(),
        );
        row.note = status
            .rows
            .iter()
            .find(|line| line.id == row.id && line.instance.is_none())
            .and_then(|line| line.reason.clone());
    }
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
