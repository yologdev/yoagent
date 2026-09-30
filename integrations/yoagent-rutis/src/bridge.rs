//! The host side: install the bridge on a rutis context and attach an agent.

use std::sync::Arc;
use std::time::Duration;

use rutis::{CordisError, Ctx};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use yoagent::{Agent, AgentEvent, SubAgentTool};

use crate::input::{RutisInputFilter, DEFAULT_INPUT_TIMEOUT};
use crate::policy::{RutisToolMiddleware, DEFAULT_POLICY_TIMEOUT};
use crate::tools::{PluginToolSource, ToolRegistry};
use crate::turn::{RutisTurnHook, DEFAULT_TURN_TIMEOUT};

/// Connects one rutis context to yoagent agents.
///
/// Cheap to clone. Install it on the context plugins are loaded under —
/// normally the root — once; every agent attached to it sees the same
/// plugins. Dispatch happens on that context's bus.
///
/// **Timeouts.** yoagent awaits tool middleware, input filters and turn hooks
/// without watching the run's cancel token, so `Agent::abort()` cannot
/// unstick a plugin that never answers. Each chain therefore has a finite
/// default bound: tool policy [`DEFAULT_POLICY_TIMEOUT`] (60 s, then deny),
/// input filter [`DEFAULT_INPUT_TIMEOUT`] (30 s, then reject), turn notes
/// [`DEFAULT_TURN_TIMEOUT`] (5 s, then keep the notes so far). Passing `None`
/// to a setter removes the bound — you then own liveness (e.g. for a policy
/// that waits on a human approval).
///
/// **Errors from plugins.** rutis reports runtime errors (listener failures
/// on `emit`, cleanup errors) to its `ErrorSink`, which by default prints to
/// stderr. Route it into your logging with `Ctx::root_with_sink`.
#[derive(Clone)]
pub struct RutisBridge {
    ctx: Ctx,
    registry: Arc<ToolRegistry>,
    policy_timeout: Option<Duration>,
    input_timeout: Option<Duration>,
    turn_timeout: Option<Duration>,
    require_policy: bool,
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
            policy_timeout: Some(DEFAULT_POLICY_TIMEOUT),
            input_timeout: Some(DEFAULT_INPUT_TIMEOUT),
            turn_timeout: Some(DEFAULT_TURN_TIMEOUT),
            require_policy: false,
        })
    }

    /// Set the same bound on all three chains.
    pub fn with_timeout(self, limit: Duration) -> Self {
        self.with_policy_timeout(Some(limit))
            .with_input_timeout(Some(limit))
            .with_turn_timeout(Some(limit))
    }

    /// Bound one tool call's policy chain; past it the call is denied.
    /// `None`: no bound (you own liveness).
    pub fn with_policy_timeout(mut self, limit: Option<Duration>) -> Self {
        self.policy_timeout = limit;
        self
    }

    /// Bound one prompt's input-filter chain; past it the prompt is rejected.
    /// `None`: no bound (you own liveness).
    pub fn with_input_timeout(mut self, limit: Option<Duration>) -> Self {
        self.input_timeout = limit;
        self
    }

    /// Bound one request's turn-note chain; past it the notes added so far
    /// are used. `None`: no bound (you own liveness).
    pub fn with_turn_timeout(mut self, limit: Option<Duration>) -> Self {
        self.turn_timeout = limit;
        self
    }

    /// Deny every tool call that no plugin policy judged.
    ///
    /// By default an empty policy chain allows — which also covers the
    /// window while a policy plugin reloads (restart, config update,
    /// dependency-driven eviction) and before it first becomes active. Use
    /// this when a policy plugin is load-bearing: during that window calls
    /// are denied instead.
    pub fn require_policy(mut self) -> Self {
        self.require_policy = true;
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
        RutisToolMiddleware::new(self.ctx.clone(), self.policy_timeout, self.require_policy)
    }

    /// yoagent `TurnHook` dispatching the turn-note chain.
    pub fn turn_hook(&self) -> RutisTurnHook {
        RutisTurnHook::new(self.ctx.clone(), self.turn_timeout)
    }

    /// yoagent `AsyncInputFilter` dispatching the input chain.
    pub fn input_filter(&self) -> RutisInputFilter {
        RutisInputFilter::new(self.ctx.clone(), self.input_timeout)
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

    /// [`event_sender`](Self::event_sender) with a label on every published
    /// event, to tell several agents on one bridge apart.
    pub fn event_sender_labeled(
        &self,
        label: impl Into<Arc<str>>,
        forward: Option<mpsc::UnboundedSender<AgentEvent>>,
    ) -> (mpsc::UnboundedSender<AgentEvent>, JoinHandle<()>) {
        crate::events::event_sender_labeled(&self.ctx, label, forward)
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
