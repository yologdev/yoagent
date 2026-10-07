//! Three rutis plugins extending a yoagent agent, offline (MockProvider).
//!
//! - `text-tools` contributes a `word_count` tool.
//! - `policy` denies tools by name and caps how often each tool may run.
//! - `redactor` masks API keys in every tool result (`after_tool`).
//!
//! The host installs the bridge's one extension; an observer on the rutis
//! bus prints each tool result as the model saw it.
//!
//! Run: `cargo run --manifest-path integrations/yoagent-rutis/Cargo.toml --example policy_plugin`

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rutis::{BoxFuture, CordisError, Ctx, Effect, FiberState, FiberView, Plugin, TypeKey};
use yoagent::extension::ToolOutput;
use yoagent::provider::mock::{MockResponse, MockToolCall};
use yoagent::provider::{MockProvider, ModelConfig};
use yoagent::{
    Agent, AgentEvent, AgentTool, Content, ToolContext, ToolDecision, ToolError, ToolResult,
};
use yoagent_rutis::{AgentPlugin, Handler, PluginCtxExt, Registry, RutisBridge};

/// The tool the `text-tools` plugin contributes.
struct WordCount;

#[async_trait::async_trait]
impl AgentTool for WordCount {
    fn name(&self) -> &str {
        "word_count"
    }
    fn label(&self) -> &str {
        "Word count"
    }
    fn description(&self) -> &str {
        "Count the words in a text"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"]
        })
    }
    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let text = params["text"].as_str().unwrap_or_default();
        Ok(ToolResult {
            content: vec![Content::Text {
                text: format!("{} words", text.split_whitespace().count()),
            }],
            details: serde_json::Value::Null,
        })
    }
}

/// A policy plugin: deny some tools outright, cap the rest per plugin
/// generation (a reload or config update resets the counts).
struct Policy {
    denied: Vec<&'static str>,
    max_calls_per_tool: usize,
    injects: Vec<TypeKey>,
}

impl Plugin for Policy {
    fn name(&self) -> &str {
        "policy"
    }
    fn injects(&self) -> &[TypeKey] {
        &self.injects
    }
    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        let denied = self.denied.clone();
        let cap = self.max_calls_per_tool;
        // Fresh for each generation: built in `apply`.
        let counts: Mutex<HashMap<String, usize>> = Mutex::default();
        Box::pin(async move {
            ctx.register_handler(Handler::new("policy").with_before_tool(move |call| {
                if denied.contains(&call.tool.as_str()) {
                    return ToolDecision::Deny(format!("`{}` is disabled", call.tool));
                }
                let mut counts = counts.lock().unwrap();
                let n = counts.entry(call.tool.clone()).or_default();
                if *n >= cap {
                    return ToolDecision::Deny(format!(
                        "rate cap: `{}` may run at most {cap} times",
                        call.tool
                    ));
                }
                *n += 1;
                ToolDecision::Allow
            }))?;
            Ok(Effect::Done)
        })
    }
}

/// Mask anything that looks like an API key (`sk-...`).
fn redact(output: &mut ToolOutput) {
    for block in &mut output.result.content {
        if let Content::Text { text } = block {
            *text = text
                .split(' ')
                .map(|word| {
                    if word.starts_with("sk-") {
                        "[key]"
                    } else {
                        word
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");
        }
    }
}

/// Stand-in for a tool whose output may carry a secret.
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
        serde_json::json!({"type": "object", "properties": {}})
    }
    async fn execute(&self, _: serde_json::Value, _: ToolContext) -> Result<ToolResult, ToolError> {
        Ok(ToolResult {
            content: vec![Content::Text {
                text: "HOME=/home/me OPENAI_KEY= sk-live-123".into(),
            }],
            details: serde_json::Value::Null,
        })
    }
}

/// Stand-in for a dangerous tool the host happens to have.
struct Bash;

#[async_trait::async_trait]
impl AgentTool for Bash {
    fn name(&self) -> &str {
        "bash"
    }
    fn label(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        "run a shell command"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {"command": {"type": "string"}}})
    }
    async fn execute(&self, _: serde_json::Value, _: ToolContext) -> Result<ToolResult, ToolError> {
        unreachable!("the policy plugin denies bash")
    }
}

fn call(name: &str, args: serde_json::Value) -> MockResponse {
    MockResponse::ToolCalls(vec![MockToolCall {
        provider_metadata: None,
        name: name.into(),
        arguments: args,
    }])
}

type BoxError = Box<dyn std::error::Error>;

/// Wait until the plugin is active; fail if it failed, was disposed, or took
/// too long.
async fn wait_active(view: &FiberView) -> Result<(), BoxError> {
    let mut rx = view.watch();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = rx.borrow_and_update().clone();
            match snapshot.state {
                FiberState::Active => return Ok(()),
                FiberState::Failed | FiberState::Disposed => {
                    return Err(
                        format!("plugin `{}` did not start: {snapshot:?}", view.name()).into(),
                    )
                }
                _ => rx.changed().await?,
            }
        }
    })
    .await
    .map_err(|_| format!("plugin `{}` did not start within 5 s", view.name()))?
}

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    let root = Ctx::root()?;
    let bridge = RutisBridge::install(&root)?;

    // Plugins can be loaded (and unloaded) at any time; each run sees the
    // set active when it starts.
    let tools = root.plugin(AgentPlugin::new(
        Handler::new("text-tools").with_tool(WordCount),
    ));
    let policy = root.plugin(Policy {
        denied: vec!["bash"],
        max_calls_per_tool: 2,
        injects: vec![TypeKey::of::<Registry>()],
    });
    let redactor = root.plugin(AgentPlugin::new(Handler::new("redactor").with_after_tool(
        |_call, output| {
            redact(output);
            Ok(())
        },
    )));
    wait_active(&tools).await?;
    wait_active(&policy).await?;
    wait_active(&redactor).await?;

    // Observe the agent's events on the bus. (A plugin would do the same
    // from its `apply`; registered on the root, it lives as long as the root.)
    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    {
        let log = log.clone();
        root.on_agent_event(move |emitted| {
            if let AgentEvent::ToolExecutionEnd {
                tool_name,
                result,
                is_error,
                ..
            } = emitted.event()
            {
                let text: String = result
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        Content::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                let mark = if *is_error { "denied/error" } else { "ok" };
                log.lock()
                    .unwrap()
                    .push(format!("{tool_name}: [{mark}] {text}"));
            }
        })?;
    }

    let script = vec![
        call("word_count", serde_json::json!({"text": "one two three"})),
        call("word_count", serde_json::json!({"text": "four five"})),
        call("word_count", serde_json::json!({"text": "six"})),
        call("env", serde_json::json!({})),
        call("bash", serde_json::json!({"command": "rm -rf /"})),
        MockResponse::Text("All done.".into()),
    ];
    let mut agent = Agent::from_provider(MockProvider::new(script), ModelConfig::mock())
        .with_tools(vec![Box::new(Bash), Box::new(Env)])
        // The redactor is trusted: withhold partial output, send only the
        // filtered result.
        .with_extension(bridge.extension().filters_tool_output());

    tokio::time::timeout(
        Duration::from_secs(10),
        agent.prompt("Count some words, then clean up."),
    )
    .await
    .map_err(|_| "the agent run did not finish within 10 s")?;

    // Bus dispatch is asynchronous: wait (bounded) for the listener.
    let expected = [
        "word_count: [ok] 3 words",
        "word_count: [ok] 2 words",
        "word_count: [denied/error] Tool call denied: rate cap: `word_count` may run at most 2 times",
        "env: [ok] HOME=/home/me OPENAI_KEY= [key]",
        "bash: [denied/error] Tool call denied: `bash` is disabled",
    ];
    tokio::time::timeout(Duration::from_secs(5), async {
        while log.lock().unwrap().len() < expected.len() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_| format!("only these events arrived: {:?}", log.lock().unwrap()))?;

    let lines = log.lock().unwrap().clone();
    for line in &lines {
        println!("{line}");
    }
    if lines != expected {
        return Err(format!("unexpected tool results: {lines:?}").into());
    }

    tokio::time::timeout(Duration::from_secs(5), root.shutdown())
        .await
        .map_err(|_| "shutdown did not finish within 5 s")??;
    Ok(())
}
