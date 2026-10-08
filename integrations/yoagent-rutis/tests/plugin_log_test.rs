//! A TypeScript plugin's diagnostics reach the host's `tracing` output
//! through the bridge's `log` method (target `yoagent_rutis::plugin`), not
//! only the plugin process's stderr — with the run they belong to, when the
//! plugin says, as a `run_id` field.
//!
//! Its own test binary with a single test: it installs a global subscriber
//! (the bridge logs from Tokio's worker threads). Needs Node 24+ and `npm ci`
//! in `plugins/`; without them it prints `SKIPPED:` and passes, or fails
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

/// Every event as `target level run_id=<..> message`.
#[derive(Clone, Default)]
struct CapturedLogs(Arc<Mutex<Vec<String>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CapturedLogs {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        #[derive(Default)]
        struct Fields {
            message: String,
            run_id: String,
        }
        impl tracing::field::Visit for Fields {
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                if field.name() == "run_id" {
                    self.run_id = value.to_string();
                }
            }
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.message = format!("{value:?}");
                }
            }
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        let meta = event.metadata();
        self.0.lock().unwrap().push(format!(
            "{} {} run_id={} {}",
            meta.target(),
            meta.level(),
            fields.run_id,
            fields.message
        ));
    }
}

fn plugins_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins")
}

/// Logs twice when it loads: a warning for a run, an error without one.
const JS_LOGGER: &str = r#"
import { definePlugin } from 'RUTIS'
export default definePlugin({
  inject: ['yoagent'],
  apply(ctx) {
    const yoagent = ctx.use('yoagent')
    yoagent.log('warn', 'hello from js', { run_id: 'run-7' })
    yoagent.log('error', 'no run here')
  },
})
"#;

#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_log_reaches_the_hosts_tracing_with_its_run() {
    let runtime = plugins_dir().join("node_modules/@arcships/rutis-runtime");
    if !runtime.join("package.json").exists() {
        if std::env::var_os("YOAGENT_RUTIS_REQUIRE_RUNTIMES").is_some_and(|v| v == "1") {
            panic!("the Node runtime is required but unavailable: run `npm ci` in plugins/");
        }
        eprintln!("SKIPPED: run `npm ci` in plugins/");
        return;
    }
    let logs = CapturedLogs::default();
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(logs.clone()))
        .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let rutis = plugins_dir()
        .join("node_modules/@arcships/rutis/src/index.mjs")
        .canonicalize()
        .map(|p| format!("file://{}", p.display()))
        .unwrap();
    let logger = dir.path().join("logger.mjs");
    std::fs::write(&logger, JS_LOGGER.replace("RUTIS", &rutis)).unwrap();

    let root = Ctx::root().unwrap();
    let _bridge = RutisBridge::install(&root).unwrap();
    let mut catalog = ServiceCatalog::new();
    catalog.register_shared("yoagent");
    let node = LocalRuntime::node(&runtime, plugins_dir().join("package.json"));
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
    let patches: Vec<Patch> = serde_json::from_value(json!([{ "insert": [{
        "id": "logger",
        "name": logger.to_string_lossy(),
    }] }]))
    .unwrap();
    loader
        .reconcile(vec![Layer::new("rows", patches)], None)
        .await
        .unwrap();

    let want = [
        "yoagent_rutis::plugin WARN run_id=run-7 hello from js",
        "yoagent_rutis::plugin ERROR run_id= no run here",
    ];
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let seen = logs.0.lock().unwrap().clone();
            if want.iter().all(|w| seen.iter().any(|l| l == w)) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("plugin logs missing: {:?}", logs.0.lock().unwrap()));
    root.shutdown().await.unwrap();
}
