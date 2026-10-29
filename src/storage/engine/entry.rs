//! The per-key value record stored inside the engine.
//!
//! Centralising [`ValueEntry`] here keeps the storage record's shape and its
//! LFU/timestamp maintenance separate from the command-level [`KvEngine`] API
//! in [`super::mod`].
//!
//! The LRU/LFU metadata is *interior mutable* (atomics) on purpose: read
//! commands hold the keyspace `RwLock` in shared mode and still need to record
//! an access. Without that, every `GET` would have to take the exclusive lock
//! and the whole server would serialise on reads (measured before the change:
//! one core's worth of throughput regardless of client count).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::storage::eviction::{self, LFU_INIT_VAL};

/// Minutes required for the Morris counter to lose one unit while the key
/// sits idle. Matches Redis' default `lfu-decay-time` of 1 minute.
pub(crate) const LFU_DECAY_MINUTES: u16 = 1;

/// Monotonic origin for the packed `last_access` timestamps.
///
/// `Instant` is opaque, so it cannot be stored in an atomic directly; entries
/// store nanoseconds since this process-wide origin instead.
fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

fn now_ns() -> u64 {
    epoch().elapsed().as_nanos().min(u64::MAX as u128) as u64
}

fn instant_from_ns(ns: u64) -> Instant {
    epoch() + Duration::from_nanos(ns)
}

/// Packs the Morris counter (low 8 bits) and the decay minute (high 16 bits)
/// into one word so a touch updates both with a single CAS.
fn pack_lfu(counter: u8, minute: u16) -> u32 {
    ((minute as u32) << 16) | counter as u32
}

fn unpack_lfu(word: u32) -> (u8, u16) {
    ((word & 0xFF) as u8, ((word >> 16) & 0xFFFF) as u16)
}

/// A value stored in the engine, together with its optional expiration.
///
/// `expire_at` is a monotonic deadline derived from [`Instant::now`] at the
/// time the TTL was installed. A value whose deadline is `<= Instant::now()`
/// is considered expired and must be treated as absent by every read path.
/// It is only ever mutated while the keyspace write lock is held, so readers
/// holding the shared lock observe a stable value.
///
/// `last_access` tracks the most recent successful read or write. Only the
/// approximate LRU policies consult it, so keeping it current is best-effort.
///
/// `lfu` is a Morris probabilistic counter (0..=255) plus its decay minute,
/// read by the LFU / AHE policies. It is decayed lazily on access, so idle keys
/// lose heat over time without the engine having to sweep every key
/// periodically.
pub(crate) struct ValueEntry {
    pub(crate) data: Vec<u8>,
    pub(crate) expire_at: Option<Instant>,
    /// Nanoseconds since [`epoch`] at the last access.
    last_access_ns: AtomicU64,
    /// Packed `(decay_minute << 16) | counter`.
    lfu: AtomicU32,
}

impl Clone for ValueEntry {
    fn clone(&self) -> Self {
        Self {
            data: self.data.clone(),
            expire_at: self.expire_at,
            last_access_ns: AtomicU64::new(self.last_access_ns.load(Ordering::Relaxed)),
            lfu: AtomicU32::new(self.lfu.load(Ordering::Relaxed)),
        }
    }
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
            last_access_ns: AtomicU64::new(now_ns()),
            lfu: AtomicU32::new(pack_lfu(LFU_INIT_VAL, eviction::current_minute_stamp())),
        }
    }

    pub(crate) fn is_expired(&self, now: Instant) -> bool {
        matches!(self.expire_at, Some(deadline) if deadline <= now)
    }

    /// `Instant` of the most recent access.
    pub(crate) fn last_access(&self) -> Instant {
        instant_from_ns(self.last_access_ns.load(Ordering::Relaxed))
    }

    /// Current Morris counter (0..=255); higher means hotter.
    pub(crate) fn lfu_counter(&self) -> u8 {
        unpack_lfu(self.lfu.load(Ordering::Relaxed)).0
    }

    /// Marks the entry as freshly accessed.
    ///
    /// Updates both the LRU timestamp and the LFU counter. `rand01` must
    /// be drawn from `[0.0, 1.0)` and is used to decide whether the Morris
    /// counter actually advances this call.
    ///
    /// Takes `&self`: concurrent readers may touch the same entry, so the
    /// update is a CAS loop over the packed counter/minute word.
    pub(crate) fn touch(&self, rand01: f32) {
        self.last_access_ns.store(now_ns(), Ordering::Relaxed);
        let now_minute = eviction::current_minute_stamp();
        let mut current = self.lfu.load(Ordering::Relaxed);
        loop {
            let (counter, minute) = unpack_lfu(current);
            let decayed = eviction::decayed_counter(counter, minute, now_minute, LFU_DECAY_MINUTES);
            let incremented = eviction::probabilistic_increment(decayed, rand01);
            let next = pack_lfu(incremented, now_minute);
            match self.lfu.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touch_advances_lfu_and_last_access() {
        let entry = ValueEntry::new(b"v".to_vec());
        let start = entry.last_access();
        std::thread::sleep(Duration::from_millis(2));
        entry.touch(0.0);
        assert!(entry.last_access() >= start);
        assert_eq!(entry.lfu_counter(), LFU_INIT_VAL + 1);
    }

    #[test]
    fn clone_preserves_metadata() {
        let entry = ValueEntry::new(b"v".to_vec());
        entry.touch(0.0);
        let copy = entry.clone();
        assert_eq!(copy.lfu_counter(), entry.lfu_counter());
        assert_eq!(copy.last_access(), entry.last_access());
    }
}
