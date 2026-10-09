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

/// How long [`PluginHost::load`] waits for plugins to start.
pub const LOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
use rutis_bridge::runtime::LocalRuntime;
use rutis_loader::{
    Chain, EntryStatus, Layer, Loader, LoaderError, LoaderOptions, LoaderPlugin, Patch, Resolved,
    Resolver, RuntimeResolver, RuntimeRowsPlugin, ServiceCatalog,
};
use serde_json::{json, Value as Json};

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

    /// Services injected into this plugin beyond those it declares — e.g.
    /// `ui` for the pi adapter, which looks it up without requiring it.
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
            rows: Vec::new(),
        })
    }
}

/// Running runtimes and a loader; [`PluginHost::load`] adds plugins.
pub struct PluginHost {
    loader: Loader,
    routes: Arc<Mutex<HashMap<String, String>>>,
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
        for row in rows {
            if let Some(runtime) = &row.runtime {
                self.routes
                    .lock()
                    .unwrap()
                    .insert(row.name.clone(), runtime.clone());
            }
            self.rows.push(row);
        }
        let result = self.reconcile_all().await;
        if result.is_err() {
            self.rows.truncate(kept);
            let _ = self.reconcile_all().await;
        }
        result
    }

    async fn reconcile_all(&mut self) -> Result<(), BoxError> {
        let insert: Vec<Json> = self.rows.iter().map(Row::json).collect();
        let patches: Vec<Patch> = serde_json::from_value(json!([{ "insert": insert }]))?;
        let report = self
            .loader
            .reconcile(vec![Layer::new("app", patches)], None)
            .await?;
        if !report.failures.is_empty() {
            return Err(format!("plugins failed to load: {:?}", report.failures).into());
        }
        // `reconcile` counts a row still waiting (for a runtime, or for a
        // service it injects) as settled. A plugin that never starts — a
        // policy among them — must not pass for loaded: wait until every row
        // runs, and name the ones that do not.
        let ids: Vec<String> = self.rows.iter().map(|r| r.id.clone()).collect();
        let deadline = tokio::time::Instant::now() + LOAD_TIMEOUT;
        loop {
            let mut waiting = Vec::new();
            for id in &ids {
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
