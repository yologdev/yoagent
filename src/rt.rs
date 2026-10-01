//! Task and timer facilities that work on every target yoagent builds for.
//!
//! Native hosts get Tokio unchanged: [`spawn`], [`sleep`], [`timeout`] and
//! [`JoinHandle`] are Tokio's own, so behaviour there is exactly as before.
//!
//! On `wasm32-unknown-unknown` (e.g. Cloudflare Workers) there is no Tokio
//! runtime or timer driver. Tasks run on the host's single-threaded executor
//! through `wasm_bindgen_futures::spawn_local`, and timers use the host's
//! `setTimeout`. Futures need not be `Send` there, because nothing crosses a
//! thread.

#[cfg(not(target_arch = "wasm32"))]
pub use native::*;
#[cfg(target_arch = "wasm32")]
pub use wasm::*;

/// `Send` on native targets, nothing on wasm32.
///
/// Used as a bound where yoagent requires `Send` only so work can move
/// between threads. On wasm32 there is one thread, and host types such as
/// `fetch` futures are not `Send`.
#[cfg(not(target_arch = "wasm32"))]
pub trait MaybeSend: Send {}
#[cfg(not(target_arch = "wasm32"))]
impl<T: Send + ?Sized> MaybeSend for T {}
#[cfg(target_arch = "wasm32")]
pub trait MaybeSend {}
#[cfg(target_arch = "wasm32")]
impl<T: ?Sized> MaybeSend for T {}

/// `Sync` on native targets, nothing on wasm32. See [`MaybeSend`].
#[cfg(not(target_arch = "wasm32"))]
pub trait MaybeSync: Sync {}
#[cfg(not(target_arch = "wasm32"))]
impl<T: Sync + ?Sized> MaybeSync for T {}
#[cfg(target_arch = "wasm32")]
pub trait MaybeSync {}
#[cfg(target_arch = "wasm32")]
impl<T: ?Sized> MaybeSync for T {}

#[cfg(not(target_arch = "wasm32"))]
mod native {
    pub use tokio::task::{spawn, JoinError, JoinHandle};
    pub use tokio::time::{error::Elapsed, sleep, timeout, Instant};
}

#[cfg(target_arch = "wasm32")]
mod wasm {
    use futures::future::{AbortHandle, Abortable};
    use std::fmt;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::task::{Context, Poll};
    use std::time::Duration;
    use tokio::sync::oneshot;

    /// The host clock (`performance.now()`); `std::time::Instant` panics here.
    pub use web_time::Instant;

    /// Why a task produced no value: it was aborted, or it panicked.
    #[derive(Debug)]
    pub struct JoinError {
        aborted: bool,
    }

    impl JoinError {
        pub fn is_cancelled(&self) -> bool {
            self.aborted
        }
    }

    impl fmt::Display for JoinError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(if self.aborted {
                "task was cancelled"
            } else {
                "task failed"
            })
        }
    }

    impl std::error::Error for JoinError {}

    /// Handle to a task spawned with [`spawn`]: awaitable, abortable.
    pub struct JoinHandle<T> {
        rx: oneshot::Receiver<T>,
        abort: AbortHandle,
        finished: Arc<AtomicBool>,
    }

    impl<T> JoinHandle<T> {
        pub fn abort(&self) {
            self.abort.abort();
        }

        pub fn is_finished(&self) -> bool {
            self.finished.load(Ordering::SeqCst)
        }
    }

    impl<T> Future for JoinHandle<T> {
        type Output = Result<T, JoinError>;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let aborted = self.abort.is_aborted();
            Pin::new(&mut self.rx)
                .poll(cx)
                .map(|r| r.map_err(|_| JoinError { aborted }))
        }
    }

    /// Run `future` on the host executor. Unlike Tokio's, it need not be `Send`.
    pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
    where
        F: Future + 'static,
        F::Output: 'static,
    {
        let (tx, rx) = oneshot::channel();
        let (abort, registration) = AbortHandle::new_pair();
        let finished = Arc::new(AtomicBool::new(false));
        let done = finished.clone();
        wasm_bindgen_futures::spawn_local(async move {
            if let Ok(value) = Abortable::new(future, registration).await {
                let _ = tx.send(value);
            }
            done.store(true, Ordering::SeqCst);
        });
        JoinHandle {
            rx,
            abort,
            finished,
        }
    }

    /// Wait for `duration` using the host's `setTimeout`.
    pub async fn sleep(duration: Duration) {
        let ms = duration.as_millis().min(i32::MAX as u128) as i32;
        let promise = js_sys::Promise::new(&mut |resolve, _reject| {
            let global = js_sys::global();
            let set_timeout = js_sys::Reflect::get(&global, &"setTimeout".into())
                .ok()
                .and_then(|f| f.dyn_into::<js_sys::Function>().ok());
            match set_timeout {
                Some(f) => {
                    let _ = f.call2(&global, &resolve, &ms.into());
                }
                // No timer on this host: resolve at once rather than hang.
                None => {
                    let _ = resolve.call0(&global);
                }
            }
        });
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }

    /// The deadline passed before the future completed.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Elapsed;

    impl fmt::Display for Elapsed {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("deadline has elapsed")
        }
    }

    impl std::error::Error for Elapsed {}

    /// Run `future` until it completes or `duration` passes.
    pub async fn timeout<F: Future>(duration: Duration, future: F) -> Result<F::Output, Elapsed> {
        use futures::future::{select, Either};
        let future = std::pin::pin!(future);
        let timer = std::pin::pin!(sleep(duration));
        match select(future, timer).await {
            Either::Left((value, _)) => Ok(value),
            Either::Right(_) => Err(Elapsed),
        }
    }

    use wasm_bindgen::JsCast;
}
