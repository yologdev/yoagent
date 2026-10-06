//! `extension::Budget`: a dollar limit per run, across runs, and across a
//! delegation tree.

use std::sync::Arc;
use tokio::sync::mpsc;
use yoagent::extension::Budget;
use yoagent::provider::mock::{MockResponse, MockToolCall};
use yoagent::provider::{CostConfig, MockProvider, ModelConfig};
use yoagent::*;

/// $1 per million input tokens, nothing for output.
fn dollar_per_million() -> CostConfig {
    CostConfig::new(1.0, 0.0)
}

/// A turn that costs `cents` (input tokens at $1 per million).
fn usage(cents: u64) -> Usage {
    Usage {
        input: cents * 10_000,
        ..Default::default()
    }
}

fn tool_call(cents: u64) -> MockResponse {
    MockResponse::ToolCallsWithUsage(
        vec![MockToolCall {
            name: "noop".into(),
            arguments: serde_json::json!({}),
            provider_metadata: None,
        }],
        usage(cents),
    )
}

fn answer(cents: u64) -> MockResponse {
    MockResponse::TextWithUsage("done".into(), usage(cents))
}

struct Noop;

#[async_trait::async_trait]
impl AgentTool for Noop {
    fn name(&self) -> &str {
        "noop"
    }
    fn label(&self) -> &str {
        "noop"
    }
    fn description(&self) -> &str {
        "does nothing"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    async fn execute(&self, _: serde_json::Value, _: ToolContext) -> Result<ToolResult, ToolError> {
        Ok(ToolResult {
            content: vec![Content::Text { text: "ok".into() }],
            details: serde_json::Value::Null,
        })
    }
}

async fn run(agent: &mut Agent, prompt: &str) -> Vec<AgentMessage> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    agent.prompt_with_sender(prompt, tx).await;
    let mut last = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let AgentEvent::AgentEnd { messages, .. } = event {
            last = messages;
        }
    }
    last
}

fn stopped_on_budget(messages: &[AgentMessage]) -> bool {
    matches!(messages.last(), Some(AgentMessage::Llm(Message::User { content, .. }))
        if matches!(content.first(), Some(Content::Text { text }) if text.starts_with("[Agent stopped: budget of $0.10 spent")))
}

#[tokio::test]
async fn a_run_stops_once_its_spend_reaches_the_limit() {
    // Three 4-cent turns with a 10-cent limit: after two (8 cents) the
    // third request is still sent (8 < 10), after it (12) the run stops.
    let provider = MockProvider::new(vec![tool_call(4), tool_call(4), tool_call(4), answer(4)]);
    let mut agent = Agent::from_provider(provider, ModelConfig::mock())
        .with_tools(vec![Box::new(Noop)])
        .with_extension(Budget::usd(0.10, dollar_per_million()));
    let messages = run(&mut agent, "go").await;
    let turns = messages
        .iter()
        .filter(|m| matches!(m, AgentMessage::Llm(Message::Assistant { .. })))
        .count();
    assert_eq!(turns, 3);
    assert!(stopped_on_budget(&messages), "{:?}", messages.last());
}

#[tokio::test]
async fn a_per_run_budget_starts_over_each_run_and_across_runs_does_not() {
    // Each run costs 8 cents; the limit is 10.
    let script = || vec![tool_call(8), answer(0), tool_call(8), answer(0)];

    let mut per_run = Agent::from_provider(MockProvider::new(script()), ModelConfig::mock())
        .with_tools(vec![Box::new(Noop)])
        .with_extension(Budget::usd(0.10, dollar_per_million()));
    assert!(!stopped_on_budget(&run(&mut per_run, "one").await));
    assert!(!stopped_on_budget(&run(&mut per_run, "two").await));

    let mut across = Agent::from_provider(MockProvider::new(script()), ModelConfig::mock())
        .with_tools(vec![Box::new(Noop)])
        .with_extension(Budget::usd(0.10, dollar_per_million()).across_runs());
    assert!(!stopped_on_budget(&run(&mut across, "one").await));
    // 8 cents spent before; after this run's first turn the total is 16.
    assert!(stopped_on_budget(&run(&mut across, "two").await));
}

#[tokio::test]
async fn a_tree_budget_counts_a_sub_agent_s_spend() {
    // Parent 6, then the child at 3 per turn, limit 10. Shared: 6 + 3 + 3 =
    // 12 after the child's second turn, so its third request is never sent.
    // Not shared, the child alone would run all four of its turns.
    let child_requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    struct Counted(MockProvider, Arc<std::sync::atomic::AtomicUsize>);
    #[async_trait::async_trait]
    impl yoagent::provider::StreamProvider for Counted {
        async fn stream(
            &self,
            config: yoagent::provider::StreamConfig,
            tx: mpsc::UnboundedSender<yoagent::provider::StreamEvent>,
            cancel: tokio_util::sync::CancellationToken,
        ) -> Result<Message, yoagent::provider::ProviderError> {
            self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.0.stream(config, tx, cancel).await
        }
    }
    let child = SubAgentTool::from_provider(
        "child",
        Arc::new(Counted(
            MockProvider::new(vec![tool_call(3), tool_call(3), tool_call(3), answer(3)]),
            child_requests.clone(),
        )),
        ModelConfig::mock(),
    )
    .with_tools(vec![Arc::new(Noop)]);
    let parent = MockProvider::new(vec![
        MockResponse::ToolCallsWithUsage(
            vec![MockToolCall {
                name: "child".into(),
                arguments: serde_json::json!({"task": "work"}),
                provider_metadata: None,
            }],
            usage(6),
        ),
        answer(1),
    ]);
    let budget = Budget::usd(0.10, dollar_per_million()).across_runs();
    let mut agent = Agent::from_provider(parent, ModelConfig::mock())
        .with_tools(vec![Box::new(child)])
        .with_tree_extension(budget);
    let messages = run(&mut agent, "go").await;
    assert_eq!(
        child_requests.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "the child stopped on the shared total"
    );
    assert!(stopped_on_budget(&messages), "{:?}", messages.last());
}

#[test]
fn spent_usd_is_the_across_runs_total() {
    assert_eq!(Budget::usd(1.0, dollar_per_million()).spent_usd(), None);
    assert_eq!(
        Budget::usd(1.0, dollar_per_million())
            .across_runs()
            .spent_usd(),
        Some(0.0)
    );
}

#[test]
fn an_unpriced_model_gets_no_budget() {
    assert!(Budget::for_model(1.0, &ModelConfig::mock()).is_none());
    let priced = ModelConfig::claude_sonnet_5();
    assert!(Budget::for_model(1.0, &priced).is_some());
}
