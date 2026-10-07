//! The plugin side: contribute to agents from inside `Plugin::apply`.
//!
//! Everything registered through these helpers belongs to the calling
//! plugin's fiber: rutis removes it when the plugin unloads, restarts, is
//! updated, or is evicted because a dependency went away.

use std::sync::Arc;

use rutis::{BoxFuture, CordisError, Ctx, Disposer, Effect, EventKey, Plugin, TypeKey};
use yoagent::AgentTool;

use crate::bridge::sealed::Sealed;
use crate::events::{AgentEventEmitted, ObserverListener};
use crate::handler::Handler;
use crate::registry::Registry;

/// Extension methods on a plugin's [`Ctx`]. Sealed: implemented for `Ctx`
/// only.
///
/// Each returns a [`Disposer`] that removes the contribution early; dropping
/// it does nothing (the plugin's unload still removes it).
pub trait PluginCtxExt: Sealed {
    /// Register a [`Handler`] with the bridge: every agent using the
    /// bridge's extension gets it from its next run on.
    ///
    /// Needs the bridge installed ([`RutisBridge::install`](crate::RutisBridge::install)):
    /// declare `TypeKey::of::<Registry>()` in the plugin's `injects` so it
    /// waits for it ([`AgentPlugin`] does). Refused with
    /// `CordisError::ServiceExists` on a name clash (see [`Registry`]).
    fn register_handler(&self, handler: Handler) -> Result<Disposer, CordisError>;

    /// Offer one tool: a handler named `tool:<name>` with only this tool.
    fn provide_tool(&self, tool: impl AgentTool + 'static) -> Result<Disposer, CordisError>;

    /// [`provide_tool`](Self::provide_tool) for a shared tool.
    fn provide_tool_arc(&self, tool: Arc<dyn AgentTool>) -> Result<Disposer, CordisError>;

    /// Observe every agent event published on the bus (see
    /// [`events`](crate::events)): `e.event()` is the event, `e.run_id()` /
    /// `e.label()` tell runs apart. Runs on rutis's dispatch task, never on
    /// the agent's — but a slow observer delays every later event on the bus.
    fn on_agent_event(
        &self,
        observer: impl Fn(&AgentEventEmitted) + Send + Sync + 'static,
    ) -> Result<Disposer, CordisError>;
}

impl PluginCtxExt for Ctx {
    fn register_handler(&self, handler: Handler) -> Result<Disposer, CordisError> {
        let registry = self.get::<Registry>().ok_or_else(|| {
            CordisError::ServiceNotFound(
                "yoagent_rutis::Registry (install the bridge with RutisBridge::install)".into(),
            )
        })?;
        registry.register(self, handler)
    }

    fn provide_tool(&self, tool: impl AgentTool + 'static) -> Result<Disposer, CordisError> {
        self.provide_tool_arc(Arc::new(tool))
    }

    fn provide_tool_arc(&self, tool: Arc<dyn AgentTool>) -> Result<Disposer, CordisError> {
        let name = format!("tool:{}", tool.name());
        self.register_handler(Handler::new(name).with_tool_arc(tool))
    }

    fn on_agent_event(
        &self,
        observer: impl Fn(&AgentEventEmitted) + Send + Sync + 'static,
    ) -> Result<Disposer, CordisError> {
        self.events().on(
            self,
            &EventKey::<AgentEventEmitted>::of(),
            ObserverListener(observer),
        )
    }
}

/// A ready-made rutis [`Plugin`] that registers one [`Handler`] — for the
/// common case where a plugin is "these tools plus this policy":
///
/// ```
/// use yoagent::ToolDecision;
/// use yoagent_rutis::{AgentPlugin, Handler};
///
/// let plugin = AgentPlugin::new(Handler::new("no-shell").with_before_tool(|call| {
///     if call.tool == "bash" {
///         ToolDecision::Deny("shell access is disabled".into())
///     } else {
///         ToolDecision::Allow
///     }
/// }));
/// // root.plugin(plugin);
/// ```
///
/// The plugin is named after the handler. It declares the [`Registry`] as a
/// dependency (plus any [`with_inject`](Self::with_inject) keys), so it
/// waits for the bridge. The handler's hooks are shared across reloads; for
/// per-generation state or config, write a `Plugin` (or a `PluginFactory`)
/// that builds its handler in `apply` and registers it with
/// [`PluginCtxExt::register_handler`].
pub struct AgentPlugin {
    handler: Handler,
    injects: Vec<TypeKey>,
}

impl AgentPlugin {
    /// A plugin registering `handler`.
    pub fn new(handler: Handler) -> Self {
        Self {
            handler,
            injects: vec![TypeKey::of::<Registry>()],
        }
    }

    /// Also wait for (and reload with) this service.
    pub fn with_inject(mut self, key: TypeKey) -> Self {
        self.injects.push(key);
        self
    }
}

impl Plugin for AgentPlugin {
    fn name(&self) -> &str {
        self.handler.name()
    }

    fn injects(&self) -> &[TypeKey] {
        &self.injects
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            ctx.register_handler(self.handler.clone())?;
            Ok(Effect::Done)
        })
    }
}
