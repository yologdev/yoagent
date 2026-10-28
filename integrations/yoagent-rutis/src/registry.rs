//! The `yoagent` registry: the handlers plugins registered, tied to the
//! plugins' lifecycles.
//!
//! rutis holds one service per type key, so plugins cannot each `provide`
//! their own. The bridge provides one [`Registry`] service, and plugins
//! **add handlers to it**. Every handler a Rust plugin registers is tied to a
//! cleanup on that plugin's fiber (`Ctx::effect_named`), so rutis removes it
//! exactly when the plugin unloads — on `dispose`, on a restart, on a config
//! update, and on a dependency-driven eviction.
//!
//! # A plugin that unloads during a run
//!
//! A run works with the handlers registered when it started (a snapshot).
//! Each handler remembers its plugin generation's cancellation token, which
//! rutis cancels at the start of an unload, before any cleanup runs. From
//! then on, for the rest of the runs that hold it, the handler is
//! **unavailable**:
//!
//! - its tool calls fail: one that **starts** afterwards with *"no longer
//!   available"*, one **in flight** is abandoned with *"plugin unloaded
//!   during the call"*;
//! - its `before_tool` denies every call, its `on_input` rejects, its
//!   `after_tool` withholds the result (fail closed — but an unload is not a
//!   failure, so a [`required`](crate::RutisExtension::required) run goes on);
//! - its `before_model`, `on_stop`, `on_event` and `finish` are skipped —
//!   except the run's `AgentEnd` (yoagent sends it after `finish`), which
//!   still goes to a handler the run's `finish` reached before its plugin
//!   began unloading.
//!
//! The same holds for a restart or config update: the run keeps the old
//! generation's handler and gets those errors; the new generation's handler
//! serves the next run. The bridge never rebinds a run to a newer generation.
//!
//! # Restricted handlers: tools only
//!
//! A handler registered from a runtime the host marked restricted
//! (`RutisBridge::restrict_runtime` (with a language feature),
//! for plugins it does not trust beyond their tools) gets only:
//!
//! - `tools` and its tool calls (`call_tool`);
//! - `before_tool` for calls to **its own** tools, called inside the tool
//!   when it runs — after every policy has passed the call — under the
//!   tool's own name, with the call alone (no request text, no
//!   conversation): it can deny its own call or adjust its arguments, and
//!   never sees another handler's or the agent's calls (a tool that merely
//!   shares a name with one of its own included). It is no policy for
//!   [`require_policy`](crate::RutisExtension::require_policy);
//! - `finish`.
//!
//! Its `after_tool`, `before_model`, `on_input`, `on_stop` and `on_event` are
//! never called, and it never gets the run's conversation. Its tools are
//! offered as `<handler>__<tool>` unless the host names them
//! ([`RutisExtension::with_restricted_tools`](crate::RutisExtension::with_restricted_tools)),
//! after every trusted handler's (a name clash drops the restricted one). Its
//! `tools` failing offers nothing; it never fails a run. A trusted handler
//! registering a handler name a restricted one holds evicts it; a restricted
//! one never takes a trusted one's. Static tool names are unique among
//! trusted handlers only (a restricted handler's are offered under other
//! names). Cancelling the run ends its check, and the tool does not run.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rutis::{CordisError, Ctx, Disposer, Effect};
use serde::Serialize;
use tokio_util::sync::CancellationToken;
use yoagent::AgentTool;

use crate::handler::{Handler, HandlerImpl, Hooks};

/// The rutis service plugins register handlers with.
///
/// Installed once per rutis root by
/// [`RutisBridge::install`](crate::RutisBridge::install). Plugins normally
/// use [`PluginCtxExt::register_handler`](crate::PluginCtxExt::register_handler)
/// (or [`AgentPlugin`](crate::AgentPlugin)) rather than calling
/// [`register`](Self::register) directly.
///
/// **Unique names.** A handler whose name a live handler holds, or one
/// offering a static tool ([`Handler::with_tool`]) whose name another live
/// handler's static tool holds, is refused with
/// [`CordisError::ServiceExists`] (the error rutis uses for a doubly-provided
/// service) and logged with `warn!` naming the holder: rutis keeps a failed
/// `apply`'s error on the fiber, not in its error sink, so the log is where a
/// host notices it. There is **no automatic retry**: when the holder later
/// unloads, the refused plugin stays failed until something restarts it.
#[derive(Default)]
pub struct Registry {
    inner: Mutex<Inner>,
    timings: Arc<Timings>,
    /// Per run in progress: which handler offered each plugin tool.
    offers: Mutex<HashMap<String, Arc<HashMap<String, ToolOwner>>>>,
    /// Extensions whose late tools are in use: told of restricted handlers.
    late: Mutex<Vec<std::sync::Weak<crate::late::Late>>>,
    /// The runtimes whose handlers are restricted (tools only).
    restricted_runtimes: Mutex<Vec<rutis_bridge_handle::RuntimeHandle>>,
    /// Every session a restricted runtime has run as, seen so far: a call
    /// from an earlier process (a restart's) or one seen while the runtime
    /// is between states still reads as restricted.
    #[cfg_attr(
        not(any(
            feature = "node",
            feature = "python",
            feature = "bun",
            feature = "go",
            feature = "websocket"
        )),
        allow(dead_code)
    )]
    restricted_sessions: Mutex<std::collections::HashSet<String>>,
}

/// `RuntimeHandle`, when a language feature brings rutis-bridge in; a type
/// that never exists otherwise (no runtime can be restricted then).
mod rutis_bridge_handle {
    #[cfg(any(
        feature = "node",
        feature = "python",
        feature = "bun",
        feature = "go",
        feature = "websocket"
    ))]
    pub(crate) use rutis_bridge::runtime::RuntimeHandle;
    #[cfg(not(any(
        feature = "node",
        feature = "python",
        feature = "bun",
        feature = "go",
        feature = "websocket"
    )))]
    pub(crate) enum RuntimeHandle {}
}

#[derive(Default)]
struct Inner {
    next_seq: u64,
    /// In registration order (ascending `seq`).
    entries: Vec<Entry>,
}

struct Entry {
    seq: u64,
    name: String,
    owner: String,
    handler: Arc<dyn HandlerImpl>,
    tool_names: Vec<String>,
    gate: Gate,
    /// Tools only (see the module docs).
    restricted: bool,
}

/// The hooks a handler gets: all it implements, or, restricted, only
/// `tools`, `before_tool` (for its own tools, decided per call) and
/// `finish` — and never the conversation.
pub(crate) fn granted(hooks: Hooks, restricted: bool) -> Hooks {
    if !restricted {
        return hooks;
    }
    Hooks {
        tools: hooks.tools,
        before_tool: hooks.before_tool,
        finish: hooks.finish,
        ..Hooks::default()
    }
}

/// Whether a registered handler can still be called: its plugin generation
/// has not begun unloading, and it has not been removed.
#[derive(Clone)]
pub(crate) struct Gate {
    generation: CancellationToken,
    removed: CancellationToken,
}

impl Gate {
    pub(crate) fn new(generation: CancellationToken) -> Self {
        Self {
            generation,
            removed: CancellationToken::new(),
        }
    }

    pub(crate) fn is_available(&self) -> bool {
        !self.generation.is_cancelled() && !self.removed.is_cancelled()
    }

    /// Resolves once the handler becomes unavailable.
    pub(crate) async fn gone(&self) {
        tokio::select! {
            _ = self.generation.cancelled() => {}
            _ = self.removed.cancelled() => {}
        }
    }

    /// Mark the handler unavailable (removed from the registry).
    pub(crate) fn remove(&self) {
        self.removed.cancel();
    }
}

/// One registered handler, as a run holds it.
#[derive(Clone)]
pub(crate) struct Registered {
    pub(crate) name: Arc<str>,
    pub(crate) owner: Arc<str>,
    pub(crate) handler: Arc<dyn HandlerImpl>,
    pub(crate) hooks: Hooks,
    pub(crate) gate: Gate,
    pub(crate) timings: Arc<Timings>,
    /// Tools only: `hooks` are already narrowed; `before_tool` is called
    /// for its own tools alone.
    pub(crate) restricted: bool,
}

impl Registered {
    /// Time one call of `hook` on this handler, from now until the
    /// returned timer's `done` (or its drop: a call yoagent abandoned).
    pub(crate) fn time(&self, hook: &'static str) -> Timer {
        self.timer(hook, None)
    }

    /// Time one execution of the plugin tool `tool`.
    pub(crate) fn time_tool(&self, tool: &str) -> Timer {
        self.timer("tool", Some(tool.to_owned()))
    }

    fn timer(&self, hook: &'static str, tool: Option<String>) -> Timer {
        Timer {
            key: Some(Key {
                handler: self.name.clone(),
                owner: self.owner.clone(),
                hook,
                tool,
            }),
            timings: self.timings.clone(),
            start: Instant::now(),
        }
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Key {
    handler: Arc<str>,
    owner: Arc<str>,
    hook: &'static str,
    tool: Option<String>,
}

/// A hook call being timed ([`Registered::time`]). Dropped before `done`,
/// it records the call with the time it had taken: as failed while a panic
/// unwinds through it (a Rust plugin tool that panicked), else as
/// cancelled (the call abandoned by yoagent: a hook of a cancelled run, a
/// `finish` cut off by yoagent's limit).
pub(crate) struct Timer {
    key: Option<Key>,
    timings: Arc<Timings>,
    start: Instant,
}

impl Timer {
    /// The call ended, as `outcome` says.
    pub(crate) fn done(mut self, outcome: Outcome) {
        if let Some(key) = self.key.take() {
            self.timings.record(key, self.start.elapsed(), outcome);
        }
    }

    /// Nothing was called after all (an event the handler does not take).
    pub(crate) fn discard(mut self) {
        self.key = None;
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        if let Some(key) = self.key.take() {
            let outcome = if std::thread::panicking() {
                Outcome::Failed
            } else {
                Outcome::Cancelled
            };
            self.timings.record(key, self.start.elapsed(), outcome);
        }
    }
}

/// How a timed call ended.
#[derive(Clone, Copy)]
pub(crate) enum Outcome {
    Ok,
    Failed,
    Cancelled,
}

impl Outcome {
    /// `Ok` when the call answered, else `Failed`.
    pub(crate) fn answered(ok: bool) -> Self {
        if ok {
            Outcome::Ok
        } else {
            Outcome::Failed
        }
    }
}

/// The time one handler spent in one kind of hook (or one of its tools),
/// as the agent waited for it: measured at the bridge, around each call, so
/// for a language plugin it includes the round trip to its runtime.
/// Cumulative since the bridge was installed, or since the last
/// [`Registry::take_timings`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct HookTiming {
    /// The handler's name.
    pub handler: String,
    /// The plugin that registered it.
    pub owner: String,
    /// `tools`, `on_input`, `before_model`, `before_tool`, `after_tool`,
    /// `on_stop`, `on_event`, `finish`, or `tool` (an execution of one of
    /// the handler's tools, named in [`tool`](Self::tool)). For a language
    /// plugin, `on_event` is the time to queue an event it subscribed to:
    /// the plugin handles it later, off the agent's path; events it does not
    /// take are not counted.
    pub hook: &'static str,
    /// The tool, when `hook` is `tool`.
    pub tool: Option<String>,
    /// Calls made (answered, failed and cancelled).
    pub calls: u64,
    /// Calls that did not answer: an error, a panic, a timeout, or the
    /// plugin unloading during the call. For `tool`, any error the tool
    /// returned other than [`ToolError::Cancelled`](yoagent::ToolError), and
    /// a panic. For `on_event`, a panic in a Rust handler only (a language
    /// plugin's late delivery failure is logged and switches its events
    /// off, as before).
    pub failed: u64,
    /// Calls ended by cancellation before they answered: a hook abandoned
    /// when its run was cancelled, a `finish` cut off by yoagent's 5 s
    /// limit (with the default turn timeout, also 5 s, either limit can
    /// end it first: a slow `finish` may count as failed instead), or a
    /// tool that returned [`ToolError::Cancelled`](yoagent::ToolError).
    /// Their time up to then is in `total` and `max`.
    pub cancelled: u64,
    /// Time spent in all of them.
    pub total: Duration,
    /// The longest one.
    pub max: Duration,
}

/// Every handler's [`HookTiming`]s, kept for the whole registry.
#[derive(Default)]
pub(crate) struct Timings(Mutex<BTreeMap<Key, Stat>>);

#[derive(Default, Clone, Copy)]
struct Stat {
    calls: u64,
    failed: u64,
    cancelled: u64,
    total: Duration,
    max: Duration,
}

impl Timings {
    fn record(&self, key: Key, took: Duration, outcome: Outcome) {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let stat = map.entry(key).or_default();
        stat.calls += 1;
        match outcome {
            Outcome::Ok => {}
            Outcome::Failed => stat.failed += 1,
            Outcome::Cancelled => stat.cancelled += 1,
        }
        stat.total += took;
        stat.max = stat.max.max(took);
    }

    fn list(&self, reset: bool) -> Vec<HookTiming> {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let out = map
            .iter()
            .map(|(k, s)| HookTiming {
                handler: k.handler.to_string(),
                owner: k.owner.to_string(),
                hook: k.hook,
                tool: k.tool.clone(),
                calls: s.calls,
                failed: s.failed,
                cancelled: s.cancelled,
                total: s.total,
                max: s.max,
            })
            .collect();
        if reset {
            map.clear();
        }
        out
    }
}

/// The plugin handler that offered a run one of its tools
/// ([`Registry::offered_by`], [`ToolCall::offered_by`](crate::ToolCall::offered_by)).
///
/// `handler` and `owner` come from the bridge: the handler's registered name
/// and the plugin it registered under — for a Rust plugin, the rutis plugin's
/// name; for a language plugin, the loader id of the row that registered it
/// (rutis 0.12's `session::caller_row`, seen by the host, not said by the
/// plugin), or its runtime session for a call from outside any row. Copies
/// of one row in several instances of an instanced group share its id, and
/// row ids and Rust plugin names (`Plugin::name()`) are one name space. `source` is what the handler
/// says about the tool: the pi adapter gives the extension file that
/// registered it; a language plugin may give any string. It is
/// self-declared — a plugin could name another's — so a policy keyed on it
/// trusts the plugins of that handler as much as the handler itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolOwner {
    pub(crate) handler: String,
    pub(crate) owner: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) source: Option<String>,
    /// The plugin's own name for the tool, when the host offers it under
    /// another ([`RutisExtension::with_restricted_tools`](crate::RutisExtension::with_restricted_tools)).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) plugin_name: Option<String>,
}

impl ToolOwner {
    /// One, for testing a policy outside an agent.
    pub fn new(handler: impl Into<String>, owner: impl Into<String>) -> Self {
        Self {
            handler: handler.into(),
            owner: owner.into(),
            source: None,
            plugin_name: None,
        }
    }

    /// With what the handler said about the tool's origin.
    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    /// The handler's registered name.
    pub fn handler(&self) -> &str {
        &self.handler
    }

    /// The plugin the handler registered under ([`HandlerInfo::owner`]).
    pub fn owner(&self) -> &str {
        &self.owner
    }

    /// Where the handler says the tool came from (self-declared; see the
    /// type docs), if it said.
    pub fn source(&self) -> Option<&str> {
        self.source.as_deref()
    }

    /// The plugin's own name for the tool, when the host offers it under
    /// another; `None` when they are the same.
    pub fn plugin_name(&self) -> Option<&str> {
        self.plugin_name.as_deref()
    }
}

/// A registered handler, for diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandlerInfo {
    name: String,
    owner: String,
    hooks: Vec<&'static str>,
    tools: Vec<String>,
    restricted: bool,
}

impl HandlerInfo {
    /// The handler's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The plugin that registered it.
    pub fn owner(&self) -> &str {
        &self.owner
    }

    /// The hooks it implements (`"before_tool"`, `"on_stop"`, ...).
    pub fn hooks(&self) -> &[&'static str] {
        &self.hooks
    }

    /// The static tools it offers.
    pub fn tools(&self) -> &[String] {
        &self.tools
    }

    /// Registered from a restricted runtime: tools only (see the
    /// [`registry`](crate::registry) docs). Its [`hooks`](Self::hooks) are
    /// the ones it gets.
    pub fn restricted(&self) -> bool {
        self.restricted
    }
}

/// The plugin's display name, for messages (a diagnostics scan: only on the
/// registration path, never per call).
fn plugin_name(ctx: &Ctx) -> String {
    let instance = ctx.instance();
    ctx.diagnostics()
        .plugins
        .into_iter()
        .find(|p| p.instance == instance)
        .map(|p| p.name)
        .unwrap_or_else(|| format!("instance {instance}"))
}

impl Registry {
    /// Register `handler`, owned by the plugin whose context `ctx` is.
    ///
    /// The handler is removed when that plugin unloads; the returned
    /// [`Disposer`] removes it earlier (dropping it does nothing). Refused
    /// with [`CordisError::ServiceExists`] on a name clash (see the type
    /// docs), and with rutis's own error when the plugin is no longer loading
    /// or active — in both cases nothing stays registered.
    pub fn register(
        self: &Arc<Self>,
        ctx: &Ctx,
        handler: Handler,
    ) -> Result<Disposer, CordisError> {
        let name = handler.name().to_string();
        let owner = plugin_name(ctx);
        let (seq, _gate) = self.insert(
            name.clone(),
            owner,
            Arc::new(handler),
            ctx.cancellation_token(),
            false,
        )?;
        let registry = Arc::downgrade(self);
        let registered = ctx.effect_named(format!("yoagent handler: {name}"), move || {
            Effect::Disposer(Box::new(move || {
                if let Some(registry) = registry.upgrade() {
                    registry.remove(seq);
                }
                Ok(())
            }))
        });
        if registered.is_err() {
            // The fiber is no longer loading/active: nothing owns the entry.
            self.remove(seq);
        }
        registered
    }

    /// Add a handler whose lifetime the caller manages (via `remove`).
    pub(crate) fn insert(
        &self,
        name: String,
        owner: String,
        handler: Arc<dyn HandlerImpl>,
        generation: CancellationToken,
        restricted: bool,
    ) -> Result<(u64, Gate), CordisError> {
        let tool_names: Vec<String> = handler
            .static_tools()
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        // A restricted holder never keeps a handler name a trusted plugin
        // wants: it is evicted (a restricted plugin must not lock a policy
        // out by squatting its name). Static tool names are unique among
        // trusted handlers; a restricted handler's are offered under other
        // names (`<handler>__<tool>`, or the host's), so their clashes are
        // settled when offered. Any other clash refuses.
        let mut evict: Vec<u64> = Vec::new();
        for tool in &tool_names {
            if tool_names.iter().filter(|t| *t == tool).count() > 1 {
                tracing::warn!(tool = %tool, refused = %owner, "two static tools of one handler with the same name; registration refused");
                return Err(CordisError::ServiceExists(format!(
                    "yoagent tool `{tool}` (already provided by plugin `{owner}`)"
                )));
            }
            if restricted {
                continue;
            }
            if let Some(holder) = inner
                .entries
                .iter()
                .find(|e| !e.restricted && e.tool_names.contains(tool))
            {
                tracing::warn!(
                    tool = %tool,
                    holder = %holder.owner,
                    refused = %owner,
                    "plugin tool name already taken; registration refused \
                     (the refused plugin is not retried when the holder unloads)"
                );
                return Err(CordisError::ServiceExists(format!(
                    "yoagent tool `{tool}` (already provided by plugin `{}`)",
                    holder.owner
                )));
            }
        }
        if let Some(existing) = inner.entries.iter().find(|e| e.name == name) {
            if existing.restricted && !restricted {
                evict.push(existing.seq);
            } else {
                tracing::warn!(
                    handler = %name,
                    holder = %existing.owner,
                    refused = %owner,
                    "plugin handler name already taken; registration refused \
                     (the refused plugin is not retried when the holder unloads)"
                );
                return Err(CordisError::ServiceExists(format!(
                    "yoagent handler `{name}` (already registered by plugin `{}`)",
                    existing.owner
                )));
            }
        }
        for seq in evict {
            if let Some(at) = inner.entries.iter().position(|e| e.seq == seq) {
                let evicted = inner.entries.remove(at);
                tracing::warn!(
                    handler = %evicted.name,
                    holder = %evicted.owner,
                    by = %owner,
                    "a restricted plugin held a name a trusted plugin registers: the restricted handler is removed"
                );
                evicted.gate.remove();
            }
        }
        let seq = inner.next_seq;
        inner.next_seq += 1;
        let gate = Gate::new(generation);
        inner.entries.push(Entry {
            seq,
            name,
            owner,
            handler,
            tool_names,
            gate: gate.clone(),
            restricted,
        });
        if restricted {
            let registered = self.registered(inner.entries.last().expect("just pushed"));
            drop(inner);
            let late: Vec<_> = self
                .late
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .filter_map(std::sync::Weak::upgrade)
                .collect();
            for late in late {
                late.registered(registered.clone());
            }
        }
        Ok((seq, gate))
    }

    /// Remove the entry `seq` (if still there) and mark it unavailable.
    pub(crate) fn remove(&self, seq: u64) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(at) = inner.entries.iter().position(|e| e.seq == seq) {
            inner.entries.remove(at).gate.remove();
        }
    }

    /// The handlers a run starting now would get, in registration order. A
    /// plugin that has begun unloading (its generation cancelled, cleanups
    /// not yet finished) contributes nothing, though its names stay taken
    /// until its cleanup runs.
    pub(crate) fn snapshot(&self) -> Vec<Registered> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner
            .entries
            .iter()
            .filter(|e| e.gate.is_available())
            .map(|e| self.registered(e))
            .collect()
    }

    fn registered(&self, e: &Entry) -> Registered {
        Registered {
            name: e.name.as_str().into(),
            owner: e.owner.as_str().into(),
            hooks: granted(e.handler.hooks(), e.restricted),
            handler: e.handler.clone(),
            gate: e.gate.clone(),
            timings: self.timings.clone(),
            restricted: e.restricted,
        }
    }

    /// Tell `late` of every restricted handler registered from now on.
    pub(crate) fn watch_late(&self, late: &Arc<crate::late::Late>) {
        let mut watching = self.late.lock().unwrap_or_else(|e| e.into_inner());
        watching.retain(|w| w.strong_count() > 0);
        watching.push(Arc::downgrade(late));
    }

    /// Add late tools' owners to the run's record ([`offered_by`](Self::offered_by)).
    pub(crate) fn add_offers(&self, run_id: &str, more: HashMap<String, ToolOwner>) {
        let mut offers = self.offers.lock().unwrap_or_else(|e| e.into_inner());
        let mut all = offers
            .get(run_id)
            .map(|first| (**first).clone())
            .unwrap_or_default();
        all.extend(more);
        offers.insert(run_id.to_string(), Arc::new(all));
    }

    /// Mark `runtime` restricted: see
    /// `RutisBridge::restrict_runtime` (with a language feature).
    #[cfg_attr(
        not(any(
            feature = "node",
            feature = "python",
            feature = "bun",
            feature = "go",
            feature = "websocket"
        )),
        allow(dead_code)
    )]
    pub(crate) fn restrict_runtime(&self, runtime: rutis_bridge_handle::RuntimeHandle) {
        self.restricted_runtimes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(runtime);
    }

    /// Whether the session `tag` is the running process of a restricted
    /// runtime. A plugin's code runs only once its runtime is ready, so a
    /// call from it finds its runtime ready, with this session's tag.
    #[cfg(any(
        feature = "node",
        feature = "python",
        feature = "bun",
        feature = "go",
        feature = "websocket"
    ))]
    pub(crate) fn is_restricted_session(&self, tag: &str) -> bool {
        use rutis_bridge::runtime::RuntimeState;
        let runtimes = self
            .restricted_runtimes
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut sessions = self
            .restricted_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut settling = false;
        for runtime in runtimes.iter() {
            match runtime.state() {
                RuntimeState::Ready(process) => {
                    sessions.insert(process.connection().tag().to_string());
                }
                RuntimeState::Starting => settling = true,
                _ => {}
            }
        }
        let restricted = sessions.contains(tag);
        if !restricted && settling {
            tracing::debug!(
                session = %tag,
                "a handler registered while a restricted runtime is starting, from a session not known \
                 to be restricted: registered unrestricted"
            );
        }
        restricted
    }

    /// The time each handler spent in each kind of hook, as the agent
    /// waited for it, sorted by handler, owner and hook: cumulative since
    /// the bridge was installed or the last [`take_timings`](Self::take_timings).
    /// Cheap to keep on (two clock reads and a short lock per call); for a
    /// whole-session view.
    pub fn timings(&self) -> Vec<HookTiming> {
        self.timings.list(false)
    }

    /// [`timings`](Self::timings), and start counting from zero: called at a
    /// run's end, the figures of that run alone (with runs one at a time;
    /// with the bridge as a tree extension, the sub-agent runs it served
    /// included).
    pub fn take_timings(&self) -> Vec<HookTiming> {
        self.timings.list(true)
    }

    /// The handlers a run starting now would get, in registration order.
    pub fn handlers(&self) -> Vec<HandlerInfo> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner
            .entries
            .iter()
            .filter(|e| e.gate.is_available())
            .map(|e| HandlerInfo {
                name: e.name.clone(),
                owner: e.owner.clone(),
                hooks: granted(e.handler.hooks(), e.restricted).names(),
                tools: e.tool_names.clone(),
                restricted: e.restricted,
            })
            .collect()
    }

    /// Names of the static tools a run starting now would get, in
    /// registration order.
    pub fn tool_names(&self) -> Vec<String> {
        self.handlers().into_iter().flat_map(|h| h.tools).collect()
    }

    /// The plugin handler that offered the run `run_id` its tool `tool`:
    /// for a policy that trusts a tool by where it came from (a run id is
    /// in yoagent's `RunContext::run_id`, and in every hook argument's
    /// [`RunInfo`](crate::RunInfo)). Known from the run's `tools` until the
    /// run ends; `None` for a tool no plugin handler offered (the agent's
    /// own), or another run's.
    ///
    /// The bridge answers for the tools **it** offered. When the agent
    /// itself, or an extension before the bridge, has a tool of the same
    /// name, yoagent keeps that one and drops the plugin's (with a
    /// warning) — and a call of that name still reads as the plugin's here.
    /// A host that scopes trust by plugin should keep its own tool names
    /// apart from its plugins' (yoagent logs every such drop).
    pub fn offered_by(&self, run_id: &str, tool: &str) -> Option<ToolOwner> {
        let offers = self.offers.lock().unwrap_or_else(|e| e.into_inner());
        offers.get(run_id)?.get(tool).cloned()
    }

    /// Record what the run `run_id` was offered, returning the record the
    /// run reads and whether this call made it (and so must forget it).
    /// A run with an earlier record — a second extension of this bridge in
    /// the same agent — keeps the first: yoagent keeps the earlier
    /// extension's tools on a name clash.
    pub(crate) fn record_offers(
        &self,
        run_id: &str,
        offered: HashMap<String, ToolOwner>,
    ) -> (Arc<HashMap<String, ToolOwner>>, bool) {
        let mut offers = self.offers.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(first) = offers.get(run_id) {
            return (first.clone(), false);
        }
        let offered = Arc::new(offered);
        if offered.is_empty() {
            return (offered, false);
        }
        offers.insert(run_id.to_owned(), offered.clone());
        (offered, true)
    }

    /// The run `run_id` ended: forget what it was offered.
    pub(crate) fn forget_offers(&self, run_id: &str) {
        let mut offers = self.offers.lock().unwrap_or_else(|e| e.into_inner());
        offers.remove(run_id);
    }
}

/// A plugin tool bound to the handler (and plugin generation) that offered it.
pub(crate) struct LiveTool {
    pub(crate) inner: Arc<dyn AgentTool>,
    /// A restricted handler's own check of its own tool's calls: its
    /// `before_tool`, called here, when the tool really runs (after every
    /// policy), never from the extension's `before_tool` — so it can only
    /// ever see, deny or adjust a call of this tool.
    pub(crate) judge: Option<Judge>,
    /// The name it is offered under, when the host renamed it (the plugin
    /// is still called with its own).
    pub(crate) offered_as: Option<String>,
    pub(crate) gate: Gate,
    /// The handler that offered it, to time its executions.
    pub(crate) handler: Registered,
}

/// What a restricted handler's `before_tool` gets for its own tool's call.
pub(crate) struct Judge {
    pub(crate) run: crate::handler::RunInfo,
    pub(crate) owner: ToolOwner,
    pub(crate) timeout: Option<Duration>,
}

impl LiveTool {
    fn gone(&self) -> yoagent::ToolError {
        yoagent::ToolError::Failed(format!(
            "tool `{}` is no longer available: the plugin that provided it was unloaded",
            self.inner.name()
        ))
    }
}

#[async_trait::async_trait]
impl AgentTool for LiveTool {
    fn name(&self) -> &str {
        self.offered_as
            .as_deref()
            .unwrap_or_else(|| self.inner.name())
    }
    fn label(&self) -> &str {
        self.inner.label()
    }
    fn description(&self) -> &str {
        self.inner.description()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        self.inner.parameters_schema()
    }
    async fn execute(
        &self,
        mut params: serde_json::Value,
        ctx: yoagent::ToolContext,
    ) -> Result<yoagent::ToolResult, yoagent::ToolError> {
        if !self.gate.is_available() {
            return Err(self.gone());
        }
        if let Some(judge) = &self.judge {
            // Its own name, its own call: no request text, no conversation.
            let call = crate::handler::ToolCall::new(
                ctx.tool_call_id.clone(),
                self.inner.name(),
                params.clone(),
            )
            .with_run(judge.run.clone())
            .with_offered_by(judge.owner.clone());
            // The run's cancel ends the check too (yoagent's own hooks race it).
            let decision = tokio::select! {
                decision = crate::extension::call(
                    &self.handler,
                    "before_tool",
                    judge.timeout,
                    self.handler.handler.before_tool(call),
                ) => decision,
                _ = ctx.cancel.cancelled() => return Err(yoagent::ToolError::Cancelled),
            };
            match decision {
                Ok(yoagent::ToolDecision::Allow) => {}
                Ok(yoagent::ToolDecision::Modify(args)) => params = args,
                Ok(yoagent::ToolDecision::Deny(reason)) => {
                    return Err(yoagent::ToolError::Failed(reason))
                }
                Err(missed) => return Err(yoagent::ToolError::Failed(missed.reason().to_string())),
            }
            if ctx.cancel.is_cancelled() {
                return Err(yoagent::ToolError::Cancelled);
            }
        }
        // A panic unwinds through `timer`, which records it as failed.
        let timer = self.handler.time_tool(self.inner.name());
        let result = tokio::select! {
            result = self.inner.execute(params, ctx) => result,
            _ = self.gate.gone() => Err(yoagent::ToolError::Failed(format!(
                "tool `{}` failed: its plugin unloaded during the call",
                self.inner.name()
            ))),
        };
        timer.done(match &result {
            Ok(_) => Outcome::Ok,
            Err(yoagent::ToolError::Cancelled) => Outcome::Cancelled,
            Err(_) => Outcome::Failed,
        });
        result
    }
}
