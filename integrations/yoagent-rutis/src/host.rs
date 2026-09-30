//! Is the rutis host still able to dispatch?

use rutis::{Ctx, FiberState};

const NOT_RUNNING: &str =
    "the plugin host is not running (shut down, or disposed and not restarted)";

/// Why the host can no longer run plugin chains, if it cannot.
///
/// rutis 0.5 does not refuse a non-instance `waterfall` / `serial` dispatch
/// after the root shut down: it drains every listener and then dispatches to
/// an empty chain, which would read as "no policy objected". So the bridge
/// checks liveness itself, before every dispatch: the dispatching context's
/// generation token (cancelled when its fiber starts unloading or the root
/// shuts down) and the root fiber's state.
///
/// A root that was disposed (and may later restart) reads the same as one
/// that shut down: until it runs again, an attached agent's tool calls are
/// all denied — its own tools included — and its prompts rejected.
pub(crate) fn closed(ctx: &Ctx) -> Option<&'static str> {
    if ctx.cancellation_token().is_cancelled() {
        return Some(NOT_RUNNING);
    }
    match ctx.root_view() {
        None => Some(NOT_RUNNING),
        Some(root) => match root.state().state {
            FiberState::Unloading | FiberState::Disposed => Some(NOT_RUNNING),
            _ => None,
        },
    }
}
