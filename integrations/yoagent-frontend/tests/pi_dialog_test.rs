//! pi's dialogs through the session, with real Node runtimes: a pi
//! extension's `ctx.ui.select` reaches a frontend through `host-ui.ts` and
//! the `ui` service, and the answer decides whether the command runs.
//!
//! Needs `npm ci` in `plugins/` and `../yoagent-rutis/plugins/pi/`; skipped
//! (passing, with `SKIPPED:`) without them unless
//! `YOAGENT_RUTIS_REQUIRE_RUNTIMES=1`.

use std::path::PathBuf;
use std::time::Duration;

use rutis::Ctx;
use serde_json::json;
use yoagent::provider::mock::{MockResponse, MockToolCall};
use yoagent::provider::{MockProvider, ModelConfig};
use yoagent::tools::BashTool;
use yoagent::{Agent, AgentEvent};
use yoagent_frontend::host::{PluginHost, Row};
use yoagent_frontend::{services, ClientMessage, ServerMessage, Session, UiRequest};
use yoagent_rutis::RutisBridge;

const WAIT: Duration = Duration::from_secs(60);

fn here() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn pi_plugins() -> PathBuf {
    here().join("../yoagent-rutis/plugins/pi")
}

fn runtimes_installed() -> bool {
    let installed = pi_plugins().join("node_modules").exists();
    if !installed {
        assert!(
            std::env::var("YOAGENT_RUTIS_REQUIRE_RUNTIMES").as_deref() != Ok("1"),
            "pi's packages are not installed (npm ci in {})",
            pi_plugins().display()
        );
        println!("SKIPPED: npm ci in {} first", pi_plugins().display());
    }
    installed
}

/// A command the pi extension asks about (`rm -r…`), harmless here.
fn dangerous() -> MockResponse {
    MockResponse::ToolCalls(vec![MockToolCall {
        name: "bash".into(),
        arguments: json!({ "command": "rm -rf ./yoagent-frontend-test-nothing-here" }),
        provider_metadata: None,
    }])
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pi_question_is_answered_by_a_frontend_and_decides_the_call() {
    if !runtimes_installed() {
        return;
    }
    let root = Ctx::root().unwrap();
    let (session, driver) = Session::new(true);
    let bridge = RutisBridge::install(&root).unwrap();
    services::provide(&root, &session).unwrap();
    let mut host = PluginHost::builder()
        .node("pi", pi_plugins())
        .share("yoagent")
        .share(services::UI)
        .start(&root)
        .await
        .unwrap();
    host.load([
        Row::new(
            "pi",
            pi_plugins().join("pi-extensions-adapter.ts").display(),
        )
        .runtime("pi")
        .config(json!({
            "extensions": [here().join("plugins/pi-extensions/confirm-dangerous.ts")],
            "cwd": std::env::temp_dir(),
        })),
        Row::new("pi-host-ui", pi_plugins().join("host-ui.ts").display()).runtime("pi"),
    ])
    .await
    .unwrap();
    let registry = bridge.registry().clone();
    tokio::time::timeout(WAIT, async {
        while !registry
            .handlers()
            .iter()
            .any(|h| h.name() == "pi-extensions")
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the pi adapter registered");

    let agent = Agent::from_provider(
        MockProvider::new(vec![
            dangerous(),
            MockResponse::Text("ran".into()),
            dangerous(),
            MockResponse::Text("blocked".into()),
        ]),
        ModelConfig::mock(),
    )
    .with_tools(vec![Box::new(BashTool::new())])
    .with_extension(bridge.extension());
    let mut ui = session.connect();
    let run = tokio::spawn(driver.run(agent));

    // The frontend answers the first question Allow, the second Block.
    let mut outcomes = Vec::new();
    for answer in ["Allow", "Block"] {
        session.send(ClientMessage::Prompt {
            text: format!("clean up ({answer})"),
        });
        let mut asked = false;
        loop {
            let message = tokio::time::timeout(WAIT, ui.messages.recv())
                .await
                .expect("a message in time")
                .expect("the session is open");
            match message {
                ServerMessage::UiRequest {
                    id,
                    request: UiRequest::Select { title, options, .. },
                } => {
                    assert!(title.contains("rm -rf"), "{title}");
                    assert_eq!(options, ["Allow", "Block"]);
                    asked = true;
                    assert!(session.send(ClientMessage::UiResponse {
                        id,
                        value: json!(answer),
                    }));
                }
                ServerMessage::Event { event, .. } => {
                    if let AgentEvent::ToolExecutionEnd {
                        tool_name,
                        is_error,
                        result,
                        ..
                    } = *event
                    {
                        assert_eq!(tool_name, "bash");
                        outcomes.push((is_error, format!("{:?}", result.content)));
                    }
                }
                ServerMessage::RunEnd { .. } => break,
                _ => {}
            }
        }
        assert!(asked, "pi asked through the frontend ({answer})");
    }
    assert_eq!(outcomes.len(), 2);
    assert!(!outcomes[0].0, "Allow ran the command: {}", outcomes[0].1);
    assert!(outcomes[1].0, "Block stopped it");
    assert!(
        outcomes[1].1.contains("blocked"),
        "with pi's reason: {}",
        outcomes[1].1
    );

    session.send(ClientMessage::Quit);
    tokio::time::timeout(WAIT, run).await.unwrap().unwrap();
    root.shutdown().await.unwrap();
}
