//! Is the rutis host still able to dispatch?

use rutis::{Ctx, FiberState};

/// Why the host can no longer run plugin chains, if it cannot.
///
/// rutis 0.5 does not refuse a non-instance `waterfall` / `serial` dispatch
/// after the root shut down: it drains every listener and then dispatches to
/// an empty chain, which would read as "no policy objected". So the bridge
/// checks liveness itself, before every dispatch: the dispatching context's
/// generation token (cancelled when its fiber starts unloading or the root
/// shuts down) and the root fiber's state.
pub(crate) fn closed(ctx: &Ctx) -> Option<&'static str> {
    if ctx.cancellation_token().is_cancelled() {
        return Some("the plugin host has shut down");
    }
    match ctx.root_view() {
        None => Some("the plugin host has shut down"),
        Some(root) => match root.state().state {
            FiberState::Unloading | FiberState::Disposed => Some("the plugin host has shut down"),
            _ => None,
        },
    }
}
