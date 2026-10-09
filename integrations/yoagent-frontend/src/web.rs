//! The browser frontend: a page, the UI plugins' modules, and the protocol
//! over a WebSocket.
//!
//! - `GET /` — the page (`web/index.html`, `web/app.js`, built in).
//! - `GET /ws` — the WebSocket: each text frame is one JSON message,
//!   [`ClientMessage`] in, [`ServerMessage`](crate::ServerMessage) out.
//! - `GET /ui-plugins/{name}` — a UI plugin's ES module (`{name}` ends in `.js`).
//!
//! No authentication: bind to localhost, or put it behind something that
//! authenticates. Anyone who reaches it can drive the agent.

use std::net::SocketAddr;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures::{SinkExt, StreamExt};

use crate::protocol::ClientMessage;
use crate::session::Session;

const INDEX: &str = include_str!("../web/index.html");
const APP: &str = include_str!("../web/app.js");

/// Serve the browser frontend for `session` on `addr` until the task is
/// dropped or aborted. Returns the bound address (`addr` may use port 0).
pub async fn serve(
    session: Session,
    addr: SocketAddr,
) -> std::io::Result<(SocketAddr, tokio::task::JoinHandle<()>)> {
    let app = Router::new()
        .route("/", get(|| async { Html(INDEX) }))
        .route("/app.js", get(|| async { javascript(APP.to_owned()) }))
        .route("/favicon.ico", get(|| async { StatusCode::NO_CONTENT }))
        .route("/ui-plugins/{name}", get(ui_plugin))
        .route("/ws", get(socket))
        .with_state(session);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    let task = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::warn!("the web frontend stopped: {e}");
        }
    });
    Ok((bound, task))
}

fn javascript(source: String) -> Response {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        source,
    )
        .into_response()
}

async fn ui_plugin(State(session): State<Session>, Path(file): Path<String>) -> Response {
    let name = file.strip_suffix(".js").unwrap_or(&file);
    match session.ui_plugin_module(name) {
        Some(module) => javascript(module.to_string()),
        None => (StatusCode::NOT_FOUND, "no such UI plugin").into_response(),
    }
}

async fn socket(State(session): State<Session>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(move |socket| connection(session, socket))
}

async fn connection(session: Session, socket: WebSocket) {
    let mut connection = session.connect();
    let id = connection.id;
    let (mut out, mut incoming) = socket.split();
    let writer = tokio::spawn(async move {
        while let Some(message) = connection.messages.recv().await {
            let Ok(text) = serde_json::to_string(&message) else {
                continue;
            };
            if out.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });
    while let Some(Ok(frame)) = incoming.next().await {
        let Message::Text(text) = frame else { continue };
        match serde_json::from_str::<ClientMessage>(&text) {
            Ok(message) => session.send(message),
            Err(e) => tracing::debug!("ignoring a frontend message that is not the protocol: {e}"),
        }
    }
    session.disconnect(id);
    writer.abort();
}
