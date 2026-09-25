//! A `cost` set on a config survives every path into the agent and is what
//! gets billed — even with process-wide price layers installed — while
//! `reprice()` replaces it, as documented, on constructor-priced configs.
//!
//! Tests install process-wide layers, so they serialize on `LOCK`.

use std::sync::Arc;
use yoagent::provider::mock::{MockResponse, MockToolCall};
use yoagent::provider::prices::global;
use yoagent::provider::{CostConfig, MockProvider, ModelConfig, PriceTable};
use yoagent::sub_agent::SubAgentTool;
use yoagent::types::Usage;
use yoagent::Agent;

static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Serialize, and install a user layer that reprices Sonnet 5 at 1.8 / 9.
async fn with_override() -> tokio::sync::MutexGuard<'static, ()> {
    let guard = LOCK.lock().await;
    global::clear_fetched();
    let _ = global::install_override(
        PriceTable::from_json_str(
            r#"{"schema": 1, "providers": {"anthropic": {"claude-sonnet-5":
                {"input": 1.8, "output": 9.0, "cache_read": 0.18, "cache_write": 2.25}}}}"#,
        )
        .unwrap(),
    );
    guard
}

/// One million input and half a million output tokens.
fn usage() -> Usage {
    Usage {
        input: 1_000_000,
        output: 500_000,
        cache_read: 0,
        cache_write: 0,
        total_tokens: 1_500_000,
    }
}

/// $3 / $15 per million: $3 + $7.50 for `usage()`.
const EXPLICIT_BILL: f64 = 10.5;
/// The override's $1.8 / $9: $1.8 + $4.50.
const OVERRIDE_BILL: f64 = 6.3;

fn explicit() -> ModelConfig {
    let mut config = ModelConfig::claude_sonnet_5();
    config.cost = Some(CostConfig::new(3.0, 15.0));
    config
}

fn text_turn() -> MockProvider {
    MockProvider::new(vec![MockResponse::TextWithUsage("ok".into(), usage())])
}

async fn run(agent: &mut Agent) {
    let mut rx = agent.prompt("hi").await;
    while rx.recv().await.is_some() {}
    agent.finish().await;
}

fn close(a: Option<f64>, b: f64) -> bool {
    a.is_some_and(|a| (a - b).abs() < 1e-9)
}

#[tokio::test]
async fn an_explicit_cost_reaches_session_cost_via_from_provider() {
    let _g = with_override().await;
    let mut agent = Agent::from_provider(text_turn(), explicit());
    run(&mut agent).await;
    assert!(
        close(agent.session_cost_usd(), EXPLICIT_BILL),
        "{:?}",
        agent.session_cost_usd()
    );
    // Positive control: the same preset without an explicit cost bills at
    // the override, so the layer really was in effect.
    let mut agent = Agent::from_provider(text_turn(), ModelConfig::claude_sonnet_5());
    run(&mut agent).await;
    assert!(
        close(agent.session_cost_usd(), OVERRIDE_BILL),
        "{:?}",
        agent.session_cost_usd()
    );
}

#[tokio::test]
async fn an_explicit_cost_survives_set_model() {
    let _g = with_override().await;
    let mut agent = Agent::from_provider(text_turn(), ModelConfig::mock());
    agent.set_model(explicit());
    run(&mut agent).await;
    assert!(
        close(agent.session_cost_usd(), EXPLICIT_BILL),
        "{:?}",
        agent.session_cost_usd()
    );
}

#[tokio::test]
async fn an_explicit_cost_prices_a_sub_agent_run() {
    let _g = with_override().await;
    let child = SubAgentTool::from_provider("researcher", Arc::new(text_turn()), explicit());
    let parent = MockProvider::new(vec![
        MockResponse::ToolCalls(vec![MockToolCall {
            name: "researcher".into(),
            arguments: serde_json::json!({"task": "look"}),
            provider_metadata: None,
        }]),
        MockResponse::Text("done".into()),
    ]);
    let mut agent =
        Agent::from_provider(parent, ModelConfig::mock()).with_tools(vec![Box::new(child)]);
    run(&mut agent).await;
    let spend = agent.sub_agent_spend();
    assert_eq!(spend.runs, 1);
    assert!(close(spend.cost_usd, EXPLICIT_BILL), "{:?}", spend.cost_usd);
}

/// `reprice` replaces an explicit cost on a constructor-priced config — the
/// documented exception to "an explicit cost wins" — and leaves other
/// configs alone.
#[tokio::test]
async fn agent_reprice_replaces_a_constructor_priced_cost_only() {
    let _g = with_override().await;
    let mut agent = Agent::from_provider(text_turn(), explicit());
    agent.reprice();
    run(&mut agent).await;
    assert!(
        close(agent.session_cost_usd(), OVERRIDE_BILL),
        "{:?}",
        agent.session_cost_usd()
    );

    // A custom config was not priced by a constructor: reprice keeps its cost.
    let mut custom = ModelConfig::mock();
    custom.cost = Some(CostConfig::new(3.0, 15.0));
    let mut agent = Agent::from_provider(text_turn(), custom);
    agent.reprice();
    run(&mut agent).await;
    assert!(
        close(agent.session_cost_usd(), EXPLICIT_BILL),
        "{:?}",
        agent.session_cost_usd()
    );
}

#[tokio::test]
async fn sub_agent_reprice_replaces_a_constructor_priced_cost() {
    let _g = with_override().await;
    let child =
        SubAgentTool::from_provider("researcher", Arc::new(text_turn()), explicit()).reprice();
    let parent = MockProvider::new(vec![
        MockResponse::ToolCalls(vec![MockToolCall {
            name: "researcher".into(),
            arguments: serde_json::json!({"task": "look"}),
            provider_metadata: None,
        }]),
        MockResponse::Text("done".into()),
    ]);
    let mut agent =
        Agent::from_provider(parent, ModelConfig::mock()).with_tools(vec![Box::new(child)]);
    run(&mut agent).await;
    let spend = agent.sub_agent_spend();
    assert!(close(spend.cost_usd, OVERRIDE_BILL), "{:?}", spend.cost_usd);
}
