//! pi coding-agent extensions, unchanged, in a yoagent agent: the adapter
//! `plugins/pi/pi-extensions-adapter.ts` loads them with pi's own loader in a
//! rutis Node runtime and registers their tools and tool policies with the
//! bridge. The agent also has yoagent's built-in tools, which the policies
//! judge under pi's names (`write_file` as `write`, `bash` as `bash`, ...).
//!
//! Pass extension files as arguments (default: the adapter's test fixture),
//! e.g. examples from pi's repository (`packages/coding-agent/examples/extensions/`).
//! The project they see (`ctx.cwd`) is a fresh temporary directory.
//!
//! By default the model is scripted (MockProvider): it asks for a write to
//! `.env` and a `sudo` command (only echoed) through yoagent's own tools, so
//! the policies decide,
//! and the example prints the tools offered, the prompt note and each
//! outcome. `--live` gives the agent to DeepSeek instead (`DEEPSEEK_API_KEY`) with `--prompt "<text>"` (a default asks it to
//! use whatever tools it has).
//!
//! The project is also the process's working directory, so yoagent's own
//! tools (`bash`, relative paths) act there, not where `cargo run` started.
//! `--without NAME` leaves a yoagent built-in out (repeatable) and tells the
//! adapter so (`withoutBuiltins`): needed when an extension replaces one
//! under the same name, e.g. pi's sandboxed `bash` — the adapter refuses to
//! load otherwise, since yoagent's own tool would win and run unsandboxed.
//!
//! Setup (once): `npm ci` in `plugins/pi/` (Node 24+).
//!
//! Run: `cargo run --manifest-path integrations/yoagent-rutis/Cargo.toml --features node --example pi_extensions [-- [--live] [--prompt TEXT] [--without NAME] EXT.ts ...]`

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rutis::Ctx;
use rutis_bridge::runtime::LocalRuntime;
use rutis_loader::{
    Chain, Layer, LoaderOptions, LoaderPlugin, Patch, RuntimeResolver, RuntimeRowsPlugin,
    ServiceCatalog,
};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use yoagent::provider::mock::{MockResponse, MockToolCall};
use yoagent::provider::{
    MockProvider, ModelConfig, ProviderError, StreamConfig, StreamEvent, StreamProvider,
};
use yoagent::tools::default_tools;
use yoagent::{Agent, AgentEvent, AgentMessage, Content, Message};
use yoagent_rutis::RutisBridge;

type BoxError = Box<dyn std::error::Error>;
/// Each request's tool names and latest user turn.
type SeenRequests = Arc<Mutex<Vec<(Vec<String>, String)>>>;

fn pi_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins/pi")
}

fn text_of(content: &[Content]) -> String {
    content
        .iter()
        .filter_map(|c| match c {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The scripted model, keeping each request's tools and latest user turn
/// (where the adapter's note lands).
struct Scripted {
    inner: MockProvider,
    seen: SeenRequests,
}

#[async_trait::async_trait]
impl StreamProvider for Scripted {
    async fn stream(
        &self,
        config: StreamConfig,
        tx: mpsc::UnboundedSender<StreamEvent>,
        cancel: CancellationToken,
    ) -> Result<Message, ProviderError> {
        let last = config.messages.iter().rev().find_map(|m| match m {
            Message::User { content, .. } => Some(text_of(content)),
            _ => None,
        });
        let tools = config.tools.iter().map(|t| t.name.clone()).collect();
        self.seen
            .lock()
            .unwrap()
            .push((tools, last.unwrap_or_default()));
        self.inner.stream(config, tx, cancel).await
    }
}

fn deepseek_key() -> Result<String, BoxError> {
    match std::env::var("DEEPSEEK_API_KEY") {
        Ok(key) if !key.trim().is_empty() => Ok(key.trim().to_string()),
        _ => Err("--live needs DEEPSEEK_API_KEY".into()),
    }
}

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    let mut live = false;
    let mut without = Vec::new();
    let mut prompt = "Use the tools you have to do one small useful thing in this project, then say what you did. Be brief.".to_string();
    let mut extensions = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--live" => live = true,
            "--prompt" => prompt = args.next().ok_or("--prompt needs a value")?,
            "--without" => without.push(args.next().ok_or("--without needs a tool name")?),
            path => extensions.push(std::fs::canonicalize(path)?),
        }
    }
    let fixture = extensions.is_empty();
    if fixture {
        extensions.push(pi_dir().join("fixture-extension.ts"));
    }
    let runtime = pi_dir().join("node_modules/@arcships/rutis-runtime");
    if !runtime.exists()
        || !pi_dir()
            .join("node_modules/@earendil-works/pi-coding-agent")
            .exists()
    {
        return Err(format!("run `npm ci` in {} first", pi_dir().display()).into());
    }
    let project = tempfile::tempdir()?;
    // Extension paths are canonical already; from here on, relative paths and
    // `bash` act in the project the extensions see as `ctx.cwd`.
    std::env::set_current_dir(project.path())?;
    std::fs::write(project.path().join("README.md"), "# demo project\n")?;

    let root = Ctx::root()?;
    let bridge = RutisBridge::install(&root)?;
    let mut catalog = ServiceCatalog::new();
    catalog.register_shared("yoagent");
    let node = LocalRuntime::node(&runtime, pi_dir().join("package.json"));
    let rows = Arc::new(RuntimeResolver::node(node.handle()).with_catalog(&catalog));
    root.plugin(node);
    let loader_plugin = LoaderPlugin::new(
        Chain::new().with_shared(rows.clone()),
        LoaderOptions {
            catalog,
            ..LoaderOptions::default()
        },
    );
    let loader = loader_plugin.handle();
    root.plugin(loader_plugin).await?;
    root.plugin(RuntimeRowsPlugin::new(rows));

    let patches: Vec<Patch> = serde_json::from_value(json!([{ "insert": [{
        "id": "pi",
        "name": pi_dir().join("pi-extensions-adapter.ts"),
        "config": {
            "extensions": extensions,
            "cwd": project.path(),
            // yoagent built-ins left out, so pi tools of the same name are the ones that run.
            "withoutBuiltins": without,
        },
    }] }]))?;
    let report = loader
        .reconcile(vec![Layer::new("app", patches)], None)
        .await?;
    if !report.failures.is_empty() {
        return Err(format!("the adapter failed to load: {report:?}").into());
    }
    tokio::time::timeout(Duration::from_secs(60), async {
        while bridge.registry().handlers().is_empty() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .map_err(|_| "the adapter never registered its handler")?;

    let seen = Arc::new(Mutex::new(Vec::new()));
    let env_file = project.path().join(".env");
    let mut agent = if live {
        Agent::from_config(ModelConfig::deepseek("deepseek-flash", "DeepSeek Flash"))
            .with_api_key(deepseek_key()?)
    } else {
        prompt = "(scripted)".into();
        let script = MockProvider::new(vec![
            MockResponse::ToolCalls(vec![
                MockToolCall {
                    provider_metadata: None,
                    name: "write_file".into(),
                    arguments: json!({"path": env_file, "content": "TOKEN=1"}),
                },
                // Harmless if a policy lets it through; pi's permission-gate
                // flags `sudo`.
                MockToolCall {
                    provider_metadata: None,
                    name: "bash".into(),
                    arguments: json!({"command": "echo sudo rm -rf build"}),
                },
            ]),
            MockResponse::Text("(scripted) done.".into()),
        ]);
        Agent::from_provider(
            Scripted {
                inner: script,
                seen: seen.clone(),
            },
            ModelConfig::mock(),
        )
    }
    .with_system_prompt(format!(
        "You are a coding agent working in {}. Be brief.",
        project.path().display()
    ))
    .with_tools(
        default_tools()
            .into_iter()
            .filter(|tool| !without.iter().any(|name| name == tool.name()))
            .collect(),
    )
    .with_extension(bridge.extension());

    let (tx, mut rx) = mpsc::unbounded_channel();
    let printer = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            if let AgentEvent::ToolExecutionStart {
                tool_name, args, ..
            } = &event
            {
                let args: String = args.to_string().chars().take(200).collect();
                println!("> {tool_name} {args}");
            }
        }
    });
    tokio::time::timeout(
        Duration::from_secs(300),
        agent.prompt_with_sender(prompt, tx),
    )
    .await
    .map_err(|_| "the run did not finish within 300 s")?;
    printer.await?;

    if let Some((tools, note)) = seen.lock().unwrap().first() {
        println!("tools offered: {}", tools.join(", "));
        if let Some((_, added)) = note.split_once("(scripted)") {
            if !added.trim().is_empty() {
                println!("note: {}", added.trim());
            }
        }
    }
    for message in agent.messages() {
        match message {
            AgentMessage::Llm(Message::ToolResult {
                tool_name,
                content,
                is_error,
                ..
            }) => {
                let text: String = text_of(content).chars().take(300).collect();
                let mark = if *is_error { "error" } else { "ok" };
                println!("[{tool_name}: {mark}] {text}");
            }
            AgentMessage::Llm(Message::Assistant {
                content,
                error_message: Some(e),
                ..
            }) if text_of(content).is_empty() => println!("error: {e}"),
            AgentMessage::Llm(Message::Assistant { content, .. }) => {
                let text = text_of(content);
                if !text.trim().is_empty() {
                    println!("assistant: {text}");
                }
            }
            _ => {}
        }
    }
    let written = env_file.exists();
    println!(".env written: {written}");
    let offered = seen.lock().unwrap().first().map(|(t, _)| t.clone());
    let results: Vec<(String, String, bool)> = agent
        .messages()
        .iter()
        .filter_map(|m| match m {
            AgentMessage::Llm(Message::ToolResult {
                tool_name,
                content,
                is_error,
                ..
            }) => Some((tool_name.clone(), text_of(content), *is_error)),
            _ => None,
        })
        .collect();
    tokio::time::timeout(Duration::from_secs(10), root.shutdown())
        .await
        .map_err(|_| "the plugins did not shut down within 10 s")??;

    // Scripted with the fixture: its tools were offered, and its policy denied
    // the write (not a failed run or an unavailable handler).
    if fixture && !live {
        if written {
            return Err("the fixture's policy should have blocked the write to .env".into());
        }
        if !offered.unwrap_or_default().iter().any(|t| t == "pi_echo") {
            return Err("the fixture's pi_echo tool was not offered".into());
        }
        let denied = results.iter().any(|(tool, text, is_error)| {
            tool == "write_file" && *is_error && text.contains("is protected")
        });
        if !denied {
            return Err(format!("the fixture's policy did not deny the write: {results:?}").into());
        }
        if !results.iter().any(|(tool, _, _)| tool == "bash") {
            return Err(format!("the bash call never ran: {results:?}").into());
        }
        println!("ok: the fixture's tools were offered and its policy denied the write");
    }
    Ok(())
}
