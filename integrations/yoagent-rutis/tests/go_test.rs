//! Go plugins through the bridge: `plugins/go` (policies, a redaction and
//! tools; see its doc), built with the local Go toolchain and run as a rutis
//! Go runtime (`go:<plugin>` rows).
//!
//! Needs Go 1.24+ on `PATH`; without it the test prints `SKIPPED:` and
//! passes, unless `YOAGENT_RUTIS_REQUIRE_RUNTIMES=1`. With Go there, a
//! plugin that does not build fails the test.
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
    // Go is there: a plugin that does not build is a failure, never a skip.
    let built = Command::new("go")
        .arg("build")
        .arg("-o")
        .arg(&binary)
        .arg(".")
        .current_dir(&source)
        .status()
        .expect("run `go build`");
    assert!(built.success(), "`go build` failed in {}", source.display());
    Some(binary)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_go_plugin_offers_tools_redacts_fails_closed_and_unloads() {
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

    let until = |line: &'static str| {
        let log = &log;
        async move {
            tokio::time::timeout(Duration::from_secs(10), async {
                while !log().contains(line) {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("Go never logged {line:?}: {}", log()));
        }
    };
    let blocked: Vec<Reply> = ["bash", "fail_gate", "odd_gate"]
        .iter()
        .map(|name| Reply::new(name, "ran"))
        .collect();
    let blocked_runs: Vec<_> = blocked.iter().map(Reply::runs).collect();

    // A tool from Go, whose output Go's after_tool redacts; and policies:
    // Go's denial, Go's error and a malformed answer all deny.
    let (first, _) = agent(vec![
        calls(&[
            ("go_echo", json!({"say": "a secret"})),
            ("bash", json!({"command": "ls"})),
            ("fail_gate", json!({})),
            ("odd_gate", json!({})),
        ]),
        text("done"),
    ]);
    let mut first = first
        .with_tools(
            blocked
                .into_iter()
                .map(|t| Box::new(t) as Box<dyn yoagent::AgentTool>)
                .collect(),
        )
        .with_extension(bridge.extension());
    let (_, results) = run(&mut first, "go").await;
    let result = |name: &str| results.iter().find(|(n, ..)| n == name).cloned().unwrap();
    let (_, echoed, failed) = result("go_echo");
    assert!(
        !failed && echoed.contains("go says") && echoed.contains("a [go redacted]"),
        "{results:?}"
    );
    assert!(!echoed.contains("secret"), "{echoed}");
    for (name, reason) in [
        ("bash", "no shell from Go"),
        ("fail_gate", "go policy backend down"),
        ("odd_gate", "unexpected value"),
    ] {
        let (_, text, denied) = result(name);
        assert!(denied && text.contains(reason), "{name}: {results:?}");
    }
    assert!(
        blocked_runs.iter().all(|r| r.load(Ordering::SeqCst) == 0),
        "{results:?}"
    );

    // A policy the host stops waiting for: denied, and Go's context cancelled.
    let gate = Reply::new("slow_gate", "ran");
    let gate_runs = gate.runs();
    let (gated, _) = agent(vec![call("slow_gate", json!({})), text("done")]);
    let mut gated = gated.with_tools(vec![Box::new(gate)]).with_extension(
        bridge
            .extension()
            .with_policy_timeout(Some(Duration::from_millis(300))),
    );
    let (_, results) = run(&mut gated, "gate").await;
    assert!(
        results[0].2 && results[0].1.contains("did not answer"),
        "{results:?}"
    );
    assert_eq!(gate_runs.load(Ordering::SeqCst), 0);
    until("go slow_gate: cancelled").await;

    // Cancelling the run cancels the Go tool call in flight.
    let (waiting, _) = agent(vec![call("go_wait", json!({})), text("never sent")]);
    let mut waiting = waiting.with_extension(bridge.extension());
    let mut events = waiting.prompt("wait").await;
    until("go wait: started").await;
    waiting.abort();
    tokio::time::timeout(Duration::from_secs(30), async {
        while events.recv().await.is_some() {}
    })
    .await
    .expect("the cancelled run ends");
    waiting.finish().await;
    until("go wait: cancelled").await;

    // Unloading the plugin removes its handler (its disposer ran): its tools
    // are gone and its policy no longer denies.
    loader.reconcile(vec![], None).await.unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        while registry.handlers().iter().any(|h| h.name() == "go-policy") {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the Go handler is removed on unload");
    let shell = Reply::new("bash", "ran");
    let shell_runs = shell.runs();
    let (after, _) = agent(vec![call("bash", json!({"command": "ls"})), text("done")]);
    let mut after = after
        .with_tools(vec![Box::new(shell)])
        .with_extension(bridge.extension());
    run(&mut after, "after").await;
    assert_eq!(shell_runs.load(Ordering::SeqCst), 1, "no Go policy left");
    root.shutdown().await.unwrap();
}

/// One response calling several tools at once.
fn calls(names: &[(&str, serde_json::Value)]) -> yoagent::provider::mock::MockResponse {
    yoagent::provider::mock::MockResponse::ToolCalls(
        names
            .iter()
            .map(|(name, args)| yoagent::provider::mock::MockToolCall {
                provider_metadata: None,
                name: (*name).into(),
                arguments: args.clone(),
            })
            .collect(),
    )
}
