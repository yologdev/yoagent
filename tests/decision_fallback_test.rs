//! Fallback chains (`DecisionModel::or`): which errors fall back, the one
//! overall timeout, per-member validation, stats, and the tool gate over a
//! chain. All `MockBackend` — no network.

use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};
use yoagent::decision::*;
use yoagent::provider::mock::*;
use yoagent::provider::{MockProvider, ModelConfig};
use yoagent::*;

fn answers(model: &'static str, p: f64) -> MockBackend {
    MockBackend::from_fn(move |req| {
        let mut eval = Evaluation::new(model, DecisionUsage::new(10, 0));
        for (id, _) in &req.questions {
            eval = eval.with_answer(id.clone(), NoulAnswer::new(p));
        }
        Ok(eval)
    })
}

fn failing(e: DecisionError) -> MockBackend {
    MockBackend::from_fn(move |_| Err(e.clone()))
}

/// A backend that never answers in time.
struct Slow;

#[async_trait::async_trait]
impl DecisionBackend for Slow {
    fn capabilities(&self) -> Capabilities {
        Capabilities::new(QuestionKind::all())
    }
    async fn evaluate(&self, _request: &Request) -> Result<Evaluation, DecisionError> {
        tokio::time::sleep(Duration::from_secs(30)).await;
        Err(DecisionError::transport("unreachable"))
    }
}

#[tokio::test]
async fn a_failing_primary_falls_back() {
    for error in [
        DecisionError::http(503, "down"),
        DecisionError::rate_limited(429, None),
        DecisionError::transport("refused"),
        DecisionError::MissingApiKey("TYPESAFE_API_KEY".into()),
        DecisionError::BadResponse("garbage".into()),
        DecisionError::Unsupported("no nouls here".into()),
        DecisionError::backend("custom failure"),
    ] {
        let primary = failing(error.clone());
        let fallback = answers("fallback-1", 0.8);
        let model = DecisionModel::from_backend(primary.clone(), "primary")
            .or(DecisionModel::from_backend(fallback.clone(), "fallback"));
        let eval = model.ask("s").noul("q", "q?").send().await.unwrap();
        assert_eq!(
            eval.model(),
            "fallback-1",
            "{error:?}: the fallback answered"
        );
        assert_eq!(eval.p_true("q"), Some(0.8));
        assert_eq!(primary.request_count(), 1);
        assert_eq!(fallback.request_count(), 1);
        // Each member is asked for its own model id.
        assert_eq!(primary.requests()[0].model, "primary");
        assert_eq!(fallback.requests()[0].model, "fallback");
    }
}

#[tokio::test]
async fn a_healthy_primary_answers_alone() {
    let primary = answers("primary-1", 0.3);
    let fallback = answers("fallback-1", 0.8);
    let model = DecisionModel::from_backend(primary.clone(), "primary")
        .or(DecisionModel::from_backend(fallback.clone(), "fallback"));
    let eval = model.ask("s").noul("q", "q?").send().await.unwrap();
    assert_eq!(eval.model(), "primary-1");
    assert_eq!(fallback.request_count(), 0);
    assert_eq!(model.model(), "primary", "model() is the primary's");
    assert_eq!(model.fallback_models().collect::<Vec<_>>(), ["fallback"]);
}

#[tokio::test]
async fn a_members_invalid_falls_through_but_a_request_invalid_everywhere_does_not() {
    // A server-side rejection (422) is that server's limit: the next member
    // is tried.
    let primary = failing(DecisionError::Invalid(
        "server rejected the request (422)".into(),
    ));
    let fallback = answers("fallback-1", 0.8);
    let model = DecisionModel::from_backend(primary, "primary")
        .or(DecisionModel::from_backend(fallback.clone(), "fallback"));
    let eval = model.ask("s").noul("q", "q?").send().await.unwrap();
    assert_eq!(eval.model(), "fallback-1");
    assert_eq!(fallback.request_count(), 1);

    // A request invalid everywhere (a one-option choice): nobody is asked.
    let primary = answers("primary-1", 0.5);
    let fallback = answers("fallback-1", 0.8);
    let model = DecisionModel::from_backend(primary.clone(), "primary")
        .or(DecisionModel::from_backend(fallback.clone(), "fallback"));
    let e = model.choice("s", "which?", ["only"]).await.unwrap_err();
    assert!(matches!(e, DecisionError::Invalid(_)), "{e:?}");
    assert_eq!(primary.request_count() + fallback.request_count(), 0);
}

#[tokio::test]
async fn members_are_validated_against_their_own_capabilities() {
    // The primary takes at most 3 options: a 5-option Choice skips it
    // without sending, and the fallback (255) answers.
    let primary = MockBackend::neutral()
        .with_capabilities(Capabilities::new(QuestionKind::all()).with_max_choice_options(3));
    let fallback = MockBackend::neutral();
    let model = DecisionModel::from_backend(primary.clone(), "small")
        .or(DecisionModel::from_backend(fallback.clone(), "big"));
    let a = model
        .choice("s", "which?", ["a", "b", "c", "d", "e"])
        .await
        .unwrap();
    assert_eq!(a.probabilities().count(), 5);
    assert_eq!(primary.request_count(), 0, "skipped, nothing sent");
    assert_eq!(fallback.request_count(), 1);

    // A kind the primary lacks falls back the same way.
    let primary =
        MockBackend::neutral().with_capabilities(Capabilities::new(&[QuestionKind::Noul]));
    let model = DecisionModel::from_backend(primary.clone(), "nouls-only")
        .or(DecisionModel::from_backend(MockBackend::neutral(), "all"));
    model.score("s", "how?", ["lo", "hi"]).await.unwrap();
    assert_eq!(primary.request_count(), 0);
}

#[tokio::test]
async fn when_every_member_fails_all_errors_are_listed() {
    let model = DecisionModel::from_backend(failing(DecisionError::http(503, "down")), "a")
        .or(DecisionModel::from_backend(
            failing(DecisionError::transport("refused")),
            "b",
        ))
        .or(DecisionModel::from_backend(
            failing(DecisionError::rate_limited(429, None)),
            "c",
        ));
    let e = model.noul("s", "q?").await.unwrap_err();
    let DecisionError::AllFailed { attempts, .. } = &e else {
        panic!("expected AllFailed, got {e:?}");
    };
    let models: Vec<&str> = attempts.iter().map(|a| a.model()).collect();
    assert_eq!(models, ["a", "b", "c"]);
    assert!(attempts.iter().all(|a| a.was_sent()));
    assert!(matches!(
        attempts[0].error(),
        DecisionError::Http { status: 503, .. }
    ));
    assert!(matches!(
        attempts[1].error(),
        DecisionError::Transport { .. }
    ));
    let text = e.to_string();
    assert!(text.contains("[a]") && text.contains("[c]"), "{text}");
    // Retryable because some members' errors were.
    assert!(e.is_retryable());

    // None retryable, one skipped unsent.
    let small = MockBackend::neutral()
        .with_capabilities(Capabilities::new(QuestionKind::all()).with_max_choice_options(2));
    let model = DecisionModel::from_backend(small, "small").or(DecisionModel::from_backend(
        failing(DecisionError::http(401, "no")),
        "b",
    ));
    let e = model
        .choice("s", "which?", ["x", "y", "z"])
        .await
        .unwrap_err();
    let DecisionError::AllFailed { attempts, .. } = &e else {
        panic!("expected AllFailed, got {e:?}");
    };
    assert!(!attempts[0].was_sent() && attempts[1].was_sent());
    assert!(e.to_string().contains("[small (skipped)]"), "{e}");
    assert!(!e.is_retryable());
}

/// A backend that reports its own timeout at once.
struct OwnTimeout;

#[async_trait::async_trait]
impl DecisionBackend for OwnTimeout {
    fn capabilities(&self) -> Capabilities {
        Capabilities::new(QuestionKind::all())
    }
    async fn evaluate(&self, _request: &Request) -> Result<Evaluation, DecisionError> {
        Err(DecisionError::Timeout(Duration::from_secs(1)))
    }
}

#[tokio::test]
async fn a_backends_own_timeout_does_not_end_the_chain() {
    let fallback = answers("fallback-1", 0.8);
    let model = DecisionModel::from_backend(OwnTimeout, "a")
        .or(DecisionModel::from_backend(fallback.clone(), "b"));
    let eval = model.ask("s").noul("q", "q?").send().await.unwrap();
    assert_eq!(eval.model(), "fallback-1");
    assert_eq!(fallback.request_count(), 1);
}

#[tokio::test]
async fn chains_flatten_in_order() {
    let a = failing(DecisionError::http(500, "x"));
    let b = failing(DecisionError::http(500, "y"));
    let c = answers("c-1", 0.9);
    // a.or(b.or(c)) tries a, b, c.
    let model =
        DecisionModel::from_backend(a.clone(), "a").or(DecisionModel::from_backend(b.clone(), "b")
            .or(DecisionModel::from_backend(c.clone(), "c")));
    assert_eq!(model.fallback_models().collect::<Vec<_>>(), ["b", "c"]);
    let eval = model.ask("s").noul("q", "q?").send().await.unwrap();
    assert_eq!(eval.model(), "c-1");
    assert_eq!(
        (a.request_count(), b.request_count(), c.request_count()),
        (1, 1, 1)
    );
}

#[tokio::test]
async fn one_overall_timeout_covers_the_chain() {
    // A hanging primary uses the whole budget: the call times out at the
    // chain's limit, not the primary's 30 s default, and the fallback never
    // runs.
    let fallback = answers("fallback-1", 0.8);
    let model = DecisionModel::from_backend(Slow, "slow")
        .or(DecisionModel::from_backend(fallback.clone(), "fallback"))
        .with_timeout(Duration::from_millis(100));
    let start = Instant::now();
    let e = model.noul("s", "q?").await.unwrap_err();
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "{:?}",
        start.elapsed()
    );
    assert!(
        matches!(e, DecisionError::Timeout(d) if d == Duration::from_millis(100)),
        "{e:?}"
    );
    assert_eq!(fallback.request_count(), 0);

    // With a per-attempt limit on the primary, the fallback gets the rest.
    let model = DecisionModel::from_backend(Slow, "slow")
        .with_attempt_timeout(Duration::from_millis(50))
        .or(DecisionModel::from_backend(fallback.clone(), "fallback"))
        .with_timeout(Duration::from_secs(2));
    let start = Instant::now();
    let eval = model.ask("s").noul("q", "q?").send().await.unwrap();
    assert!(start.elapsed() < Duration::from_secs(1));
    assert_eq!(eval.model(), "fallback-1");

    // A fallback's own `with_timeout` does not extend the budget.
    let model = DecisionModel::from_backend(failing(DecisionError::http(500, "x")), "a")
        .or(DecisionModel::from_backend(Slow, "slow").with_timeout(Duration::from_secs(60)))
        .with_timeout(Duration::from_millis(100));
    let start = Instant::now();
    let e = model.noul("s", "q?").await.unwrap_err();
    assert!(start.elapsed() < Duration::from_secs(2));
    assert!(matches!(e, DecisionError::Timeout(_)), "{e:?}");
}

/// The run's `SessionStats` for one prompt.
async fn run_stats(mut agent: Agent, prompt: &str) -> (Agent, SessionStats) {
    let mut rx = agent.prompt(prompt).await;
    let mut stats = None;
    while let Some(e) = rx.recv().await {
        if let AgentEvent::AgentEnd { stats: s, .. } = e {
            stats = Some(s);
        }
    }
    agent.finish().await;
    (agent, stats.expect("AgentEnd"))
}

/// A tool that records its runs.
struct Rm(Arc<std::sync::Mutex<usize>>);

#[async_trait::async_trait]
impl AgentTool for Rm {
    fn name(&self) -> &str {
        "rm"
    }
    fn label(&self) -> &str {
        "rm"
    }
    fn description(&self) -> &str {
        "Remove a file."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({"type": "object"})
    }
    async fn execute(
        &self,
        _params: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        *self.0.lock().unwrap() += 1;
        Ok(ToolResult {
            content: vec![Content::Text { text: "ok".into() }],
            details: serde_json::Value::Null,
        })
    }
}

fn gate_answers(destructive: f64, requested: f64) -> MockBackend {
    MockBackend::from_fn(move |req| {
        let mut eval = Evaluation::new("fallback-1", DecisionUsage::new(50, 0));
        for (id, _) in &req.questions {
            let p = if id == "destructive" {
                destructive
            } else {
                requested
            };
            eval = eval.with_answer(id.clone(), NoulAnswer::new(p));
        }
        Ok(eval)
    })
}

async fn gated_run(fallback: MockBackend) -> (usize, SessionStats) {
    let ran = Arc::new(std::sync::Mutex::new(0));
    let provider = MockProvider::new(vec![
        MockResponse::ToolCalls(vec![MockToolCall {
            provider_metadata: None,
            name: "rm".into(),
            arguments: json!({"path": "/tmp/x"}),
        }]),
        MockResponse::Text("done".into()),
    ]);
    let chain = DecisionModel::from_backend(failing(DecisionError::http(503, "down")), "primary")
        .or(DecisionModel::from_backend(fallback, "fallback"));
    let agent = Agent::from_provider(provider, ModelConfig::mock())
        .with_tools(vec![Box::new(Rm(ran.clone()))])
        .with_tool_gate(ToolGate::new(chain));
    let (_agent, stats) = run_stats(agent, "summarize the README").await;
    let n = *ran.lock().unwrap();
    (n, stats)
}

#[tokio::test]
async fn the_gate_follows_the_fallbacks_verdict() {
    // Primary down, fallback says destructive and unrequested: denied.
    let (ran, stats) = gated_run(gate_answers(0.95, 0.1)).await;
    assert_eq!(ran, 0, "denied by the fallback");
    // Both attempts recorded: the primary's failure and the fallback's answer.
    assert_eq!(stats.decision.requests, 2);
    assert_eq!(stats.decision.failures, 1);
    assert_eq!(stats.decision.usage.input, 50);

    // Primary down, fallback says harmless: allowed.
    let (ran, _) = gated_run(gate_answers(0.05, 0.1)).await;
    assert_eq!(ran, 1, "allowed by the fallback");

    // Every member down: the gate still fails closed.
    let (ran, stats) = gated_run(failing(DecisionError::transport("refused"))).await;
    assert_eq!(ran, 0);
    assert_eq!(stats.decision.requests, 2);
    assert_eq!(stats.decision.failures, 2);
}

#[tokio::test]
async fn from_arc_shares_one_backend() {
    let mock = MockBackend::neutral();
    let shared: Arc<dyn DecisionBackend> = Arc::new(mock.clone());
    let a = DecisionModel::from_arc(shared.clone(), "a");
    let b = DecisionModel::from_arc(shared, "b");
    a.noul("s", "q?").await.unwrap();
    b.noul("s", "q?").await.unwrap();
    let models: Vec<String> = mock.requests().into_iter().map(|r| r.model).collect();
    assert_eq!(models, ["a", "b"]);
}

/// Counts evaluations in flight and records the peak.
struct InFlight {
    caps: Capabilities,
    now: Arc<std::sync::atomic::AtomicUsize>,
    peak: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl DecisionBackend for InFlight {
    fn capabilities(&self) -> Capabilities {
        self.caps.clone()
    }
    async fn evaluate(&self, request: &Request) -> Result<Evaluation, DecisionError> {
        use std::sync::atomic::Ordering;
        let n = self.now.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(n, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(30)).await;
        self.now.fetch_sub(1, Ordering::SeqCst);
        let mut eval = Evaluation::new("m", DecisionUsage::default());
        for (id, _) in &request.questions {
            eval = eval.with_answer(id.clone(), NoulAnswer::new(0.5));
        }
        Ok(eval)
    }
}

async fn peak_for(caps: Capabilities) -> usize {
    let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let backend = InFlight {
        caps,
        now: Arc::default(),
        peak: peak.clone(),
    };
    let model = DecisionModel::from_backend(backend, "m");
    let mut ask = model.ask("s");
    for i in 0..12 {
        ask = ask.noul(format!("q{i}"), "q?");
    }
    let eval = ask.send().await.unwrap();
    assert_eq!(eval.answers().count(), 12);
    peak.load(std::sync::atomic::Ordering::SeqCst)
}

#[tokio::test]
async fn a_split_request_respects_the_concurrency_cap() {
    let split = || Capabilities::new(QuestionKind::all()).with_batching(false);
    // The default: one at a time.
    assert_eq!(split().max_concurrent_requests, 1);
    assert_eq!(peak_for(split()).await, 1);
    // The logprob backend's cap.
    let peak = peak_for(split().with_max_concurrent_requests(8)).await;
    assert!(peak > 1 && peak <= 8, "{peak}");
    let peak = peak_for(split().with_max_concurrent_requests(3)).await;
    assert!(peak > 1 && peak <= 3, "{peak}");
}
