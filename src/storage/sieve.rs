//! Stateful SIEVE eviction structure (Zhang et al., NSDI'24).
//!
//! Unlike the approximate policies ([`crate::storage::eviction`]) that pick a
//! victim from a small *random sample* of the keyspace, SIEVE keeps a single
//! FIFO queue of live keys plus one *hand* that sweeps it. Every key carries
//! a `visited` bit:
//!
//! - on insert, the key joins the tail of the queue with `visited = false`;
//! - on access, its `visited` bit is set to `true`;
//! - on eviction, the hand advances, clearing `visited` bits as it goes,
//!   until it finds a key whose bit is still `false` — that key is the victim.
//!
//! The insight (counter-intuitive, and the reason SIEVE beats LRU on many
//! real traces) is that a single missed access is enough to evict an object:
//! SIEVE gives every object exactly one "second chance", whereas LRU promotes
//! an object on every touch and only evicts the globally oldest. The whole
//! thing is O(1) amortised and needs no per-eviction sampling RNG.
//!
//! ## SIEVE-S (FerrumKV original)
//!
//! When `ttl_aware` is enabled, keys that are within
//! [`SIEVE_S_TTL_THRESHOLD`] of expiry are *force-demoted*: they are treated
//! as if `visited = false` regardless of recent accesses, so they become
//! immediate eviction candidates. The NSDI'24 paper does not discuss TTL
//! integration; this twist lets SIEVE prefer keys that are about to expire
//! on their own, sparing hotter long-lived keys. Because the engine only
//! stores a monotonic deadline (not the original TTL), "near expiry" is
//! approximated against a fixed horizon ([`SIEVE_S_HORIZON_MS`]) rather than
//! against each key's original lifetime — see [`is_near_expiry`].

use std::collections::HashMap;
use std::time::Instant;

use indexmap::IndexMap;

use crate::storage::engine::entry::ValueEntry;
use crate::storage::eviction::EvictionScope;

/// Default fraction of the TTL horizon under which SIEVE-S force-demotes a key.
pub const SIEVE_S_TTL_THRESHOLD: f64 = 0.1;

/// Horizon (ms) used as the "original TTL" proxy for the SIEVE-S
/// force-demote test. A key with under `SIEVE_S_TTL_THRESHOLD *
/// SIEVE_S_HORIZON_MS` of remaining life is treated as near-expiry.
const SIEVE_S_HORIZON_MS: u64 = 60_000;

/// Stateful SIEVE eviction structure.
///
/// The `bool` payload of each queue entry is the `visited` bit; `true` once
/// the key has been accessed since it joined the queue, `false` when it was
/// last inserted or reset.
pub(crate) struct SieveState {
    /// FIFO queue of keys in insertion order (preserved by `IndexMap`).
    /// `IndexMap` gives us O(1) insertion-order iteration *and* O(1)
    /// removal by key, which the hand sweep needs to drop tombstones.
    queue: IndexMap<Vec<u8>, bool>,
    /// Index of the hand into `queue`. Wraps modulo `queue.len()`.
    hand: usize,
    /// For SIEVE-S: keys within this fraction of the TTL horizon are
    /// force-demoted (treated as not-visited).
    pub ttl_demote_threshold: f64,
}

impl Default for SieveState {
    fn default() -> Self {
        Self::new()
    }
}

impl SieveState {
    pub(crate) fn new() -> Self {
        Self {
            queue: IndexMap::new(),
            hand: 0,
            ttl_demote_threshold: SIEVE_S_TTL_THRESHOLD,
        }
    }

    /// Records a (re)insertion of `key`: it joins the tail of the queue with
    /// its `visited` bit cleared, exactly as SIEVE's `insert` does. For a key
    /// already in the queue the bit is reset in place but its position is
    /// kept, mirroring a fresh write of an existing object.
    pub(crate) fn observe_insert(&mut self, key: &[u8]) {
        self.queue.insert(key.to_vec(), false);
    }

    /// Records an access to `key`, setting its `visited` bit.
    pub(crate) fn observe_access(&mut self, key: &[u8]) {
        if let Some(visited) = self.queue.get_mut(key) {
            *visited = true;
        }
    }

    /// Removes `key` from the queue (DEL / EXPIRE / FLUSHDB / eviction). The
    /// hand is shifted down by one when the removed key sat before it.
    pub(crate) fn observe_remove(&mut self, key: &[u8]) {
        if let Some(idx) = self.queue.get_index_of(key) {
            if idx < self.hand {
                self.hand = self.hand.saturating_sub(1);
            }
            self.queue.shift_remove(key);
            if self.hand >= self.queue.len() {
                self.hand = 0;
            }
        }
    }

    /// Rebuilds the queue from the current keyspace. Used when the policy is
    /// switched to a SIEVE variant at runtime: `allkeys` enqueues every key,
    /// `volatile` enqueues only keys that have a TTL. All bits start cleared,
    /// matching a from-scratch start.
    pub(crate) fn rebuild(&mut self, store: &HashMap<Vec<u8>, ValueEntry>, scope: EvictionScope) {
        self.queue.clear();
        self.hand = 0;
        for (key, entry) in store.iter() {
            let eligible = match scope {
                EvictionScope::AllKeys => true,
                EvictionScope::Volatile => entry.expire_at.is_some(),
            };
            if eligible {
                self.queue.insert(key.clone(), false);
            }
        }
    }

    /// Empties the queue. Called when the policy is switched away from a SIEVE
    /// variant so stale entries don't linger.
    pub(crate) fn clear(&mut self) {
        self.queue.clear();
        self.hand = 0;
    }

    /// Number of keys currently tracked by the queue. Exposed for tests and
    /// future observability.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.queue.len()
    }

    /// Runs one SIEVE sweep and returns the key to evict, or `None` when no
    /// eligible victim exists (empty queue, or every key out of scope).
    ///
    /// `store` is consulted only to test eligibility (a key may have been
    /// removed from the store but not yet from the queue, or — under
    /// `volatile` scope — may lack a TTL); it is never mutated.
    pub(crate) fn evict_one(
        &mut self,
        store: &HashMap<Vec<u8>, ValueEntry>,
        scope: EvictionScope,
        ttl_aware: bool,
        now: Instant,
    ) -> Option<Vec<u8>> {
        let n = self.queue.len();
        if n == 0 {
            return None;
        }
        // Bounded by `n + 1`: in the worst case every key is `visited`, so
        // the hand clears `n` bits and then evicts the first one it reaches
        // on the wrap-around (the `n + 1`-th look).
        let mut scanned = 0usize;
        while scanned < n + 1 {
            if self.hand >= self.queue.len() {
                self.hand = 0;
            }
            let key = self.queue.get_index(self.hand)?.0.clone();
            // Drop tombstones (gone from the store) or keys outside the
            // policy scope. The element shifts down into `hand`, so the hand
            // already points at the next key.
            let entry = store.get(&key);
            let eligible = match entry {
                None => false,
                Some(e) => match scope {
                    EvictionScope::AllKeys => true,
                    EvictionScope::Volatile => e.expire_at.is_some(),
                },
            };
            if !eligible {
                self.queue.shift_remove(&key);
                scanned += 1;
                continue;
            }
            let visited = *self.queue.get_index(self.hand).unwrap().1;
            let force_demote =
                ttl_aware && is_near_expiry(entry.unwrap(), now, self.ttl_demote_threshold);
            if !visited || force_demote {
                // Evict this victim. The hand now points at the next element.
                self.queue.shift_remove(&key);
                if self.hand >= self.queue.len() {
                    self.hand = 0;
                }
                return Some(key);
            }
            // Seen recently: clear the bit and advance the hand.
            *self.queue.get_index_mut(self.hand).unwrap().1 = false;
            self.hand += 1;
            scanned += 1;
        }
        None
    }
}

/// Returns `true` when `entry`'s remaining TTL is below
/// `threshold * SIEVE_S_HORIZON_MS`. Keys without a TTL, or whose deadline is
/// already in the past, are always treated as near-expiry (eagerly evicted).
fn is_near_expiry(entry: &ValueEntry, now: Instant, threshold: f64) -> bool {
    let deadline = match entry.expire_at {
        Some(d) => d,
        None => return false,
    };
    if deadline <= now {
        return true;
    }
    let remaining = deadline.duration_since(now).as_secs_f64();
    let horizon = SIEVE_S_HORIZON_MS as f64 / 1000.0;
    remaining <= threshold * horizon
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn entry_with_ttl(remaining_ms: u64) -> ValueEntry {
        let now = Instant::now();
        ValueEntry::new_with(
            b"x".to_vec(),
            Some(now + Duration::from_millis(remaining_ms)),
        )
    }

    fn entry_no_ttl() -> ValueEntry {
        ValueEntry::new(b"x".to_vec())
    }

    fn store_with(entries: &[(&str, ValueEntry)]) -> HashMap<Vec<u8>, ValueEntry> {
        let mut m = HashMap::new();
        for (k, e) in entries {
            m.insert(k.as_bytes().to_vec(), e.clone());
        }
        m
    }

    #[test]
    fn quick_demotion_evicts_unvisited_first() {
        // a visited, b not. SIEVE should evict b (the unvisited one).
        let mut s = SieveState::new();
        s.observe_insert(b"a");
        s.observe_insert(b"b");
        s.observe_access(b"a"); // a.visited = true
        let store = store_with(&[("a", entry_no_ttl()), ("b", entry_no_ttl())]);
        let victim = s.evict_one(&store, EvictionScope::AllKeys, false, Instant::now());
        assert_eq!(victim.as_deref(), Some(&b"b"[..]));
    }

    #[test]
    fn all_visited_evicts_hand_position_after_clearing() {
        // Both visited: hand clears a, then b, then wraps to a (now false).
        let mut s = SieveState::new();
        s.observe_insert(b"a");
        s.observe_insert(b"b");
        s.observe_access(b"a");
        s.observe_access(b"b");
        let store = store_with(&[("a", entry_no_ttl()), ("b", entry_no_ttl())]);
        let victim = s.evict_one(&store, EvictionScope::AllKeys, false, Instant::now());
        assert_eq!(victim.as_deref(), Some(&b"a"[..]));
    }

    #[test]
    fn single_visited_key_is_evicted() {
        // n = 1, entirely visited: must still yield a victim.
        let mut s = SieveState::new();
        s.observe_insert(b"only");
        s.observe_access(b"only");
        let store = store_with(&[("only", entry_no_ttl())]);
        let victim = s.evict_one(&store, EvictionScope::AllKeys, false, Instant::now());
        assert_eq!(victim.as_deref(), Some(&b"only"[..]));
    }

    #[test]
    fn empty_queue_yields_no_victim() {
        let mut s = SieveState::new();
        let store = store_with(&[]);
        assert!(
            s.evict_one(&store, EvictionScope::AllKeys, false, Instant::now())
                .is_none()
        );
    }

    #[test]
    fn volatile_scope_skips_keys_without_ttl() {
        // Only `a` has a TTL; under volatile scope it is the sole candidate.
        let mut s = SieveState::new();
        s.observe_insert(b"a");
        s.observe_insert(b"b"); // no TTL
        s.observe_access(b"a");
        let store = store_with(&[("a", entry_with_ttl(60_000)), ("b", entry_no_ttl())]);
        let victim = s.evict_one(&store, EvictionScope::Volatile, false, Instant::now());
        assert_eq!(victim.as_deref(), Some(&b"a"[..]));
    }

    #[test]
    fn volatile_scope_with_no_ttl_keys_yields_none() {
        let mut s = SieveState::new();
        s.observe_insert(b"a");
        s.observe_insert(b"b");
        let store = store_with(&[("a", entry_no_ttl()), ("b", entry_no_ttl())]);
        assert!(
            s.evict_one(&store, EvictionScope::Volatile, false, Instant::now())
                .is_none()
        );
    }

    #[test]
    fn sieve_s_force_demotes_near_expiry_even_when_visited() {
        // Both visited. `far` is first in the queue, `near` is short-lived.
        // Plain SIEVE would evict `far` (FIFO order); SIEVE-S should evict
        // the near-expiry `near` instead.
        let mut plain = SieveState::new();
        plain.observe_insert(b"far");
        plain.observe_insert(b"near");
        plain.observe_access(b"far");
        plain.observe_access(b"near");
        let store = store_with(&[
            ("far", entry_with_ttl(600_000)), // 10 min remaining
            ("near", entry_with_ttl(2_000)),  // 2 s remaining
        ]);

        let plain_victim = plain.evict_one(&store, EvictionScope::AllKeys, false, Instant::now());
        assert_eq!(plain_victim.as_deref(), Some(&b"far"[..]));

        let mut aware = SieveState::new();
        aware.observe_insert(b"far");
        aware.observe_insert(b"near");
        aware.observe_access(b"far");
        aware.observe_access(b"near");
        let aware_victim = aware.evict_one(&store, EvictionScope::AllKeys, true, Instant::now());
        assert_eq!(aware_victim.as_deref(), Some(&b"near"[..]));
    }

    #[test]
    fn near_expiry_is_below_horizon_threshold() {
        // 2s remaining vs 60s horizon * 0.1 = 6s → near expiry.
        assert!(is_near_expiry(
            &entry_with_ttl(2_000),
            Instant::now(),
            SIEVE_S_TTL_THRESHOLD
        ));
        // 30s remaining is above the 6s window → not near expiry.
        assert!(!is_near_expiry(
            &entry_with_ttl(30_000),
            Instant::now(),
            SIEVE_S_TTL_THRESHOLD
        ));
    }

    #[test]
    fn observe_remove_keeps_queue_consistent() {
        let mut s = SieveState::new();
        s.observe_insert(b"a");
        s.observe_insert(b"b");
        s.observe_insert(b"c");
        s.observe_remove(b"b");
        assert_eq!(s.len(), 2);
        let store = store_with(&[("a", entry_no_ttl()), ("c", entry_no_ttl())]);
        // Remaining queue order [a, c]; evict should pick a (first unvisited).
        let victim = s.evict_one(&store, EvictionScope::AllKeys, false, Instant::now());
        assert_eq!(victim.as_deref(), Some(&b"a"[..]));
    }

    #[test]
    fn rebuild_repopulates_from_store() {
        let mut s = SieveState::new();
        s.observe_insert(b"stale");
        let store = store_with(&[("a", entry_no_ttl()), ("b", entry_no_ttl())]);
        s.rebuild(&store, EvictionScope::AllKeys);
        assert_eq!(s.len(), 2);
        let victim = s.evict_one(&store, EvictionScope::AllKeys, false, Instant::now());
        assert!(victim.is_some());
    }
}
