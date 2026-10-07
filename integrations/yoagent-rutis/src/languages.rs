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
//! An abandoned call (a timeout, a cancelled run, an unloaded plugin) is
//! dropped on the host side; a Python coroutine is then cancelled, but a
//! JavaScript function runs to completion (no `AbortSignal` is passed:
//! rutis-bridge passes one only as an extra positional argument, which a
//! Python method with a fixed signature would refuse), so a `call_tool` can
//! still act after its run was cancelled.
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
use std::sync::Arc;

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
            other => Err(invalid(format!(
                "the yoagent service has no method `{other}`"
            ))),
        }
    }

    fn methods(&self) -> Option<Json> {
        // Synchronous: `register` returns the disposer the plugin passes to
        // `ctx.effect`. It reads the handler's members back while the plugin
        // waits, on the same call chain.
        Some(json!({ "register": "sync" }))
    }
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
    async fn call(&self, hook: &str, args: Vec<Json>) -> Result<Json, ExtensionError> {
        let args = Value::List(args.into_iter().map(Value::Data).collect());
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
            .any(|k| !["text", "details", "is_error"].contains(&k.as_str()))
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
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Delivery>(EVENT_QUEUE);
        let target = self.target.clone();
        let name = self.name.clone();
        let failed = Arc::new(std::sync::Mutex::new(None::<String>));
        let failure = failed.clone();
        let run_id = run.run_id.clone();
        // One task per run and handler: events arrive in order, and the run
        // never waits for them. It ends once the run's hooks are dropped and
        // the queue is drained (`AgentEnd` comes after `finish`).
        tokio::spawn(async move {
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
                let call = target.call("on_event", vec![event]);
                let outcome = match limit {
                    Some(limit) => tokio::time::timeout(limit, call).await.unwrap_or_else(|_| {
                        Err(ExtensionError::new(format!(
                            "did not answer within {limit:?}"
                        )))
                    }),
                    None => call.await,
                };
                if let Err(error) = outcome {
                    let why = format!(
                        "plugin handler `{name}` failed in `on_event`: {error} (not called again this run)"
                    );
                    // Logged here too: a failure on the run's last events
                    // (`AgentEnd` comes after `finish`) is seen by nothing else.
                    tracing::warn!(run_id = %run_id, "{why}");
                    failure
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get_or_insert(why);
                }
            }
        });
        Some(Box::new(RemoteEvents {
            name: self.name.clone(),
            filter: self.events.clone(),
            run: run.clone(),
            tx,
            failed,
        }))
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

/// Queues a run's events (those of the subscribed types) for delivery.
struct RemoteEvents {
    name: String,
    filter: Arc<Vec<String>>,
    run: RunInfo,
    tx: tokio::sync::mpsc::Sender<Delivery>,
    /// The first delivery failure (an error, a timeout, a full queue): the
    /// handler's `on_event` is switched off for the run. Never raised as a
    /// panic: the extension reads it with [`EventSink::failure`].
    failed: Arc<std::sync::Mutex<Option<String>>>,
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
        if let Err(tokio::sync::mpsc::error::TrySendError::Full(_)) =
            self.tx.try_send(Delivery::Event(json))
        {
            self.failed
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get_or_insert(format!(
                    "plugin handler `{}` fell {EVENT_QUEUE} events behind in `on_event` (not called again this run)",
                    self.name
                ));
        }
    }

    fn failure(&self) -> Option<String> {
        self.failed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn flush(&self) -> futures::future::BoxFuture<'static, ()> {
        let (done, flushed) = tokio::sync::oneshot::channel();
        let tx = self.tx.clone();
        Box::pin(async move {
            if tx.send(Delivery::Flush(done)).await.is_ok() {
                let _ = flushed.await;
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
        let (text, details, is_error) = match &value {
            Json::String(text) => (text.clone(), Json::Null, false),
            // `{}` is an empty text, as `after_tool`'s edit reads it.
            Json::Object(fields)
                if fields
                    .keys()
                    .all(|k| ["text", "details", "is_error"].contains(&k.as_str())) =>
            {
                let text = match fields.get("text") {
                    None => String::new(),
                    Some(Json::String(text)) => text.clone(),
                    Some(_) => return Err(unexpected()),
                };
                let is_error = match fields.get("is_error") {
                    None => false,
                    Some(Json::Bool(is_error)) => *is_error,
                    Some(_) => return Err(unexpected()),
                };
                let details = fields.get("details").cloned().unwrap_or(Json::Null);
                (text, details, is_error)
            }
            _ => return Err(unexpected()),
        };
        if is_error {
            return Err(ToolError::Failed(text));
        }
        Ok(ToolResult {
            content: vec![Content::Text { text }],
            details,
        })
    }
}
