//! A few lines in place of the rutis setup: Node runtimes, a loader, shared
//! services, and plugin rows routed to the runtime each names.
//!
//! ```no_run
//! # async fn run(root: &rutis::Ctx) -> Result<(), Box<dyn std::error::Error>> {
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

use rutis::{BoxFuture, Ctx};
use rutis_bridge::runtime::LocalRuntime;
use rutis_loader::{
    Chain, Layer, Loader, LoaderError, LoaderOptions, LoaderPlugin, Patch, Resolved, Resolver,
    RuntimeResolver, RuntimeRowsPlugin, ServiceCatalog,
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
}

impl Row {
    pub fn new(id: impl Into<String>, name: impl ToString) -> Self {
        Row {
            id: id.into(),
            name: name.to_string(),
            runtime: None,
            config: json!({}),
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

    fn json(&self) -> Json {
        json!({ "id": self.id, "name": self.name, "config": self.config })
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

    /// Load more plugins, after those already loaded. Fails, naming the
    /// failures, when any plugin fails to load.
    pub async fn load(&mut self, rows: impl IntoIterator<Item = Row>) -> Result<(), BoxError> {
        for row in rows {
            if let Some(runtime) = &row.runtime {
                self.routes
                    .lock()
                    .unwrap()
                    .insert(row.name.clone(), runtime.clone());
            }
            self.rows.push(row);
        }
        let insert: Vec<Json> = self.rows.iter().map(Row::json).collect();
        let patches: Vec<Patch> = serde_json::from_value(json!([{ "insert": insert }]))?;
        let report = self
            .loader
            .reconcile(vec![Layer::new("app", patches)], None)
            .await?;
        if report.failures.is_empty() {
            Ok(())
        } else {
            Err(format!("plugins failed to load: {:?}", report.failures).into())
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
