//! The session controller, with a scripted model and no plugins.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio::sync::{mpsc, Semaphore};
use tokio_util::sync::CancellationToken;
use yoagent::provider::mock::{MockResponse, MockToolCall};
use yoagent::provider::{
    MockProvider, ModelConfig, ProviderError, StreamConfig, StreamEvent, StreamProvider,
};
use yoagent::{
    Agent, AgentEvent, AgentTool, FilterResult, InputFilter, Message, ToolContext, ToolError,
    ToolResult, Usage,
};
use yoagent_frontend::{
    ClientMessage, Connection, NoticeLevel, ResolveReason, RunOutcome, ServerMessage, Session,
    UiPlugin, UiRequest,
};

const WAIT: Duration = Duration::from_secs(10);

fn agent(responses: Vec<MockResponse>) -> Agent {
    Agent::from_provider(MockProvider::new(responses), ModelConfig::mock())
}

/// A tool that runs until the test opens the gate (or the run is cancelled),
/// so a run is reliably in progress while the test acts on it.
struct Gate(Arc<Semaphore>);

#[async_trait::async_trait]
impl AgentTool for Gate {
    fn name(&self) -> &str {
        "gate"
    }
    fn label(&self) -> &str {
        "gate"
    }
    fn description(&self) -> &str {
        "waits for the test"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({"type": "object"})
    }
    async fn execute(
        &self,
        _params: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        tokio::select! {
            permit = self.0.acquire() => {
                permit.unwrap().forget();
                Ok(ToolResult { content: vec![yoagent::Content::Text { text: "opened".into() }], details: json!({}) })
            }
            _ = ctx.cancel.cancelled() => Err(ToolError::Cancelled),
        }
    }
}

fn gate_call() -> MockResponse {
    MockResponse::ToolCalls(vec![MockToolCall {
        name: "gate".into(),
        arguments: json!({}),
        provider_metadata: None,
    }])
}

/// An agent whose first response calls the gate, then `rest`.
fn gated(rest: Vec<MockResponse>) -> (Agent, Arc<Semaphore>) {
    let gate = Arc::new(Semaphore::new(0));
    let mut responses = vec![gate_call()];
    responses.extend(rest);
    let agent = agent(responses).with_tools(vec![Box::new(Gate(gate.clone()))]);
    (agent, gate)
}

async fn next(connection: &mut Connection) -> ServerMessage {
    tokio::time::timeout(WAIT, connection.messages.recv())
        .await
        .expect("a message in time")
        .expect("the session is open")
}

/// Messages until (and including) the first that matches.
async fn until(
    connection: &mut Connection,
    done: impl Fn(&ServerMessage) -> bool,
) -> Vec<ServerMessage> {
    let mut seen = Vec::new();
    loop {
        let message = next(connection).await;
        let stop = done(&message);
        seen.push(message);
        if stop {
            return seen;
        }
    }
}

async fn until_run_end(connection: &mut Connection) -> Vec<ServerMessage> {
    until(connection, |m| matches!(m, ServerMessage::RunEnd { .. })).await
}

/// Until the gate tool runs.
async fn until_gated(connection: &mut Connection) {
    until(connection, |m| {
        matches!(m, ServerMessage::Event { event, .. }
            if matches!(&**event, AgentEvent::ToolExecutionStart { tool_name, .. } if tool_name == "gate"))
    })
    .await;
}

fn outcome(message: Option<&ServerMessage>) -> (RunOutcome, Option<String>) {
    match message {
        Some(ServerMessage::RunEnd { outcome, error, .. }) => (*outcome, error.clone()),
        other => panic!("a RunEnd, got {other:?}"),
    }
}

async fn finished(run: tokio::task::JoinHandle<Agent>) -> Agent {
    tokio::time::timeout(WAIT, run).await.unwrap().unwrap()
}

#[tokio::test]
async fn every_frontend_gets_hello_the_run_and_its_end() {
    let (session, driver) = Session::new(true);
    let mut a = session.connect();
    let mut b = session.connect();
    let run = tokio::spawn(driver.run(agent(vec![MockResponse::Text("hello there".into())])));
    assert!(session.send(ClientMessage::Prompt { text: "hi".into() }));

    for connection in [&mut a, &mut b] {
        let seen = until_run_end(connection).await;
        assert!(matches!(
            seen[0],
            ServerMessage::Hello { running: false, .. }
        ));
        assert!(matches!(&seen[1], ServerMessage::RunStart { run: 1, prompt } if prompt == "hi"));
        let text: String = seen
            .iter()
            .filter_map(|m| match m {
                ServerMessage::Event { event, .. } => match &**event {
                    AgentEvent::MessageUpdate {
                        delta: yoagent::StreamDelta::Text { delta },
                        ..
                    } => Some(delta.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(text, "hello there");
        assert_eq!(outcome(seen.last()), (RunOutcome::Completed, None));
    }
    session.send(ClientMessage::Quit);
    finished(run).await;
    assert!(matches!(next(&mut a).await, ServerMessage::Closed));
    assert!(session.is_closed());
    assert!(
        !session.send(ClientMessage::Prompt {
            text: "late".into()
        }),
        "an ended session says so"
    );
}

/// A prompt sent while a run is in progress runs next instead of being lost.
#[tokio::test]
async fn a_prompt_sent_mid_run_runs_next() {
    let (session, driver) = Session::new(true);
    let mut ui = session.connect();
    let (agent, gate) = gated(vec![
        MockResponse::Text("first".into()),
        MockResponse::Text("second".into()),
    ]);
    let run = tokio::spawn(driver.run(agent));
    session.send(ClientMessage::Prompt { text: "one".into() });
    until_gated(&mut ui).await;
    session.send(ClientMessage::Prompt { text: "two".into() });
    gate.add_permits(1);
    until_run_end(&mut ui).await;
    let second = until_run_end(&mut ui).await;
    assert!(matches!(&second[0], ServerMessage::RunStart { run: 2, prompt } if prompt == "two"));
    assert_eq!(outcome(second.last()).0, RunOutcome::Completed);
    session.send(ClientMessage::Quit);
    finished(run).await;
}

/// Steering or a follow-up with no run in progress starts one.
#[tokio::test]
async fn steering_when_idle_starts_a_run() {
    let (session, driver) = Session::new(true);
    let mut ui = session.connect();
    let run = tokio::spawn(driver.run(agent(vec![
        MockResponse::Text("a".into()),
        MockResponse::Text("b".into()),
    ])));
    session.send(ClientMessage::Steer { text: "go".into() });
    let seen = until_run_end(&mut ui).await;
    assert!(seen
        .iter()
        .any(|m| matches!(m, ServerMessage::RunStart { prompt, .. } if prompt == "go")));
    session.send(ClientMessage::FollowUp {
        text: "more".into(),
    });
    let seen = until_run_end(&mut ui).await;
    assert!(matches!(&seen[0], ServerMessage::RunStart { run: 2, prompt } if prompt == "more"));
    session.send(ClientMessage::Quit);
    finished(run).await;
}

/// Without `accept_quit` (a server), a frontend's Quit is ignored.
#[tokio::test]
async fn quit_is_ignored_unless_accepted() {
    let (session, driver) = Session::new(false);
    let run = tokio::spawn(driver.run(agent(vec![])));
    session.send(ClientMessage::Quit);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!run.is_finished());
    drop(session);
    finished(run).await;
}

/// Quitting mid-run stops the run: it ends aborted, then the session closes.
#[tokio::test]
async fn quit_mid_run_ends_the_run_then_the_session() {
    let (session, driver) = Session::new(true);
    let mut ui = session.connect();
    let (agent, _gate) = gated(vec![MockResponse::Text("never".into())]);
    let run = tokio::spawn(driver.run(agent));
    session.send(ClientMessage::Prompt { text: "one".into() });
    until_gated(&mut ui).await;
    session.send(ClientMessage::Quit);
    let seen = until_run_end(&mut ui).await;
    assert_eq!(outcome(seen.last()).0, RunOutcome::Aborted);
    assert!(matches!(next(&mut ui).await, ServerMessage::Closed));
    finished(run).await;
}

/// A model failure is reported as an `error` outcome with its message.
#[tokio::test]
async fn a_failed_run_reports_its_error() {
    let (session, driver) = Session::new(true);
    let mut ui = session.connect();
    let run = tokio::spawn(driver.run(agent(vec![MockResponse::ErrorWithUsage(
        "overloaded".into(),
        Usage::default(),
    )])));
    session.send(ClientMessage::Prompt { text: "hi".into() });
    let (outcome, error) = outcome(until_run_end(&mut ui).await.last());
    assert_eq!(outcome, RunOutcome::Error);
    assert!(error.unwrap().contains("overloaded"));
    session.send(ClientMessage::Quit);
    finished(run).await;
}

struct RejectAll;

impl InputFilter for RejectAll {
    fn filter(&self, _text: &str) -> FilterResult {
        FilterResult::Reject("not today".into())
    }
}

#[tokio::test]
async fn a_rejected_input_is_reported_as_rejected() {
    let (session, driver) = Session::new(true);
    let mut ui = session.connect();
    let agent = agent(vec![MockResponse::Text("never".into())]).with_input_filter(RejectAll);
    let run = tokio::spawn(driver.run(agent));
    session.send(ClientMessage::Prompt { text: "hi".into() });
    let (outcome, error) = outcome(until_run_end(&mut ui).await.last());
    assert_eq!(outcome, RunOutcome::Rejected);
    assert!(error.unwrap().contains("not today"));
    session.send(ClientMessage::Quit);
    finished(run).await;
}

/// A provider that panics: the agent's task fails without an `AgentEnd`.
struct Panics;

#[async_trait::async_trait]
impl StreamProvider for Panics {
    async fn stream(
        &self,
        _config: StreamConfig,
        _tx: mpsc::UnboundedSender<StreamEvent>,
        _cancel: CancellationToken,
    ) -> Result<Message, ProviderError> {
        panic!("the provider panicked")
    }
}

/// A run whose task fails is an error, not a success.
#[tokio::test]
async fn a_run_whose_task_failed_is_an_error() {
    let (session, driver) = Session::new(true);
    let mut ui = session.connect();
    let run = tokio::spawn(driver.run(Agent::from_provider(Panics, ModelConfig::mock())));
    session.send(ClientMessage::Prompt { text: "hi".into() });
    let seen = until_run_end(&mut ui).await;
    assert_eq!(outcome(seen.last()).0, RunOutcome::Error);
    assert!(seen.iter().any(|m| matches!(
        m,
        ServerMessage::Notice {
            level: NoticeLevel::Error,
            ..
        }
    )));
    session.send(ClientMessage::Quit);
    finished(run).await;
}

/// The driver's task going away mid-run still ends the run and the session
/// for every frontend.
#[tokio::test]
async fn a_driver_that_dies_mid_run_ends_the_run_and_the_session() {
    let (session, driver) = Session::new(true);
    let mut ui = session.connect();
    let (agent, _gate) = gated(vec![]);
    let run = tokio::spawn(driver.run(agent));
    session.send(ClientMessage::Prompt { text: "one".into() });
    until_gated(&mut ui).await;
    run.abort();
    let (outcome, error) = outcome(until_run_end(&mut ui).await.last());
    assert_eq!(outcome, RunOutcome::Error);
    assert!(error.is_some());
    assert!(matches!(next(&mut ui).await, ServerMessage::Closed));
    assert!(!session.send(ClientMessage::Prompt { text: "x".into() }));
    // A frontend connecting now learns at once that the session ended.
    let mut late = session.connect();
    assert!(matches!(next(&mut late).await, ServerMessage::Hello { .. }));
    assert!(matches!(next(&mut late).await, ServerMessage::Closed));
}

/// An abort mid-run is reported as such; a reset mid-run also drops the
/// queued prompts and forgets the conversation once the run ends.
#[tokio::test]
async fn abort_and_reset_mid_run_are_reported_and_applied() {
    let (session, driver) = Session::new(true);
    let mut ui = session.connect();
    let gate = Arc::new(Semaphore::new(0));
    // An aborted run makes no further model call: each run takes one response.
    let agent = agent(vec![
        gate_call(),
        gate_call(),
        MockResponse::Text("queued".into()),
    ])
    .with_tools(vec![Box::new(Gate(gate))]);
    let run = tokio::spawn(driver.run(agent));
    session.send(ClientMessage::Prompt { text: "one".into() });
    until_gated(&mut ui).await;
    session.send(ClientMessage::Abort);
    let (outcome_one, error) = outcome(until_run_end(&mut ui).await.last());
    assert_eq!(
        (outcome_one, error.as_deref()),
        (RunOutcome::Aborted, Some("aborted"))
    );

    session.send(ClientMessage::Prompt { text: "two".into() });
    until_gated(&mut ui).await;
    session.send(ClientMessage::Prompt {
        text: "three".into(),
    });
    session.send(ClientMessage::Reset);
    assert_eq!(
        outcome(until_run_end(&mut ui).await.last()).0,
        RunOutcome::Aborted
    );
    session.send(ClientMessage::Quit);
    let rest = until(&mut ui, |m| matches!(m, ServerMessage::Closed)).await;
    assert!(
        !rest
            .iter()
            .any(|m| matches!(m, ServerMessage::RunStart { .. })),
        "the queued prompt was dropped"
    );
    let agent = finished(run).await;
    assert!(
        agent.messages().is_empty(),
        "the reset forgot the conversation"
    );
}

#[tokio::test]
async fn a_reset_when_idle_forgets_the_conversation() {
    let (session, driver) = Session::new(true);
    let mut ui = session.connect();
    let run = tokio::spawn(driver.run(agent(vec![MockResponse::Text("hi".into())])));
    session.send(ClientMessage::Prompt { text: "hi".into() });
    until_run_end(&mut ui).await;
    session.send(ClientMessage::Reset);
    session.send(ClientMessage::Quit);
    assert!(finished(run).await.messages().is_empty());
}

fn confirm(title: &str) -> UiRequest {
    UiRequest::Confirm {
        title: title.into(),
        message: String::new(),
    }
}

fn ask(
    session: &Session,
    request: UiRequest,
    timeout: Duration,
    key: Option<&str>,
) -> tokio::task::JoinHandle<serde_json::Value> {
    let session = session.clone();
    let key = key.map(str::to_owned);
    tokio::spawn(async move { session.ask_keyed(request, timeout, key).await })
}

async fn asked(connection: &mut Connection) -> u64 {
    match next(connection).await {
        ServerMessage::UiRequest { id, .. } => id,
        other => panic!("a UiRequest, got {other:?}"),
    }
}

async fn resolved(connection: &mut Connection) -> (u64, ResolveReason) {
    match next(connection).await {
        ServerMessage::UiResolved { id, reason } => (id, reason),
        other => panic!("a UiResolved, got {other:?}"),
    }
}

/// The first fitting answer wins; one that does not fit the question is
/// ignored and the question stays open.
#[tokio::test]
async fn questions_go_to_a_frontend_and_the_first_fitting_answer_wins() {
    let (session, _driver) = Session::new(true);
    let mut ui = session.connect();
    next(&mut ui).await;
    let asking = ask(&session, confirm("Run rm?"), WAIT, None);
    let id = asked(&mut ui).await;
    for value in [json!("yes"), json!(true), json!(false)] {
        session.send(ClientMessage::UiResponse { id, value });
    }
    assert_eq!(asking.await.unwrap(), json!(true));
    assert_eq!(resolved(&mut ui).await, (id, ResolveReason::Answered));

    let choice = ask(
        &session,
        UiRequest::Select {
            title: "Which?".into(),
            options: vec!["a".into(), "b".into()],
        },
        WAIT,
        None,
    );
    let id = asked(&mut ui).await;
    session.send(ClientMessage::UiResponse {
        id,
        value: json!("c"),
    });
    session.send(ClientMessage::UiResponse {
        id,
        value: json!("b"),
    });
    assert_eq!(choice.await.unwrap(), json!("b"));
}

#[tokio::test]
async fn questions_without_a_frontend_or_an_answer_get_the_safe_default() {
    let (session, _driver) = Session::new(true);
    // Nobody attached: no wait at all.
    assert_eq!(
        session
            .ask(confirm("Delete?"), Duration::from_secs(60))
            .await,
        json!(false)
    );
    // Attached but silent: the timeout, then the default.
    let mut ui = session.connect();
    next(&mut ui).await;
    let answer = session
        .ask(confirm("Delete?"), Duration::from_millis(50))
        .await;
    assert_eq!(answer, json!(false));
    let id = asked(&mut ui).await;
    assert_eq!(resolved(&mut ui).await, (id, ResolveReason::TimedOut));
}

#[tokio::test]
async fn ui_plugins_are_announced_versioned_and_removed() {
    let (session, _driver) = Session::new(true);
    let mut ui = session.connect();
    next(&mut ui).await;
    let plugin = UiPlugin {
        name: "links".into(),
        tools: vec!["search".into()],
        panel: false,
        version: 0,
    };
    let first = session
        .add_ui_plugin(plugin.clone(), "export function renderTool() {}")
        .unwrap();
    let ServerMessage::UiPlugins { ui_plugins } = next(&mut ui).await else {
        panic!("UiPlugins")
    };
    assert_eq!(ui_plugins[0].name, "links");
    assert_eq!(ui_plugins[0].version, first);
    assert!(session
        .ui_plugin_module("links")
        .unwrap()
        .contains("renderTool"));

    // Offered again (the plugin reloaded): a new version replaces it, and the
    // old offer's removal leaves the new one.
    let second = session.add_ui_plugin(plugin.clone(), "v2").unwrap();
    assert!(second > first);
    session.remove_ui_plugin("links", Some(first));
    assert_eq!(session.ui_plugin_module("links").as_deref(), Some("v2"));
    session.remove_ui_plugin("links", None);
    assert!(session.ui_plugin_module("links").is_none());

    let unnamed = UiPlugin {
        name: "".into(),
        ..plugin
    };
    assert!(session.add_ui_plugin(unnamed, "x").is_none());
}

/// A question whose asker goes away (its call cancelled) is withdrawn:
/// frontends close it, and a late joiner never sees it.
#[tokio::test]
async fn a_dropped_question_is_withdrawn() {
    let (session, _driver) = Session::new(true);
    let mut ui = session.connect();
    next(&mut ui).await;
    let asking = ask(&session, confirm("Delete?"), Duration::from_secs(60), None);
    let id = asked(&mut ui).await;
    // A frontend joining now still gets the open question.
    let mut late = session.connect();
    let ServerMessage::Hello { ui_requests, .. } = next(&mut late).await else {
        panic!("Hello")
    };
    assert_eq!(ui_requests.len(), 1);
    assert_eq!(ui_requests[0].id, id);
    asking.abort();
    let _ = asking.await;
    assert_eq!(resolved(&mut ui).await, (id, ResolveReason::Withdrawn));
    let mut later = session.connect();
    let ServerMessage::Hello { ui_requests, .. } = next(&mut later).await else {
        panic!("Hello")
    };
    assert!(ui_requests.is_empty());
}

/// An asker that cannot drop its future (a plugin in another process)
/// withdraws by key: it gets the default, frontends close the question.
#[tokio::test]
async fn a_question_withdrawn_by_key_resolves_to_the_default() {
    let (session, _driver) = Session::new(true);
    let mut ui = session.connect();
    next(&mut ui).await;
    let asking = ask(&session, confirm("Push?"), WAIT, Some("k1"));
    let id = asked(&mut ui).await;
    session.withdraw("unrelated");
    session.withdraw("k1");
    assert_eq!(asking.await.unwrap(), json!(false));
    assert_eq!(resolved(&mut ui).await, (id, ResolveReason::Withdrawn));
}

/// A withdrawal that overtakes its question (both cross a process boundary)
/// still counts: the question is never shown.
#[tokio::test]
async fn a_withdrawal_before_its_question_counts() {
    let (session, _driver) = Session::new(true);
    let mut ui = session.connect();
    next(&mut ui).await;
    session.withdraw("early");
    let answer = session
        .ask_keyed(confirm("Push?"), WAIT, Some("early".into()))
        .await;
    assert_eq!(answer, json!(false));
    assert!(ui.messages.try_recv().is_err(), "never shown");
}

/// A key already in use does not take over the first question's key: a
/// withdrawal reaches the first only, and the second ending leaves it alone.
#[tokio::test]
async fn a_reused_key_withdraws_only_the_first_question() {
    let (session, _driver) = Session::new(true);
    let mut ui = session.connect();
    next(&mut ui).await;
    let first = ask(&session, confirm("One?"), WAIT, Some("k"));
    let first_id = asked(&mut ui).await;
    let second = ask(&session, confirm("Two?"), WAIT, Some("k"));
    let second_id = asked(&mut ui).await;
    session.send(ClientMessage::UiResponse {
        id: second_id,
        value: json!(true),
    });
    assert_eq!(second.await.unwrap(), json!(true));
    assert_eq!(
        resolved(&mut ui).await,
        (second_id, ResolveReason::Answered)
    );
    session.withdraw("k");
    assert_eq!(first.await.unwrap(), json!(false));
    assert_eq!(
        resolved(&mut ui).await,
        (first_id, ResolveReason::Withdrawn)
    );
}
