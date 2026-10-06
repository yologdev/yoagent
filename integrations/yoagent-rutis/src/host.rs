//! Is the rutis host still able to dispatch — and running one plugin chain.

use std::future::Future;
use std::time::Duration;

use futures::FutureExt;
use rutis::{CordisError, Ctx, FiberState};

const NOT_RUNNING: &str =
    "the plugin host is not running (shut down, or disposed and not restarted)";

/// Why the host can no longer run plugin chains, if it cannot.
///
/// rutis 0.6 does not refuse a non-instance `waterfall` / `serial` dispatch
/// after the root shut down: it drains every listener and then dispatches to
/// an empty chain, which would read as "no policy objected". So the bridge
/// checks liveness itself, before every dispatch: the dispatching context's
/// generation token (cancelled when its fiber starts unloading or the root
/// shuts down) and the root fiber's state.
///
/// A root that was disposed (and may later restart) reads the same as one
/// that shut down: until it runs again, an attached agent's tool calls are
/// all denied — its own tools included — and its prompts rejected. A bridge
/// installed on a *plugin's* context is bound to that plugin's generation:
/// once the plugin unloads or reloads, that token stays cancelled and the
/// bridge reads as stopped for good — install on the root.
pub(crate) fn closed(ctx: &Ctx) -> Option<&'static str> {
    let root_stopped = ctx.root_view().is_none_or(|root| {
        matches!(
            root.state().state,
            FiberState::Unloading | FiberState::Disposed
        )
    });
    (ctx.cancellation_token().is_cancelled() || root_stopped).then_some(NOT_RUNNING)
}

/// How one dispatch of a plugin chain ended. Every hook maps each arm
/// explicitly — there is deliberately no catch-all.
pub(crate) enum Outcome<T> {
    /// The host is not running; the chain was not dispatched.
    Closed(&'static str),
    /// The chain did not finish within this bound; it was abandoned.
    TimedOut(Duration),
    /// A listener panicked (rutis propagates waterfall panics to the
    /// dispatcher).
    Panicked,
    /// The chain ran to completion, successfully or with an error.
    Finished(Result<T, CordisError>),
}

/// Check the host, then run `chain()` under `timeout` with panics contained.
/// `chain` is only called when the host is running.
pub(crate) async fn run_chain<T, F>(
    ctx: &Ctx,
    timeout: Option<Duration>,
    chain: impl FnOnce() -> F,
) -> Outcome<T>
where
    F: Future<Output = Result<T, CordisError>>,
{
    if let Some(why) = closed(ctx) {
        return Outcome::Closed(why);
    }
    let guarded = std::panic::AssertUnwindSafe(chain()).catch_unwind();
    let caught = match timeout {
        Some(limit) => match tokio::time::timeout(limit, guarded).await {
            Ok(caught) => caught,
            Err(_) => return Outcome::TimedOut(limit),
        },
        None => guarded.await,
    };
    match caught {
        Ok(result) => Outcome::Finished(result),
        Err(_) => Outcome::Panicked,
    }
}
