//! MONITOR command — streams every processed command to subscribed clients.
//!
//! When a client issues `MONITOR` the connection enters a dedicated loop that
//! receives every command the server processes via unbounded mpsc channels.
//! The sending side is non-blocking so monitored connections never slow down
//! normal command execution.
//!
//! Broadcasts are gated on [`MonitorRegistry::has_subscribers`]: the command
//! hot path used to clone the full argv (including multi-megabyte values) for
//! every command even when nobody was listening.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

/// One command snapshot broadcast to all MONITOR clients.
#[derive(Clone)]
pub(crate) struct MonitorMessage {
    /// Absolute millisecond timestamp (matches Redis' PEXPIREAT epoch).
    pub(crate) timestamp_ms: i64,
    /// Address of the client that issued the command.
    pub(crate) peer: SocketAddr,
    /// The command and its arguments (e.g. `["SET", "key", "value"]`).
    pub(crate) args: Vec<Vec<u8>>,
}

/// Thread-safe registry of active MONITOR clients.
///
/// Shared via `Arc` so it can be stored on [`crate::storage::engine::KvEngine`]
/// (and thus automatically cloned into every connection task via
/// `engine.clone()`).
#[derive(Clone, Default)]
pub(crate) struct MonitorRegistry {
    senders: Arc<Mutex<Vec<UnboundedSender<MonitorMessage>>>>,
}

impl MonitorRegistry {
    /// Registers a new MONITOR client and returns its receiver handle.
    /// The corresponding sender is automatically removed from the registry
    /// when the receiver is dropped.
    pub(crate) fn subscribe(&self) -> UnboundedReceiver<MonitorMessage> {
        let (tx, rx) = mpsc::unbounded_channel();
        if let Ok(mut guard) = self.senders.lock() {
            guard.push(tx);
        }
        rx
    }

    /// `true` when at least one client is monitoring. Lets the hot path skip
    /// building a [`MonitorMessage`] entirely.
    #[inline]
    pub(crate) fn has_subscribers(&self) -> bool {
        // A poisoned lock means a monitor connection panicked; treat it as
        // "nobody is listening" rather than panicking the data path.
        self.senders.lock().map(|g| !g.is_empty()).unwrap_or(false)
    }

    /// Sends a command snapshot to every active MONITOR client.
    /// Dead senders (disconnected clients) are lazily purged.
    pub(crate) fn send(&self, msg: MonitorMessage) {
        let Ok(mut guard) = self.senders.lock() else {
            return;
        };
        guard.retain(|tx| tx.send(msg.clone()).is_ok());
    }
}
