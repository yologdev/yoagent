//! Tools contributed by plugins.
//!
//! rutis holds one service per type key, so plugins cannot each `provide` a
//! tool service under one key. Instead the bridge provides a single
//! [`ToolRegistry`] service, and plugins **add entries to it**. Every entry is
//! tied to a cleanup registered on the contributing plugin's fiber
//! (`Ctx::effect_named`), so rutis removes it exactly when that plugin
//! unloads — on `dispose`, on a restart, on a config update, and on a
//! dependency-driven eviction. [`PluginToolSource`] implements yoagent's
//! [`ToolSource`] over the registry: an agent sees the tools of the plugins
//! active (not unloading) when its run starts.
//!
//! # A plugin that unloads while its tool is in use
//!
//! An agent's tool list is fixed per run, so a run that started before an
//! unload still offers the tool. Each entry remembers the contributing
//! plugin generation's cancellation token, which rutis cancels at the start
//! of an unload (before any cleanup runs):
//!
//! - a call that **starts** after that fails with *"no longer available"*;
//! - a call **in flight** is abandoned (its future dropped) and fails with
//!   *"plugin unloaded during the call"*.
//!
//! The same holds for a restart or config update: the run keeps the old
//! generation's tool and gets the error; the new generation's tool — whose
//! schema may differ — is offered from the next run. The bridge never
//! silently rebinds a call to a newer generation.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use rutis::{CordisError, Ctx, Disposer, Effect};
use tokio_util::sync::CancellationToken;
use yoagent::{AgentTool, ToolContext, ToolError, ToolResult, ToolSource};

/// The rutis service through which plugins contribute tools.
///
/// Installed once per rutis root by [`RutisBridge::install`](crate::RutisBridge::install).
/// Plugins normally use [`PluginCtxExt::provide_tool`](crate::PluginCtxExt::provide_tool)
/// rather than calling [`register`](Self::register) directly.
///
/// **One tool per name.** A registration whose name is already held by a
/// live entry is refused with [`CordisError::ServiceExists`] (the same error
/// rutis uses for a doubly-provided service) and logged with `warn!` naming
/// the tool and its holder — rutis keeps a failed `apply`'s error on the
/// fiber (visible through `FiberView::state()` / diagnostics), not in its
/// error sink, so the log is where a host notices it. There is **no
/// automatic retry**: when the holder later unloads, the refused plugin
/// stays failed until something restarts it. Tools the agent was built with
/// win over plugin tools at run start (yoagent's [`ToolSource`] collision
/// rule).
#[derive(Default)]
pub struct ToolRegistry {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    next_seq: u64,
    /// In registration order (ascending `seq`); names are unique.
    entries: Vec<Entry>,
}

struct Entry {
    seq: u64,
    name: String,
    tool: Arc<dyn AgentTool>,
    owner: String,
    live: Arc<AtomicBool>,
    generation: CancellationToken,
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

impl ToolRegistry {
    /// Add `tool`, owned by the plugin whose context `ctx` is.
    ///
    /// The entry is removed when that plugin unloads; the returned
    /// [`Disposer`] removes it earlier (dropping it does nothing). Refused
    /// with [`CordisError::ServiceExists`] when another live entry has the
    /// same name, and with rutis's own error when the plugin is no longer
    /// loading or active — in both cases nothing stays registered.
    pub fn register(
        self: &Arc<Self>,
        ctx: &Ctx,
        tool: Arc<dyn AgentTool>,
    ) -> Result<Disposer, CordisError> {
        let name = tool.name().to_string();
        let owner = plugin_name(ctx);
        let live = Arc::new(AtomicBool::new(true));
        let seq = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(existing) = inner.entries.iter().find(|e| e.name == name) {
                tracing::warn!(
                    tool = %name,
                    holder = %existing.owner,
                    refused = %owner,
                    "plugin tool name already taken; registration refused \
                     (the refused plugin is not retried when the holder unloads)"
                );
                return Err(CordisError::ServiceExists(format!(
                    "yoagent tool `{name}` (already provided by plugin `{}`)",
                    existing.owner
                )));
            }
            let seq = inner.next_seq;
            inner.next_seq += 1;
            inner.entries.push(Entry {
                seq,
                name: name.clone(),
                tool,
                owner,
                live: live.clone(),
                generation: ctx.cancellation_token(),
            });
            seq
        };
        let registry = Arc::downgrade(self);
        let cleanup_name = name.clone();
        let registered = ctx.effect_named(format!("yoagent tool: {name}"), move || {
            Effect::Disposer(Box::new(move || {
                live.store(false, Ordering::SeqCst);
                if let Some(registry) = registry.upgrade() {
                    registry.remove_if(&cleanup_name, seq);
                }
                Ok(())
            }))
        });
        if registered.is_err() {
            // The fiber is no longer loading/active: nothing owns the entry.
            self.remove_if(&name, seq);
        }
        registered
    }

    fn remove_if(&self, name: &str, seq: u64) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(at) = inner
            .entries
            .iter()
            .position(|e| e.seq == seq && e.name == name)
        {
            inner.entries.remove(at).live.store(false, Ordering::SeqCst);
        }
    }

    /// Names of the tools a run starting now would get, in registration
    /// order.
    pub fn names(&self) -> Vec<String> {
        self.snapshot()
            .into_iter()
            .map(|t| t.name().into())
            .collect()
    }

    /// The tools of the plugins that are active now, in registration order.
    /// A plugin that has begun unloading (its generation cancelled, cleanups
    /// not yet finished) contributes nothing, though its name stays taken
    /// until its cleanup runs. Each tool is wrapped so that a call made after
    /// its plugin began unloading fails with an error result instead of
    /// reaching a torn-down plugin (see the [module docs](self)).
    pub fn snapshot(&self) -> Vec<Arc<dyn AgentTool>> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner
            .entries
            .iter()
            .filter(|e| !e.generation.is_cancelled() && e.live.load(Ordering::SeqCst))
            .map(|e| {
                Arc::new(LiveTool {
                    inner: e.tool.clone(),
                    live: e.live.clone(),
                    generation: e.generation.clone(),
                }) as Arc<dyn AgentTool>
            })
            .collect()
    }
}

/// yoagent [`ToolSource`] over a [`ToolRegistry`]: the tools of the plugins
/// active at the start of each run.
#[derive(Clone)]
pub struct PluginToolSource {
    registry: Arc<ToolRegistry>,
}

impl PluginToolSource {
    /// A source reading `registry`.
    pub fn new(registry: Arc<ToolRegistry>) -> Self {
        Self { registry }
    }
}

#[async_trait::async_trait]
impl ToolSource for PluginToolSource {
    async fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        self.registry.snapshot()
    }
}

/// A plugin tool bound to the generation that provided it.
struct LiveTool {
    inner: Arc<dyn AgentTool>,
    live: Arc<AtomicBool>,
    generation: CancellationToken,
}

impl LiveTool {
    fn gone(&self) -> ToolError {
        ToolError::Failed(format!(
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
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        if self.generation.is_cancelled() || !self.live.load(Ordering::SeqCst) {
            return Err(self.gone());
        }
        tokio::select! {
            result = self.inner.execute(params, ctx) => result,
            _ = self.generation.cancelled() => Err(ToolError::Failed(format!(
                "tool `{}` failed: its plugin unloaded during the call",
                self.inner.name()
            ))),
        }
    }
}
