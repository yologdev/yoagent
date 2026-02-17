//! Mini coding agent CLI — a baby Claude Code in ~150 lines.
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
//! Commands:
//!   /quit, /exit    Exit the agent
//!   /clear          Clear conversation history
//!   /model <name>   Switch model mid-session

use std::io::{self, BufRead, Write};
use yoagent::agent::Agent;
use yoagent::provider::AnthropicProvider;
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
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .or_else(|_| std::env::var("API_KEY"))
        .expect("Set ANTHROPIC_API_KEY or API_KEY");

    let args: Vec<String> = std::env::args().collect();

    let model = args
        .iter()
        .position(|a| a == "--model")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| "claude-sonnet-4-20250514".into());

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

    let mut agent = Agent::new(AnthropicProvider)
        .with_system_prompt(SYSTEM_PROMPT)
        .with_model(&model)
        .with_api_key(&api_key)
        .with_skills(skills.clone())
        .with_tools(default_tools());

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
                agent = Agent::new(AnthropicProvider)
                    .with_system_prompt(SYSTEM_PROMPT)
                    .with_model(&model)
                    .with_api_key(&api_key)
                    .with_skills(skills.clone())
                    .with_tools(default_tools());
                println!("{DIM}  (conversation cleared){RESET}\n");
                continue;
            }
            s if s.starts_with("/model ") => {
                let new_model = s.trim_start_matches("/model ").trim();
                agent = Agent::new(AnthropicProvider)
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

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        &s[..max]
    }
}
