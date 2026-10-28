//! The host side: install the bridge on a rutis context and hand its
//! extension to agents.

use std::sync::Arc;

use rutis::{CordisError, Ctx};

use crate::extension::RutisExtension;
use crate::host::Host;
use crate::registry::Registry;

/// Connects one rutis context to yoagent agents.
///
/// Cheap to clone. Install it **on the root** once; every agent using its
/// [`extension`](Self::extension) sees the same plugins.
///
/// **Errors from plugins.** rutis reports runtime errors (bus listener
/// failures, cleanup errors) to its `ErrorSink`, which by default prints to
/// stderr. Route it into your logging with `Ctx::root_with_sink`.
#[derive(Clone)]
pub struct RutisBridge {
    host: Host,
    registry: Arc<Registry>,
}

impl RutisBridge {
    /// Install the bridge on `ctx`: provide the [`Registry`] service there
    /// (or reuse the one already visible from `ctx`).
    ///
    /// **Install on the root.** The registry lives as long as the fiber that
    /// provides it, and the bridge is bound to `ctx`'s generation: installed
    /// on a plugin's context, it reads as stopped — denying every tool call
    /// and rejecting every prompt, for good — once that plugin unloads or
    /// reloads. The same holds for a root that is disposed or restarted: the
    /// registry went with that generation, and an empty registry must not
    /// read as "no policy objected". Fails only when rutis refuses the
    /// registration (e.g. the root was shut down).
    pub fn install(ctx: &Ctx) -> Result<Self, CordisError> {
        let registry = match ctx.get::<Registry>() {
            Some(existing) => existing,
            None => {
                ctx.provide(Registry::default())?;
                ctx.get::<Registry>()
                    .ok_or_else(|| CordisError::ServiceNotFound("Registry".into()))?
            }
        };
        #[cfg(any(
            feature = "node",
            feature = "python",
            feature = "bun",
            feature = "go",
            feature = "websocket"
        ))]
        crate::languages::provide(ctx, &registry)?;
        Ok(Self {
            host: Host::new(ctx),
            registry,
        })
    }

    /// The bridge's plugins as one yoagent `Extension`, with the defaults
    /// (advisory, finite timeouts). Install it with `Agent::with_extension`,
    /// or `with_tree_extension` to cover sub-agents too; see
    /// [`RutisExtension`] for the host's options.
    pub fn extension(&self) -> RutisExtension {
        RutisExtension::new(self.host.clone(), self.registry.clone())
    }

    /// The rutis context the bridge is installed on.
    pub fn ctx(&self) -> &Ctx {
        self.host.ctx()
    }

    /// Mark a runtime **restricted**: every handler registered over its
    /// connections — from a row or from none, from a Cordis child, after a
    /// restart — is tools only (see the [`registry`](crate::registry) docs).
    /// For plugins trusted with their tools and nothing else: a package
    /// found on npm and allowed for one session, say.
    ///
    /// Mark it **before it starts**: a handler is judged when it registers
    /// (its session against every session the runtime has run as — a
    /// restart's earlier process included), and keeps that for its life.
    /// The mark stays for the bridge's life.
    #[cfg(any(
        feature = "node",
        feature = "python",
        feature = "bun",
        feature = "go",
        feature = "websocket"
    ))]
    pub fn restrict_runtime(&self, runtime: rutis_bridge::runtime::RuntimeHandle) {
        self.registry.restrict_runtime(runtime);
    }

    /// The registry plugins add handlers to.
    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }
}

pub(crate) mod sealed {
    /// Seals [`PluginCtxExt`](crate::PluginCtxExt): implemented for rutis's
    /// `Ctx` here, and nowhere else.
    pub trait Sealed {}
    impl Sealed for rutis::Ctx {}
}
