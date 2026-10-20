//! Where this crate's adapter files are: the pi and DSH adapters and the
//! TypeScript declarations of the `yoagent` host service, as published in
//! the crate (`plugins/`, without `node_modules`).
//!
//! The files are this crate's own, in cargo's registry (or the path
//! dependency's folder): read them, or copy them to install their npm
//! dependencies — never install into them. Copy the whole `root` (the
//! adapters import `../yoagent.d.ts`) and run `npm ci` in the copy's `pi/`
//! and `dsh/`; that copy is what the `probe` module (feature `probe`) takes. The paths are where
//! the crate was built: a binary run on another machine must ship a copy.
//!
//! ```no_run
//! let adapters = yoagent_rutis::adapters::adapters();
//! let adapter = adapters.pi.join("pi-extensions-adapter.ts");
//! # let _ = adapter;
//! ```

use std::path::{Path, PathBuf};

/// The adapter folders and files of this crate.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Adapters {
    /// `plugins/`: everything below.
    pub root: PathBuf,
    /// `plugins/pi`: the pi extensions adapter (`pi-extensions-adapter.ts`),
    /// `host-ui.ts`, `resolve-package.mjs`, and their `package.json` and lock.
    pub pi: PathBuf,
    /// `plugins/dsh`: the DSH tools adapter (`dsh-tools-adapter.ts`), the
    /// host services a headless DSH host loads (`host-services.json`), the
    /// stand-in DSH agent's rows (`dsh-host.json`, `dsh-host.ts`), and
    /// their `package.json` and lock.
    pub dsh: PathBuf,
    /// `plugins/yoagent.d.ts`: the `yoagent` host service's TypeScript types.
    pub types: PathBuf,
}

/// This crate's adapter files.
pub fn adapters() -> Adapters {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins");
    Adapters {
        pi: root.join("pi"),
        dsh: root.join("dsh"),
        types: root.join("yoagent.d.ts"),
        root,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_adapter_files_are_there() {
        let a = adapters();
        for file in [
            a.pi.join("pi-extensions-adapter.ts"),
            a.pi.join("package.json"),
            a.dsh.join("dsh-tools-adapter.ts"),
            a.pi.join("package-lock.json"),
            a.dsh.join("host-services.json"),
            a.dsh.join("dsh-host.json"),
            a.dsh.join("dsh-host.ts"),
            a.dsh.join("probe-services.ts"),
            a.dsh.join("package.json"),
            a.dsh.join("package-lock.json"),
            a.types.clone(),
        ] {
            assert!(file.is_file(), "{}", file.display());
        }
        assert!(a.pi.starts_with(&a.root) && a.dsh.starts_with(&a.root));
    }
}
