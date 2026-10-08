//! A TypeScript plugin's diagnostics reach the host's `tracing` output
//! through the bridge's `log` method (target `yoagent_rutis::plugin`), not
//! only the plugin process's stderr: the pi adapter reports what it ignores.
//!
//! Its own test binary with a single test: it installs a global subscriber
//! (the bridge logs from Tokio's worker threads). Needs Node 24+ and `npm ci`
//! in `plugins/pi/`; without them it prints `SKIPPED:` and passes, or fails
//! with `YOAGENT_RUTIS_REQUIRE_RUNTIMES=1`.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rutis::Ctx;
use rutis_bridge::runtime::LocalRuntime;
use rutis_loader::{
    Chain, Layer, LoaderOptions, LoaderPlugin, Patch, RuntimeResolver, RuntimeRowsPlugin,
    ServiceCatalog,
};
use serde_json::json;
use tracing_subscriber::layer::SubscriberExt;
use yoagent_rutis::RutisBridge;

/// Every event as `target level message`.
#[derive(Clone, Default)]
struct CapturedLogs(Arc<Mutex<Vec<String>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CapturedLogs {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        struct Message(String);
        impl tracing::field::Visit for Message {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.0 = format!("{value:?}");
                }
            }
        }
        let mut message = Message(String::new());
        event.record(&mut message);
        let meta = event.metadata();
        self.0
            .lock()
            .unwrap()
            .push(format!("{} {} {}", meta.target(), meta.level(), message.0));
    }
}

fn pi_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins/pi")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_warning_reaches_the_hosts_logs() {
    let runtime = pi_dir().join("node_modules/@arcships/rutis-runtime");
    if !runtime.join("package.json").exists() {
        if std::env::var_os("YOAGENT_RUTIS_REQUIRE_RUNTIMES").is_some_and(|v| v == "1") {
            panic!("the pi runtime is required but unavailable: run `npm ci` in plugins/pi/");
        }
        eprintln!("SKIPPED: run `npm ci` in plugins/pi/");
        return;
    }
    let logs = CapturedLogs::default();
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(logs.clone()))
        .unwrap();

    let root = Ctx::root().unwrap();
    let bridge = RutisBridge::install(&root).unwrap();
    let mut catalog = ServiceCatalog::new();
    catalog.register_shared("yoagent");
    let node = LocalRuntime::node(&runtime, pi_dir().join("package.json"));
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
    // The fixture registers a command: the adapter reports it as not available.
    let patches: Vec<Patch> = serde_json::from_value(json!([{ "insert": [{
        "id": "pi",
        "name": pi_dir().join("pi-extensions-adapter.ts"),
        "config": { "extensions": [pi_dir().join("fixture-extension.ts")] },
    }] }]))
    .unwrap();
    let report = loader
        .reconcile(vec![Layer::new("rows", patches)], None)
        .await
        .unwrap();
    assert!(report.failures.is_empty(), "{report:?}");

    let found = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let hit = logs
                .0
                .lock()
                .unwrap()
                .iter()
                .find(|l| {
                    l.starts_with("yoagent_rutis::plugin WARN") && l.contains("command /fixture")
                })
                .cloned();
            if let Some(line) = hit {
                return line;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "no plugin warning in the host's logs: {:?}",
            logs.0.lock().unwrap()
        )
    });
    assert!(found.contains("not available in yoagent"), "{found}");
    let _ = bridge;
    root.shutdown().await.unwrap();
}
