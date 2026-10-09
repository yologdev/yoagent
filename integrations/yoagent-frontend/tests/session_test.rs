//! The session controller, with a scripted model and no plugins.

use std::time::Duration;

use serde_json::json;
use yoagent::provider::mock::{MockResponse, MockToolCall};
use yoagent::provider::{MockProvider, ModelConfig};
use yoagent::Agent;
use yoagent_frontend::{ClientMessage, Connection, ServerMessage, Session, UiRequest};

fn agent(responses: Vec<MockResponse>) -> Agent {
    Agent::from_provider(MockProvider::new(responses), ModelConfig::mock())
}

/// Messages until (and including) the next `RunEnded`.
async fn until_run_ended(connection: &mut Connection) -> Vec<ServerMessage> {
    let mut seen = Vec::new();
    loop {
        let message = tokio::time::timeout(Duration::from_secs(10), connection.messages.recv())
            .await
            .expect("a message in time")
            .expect("the session is open");
        let done = matches!(message, ServerMessage::RunEnded { .. });
        seen.push(message);
        if done {
            return seen;
        }
    }
}

#[tokio::test]
async fn every_frontend_gets_hello_the_run_and_its_end() {
    let (session, driver) = Session::new(true);
    let mut a = session.connect();
    let mut b = session.connect();
    let run = tokio::spawn(driver.run(agent(vec![MockResponse::Text("hello there".into())])));
    session.send(ClientMessage::Prompt { text: "hi".into() });

    for connection in [&mut a, &mut b] {
        let seen = until_run_ended(connection).await;
        assert!(matches!(
            seen[0],
            ServerMessage::Hello { running: false, .. }
        ));
        assert!(matches!(&seen[1], ServerMessage::RunStarted { run: 1, prompt } if prompt == "hi"));
        let text: String = seen
            .iter()
            .filter_map(|m| match m {
                ServerMessage::Event { event, .. } => match &**event {
                    yoagent::AgentEvent::MessageUpdate {
                        delta: yoagent::StreamDelta::Text { delta },
                        ..
                    } => Some(delta.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(text, "hello there");
        assert!(matches!(
            seen.last(),
            Some(ServerMessage::RunEnded {
                run: 1,
                error: None,
                ..
            })
        ));
    }
    session.send(ClientMessage::Quit);
    tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        a.messages.recv().await,
        Some(ServerMessage::Closed)
    ));
}

/// A prompt sent while a run is in progress runs next instead of being lost.
#[tokio::test]
async fn a_prompt_sent_mid_run_runs_next() {
    let (session, driver) = Session::new(true);
    let mut ui = session.connect();
    // The first run calls a tool, so it is still going when the second prompt arrives.
    let agent = agent(vec![
        MockResponse::ToolCalls(vec![MockToolCall {
            name: "missing_tool".into(),
            arguments: json!({}),
            provider_metadata: None,
        }]),
        MockResponse::Text("first".into()),
        MockResponse::Text("second".into()),
    ]);
    let run = tokio::spawn(driver.run(agent));
    session.send(ClientMessage::Prompt { text: "one".into() });
    session.send(ClientMessage::Prompt { text: "two".into() });
    until_run_ended(&mut ui).await;
    let second = until_run_ended(&mut ui).await;
    assert!(second
        .iter()
        .any(|m| matches!(m, ServerMessage::RunStarted { run: 2, prompt } if prompt == "two")));
    session.send(ClientMessage::Quit);
    tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .unwrap()
        .unwrap();
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
    tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn questions_go_to_a_frontend_and_the_first_answer_wins() {
    let (session, _driver) = Session::new(true);
    let mut ui = session.connect();
    assert!(matches!(
        ui.messages.recv().await,
        Some(ServerMessage::Hello { .. })
    ));
    let asking = tokio::spawn({
        let session = session.clone();
        async move {
            session
                .ask(
                    UiRequest::Confirm {
                        title: "Run rm?".into(),
                        message: String::new(),
                    },
                    Duration::from_secs(10),
                )
                .await
        }
    });
    let Some(ServerMessage::UiRequest { id, request }) = ui.messages.recv().await else {
        panic!("a UiRequest")
    };
    assert!(matches!(request, UiRequest::Confirm { .. }));
    session.send(ClientMessage::UiResponse {
        id,
        value: json!(true),
    });
    session.send(ClientMessage::UiResponse {
        id,
        value: json!(false),
    });
    assert_eq!(asking.await.unwrap(), json!(true));
    assert!(
        matches!(ui.messages.recv().await, Some(ServerMessage::UiResolved { id: r }) if r == id)
    );
}

#[tokio::test]
async fn questions_without_a_frontend_or_an_answer_get_the_safe_default() {
    let (session, _driver) = Session::new(true);
    let confirm = UiRequest::Confirm {
        title: "Delete?".into(),
        message: String::new(),
    };
    // Nobody attached: no wait at all.
    assert_eq!(
        session.ask(confirm.clone(), Duration::from_secs(60)).await,
        json!(false)
    );
    // Attached but silent: the timeout, then the default.
    let mut ui = session.connect();
    let answer = session.ask(confirm, Duration::from_millis(50)).await;
    assert_eq!(answer, json!(false));
    let mut kinds = Vec::new();
    while let Ok(m) = ui.messages.try_recv() {
        kinds.push(m);
    }
    assert!(kinds
        .iter()
        .any(|m| matches!(m, ServerMessage::UiResolved { .. })));
}

#[tokio::test]
async fn ui_plugins_are_announced_and_served() {
    let (session, _driver) = Session::new(true);
    let mut ui = session.connect();
    ui.messages.recv().await;
    session.add_ui_plugin(
        yoagent_frontend::UiPlugin {
            name: "links".into(),
            tools: vec!["search".into()],
            panel: false,
        },
        "export function renderTool() {}",
    );
    let Some(ServerMessage::UiPlugins { ui_plugins }) = ui.messages.recv().await else {
        panic!("UiPlugins")
    };
    assert_eq!(ui_plugins[0].name, "links");
    assert!(session
        .ui_plugin_module("links")
        .unwrap()
        .contains("renderTool"));
    session.remove_ui_plugin("links");
    assert!(session.ui_plugin_module("links").is_none());
}
