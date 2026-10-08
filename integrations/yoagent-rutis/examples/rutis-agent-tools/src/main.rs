//! rutis-agent's tools as yoagent tools.
//!
//! [rutis-agent](https://github.com/arcships/rutis/tree/v0.7.0/crates/rutis-agent)
//! keeps its tools in a `ToolRegistry` service (`tools_key()`): `ToolDef`s
//! (an aimux `FunctionTool` schema plus an async runner), listed by
//! `schemas()` and run by `execute(call, cancel)`. This example's adapter is
//! a rutis plugin that injects that registry and the bridge's `Registry` and
//! registers one yoagent-rutis `Handler` whose per-run `with_tools` maps every
//! `ToolDef` to a yoagent tool:
//!
//! - its schema (name, description, parameters) is the tool's;
//! - a result is text, a failure (rutis-agent's `ok: false`, its
//!   `error: ...` text) is a real `ToolError::Failed`;
//! - images: rutis-agent's results are text (a runner's JSON value is
//!   serialized). By this adapter's convention a runner that returns
//!   `{"content": [blocks]}` — yoagent's text and image blocks,
//!   `{"type": "image", "data": <base64>, "mimeType": "image/…"}` — gives
//!   yoagent those blocks, images included (rutis-agent's own agent still
//!   sees the JSON text);
//! - yoagent's cancel token *is* the token rutis-agent's `execute` watches,
//!   so cancelling the run stops the runner. The tool returns
//!   `ToolError::Cancelled`, but the transcript shows yoagent's "Tool result
//!   withheld…" error instead: the cancel also cuts off the bridge's `after_tool`,
//!   and yoagent withholds a result an extension didn't finish with.
//!
//! The showcase, in a temporary directory:
//!
//! 1. a yoagent agent edits a file with rutis-agent's own `replace_text`
//!    (create, a `str_replace` that fails because its text is not there, a
//!    good one, view);
//! 2. while the host runs, a `word_count` tool is registered into
//!    rutis-agent's `ToolRegistry` — it appears on the agent's next run, with
//!    no change to the agent or the adapter (hot add);
//! 3. (scripted only) a `dot_picture` tool, added the same way, returns a
//!    picture: it reaches yoagent as an image;
//! 4. (scripted only) a slow tool, added the same way, is cancelled with
//!    `Agent::abort()`: its runner never finishes.
//!
//! The model is scripted by default; `--live` asks DeepSeek instead
//! (`DEEPSEEK_API_KEY`, else the key in `~/.dskey`). Either way the example
//! checks its outcome and exits non-zero if something is off.
//!
//! **Pinning caveat**: see `Cargo.toml`. rutis-agent comes from git (the
//! rutis repository at tag v0.7.0), with a `[patch.crates-io]` for `rutis`, so
//! that the graph holds one `rutis` crate — rutis matches services by Rust
//! type.
//!
//! Run: `cargo run --manifest-path integrations/yoagent-rutis/examples/rutis-agent-tools/Cargo.toml [-- --live]`

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aimux_core::options::Tool;
use rutis::{BoxFuture, CordisError, Ctx, Effect, Plugin, TypeKey};
use rutis_agent::{replace_text_tool, tool_call, tools_key, ToolDef, ToolRegistry, ToolsPlugin};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use yoagent::provider::mock::{MockResponse, MockToolCall};
use yoagent::provider::{
    MockProvider, ModelConfig, ProviderError, StreamConfig, StreamEvent, StreamProvider,
};
use yoagent::{
    Agent, AgentEvent, AgentMessage, AgentTool, Content, Message, ToolContext, ToolError,
    ToolResult,
};
use yoagent_rutis::{Handler, PluginCtxExt, Registry, RutisBridge};

type BoxError = Box<dyn std::error::Error>;

// ── The adapter ─────────────────────────────────────────────────

/// A rutis plugin offering every tool of rutis-agent's `ToolRegistry` to
/// yoagent agents. It waits for (and reloads with) the registry and the
/// bridge.
struct RutisAgentTools {
    injects: Vec<TypeKey>,
}

impl RutisAgentTools {
    fn new() -> Self {
        Self {
            injects: vec![tools_key(), TypeKey::of::<Registry>()],
        }
    }
}

impl Plugin for RutisAgentTools {
    fn name(&self) -> &str {
        "rutis-agent-tools"
    }

    fn injects(&self) -> &[TypeKey] {
        &self.injects
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            let registry = ctx
                .get::<ToolRegistry>()
                .ok_or_else(|| CordisError::ServiceNotFound("rutis_agent::ToolRegistry".into()))?;
            // Per run: a tool registered into rutis-agent's registry later is
            // offered from the next run on.
            ctx.register_handler(
                Handler::new("rutis-agent-tools").with_tools(move |_run| tools_of(&registry)),
            )?;
            Ok(Effect::Done)
        })
    }
}

/// The registry's tools, as yoagent tools, sorted by name (a stable order
/// keeps the request prefix cacheable).
fn tools_of(registry: &Arc<ToolRegistry>) -> Vec<Arc<dyn AgentTool>> {
    let mut tools: Vec<_> = registry
        .schemas()
        .into_iter()
        .filter_map(|schema| match schema {
            Tool::Function(function) => Some(function),
            _ => None, // provider-defined tools have no runner here
        })
        .map(|function| RegistryTool {
            description: function.description.clone().unwrap_or_default(),
            parameters: function.input_schema.clone(),
            name: function.name,
            registry: registry.clone(),
        })
        .collect();
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    tools
        .into_iter()
        .map(|tool| Arc::new(tool) as Arc<dyn AgentTool>)
        .collect()
}

/// One `ToolDef` of rutis-agent's registry, run through `execute`.
struct RegistryTool {
    name: String,
    description: String,
    parameters: Value,
    registry: Arc<ToolRegistry>,
}

#[async_trait::async_trait]
impl AgentTool for RegistryTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn label(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters_schema(&self) -> Value {
        self.parameters.clone()
    }
    async fn execute(&self, params: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
        let call = tool_call(ctx.tool_call_id.clone(), self.name.clone(), params);
        // rutis-agent watches the same token: cancelling the run aborts the
        // runner (and kills a `bash` process group).
        let out = self.registry.execute(&call, &ctx.cancel).await;
        if !out.ok {
            // A finished result stands even if the cancel came right after.
            if ctx.cancel.is_cancelled() {
                return Err(ToolError::Cancelled);
            }
            // rutis-agent already wrote it for the model: `error: ...`.
            return Err(ToolError::Failed(out.output));
        }
        Ok(ToolResult {
            content: content_of(&out.output)
                .unwrap_or_else(|| vec![Content::Text { text: out.output }]),
            details: Value::Null,
        })
    }
}

/// A runner's `{"content": [blocks]}` value (as rutis-agent serialized it),
/// read back as yoagent content: only text and image blocks, and only when
/// the whole value is that shape — any other output stays text.
fn content_of(output: &str) -> Option<Vec<Content>> {
    if !output.starts_with('{') {
        return None;
    }
    let mut value: serde_json::Map<String, Value> = serde_json::from_str(output).ok()?;
    if value.len() != 1 {
        return None;
    }
    let blocks: Vec<Content> = serde_json::from_value(value.remove("content")?).ok()?;
    blocks
        .iter()
        .all(|b| match b {
            Content::Text { .. } => true,
            Content::Image { data, mime_type } => {
                !data.is_empty() && mime_type.starts_with("image/")
            }
            _ => false,
        })
        .then_some(blocks)
}

// ── The host ────────────────────────────────────────────────────

/// A tool registered into rutis-agent's registry while the host runs.
fn word_count_tool() -> ToolDef {
    ToolDef::new(
        "word_count",
        "Count the words in a file.",
        json!({
            "type": "object",
            "properties": { "path": { "type": "string", "description": "Absolute path of the file." } },
            "required": ["path"]
        }),
        |args: Value| async move {
            let path = args["path"].as_str().ok_or("`path` is required")?;
            let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
            Ok(Value::String(format!(
                "{} words",
                text.split_whitespace().count()
            )))
        },
    )
}

/// A 1×1 PNG, base64.
const DOT_PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGMAAQAABQABDQottAAAAABJRU5ErkJggg==";

/// A tool returning a picture: text and an image, in the content-block
/// convention this adapter reads.
fn dot_picture_tool() -> ToolDef {
    ToolDef::new(
        "dot_picture",
        "Shows a picture of a dot.",
        json!({"type": "object", "properties": {}}),
        |_| async move {
            Ok(json!({"content": [
                {"type": "text", "text": "a dot"},
                {"type": "image", "data": DOT_PNG, "mimeType": "image/png"},
            ]}))
        },
    )
}

/// A tool that takes a minute unless cancelled; `finished` says whether its
/// runner ever completed.
fn slow_tool(finished: Arc<AtomicBool>) -> ToolDef {
    ToolDef::new(
        "slow",
        "Takes a minute.",
        json!({"type": "object", "properties": {}}),
        move |_| {
            let finished = finished.clone();
            async move {
                tokio::time::sleep(Duration::from_secs(60)).await;
                finished.store(true, Ordering::SeqCst);
                Ok(Value::String("slept".into()))
            }
        },
    )
}

/// The scripted model, keeping the tool names of every request.
struct Scripted {
    inner: MockProvider,
    offered: Arc<Mutex<Vec<Vec<String>>>>,
}

#[async_trait::async_trait]
impl StreamProvider for Scripted {
    async fn stream(
        &self,
        config: StreamConfig,
        tx: mpsc::UnboundedSender<StreamEvent>,
        cancel: CancellationToken,
    ) -> Result<Message, ProviderError> {
        self.offered
            .lock()
            .unwrap()
            .push(config.tools.iter().map(|t| t.name.clone()).collect());
        self.inner.stream(config, tx, cancel).await
    }
}

fn call(name: &str, args: Value) -> MockResponse {
    MockResponse::ToolCalls(vec![MockToolCall {
        provider_metadata: None,
        name: name.into(),
        arguments: args,
    }])
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

/// The tool results among `messages`: (tool, text, is_error).
fn results(messages: &[AgentMessage]) -> Vec<(String, String, bool)> {
    messages
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
        .collect()
}

/// Run one prompt; print and return this run's tool results.
async fn run(agent: &mut Agent, prompt: &str) -> Result<Vec<(String, String, bool)>, BoxError> {
    println!("\n> {prompt}");
    let before = agent.messages().len();
    let (tx, _rx) = mpsc::unbounded_channel();
    tokio::time::timeout(
        Duration::from_secs(180),
        agent.prompt_with_sender(prompt, tx),
    )
    .await
    .map_err(|_| "the run did not finish within 180 s")?;
    let results = results(&agent.messages()[before..]);
    for (tool, text, is_error) in &results {
        let mark = if *is_error { "error" } else { "ok" };
        let shown: String = text.chars().take(300).collect();
        println!("  [{tool}: {mark}] {shown}");
    }
    Ok(results)
}

fn deepseek_key() -> Result<String, BoxError> {
    if let Ok(key) = std::env::var("DEEPSEEK_API_KEY") {
        if !key.trim().is_empty() {
            return Ok(key.trim().to_string());
        }
    }
    let path = Path::new(&std::env::var("HOME")?).join(".dskey");
    let key = std::fs::read_to_string(&path)
        .map_err(|e| format!("--live needs DEEPSEEK_API_KEY or {}: {e}", path.display()))?;
    Ok(key.split_whitespace().collect())
}

fn check(ok: bool, what: impl Into<String>) -> Result<(), BoxError> {
    if ok {
        Ok(())
    } else {
        Err(what.into().into())
    }
}

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    let live = std::env::args().any(|a| a == "--live");
    let dir = tempfile::tempdir()?;
    let notes = dir.path().join("notes.txt");
    let path = notes.to_string_lossy().to_string();

    // rutis: the bridge, rutis-agent's tool registry with its real
    // `replace_text`, and the adapter.
    let root = Ctx::root()?;
    let bridge = RutisBridge::install(&root)?;
    (&root.plugin(ToolsPlugin::new(vec![replace_text_tool()]))).await?;
    (&root.plugin(RutisAgentTools::new())).await?;
    let registry = root
        .get::<ToolRegistry>()
        .ok_or("rutis-agent's ToolRegistry is not provided")?;

    let offered = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
    let mut agent = if live {
        Agent::from_config(ModelConfig::deepseek("deepseek-flash", "DeepSeek Flash"))
            .with_api_key(deepseek_key()?)
    } else {
        let script = MockProvider::new(vec![
            // Run 1: replace_text.
            call(
                "replace_text",
                json!({"command": "create", "path": path, "file_text": "rutis loads plugins\nyoagent runs agents\n"}),
            ),
            call(
                "replace_text",
                json!({"command": "str_replace", "path": path, "old_str": "not in the file", "new_str": "x"}),
            ),
            call(
                "replace_text",
                json!({"command": "str_replace", "path": path, "old_str": "yoagent runs agents", "new_str": "yoagent runs agents with rutis tools"}),
            ),
            call("replace_text", json!({"command": "view", "path": path})),
            MockResponse::Text("Edited.".into()),
            // Run 2: the tool added while the host ran.
            call("word_count", json!({"path": path})),
            MockResponse::Text("Counted.".into()),
            // Run 3: a picture.
            call("dot_picture", json!({})),
            MockResponse::Text("Seen.".into()),
            // Run 4: cancelled mid-call.
            call("slow", json!({})),
            MockResponse::Text("never sent".into()),
        ]);
        Agent::from_provider(
            Scripted {
                inner: script,
                offered: offered.clone(),
            },
            ModelConfig::mock(),
        )
    }
    .with_system_prompt("You edit files with the tools you have. Use absolute paths. Be brief.")
    .with_extension(bridge.extension());

    // 1. rutis-agent's replace_text, driven by a yoagent agent.
    let first = run(
        &mut agent,
        &format!(
            "Create {path} with the two lines `rutis loads plugins` and `yoagent runs agents`, \
             then change the second line to `yoagent runs agents with rutis tools`, \
             then view the file."
        ),
    )
    .await?;
    let content = std::fs::read_to_string(&notes).unwrap_or_default();
    check(
        content == "rutis loads plugins\nyoagent runs agents with rutis tools\n",
        format!("replace_text left {path} as {content:?}"),
    )?;
    check(
        first.iter().any(|(t, _, e)| t == "replace_text" && !e),
        "no replace_text call succeeded",
    )?;
    if !live {
        // A failure is a real tool error, carrying rutis-agent's text.
        check(
            first
                .iter()
                .any(|(_, text, e)| *e && text.contains("error:")),
            format!("the failing str_replace was not an error: {first:?}"),
        )?;
    }

    // 2. Hot add: register a tool into rutis-agent's registry while the host
    //    runs. Nothing else changes; the next run offers it.
    registry.register(word_count_tool());
    let second = run(
        &mut agent,
        &format!("How many words are in {path}? Use word_count."),
    )
    .await?;
    check(
        second
            .iter()
            .any(|(t, text, e)| t == "word_count" && !e && text == "9 words"),
        format!("word_count did not answer `9 words`: {second:?}"),
    )?;

    if !live {
        let offered = offered.lock().unwrap().clone();
        check(
            !offered[0].contains(&"word_count".to_string())
                && offered.last().unwrap().contains(&"word_count".to_string()),
            format!("word_count was not hot-added between the runs: {offered:?}"),
        )?;

        // 3. Images: a runner's content blocks reach yoagent as an image.
        registry.register(dot_picture_tool());
        let before = agent.messages().len();
        run(&mut agent, "Show me the dot.").await?;
        let image = agent.messages()[before..].iter().any(|m| {
            matches!(m, AgentMessage::Llm(Message::ToolResult { tool_name, content, .. })
                if tool_name == "dot_picture"
                    && content.iter().any(|c| matches!(c, Content::Image { data, .. } if data == DOT_PNG)))
        });
        println!("  dot_picture returned an image: {image}");
        check(image, "dot_picture's image did not reach yoagent")?;

        // 4. Cancel: yoagent's token stops rutis-agent's runner.
        let finished = Arc::new(AtomicBool::new(false));
        registry.register(slow_tool(finished.clone()));
        let mut events = agent.prompt("Take your time.").await;
        while let Some(event) = events.recv().await {
            if matches!(&event, AgentEvent::ToolExecutionStart { tool_name, .. } if tool_name == "slow")
            {
                tokio::time::sleep(Duration::from_millis(100)).await;
                agent.abort();
            }
        }
        agent.finish().await;
        let third = results(agent.messages());
        let slow = third.iter().rev().find(|(t, ..)| t == "slow");
        println!("\n> (cancelled) {slow:?}");
        check(
            matches!(slow, Some((_, _, true))),
            format!("the cancelled call was no error: {slow:?}"),
        )?;
        tokio::time::sleep(Duration::from_millis(200)).await;
        check(
            !finished.load(Ordering::SeqCst),
            "the slow runner kept going",
        )?;
    }

    tokio::time::timeout(Duration::from_secs(10), root.shutdown())
        .await
        .map_err(|_| "shutdown did not finish within 10 s")??;
    println!("\nok");
    Ok(())
}
