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

/// The rows a DSH host with the stand-in agent loads, from `dsh` (this
/// crate's `plugins/dsh`, or the copy the host installed): every row of
/// `host-services.json`, with the rows `dsh-host.json` also names replaced
/// by its own — rutis-loader keeps the first of two rows with one id, so
/// adding `dsh-host.json` after `host-services.json` would leave the
/// storage-less `dsh-session-persistence` in place — then `dsh-host.json`'s
/// other rows. File names become absolute (under `dsh`), and the session
/// log is kept under `sessions` (the jsonl backend's `config.root`).
///
/// The host also shares `host_session` and `host_workspace` with the
/// runtime (see the crate README's DSH section).
pub fn dsh_host_rows(dsh: &Path, sessions: &Path) -> std::io::Result<Vec<serde_json::Value>> {
    let read = |file: &str| -> std::io::Result<Vec<serde_json::Value>> {
        serde_json::from_str(&std::fs::read_to_string(dsh.join(file))?).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{file}: {e}"))
        })
    };
    let host: Vec<serde_json::Value> = read("dsh-host.json")?
        .into_iter()
        .map(|mut row| {
            if let Some(name) = row["name"].as_str().filter(|n| n.ends_with(".ts")) {
                row["name"] = serde_json::json!(dsh.join(name));
            }
            if row["name"] == "@deepseek-ai/dsh-session-persistence-jsonl" {
                row["config"] = serde_json::json!({ "root": sessions });
            }
            row
        })
        .collect();
    let mut rows = Vec::new();
    for row in read("host-services.json")? {
        match host.iter().find(|h| h["id"] == row["id"]) {
            Some(replacement) => rows.push(replacement.clone()),
            None => rows.push(row),
        }
    }
    for row in host {
        if !rows.iter().any(|r| r["id"] == row["id"]) {
            rows.push(row);
        }
    }
    Ok(rows)
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

    #[test]
    fn dsh_host_rows_replace_the_storage_less_session_persistence() {
        let a = adapters();
        let rows = dsh_host_rows(&a.dsh, Path::new("/tmp/sessions")).unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r["id"].as_str().unwrap()).collect();
        let mut unique = ids.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), ids.len(), "one row per id: {ids:?}");
        let persistence = rows
            .iter()
            .find(|r| r["id"] == "dsh-session-persistence")
            .unwrap();
        assert_eq!(
            persistence["name"],
            "@deepseek-ai/dsh-session-persistence-jsonl"
        );
        assert_eq!(persistence["config"]["root"], "/tmp/sessions");
        let host = rows.iter().find(|r| r["id"] == "dsh-host").unwrap();
        assert_eq!(host["name"], serde_json::json!(a.dsh.join("dsh-host.ts")));
        for id in ["dsh-fs", "dsh-session", "dsh-agent", "dsh-user-approval"] {
            assert!(ids.contains(&id), "{id}: {ids:?}");
        }
    }
}
