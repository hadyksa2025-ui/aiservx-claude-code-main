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
    atomic::{AtomicBool, AtomicU8, Ordering},
    Arc,
};

use tokio::sync::Notify;

/// Why a [`CancelToken`] was tripped. Recorded at `cancel()` time and
/// read back through [`CancelToken::reason()`]; callers propagate it
/// into error strings / events so the UI, cost log, and tests can tell
/// user-initiated cancel apart from timeouts, goal-level cancel, and
/// the circuit breaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CancelReason {
    /// The token has never been tripped, or has since been `reset()`.
    None = 0,
    /// End user pressed Cancel in the UI (per-turn scope).
    User = 1,
    /// A goal-wide cancel fired (e.g. `cancel_goal`, goal timeout
    /// indirectly tripping the per-turn token).
    Goal = 2,
    /// A `tokio::time::timeout` elapsed and the caller tripped the
    /// token on its way out.
    Timeout = 3,
    /// Circuit breaker opened after too many consecutive failures.
    CircuitOpen = 4,
    /// A parent token fired and propagated through `link_from`.
    Parent = 5,
}

impl CancelReason {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => CancelReason::User,
            2 => CancelReason::Goal,
            3 => CancelReason::Timeout,
            4 => CancelReason::CircuitOpen,
            5 => CancelReason::Parent,
            _ => CancelReason::None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            CancelReason::None => "none",
            CancelReason::User => "user",
            CancelReason::Goal => "goal",
            CancelReason::Timeout => "timeout",
            CancelReason::CircuitOpen => "circuit_open",
            CancelReason::Parent => "parent",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct CancelToken {
    inner: Arc<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    flag: AtomicBool,
    reason: AtomicU8,
    notify: Notify,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// Trip the token with an explicit reason. Safe to call from any
    /// thread any number of times; the first call is the one that takes
    /// effect (later calls do not overwrite the reason). Notifies every
    /// current and future waiter.
    pub fn cancel_with(&self, reason: CancelReason) {
        let was = self.inner.flag.swap(true, Ordering::AcqRel);
        if !was {
            // Record reason before waking waiters so anyone who reads
            // `is_cancelled()` -> `reason()` sees a consistent pair.
            self.inner.reason.store(reason as u8, Ordering::Release);
            self.inner.notify.notify_waiters();
        }
        // Poke for any late-arriving waiter stuck between swap and await.
        self.inner.notify.notify_one();
    }

    /// Trip with the default `User` reason. Backwards-compatible shim.
    pub fn cancel(&self) {
        self.cancel_with(CancelReason::User);
    }

    /// Reset the token back to un-cancelled. Used to re-arm the per-turn
    /// token at the top of a new chat turn so prior cancel presses don't
    /// bleed into the next turn.
    pub fn reset(&self) {
        self.inner.flag.store(false, Ordering::Release);
        self.inner.reason.store(CancelReason::None as u8, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.flag.load(Ordering::Acquire)
    }

    /// Returns the reason the token was cancelled, or `None` if it has
    /// not been tripped. Safe to call from any thread at any time.
    pub fn reason(&self) -> CancelReason {
        CancelReason::from_u8(self.inner.reason.load(Ordering::Acquire))
    }

    /// Build an `Err` string of the form `"cancelled: <reason>"` for
    /// use as a return value. Callers can still substring-match on
    /// "cancelled" if they only need the boolean. If the token is not
    /// cancelled, falls back to `"cancelled"` with no reason.
    pub fn err_string(&self) -> String {
        if self.is_cancelled() {
            format!("cancelled: {}", self.reason().as_str())
        } else {
            "cancelled".to_string()
        }
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
    /// also fires with [`CancelReason::Parent`]. Returns immediately and
    /// runs the propagator as a detached `tokio::spawn`. If `parent` is
    /// already cancelled, cancels `self` synchronously before returning.
    ///
    /// Fire-and-forget by design: there is no handle returned and no
    /// cleanup if the child token is dropped before the parent fires.
    /// That is fine for the current call sites (one propagator per
    /// goal / per turn), but callers that create tokens dynamically in a
    /// hot loop should reach for an explicit parent-registry instead.
    pub fn link_from(&self, parent: &CancelToken) {
        if parent.is_cancelled() {
            self.cancel_with(CancelReason::Parent);
            return;
        }
        let this = self.clone();
        let parent_clone = parent.clone();
        tokio::spawn(async move {
            parent_clone.cancelled().await;
            this.cancel_with(CancelReason::Parent);
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

    #[tokio::test]
    async fn reason_is_preserved_through_cancel_with() {
        let t = CancelToken::new();
        assert_eq!(t.reason(), CancelReason::None);
        t.cancel_with(CancelReason::Timeout);
        assert!(t.is_cancelled());
        assert_eq!(t.reason(), CancelReason::Timeout);
        assert_eq!(t.err_string(), "cancelled: timeout");
    }

    #[tokio::test]
    async fn first_reason_wins_subsequent_cancels_are_no_ops() {
        let t = CancelToken::new();
        t.cancel_with(CancelReason::User);
        t.cancel_with(CancelReason::Timeout);
        assert_eq!(t.reason(), CancelReason::User);
    }

    #[tokio::test]
    async fn link_from_records_parent_reason() {
        let parent = CancelToken::new();
        let child = CancelToken::new();
        child.link_from(&parent);
        parent.cancel_with(CancelReason::Goal);
        tokio::time::timeout(Duration::from_millis(200), child.cancelled())
            .await
            .expect("child should fire");
        assert_eq!(child.reason(), CancelReason::Parent);
        assert_eq!(parent.reason(), CancelReason::Goal);
    }

    #[tokio::test]
    async fn reset_clears_reason_too() {
        let t = CancelToken::new();
        t.cancel_with(CancelReason::CircuitOpen);
        t.reset();
        assert_eq!(t.reason(), CancelReason::None);
        assert_eq!(t.err_string(), "cancelled");
    }
}
