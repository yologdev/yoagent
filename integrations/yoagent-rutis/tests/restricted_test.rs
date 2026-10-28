//! A restricted runtime ([`RutisBridge::restrict_runtime`]), end to end: a
//! real Node runtime whose handlers get tools only — from a row, from no
//! row, and after the runtime restarts.
//!
//! Needs Node 22.19+ with `npm ci` run in `plugins/`; a missing runtime
//! skips (fails under `YOAGENT_RUTIS_REQUIRE_RUNTIMES=1`, as CI sets).
#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use rutis::Ctx;
use rutis_bridge::runtime::LocalRuntime;
use rutis_bridge::session::{host_key, HostDispatch, Reply as RpcReply, Value as RpcValue};
use rutis_loader::{
    Chain, Layer, Loader, LoaderOptions, LoaderPlugin, Patch, RuntimeResolver, RuntimeRowsPlugin,
    ServiceCatalog,
};
use serde_json::{json, Value};
use yoagent::ToolDecision;
use yoagent_rutis::RutisBridge;

fn plugins_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins")
}

/// The Node runtime package, if Node 22.19+ and `npm ci` in `plugins/` are there.
fn node_runtime() -> Option<PathBuf> {
    let skip = |why: String| -> Option<PathBuf> {
        if std::env::var_os("YOAGENT_RUTIS_REQUIRE_RUNTIMES").is_some_and(|v| v == "1") {
            panic!("Node is required (YOAGENT_RUTIS_REQUIRE_RUNTIMES=1) but unavailable: {why}");
        }
        eprintln!("SKIPPED: Node unavailable: {why}");
        None
    };
    let ok = Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success());
    if !ok {
        return skip("no `node` on PATH".into());
    }
    let runtime = plugins_dir().join("node_modules/@arcships/rutis-runtime");
    if !runtime.join("package.json").exists() {
        return skip(format!(
            "{} missing: run `npm ci` in plugins/",
            runtime.display()
        ));
    }
    Some(runtime)
}

/// Records what plugins say, as `probe.record(text)`.
#[derive(Clone, Default)]
struct Probe(Arc<Mutex<Vec<String>>>);

impl HostDispatch for Probe {
    fn invoke(&self, method: &str, args: RpcValue) -> RpcReply {
        if method == "record" {
            let text = args
                .list()?
                .into_iter()
                .next()
                .map(|v| v.json())
                .transpose()?;
            if let Some(Value::String(text)) = text {
                self.0.lock().unwrap().push(text);
            }
        }
        Ok(RpcValue::Undefined)
    }
    fn methods(&self) -> Option<Value> {
        Some(json!({ "record": "async" }))
    }
}

impl Probe {
    fn lines(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

/// A handler with every hook, each recorded as `<name> <hook>[ <tool>]`, and
/// a tool `<name>_tool`. With `config.spawn`, calling that tool registers a
/// second such handler, `<name>-more`: mid-run.
const JS_ALL: &str = r#"
import { definePlugin } from 'RUTIS'
export default definePlugin({
  inject: ['yoagent', 'probe'],
  apply(ctx, config) {
    const probe = ctx.use('probe')
    const yoagent = ctx.use('yoagent')
    const hooks = (name) => ({
      async tools() { return [{ name: `${name}_tool` }] },
      async call_tool(call) {
        if (call.tool === `${name}_tool` && config.spawn && !name.endsWith('-more')) {
          ctx.effect(yoagent.register(`${name}-more`, hooks(`${name}-more`), { events: ['agentEnd'] }))
        }
        return `${call.tool} ran`
      },
      async before_tool(call) { probe.record(`${name} before_tool ${call.tool}`) },
      async after_tool() { probe.record(`${name} after_tool`) },
      async before_model() { probe.record(`${name} before_model`) },
      async on_input() { probe.record(`${name} on_input`) },
      async on_stop() { probe.record(`${name} on_stop`) },
      async finish() { probe.record(`${name} finish`) },
      async on_event(event) { probe.record(`${name} on_event`) },
    })
    ctx.effect(yoagent.register(config.name, hooks(config.name), { events: ['agentEnd'], messages: true }))
  },
})
"#;

struct Host {
    root: Ctx,
    bridge: RutisBridge,
    loader: Loader,
    probe: Probe,
    runtime: rutis::FiberView,
    _dir: tempfile::TempDir,
    file: PathBuf,
}

/// A Node runtime the bridge marks restricted before it starts.
async fn host(node: &Path) -> Host {
    let dir = tempfile::tempdir().unwrap();
    let rutis = plugins_dir()
        .join("node_modules/@arcships/rutis/src/index.mjs")
        .canonicalize()
        .map(|p| format!("file://{}", p.display()))
        .unwrap();
    let file = dir.path().join("all.mjs");
    std::fs::write(&file, JS_ALL.replace("RUTIS", &rutis)).unwrap();
    let root = Ctx::root().unwrap();
    let bridge = RutisBridge::install(&root).unwrap();
    let probe = Probe::default();
    root.provide_as::<dyn HostDispatch>(host_key("probe"), Arc::new(probe.clone()))
        .unwrap();
    let mut catalog = ServiceCatalog::new();
    catalog.register_shared("yoagent");
    catalog.register_shared("probe");
    let runtime = LocalRuntime::node(node, plugins_dir().join("package.json"));
    bridge.restrict_runtime(runtime.handle());
    let resolver = Arc::new(RuntimeResolver::node(runtime.handle()).with_catalog(&catalog));
    let runtime = root.plugin(runtime);
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
        probe,
        runtime,
        _dir: dir,
        file,
    }
}

impl Host {
    async fn load(&self, name: &str, spawn: bool) {
        let row = json!({ "id": name, "name": self.file.to_string_lossy(), "config": { "name": name, "spawn": spawn } });
        let patches: Vec<Patch> = serde_json::from_value(json!([{ "insert": [row] }])).unwrap();
        let report = self
            .loader
            .reconcile(vec![Layer::new("rows", patches)], None)
            .await
            .unwrap();
        assert!(report.failures.is_empty(), "{report:?}");
    }

    async fn until_handlers(&self, names: &[&str]) {
        let registry = self.bridge.registry().clone();
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let have: Vec<String> = registry
                    .handlers()
                    .iter()
                    .map(|h| h.name().to_string())
                    .collect();
                if names.iter().all(|n| have.iter().any(|h| h == n)) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{names:?} not registered: {:?}", registry.handlers()));
    }

    /// Wait for the probe line `line` (deliveries are asynchronous).
    async fn until_line(&self, line: &str) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !self.probe.lines().iter().any(|l| l == line) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no `{line}`: {:?}", self.probe.lines()));
    }
}

/// A restricted runtime's handler gets `tools`, `before_tool` for its own
/// tool and `finish`; an unrestricted (Rust) handler in the same run gets
/// every call.
#[tokio::test(flavor = "multi_thread")]
async fn a_restricted_runtimes_handler_gets_tools_only() {
    let Some(node) = node_runtime() else {
        return;
    };
    let host = host(&node).await;
    host.load("boxed", false).await;
    host.until_handlers(&["boxed"]).await;
    let info = host.bridge.registry().handlers();
    let boxed = info.iter().find(|h| h.name() == "boxed").unwrap();
    assert!(boxed.restricted());
    assert_eq!(boxed.hooks(), ["tools", "before_tool", "finish"]);

    let plain = Arc::new(Mutex::new(Vec::<String>::new()));
    let record = plain.clone();
    let watch = host
        .root
        .plugin(plugin(handler("plain").with_before_tool(move |call| {
            record.lock().unwrap().push(call.tool.clone());
            ToolDecision::Allow
        })));
    wait_active(&watch).await;
    let own = Reply::new("own", "own ran");
    let (agent, seen) = agent(vec![
        call("boxed__boxed_tool", json!({})),
        call("own", json!({})),
        text("done"),
    ]);
    let mut agent = agent
        .with_tools(vec![Box::new(own)])
        .with_extension(host.bridge.extension());
    let (_, results) = run(&mut agent, "go").await;
    assert!(seen.lock().unwrap()[0]
        .tools
        .contains(&"boxed__boxed_tool".to_string()));
    assert_eq!(
        results,
        vec![
            ("boxed__boxed_tool".into(), "boxed_tool ran".into(), false),
            ("own".into(), "own ran".into(), false),
        ]
    );
    host.until_line("boxed finish").await;
    let lines = host.probe.lines();
    let boxed: Vec<&String> = lines.iter().filter(|l| l.starts_with("boxed ")).collect();
    assert_eq!(
        boxed,
        ["boxed before_tool boxed_tool", "boxed finish"],
        "{lines:?}"
    );
    assert_eq!(
        *plain.lock().unwrap(),
        ["boxed__boxed_tool", "own"],
        "an unrestricted policy sees every call"
    );
    host.root.shutdown().await.unwrap();
}

/// A handler a restricted runtime registers mid-run (its plugin's tool
/// registers it) joins the run: offered from the next turn under the
/// host's name, it judges its own call by its own name, runs it, and is
/// finished with the run.
#[tokio::test(flavor = "multi_thread")]
async fn a_handler_registered_mid_run_joins_the_run() {
    let Some(node) = node_runtime() else {
        return;
    };
    let host = host(&node).await;
    host.load("boxed", true).await;
    host.until_handlers(&["boxed"]).await;
    let (agent, seen) = agent(vec![
        call("boxed__boxed_tool", json!({})),
        call("boxed-more__boxed-more_tool", json!({})),
        text("done"),
    ]);
    let extension = host
        .bridge
        .extension()
        .with_restricted_tools(|handler, tool| Some(format!("{handler}__{tool}")));
    let late = extension.late_tools();
    let mut agent = agent.with_extension(extension).with_tool_source(late);
    let (_, results) = run(&mut agent, "go").await;
    let seen = seen.lock().unwrap().clone();
    assert!(
        !seen[0].tools.iter().any(|t| t.contains("more")),
        "{:?}",
        seen[0].tools
    );
    assert!(
        seen[1]
            .tools
            .contains(&"boxed-more__boxed-more_tool".to_string()),
        "{:?}",
        seen[1].tools
    );
    assert_eq!(
        results,
        vec![
            ("boxed__boxed_tool".into(), "boxed_tool ran".into(), false),
            (
                "boxed-more__boxed-more_tool".into(),
                "boxed-more_tool ran".into(),
                false
            ),
        ]
    );
    host.until_line("boxed-more finish").await;
    let lines = host.probe.lines();
    let more: Vec<&String> = lines
        .iter()
        .filter(|l| l.starts_with("boxed-more "))
        .collect();
    assert_eq!(
        more,
        [
            "boxed-more before_tool boxed-more_tool",
            "boxed-more finish"
        ],
        "{lines:?}"
    );
    host.root.shutdown().await.unwrap();
}

/// After the runtime restarts (a new process), its handlers are restricted
/// still: the mark is on the runtime, not on a process.
#[tokio::test(flavor = "multi_thread")]
async fn a_restarted_restricted_runtime_stays_restricted() {
    let Some(node) = node_runtime() else {
        return;
    };
    let host = host(&node).await;
    host.load("boxed", false).await;
    host.until_handlers(&["boxed"]).await;
    host.runtime.restart().await.unwrap();
    // The old process's handler goes with its session; the row registers again.
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let handlers = host.bridge.registry().handlers();
            if let Some(h) = handlers.iter().find(|h| h.name() == "boxed") {
                if h.restricted() {
                    return;
                }
                panic!("re-registered unrestricted: {:?}", h.hooks());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the row registers again after the restart");
    host.root.shutdown().await.unwrap();
}
