//! Go plugins through the bridge: `plugins/go` (a policy, a tool, and a
//! slow policy that watches its call's context), built with the local Go
//! toolchain and run as a rutis Go runtime (`go:<plugin>` rows).
//!
//! Needs Go 1.24+ on `PATH`; without it the test prints `SKIPPED:` and
//! passes, unless `YOAGENT_RUTIS_REQUIRE_RUNTIMES=1`.
#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use rutis::Ctx;
use rutis_loader::{
    Chain, GoBinaries, GoResolver, GoRuntimes, Layer, LoaderOptions, LoaderPlugin, Patch,
    ServiceCatalog,
};
use serde_json::json;
use yoagent_rutis::RutisBridge;

/// The example plugin built into `dir`, if Go 1.24+ is there.
fn build(dir: &Path) -> Option<PathBuf> {
    let required = std::env::var_os("YOAGENT_RUTIS_REQUIRE_RUNTIMES").is_some_and(|v| v == "1");
    let skip = |why: String| {
        assert!(
            !required,
            "Go is required (YOAGENT_RUTIS_REQUIRE_RUNTIMES=1): {why}"
        );
        eprintln!("SKIPPED: Go unavailable: {why}");
        None
    };
    let version = Command::new("go")
        .args(["env", "GOVERSION"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string());
    let Some(version) = version else {
        return skip("no `go` on PATH".into());
    };
    let numbers: Vec<u32> = version
        .trim_start_matches("go")
        .split('.')
        .take(2)
        .filter_map(|n| n.parse().ok())
        .collect();
    if numbers.len() < 2 || (numbers[0], numbers[1]) < (1, 24) {
        return skip(format!("{version} found, 1.24+ needed"));
    }
    let binary = dir.join("go-policy");
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins/go");
    let built = Command::new("go")
        .arg("build")
        .arg("-o")
        .arg(&binary)
        .arg(".")
        .current_dir(&source)
        .status()
        .ok()
        .filter(|status| status.success());
    match built {
        Some(_) => Some(binary),
        None => skip(format!("`go build` failed in {}", source.display())),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_go_plugin_offers_a_tool_denies_and_sees_its_call_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    let Some(binary) = build(dir.path()) else {
        return;
    };
    let out = dir.path().join("go.log");

    let root = Ctx::root().unwrap();
    let bridge = RutisBridge::install(&root).unwrap();
    let mut catalog = ServiceCatalog::new();
    catalog.register_shared("yoagent");
    let go = Arc::new(GoResolver::new(GoBinaries::new().file(&binary)).with_catalog(&catalog));
    let plugin = LoaderPlugin::new(
        Chain::new().with_shared(go.clone()),
        LoaderOptions {
            catalog,
            ..LoaderOptions::default()
        },
    );
    let loader = plugin.handle();
    root.plugin(plugin).await.unwrap();
    root.plugin(GoRuntimes::new(go, dir.path()).idle(None))
        .await
        .unwrap();
    let rows = json!([{ "id": "go", "name": "go:go-policy", "config": { "out": out } }]);
    let patches: Vec<Patch> = serde_json::from_value(json!([{ "insert": rows }])).unwrap();
    let report = loader
        .reconcile(vec![Layer::new("rows", patches)], None)
        .await
        .unwrap();
    assert!(report.failures.is_empty(), "{report:?}");
    let log = || std::fs::read_to_string(&out).unwrap_or_default();
    let registry = bridge.registry().clone();
    tokio::time::timeout(Duration::from_secs(60), async {
        while !registry.handlers().iter().any(|h| h.name() == "go-policy") {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the Go handler did not register: {}", log()));

    // A tool from Go, and a call Go denies.
    let shell = Reply::new("bash", "ran");
    let shell_runs = shell.runs();
    let (first, _) = agent(vec![
        call("go_echo", json!({"say": "hi"})),
        call("bash", json!({"command": "ls"})),
        text("done"),
    ]);
    let mut first = first
        .with_tools(vec![Box::new(shell)])
        .with_extension(bridge.extension());
    let (_, results) = run(&mut first, "go").await;
    assert_eq!(results[0].0, "go_echo");
    assert!(
        results[0].1.contains("go says") && results[0].1.contains("hi"),
        "{results:?}"
    );
    assert!(
        results[1].2 && results[1].1.contains("no shell from Go"),
        "{results:?}"
    );
    assert_eq!(shell_runs.load(Ordering::SeqCst), 0);

    // A policy the host stops waiting for: Go's call context is cancelled.
    let gate = Reply::new("slow_gate", "ran");
    let gate_runs = gate.runs();
    let (gated, _) = agent(vec![call("slow_gate", json!({})), text("done")]);
    let mut gated = gated.with_tools(vec![Box::new(gate)]).with_extension(
        bridge
            .extension()
            .with_policy_timeout(Some(Duration::from_millis(300))),
    );
    let (_, results) = run(&mut gated, "gate").await;
    assert!(results[0].2, "{results:?}");
    assert_eq!(gate_runs.load(Ordering::SeqCst), 0);
    tokio::time::timeout(Duration::from_secs(10), async {
        while !log().contains("go slow_gate: cancelled") {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("Go never saw the cancel: {}", log()));
    root.shutdown().await.unwrap();
}
