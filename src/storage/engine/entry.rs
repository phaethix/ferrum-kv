//! The per-key value record stored inside the engine.
//!
//! Centralising [`ValueEntry`] here keeps the storage record's shape and its
//! LFU/timestamp maintenance separate from the command-level [`KvEngine`] API
//! in [`super::mod`].

use std::time::Instant;

use crate::storage::eviction::{self, LFU_INIT_VAL};

/// Minutes required for the Morris counter to lose one unit while the key
/// sits idle. Matches Redis' default `lfu-decay-time` of 1 minute.
pub(crate) const LFU_DECAY_MINUTES: u16 = 1;

/// A value stored in the engine, together with its optional expiration.
///
/// `expire_at` is a monotonic deadline derived from [`Instant::now`] at the
/// time the TTL was installed. A value whose deadline is `<= Instant::now()`
/// is considered expired and must be treated as absent by every read path.
///
/// `last_access` tracks the most recent successful read or write and is
/// maintained inside the store's write lock. Only the approximate LRU
/// policies consult it, so keeping it current is best-effort.
///
/// `lfu_counter` is a Morris probabilistic counter (0..=255) read by the
/// LFU / AHE policies. It is decayed lazily on access using
/// `lfu_decay_minute`, so idle keys lose heat over time without the engine
/// having to sweep every key periodically.
#[derive(Clone)]
pub(crate) struct ValueEntry {
    pub(crate) data: Vec<u8>,
    pub(crate) expire_at: Option<Instant>,
    pub(crate) last_access: Instant,
    pub(crate) lfu_counter: u8,
    pub(crate) lfu_decay_minute: u16,
}

impl ValueEntry {
    /// Creates a fresh entry with default LFU metadata.
    ///
    /// The counter starts at [`LFU_INIT_VAL`] (mirroring Redis) so brand
    /// new keys are not evicted before they have had a chance to be
    /// re-accessed.
    pub(crate) fn new(data: Vec<u8>) -> Self {
        Self::new_with(data, None)
    }

    /// Like [`Self::new`] but preserves an existing TTL, used by write
    /// paths that overwrite a key while honouring `KEEPTTL` semantics.
    pub(crate) fn new_with(data: Vec<u8>, expire_at: Option<Instant>) -> Self {
        Self {
            data,
            expire_at,
            last_access: Instant::now(),
            lfu_counter: LFU_INIT_VAL,
            lfu_decay_minute: eviction::current_minute_stamp(),
        }
    }

    pub(crate) fn is_expired(&self, now: Instant) -> bool {
        matches!(self.expire_at, Some(deadline) if deadline <= now)
    }

    /// Marks the entry as freshly accessed.
    ///
    /// Updates both the LRU timestamp and the LFU counter. `rand01` must
    /// be drawn from `[0.0, 1.0)` and is used to decide whether the Morris
    /// counter actually advances this call.
    pub(crate) fn touch(&mut self, rand01: f32) {
        self.last_access = Instant::now();
        let now_minute = eviction::current_minute_stamp();
        let decayed = eviction::decayed_counter(
            self.lfu_counter,
            self.lfu_decay_minute,
            now_minute,
            LFU_DECAY_MINUTES,
        );
        self.lfu_counter = eviction::probabilistic_increment(decayed, rand01);
        self.lfu_decay_minute = now_minute;
    }
}

/// Returns the payload of `entry` if it has not already expired.
pub(crate) fn live_payload(entry: ValueEntry) -> Option<Vec<u8>> {
    let now = Instant::now();
    if entry.is_expired(now) {
        None
    } else {
        Some(entry.data)
    }
}
