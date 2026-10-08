//! pi coding-agent extensions in yoagent, end to end: the adapter
//! `plugins/pi/pi-extensions-adapter.ts` loading a fixture pi extension
//! (`plugins/pi/fixture-extension.ts`, no network) as a rutis-loader row of a
//! Node runtime.
//!
//! Needs Node 24+ and `npm ci` in `plugins/pi/`. Without them the test
//! prints `SKIPPED:` and passes; `YOAGENT_RUTIS_REQUIRE_RUNTIMES=1` (CI) makes
//! it fail instead.
#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use rutis::Ctx;
use rutis_bridge::runtime::LocalRuntime;
use rutis_loader::{
    Chain, Layer, Loader, LoaderOptions, LoaderPlugin, Patch, RuntimeResolver, RuntimeRowsPlugin,
    ServiceCatalog,
};
use serde_json::{json, Value};
use yoagent::provider::mock::{MockResponse, MockToolCall};
use yoagent::tools::{BashTool, EditFileTool, WriteFileTool};
use yoagent_rutis::RutisBridge;

fn pi_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins/pi")
}

/// The Node runtime package, if Node 24+ and `npm ci` in `plugins/pi/` are there.
fn node_runtime() -> Option<PathBuf> {
    let skip = |why: String| -> Option<PathBuf> {
        if std::env::var_os("YOAGENT_RUTIS_REQUIRE_RUNTIMES").is_some_and(|v| v == "1") {
            panic!("the pi runtime is required (YOAGENT_RUTIS_REQUIRE_RUNTIMES=1) but unavailable: {why}");
        }
        eprintln!("SKIPPED: the pi runtime is unavailable: {why}");
        None
    };
    let major = Command::new("node")
        .arg("--version")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| {
            String::from_utf8_lossy(&out.stdout)
                .trim()
                .trim_start_matches('v')
                .split('.')
                .next()
                .and_then(|m| m.parse::<u32>().ok())
        });
    match major {
        Some(major) if major >= 24 => {}
        Some(major) => return skip(format!("Node {major} found, 24+ needed")),
        None => return skip("no `node` on PATH".into()),
    }
    let modules = pi_dir().join("node_modules");
    for package in ["@arcships/rutis-runtime", "@earendil-works/pi-coding-agent"] {
        if !modules.join(package).join("package.json").exists() {
            return skip(format!(
                "{package} missing from {}: run `npm ci` in plugins/pi/",
                modules.display()
            ));
        }
    }
    Some(modules.join("@arcships/rutis-runtime"))
}

struct Host {
    root: Ctx,
    bridge: RutisBridge,
    loader: Loader,
}

async fn host(runtime: &Path) -> Host {
    let root = Ctx::root().unwrap();
    let bridge = RutisBridge::install(&root).unwrap();
    let mut catalog = ServiceCatalog::new();
    catalog.register_shared("yoagent");
    let node = LocalRuntime::node(runtime, pi_dir().join("package.json"));
    let resolver = Arc::new(RuntimeResolver::node(node.handle()).with_catalog(&catalog));
    root.plugin(node);
    let plugin = LoaderPlugin::new(
        Chain::new().with_shared(resolver.clone()),
        LoaderOptions {
            catalog,
            ..LoaderOptions::default()
        },
    );
    let loader = plugin.handle();
    root.plugin(plugin).await.unwrap();
    root.plugin(RuntimeRowsPlugin::new(resolver));
    Host {
        root,
        bridge,
        loader,
    }
}

impl Host {
    async fn load(&self, rows: Value) {
        let patches: Vec<Patch> = serde_json::from_value(json!([{ "insert": rows }])).unwrap();
        let report = self
            .loader
            .reconcile(vec![Layer::new("rows", patches)], None)
            .await
            .unwrap();
        assert!(report.failures.is_empty(), "{report:?}");
        let registry = self.bridge.registry().clone();
        tokio::time::timeout(Duration::from_secs(60), async {
            while !registry
                .handlers()
                .iter()
                .any(|h| h.name() == "pi-extensions")
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the adapter registers its handler");
    }
}

fn calls(names: &[(&str, Value)]) -> MockResponse {
    MockResponse::ToolCalls(
        names
            .iter()
            .map(|(name, args)| MockToolCall {
                provider_metadata: None,
                name: (*name).into(),
                arguments: args.clone(),
            })
            .collect(),
    )
}

async fn wait_file(path: &Path, want: impl Fn(&str) -> bool) -> String {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Ok(text) = std::fs::read_to_string(path) {
                if want(&text) {
                    return text;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "{} never matched: {:?}",
            path.display(),
            std::fs::read_to_string(path)
        )
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn pi_extensions_reach_a_yoagent_agent_through_the_adapter() {
    let Some(runtime) = node_runtime() else {
        return;
    };
    let project = tempfile::tempdir().unwrap();
    let env_file = project.path().join(".env");
    let notes = project.path().join("notes.txt");
    std::fs::write(&notes, "hello world").unwrap();

    let host = host(&runtime).await;
    host.load(json!([{
        "id": "pi",
        "name": pi_dir().join("pi-extensions-adapter.ts"),
        "config": {
            "extensions": [pi_dir().join("fixture-extension.ts")],
            "cwd": project.path(),
        },
    }]))
    .await;

    let (agent, seen) = agent(vec![
        calls(&[
            ("pi_echo", json!({"text": "SECRET hi"})),
            ("pi_fail", json!({"why": "nope"})),
            ("pi_dynamic", json!({})),
            (
                "write_file",
                json!({"path": env_file, "content": "TOKEN=1"}),
            ),
            ("bash", json!({"command": "echo original"})),
            (
                "edit_file",
                json!({"path": notes, "old_text": "world", "new_text": "pi"}),
            ),
        ]),
        text("done"),
        call("pi_slow", json!({})),
        text("never sent"),
    ]);
    let mut agent = agent
        .with_tools(vec![
            Box::new(BashTool::new()),
            Box::new(WriteFileTool::new()),
            Box::new(EditFileTool::new()),
        ])
        .with_extension(host.bridge.extension());
    let (_, results) = tokio::time::timeout(Duration::from_secs(60), run(&mut agent, "go"))
        .await
        .expect("the run finishes");

    // The extension's tools are offered, the one registered at session start too...
    let seen_now = seen.lock().unwrap().clone();
    for tool in ["pi_echo", "pi_fail", "pi_slow", "pi_dynamic"] {
        assert!(
            seen_now[0].tools.contains(&tool.to_string()),
            "{seen_now:?}"
        );
    }
    let result = |name: &str| {
        results
            .iter()
            .find(|(n, _, _)| n == name)
            .unwrap_or_else(|| panic!("no {name} result: {results:?}"))
            .clone()
    };
    // ...a tool's text comes back, through the extension's tool_result redaction...
    assert_eq!(
        result("pi_echo"),
        ("pi_echo".into(), "pi echo: [redacted] hi".into(), false)
    );
    // ...a throwing tool is an error result...
    let (_, text, is_error) = result("pi_fail");
    assert!(is_error && text.contains("pi failure: nope"), "{results:?}");
    assert_eq!(result("pi_dynamic").1, "dynamic ok");
    // ...yoagent's own write_file is judged as pi's `write` and blocked...
    let (_, text, is_error) = result("write_file");
    assert!(is_error && text.contains("is protected"), "{results:?}");
    assert!(!env_file.exists(), "the blocked write never ran");
    // ...an in-place rewrite of `event.input` changes the call...
    let (_, text, is_error) = result("bash");
    assert!(!is_error && text.contains("rewritten"), "{results:?}");
    // ...including edit_file's arguments, translated to pi's `edits` and back.
    assert!(!result("edit_file").2, "{results:?}");
    assert_eq!(std::fs::read_to_string(&notes).unwrap(), "hello PI");
    // before_agent_start's addition to the system prompt arrives as a note, never stored.
    let note = &seen_now[0].last_user;
    assert!(
        note.contains("Fixture rules: answer in one line."),
        "{note}"
    );
    assert!(
        !format!("{:?}", agent.messages()).contains("Fixture rules"),
        "the note is never stored"
    );

    // Cancelling the run aborts the pi tool through its signal.
    let mut rx = agent.prompt("slow").await;
    let slow = project.path().join("slow.txt");
    wait_file(&slow, |t| t == "started").await;
    agent.abort();
    tokio::time::timeout(Duration::from_secs(30), async {
        while rx.recv().await.is_some() {}
    })
    .await
    .expect("the cancelled run ends");
    agent.finish().await;
    let ended = wait_file(&slow, |t| t != "started").await;
    assert_eq!(ended, "aborted: AbortError");
    host.root.shutdown().await.unwrap();
}
