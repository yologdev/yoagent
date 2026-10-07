//! An `LlmCompaction` summarizer built before the pricing opt-in is priced
//! once `Agent::reprice` runs, like the agent's own model.
//!
//! Its own binary with a single test: it changes the process-wide price
//! layers.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use yoagent::context::ContextConfig;
use yoagent::provider::mock::*;
use yoagent::provider::prices::{self, global};
use yoagent::provider::{
    CostConfig, MockProvider, ModelConfig, PriceTable, ProviderError, StreamConfig, StreamEvent,
    StreamProvider,
};
use yoagent::*;

const SUMMARY_USAGE: Usage = Usage {
    input: 1_000,
    output: 100,
    cache_read: 0,
    cache_write: 0,
    total_tokens: 1_100,
};

struct Summarizer(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl StreamProvider for Summarizer {
    async fn stream(
        &self,
        _config: StreamConfig,
        _tx: mpsc::UnboundedSender<StreamEvent>,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Message, ProviderError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(Message::assistant(
            vec![Content::Text {
                text: "Briefing: the user is filling the context with bulk output.".into(),
            }],
            StopReason::Stop,
            "summarizer",
            "mock",
            SUMMARY_USAGE,
        ))
    }
}

struct BulkTool;

#[async_trait::async_trait]
impl AgentTool for BulkTool {
    fn name(&self) -> &str {
        "bulk"
    }
    fn label(&self) -> &str {
        "Bulk"
    }
    fn description(&self) -> &str {
        "Returns a lot of text"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {"i": {"type": "integer"}}})
    }
    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        tokio::time::sleep(Duration::from_millis(40)).await;
        let i = params["i"].as_u64().unwrap_or(0);
        let text = (0..40)
            .map(|line| format!("call {i} line {line}: {}", "z".repeat(80)))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolResult {
            content: vec![Content::Text { text }],
            details: serde_json::Value::Null,
        })
    }
}

fn script() -> Vec<MockResponse> {
    let mut script: Vec<MockResponse> = (0..8)
        .map(|i| {
            MockResponse::ToolCalls(vec![MockToolCall {
                provider_metadata: None,
                name: "bulk".into(),
                arguments: serde_json::json!({ "i": i }),
            }])
        })
        .collect();
    script.push(MockResponse::Text("done".into()));
    script
}

/// An agent whose summarizer is `claude-sonnet-5`, built now.
fn compacting_agent() -> Agent {
    let compaction = LlmCompaction::from_provider(
        Arc::new(Summarizer(Arc::default())),
        ModelConfig::anthropic("claude-sonnet-5", "Sonnet"),
    )
    .with_api_key("test")
    .with_trigger_ratio(0.3)
    .with_retain_tail_tokens(400);
    let mut main = ModelConfig::mock();
    main.cost = Some(CostConfig::new(1.0, 2.0));
    Agent::from_provider(MockProvider::new(script()), main)
        .with_api_key("test")
        .with_tools(vec![Box::new(BulkTool)])
        .with_context_config(ContextConfig {
            max_context_tokens: 4_000,
            system_prompt_tokens: 0,
            keep_first: 1,
            keep_recent: 2,
            ..Default::default()
        })
        .with_compaction_strategy(compaction)
}

async fn run(agent: &mut Agent) -> SessionStats {
    let (tx, mut rx) = mpsc::unbounded_channel();
    agent.prompt_with_sender("fill the context", tx).await;
    let mut stats = None;
    while let Ok(event) = rx.try_recv() {
        if let AgentEvent::AgentEnd { stats: s, .. } = event {
            stats = Some(s);
        }
    }
    stats.expect("AgentEnd")
}

#[tokio::test]
async fn agent_reprice_reaches_the_summarizer() {
    global::clear_override();
    global::clear_fetched();
    global::clear_bundled();

    // Both built before the opt-in: the summarizer is unpriced.
    let mut repriced = compacting_agent();
    let mut left = compacting_agent();
    let _ = prices::enable_bundled();
    repriced.reprice();

    let stats = run(&mut repriced).await;
    assert!(stats.compaction.requests >= 1, "the test must summarize");
    let rate = PriceTable::builtin()
        .cost("anthropic", "claude-sonnet-5")
        .expect("bundled");
    let expected = f64::from(stats.compaction.requests) * rate.cost_usd(&SUMMARY_USAGE);
    let got = stats.compaction.cost_usd.expect("repriced summarizer");
    assert!((got - expected).abs() < 1e-12, "{got} != {expected}");

    // Positive control: without `reprice` it stays unpriced.
    let stats = run(&mut left).await;
    assert!(stats.compaction.requests >= 1, "the test must summarize");
    assert!(stats.compaction.is_unpriced(), "{:?}", stats.compaction);

    global::clear_bundled();
}
