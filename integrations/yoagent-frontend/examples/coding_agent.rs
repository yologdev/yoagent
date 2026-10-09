//! EXPERIMENTAL. A coding agent with a terminal or a browser frontend, on
//! the yoagent-frontend layer: yoagent's loop and tools, a pi extension that
//! asks before dangerous commands (its `ctx.ui.select` reaches the frontend),
//! DSH's web search and `ask_user_question` (its questions and approvals reach
//! the frontend too), and a UI plugin that renders search results as links in
//! the browser. The pi extension and DSH's packages are used unchanged; the
//! frontends are plugins too.
//!
//! - default: the terminal frontend (pi-tui; macOS / Linux).
//! - `--web [ADDR]`: serve the browser frontend (default 127.0.0.1:8787);
//!   no terminal UI. Open the printed URL; any number of tabs share the session.
//! - `--demo PROMPT`: one prompt, terminal frontend headless, screen printed
//!   (a question gets "no", as from a user who walked away). CI runs it.
//! - `--live`: DeepSeek (`DEEPSEEK_API_KEY`); otherwise a scripted model.
//! - `--no-dsh`: skip DSH (web search, `ask_user_question` and its dialogs).
//!
//! Logs (`RUST_LOG`) go to stderr with `--web` / `--demo`, else to
//! `$TMPDIR/yoagent-coding-agent.log` (the terminal UI owns the screen).
//!
//! Setup (once): `npm ci` in `integrations/yoagent-frontend/plugins/` and in
//! `integrations/yoagent-rutis/plugins/pi/` (and `plugins/dsh/` for DSH's search and dialogs).
//!
//! Run: `cargo run --manifest-path integrations/yoagent-frontend/Cargo.toml --example coding_agent [-- --web]`

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use rutis::Ctx;
use serde_json::json;
use yoagent::provider::mock::{MockResponse, MockToolCall};
use yoagent::provider::{MockProvider, ModelConfig};
use yoagent::tools::default_tools;
use yoagent::Agent;
use yoagent_frontend::host::{PluginHost, Row};
use yoagent_frontend::{services, session, web, Session};
use yoagent_rutis::RutisBridge;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

fn here() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}
fn rutis_plugins() -> PathBuf {
    here().join("../yoagent-rutis/plugins")
}

struct Options {
    key: Option<String>,
    web: Option<SocketAddr>,
    demo: Option<String>,
    dsh: bool,
}

fn main() -> Result<(), BoxError> {
    let (mut live, mut web, mut demo) = (false, None, None);
    let mut dsh = rutis_plugins().join("dsh/node_modules").exists();
    let mut args = std::env::args().skip(1).peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--live" => live = true,
            "--web" => {
                let addr = match args.peek() {
                    Some(next) if !next.starts_with("--") => args.next().unwrap(),
                    _ => "127.0.0.1:8787".into(),
                };
                web = Some(addr.parse()?);
            }
            "--demo" => demo = Some(args.next().ok_or("--demo needs a prompt")?),
            "--no-dsh" => dsh = false,
            other => return Err(format!("unknown argument {other}").into()),
        }
    }
    for dir in [here().join("plugins"), rutis_plugins().join("pi")] {
        if !dir.join("node_modules").exists() {
            return Err(format!("run `npm ci` in {} first", dir.display()).into());
        }
    }
    // Fail before any frontend owns the terminal.
    let key = match live {
        true => match std::env::var("DEEPSEEK_API_KEY") {
            Ok(key) if !key.trim().is_empty() => Some(key.trim().to_owned()),
            _ => return Err("--live needs DEEPSEEK_API_KEY".into()),
        },
        false => None,
    };
    // The plugin runtimes share the terminal; DSH's packages warn on stderr.
    if std::env::var_os("NODE_OPTIONS").is_none() {
        std::env::set_var("NODE_OPTIONS", "--no-warnings");
    }
    let log = logging(web.is_some() || demo.is_some())?;
    let options = Options {
        key,
        web,
        demo,
        dsh,
    };
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let root = Ctx::root()?;
            let result = run(&root, options).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            // A failed shutdown must not hide why the run failed.
            if let Err(e) = root.shutdown().await {
                match &result {
                    Ok(()) => return Err(e.into()),
                    Err(_) => eprintln!("shutting the plugins down also failed: {e}"),
                }
            }
            if let (Err(_), Some(path)) = (&result, &log) {
                eprintln!("logs: {}", path.display());
            }
            result
        })
}

/// Logs (`RUST_LOG`, default warnings and the frontend layer's info): on
/// stderr when nothing else draws there, else to a file — the terminal UI
/// owns the screen. Returns the file's path.
fn logging(stderr: bool) -> Result<Option<PathBuf>, BoxError> {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env()
        // The registry's "no API key" warning is about the scripted model's
        // `mock` provider; `--live` checks its key before starting.
        .unwrap_or_else(|_| {
            EnvFilter::new("warn,yoagent::provider::registry=error,yoagent_frontend=info")
        });
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    if stderr {
        builder.with_writer(std::io::stderr).init();
        return Ok(None);
    }
    let path = std::env::temp_dir().join("yoagent-coding-agent.log");
    let file = std::fs::File::create(&path)?;
    builder
        .with_ansi(false)
        .with_writer(std::sync::Mutex::new(file))
        .init();
    Ok(Some(path))
}

async fn run(root: &Ctx, options: Options) -> Result<(), BoxError> {
    let cwd = std::env::current_dir()?;
    // A terminal UI ends the session when it quits; a server keeps going.
    let (session, driver) = Session::new(options.web.is_none());
    let bridge = RutisBridge::install(root)?;
    services::provide(root, &session)?;

    let mut builder = PluginHost::builder()
        .node("ui", here().join("plugins"))
        .node("pi", rutis_plugins().join("pi"));
    if options.dsh {
        builder = builder.node("dsh", rutis_plugins().join("dsh"));
    }
    let mut host = builder
        .share("yoagent")
        .share(services::FRONTEND)
        .share(services::UI)
        .start(root)
        .await?;

    let mut rows = vec![
        Row::new(
            "pi",
            rutis_plugins()
                .join("pi/pi-extensions-adapter.ts")
                .display(),
        )
        .runtime("pi")
        .config(json!({
            "extensions": [here().join("plugins/pi-extensions/confirm-dangerous.ts")],
            "cwd": cwd,
        })),
        // Re-provides the host's `ui` in the pi runtime: pi's dialogs reach the frontend.
        Row::new(
            "pi-host-ui",
            rutis_plugins().join("pi/host-ui.ts").display(),
        )
        .runtime("pi"),
        Row::new(
            "search-links",
            here().join("plugins/search-links.ts").display(),
        )
        .runtime("ui"),
    ];
    if options.dsh {
        let dsh = |id: &str, name: &str| Row::new(id, name).runtime("dsh");
        rows.extend([
            dsh("dsh-web", "@deepseek-ai/dsh-web"),
            dsh("dsh-system-prompt", "@deepseek-ai/dsh-system-prompt"),
            dsh("dsh-tools", "@deepseek-ai/dsh-tools"),
            // `ask_user_question`, answered through the frontends.
            dsh("dsh-user-questions", "@deepseek-ai/dsh-user-questions"),
            dsh("dsh-tool-ask-user", "@deepseek-ai/dsh-tool-ask-user"),
            dsh("dsh-free-search", "dsh-free-search").config(
                json!({ "provider": "bing", "disabledEngines": [], "bingMarket": "en-US" }),
            ),
            Row::new(
                "dsh",
                rutis_plugins().join("dsh/dsh-tools-adapter.ts").display(),
            )
            .runtime("dsh"),
            // dsh's approvals and questions reach the frontends (it injects `ui`).
            Row::new(
                "dsh-host-dialogs",
                rutis_plugins().join("dsh/host-dialogs.ts").display(),
            )
            .runtime("dsh"),
        ]);
    }
    host.load(rows).await?;
    // A TypeScript row reads as running before its own async start-up is
    // done: wait for the handlers the agent depends on — the pi policy above
    // all — before any frontend can send a prompt.
    let mut needed = vec!["pi-extensions"];
    if options.dsh {
        needed.push("dsh-tools");
    }
    let registry = bridge.registry().clone();
    tokio::time::timeout(Duration::from_secs(60), async {
        while !needed
            .iter()
            .all(|name| registry.handlers().iter().any(|h| h.name() == *name))
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .map_err(|_| {
        format!(
            "plugins did not register their handlers: {:?}",
            host.status()
        )
    })?;

    let mut agent = match options.key {
        Some(key) => Agent::from_config(ModelConfig::deepseek("deepseek-flash", "DeepSeek Flash"))
            .with_api_key(key),
        None => Agent::from_provider(scripted(), ModelConfig::mock()),
    }
    .with_system_prompt(format!(
        "You are a coding agent working in {}. Be brief.",
        cwd.display()
    ))
    .with_tools(default_tools())
    // A pi policy may ask the user: give its hook as long as a question
    // waits (yoagent-rutis's default policy timeout is 60 s).
    .with_extension(
        bridge
            .extension()
            .with_policy_timeout(Some(session::UI_TIMEOUT + Duration::from_secs(30))),
    );

    match options.web {
        Some(addr) => {
            let served = web::serve(session.clone(), addr).await?;
            println!("yoagent web frontend: {}  (Ctrl+C to stop)", served.url());
            tokio::select! {
                _ = driver.run(agent) => {}
                stop = tokio::signal::ctrl_c() => {
                    if let Err(e) = stop {
                        eprintln!("cannot listen for Ctrl+C ({e}); stopping");
                    }
                }
            }
            served.stop();
        }
        None => {
            // Last, once everything else loaded: a failure prints on a normal terminal.
            let demo = options.demo.map_or(json!({}), |p| json!({ "demo": p }));
            host.load([Row::new(
                "terminal-ui",
                here().join("plugins/terminal-ui.ts").display(),
            )
            .runtime("ui")
            .config(demo)])
                .await?;
            agent = driver.run(agent).await;
            drop(agent);
        }
    }
    Ok(())
}

/// Without `--live`: a harmless tool call, then one the pi extension asks
/// about, then an answer.
fn scripted() -> MockProvider {
    MockProvider::new(vec![
        MockResponse::ToolCalls(vec![
            MockToolCall {
                provider_metadata: None,
                name: "list_files".into(),
                arguments: json!({ "path": "." }),
            },
            MockToolCall {
                provider_metadata: None,
                name: "bash".into(),
                arguments: json!({ "command": "rm -rf build" }),
            },
        ]),
        MockResponse::Text("**Scripted** model: run with `--live` for a real one.".into()),
    ])
}
