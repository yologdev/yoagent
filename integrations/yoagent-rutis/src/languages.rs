//! TypeScript / JavaScript and Python plugins, through
//! [rutis-bridge](https://docs.rs/rutis-bridge) 0.7 (the `node`, `python`
//! and `websocket` features).
//!
//! With one of those features, [`RutisBridge::install`](crate::RutisBridge::install)
//! also provides the registry to language plugins as the host service
//! `yoagent` (a `dyn HostDispatch` under `host_key("yoagent")`). A plugin
//! that injects `yoagent` registers a handler — an object (or, in Python, a
//! dict) of async functions, passed by reference:
//!
//! ```ts
//! import { definePlugin } from '@arcships/rutis'
//!
//! export default definePlugin({
//!   inject: ['yoagent'],
//!   apply(ctx) {
//!     const yoagent = ctx.use('yoagent')
//!     ctx.effect(yoagent.register('no-shell', {
//!       async before_tool(call) {
//!         if (call.tool === 'bash') return { deny: 'shell access is disabled' }
//!       },
//!     }))
//!   },
//! })
//! ```
//!
//! `register(name, handler, options?)` returns a function that unregisters
//! the handler: pass it to `ctx.effect`, so the handler goes when the plugin
//! unloads. When the plugin's runtime process exits or crashes, its session
//! closes and every handler it registered is removed too. A handler whose
//! runtime is gone while a run still holds it is unavailable, like a Rust
//! plugin's after an unload: its tool calls fail, its `before_tool` denies,
//! its `on_input` rejects, its `after_tool` withholds.
//!
//! Every hook but `on_event` finds a cancel handle in its first argument,
//! the `signal` field: an `AbortSignal` in JavaScript, a `rutis` `Signal`
//! (`.cancelled`, `await signal.wait()`) in Python. It is aborted when the
//! host gives the call up — the run cancelled (`Agent::abort()`), the hook's
//! timeout passed, the plugin unloaded mid-call — and never when the call
//! completes. A JavaScript function that ignores it runs to completion (its
//! answer is discarded); a Python coroutine is also cancelled
//! (`asyncio.CancelledError` at its next `await`). Pass `call.signal` on
//! (to `fetch`, to a child process) so a `call_tool` stops acting once its
//! run is cancelled.
//!
//! The hooks, their plain-JSON arguments and the values they return are
//! described in `plugins/yoagent.d.ts` in the crate's repository. Every hook
//! is called asynchronously (rutis warns that synchronous calls across
//! runtimes can deadlock); `on_event` costs one cross-process call per
//! event, so it is opt-in and filtered by event type
//! (`options.events: ["toolExecutionEnd", ...]`).
//!
//! The host must share the name with the loader's catalog
//! (`ServiceCatalog::register_shared("yoagent")`, or `share_by_name()`), so
//! language rows can inject it.

use std::collections::BTreeMap;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use futures::future::BoxFuture;
use futures::FutureExt;

use rutis::Ctx;
use rutis_bridge::session::{self, host_key, Error, HostDispatch, Reference, Reply, Value};
use serde_json::{json, Value as Json};
use tokio_util::sync::CancellationToken;
use yoagent::extension::{
    ExtensionError, InputDecision, RunEnd, RunOutcome, StopDecision, ToolOutput, TurnDecision,
};
use yoagent::{AgentEvent, AgentTool, Content, ToolContext, ToolDecision, ToolError, ToolResult};

use std::time::Duration;

use crate::handler::{EventSink, HandlerImpl, Hooks, Input, RunInfo, Stop, ToolCall, Turn};
use crate::registry::Registry;

/// How many events may wait for one handler's `on_event` before it counts
/// as failed (fallen behind).
const EVENT_QUEUE: usize = 1024;

/// The event types (`AgentEvent`'s `type` tag) this bridge knows.
const EVENT_TYPES: [&str; 15] = [
    "agentStart",
    "agentEnd",
    "turnStart",
    "turnEnd",
    "messageStart",
    "messageUpdate",
    "messageEnd",
    "toolExecutionStart",
    "toolExecutionUpdate",
    "toolExecutionEnd",
    "progressMessage",
    "inputRejected",
    "providerRetry",
    "loopDetected",
    "contextCompacted",
];

/// The host service's name.
pub const SERVICE: &str = "yoagent";

/// Every hook a language handler may implement, as named on the handler.
const HOOKS: [&str; 9] = [
    "tools",
    "call_tool",
    "before_tool",
    "after_tool",
    "before_model",
    "on_input",
    "on_stop",
    "finish",
    "on_event",
];

/// Provide the `yoagent` host service on `ctx` (once).
pub(crate) fn provide(ctx: &Ctx, registry: &Arc<Registry>) -> Result<(), rutis::CordisError> {
    let key = host_key(SERVICE);
    if ctx.get_as::<dyn HostDispatch>(key.clone()).is_some() {
        return Ok(());
    }
    let service = Service {
        registry: registry.clone(),
        runtime: ctx.handle().clone(),
    };
    ctx.provide_as::<dyn HostDispatch>(key, Arc::new(service))?;
    Ok(())
}

/// The `yoagent` host service.
struct Service {
    registry: Arc<Registry>,
    runtime: tokio::runtime::Handle,
}

fn invalid(message: impl Into<String>) -> Error {
    Error::Value(message.into())
}

impl HostDispatch for Service {
    fn invoke(&self, method: &str, args: Value) -> Reply {
        match method {
            "register" => self.register(args),
            "log" => log(args),
            other => Err(invalid(format!(
                "the yoagent service has no method `{other}`"
            ))),
        }
    }

    fn methods(&self) -> Option<Json> {
        // Synchronous: `register` returns the disposer the plugin passes to
        // `ctx.effect`. It reads the handler's members back while the plugin
        // waits, on the same call chain. `log` is fire-and-forget.
        Some(json!({ "register": "sync", "log": "async" }))
    }
}

/// Longest message `log` writes, in characters; the rest is cut (a plugin cannot flood the host's logs).
const MAX_LOG_CHARS: usize = 8 * 1024;

/// `log(level, message, context?)`: a plugin's diagnostic in the host's
/// `tracing` output (target `yoagent_rutis::plugin`), not on the plugin
/// process's stderr. Levels `error`, `warn`, `info`, `debug`; anything else
/// is `warn`. `context.run_id` (the `run_id` every hook argument carries) is
/// recorded as the event's `run_id` field, so a consumer — a GASP recorder,
/// a log viewer — can attribute the line to its run.
///
/// **Never fails.** A plugin calls it fire-and-forget, and a rejected call
/// it does not catch would end its runtime process. So any value is taken:
/// a non-string message is written as JSON, other context fields (a whole
/// hook argument, say) are ignored, a missing argument is empty.
fn log(args: Value) -> Reply {
    let args: Vec<Json> = args
        .list()
        .map(|list| list.into_iter().filter_map(|v| v.json().ok()).collect())
        .unwrap_or_default();
    let text = |value: Option<&Json>| match value {
        Some(Json::String(text)) => text.clone(),
        Some(Json::Null) | None => String::new(),
        Some(other) => other.to_string(),
    };
    let level = text(args.first());
    let message = text(args.get(1));
    let run_id = args
        .get(2)
        .and_then(|context| context.get("run_id"))
        .map(|id| text(Some(id)))
        .unwrap_or_default();
    let message: String = match message.char_indices().nth(MAX_LOG_CHARS) {
        Some((cut, _)) => format!("{}… (cut)", &message[..cut]),
        None => message,
    };
    let run_id = run_id.as_str();
    match level.as_str() {
        "error" => tracing::error!(target: "yoagent_rutis::plugin", run_id, "{message}"),
        "info" => tracing::info!(target: "yoagent_rutis::plugin", run_id, "{message}"),
        "debug" => tracing::debug!(target: "yoagent_rutis::plugin", run_id, "{message}"),
        _ => tracing::warn!(target: "yoagent_rutis::plugin", run_id, "{message}"),
    }
    Ok(Value::Undefined)
}

/// `register`'s third argument.
#[derive(Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Options {
    /// The event types (`AgentEvent`'s `type` tag) `on_event` receives.
    events: Vec<String>,
}

impl Service {
    fn register(&self, args: Value) -> Reply {
        let mut args = args.list()?.into_iter();
        let name: String = session::decode(
            args.next()
                .ok_or_else(|| invalid("register(name, handler, options?): missing the name"))?
                .json()?,
        )?;
        if name.trim().is_empty() {
            return Err(invalid("register: the handler name is empty"));
        }
        let target =
            Target::from_value(args.next().ok_or_else(|| {
                invalid("register(name, handler, options?): missing the handler")
            })?)?;
        let options: Options = match args.next() {
            None | Some(Value::Undefined) => Options::default(),
            Some(value) => match value.json()? {
                Json::Null => Options::default(),
                json => session::decode(json)?,
            },
        };
        let hooks = target.hooks()?;
        if hooks.tools && !target.has("call_tool")? {
            return Err(invalid(format!(
                "register `{name}`: a handler with `tools` must implement `call_tool`"
            )));
        }
        if !hooks.on_event && !options.events.is_empty() {
            return Err(invalid(format!(
                "register `{name}`: `options.events` needs an `on_event` method"
            )));
        }
        if hooks.on_event && options.events.is_empty() {
            return Err(invalid(format!(
                "register `{name}`: `on_event` needs `options.events`, the event types to send"
            )));
        }
        for kind in &options.events {
            if !EVENT_TYPES.contains(&kind.as_str()) {
                tracing::warn!(handler = %name, event = %kind, "not an event type this bridge knows; it may never be sent");
            }
        }
        let caller = session::caller();
        let owner = match &caller {
            Some(connection) => format!("language plugin (session {})", connection.tag()),
            None => "language plugin".to_string(),
        };
        let handler = Arc::new(RemoteHandler {
            name: name.clone(),
            target: Arc::new(target),
            hooks,
            events: Arc::new(options.events),
        });
        let lifetime = CancellationToken::new();
        let (seq, _gate) = self
            .registry
            .insert(name.clone(), owner, handler, lifetime.clone())
            .map_err(session::native_error)?;
        // The runtime crashed or exited: its session closed, and the handler
        // goes with it.
        match caller {
            Some(connection) => {
                let registry = Arc::downgrade(&self.registry);
                let lifetime = lifetime.clone();
                self.runtime.spawn(async move {
                    tokio::select! {
                        _ = connection.closed() => {
                            tracing::info!(handler = %name, "the plugin's session closed; unregistering its handler");
                        }
                        _ = lifetime.cancelled() => {}
                    }
                    lifetime.cancel();
                    if let Some(registry) = registry.upgrade() {
                        registry.remove(seq);
                    }
                });
            }
            None => tracing::warn!(
                handler = %name,
                "registered outside a session call: only its disposer removes it"
            ),
        }
        let registry = Arc::downgrade(&self.registry);
        let _entered = self.runtime.enter();
        Ok(Value::callback(move |_| {
            lifetime.cancel();
            if let Some(registry) = registry.upgrade() {
                registry.remove(seq);
            }
            Ok(Value::Undefined)
        }))
    }
}

/// What a plugin passed as its handler.
enum Target {
    /// A live object (JavaScript objects with methods, Python class
    /// instances): hooks are its methods.
    Object(Reference),
    /// A record of functions (a Python dict): hooks are its entries.
    Record(BTreeMap<String, Reference>),
}

impl Target {
    fn from_value(value: Value) -> Result<Self, Error> {
        match value {
            Value::Reference(reference) if reference.is_object() => Ok(Self::Object(reference)),
            Value::Record(fields) => {
                let mut functions = BTreeMap::new();
                for (key, value) in fields {
                    match value {
                        Value::Reference(f) if f.is_function() => {
                            functions.insert(key, f);
                        }
                        // Absent, as on a live object (`None` in Python).
                        Value::Undefined | Value::Data(Json::Null) => {}
                        _ if HOOKS.contains(&key.as_str()) => {
                            return Err(invalid(format!("register: `{key}` is not a function")))
                        }
                        _ => {}
                    }
                }
                Ok(Self::Record(functions))
            }
            _ => Err(invalid(
                "register: the handler must be an object (or dict) of async functions",
            )),
        }
    }

    /// Whether the handler implements `hook`. For a live object this reads
    /// the member back (a synchronous call on the plugin's call chain). A
    /// member that is there but not a function, or that cannot be read, is
    /// an error: a policy must never go missing silently.
    fn has(&self, hook: &str) -> Result<bool, Error> {
        match self {
            Self::Record(functions) => Ok(functions.contains_key(hook)),
            Self::Object(object) => match object.get(hook) {
                Ok(Value::Undefined) | Ok(Value::Data(Json::Null)) => Ok(false),
                Ok(Value::Reference(member)) if member.is_function() => Ok(true),
                Ok(_) => Err(invalid(format!("register: `{hook}` is not a function"))),
                // Python's getattr on a missing attribute.
                Err(Error::Remote { name, .. }) if name == "AttributeError" => Ok(false),
                Err(error) => Err(invalid(format!(
                    "register: cannot read the handler's `{hook}`: {error}"
                ))),
            },
        }
    }

    fn hooks(&self) -> Result<Hooks, Error> {
        let hooks = Hooks {
            tools: self.has("tools")?,
            before_tool: self.has("before_tool")?,
            after_tool: self.has("after_tool")?,
            before_model: self.has("before_model")?,
            on_input: self.has("on_input")?,
            on_stop: self.has("on_stop")?,
            finish: self.has("finish")?,
            on_event: self.has("on_event")?,
        };
        if hooks == Hooks::default() && !self.has("call_tool")? {
            return Err(invalid(format!(
                "register: the handler implements none of {}",
                HOOKS.join(", ")
            )));
        }
        Ok(hooks)
    }

    /// Call `hook` with JSON arguments; a returned Promise / coroutine is
    /// awaited. The result must be plain data.
    ///
    /// The first argument, when it is an object, also carries the call's
    /// cancel handle as its `signal` field ([`with_signal`]): dropping this
    /// future (a timeout, the run cancelled, the plugin unloaded) cancels the
    /// call, which aborts the handle.
    async fn call(&self, hook: &str, args: Vec<Json>) -> Result<Json, ExtensionError> {
        let args = Value::List(
            args.into_iter()
                .enumerate()
                .map(|(i, arg)| {
                    if i == 0 {
                        with_signal(arg)
                    } else {
                        Value::Data(arg)
                    }
                })
                .collect(),
        );
        self.call_with(hook, args).await
    }

    /// Call `hook` with plain JSON arguments and no cancel handle
    /// (`on_event`: an event is data the plugin may keep or forward as is).
    async fn call_plain(&self, hook: &str, args: Vec<Json>) -> Result<Json, ExtensionError> {
        self.call_with(
            hook,
            Value::List(args.into_iter().map(Value::Data).collect()),
        )
        .await
    }

    async fn call_with(&self, hook: &str, args: Value) -> Result<Json, ExtensionError> {
        let reply = match self {
            Self::Object(object) => object.call_method_async(hook, args).await,
            Self::Record(functions) => match functions.get(hook) {
                Some(function) => function.call_async(args).await,
                None => Err(invalid(format!("no `{hook}` function"))),
            },
        };
        let settled = match reply {
            Ok(value) => session::settle(value).await,
            Err(error) => Err(error),
        };
        settled
            .and_then(Value::json)
            .map_err(|error| ExtensionError::new(error.to_string()))
    }
}

/// The field of a hook's first argument that carries the call's cancel
/// handle.
const SIGNAL_FIELD: &str = "signal";

/// `arg` with the call's cancel handle as its [`SIGNAL_FIELD`]: a JSON object
/// becomes a record whose fields are copied as data, plus rutis-bridge's
/// call signal — an `AbortSignal` in JavaScript, a `rutis` `Signal`
/// (`.cancelled`, `await .wait()`) in Python. rutis-bridge aborts it when
/// the host gives the call up (its future dropped): the run cancelled, the
/// hook's timeout, the plugin unloaded. It is a field rather than an extra
/// positional argument so that a Python method with a fixed signature still
/// accepts the call. Anything but an object is passed as it is.
fn with_signal(arg: Json) -> Value {
    match arg {
        Json::Object(fields) => Value::Record(
            fields
                .into_iter()
                .map(|(key, value)| (key, Value::Data(value)))
                .chain([(SIGNAL_FIELD.to_string(), Value::Signal)])
                .collect(),
        ),
        other => Value::Data(other),
    }
}

/// A language plugin's handler.
struct RemoteHandler {
    name: String,
    target: Arc<Target>,
    hooks: Hooks,
    events: Arc<Vec<String>>,
}

fn to_json<T: serde::Serialize>(value: &T) -> Json {
    serde_json::to_value(value).unwrap_or(Json::Null)
}

fn unexpected(hook: &str, value: &Json) -> ExtensionError {
    ExtensionError::new(format!("`{hook}` returned an unexpected value: {value}"))
}

/// One object key, with nothing else: `{deny: "..."}`.
fn only<'a>(value: &'a Json, key: &str) -> Option<&'a Json> {
    let object = value.as_object()?;
    (object.len() == 1).then(|| object.get(key)).flatten()
}

#[async_trait::async_trait]
impl HandlerImpl for RemoteHandler {
    fn hooks(&self) -> Hooks {
        self.hooks
    }

    fn static_tools(&self) -> Vec<Arc<dyn AgentTool>> {
        Vec::new()
    }

    async fn tools(&self, run: RunInfo) -> Result<Vec<Arc<dyn AgentTool>>, ExtensionError> {
        let specs = self.target.call("tools", vec![to_json(&run)]).await?;
        let specs: Vec<ToolSpec> = serde_json::from_value(specs)
            .map_err(|e| ExtensionError::new(format!("`tools` returned invalid tools: {e}")))?;
        Ok(specs
            .into_iter()
            .map(|spec| {
                Arc::new(RemoteTool {
                    label: spec.label.clone().unwrap_or_else(|| spec.name.clone()),
                    spec,
                    target: self.target.clone(),
                    run: run.clone(),
                }) as Arc<dyn AgentTool>
            })
            .collect())
    }

    async fn before_tool(&self, call: ToolCall) -> Result<ToolDecision, ExtensionError> {
        let value = self
            .target
            .call("before_tool", vec![to_json(&call)])
            .await?;
        if value.is_null() {
            return Ok(ToolDecision::Allow);
        }
        if let Some(reason) = only(&value, "deny") {
            return match reason.as_str() {
                Some(reason) => Ok(ToolDecision::Deny(reason.to_string())),
                None => Err(unexpected("before_tool", &value)),
            };
        }
        if let Some(args) = only(&value, "args").filter(|args| args.is_object()) {
            return Ok(ToolDecision::Modify(args.clone()));
        }
        Err(unexpected("before_tool", &value))
    }

    async fn after_tool(
        &self,
        call: ToolCall,
        mut output: ToolOutput,
    ) -> Result<ToolOutput, ExtensionError> {
        let text: String = output
            .result
            .content
            .iter()
            .filter_map(|c| match c {
                Content::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        let shown = json!({
            "text": text,
            "content": output.result.content,
            "details": output.result.details,
            "is_error": output.is_error,
        });
        let value = self
            .target
            .call("after_tool", vec![to_json(&call), shown])
            .await?;
        if value.is_null() {
            return Ok(output);
        }
        let Some(edit) = value.as_object() else {
            return Err(unexpected("after_tool", &value));
        };
        if edit
            .keys()
            .any(|k| !["text", "content", "details", "is_error"].contains(&k.as_str()))
            || (edit.contains_key("text") && edit.contains_key("content"))
        {
            return Err(unexpected("after_tool", &value));
        }
        if let Some(text) = edit.get("text") {
            let text = text
                .as_str()
                .ok_or_else(|| unexpected("after_tool", &value))?;
            output.result.content = vec![Content::Text {
                text: text.to_string(),
            }];
        }
        if let Some(content) = edit.get("content") {
            output.result.content = content_blocks(content, &output.result.content)
                .ok_or_else(|| unexpected("after_tool", &value))?;
        }
        if let Some(details) = edit.get("details") {
            output.result.details = details.clone();
        }
        if let Some(is_error) = edit.get("is_error") {
            output.is_error = is_error
                .as_bool()
                .ok_or_else(|| unexpected("after_tool", &value))?;
        }
        Ok(output)
    }

    async fn before_model(&self, turn: Turn) -> Result<TurnDecision, ExtensionError> {
        let value = self
            .target
            .call("before_model", vec![to_json(&turn)])
            .await?;
        match &value {
            Json::Null => Ok(TurnDecision::Continue),
            Json::String(note) => Ok(TurnDecision::Note(note.clone())),
            _ => {
                if let Some(note) = only(&value, "note").and_then(Json::as_str) {
                    Ok(TurnDecision::Note(note.to_string()))
                } else if let Some(reason) = only(&value, "stop").and_then(Json::as_str) {
                    Ok(TurnDecision::Stop(reason.to_string()))
                } else {
                    Err(unexpected("before_model", &value))
                }
            }
        }
    }

    async fn on_input(&self, input: Input) -> Result<InputDecision, ExtensionError> {
        let value = self.target.call("on_input", vec![to_json(&input)]).await?;
        if value.is_null() {
            return Ok(InputDecision::Pass);
        }
        match only(&value, "reject").and_then(Json::as_str) {
            Some(reason) => Ok(InputDecision::Reject(reason.to_string())),
            None => Err(unexpected("on_input", &value)),
        }
    }

    async fn on_stop(&self, stop: Stop) -> Result<StopDecision, ExtensionError> {
        let value = self.target.call("on_stop", vec![to_json(&stop)]).await?;
        if value.is_null() {
            return Ok(StopDecision::Accept);
        }
        if let Some(message) = only(&value, "continue").and_then(Json::as_str) {
            return Ok(StopDecision::Continue(message.to_string()));
        }
        if let Some(reason) = only(&value, "fail").and_then(Json::as_str) {
            return Ok(StopDecision::Fail(reason.to_string()));
        }
        Err(unexpected("on_stop", &value))
    }

    async fn finish(&self, outcome: RunOutcome, run: RunInfo) -> Result<(), ExtensionError> {
        self.target
            .call("finish", vec![outcome_json(&outcome, &run)])
            .await?;
        Ok(())
    }

    fn events(&self, run: &RunInfo, limit: Option<Duration>) -> Option<Box<dyn EventSink>> {
        if !self.hooks.on_event {
            return None;
        }
        let target = self.target.clone();
        let deliver: Deliver = Arc::new(move |event| {
            let target = target.clone();
            Box::pin(async move { target.call_plain("on_event", vec![event]).await })
        });
        Some(Box::new(RemoteEvents::start(
            self.name.clone(),
            self.events.clone(),
            run.clone(),
            limit,
            deliver,
        )))
    }
}

/// `finish`'s argument: `{end, reason?, error?, extension?, stop_reason?, run...}`.
fn outcome_json(outcome: &RunOutcome, run: &RunInfo) -> Json {
    let mut value = match outcome.end() {
        RunEnd::Completed => json!({"end": "completed"}),
        RunEnd::Stopped { reason } => json!({"end": "stopped", "reason": reason}),
        RunEnd::Rejected { reason } => json!({"end": "rejected", "reason": reason}),
        RunEnd::Cancelled => json!({"end": "cancelled"}),
        RunEnd::Failed { error, extension } => {
            json!({"end": "failed", "error": error, "extension": extension})
        }
        _ => json!({"end": "other"}),
    };
    value["stop_reason"] = to_json(&outcome.stop_reason());
    if let (Some(fields), Json::Object(run)) = (value.as_object_mut(), to_json(run)) {
        fields.extend(run);
    }
    value
}

/// What the delivery task of one run and handler receives.
enum Delivery {
    Event(Json),
    /// Acknowledge once every event queued before it was delivered.
    Flush(tokio::sync::oneshot::Sender<()>),
}

/// The `type` tag of an event, without serializing it for the variants
/// known here (a newer variant is serialized to read it).
fn event_type(event: &AgentEvent) -> String {
    let known = match event {
        AgentEvent::AgentStart => "agentStart",
        AgentEvent::AgentEnd { .. } => "agentEnd",
        AgentEvent::TurnStart => "turnStart",
        AgentEvent::TurnEnd { .. } => "turnEnd",
        AgentEvent::MessageStart { .. } => "messageStart",
        AgentEvent::MessageUpdate { .. } => "messageUpdate",
        AgentEvent::MessageEnd { .. } => "messageEnd",
        AgentEvent::ToolExecutionStart { .. } => "toolExecutionStart",
        AgentEvent::ToolExecutionUpdate { .. } => "toolExecutionUpdate",
        AgentEvent::ToolExecutionEnd { .. } => "toolExecutionEnd",
        AgentEvent::ProgressMessage { .. } => "progressMessage",
        AgentEvent::InputRejected { .. } => "inputRejected",
        _ => {
            return to_json(event)
                .get("type")
                .and_then(Json::as_str)
                .unwrap_or_default()
                .to_string()
        }
    };
    known.to_string()
}

/// Delivers one event to a handler's `on_event` (a call into its runtime).
type Deliver = Arc<dyn Fn(Json) -> BoxFuture<'static, Result<Json, ExtensionError>> + Send + Sync>;

/// Queues a run's events (those of the subscribed types) for delivery.
struct RemoteEvents {
    name: String,
    filter: Arc<Vec<String>>,
    run: RunInfo,
    tx: tokio::sync::mpsc::Sender<Delivery>,
    /// The first delivery failure (an error, a panic, a timeout, a full
    /// queue, a delivery task that ended): the handler's `on_event` is
    /// switched off for the run. Never raised as a panic: the extension reads
    /// it with [`EventSink::failure`].
    failed: Arc<std::sync::Mutex<Option<String>>>,
}

/// Record `why` as the first delivery failure (later ones are dropped).
fn record_failure(failed: &std::sync::Mutex<Option<String>>, why: String) {
    failed
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert(why);
}

/// The failure of a delivery task that is gone while the run still sends.
fn delivery_ended(name: &str) -> String {
    format!(
        "plugin handler `{name}`: its event delivery ended unexpectedly (`on_event` not called again this run)"
    )
}

/// Text of a panic payload.
fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".into())
}

impl RemoteEvents {
    /// Spawn the run's delivery task: one per run and handler, so events
    /// arrive in order and the run never waits for them. It ends once the
    /// sink is dropped and the queue drained (`AgentEnd` comes after
    /// `finish`). A delivery that panics (the handler's call or the session
    /// code around it) is contained and recorded as the handler's failure;
    /// should the task end anyway, the next `send` or `flush` records that.
    fn start(
        name: String,
        filter: Arc<Vec<String>>,
        run: RunInfo,
        limit: Option<Duration>,
        deliver: Deliver,
    ) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Delivery>(EVENT_QUEUE);
        let failed = Arc::new(std::sync::Mutex::new(None::<String>));
        let failure = failed.clone();
        let task_name = name.clone();
        let run_id = run.run_id.clone();
        tokio::spawn(async move {
            let name = task_name;
            while let Some(delivery) = rx.recv().await {
                let event = match delivery {
                    Delivery::Event(event) => event,
                    Delivery::Flush(done) => {
                        let _ = done.send(());
                        continue;
                    }
                };
                if failure.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
                    continue; // switched off: drop what is still queued
                }
                // Built inside the guard too: a panic before the future exists.
                let call = AssertUnwindSafe(async { deliver(event).await }).catch_unwind();
                let outcome = match limit {
                    Some(limit) => tokio::time::timeout(limit, call).await.unwrap_or_else(|_| {
                        Ok(Err(ExtensionError::new(format!(
                            "did not answer within {limit:?}"
                        ))))
                    }),
                    None => call.await,
                };
                let why = match outcome {
                    Ok(Ok(_)) => continue,
                    Ok(Err(error)) => format!(
                        "plugin handler `{name}` failed in `on_event`: {error} (not called again this run)"
                    ),
                    Err(payload) => format!(
                        "plugin handler `{name}` panicked in `on_event`: {} (not called again this run)",
                        panic_text(&*payload)
                    ),
                };
                // Logged here too: a failure on the run's last events
                // (`AgentEnd` comes after `finish`) is seen by nothing else.
                tracing::warn!(run_id = %run_id, "{why}");
                record_failure(&failure, why);
            }
        });
        Self {
            name,
            filter,
            run,
            tx,
            failed,
        }
    }

    /// The delivery task is gone while the run still holds the sink: never
    /// silently, or a required handler would never fail.
    fn ended(&self) {
        let why = delivery_ended(&self.name);
        tracing::warn!(run_id = %self.run.run_id, "{why}");
        record_failure(&self.failed, why);
    }
}

impl EventSink for RemoteEvents {
    fn send(&self, event: &AgentEvent) {
        if self.failure().is_some() {
            return;
        }
        let kind = event_type(event);
        if !self.filter.contains(&kind) {
            return;
        }
        let mut json = to_json(event);
        if let (Some(fields), Json::Object(run)) = (json.as_object_mut(), to_json(&self.run)) {
            fields.insert("run".into(), Json::Object(run));
        }
        use tokio::sync::mpsc::error::TrySendError;
        match self.tx.try_send(Delivery::Event(json)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => record_failure(
                &self.failed,
                format!(
                    "plugin handler `{}` fell {EVENT_QUEUE} events behind in `on_event` (not called again this run)",
                    self.name
                ),
            ),
            Err(TrySendError::Closed(_)) => self.ended(),
        }
    }

    fn failure(&self) -> Option<String> {
        self.failed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn flush(&self) -> BoxFuture<'static, ()> {
        let (done, flushed) = tokio::sync::oneshot::channel();
        let tx = self.tx.clone();
        let failed = self.failed.clone();
        let name = self.name.clone();
        let run_id = self.run.run_id.clone();
        Box::pin(async move {
            // Either fails only when the delivery task is gone: it answers
            // every flush it receives, even once switched off.
            let delivered = tx.send(Delivery::Flush(done)).await.is_ok() && flushed.await.is_ok();
            if !delivered {
                let why = delivery_ended(&name);
                tracing::warn!(run_id = %run_id, "{why}");
                record_failure(&failed, why);
            }
        })
    }
}

/// A tool as `tools` describes it.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolSpec {
    name: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    description: String,
    #[serde(default = "empty_schema", deserialize_with = "null_as_empty_schema")]
    parameters: Json,
}

fn empty_schema() -> Json {
    json!({"type": "object", "properties": {}})
}

/// `null` (Python's `None`) reads as a missing field.
fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de> + Default,
{
    use serde::Deserialize;
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

fn null_as_empty_schema<'de, D>(deserializer: D) -> Result<Json, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    Ok(Option::<Json>::deserialize(deserializer)?.unwrap_or_else(empty_schema))
}

/// A language plugin's tool, run through its handler's `call_tool`.
struct RemoteTool {
    spec: ToolSpec,
    label: String,
    target: Arc<Target>,
    run: RunInfo,
}

#[async_trait::async_trait]
impl AgentTool for RemoteTool {
    fn name(&self) -> &str {
        &self.spec.name
    }
    fn label(&self) -> &str {
        &self.label
    }
    fn description(&self) -> &str {
        &self.spec.description
    }
    fn parameters_schema(&self) -> Json {
        self.spec.parameters.clone()
    }
    async fn execute(&self, params: Json, ctx: ToolContext) -> Result<ToolResult, ToolError> {
        let call = ToolCall::new(ctx.tool_call_id.clone(), self.spec.name.clone(), params)
            .with_run(self.run.clone());
        let value = tokio::select! {
            value = self.target.call("call_tool", vec![to_json(&call)]) => value,
            _ = ctx.cancel.cancelled() => return Err(ToolError::Cancelled),
        }
        .map_err(|e| ToolError::Failed(e.to_string()))?;
        let unexpected =
            || ToolError::Failed(format!("`call_tool` returned an unexpected value: {value}"));
        let (content, details, is_error) = match &value {
            Json::String(text) => (
                vec![Content::Text { text: text.clone() }],
                Json::Null,
                false,
            ),
            // `{}` is an empty text, as `after_tool`'s edit reads it.
            Json::Object(fields)
                if fields
                    .keys()
                    .all(|k| ["text", "content", "details", "is_error"].contains(&k.as_str()))
                    && !(fields.contains_key("text") && fields.contains_key("content")) =>
            {
                let content = match (fields.get("text"), fields.get("content")) {
                    (Some(Json::String(text)), _) => vec![Content::Text { text: text.clone() }],
                    (Some(_), _) => return Err(unexpected()),
                    (None, Some(blocks)) => content_blocks(blocks, &[]).ok_or_else(unexpected)?,
                    (None, None) => vec![Content::Text {
                        text: String::new(),
                    }],
                };
                let is_error = match fields.get("is_error") {
                    None => false,
                    Some(Json::Bool(is_error)) => *is_error,
                    Some(_) => return Err(unexpected()),
                };
                let details = fields.get("details").cloned().unwrap_or(Json::Null);
                (content, details, is_error)
            }
            _ => return Err(unexpected()),
        };
        if is_error {
            let text = content
                .iter()
                .filter_map(|c| match c {
                    Content::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            return Err(ToolError::Failed(text));
        }
        Ok(ToolResult { content, details })
    }
}

/// Image types every provider takes; others (svg, bmp, tiff, ...) are refused
/// rather than put into history, where they would fail every later request.
const IMAGE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// Largest image a plugin may hand over, decoded. Keeps one answer under the
/// websocket transport's frame limit (16 MiB) once base64-encoded; providers
/// may take less (Anthropic: 5 MB) — plugins should stay under theirs.
const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;

/// A handler's content blocks — `{"type": "text", "text"}` and
/// `{"type": "image", "data", "mimeType"}`, yoagent's own JSON shape (and
/// pi's, and MCP's bare blocks) — as yoagent content. `None` for anything
/// else: not an array, an unknown block type, an extra or missing field, an
/// image whose `mimeType` is not one of IMAGE_TYPES or whose `data` is not
/// standard base64 of 1 to MAX_IMAGE_BYTES bytes. An image identical to one
/// in `kept` (the output an `after_tool` edit was given) passes as it is:
/// yoagent let it in (`read_file` takes bmp, up to 20 MB), and keeping it
/// must not fail the edit.
fn content_blocks(blocks: &Json, kept: &[Content]) -> Option<Vec<Content>> {
    use base64::Engine as _;
    blocks
        .as_array()?
        .iter()
        .map(|block| {
            let fields = block.as_object()?;
            let field = |name: &str| fields.get(name)?.as_str();
            match field("type")? {
                "text" if fields.len() == 2 => Some(Content::Text {
                    text: field("text")?.to_string(),
                }),
                "image" if fields.len() == 3 => {
                    let data = field("data")?;
                    let mime_type = field("mimeType")?;
                    let unchanged = kept.iter().any(|c| {
                        matches!(c, Content::Image { data: d, mime_type: m } if d == data && m == mime_type)
                    });
                    if unchanged {
                        return Some(Content::Image {
                            data: data.to_string(),
                            mime_type: mime_type.to_string(),
                        });
                    }
                    // A decoded length bound before decoding: 3 bytes per 4 characters.
                    let fits = data.len() / 4 * 3 <= MAX_IMAGE_BYTES + 2;
                    let bytes = fits
                        .then(|| base64::engine::general_purpose::STANDARD.decode(data).ok())
                        .flatten()?;
                    let valid = !bytes.is_empty()
                        && bytes.len() <= MAX_IMAGE_BYTES
                        && IMAGE_TYPES.contains(&mime_type);
                    valid.then(|| Content::Image {
                        data: data.to_string(),
                        mime_type: mime_type.to_string(),
                    })
                }
                _ => None,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use yoagent::extension::RunContext;
    use yoagent::{Extension, ToolCallRequest};

    type Failed = Arc<Mutex<Option<String>>>;

    #[test]
    fn content_blocks_take_text_and_images_in_yoagents_shape() {
        let blocks = content_blocks(
            &json!([
                {"type": "text", "text": "see"},
                {"type": "image", "data": "iVBORw0KGgo=", "mimeType": "image/png"},
            ]),
            &[],
        )
        .unwrap();
        assert!(matches!(&blocks[0], Content::Text { text } if text == "see"));
        assert!(matches!(&blocks[1], Content::Image { data, mime_type }
            if data == "iVBORw0KGgo=" && mime_type == "image/png"));
        assert!(content_blocks(&json!([]), &[]).unwrap().is_empty());
        for bad in [
            json!("text"),
            json!([{"type": "text"}]),
            json!([{"type": "text", "text": 1}]),
            json!([{"type": "text", "text": "a", "extra": 1}]),
            json!([{"type": "image", "data": "", "mimeType": "image/png"}]),
            json!([{"type": "image", "data": "not base64!", "mimeType": "image/png"}]),
            json!([{"type": "image", "data": "AAAA", "mimeType": "text/plain"}]),
            json!([{"type": "image", "data": "AAAA"}]),
            json!([{"type": "file", "data": "AAAA", "mimeType": "image/png"}]),
            // Mis-padded, or padding in the middle: not base64.
            json!([{"type": "image", "data": "AAAAA", "mimeType": "image/png"}]),
            json!([{"type": "image", "data": "A=AA", "mimeType": "image/png"}]),
            json!([{"type": "image", "data": "=", "mimeType": "image/png"}]),
            // A type providers do not take.
            json!([{"type": "image", "data": "AAAA", "mimeType": "image/svg+xml"}]),
            json!([{"type": "image", "data": "AAAA", "mimeType": "image/"}]),
            // Over the size limit: refused after decoding, and before it.
            json!([{"type": "image", "data": "A".repeat((MAX_IMAGE_BYTES + 3) / 3 * 4), "mimeType": "image/png"}]),
            json!([{"type": "image", "data": "A".repeat(MAX_IMAGE_BYTES / 3 * 4 + 8), "mimeType": "image/png"}]),
        ] {
            assert!(content_blocks(&bad, &[]).is_none(), "{bad}");
        }
    }

    #[test]
    fn an_after_tool_edit_may_keep_an_image_yoagent_let_in() {
        // read_file returns bmp; MCP tools other types: keeping one is no new image.
        let kept = [Content::Image {
            data: "Qk0=".into(),
            mime_type: "image/bmp".into(),
        }];
        let same = json!([{"type": "image", "data": "Qk0=", "mimeType": "image/bmp"}]);
        assert!(content_blocks(&same, &[]).is_none());
        let blocks = content_blocks(&same, &kept).unwrap();
        assert!(matches!(&blocks[0], Content::Image { mime_type, .. } if mime_type == "image/bmp"));
        // Changed data is a new image, checked as one.
        let changed = json!([{"type": "image", "data": "Qk1=", "mimeType": "image/bmp"}]);
        assert!(content_blocks(&changed, &kept).is_none());
    }

    /// How the test handler's event delivery goes wrong.
    #[derive(Clone, Copy)]
    enum Breaks {
        /// The handler's call panics inside the delivery task.
        Panics,
        /// The delivery task is gone (it died) while the run still sends.
        TaskDies,
    }

    /// A handler observing `toolExecutionEnd` whose delivery breaks.
    struct Broken {
        breaks: Breaks,
        /// The sink's failure slot, once a run started.
        failed: Mutex<Option<Failed>>,
    }

    fn sink(breaks: Breaks, run: &RunInfo) -> RemoteEvents {
        let filter = Arc::new(vec!["toolExecutionEnd".to_string()]);
        match breaks {
            Breaks::Panics => RemoteEvents::start(
                "broken".into(),
                filter,
                run.clone(),
                None,
                Arc::new(
                    |_event| -> BoxFuture<'static, Result<Json, ExtensionError>> {
                        Box::pin(async { panic!("delivery bug") })
                    },
                ),
            ),
            Breaks::TaskDies => {
                let (tx, rx) = tokio::sync::mpsc::channel(EVENT_QUEUE);
                drop(rx);
                RemoteEvents {
                    name: "broken".into(),
                    filter,
                    run: run.clone(),
                    tx,
                    failed: Arc::default(),
                }
            }
        }
    }

    #[async_trait::async_trait]
    impl HandlerImpl for Broken {
        fn hooks(&self) -> Hooks {
            Hooks {
                on_event: true,
                ..Hooks::default()
            }
        }
        fn static_tools(&self) -> Vec<Arc<dyn AgentTool>> {
            Vec::new()
        }
        async fn tools(&self, _: RunInfo) -> Result<Vec<Arc<dyn AgentTool>>, ExtensionError> {
            Ok(Vec::new())
        }
        async fn before_tool(&self, _: ToolCall) -> Result<ToolDecision, ExtensionError> {
            Ok(ToolDecision::Allow)
        }
        async fn after_tool(
            &self,
            _: ToolCall,
            output: ToolOutput,
        ) -> Result<ToolOutput, ExtensionError> {
            Ok(output)
        }
        async fn before_model(&self, _: Turn) -> Result<TurnDecision, ExtensionError> {
            Ok(TurnDecision::Continue)
        }
        async fn on_input(&self, _: Input) -> Result<InputDecision, ExtensionError> {
            Ok(InputDecision::Pass)
        }
        async fn on_stop(&self, _: Stop) -> Result<StopDecision, ExtensionError> {
            Ok(StopDecision::Accept)
        }
        async fn finish(&self, _: RunOutcome, _: RunInfo) -> Result<(), ExtensionError> {
            Ok(())
        }
        fn events(&self, run: &RunInfo, _: Option<Duration>) -> Option<Box<dyn EventSink>> {
            let sink = sink(self.breaks, run);
            *self.failed.lock().unwrap() = Some(sink.failed.clone());
            Some(Box::new(sink))
        }
    }

    fn tool_end() -> AgentEvent {
        AgentEvent::ToolExecutionEnd {
            tool_call_id: "c1".into(),
            tool_name: "act".into(),
            result: ToolResult {
                content: vec![],
                details: Json::Null,
            },
            is_error: false,
        }
    }

    async fn until_failed(failed: &Failed) -> String {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(why) = failed.lock().unwrap().clone() {
                    break why;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the delivery failure is recorded")
    }

    /// One event through a run of the bridge's extension: what the sink
    /// recorded, what `take_failure` handed yoagent, and the decision on a
    /// later tool call.
    async fn observe(breaks: Breaks, required: bool) -> (String, Option<String>, ToolDecision) {
        let root = rutis::Ctx::root().unwrap();
        let bridge = crate::RutisBridge::install(&root).unwrap();
        let handler = Arc::new(Broken {
            breaks,
            failed: Mutex::new(None),
        });
        bridge
            .registry()
            .insert(
                "broken".into(),
                "test".into(),
                handler.clone(),
                CancellationToken::new(),
            )
            .unwrap();
        let extension = if required {
            bridge.extension().required()
        } else {
            bridge.extension()
        };
        let cancel = CancellationToken::new();
        let hooks = extension
            .start_run(&RunContext::new("run-1", &[], &cancel))
            .await
            .unwrap();
        hooks.on_event(&tool_end());
        let failed = handler.failed.lock().unwrap().clone().unwrap();
        let recorded = until_failed(&failed).await;
        let args = json!({});
        let decision = hooks
            .before_tool(&ToolCallRequest::new("c2", "act", &args))
            .await;
        let taken = hooks.take_failure();
        drop(hooks);
        root.shutdown().await.unwrap();
        (recorded, taken, decision)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_panicking_delivery_is_the_handlers_failure() {
        let (recorded, taken, decision) = observe(Breaks::Panics, true).await;
        assert!(
            recorded.contains("`broken` panicked in `on_event`: delivery bug"),
            "{recorded}"
        );
        assert!(
            matches!(&decision, ToolDecision::Deny(r) if r.contains("delivery bug")),
            "{decision:?}"
        );
        assert_eq!(
            taken.as_deref(),
            Some(recorded.as_str()),
            "required: fails the run"
        );

        let (recorded, taken, decision) = observe(Breaks::Panics, false).await;
        assert!(recorded.contains("delivery bug"), "{recorded}");
        assert_eq!(taken, None, "advisory: logged only");
        assert!(matches!(decision, ToolDecision::Allow), "{decision:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_delivery_task_that_died_is_the_handlers_failure() {
        let (recorded, taken, decision) = observe(Breaks::TaskDies, true).await;
        assert!(
            recorded.contains("`broken`") && recorded.contains("event delivery ended"),
            "{recorded}"
        );
        assert!(
            matches!(&decision, ToolDecision::Deny(r) if r.contains("event delivery ended")),
            "{decision:?}"
        );
        assert_eq!(
            taken.as_deref(),
            Some(recorded.as_str()),
            "required: fails the run"
        );

        let (recorded, taken, decision) = observe(Breaks::TaskDies, false).await;
        assert!(recorded.contains("event delivery ended"), "{recorded}");
        assert_eq!(taken, None, "advisory: logged only");
        assert!(matches!(decision, ToolDecision::Allow), "{decision:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_flush_finding_the_delivery_task_gone_records_it() {
        let sink = sink(Breaks::TaskDies, &RunInfo::new("run-1"));
        sink.flush().await;
        let why = sink.failure().expect("recorded");
        assert!(why.contains("event delivery ended"), "{why}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_working_delivery_records_nothing() {
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let sink = RemoteEvents::start(
            "fine".into(),
            Arc::new(vec!["toolExecutionEnd".to_string()]),
            RunInfo::new("run-1"),
            None,
            Arc::new({
                let delivered = delivered.clone();
                move |event| -> BoxFuture<'static, Result<Json, ExtensionError>> {
                    delivered.lock().unwrap().push(event);
                    Box::pin(async { Ok(Json::Null) })
                }
            }),
        );
        sink.send(&tool_end());
        sink.flush().await;
        assert_eq!(delivered.lock().unwrap().len(), 1);
        assert_eq!(sink.failure(), None);
    }

    #[test]
    fn an_object_argument_carries_the_cancel_handle() {
        let Value::Record(fields) = with_signal(json!({"tool": "t", "args": {"a": 1}})) else {
            panic!("an object becomes a record");
        };
        assert!(matches!(fields.get(SIGNAL_FIELD), Some(Value::Signal)));
        assert!(matches!(fields.get("tool"), Some(Value::Data(t)) if t == "t"));
        assert!(matches!(fields.get("args"), Some(Value::Data(a)) if a == &json!({"a": 1})));
        assert_eq!(fields.len(), 3);
        // Anything else is passed as it is.
        assert!(matches!(with_signal(json!("text")), Value::Data(t) if t == "text"));
    }
}
