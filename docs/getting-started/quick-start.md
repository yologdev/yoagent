# Quick Start

## Basic Example with Anthropic

```rust
use yoagent::{Agent, AgentEvent, StreamDelta};
use yoagent::provider::AnthropicProvider;
use yoagent::tools::default_tools;

#[tokio::main]
async fn main() {
    let mut agent = Agent::new(AnthropicProvider)
        .with_system_prompt("You are a helpful coding assistant.")
        .with_model("claude-sonnet-4-20250514")
        .with_api_key(std::env::var("ANTHROPIC_API_KEY").unwrap())
        .with_tools(default_tools());

    let mut rx = agent.prompt("List the files in the current directory").await;

    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::MessageUpdate { delta, .. } => match delta {
                StreamDelta::Text { delta } => print!("{}", delta),
                StreamDelta::Thinking { delta } => print!("[thinking] {}", delta),
                _ => {}
            },
            AgentEvent::ToolExecutionStart { tool_name, .. } => {
                println!("\n→ Running tool: {}", tool_name);
            }
            AgentEvent::ToolExecutionEnd { tool_name, result, is_error, .. } => {
                if is_error {
                    println!("  ✗ {} failed", tool_name);
                } else {
                    println!("  ✓ {} done", tool_name);
                }
            }
            AgentEvent::AgentEnd { .. } => {
                println!("\n\nDone.");
            }
            _ => {}
        }
    }
}
```

## Example with OpenAI-Compatible Provider

For OpenAI, xAI, Groq, or any compatible API, use `OpenAiCompatProvider` with a `ModelConfig`:

```rust
use yoagent::{Agent, AgentEvent};
use yoagent::provider::OpenAiCompatProvider;
use yoagent::tools::default_tools;

#[tokio::main]
async fn main() {
    let mut agent = Agent::new(OpenAiCompatProvider)
        .with_system_prompt("You are a helpful assistant.")
        .with_model("gpt-4o")
        .with_api_key(std::env::var("OPENAI_API_KEY").unwrap())
        .with_tools(default_tools());

    let mut rx = agent.prompt("What is 2 + 2?").await;

    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::MessageUpdate { delta, .. } => {
                if let yoagent::StreamDelta::Text { delta } = delta {
                    print!("{}", delta);
                }
            }
            AgentEvent::AgentEnd { .. } => println!(),
            _ => {}
        }
    }
}
```

## Using the Low-Level API

For more control, use `agent_loop()` directly:

```rust
use yoagent::agent_loop::{agent_loop, AgentLoopConfig};
use yoagent::provider::AnthropicProvider;
use yoagent::types::*;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();

    let mut context = AgentContext {
        system_prompt: "You are helpful.".into(),
        messages: Vec::new(),
        tools: yoagent::tools::default_tools(),
    };

    let config = AgentLoopConfig {
        provider: &AnthropicProvider,
        model: "claude-sonnet-4-20250514".into(),
        api_key: std::env::var("ANTHROPIC_API_KEY").unwrap(),
        thinking_level: ThinkingLevel::Off,
        max_tokens: None,
        temperature: None,
        convert_to_llm: None,
        transform_context: None,
        get_steering_messages: None,
        get_follow_up_messages: None,
        context_config: None,
        execution_limits: None,
    };

    let prompts = vec![AgentMessage::Llm(Message::user("Hello!"))];
    let new_messages = agent_loop(prompts, &mut context, &config, tx, cancel).await;

    // Drain events
    while let Ok(event) = rx.try_recv() {
        // handle events...
    }

    println!("Got {} new messages", new_messages.len());
}
```
