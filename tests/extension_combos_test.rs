//! Extensions combined with the rest of the loop (#241): the older hooks and
//! the tool gate on one call, compaction, steering, the sequential and
//! batched tool strategies, structured output, and provider retries.
//!
//! `extension_test.rs` pins each hook on its own; these pin that the pieces
//! still agree when a run uses several features at once. Every test carries a
//! positive control (the feature really engaged), so none passes vacuously.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use yoagent::extension::*;
use yoagent::provider::mock::{MockResponse, MockToolCall};
use yoagent::provider::{
    CostConfig, MockProvider, ModelConfig, ProviderError, StreamConfig, StreamEvent, StreamProvider,
};
use yoagent::*;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

type Log = Arc<Mutex<Vec<String>>>;

/// Records every request's messages; fails the attempts whose (0-based)
/// index is in `fail`, with a retryable rate limit, before streaming.
struct Recording {
    inner: MockProvider,
    requests: Arc<Mutex<Vec<Vec<Message>>>>,
    attempts: AtomicUsize,
    fail: Vec<usize>,
}

#[async_trait::async_trait]
impl StreamProvider for Recording {
    async fn stream(
        &self,
        config: StreamConfig,
        tx: mpsc::UnboundedSender<StreamEvent>,
        cancel: CancellationToken,
    ) -> Result<Message, ProviderError> {
        let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
        if self.fail.contains(&attempt) {
            return Err(ProviderError::RateLimited {
                retry_after_ms: Some(1),
            });
        }
        self.requests.lock().unwrap().push(config.messages.clone());
        self.inner.stream(config, tx, cancel).await
    }
}

type Requests = Arc<Mutex<Vec<Vec<Message>>>>;

fn provider(script: Vec<MockResponse>, fail: Vec<usize>) -> (Recording, Requests) {
    let requests = Requests::default();
    let provider = Recording {
        inner: MockProvider::new(script),
        requests: requests.clone(),
        attempts: AtomicUsize::new(0),
        fail,
    };
    (provider, requests)
}

fn calls(list: &[(&str, serde_json::Value)]) -> MockResponse {
    MockResponse::ToolCalls(
        list.iter()
            .map(|(name, args)| MockToolCall {
                name: name.to_string(),
                arguments: args.clone(),
                provider_metadata: None,
            })
            .collect(),
    )
}

fn echo(text: &str) -> MockResponse {
    calls(&[("echo", serde_json::json!({ "text": text }))])
}

fn text(t: &str) -> MockResponse {
    MockResponse::Text(t.into())
}

async fn run(agent: &mut Agent, prompt: &str) -> Vec<AgentEvent> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    agent.prompt_with_sender(prompt, tx).await;
    let mut events = Vec::new();
    while let Ok(e) = rx.try_recv() {
        events.push(e);
    }
    events
}

fn texts(content: &[Content]) -> String {
    content
        .iter()
        .filter_map(|c| match c {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Every `ToolExecutionEnd`: `(text, is_error)`, in event order.
fn tool_results(events: &[AgentEvent]) -> Vec<(String, bool)> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolExecutionEnd {
                result, is_error, ..
            } => Some((texts(&result.content), *is_error)),
            _ => None,
        })
        .collect()
}

fn assistant_messages(events: &[AgentEvent]) -> usize {
    events
        .iter()
        .filter(|e| {
            matches!(
                e,
                AgentEvent::MessageEnd {
                    message: AgentMessage::Llm(Message::Assistant { .. })
                }
            )
        })
        .count()
}

/// Echoes `text`, streaming a partial result first; records each run.
struct Echo(Log);

#[async_trait::async_trait]
impl AgentTool for Echo {
    fn name(&self) -> &str {
        "echo"
    }
    fn label(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "Echo the text"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}}})
    }
    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let text = params["text"].as_str().unwrap_or_default().to_string();
        self.0.lock().unwrap().push(text.clone());
        if let Some(update) = &ctx.on_update {
            update(ToolResult {
                content: vec![Content::Text {
                    text: format!("partial {text}"),
                }],
                details: serde_json::Value::Null,
            });
        }
        Ok(ToolResult {
            content: vec![Content::Text {
                text: format!("echo {text}"),
            }],
            details: serde_json::Value::Null,
        })
    }
}

// ---------------------------------------------------------------------------
// Tool strategies
// ---------------------------------------------------------------------------

/// Denies `forbidden`, upper-cases every result, and counts its calls.
#[derive(Clone)]
struct Guard {
    before: Arc<AtomicUsize>,
    after: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl RunHooks for Guard {
    async fn before_tool(&self, call: &ToolCallRequest<'_>) -> ToolDecision {
        self.before.fetch_add(1, Ordering::SeqCst);
        if call.args["text"] == "forbidden" {
            ToolDecision::Deny("forbidden is not allowed".into())
        } else {
            ToolDecision::Allow
        }
    }
    async fn after_tool(
        &self,
        _call: &ToolCallRequest<'_>,
        output: &mut ToolOutput,
    ) -> Result<(), ExtensionError> {
        self.after.fetch_add(1, Ordering::SeqCst);
        for c in &mut output.result.content {
            if let Content::Text { text } = c {
                *text = text.to_uppercase();
            }
        }
        Ok(())
    }
}

/// Every strategy goes through the same choke point: each call is judged,
/// a denial stays with its call, every result that ran is filtered, and
/// partial output is withheld.
#[tokio::test]
async fn every_tool_strategy_judges_filters_and_withholds_each_call() {
    for strategy in [
        ToolExecutionStrategy::Sequential,
        ToolExecutionStrategy::Parallel,
        ToolExecutionStrategy::Batched { size: 2 },
        // Runs as size 1.
        ToolExecutionStrategy::Batched { size: 0 },
    ] {
        for filters in [true, false] {
            let ran = Log::default();
            let guard = Guard {
                before: Arc::default(),
                after: Arc::default(),
            };
            let (p, _) = provider(
                vec![
                    calls(&[
                        ("echo", serde_json::json!({"text": "a"})),
                        ("echo", serde_json::json!({"text": "forbidden"})),
                        ("echo", serde_json::json!({"text": "c"})),
                    ]),
                    text("done"),
                ],
                vec![],
            );
            let mut ext = ClonedHooks::new("guard", guard.clone());
            if filters {
                ext = ext.filters_tool_output();
            }
            let mut agent = Agent::from_provider(p, ModelConfig::mock())
                .with_tools(vec![Box::new(Echo(ran.clone()))])
                .with_tool_execution(strategy.clone())
                .with_extension(ext);
            let events = run(&mut agent, "go").await;

            // Positive control: the tools really ran.
            let mut ran = ran.lock().unwrap().clone();
            ran.sort();
            assert_eq!(ran, ["a", "c"], "{strategy:?}");
            let mut results = tool_results(&events);
            results.sort();
            assert_eq!(
                results,
                [
                    ("ECHO A".to_string(), false),
                    ("ECHO C".to_string(), false),
                    (
                        "Tool call denied: forbidden is not allowed".to_string(),
                        true
                    ),
                ],
                "{strategy:?}"
            );
            assert_eq!(guard.before.load(Ordering::SeqCst), 3, "{strategy:?}");
            assert_eq!(
                guard.after.load(Ordering::SeqCst),
                2,
                "not for the denied call: {strategy:?}"
            );
            // Withheld while the extension filters; streamed (the control)
            // when it does not say so.
            let updates = events
                .iter()
                .filter(|e| matches!(e, AgentEvent::ToolExecutionUpdate { .. }))
                .count();
            assert_eq!(
                updates,
                if filters { 0 } else { 2 },
                "{strategy:?} filters={filters}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Compaction and steering
// ---------------------------------------------------------------------------

/// The level-3 compaction marker (crate-private; `is_loop_injected` knows it).
const COMPACTION_MARKER_TEXT: &str =
    "[Context compacted: earlier messages removed to fit the context window]";

/// Keeps the session's head, a marker and the last two messages on every
/// turn: the run's own prompt is gone after its first turn.
struct HeadMarkerTail;

impl yoagent::context::CompactionStrategy for HeadMarkerTail {
    fn compact(
        &self,
        messages: Vec<AgentMessage>,
        _config: &yoagent::context::ContextConfig,
    ) -> Vec<AgentMessage> {
        if messages.len() <= 4 {
            return messages;
        }
        let mut out = vec![
            messages[0].clone(),
            AgentMessage::Llm(Message::user(COMPACTION_MARKER_TEXT)),
        ];
        out.extend_from_slice(&messages[messages.len() - 2..]);
        out
    }
}

fn earlier_session() -> Vec<AgentMessage> {
    vec![
        AgentMessage::Llm(Message::user("an earlier question")),
        AgentMessage::Llm(Message::assistant(
            vec![Content::Text {
                text: "an earlier answer".into(),
            }],
            StopReason::Stop,
            "mock",
            "mock",
            Usage::default(),
        )),
    ]
}

/// Records what each hook saw of the user's request.
#[derive(Clone)]
struct Witness(Log);

#[async_trait::async_trait]
impl RunHooks for Witness {
    async fn on_input(&mut self, input: &InputContext<'_>) -> InputDecision {
        self.0
            .lock()
            .unwrap()
            .push(format!("input: {}", input.text));
        InputDecision::Pass
    }
    async fn before_model(&mut self, turn: &TurnContext<'_>) -> TurnDecision {
        let prompts: Vec<String> = turn.run_prompts.iter().map(user_text).collect();
        self.0
            .lock()
            .unwrap()
            .push(format!("model: run prompts {prompts:?}"));
        TurnDecision::Continue
    }
    async fn before_tool(&self, call: &ToolCallRequest<'_>) -> ToolDecision {
        self.0.lock().unwrap().push(format!(
            "tool {}: {}",
            call.args["text"].as_str().unwrap_or_default(),
            call.user_request().unwrap_or_default()
        ));
        ToolDecision::Allow
    }
    async fn on_stop(&mut self, _stop: &StopContext<'_>) -> StopDecision {
        self.0.lock().unwrap().push("stop".into());
        StopDecision::Accept
    }
}

fn user_text(m: &Message) -> String {
    match m {
        Message::User { content, .. } => texts(content),
        _ => String::new(),
    }
}

/// Compaction drops the run's prompt from the transcript mid-run; an
/// extension's hooks still see it (the run prompts survive compaction).
#[tokio::test]
async fn extension_hooks_keep_the_user_request_after_compaction() {
    let log = Log::default();
    let (p, requests) = provider(vec![echo("one"), echo("two"), text("done")], vec![]);
    let mut agent = Agent::from_provider(p, ModelConfig::mock())
        .with_messages(earlier_session())
        .with_tools(vec![Box::new(Echo(Log::default()))])
        .with_compaction_strategy(HeadMarkerTail)
        .with_extension(ClonedHooks::new("witness", Witness(log.clone())));
    let _ = run(&mut agent, "clean the cache").await;

    // Positive control: compaction really removed the prompt from what the
    // model was sent after the first turn.
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(format!("{:?}", requests[0]).contains("clean the cache"));
    assert!(
        !format!("{:?}", requests[2]).contains("clean the cache"),
        "{:?}",
        requests[2]
    );
    assert!(format!("{:?}", requests[2]).contains(COMPACTION_MARKER_TEXT));

    let log = log.lock().unwrap().clone();
    let tool_lines: Vec<_> = log.iter().filter(|l| l.starts_with("tool")).collect();
    assert_eq!(tool_lines.len(), 2, "{log:?}");
    for line in &tool_lines {
        assert!(line.ends_with("clean the cache"), "{log:?}");
        assert!(!line.contains("an earlier question"), "{log:?}");
    }
    let model_lines: Vec<_> = log.iter().filter(|l| l.starts_with("model")).collect();
    assert_eq!(model_lines.len(), 3, "{log:?}");
    assert!(
        model_lines
            .iter()
            .all(|l| l.as_str() == r#"model: run prompts ["clean the cache"]"#),
        "{log:?}"
    );
}

/// A steering message is not screened by `on_input` (only the run's input
/// is), but extensions see it everywhere else: in the run prompts
/// `before_model` gets, and as the request `before_tool` judges against.
/// `on_stop` judges once, at the very end.
#[tokio::test]
async fn steering_reaches_before_model_and_before_tool_but_not_on_input() {
    let log = Log::default();
    let (p, requests) = provider(vec![echo("one"), echo("two"), text("done")], vec![]);
    let mut agent = Agent::from_provider(p, ModelConfig::mock())
        .with_tools(vec![Box::new(Echo(Log::default()))])
        .with_extension(ClonedHooks::new("witness", Witness(log.clone())));
    // Queued before the run: injected before its first request.
    agent.steer(AgentMessage::Llm(Message::user("also clear the logs")));
    let _ = run(&mut agent, "clean the cache").await;

    // Positive control: the steering message reached the model.
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(format!("{:?}", requests[0]).contains("also clear the logs"));

    let log = log.lock().unwrap().clone();
    let inputs: Vec<_> = log.iter().filter(|l| l.starts_with("input")).collect();
    assert_eq!(inputs, ["input: clean the cache"], "{log:?}");
    let models: Vec<_> = log.iter().filter(|l| l.starts_with("model")).collect();
    assert_eq!(models.len(), 3, "{log:?}");
    assert!(
        models
            .iter()
            .all(|l| l.as_str()
                == r#"model: run prompts ["clean the cache", "also clear the logs"]"#),
        "{log:?}"
    );
    let tools: Vec<_> = log.iter().filter(|l| l.starts_with("tool")).collect();
    assert_eq!(
        tools,
        [
            "tool one: also clear the logs",
            "tool two: also clear the logs"
        ],
        "{log:?}"
    );
    assert_eq!(log.last().map(String::as_str), Some("stop"), "{log:?}");
    assert_eq!(log.iter().filter(|l| *l == "stop").count(), 1, "{log:?}");
}

// ---------------------------------------------------------------------------
// Structured output
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize, Debug, PartialEq)]
struct Count {
    n: u32,
}

/// Sends the model back until the JSON answer's `n` is at least 2.
#[derive(Clone)]
struct AtLeastTwo(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl RunHooks for AtLeastTwo {
    async fn on_stop(&mut self, stop: &StopContext<'_>) -> StopDecision {
        self.0.fetch_add(1, Ordering::SeqCst);
        let Message::Assistant { content, .. } = stop.answer else {
            return StopDecision::Accept;
        };
        let n = serde_json::from_str::<serde_json::Value>(&texts(content))
            .ok()
            .and_then(|v| v["n"].as_u64())
            .unwrap_or(0);
        if n >= 2 {
            StopDecision::Accept
        } else {
            StopDecision::Continue("n must be at least 2".into())
        }
    }
}

/// `prompt_structured` parses the answer the verifier accepted, not the
/// first one: only this call's last assistant message counts.
#[tokio::test]
async fn prompt_structured_returns_the_answer_on_stop_accepted() {
    let script = || vec![text(r#"{"n": 1}"#), text(r#"{"n": 2}"#)];
    let schema = serde_json::json!({
        "type": "object",
        "properties": {"n": {"type": "integer"}},
        "required": ["n"]
    });

    // Positive control: without the verifier the first answer is returned.
    let (p, _) = provider(script(), vec![]);
    let mut plain = Agent::from_provider(p, ModelConfig::mock());
    let got: Count = plain
        .prompt_structured("count", schema.clone())
        .await
        .unwrap();
    assert_eq!(got, Count { n: 1 });

    let asked = Arc::new(AtomicUsize::new(0));
    let (p, requests) = provider(script(), vec![]);
    let mut verified = Agent::from_provider(p, ModelConfig::mock())
        .with_extension(ClonedHooks::new("at-least-two", AtLeastTwo(asked.clone())));
    let got: Count = verified.prompt_structured("count", schema).await.unwrap();
    assert_eq!(got, Count { n: 2 });
    assert_eq!(asked.load(Ordering::SeqCst), 2, "judged both answers");
    let second = format!("{:?}", requests.lock().unwrap()[1]);
    assert!(
        second.contains("[Extension message: at-least-two] n must be at least 2"),
        "{second}"
    );
}

// ---------------------------------------------------------------------------
// Retries
// ---------------------------------------------------------------------------

/// Counts `before_model` calls and the retries `on_event` observes.
#[derive(Clone)]
struct TurnCounter {
    before_model: Arc<AtomicUsize>,
    retries_seen: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl RunHooks for TurnCounter {
    async fn before_model(&mut self, _turn: &TurnContext<'_>) -> TurnDecision {
        self.before_model.fetch_add(1, Ordering::SeqCst);
        TurnDecision::Continue
    }
    fn on_event(&self, event: &AgentEvent) {
        if matches!(event, AgentEvent::ProviderRetry { .. }) {
            self.retries_seen.fetch_add(1, Ordering::SeqCst);
        }
    }
}

fn costing(cents: u64, response: MockResponse) -> MockResponse {
    let usage = Usage {
        input: cents * 10_000,
        ..Default::default()
    };
    match response {
        MockResponse::ToolCalls(c) => MockResponse::ToolCallsWithUsage(c, usage),
        MockResponse::Text(t) => MockResponse::TextWithUsage(t, usage),
        other => other,
    }
}

/// A retried request is judged once by `before_model`, seen by `on_event`
/// as a `ProviderRetry`, and billed once: the budget stops the run at the
/// same point it would without the retry.
#[tokio::test]
async fn a_retried_request_is_judged_and_billed_once() {
    let script = || {
        vec![
            costing(4, echo("one")),
            costing(4, echo("two")),
            costing(4, echo("three")),
            costing(4, text("done")),
        ]
    };
    let fast_retry = yoagent::retry::RetryConfig {
        max_retries: 2,
        initial_delay_ms: 1,
        backoff_multiplier: 1.0,
        max_delay_ms: 1,
    };
    // Attempt 1 (the second request) fails once and is retried.
    for fail in [vec![], vec![1]] {
        let counter = TurnCounter {
            before_model: Arc::default(),
            retries_seen: Arc::default(),
        };
        let budget = Arc::new(Budget::usd(0.10, CostConfig::new(1.0, 0.0)).across_runs());
        let (p, requests) = provider(script(), fail.clone());
        let mut agent = Agent::from_provider(p, ModelConfig::mock())
            .with_tools(vec![Box::new(Echo(Log::default()))])
            .with_retry_config(fast_retry.clone())
            .with_extension(ClonedHooks::new("counter", counter.clone()))
            .with_extension(budget.clone());
        let events = run(&mut agent, "go").await;

        let retried = !fail.is_empty();
        // Positive control: the retry really happened (or really did not).
        let retry_events = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ProviderRetry { .. }))
            .count();
        assert_eq!(retry_events, usize::from(retried), "fail={fail:?}");
        assert_eq!(
            counter.retries_seen.load(Ordering::SeqCst),
            retry_events,
            "on_event saw the retry: fail={fail:?}"
        );
        // Three requests answered, the fourth stopped by the budget.
        assert_eq!(requests.lock().unwrap().len(), 3, "fail={fail:?}");
        assert_eq!(
            counter.before_model.load(Ordering::SeqCst),
            4,
            "once per turn, not per attempt: fail={fail:?}"
        );
        let spent = budget.spent_usd().unwrap();
        assert!((spent - 0.12).abs() < 1e-9, "fail={fail:?}: {spent}");
        // The failed attempt streamed nothing, so it left no message.
        assert_eq!(assistant_messages(&events), 3, "fail={fail:?}");
        let Some(AgentEvent::AgentEnd { messages, .. }) = events.last() else {
            panic!("ends with AgentEnd");
        };
        assert!(
            format!("{:?}", messages.last()).contains("[Agent stopped: budget of $0.10 spent"),
            "fail={fail:?}: {:?}",
            messages.last()
        );
    }
}

// ---------------------------------------------------------------------------
// Middleware, extension and tool gate on one call
// ---------------------------------------------------------------------------

#[cfg(feature = "decision")]
mod with_the_gate {
    use super::*;
    use yoagent::decision::*;

    /// Rewrites every call's path to `/sandbox/a`.
    struct Sandbox;

    #[async_trait::async_trait]
    impl ToolMiddleware for Sandbox {
        async fn before_tool(&self, _call: &ToolCallRequest<'_>) -> ToolDecision {
            ToolDecision::Modify(serde_json::json!({"path": "/sandbox/a"}))
        }
    }

    /// Records the arguments it sees; then denies, or rewrites to `/sandbox/b`.
    #[derive(Clone)]
    struct Narrow {
        seen: Log,
        deny: bool,
    }

    #[async_trait::async_trait]
    impl RunHooks for Narrow {
        async fn before_tool(&self, call: &ToolCallRequest<'_>) -> ToolDecision {
            self.seen.lock().unwrap().push(call.args.to_string());
            if self.deny {
                ToolDecision::Deny("narrowed away".into())
            } else {
                ToolDecision::Modify(serde_json::json!({"path": "/sandbox/b"}))
            }
        }
    }

    /// A tool that records its arguments.
    struct Rm(Log);

    #[async_trait::async_trait]
    impl AgentTool for Rm {
        fn name(&self) -> &str {
            "rm"
        }
        fn label(&self) -> &str {
            "rm"
        }
        fn description(&self) -> &str {
            "remove a file"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(
            &self,
            args: serde_json::Value,
            _: ToolContext,
        ) -> Result<ToolResult, ToolError> {
            self.0.lock().unwrap().push(args.to_string());
            Ok(ToolResult {
                content: vec![Content::Text { text: "ok".into() }],
                details: serde_json::Value::Null,
            })
        }
    }

    fn harmless() -> MockBackend {
        MockBackend::from_fn(|req| {
            let mut eval = Evaluation::new("gate-test", DecisionUsage::default());
            for (id, _) in &req.questions {
                let p = if id == "requested" { 0.9 } else { 0.1 };
                eval = eval.with_answer(id.clone(), NoulAnswer::new(p));
            }
            Ok(eval)
        })
    }

    /// Middleware, then the agent's extension, then the gate (appended
    /// last): each sees the previous rewrite, and the tool runs with the last.
    /// An extension's denial ends the chain before the gate is asked.
    #[tokio::test]
    async fn middleware_extension_and_gate_judge_one_call_in_that_order() {
        for deny in [false, true] {
            let backend = harmless();
            let seen = Log::default();
            let ran = Log::default();
            let (p, _) = provider(
                vec![
                    calls(&[("rm", serde_json::json!({"path": "/etc/a"}))]),
                    text("done"),
                ],
                vec![],
            );
            let mut agent = Agent::from_provider(p, ModelConfig::mock())
                .with_tools(vec![Box::new(Rm(ran.clone()))])
                // Installed first, still runs last among the three.
                .with_tool_gate(ToolGate::new(DecisionModel::from_backend(
                    backend.clone(),
                    "gate-test",
                )))
                .with_extension(ClonedHooks::new(
                    "narrow",
                    Narrow {
                        seen: seen.clone(),
                        deny,
                    },
                ))
                .with_tool_middleware(Sandbox);
            let events = run(&mut agent, "remove the sandbox file").await;

            // The extension saw the middleware's rewrite, not the model's.
            assert_eq!(
                *seen.lock().unwrap(),
                [r#"{"path":"/sandbox/a"}"#],
                "deny={deny}"
            );
            if deny {
                assert_eq!(backend.request_count(), 0, "the gate was never asked");
                assert!(ran.lock().unwrap().is_empty());
                assert_eq!(
                    tool_results(&events),
                    [("Tool call denied: narrowed away".to_string(), true)]
                );
            } else {
                // Positive control: the gate was asked, about the final
                // arguments, and allowed them.
                let requests = backend.requests();
                assert_eq!(requests.len(), 1);
                assert_eq!(
                    requests[0].state["tool_call"]["arguments"],
                    serde_json::json!({"path": "/sandbox/b"})
                );
                assert_eq!(*ran.lock().unwrap(), [r#"{"path":"/sandbox/b"}"#]);
            }
        }
    }
}
