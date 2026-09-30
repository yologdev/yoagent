//! The host side: install the bridge on a rutis context and attach an agent.

use std::sync::Arc;
use std::time::Duration;

use rutis::{CordisError, Ctx};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use yoagent::{Agent, AgentEvent, SubAgentTool};

use crate::input::RutisInputFilter;
use crate::policy::RutisToolMiddleware;
use crate::tools::{PluginToolSource, ToolRegistry};
use crate::turn::RutisTurnHook;

/// Connects one rutis context to yoagent agents.
///
/// Cheap to clone. Install it on the context plugins are loaded under —
/// normally the root — once; every agent attached to it sees the same
/// plugins. Dispatch happens on that context's bus.
#[derive(Clone)]
pub struct RutisBridge {
    ctx: Ctx,
    registry: Arc<ToolRegistry>,
    timeout: Option<Duration>,
}

impl RutisBridge {
    /// Install the bridge on `ctx`: provide the [`ToolRegistry`] service
    /// there (or reuse the one already visible from `ctx`).
    ///
    /// The registry lives as long as the fiber that provides it, so install
    /// on the root (or a plugin that outlives every tool plugin). Fails only
    /// when rutis refuses the registration (e.g. the root was shut down).
    pub fn install(ctx: &Ctx) -> Result<Self, CordisError> {
        let registry = match ctx.get::<ToolRegistry>() {
            Some(existing) => existing,
            None => {
                ctx.provide(ToolRegistry::default())?;
                ctx.get::<ToolRegistry>()
                    .ok_or_else(|| CordisError::ServiceNotFound("ToolRegistry".into()))?
            }
        };
        Ok(Self {
            ctx: ctx.clone(),
            registry,
            timeout: None,
        })
    }

    /// Bound how long the policy chain, turn-hook chain and input-filter
    /// chain may take per dispatch. Past it a tool call is denied, a prompt
    /// rejected (both fail closed), and a turn keeps the notes added so far.
    /// Default: no limit — a plugin that never answers stalls the agent.
    pub fn with_timeout(mut self, limit: Duration) -> Self {
        self.timeout = Some(limit);
        self
    }

    /// The rutis context the bridge dispatches on.
    pub fn ctx(&self) -> &Ctx {
        &self.ctx
    }

    /// The tool registry plugins add to.
    pub fn registry(&self) -> &Arc<ToolRegistry> {
        &self.registry
    }

    /// yoagent `ToolSource` of the active plugins' tools.
    pub fn tool_source(&self) -> PluginToolSource {
        PluginToolSource::new(self.registry.clone())
    }

    /// yoagent `ToolMiddleware` dispatching the tool-call policy chain.
    pub fn tool_middleware(&self) -> RutisToolMiddleware {
        RutisToolMiddleware::new(self.ctx.clone(), self.timeout)
    }

    /// yoagent `TurnHook` dispatching the turn-note chain.
    pub fn turn_hook(&self) -> RutisTurnHook {
        RutisTurnHook::new(self.ctx.clone(), self.timeout)
    }

    /// yoagent `AsyncInputFilter` dispatching the input chain.
    pub fn input_filter(&self) -> RutisInputFilter {
        RutisInputFilter::new(self.ctx.clone(), self.timeout)
    }

    /// Wire tools, tool policy, turn notes and input filtering into `agent`,
    /// using only yoagent's own builder methods (`with_tool_source`,
    /// `with_tool_middleware`, `with_turn_hook`, `with_async_input_filter`).
    ///
    /// The middleware is appended after any already installed, so plugin
    /// policy sees arguments after the host's own middleware. Events are not
    /// wired here — pass [`event_sender`](Self::event_sender) to a
    /// `*_with_sender` call.
    pub fn attach(&self, agent: Agent) -> Agent {
        agent
            .with_tool_source(self.tool_source())
            .with_tool_middleware(self.tool_middleware())
            .with_turn_hook(self.turn_hook())
            .with_async_input_filter(self.input_filter())
    }

    /// [`attach`](Self::attach) for a sub-agent: its runs see the same plugin
    /// tools and go through the same chains.
    pub fn attach_sub_agent(&self, sub: SubAgentTool) -> SubAgentTool {
        sub.with_tool_source(self.tool_source())
            .with_tool_middleware(self.tool_middleware())
            .with_turn_hook(self.turn_hook())
            .with_async_input_filter(self.input_filter())
    }

    /// A sender that publishes every agent event on the bus (and forwards it
    /// to `forward` first, if given). See [`event_sender`](crate::event_sender).
    pub fn event_sender(
        &self,
        forward: Option<mpsc::UnboundedSender<AgentEvent>>,
    ) -> (mpsc::UnboundedSender<AgentEvent>, JoinHandle<()>) {
        crate::events::event_sender(&self.ctx, forward)
    }
}

/// Install the bridge on `ctx` (or reuse it) and attach `agent` — the one-line
/// form of [`RutisBridge::install`] + [`RutisBridge::attach`].
pub fn attach(agent: Agent, ctx: &Ctx) -> Result<Agent, CordisError> {
    Ok(RutisBridge::install(ctx)?.attach(agent))
}

/// `agent.with_rutis(&bridge)` — builder-style [`RutisBridge::attach`].
pub trait AgentRutisExt: Sized {
    /// Attach this agent (or sub-agent) to the bridge's plugins.
    fn with_rutis(self, bridge: &RutisBridge) -> Self;
}

impl AgentRutisExt for Agent {
    fn with_rutis(self, bridge: &RutisBridge) -> Self {
        bridge.attach(self)
    }
}

impl AgentRutisExt for SubAgentTool {
    fn with_rutis(self, bridge: &RutisBridge) -> Self {
        bridge.attach_sub_agent(self)
    }
}
