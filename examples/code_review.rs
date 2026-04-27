//! Code review agent: parallel sub-agents reviewing the same source file.
//!
//! Demonstrates:
//!   - Reading a real file and storing it in SharedState
//!   - 3 parallel sub-agents each analyzing a different aspect
//!   - Sub-agents writing structured findings back to shared state
//!   - Parent aggregating all reviews into a unified report
//!
//! Run on any source file:
//!   ANTHROPIC_API_KEY=sk-... cargo run --example code_review -- path/to/file.rs
//!
//! Try it on this repo:
//!   ANTHROPIC_API_KEY=sk-... cargo run --example code_review -- src/shared_state.rs

use std::sync::Arc;
use yoagent::provider::{AnthropicProvider, StreamProvider};
use yoagent::shared_state::SharedState;
use yoagent::sub_agent::SubAgentTool;
use yoagent::*;

#[tokio::main]
async fn main() {
    let api_key = std::env::var("ANTHROPIC_API_KEY").expect("Set ANTHROPIC_API_KEY");
    let model = "claude-sonnet-4-20250514";
    let provider: Arc<dyn StreamProvider> = Arc::new(AnthropicProvider);

    // --- Read the target file from CLI args ---
    let file_path = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("Usage: cargo run --example code_review -- <file_path>");
        std::process::exit(1);
    });

    let source_code = std::fs::read_to_string(&file_path).unwrap_or_else(|e| {
        eprintln!("Failed to read '{}': {}", file_path, e);
        std::process::exit(1);
    });

    println!(
        "Reviewing: {} ({} bytes)\n",
        file_path,
        source_code.len()
    );

    // --- Store the source code once in shared state ---
    let state = SharedState::new();
    state
        .set("source_code", source_code)
        .await
        .expect("store source code");
    state
        .set("file_path", file_path.clone())
        .await
        .expect("store file path");

    // --- Three reviewers, each focused on a different aspect ---

    let bug_reviewer = SubAgentTool::new("bug_reviewer", Arc::clone(&provider))
        .with_description("Reviews code for bugs and logic errors")
        .with_system_prompt(
            "You are a bug-finding specialist. Read the source code from shared state \
             (key: 'source_code'), look for bugs, logic errors, off-by-one errors, \
             race conditions, and potential panics. Write your findings to shared state \
             under key 'bugs_review'. Format: bullet points, each with line reference \
             and severity (critical/warning/info). If no bugs found, say so.",
        )
        .with_model(model)
        .with_api_key(&api_key)
        .with_shared_state(state.clone())
        .with_max_turns(5);

    let quality_reviewer = SubAgentTool::new("quality_reviewer", Arc::clone(&provider))
        .with_description("Reviews code quality and style")
        .with_system_prompt(
            "You are a code quality reviewer. Read the source code from shared state \
             (key: 'source_code'), evaluate naming, structure, idiomatic usage, \
             error handling patterns, and API design. Write your findings to shared state \
             under key 'quality_review'. Format: bullet points with specific suggestions. \
             Mention what's done well too.",
        )
        .with_model(model)
        .with_api_key(&api_key)
        .with_shared_state(state.clone())
        .with_max_turns(5);

    let docs_reviewer = SubAgentTool::new("docs_reviewer", Arc::clone(&provider))
        .with_description("Reviews documentation completeness")
        .with_system_prompt(
            "You are a documentation reviewer. Read the source code from shared state \
             (key: 'source_code') and the file path (key: 'file_path'). Evaluate: \
             are public items documented? Are doc comments accurate? Are edge cases \
             explained? Are examples provided where helpful? Write findings to shared \
             state under key 'docs_review'. Format: bullet points.",
        )
        .with_model(model)
        .with_api_key(&api_key)
        .with_shared_state(state.clone())
        .with_max_turns(5);

    // --- Run all three in parallel ---
    println!("Dispatching 3 reviewers in parallel...\n");

    let ctx = |name: &str| ToolContext {
        tool_call_id: format!("tc-{}", name),
        tool_name: name.to_string(),
        cancel: tokio_util::sync::CancellationToken::new(),
        on_update: None,
        on_progress: None,
    };

    let (r1, r2, r3) = tokio::join!(
        bug_reviewer.execute(
            serde_json::json!({"task": "Review the source code for bugs and logic errors."}),
            ctx("bug_reviewer"),
        ),
        quality_reviewer.execute(
            serde_json::json!({"task": "Review the source code for quality and style."}),
            ctx("quality_reviewer"),
        ),
        docs_reviewer.execute(
            serde_json::json!({"task": "Review the source code for documentation completeness."}),
            ctx("docs_reviewer"),
        ),
    );

    r1.expect("bug reviewer failed");
    r2.expect("quality reviewer failed");
    r3.expect("docs reviewer failed");

    // --- Print unified review ---
    println!("═══════════════════════════════════════════════════════════");
    println!("  Code Review: {}", file_path);
    println!("═══════════════════════════════════════════════════════════\n");

    let sections = [
        ("bugs_review", "Bug Analysis"),
        ("quality_review", "Code Quality"),
        ("docs_review", "Documentation"),
    ];

    for (key, title) in sections {
        println!("── {} ──\n", title);
        match state.get(key).await {
            Some(value) => println!("{}\n", value),
            None => println!("(reviewer did not produce findings)\n"),
        }
    }

    println!("═══════════════════════════════════════════════════════════");
    println!("  Review complete. Shared state keys: {}", state.summary().await);
    println!("═══════════════════════════════════════════════════════════");
}
