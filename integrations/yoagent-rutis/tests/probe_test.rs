//! The probe (`yoagent_rutis::probe`, feature `probe`) on this crate's own
//! fixtures: what the pi and DSH adapters give an agent, and what plugins
//! log. Needs `npm ci` in `plugins/pi/` and `plugins/dsh/`; skipped
//! without, unless `YOAGENT_RUTIS_REQUIRE_RUNTIMES=1`.

use std::path::Path;
use std::sync::OnceLock;

use serde_json::json;
use tracing_subscriber::layer::SubscriberExt;
use yoagent_rutis::adapters::adapters;
use yoagent_rutis::probe::{self, Logs};

/// Installed once for the test binary: the runtime logs from its own threads.
fn logs() -> &'static Logs {
    static LOGS: OnceLock<Logs> = OnceLock::new();
    LOGS.get_or_init(|| {
        let logs = Logs::default();
        tracing::subscriber::set_global_default(tracing_subscriber::registry().with(logs.clone()))
            .unwrap();
        logs
    })
}

fn installed(dir: &Path) -> bool {
    if dir
        .join("node_modules/@arcships/rutis-runtime/package.json")
        .exists()
    {
        return true;
    }
    if std::env::var_os("YOAGENT_RUTIS_REQUIRE_RUNTIMES").is_some_and(|v| v == "1") {
        panic!(
            "the runtime is required but unavailable: run `npm ci` in {}",
            dir.display()
        );
    }
    eprintln!("SKIPPED: run `npm ci` in {}", dir.display());
    false
}

#[tokio::test(flavor = "multi_thread")]
async fn pi_extensions_are_loaded_and_their_tools_listed() {
    let pi = adapters().pi;
    if !installed(&pi) {
        return;
    }
    let cwd = tempfile::tempdir().unwrap();
    let extension = pi.join("fixture-extension.ts").display().to_string();
    let report = probe::pi(cwd.path(), &[extension], &[], Some(logs())).await;
    assert_eq!(report.loaded, Some(true), "{report:?}");
    assert!(report.tools.iter().any(|t| t == "pi_echo"), "{report:?}");
    assert_eq!(report.rows.len(), 1);
    assert_eq!(report.rows[0].state, "active");

    // No extensions: only skills (none here), nothing loaded.
    let report = probe::pi(cwd.path(), &[], &[], None).await;
    assert_eq!(report.loaded, None);
    assert_eq!(report.skills.found, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pi_extension_that_does_not_load_says_why() {
    let pi = adapters().pi;
    if !installed(&pi) {
        return;
    }
    let cwd = tempfile::tempdir().unwrap();
    let missing = cwd
        .path()
        .join("no-such-extension.ts")
        .display()
        .to_string();
    let report = probe::pi(cwd.path(), &[missing], &[], None).await;
    assert_eq!(report.loaded, Some(false), "{report:?}");
    assert!(report.error.is_some(), "{report:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn dsh_rows_report_the_tools_they_add_and_the_services_they_need() {
    let dsh = adapters().dsh;
    if !installed(&dsh) {
        return;
    }
    let rows = json!([{ "id": "fx", "name": dsh.join("fixture-tools.ts") }]);
    let report = probe::dsh(&dsh, &rows, &[], None).await;
    assert_eq!(report.error, None, "{report:?}");
    assert!(
        report.tools.iter().any(|t| t == "fixture_echo"),
        "{report:?}"
    );
    assert_eq!(report.rows.len(), 1);
    assert_eq!(report.services["missing"], json!([]), "{report:?}");

    // A service nobody provides is reported missing.
    let report = probe::dsh(&dsh, &rows, &["no-such-service".into()], None).await;
    assert_eq!(
        report.services["missing"],
        json!(["no-such-service"]),
        "{report:?}"
    );
}
