//! Tools contributed by plugins.
//!
//! rutis holds one service per type key, so plugins cannot each `provide` a
//! tool service under one key. Instead the bridge provides a single
//! [`ToolRegistry`] service, and plugins **add entries to it**. Every entry is
//! tied to a cleanup registered on the contributing plugin's fiber
//! (`Ctx::effect_named`), so rutis removes it exactly when that plugin
//! unloads — on `dispose`, on a restart, on a config update, and on a
//! dependency-driven eviction. [`PluginToolSource`] implements yoagent's
//! [`ToolSource`] over the registry: an agent sees exactly the tools of the
//! plugins active when its run starts.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use rutis::{CordisError, Ctx, Disposer, Effect, InstanceId};
use yoagent::{AgentTool, ToolContext, ToolError, ToolResult, ToolSource};

/// The rutis service through which plugins contribute tools.
///
/// Installed once per rutis root by [`RutisBridge::install`](crate::RutisBridge::install).
/// Plugins normally use [`PluginCtxExt::provide_tool`](crate::PluginCtxExt::provide_tool)
/// rather than calling [`register`](Self::register) directly.
///
/// **One tool per name.** A registration whose name is already held by a
/// live entry is refused with [`CordisError::ServiceExists`] (the same error
/// rutis uses for a doubly-provided service): a plugin that `?`s it fails to
/// load, which is visible in rutis diagnostics, instead of silently shadowing
/// or being shadowed. Tools the agent was built with win over plugin tools at
/// run start (yoagent's [`ToolSource`] collision rule).
#[derive(Default)]
pub struct ToolRegistry {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    next_seq: u64,
    entries: HashMap<String, Entry>,
}

struct Entry {
    seq: u64,
    tool: Arc<dyn AgentTool>,
    owner: InstanceId,
    live: Arc<AtomicBool>,
}

impl ToolRegistry {
    /// Add `tool`, owned by the plugin whose context `ctx` is.
    ///
    /// The entry is removed when that plugin unloads; the returned
    /// [`Disposer`] removes it earlier (dropping it does nothing). Refused
    /// with [`CordisError::ServiceExists`] when another live entry has the
    /// same name, and with rutis's own error when the plugin is no longer
    /// loading or active.
    pub fn register(
        self: &Arc<Self>,
        ctx: &Ctx,
        tool: Arc<dyn AgentTool>,
    ) -> Result<Disposer, CordisError> {
        let name = tool.name().to_string();
        let live = Arc::new(AtomicBool::new(true));
        let seq = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(existing) = inner.entries.get(&name) {
                return Err(CordisError::ServiceExists(format!(
                    "yoagent tool `{name}` (already provided by plugin instance {})",
                    existing.owner
                )));
            }
            let seq = inner.next_seq;
            inner.next_seq += 1;
            inner.entries.insert(
                name.clone(),
                Entry {
                    seq,
                    tool,
                    owner: ctx.instance(),
                    live: live.clone(),
                },
            );
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
        if inner.entries.get(name).is_some_and(|e| e.seq == seq) {
            if let Some(entry) = inner.entries.remove(name) {
                entry.live.store(false, Ordering::SeqCst);
            }
        }
    }

    /// Names of the registered tools, in registration order.
    pub fn names(&self) -> Vec<String> {
        self.snapshot()
            .into_iter()
            .map(|t| t.name().into())
            .collect()
    }

    /// The registered tools, in registration order. Each is wrapped so that a
    /// call made after its plugin unloaded (possible within a run that
    /// started earlier) fails with an error result instead of reaching a
    /// torn-down plugin.
    pub fn snapshot(&self) -> Vec<Arc<dyn AgentTool>> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut entries: Vec<&Entry> = inner.entries.values().collect();
        entries.sort_by_key(|e| e.seq);
        entries
            .into_iter()
            .map(|e| {
                Arc::new(LiveTool {
                    inner: e.tool.clone(),
                    live: e.live.clone(),
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

/// A plugin tool that refuses to run once its plugin unloaded.
struct LiveTool {
    inner: Arc<dyn AgentTool>,
    live: Arc<AtomicBool>,
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
        if !self.live.load(Ordering::SeqCst) {
            return Err(ToolError::Failed(format!(
                "tool `{}` is no longer available: the plugin that provided it was unloaded",
                self.inner.name()
            )));
        }
        self.inner.execute(params, ctx).await
    }
}
