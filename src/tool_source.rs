//! Tools resolved per run.
//!
//! An [`Agent`](crate::Agent)'s own tools ([`with_tools`](crate::Agent::with_tools))
//! are fixed until you replace them. A [`ToolSource`] is for tools that come
//! and go while the agent lives: a plugin system loading and unloading
//! extensions, an MCP server that reconnects with a different tool list, a
//! feature flag. The agent asks each source for its tools **once at the start
//! of every run** — every `prompt*` and `continue_loop*` call — and offers the
//! model the static tools plus whatever the sources returned.
//!
//! # Per run — and again when a source says it changed
//!
//! By default the list is fixed for the whole run: a tool that disappears
//! mid-run stays offered (and callable) until the run ends, and a tool that
//! appears mid-run is first offered on the next run.
//!
//! A source whose tools can change while a run is going (an MCP server that
//! announces `notifications/tools/list_changed`, a plugin loaded mid-run, a
//! tool that loads others on demand) reports it through
//! [`generation`](ToolSource::generation): a number that moves whenever its
//! tools change. Before each model request after the first — once every tool
//! call of the last turn has returned, so no call the model made is left
//! without its tool — the loop compares each source's generation with the one
//! it read the source's tools at, and asks again only the sources whose
//! generation moved. A source's new tools replace its old ones (a shorter
//! list too: a server that went away), and the name-collision rules below are
//! applied again over the whole list. Extensions'
//! [per-run tools](crate::extension::RunHooks::tools) are not asked again. A
//! source whose generation stays 0 (the default) is asked once per run, as
//! before. A tool the model calls after it was withdrawn is answered as an
//! unknown tool.
//!
//! **Prompt caching.** Tool definitions open the provider's cached prefix, so
//! any change to the offered set rewrites the whole cache — once per change,
//! mid-run included. Consulting once per run (and again only on a change)
//! *bounds* that cost; it does not prevent it. The same set in a different
//! order costs nothing: sourced tools are sorted by name (the agent's own
//! tools keep their order).
//!
//! A source whose tools can go stale mid-run should make them fail cleanly
//! (return an error result) rather than rely on being withdrawn.
//!
//! **No timeout.** The run waits for every source. On an
//! [`Agent`](crate::Agent), dropping the prompt future while it waits is the
//! escape hatch — sources are consulted before any agent state is touched, so
//! the agent is left as it was. A [`SubAgentTool`](crate::SubAgentTool)
//! stops waiting when the parent run is cancelled.
//!
//! # Name collisions
//!
//! Providers reject a request that declares two tools with one name, so the
//! merge keeps exactly one tool per name, deterministically:
//!
//! 1. **Static tools win.** The agent's own tools (including the injected
//!    `shared_state` tool) are the host's explicit choice; a source cannot
//!    shadow them.
//! 2. **Then earlier sources win** (installation order), and within one
//!    source, the earlier tool in its list.
//!
//! The surviving sourced tools are then sorted by name (see *Prompt
//! caching* above). Every dropped duplicate is logged with `tracing::warn!` naming the tool.
//! The run is never refused over a collision: a misbehaving source must not be
//! able to take the whole agent down.
//!
//! A source that **panics** is contained, logged, and contributes no tools
//! for that run — or, asked again mid-run, from then on; its generation is
//! recorded, so it is not asked again until it moves once more.

use std::collections::HashSet;
use std::sync::Arc;

use crate::types::{AgentTool, ToolContext, ToolError, ToolResult};

/// A provider of tools consulted at the start of every run.
///
/// Install with [`Agent::with_tool_source`](crate::Agent::with_tool_source)
/// or [`SubAgentTool::with_tool_source`](crate::SubAgentTool::with_tool_source).
/// See the [module docs](self) for when it is consulted and how name
/// collisions resolve.
///
/// ```
/// use std::sync::{Arc, Mutex};
/// use yoagent::{AgentTool, ToolSource};
///
/// /// A tool list that the application swaps at runtime.
/// struct Swappable(Mutex<Vec<Arc<dyn AgentTool>>>);
///
/// #[async_trait::async_trait]
/// impl ToolSource for Swappable {
///     async fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
///         self.0.lock().unwrap().clone()
///     }
/// }
/// ```
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
pub trait ToolSource: crate::rt::MaybeSend + crate::rt::MaybeSync {
    /// The tools to offer for the run about to start — or, when
    /// [`generation`](Self::generation) moved, from the next request on.
    ///
    /// Called once per run, before the first request, and again between
    /// turns when the generation moved. Keep it quick: the run waits for it.
    /// The returned `Arc`s are held until the run ends or the source is asked
    /// again.
    async fn tools(&self) -> Vec<Arc<dyn AgentTool>>;

    /// A number that changes whenever this source's tools change (any
    /// change; it need not grow). The loop reads it before asking for the
    /// tools and, before each later request in the run, asks again only when
    /// it moved. Cheap and non-blocking: it is read every turn. Default `0`:
    /// a source that never changes mid-run is asked once per run.
    fn generation(&self) -> u64 {
        0
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl<T: ToolSource + ?Sized> ToolSource for Arc<T> {
    async fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        (**self).tools().await
    }
    fn generation(&self) -> u64 {
        (**self).generation()
    }
}

/// Wraps an `Arc<dyn AgentTool>` so it fits the loop's
/// `Vec<Box<dyn AgentTool>>` (see [`AgentContext`](crate::AgentContext)).
pub(crate) struct ArcTool(pub(crate) Arc<dyn AgentTool>);

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl AgentTool for ArcTool {
    fn name(&self) -> &str {
        self.0.name()
    }
    fn label(&self) -> &str {
        self.0.label()
    }
    fn description(&self) -> &str {
        self.0.description()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        self.0.parameters_schema()
    }
    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        self.0.execute(params, ctx).await
    }
}

/// A run's sources, each with the generation its tools were read at and
/// the tools it gave.
#[derive(Clone, Default)]
pub(crate) struct Sourced {
    sources: Vec<Arc<dyn ToolSource>>,
    generations: Vec<u64>,
    tools: Vec<Vec<Arc<dyn AgentTool>>>,
}

impl Sourced {
    /// Every source's tools, in installation order (for [`merge`]).
    pub(crate) fn all(&self) -> Vec<Arc<dyn AgentTool>> {
        self.tools.iter().flatten().cloned().collect()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }

    /// Ask again every source whose generation moved since its tools were
    /// read; returns whether any did. A cancel stops the asking: the sources
    /// not asked yet keep their tools (and are asked at the next boundary).
    pub(crate) async fn refresh(&mut self, cancel: &tokio_util::sync::CancellationToken) -> bool {
        let mut changed = false;
        for index in 0..self.sources.len() {
            let now = generation_of(&self.sources[index], index);
            if now == self.generations[index] {
                continue;
            }
            let asked = tokio::select! {
                tools = ask(&self.sources[index], index) => tools,
                _ = cancel.cancelled() => return changed,
            };
            tracing::info!(
                source = index,
                from = self.generations[index],
                to = now,
                tools = asked.len(),
                "tool source changed; its tools are offered again from this request"
            );
            self.generations[index] = now;
            self.tools[index] = asked;
            changed = true;
        }
        changed
    }
}

/// A source's generation; a `generation()` that panics reads as 0 (logged).
fn generation_of(source: &Arc<dyn ToolSource>, index: usize) -> u64 {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| source.generation())).unwrap_or_else(
        |payload| {
            tracing::warn!(
                source = index,
                panic = %panic_message(payload.as_ref()),
                "tool source's generation() panicked; read as 0"
            );
            0
        },
    )
}

/// One source's tools; a panic — while building its future or while it runs
/// — is contained, logged with its payload, and gives nothing.
async fn ask(source: &Arc<dyn ToolSource>, index: usize) -> Vec<Arc<dyn AgentTool>> {
    use futures::FutureExt;
    // The call itself is inside the guarded block, so a hand-written
    // `tools()` that panics before returning its future is caught too.
    let guarded = std::panic::AssertUnwindSafe(async move { (**source).tools().await });
    match guarded.catch_unwind().await {
        Ok(tools) => tools,
        Err(payload) => {
            tracing::warn!(
                source = index,
                panic = %panic_message(payload.as_ref()),
                "tool source panicked; it contributes no tools"
            );
            Vec::new()
        }
    }
}

/// Ask every source for its tools, in installation order, reading each one's
/// generation first (so a change while it answers is seen at the next
/// boundary).
pub(crate) async fn collect(sources: &[Arc<dyn ToolSource>]) -> Sourced {
    let mut sourced = Sourced::default();
    for (index, source) in sources.iter().enumerate() {
        sourced.generations.push(generation_of(source, index));
        sourced.tools.push(ask(source, index).await);
        sourced.sources.push(source.clone());
    }
    sourced
}

/// The tool list a run rebuilds when a source changed: the tools that are not
/// sourced (the agent's own, `static_len` of them), then the sources' tools,
/// then the extensions' per-run tools — merged in that order, so the
/// collision rules hold over the whole list.
pub(crate) struct RunTools {
    pub(crate) sourced: Sourced,
    pub(crate) static_len: usize,
    pub(crate) extension_tools: Vec<Arc<dyn AgentTool>>,
}

impl RunTools {
    /// Ask the changed sources again and rebuild `tools` when any changed.
    /// Returns the length of the non-extension part of `tools` (what the run
    /// truncates back to at its end).
    pub(crate) async fn refresh(
        &mut self,
        tools: &mut Vec<Box<dyn AgentTool>>,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Option<usize> {
        if self.sourced.is_empty() || !self.sourced.refresh(cancel).await {
            return None;
        }
        tools.truncate(self.static_len);
        merge(tools, self.sourced.all());
        let base = tools.len();
        if !self.extension_tools.is_empty() {
            merge(tools, self.extension_tools.clone());
        }
        Some(base)
    }
}

pub(crate) fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

/// Append `sourced` to `tools`, skipping any whose name is already taken
/// (static tools first, then earlier sourced ones), then sort the appended
/// tools by name.
///
/// Dedup happens before the sort so "earlier wins" keeps its meaning. The
/// sort makes the offered list independent of the order a source returns
/// its tools in (a `HashMap`-backed source, say): tool definitions open the
/// provider's cached prefix, so a mere reordering would rewrite the whole
/// cache. The agent's own tools keep their order.
pub(crate) fn merge(tools: &mut Vec<Box<dyn AgentTool>>, sourced: Vec<Arc<dyn AgentTool>>) {
    let static_names: HashSet<String> = tools.iter().map(|t| t.name().to_string()).collect();
    let mut taken = static_names.clone();
    let mut kept: Vec<Arc<dyn AgentTool>> = Vec::new();
    for tool in sourced {
        let name = tool.name().to_string();
        if taken.contains(&name) {
            if static_names.contains(&name) {
                tracing::warn!(
                    tool = %name,
                    "a tool source offered a tool whose name a static tool already uses; \
                     the static tool is kept"
                );
            } else {
                tracing::warn!(
                    tool = %name,
                    "two tool sources offered tools with the same name; \
                     the first one is kept"
                );
            }
            continue;
        }
        taken.insert(name);
        kept.push(tool);
    }
    kept.sort_by(|a, b| a.name().cmp(b.name()));
    tools.extend(
        kept.into_iter()
            .map(|t| Box::new(ArcTool(t)) as Box<dyn AgentTool>),
    );
}
