//! The [`WriteGuard`] RAII helper that centralises the per-command write
//! lock, AOF logging, memory-limit enforcement, and the duplicated
//! lazy-expiry dance shared by read and write paths.
//!
//! See `docs/plans/2026-07-20-writeguard-refactor-design.md` (F-07).

use std::collections::HashMap;
use std::sync::RwLockWriteGuard;
use std::time::{Duration, Instant};

use crate::error::FerrumError;
use crate::storage::engine::entry::{ValueEntry, live_payload};
use crate::storage::engine::util::{
    current_epoch_ms, deadline_to_epoch_ms, entry_bytes, log_aof_result, validate_key,
    validate_value,
};
use crate::storage::engine::{KvEngine, TtlStatus};

/// Holds the store write lock for the duration of a single command and
/// exposes typed operations that encapsulate the boilerplate every mutating
/// and lazy-expiring command used to repeat by hand.
///
/// The lock is taken once in [`WriteGuard::begin`] and released when the
/// guard is dropped, exactly as the old per-method `store.write()?` did — so
/// the AOF ordering invariant (the in-memory state and on-disk log agree on
/// write order because the lock is held while appending) is preserved.
pub(crate) struct WriteGuard<'a> {
    engine: &'a KvEngine,
    store: RwLockWriteGuard<'a, HashMap<Vec<u8>, ValueEntry>>,
    now: Instant,
    now_ms: i64,
}

impl<'a> WriteGuard<'a> {
    /// Acquires the store write lock and snapshots the current time once.
    pub(crate) fn begin(engine: &'a KvEngine) -> Result<Self, FerrumError> {
        let store = engine.store.write()?;
        Ok(Self {
            engine,
            store,
            now: Instant::now(),
            now_ms: current_epoch_ms(),
        })
    }

    /// Logs an AOF record only when an AOF writer is attached. The `res`
    /// future is computed lazily by the caller (it is `None` when there is no
    /// writer, so `append_*` is never evaluated needlessly).
    fn log(&self, cmd: &str, res: Option<Result<(), FerrumError>>) {
        if let Some(r) = res {
            log_aof_result(cmd, r);
        }
    }

    /// Returns the cloned value for `key` after applying lazy expiry.
    ///
    /// Mirrors the old `get` body: an expired entry is removed and logged as
    /// a `DEL` (lazy expiration), a live entry is touched (LRU/LFU + SIEVE/AC
    /// notifications) and counted as a hit, and a missing key is a miss.
    pub(crate) fn fetch_live(&mut self, key: &[u8]) -> Option<Vec<u8>> {
        let now = self.now;
        match self.store.get(key) {
            Some(entry) if entry.is_expired(now) => {
                self.engine.track_remove(&mut self.store, key);
                self.engine.log_expire_drop(key);
                self.engine.record_miss();
                None
            }
            Some(entry) => {
                let data = entry.data.clone();
                self.engine.touch_access(&mut self.store, key);
                self.engine.record_hit();
                Some(data)
            }
            None => {
                self.engine.record_miss();
                None
            }
        }
    }

    /// Like [`Self::fetch_live`] but reports presence instead of value.
    pub(crate) fn contains_live(&mut self, key: &[u8]) -> bool {
        let now = self.now;
        match self.store.get(key) {
            Some(entry) if entry.is_expired(now) => {
                self.engine.track_remove(&mut self.store, key);
                self.engine.log_expire_drop(key);
                self.engine.record_miss();
                false
            }
            Some(_) => {
                self.engine.record_hit();
                true
            }
            None => {
                self.engine.record_miss();
                false
            }
        }
    }

    /// Inserts `value` at `key`, clearing any prior TTL (Redis `SET` semantics).
    pub(crate) fn insert(
        &mut self,
        key: Vec<u8>,
        value: Vec<u8>,
    ) -> Result<Option<Vec<u8>>, FerrumError> {
        validate_key(&key)?;
        validate_value(&value)?;
        self.engine
            .enforce_for_write(&mut self.store, &key, value.len())?;
        self.log(
            "SET",
            self.engine.aof.as_ref().map(|a| a.append_set(&key, &value)),
        );
        let prev = self
            .engine
            .track_insert(&mut self.store, key, ValueEntry::new(value));
        Ok(prev.and_then(live_payload))
    }

    /// `SETNX`: inserts only when `key` is absent or already expired.
    pub(crate) fn insert_nx(&mut self, key: Vec<u8>, value: Vec<u8>) -> Result<bool, FerrumError> {
        validate_key(&key)?;
        validate_value(&value)?;
        let now = self.now;
        if let Some(entry) = self.store.get(key.as_slice()) {
            if !entry.is_expired(now) {
                return Ok(false);
            }
            self.engine.track_remove(&mut self.store, key.as_slice());
        }
        self.engine
            .enforce_for_write(&mut self.store, &key, value.len())?;
        self.log(
            "SETNX",
            self.engine.aof.as_ref().map(|a| a.append_set(&key, &value)),
        );
        self.engine
            .track_insert(&mut self.store, key, ValueEntry::new(value));
        Ok(true)
    }

    /// `MSET`: validates every pair before mutating, then applies atomically.
    pub(crate) fn insert_many(
        &mut self,
        pairs: Vec<(Vec<u8>, Vec<u8>)>,
    ) -> Result<(), FerrumError> {
        for (k, v) in &pairs {
            validate_key(k)?;
            validate_value(v)?;
        }
        for (k, v) in &pairs {
            self.engine.enforce_for_write(&mut self.store, k, v.len())?;
        }
        self.log(
            "MSET",
            self.engine.aof.as_ref().map(|a| a.append_set_many(&pairs)),
        );
        for (k, v) in pairs {
            self.engine
                .track_insert(&mut self.store, k, ValueEntry::new(v));
        }
        Ok(())
    }

    /// Inserts `value`, preserving `keep_ttl` and re-emitting `PEXPIREAT` to
    /// the AOF when the preserved deadline is still in the future. Callers
    /// validate as needed (`INCRBY` skips validation; `APPEND` validates its
    /// built value first).
    pub(crate) fn insert_keep_ttl(
        &mut self,
        key: Vec<u8>,
        value: Vec<u8>,
        keep_ttl: Option<Instant>,
    ) -> Result<Option<Vec<u8>>, FerrumError> {
        self.engine
            .enforce_for_write(&mut self.store, &key, value.len())?;
        self.log(
            "SET",
            self.engine.aof.as_ref().map(|a| a.append_set(&key, &value)),
        );
        if let Some(deadline) = keep_ttl
            && let Some(abs_ms) = deadline_to_epoch_ms(deadline, self.now)
        {
            self.log(
                "PEXPIREAT",
                self.engine
                    .aof
                    .as_ref()
                    .map(|a| a.append_pexpireat(&key, abs_ms)),
            );
        }
        let prev =
            self.engine
                .track_insert(&mut self.store, key, ValueEntry::new_with(value, keep_ttl));
        Ok(prev.and_then(live_payload))
    }

    /// Reads the live value and its TTL for an in-place update (used by
    /// `INCRBY`/`APPEND`). Applies lazy expiry (removing + logging the
    /// expired entry) but does **not** record a hit/miss, matching the
    /// original commands which treat the pre-read as a silent overwrite.
    pub(crate) fn read_state(&mut self, key: &[u8]) -> Option<(Vec<u8>, Option<Instant>)> {
        let now = self.now;
        match self.store.get(key) {
            Some(entry) if entry.is_expired(now) => {
                self.engine.track_remove(&mut self.store, key);
                self.engine.log_expire_drop(key);
                None
            }
            Some(entry) => Some((entry.data.clone(), entry.expire_at)),
            None => None,
        }
    }

    /// `DEL`: removes `key`, logging a `DEL` only when it was live.
    pub(crate) fn remove(&mut self, key: &[u8]) -> bool {
        let now = self.now;
        let existed = match self.engine.track_remove(&mut self.store, key) {
            Some(entry) => !entry.is_expired(now),
            None => false,
        };
        if existed {
            self.log("DEL", self.engine.aof.as_ref().map(|a| a.append_del(key)));
        }
        existed
    }

    /// `DEL` for many keys: logs `DEL` only for actually-removed live keys.
    pub(crate) fn remove_many(&mut self, keys: &[Vec<u8>]) -> usize {
        let now = self.now;
        let mut removed: Vec<&[u8]> = Vec::with_capacity(keys.len());
        for key in keys {
            if let Some(entry) = self.engine.track_remove(&mut self.store, key.as_slice())
                && !entry.is_expired(now)
            {
                removed.push(key.as_slice());
            }
        }
        for key in &removed {
            self.log("DEL", self.engine.aof.as_ref().map(|a| a.append_del(key)));
        }
        removed.len()
    }

    /// `EXPIRE`/`PEXPIREAT`: installs an absolute deadline, removing the key
    /// immediately when the deadline is in the past.
    pub(crate) fn expire_at(&mut self, key: &[u8], abs_epoch_ms: i64) -> Result<bool, FerrumError> {
        let now_instant = self.now;
        let now_ms = self.now_ms;
        if let Some(entry) = self.store.get(key)
            && entry.is_expired(now_instant)
        {
            self.engine.track_remove(&mut self.store, key);
            self.engine.log_expire_drop(key);
        }
        if !self.store.contains_key(key) {
            return Ok(false);
        }
        if abs_epoch_ms <= now_ms {
            self.engine.track_remove(&mut self.store, key);
            self.log("DEL", self.engine.aof.as_ref().map(|a| a.append_del(key)));
            return Ok(true);
        }
        let delta_ms = (abs_epoch_ms - now_ms) as u64;
        let deadline = now_instant + Duration::from_millis(delta_ms);
        if let Some(entry) = self.store.get_mut(key) {
            entry.expire_at = Some(deadline);
        }
        self.log(
            "PEXPIREAT",
            self.engine
                .aof
                .as_ref()
                .map(|a| a.append_pexpireat(key, abs_epoch_ms)),
        );
        Ok(true)
    }

    /// `PERSIST`: clears a live key's TTL.
    pub(crate) fn persist(&mut self, key: &[u8]) -> Result<bool, FerrumError> {
        let now = self.now;
        if let Some(entry) = self.store.get(key)
            && entry.is_expired(now)
        {
            self.engine.track_remove(&mut self.store, key);
            self.engine.log_expire_drop(key);
            return Ok(false);
        }
        let Some(entry) = self.store.get_mut(key) else {
            return Ok(false);
        };
        if entry.expire_at.is_none() {
            return Ok(false);
        }
        entry.expire_at = None;
        self.log(
            "PERSIST",
            self.engine.aof.as_ref().map(|a| a.append_persist(key)),
        );
        Ok(true)
    }

    /// `FLUSHDB`: clears the keyspace.
    pub(crate) fn clear(&mut self) -> Result<(), FerrumError> {
        self.engine.track_clear(&mut self.store);
        self.log(
            "FLUSHDB",
            self.engine.aof.as_ref().map(|a| a.append_flushdb()),
        );
        Ok(())
    }

    /// `TTL`: reports remaining TTL, applying lazy expiry first.
    pub(crate) fn ttl_of(&mut self, key: &[u8]) -> TtlStatus {
        let now = self.now;
        match self.store.get(key) {
            None => TtlStatus::Missing,
            Some(entry) if entry.is_expired(now) => {
                self.engine.track_remove(&mut self.store, key);
                self.engine.log_expire_drop(key);
                TtlStatus::Missing
            }
            Some(entry) => match entry.expire_at {
                None => TtlStatus::NoExpire,
                Some(deadline) => {
                    let remaining = deadline.saturating_duration_since(now);
                    TtlStatus::Millis(remaining.as_millis() as i64)
                }
            },
        }
    }

    /// `MEMORY USAGE`: reports a single entry's contribution, applying lazy
    /// expiry first.
    pub(crate) fn memory_of(&mut self, key: &[u8]) -> Result<Option<u64>, FerrumError> {
        let now = self.now;
        match self.store.get(key) {
            Some(entry) if entry.is_expired(now) => {
                if let Some(entry) = self.store.remove(key) {
                    self.engine.untrack(key, &entry);
                }
                self.engine.log_expire_drop(key);
                Ok(None)
            }
            Some(entry) => Ok(Some(entry_bytes(key, entry))),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use crate::storage::engine::util::current_epoch_ms;
    use crate::storage::engine::{KvEngine, TtlStatus};
    use crate::storage::eviction::{EvictionConfig, EvictionPolicy};

    use super::WriteGuard;

    #[test]
    fn fetch_live_expires_stale_key() {
        let engine = KvEngine::new();
        engine.set(b"k".to_vec(), b"v".to_vec()).unwrap();
        engine
            .expire_at_ms(&b"k"[..], current_epoch_ms() + 1)
            .unwrap();
        std::thread::sleep(Duration::from_millis(5));

        let mut g = WriteGuard::begin(&engine).unwrap();
        assert_eq!(g.fetch_live(&b"k"[..]), None);
        drop(g);
        // The expired entry must be physically gone, not just masked.
        assert_eq!(engine.get(&b"k"[..]).unwrap(), None);
    }

    #[test]
    fn insert_clears_prior_ttl() {
        let engine = KvEngine::new();
        engine.set(b"k".to_vec(), b"v".to_vec()).unwrap();
        engine
            .expire_at_ms(&b"k"[..], current_epoch_ms() + 1000)
            .unwrap();
        assert!(matches!(
            engine.ttl_ms(&b"k"[..]).unwrap(),
            TtlStatus::Millis(_)
        ));

        let mut g = WriteGuard::begin(&engine).unwrap();
        g.insert(b"k".to_vec(), b"v2".to_vec()).unwrap();
        drop(g);
        // A plain SET clears any prior TTL (Redis semantics).
        assert!(matches!(
            engine.ttl_ms(&b"k"[..]).unwrap(),
            TtlStatus::NoExpire
        ));
    }

    #[test]
    fn insert_keep_ttl_preserves_ttl() {
        let engine = KvEngine::new();
        engine.set(b"k".to_vec(), b"v".to_vec()).unwrap();
        engine
            .expire_at_ms(&b"k"[..], current_epoch_ms() + 1000)
            .unwrap();

        let mut g = WriteGuard::begin(&engine).unwrap();
        g.insert_keep_ttl(
            b"k".to_vec(),
            b"v2".to_vec(),
            Some(Instant::now() + Duration::from_millis(1000)),
        )
        .unwrap();
        drop(g);
        // INCRBY/APPEND-style writes preserve the TTL.
        assert!(matches!(
            engine.ttl_ms(&b"k"[..]).unwrap(),
            TtlStatus::Millis(_)
        ));
    }

    #[test]
    fn remove_reports_liveness() {
        let engine = KvEngine::new();
        engine.set(b"a".to_vec(), b"1".to_vec()).unwrap();

        let mut g = WriteGuard::begin(&engine).unwrap();
        assert!(g.remove(&b"a"[..]));
        assert!(!g.remove(&b"a"[..]));
        assert!(!g.remove(&b"missing"[..]));
        drop(g);
        assert_eq!(engine.get(&b"a"[..]).unwrap(), None);
    }

    #[test]
    fn insert_enforces_max_memory() {
        let engine = KvEngine::new();
        let cfg = EvictionConfig {
            policy: EvictionPolicy::AllKeysLru,
            max_memory: 1024,
            ..Default::default()
        };
        engine.set_eviction_config(cfg).unwrap();

        for i in 0..200u32 {
            engine
                .set(format!("key{i:04}").into_bytes(), vec![b'x'; 32])
                .unwrap();
        }
        // Eviction must keep the live keyspace bounded well under the
        // 200 * ~88-byte naive footprint.
        assert!(engine.dbsize().unwrap() <= 20);
    }
}
