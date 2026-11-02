//! Local language runtimes as a rutis 0.20 host starts them: each one a
//! `rutis_host::connect::SpawnPlugin` (its process the far end of a
//! session), every row resolved by one `SessionResolver` — npm names and
//! files on the `node` runtime, `<runtime>:<plugin>` on the others
//! (`py:<module>`, `bun:<file>`).
#![allow(dead_code)]

use std::path::Path;
use std::sync::Arc;

use rutis::host::FiberView;
use rutis::Ctx;
use rutis_bridge::runtime::RuntimeHandle;
use rutis_bridge::{LocalId, PeerId};
use rutis_host::connect::SpawnPlugin;
use rutis_host::languages;
use rutis_loader::runtime::SessionResolver;
use rutis_loader::{Chain, Loader, LoaderOptions, LoaderPlugin, ServiceCatalog};

/// The Node runtime `node` of the `@arcships/rutis-runtime` package at
/// `runtime`, its plugins resolved from `package_json`.
pub fn node(runtime: &Path, package_json: &Path) -> SpawnPlugin {
    SpawnPlugin::new("node", languages::node::launcher(runtime), package_json)
}

/// The Python runtime `py`, importing plugin modules from `modules`, run
/// by `interpreter` (an environment with `rutis` installed).
pub fn python(modules: &Path, interpreter: &Path) -> SpawnPlugin {
    SpawnPlugin::new("py", languages::python::launcher(modules), modules).interpreter(interpreter)
}

/// The Bun runtime `bun` of the `@arcships/rutis-bun` package at
/// `package`, run by the Bun executable `bun`, in `project`.
pub fn bun(package: &Path, bun: &Path, project: &Path) -> SpawnPlugin {
    SpawnPlugin::new(
        "bun",
        languages::bun::launcher(bun, package, project),
        project,
    )
}

/// What [`start`] mounted.
pub struct Started {
    pub loader: Loader,
    /// Each runtime's plugin, in the order given (to restart one).
    pub runtimes: Vec<FiberView>,
}

/// Provide the id `root` greets its runtimes with, start `runtimes` and a
/// loader whose rows run on them, and wait until every runtime is up
/// (`reconcile` does not wait for a runtime still starting). Panics when
/// one does not start.
pub async fn start(root: &Ctx, catalog: ServiceCatalog, runtimes: Vec<SpawnPlugin>) -> Started {
    root.provide(LocalId(PeerId::new("main").unwrap())).unwrap();
    let sessions = Arc::new(SessionResolver::npm(&catalog, "node"));
    let mut handles: Vec<(String, RuntimeHandle)> = Vec::new();
    let mut views = Vec::new();
    for runtime in runtimes {
        let handle = runtime.handle();
        sessions.runtime(handle.clone());
        handles.push((handle.name().to_owned(), handle));
        views.push(root.plugin(runtime));
    }
    let plugin = LoaderPlugin::new(
        Chain::new().with_shared(sessions.clone()),
        LoaderOptions {
            catalog,
            ..LoaderOptions::default()
        },
    );
    let loader = plugin.handle();
    sessions.attach(&loader);
    root.plugin(plugin).await.unwrap();
    for (name, _) in &handles {
        sessions.follow_row(name);
    }
    for (name, handle) in &handles {
        assert!(
            handle.ready().await.is_some(),
            "the {name} runtime did not start: {:?}",
            handle.state()
        );
    }
    Started {
        loader,
        runtimes: views,
    }
}
