//! Extension example: dollar budgets with `extension::Budget`.
//!
//! Demonstrates:
//!   - a per-run budget: every run starts from zero and stops (with an
//!     `[Agent stopped: budget …]` marker) once its spend reaches the limit
//!   - `.across_runs()`: one total for the whole session, read with
//!     `spent_usd()` through an `Arc<Budget>` kept after installing it
//!
//! The check runs before each model request, so the request that crosses the
//! limit still completes.
//!
//! Run (offline, scripted model; exits non-zero on a regression):
//!   cargo run --example extension_budget
//! Or against a real model (`DEEPSEEK_API_KEY` or `ANTHROPIC_API_KEY`):
//!   cargo run --example extension_budget -- --live

mod support;

use std::sync::Arc;
use support::*;
use yoagent::agent_loop::AGENT_STOPPED_PREFIX;
use yoagent::extension::Budget;
use yoagent::provider::mock::{MockResponse, MockToolCall};
use yoagent::provider::CostConfig;
use yoagent::*;

/// A budget of `max_usd`. Live, it charges the model's own prices, and there
/// is none for a model yoagent has no prices for (`Budget::for_model` returns
/// `None`): a made-up price would make the limit meaningless. Offline, the
/// scripted model costs $1 per million input tokens, so the usage below
/// reads in cents.
fn budget(max_usd: f64) -> Option<Budget> {
    match live_model() {
        Some(model) => Budget::for_model(max_usd, &model),
        None => Some(Budget::usd(max_usd, CostConfig::new(1.0, 0.0))),
    }
}

/// A scripted turn that calls `search` and costs `cents`.
fn search_costing(cents: u64) -> MockResponse {
    MockResponse::ToolCallsWithUsage(
        vec![MockToolCall {
            name: "search".into(),
            arguments: serde_json::json!({ "query": "flaky test" }),
            provider_metadata: None,
        }],
        Usage {
            input: cents * 10_000,
            ..Default::default()
        },
    )
}

/// A pretend search tool.
struct Search;

#[async_trait::async_trait]
impl AgentTool for Search {
    fn name(&self) -> &str {
        "search"
    }
    fn label(&self) -> &str {
        "Search"
    }
    fn description(&self) -> &str {
        "Search the issue tracker (simulated)"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "query": { "type": "string" } }
        })
    }
    async fn execute(&self, _: serde_json::Value, _: ToolContext) -> Result<ToolResult, ToolError> {
        Ok(ToolResult {
            content: vec![Content::Text {
                text: "3 issues mention it; none is resolved.".into(),
            }],
            details: serde_json::Value::Null,
        })
    }
}

/// A run's model requests, and whether the budget stopped it.
fn summarize(events: &[AgentEvent]) -> (usize, bool) {
    let requests = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                AgentEvent::MessageEnd {
                    message: AgentMessage::Llm(Message::Assistant { .. })
                }
            )
        })
        .count();
    // "[Agent stopped: <budget name> of $0.10 spent ($0.1200)]"
    let last = last_text(events);
    let stopped = last.starts_with(AGENT_STOPPED_PREFIX) && last.contains(" spent ($");
    (requests, stopped)
}

#[tokio::main]
async fn main() {
    const PROMPT: &str = "Keep searching the tracker for `flaky test` until you are sure.";
    let (Some(per_run), Some(session)) = (budget(0.10), budget(0.10)) else {
        println!("model unpriced, skipping the live budget");
        return;
    };

    // Per run: 10 cents each. A model that keeps searching at 4 cents a
    // turn is stopped before its fourth request (4 + 4 + 4 = 12 >= 10).
    let script = (0..8).map(|_| search_costing(4)).collect();
    let mut agent = new_agent(script)
        .with_tools(vec![Box::new(Search)])
        .with_extension(per_run);
    for n in 1..=2 {
        let events = run(&mut agent, PROMPT).await;
        let (requests, stopped) = summarize(&events);
        println!(
            "per-run budget, run {n}: {requests} requests, then {:?}",
            last_text(&events)
        );
        check(
            requests == 3 && stopped,
            "every run gets its own 10 cents (three 4-cent requests)",
        );
    }

    // Across runs: one 10-cent total for the session. Keep an `Arc` to read it.
    let session = Arc::new(session.across_runs().with_name("session"));
    let script = (0..8).map(|_| search_costing(4)).collect();
    let mut agent = new_agent(script)
        .with_tools(vec![Box::new(Search)])
        .with_extension(session.clone());
    let first = run(&mut agent, PROMPT).await;
    let second = run(&mut agent, PROMPT).await;
    let spent = session
        .spent_usd()
        .expect("an across-runs budget keeps a total");
    println!(
        "\nsession budget: run 1 made {} requests, run 2 made {}; ${spent:.2} spent",
        summarize(&first).0,
        summarize(&second).0,
    );
    println!("  run 2 ended with {:?}", last_text(&second));
    check(
        summarize(&first) == (3, true) && summarize(&second) == (0, true),
        "the session total carries over: the second run is stopped before its first request",
    );
    check((spent - 0.12).abs() < 1e-9, "the total reads $0.12");
}
