//! A TypeScript and a Python plugin extending a yoagent agent, offline
//! (MockProvider): the host side of `plugins/ts/example.ts` and
//! `plugins/python/yoagent_example.py`.
//!
//! The bridge never loads plugins; the host does, here with rutis-loader
//! rows, one local runtime per language. The `yoagent` service must be
//! shared in the loader's catalog so the rows can inject it.
//!
//! Setup (once): `npm ci` in `plugins/`, and a Python 3.12+ with rutis 0.7:
//! `uv venv plugins/.venv --python 3.12 && uv pip install --python plugins/.venv/bin/python rutis==0.7.0`.
//!
//! Run: `cargo run --manifest-path integrations/yoagent-rutis/Cargo.toml --features node,python --example language_plugins`

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rutis::Ctx;
use rutis_bridge::runtime::LocalRuntime;
use rutis_loader::{
    Chain, Layer, LoaderOptions, LoaderPlugin, Patch, RuntimeResolver, RuntimeRowsPlugin,
    ServiceCatalog,
};
use serde_json::json;
use yoagent::provider::mock::{MockResponse, MockToolCall};
use yoagent::provider::{MockProvider, ModelConfig};
use yoagent::{
    Agent, AgentMessage, AgentTool, Content, Message, ToolContext, ToolError, ToolResult,
};
use yoagent_rutis::RutisBridge;

type BoxError = Box<dyn std::error::Error>;

/// A host tool whose output carries a secret, for the redactors.
struct Env;

#[async_trait::async_trait]
impl AgentTool for Env {
    fn name(&self) -> &str {
        "env"
    }
    fn label(&self) -> &str {
        "env"
    }
    fn description(&self) -> &str {
        "print the environment"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({"type": "object", "properties": {}})
    }
    async fn execute(&self, _: serde_json::Value, _: ToolContext) -> Result<ToolResult, ToolError> {
        Ok(ToolResult {
            content: vec![Content::Text {
                text: "OPENAI_KEY= sk-live-123".into(),
            }],
            details: serde_json::Value::Null,
        })
    }
}

fn plugins() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins")
}

fn call(name: &str, args: serde_json::Value) -> MockToolCall {
    MockToolCall {
        provider_metadata: None,
        name: name.into(),
        arguments: args,
    }
}

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    let node_runtime = plugins().join("node_modules/@arcships/rutis-runtime");
    let python = std::env::var_os("YOAGENT_RUTIS_PYTHON")
        .map(PathBuf::from)
        .unwrap_or_else(|| plugins().join(".venv/bin/python"));
    if !node_runtime.exists() || !python.exists() {
        return Err(format!(
            "needs {} (`npm ci` in plugins/) and {} (see the setup at the top of this file)",
            node_runtime.display(),
            python.display()
        )
        .into());
    }

    let root = Ctx::root()?;
    // With the `node` / `python` features, installing also provides the
    // `yoagent` host service to language plugins.
    let bridge = RutisBridge::install(&root)?;

    // The host's part: runtimes, a loader, and `yoagent` shared by name.
    let mut catalog = ServiceCatalog::new();
    catalog.register_shared("yoagent");
    let node = LocalRuntime::node(&node_runtime, plugins().join("package.json"));
    let node_rows = Arc::new(RuntimeResolver::node(node.handle()).with_catalog(&catalog));
    let py = LocalRuntime::python(plugins().join("python")).interpreter(&python);
    let py_rows = Arc::new(RuntimeResolver::modules(py.handle()).with_catalog(&catalog));
    root.plugin(node);
    root.plugin(py);
    let loader_plugin = LoaderPlugin::new(
        Chain::new()
            .with_shared(py_rows.clone())
            .with_shared(node_rows.clone()),
        LoaderOptions {
            catalog,
            ..LoaderOptions::default()
        },
    );
    let loader = loader_plugin.handle();
    root.plugin(loader_plugin).await?;
    root.plugin(RuntimeRowsPlugin::new(node_rows));
    root.plugin(RuntimeRowsPlugin::new(py_rows));

    let rows: Vec<Patch> = serde_json::from_value(json!([{ "insert": [
        { "id": "ts", "name": plugins().join("ts/example.ts"), "config": { "denied": ["bash"] } },
        { "id": "py", "name": "py:yoagent_example", "config": { "denied": ["rm"] } },
    ] }]))?;
    let report = loader
        .reconcile(vec![Layer::new("app", rows)], None)
        .await?;
    if !report.failures.is_empty() {
        return Err(format!("plugins failed to load: {report:?}").into());
    }
    // Rows start asynchronously: wait until both handlers are registered.
    tokio::time::timeout(Duration::from_secs(30), async {
        while bridge.registry().handlers().len() < 2 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .map_err(|_| format!("handlers: {:?}", bridge.registry().handlers()))?;
    for handler in bridge.registry().handlers() {
        println!("registered {} {:?}", handler.name(), handler.hooks());
    }

    let script = vec![
        MockResponse::ToolCalls(vec![
            call("ts_word_count", json!({"text": "one two three"})),
            call("py_reverse", json!({"text": "stressed"})),
            call("env", json!({})),
            call("bash", json!({"command": "ls"})),
        ]),
        MockResponse::Text("All done.".into()),
    ];
    let mut agent = Agent::from_provider(MockProvider::new(script), ModelConfig::mock())
        .with_tools(vec![Box::new(Env)])
        .with_extension(bridge.extension().filters_tool_output());
    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    tokio::time::timeout(
        Duration::from_secs(30),
        agent.prompt_with_sender("Count, reverse, and peek.", events),
    )
    .await
    .map_err(|_| "the agent run did not finish within 30 s")?;

    let mut lines = Vec::new();
    for message in agent.messages() {
        if let AgentMessage::Llm(Message::ToolResult {
            tool_name,
            content,
            is_error,
            ..
        }) = message
        {
            let text: String = content
                .iter()
                .filter_map(|c| match c {
                    Content::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            let mark = if *is_error { "denied/error" } else { "ok" };
            lines.push(format!("{tool_name}: [{mark}] {text}"));
        }
    }
    for line in &lines {
        println!("{line}");
    }
    let expected = [
        "ts_word_count: [ok] 3 words",
        "py_reverse: [ok] desserts",
        "env: [ok] OPENAI_KEY= [key]",
        "bash: [denied/error] Tool call denied: `bash` is disabled by the ts-example plugin",
    ];
    if lines != expected {
        return Err(format!("unexpected tool results: {lines:?}").into());
    }
    tokio::time::timeout(Duration::from_secs(10), root.shutdown())
        .await
        .map_err(|_| "shutdown did not finish within 10 s")??;
    Ok(())
}
