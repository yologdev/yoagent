//! Basic example: simple text prompt with Anthropic.
//!
//! Run with: ANTHROPIC_API_KEY=sk-... cargo run --example basic

use yo_agent::agent::Agent;
use yo_agent::provider::AnthropicProvider;
use yo_agent::*;

#[tokio::main]
async fn main() {
    let api_key = std::env::var("ANTHROPIC_API_KEY").expect("Set ANTHROPIC_API_KEY");

    let mut agent = Agent::new(AnthropicProvider)
        .with_system_prompt("You are a helpful assistant. Be concise.")
        .with_model("claude-sonnet-4-20250514")
        .with_api_key(api_key);

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
