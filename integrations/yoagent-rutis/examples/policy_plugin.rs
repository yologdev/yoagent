//! Two rutis plugins extending a yoagent agent, offline (MockProvider).
//!
//! - `text-tools` contributes a `word_count` tool.
//! - `policy` denies tools by name and caps how often each tool may run.
//!
//! Run: `cargo run --manifest-path integrations/yoagent-rutis/Cargo.toml --example policy_plugin`

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rutis::{BoxFuture, CordisError, Ctx, Effect, FiberState, FiberView, Plugin, TypeKey};
use yoagent::provider::mock::{MockResponse, MockToolCall};
use yoagent::provider::{MockProvider, ModelConfig};
use yoagent::{Agent, AgentEvent, AgentTool, Content, ToolContext, ToolError, ToolResult};
use yoagent_rutis::{
    AgentPlugin, AgentRutisExt, PluginCtxExt, RutisBridge, ToolRegistry, ToolVerdict,
};

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
        let counts: Mutex<HashMap<String, usize>> = Mutex::default();
        Box::pin(async move {
            ctx.on_tool_call(move |call| {
                if denied.contains(&call.tool_name()) {
                    return ToolVerdict::deny(format!("`{}` is disabled", call.tool_name()));
                }
                let mut counts = counts.lock().unwrap();
                let n = counts.entry(call.tool_name().to_string()).or_default();
                if *n >= cap {
                    return ToolVerdict::deny(format!(
                        "rate cap: `{}` may run at most {cap} times",
                        call.tool_name()
                    ));
                }
                *n += 1;
                ToolVerdict::Allow
            })?;
            Ok(Effect::Done)
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

async fn wait_active(view: &FiberView) {
    let mut rx = view.watch();
    while rx.borrow_and_update().state != FiberState::Active {
        rx.changed().await.expect("fiber driver alive");
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = Ctx::root()?;
    let bridge = RutisBridge::install(&root)?;

    // Plugins can be loaded (and unloaded) at any time; each run sees the
    // set active when it starts.
    let tools = root.plugin(AgentPlugin::new("text-tools").with_tool(WordCount));
    let policy = root.plugin(Policy {
        denied: vec!["bash"],
        max_calls_per_tool: 2,
        injects: vec![TypeKey::of::<ToolRegistry>()],
    });
    wait_active(&tools).await;
    wait_active(&policy).await;

    // Observe the agent's events on the bus. (A plugin would do the same
    // from its `apply`; registered on the root, it lives as long as the root.)
    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    {
        let log = log.clone();
        root.on_agent_event(move |event| {
            if let AgentEvent::ToolExecutionEnd {
                tool_name,
                result,
                is_error,
                ..
            } = event
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
        call("bash", serde_json::json!({"command": "rm -rf /"})),
        MockResponse::Text("All done.".into()),
    ];
    let mut agent = Agent::from_provider(MockProvider::new(script), ModelConfig::mock())
        .with_tools(vec![Box::new(Bash)])
        .with_rutis(&bridge);

    let (tx, forwarder) = bridge.event_sender(None);
    agent
        .prompt_with_sender("Count some words, then clean up.", tx)
        .await;
    forwarder.await?;

    // Bus dispatch is asynchronous: give the listener a moment to finish.
    for _ in 0..100 {
        if log.lock().unwrap().len() == 4 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    for line in log.lock().unwrap().iter() {
        println!("{line}");
    }

    root.shutdown().await?;
    Ok(())
}
