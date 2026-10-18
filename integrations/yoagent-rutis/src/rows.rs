//! Waiting for plugin rows to run, with what keeps a row waiting.
//!
//! The bridge never loads plugins; a host does, with
//! [rutis-loader](https://docs.rs/rutis-loader). Its `reconcile` counts a row
//! still waiting for a service as settled, and since rutis 0.12
//! (`ServiceCatalog::new()` shares every name) a row injecting a service
//! nobody provides waits for good — reading only `reconcile`'s failures, a
//! plugin that never starts looks like a success. [`wait_running`] waits
//! until each row runs, fails at once on a row that cannot, and when the
//! wait ends names, per row still waiting, the services it injects that do
//! not exist: [`Loader::check`] asks the runtimes what each plugin injects,
//! and each name is looked up in rutis as a shared service. (`check` alone
//! cannot say: since rutis 0.12 a JavaScript row may provide services it
//! never declares, so an unprovided name reads as one it might announce.)

use std::time::Duration;

use rutis::{Ctx, FiberState};
use rutis_bridge::session::{host_key, HostDispatch};
use rutis_loader::{EntryStatus, Layer, Loader, RowStatus, Supply};

/// A row that is not running.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct NotRunning {
    /// The row's id.
    pub id: String,
    /// `waiting` (still pending), `failed`, `unresolved`, `missing` (no such
    /// row), or another rutis state in lower case.
    pub state: String,
    /// Why it failed or cannot resolve, when it did.
    pub error: Option<String>,
    /// For a waiting row: the services it injects that do not exist in
    /// rutis when the wait ends. Empty when all of them do — the row waits
    /// for something else.
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

/// Wait until every row in `ids` runs (its fiber is active), at most
/// `timeout`. A row that fails or cannot resolve ends the wait at once; at
/// the end of the wait, each row still waiting is reported with the services
/// it injects (by `layers`, the layers just reconciled) that do not exist,
/// looked up from `ctx` (the context the loader runs under, usually the
/// root) as shared services.
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
        let failed: Vec<NotRunning> = rows
            .iter()
            .filter(|row| row.state != "waiting" && row.state != "inactive")
            .cloned()
            .collect();
        if !failed.is_empty() {
            return Err(RowsNotRunning { rows: failed });
        }
        if rows.is_empty() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            let mut rows = rows;
            let report = loader.check(layers).await;
            for row in &mut rows {
                if let Some(check) = report.rows.iter().find(|check| check.id == row.id) {
                    if let RowStatus::Checked { requires, .. } = &check.status {
                        row.missing = requires
                            .iter()
                            .filter(|r| !matches!(r.supply, Supply::Provided(_)))
                            .filter(|r| {
                                ctx.get_as::<dyn HostDispatch>(host_key(&r.service))
                                    .is_none()
                            })
                            .map(|r| r.service.clone())
                            .collect();
                    }
                }
            }
            return Err(RowsNotRunning { rows });
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The row `id`, unless it runs.
fn not_running(loader: &Loader, id: &str) -> Option<NotRunning> {
    let (state, error) = match loader.get(id).map(|info| info.status) {
        Some(EntryStatus::Running(s)) => match s.state {
            FiberState::Active => return None,
            FiberState::Failed => ("failed".into(), s.error.map(|e| e.to_string())),
            FiberState::Pending => ("waiting".into(), None),
            other => (format!("{other:?}").to_lowercase(), None),
        },
        Some(EntryStatus::Unresolved(e)) => ("unresolved".into(), Some(e.to_string())),
        Some(other) => (format!("{other:?}").to_lowercase(), None),
        None => ("missing".into(), None),
    };
    Some(NotRunning {
        id: id.to_owned(),
        state,
        error,
        missing: Vec::new(),
    })
}
