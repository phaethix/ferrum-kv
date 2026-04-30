//! Cooperative shutdown signalling for the TCP server.
//!
//! A [`Shutdown`] is a cheap, cloneable handle wrapping an `AtomicBool` plus
//! a [`tokio::sync::Notify`]. Synchronous observers (the background expiration
//! sweeper, tests) consult the atomic flag between blocking operations, while
//! async observers (the accept loop, per-connection handlers) park on
//! [`Shutdown::notified`] and wake up instantly when the flag flips.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;

/// Shared cooperative shutdown flag.
///
/// Cloning a `Shutdown` is cheap: it only bumps the refcount on the inner
/// `Arc`. All clones observe the same boolean and share the same waker.
#[derive(Clone, Default)]
pub struct Shutdown {
    flag: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl Shutdown {
    /// Creates a new, un-triggered shutdown handle.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `true` once [`trigger`](Self::trigger) has been called from
    /// anywhere.
    pub fn is_triggered(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// Flips the flag so every observer will see `is_triggered() == true`
    /// and wakes every task currently parked on [`notified`](Self::notified).
    ///
    /// Safe to call multiple times and from multiple threads; subsequent
    /// calls re-notify waiters but are otherwise harmless.
    pub fn trigger(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    /// Async future that resolves as soon as [`trigger`](Self::trigger) is
    /// called.
    ///
    /// If the flag is already set the future returns on the next poll, so
    /// callers that take the shutdown path after a blocking operation do not
    /// deadlock.
    pub async fn notified(&self) {
        if self.is_triggered() {
            return;
        }
        let waiter = self.notify.notified();
        // Re-check after arming the waiter to plug the race where `trigger`
        // fires between the `is_triggered` probe above and installing the
        // waiter.
        if self.is_triggered() {
            return;
        }
        waiter.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_is_not_triggered() {
        let s = Shutdown::new();
        assert!(!s.is_triggered());
    }

    #[test]
    fn trigger_is_visible_to_clones() {
        let a = Shutdown::new();
        let b = a.clone();
        a.trigger();
        assert!(b.is_triggered());
    }

    #[test]
    fn trigger_is_idempotent() {
        let s = Shutdown::new();
        s.trigger();
        s.trigger();
        assert!(s.is_triggered());
    }

    #[tokio::test]
    async fn notified_resolves_after_trigger() {
        let s = Shutdown::new();
        let s2 = s.clone();
        let task = tokio::spawn(async move { s2.notified().await });
        // Give the task a chance to park on `notified`.
        tokio::task::yield_now().await;
        s.trigger();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn notified_returns_immediately_if_already_triggered() {
        let s = Shutdown::new();
        s.trigger();
        s.notified().await;
    }
}
