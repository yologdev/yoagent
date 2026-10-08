//! dsh (DeepSeek Harness) tool plugins as yoagent tools, end to end: the
//! adapter `plugins/dsh/dsh-tools-adapter.ts` and a fixture dsh tool plugin
//! (`plugins/dsh/fixture-tools.ts`, no network) loaded with
//! `@deepseek-ai/dsh-system-prompt` and `@deepseek-ai/dsh-tools` as
//! rutis-loader rows of one Node runtime.
//!
//! Needs Node 24+ and `npm ci` in `plugins/dsh/`. Without them the test
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
use yoagent::{AgentMessage, Content, Message};
use yoagent_rutis::RutisBridge;

fn dsh_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins/dsh")
}

/// The Node runtime package, if Node 24+ and `npm ci` in `plugins/dsh/` are there.
fn node_runtime() -> Option<PathBuf> {
    let skip = |why: String| -> Option<PathBuf> {
        if std::env::var_os("YOAGENT_RUTIS_REQUIRE_RUNTIMES").is_some_and(|v| v == "1") {
            panic!("the dsh runtime is required (YOAGENT_RUTIS_REQUIRE_RUNTIMES=1) but unavailable: {why}");
        }
        eprintln!("SKIPPED: the dsh runtime is unavailable: {why}");
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
    let modules = dsh_dir().join("node_modules");
    for package in ["@arcships/rutis-runtime", "@deepseek-ai/dsh-tools"] {
        if !modules.join(package).join("package.json").exists() {
            return skip(format!(
                "{package} missing from {}: run `npm ci` in plugins/dsh/",
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
    let node = LocalRuntime::node(runtime, dsh_dir().join("package.json"));
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
            while !registry.handlers().iter().any(|h| h.name() == "dsh-tools") {
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
async fn dsh_tools_reach_a_yoagent_agent_through_the_adapter() {
    let Some(runtime) = node_runtime() else {
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    let abort_file = scratch.path().join("slow.txt");
    let host = host(&runtime).await;
    host.load(json!([
        { "id": "system-prompt", "name": "@deepseek-ai/dsh-system-prompt" },
        { "id": "tools", "name": "@deepseek-ai/dsh-tools" },
        {
            "id": "fixture",
            "name": dsh_dir().join("fixture-tools.ts"),
            "config": { "abortFile": abort_file },
        },
        { "id": "adapter", "name": dsh_dir().join("dsh-tools-adapter.ts") },
    ]))
    .await;

    let (agent, seen) = agent(vec![
        calls(&[
            ("fixture_echo", json!({"text": "hi"})),
            ("fixture_fail", json!({"why": "nope"})),
        ]),
        text("done"),
        call("fixture_slow", json!({})),
        text("never sent"),
    ]);
    let mut agent = agent.with_extension(host.bridge.extension());
    let (_, results) = tokio::time::timeout(Duration::from_secs(60), run(&mut agent, "go"))
        .await
        .expect("the run finishes");

    // The adapter offered the fixture's tools...
    let seen_now = seen.lock().unwrap().clone();
    for tool in ["fixture_echo", "fixture_fail", "fixture_slow"] {
        assert!(
            seen_now[0].tools.contains(&tool.to_string()),
            "{seen_now:?}"
        );
    }
    // ...a call returns its text...
    assert_eq!(
        results[0],
        ("fixture_echo".into(), "echo: hi".into(), false),
        "{results:?}"
    );
    // ...an `isError` result is an error tool result...
    let (name, text, is_error) = &results[1];
    assert_eq!(name, "fixture_fail");
    assert!(
        *is_error && text.contains("fixture failure: nope"),
        "{results:?}"
    );
    // ...and the plugin's prompt section reached the request as a note,
    // without the harness's own sections.
    let note = &seen_now[0].last_user;
    assert!(
        note.contains("[Guidance from dsh plugins]")
            && note.contains("Fixture guidance: prefer fixture_echo for echoing."),
        "{note}"
    );
    assert!(!note.contains("DeepSeek Harness"), "{note}");
    assert!(
        !format!("{:?}", agent.messages()).contains("Fixture guidance"),
        "the note is never stored"
    );

    // Cancelling the run aborts the dsh tool through its signal.
    let mut rx = agent.prompt("slow").await;
    wait_file(&abort_file, |t| t == "started").await;
    agent.abort();
    tokio::time::timeout(Duration::from_secs(30), async {
        while rx.recv().await.is_some() {}
    })
    .await
    .expect("the cancelled run ends");
    agent.finish().await;
    // Not empty: writeFileSync truncates before it writes.
    let ended = wait_file(&abort_file, |t| t != "started" && !t.is_empty()).await;
    assert_eq!(ended, "aborted: AbortError");
    host.root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn dsh_images_arrive_as_images_when_an_attachment_store_is_loaded() {
    let Some(runtime) = node_runtime() else {
        return;
    };
    for with_store in [true, false] {
        let host = host(&runtime).await;
        let mut rows = vec![
            json!({ "id": "system-prompt", "name": "@deepseek-ai/dsh-system-prompt" }),
            json!({ "id": "tools", "name": "@deepseek-ai/dsh-tools" }),
            json!({ "id": "fixture", "name": dsh_dir().join("fixture-tools.ts") }),
            json!({ "id": "adapter", "name": dsh_dir().join("dsh-tools-adapter.ts") }),
        ];
        if with_store {
            // Loaded after the adapter: it is looked up when an image is read.
            rows.push(
                json!({ "id": "attachments", "name": dsh_dir().join("fixture-attachments.ts") }),
            );
        }
        host.load(Value::Array(rows)).await;
        let (agent, _) = agent(vec![call("fixture_dot", json!({})), text("done")]);
        let mut agent = agent.with_extension(host.bridge.extension());
        run(&mut agent, "go").await;
        let content = agent
            .messages()
            .iter()
            .find_map(|m| match m {
                AgentMessage::Llm(Message::ToolResult {
                    tool_name, content, ..
                }) if tool_name == "fixture_dot" => Some(content.clone()),
                _ => None,
            })
            .expect("fixture_dot ran");
        assert!(
            matches!(&content[0], Content::Text { text } if text == "a dot"),
            "{content:?}"
        );
        if with_store {
            // The attachment's bytes, read from the store, as a yoagent image.
            assert!(
                matches!(&content[1], Content::Image { data, mime_type }
                    if mime_type == "image/png" && data.starts_with("iVBORw0KGgo")),
                "{content:?}"
            );
        } else {
            assert!(
                matches!(&content[1], Content::Text { text } if text.contains("not available")),
                "{content:?}"
            );
        }
        host.root.shutdown().await.unwrap();
    }
}
