//! **yoagent-rutis** — extend a [yoagent](https://docs.rs/yoagent) `Agent` at
//! runtime with [rutis](https://docs.rs/rutis) plugins.
//!
//! rutis (a Rust port of the Cordis plugin kernel) loads, unloads, reloads
//! and hot-updates plugins, and tears down everything a plugin registered
//! when it goes. This crate lets such plugins contribute to an agent while
//! it lives:
//!
//! | Plugin contributes | rutis mechanism | yoagent seam |
//! |---|---|---|
//! | tools | a [`ToolRegistry`] service; entries owned by the plugin's fiber | [`ToolSource`](yoagent::ToolSource) (resolved per run) |
//! | tool policy: allow / deny / rewrite args | `waterfall` of [`ToolCallEvent`] | [`ToolMiddleware`](yoagent::ToolMiddleware) |
//! | turn notes | `waterfall` of [`TurnEvent`] | [`TurnHook`](yoagent::TurnHook) |
//! | input rejection | `serial` of [`InputEvent`] | [`AsyncInputFilter`](yoagent::AsyncInputFilter) |
//! | observing agent events | `emit` of [`AgentEventEmitted`] | the `*_with_sender` event channel |
//!
//! yoagent itself knows nothing about rutis: the bridge only uses yoagent's
//! public builder methods.
//!
//! # Host
//!
//! ```no_run
//! use rutis::Ctx;
//! use yoagent::{provider::{MockProvider, ModelConfig}, Agent};
//! use yoagent_rutis::RutisBridge;
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let root = Ctx::root()?;
//! let bridge = RutisBridge::install(&root)?;
//! let mut agent = bridge.attach(Agent::from_provider(MockProvider::text("hi"), ModelConfig::mock()));
//!
//! // Load plugins whenever you like; each run sees the ones active at its start.
//! // root.plugin(MyPlugin);
//!
//! let (tx, forwarder) = bridge.event_sender(None); // optional: events onto the bus
//! agent.prompt_with_sender("hello", tx).await;
//! forwarder.await?;
//! # Ok(()) }
//! ```
//!
//! # Plugin
//!
//! ```
//! use rutis::{BoxFuture, CordisError, Ctx, Effect, Plugin, TypeKey};
//! use yoagent_rutis::{PluginCtxExt, ToolRegistry, ToolVerdict};
//!
//! struct NoShell {
//!     injects: Vec<TypeKey>,
//! }
//!
//! impl Plugin for NoShell {
//!     fn name(&self) -> &str { "no-shell" }
//!     fn injects(&self) -> &[TypeKey] { &self.injects } // wait for the bridge
//!     fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
//!         Box::pin(async move {
//!             ctx.on_tool_call(|call| match call.tool_name() {
//!                 "bash" => ToolVerdict::deny("shell access is disabled"),
//!                 _ => ToolVerdict::Allow,
//!             })?;
//!             Ok(Effect::Done)
//!         })
//!     }
//! }
//! let _plugin = NoShell { injects: vec![TypeKey::of::<ToolRegistry>()] };
//! ```
//!
//! [`AgentPlugin`] builds the same from closures in one expression.
//!
//! # Semantics worth knowing
//!
//! - **Tools change at run boundaries.** An agent asks for plugin tools once
//!   per run; a plugin unloaded mid-run leaves its tools offered until the
//!   run ends, but calling one then returns an error result.
//! - **Tool names are unique across plugins**: a second plugin registering a
//!   taken name is refused (`CordisError::ServiceExists`). The agent's own
//!   tools win over plugin tools.
//! - **Policy and input filtering fail closed** (an erroring, panicking or
//!   timed-out plugin denies / rejects); **turn notes fail open**.
//! - **One bridge per rutis root.** All attached agents share its plugins;
//!   events of several agents share one bus channel.

mod bridge;
mod events;
mod input;
mod plugin;
mod policy;
mod tools;
mod turn;

pub use bridge::{attach, AgentRutisExt, RutisBridge};
pub use events::{emit_agent_event, event_sender, AgentEventEmitted};
pub use input::{InputEvent, RutisInputFilter};
pub use plugin::{AgentPlugin, PluginCtxExt};
pub use policy::{RutisToolMiddleware, ToolCallEvent, ToolPolicy, ToolVerdict};
pub use tools::{PluginToolSource, ToolRegistry};
pub use turn::{RutisTurnHook, TurnEvent};

/// The exact rutis version this bridge is built against (`=0.5.0`).
pub use rutis;
