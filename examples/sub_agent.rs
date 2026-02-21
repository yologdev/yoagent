//! Sub-agent example: a coordinator with two specialized sub-agents.
//!
//! Demonstrates:
//!   - Creating SubAgentTools with their own system prompts and tools
//!   - Registering sub-agents on a parent Agent via `with_sub_agent()`
//!   - The parent LLM decides when to delegate to sub-agents
//!
//! Run:
//!   ANTHROPIC_API_KEY=sk-... cargo run --example sub_agent

use std::sync::Arc;
use yoagent::agent::Agent;
use yoagent::provider::{AnthropicProvider, StreamProvider};
use yoagent::sub_agent::SubAgentTool;
use yoagent::tools;
use yoagent::*;

#[tokio::main]
async fn main() {
    let api_key = std::env::var("ANTHROPIC_API_KEY").expect("Set ANTHROPIC_API_KEY");
    let model = "claude-sonnet-4-20250514";
    let provider: Arc<dyn StreamProvider> = Arc::new(AnthropicProvider);

    // Sub-agent 1: a researcher with file reading tools
    let researcher = SubAgentTool::new("researcher", Arc::clone(&provider))
        .with_description(
            "Searches and reads files to gather information. Delegate research tasks here.",
        )
        .with_system_prompt(
            "You are a research assistant. Read files and summarize findings concisely.",
        )
        .with_model(model)
        .with_api_key(&api_key)
        .with_tools(vec![
            Arc::new(tools::ReadFileTool::new()),
            Arc::new(tools::SearchTool::new()),
            Arc::new(tools::ListFilesTool::new()),
        ])
        .with_max_turns(10);

    // Sub-agent 2: a coder with file editing tools
    let coder = SubAgentTool::new("coder", Arc::clone(&provider))
        .with_description("Writes and edits code files. Delegate coding tasks here.")
        .with_system_prompt("You are a coding assistant. Write clean, correct code. Be concise.")
        .with_model(model)
        .with_api_key(&api_key)
        .with_tools(vec![
            Arc::new(tools::ReadFileTool::new()),
            Arc::new(tools::WriteFileTool::new()),
            Arc::new(tools::EditFileTool::new()),
        ])
        .with_max_turns(15);

    // Parent agent: coordinates between sub-agents
    let mut agent = Agent::new(AnthropicProvider)
        .with_system_prompt(
            "You are a coordinator agent. You have two sub-agents:\n\
             - 'researcher': for reading files and gathering information\n\
             - 'coder': for writing and editing code\n\n\
             Delegate tasks to the appropriate sub-agent. You can run both in parallel \
             when the tasks are independent.",
        )
        .with_model(model)
        .with_api_key(api_key)
        .with_sub_agent(researcher)
        .with_sub_agent(coder);

    println!("Coordinator agent with 2 sub-agents ready.");
    println!("Try: 'Read the README and then create a hello.py file'\n");

    let mut rx = agent
        .prompt("List the files in the current directory, then create a file called hello.txt with 'Hello from sub-agents!'")
        .await;

    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::MessageUpdate {
                delta: StreamDelta::Text { delta },
                ..
            } => {
                print!("{}", delta);
            }
            AgentEvent::ToolExecutionStart { tool_name, .. } => {
                eprintln!("\n  [calling: {}]", tool_name);
            }
            AgentEvent::ToolExecutionEnd { tool_name, .. } => {
                eprintln!("  [done: {}]", tool_name);
            }
            AgentEvent::AgentEnd { .. } => {
                println!("\n\n--- Done ---");
            }
            _ => {}
        }
    }
}
