//! Stateful AdaptiveClimb eviction structure (Berend et al., arXiv:2511.21235).
//!
//! AdaptiveClimb is a self-tuning variant of CLIMB. It models the cache as an
//! ordered list (position `1` = MRU … position `K` = LRU) and maintains a
//! single scalar, `jump`, that controls how far a hit promotes a key toward the
//! MRU end. Crucially it keeps **no per-item statistics**: the only mutable
//! state is the ordered list and the `jump` counter, which is what makes it
//! cheap enough to rival SIEVE while still adapting to the workload.
//!
//! ## The rules (Algorithm 1 of the paper)
//!
//! *Init*: `jump = K` (so it starts LRU-like).
//!
//! *On a hit* at position `i` (1-indexed):
//! - `jump = max(1, jump - 1)`
//! - promote the key to position `i - jump` (items in between shift down one).
//!
//! *On a miss* (insert key `j` at the tail, evicting the LRU end):
//! - `jump = min(K, jump + 1)`
//! - evict position `K`
//! - insert `j` at position `K - jump + 1`.
//!
//! ## Porting to a memory-capped store
//!
//! FerrumKV evicts only when `used_memory` would exceed `maxmemory`, not on
//! every miss, so the list cannot be held at a fixed size `K`. We treat the
//! ordered list as the *entire live keyspace* (MRU → LRU) and adapt the
//! paper's rules as follows:
//!
//! - `K` is the current list length (approximating the cache's capacity).
//! - On a **hit** we apply the promotion rule verbatim (decrement `jump`,
//!   move the key up by `jump`).
//! - On an **insert/miss** we apply the insertion rule: bump `jump`, then
//!   splice the new key in `jump` positions from the LRU end.
//! - On **eviction** the engine removes the LRU-end key (index `len - 1`),
//!   exactly the paper's "evict position `K`".
//!
//! Because `jump` is clamped to `[1, K]` and adapts from live hit/miss
//! ratios, the policy is self-tuning: more misses push `jump` up (new keys
//! enter nearer the LRU end, resisting cache pollution), while more hits push
//! it down (frequent keys climb toward the MRU end and stay protected). This
//! is the same single-parameter hill-climbing idea behind AHE's `alpha`, but
//! AdaptiveClimb needs no per-item counters to achieve it.

use std::collections::HashMap;

use indexmap::IndexMap;

use crate::storage::engine::entry::ValueEntry;
use crate::storage::eviction::EvictionScope;

/// Stateful AdaptiveClimb eviction structure.
///
/// `queue` holds the live keys in MRU → LRU order (index `0` is most-recently
/// used, index `len - 1` is the eviction victim). `jump` is the promotion
/// distance, clamped to `[1, queue.len()]`.
pub(crate) struct AdaptiveClimbState {
    queue: IndexMap<Vec<u8>, ()>,
    jump: usize,
}

impl Default for AdaptiveClimbState {
    fn default() -> Self {
        Self::new()
    }
}

impl AdaptiveClimbState {
    pub(crate) fn new() -> Self {
        Self {
            queue: IndexMap::new(),
            jump: 1,
        }
    }

    /// Keeps `jump` within `[1, len]` after the list shrinks or grows so the
    /// promotion math can never index past the tail.
    fn clamp_jump(&mut self) {
        let len = self.queue.len();
        if len == 0 {
            self.jump = 1;
        } else {
            self.jump = self.jump.clamp(1, len);
        }
    }

    /// Records a (re)insertion of `key`: splice it in `jump` positions from
    /// the LRU end (the paper's miss rule), after bumping `jump`. An existing
    /// key is removed first so an overwrite re-enters like a fresh miss.
    pub(crate) fn observe_insert(&mut self, key: &[u8]) {
        self.queue.shift_remove(key);
        self.clamp_jump();
        let len = self.queue.len();
        if len == 0 {
            self.jump = 1;
        } else if self.jump < len {
            self.jump += 1;
        }
        // Insertion position: 0-indexed translation of the paper's
        // `K - jump + 1` where `K = len` (the length *before* this splice)
        // => `len - jump`. `saturating_sub` guards against `jump > len`.
        let pos = len.saturating_sub(self.jump).min(len);
        self.queue.insert_before(pos, key.to_vec(), ());
    }

    /// Records a hit on `key`: promote it up by `jump` (decrementing `jump`
    /// first), exactly the paper's hit rule.
    pub(crate) fn observe_access(&mut self, key: &[u8]) {
        let Some(p) = self.queue.get_index_of(key) else {
            return;
        };
        if self.jump > 1 {
            self.jump -= 1;
        }
        if p > 0 {
            // New index = `p - jump` (clamped to 0). Because `jump >= 1`,
            // this is always `< p`, so the key genuinely moves toward the MRU
            // end and the items in between shift down by one.
            let target = p.saturating_sub(self.jump);
            if target < p {
                let key_vec = self.queue.shift_remove_index(p).unwrap().0;
                self.queue.insert_before(target, key_vec, ());
            }
        }
    }

    /// Removes `key` from the list (DEL / EXPIRE / FLUSHDB / eviction). The
    /// hand is left as-is: indices beyond the removed slot simply shift down.
    pub(crate) fn observe_remove(&mut self, key: &[u8]) {
        self.queue.shift_remove(key);
        self.clamp_jump();
    }

    /// Rebuilds the list from the current keyspace. Used when the policy is
    /// switched to an AdaptiveClimb variant at runtime: `allkeys` enqueues
    /// every key (MRU→LRU arbitrary order), `volatile` only those with a TTL.
    pub(crate) fn rebuild(&mut self, store: &HashMap<Vec<u8>, ValueEntry>, scope: EvictionScope) {
        self.queue.clear();
        self.jump = 1;
        for (key, entry) in store.iter() {
            let eligible = match scope {
                EvictionScope::AllKeys => true,
                EvictionScope::Volatile => entry.expire_at.is_some(),
            };
            if eligible {
                self.queue.insert_before(self.queue.len(), key.clone(), ());
            }
        }
    }

    /// Empties the list. Called when the policy is switched away from an
    /// AdaptiveClimb variant so stale entries don't linger.
    pub(crate) fn clear(&mut self) {
        self.queue.clear();
        self.jump = 1;
    }

    /// Number of keys currently tracked by the list. Exposed for tests and
    /// future observability.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.queue.len()
    }

    /// Current `jump` value. Exposed for tests.
    #[cfg(test)]
    pub(crate) fn jump(&self) -> usize {
        self.jump
    }

    /// MRU→LRU position of `key`, or `None` if absent. Exposed for tests.
    #[cfg(test)]
    pub(crate) fn position(&self, key: &[u8]) -> Option<usize> {
        self.queue.get_index_of(key)
    }

    /// Runs one sweep and returns the LRU-end eligible key to evict, or `None`
    /// when the queue is empty or every key is out of scope / a tombstone.
    ///
    /// `store` is consulted only to test eligibility (a key may have been
    /// removed from the store but not yet from the list, or — under `volatile`
    /// scope — may lack a TTL); it is never mutated.
    pub(crate) fn evict_one(
        &mut self,
        store: &HashMap<Vec<u8>, ValueEntry>,
        scope: EvictionScope,
    ) -> Option<Vec<u8>> {
        let n = self.queue.len();
        if n == 0 {
            return None;
        }
        // Walk from the LRU end (index `n - 1`) toward the MRU end, evicting
        // the first eligible key. Out-of-scope / tombstoned keys are skipped;
        // we leave them in place rather than compacting on every sweep.
        for idx in (0..n).rev() {
            let key = self.queue.get_index(idx)?.0.clone();
            let eligible = match store.get(&key) {
                None => false,
                Some(e) => match scope {
                    EvictionScope::AllKeys => true,
                    EvictionScope::Volatile => e.expire_at.is_some(),
                },
            };
            if eligible {
                return self.queue.shift_remove_index(idx).map(|(k, _)| k);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

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
    fn fresh_insert_enters_at_mru() {
        // First key is the only key (MRU). Second key prepends, pushing the
        // first to the LRU end.
        let mut s = AdaptiveClimbState::new();
        s.observe_insert(b"a");
        s.observe_insert(b"b");
        assert_eq!(s.position(b"b"), Some(0));
        assert_eq!(s.position(b"a"), Some(1));
    }

    #[test]
    fn evict_picks_lru_end() {
        let mut s = AdaptiveClimbState::new();
        s.observe_insert(b"a");
        s.observe_insert(b"b"); // b is MRU, a is LRU
        let store = store_with(&[("a", entry_no_ttl()), ("b", entry_no_ttl())]);
        let victim = s.evict_one(&store, EvictionScope::AllKeys);
        assert_eq!(victim.as_deref(), Some(&b"a"[..]));
    }

    #[test]
    fn hit_promotes_key_toward_mru() {
        // a (LRU), b (MRU). A hit on `a` must move it up by `jump`.
        let mut s = AdaptiveClimbState::new();
        s.observe_insert(b"a");
        s.observe_insert(b"b");
        assert_eq!(s.position(b"a"), Some(1));
        s.observe_access(b"a");
        // After a hit `jump` was at least decremented and `a` moved up; it is
        // now closer to (or at) the MRU than its previous LRU position.
        assert!(s.position(b"a").unwrap() < 1);
    }

    #[test]
    fn repeated_hits_keep_hot_key_at_mru() {
        let mut s = AdaptiveClimbState::new();
        s.observe_insert(b"a");
        s.observe_insert(b"b");
        s.observe_insert(b"c"); // order: c MRU, b, a LRU
        for _ in 0..5 {
            s.observe_access(b"a"); // repeatedly hit the LRU key
        }
        // A stream of hits drives `jump` toward 1 (CLIMB-like), so `a`
        // climbs all the way to the MRU end.
        assert_eq!(s.position(b"a"), Some(0));
    }

    #[test]
    fn overwrite_reenters_like_a_miss() {
        // Insert a (MRU), then overwrite it. The key re-enters and the list
        // still contains exactly one entry.
        let mut s = AdaptiveClimbState::new();
        s.observe_insert(b"a");
        s.observe_access(b"a");
        s.observe_insert(b"a");
        assert_eq!(s.len(), 1);
        assert_eq!(s.position(b"a"), Some(0));
    }

    #[test]
    fn volatile_scope_skips_keys_without_ttl() {
        let mut s = AdaptiveClimbState::new();
        s.observe_insert(b"a"); // no TTL
        s.observe_insert(b"b"); // no TTL
        let store = store_with(&[("a", entry_no_ttl()), ("b", entry_no_ttl())]);
        // No key has a TTL under volatile scope → no eligible victim.
        assert!(s.evict_one(&store, EvictionScope::Volatile).is_none());
    }

    #[test]
    fn volatile_scope_evicts_ttl_key_at_lru() {
        let mut s = AdaptiveClimbState::new();
        s.observe_insert(b"a"); // no TTL
        s.observe_insert(b"b"); // TTL → LRU end, sole eligible victim
        let store = store_with(&[("a", entry_no_ttl()), ("b", entry_with_ttl(60_000))]);
        let victim = s.evict_one(&store, EvictionScope::Volatile);
        assert_eq!(victim.as_deref(), Some(&b"b"[..]));
    }

    #[test]
    fn tombstone_in_store_is_skipped() {
        let mut s = AdaptiveClimbState::new();
        s.observe_insert(b"a");
        s.observe_insert(b"b");
        // `a` is gone from the store but still in the list — `b` (LRU-end
        // eligible) must be chosen instead.
        let store = store_with(&[("b", entry_no_ttl())]);
        let victim = s.evict_one(&store, EvictionScope::AllKeys);
        assert_eq!(victim.as_deref(), Some(&b"b"[..]));
    }

    #[test]
    fn jump_starts_at_one_when_empty() {
        let mut s = AdaptiveClimbState::new();
        s.observe_insert(b"a");
        assert_eq!(s.jump(), 1);
    }

    #[test]
    fn rebuild_repopulates_from_store() {
        let mut s = AdaptiveClimbState::new();
        s.observe_insert(b"stale");
        let store = store_with(&[("a", entry_no_ttl()), ("b", entry_no_ttl())]);
        s.rebuild(&store, EvictionScope::AllKeys);
        assert_eq!(s.len(), 2);
        let victim = s.evict_one(&store, EvictionScope::AllKeys);
        assert!(victim.is_some());
    }
}
