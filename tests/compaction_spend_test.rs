//! `LlmCompaction`'s summarization spend reaches `SessionStats::compaction`,
//! `total_usage()` / `total_cost_usd()` and `Agent::compaction_spend()` —
//! counted once, priced at the summarizer's own rates, unpriced when the
//! summarizer is, and folded in from sub-agents.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use yoagent::context::ContextConfig;
use yoagent::provider::mock::*;
use yoagent::provider::{
    CostConfig, MockProvider, ModelConfig, ProviderError, StreamConfig, StreamEvent, StreamProvider,
};
use yoagent::*;

const MAIN_USAGE: Usage = Usage {
    input: 100,
    output: 10,
    cache_read: 0,
    cache_write: 0,
    total_tokens: 0,
};

const SUMMARY_USAGE: Usage = Usage {
    input: 1_000,
    output: 100,
    cache_read: 0,
    cache_write: 0,
    total_tokens: 1_100,
};

fn main_cost() -> CostConfig {
    CostConfig::new(1.0, 2.0)
}

fn summarizer_cost() -> CostConfig {
    CostConfig::new(10.0, 20.0)
}

/// Answers every summarization request with the same briefing and usage, and
/// counts the requests.
struct Summarizer {
    calls: Arc<AtomicUsize>,
    stop: StopReason,
}

#[async_trait::async_trait]
impl StreamProvider for Summarizer {
    async fn stream(
        &self,
        _config: StreamConfig,
        _tx: mpsc::UnboundedSender<StreamEvent>,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Message, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Message::assistant(
            vec![Content::Text {
                text: "Briefing: the user is filling the context with bulk output.".into(),
            }],
            self.stop.clone(),
            "summarizer",
            "mock",
            SUMMARY_USAGE,
        ))
    }
}

/// Returns a large output after a pause, so history grows past the trigger
/// and a background summarization has time to finish before the next turn.
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

fn bulk_call(i: usize) -> MockResponse {
    MockResponse::ToolCallsWithUsage(
        vec![MockToolCall {
            provider_metadata: None,
            name: "bulk".into(),
            arguments: serde_json::json!({ "i": i }),
        }],
        MAIN_USAGE,
    )
}

/// `tool_turns` bulk-tool turns and a closing text turn per run, for `runs`
/// runs.
fn main_script(runs: usize, tool_turns: usize) -> Vec<MockResponse> {
    let mut script = Vec::new();
    for run in 0..runs {
        for t in 0..tool_turns {
            script.push(bulk_call(run * 100 + t));
        }
        script.push(MockResponse::TextWithUsage("done".into(), MAIN_USAGE));
    }
    script
}

fn compacting_agent(
    script: Vec<MockResponse>,
    summarizer_cost: Option<CostConfig>,
    stop: StopReason,
) -> (Agent, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut summarizer_config = ModelConfig::mock();
    summarizer_config.cost = summarizer_cost;
    let compaction = LlmCompaction::from_provider(
        Arc::new(Summarizer {
            calls: Arc::clone(&calls),
            stop,
        }),
        summarizer_config,
    )
    .with_api_key("test")
    .with_trigger_ratio(0.3)
    .with_retain_tail_tokens(400);

    let mut main_config = ModelConfig::mock();
    main_config.cost = Some(main_cost());
    let agent = Agent::from_provider(MockProvider::new(script), main_config)
        .with_api_key("test")
        .with_tools(vec![Box::new(BulkTool)])
        .with_context_config(ContextConfig {
            max_context_tokens: 4_000,
            system_prompt_tokens: 0,
            keep_first: 1,
            keep_recent: 2,
            ..Default::default()
        })
        .with_compaction_strategy(compaction);
    (agent, calls)
}

/// Run one prompt and return the stats on its `AgentEnd`.
async fn run(agent: &mut Agent, prompt: &str) -> SessionStats {
    let (tx, mut rx) = mpsc::unbounded_channel();
    agent.prompt_with_sender(prompt, tx).await;
    let mut stats = None;
    while let Ok(event) = rx.try_recv() {
        if let AgentEvent::AgentEnd { stats: s, .. } = event {
            stats = Some(s);
        }
    }
    stats.expect("the run sent AgentEnd")
}

fn assert_close(a: f64, b: f64) {
    assert!((a - b).abs() < 1e-12, "{a} != {b}");
}

fn has_briefing(agent: &Agent) -> bool {
    agent.messages().iter().any(|m| {
        matches!(m, AgentMessage::Llm(Message::User { content, .. })
            if content.iter().any(|c| matches!(c, Content::Text { text }
                if text.starts_with(yoagent::llm_compaction::SUMMARY_MARKER))))
    })
}

#[tokio::test]
async fn summary_spend_is_counted_once_at_the_summarizers_rates() {
    // A run of bulk-tool turns, then a one-turn run: its single compaction
    // step comes before its only request, so it can only count what the
    // first run left in flight.
    let mut script = main_script(1, 8);
    script.push(MockResponse::TextWithUsage("ok".into(), MAIN_USAGE));
    let (mut agent, calls) = compacting_agent(script, Some(summarizer_cost()), StopReason::Stop);

    let first = run(&mut agent, "fill the context").await;
    assert!(
        first.compaction.requests >= 1,
        "a summary finished between turns must be counted by this run: {first:?}"
    );
    assert!(has_briefing(&agent), "the test must exercise a splice");

    // Let anything still in flight finish; the next run's first compaction
    // step counts it.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let finished = calls.load(Ordering::SeqCst) as u32;
    let second = run(&mut agent, "keep going").await;

    // Counted once: every request that finished before the second run's
    // compaction step, no more — a splice later in the session does not
    // count its request again. (One spawned by that step is still unreported.)
    let spend = agent.compaction_spend();
    assert_eq!(spend.requests, finished);
    assert_eq!(
        first.compaction.requests + second.compaction.requests,
        spend.requests
    );
    let n = u64::from(spend.requests);
    assert_eq!(spend.usage.input, n * SUMMARY_USAGE.input);
    assert_eq!(spend.usage.output, n * SUMMARY_USAGE.output);
    assert_eq!(spend.usage.total_tokens, 0, "total_tokens is never summed");
    let per_request = summarizer_cost().cost_usd(&SUMMARY_USAGE);
    assert_close(spend.cost_usd.unwrap(), n as f64 * per_request);

    // The main model's own figures are untouched by compaction.
    for stats in [&first, &second] {
        assert_eq!(stats.usage.input, u64::from(stats.turns) * MAIN_USAGE.input);
        assert_eq!(
            stats.usage.output,
            u64::from(stats.turns) * MAIN_USAGE.output
        );
        assert_close(stats.cost_usd.unwrap(), main_cost().cost_usd(&stats.usage));
        assert!(stats.sub_agents.is_empty());
        // Totals add the compaction bucket.
        assert_eq!(
            stats.total_usage().input,
            stats.usage.input + stats.compaction.usage.input
        );
        assert_close(
            stats.total_cost_usd().unwrap(),
            stats.cost_usd.unwrap() + stats.compaction.cost_usd.unwrap_or(0.0),
        );
    }

    // The agent's window: both runs, own turns plus compaction.
    let own = first.cost_usd.unwrap() + second.cost_usd.unwrap();
    assert_close(
        agent.total_cost_usd().unwrap(),
        own + spend.cost_usd.unwrap(),
    );
    assert_eq!(
        agent.total_usage().input,
        first.usage.input + second.usage.input + spend.usage.input
    );
}

#[tokio::test]
async fn an_unpriced_summarizer_makes_the_total_unknown() {
    let (mut agent, _calls) = compacting_agent(main_script(1, 8), None, StopReason::Stop);
    let stats = run(&mut agent, "fill the context").await;

    assert!(stats.compaction.requests >= 1);
    assert_eq!(stats.compaction.cost_usd, None);
    assert!(stats.compaction.is_unpriced());
    // The main model is still priced; only the whole bill is unknown.
    assert!(stats.cost_usd.is_some());
    assert!(!stats.is_unpriced());
    assert_eq!(stats.total_cost_usd(), None);
    assert_eq!(agent.total_cost_usd(), None);
}

#[tokio::test]
async fn a_rejected_summary_is_still_counted() {
    // `Length`: the briefing was cut off, so it is never spliced — but the
    // provider billed the request.
    let (mut agent, calls) = compacting_agent(
        main_script(1, 8),
        Some(summarizer_cost()),
        StopReason::Length,
    );
    let stats = run(&mut agent, "fill the context").await;

    assert!(
        !has_briefing(&agent),
        "a rejected briefing is never spliced"
    );
    assert!(stats.compaction.requests >= 1);
    assert!(stats.compaction.requests as usize <= calls.load(Ordering::SeqCst));
    assert_close(
        stats.compaction.cost_usd.unwrap(),
        f64::from(stats.compaction.requests) * summarizer_cost().cost_usd(&SUMMARY_USAGE),
    );
}

#[tokio::test]
async fn deterministic_compaction_spends_nothing() {
    let mut main_config = ModelConfig::mock();
    main_config.cost = Some(main_cost());
    let mut agent = Agent::from_provider(MockProvider::new(main_script(1, 8)), main_config)
        .with_api_key("test")
        .with_tools(vec![Box::new(BulkTool)])
        .with_context_config(ContextConfig {
            max_context_tokens: 4_000,
            system_prompt_tokens: 0,
            keep_first: 1,
            keep_recent: 2,
            ..Default::default()
        });
    let stats = run(&mut agent, "fill the context").await;
    assert!(stats.compactions > 0, "the test must compact");
    assert!(stats.compaction.is_empty());
    assert_eq!(stats.total_cost_usd(), stats.cost_usd);
    // Omitted from the wire when empty.
    let json = serde_json::to_value(&stats).unwrap();
    assert!(json.get("compaction").is_none(), "{json}");
}

/// Runs a whole compacting child agent and reports its run, the way a
/// custom delegation tool does.
struct DelegateTool;

#[async_trait::async_trait]
impl AgentTool for DelegateTool {
    fn name(&self) -> &str {
        "delegate"
    }
    fn label(&self) -> &str {
        "Delegate"
    }
    fn description(&self) -> &str {
        "Runs a child agent"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    async fn execute(
        &self,
        _params: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let (mut child, _calls) =
            compacting_agent(main_script(1, 8), Some(summarizer_cost()), StopReason::Stop);
        let stats = run(&mut child, "child work").await;
        ctx.report_delegated_run(stats.clone());
        Ok(ToolResult {
            content: vec![Content::Text {
                text: "child done".into(),
            }],
            details: serde_json::json!({ "child": stats }),
        })
    }
}

#[tokio::test]
async fn a_sub_agents_compaction_spend_folds_into_the_parent() {
    let mut main_config = ModelConfig::mock();
    main_config.cost = Some(main_cost());
    let mut parent = Agent::from_provider(
        MockProvider::new(vec![
            MockResponse::ToolCallsWithUsage(
                vec![MockToolCall {
                    provider_metadata: None,
                    name: "delegate".into(),
                    arguments: serde_json::json!({}),
                }],
                MAIN_USAGE,
            ),
            MockResponse::TextWithUsage("all done".into(), MAIN_USAGE),
        ]),
        main_config,
    )
    .with_api_key("test")
    .with_tools(vec![Box::new(DelegateTool)]);

    let stats = run(&mut parent, "delegate it").await;

    // The child's figures, as it reported them.
    let child_compaction = &stats.compaction;
    assert!(
        child_compaction.requests >= 1,
        "the child's compaction spend reaches the parent: {stats:?}"
    );
    // One bucket for the tree, like decision spend: not also in `sub_agents`.
    assert_eq!(stats.sub_agents.runs, 1);
    assert_eq!(
        stats.sub_agents.usage.input % MAIN_USAGE.input,
        0,
        "sub_agents holds the child's own turns only"
    );
    assert_eq!(stats.usage.input, 2 * MAIN_USAGE.input);
    assert_close(
        stats.total_cost_usd().unwrap(),
        stats.cost_usd.unwrap()
            + stats.sub_agents.cost_usd.unwrap()
            + child_compaction.cost_usd.unwrap(),
    );
    assert_eq!(
        stats.total_usage().input,
        stats.usage.input + stats.sub_agents.usage.input + child_compaction.usage.input
    );
    assert_eq!(parent.compaction_spend(), child_compaction);
}

#[test]
fn stats_without_the_field_still_deserialize() {
    let old = serde_json::json!({"usage": {"input": 5, "output": 1, "cacheRead": 0,
        "cacheWrite": 0, "totalTokens": 0}, "turns": 1, "compactions": 2});
    let stats: SessionStats = serde_json::from_value(old).unwrap();
    assert!(stats.compaction.is_empty());
    assert_eq!(stats.compactions, 2);
}
