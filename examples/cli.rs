//! Mini coding agent CLI — a baby Claude Code in ~250 lines.
//!
//! Features:
//!   - Interactive REPL with multi-turn conversation
//!   - All built-in tools (bash, read/write/edit files, search, list)
//!   - Streaming text output with colored tool feedback
//!   - Token usage after each turn
//!
//! Run:
//!   ANTHROPIC_API_KEY=sk-... cargo run --example cli
//!   ANTHROPIC_API_KEY=sk-... cargo run --example cli -- --model claude-sonnet-4-20250514
//!   ANTHROPIC_API_KEY=sk-... cargo run --example cli -- --skills ./skills
//!
//! Run with a named provider (zai, openai, xai, groq, deepseek, mistral, minimax, google):
//!   API_KEY=... cargo run --example cli -- --provider zai --model glm-4.7
//!
//! Run with LM Studio / Ollama / local OpenAI-compatible server:
//!   cargo run --example cli -- --api-url http://localhost:1234/v1 --model local-model
//!
//! Commands:
//!   /quit, /exit    Exit the agent
//!   /clear          Clear conversation history
//!   /model <name>   Switch model mid-session

use std::io::{self, BufRead, Write};
use yoagent::agent::Agent;
use yoagent::provider::{AnthropicProvider, GoogleProvider, ModelConfig, OpenAiCompatProvider};
use yoagent::skills::SkillSet;
use yoagent::tools::default_tools;
use yoagent::*;

// ANSI color helpers
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const CYAN: &str = "\x1b[36m";
const RED: &str = "\x1b[31m";

const SYSTEM_PROMPT: &str = r#"You are a coding assistant working in the user's terminal.
You have access to the filesystem and shell. Be direct and concise.
When the user asks you to do something, do it — don't just explain how.
Use tools proactively: read files to understand context, run commands to verify your work.
After making changes, run tests or verify the result when appropriate."#;

fn print_banner() {
    println!("\n{BOLD}{CYAN}  yoagent cli{RESET} {DIM}— mini coding agent{RESET}");
    println!("{DIM}  Type /quit to exit, /clear to reset{RESET}\n");
}

fn print_usage(usage: &Usage) {
    if usage.input > 0 || usage.output > 0 {
        println!(
            "\n{DIM}  tokens: {} in / {} out{RESET}",
            usage.input, usage.output
        );
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();

    let api_url = args
        .iter()
        .position(|a| a == "--api-url")
        .and_then(|i| args.get(i + 1))
        .cloned();

    let provider_name = args
        .iter()
        .position(|a| a == "--provider")
        .and_then(|i| args.get(i + 1))
        .cloned();

    let api_key = if api_url.is_some() {
        std::env::var("ANTHROPIC_API_KEY")
            .or_else(|_| std::env::var("API_KEY"))
            .unwrap_or_default() // empty string OK for local
    } else {
        std::env::var("ANTHROPIC_API_KEY")
            .or_else(|_| std::env::var("API_KEY"))
            .expect("Set ANTHROPIC_API_KEY or API_KEY")
    };

    let default_model = match provider_name.as_deref() {
        Some("zai") => "glm-4.7",
        Some("openai") => "gpt-4o",
        Some("xai") => "grok-3-mini",
        Some("groq") => "llama-3.3-70b-versatile",
        Some("deepseek") => "deepseek-chat",
        Some("mistral") => "mistral-large-latest",
        Some("minimax") => "MiniMax-Text-01",
        Some("google") => "gemini-2.5-pro",
        _ => "claude-sonnet-4-20250514",
    };

    let model = args
        .iter()
        .position(|a| a == "--model")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| default_model.into());

    // Collect --skills directories (can be specified multiple times)
    let skill_dirs: Vec<String> = args
        .iter()
        .enumerate()
        .filter(|(_, a)| a.as_str() == "--skills")
        .filter_map(|(i, _)| args.get(i + 1).cloned())
        .collect();

    let skills = if skill_dirs.is_empty() {
        SkillSet::empty()
    } else {
        SkillSet::load(&skill_dirs).expect("Failed to load skills")
    };

    let mut agent = if let Some(ref url) = api_url {
        Agent::new(OpenAiCompatProvider).with_model_config(ModelConfig::local(url, &model))
    } else if let Some(ref prov) = provider_name {
        make_provider_agent(prov, &model)
    } else {
        Agent::new(AnthropicProvider)
    };
    agent = agent
        .with_system_prompt(SYSTEM_PROMPT)
        .with_model(&model)
        .with_api_key(&api_key)
        .with_skills(skills.clone())
        .with_tools(default_tools());

    // Graceful Ctrl+C exit
    tokio::spawn(async {
        tokio::signal::ctrl_c().await.ok();
        println!("\n{DIM}  bye 👋{RESET}\n");
        std::process::exit(0);
    });

    print_banner();
    println!("{DIM}  model: {model}{RESET}");
    if !skills.is_empty() {
        println!("{DIM}  skills: {} loaded{RESET}", skills.len());
    }
    println!(
        "{DIM}  cwd:   {}{RESET}\n",
        std::env::current_dir().unwrap().display()
    );

    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();

    loop {
        // Prompt
        print!("{BOLD}{GREEN}> {RESET}");
        io::stdout().flush().ok();

        let line = match lines.next() {
            Some(Ok(l)) => l,
            _ => break,
        };

        let input = line.trim();
        if input.is_empty() {
            continue;
        }

        // Commands
        match input {
            "/quit" | "/exit" => break,
            "/clear" => {
                agent.clear_messages();
                println!("{DIM}  (conversation cleared){RESET}\n");
                continue;
            }
            s if s.starts_with("/model ") => {
                let new_model = s.trim_start_matches("/model ").trim();
                agent = if let Some(ref url) = api_url {
                    Agent::new(OpenAiCompatProvider)
                        .with_model_config(ModelConfig::local(url, new_model))
                } else if let Some(ref prov) = provider_name {
                    make_provider_agent(prov, new_model)
                } else {
                    Agent::new(AnthropicProvider)
                };
                agent = agent
                    .with_system_prompt(SYSTEM_PROMPT)
                    .with_model(new_model)
                    .with_api_key(&api_key)
                    .with_skills(skills.clone())
                    .with_tools(default_tools());
                println!("{DIM}  (switched to {new_model}, conversation cleared){RESET}\n");
                continue;
            }
            _ => {}
        }

        // Send to agent
        let mut rx = agent.prompt(input).await;
        let mut last_usage = Usage::default();
        let mut in_text = false;

        while let Some(event) = rx.recv().await {
            match event {
                AgentEvent::ToolExecutionStart {
                    tool_name, args, ..
                } => {
                    if in_text {
                        println!();
                        in_text = false;
                    }
                    let summary = match tool_name.as_str() {
                        "bash" => {
                            let cmd = args
                                .get("command")
                                .and_then(|v| v.as_str())
                                .unwrap_or("...");
                            format!("$ {}", truncate(cmd, 80))
                        }
                        "read_file" => {
                            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("?");
                            format!("read {}", path)
                        }
                        "write_file" => {
                            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("?");
                            format!("write {}", path)
                        }
                        "edit_file" => {
                            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("?");
                            format!("edit {}", path)
                        }
                        "list_files" => {
                            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
                            format!("ls {}", path)
                        }
                        "search" => {
                            let pat = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("?");
                            format!("search '{}'", truncate(pat, 60))
                        }
                        _ => tool_name.clone(),
                    };
                    print!("{YELLOW}  ▶ {summary}{RESET}");
                    io::stdout().flush().ok();
                }
                AgentEvent::ToolExecutionEnd { is_error, .. } => {
                    if is_error {
                        println!(" {RED}✗{RESET}");
                    } else {
                        println!(" {GREEN}✓{RESET}");
                    }
                }
                AgentEvent::MessageUpdate {
                    delta: StreamDelta::Text { delta },
                    ..
                } => {
                    if !in_text {
                        println!();
                        in_text = true;
                    }
                    print!("{}", delta);
                    io::stdout().flush().ok();
                }
                AgentEvent::MessageEnd {
                    message:
                        AgentMessage::Llm(Message::Assistant {
                            stop_reason: StopReason::Error,
                            error_message,
                            ..
                        }),
                } => {
                    if in_text {
                        println!();
                        in_text = false;
                    }
                    let msg = error_message.as_deref().unwrap_or("unknown error");
                    println!("{RED}  error: {msg}{RESET}");
                }
                AgentEvent::AgentEnd { messages } => {
                    // Extract usage from the last assistant message
                    for msg in messages.iter().rev() {
                        if let AgentMessage::Llm(Message::Assistant { usage, .. }) = msg {
                            last_usage = usage.clone();
                            break;
                        }
                    }
                }
                _ => {}
            }
        }

        if in_text {
            println!();
        }
        print_usage(&last_usage);
        println!();
    }

    println!("\n{DIM}  bye 👋{RESET}\n");
}

fn make_provider_agent(provider: &str, model: &str) -> Agent {
    match provider {
        "zai" => Agent::new(OpenAiCompatProvider).with_model_config(ModelConfig::zai(model, model)),
        "openai" => {
            Agent::new(OpenAiCompatProvider).with_model_config(ModelConfig::openai(model, model))
        }
        "xai" => Agent::new(OpenAiCompatProvider).with_model_config(ModelConfig::xai(model, model)),
        "groq" => {
            Agent::new(OpenAiCompatProvider).with_model_config(ModelConfig::groq(model, model))
        }
        "deepseek" => {
            Agent::new(OpenAiCompatProvider).with_model_config(ModelConfig::deepseek(model, model))
        }
        "mistral" => {
            Agent::new(OpenAiCompatProvider).with_model_config(ModelConfig::mistral(model, model))
        }
        "minimax" => {
            Agent::new(OpenAiCompatProvider).with_model_config(ModelConfig::minimax(model, model))
        }
        "google" => Agent::new(GoogleProvider).with_model_config(ModelConfig::google(model, model)),
        other => {
            eprintln!("Unknown provider: {other}. Supported: zai, openai, xai, groq, deepseek, mistral, minimax, google.");
            std::process::exit(1);
        }
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        match s.char_indices().nth(max) {
            Some((idx, _)) => &s[..idx],
            None => s,
        }
    }
}
