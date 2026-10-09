//! The browser frontend's server: the page, UI plugin modules, and the
//! protocol over a real WebSocket.

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value as Json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message;
use yoagent::provider::mock::MockResponse;
use yoagent::provider::{MockProvider, ModelConfig};
use yoagent::Agent;
use yoagent_frontend::{web, Session, UiPlugin, UiRequest};

async fn get(addr: std::net::SocketAddr, path: &str) -> String {
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    response
}

#[tokio::test]
async fn a_browser_runs_a_prompt_and_answers_a_question_over_the_websocket() {
    let (session, driver) = Session::new(false);
    let agent = Agent::from_provider(
        MockProvider::new(vec![MockResponse::Text("hi from the agent".into())]),
        ModelConfig::mock(),
    );
    tokio::spawn(driver.run(agent));
    let (addr, server) = web::serve(session.clone(), "127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .unwrap();
    let hello = tokio::time::timeout(Duration::from_secs(10), ws.next())
        .await
        .expect("a frame in time")
        .unwrap()
        .unwrap();
    let hello: Json = serde_json::from_str(hello.to_text().unwrap()).unwrap();
    assert_eq!(hello["type"], "hello");

    ws.send(Message::Text(
        json!({"type": "prompt", "text": "hello"}).to_string().into(),
    ))
    .await
    .unwrap();
    let mut text = String::new();
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(10), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let message: Json = serde_json::from_str(frame.to_text().unwrap()).unwrap();
        if message["type"] == "event" && message["event"]["type"] == "messageUpdate" {
            text.push_str(message["event"]["delta"]["delta"].as_str().unwrap_or_default());
        }
        if message["type"] == "runEnded" {
            break;
        }
    }
    assert_eq!(text, "hi from the agent");

    // A plugin's question reaches the browser; its answer comes back.
    let asking = tokio::spawn({
        let session = session.clone();
        async move {
            session
                .ask(
                    UiRequest::Select {
                        title: "Which?".into(),
                        options: vec!["a".into(), "b".into()],
                    },
                    Duration::from_secs(10),
                )
                .await
        }
    });
    let request: Json = loop {
        let frame = ws.next().await.unwrap().unwrap();
        let message: Json = serde_json::from_str(frame.to_text().unwrap()).unwrap();
        if message["type"] == "uiRequest" {
            break message;
        }
    };
    assert_eq!(request["request"]["kind"], "select");
    ws.send(Message::Text(
        json!({"type": "uiResponse", "id": request["id"], "value": "b"})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    assert_eq!(asking.await.unwrap(), json!("b"));
    server.abort();
}

#[tokio::test]
async fn the_page_and_ui_plugin_modules_are_served() {
    let (session, _driver) = Session::new(false);
    session.add_ui_plugin(
        UiPlugin {
            name: "links".into(),
            tools: vec!["search".into()],
            panel: false,
        },
        "export function renderTool() {}",
    );
    let (addr, server) = web::serve(session, "127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    assert!(get(addr, "/").await.contains("<title>yoagent</title>"));
    assert!(get(addr, "/app.js").await.contains("text/javascript"));
    let module = get(addr, "/ui-plugins/links.js").await;
    assert!(module.contains("200 OK") && module.contains("renderTool"));
    assert!(get(addr, "/ui-plugins/nope.js").await.contains("404"));
    server.abort();
}
