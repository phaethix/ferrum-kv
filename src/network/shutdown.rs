//! Cooperative shutdown signalling for the TCP server.
//!
//! A [`Shutdown`] is a cheap, cloneable handle wrapping an `AtomicBool`. The
//! accept loop and every connection handler consult it between blocking
//! operations; when a signal handler (or a test) flips the flag, all of them
//! wind down at the next opportunity.
//!
//! Because [`TcpListener::accept`] is a blocking syscall that will not return
//! on its own when the flag is flipped, the caller also remembers the bound
//! address and opens a short-lived local TCP connection via
//! [`Shutdown::wake_listener`]. That stray connection unblocks `accept`; the
//! loop then notices the flag and exits cleanly.

use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Shared cooperative shutdown flag.
///
/// Cloning a `Shutdown` is cheap: it only bumps the refcount on the inner
/// `Arc`. All clones observe the same boolean.
#[derive(Clone, Default)]
pub struct Shutdown {
    flag: Arc<AtomicBool>,
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

    /// Flips the flag so every observer will see `is_triggered() == true`.
    ///
    /// Safe to call multiple times and from multiple threads; subsequent
    /// calls are no-ops.
    pub fn trigger(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    /// Wakes a blocked [`std::net::TcpListener::accept`] bound to `addr` by
    /// opening a single short-lived connection to it.
    ///
    /// The connection is immediately dropped; the accept loop will receive an
    /// `Ok(stream)`, observe the shutdown flag, and exit before doing any
    /// work. Errors are swallowed because by the time we try to self-connect
    /// the listener may already be closed — that is harmless.
    pub fn wake_listener(addr: SocketAddr) {
        if let Ok(stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(200)) {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
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
}
