//! Loads one third-party plugin package through its adapter, as an agent
//! would, and prints what yoagent gets from it as one JSON line: the
//! command-line face of [`yoagent_rutis::probe`]. The compatibility matrix
//! (`scripts/ecosystem-matrix.mjs`) runs it per package; it installs each
//! package and resolves what is in it first.
//!
//! - `pi --cwd DIR [--skill PATH]... EXTENSION...`: [`probe::pi`].
//! - `dsh --base DIR --rows JSON [--inject SERVICE]...`: [`probe::dsh`].
//!
//! `--plugins DIR` names the installed adapters (default: this crate's own
//! `plugins/`, with `npm ci` run in its `pi/` and `dsh/`).
//!
//! Nothing is called: tools are listed, not run, and the model is never
//! asked. Loading a package runs its code, as its own host would.
//!
//! Run: `cargo run --manifest-path integrations/yoagent-rutis/Cargo.toml --features probe --example ecosystem_probe -- pi --cwd /tmp/x ext.ts`

use std::path::PathBuf;

use serde_json::Value;
use tracing_subscriber::layer::SubscriberExt;
use yoagent_rutis::adapters::adapters;
use yoagent_rutis::probe::{self, Logs};

type BoxError = Box<dyn std::error::Error>;

fn usage() -> BoxError {
    "usage: ecosystem_probe pi [--plugins DIR] --cwd DIR [--skill PATH]... EXTENSION... | dsh [--plugins DIR] --base DIR --rows JSON [--inject SERVICE]..."
        .into()
}

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    let mut args = std::env::args().skip(1);
    let mode = args.next().ok_or_else(usage)?;
    let mut plugins = adapters().root;
    let mut cwd = None;
    let mut base = None;
    let mut rows = None;
    let mut inject = Vec::new();
    let mut skills = Vec::new();
    let mut extensions = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--plugins" => plugins = args.next().map(PathBuf::from).ok_or_else(usage)?,
            "--cwd" => cwd = args.next().map(PathBuf::from),
            "--base" => base = args.next().map(PathBuf::from),
            "--rows" => {
                rows = Some(serde_json::from_str::<Value>(
                    &args.next().ok_or_else(usage)?,
                )?)
            }
            "--skill" => skills.extend(args.next().map(PathBuf::from)),
            "--inject" => inject.extend(args.next()),
            _ => extensions.push(arg),
        }
    }
    // Global: the bridge logs from the runtime's worker threads.
    let logs = Logs::default();
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(logs.clone()))?;
    let mut result = match mode.as_str() {
        "pi" => {
            let cwd = cwd.ok_or_else(usage)?;
            serde_json::to_value(
                probe::pi(&plugins, &cwd, &extensions, &skills, Some(&logs)).await,
            )?
        }
        "dsh" => {
            let (base, rows) = (base.ok_or_else(usage)?, rows.ok_or_else(usage)?);
            serde_json::to_value(probe::dsh(&plugins, &base, &rows, &inject, Some(&logs)).await)?
        }
        _ => return Err(usage()),
    };
    result["ecosystem"] = mode.into();
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}
