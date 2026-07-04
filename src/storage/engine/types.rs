//! Public ancillary types returned by [`KvEngine`] command methods.
//!
//! Kept in a dedicated module so [`super::mod`] can stay focused on the
//! engine's command API; both types are re-exported at `engine::` level for
//! path stability with existing callers (`network/server.rs`,
//! `persistence/replay.rs`, integration tests).

/// Outcome of a single call to [`KvEngine::sweep_expired`](super::KvEngine::sweep_expired).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SweepStats {
    /// Number of entries inspected during the sweep.
    pub examined: usize,
    /// Number of entries that were actually expired and removed.
    pub evicted: usize,
}

impl SweepStats {
    /// Returns `true` when the expired ratio warrants a follow-up sweep.
    ///
    /// Matches Redis' active expiration heuristic: keep running while at
    /// least 25% of the sample is expired.
    pub fn should_continue(&self) -> bool {
        self.examined > 0 && self.evicted * 4 > self.examined
    }
}

/// Tri-state return for [`KvEngine::ttl_ms`](super::KvEngine::ttl_ms).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtlStatus {
    /// The key does not exist; RESP reply is `:-2`.
    Missing,
    /// The key exists with no TTL; RESP reply is `:-1`.
    NoExpire,
    /// Remaining TTL in milliseconds (`>= 0`).
    Millis(i64),
}
