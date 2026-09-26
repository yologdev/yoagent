//! Decision models inside the agent: the advisory skill and tool hints, the
//! tool gate (allow, deny, fail-closed), and that nothing is sent when
//! nothing needs asking. All driven by `MockBackend` — no network.

use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use yoagent::decision::*;
use yoagent::provider::mock::*;
use yoagent::provider::{
    MockProvider, ModelConfig, ProviderError, StreamConfig, StreamEvent, StreamProvider,
};
use yoagent::skills::SkillSet;
use yoagent::*;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

type Seen = Arc<Mutex<Vec<(String, usize)>>>;

/// Records (system prompt, tool count) per request, then delegates.
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
        self.seen
            .lock()
            .unwrap()
            .push((config.system_prompt.clone(), config.tools.len()));
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

/// The model calls `rm` once, then answers.
fn calls_rm() -> MockProvider {
    MockProvider::new(vec![
        MockResponse::ToolCalls(vec![MockToolCall {
            provider_metadata: None,
            name: "rm".into(),
            arguments: json!({"path": "/tmp/scratch.txt"}),
        }]),
        MockResponse::Text("done".into()),
    ])
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

async fn run(mut agent: Agent, prompt: &str) -> Agent {
    let mut rx = agent.prompt(prompt).await;
    while rx.recv().await.is_some() {}
    agent.finish().await;
    agent
}

fn tool_result_text(agent: &Agent) -> Option<(String, bool)> {
    agent.messages().iter().find_map(|m| match m {
        AgentMessage::Llm(Message::ToolResult {
            content, is_error, ..
        }) => match content.first() {
            Some(Content::Text { text }) => Some((text.clone(), *is_error)),
            _ => None,
        },
        _ => None,
    })
}

/// Answers the advisory questions: `skill` picked with `pick`, `skill_needed`
/// at `needed`, `tools` concentrated on the first three tools.
fn advisory_answers(pick: &'static str, needed: f64) -> MockBackend {
    MockBackend::from_fn(move |req| {
        let mut eval = Evaluation::new("jev-test", DecisionUsage::new(100, 0));
        for (id, q) in &req.questions {
            match id.as_str() {
                "skill" => {
                    let opts = q.options().unwrap();
                    let rest = 0.1 / (opts.len() - 1) as f64;
                    eval = eval.with_answer(
                        id.clone(),
                        ChoiceAnswer::new(
                            opts.iter()
                                .map(|o| (o.to_string(), if *o == pick { 0.9 } else { rest })),
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

/// Answers the gate's questions with fixed probabilities.
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

// ---------------------------------------------------------------------------
// Advisory: skill hint
// ---------------------------------------------------------------------------

const SKILL_LINE: &str = "Relevant to the current request: pdf-fill. Ignore this if it does not \
                          fit what the user actually asked for.";

#[tokio::test]
async fn skill_hint_adds_one_line_and_asks_once_per_request() {
    let tmp = tempfile::tempdir().unwrap();
    let mock = advisory_answers("pdf-fill", 0.9);
    let (provider, seen) = recording(calls_rm());
    let (tools, _) = make_tools(1);
    let agent = Agent::from_provider(provider, ModelConfig::mock())
        .with_system_prompt("Base.")
        .with_skills(skills(tmp.path()))
        .with_tools(tools)
        .with_decision_model(DecisionModel::from_backend(mock.clone(), "jev-test"));
    let agent = run(agent, "fill in the tax form PDF").await;

    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "tool turn + final turn");
        for (prompt, _) in seen.iter() {
            assert!(prompt.starts_with("Base.\n\n"), "{prompt}");
            assert!(prompt.ends_with(SKILL_LINE), "{prompt}");
            assert_eq!(prompt.matches("Relevant to the current request").count(), 1);
        }
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
    // Transient: the stored prompt is untouched.
    assert!(!agent.system_prompt.contains("Relevant to"));

    // A new user request asks again.
    let agent = run(agent, "cut the 1.0 release").await;
    assert_eq!(mock.request_count(), 2);
    drop(agent);
}

#[tokio::test]
async fn skill_hint_stays_silent_when_not_confident() {
    for (pick, needed) in [("pdf-fill", 0.1), ("none", 0.9)] {
        let tmp = tempfile::tempdir().unwrap();
        let mock = advisory_answers(pick, needed);
        let (provider, seen) = recording(MockProvider::text("ok"));
        let agent = Agent::from_provider(provider, ModelConfig::mock())
            .with_system_prompt("Base.")
            .with_skills(skills(tmp.path()))
            .with_decision_model(DecisionModel::from_backend(mock.clone(), "jev-test"));
        let base = agent.system_prompt.clone();
        run(agent, "explain what a monad is").await;
        assert_eq!(mock.request_count(), 1, "asked");
        assert_eq!(seen.lock().unwrap()[0].0, base, "{pick}/{needed}: no line");
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
        .with_decision_model(DecisionModel::from_backend(mock.clone(), "jev-test"));
    run(agent, "do the thing").await;

    let seen = seen.lock().unwrap();
    assert_eq!(seen[0].1, 45, "every tool still offered");
    assert_eq!(
        seen[0].0,
        "Base.\n\nTools likely relevant to the current request: tool_7, tool_3, tool_9. \
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
            Advisory::new(DecisionModel::from_backend(mock.clone(), "jev-test"))
                .with_tool_hint_min_tools(10)
                .with_max_tool_hints(1),
        );
    run(agent, "do the thing").await;
    assert_eq!(mock.request_count(), 1);
    assert!(seen.lock().unwrap()[0]
        .0
        .starts_with("Tools likely relevant to the current request: tool_7. "));
}

// ---------------------------------------------------------------------------
// Advisory fails open
// ---------------------------------------------------------------------------

#[tokio::test]
async fn advisory_errors_and_timeouts_add_nothing_and_the_run_continues() {
    let tmp = tempfile::tempdir().unwrap();
    // Error.
    let mock = MockBackend::new().push_error(DecisionError::http(500, "down"));
    let (provider, seen) = recording(MockProvider::text("answer"));
    let agent = Agent::from_provider(provider, ModelConfig::mock())
        .with_system_prompt("Base.")
        .with_skills(skills(tmp.path()))
        .with_decision_model(DecisionModel::from_backend(mock.clone(), "jev-test"));
    let base = agent.system_prompt.clone();
    let agent = run(agent, "fill in the PDF").await;
    assert_eq!(mock.request_count(), 1);
    assert_eq!(seen.lock().unwrap()[0].0, base);
    assert!(matches!(
        agent.messages().last(),
        Some(AgentMessage::Llm(Message::Assistant { .. }))
    ));

    // Timeout: bounded by the advisory's limit, not the backend's.
    let (provider, seen) = recording(MockProvider::text("answer"));
    let agent = Agent::from_provider(provider, ModelConfig::mock())
        .with_system_prompt("Base.")
        .with_skills(skills(tmp.path()))
        .with_decision_advisory(
            Advisory::new(DecisionModel::from_backend(Slow, "jev-test"))
                .with_timeout(Duration::from_millis(50)),
        );
    let base = agent.system_prompt.clone();
    let start = Instant::now();
    run(agent, "fill in the PDF").await;
    assert!(start.elapsed() < Duration::from_secs(5));
    assert_eq!(seen.lock().unwrap()[0].0, base);
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
        .with_decision_model(DecisionModel::from_backend(mock.clone(), "jev-test"));
    run(agent, "delete the scratch file").await;
    assert_eq!(mock.request_count(), 0, "no decision request");
    assert_eq!(ran.lock().unwrap().len(), 1, "the tool ran: no gate");
    assert!(seen.lock().unwrap().iter().all(|(p, _)| p == "Base."));

    // Positive control: the same agent with skills does ask.
    let tmp = tempfile::tempdir().unwrap();
    let (tools, _) = make_tools(5);
    let agent = Agent::from_provider(MockProvider::text("ok"), ModelConfig::mock())
        .with_skills(skills(tmp.path()))
        .with_tools(tools)
        .with_decision_model(DecisionModel::from_backend(mock.clone(), "jev-test"));
    run(agent, "delete the scratch file").await;
    assert_eq!(mock.request_count(), 1);
}

#[tokio::test]
async fn an_env_key_alone_enables_nothing() {
    // No decision model configured: skills and many tools present, yet the
    // request is exactly what it would be without the feature.
    let tmp = tempfile::tempdir().unwrap();
    let (provider, seen) = recording(MockProvider::text("ok"));
    let (tools, _) = make_tools(45);
    let agent = Agent::from_provider(provider, ModelConfig::mock())
        .with_system_prompt("Base.")
        .with_skills(skills(tmp.path()))
        .with_tools(tools);
    let base = agent.system_prompt.clone();
    run(agent, "fill in the PDF").await;
    assert_eq!(seen.lock().unwrap()[0].0, base);
}

// ---------------------------------------------------------------------------
// Tool gate
// ---------------------------------------------------------------------------

fn gated(mock: &MockBackend) -> (Agent, Ran) {
    let (tools, ran) = make_tools(1);
    let agent = Agent::from_provider(calls_rm(), ModelConfig::mock())
        .with_tools(tools)
        .with_decision_model(DecisionModel::from_backend(mock.clone(), "jev-test"))
        .with_tool_gate();
    (agent, ran)
}

#[tokio::test]
async fn gate_allows_a_destructive_call_the_user_asked_for() {
    let mock = gate_answers(0.95, 0.9);
    let (agent, ran) = gated(&mock);
    let agent = run(agent, "delete /tmp/scratch.txt").await;
    assert_eq!(ran.lock().unwrap().len(), 1, "allowed");
    assert!(!tool_result_text(&agent).unwrap().1, "not an error result");
    // One request per call, carrying the user's request and the call.
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
    // The loop continued to the final answer.
    assert!(matches!(
        agent.messages().last(),
        Some(AgentMessage::Llm(Message::Assistant { .. }))
    ));
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
        .with_tool_gate_config(
            ToolGate::new(DecisionModel::from_backend(Slow, "jev-test"))
                .with_timeout(Duration::from_millis(50)),
        );
    let start = Instant::now();
    let agent = run(agent, "delete /tmp/scratch.txt").await;
    assert!(start.elapsed() < Duration::from_secs(5));
    assert!(ran.lock().unwrap().is_empty());
    assert!(tool_result_text(&agent)
        .unwrap()
        .0
        .contains("did not answer within"));
}

#[tokio::test]
async fn gate_without_a_decision_model_denies_everything() {
    let (tools, ran) = make_tools(1);
    let agent = Agent::from_provider(calls_rm(), ModelConfig::mock())
        .with_tools(tools)
        .with_tool_gate();
    let agent = run(agent, "delete /tmp/scratch.txt").await;
    assert!(ran.lock().unwrap().is_empty());
    assert!(tool_result_text(&agent)
        .unwrap()
        .0
        .contains("no decision model is configured"));
}

#[tokio::test]
async fn gate_thresholds_and_checks_are_overridable() {
    // Stricter requested threshold turns the allow above into a deny.
    let mock = gate_answers(0.95, 0.9);
    let (tools, ran) = make_tools(1);
    let agent = Agent::from_provider(calls_rm(), ModelConfig::mock())
        .with_tools(tools)
        .with_tool_gate_config(
            ToolGate::new(DecisionModel::from_backend(mock, "jev-test")).with_thresholds(0.5, 0.95),
        );
    run(agent, "delete /tmp/scratch.txt").await;
    assert!(ran.lock().unwrap().is_empty());

    // An extra check denies on its own.
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
        .with_tool_gate_config(
            ToolGate::new(DecisionModel::from_backend(mock.clone(), "jev-test")).with_check(
                "secrets",
                "Does `tool_call` read or send credentials?",
                0.5,
            ),
        );
    let agent = run(agent, "clean up").await;
    assert!(ran.lock().unwrap().is_empty());
    assert!(tool_result_text(&agent).unwrap().0.contains("`secrets`"));
    assert_eq!(mock.requests()[0].questions.len(), 3);
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
    // The gate is enabled *before* the user middleware is added, and still
    // runs last.
    let agent = Agent::from_provider(calls_rm(), ModelConfig::mock())
        .with_tools(tools)
        .with_decision_model(DecisionModel::from_backend(mock.clone(), "jev-test"))
        .with_tool_gate()
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
        .with_decision_model(DecisionModel::from_backend(mock.clone(), "jev-test"))
        .with_tool_gate();
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
}
