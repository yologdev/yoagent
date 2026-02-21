//! Lifecycle callbacks example.
//!
//! Demonstrates:
//! - `on_before_turn` to limit turns
//! - `on_after_turn` to track token usage
//! - `on_error` to log errors
//!
//! Uses MockProvider so no API key is needed.
//!   cargo run --example callbacks

use std::sync::{Arc, Mutex};
use yoagent::agent::Agent;
use yoagent::provider::mock::*;
use yoagent::provider::MockProvider;
use yoagent::types::*;

#[tokio::main]
async fn main() {
    // Provider: tool call → text response (2-turn conversation)
    let provider = MockProvider::new(vec![
        MockResponse::ToolCalls(vec![MockToolCall {
            name: "greet".into(),
            arguments: serde_json::json!({"name": "World"}),
        }]),
        MockResponse::Text("I greeted the world for you!".into()),
    ]);

    // Track usage across turns
    let token_log: Arc<Mutex<Vec<(usize, u64, u64)>>> = Arc::new(Mutex::new(Vec::new()));
    let log_clone = token_log.clone();

    // Track errors
    let errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let errors_clone = errors.clone();

    struct GreetTool;

    #[async_trait::async_trait]
    impl AgentTool for GreetTool {
        fn name(&self) -> &str {
            "greet"
        }
        fn label(&self) -> &str {
            "Greet"
        }
        fn description(&self) -> &str {
            "Greets someone"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"}
                }
            })
        }
        async fn execute(
            &self,
            _id: &str,
            params: serde_json::Value,
            _cancel: tokio_util::sync::CancellationToken,
            _on_update: Option<ToolUpdateFn>,
        ) -> Result<ToolResult, ToolError> {
            let name = params["name"].as_str().unwrap_or("stranger");
            Ok(ToolResult {
                content: vec![Content::Text {
                    text: format!("Hello, {}!", name),
                }],
                details: serde_json::Value::Null,
            })
        }
    }

    let mut agent = Agent::new(provider)
        .with_system_prompt("You are helpful.")
        .with_model("mock")
        .with_api_key("test")
        .with_tools(vec![Box::new(GreetTool)])
        // Limit to 5 turns (plenty for this example)
        .on_before_turn(|messages, turn| {
            println!("[before_turn] turn={}, messages={}", turn, messages.len());
            turn < 5
        })
        // Log token usage after each turn
        .on_after_turn(move |messages, usage| {
            let entry = (messages.len(), usage.input, usage.output);
            println!(
                "[after_turn]  messages={}, tokens: {} in / {} out",
                entry.0, entry.1, entry.2
            );
            log_clone.lock().unwrap().push(entry);
        })
        // Log any errors
        .on_error(move |err| {
            println!("[on_error]    {}", err);
            errors_clone.lock().unwrap().push(err.to_string());
        });

    println!("=== Running agent with callbacks ===\n");

    let mut rx = agent.prompt("Greet the world").await;
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::MessageUpdate {
                delta: StreamDelta::Text { delta },
                ..
            } => print!("{}", delta),
            AgentEvent::ToolExecutionStart { tool_name, .. } => {
                println!("\n  [tool: {}]", tool_name);
            }
            AgentEvent::AgentEnd { .. } => println!("\n"),
            _ => {}
        }
    }

    // Print summary
    let log = token_log.lock().unwrap();
    println!("=== Callback Summary ===");
    println!("after_turn called {} time(s)", log.len());
    for (i, (msgs, input, output)) in log.iter().enumerate() {
        println!(
            "  Turn {}: {} messages, {} input / {} output tokens",
            i, msgs, input, output
        );
    }

    let errs = errors.lock().unwrap();
    if errs.is_empty() {
        println!("No errors recorded.");
    } else {
        println!("{} error(s): {:?}", errs.len(), *errs);
    }
}
