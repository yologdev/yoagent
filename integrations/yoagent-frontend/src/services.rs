//! The rutis host services: `frontend` for UI plugins, `ui` for any plugin
//! that asks the user something.
//!
//! - `frontend.connect(client)` — `client.receive(message)` is called with
//!   every [`ServerMessage`](crate::ServerMessage), in order (the next waits for the previous
//!   call). Returns a disposer; the connection also ends when the plugin's
//!   runtime does, or when a `receive` call fails (logged).
//! - `frontend.send(message)` — a [`ClientMessage`] (`{type: "prompt", text}`, …);
//!   rejects once the session ended.
//! - `frontend.addUiPlugin(info, module)` — offer a browser component
//!   ([`UiPlugin`] fields, plus the ES module source). Returns a disposer
//!   that withdraws this offer only (not a later one under the same name).
//! - `ui.request(request, timeoutMs?)` — a [`UiRequest`] (`timeoutMs`: a
//!   non-negative number, default 5 minutes); resolves to the answer, or the safe default with no frontend or no answer in time. A
//!   `key` field in the request makes it withdrawable:
//! - `ui.withdraw(key)` — the asker gave up (its own call was cancelled):
//!   the question resolves to the default and frontends close it. (A plugin
//!   cannot cancel its call to the host itself in rutis 0.7.) A withdrawal
//!   that overtakes its request still counts.
//! - `ui.frontends()` — how many frontends are attached (sync): an asker with
//!   a fallback of its own uses it when nobody could answer.

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
                if !self.session.send(message) {
                    return Err(invalid("send(message): the session has ended"));
                }
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
                let version = self
                    .session
                    .add_ui_plugin(info, module)
                    .ok_or_else(|| invalid("addUiPlugin(info, module): info.name is empty"))?;
                let session = self.session.clone();
                // Removes this offer only: a later one under the same name
                // (the plugin reloaded) stays.
                Ok(Value::callback(move |_| {
                    session.remove_ui_plugin(&name, Some(version));
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
                // Raced like the wait above: a `receive` that never settles
                // must not keep the disposer or a closed runtime waiting.
                let deliver = async {
                    let value = client
                        .call_method_async("receive", Value::List(vec![Value::Data(data)]))
                        .await?;
                    session::settle(value).await.map(|_| ())
                };
                let delivered = tokio::select! {
                    _ = stop.cancelled() => break,
                    _ = &mut closed => break,
                    delivered = deliver => delivered,
                };
                if let Err(e) = delivered {
                    tracing::warn!("a frontend plugin's receive failed, disconnecting it: {e}");
                    break;
                }
            }
            session.disconnect(id);
        });
        Ok(Value::callback(move |_| {
            done.cancel();
            Ok(Value::Undefined)
        }))
    }
}

/// `timeoutMs`: absent or `null` → [`UI_TIMEOUT`]; else a finite,
/// non-negative number of milliseconds (JS numbers may be fractional).
fn timeout(ms: Option<Json>) -> Result<Duration, Error> {
    match ms {
        None | Some(Json::Null) => Ok(UI_TIMEOUT),
        Some(Json::Number(ms)) => ms
            .as_f64()
            .filter(|ms| ms.is_finite() && *ms >= 0.0)
            .and_then(|ms| Duration::try_from_secs_f64(ms / 1000.0).ok())
            .ok_or_else(|| {
                invalid("request(request, timeoutMs?): timeoutMs is a non-negative number")
            }),
        Some(_) => Err(invalid(
            "request(request, timeoutMs?): timeoutMs is a non-negative number",
        )),
    }
}

struct UiService {
    session: Session,
}

impl HostDispatch for UiService {
    fn invoke(&self, method: &str, value: Value) -> Reply {
        if method == "withdraw" {
            let key = args(value)?
                .into_iter()
                .next()
                .map(Value::json)
                .transpose()?;
            let Some(Json::String(key)) = key else {
                return Err(invalid("withdraw(key): key is a string"));
            };
            self.session.withdraw(&key);
            return Ok(Value::Undefined);
        }
        if method == "frontends" {
            return Ok(Value::Data(json!(self.session.frontends())));
        }
        if method != "request" {
            return Err(invalid(format!("the ui service has no method `{method}`")));
        }
        let mut args = args(value)?.into_iter();
        let mut request = args
            .next()
            .ok_or_else(|| invalid("request(request, timeoutMs?): missing the request"))?
            .json()?;
        let key = match request
            .as_object_mut()
            .and_then(|fields| fields.remove("key"))
        {
            None | Some(Json::Null) => None,
            Some(Json::String(key)) => Some(key),
            Some(_) => return Err(invalid("request(request, timeoutMs?): key is a string")),
        };
        let request: UiRequest = session::decode(request)?;
        let timeout = timeout(args.next().map(Value::json).transpose()?)?;
        let session = self.session.clone();
        Ok(Value::future(async move {
            Ok(Value::Data(session.ask_keyed(request, timeout, key).await))
        }))
    }

    fn methods(&self) -> Option<Json> {
        Some(json!({ "request": "async", "withdraw": "async", "frontends": "sync" }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ServerMessage;

    fn list(values: Vec<Json>) -> Value {
        Value::List(values.into_iter().map(Value::Data).collect())
    }

    fn error(reply: Reply) -> String {
        match reply {
            Err(e) => e.to_string(),
            Ok(_) => panic!("expected an error"),
        }
    }

    #[test]
    fn timeouts_take_fractions_and_refuse_nonsense() {
        assert_eq!(timeout(None).unwrap(), UI_TIMEOUT);
        assert_eq!(timeout(Some(Json::Null)).unwrap(), UI_TIMEOUT);
        assert_eq!(
            timeout(Some(json!(1500))).unwrap(),
            Duration::from_millis(1500)
        );
        assert_eq!(
            timeout(Some(json!(2.5))).unwrap(),
            Duration::from_micros(2500)
        );
        assert!(timeout(Some(json!(-1))).is_err());
        assert!(
            timeout(Some(json!(1e300))).is_err(),
            "too long for a Duration"
        );
        assert!(timeout(Some(json!("60s"))).is_err());
    }

    #[tokio::test]
    async fn requests_and_withdrawals_check_their_arguments() {
        let (session, _driver) = Session::new(true);
        let ui = UiService { session };
        let confirm = json!({"kind": "confirm", "title": "Delete?"});
        assert!(error(ui.invoke("withdraw", list(vec![json!(7)]))).contains("key is a string"));
        let mut keyed = confirm.clone();
        keyed["key"] = json!(42);
        assert!(error(ui.invoke("request", list(vec![keyed]))).contains("key is a string"));
        assert!(
            error(ui.invoke("request", list(vec![confirm.clone(), json!("soon")])))
                .contains("timeoutMs")
        );
        assert!(!error(ui.invoke("request", list(vec![json!({"kind": "dance"})]))).is_empty());
        assert!(error(ui.invoke("nope", list(vec![]))).contains("no method"));
        assert!(ui.invoke("request", list(vec![confirm, json!(10)])).is_ok());
    }

    #[tokio::test]
    async fn a_ui_plugin_disposer_removes_only_its_own_offer() {
        let (session, _driver) = Session::new(true);
        let frontend = FrontendService {
            session: session.clone(),
            runtime: tokio::runtime::Handle::current(),
        };
        let offer = |module: &str| {
            frontend.invoke(
                "addUiPlugin",
                list(vec![json!({"name": "links"}), json!(module)]),
            )
        };
        let first = offer("v1").unwrap();
        let _second = offer("v2").unwrap();
        first
            .reference()
            .unwrap()
            .call(Value::List(vec![]))
            .unwrap();
        assert_eq!(session.ui_plugin_module("links").as_deref(), Some("v2"));
        assert!(error(
            frontend.invoke("addUiPlugin", list(vec![json!({"name": " "}), json!("x")]))
        )
        .contains("name is empty"));
    }

    #[tokio::test]
    async fn send_reports_an_ended_session() {
        let (session, driver) = Session::new(true);
        let frontend = FrontendService {
            session: session.clone(),
            runtime: tokio::runtime::Handle::current(),
        };
        let mut watcher = session.connect();
        // The driver dropped before it ran: the session ended.
        drop(driver);
        assert!(session.is_closed());
        assert!(error(
            frontend.invoke("send", list(vec![json!({"type": "prompt", "text": "hi"})]))
        )
        .contains("ended"));
        assert!(matches!(
            watcher.messages.recv().await,
            Some(ServerMessage::Hello { .. })
        ));
        assert!(matches!(
            watcher.messages.recv().await,
            Some(ServerMessage::Closed)
        ));
    }
}
