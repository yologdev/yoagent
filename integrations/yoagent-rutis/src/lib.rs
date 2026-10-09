// The `doc(cfg)` feature badges, on docs.rs only; a no-op on stable.
#![cfg_attr(docsrs, feature(doc_cfg))]
//! **yoagent-rutis** — extend [yoagent](https://docs.rs/yoagent) agents at
//! runtime with [rutis](https://docs.rs/rutis) plugins, in Rust, TypeScript
//! or Python.
//!
//! rutis (a Rust port of the Cordis plugin kernel) loads, unloads, reloads
//! and hot-updates plugins, and tears down everything a plugin registered
//! when it goes. This crate lets such plugins contribute to agents while
//! they live, through **one yoagent [`Extension`](yoagent::Extension)**:
//!
//! 1. [`RutisBridge::install`] provides the `yoagent` [`Registry`] service on
//!    the rutis root.
//! 2. Plugins register a [`Handler`] — a name plus whichever hooks it
//!    implements: `tools`, `before_tool`, `after_tool`, `before_model`,
//!    `on_input`, `on_stop`, `finish`, `on_event`. A registration ends when
//!    its plugin unloads.
//! 3. [`RutisBridge::extension`] returns a [`RutisExtension`] the host
//!    installs with `Agent::with_extension` (or `with_tree_extension`, so
//!    plugin policy also covers sub-agents). Each run snapshots the
//!    registered handlers at its start.
//!
//! Every agent event is also published on the rutis bus as an
//! [`AgentEventEmitted`] for decoupled observers.
//!
//! yoagent itself knows nothing about rutis; the bridge uses only yoagent's
//! public API.
//!
//! TypeScript and Python handlers (features `node`, `python`, `websocket`)
//! can return images in tool results and write logs into the host's
//! `tracing` output (target `yoagent_rutis::plugin`). Plugins from other
//! agent ecosystems — DSH tool plugins, pi extensions — plug in through
//! adapters shipped in the repository's `plugins/` directory (experimental);
//! see the [README](https://github.com/yologdev/yoagent/tree/main/integrations/yoagent-rutis)
//! and its adapter contract.
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
//! let bridge = RutisBridge::install(&root)?; // on the root, once
//! let mut agent = Agent::from_provider(MockProvider::text("hi"), ModelConfig::mock())
//!     .with_extension(bridge.extension());
//!
//! // Load plugins whenever you like; each run sees the ones active at its start.
//! // root.plugin(MyPlugin);
//!
//! agent.prompt("hello").await;
//! # Ok(()) }
//! ```
//!
//! # Plugin
//!
//! ```
//! use rutis::{BoxFuture, CordisError, Ctx, Effect, Plugin, TypeKey};
//! use yoagent::ToolDecision;
//! use yoagent_rutis::{Handler, PluginCtxExt, Registry};
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
//!             ctx.register_handler(Handler::new("no-shell").with_before_tool(|call| {
//!                 match call.tool.as_str() {
//!                     "bash" => ToolDecision::Deny("shell access is disabled".into()),
//!                     _ => ToolDecision::Allow,
//!                 }
//!             }))?;
//!             Ok(Effect::Done)
//!         })
//!     }
//! }
//! let _plugin = NoShell { injects: vec![TypeKey::of::<Registry>()] };
//! ```
//!
//! [`AgentPlugin`] wraps a handler as a plugin in one expression.
//!
//! # TypeScript and Python plugins
//!
//! With the `node`, `python` or `websocket` feature (rutis-bridge 0.7),
//! [`RutisBridge::install`] also provides the registry to plugins in other
//! languages, as the host service `yoagent`. Such a plugin registers an
//! object (or a Python dict) of async functions, with the same hooks and the
//! same plain-JSON arguments as a Rust [`Handler`]:
//!
//! ```ts
//! const yoagent = ctx.use('yoagent')
//! ctx.effect(yoagent.register('no-shell', {
//!   async before_tool(call) {
//!     if (call.tool === 'bash') return { deny: 'shell access is disabled' }
//!   },
//! }))
//! ```
//!
//! The bridge never loads plugins: the host does (with rutis-loader, sharing
//! `yoagent` in its catalog). See the `languages` module (built with those
//! features) and `plugins/yoagent.d.ts` in the repository.
//!
//! # Semantics worth knowing
//!
//! - **A run uses the handlers registered when it started.** A plugin
//!   unloaded (or restarted / updated) mid-run leaves its handler in that run
//!   but **unavailable**: its tools fail ("no longer available", or "plugin
//!   unloaded during the call" for one in flight), its `before_tool` denies,
//!   its `on_input` rejects, its `after_tool` withholds the result (without
//!   failing a required run), its other hooks are skipped. Never rebound to the new generation. See
//!   [`registry`].
//! - **Names are unique across plugins**: handler names, and the names of
//!   static tools. A clash is refused (`CordisError::ServiceExists`, logged;
//!   no retry when the holder unloads). The agent's own tools win over plugin
//!   tools.
//! - **Policies gate every tool call** — the agent's own tools too, not only
//!   plugin tools.
//! - **Handlers combine like extensions**: in registration order, a `Deny`
//!   wins, a `Modify` feeds the next handler, notes are joined. See
//!   [`extension`].
//! - **Fail closed where it guards**: a `before_tool` that errors, panics or
//!   times out denies the call; an `on_input` rejects; an `after_tool`
//!   withholds the result. Other hooks are skipped and logged — unless the
//!   host made the extension [`required`](RutisExtension::required), when
//!   they fail the run.
//! - **No policy means allow**, including while a policy plugin reloads and
//!   before it first loads, unless the host sets
//!   [`require_policy`](RutisExtension::require_policy).
//! - **A host that is not running** (the root shut down, disposed or
//!   restarted since the bridge was installed) denies
//!   every tool call and rejects every prompt.
//! - **Finite default timeouts** per handler call (60 s policy hooks, 30 s
//!   input, 5 s turn hooks), plus the run's cancellation through yoagent.
//! - **Install on the root.** A bridge installed on a plugin's context reads
//!   as stopped for good once that plugin unloads or reloads.
//! - **Route rutis's `ErrorSink`** (default: stderr) into your logging with
//!   `Ctx::root_with_sink`.

pub mod bridge;
pub mod events;
pub mod extension;
pub mod handler;
mod host;
#[cfg(any(feature = "node", feature = "python", feature = "websocket"))]
#[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "node", feature = "python", feature = "websocket")))
)]
pub mod languages;
pub mod plugin;
pub mod registry;

pub use bridge::RutisBridge;
pub use events::AgentEventEmitted;
pub use extension::{
    RutisExtension, DEFAULT_INPUT_TIMEOUT, DEFAULT_POLICY_TIMEOUT, DEFAULT_TURN_TIMEOUT,
};
pub use handler::{Handler, Input, RunInfo, Stop, ToolCall, Turn};
pub use plugin::{AgentPlugin, PluginCtxExt};
pub use registry::{HandlerInfo, Registry};

/// The rutis this bridge is built against (`0.6`). Its types are part of this
/// crate's API, so any rutis minor bump (0.6 → 0.7) is a yoagent-rutis minor
/// bump.
pub use rutis;
