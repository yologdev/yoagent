//! Loads one third-party plugin package through its adapter, as an agent
//! would, and prints what yoagent gets from it as one JSON line. The
//! compatibility matrix (`scripts/ecosystem-matrix.mjs`) runs it per package;
//! it installs each package and resolves what is in it first.
//!
//! - `pi --cwd DIR [--skill PATH]... EXTENSION...`: pi extension files (as
//!   pi's package manager resolved them) through the pi adapter. When the
//!   adapter refuses a tool named like a yoagent built-in, the load is tried
//!   again with that built-in left out (`withoutBuiltins`), as a host would.
//!   Skills are loaded with yoagent's own skills loader.
//! - `dsh --base DIR --rows JSON [--inject SERVICE]...`: DSH plugin rows (its bundle patch, or one
//!   row naming the package) over DSH's base packages and the tools adapter,
//!   in a Node runtime whose `package.json` is `DIR`'s (with a copy of
//!   `plugins/dsh/dsh-tools-adapter.ts` in `DIR` when it is not this crate's
//!   `plugins/dsh`, so both resolve one Cordis). Reports what the rows add
//!   to the base: tools and prompt guidance, and which of the services the
//!   plugin injects exist (`plugins/dsh/probe-services.ts`, from inside the
//!   runtime: a plugin missing one never starts, though its row reads as
//!   started to the host).
//!
//! Nothing is called: tools are listed, not run, and the model is never
//! asked. Loading a package runs its code, as its own host would.
//!
//! Run: `cargo run --manifest-path integrations/yoagent-rutis/Cargo.toml --features node --example ecosystem_probe -- pi --cwd /tmp/x ext.ts`

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rutis::{Ctx, FiberState};
use rutis_bridge::runtime::LocalRuntime;
use rutis_loader::{
    Chain, EntryStatus, Layer, Loader, LoaderOptions, LoaderPlugin, Patch, RuntimeResolver,
    RuntimeRowsPlugin, ServiceCatalog,
};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::layer::SubscriberExt;
use yoagent::extension::{RunContext, RunEnd, RunOutcome, TurnDecision};
use yoagent::{Extension, Message, SkillSet, TurnContext};
use yoagent_rutis::RutisBridge;

type BoxError = Box<dyn std::error::Error>;

/// How long rows may take to start, and the adapter to register.
const SETTLE: Duration = Duration::from_secs(60);

fn plugins_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins")
}

/// Every plugin log line (the adapters report through the bridge's `log`).
#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<Value>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Logs {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        if event.metadata().target() != "yoagent_rutis::plugin" {
            return;
        }
        #[derive(Default)]
        struct Message(String);
        impl tracing::field::Visit for Message {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.0 = format!("{value:?}");
                }
            }
        }
        let mut message = Message::default();
        event.record(&mut message);
        let level = event.metadata().level().to_string().to_lowercase();
        self.0
            .lock()
            .unwrap()
            .push(json!({ "level": level, "message": message.0 }));
    }
}

struct Host {
    root: Ctx,
    bridge: RutisBridge,
    loader: Loader,
}

/// A rutis root with the bridge and one Node runtime whose `package.json` is
/// `package_json` (where row names resolve).
async fn host(runtime: &Path, package_json: &Path) -> Result<Host, BoxError> {
    let root = Ctx::root()?;
    let bridge = RutisBridge::install(&root)?;
    let mut catalog = ServiceCatalog::new();
    catalog.register_shared("yoagent");
    let node = LocalRuntime::node(runtime, package_json);
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
    root.plugin(plugin).await?;
    root.plugin(RuntimeRowsPlugin::new(resolver));
    Ok(Host {
        root,
        bridge,
        loader,
    })
}

/// One row's state once settled: `active`, `failed`, `unresolved`, or
/// `waiting` (still pending after the wait: a dependency never came).
fn row_state(loader: &Loader, id: &str) -> (String, Option<String>) {
    match loader.get(id).map(|info| info.status) {
        Some(EntryStatus::Running(s)) => match s.state {
            FiberState::Active => ("active".into(), None),
            FiberState::Failed => ("failed".into(), s.error.map(|e| e.to_string())),
            FiberState::Pending => ("waiting".into(), None),
            other => (format!("{other:?}").to_lowercase(), None),
        },
        Some(EntryStatus::Unresolved(e)) => ("unresolved".into(), Some(e.to_string())),
        Some(other) => (format!("{other:?}").to_lowercase(), None),
        None => ("missing".into(), None),
    }
}

/// Load the given layers in one reconcile and wait until each row is
/// active or failed (or the wait ends); each row's state.
async fn load(host: &Host, layers: Vec<(&str, Value)>) -> Result<Vec<Value>, BoxError> {
    let mut ids = Vec::new();
    let mut built = Vec::new();
    for (name, rows) in layers {
        ids.extend(
            rows.as_array()
                .into_iter()
                .flatten()
                .filter_map(|row| row["id"].as_str().map(str::to_owned)),
        );
        let patches: Vec<Patch> = serde_json::from_value(json!([{ "insert": rows }]))?;
        built.push(Layer::new(name, patches));
    }
    host.loader.reconcile(built, None).await?;
    // Rows still pending when the wait ends are reported as `waiting`.
    let _ = tokio::time::timeout(SETTLE, async {
        while !ids.iter().all(|id| {
            matches!(
                row_state(&host.loader, id).0.as_str(),
                "active" | "failed" | "unresolved"
            )
        }) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    Ok(ids
        .iter()
        .map(|id| {
            let (state, error) = row_state(&host.loader, id);
            json!({ "id": id, "state": state, "error": error })
        })
        .collect())
}

/// Wait until a handler named `name` is registered.
async fn handler(host: &Host, name: &str) -> bool {
    tokio::time::timeout(SETTLE, async {
        while !host
            .bridge
            .registry()
            .handlers()
            .iter()
            .any(|h| h.name() == name)
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .is_ok()
}

/// The tools a run would be offered, and the note its first request would carry.
async fn offered(host: &Host) -> Result<(Vec<String>, Option<String>), BoxError> {
    let cancel = CancellationToken::new();
    let prompt = [Message::user("hello")];
    let run = RunContext::new("probe", &prompt, &cancel);
    let mut hooks = host.bridge.extension().start_run(&run).await?;
    let mut tools: Vec<String> = hooks
        .tools(&run)
        .await
        .iter()
        .map(|t| t.name().to_string())
        .collect();
    tools.sort();
    let turn = TurnContext::new("", &prompt, &[], "probe").with_run_prompts(&prompt);
    let note = match hooks.before_model(&turn).await {
        TurnDecision::Note(note) => Some(note),
        _ => None,
    };
    hooks
        .finish(&RunOutcome::new(RunEnd::Completed, None))
        .await;
    Ok((tools, note))
}

async fn shutdown(host: Host) {
    let _ = tokio::time::timeout(Duration::from_secs(10), host.root.shutdown()).await;
}

/// `SKILL.md` paths or skill directories, loaded with yoagent's loader.
fn skills(paths: &[PathBuf]) -> Value {
    let mut loaded = Vec::new();
    let mut errors = Vec::new();
    for path in paths {
        let dir = if path.ends_with("SKILL.md") {
            path.parent().unwrap_or(path)
        } else {
            path
        };
        let Some(parent) = dir.parent() else { continue };
        let (set, errs) = SkillSet::load_dir_resilient(parent, "probe");
        // The loader keeps canonical paths (on macOS /var is /private/var).
        let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        match set.skills().iter().find(|s| s.base_dir == dir) {
            Some(skill) => loaded.push(skill.name.clone()),
            None => {
                // The loader's error about this skill, not a sibling's.
                let name = dir.file_name().map(|n| n.to_string_lossy().into_owned());
                let error = errs
                    .iter()
                    .map(|e| e.to_string())
                    .find(|e| name.as_deref().is_some_and(|n| e.contains(n)));
                errors.push(format!(
                    "{}: {}",
                    dir.display(),
                    error.unwrap_or_else(|| "not loaded, no error given".into())
                ));
            }
        }
    }
    json!({ "found": paths.len(), "loaded": loaded, "errors": errors })
}

/// The built-in yoagent's tool `name` a refusal names, if it is a
/// same-name refusal.
fn shadowed(error: &str) -> Option<String> {
    let rest = error.split("named like yoagent's built-in \"").nth(1)?;
    Some(rest.split('"').next()?.to_string())
}

async fn probe_pi(
    logs: Logs,
    cwd: PathBuf,
    extensions: Vec<String>,
    skill_paths: Vec<PathBuf>,
) -> Value {
    let runtime = plugins_dir().join("pi/node_modules/@arcships/rutis-runtime");
    let mut without: Vec<String> = Vec::new();
    let mut result = json!({ "ecosystem": "pi", "skills": skills(&skill_paths) });
    if extensions.is_empty() {
        result["loaded"] = json!(null);
        return result;
    }
    // A same-name refusal is retried with that built-in left out, a few times.
    for _ in 0..6 {
        logs.0.lock().unwrap().clear();
        let host = match host(&runtime, &plugins_dir().join("pi/package.json")).await {
            Ok(host) => host,
            Err(e) => {
                result["loaded"] = json!(false);
                result["error"] = json!(format!("host: {e}"));
                return result;
            }
        };
        let row = json!([{
            "id": "pi",
            "name": plugins_dir().join("pi/pi-extensions-adapter.ts"),
            "config": { "extensions": extensions, "cwd": cwd, "withoutBuiltins": without },
        }]);
        let rows = match load(&host, vec![("probe", row)]).await {
            Ok(rows) => rows,
            Err(e) => {
                result["loaded"] = json!(false);
                result["error"] = json!(e.to_string());
                shutdown(host).await;
                return result;
            }
        };
        // A TS row reads active before its async `apply` ends: wait for the
        // adapter's handler, then read the row again (a refusal fails it late).
        let registered = rows[0]["state"] == "active" && handler(&host, "pi-extensions").await;
        let (state, error) = row_state(&host.loader, "pi");
        if let Some(name) = error.as_deref().and_then(shadowed) {
            if !without.contains(&name) {
                without.push(name);
                shutdown(host).await;
                continue;
            }
        }
        result["rows"] = json!([{ "id": "pi", "state": state, "error": error }]);
        result["without"] = json!(without);
        if registered && state == "active" {
            match offered(&host).await {
                Ok((tools, note)) => {
                    result["loaded"] = json!(true);
                    result["tools"] = json!(tools);
                    result["note_chars"] = json!(note.map_or(0, |n| n.len()));
                }
                Err(e) => {
                    result["loaded"] = json!(false);
                    result["error"] = json!(format!("starting a run: {e}"));
                }
            }
        } else {
            result["loaded"] = json!(false);
            result["error"] = json!(
                error.unwrap_or_else(|| format!("the adapter did not register (row {state})"))
            );
        }
        // Let late log lines arrive (a warning decides "works, partly"),
        // then again after shutdown, which flushes the runtime.
        tokio::time::sleep(Duration::from_millis(500)).await;
        shutdown(host).await;
        result["logs"] = json!(logs.0.lock().unwrap().clone());
        return result;
    }
    result["loaded"] = json!(false);
    result["error"] = json!("too many same-name tools");
    result
}

/// A file of `plugins/dsh`, its copy in `base` when there is one: it must
/// resolve the base's Cordis.
fn dsh_file(base: &Path, name: &str) -> PathBuf {
    let copy = base.join(name);
    if copy.exists() {
        copy
    } else {
        plugins_dir().join("dsh").join(name)
    }
}

/// The row id of `plugins/dsh/probe-services.ts`; a failure of it is
/// reported under `services.error`.
const SERVICE_CHECK: &str = "probe-services";

/// DSH's base packages and the tools adapter: what every DSH plugin gets.
fn dsh_base_rows(base: &Path) -> Value {
    let adapter = dsh_file(base, "dsh-tools-adapter.ts");
    json!([
        { "id": "base-web", "name": "@deepseek-ai/dsh-web" },
        { "id": "base-system-prompt", "name": "@deepseek-ai/dsh-system-prompt" },
        { "id": "base-tools", "name": "@deepseek-ai/dsh-tools" },
        { "id": "base-user-questions", "name": "@deepseek-ai/dsh-user-questions" },
        { "id": "base-adapter", "name": adapter },
    ])
}

async fn probe_dsh(logs: Logs, base: PathBuf, rows: Value, inject: Vec<String>) -> Value {
    let mut result = json!({ "ecosystem": "dsh" });
    let runtime = base.join("node_modules/@arcships/rutis-runtime");
    let host = match host(&runtime, &base.join("package.json")).await {
        Ok(host) => host,
        Err(e) => {
            result["error"] = json!(format!("host: {e}"));
            return result;
        }
    };
    // The base alone first, then the plugin's rows on top: the difference is
    // what the plugin adds.
    let base_state = load(&host, vec![("base", dsh_base_rows(&base))]).await;
    let ready = matches!(&base_state, Ok(rows) if rows.iter().all(|r| r["state"] == "active"))
        && handler(&host, "dsh-tools").await;
    if !ready {
        result["error"] = json!(format!("the DSH base did not start: {base_state:?}"));
        shutdown(host).await;
        return result;
    }
    let before = offered(&host).await;
    // The plugin's rows, and a check (from inside the runtime) of which of
    // the services it injects exist: without one it never starts.
    let out = std::env::temp_dir().join(format!(
        "yoagent-probe-services-{}.json",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&out);
    let mut plugin_rows = rows.as_array().cloned().unwrap_or_default();
    plugin_rows.push(json!({
        "id": SERVICE_CHECK,
        "name": dsh_file(&base, "probe-services.ts"),
        "config": { "names": inject, "out": out, "waitMs": 3000 },
    }));
    let state = load(
        &host,
        vec![
            ("base", dsh_base_rows(&base)),
            ("plugin", json!(plugin_rows)),
        ],
    )
    .await;
    let services = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(found) = std::fs::read_to_string(&out)
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            {
                return found;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        let (state, error) = row_state(&host.loader, SERVICE_CHECK);
        json!({ "error": format!("no report within 20 s (row {state}{})", error.map(|e| format!(": {e}")).unwrap_or_default()) })
    });
    let _ = std::fs::remove_file(&out);
    result["services"] = services;
    let after = offered(&host).await;
    match (state, before, after) {
        (Ok(state), Ok((tools_before, note_before)), Ok((tools_after, note_after))) => {
            let plugin_rows: Vec<Value> = state
                .into_iter()
                .filter(|r| {
                    let id = r["id"].as_str().unwrap_or("");
                    !id.starts_with("base-") && id != SERVICE_CHECK
                })
                .collect();
            let tools: Vec<String> = tools_after
                .into_iter()
                .filter(|t| !tools_before.contains(t))
                .collect();
            let (before, after) = (
                note_before.unwrap_or_default(),
                note_after.unwrap_or_default(),
            );
            let guidance: Vec<&str> = after
                .lines()
                .filter(|line| !line.trim().is_empty() && !before.contains(line))
                .collect();
            result["rows"] = json!(plugin_rows);
            result["tools"] = json!(tools);
            result["guidance_chars"] = json!(guidance.iter().map(|l| l.len() + 1).sum::<usize>());
        }
        (state, before, after) => {
            result["error"] = json!(format!(
                "{:?} {:?} {:?}",
                state.err(),
                before.err(),
                after.err()
            ));
        }
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    shutdown(host).await;
    result["logs"] = json!(logs.0.lock().unwrap().clone());
    result
}

fn usage() -> BoxError {
    "usage: ecosystem_probe pi --cwd DIR [--skill PATH]... EXTENSION... | dsh --base DIR --rows JSON [--inject SERVICE]..."
        .into()
}

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    let mut args = std::env::args().skip(1);
    let mode = args.next().ok_or_else(usage)?;
    let mut cwd = None;
    let mut base = None;
    let mut rows = None;
    let mut inject = Vec::new();
    let mut skill_paths = Vec::new();
    let mut extensions = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--cwd" => cwd = args.next().map(PathBuf::from),
            "--base" => base = args.next().map(PathBuf::from),
            "--rows" => {
                rows = Some(serde_json::from_str::<Value>(
                    &args.next().ok_or_else(usage)?,
                )?)
            }
            "--skill" => skill_paths.extend(args.next().map(PathBuf::from)),
            "--inject" => inject.extend(args.next()),
            _ => extensions.push(arg),
        }
    }
    // Global: the bridge logs from the runtime's worker threads.
    let logs = Logs::default();
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(logs.clone()))?;
    let result = match mode.as_str() {
        "pi" => probe_pi(logs, cwd.ok_or_else(usage)?, extensions, skill_paths).await,
        "dsh" => {
            probe_dsh(
                logs,
                base.ok_or_else(usage)?,
                rows.ok_or_else(usage)?,
                inject,
            )
            .await
        }
        _ => return Err(usage()),
    };
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}
