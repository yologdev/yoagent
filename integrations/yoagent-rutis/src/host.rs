//! Is the rutis host still running.

use rutis::{Ctx, FiberState};

const NOT_RUNNING: &str =
    "the plugin host is not running (shut down, or disposed and not restarted)";

/// Why the host can no longer serve plugins, if it cannot: the bridge's
/// context generation was cancelled (its fiber started unloading, or the root
/// shut down) or the root fiber is unloading or disposed.
///
/// Every plugin unloads with the host, so its registry is then empty — which
/// must not read as "no policy objected". A root that was disposed (and may
/// later restart) reads the same as one that shut down. A bridge installed on
/// a *plugin's* context is bound to that plugin's generation: once the
/// plugin unloads or reloads, it reads as stopped for good — install on the
/// root.
pub(crate) fn closed(ctx: &Ctx) -> Option<&'static str> {
    let root_stopped = ctx.root_view().is_none_or(|root| {
        matches!(
            root.state().state,
            FiberState::Unloading | FiberState::Disposed
        )
    });
    (ctx.cancellation_token().is_cancelled() || root_stopped).then_some(NOT_RUNNING)
}
