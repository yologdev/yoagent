//! The browser frontend: a page, the UI plugins' modules, and the protocol
//! over a WebSocket.
//!
//! - `GET /` — the page (`web/index.html`, `web/app.js`, built in).
//! - `GET /ws` — the WebSocket: each text frame is one JSON message,
//!   [`ClientMessage`] in, [`ServerMessage`](crate::ServerMessage) out.
//! - `GET /ui-plugins/{name}` — a UI plugin's ES module (`{name}` ends in `.js`).
//!
//! **The WebSocket needs a token.** Browsers do not apply cross-origin rules
//! to WebSockets, so without one any page the user has open — on any site —
//! could connect to a server on localhost and drive an agent that has
//! `bash`. [`serve`] makes a random token and returns the URL that carries it
//! (`/?t=…`); the page passes it on to `/ws`, which refuses a missing or
//! wrong one. Share that URL only with whoever may drive the agent. UI plugin
//! modules are trusted code: they run in the page with its privileges.

use std::net::SocketAddr;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, RawQuery, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures::{SinkExt, StreamExt};

use crate::protocol::ClientMessage;
use crate::session::Session;

const INDEX: &str = include_str!("../web/index.html");
const APP: &str = include_str!("../web/app.js");

/// A running browser frontend.
pub struct Served {
    /// The bound address (the one passed may use port 0).
    pub addr: SocketAddr,
    /// The token `/ws` requires.
    pub token: String,
    /// The server; abort it to stop serving.
    pub task: tokio::task::JoinHandle<()>,
}

impl Served {
    /// The page's URL, token included: open this one.
    pub fn url(&self) -> String {
        format!("http://{}/?t={}", self.addr, self.token)
    }
}

#[derive(Clone)]
struct App {
    session: Session,
    token: std::sync::Arc<str>,
}

/// Serve the browser frontend for `session` on `addr`, with a new random
/// token, until [`Served::task`] is aborted.
pub async fn serve(session: Session, addr: SocketAddr) -> std::io::Result<Served> {
    let token = uuid::Uuid::new_v4().simple().to_string();
    let state = App {
        session,
        token: token.clone().into(),
    };
    let app = Router::new()
        .route("/", get(|| async { Html(INDEX) }))
        .route("/app.js", get(|| async { javascript(APP.to_owned()) }))
        .route("/favicon.ico", get(|| async { StatusCode::NO_CONTENT }))
        .route("/ui-plugins/{name}", get(ui_plugin))
        .route("/ws", get(socket))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    let task = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::warn!("the web frontend stopped: {e}");
        }
    });
    Ok(Served {
        addr: bound,
        token,
        task,
    })
}

fn javascript(source: String) -> Response {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        source,
    )
        .into_response()
}

async fn ui_plugin(State(app): State<App>, Path(file): Path<String>) -> Response {
    let name = file.strip_suffix(".js").unwrap_or(&file);
    match app.session.ui_plugin_module(name) {
        Some(module) => javascript(module.to_string()),
        None => (StatusCode::NOT_FOUND, "no such UI plugin").into_response(),
    }
}

async fn socket(
    State(app): State<App>,
    RawQuery(query): RawQuery,
    upgrade: WebSocketUpgrade,
) -> Response {
    let given = query
        .as_deref()
        .unwrap_or_default()
        .split('&')
        .find_map(|pair| pair.strip_prefix("t="));
    if given != Some(&*app.token) {
        return (StatusCode::FORBIDDEN, "a valid token is required").into_response();
    }
    let session = app.session;
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
