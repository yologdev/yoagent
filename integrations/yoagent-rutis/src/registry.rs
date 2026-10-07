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
//!   `after_tool` withholds the result (fail closed);
//! - its `before_model`, `on_stop`, `on_event` and `finish` are skipped.
//!
//! The same holds for a restart or config update: the run keeps the old
//! generation's handler and gets those errors; the new generation's handler
//! serves the next run. The bridge never rebinds a run to a newer generation.

use std::sync::{Arc, Mutex};

use rutis::{CordisError, Ctx, Disposer, Effect};
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
    pub(crate) handler: Arc<dyn HandlerImpl>,
    pub(crate) hooks: Hooks,
    pub(crate) gate: Gate,
}

/// A registered handler, for diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandlerInfo {
    name: String,
    owner: String,
    hooks: Vec<&'static str>,
    tools: Vec<String>,
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
    ) -> Result<(u64, Gate), CordisError> {
        let tool_names: Vec<String> = handler
            .static_tools()
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        for tool in &tool_names {
            let holder = inner
                .entries
                .iter()
                .find(|e| e.tool_names.contains(tool))
                .map(|e| e.owner.clone())
                .or_else(|| {
                    // Two static tools of one handler with the same name.
                    (tool_names.iter().filter(|t| *t == tool).count() > 1).then(|| owner.clone())
                });
            if let Some(holder) = holder {
                tracing::warn!(
                    tool = %tool,
                    holder = %holder,
                    refused = %owner,
                    "plugin tool name already taken; registration refused \
                     (the refused plugin is not retried when the holder unloads)"
                );
                return Err(CordisError::ServiceExists(format!(
                    "yoagent tool `{tool}` (already provided by plugin `{holder}`)"
                )));
            }
        }
        if let Some(existing) = inner.entries.iter().find(|e| e.name == name) {
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
        });
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
            .map(|e| Registered {
                name: e.name.as_str().into(),
                hooks: e.handler.hooks(),
                handler: e.handler.clone(),
                gate: e.gate.clone(),
            })
            .collect()
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
                hooks: e.handler.hooks().names(),
                tools: e.tool_names.clone(),
            })
            .collect()
    }

    /// Names of the static tools a run starting now would get, in
    /// registration order.
    pub fn tool_names(&self) -> Vec<String> {
        self.handlers().into_iter().flat_map(|h| h.tools).collect()
    }
}

/// A plugin tool bound to the handler (and plugin generation) that offered it.
pub(crate) struct LiveTool {
    pub(crate) inner: Arc<dyn AgentTool>,
    pub(crate) gate: Gate,
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
        self.inner.name()
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
        params: serde_json::Value,
        ctx: yoagent::ToolContext,
    ) -> Result<yoagent::ToolResult, yoagent::ToolError> {
        if !self.gate.is_available() {
            return Err(self.gone());
        }
        tokio::select! {
            result = self.inner.execute(params, ctx) => result,
            _ = self.gate.gone() => Err(yoagent::ToolError::Failed(format!(
                "tool `{}` failed: its plugin unloaded during the call",
                self.inner.name()
            ))),
        }
    }
}
