//! Basic example: simple text prompt with Anthropic.
//!
//! Run with: ANTHROPIC_API_KEY=sk-... cargo run --example basic

use yoagent::agent::Agent;
use yoagent::provider::ModelConfig;
use yoagent::*;

#[tokio::main]
async fn main() {
    // The Anthropic provider is selected from the config's protocol, and the
    // API key is read from ANTHROPIC_API_KEY. Add `.with_api_key(key)` to
    // pass one explicitly.
    let mut agent = Agent::from_config(ModelConfig::anthropic("claude-sonnet-5", "Sonnet 5"))
        .with_system_prompt("You are a helpful assistant. Be concise.");

    println!("Sending prompt...");

    let mut rx = agent
        .prompt("What is Rust's ownership model in 2 sentences?")
        .await;

    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::MessageUpdate {
                delta: StreamDelta::Text { delta },
                ..
            } => {
                print!("{}", delta);
            }
            AgentEvent::AgentEnd { .. } => {
                println!("\n\n--- Done ---");
            }
            _ => {}
        }
    }
}
