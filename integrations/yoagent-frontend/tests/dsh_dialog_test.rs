//! DSH's dialogs through the session, with a real DSH runtime: an approval a
//! DSH tool policy asks for and `ask_user_question` reach a frontend through
//! `host-dialogs.ts` and the `ui` service; a DSH tool's presenters reach the
//! frontend as `details.view`.
//!
//! Needs `npm ci` in `../yoagent-rutis/plugins/dsh/`; skipped (passing, with
//! `SKIPPED:`) without it unless `YOAGENT_RUTIS_REQUIRE_RUNTIMES=1`.

use std::path::PathBuf;
use std::time::Duration;

use rutis::Ctx;
use serde_json::{json, Value};
use yoagent::provider::mock::{MockResponse, MockToolCall};
use yoagent::provider::{MockProvider, ModelConfig};
use yoagent::{Agent, AgentEvent};
use yoagent_frontend::host::{PluginHost, Row};
use yoagent_frontend::{
    services, ClientMessage, ResolveReason, RunOutcome, ServerMessage, Session, UiRequest,
};
use yoagent_rutis::RutisBridge;

const WAIT: Duration = Duration::from_secs(60);

fn dsh_plugins() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../yoagent-rutis/plugins/dsh")
}

fn runtime_installed() -> bool {
    let installed = dsh_plugins()
        .join("node_modules/@deepseek-ai/dsh-tool-ask-user")
        .exists();
    if !installed {
        assert!(
            std::env::var("YOAGENT_RUTIS_REQUIRE_RUNTIMES").as_deref() != Ok("1"),
            "DSH's packages are not installed (npm ci in {})",
            dsh_plugins().display()
        );
        println!("SKIPPED: npm ci in {} first", dsh_plugins().display());
    }
    installed
}

fn call(name: &str, arguments: Value) -> MockResponse {
    MockResponse::ToolCalls(vec![MockToolCall {
        name: name.into(),
        arguments,
        provider_metadata: None,
    }])
}

/// A root with the bridge, the session's services and the DSH rows loaded.
async fn dsh_host() -> (
    Ctx,
    Session,
    yoagent_frontend::Driver,
    RutisBridge,
    PluginHost,
) {
    let root = Ctx::root().unwrap();
    let (session, driver) = Session::new(true);
    let bridge = RutisBridge::install(&root).unwrap();
    services::provide(&root, &session).unwrap();
    let mut host = PluginHost::builder()
        .node("dsh", dsh_plugins())
        .share("yoagent")
        .share(services::UI)
        .start(&root)
        .await
        .unwrap();
    let dsh = |id: &str, name: &str| Row::new(id, name).runtime("dsh");
    let file =
        |id: &str, file: &str| Row::new(id, dsh_plugins().join(file).display()).runtime("dsh");
    host.load([
        dsh("system-prompt", "@deepseek-ai/dsh-system-prompt"),
        dsh("tools", "@deepseek-ai/dsh-tools"),
        dsh("questions", "@deepseek-ai/dsh-user-questions"),
        dsh("ask-user", "@deepseek-ai/dsh-tool-ask-user"),
        file("fixture", "fixture-tools.ts"),
        file("adapter", "dsh-tools-adapter.ts"),
        file("host-dialogs", "host-dialogs.ts"),
    ])
    .await
    .unwrap();
    let registry = bridge.registry().clone();
    tokio::time::timeout(WAIT, async {
        while !registry.handlers().iter().any(|h| h.name() == "dsh-tools") {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the DSH adapter registered");

    (root, session, driver, bridge, host)
}

#[tokio::test(flavor = "multi_thread")]
async fn dsh_approvals_and_questions_reach_a_frontend() {
    if !runtime_installed() {
        return;
    }
    let (root, session, driver, bridge, _host) = dsh_host().await;
    let agent = Agent::from_provider(
        MockProvider::new(vec![
            call("fixture_guarded", json!({})),
            call("fixture_guarded", json!({})),
            call(
                "ask_user_question",
                json!({ "questions": [{ "id": "colour", "question": "Which colour?", "options": [{ "label": "red" }, { "label": "blue" }] }] }),
            ),
            call("fixture_echo", json!({ "text": "hi" })),
            MockResponse::Text("done".into()),
        ]),
        ModelConfig::mock(),
    )
    .with_extension(bridge.extension());
    let mut ui = session.connect();
    let run = tokio::spawn(driver.run(agent));
    session.send(ClientMessage::Prompt { text: "go".into() });

    // The frontend: yes to the first approval, no to the second, "blue".
    let mut confirms = vec![true, false].into_iter();
    let mut asked = Vec::new();
    let mut ends = Vec::new();
    loop {
        let message = tokio::time::timeout(WAIT, ui.messages.recv())
            .await
            .expect("a message in time")
            .expect("the session is open");
        match message {
            ServerMessage::UiRequest { id, request } => {
                let value = match &request {
                    UiRequest::Confirm { .. } => json!(confirms.next().unwrap()),
                    UiRequest::Select { .. } => json!("blue"),
                    other => panic!("not asked: {other:?}"),
                };
                asked.push(request);
                assert!(session.send(ClientMessage::UiResponse { id, value }));
            }
            ServerMessage::Event { event, .. } => {
                if let AgentEvent::ToolExecutionEnd {
                    tool_name,
                    is_error,
                    result,
                    ..
                } = *event
                {
                    ends.push((
                        tool_name,
                        is_error,
                        format!("{:?}", result.content),
                        result.details,
                    ));
                }
            }
            ServerMessage::RunEnd { .. } => break,
            _ => {}
        }
    }
    assert!(
        matches!(&asked[0], UiRequest::Confirm { title, message, .. } if title.contains("fixture_guarded") && message.contains("needs a yes")),
        "{asked:?}"
    );
    assert!(!ends[0].1, "yes ran it: {:?}", ends[0]);
    assert!(
        ends[1].1 && ends[1].2.contains("was not approved"),
        "no denied it: {:?}",
        ends[1]
    );
    assert!(
        matches!(&asked[2], UiRequest::Select { title, options, .. } if title == "Which colour?" && options[..2] == ["red", "blue"]),
        "{asked:?}"
    );
    assert!(
        ends[2].2.contains(r#"\"selected\":[\"blue\"]"#),
        "{:?}",
        ends[2]
    );
    assert_eq!(
        ends[3].3["view"]["call"]["title"], "echo hi",
        "{:?}",
        ends[3]
    );

    session.send(ClientMessage::Quit);
    tokio::time::timeout(WAIT, run).await.unwrap().unwrap();
    root.shutdown().await.unwrap();
}

/// A run stopped while its approval is open withdraws the question: the
/// frontend closes it, the run ends aborted, and the tool never ran.
#[tokio::test(flavor = "multi_thread")]
async fn stopping_a_run_withdraws_its_dsh_approval() {
    if !runtime_installed() {
        return;
    }
    let (root, session, driver, bridge, _host) = dsh_host().await;
    let agent = Agent::from_provider(
        MockProvider::new(vec![
            call("fixture_guarded", json!({})),
            MockResponse::Text("never".into()),
        ]),
        ModelConfig::mock(),
    )
    .with_extension(bridge.extension());
    let mut ui = session.connect();
    let run = tokio::spawn(driver.run(agent));
    session.send(ClientMessage::Prompt { text: "go".into() });
    let mut asked = None;
    let mut withdrawn = false;
    let mut ran = false;
    let outcome = loop {
        let message = tokio::time::timeout(WAIT, ui.messages.recv())
            .await
            .expect("a message in time")
            .expect("the session is open");
        match message {
            ServerMessage::UiRequest { id, .. } => {
                asked = Some(id);
                // Nobody answers: the user stops the run instead.
                session.send(ClientMessage::Abort);
            }
            ServerMessage::UiResolved { id, reason } => {
                assert_eq!(Some(id), asked);
                assert_eq!(reason, ResolveReason::Withdrawn);
                withdrawn = true;
            }
            ServerMessage::Event { event, .. } => {
                if let AgentEvent::ToolExecutionEnd { result, .. } = *event {
                    ran |= format!("{:?}", result.content).contains("guarded ran");
                }
            }
            ServerMessage::RunEnd { outcome, .. } => break outcome,
            _ => {}
        }
    };
    assert!(asked.is_some(), "the approval was asked");
    assert_eq!(outcome, RunOutcome::Aborted);
    assert!(!ran, "the tool never ran");
    // The withdrawal may arrive just after the run's end.
    if !withdrawn {
        let resolved = tokio::time::timeout(WAIT, async {
            loop {
                if let Some(ServerMessage::UiResolved { reason, .. }) = ui.messages.recv().await {
                    return reason;
                }
            }
        })
        .await
        .expect("the question was closed");
        assert_eq!(resolved, ResolveReason::Withdrawn);
    }
    session.send(ClientMessage::Quit);
    tokio::time::timeout(WAIT, run).await.unwrap().unwrap();
    root.shutdown().await.unwrap();
}
