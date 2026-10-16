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
    let report = probe::pi(
        &adapters().root,
        cwd.path(),
        &[extension],
        &[],
        Some(logs()),
    )
    .await;
    assert_eq!(report.loaded, Some(true), "{report:?}");
    assert!(report.tools.iter().any(|t| t == "pi_echo"), "{report:?}");
    assert_eq!(report.rows.len(), 1);
    assert_eq!(report.rows[0].state, "active");
    assert_eq!(report.problem(), None);
    // The adapter's load line, which the matrix reads the events from.
    assert!(
        report.logs.iter().any(|l| l.level == "debug"
            && l.message.contains("[pi] loaded")
            && l.message.contains("events")),
        "{:?}",
        report.logs
    );

    // No extensions: only skills (none here), nothing loaded.
    let report = probe::pi(&adapters().root, cwd.path(), &[], &[], None).await;
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
    let report = probe::pi(&adapters().root, cwd.path(), &[missing], &[], None).await;
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
    let report = probe::dsh(&adapters().root, &dsh, &rows, &[], None).await;
    assert_eq!(report.error, None, "{report:?}");
    assert!(
        report.tools.iter().any(|t| t == "fixture_echo"),
        "{report:?}"
    );
    assert_eq!(report.rows.len(), 1);
    assert_eq!(report.services["missing"], json!([]), "{report:?}");
    assert_eq!(report.problem(), None, "{report:?}");

    // A service nobody provides is reported missing.
    let report = probe::dsh(
        &adapters().root,
        &dsh,
        &rows,
        &["no-such-service".into()],
        None,
    )
    .await;
    assert_eq!(
        report.services["missing"],
        json!(["no-such-service"]),
        "{report:?}"
    );
    assert!(report.problem().unwrap().contains("no-such-service"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dsh_plugin_missing_a_service_it_injects_is_not_clean() {
    let dsh = adapters().dsh;
    if !installed(&dsh) {
        return;
    }
    // Its module injects a service no host provides: it never starts, though
    // its row reads as started. `inject` is empty: the probe reads what the
    // module injects itself.
    let rows = json!([{ "id": "needs", "name": dsh.join("fixture-missing-service.ts") }]);
    let report = probe::dsh(&adapters().root, &dsh, &rows, &[], None).await;
    assert_eq!(
        report.services["missing"],
        json!(["yoagentNoSuchService"]),
        "{report:?}"
    );
    assert!(report.problem().is_some(), "{report:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn dsh_rows_must_be_an_array_with_ids() {
    let dsh = adapters().dsh;
    let report = probe::dsh(&adapters().root, &dsh, &json!({ "id": "x" }), &[], None).await;
    assert!(report.error.is_some(), "{report:?}");
    let report = probe::dsh(&adapters().root, &dsh, &json!([{ "name": "x" }]), &[], None).await;
    assert!(report.error.is_some(), "{report:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pi_extension_that_refuses_after_load_is_not_clean() {
    let pi = adapters().pi;
    if !installed(&pi) {
        return;
    }
    let cwd = tempfile::tempdir().unwrap();
    let extension = pi.join("fixture-late-refusal.ts").display().to_string();
    let report = probe::pi(
        &adapters().root,
        cwd.path(),
        &[extension],
        &[],
        Some(logs()),
    )
    .await;
    assert_eq!(report.loaded, Some(false), "{report:?}");
    let error = report.error.as_deref().unwrap_or_default();
    assert!(error.contains("adapter stopped"), "{report:?}");
    assert!(report.problem().is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tool_named_like_a_built_in_is_retried_without_it() {
    let pi = adapters().pi;
    if !installed(&pi) {
        return;
    }
    let cwd = tempfile::tempdir().unwrap();
    let extension = pi.join("fixture-shadow.ts").display().to_string();
    let report = probe::pi(&adapters().root, cwd.path(), &[extension], &[], None).await;
    assert_eq!(report.loaded, Some(true), "{report:?}");
    assert_eq!(report.without, ["read_file"], "{report:?}");
    assert!(report.tools.iter().any(|t| t == "read_file"), "{report:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_service_declared_required_false_is_still_required() {
    let dsh = adapters().dsh;
    if !installed(&dsh) {
        return;
    }
    let rows = json!([{ "id": "opt", "name": dsh.join("fixture-optional-service.ts") }]);
    let report = probe::dsh(&adapters().root, &dsh, &rows, &[], None).await;
    assert_eq!(
        report.services["missing"],
        json!(["yoagentNoSuchOptional"]),
        "{report:?}"
    );
    assert!(report.problem().is_some(), "{report:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_late_tool_named_like_a_built_in_is_retried_without_it() {
    let pi = adapters().pi;
    if !installed(&pi) {
        return;
    }
    let cwd = tempfile::tempdir().unwrap();
    let extension = pi.join("fixture-late-shadow.ts").display().to_string();
    let report = probe::pi(&adapters().root, cwd.path(), &[extension], &[], None).await;
    assert_eq!(report.loaded, Some(true), "{report:?}");
    assert_eq!(report.without, ["read_file"], "{report:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_base_of_its_own_gets_the_probe_files_copied_in() {
    let dsh = adapters().dsh;
    if !installed(&dsh) {
        return;
    }
    // A base like a host's: DSH's packages and a package.json, without the
    // probe's two files. Node follows the symlink, so one Cordis.
    let base = tempfile::tempdir().unwrap();
    std::fs::copy(dsh.join("package.json"), base.path().join("package.json")).unwrap();
    std::os::unix::fs::symlink(dsh.join("node_modules"), base.path().join("node_modules")).unwrap();
    let rows = json!([{ "id": "fx", "name": dsh.join("fixture-tools.ts") }]);
    let report = probe::dsh(&adapters().root, base.path(), &rows, &[], None).await;
    assert_eq!(report.problem(), None, "{report:?}");
    assert!(base.path().join("probe-services.ts").is_file());
    assert!(base.path().join("dsh-tools-adapter.ts").is_file());
}
