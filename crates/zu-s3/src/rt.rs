//! A tiny single-future executor so the crate needs no async runtime.
//!
//! `object_store` exposes an async API, but the backends this crate drives complete without a reactor: `InMemory` futures resolve immediately and `LocalFileSystem` runs its blocking work inline when no tokio runtime is active.
//! Cloud backends complete on their own transport threads and wake us, so parking the calling thread until woken is a correct general executor.

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};

struct ThreadWaker(Thread);

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

/// Drives `future` to completion on the calling thread.
pub(crate) fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let waker = Waker::from(Arc::new(ThreadWaker(thread::current())));
    let mut cx = Context::from_waker(&waker);
    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(value) => return value,
            Poll::Pending => thread::park(),
        }
    }
}
