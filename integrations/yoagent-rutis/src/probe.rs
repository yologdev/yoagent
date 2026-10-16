//! Loads one third-party plugin package through its adapter, as an agent
//! would, and reports what yoagent gets from it — without calling a tool or
//! asking a model. The compatibility matrix (`scripts/ecosystem-matrix.mjs`,
//! through the `ecosystem_probe` example) and hosts that vet a plugin before
//! using it (`yobuild check`) share it.
//!
//! - [`pi`]: pi extension files (as pi's package manager resolved them)
//!   through the pi adapter. A tool the adapter refuses because it is named
//!   like a yoagent built-in is tried again with that built-in left out
//!   (`withoutBuiltins`), as a host would. Skills are loaded with yoagent's
//!   own skills loader.
//! - [`dsh`]: DSH plugin rows over DSH's base packages and the tools
//!   adapter, in a Node runtime whose `package.json` is the base folder's.
//!   Reports what the rows add to the base — tools and prompt guidance — and
//!   which of the services the plugin injects exist (a plugin missing one
//!   never starts, though its row reads as started to the host).
//!
//! Loading a package runs its code, as its own host would. The adapters
//! need their npm dependencies installed (`npm ci` in a copy of
//! [`adapters`](crate::adapters)' folders, or in the crate's own
//! when it is a path dependency).
//!
//! What plugins log reaches [`Logs`] when the host installs it as a global
//! `tracing` layer: the runtime logs from its own worker threads.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rutis::{Ctx, FiberState};
use rutis_bridge::runtime::{Launcher, LocalRuntime};
use rutis_loader::{
    Chain, EntryStatus, Layer, Loader, LoaderOptions, LoaderPlugin, Patch, RuntimeResolver,
    RuntimeRowsPlugin, ServiceCatalog,
};
use serde::Serialize;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use yoagent::extension::{RunContext, RunEnd, RunOutcome, TurnDecision};
use yoagent::{Extension, Message, SkillSet, TurnContext};

use crate::adapters::adapters;
use crate::RutisBridge;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// How long rows may take to start, and the adapter to register.
const SETTLE: Duration = Duration::from_secs(60);

/// One line a plugin logged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct LogLine {
    /// `error`, `warn`, `info`, `debug` or `trace`.
    pub level: String,
    pub message: String,
}

/// Collects what plugins log (target `yoagent_rutis::plugin`), as a
/// `tracing` layer. Install it globally (the runtime logs from its own
/// threads) and pass it to [`pi`] or [`dsh`]: each report then carries the
/// lines of its own load.
#[derive(Debug, Clone, Default)]
pub struct Logs(Arc<Mutex<Vec<LogLine>>>);

impl Logs {
    fn clear(&self) {
        self.0.lock().unwrap().clear();
    }

    fn lines(&self) -> Vec<LogLine> {
        self.0.lock().unwrap().clone()
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Logs {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        if event.metadata().target() != "yoagent_rutis::plugin" {
            return;
        }
        #[derive(Default)]
        struct Text(String);
        impl tracing::field::Visit for Text {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.0 = format!("{value:?}");
                }
            }
        }
        let mut text = Text::default();
        event.record(&mut text);
        self.0.lock().unwrap().push(LogLine {
            level: event.metadata().level().to_string().to_lowercase(),
            message: text.0,
        });
    }
}

/// A row's state once settled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct Row {
    pub id: String,
    /// `active`, `failed`, `unresolved`, `waiting` (still pending after the
    /// wait: a dependency never came), or another rutis state.
    pub state: String,
    pub error: Option<String>,
}

/// Skills, loaded with yoagent's skills loader.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct Skills {
    /// How many were given.
    pub found: usize,
    /// The names of those that loaded.
    pub loaded: Vec<String>,
    /// Why the others did not.
    pub errors: Vec<String>,
}

/// What the pi adapter made of extension files.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct PiReport {
    pub skills: Skills,
    /// Whether the adapter loaded the extensions; `None` when there were none.
    pub loaded: Option<bool>,
    /// Why it did not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub rows: Vec<Row>,
    /// The yoagent built-ins left out because an extension's tool has
    /// their name.
    pub without: Vec<String>,
    /// The tools a run is offered.
    pub tools: Vec<String>,
    /// The length of the note the first request carries (prompt additions).
    pub note_chars: usize,
    /// What the plugins logged during the load.
    pub logs: Vec<LogLine>,
}

/// What DSH plugin rows add to DSH's base.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[non_exhaustive]
pub struct DshReport {
    /// Which injected services exist, as the runtime saw it
    /// (`{ "missing": [...] }`, or `{ "error": … }`).
    pub services: Value,
    /// The plugin's rows (not the base's).
    pub rows: Vec<Row>,
    /// The tools the rows add.
    pub tools: Vec<String>,
    /// The length of the prompt guidance the rows add.
    pub guidance_chars: usize,
    /// Why the probe could not tell.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub logs: Vec<LogLine>,
}

struct Host {
    root: Ctx,
    bridge: RutisBridge,
    loader: Loader,
}

/// A rutis root with the bridge and one Node runtime (`runtime`, the
/// `@arcships/rutis-runtime` package) whose `package.json` is
/// `package_json`, where row names resolve; its process given `env`.
async fn host(
    runtime: &Path,
    package_json: &Path,
    env: &[(&str, &Path)],
) -> Result<Host, BoxError> {
    let root = Ctx::root()?;
    let bridge = RutisBridge::install(&root)?;
    let mut catalog = ServiceCatalog::new();
    catalog.register_shared("yoagent");
    let launcher = env
        .iter()
        .fold(Launcher::node(runtime), |launcher, (name, value)| {
            launcher.env(*name, *value)
        });
    let node = LocalRuntime::launcher("node", launcher, package_json);
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

fn row(loader: &Loader, id: &str) -> Row {
    let (state, error) = match loader.get(id).map(|info| info.status) {
        Some(EntryStatus::Running(s)) => match s.state {
            FiberState::Active => ("active".into(), None),
            FiberState::Failed => ("failed".into(), s.error.map(|e| e.to_string())),
            FiberState::Pending => ("waiting".into(), None),
            other => (format!("{other:?}").to_lowercase(), None),
        },
        Some(EntryStatus::Unresolved(e)) => ("unresolved".into(), Some(e.to_string())),
        Some(other) => (format!("{other:?}").to_lowercase(), None),
        None => ("missing".into(), None),
    };
    Row {
        id: id.to_owned(),
        state,
        error,
    }
}

/// Load the given layers in one reconcile and wait until each row is
/// active or failed (or the wait ends); each row's state.
async fn load(host: &Host, layers: Vec<(&str, Value)>) -> Result<Vec<Row>, BoxError> {
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
    let settled = |id: &String| {
        matches!(
            row(&host.loader, id).state.as_str(),
            "active" | "failed" | "unresolved"
        )
    };
    let _ = tokio::time::timeout(SETTLE, async {
        while !ids.iter().all(settled) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    Ok(ids.iter().map(|id| row(&host.loader, id)).collect())
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
fn skills(paths: &[PathBuf]) -> Skills {
    let mut out = Skills {
        found: paths.len(),
        ..Skills::default()
    };
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
            Some(skill) => out.loaded.push(skill.name.clone()),
            None => {
                // The loader's error about this skill, not a sibling's.
                let name = dir.file_name().map(|n| n.to_string_lossy().into_owned());
                let error = errs
                    .iter()
                    .map(|e| e.to_string())
                    .find(|e| name.as_deref().is_some_and(|n| e.contains(n)));
                out.errors.push(format!(
                    "{}: {}",
                    dir.display(),
                    error.unwrap_or_else(|| "not loaded, no error given".into())
                ));
            }
        }
    }
    out
}

/// The built-in yoagent tool's name a refusal names, if it is a same-name
/// refusal.
fn shadowed(error: &str) -> Option<String> {
    let rest = error.split("named like yoagent's built-in \"").nth(1)?;
    Some(rest.split('"').next()?.to_string())
}

/// Loads pi `extensions` (files, as pi's package manager resolved them) with
/// `cwd` as their working folder, and `skills` (`SKILL.md` paths or skill
/// folders) with yoagent's loader. Uses the pi adapter in this crate's
/// `plugins/pi`, which needs `npm ci` there.
pub async fn pi(
    cwd: &Path,
    extensions: &[String],
    skill_paths: &[PathBuf],
    logs: Option<&Logs>,
) -> PiReport {
    let pi_dir = adapters().pi;
    let runtime = pi_dir.join("node_modules/@arcships/rutis-runtime");
    let mut report = PiReport {
        skills: skills(skill_paths),
        ..PiReport::default()
    };
    if extensions.is_empty() {
        return report;
    }
    let mut without: Vec<String> = Vec::new();
    // A same-name refusal is retried with that built-in left out, a few times.
    for _ in 0..6 {
        if let Some(logs) = logs {
            logs.clear();
        }
        let host = match host(&runtime, &pi_dir.join("package.json"), &[]).await {
            Ok(host) => host,
            Err(e) => {
                report.loaded = Some(false);
                report.error = Some(format!("host: {e}"));
                return report;
            }
        };
        let rows = json!([{
            "id": "pi",
            "name": pi_dir.join("pi-extensions-adapter.ts"),
            "config": { "extensions": extensions, "cwd": cwd, "withoutBuiltins": without },
        }]);
        let rows = match load(&host, vec![("probe", rows)]).await {
            Ok(rows) => rows,
            Err(e) => {
                report.loaded = Some(false);
                report.error = Some(e.to_string());
                shutdown(host).await;
                return report;
            }
        };
        // A TS row reads active before its async `apply` ends: wait for the
        // adapter's handler, then read the row again (a refusal fails it late).
        let registered = rows[0].state == "active" && handler(&host, "pi-extensions").await;
        let pi_row = row(&host.loader, "pi");
        if let Some(name) = pi_row.error.as_deref().and_then(shadowed) {
            if !without.contains(&name) {
                without.push(name);
                shutdown(host).await;
                continue;
            }
        }
        report.without = without;
        if registered && pi_row.state == "active" {
            match offered(&host).await {
                Ok((tools, note)) => {
                    report.loaded = Some(true);
                    report.tools = tools;
                    report.note_chars = note.map_or(0, |n| n.len());
                }
                Err(e) => {
                    report.loaded = Some(false);
                    report.error = Some(format!("starting a run: {e}"));
                }
            }
        } else {
            report.loaded = Some(false);
            report.error =
                Some(pi_row.error.clone().unwrap_or_else(|| {
                    format!("the adapter did not register (row {})", pi_row.state)
                }));
        }
        report.rows = vec![pi_row];
        // Let late log lines arrive (a warning decides "works, partly"),
        // then again after shutdown, which flushes the runtime.
        tokio::time::sleep(Duration::from_millis(500)).await;
        shutdown(host).await;
        report.logs = logs.map(Logs::lines).unwrap_or_default();
        return report;
    }
    report.loaded = Some(false);
    report.error = Some("too many same-name tools".into());
    report
}

/// The row id of `plugins/dsh/probe-services.ts`; a failure of it is
/// reported under `services.error`.
const SERVICE_CHECK: &str = "probe-services";

/// A file of the DSH adapter folder, its copy in `base` when there is one:
/// it must resolve the base's Cordis.
fn dsh_file(base: &Path, name: &str) -> PathBuf {
    let copy = base.join(name);
    if copy.exists() {
        copy
    } else {
        adapters().dsh.join(name)
    }
}

/// DSH's base packages and the tools adapter: what every DSH plugin gets.
fn dsh_base_rows(base: &Path) -> Result<Value, BoxError> {
    // The services a headless DSH host runs (fs, attachments, credentials,
    // storage, session persistence, skills): DSH's own packages, listed in
    // `host-services.json` for every host to load.
    let path = adapters().dsh.join("host-services.json");
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut rows: Vec<Value> =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    rows.extend([
        json!({ "id": "base-web", "name": "@deepseek-ai/dsh-web" }),
        json!({ "id": "base-system-prompt", "name": "@deepseek-ai/dsh-system-prompt" }),
        json!({ "id": "base-tools", "name": "@deepseek-ai/dsh-tools" }),
        json!({ "id": "base-user-questions", "name": "@deepseek-ai/dsh-user-questions" }),
        json!({ "id": "base-adapter", "name": dsh_file(base, "dsh-tools-adapter.ts") }),
    ]);
    Ok(Value::Array(rows))
}

/// Loads DSH plugin `rows` (its bundle patch, or one row naming the
/// package) over DSH's base packages and the tools adapter, in a Node
/// runtime whose `package.json` is `base`'s (`base` holds DSH's base
/// packages and the plugin; with a copy of `dsh-tools-adapter.ts` when it
/// is not this crate's DSH adapter folder, so both resolve one Cordis), and
/// checks which of the services named in `inject` exist. Each probe gets a
/// DSH home of its own: `~/.dsh` and `~/.agents` are not handed to the
/// plugins (the process environment still is).
pub async fn dsh(base: &Path, rows: &Value, inject: &[String], logs: Option<&Logs>) -> DshReport {
    if let Some(logs) = logs {
        logs.clear();
    }
    let mut report = match dsh_load(base, rows, inject).await {
        Ok(report) => report,
        Err(e) => DshReport {
            error: Some(e.to_string()),
            ..DshReport::default()
        },
    };
    report.logs = logs.map(Logs::lines).unwrap_or_default();
    report
}

async fn dsh_load(base: &Path, rows: &Value, inject: &[String]) -> Result<DshReport, BoxError> {
    let mut report = DshReport::default();
    let runtime = base.join("node_modules/@arcships/rutis-runtime");
    let dsh_home = tempfile::tempdir()?;
    let agents_home = dsh_home.path().join("agents");
    let base_rows = dsh_base_rows(base)?;
    let base_ids: Vec<String> = base_rows
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| row["id"].as_str().map(str::to_owned))
        .collect();
    let host = host(
        &runtime,
        &base.join("package.json"),
        &[
            ("DSH_HOME", dsh_home.path()),
            ("DSH_AGENTS_HOME", &agents_home),
        ],
    )
    .await
    .map_err(|e| format!("host: {e}"))?;
    // The base alone first, then the plugin's rows on top: the difference is
    // what the plugin adds.
    let base_state = load(&host, vec![("base", base_rows.clone())]).await;
    let ready = matches!(&base_state, Ok(rows) if rows.iter().all(|r| r.state == "active"))
        && handler(&host, "dsh-tools").await;
    if !ready {
        shutdown(host).await;
        return Err(format!("the DSH base did not start: {base_state:?}").into());
    }
    let before = offered(&host).await;
    // The plugin's rows, and a check (from inside the runtime) of which of
    // the services it injects exist: without one it never starts.
    let out = std::env::temp_dir().join(format!(
        "yoagent-probe-services-{}-{}.json",
        std::process::id(),
        dsh_home
            .path()
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    ));
    let _ = std::fs::remove_file(&out);
    let mut plugin_rows = rows.as_array().cloned().unwrap_or_default();
    plugin_rows.push(json!({
        "id": SERVICE_CHECK,
        "name": dsh_file(base, "probe-services.ts"),
        "config": { "names": inject, "out": out, "waitMs": 3000 },
    }));
    let state = load(
        &host,
        vec![("base", base_rows), ("plugin", json!(plugin_rows))],
    )
    .await;
    report.services = tokio::time::timeout(Duration::from_secs(20), async {
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
        let check = row(&host.loader, SERVICE_CHECK);
        let error = check.error.map(|e| format!(": {e}")).unwrap_or_default();
        json!({ "error": format!("no report within 20 s (row {}{error})", check.state) })
    });
    let _ = std::fs::remove_file(&out);
    let after = offered(&host).await;
    match (state, before, after) {
        (Ok(state), Ok((tools_before, note_before)), Ok((tools_after, note_after))) => {
            report.rows = state
                .into_iter()
                .filter(|r| !base_ids.contains(&r.id) && r.id != SERVICE_CHECK)
                .collect();
            report.tools = tools_after
                .into_iter()
                .filter(|t| !tools_before.contains(t))
                .collect();
            let (before, after) = (
                note_before.unwrap_or_default(),
                note_after.unwrap_or_default(),
            );
            report.guidance_chars = after
                .lines()
                .filter(|line| !line.trim().is_empty() && !before.contains(line))
                .map(|line| line.len() + 1)
                .sum();
        }
        (state, before, after) => {
            report.error = Some(format!(
                "{:?} {:?} {:?}",
                state.err(),
                before.err(),
                after.err()
            ));
        }
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    shutdown(host).await;
    Ok(report)
}
