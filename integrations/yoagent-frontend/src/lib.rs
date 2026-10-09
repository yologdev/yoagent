//! **Experimental.** One frontend protocol for yoagent agents.
//!
//! A [`Session`] runs one agent for any number of frontends — a terminal UI
//! that is a rutis plugin, browsers over WebSocket — through the same
//! [`protocol`]: commands in (prompt, steer, abort, …), and out every event,
//! a reliable run lifecycle (`RunStarted` / `RunEnded`), and plugins'
//! questions to the user (`UiRequest`).
//!
//! Plugins reach it through two rutis host services ([`services`]):
//! `frontend` (a UI plugin connects and offers browser components) and `ui`
//! (any plugin asks the user — the pi adapter routes pi's `ctx.ui` dialogs
//! here). [`web`] serves a browser frontend; [`host`] cuts the rutis setup to
//! a few lines.
//!
//! Not published and without an API promise: yoagent's core and
//! yoagent-rutis's API do not depend on it.

pub mod host;
pub mod protocol;
pub mod services;
pub mod session;
pub mod web;

pub use protocol::{ClientMessage, ServerMessage, UiPlugin, UiRequest};
pub use session::{Connection, Driver, Session};
