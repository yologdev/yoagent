//! Is the rutis host the bridge was installed on still running.

use rutis::{Ctx, FiberState};
use tokio_util::sync::CancellationToken;

pub(crate) const NOT_RUNNING: &str =
    "the plugin host is not running (shut down, disposed or restarted since the bridge was installed)";

/// The context the bridge was installed on, bound to the generation it was
/// installed in.
#[derive(Clone)]
pub(crate) struct Host {
    ctx: Ctx,
    /// The installing context's generation token at install time. A root
    /// that is disposed or restarted cancels it for good: the registry the
    /// bridge holds was disposed with that generation, so a restarted root
    /// needs a new bridge.
    generation: CancellationToken,
}

impl Host {
    pub(crate) fn new(ctx: &Ctx) -> Self {
        Self {
            ctx: ctx.clone(),
            generation: ctx.cancellation_token(),
        }
    }

    pub(crate) fn ctx(&self) -> &Ctx {
        &self.ctx
    }

    /// Whether the host can no longer serve plugins: the generation the
    /// bridge was installed in has ended (its fiber started unloading, the
    /// root shut down, was disposed or restarted), or the root fiber is
    /// unloading or disposed.
    ///
    /// Every plugin unloads with the host, so its registry is then empty —
    /// which must not read as "no policy objected". A bridge installed on a
    /// *plugin's* context is bound to that plugin's generation: once the
    /// plugin unloads or reloads, it reads as stopped for good — install on
    /// the root.
    pub(crate) fn is_closed(&self) -> bool {
        let root_stopped = self.ctx.root_view().is_none_or(|root| {
            matches!(
                root.state().state,
                FiberState::Unloading | FiberState::Disposed
            )
        });
        self.generation.is_cancelled()
            || self.ctx.cancellation_token().is_cancelled()
            || root_stopped
    }
}
