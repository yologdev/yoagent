//! An unchanged dsh (DeepSeek Harness) tool plugin, `dsh-free-search`, as
//! tools of a yoagent agent: dsh's own packages and the adapter
//! `plugins/dsh/dsh-tools-adapter.ts` run as rutis-loader rows in one Node
//! runtime; the adapter registers dsh's tool registry with the bridge.
//!
//! By default the model is scripted (MockProvider) but the tool is real:
//! **`platform_search` searches the web, so this needs network access.**
//! `--live` asks DeepSeek instead (`DEEPSEEK_API_KEY`). Either way the example checks its outcome and exits non-zero
//! when the dsh tools were not offered or did not answer.
//!
//! Setup (once): `npm ci` in `plugins/dsh/` (Node 24+).
//!
//! Run: `cargo run --manifest-path integrations/yoagent-rutis/Cargo.toml --features node --example dsh_tools [-- --live]`

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
use yoagent::{Agent, AgentEvent, AgentMessage, Content, Message};
use yoagent_rutis::RutisBridge;

type BoxError = Box<dyn std::error::Error>;

fn dsh_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins/dsh")
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

/// The scripted model, keeping the latest user turn of each request (where
/// the adapter's turn note lands).
struct Scripted {
    inner: MockProvider,
    last_user: Arc<Mutex<Vec<String>>>,
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
        self.last_user
            .lock()
            .unwrap()
            .push(last.unwrap_or_default());
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
    let live = std::env::args().any(|a| a == "--live");
    let runtime = dsh_dir().join("node_modules/@arcships/rutis-runtime");
    if !runtime.exists() || !dsh_dir().join("node_modules/dsh-free-search").exists() {
        return Err(format!("run `npm ci` in {} first", dsh_dir().display()).into());
    }

    let root = Ctx::root()?;
    let bridge = RutisBridge::install(&root)?;

    // One Node runtime, its package.json the dsh directory's; `yoagent`
    // shared by name so the adapter row can inject it.
    let mut catalog = ServiceCatalog::new();
    catalog.register_shared("yoagent");
    let node = LocalRuntime::node(&runtime, dsh_dir().join("package.json"));
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

    // dsh's own packages, unchanged, then the adapter.
    let patches: Vec<Patch> = serde_json::from_value(json!([{ "insert": [
        { "id": "web", "name": "@deepseek-ai/dsh-web" },
        { "id": "system-prompt", "name": "@deepseek-ai/dsh-system-prompt" },
        { "id": "tools", "name": "@deepseek-ai/dsh-tools" },
        {
            "id": "free-search",
            "name": "dsh-free-search",
            "config": { "provider": "bing", "disabledEngines": [], "bingMarket": "en-US" },
        },
        { "id": "adapter", "name": dsh_dir().join("dsh-tools-adapter.ts") },
    ] }]))?;
    let report = loader
        .reconcile(vec![Layer::new("app", patches)], None)
        .await?;
    if !report.failures.is_empty() {
        return Err(format!("rows failed to load: {report:?}").into());
    }
    tokio::time::timeout(Duration::from_secs(60), async {
        while bridge.registry().handlers().is_empty() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .map_err(|_| "the adapter never registered its handler")?;

    let last_user = Arc::new(Mutex::new(Vec::new()));
    let prompt = "Find the yoagent repository on GitHub and say who maintains it. Use one search.";
    let mut agent = if live {
        Agent::from_config(ModelConfig::deepseek("deepseek-flash", "DeepSeek Flash"))
            .with_api_key(deepseek_key()?)
    } else {
        let script = MockProvider::new(vec![
            MockResponse::ToolCalls(vec![MockToolCall {
                provider_metadata: None,
                name: "platform_search".into(),
                arguments: json!({"query": "yoagent", "platform": "github"}),
            }]),
            MockResponse::Text("(scripted) The search ran.".into()),
        ]);
        let scripted = Scripted {
            inner: script,
            last_user: last_user.clone(),
        };
        Agent::from_provider(scripted, ModelConfig::mock())
    }
    .with_system_prompt("You answer questions with the search tools you have. Be brief.")
    .with_extension(bridge.extension());

    // The agent has no tools of its own: every tool call is a dsh tool's.
    let (tx, mut rx) = mpsc::unbounded_channel();
    let printer = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            if let AgentEvent::ToolExecutionStart {
                tool_name, args, ..
            } = &event
            {
                println!("> {tool_name} {args}");
            }
        }
    });
    tokio::time::timeout(
        Duration::from_secs(180),
        agent.prompt_with_sender(prompt, tx),
    )
    .await
    .map_err(|_| "the run did not finish within 180 s")?;
    printer.await?;

    // What happened, and the checks.
    let mut dsh_ok = 0;
    let mut answer = String::new();
    for message in agent.messages() {
        match message {
            AgentMessage::Llm(Message::ToolResult {
                tool_name,
                content,
                is_error,
                ..
            }) => {
                let text = text_of(content);
                let shown: String = text.chars().take(400).collect();
                let mark = if *is_error { "error" } else { "ok" };
                println!("[{tool_name}: {mark}] {shown}");
                if !*is_error && !text.trim().is_empty() {
                    dsh_ok += 1;
                }
            }
            AgentMessage::Llm(Message::Assistant { content, .. }) => {
                let text = text_of(content);
                if !text.trim().is_empty() {
                    answer = text;
                }
            }
            _ => {}
        }
    }
    println!("assistant: {answer}");
    let _ = tokio::time::timeout(Duration::from_secs(10), root.shutdown()).await;

    if dsh_ok == 0 {
        return Err("no dsh tool call succeeded (is the network up?)".into());
    }
    if answer.trim().is_empty() {
        return Err("the model gave no answer".into());
    }
    if !live {
        // The adapter's note carried dsh-free-search's prompt section.
        let first = last_user
            .lock()
            .unwrap()
            .first()
            .cloned()
            .unwrap_or_default();
        if !first.contains("[Guidance from dsh plugins]") || !first.contains("free-search") {
            return Err(format!("the dsh prompt note did not reach the model: {first}").into());
        }
        println!("note: {} chars from dsh plugins", first.len());
    }
    println!("ok: {dsh_ok} dsh tool call(s) answered");
    Ok(())
}
