//! A coding agent in a few files: the loop is yoagent, the terminal UI is
//! pi's (`@earendil-works/pi-tui`, in `plugins/pi/tui-frontend.ts`), and
//! tools and tool policies come from other agents' plugins, unchanged: pi
//! extensions through the pi adapter, and DSH's web search
//! (`dsh-free-search`) through the DSH adapter. All of them are rutis
//! plugins, in two Node runtimes (pi's packages and DSH's); this host is the
//! loop, yoagent's built-in tools, and a `chat` service the UI plugin calls
//! (`prompt`, `abort`, `quit`).
//!
//! Interactive (macOS / Linux; the UI reads the keyboard from `/dev/tty`):
//! `--live` talks to DeepSeek (`DEEPSEEK_API_KEY`, else `~/.dskey`); without it
//! the model is scripted. `--demo "<prompt>"` runs one prompt headless and
//! prints the rendered screen (no terminal needed; CI runs it scripted).
//!
//! Setup (once): `npm ci` in `plugins/pi/` (Node 24+), and in `plugins/dsh/`
//! for web search (skipped when not installed, or with `--no-dsh`).
//!
//! Run: `cargo run --manifest-path integrations/yoagent-rutis/Cargo.toml --features node --example pi_tui_agent [-- [--live] [--demo PROMPT] [--no-dsh] [EXTENSION.ts ...]]`

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rutis::BoxFuture;
use rutis::Ctx;
use rutis_bridge::runtime::LocalRuntime;
use rutis_bridge::session::{host_key, HostDispatch, Reply, Value};
use rutis_loader::{
    Chain, Layer, LoaderError, LoaderOptions, LoaderPlugin, Patch, Resolved, Resolver,
    RuntimeResolver, RuntimeRowsPlugin, ServiceCatalog,
};
use serde_json::json;
use tokio::sync::mpsc;
use yoagent::provider::mock::{MockResponse, MockToolCall};
use yoagent::provider::{MockProvider, ModelConfig};
use yoagent::tools::default_tools;
use yoagent::Agent;
use yoagent_rutis::RutisBridge;

type BoxError = Box<dyn std::error::Error>;

fn pi_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins/pi")
}

fn dsh_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins/dsh")
}

/// Sends DSH's rows to DSH's runtime. Any Node runtime takes a file path, so
/// the loader asks this one first, and it claims only DSH's packages.
struct DshRows(Arc<RuntimeResolver>);

impl Resolver for DshRows {
    fn resolve<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Arc<Resolved>, LoaderError>> {
        let ours = name.starts_with("@deepseek-ai/")
            || name.starts_with("dsh-")
            || Path::new(name).starts_with(dsh_dir());
        if ours {
            return self.0.resolve(name);
        }
        let name = name.to_owned();
        Box::pin(async move { Err(LoaderError::NotFound { name }) })
    }
}

/// What the UI asks of the host.
enum Command {
    Prompt(String),
    Abort,
    Quit,
}

/// The `chat` host service: the UI plugin's way into the loop.
struct Chat(mpsc::UnboundedSender<Command>);

impl HostDispatch for Chat {
    fn invoke(&self, method: &str, args: Value) -> Reply {
        let command = match method {
            "prompt" => {
                let text = args.list()?.into_iter().next().and_then(|v| v.json().ok());
                Command::Prompt(
                    text.and_then(|t| t.as_str().map(str::to_owned))
                        .unwrap_or_default(),
                )
            }
            "abort" => Command::Abort,
            _ => Command::Quit,
        };
        let _ = self.0.send(command);
        Ok(Value::Undefined)
    }

    fn methods(&self) -> Option<serde_json::Value> {
        Some(json!({ "prompt": "async", "abort": "async", "quit": "async" }))
    }
}

fn deepseek_key() -> Result<String, BoxError> {
    if let Ok(key) = std::env::var("DEEPSEEK_API_KEY") {
        if !key.trim().is_empty() {
            return Ok(key.trim().to_owned());
        }
    }
    let path = Path::new(&std::env::var("HOME")?).join(".dskey");
    Ok(std::fs::read_to_string(&path)
        .map_err(|e| format!("--live needs DEEPSEEK_API_KEY or {}: {e}", path.display()))?
        .split_whitespace()
        .collect())
}

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    let (mut live, mut demo, mut extensions) = (false, None, Vec::new());
    let mut dsh = dsh_dir()
        .join("node_modules/@arcships/rutis-runtime")
        .exists();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--live" => live = true,
            "--demo" => demo = Some(args.next().ok_or("--demo needs a prompt")?),
            "--no-dsh" => dsh = false,
            path => extensions.push(std::fs::canonicalize(path)?),
        }
    }
    if extensions.is_empty() {
        extensions.push(pi_dir().join("fixture-extension.ts"));
    }
    let runtime = pi_dir().join("node_modules/@arcships/rutis-runtime");
    if !runtime.exists() {
        return Err(format!("run `npm ci` in {} first", pi_dir().display()).into());
    }
    let cwd = std::env::current_dir()?;
    // The plugin runtimes share this terminal: Node's warnings (DSH's packages
    // use the experimental SQLite module) would draw over the UI.
    if std::env::var_os("NODE_OPTIONS").is_none() {
        std::env::set_var("NODE_OPTIONS", "--no-warnings");
    }

    // rutis: the bridge, the `chat` service, the Node runtimes and a loader.
    let root = Ctx::root()?;
    let bridge = RutisBridge::install(&root)?;
    let (tx, mut commands) = mpsc::unbounded_channel();
    root.provide_as::<dyn HostDispatch>(host_key("chat"), Arc::new(Chat(tx)))?;
    let mut catalog = ServiceCatalog::new();
    catalog.register_shared("yoagent");
    catalog.register_shared("chat");
    let node = LocalRuntime::node(&runtime, pi_dir().join("package.json"));
    let pi_rows = Arc::new(RuntimeResolver::node(node.handle()).with_catalog(&catalog));
    root.plugin(node);
    let mut chain = Chain::new();
    let mut resolvers = vec![pi_rows.clone()];
    if dsh {
        let node = LocalRuntime::node(
            dsh_dir().join("node_modules/@arcships/rutis-runtime"),
            dsh_dir().join("package.json"),
        )
        .named("dsh");
        let dsh_rows = Arc::new(RuntimeResolver::node(node.handle()).with_catalog(&catalog));
        root.plugin(node);
        chain = chain.with_shared(Arc::new(DshRows(dsh_rows.clone())));
        resolvers.push(dsh_rows);
    }
    let loader_plugin = LoaderPlugin::new(
        chain.with_shared(pi_rows),
        LoaderOptions {
            catalog,
            ..LoaderOptions::default()
        },
    );
    let loader = loader_plugin.handle();
    root.plugin(loader_plugin).await?;
    for rows in resolvers {
        root.plugin(RuntimeRowsPlugin::new(rows));
    }

    // The plugins: pi extensions through the pi adapter, the UI, and DSH's
    // web search through the DSH adapter (DSH's own packages first).
    let mut rows = vec![
        json!({
            "id": "pi",
            "name": pi_dir().join("pi-extensions-adapter.ts"),
            "config": { "extensions": extensions, "cwd": cwd },
        }),
        json!({
            "id": "ui",
            "name": pi_dir().join("tui-frontend.ts"),
            "config": demo.as_ref().map_or(json!({}), |prompt| json!({ "demo": prompt })),
        }),
    ];
    if dsh {
        rows.extend([
            json!({ "id": "dsh-web", "name": "@deepseek-ai/dsh-web" }),
            json!({ "id": "dsh-system-prompt", "name": "@deepseek-ai/dsh-system-prompt" }),
            json!({ "id": "dsh-tools", "name": "@deepseek-ai/dsh-tools" }),
            json!({
                "id": "dsh-free-search",
                "name": "dsh-free-search",
                "config": { "provider": "bing", "disabledEngines": [], "bingMarket": "en-US" },
            }),
            json!({ "id": "dsh", "name": dsh_dir().join("dsh-tools-adapter.ts") }),
        ]);
    }
    let patches: Vec<Patch> = serde_json::from_value(json!([{ "insert": rows }]))?;
    let report = loader
        .reconcile(vec![Layer::new("app", patches)], None)
        .await?;
    if !report.failures.is_empty() {
        return Err(format!("a plugin failed to load: {report:?}").into());
    }

    // The loop: yoagent's built-in tools plus everything the plugins offer.
    let mut agent = if live {
        Agent::from_config(ModelConfig::deepseek("deepseek-flash", "DeepSeek Flash"))
            .with_api_key(deepseek_key()?)
    } else {
        Agent::from_provider(scripted(), ModelConfig::mock())
    }
    .with_system_prompt(format!(
        "You are a coding agent working in {}. Be brief.",
        cwd.display()
    ))
    .with_tools(default_tools())
    .with_extension(bridge.extension());

    while let Some(command) = commands.recv().await {
        let Command::Prompt(text) = command else {
            if matches!(command, Command::Quit) {
                break;
            }
            continue;
        };
        let mut events = agent.prompt(text).await;
        loop {
            tokio::select! {
                event = events.recv() => if event.is_none() { break },
                Some(command) = commands.recv() => match command {
                    Command::Abort => agent.abort(),
                    Command::Quit => { agent.abort(); agent.finish().await; return shutdown(root).await }
                    Command::Prompt(_) => {} // the UI sends one prompt at a time
                },
            }
        }
        agent.finish().await;
    }
    shutdown(root).await
}

async fn shutdown(root: Ctx) -> Result<(), BoxError> {
    // Give the UI plugin's last writes a moment, then stop the runtime.
    tokio::time::sleep(Duration::from_millis(100)).await;
    root.shutdown().await?;
    Ok(())
}

/// Without `--live`: one tool call through the plugin-guarded tools, then an answer.
fn scripted() -> MockProvider {
    MockProvider::new(vec![
        MockResponse::ToolCalls(vec![MockToolCall {
            provider_metadata: None,
            name: "list_files".into(),
            arguments: json!({ "path": "." }),
        }]),
        MockResponse::Text(
            "I listed the project. **Scripted** model: run with `--live` for a real one.".into(),
        ),
    ])
}
