//! Cooperative cancellation primitive shared by the tool, AI loop, and
//! controller layers.
//!
//! Design goals:
//! - Cheap `Clone` (it's just `Arc<Inner>`).
//! - Sync `is_cancelled()` — any code path can branch without awaiting.
//! - Async `cancelled()` — returns a future that resolves as soon as the
//!   token is tripped. Suitable for `tokio::select!`.
//! - Linking: a child token can be wired to also fire when any of its
//!   parents fire (`link_from(&parent)`), which lets us combine
//!   `goal_cancelled`, `cancelled`, plus a per-task token into one future
//!   the tool layer can await without knowing the whole tree.
//! - No external deps beyond tokio.
//!
//! All operations are lock-free on the hot path: `is_cancelled()` is an
//! `Ordering::Acquire` atomic load, and `cancel()` does a single compare
//! then a `Notify::notify_waiters()` (no-op if nobody is waiting).

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use tokio::sync::Notify;

#[derive(Clone, Debug, Default)]
pub struct CancelToken {
    inner: Arc<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    flag: AtomicBool,
    notify: Notify,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// Trip the token. Safe to call from any thread any number of times;
    /// only the first call actually transitions state, but all calls are
    /// no-op-safe. Notifies every current and future waiter.
    pub fn cancel(&self) {
        let was = self.inner.flag.swap(true, Ordering::AcqRel);
        if !was {
            // Wake anything currently awaiting `cancelled()`.
            self.inner.notify.notify_waiters();
        }
        // Also poke so a waiter that arrives *after* the swap but before
        // the `await` doesn't get stuck.
        self.inner.notify.notify_one();
    }

    /// Reset the token back to un-cancelled. Used to re-arm the per-turn
    /// token at the top of a new chat turn so prior cancel presses don't
    /// bleed into the next turn.
    pub fn reset(&self) {
        self.inner.flag.store(false, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.flag.load(Ordering::Acquire)
    }

    /// Returns a future that resolves immediately if the token is already
    /// cancelled, or the first time `cancel()` is called afterwards.
    /// Holding the returned future across `await` points is safe.
    pub async fn cancelled(&self) {
        loop {
            if self.is_cancelled() {
                return;
            }
            let notified = self.inner.notify.notified();
            // Re-check after registering to avoid the classic lost-wakeup
            // race between `notify_waiters()` and `notified()` registration.
            if self.is_cancelled() {
                return;
            }
            notified.await;
            // Loop around: Notify::notified() can legally wake spuriously.
        }
    }

    /// Link another token into this one: when `parent` fires, this token
    /// also fires. Returns immediately; runs the propagator as a detached
    /// tokio task. If `parent` is already cancelled, cancels `self`
    /// synchronously before returning.
    pub fn link_from(&self, parent: &CancelToken) {
        if parent.is_cancelled() {
            self.cancel();
            return;
        }
        let this = self.clone();
        let parent_clone = parent.clone();
        tokio::spawn(async move {
            parent_clone.cancelled().await;
            this.cancel();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::sleep;

    #[tokio::test]
    async fn cancel_before_await_resolves_immediately() {
        let t = CancelToken::new();
        t.cancel();
        tokio::time::timeout(Duration::from_millis(50), t.cancelled())
            .await
            .expect("cancelled() should resolve for a pre-cancelled token");
        assert!(t.is_cancelled());
    }

    #[tokio::test]
    async fn cancel_during_await_wakes_waiter() {
        let t = CancelToken::new();
        let t2 = t.clone();
        let fut = tokio::spawn(async move {
            t2.cancelled().await;
        });
        // Give the waiter a chance to register.
        sleep(Duration::from_millis(10)).await;
        t.cancel();
        tokio::time::timeout(Duration::from_millis(200), fut)
            .await
            .expect("waiter should be woken")
            .unwrap();
    }

    #[tokio::test]
    async fn link_from_propagates_cancel() {
        let parent = CancelToken::new();
        let child = CancelToken::new();
        child.link_from(&parent);
        parent.cancel();
        tokio::time::timeout(Duration::from_millis(200), child.cancelled())
            .await
            .expect("child should fire when parent fires");
        assert!(child.is_cancelled());
    }

    #[tokio::test]
    async fn link_from_already_cancelled_parent_cancels_child_immediately() {
        let parent = CancelToken::new();
        parent.cancel();
        let child = CancelToken::new();
        child.link_from(&parent);
        assert!(child.is_cancelled());
    }

    #[tokio::test]
    async fn reset_clears_flag() {
        let t = CancelToken::new();
        t.cancel();
        assert!(t.is_cancelled());
        t.reset();
        assert!(!t.is_cancelled());
        // After reset, a fresh await should block until the next cancel.
        let t2 = t.clone();
        let handle = tokio::spawn(async move {
            t2.cancelled().await;
        });
        sleep(Duration::from_millis(20)).await;
        assert!(!handle.is_finished());
        t.cancel();
        tokio::time::timeout(Duration::from_millis(200), handle)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn uncancelled_token_does_not_spuriously_fire() {
        let t = CancelToken::new();
        let res = tokio::time::timeout(Duration::from_millis(50), t.cancelled()).await;
        assert!(res.is_err(), "cancelled() should not resolve without cancel");
    }
}
