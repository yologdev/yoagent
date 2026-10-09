//! A few lines in place of the rutis setup: Node runtimes, a loader, shared
//! services, and plugin rows routed to the runtime each names.
//!
//! ```no_run
//! # async fn run(root: &rutis::Ctx) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
//! use yoagent_frontend::host::{PluginHost, Row};
//! let mut host = PluginHost::builder()
//!     .node("pi", "plugins/pi")        // package.json + node_modules here
//!     .node("dsh", "plugins/dsh")
//!     .share("yoagent")
//!     .start(root)
//!     .await?;
//! host.load([
//!     Row::new("pi", "plugins/pi/pi-extensions-adapter.ts").runtime("pi"),
//!     Row::new("dsh-tools", "@deepseek-ai/dsh-tools").runtime("dsh"),
//! ])
//! .await?;
//! # Ok(()) }
//! ```

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rutis::{BoxFuture, Ctx, FiberState};
use rutis_bridge::runtime::LocalRuntime;
use rutis_loader::{
    Chain, EntryStatus, Layer, Loader, LoaderError, LoaderOptions, LoaderPlugin, Patch, Resolved,
    Resolver, RuntimeResolver, RuntimeRowsPlugin, ServiceCatalog,
};
use serde_json::{json, Value as Json};

/// How long [`PluginHost::load`] waits for plugins to start.
pub const LOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// A plugin to load: an id, what to load (an npm name or a file), the
/// runtime that loads it, and its config.
#[derive(Debug, Clone)]
pub struct Row {
    id: String,
    name: String,
    runtime: Option<String>,
    config: Json,
    inject: Vec<String>,
}

impl Row {
    pub fn new(id: impl Into<String>, name: impl ToString) -> Self {
        Row {
            id: id.into(),
            name: name.to_string(),
            runtime: None,
            config: json!({}),
            inject: Vec::new(),
        }
    }

    /// The runtime that loads it (a name given to [`PluginHostBuilder::node`]).
    /// Without one, the first runtime that can resolve the name does.
    pub fn runtime(mut self, runtime: impl Into<String>) -> Self {
        self.runtime = Some(runtime.into());
        self
    }

    pub fn config(mut self, config: Json) -> Self {
        self.config = config;
        self
    }

    /// The services injected into this plugin, in place of those it declares.
    ///
    /// Rarely what you want: in rutis 0.7 a TypeScript plugin given a row
    /// inject list never applies. To hand a plugin a service it only looks
    /// up (as the pi adapter does with `ui`), load a small plugin that
    /// injects it and provides it again under the name looked up — see
    /// `yoagent-rutis/plugins/pi/host-ui.ts`.
    pub fn inject(mut self, services: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.inject = services.into_iter().map(Into::into).collect();
        self
    }

    fn json(&self) -> Json {
        let mut row = json!({ "id": self.id, "name": self.name, "config": self.config });
        if !self.inject.is_empty() {
            row["inject"] = json!(self.inject);
        }
        row
    }
}

#[derive(Default)]
pub struct PluginHostBuilder {
    runtimes: Vec<(String, PathBuf)>,
    shared: Vec<String>,
}

impl PluginHostBuilder {
    /// A Node runtime named `name`, its packages in `dir` (`package.json`
    /// and `node_modules`, with `@arcships/rutis-runtime` installed).
    pub fn node(mut self, name: impl Into<String>, dir: impl Into<PathBuf>) -> Self {
        self.runtimes.push((name.into(), dir.into()));
        self
    }

    /// A host service plugins may inject (`yoagent`, `frontend`, `ui`, …).
    pub fn share(mut self, service: impl Into<String>) -> Self {
        self.shared.push(service.into());
        self
    }

    /// Start the runtimes and the loader on `root`.
    pub async fn start(self, root: &Ctx) -> Result<PluginHost, BoxError> {
        let mut catalog = ServiceCatalog::new();
        for service in &self.shared {
            catalog.register_shared(service);
        }
        let routes: Arc<Mutex<HashMap<String, String>>> = Arc::default();
        let mut resolvers = Vec::new();
        for (name, dir) in &self.runtimes {
            let runtime = LocalRuntime::node(
                dir.join("node_modules/@arcships/rutis-runtime"),
                dir.join("package.json"),
            )
            .named(name.clone());
            let rows = Arc::new(RuntimeResolver::node(runtime.handle()).with_catalog(&catalog));
            root.plugin(runtime);
            resolvers.push((name.clone(), rows));
        }
        let routed = Routed {
            routes: routes.clone(),
            runtimes: resolvers.clone(),
        };
        let runtimes = resolvers.clone();
        let loader_plugin = LoaderPlugin::new(
            Chain::new().with(routed),
            LoaderOptions {
                catalog,
                ..LoaderOptions::default()
            },
        );
        let loader = loader_plugin.handle();
        root.plugin(loader_plugin).await?;
        for (_, rows) in resolvers {
            root.plugin(RuntimeRowsPlugin::new(rows));
        }
        Ok(PluginHost {
            loader,
            routes,
            runtimes,
            rows: Vec::new(),
        })
    }
}

/// Running runtimes and a loader; [`PluginHost::load`] adds plugins.
pub struct PluginHost {
    loader: Loader,
    routes: Arc<Mutex<HashMap<String, String>>>,
    /// Each runtime's resolver: `reload` makes them forget a module.
    runtimes: Vec<(String, Arc<RuntimeResolver>)>,
    rows: Vec<Row>,
}

impl PluginHost {
    pub fn builder() -> PluginHostBuilder {
        PluginHostBuilder::default()
    }

    /// Each loaded row and its state (`Active`, `Pending`, an error, …).
    pub fn status(&self) -> Vec<(String, String)> {
        self.rows
            .iter()
            .map(|row| {
                let state = match self.loader.get(&row.id).map(|info| info.status) {
                    Some(EntryStatus::Running(s)) => match s.error {
                        Some(e) => format!("{:?}: {e}", s.state),
                        None => format!("{:?}", s.state),
                    },
                    Some(EntryStatus::Unresolved(e)) => format!("unresolved: {e}"),
                    Some(EntryStatus::Disabled) => "disabled".into(),
                    Some(EntryStatus::Inactive) => "inactive".into(),
                    None => "not loaded".into(),
                };
                (row.id.clone(), state)
            })
            .collect()
    }

    /// Load more plugins, after those already loaded. Fails, naming the
    /// failures, when any plugin fails to load.
    ///
    /// Rows are routed by `name`: two rows loading the same name must name
    /// the same runtime. A batch that fails is dropped again, so a later
    /// `load` does not keep failing on it.
    pub async fn load(&mut self, rows: impl IntoIterator<Item = Row>) -> Result<(), BoxError> {
        let kept = self.rows.len();
        let routes = self.routes.lock().unwrap().clone();
        let result = self.add(rows);
        let result = match result {
            Ok(()) => {
                let added: Vec<String> = self.rows[kept..].iter().map(|r| r.id.clone()).collect();
                self.reconcile(&added).await
            }
            Err(e) => Err(e),
        };
        if result.is_err() {
            self.rows.truncate(kept);
            *self.routes.lock().unwrap() = routes;
            if let Err(e) = self.reconcile(&[]).await {
                tracing::warn!("dropping a plugin batch that failed to load also failed: {e}");
            }
        }
        result
    }

    fn add(&mut self, rows: impl IntoIterator<Item = Row>) -> Result<(), BoxError> {
        for row in rows {
            if let Some(runtime) = &row.runtime {
                let mut routes = self.routes.lock().unwrap();
                match routes.get(&row.name) {
                    Some(other) if other != runtime => {
                        return Err(format!(
                            "`{}` is loaded by runtime `{other}` already, not `{runtime}`",
                            row.name
                        )
                        .into())
                    }
                    _ => {
                        routes.insert(row.name.clone(), runtime.clone());
                    }
                }
            }
            self.rows.push(row);
        }
        Ok(())
    }

    /// Unload one plugin: its fiber is disposed, and with it what it
    /// registered (handlers and tools, services, UI plugin offers). A run in
    /// progress keeps the tools it started with; a call into the plugin after
    /// it left fails ("no longer available").
    pub async fn unload(&mut self, id: &str) -> Result<(), BoxError> {
        let index = self
            .rows
            .iter()
            .position(|row| row.id == id)
            .ok_or_else(|| format!("no plugin `{id}` is loaded"))?;
        let routes = self.routes.lock().unwrap().clone();
        let row = self.rows.remove(index);
        if !self.rows.iter().any(|other| other.name == row.name) {
            self.routes.lock().unwrap().remove(&row.name);
        }
        let result = self.reconcile(&[]).await;
        if result.is_err() {
            self.rows.insert(index, row);
            *self.routes.lock().unwrap() = routes;
            if let Err(e) = self.reconcile(&[]).await {
                tracing::warn!("putting back plugin `{id}` after a failed unload also failed: {e}");
            }
        }
        result
    }

    /// Whether the plugin `id` is running (started, not failed or stopped).
    pub fn is_running(&self, id: &str) -> bool {
        matches!(
            self.loader.get(id).map(|info| info.status),
            Some(EntryStatus::Running(s)) if s.state == FiberState::Active
        )
    }

    /// Reload one plugin, picking up its edited file, and restart it. The
    /// runtime imports the plugin's own module again when its file changed
    /// (not the modules that one imports: an edit there needs the runtime
    /// restarted). Waits until it runs again — though, as on `load`, a
    /// TypeScript plugin's async start-up may still be going.
    ///
    /// Not all or nothing. New code that does not import (a syntax error) is
    /// refused before anything stops: the running version stays. New code
    /// that imports but fails when it starts has replaced the old by then,
    /// and the plugin stays stopped until a later reload succeeds — so a
    /// plugin that is a policy should run under a bridge that requires one
    /// (`RutisExtension::require_policy`). The error says which happened;
    /// [`PluginHost::is_running`] tells too.
    pub async fn reload(&mut self, id: &str) -> Result<(), BoxError> {
        let name = self
            .rows
            .iter()
            .find(|row| row.id == id)
            .map(|row| row.name.clone())
            .ok_or_else(|| format!("no plugin `{id}` is loaded"))?;
        // A runtime resolver keeps a module's description until its package
        // version changes, and a file has none: forget it, so the row is
        // described (and imported) again and replaced.
        for (_, resolver) in &self.runtimes {
            resolver.invalidate(&name);
        }
        let result = match self.loader.reload(id).await {
            Ok(report) if !report.new_failures.is_empty() => {
                Err(format!("{:?}", report.new_failures).into())
            }
            Ok(_) => self.wait_running(&[id.to_owned()]).await,
            Err(e) => Err(e.into()),
        };
        result.map_err(|e: BoxError| {
            if self.is_running(id) {
                format!("`{id}` was not reloaded; its running version stays: {e}").into()
            } else {
                format!("`{id}` failed to start with its new code and is stopped until a reload succeeds: {e}").into()
            }
        })
    }

    /// Apply the rows; fail on plugins this newly broke, and wait for
    /// `started` (the rows just added) to run. A plugin already failing — one
    /// a reload left stopped — does not fail every later change.
    async fn reconcile(&mut self, started: &[String]) -> Result<(), BoxError> {
        let insert: Vec<Json> = self.rows.iter().map(Row::json).collect();
        let patches: Vec<Patch> = serde_json::from_value(json!([{ "insert": insert }]))?;
        let report = self
            .loader
            .reconcile(vec![Layer::new("app", patches)], None)
            .await?;
        if !report.new_failures.is_empty() {
            return Err(format!("plugins failed to load: {:?}", report.new_failures).into());
        }
        // `reconcile` counts a row still waiting (for a runtime, or for a
        // service it injects) as settled. A plugin that never starts — a
        // policy among them — must not pass for loaded: wait until the new
        // rows run, and name the ones that do not.
        self.wait_running(started).await
    }

    /// Wait until these rows run; fail naming those that failed or did not.
    async fn wait_running(&self, ids: &[String]) -> Result<(), BoxError> {
        let deadline = tokio::time::Instant::now() + LOAD_TIMEOUT;
        loop {
            let mut waiting = Vec::new();
            for id in ids {
                match self.loader.get(id).map(|info| info.status) {
                    Some(EntryStatus::Running(s)) if s.state == FiberState::Active => {}
                    Some(EntryStatus::Running(s)) if s.state == FiberState::Failed => {
                        return Err(format!("plugin `{id}` failed: {:?}", s.error).into());
                    }
                    Some(EntryStatus::Unresolved(e)) => {
                        return Err(format!("plugin `{id}` cannot load: {e}").into());
                    }
                    other => waiting.push(format!(
                        "{id} ({})",
                        match other {
                            Some(EntryStatus::Running(s)) => format!("{:?}", s.state),
                            Some(EntryStatus::Disabled) => "disabled".into(),
                            Some(EntryStatus::Inactive) => "inactive".into(),
                            _ => "unknown".into(),
                        }
                    )),
                }
            }
            if waiting.is_empty() {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(format!("plugins did not start: {}", waiting.join(", ")).into());
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
}

/// Resolves a row with the runtime it named; other names (no runtime given)
/// with the first runtime that knows them.
struct Routed {
    routes: Arc<Mutex<HashMap<String, String>>>,
    runtimes: Vec<(String, Arc<RuntimeResolver>)>,
}

impl Resolver for Routed {
    fn resolve<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Arc<Resolved>, LoaderError>> {
        let route = self.routes.lock().unwrap().get(name).cloned();
        Box::pin(async move {
            if let Some(route) = route {
                let Some((_, resolver)) = self.runtimes.iter().find(|(n, _)| *n == route) else {
                    return Err(LoaderError::NotFound {
                        name: name.to_owned(),
                    });
                };
                return resolver.resolve(name).await;
            }
            for (_, resolver) in &self.runtimes {
                match resolver.resolve(name).await {
                    Err(LoaderError::NotFound { .. }) => continue,
                    other => return other,
                }
            }
            Err(LoaderError::NotFound {
                name: name.to_owned(),
            })
        })
    }
}
