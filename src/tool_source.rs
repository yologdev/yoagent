//! Tools resolved per run — and again mid-run when a source says they changed.
//!
//! An [`Agent`](crate::Agent)'s own tools ([`with_tools`](crate::Agent::with_tools))
//! are fixed until you replace them. A [`ToolSource`] is for tools that come
//! and go while the agent lives: a plugin system loading and unloading
//! extensions, an MCP server that reconnects with a different tool list, a
//! feature flag. The agent asks each source for its tools **at the start of
//! every run** — every `prompt*` and `continue_loop*` call — and offers the
//! model the static tools plus whatever the sources returned; a source that
//! reports a change is asked again mid-run (below).
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
//! source whose generation stays 0 (the default) is asked only at the start
//! of the run. A tool the model calls after it was withdrawn is answered as
//! an unknown tool.
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
//! **No timeout.** The run waits for every source. At the start of a run on
//! an [`Agent`](crate::Agent), dropping the prompt future while it waits is
//! the escape hatch — sources are consulted before any agent state is
//! touched, so the agent is left as it was. A
//! [`SubAgentTool`](crate::SubAgentTool) stops waiting when the parent run
//! is cancelled. Asked again mid-run, a source is raced against the run's
//! cancel: a cancelled run stops waiting, and that source keeps its previous
//! tools while the run ends.
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
//! 3. **Extensions' per-run tools come last** (see
//!    [`RunHooks::tools`](crate::extension::RunHooks::tools)): a name the
//!    agent or a source already offers is dropped from them — after a
//!    mid-run change too, so a source can take a name over from an extension.
//!
//! The surviving sourced tools are then sorted by name (see *Prompt
//! caching* above). Every dropped duplicate is logged with `tracing::warn!` naming the tool.
//! The run is never refused over a collision: a misbehaving source must not be
//! able to take the whole agent down.
//!
//! A source that **panics** is contained, logged each time, and contributes
//! no tools for that run — or, asked again mid-run, from then on; its
//! generation is recorded, so it is not asked again until it moves once
//! more. A [`generation`](ToolSource::generation) that panics counts as
//! unchanged (logged once per run).

use std::collections::HashSet;
use std::sync::Arc;

use crate::types::{AgentTool, ToolContext, ToolError, ToolResult};

/// A provider of tools consulted at the start of every run, and again
/// mid-run when its [`generation`](Self::generation) moves.
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

    /// A number that changes whenever this source's tools change. The loop
    /// reads it before asking for the tools and, before each later request in
    /// the run, asks again only when it differs from the value it read then —
    /// so use a counter that only moves forward: a value that changes and
    /// comes back between two turns would be missed. Cheap and non-blocking:
    /// it is read every turn. Default `0`: a source that never changes mid-run
    /// is asked only at the start of each run. A `generation()` that panics
    /// counts as unchanged (logged once per run).
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

/// One source of a run: the generation its tools were read at, the tools it
/// gave, and whether its `generation()` has panicked (logged once per run).
#[derive(Clone)]
struct SourceState {
    source: Arc<dyn ToolSource>,
    generation: u64,
    tools: Vec<Arc<dyn AgentTool>>,
    generation_panicked: bool,
}

/// A run's sources, and how many of the context's tools come before theirs
/// (the agent's own, measured where the sourced tools are merged in).
#[derive(Default)]
pub(crate) struct Sourced {
    states: Vec<SourceState>,
    static_len: usize,
}

impl Sourced {
    /// Every source's tools, in installation order.
    fn all(&self) -> Vec<Arc<dyn AgentTool>> {
        self.states
            .iter()
            .flat_map(|s| s.tools.iter().cloned())
            .collect()
    }

    /// Append the sources' tools to `tools` (see [`merge`]), recording how
    /// many tools were there before them: what a rebuild keeps.
    pub(crate) fn merge_into(&mut self, tools: &mut Vec<Box<dyn AgentTool>>) {
        self.static_len = tools.len();
        merge(tools, self.all());
    }

    /// Ask again every source whose generation moved since its tools were
    /// read; returns whether any did. A cancel stops the asking: the source
    /// being asked and those after it keep their previous tools and
    /// generation (the run is ending).
    async fn refresh(&mut self, cancel: &tokio_util::sync::CancellationToken) -> bool {
        let mut changed = false;
        for (index, state) in self.states.iter_mut().enumerate() {
            // A `generation()` that panics counts as unchanged: a panic must
            // not cost a re-ask (and a prompt-cache rewrite) every turn.
            let Some(now) = generation_of(&state.source, index, &mut state.generation_panicked)
            else {
                continue;
            };
            if now == state.generation {
                continue;
            }
            let asked = tokio::select! {
                tools = ask(&state.source, index) => tools,
                _ = cancel.cancelled() => return changed,
            };
            tracing::info!(
                source = index,
                from = state.generation,
                to = now,
                tools = asked.len(),
                "tool source changed; its tools are offered again from this request"
            );
            state.generation = now;
            state.tools = asked;
            changed = true;
        }
        changed
    }
}

/// A source's generation, or `None` when `generation()` panicked (logged the
/// first time in the run, `panicked` remembering it).
fn generation_of(source: &Arc<dyn ToolSource>, index: usize, panicked: &mut bool) -> Option<u64> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| source.generation())) {
        Ok(generation) => Some(generation),
        Err(payload) => {
            if !*panicked {
                *panicked = true;
                tracing::warn!(
                    source = index,
                    panic = %panic_message(payload.as_ref()),
                    "tool source's generation() panicked; counted as unchanged \
                     (logged once per run)"
                );
            }
            None
        }
    }
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
/// boundary). Merge them with [`Sourced::merge_into`].
pub(crate) async fn collect(sources: &[Arc<dyn ToolSource>]) -> Sourced {
    let mut sourced = Sourced::default();
    for (index, source) in sources.iter().enumerate() {
        let mut generation_panicked = false;
        let generation = generation_of(source, index, &mut generation_panicked).unwrap_or(0);
        let tools = ask(source, index).await;
        sourced.states.push(SourceState {
            source: source.clone(),
            generation,
            tools,
            generation_panicked,
        });
    }
    sourced
}

/// A run's tool list, kept rebuildable: the agent's own tools, then the
/// sources' tools ([`Sourced::merge_into`] put them there), then the
/// extensions' per-run tools — merged in that order, so the collision rules
/// hold over the whole list whenever a source changes.
pub(crate) struct RunTools {
    sourced: Sourced,
    extension_tools: Vec<Arc<dyn AgentTool>>,
    /// The length of the non-extension part of the list.
    base_len: usize,
}

impl RunTools {
    /// Start a run's list: `tools` holds the agent's own tools and the
    /// sources' (merged by `sourced`); the extensions' are merged after them.
    pub(crate) fn start(
        tools: &mut Vec<Box<dyn AgentTool>>,
        sourced: Sourced,
        extension_tools: Vec<Arc<dyn AgentTool>>,
    ) -> Self {
        debug_assert!(sourced.static_len <= tools.len());
        let base_len = tools.len();
        if !extension_tools.is_empty() {
            merge(tools, extension_tools.clone());
        }
        Self {
            sourced,
            extension_tools,
            base_len,
        }
    }

    /// The length of the non-extension part of the list: what the run
    /// truncates back to at its end.
    pub(crate) fn base_len(&self) -> usize {
        self.base_len
    }

    /// Ask the changed sources again and, when any changed, rebuild `tools`.
    pub(crate) async fn refresh(
        &mut self,
        tools: &mut Vec<Box<dyn AgentTool>>,
        cancel: &tokio_util::sync::CancellationToken,
    ) {
        if self.sourced.states.is_empty() || !self.sourced.refresh(cancel).await {
            return;
        }
        tools.truncate(self.sourced.static_len);
        merge(tools, self.sourced.all());
        self.base_len = tools.len();
        if !self.extension_tools.is_empty() {
            merge(tools, self.extension_tools.clone());
        }
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
