//! Free-standing helpers used by the [`KvEngine`](super::KvEngine) implementation.
//!
//! These functions do not depend on engine state and are grouped here so the
//! main engine file reads as a command API. Visibility is `pub(super)` so the
//! sibling `mod` (and its `impl KvEngine` blocks) can call them, while they
//! stay hidden from the rest of the crate.

use std::collections::HashMap;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use log::warn;

use crate::error::FerrumError;
use crate::storage::eviction::{Candidate, EvictionScope};

use super::entry::ValueEntry;
use super::{KEY_MAX_BYTES, PER_ENTRY_OVERHEAD, VALUE_MAX_BYTES};

/// Approximate byte cost of a single live entry.
pub(super) fn entry_bytes(key: &[u8], entry: &ValueEntry) -> u64 {
    key.len() as u64 + entry.data.len() as u64 + PER_ENTRY_OVERHEAD
}

/// Samples up to `sample` candidates from `store` under the given scope.
///
/// `HashMap`'s iteration order is already randomised by its hasher, so we
/// rely on that for "pick at random" without introducing an extra RNG
/// dependency. For `EvictionScope::Volatile` we skip keys without a TTL and
/// keep walking until the cap is reached or the store is exhausted.
pub(super) fn sample_candidates(
    store: &HashMap<Vec<u8>, ValueEntry>,
    scope: EvictionScope,
    sample: usize,
) -> Vec<Candidate> {
    let mut out = Vec::with_capacity(sample);
    for (key, entry) in store.iter() {
        if out.len() >= sample {
            break;
        }
        if matches!(scope, EvictionScope::Volatile) && entry.expire_at.is_none() {
            continue;
        }
        out.push(Candidate {
            key: key.clone(),
            last_access: Some(entry.last_access),
            expire_at: entry.expire_at,
            lfu_counter: entry.lfu_counter,
        });
    }
    out
}

/// Current wall-clock time expressed as Unix epoch milliseconds.
///
/// A system clock earlier than the Unix epoch (extremely unusual in
/// practice) falls back to zero so the engine never panics.
pub(crate) fn current_epoch_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Converts a monotonic `deadline` to a Unix epoch millisecond timestamp.
///
/// Returns `None` when the deadline is already in the past (the caller will
/// drop the key instead of writing an expiration record).
pub(super) fn deadline_to_epoch_ms(deadline: Instant, now: Instant) -> Option<i64> {
    if deadline <= now {
        return None;
    }
    let remaining = deadline - now;
    let now_ms = current_epoch_ms();
    let abs_ms = now_ms.saturating_add(remaining.as_millis() as i64);
    Some(abs_ms)
}

pub(super) fn validate_key(key: &[u8]) -> Result<(), FerrumError> {
    let len = key.len();
    if len == 0 {
        return Err(FerrumError::ParseError("key must not be empty".into()));
    }
    if len > KEY_MAX_BYTES {
        return Err(FerrumError::KeyTooLong {
            len,
            max: KEY_MAX_BYTES,
        });
    }
    Ok(())
}

pub(super) fn validate_value(value: &[u8]) -> Result<(), FerrumError> {
    let len = value.len();
    if len > VALUE_MAX_BYTES {
        return Err(FerrumError::ValueTooLarge {
            len,
            max: VALUE_MAX_BYTES,
        });
    }
    Ok(())
}

/// Logs AOF append failures without failing the originating command.
///
/// The whitepaper (§7.2) specifies that persistence errors are best-effort:
/// in-memory state is authoritative during runtime and a failed AOF append is
/// reported but does not propagate to the client.
pub(super) fn log_aof_result(cmd: &str, result: Result<(), FerrumError>) {
    if let Err(e) = result {
        warn!("aof append for {cmd} failed: {e}");
    }
}

/// Builds the initial xorshift32 seed. Mixes the low 32 bits of the wall
/// clock with a constant offset so identical process start times still
/// produce different sequences on repeat runs.
pub(super) fn rng_seed() -> u32 {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15);
    // xorshift requires a non-zero seed; the OR keeps us out of that hole.
    ((secs as u32) ^ 0xDEAD_BEEF) | 1
}

/// Advances an xorshift32 state by one step and returns the new value.
pub(super) fn xorshift32(state: u32) -> u32 {
    let mut x = state;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    x
}
