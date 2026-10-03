//! Run [yoagent](https://docs.rs/yoagent) on Cloudflare Workers through the
//! Worker's own bindings.
//!
//! yoagent itself builds for `wasm32-unknown-unknown` with
//! `default-features = false` and reaches every provider over the host's
//! `fetch`. This crate adds what only a Worker has: the objects the Workers
//! runtime hands it in `env`, already authenticated as the account, so no API
//! token is needed (and, per Cloudflare, faster and less restricted than the
//! REST API).
//!
//! - `ai`: the Workers AI binding (`env.AI`) as a yoagent `DecisionBackend`,
//!   with presets for Cloudflare's Clef decision models.
//!
//! ```toml
//! # wrangler.toml
//! [ai]
//! binding = "AI"
//! ```
//!
//! ```ignore
//! // In a workers-rs fetch handler (a raw wasm-bindgen Worker passes its
//! // `env.AI` JsValue the same way). `worker::Error` has no conversion from
//! // `DecisionError`, hence the `map_err`.
//! let clef = yoagent_workers::ai::clef(env.ai("AI")?);
//! let urgent = clef
//!     .noul("Checkout fails for every customer.", "Is this urgent?")
//!     .await
//!     .map_err(|e| worker::Error::RustError(e.to_string()))?;
//! ```
//!
//! A binding belongs to one request: build the decision model (and the agent
//! that uses it) inside the handler, from that request's `env`.
//!
//! Everything here exists only on `wasm32`; on other targets the crate is
//! empty. yoagent's own core never depends on this crate.

#[cfg(target_arch = "wasm32")]
pub mod ai;
