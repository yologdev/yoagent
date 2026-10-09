//! The rutis host services: `frontend` for UI plugins, `ui` for any plugin
//! that asks the user something.
//!
//! - `frontend.connect(client)` — `client.receive(message)` is called with
//!   every [`ServerMessage`](crate::ServerMessage), in order (the next waits for the previous
//!   call). Returns a disposer; the connection also ends when the plugin's
//!   runtime does.
//! - `frontend.send(message)` — a [`ClientMessage`] (`{type: "prompt", text}`, …).
//! - `frontend.addUiPlugin(info, module)` — offer a browser component
//!   ([`UiPlugin`] fields, plus the ES module source). Returns a disposer.
//! - `ui.request(request, timeoutMs?)` — a [`UiRequest`]; resolves to the
//!   answer, or the safe default with no frontend or no answer in time.

use std::sync::Arc;
use std::time::Duration;

use rutis::{CordisError, Ctx};
use rutis_bridge::session::{self, host_key, Error, HostDispatch, Reply, Value};
use serde_json::{json, Value as Json};
use tokio_util::sync::CancellationToken;

use crate::protocol::{ClientMessage, UiPlugin, UiRequest};
use crate::session::{Session, UI_TIMEOUT};

/// The service names, for the loader's catalog (`register_shared`).
pub const FRONTEND: &str = "frontend";
pub const UI: &str = "ui";

/// Provide `frontend` and `ui` on `ctx` (the rutis root).
pub fn provide(ctx: &Ctx, session: &Session) -> Result<(), CordisError> {
    let runtime = ctx.handle().clone();
    ctx.provide_as::<dyn HostDispatch>(
        host_key(FRONTEND),
        Arc::new(FrontendService {
            session: session.clone(),
            runtime,
        }),
    )?;
    ctx.provide_as::<dyn HostDispatch>(
        host_key(UI),
        Arc::new(UiService {
            session: session.clone(),
        }),
    )?;
    Ok(())
}

fn invalid(message: impl Into<String>) -> Error {
    Error::Value(message.into())
}

fn args(value: Value) -> Result<Vec<Value>, Error> {
    value.list()
}

struct FrontendService {
    session: Session,
    runtime: tokio::runtime::Handle,
}

impl HostDispatch for FrontendService {
    fn invoke(&self, method: &str, value: Value) -> Reply {
        match method {
            "connect" => self.connect(value),
            "send" => {
                let message = args(value)?
                    .into_iter()
                    .next()
                    .ok_or_else(|| invalid("send(message): missing the message"))?
                    .json()?;
                let message: ClientMessage = session::decode(message)?;
                self.session.send(message);
                Ok(Value::Undefined)
            }
            "addUiPlugin" => {
                let mut args = args(value)?.into_iter();
                let info: UiPlugin = session::decode(
                    args.next()
                        .ok_or_else(|| invalid("addUiPlugin(info, module): missing info"))?
                        .json()?,
                )?;
                let module = match args.next().map(Value::json).transpose()? {
                    Some(Json::String(source)) => source,
                    _ => return Err(invalid("addUiPlugin(info, module): module is a string")),
                };
                let name = info.name.clone();
                self.session.add_ui_plugin(info, module);
                let session = self.session.clone();
                Ok(Value::callback(move |_| {
                    session.remove_ui_plugin(&name);
                    Ok(Value::Undefined)
                }))
            }
            other => Err(invalid(format!(
                "the frontend service has no method `{other}`"
            ))),
        }
    }

    fn methods(&self) -> Option<Json> {
        Some(json!({ "connect": "sync", "send": "async", "addUiPlugin": "sync" }))
    }
}

impl FrontendService {
    fn connect(&self, value: Value) -> Reply {
        let client = args(value)?
            .into_iter()
            .next()
            .ok_or_else(|| invalid("connect(client): missing the client"))?
            .reference()?;
        if !client.is_object() {
            return Err(invalid(
                "connect(client): an object with a `receive` method",
            ));
        }
        let mut connection = self.session.connect();
        let id = connection.id;
        let done = CancellationToken::new();
        let caller = session::caller();
        let session = self.session.clone();
        let stop = done.clone();
        self.runtime.spawn(async move {
            let closed = async {
                match &caller {
                    Some(connection) => connection.closed().await,
                    None => std::future::pending().await,
                }
            };
            tokio::pin!(closed);
            loop {
                let message = tokio::select! {
                    _ = stop.cancelled() => break,
                    _ = &mut closed => break,
                    message = connection.messages.recv() => match message {
                        Some(message) => message,
                        None => break,
                    },
                };
                let data = match serde_json::to_value(&message) {
                    Ok(data) => data,
                    Err(e) => {
                        tracing::warn!("frontend message not serializable: {e}");
                        continue;
                    }
                };
                let delivered = client
                    .call_method_async("receive", Value::List(vec![Value::Data(data)]))
                    .await;
                let delivered = match delivered {
                    Ok(value) => session::settle(value).await.map(|_| ()),
                    Err(e) => Err(e),
                };
                if let Err(e) = delivered {
                    tracing::warn!("a frontend plugin's receive failed, disconnecting it: {e}");
                    break;
                }
            }
            session.disconnect(id);
        });
        let _entered = self.runtime.enter();
        Ok(Value::callback(move |_| {
            done.cancel();
            Ok(Value::Undefined)
        }))
    }
}

struct UiService {
    session: Session,
}

impl HostDispatch for UiService {
    fn invoke(&self, method: &str, value: Value) -> Reply {
        if method != "request" {
            return Err(invalid(format!("the ui service has no method `{method}`")));
        }
        let mut args = args(value)?.into_iter();
        let request: UiRequest = session::decode(
            args.next()
                .ok_or_else(|| invalid("request(request, timeoutMs?): missing the request"))?
                .json()?,
        )?;
        let timeout = match args.next().map(Value::json).transpose()? {
            Some(Json::Number(ms)) => ms.as_u64().map(Duration::from_millis).unwrap_or(UI_TIMEOUT),
            _ => UI_TIMEOUT,
        };
        let session = self.session.clone();
        Ok(Value::future(async move {
            Ok(Value::Data(session.ask(request, timeout).await))
        }))
    }

    fn methods(&self) -> Option<Json> {
        Some(json!({ "request": "async" }))
    }
}
