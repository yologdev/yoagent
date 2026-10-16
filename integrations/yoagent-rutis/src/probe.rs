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
//!   which of the services the rows' plugins inject exist (a plugin missing
//!   one never starts, though its row reads as started to the host).
//!
//! **The adapters, installed.** Both take `plugins`: a folder laid out like
//! [`adapters`](crate::adapters)' `root` (the pi and DSH adapters beside
//! `yoagent.d.ts`, which they import), with `npm ci` run in its `pi/` and
//! `dsh/`. Copy `root` there — never install into the crate's own files, in
//! cargo's registry — or, for a path dependency, pass `root` itself.
//!
//! **A verdict.** Each report's `problem()` says what kept the package from
//! giving an agent what it offers, if anything; `None` is a clean load.
//!
//! **One at a time.** Probes in one process run one after another (a second
//! waits): [`Logs`] is one buffer for the process, and each report carries
//! the lines of its own load. Loading a package runs its code, as its own
//! host would.

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

use crate::RutisBridge;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// How long rows may take to start, and the adapter to register.
const SETTLE: Duration = Duration::from_secs(60);

/// Probes in this process, one at a time (their logs share [`Logs`]).
static ONE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The `tracing` targets [`Logs`] collects: what plugins log, and the
/// bridge's own reports of a hook that failed.
const LOG_TARGETS: [&str; 2] = ["yoagent_rutis::plugin", "yoagent_rutis::extension"];

/// One line a plugin (or the bridge, about one) logged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct LogLine {
    /// `error`, `warn`, `info`, `debug` or `trace`.
    pub level: String,
    pub message: String,
}

/// Collects what plugins log (target `yoagent_rutis::plugin`) and the
/// bridge's reports of their failed hooks (`yoagent_rutis::extension`), as
/// a `tracing` layer. Install it globally (the runtime logs from its own
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
        if !LOG_TARGETS.contains(&event.metadata().target()) {
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
    /// Whether the adapter loaded the extensions and a run would get them;
    /// `None` when there were none.
    pub loaded: Option<bool>,
    /// Why not: the load's error, or the adapter's refusal at the first
    /// model request (a handler it cannot honour, found after load).
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

impl PiReport {
    /// What kept the extensions from loading, if anything: `None` when they
    /// loaded (or there were none).
    pub fn problem(&self) -> Option<String> {
        match self.loaded {
            Some(false) => Some(
                self.error
                    .clone()
                    .unwrap_or_else(|| "not loaded, no error given".into()),
            ),
            _ => None,
        }
    }
}

/// What DSH plugin rows add to DSH's base.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[non_exhaustive]
pub struct DshReport {
    /// Which services the plugin's rows inject, as the runtime saw it:
    /// `{ "present": [...], "missing": [...], "importErrors": {...} }`
    /// (a row whose module could not be imported: what it injects is
    /// unknown), or `{ "error": … }`; `null` when the probe did not get
    /// that far (see `error`).
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

impl DshReport {
    /// What kept the plugin from giving an agent what it offers, if
    /// anything: the probe's error, a service check that did not report, a
    /// service the plugin needs and the host lacks, or a row not active.
    /// `None` is a clean load.
    pub fn problem(&self) -> Option<String> {
        if let Some(error) = &self.error {
            return Some(error.clone());
        }
        if let Some(error) = self.services.get("error") {
            return Some(format!("the service check: {error}"));
        }
        let missing = self.services["missing"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if !missing.is_empty() {
            return Some(format!("needs services the host lacks: {}", json!(missing)));
        }
        if self.services["missing"].is_null() {
            return Some("the service check did not report".into());
        }
        if let Some(errors) = self.services["importErrors"].as_object() {
            if !errors.is_empty() {
                return Some(format!(
                    "could not read what these rows inject: {}",
                    Value::Object(errors.clone())
                ));
            }
        }
        self.rows
            .iter()
            .find(|r| r.state != "active")
            .map(|r| match &r.error {
                Some(e) => format!("row {} {}: {e}", r.id, r.state),
                None => format!("row {} {}", r.id, r.state),
            })
    }
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

/// The ids of `rows`: an array of rows, each with a string `id` (a row
/// without one would load but could be neither waited for nor reported).
fn row_ids(rows: &Value) -> Result<Vec<String>, BoxError> {
    let rows = rows.as_array().ok_or("rows: an array of rows")?;
    rows.iter()
        .map(|row| {
            row["id"]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("a row without a string id: {row}").into())
        })
        .collect()
}

/// Load the given layers in one reconcile and wait until each row is
/// active or failed (or the wait ends); each row's state.
async fn load(host: &Host, layers: Vec<(&str, Value)>) -> Result<Vec<Row>, BoxError> {
    let mut ids = Vec::new();
    let mut built = Vec::new();
    for (name, rows) in layers {
        ids.extend(row_ids(&rows)?);
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

/// What a run gets: the tools it is offered, and what its first model
/// request meets.
struct Offered {
    tools: Vec<String>,
    /// The note the request carries.
    note: Option<String>,
    /// A run stopped (or failed) before its first request: why.
    stopped: Option<String>,
}

async fn offered(host: &Host) -> Result<Offered, BoxError> {
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
    let (note, stopped) = match hooks.before_model(&turn).await {
        TurnDecision::Note(note) => (Some(note), None),
        TurnDecision::Stop(why) | TurnDecision::Fail(why) => (None, Some(why)),
        _ => (None, None),
    };
    hooks
        .finish(&RunOutcome::new(RunEnd::Completed, None))
        .await;
    Ok(Offered {
        tools,
        note,
        stopped,
    })
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
        // A bare `SKILL.md`: the current folder.
        let dir = if dir.as_os_str().is_empty() {
            Path::new(".")
        } else {
            dir
        };
        // The loader keeps canonical paths (on macOS /var is /private/var).
        let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        let Some(parent) = dir.parent() else {
            out.errors
                .push(format!("{}: not a skill folder", dir.display()));
            continue;
        };
        let (set, errs) = SkillSet::load_dir_resilient(parent, "probe");
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
/// refusal (the pi adapter's wording: `named like yoagent's built-in "…"`).
fn shadowed(error: &str) -> Option<String> {
    let rest = error.split("named like yoagent's built-in \"").nth(1)?;
    Some(rest.split('"').next()?.to_string())
}

/// Loads pi `extensions` (files, as pi's package manager resolved them) with
/// `cwd` as their working folder, and `skills` (`SKILL.md` paths or skill
/// folders) with yoagent's loader. `plugins`: the adapters, installed (see
/// the module's doc).
pub async fn pi(
    plugins: &Path,
    cwd: &Path,
    extensions: &[String],
    skill_paths: &[PathBuf],
    logs: Option<&Logs>,
) -> PiReport {
    let _one = ONE_AT_A_TIME.lock().await;
    let mut report = PiReport {
        skills: skills(skill_paths),
        ..PiReport::default()
    };
    if extensions.is_empty() {
        return report;
    }
    pi_load(&mut report, &plugins.join("pi"), cwd, extensions, logs).await;
    report.logs = logs.map(Logs::lines).unwrap_or_default();
    report
}

async fn pi_load(
    report: &mut PiReport,
    pi_dir: &Path,
    cwd: &Path,
    extensions: &[String],
    logs: Option<&Logs>,
) {
    let runtime = pi_dir.join("node_modules/@arcships/rutis-runtime");
    let mut without: Vec<String> = Vec::new();
    // A same-name refusal (at load, or at the first request for a tool
    // added later) is retried with that built-in left out, a few times.
    for _ in 0..6 {
        if let Some(logs) = logs {
            logs.clear();
        }
        report.without = without.clone();
        let host = match host(&runtime, &pi_dir.join("package.json"), &[]).await {
            Ok(host) => host,
            Err(e) => {
                report.loaded = Some(false);
                report.error = Some(format!("host: {e}"));
                return;
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
                return;
            }
        };
        // A TS row reads active before its async `apply` ends: wait for the
        // adapter's handler, then read the row again (a refusal fails it late).
        let registered = rows[0].state == "active" && handler(&host, "pi-extensions").await;
        let pi_row = row(&host.loader, "pi");
        let run = match (registered && pi_row.state == "active").then_some(()) {
            Some(()) => Some(offered(&host).await),
            None => None,
        };
        // Retried: a same-name refusal at load, or at the first request.
        let refusal = pi_row.error.clone().or_else(|| match &run {
            Some(Ok(offered)) => offered.stopped.clone(),
            _ => None,
        });
        if let Some(name) = refusal.as_deref().and_then(shadowed) {
            if !without.contains(&name) {
                without.push(name);
                shutdown(host).await;
                continue;
            }
        }
        match run {
            Some(Ok(offered)) => {
                report.tools = offered.tools;
                report.note_chars = offered.note.map_or(0, |n| n.len());
                match offered.stopped {
                    // The adapter refused after load: a run would stop there.
                    Some(why) => {
                        report.loaded = Some(false);
                        report.error = Some(why);
                    }
                    None => report.loaded = Some(true),
                }
            }
            Some(Err(e)) => {
                report.loaded = Some(false);
                report.error = Some(format!("starting a run: {e}"));
            }
            None => {
                report.loaded = Some(false);
                report.error = Some(pi_row.error.clone().unwrap_or_else(|| {
                    format!("the adapter did not register (row {})", pi_row.state)
                }));
            }
        }
        report.rows = vec![pi_row];
        // Let late log lines arrive (a warning decides "works, partly"),
        // then again after shutdown, which flushes the runtime.
        tokio::time::sleep(Duration::from_millis(500)).await;
        shutdown(host).await;
        return;
    }
    report.loaded = Some(false);
    report.error = Some("too many same-name tools".into());
}

/// The row id of `dsh/probe-services.ts`; a failure of it is reported under
/// `services.error`.
const SERVICE_CHECK: &str = "probe-services";

/// A file of the DSH adapter folder, as loaded from `base`: it must resolve
/// the base's Cordis (a copy elsewhere resolves another one, and the runtime
/// refuses it), so it is copied into `base` when not there yet.
fn dsh_file(dsh_dir: &Path, base: &Path, name: &str) -> Result<PathBuf, BoxError> {
    let copy = base.join(name);
    if !copy.exists() {
        std::fs::copy(dsh_dir.join(name), &copy)
            .map_err(|e| format!("copying {name} into {}: {e}", base.display()))?;
    }
    Ok(copy)
}

/// DSH's base packages and the tools adapter: what every DSH plugin gets.
fn dsh_base_rows(dsh_dir: &Path, base: &Path) -> Result<Value, BoxError> {
    // The services a headless DSH host runs (fs, attachments, credentials,
    // storage, session persistence, skills): DSH's own packages, listed in
    // `host-services.json` for every host to load.
    let path = dsh_dir.join("host-services.json");
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut rows: Vec<Value> =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    rows.extend([
        json!({ "id": "base-web", "name": "@deepseek-ai/dsh-web" }),
        json!({ "id": "base-system-prompt", "name": "@deepseek-ai/dsh-system-prompt" }),
        json!({ "id": "base-tools", "name": "@deepseek-ai/dsh-tools" }),
        json!({ "id": "base-user-questions", "name": "@deepseek-ai/dsh-user-questions" }),
        json!({ "id": "base-adapter", "name": dsh_file(dsh_dir, base, "dsh-tools-adapter.ts")? }),
    ]);
    Ok(Value::Array(rows))
}

/// Loads DSH plugin `rows` (its bundle patch, or one row naming the
/// package; an array of rows, each with a string `id`) over DSH's base
/// packages and the tools adapter, in a Node runtime whose `package.json`
/// is `base`'s (`base` holds DSH's base packages and the plugin), and checks
/// which of the services the rows' plugins inject exist — read from their
/// modules inside the runtime (every key: Cordis requires them all), plus any
/// named in `inject`. The probe's two DSH files (`dsh-tools-adapter.ts`,
/// `probe-services.ts`) must resolve `base`'s Cordis, so they are copied
/// into `base` when they are not there. `plugins`: the adapters, installed (see the module's doc).
/// Each probe gets a DSH home of its own: `~/.dsh` and `~/.agents` are not
/// handed to the plugins (the process environment still is).
pub async fn dsh(
    plugins: &Path,
    base: &Path,
    rows: &Value,
    inject: &[String],
    logs: Option<&Logs>,
) -> DshReport {
    let _one = ONE_AT_A_TIME.lock().await;
    if let Some(logs) = logs {
        logs.clear();
    }
    let mut report = match dsh_load(&plugins.join("dsh"), base, rows, inject).await {
        Ok(report) => report,
        Err(e) => DshReport {
            error: Some(e.to_string()),
            ..DshReport::default()
        },
    };
    report.logs = logs.map(Logs::lines).unwrap_or_default();
    report
}

async fn dsh_load(
    dsh_dir: &Path,
    base: &Path,
    rows: &Value,
    inject: &[String],
) -> Result<DshReport, BoxError> {
    let plugin_ids = row_ids(rows)?;
    let mut report = DshReport::default();
    let runtime = base.join("node_modules/@arcships/rutis-runtime");
    let dsh_home = tempfile::tempdir()?;
    let agents_home = dsh_home.path().join("agents");
    let base_rows = dsh_base_rows(dsh_dir, base)?;
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
    // the services they inject exist: without one a plugin never starts.
    let out = dsh_home.path().join("probe-services.json");
    let modules: Vec<&Value> = rows
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| row.get("name").filter(|n| n.is_string()))
        .collect();
    let mut plugin_rows = rows.as_array().cloned().unwrap_or_default();
    plugin_rows.push(json!({
        "id": SERVICE_CHECK,
        "name": dsh_file(dsh_dir, base, "probe-services.ts")?,
        "config": { "names": inject, "modules": modules, "base": base, "out": out, "waitMs": 3000 },
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
    let after = offered(&host).await;
    match (state, before, after) {
        (Ok(state), Ok(before), Ok(after)) => {
            report.rows = state
                .into_iter()
                .filter(|r| plugin_ids.contains(&r.id))
                .collect();
            report.tools = after
                .tools
                .into_iter()
                .filter(|t| !before.tools.contains(t))
                .collect();
            let (before, after) = (
                before.note.unwrap_or_default(),
                after.note.unwrap_or_default(),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(value: &Value) -> Vec<&str> {
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort();
        keys
    }

    #[test]
    fn the_reports_serialize_to_the_json_the_matrix_reads() {
        let pi = PiReport {
            rows: vec![Row {
                id: "pi".into(),
                state: "active".into(),
                error: None,
            }],
            logs: vec![LogLine {
                level: "warn".into(),
                message: "m".into(),
            }],
            ..PiReport::default()
        };
        let json = serde_json::to_value(&pi).unwrap();
        assert_eq!(
            keys(&json),
            [
                "loaded",
                "logs",
                "note_chars",
                "rows",
                "skills",
                "tools",
                "without"
            ]
        );
        assert_eq!(keys(&json["skills"]), ["errors", "found", "loaded"]);
        assert_eq!(keys(&json["rows"][0]), ["error", "id", "state"]);
        assert_eq!(keys(&json["logs"][0]), ["level", "message"]);
        // `error` only when there is one.
        let failed = PiReport {
            error: Some("x".into()),
            ..PiReport::default()
        };
        assert_eq!(serde_json::to_value(&failed).unwrap()["error"], "x");

        let dsh = DshReport {
            services: json!({ "present": [], "missing": [] }),
            ..DshReport::default()
        };
        let json = serde_json::to_value(&dsh).unwrap();
        assert_eq!(
            keys(&json),
            ["guidance_chars", "logs", "rows", "services", "tools"]
        );
        assert_eq!(keys(&json["services"]), ["missing", "present"]);
        let json = serde_json::to_value(DshReport::default()).unwrap();
        assert!(json["services"].is_null());
    }

    #[test]
    fn a_same_name_refusal_names_the_built_in() {
        // The pi adapter's wording (pi-extensions-adapter.ts, checkShadowing).
        let error = "the pi extensions adapter stopped: a pi extension's tool \"read_file\" is \
                     named like yoagent's built-in \"read_file\"; if the host installs that one \
                     too, it runs instead of the pi tool.";
        assert_eq!(shadowed(error).as_deref(), Some("read_file"));
        assert_eq!(shadowed("something else"), None);
    }

    #[test]
    fn a_verdict_from_each_report() {
        let clean = DshReport {
            services: json!({ "present": ["fs"], "missing": [] }),
            rows: vec![Row {
                id: "fx".into(),
                state: "active".into(),
                error: None,
            }],
            ..DshReport::default()
        };
        assert_eq!(clean.problem(), None);
        let missing = DshReport {
            services: json!({ "missing": ["db"] }),
            ..clean.clone()
        };
        assert_eq!(
            missing.problem().as_deref(),
            Some("needs services the host lacks: [\"db\"]")
        );
        let waiting = DshReport {
            rows: vec![Row {
                id: "fx".into(),
                state: "waiting".into(),
                error: None,
            }],
            ..clean.clone()
        };
        assert_eq!(waiting.problem().as_deref(), Some("row fx waiting"));
        let unchecked = DshReport {
            services: json!({ "error": "no report" }),
            ..clean.clone()
        };
        assert!(unchecked
            .problem()
            .unwrap()
            .starts_with("the service check"));
        assert_eq!(
            DshReport::default().problem().as_deref(),
            Some("the service check did not report")
        );
        let unread = DshReport {
            services: json!({ "missing": [], "importErrors": { "fx": "cannot import" } }),
            ..clean.clone()
        };
        assert!(unread
            .problem()
            .unwrap()
            .contains("could not read what these rows inject"));

        let pi = PiReport {
            loaded: Some(false),
            error: Some("refused".into()),
            ..PiReport::default()
        };
        assert_eq!(pi.problem().as_deref(), Some("refused"));
        assert_eq!(PiReport::default().problem(), None);
    }

    #[test]
    fn rows_need_an_array_and_ids() {
        assert!(row_ids(&json!({ "id": "x" })).is_err());
        assert!(row_ids(&json!([{ "name": "x" }])).is_err());
        assert_eq!(row_ids(&json!([{ "id": "a" }])).unwrap(), ["a"]);
    }

    #[test]
    fn skills_from_folders_and_files() {
        let dir = tempfile::tempdir().unwrap();
        let skill = dir.path().join("greet");
        std::fs::create_dir(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: greet\ndescription: Says hello.\n---\nSay hello.\n",
        )
        .unwrap();
        let empty = dir.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        let out = skills(&[skill.join("SKILL.md"), skill.clone(), empty.clone()]);
        assert_eq!(out.found, 3);
        assert_eq!(out.loaded, ["greet", "greet"]);
        assert_eq!(out.errors.len(), 1, "{out:?}");
        assert!(out.errors[0].contains("empty"), "{out:?}");
    }
}
