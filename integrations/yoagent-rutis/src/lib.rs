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
//!   per run; a plugin unloaded (or restarted / updated) mid-run leaves its
//!   tools offered until the run ends, but a call that starts afterwards
//!   fails as *no longer available* and a call in flight is abandoned with an
//!   error — never rebound to the new generation.
//! - **Tool names are unique across plugins**: a second plugin registering a
//!   taken name is refused (`CordisError::ServiceExists`, logged; no retry
//!   when the holder unloads). The agent's own tools win over plugin tools.
//! - **Every policy must pass.** Any `Deny` wins; an `Allow` that skips the
//!   rest of the chain is treated as a denial.
//! - **Policy and input filtering fail closed** — an erroring, panicking or
//!   timed-out plugin, or a host that has shut down, denies / rejects —
//!   **except for an empty chain**, which allows / passes. That includes the
//!   window while a policy plugin reloads and before it first loads; close it
//!   with [`RutisBridge::require_policy`]. **Turn notes fail open.**
//! - **Finite default timeouts** per chain (60 s policy, 30 s input, 5 s
//!   turn notes): yoagent's `abort()` cannot interrupt a hung hook. `None`
//!   opts out.
//! - **One bridge per rutis root.** All attached agents share its plugins;
//!   their events share one bus queue (label them with
//!   [`RutisBridge::event_sender_labeled`]).
//! - **Route rutis's `ErrorSink`** (default: stderr) into your logging with
//!   `Ctx::root_with_sink`.

pub mod bridge;
pub mod events;
mod host;
pub mod input;
pub mod plugin;
pub mod policy;
pub mod tools;
pub mod turn;

pub use bridge::{attach, AgentRutisExt, RutisBridge};
pub use events::{emit_agent_event, event_sender, event_sender_labeled, AgentEventEmitted};
pub use input::{InputEvent, RutisInputFilter, DEFAULT_INPUT_TIMEOUT};
pub use plugin::{AgentPlugin, PluginCtxExt};
pub use policy::{
    RutisToolMiddleware, ToolCallEvent, ToolPolicy, ToolVerdict, DEFAULT_POLICY_TIMEOUT,
};
pub use tools::{PluginToolSource, ToolRegistry};
pub use turn::{RutisTurnHook, TurnEvent, DEFAULT_TURN_TIMEOUT};

/// The exact rutis version this bridge is built against (`=0.5.0`).
pub use rutis;
