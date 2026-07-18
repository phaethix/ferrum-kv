//! The core key-value engine.
//!
//! This module owns the [`KvEngine`] type, its command-level API, and the
//! memory-tracking / eviction machinery that hangs off it. Auxiliary pieces
//! live in sibling files:
//!
//! - [`entry`] — the per-key [`ValueEntry`] record and its LFU/timestamp
//!   maintenance.
//! - [`types`] — public ancillary types ([`SweepStats`], [`TtlStatus`]).
//! - [`util`] — free-standing helpers (validation, time, RNG, AOF logging,
//!   eviction sampling).
//! - [`tests`] — the engine's inline unit tests, kept here so the command
//!   API stays readable.
//!
//! The split keeps each file focused on a single concern without changing
//! the public surface: everything remains reachable at `storage::engine::*`
//! exactly as before. See FERRUM-005 for the rationale.

pub(crate) mod entry;
pub mod types;
pub(crate) mod util;

#[cfg(test)]
mod tests;

// Re-export the public ancillary types and the crate-visible time helper so
// existing paths (`storage::engine::SweepStats`, `::current_epoch_ms`, …)
// keep resolving for callers in `network`, `persistence`, and the
// integration tests.
pub use types::{SweepStats, TtlStatus};
pub(crate) use util::current_epoch_ms;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use crate::error::FerrumError;
use crate::persistence::AofWriter;
use crate::storage::adaptive_climb::AdaptiveClimbState;
use crate::storage::engine::entry::{ValueEntry, live_payload};
use crate::storage::engine::util::{
    deadline_to_epoch_ms, log_aof_result, rng_seed, sample_candidates, validate_key,
    validate_value, xorshift32,
};
use crate::storage::eviction::{
    self, AdaptiveHybridState, EvictionConfig, EvictionPolicy, EvictionScope,
};
use crate::storage::sieve::SieveState;

/// Maximum allowed key size in bytes (64 KiB).
pub const KEY_MAX_BYTES: usize = 64 * 1024;

/// Maximum allowed value size in bytes (16 MiB).
pub const VALUE_MAX_BYTES: usize = 16 * 1024 * 1024;

/// Approximate per-entry overhead charged on top of `key.len() + value.len()`.
///
/// Covers the `HashMap` bucket, the `ValueEntry` metadata, and the per-key
/// allocations that `Vec<u8>` carries. Chosen as a conservative constant so
/// the reported `used_memory` stays a useful safety bound without pretending
/// to match the allocator to the byte.
pub const PER_ENTRY_OVERHEAD: u64 = 48;

/// Upper bound on the number of keys [`KvEngine::scan_keys`] returns in one
/// call.
///
/// The dashboard key browser paginates over this window, so the limit only
/// bounds a single response's footprint — not the total addressable keyspace.
pub const SCAN_KEYS_LIMIT: usize = 10_000;

/// A thread-safe key-value storage engine backed by a [`HashMap`].
///
/// Keys and values are stored as `Vec<u8>`, making the engine fully
/// binary-safe: NUL bytes, bytes above 0x7F, and embedded CRLF are all
/// preserved verbatim. This matches the contract of the RESP2 bulk string,
/// which is already byte-oriented on the wire.
///
/// Each value carries an optional expiration deadline. Expired entries are
/// removed lazily on access (Redis-style) and proactively by a background
/// sweeper that calls [`KvEngine::sweep_expired`] periodically.
///
/// Mutating commands (`SET`, `DEL`, `FLUSHDB`, `EXPIRE`, `PERSIST`) are
/// optionally forwarded to an [`AofWriter`] so changes survive a restart.
/// The log is appended while the write lock is still held, which preserves
/// the ordering invariant described in the whitepaper (§8.7): the in-memory
/// state and the on-disk log always agree on the relative order of
/// successful writes.
///
/// Public methods return [`Result`] so lock poisoning can be reported instead
/// of causing a panic.
#[derive(Clone)]
pub struct KvEngine {
    /// The live keyspace. Fields are `pub(super)` so the engine's own test
    /// module can poke at internal state for white-box assertions; they stay
    /// hidden from the rest of the crate.
    pub(crate) store: Arc<RwLock<HashMap<Vec<u8>, ValueEntry>>>,
    pub(crate) aof: Option<Arc<AofWriter>>,
    /// Approximate memory footprint of the live dataset, in bytes.
    ///
    /// The counter is updated inside the store's write lock so it stays
    /// consistent with the `HashMap` it describes. Readers can load it
    /// without holding any engine lock, which keeps `INFO memory` cheap.
    pub(crate) used_memory: Arc<AtomicU64>,
    /// Live eviction configuration.
    ///
    /// Wrapped in a lock so the server can mutate it at runtime (e.g. via
    /// a future `CONFIG SET` command) without cloning the whole engine.
    pub(crate) eviction: Arc<RwLock<EvictionConfig>>,
    /// Cumulative number of keyspace hits (read commands that found a
    /// live key). Exposed via `INFO stats` and consumed by the AHE
    /// feedback loop.
    pub(crate) hits: Arc<AtomicU64>,
    /// Cumulative number of keyspace misses (read commands that found
    /// nothing or an expired key).
    pub(crate) misses: Arc<AtomicU64>,
    /// Adaptive Hybrid Eviction controller. Shared state so every caller
    /// sees the same `alpha`, regardless of which engine clone they hold.
    pub(crate) ahe: Arc<Mutex<AdaptiveHybridState>>,
    /// SIEVE eviction FIFO queue + hand (NSDI'24). Like [`KvEngine::ahe`], it
    /// is shared so every engine clone sees the same queue. It is maintained
    /// on every insert / access / remove so a runtime switch to a SIEVE
    /// policy (via [`KvEngine::set_eviction_config`]) is immediately
    /// consistent without replaying the whole keyspace.
    pub(crate) sieve: Arc<Mutex<SieveState>>,
    /// AdaptiveClimb eviction state (arXiv:2511.21235) — an ordered
    /// MRU→LRU list plus a single self-tuning `jump` scalar. Like
    /// [`KvEngine::sieve`] it is shared so every engine clone sees the same
    /// order; maintained on every insert / access / remove so a runtime
    /// switch to an AdaptiveClimb policy is immediately consistent.
    pub(crate) ac: Arc<Mutex<AdaptiveClimbState>>,
    /// Optional connection password (`requirepass`). When `Some`, every
    /// freshly accepted connection must issue a successful `AUTH` before
    /// any other command runs. Shared via `Arc` so a runtime
    /// `CONFIG SET requirepass` is visible to already-accepted
    /// connections; the per-connection "already authed" flag lives in
    /// the network layer, not here, because it is connection-local.
    pub(crate) auth_password: Arc<RwLock<Option<Vec<u8>>>>,
    /// Seed for the per-call PRNG used to drive probabilistic LFU
    /// increments and random-policy tie breaks. Xorshift32 is plenty for
    /// sampling purposes and keeps the engine free of `rand` as a
    /// runtime dependency.
    pub(crate) rng: Arc<AtomicU32>,
}

impl Default for KvEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl KvEngine {
    /// Creates a new empty key-value engine without persistence.
    pub fn new() -> Self {
        Self {
            store: Arc::default(),
            aof: None,
            used_memory: Arc::new(AtomicU64::new(0)),
            eviction: Arc::new(RwLock::new(EvictionConfig::default())),
            hits: Arc::new(AtomicU64::new(0)),
            misses: Arc::new(AtomicU64::new(0)),
            ahe: Arc::new(Mutex::new(AdaptiveHybridState::default())),
            sieve: Arc::new(Mutex::new(SieveState::new())),
            ac: Arc::new(Mutex::new(AdaptiveClimbState::new())),
            // No password by default: authentication is opt-in, matching
            // Redis' `requirepass` semantics (absent = open access).
            auth_password: Arc::new(RwLock::new(None)),
            // Seed the PRNG from the system clock so two engines started
            // in the same second don't share a sequence. The seed is
            // never exposed, so predictability is not a concern.
            rng: Arc::new(AtomicU32::new(rng_seed())),
        }
    }

    /// Attaches an AOF writer so subsequent mutating commands are persisted.
    ///
    /// The writer is shared via [`Arc`], allowing the same instance to be used
    /// across cloned engine handles.
    pub fn with_aof(mut self, writer: Arc<AofWriter>) -> Self {
        self.aof = Some(writer);
        self
    }

    /// Replaces the live eviction configuration.
    ///
    /// Takes effect for all subsequent writes; already-stored keys are
    /// eligible under the new policy immediately. When the new policy is a
    /// SIEVE variant the FIFO queue is rebuilt from the current keyspace so
    /// the algorithm has accurate state at once; when leaving SIEVE the queue
    /// is cleared so it does not retain stale entries.
    pub fn set_eviction_config(&self, cfg: EvictionConfig) -> Result<(), FerrumError> {
        // When leaving a stateful policy, clear its structure; when entering
        // one, (re)build it from the live keyspace so the algorithm has
        // accurate state at once. Only one stateful policy is active at a
        // time, so the other is always cleared.
        match cfg.policy {
            p if p.is_sieve() => {
                let store = self.store.read()?;
                let mut sieve = self.sieve.lock()?;
                sieve.rebuild(&store, cfg.policy.scope());
                let mut ac = self.ac.lock()?;
                ac.clear();
            }
            p if p.is_adaptive_climb() => {
                let store = self.store.read()?;
                let mut ac = self.ac.lock()?;
                ac.rebuild(&store, cfg.policy.scope());
                let mut sieve = self.sieve.lock()?;
                sieve.clear();
            }
            _ => {
                let mut sieve = self.sieve.lock()?;
                sieve.clear();
                let mut ac = self.ac.lock()?;
                ac.clear();
            }
        }
        let mut guard = self.eviction.write()?;
        *guard = cfg;
        Ok(())
    }

    /// Returns a copy of the current eviction configuration.
    pub fn eviction_config(&self) -> Result<EvictionConfig, FerrumError> {
        Ok(*self.eviction.read()?)
    }

    /// Replaces the `requirepass` password.
    ///
    /// `None` disables authentication (the default); `Some(bytes)` enables
    /// it for every connection that has not yet authenticated. Already
    /// authenticated connections stay authenticated even if the password is
    /// later changed or cleared. The change is runtime-only and is not
    /// persisted to the AOF, consistent with Redis.
    pub fn set_requirepass(&self, pw: Option<Vec<u8>>) -> Result<(), FerrumError> {
        let mut guard = self.auth_password.write()?;
        *guard = pw;
        Ok(())
    }

    /// Returns a copy of the current `requirepass` password, if any.
    pub fn requirepass(&self) -> Result<Option<Vec<u8>>, FerrumError> {
        Ok(self.auth_password.read()?.clone())
    }

    /// Verifies an `AUTH` attempt against the configured password.
    ///
    /// Returns `Ok(true)` when the password matches, `Ok(false)` when a
    /// password is configured but the attempt does not match, and `Err` when
    /// no password is configured at all (Redis replies
    /// `ERR Client sent AUTH, but no password is set`). The caller maps
    /// these onto `+OK` / `-WRONGPASS` / `-ERR` replies and tracks
    /// the per-connection authenticated flag.
    pub fn authenticate(&self, provided: &[u8]) -> Result<bool, FerrumError> {
        let guard = self.auth_password.read()?;
        match guard.as_deref() {
            Some(expected) => Ok(provided == expected),
            None => Err(FerrumError::ParseError(
                "Client sent AUTH, but no password is set".into(),
            )),
        }
    }

    /// Sets a key-value pair and returns the previous value, if any.
    ///
    /// Matches Redis' default semantics: any existing TTL on the key is
    /// cleared by the write. Callers that need the Redis `KEEPTTL` option
    /// will have to go through a dedicated future API.
    ///
    /// Returns [`FerrumError::KeyTooLong`] or [`FerrumError::ValueTooLarge`] if
    /// the configured size limits are exceeded.
    pub fn set(&self, key: Vec<u8>, value: Vec<u8>) -> Result<Option<Vec<u8>>, FerrumError> {
        validate_key(&key)?;
        validate_value(&value)?;

        let mut store = self.store.write()?;
        self.enforce_for_write(&mut store, &key, value.len())?;
        if let Some(aof) = &self.aof {
            log_aof_result("SET", aof.append_set(&key, &value));
        }
        let previous = self.track_insert(&mut store, key, ValueEntry::new(value));
        Ok(previous.and_then(live_payload))
    }

    /// Records a (re)insertion in auxiliary eviction structures.
    ///
    /// Kept separate so the public command methods stay terse; callers must
    /// already hold the write lock or own the engine reference. The SIEVE
    /// queue is updated even when the active policy is not SIEVE so a runtime
    /// switch to a SIEVE variant is immediately consistent.
    #[inline]
    fn sieve_notify_insert(&self, key: &[u8]) {
        if let Ok(mut s) = self.sieve.lock() {
            s.observe_insert(key);
        }
    }

    /// Mirrors a key access into the SIEVE `visited` bit.
    #[inline]
    fn sieve_notify_access(&self, key: &[u8]) {
        if let Ok(mut s) = self.sieve.lock() {
            s.observe_access(key);
        }
    }

    /// Mirrors a key removal from the SIEVE FIFO queue.
    #[inline]
    fn sieve_notify_remove(&self, key: &[u8]) {
        if let Ok(mut s) = self.sieve.lock() {
            s.observe_remove(key);
        }
    }

    /// Clears the SIEVE FIFO queue (used by `FLUSHDB`).
    #[inline]
    fn sieve_notify_clear(&self) {
        if let Ok(mut s) = self.sieve.lock() {
            s.clear();
        }
    }

    /// Mirrors a (re)insertion into the AdaptiveClimb ordered list.
    #[inline]
    fn ac_notify_insert(&self, key: &[u8]) {
        if let Ok(mut s) = self.ac.lock() {
            s.observe_insert(key);
        }
    }

    /// Mirrors a hit into the AdaptiveClimb promotion rule.
    #[inline]
    fn ac_notify_access(&self, key: &[u8]) {
        if let Ok(mut s) = self.ac.lock() {
            s.observe_access(key);
        }
    }

    /// Mirrors a removal from the AdaptiveClimb ordered list.
    #[inline]
    fn ac_notify_remove(&self, key: &[u8]) {
        if let Ok(mut s) = self.ac.lock() {
            s.observe_remove(key);
        }
    }

    /// Clears the AdaptiveClimb ordered list (used by `FLUSHDB`).
    #[inline]
    fn ac_notify_clear(&self) {
        if let Ok(mut s) = self.ac.lock() {
            s.clear();
        }
    }

    /// Sets `key` to `value` only if the key is not already present.
    ///
    /// Returns `true` when the insert happened and `false` when the key was
    /// already set. The AOF records a `SET` only on a successful insert,
    /// mirroring the Redis semantics of `SETNX`.
    pub fn set_nx(&self, key: Vec<u8>, value: Vec<u8>) -> Result<bool, FerrumError> {
        validate_key(&key)?;
        validate_value(&value)?;

        let mut store = self.store.write()?;
        let now = Instant::now();
        if let Some(entry) = store.get(key.as_slice()) {
            if !entry.is_expired(now) {
                return Ok(false);
            }
            // The old value has already expired: remove it and proceed as if
            // the key were absent. We intentionally do not log a DEL because
            // the subsequent SET, once replayed, overwrites the stale entry
            // anyway and skipping the DEL keeps the log shorter.
            self.track_remove(&mut store, key.as_slice());
        }
        self.enforce_for_write(&mut store, &key, value.len())?;
        if let Some(aof) = &self.aof {
            log_aof_result("SETNX", aof.append_set(&key, &value));
        }
        self.track_insert(&mut store, key, ValueEntry::new(value));
        Ok(true)
    }

    /// Sets every `(key, value)` pair in `pairs` atomically.
    ///
    /// Each pair is validated before any mutation happens, so either the
    /// whole batch is applied or none of it is. The AOF log records the
    /// batch in a single write so concurrent appenders never observe a
    /// half-committed MSET.
    pub fn mset(&self, pairs: Vec<(Vec<u8>, Vec<u8>)>) -> Result<(), FerrumError> {
        for (k, v) in &pairs {
            validate_key(k)?;
            validate_value(v)?;
        }

        let mut store = self.store.write()?;
        for (k, v) in &pairs {
            self.enforce_for_write(&mut store, k, v.len())?;
        }
        if let Some(aof) = &self.aof {
            log_aof_result("MSET", aof.append_set_many(&pairs));
        }
        for (k, v) in pairs {
            self.track_insert(&mut store, k, ValueEntry::new(v));
        }
        Ok(())
    }

    /// Returns the stored value for every key in `keys`, preserving order.
    ///
    /// Missing keys map to `None` so the caller can serialise them as
    /// null bulk strings without ambiguity. Expired entries are dropped in
    /// the same pass so the result reflects live state only.
    pub fn mget(&self, keys: &[Vec<u8>]) -> Result<Vec<Option<Vec<u8>>>, FerrumError> {
        let mut store = self.store.write()?;
        let now = Instant::now();
        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            match store.get(key.as_slice()) {
                Some(entry) if entry.is_expired(now) => {
                    self.track_remove(&mut store, key.as_slice());
                    self.log_expire_drop(key);
                    self.record_miss();
                    out.push(None);
                }
                Some(entry) => {
                    let data = entry.data.clone();
                    self.touch_access(&mut store, key.as_slice());
                    self.record_hit();
                    out.push(Some(data));
                }
                None => {
                    self.record_miss();
                    out.push(None);
                }
            }
        }
        Ok(out)
    }

    /// Atomically adds `delta` to the integer value at `key` and returns
    /// the new value.
    ///
    /// A missing key is treated as starting from zero, matching Redis'
    /// `INCR` semantics. The existing value, if any, must be a decimal
    /// ASCII integer that fits into an [`i64`]; values outside that range
    /// or that fail to parse produce the Redis-standard reply
    /// `-ERR value is not an integer or out of range`. Overflow of the
    /// addition itself is treated the same way.
    ///
    /// The key's existing TTL, if any, is preserved.
    pub fn incr_by(&self, key: Vec<u8>, delta: i64) -> Result<i64, FerrumError> {
        validate_key(&key)?;

        let mut store = self.store.write()?;
        let now = Instant::now();
        let (current, existing_deadline) = match store.get(key.as_slice()) {
            Some(entry) if entry.is_expired(now) => {
                self.track_remove(&mut store, key.as_slice());
                self.log_expire_drop(&key);
                (0i64, None)
            }
            Some(entry) => {
                let text = std::str::from_utf8(&entry.data).map_err(|_| {
                    FerrumError::ParseError("value is not an integer or out of range".into())
                })?;
                let n = text.parse::<i64>().map_err(|_| {
                    FerrumError::ParseError("value is not an integer or out of range".into())
                })?;
                (n, entry.expire_at)
            }
            None => (0i64, None),
        };

        let new_value = current.checked_add(delta).ok_or_else(|| {
            FerrumError::ParseError("value is not an integer or out of range".into())
        })?;
        let serialised = new_value.to_string().into_bytes();

        self.enforce_for_write(&mut store, &key, serialised.len())?;
        if let Some(aof) = &self.aof {
            log_aof_result("INCRBY", aof.append_set(&key, &serialised));
            // INCR preserves TTL: re-emit the existing deadline so replay
            // converges to the same state regardless of record ordering.
            if let Some(deadline) = existing_deadline
                && let Some(abs_ms) = deadline_to_epoch_ms(deadline, now)
            {
                log_aof_result("PEXPIREAT", aof.append_pexpireat(&key, abs_ms));
            }
        }
        self.track_insert(
            &mut store,
            key,
            ValueEntry::new_with(serialised, existing_deadline),
        );
        Ok(new_value)
    }

    /// Returns the value for `key`, or `None` if the key does not exist.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, FerrumError> {
        let mut store = self.store.write()?;
        let now = Instant::now();
        match store.get(key) {
            Some(entry) if entry.is_expired(now) => {
                self.track_remove(&mut store, key);
                self.log_expire_drop(key);
                self.record_miss();
                Ok(None)
            }
            Some(entry) => {
                let data = entry.data.clone();
                self.touch_access(&mut store, key);
                self.record_hit();
                Ok(Some(data))
            }
            None => {
                self.record_miss();
                Ok(None)
            }
        }
    }

    /// Deletes `key` and returns `true` if it existed.
    pub fn del(&self, key: &[u8]) -> Result<bool, FerrumError> {
        let mut store = self.store.write()?;
        let now = Instant::now();
        let existed = match self.track_remove(&mut store, key) {
            Some(entry) => !entry.is_expired(now),
            None => false,
        };
        if existed && let Some(aof) = &self.aof {
            log_aof_result("DEL", aof.append_del(key));
        }
        Ok(existed)
    }

    /// Deletes every key in `keys` and returns the count of keys that
    /// actually existed.
    ///
    /// The write lock is held for the entire batch so the operation is
    /// atomic from an observer's point of view: concurrent readers see
    /// either all deletions or none of them. Persisted log records are
    /// appended only for keys that were actually removed, mirroring
    /// Redis' behaviour. Already-expired keys do not count as removed.
    pub fn del_many(&self, keys: &[Vec<u8>]) -> Result<usize, FerrumError> {
        if keys.is_empty() {
            return Ok(0);
        }
        let mut store = self.store.write()?;
        let now = Instant::now();
        let mut removed: Vec<&[u8]> = Vec::with_capacity(keys.len());
        for key in keys {
            if let Some(entry) = self.track_remove(&mut store, key.as_slice())
                && !entry.is_expired(now)
            {
                removed.push(key.as_slice());
            }
        }
        if let Some(aof) = &self.aof {
            for key in &removed {
                log_aof_result("DEL", aof.append_del(key));
            }
        }
        Ok(removed.len())
    }

    /// Returns `true` if `key` exists and has not expired.
    pub fn exists(&self, key: &[u8]) -> Result<bool, FerrumError> {
        let mut store = self.store.write()?;
        let now = Instant::now();
        match store.get(key) {
            Some(entry) if entry.is_expired(now) => {
                self.track_remove(&mut store, key);
                self.log_expire_drop(key);
                self.record_miss();
                Ok(false)
            }
            Some(_) => {
                self.record_hit();
                Ok(true)
            }
            None => {
                self.record_miss();
                Ok(false)
            }
        }
    }

    /// Counts how many of the given keys currently exist (lazily expiring
    /// any that are past their TTL). Duplicate keys are counted per
    /// occurrence, matching Redis `EXISTS` semantics.
    ///
    /// Empty input returns `0` without touching the keyspace counters.
    pub fn exists_many(&self, keys: &[Vec<u8>]) -> Result<usize, FerrumError> {
        if keys.is_empty() {
            return Ok(0);
        }
        let mut store = self.store.write()?;
        let now = Instant::now();
        let mut count = 0usize;
        for key in keys {
            match store.get(key.as_slice()) {
                Some(entry) if entry.is_expired(now) => {
                    self.track_remove(&mut store, key.as_slice());
                    self.log_expire_drop(key.as_slice());
                    self.record_miss();
                }
                Some(_) => {
                    self.record_hit();
                    count += 1;
                }
                None => {
                    self.record_miss();
                }
            }
        }
        Ok(count)
    }

    /// Returns the number of keys currently stored.
    ///
    /// Already-expired keys still pending lazy cleanup are excluded so
    /// callers see a value consistent with `EXISTS`.
    pub fn dbsize(&self) -> Result<usize, FerrumError> {
        let store = self.store.read()?;
        let now = Instant::now();
        Ok(store.values().filter(|v| !v.is_expired(now)).count())
    }

    /// Returns up to [`SCAN_KEYS_LIMIT`] live keys whose name matches the
    /// Redis-style glob `pattern` (e.g. `*` for everything, `user:*` for a
    /// prefix, `session?` for a single wildcard). Expired keys are skipped.
    ///
    /// The dashboard key browser calls this for search and pagination; the
    /// cap keeps a single response bounded even when the keyspace is huge.
    pub fn scan_keys(&self, pattern: &[u8]) -> Result<Vec<Vec<u8>>, FerrumError> {
        let store = self.store.read()?;
        let now = Instant::now();
        let mut out = Vec::new();
        for (key, entry) in store.iter() {
            if entry.is_expired(now) {
                continue;
            }
            if Self::glob_match(pattern, key) {
                out.push(key.clone());
                if out.len() >= SCAN_KEYS_LIMIT {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// Returns `(expires, avg_ttl_ms)` for `INFO keyspace`, mirroring Redis.
    ///
    /// - `expires`: count of live keys that have a TTL (already-expired keys
    ///   are excluded, matching `dbsize`).
    /// - `avg_ttl_ms`: mean remaining TTL in milliseconds across those keys,
    ///   rounded down. `0` when no keys have a TTL.
    ///
    /// O(n) over the dataset; acceptable since `INFO` is an infrequent
    /// administrative command (FERRUM-002 Option B).
    pub fn expire_stats(&self) -> Result<(usize, u64), FerrumError> {
        let store = self.store.read()?;
        let now = Instant::now();
        let mut count = 0usize;
        let mut sum_ms: u64 = 0;
        for entry in store.values() {
            if entry.is_expired(now) {
                continue;
            }
            if let Some(deadline) = entry.expire_at
                && let Some(remaining_ms) = deadline
                    .checked_duration_since(now)
                    .map(|d| d.as_millis() as u64)
            {
                count += 1;
                sum_ms = sum_ms.saturating_add(remaining_ms);
            }
        }
        let avg_ttl = if count == 0 { 0 } else { sum_ms / count as u64 };
        Ok((count, avg_ttl))
    }

    /// Appends `suffix` to the value at `key` and returns the new length.
    ///
    /// If `key` is absent, the command behaves like `SET` with an empty
    /// initial value (the same contract as Redis). The resulting value is
    /// subject to the usual size validation. The existing TTL, if any, is
    /// preserved and re-emitted to the AOF so replay converges regardless
    /// of record ordering.
    pub fn append(&self, key: Vec<u8>, suffix: Vec<u8>) -> Result<usize, FerrumError> {
        validate_key(&key)?;

        let mut store = self.store.write()?;
        let now = Instant::now();
        let (new_value, existing_deadline) = match store.get(key.as_slice()) {
            Some(entry) if entry.is_expired(now) => {
                self.track_remove(&mut store, key.as_slice());
                self.log_expire_drop(&key);
                (suffix, None)
            }
            Some(entry) => {
                let mut buf = Vec::with_capacity(entry.data.len() + suffix.len());
                buf.extend_from_slice(&entry.data);
                buf.extend_from_slice(&suffix);
                (buf, entry.expire_at)
            }
            None => (suffix, None),
        };
        validate_value(&new_value)?;

        self.enforce_for_write(&mut store, &key, new_value.len())?;
        if let Some(aof) = &self.aof {
            log_aof_result("APPEND", aof.append_set(&key, &new_value));
            if let Some(deadline) = existing_deadline
                && let Some(abs_ms) = deadline_to_epoch_ms(deadline, now)
            {
                log_aof_result("PEXPIREAT", aof.append_pexpireat(&key, abs_ms));
            }
        }
        let new_len = new_value.len();
        self.track_insert(
            &mut store,
            key,
            ValueEntry::new_with(new_value, existing_deadline),
        );
        Ok(new_len)
    }

    /// Returns the byte length of the value at `key`, or `0` if absent.
    pub fn strlen(&self, key: &[u8]) -> Result<usize, FerrumError> {
        let mut store = self.store.write()?;
        let now = Instant::now();
        match store.get(key) {
            Some(entry) if entry.is_expired(now) => {
                self.track_remove(&mut store, key);
                self.log_expire_drop(key);
                Ok(0)
            }
            Some(entry) => Ok(entry.data.len()),
            None => Ok(0),
        }
    }

    /// Removes all keys from the store.
    pub fn flushdb(&self) -> Result<(), FerrumError> {
        let mut store = self.store.write()?;
        self.track_clear(&mut store);
        if let Some(aof) = &self.aof {
            log_aof_result("FLUSHDB", aof.append_flushdb());
        }
        Ok(())
    }

    /// Installs an absolute expiration time on `key`, measured in Unix
    /// epoch milliseconds.
    ///
    /// Returns `true` if the key exists and the deadline was recorded.
    /// Returns `false` when the key is absent or has already expired,
    /// matching the Redis semantics of `EXPIRE`/`PEXPIREAT` returning `0`.
    ///
    /// A deadline in the past (`abs_epoch_ms <= now`) causes the key to
    /// be removed immediately and an accompanying `DEL` to be logged,
    /// keeping the AOF idempotent across replays.
    pub fn expire_at_ms(&self, key: &[u8], abs_epoch_ms: i64) -> Result<bool, FerrumError> {
        let mut store = self.store.write()?;
        let now_instant = Instant::now();
        let now_ms = current_epoch_ms();

        // Drop the entry if it is already expired under its current TTL.
        if let Some(entry) = store.get(key)
            && entry.is_expired(now_instant)
        {
            self.track_remove(&mut store, key);
            self.log_expire_drop(key);
        }

        if !store.contains_key(key) {
            return Ok(false);
        }

        if abs_epoch_ms <= now_ms {
            self.track_remove(&mut store, key);
            if let Some(aof) = &self.aof {
                log_aof_result("DEL", aof.append_del(key));
            }
            return Ok(true);
        }

        let delta_ms = (abs_epoch_ms - now_ms) as u64;
        let deadline = now_instant + Duration::from_millis(delta_ms);
        if let Some(entry) = store.get_mut(key) {
            entry.expire_at = Some(deadline);
        }
        if let Some(aof) = &self.aof {
            log_aof_result("PEXPIREAT", aof.append_pexpireat(key, abs_epoch_ms));
        }
        Ok(true)
    }

    /// Removes any TTL from `key`.
    ///
    /// Returns `true` only when the key existed **and** had a TTL before
    /// the call — matching Redis' `PERSIST` return semantics.
    pub fn persist(&self, key: &[u8]) -> Result<bool, FerrumError> {
        let mut store = self.store.write()?;
        let now = Instant::now();

        if let Some(entry) = store.get(key)
            && entry.is_expired(now)
        {
            self.track_remove(&mut store, key);
            self.log_expire_drop(key);
            return Ok(false);
        }

        let Some(entry) = store.get_mut(key) else {
            return Ok(false);
        };
        if entry.expire_at.is_none() {
            return Ok(false);
        }
        entry.expire_at = None;
        if let Some(aof) = &self.aof {
            log_aof_result("PERSIST", aof.append_persist(key));
        }
        Ok(true)
    }

    /// Returns the remaining TTL for `key` in milliseconds.
    ///
    /// * `Ok(TtlStatus::Missing)` — key does not exist (Redis reports `-2`).
    /// * `Ok(TtlStatus::NoExpire)` — key exists without a TTL (Redis `-1`).
    /// * `Ok(TtlStatus::Millis(n))` — `n` milliseconds remaining.
    pub fn ttl_ms(&self, key: &[u8]) -> Result<TtlStatus, FerrumError> {
        let mut store = self.store.write()?;
        let now = Instant::now();
        match store.get(key) {
            None => Ok(TtlStatus::Missing),
            Some(entry) if entry.is_expired(now) => {
                self.track_remove(&mut store, key);
                self.log_expire_drop(key);
                Ok(TtlStatus::Missing)
            }
            Some(entry) => match entry.expire_at {
                None => Ok(TtlStatus::NoExpire),
                Some(deadline) => {
                    let remaining = deadline.saturating_duration_since(now);
                    Ok(TtlStatus::Millis(remaining.as_millis() as i64))
                }
            },
        }
    }

    /// Proactively removes up to `sample` expired entries.
    ///
    /// Sampling is random — the first `sample` keys yielded by the map's
    /// iteration order are checked. This mirrors Redis' active expiration
    /// loop and is the primary caller of the `ferrum-expire` background
    /// thread. Returns the number of entries actually evicted.
    ///
    /// When more than 25% of the sample was expired the caller is expected
    /// to invoke this method again immediately; the signal is surfaced via
    /// the returned fraction so the scheduler can make that decision.
    pub fn sweep_expired(&self, sample: usize) -> Result<SweepStats, FerrumError> {
        if sample == 0 {
            return Ok(SweepStats {
                examined: 0,
                evicted: 0,
            });
        }
        let mut store = self.store.write()?;
        let now = Instant::now();

        let mut victims: Vec<Vec<u8>> = Vec::new();
        let mut examined = 0usize;
        for (key, entry) in store.iter() {
            if examined >= sample {
                break;
            }
            examined += 1;
            if entry.is_expired(now) {
                victims.push(key.clone());
            }
        }

        let evicted = victims.len();
        for key in &victims {
            self.track_remove(&mut store, key.as_slice());
            if let Some(aof) = &self.aof {
                log_aof_result("DEL", aof.append_del(key));
            }
        }

        Ok(SweepStats { examined, evicted })
    }

    /// Logs a DEL record caused by lazy expiration.
    ///
    /// Kept separate from `del()` so call sites stay terse; callers must
    /// already hold the write lock.
    fn log_expire_drop(&self, key: &[u8]) {
        if let Some(aof) = &self.aof {
            log_aof_result("DEL", aof.append_del(key));
        }
    }

    /// Returns the current approximate memory footprint, in bytes.
    ///
    /// Includes key bytes, value bytes, and a fixed per-entry overhead
    /// (see [`PER_ENTRY_OVERHEAD`]). The value is eventually consistent
    /// with the store: it is updated inside the write lock that guards
    /// every mutation, so callers observing it after a successful command
    /// always see the post-mutation total.
    pub fn used_memory(&self) -> u64 {
        self.used_memory.load(Ordering::Relaxed)
    }

    /// Returns the approximate per-entry cost of `key`, in bytes, or
    /// `None` if the key is absent or already expired.
    ///
    /// Matches the `MEMORY USAGE` command's contract of reporting the
    /// single-entry contribution that `used_memory()` would shed if the
    /// key were removed.
    pub fn memory_usage(&self, key: &[u8]) -> Result<Option<u64>, FerrumError> {
        let mut store = self.store.write()?;
        let now = Instant::now();
        match store.get(key) {
            Some(entry) if entry.is_expired(now) => {
                // Surface the expiration through the normal lazy path so
                // both MEMORY USAGE and GET agree on the key being gone.
                let removed = store.remove(key);
                if let Some(entry) = removed {
                    self.untrack(key, &entry);
                }
                self.log_expire_drop(key);
                Ok(None)
            }
            Some(entry) => Ok(Some(util::entry_bytes(key, entry))),
            None => Ok(None),
        }
    }

    /// Inserts `entry` into `store` and updates the memory counter,
    /// returning the old entry (if any) so callers can reason about
    /// overwrite semantics.
    fn track_insert(
        &self,
        store: &mut HashMap<Vec<u8>, ValueEntry>,
        key: Vec<u8>,
        entry: ValueEntry,
    ) -> Option<ValueEntry> {
        let incoming = util::entry_bytes(&key, &entry);
        let previous = store.insert(key.clone(), entry);
        let outgoing = previous
            .as_ref()
            .map(|p| util::entry_bytes(&key, p))
            .unwrap_or(0);
        self.apply_delta(incoming as i64 - outgoing as i64);
        self.sieve_notify_insert(&key);
        self.ac_notify_insert(&key);
        previous
    }

    /// Evicts keys until `used_memory + incoming <= max_memory`, honouring
    /// the active [`EvictionPolicy`].
    ///
    /// `incoming` is the approximate byte cost of the entry that is about
    /// to be written. Passing it up front lets the sweep leave enough
    /// headroom for a single write so the caller never overshoots the
    /// ceiling by a full entry. Callers that only want the current
    /// counter to fit (for example, retroactive enforcement after a
    /// `maxmemory` shrink) can pass `0`.
    ///
    /// Returns `Err(FerrumError::OutOfMemory)` when the policy is
    /// `noeviction`, when no policy-eligible key exists, or when the best
    /// effort eviction still cannot free enough space. Successful evictions
    /// are logged to the AOF so replay converges on the post-eviction state.
    ///
    /// The caller must already hold the write lock on `store`.
    fn enforce_memory_limit(
        &self,
        store: &mut HashMap<Vec<u8>, ValueEntry>,
        incoming: u64,
    ) -> Result<(), FerrumError> {
        let cfg = *self.eviction.read()?;
        if cfg.max_memory == 0 {
            return Ok(());
        }
        let fits = |used: u64| used.saturating_add(incoming) <= cfg.max_memory;
        let mut evicted = 0u64;
        // Bounded loop so a pathological policy cannot spin forever.
        for _ in 0..store.len().max(1) + 16 {
            if fits(self.used_memory.load(Ordering::Relaxed)) {
                break;
            }
            if cfg.policy == EvictionPolicy::NoEviction {
                return Err(FerrumError::OutOfMemory);
            }

            // SIEVE keeps its own FIFO queue + hand, so it does not consult a
            // random sample; the other policies do.
            let victim_key: Option<Vec<u8>> = match cfg.policy {
                EvictionPolicy::AllKeysAhe | EvictionPolicy::VolatileAhe => {
                    let alpha = self.ahe_alpha();
                    eviction::pick_victim_ahe(
                        alpha,
                        sample_candidates(store, cfg.policy.scope(), cfg.samples),
                    )
                    .map(|c| c.key)
                }
                EvictionPolicy::AllKeysSieve
                | EvictionPolicy::VolatileSieve
                | EvictionPolicy::AllKeysSieveS
                | EvictionPolicy::VolatileSieveS => {
                    let ttl_aware = matches!(
                        cfg.policy,
                        EvictionPolicy::AllKeysSieveS | EvictionPolicy::VolatileSieveS
                    );
                    self.evict_sieve(store, cfg.policy.scope(), ttl_aware)?
                }
                EvictionPolicy::AllKeysAdaptiveClimb | EvictionPolicy::VolatileAdaptiveClimb => {
                    self.evict_ac(store, cfg.policy.scope())?
                }
                _ => eviction::pick_victim(
                    cfg.policy,
                    sample_candidates(store, cfg.policy.scope(), cfg.samples),
                )
                .map(|c| c.key),
            };
            let Some(victim_key) = victim_key else {
                // For volatile policies on a dataset without TTL, Redis
                // reports OOM; we do the same so the caller sees a clear
                // failure instead of a silent accept.
                return Err(FerrumError::OutOfMemory);
            };
            self.track_remove(store, &victim_key);
            evicted += 1;
            if let Some(aof) = &self.aof {
                log_aof_result("DEL", aof.append_del(&victim_key));
            }
        }

        // Feed the AHE controller exactly once per sweep so it sees
        // post-eviction hit ratios, not partial state.
        if evicted > 0
            && matches!(
                cfg.policy,
                EvictionPolicy::AllKeysAhe | EvictionPolicy::VolatileAhe
            )
        {
            let (hits, misses) = self.keyspace_stats();
            self.ahe_observe(hits, misses);
        }

        if fits(self.used_memory.load(Ordering::Relaxed)) {
            Ok(())
        } else {
            Err(FerrumError::OutOfMemory)
        }
    }

    /// Updates the `last_access` stamp on `key` if it is still live.
    ///
    /// Used by read paths to give the LRU policy some accuracy without
    /// forcing the caller to touch `ValueEntry` internals. Also advances
    /// the Morris LFU counter probabilistically.
    fn touch_access(&self, store: &mut HashMap<Vec<u8>, ValueEntry>, key: &[u8]) {
        if let Some(entry) = store.get_mut(key) {
            let r = self.next_rand01();
            entry.touch(r);
            self.sieve_notify_access(key);
            self.ac_notify_access(key);
        }
    }

    /// Runs one SIEVE sweep and returns the key to evict, or `None` when no
    /// eligible victim exists. The store is consulted read-only to test
    /// eligibility (TTL scope, tombstones); the queue mutation happens inside
    /// [`SieveState::evict_one`].
    fn evict_sieve(
        &self,
        store: &HashMap<Vec<u8>, ValueEntry>,
        scope: EvictionScope,
        ttl_aware: bool,
    ) -> Result<Option<Vec<u8>>, FerrumError> {
        let mut sieve = self.sieve.lock()?;
        Ok(sieve.evict_one(store, scope, ttl_aware, Instant::now()))
    }

    /// Runs one AdaptiveClimb sweep and returns the LRU-end key to evict, or
    /// `None` when no eligible victim exists. The store is consulted read-only
    /// for scope / tombstone checks; the list mutation happens inside
    /// [`AdaptiveClimbState::evict_one`].
    fn evict_ac(
        &self,
        store: &HashMap<Vec<u8>, ValueEntry>,
        scope: EvictionScope,
    ) -> Result<Option<Vec<u8>>, FerrumError> {
        let mut ac = self.ac.lock()?;
        Ok(ac.evict_one(store, scope))
    }

    /// Convenience wrapper: computes the net byte increase that writing
    /// `(key, value_len)` would introduce and forwards it to
    /// [`Self::enforce_memory_limit`].
    fn enforce_for_write(
        &self,
        store: &mut HashMap<Vec<u8>, ValueEntry>,
        key: &[u8],
        value_len: usize,
    ) -> Result<(), FerrumError> {
        let incoming = key.len() as u64 + value_len as u64 + PER_ENTRY_OVERHEAD;
        let net = incoming.saturating_sub(
            store
                .get(key)
                .map(|e| util::entry_bytes(key, e))
                .unwrap_or(0),
        );
        self.enforce_memory_limit(store, net)
    }

    /// Matches `text` against a Redis-style glob `pattern`.
    ///
    /// Supports `*`, `?`, character classes (`[abc]`, `[a-z]`, `[!abc]`) and
    /// backslash escapes. Used by [`KvEngine::scan_keys`] so the dashboard
    /// can offer prefix / wildcard key search.
    fn glob_match(pattern: &[u8], text: &[u8]) -> bool {
        fn matches(pat: &[u8], txt: &[u8]) -> bool {
            let (mut p, mut t) = (0usize, 0usize);
            let (pl, tl) = (pat.len(), txt.len());
            while p < pl {
                match pat[p] {
                    b'*' => {
                        while p + 1 < pl && pat[p + 1] == b'*' {
                            p += 1;
                        }
                        if p + 1 == pl {
                            return true;
                        }
                        for i in t..=tl {
                            if matches(&pat[p + 1..], &txt[i..]) {
                                return true;
                            }
                        }
                        return false;
                    }
                    b'?' => {
                        if t >= tl {
                            return false;
                        }
                        p += 1;
                        t += 1;
                    }
                    b'\\' => {
                        let pc = if p + 1 < pl { pat[p + 1] } else { pat[p] };
                        if t >= tl || txt[t] != pc {
                            return false;
                        }
                        p += 2;
                        t += 1;
                    }
                    b'[' => {
                        if t >= tl {
                            return false;
                        }
                        let mut j = p + 1;
                        let mut negate = false;
                        if j < pl && pat[j] == b'!' {
                            negate = true;
                            j += 1;
                        }
                        let mut matched = false;
                        let mut k = j;
                        while k < pl && pat[k] != b']' {
                            if pat[k] == b'\\' && k + 1 < pl {
                                if txt[t] == pat[k + 1] {
                                    matched = true;
                                }
                                k += 2;
                            } else if k + 2 < pl && pat[k + 1] == b'-' && pat[k + 2] != b']' {
                                let lo = pat[k];
                                let hi = pat[k + 2];
                                let c = txt[t];
                                if (lo <= hi && lo <= c && c <= hi)
                                    || (lo > hi && (c >= lo || c <= hi))
                                {
                                    matched = true;
                                }
                                k += 3;
                            } else if txt[t] == pat[k] {
                                matched = true;
                                k += 1;
                            } else {
                                k += 1;
                            }
                        }
                        if k >= pl {
                            // No closing bracket: treat '[' as a literal.
                            if txt[t] != b'[' {
                                return false;
                            }
                            p += 1;
                            t += 1;
                            continue;
                        }
                        if matched == negate {
                            return false;
                        }
                        p = k + 1;
                        t += 1;
                    }
                    c => {
                        if t >= tl || txt[t] != c {
                            return false;
                        }
                        p += 1;
                        t += 1;
                    }
                }
            }
            t == tl
        }
        matches(pattern, text)
    }

    /// Removes `key` from `store`, updates the memory counter, and
    /// returns the evicted entry. A no-op when the key is absent.
    fn track_remove(
        &self,
        store: &mut HashMap<Vec<u8>, ValueEntry>,
        key: &[u8],
    ) -> Option<ValueEntry> {
        let removed = store.remove(key);
        if let Some(entry) = &removed {
            self.apply_delta(-(util::entry_bytes(key, entry) as i64));
        }
        self.sieve_notify_remove(key);
        self.ac_notify_remove(key);
        removed
    }

    /// Clears every entry, resetting the memory counter to zero.
    fn track_clear(&self, store: &mut HashMap<Vec<u8>, ValueEntry>) {
        store.clear();
        self.sieve_notify_clear();
        self.ac_notify_clear();
        self.used_memory.store(0, Ordering::Relaxed);
    }

    /// Untracks an entry that was already removed by some other path
    /// (e.g. a caller that held onto the returned `Option<ValueEntry>`).
    fn untrack(&self, key: &[u8], entry: &ValueEntry) {
        self.apply_delta(-(util::entry_bytes(key, entry) as i64));
    }

    fn apply_delta(&self, delta: i64) {
        if delta == 0 {
            return;
        }
        if delta > 0 {
            self.used_memory.fetch_add(delta as u64, Ordering::Relaxed);
        } else {
            let mag = (-delta) as u64;
            // Saturating sub: the counter is an approximation, and any
            // underflow would only happen if the invariants slipped, which
            // we'd rather clamp than panic on in release builds.
            let mut cur = self.used_memory.load(Ordering::Relaxed);
            loop {
                let next = cur.saturating_sub(mag);
                match self.used_memory.compare_exchange_weak(
                    cur,
                    next,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(obs) => cur = obs,
                }
            }
        }
    }
}

impl KvEngine {
    /// Draws a uniform sample from `[0.0, 1.0)` using the engine's
    /// shared xorshift32 state. Used for LFU probabilistic increments
    /// and for any future tie-breaking that wants a cheap RNG.
    fn next_rand01(&self) -> f32 {
        let next = xorshift32(self.rng.load(Ordering::Relaxed));
        self.rng.store(next, Ordering::Relaxed);
        // Map the top 24 bits onto `[0, 1)`; enough precision for `f32`
        // without worrying about the exponent corner cases of the full
        // 32-bit range.
        (next >> 8) as f32 / ((1u32 << 24) as f32)
    }

    /// Records a keyspace hit. Called by every read path that returns a
    /// live value so the AHE feedback loop sees representative traffic.
    pub(crate) fn record_hit(&self) {
        self.hits.fetch_add(1, Ordering::Relaxed);
    }

    /// Records a keyspace miss (key absent or expired).
    pub(crate) fn record_miss(&self) {
        self.misses.fetch_add(1, Ordering::Relaxed);
    }

    /// Snapshot of `(hits, misses)` used by `INFO stats`.
    pub fn keyspace_stats(&self) -> (u64, u64) {
        (
            self.hits.load(Ordering::Relaxed),
            self.misses.load(Ordering::Relaxed),
        )
    }

    /// Returns a copy of the live AHE controller state. Intended for
    /// observability; callers must not rely on values being stable across
    /// consecutive reads since the controller mutates during evictions.
    pub fn ahe_snapshot(&self) -> AdaptiveHybridState {
        match self.ahe.lock() {
            Ok(g) => *g,
            // Lock poisoning happens only if another thread panicked
            // while holding it; return defaults rather than propagating
            // so observability calls can't take the server down.
            Err(p) => *p.into_inner(),
        }
    }

    /// Returns the current AHE blend weight, falling back to the default
    /// if the controller lock is poisoned.
    fn ahe_alpha(&self) -> f32 {
        match self.ahe.lock() {
            Ok(g) => g.alpha,
            Err(p) => p.into_inner().alpha,
        }
    }

    /// Feeds a `(hits, misses)` sample into the AHE controller. Silently
    /// ignored on lock poisoning so an observability mishap cannot abort
    /// the eviction sweep.
    fn ahe_observe(&self, hits: u64, misses: u64) {
        if let Ok(mut g) = self.ahe.lock() {
            g.observe(hits, misses);
        }
    }
}
