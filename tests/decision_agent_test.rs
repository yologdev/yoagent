//! Decision models inside the agent: the advisory skill and tool hints, the
//! tool gate (allow, deny, fail-closed), spend reporting, and that nothing is
//! sent when nothing needs asking. All driven by `MockBackend` — no network.

use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use yoagent::decision::*;
use yoagent::provider::mock::*;
use yoagent::provider::{
    CostConfig, MockProvider, ModelConfig, ProviderError, StreamConfig, StreamEvent, StreamProvider,
};
use yoagent::skills::SkillSet;
use yoagent::*;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// One request as the provider received it.
#[derive(Clone)]
struct Sent {
    system: String,
    messages: Vec<Message>,
    tools: usize,
}

impl Sent {
    /// Text of the last user message sent (hint notes included).
    fn last_user_text(&self) -> String {
        self.messages
            .iter()
            .rev()
            .find_map(|m| match m {
                Message::User { content, .. } => Some(
                    content
                        .iter()
                        .filter_map(|c| match c {
                            Content::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("|"),
                ),
                _ => None,
            })
            .unwrap_or_default()
    }
}

type Seen = Arc<Mutex<Vec<Sent>>>;

/// Records every request, then delegates.
struct Recording {
    inner: MockProvider,
    seen: Seen,
}

#[async_trait::async_trait]
impl StreamProvider for Recording {
    async fn stream(
        &self,
        config: StreamConfig,
        tx: mpsc::UnboundedSender<StreamEvent>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Message, ProviderError> {
        self.seen.lock().unwrap().push(Sent {
            system: config.system_prompt.clone(),
            messages: config.messages.clone(),
            tools: config.tools.len(),
        });
        self.inner.stream(config, tx, cancel).await
    }
}

fn recording(inner: MockProvider) -> (Recording, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    (
        Recording {
            inner,
            seen: seen.clone(),
        },
        seen,
    )
}

fn sent(seen: &Seen) -> Vec<Sent> {
    seen.lock().unwrap().clone()
}

/// A tool that records whether it ran.
struct Named {
    name: String,
    ran: Arc<Mutex<Vec<serde_json::Value>>>,
}

#[async_trait::async_trait]
impl AgentTool for Named {
    fn name(&self) -> &str {
        &self.name
    }
    fn label(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "A test tool."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({"type": "object"})
    }
    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        self.ran.lock().unwrap().push(params);
        Ok(ToolResult {
            content: vec![Content::Text { text: "ok".into() }],
            details: serde_json::Value::Null,
        })
    }
}

type Ran = Arc<Mutex<Vec<serde_json::Value>>>;

fn make_tools(n: usize) -> (Vec<Box<dyn AgentTool>>, Ran) {
    let ran: Ran = Arc::new(Mutex::new(Vec::new()));
    let tools = (0..n)
        .map(|i| {
            Box::new(Named {
                name: if i == 0 {
                    "rm".into()
                } else {
                    format!("tool_{i}")
                },
                ran: ran.clone(),
            }) as Box<dyn AgentTool>
        })
        .collect();
    (tools, ran)
}

fn rm_call(args: serde_json::Value) -> MockResponse {
    MockResponse::ToolCalls(vec![MockToolCall {
        provider_metadata: None,
        name: "rm".into(),
        arguments: args,
    }])
}

/// The model calls `rm` once, then answers.
fn calls_rm() -> MockProvider {
    calls_rm_with(json!({"path": "/tmp/scratch.txt"}))
}

fn calls_rm_with(args: serde_json::Value) -> MockProvider {
    MockProvider::new(vec![rm_call(args), MockResponse::Text("done".into())])
}

fn skills(dir: &std::path::Path) -> SkillSet {
    for (name, desc) in [
        ("pdf-fill", "Fill in PDF forms."),
        ("git-release", "Cut a release with git tags."),
    ] {
        let d = dir.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {desc}\n---\n\nSteps.\n"),
        )
        .unwrap();
    }
    SkillSet::load(&[dir]).unwrap()
}

/// Run one prompt; returns the agent and the run's `SessionStats`.
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

async fn run(agent: Agent, prompt: &str) -> Agent {
    run_stats(agent, prompt).await.0
}

/// Every tool result's (text, is_error), in order.
fn tool_results(agent: &Agent) -> Vec<(String, bool)> {
    agent
        .messages()
        .iter()
        .filter_map(|m| match m {
            AgentMessage::Llm(Message::ToolResult {
                content, is_error, ..
            }) => match content.first() {
                Some(Content::Text { text }) => Some((text.clone(), *is_error)),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn tool_result_text(agent: &Agent) -> Option<(String, bool)> {
    tool_results(agent).into_iter().next()
}

/// Answers the advisory questions: `skill` picked by `pick(request)`,
/// `skill_needed` at `needed`, `tools` concentrated on three tools.
fn advisory_answers_by(pick: fn(&str) -> &'static str, needed: f64) -> MockBackend {
    MockBackend::from_fn(move |req| {
        // Judge the latest message when the request carries earlier context.
        let full = req.state["request"].as_str().unwrap_or_default();
        let request = full
            .rsplit("Latest user message: ")
            .next()
            .unwrap_or(full)
            .to_string();
        let mut eval = Evaluation::new("jev-test", DecisionUsage::new(100, 0));
        for (id, q) in &req.questions {
            match id.as_str() {
                "skill" => {
                    let opts = q.options().unwrap();
                    let chosen = pick(&request);
                    let rest = 0.1 / (opts.len() - 1) as f64;
                    eval = eval.with_answer(
                        id.clone(),
                        ChoiceAnswer::new(
                            opts.iter()
                                .map(|o| (o.to_string(), if *o == chosen { 0.9 } else { rest })),
                        ),
                    );
                }
                "skill_needed" => eval = eval.with_answer(id.clone(), NoulAnswer::new(needed)),
                "tools" => {
                    let opts = q.options().unwrap();
                    let p = |o: &str| match o {
                        "tool_7" => 0.5,
                        "tool_3" => 0.3,
                        "tool_9" => 0.15,
                        _ => 0.05 / (opts.len() - 3) as f64,
                    };
                    eval = eval.with_answer(
                        id.clone(),
                        ChoiceAnswer::new(opts.iter().map(|o| (o.to_string(), p(o)))),
                    );
                }
                other => panic!("unexpected advisory question {other}"),
            }
        }
        Ok(eval)
    })
}

fn advisory_answers(pick: &'static str, needed: f64) -> MockBackend {
    match pick {
        "pdf-fill" => advisory_answers_by(|_| "pdf-fill", needed),
        "none" => advisory_answers_by(|_| "none", needed),
        other => panic!("fixture: {other}"),
    }
}

/// Answers the gate's questions with fixed probabilities (`p` for any other
/// check id).
fn gate_answers(destructive: f64, requested: f64) -> MockBackend {
    MockBackend::from_fn(move |req| {
        let mut eval = Evaluation::new("jev-test", DecisionUsage::new(50, 0));
        for (id, _) in &req.questions {
            let p = match id.as_str() {
                "destructive" => destructive,
                "requested" => requested,
                _ => 0.0,
            };
            eval = eval.with_answer(id.clone(), NoulAnswer::new(p));
        }
        Ok(eval)
    })
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
        Err(DecisionError::Transport("unreachable".into()))
    }
}

fn model(mock: &MockBackend) -> DecisionModel {
    DecisionModel::from_backend(mock.clone(), "jev-test")
}

// ---------------------------------------------------------------------------
// Advisory: skill hint
// ---------------------------------------------------------------------------

const SKILL_LINE: &str = "Relevant to the current request: pdf-fill. Ignore this if it does not \
                          fit what the user actually asked for.";

#[tokio::test]
async fn skill_hint_goes_to_the_latest_user_turn_and_asks_once_per_request() {
    let tmp = tempfile::tempdir().unwrap();
    let mock = advisory_answers("pdf-fill", 0.9);
    let (provider, seen) = recording(calls_rm());
    let (tools, _) = make_tools(1);
    let agent = Agent::from_provider(provider, ModelConfig::mock())
        .with_system_prompt("Base.")
        .with_skills(skills(tmp.path()))
        .with_tools(tools)
        .with_decision_model(model(&mock));
    let base = agent.system_prompt.clone();
    let agent = run(agent, "fill in the tax form PDF").await;

    let sent = sent(&seen);
    assert_eq!(sent.len(), 2, "tool turn + final turn");
    for s in &sent {
        assert_eq!(s.system, base, "the system prompt is never touched");
        assert_eq!(
            s.last_user_text(),
            format!("fill in the tax form PDF|{SKILL_LINE}")
        );
    }
    // One decision request for the whole user request, batching both
    // questions; memoized for the second turn.
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 1);
    let ids: Vec<&str> = reqs[0]
        .questions
        .iter()
        .map(|(id, _)| id.as_str())
        .collect();
    assert_eq!(
        ids,
        ["skill", "skill_needed"],
        "no tool question under 40 tools"
    );
    assert_eq!(
        reqs[0].state,
        json!({"request": "fill in the tax form PDF"})
    );
    assert_eq!(
        reqs[0].get("skill").unwrap().options().unwrap(),
        ["git-release", "pdf-fill", "none"]
    );
    // Transient: never stored.
    for m in agent.messages() {
        assert!(!serde_json::to_string(m).unwrap().contains("Relevant to"));
    }
}

#[tokio::test]
async fn hints_keep_the_system_prompt_and_history_prefix_byte_identical() {
    let tmp = tempfile::tempdir().unwrap();
    let mock = advisory_answers_by(
        |r| {
            if r.contains("PDF") {
                "pdf-fill"
            } else {
                "git-release"
            }
        },
        0.9,
    );
    let (provider, seen) = recording(MockProvider::texts(vec!["a1", "a2"]));
    let history = vec![
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
    ];
    let agent = Agent::from_provider(provider, ModelConfig::mock())
        .with_system_prompt("Base.")
        .with_skills(skills(tmp.path()))
        .with_messages(history.clone())
        .with_decision_model(model(&mock));
    let agent = run(agent, "fill in the PDF").await;
    let stored_before_second: Vec<Message> = agent
        .messages()
        .iter()
        .filter_map(|m| m.as_llm().cloned())
        .collect();
    let agent = run(agent, "cut the release").await;

    let sent = sent(&seen);
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].system, sent[1].system, "identical system prompts");
    let h = serde_json::to_value(&history).unwrap();
    let prefix = |s: &Sent, n: usize| serde_json::to_value(&s.messages[..n]).unwrap();
    assert_eq!(prefix(&sent[0], 2), h);
    assert_eq!(prefix(&sent[1], 2), h);
    // Everything before the second request's latest user turn is exactly the
    // stored history — no earlier hint survives into it.
    let n = sent[1].messages.len() - 1;
    assert_eq!(
        prefix(&sent[1], n),
        serde_json::to_value(&stored_before_second).unwrap()
    );
    // Positive control: each request did carry its own, different hint.
    assert!(sent[0].last_user_text().contains("pdf-fill"));
    assert!(sent[1].last_user_text().contains("git-release"));
    assert!(!sent[1].last_user_text().contains("pdf-fill"));
    drop(agent);
}

#[tokio::test]
async fn skill_hint_stays_silent_when_not_confident() {
    for (pick, needed) in [("pdf-fill", 0.1), ("none", 0.9)] {
        let tmp = tempfile::tempdir().unwrap();
        let mock = advisory_answers(pick, needed);
        let (provider, seen) = recording(MockProvider::text("ok"));
        let agent = Agent::from_provider(provider, ModelConfig::mock())
            .with_skills(skills(tmp.path()))
            .with_decision_model(model(&mock));
        run(agent, "explain what a monad is").await;
        assert_eq!(mock.request_count(), 1, "asked");
        assert_eq!(
            sent(&seen)[0].last_user_text(),
            "explain what a monad is",
            "{pick}/{needed}: no note"
        );
    }
}

// ---------------------------------------------------------------------------
// Advisory: tool hint
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tool_hint_names_the_top_tools_and_removes_none() {
    let mock = advisory_answers("none", 0.0);
    let (provider, seen) = recording(MockProvider::text("ok"));
    let (tools, _) = make_tools(45);
    let agent = Agent::from_provider(provider, ModelConfig::mock())
        .with_system_prompt("Base.")
        .with_tools(tools)
        .with_decision_model(model(&mock));
    run(agent, "do the thing").await;

    let s = &sent(&seen)[0];
    assert_eq!(s.tools, 45, "every tool still offered");
    assert_eq!(s.system, "Base.");
    assert_eq!(
        s.last_user_text(),
        "do the thing|Tools likely relevant to the current request: tool_7, tool_3, tool_9. \
         This is a hint only; every tool remains available."
    );
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 1);
    let ids: Vec<&str> = reqs[0]
        .questions
        .iter()
        .map(|(id, _)| id.as_str())
        .collect();
    assert_eq!(ids, ["tools"], "no skills, so no skill questions");
    assert_eq!(reqs[0].get("tools").unwrap().options().unwrap().len(), 45);
}

#[tokio::test]
async fn tool_hint_threshold_is_configurable() {
    let mock = advisory_answers("none", 0.0);
    let (provider, seen) = recording(MockProvider::text("ok"));
    let (tools, _) = make_tools(12);
    let agent = Agent::from_provider(provider, ModelConfig::mock())
        .with_tools(tools)
        .with_decision_advisory(
            Advisory::new(model(&mock))
                .with_tool_hint_min_tools(10)
                .with_max_tool_hints(1),
        );
    run(agent, "do the thing").await;
    assert_eq!(mock.request_count(), 1);
    assert!(sent(&seen)[0]
        .last_user_text()
        .contains("Tools likely relevant to the current request: tool_7. "));
}

#[test]
#[should_panic(expected = "must be a probability")]
fn advisory_thresholds_must_be_probabilities() {
    let _ = Advisory::new(DecisionModel::local("http://x")).with_skill_need_threshold(f64::NAN);
}

// ---------------------------------------------------------------------------
// Advisory fails open
// ---------------------------------------------------------------------------

#[tokio::test]
async fn advisory_errors_timeouts_and_missing_keys_add_nothing_and_are_counted() {
    let tmp = tempfile::tempdir().unwrap();
    // Error.
    let mock = MockBackend::new().push_error(DecisionError::http(500, "down"));
    let (provider, seen) = recording(MockProvider::text("answer"));
    let agent = Agent::from_provider(provider, ModelConfig::mock())
        .with_skills(skills(tmp.path()))
        .with_decision_model(model(&mock));
    let (agent, stats) = run_stats(agent, "fill in the PDF").await;
    assert_eq!(mock.request_count(), 1);
    assert_eq!(sent(&seen)[0].last_user_text(), "fill in the PDF");
    assert!(matches!(
        agent.messages().last(),
        Some(AgentMessage::Llm(Message::Assistant { .. }))
    ));
    assert_eq!((stats.decision.requests, stats.decision.failures), (1, 1));

    // Timeout: bounded by the advisory's limit, not the backend's.
    let (provider, seen) = recording(MockProvider::text("answer"));
    let agent = Agent::from_provider(provider, ModelConfig::mock())
        .with_skills(skills(tmp.path()))
        .with_decision_advisory(
            Advisory::new(DecisionModel::from_backend(Slow, "jev-test"))
                .with_timeout(Duration::from_millis(50)),
        );
    let start = Instant::now();
    let (_, stats) = run_stats(agent, "fill in the PDF").await;
    assert!(start.elapsed() < Duration::from_secs(5));
    assert_eq!(sent(&seen)[0].last_user_text(), "fill in the PDF");
    assert_eq!(
        (stats.decision.failures, stats.decision.timeouts),
        (1, 1),
        "{:?}",
        stats.decision
    );

    // A missing API key fails at first use, is counted, and sends nothing.
    let keyless = DecisionModel::from_backend(
        SystemOneBackend::typesafe()
            .with_base_url("http://127.0.0.1:9")
            .with_api_key_env("YOAGENT_DECISION_TEST_NEVER_SET"),
        "jev-latest",
    );
    let agent = Agent::from_provider(MockProvider::text("answer"), ModelConfig::mock())
        .with_skills(skills(tmp.path()))
        .with_decision_model(keyless);
    let (_, stats) = run_stats(agent, "fill in the PDF").await;
    assert_eq!((stats.decision.requests, stats.decision.failures), (1, 1));
}

// ---------------------------------------------------------------------------
// Nothing is sent when nothing needs asking
// ---------------------------------------------------------------------------

#[tokio::test]
async fn nothing_is_sent_without_skills_and_with_few_tools() {
    let mock = MockBackend::neutral();
    let (provider, seen) = recording(calls_rm());
    let (tools, ran) = make_tools(5);
    let agent = Agent::from_provider(provider, ModelConfig::mock())
        .with_system_prompt("Base.")
        .with_tools(tools)
        .with_decision_model(model(&mock));
    let (_, stats) = run_stats(agent, "delete the scratch file").await;
    assert_eq!(mock.request_count(), 0, "no decision request");
    assert!(stats.decision.is_empty());
    assert_eq!(ran.lock().unwrap().len(), 1, "the tool ran: no gate");
    for s in sent(&seen) {
        assert_eq!(s.system, "Base.");
        assert_eq!(s.last_user_text(), "delete the scratch file");
    }

    // Positive control: the same agent with skills does ask.
    let tmp = tempfile::tempdir().unwrap();
    let (tools, _) = make_tools(5);
    let agent = Agent::from_provider(MockProvider::text("ok"), ModelConfig::mock())
        .with_skills(skills(tmp.path()))
        .with_tools(tools)
        .with_decision_model(model(&mock));
    run(agent, "delete the scratch file").await;
    assert_eq!(mock.request_count(), 1);
}

// ---------------------------------------------------------------------------
// Tool gate
// ---------------------------------------------------------------------------

fn gated(mock: &MockBackend) -> (Agent, Ran) {
    let (tools, ran) = make_tools(1);
    let agent = Agent::from_provider(calls_rm(), ModelConfig::mock())
        .with_tools(tools)
        .with_tool_gate(ToolGate::new(model(mock)));
    (agent, ran)
}

#[tokio::test]
async fn gate_allows_a_destructive_call_the_user_asked_for() {
    let mock = gate_answers(0.95, 0.9);
    let (agent, ran) = gated(&mock);
    let agent = run(agent, "delete /tmp/scratch.txt").await;
    assert_eq!(ran.lock().unwrap().len(), 1, "allowed");
    assert!(!tool_result_text(&agent).unwrap().1, "not an error result");
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].state["user_request"], "delete /tmp/scratch.txt");
    assert_eq!(reqs[0].state["tool_call"]["tool"], "rm");
    assert_eq!(
        reqs[0].state["tool_call"]["arguments"],
        json!({"path": "/tmp/scratch.txt"})
    );
    let ids: Vec<&str> = reqs[0]
        .questions
        .iter()
        .map(|(id, _)| id.as_str())
        .collect();
    assert_eq!(ids, ["destructive", "requested"]);
}

#[tokio::test]
async fn gate_allows_a_harmless_call_even_when_unrequested() {
    let mock = gate_answers(0.1, 0.1);
    let (agent, ran) = gated(&mock);
    run(agent, "what's in /tmp?").await;
    assert_eq!(ran.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn gate_denies_a_destructive_unrequested_call() {
    let mock = gate_answers(0.9, 0.2);
    let (agent, ran) = gated(&mock);
    let agent = run(agent, "summarize the README").await;
    assert!(ran.lock().unwrap().is_empty(), "denied: never ran");
    let (text, is_error) = tool_result_text(&agent).unwrap();
    assert!(is_error);
    assert!(text.contains("Tool gate"), "{text}");
    assert!(text.contains("destructive"), "{text}");
    assert!(matches!(
        agent.messages().last(),
        Some(AgentMessage::Llm(Message::Assistant { .. }))
    ));
}

#[tokio::test]
async fn gate_denies_malformed_answers() {
    type Answer = fn() -> Evaluation;
    let cases: [(&str, Answer); 5] = [
        ("NaN", || {
            Evaluation::new("m", DecisionUsage::default())
                .with_answer("destructive", NoulAnswer::new(0.0))
                .with_answer("requested", NoulAnswer::new(f64::NAN))
        }),
        ("negative", || {
            Evaluation::new("m", DecisionUsage::default())
                .with_answer("destructive", NoulAnswer::new(-0.5))
                .with_answer("requested", NoulAnswer::new(1.0))
        }),
        ("wrong kind", || {
            Evaluation::new("m", DecisionUsage::default())
                .with_answer(
                    "destructive",
                    ChoiceAnswer::new([("no", 1.0), ("yes", 0.0)]),
                )
                .with_answer("requested", NoulAnswer::new(1.0))
        }),
        ("missing", || {
            Evaluation::new("m", DecisionUsage::default())
                .with_answer("requested", NoulAnswer::new(1.0))
        }),
        ("NaN confidence", || {
            Evaluation::new("m", DecisionUsage::default())
                .with_answer("destructive", NoulAnswer::new(0.0))
                .with_answer(
                    "requested",
                    NoulAnswer::new(1.0).with_confidence(f64::INFINITY),
                )
        }),
    ];
    for (what, answer) in cases {
        let mock = MockBackend::from_fn(move |_| Ok(answer()));
        let (agent, ran) = gated(&mock);
        let agent = run(agent, "delete /tmp/scratch.txt").await;
        assert!(ran.lock().unwrap().is_empty(), "{what}: must deny");
        let (text, _) = tool_result_text(&agent).unwrap();
        assert!(text.contains("could not be consulted"), "{what}: {text}");
    }
    // Positive control: the same shape, well formed, is allowed.
    let mock = MockBackend::from_fn(|_| {
        Ok(Evaluation::new("m", DecisionUsage::default())
            .with_answer("destructive", NoulAnswer::new(0.0))
            .with_answer("requested", NoulAnswer::new(1.0)))
    });
    let (agent, ran) = gated(&mock);
    run(agent, "delete /tmp/scratch.txt").await;
    assert_eq!(ran.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn gate_fails_closed_on_error() {
    let mock = MockBackend::new().push_error(DecisionError::http(503, "unavailable"));
    let (agent, ran) = gated(&mock);
    let agent = run(agent, "delete /tmp/scratch.txt").await;
    assert!(ran.lock().unwrap().is_empty());
    let (text, is_error) = tool_result_text(&agent).unwrap();
    assert!(is_error);
    assert!(text.contains("could not be consulted"), "{text}");
    assert!(text.contains("fails closed"), "{text}");
}

#[tokio::test]
async fn gate_fails_closed_on_timeout() {
    let (tools, ran) = make_tools(1);
    let agent = Agent::from_provider(calls_rm(), ModelConfig::mock())
        .with_tools(tools)
        .with_tool_gate(
            ToolGate::new(DecisionModel::from_backend(Slow, "jev-test"))
                .with_timeout(Duration::from_millis(50)),
        );
    let start = Instant::now();
    let (agent, stats) = run_stats(agent, "delete /tmp/scratch.txt").await;
    assert!(start.elapsed() < Duration::from_secs(5));
    assert!(ran.lock().unwrap().is_empty());
    assert!(tool_result_text(&agent).unwrap().0.contains("timed out"));
    assert_eq!(stats.decision.timeouts, 1);
}

// --- long arguments --------------------------------------------------------

#[tokio::test]
async fn gate_sees_the_tail_of_a_long_command() {
    let heredoc = format!(
        "cat > notes.txt <<'EOF'\n{}\nEOF\nrm -rf /srv/data",
        "lorem ipsum ".repeat(700)
    );
    assert!(heredoc.len() > 8_000);
    let mock = gate_answers(0.9, 0.1);
    let (tools, ran) = make_tools(1);
    let agent = Agent::from_provider(
        calls_rm_with(json!({"command": heredoc})),
        ModelConfig::mock(),
    )
    .with_tools(tools)
    .with_tool_gate(ToolGate::new(model(&mock)));
    run(agent, "write some notes").await;
    assert!(ran.lock().unwrap().is_empty());
    let shown = mock.requests()[0].state["tool_call"]["arguments"]["command"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(shown.ends_with("rm -rf /srv/data"), "the tail is visible");
    assert!(shown.starts_with("cat > notes.txt"), "the head is visible");
    assert!(shown.contains("[truncated "), "the cut is explicit");
    assert!(shown.len() < heredoc.len());
}

#[tokio::test]
async fn gate_keeps_short_fields_whole_and_shortens_long_content() {
    let content = format!("{}END-OF-FILE", "x".repeat(9 * 1024));
    let mock = gate_answers(0.1, 0.9);
    let (tools, ran) = make_tools(1);
    let agent = Agent::from_provider(
        calls_rm_with(json!({"path": "/etc/app/config.toml", "content": content, "append": false})),
        ModelConfig::mock(),
    )
    .with_tools(tools)
    .with_tool_gate(ToolGate::new(model(&mock)));
    run(agent, "update the config").await;
    assert_eq!(ran.lock().unwrap().len(), 1, "allowed");
    let args = &mock.requests()[0].state["tool_call"]["arguments"];
    assert_eq!(args["path"], "/etc/app/config.toml");
    assert_eq!(args["append"], false);
    let shown = args["content"].as_str().unwrap();
    assert!(shown.ends_with("END-OF-FILE"));
    assert!(shown.contains("[truncated "));
    // Positive control: the executed call still got the full content.
    assert_eq!(
        ran.lock().unwrap()[0]["content"].as_str().unwrap().len(),
        content.len()
    );
}

#[tokio::test]
async fn gate_denies_arguments_too_large_to_read() {
    let fields: serde_json::Map<String, serde_json::Value> = (0..10)
        .map(|i| (format!("f{i}"), json!("y".repeat(5_000))))
        .collect();
    let mock = gate_answers(0.0, 1.0);
    let (tools, ran) = make_tools(1);
    let agent = Agent::from_provider(calls_rm_with(json!(fields)), ModelConfig::mock())
        .with_tools(tools)
        .with_tool_gate(ToolGate::new(model(&mock)));
    let agent = run(agent, "go").await;
    assert!(ran.lock().unwrap().is_empty());
    assert!(tool_result_text(&agent).unwrap().0.contains("too large"));
    assert_eq!(mock.request_count(), 0, "denied before asking");
}

// --- what the user asked -----------------------------------------------------

/// Requested only when the state shows the confirmation of a deletion.
fn confirmation_aware() -> MockBackend {
    MockBackend::from_fn(|req| {
        let ur = req.state["user_request"].as_str().unwrap_or_default();
        let confirmed = ur.contains("Latest user message: yes, go ahead")
            && ur.contains("Should I delete /tmp/scratch.txt?")
            && ur.contains("Earlier user request: clean up the workspace");
        Ok(Evaluation::new("m", DecisionUsage::default())
            .with_answer("destructive", NoulAnswer::new(0.9))
            .with_answer(
                "requested",
                NoulAnswer::new(if confirmed { 0.95 } else { 0.1 }),
            ))
    })
}

#[tokio::test]
async fn a_confirmation_after_a_denial_is_allowed() {
    let mock = confirmation_aware();
    let (tools, ran) = make_tools(1);
    let provider = MockProvider::new(vec![
        rm_call(json!({"path": "/tmp/scratch.txt"})),
        MockResponse::Text("The gate blocked that. Should I delete /tmp/scratch.txt?".into()),
        rm_call(json!({"path": "/tmp/scratch.txt"})),
        MockResponse::Text("Deleted.".into()),
    ]);
    let agent = Agent::from_provider(provider, ModelConfig::mock())
        .with_tools(tools)
        .with_tool_gate(ToolGate::new(model(&mock)));
    let agent = run(agent, "clean up the workspace").await;
    assert!(ran.lock().unwrap().is_empty(), "first attempt denied");
    let agent = run(agent, "yes, go ahead").await;
    assert_eq!(ran.lock().unwrap().len(), 1, "confirmed attempt allowed");
    let results = tool_results(&agent);
    assert!(results[0].1 && !results[1].1, "{results:?}");
    let reqs = mock.requests();
    assert_eq!(reqs[0].state["user_request"], "clean up the workspace");
}

#[tokio::test]
async fn a_loop_nudge_does_not_become_the_user_request() {
    let mock = gate_answers(0.1, 0.9);
    let (tools, _) = make_tools(1);
    let same = json!({"path": "/tmp/a"});
    let provider = MockProvider::new(vec![
        rm_call(same.clone()),
        rm_call(same.clone()),
        rm_call(same),
        rm_call(json!({"path": "/tmp/b"})),
        MockResponse::Text("done".into()),
    ]);
    let agent = Agent::from_provider(provider, ModelConfig::mock())
        .with_tools(tools)
        .with_tool_gate(ToolGate::new(model(&mock)));
    let agent = run(agent, "tidy up the temp directory please").await;
    // Positive control: the loop did inject its nudge before the last call.
    let nudged = agent.messages().iter().any(|m| {
        serde_json::to_string(m)
            .unwrap()
            .contains(yoagent::agent_loop::LOOP_NUDGE_PREFIX)
    });
    assert!(nudged, "loop detection nudged");
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 4);
    for r in &reqs {
        assert_eq!(r.state["user_request"], "tidy up the temp directory please");
    }
}

// --- configuration -------------------------------------------------------

#[tokio::test]
async fn gate_thresholds_and_checks_are_overridable() {
    // A stricter requested threshold turns the allow above into a deny.
    let mock = gate_answers(0.95, 0.9);
    let (tools, ran) = make_tools(1);
    let agent = Agent::from_provider(calls_rm(), ModelConfig::mock())
        .with_tools(tools)
        .with_tool_gate(ToolGate::new(model(&mock)).with_requested_threshold(0.95));
    run(agent, "delete /tmp/scratch.txt").await;
    assert!(ran.lock().unwrap().is_empty());

    // An added check denies on its own.
    let mock = MockBackend::from_fn(|req| {
        let mut eval = Evaluation::new("m", DecisionUsage::default());
        for (id, _) in &req.questions {
            let p = if id == "secrets" { 0.8 } else { 0.0 };
            eval = eval.with_answer(id.clone(), NoulAnswer::new(p));
        }
        Ok(eval)
    });
    let (tools, ran) = make_tools(1);
    let agent = Agent::from_provider(calls_rm(), ModelConfig::mock())
        .with_tools(tools)
        .with_tool_gate(ToolGate::new(model(&mock)).with_check(
            "secrets",
            "Does `tool_call` read or send credentials?",
            0.5,
        ));
    let agent = run(agent, "clean up").await;
    assert!(ran.lock().unwrap().is_empty());
    assert!(tool_result_text(&agent).unwrap().0.contains("`secrets`"));
    assert_eq!(mock.requests()[0].questions.len(), 3);
}

#[test]
#[should_panic(expected = "collides with a built-in question")]
fn a_check_cannot_reuse_a_built_in_id() {
    let _ = ToolGate::new(DecisionModel::local("http://x")).with_check("requested", "q?", 0.5);
}

#[test]
#[should_panic(expected = "used twice")]
fn a_check_id_cannot_repeat() {
    let _ = ToolGate::new(DecisionModel::local("http://x"))
        .with_check("a", "q?", 0.5)
        .with_check("a", "r?", 0.5);
}

#[test]
#[should_panic(expected = "must be a probability")]
fn gate_thresholds_must_be_probabilities() {
    let _ = ToolGate::new(DecisionModel::local("http://x")).with_destructive_threshold(f64::NAN);
}

#[test]
#[should_panic(expected = "must be a probability")]
fn check_thresholds_must_be_probabilities() {
    let _ = ToolGate::new(DecisionModel::local("http://x")).with_check("c", "q?", 1.5);
}

/// Rewrites every call's path.
struct Sandbox;

#[async_trait::async_trait]
impl ToolMiddleware for Sandbox {
    async fn before_tool(&self, _call: &ToolCallRequest<'_>) -> ToolDecision {
        ToolDecision::Modify(json!({"path": "/sandbox/scratch.txt"}))
    }
}

#[tokio::test]
async fn gate_runs_after_user_middleware() {
    let mock = gate_answers(0.1, 0.9);
    let (tools, ran) = make_tools(1);
    // The gate is set *before* the user middleware is added, and still runs
    // last.
    let agent = Agent::from_provider(calls_rm(), ModelConfig::mock())
        .with_tools(tools)
        .with_tool_gate(ToolGate::new(model(&mock)))
        .with_tool_middleware(Sandbox);
    run(agent, "delete it").await;
    assert_eq!(
        mock.requests()[0].state["tool_call"]["arguments"],
        json!({"path": "/sandbox/scratch.txt"})
    );
    assert_eq!(
        ran.lock().unwrap()[0],
        json!({"path": "/sandbox/scratch.txt"})
    );
}

#[tokio::test]
async fn sub_agent_gate_denies_its_own_calls() {
    let mock = gate_answers(0.9, 0.1);
    let ran: Ran = Arc::new(Mutex::new(Vec::new()));
    let sub = SubAgentTool::from_provider("helper", Arc::new(calls_rm()), ModelConfig::mock())
        .with_tools(vec![Arc::new(Named {
            name: "rm".into(),
            ran: ran.clone(),
        })])
        .with_tool_gate(ToolGate::new(model(&mock)));
    let result = sub
        .execute(
            json!({"task": "tidy up"}),
            ToolContext::new("call-1", "helper"),
        )
        .await;
    assert!(result.is_ok(), "{result:?}");
    assert!(
        ran.lock().unwrap().is_empty(),
        "the sub-agent's rm was denied"
    );
    assert_eq!(mock.request_count(), 1);
    // In a sub-agent, the "user request" is the parent model's task text.
    assert_eq!(mock.requests()[0].state["user_request"], "tidy up");
}

// ---------------------------------------------------------------------------
// Spend
// ---------------------------------------------------------------------------

#[tokio::test]
async fn advisory_and_gate_spend_is_reported_in_session_stats() {
    let tmp = tempfile::tempdir().unwrap();
    // Answers both the advisory and the gate questions.
    let mock = MockBackend::from_fn(|req| {
        let mut eval = Evaluation::new("jev-test", DecisionUsage::new(1_000, 0));
        for (id, q) in &req.questions {
            eval = match q.kind() {
                QuestionKind::Choice => {
                    let opts = q.options().unwrap();
                    let p = 1.0 / opts.len() as f64;
                    eval.with_answer(
                        id.clone(),
                        ChoiceAnswer::new(opts.iter().map(|o| (o.to_string(), p))),
                    )
                }
                _ => eval.with_answer(
                    id.clone(),
                    NoulAnswer::new(if id == "requested" { 1.0 } else { 0.0 }),
                ),
            };
        }
        Ok(eval)
    });
    let priced = model(&mock).with_cost(Some(CostConfig::new(1.0, 0.0)));
    let (tools, ran) = make_tools(1);
    let agent = Agent::from_provider(calls_rm(), ModelConfig::mock())
        .with_skills(skills(tmp.path()))
        .with_tools(tools)
        .with_decision_model(priced.clone())
        .with_tool_gate(ToolGate::new(priced));
    let (agent, stats) = run_stats(agent, "delete /tmp/scratch.txt").await;
    assert_eq!(ran.lock().unwrap().len(), 1);
    let d = &stats.decision;
    assert_eq!(d.requests, 2, "one advisory + one gate request: {d:?}");
    assert_eq!(d.failures, 0);
    assert_eq!(d.usage.input, 2_000);
    let cost = d.cost_usd.expect("priced");
    assert!((cost - 0.002).abs() < 1e-12, "{cost}");
    // Part of the whole bill (the mock LLM spent nothing).
    assert_eq!(stats.total_cost_usd(), Some(cost));
    assert_eq!(agent.total_cost_usd(), Some(cost));

    // Unpriced decision spend makes the bill unknown, never low.
    let (tools, _) = make_tools(1);
    let agent = Agent::from_provider(calls_rm(), ModelConfig::mock())
        .with_tools(tools)
        .with_tool_gate(ToolGate::new(model(&gate_answers(0.0, 1.0))));
    let (_, stats) = run_stats(agent, "delete /tmp/scratch.txt").await;
    assert!(stats.decision.is_unpriced());
    assert_eq!(stats.total_cost_usd(), None);
}
