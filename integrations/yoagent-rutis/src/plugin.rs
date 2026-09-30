//! The plugin side: contribute to an agent from inside `Plugin::apply`.
//!
//! Everything registered through these helpers belongs to the calling
//! plugin's fiber: rutis removes it when the plugin unloads, restarts, is
//! updated, or is evicted because a dependency went away.

use std::sync::Arc;

use rutis::{BoxFuture, CordisError, Ctx, Disposer, Effect, EventKey, Plugin, TypeKey};
use yoagent::{AgentEvent, AgentTool};

use crate::events::AgentEventEmitted;
use crate::input::{InputEvent, InputListener};
use crate::policy::{FnPolicy, PolicyListener, ToolCallEvent, ToolPolicy, ToolVerdict};
use crate::tools::ToolRegistry;
use crate::turn::{NoteListener, TurnEvent};

/// Extension methods on a plugin's [`Ctx`].
///
/// Each returns a [`Disposer`] that removes the contribution early; dropping
/// it does nothing (the plugin's unload still removes it).
pub trait PluginCtxExt {
    /// Contribute a tool to every attached agent, from the next run on.
    ///
    /// Needs the bridge installed ([`RutisBridge::install`](crate::RutisBridge::install)):
    /// declare `TypeKey::of::<ToolRegistry>()` in the plugin's `injects` so
    /// it waits for it ([`AgentPlugin`] does). Refused with
    /// `CordisError::ServiceExists` if a live plugin tool has the same name.
    fn provide_tool(&self, tool: impl AgentTool + 'static) -> Result<Disposer, CordisError>;

    /// [`provide_tool`](Self::provide_tool) for a shared tool.
    fn provide_tool_arc(&self, tool: Arc<dyn AgentTool>) -> Result<Disposer, CordisError>;

    /// Judge every tool call with a synchronous closure: return
    /// [`ToolVerdict::Allow`] to pass it on (after optionally
    /// [`set_args`](ToolCallEvent::set_args)) or [`ToolVerdict::Deny`] to
    /// stop it.
    fn on_tool_call(
        &self,
        policy: impl Fn(&ToolCallEvent) -> ToolVerdict + Send + Sync + 'static,
    ) -> Result<Disposer, CordisError>;

    /// Judge every tool call with an async [`ToolPolicy`].
    fn on_tool_call_async(&self, policy: impl ToolPolicy) -> Result<Disposer, CordisError>;

    /// Add a note to requests: return `Some(note)` to append it to the
    /// request's latest user turn.
    fn on_turn(
        &self,
        note: impl Fn(&TurnEvent) -> Option<String> + Send + Sync + 'static,
    ) -> Result<Disposer, CordisError>;

    /// Check prompts: return `Some(reason)` to reject one.
    fn on_input(
        &self,
        check: impl Fn(&InputEvent) -> Option<String> + Send + Sync + 'static,
    ) -> Result<Disposer, CordisError>;

    /// Observe the agent's events (published with an event sender from
    /// [`RutisBridge::event_sender`](crate::RutisBridge::event_sender)).
    /// Runs on rutis's dispatch task, never on the agent's.
    fn on_agent_event(
        &self,
        observer: impl Fn(&AgentEvent) + Send + Sync + 'static,
    ) -> Result<Disposer, CordisError>;
}

impl PluginCtxExt for Ctx {
    fn provide_tool(&self, tool: impl AgentTool + 'static) -> Result<Disposer, CordisError> {
        self.provide_tool_arc(Arc::new(tool))
    }

    fn provide_tool_arc(&self, tool: Arc<dyn AgentTool>) -> Result<Disposer, CordisError> {
        let registry = self.get::<ToolRegistry>().ok_or_else(|| {
            CordisError::ServiceNotFound(
                "yoagent_rutis::ToolRegistry (install the bridge with RutisBridge::install)".into(),
            )
        })?;
        registry.register(self, tool)
    }

    fn on_tool_call(
        &self,
        policy: impl Fn(&ToolCallEvent) -> ToolVerdict + Send + Sync + 'static,
    ) -> Result<Disposer, CordisError> {
        self.on_tool_call_async(FnPolicy(policy))
    }

    fn on_tool_call_async(&self, policy: impl ToolPolicy) -> Result<Disposer, CordisError> {
        self.events().on_waterfall(
            self,
            &EventKey::<ToolCallEvent>::of(),
            PolicyListener(Arc::new(policy)),
        )
    }

    fn on_turn(
        &self,
        note: impl Fn(&TurnEvent) -> Option<String> + Send + Sync + 'static,
    ) -> Result<Disposer, CordisError> {
        self.events()
            .on_waterfall(self, &EventKey::<TurnEvent>::of(), NoteListener(note))
    }

    fn on_input(
        &self,
        check: impl Fn(&InputEvent) -> Option<String> + Send + Sync + 'static,
    ) -> Result<Disposer, CordisError> {
        self.events()
            .on(self, &EventKey::<InputEvent>::of(), InputListener(check))
    }

    fn on_agent_event(
        &self,
        observer: impl Fn(&AgentEvent) + Send + Sync + 'static,
    ) -> Result<Disposer, CordisError> {
        self.events().on(
            self,
            &EventKey::<AgentEventEmitted>::of(),
            ObserverListener(observer),
        )
    }
}

struct ObserverListener<F>(F);

impl<F> rutis::Listener<AgentEventEmitted> for ObserverListener<F>
where
    F: Fn(&AgentEvent) + Send + Sync + 'static,
{
    fn call<'a>(
        &'a self,
        _ctx: &'a Ctx,
        e: &'a AgentEventEmitted,
    ) -> BoxFuture<'a, Result<Option<()>, CordisError>> {
        Box::pin(async move {
            (self.0)(e.event());
            Ok(None)
        })
    }
}

type PolicyFn = Arc<dyn Fn(&ToolCallEvent) -> ToolVerdict + Send + Sync>;
type NoteFn = Arc<dyn Fn(&TurnEvent) -> Option<String> + Send + Sync>;
type InputFn = Arc<dyn Fn(&InputEvent) -> Option<String> + Send + Sync>;

/// A ready-made rutis [`Plugin`] from tools and closures — for the common
/// case where a plugin is "these tools plus this policy":
///
/// ```
/// use yoagent_rutis::{AgentPlugin, ToolVerdict};
///
/// let plugin = AgentPlugin::new("no-shell")
///     .with_policy(|call| {
///         if call.tool_name() == "bash" {
///             ToolVerdict::deny("shell access is disabled")
///         } else {
///             ToolVerdict::Allow
///         }
///     });
/// // root.plugin(plugin);
/// ```
///
/// It declares the [`ToolRegistry`] as a dependency (plus any
/// [`with_inject`](Self::with_inject) keys), so it waits for the bridge.
/// Tools are shared across reloads (`Arc`); closures are called on every
/// dispatch. For per-generation state or config, write a `Plugin` (or a
/// `PluginFactory`) using [`PluginCtxExt`] directly.
pub struct AgentPlugin {
    name: String,
    injects: Vec<TypeKey>,
    tools: Vec<Arc<dyn AgentTool>>,
    policies: Vec<PolicyFn>,
    notes: Vec<NoteFn>,
    inputs: Vec<InputFn>,
}

impl AgentPlugin {
    /// An empty plugin named `name`.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            injects: vec![TypeKey::of::<ToolRegistry>()],
            tools: Vec::new(),
            policies: Vec::new(),
            notes: Vec::new(),
            inputs: Vec::new(),
        }
    }

    /// Contribute a tool.
    pub fn with_tool(self, tool: impl AgentTool + 'static) -> Self {
        self.with_tool_arc(Arc::new(tool))
    }

    /// Contribute a shared tool.
    pub fn with_tool_arc(mut self, tool: Arc<dyn AgentTool>) -> Self {
        self.tools.push(tool);
        self
    }

    /// Add a tool-call policy (see [`PluginCtxExt::on_tool_call`]).
    pub fn with_policy(
        mut self,
        policy: impl Fn(&ToolCallEvent) -> ToolVerdict + Send + Sync + 'static,
    ) -> Self {
        self.policies.push(Arc::new(policy));
        self
    }

    /// Add a turn note (see [`PluginCtxExt::on_turn`]).
    pub fn with_turn_note(
        mut self,
        note: impl Fn(&TurnEvent) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        self.notes.push(Arc::new(note));
        self
    }

    /// Add an input check (see [`PluginCtxExt::on_input`]).
    pub fn with_input_check(
        mut self,
        check: impl Fn(&InputEvent) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        self.inputs.push(Arc::new(check));
        self
    }

    /// Also wait for (and reload with) this service.
    pub fn with_inject(mut self, key: TypeKey) -> Self {
        self.injects.push(key);
        self
    }
}

impl Plugin for AgentPlugin {
    fn name(&self) -> &str {
        &self.name
    }

    fn injects(&self) -> &[TypeKey] {
        &self.injects
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            for tool in &self.tools {
                ctx.provide_tool_arc(tool.clone())?;
            }
            for policy in &self.policies {
                let policy = policy.clone();
                ctx.on_tool_call(move |call| policy(call))?;
            }
            for note in &self.notes {
                let note = note.clone();
                ctx.on_turn(move |turn| note(turn))?;
            }
            for input in &self.inputs {
                let input = input.clone();
                ctx.on_input(move |event| input(event))?;
            }
            Ok(Effect::Done)
        })
    }
}
