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
//! # Per run, not per turn
//!
//! The list is fixed for the whole run. A tool that disappears mid-run stays
//! offered (and callable) until the run ends, and a tool that appears mid-run
//! is first offered on the next run. Changing the list turn to turn could
//! strand a tool call the model already emitted.
//!
//! **Prompt caching.** Tool definitions open the provider's cached prefix, so
//! any change to the offered set rewrites the whole cache. Consulting once per
//! run *bounds* that cost to run boundaries; it does not prevent it — a run
//! that starts with a different set than the previous one pays for a full
//! prefix rewrite. The same set in a different order costs nothing: sourced
//! tools are sorted by name (the agent's own tools keep their order).
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
//! for that run.

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
    /// The tools to offer for the run about to start.
    ///
    /// Called once per run, before the first request. Keep it quick: the run
    /// waits for it. The returned `Arc`s are held until the run ends.
    async fn tools(&self) -> Vec<Arc<dyn AgentTool>>;
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl<T: ToolSource + ?Sized> ToolSource for Arc<T> {
    async fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        (**self).tools().await
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

/// Ask every source for its tools, in installation order. A panicking source
/// — whether it panics while building its future or while it runs — is
/// contained, logged with its payload, and contributes nothing.
pub(crate) async fn collect(sources: &[Arc<dyn ToolSource>]) -> Vec<Arc<dyn AgentTool>> {
    use futures::FutureExt;
    let mut out = Vec::new();
    for (index, source) in sources.iter().enumerate() {
        // The call itself is inside the guarded block, so a hand-written
        // `tools()` that panics before returning its future is caught too.
        let guarded = std::panic::AssertUnwindSafe(async move { (**source).tools().await });
        match guarded.catch_unwind().await {
            Ok(tools) => out.extend(tools),
            Err(payload) => tracing::warn!(
                source = index,
                panic = %panic_message(payload.as_ref()),
                "tool source panicked; it contributes no tools to this run"
            ),
        }
    }
    out
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
