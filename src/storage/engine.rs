use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use log::warn;

use crate::error::FerrumError;
use crate::persistence::AofWriter;
use crate::storage::eviction::{self, Candidate, EvictionConfig, EvictionPolicy, EvictionScope};

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

/// A value stored in the engine, together with its optional expiration.
///
/// `expire_at` is a monotonic deadline derived from [`Instant::now`] at the
/// time the TTL was installed. A value whose deadline is `<= Instant::now()`
/// is considered expired and must be treated as absent by every read path.
///
/// `last_access` tracks the most recent successful read or write and is
/// maintained inside the store's write lock. Only the approximate LRU
/// policies consult it, so keeping it current is best-effort.
#[derive(Clone)]
struct ValueEntry {
    data: Vec<u8>,
    expire_at: Option<Instant>,
    last_access: Instant,
}

impl ValueEntry {
    fn new(data: Vec<u8>) -> Self {
        Self {
            data,
            expire_at: None,
            last_access: Instant::now(),
        }
    }

    fn is_expired(&self, now: Instant) -> bool {
        matches!(self.expire_at, Some(deadline) if deadline <= now)
    }

    fn touch(&mut self) {
        self.last_access = Instant::now();
    }
}

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
    store: Arc<RwLock<HashMap<Vec<u8>, ValueEntry>>>,
    aof: Option<Arc<AofWriter>>,
    /// Approximate memory footprint of the live dataset, in bytes.
    ///
    /// The counter is updated inside the store's write lock so it stays
    /// consistent with the `HashMap` it describes. Readers can load it
    /// without holding any engine lock, which keeps `INFO memory` cheap.
    used_memory: Arc<AtomicU64>,
    /// Live eviction configuration.
    ///
    /// Wrapped in a lock so the server can mutate it at runtime (e.g. via
    /// a future `CONFIG SET` command) without cloning the whole engine.
    eviction: Arc<RwLock<EvictionConfig>>,
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
    /// eligible under the new policy immediately.
    pub fn set_eviction_config(&self, cfg: EvictionConfig) -> Result<(), FerrumError> {
        let mut guard = self.eviction.write()?;
        *guard = cfg;
        Ok(())
    }

    /// Returns a copy of the current eviction configuration.
    pub fn eviction_config(&self) -> Result<EvictionConfig, FerrumError> {
        Ok(*self.eviction.read()?)
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
                    out.push(None);
                }
                Some(entry) => {
                    let data = entry.data.clone();
                    self.touch_access(&mut store, key.as_slice());
                    out.push(Some(data));
                }
                None => out.push(None),
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
            ValueEntry {
                data: serialised,
                expire_at: existing_deadline,
                last_access: Instant::now(),
            },
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
                Ok(None)
            }
            Some(entry) => {
                let data = entry.data.clone();
                self.touch_access(&mut store, key);
                Ok(Some(data))
            }
            None => Ok(None),
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
                Ok(false)
            }
            Some(_) => Ok(true),
            None => Ok(false),
        }
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
            ValueEntry {
                data: new_value,
                expire_at: existing_deadline,
                last_access: Instant::now(),
            },
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
            Some(entry) => Ok(Some(entry_bytes(key, entry))),
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
        let incoming = entry_bytes(&key, &entry);
        let previous = store.insert(key.clone(), entry);
        let outgoing = previous.as_ref().map(|p| entry_bytes(&key, p)).unwrap_or(0);
        self.apply_delta(incoming as i64 - outgoing as i64);
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
        // Bounded loop so a pathological policy cannot spin forever.
        for _ in 0..store.len().max(1) + 16 {
            if fits(self.used_memory.load(Ordering::Relaxed)) {
                return Ok(());
            }
            if cfg.policy == EvictionPolicy::NoEviction {
                return Err(FerrumError::OutOfMemory);
            }

            let candidates = sample_candidates(store, cfg.policy.scope(), cfg.samples);
            let Some(victim) = eviction::pick_victim(cfg.policy, candidates) else {
                // For volatile policies on a dataset without TTL, Redis
                // reports OOM; we do the same so the caller sees a clear
                // failure instead of a silent accept.
                return Err(FerrumError::OutOfMemory);
            };
            self.track_remove(store, &victim.key);
            if let Some(aof) = &self.aof {
                log_aof_result("DEL", aof.append_del(&victim.key));
            }
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
    /// forcing the caller to touch `ValueEntry` internals.
    fn touch_access(&self, store: &mut HashMap<Vec<u8>, ValueEntry>, key: &[u8]) {
        if let Some(entry) = store.get_mut(key) {
            entry.touch();
        }
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
        let net = incoming.saturating_sub(store.get(key).map(|e| entry_bytes(key, e)).unwrap_or(0));
        self.enforce_memory_limit(store, net)
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
            self.apply_delta(-(entry_bytes(key, entry) as i64));
        }
        removed
    }

    /// Clears every entry, resetting the memory counter to zero.
    fn track_clear(&self, store: &mut HashMap<Vec<u8>, ValueEntry>) {
        store.clear();
        self.used_memory.store(0, Ordering::Relaxed);
    }

    /// Untracks an entry that was already removed by some other path
    /// (e.g. a caller that held onto the returned `Option<ValueEntry>`).
    fn untrack(&self, key: &[u8], entry: &ValueEntry) {
        self.apply_delta(-(entry_bytes(key, entry) as i64));
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

/// Approximate byte cost of a single live entry.
fn entry_bytes(key: &[u8], entry: &ValueEntry) -> u64 {
    key.len() as u64 + entry.data.len() as u64 + PER_ENTRY_OVERHEAD
}

/// Samples up to `sample` candidates from `store` under the given scope.
///
/// `HashMap`'s iteration order is already randomised by its hasher, so we
/// rely on that for "pick at random" without introducing an extra RNG
/// dependency. For `EvictionScope::Volatile` we skip keys without a TTL and
/// keep walking until the cap is reached or the store is exhausted.
fn sample_candidates(
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
        });
    }
    out
}

/// Outcome of a single call to [`KvEngine::sweep_expired`].
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

/// Tri-state return for [`KvEngine::ttl_ms`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtlStatus {
    /// The key does not exist; RESP reply is `:-2`.
    Missing,
    /// The key exists with no TTL; RESP reply is `:-1`.
    NoExpire,
    /// Remaining TTL in milliseconds (`>= 0`).
    Millis(i64),
}

/// Returns the payload of `entry` if it has not already expired.
fn live_payload(entry: ValueEntry) -> Option<Vec<u8>> {
    let now = Instant::now();
    if entry.is_expired(now) {
        None
    } else {
        Some(entry.data)
    }
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
fn deadline_to_epoch_ms(deadline: Instant, now: Instant) -> Option<i64> {
    if deadline <= now {
        return None;
    }
    let remaining = deadline - now;
    let now_ms = current_epoch_ms();
    let abs_ms = now_ms.saturating_add(remaining.as_millis() as i64);
    Some(abs_ms)
}

fn validate_key(key: &[u8]) -> Result<(), FerrumError> {
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

fn validate_value(value: &[u8]) -> Result<(), FerrumError> {
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
fn log_aof_result(cmd: &str, result: Result<(), FerrumError>) {
    if let Err(e) = result {
        warn!("aof append for {cmd} failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::AofWriter;
    use crate::persistence::config::{AofConfig, FsyncPolicy};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tmp_aof_path(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("ferrum-engine-{label}-{nanos}-{n}.aof"))
    }

    fn engine_with_aof(path: &PathBuf) -> (KvEngine, Arc<AofWriter>) {
        let cfg = AofConfig::new(path, FsyncPolicy::Always);
        let writer = Arc::new(AofWriter::open(&cfg).unwrap());
        let engine = KvEngine::new().with_aof(Arc::clone(&writer));
        (engine, writer)
    }

    #[test]
    fn test_set_and_get() {
        let engine = KvEngine::new();
        engine.set(b"name".to_vec(), b"ferrum".to_vec()).unwrap();
        assert_eq!(engine.get(b"name").unwrap(), Some(b"ferrum".to_vec()));
    }

    #[test]
    fn test_get_nonexistent() {
        let engine = KvEngine::new();
        assert_eq!(engine.get(b"missing").unwrap(), None);
    }

    #[test]
    fn test_set_overwrite() {
        let engine = KvEngine::new();
        engine.set(b"key".to_vec(), b"v1".to_vec()).unwrap();
        let old = engine.set(b"key".to_vec(), b"v2".to_vec()).unwrap();
        assert_eq!(old, Some(b"v1".to_vec()));
        assert_eq!(engine.get(b"key").unwrap(), Some(b"v2".to_vec()));
    }

    #[test]
    fn test_del_existing() {
        let engine = KvEngine::new();
        engine.set(b"key".to_vec(), b"value".to_vec()).unwrap();
        assert!(engine.del(b"key").unwrap());
        assert_eq!(engine.get(b"key").unwrap(), None);
    }

    #[test]
    fn test_del_nonexistent() {
        let engine = KvEngine::new();
        assert!(!engine.del(b"missing").unwrap());
    }

    #[test]
    fn del_many_counts_existing_keys_only() {
        let engine = KvEngine::new();
        engine.set(b"a".to_vec(), b"1".to_vec()).unwrap();
        engine.set(b"b".to_vec(), b"2".to_vec()).unwrap();

        let removed = engine
            .del_many(&[b"a".to_vec(), b"missing".to_vec(), b"b".to_vec()])
            .unwrap();
        assert_eq!(removed, 2);
        assert_eq!(engine.dbsize().unwrap(), 0);
    }

    #[test]
    fn del_many_with_empty_input_returns_zero() {
        let engine = KvEngine::new();
        assert_eq!(engine.del_many(&[]).unwrap(), 0);
    }

    #[test]
    fn del_many_logs_only_existing_keys_to_aof() {
        let path = tmp_aof_path("del-many");
        let (engine, writer) = engine_with_aof(&path);

        engine.set(b"a".to_vec(), b"1".to_vec()).unwrap();
        engine
            .del_many(&[b"a".to_vec(), b"missing".to_vec()])
            .unwrap();
        drop(engine);
        drop(writer);

        let bytes = fs::read(&path).unwrap();
        let expected = concat!(
            "*3\r\n$3\r\nSET\r\n$1\r\na\r\n$1\r\n1\r\n",
            "*2\r\n$3\r\nDEL\r\n$1\r\na\r\n",
        );
        assert_eq!(bytes, expected.as_bytes());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn append_to_missing_key_creates_it() {
        let engine = KvEngine::new();
        let len = engine.append(b"k".to_vec(), b"hello".to_vec()).unwrap();
        assert_eq!(len, 5);
        assert_eq!(engine.get(b"k").unwrap(), Some(b"hello".to_vec()));
    }

    #[test]
    fn append_extends_existing_value() {
        let engine = KvEngine::new();
        engine.set(b"k".to_vec(), b"hello ".to_vec()).unwrap();
        let len = engine.append(b"k".to_vec(), b"world".to_vec()).unwrap();
        assert_eq!(len, 11);
        assert_eq!(engine.get(b"k").unwrap(), Some(b"hello world".to_vec()));
    }

    #[test]
    fn append_respects_value_size_limit() {
        let engine = KvEngine::new();
        let big = vec![b'x'; VALUE_MAX_BYTES];
        engine.set(b"k".to_vec(), big).unwrap();
        let err = engine.append(b"k".to_vec(), vec![b'y']).unwrap_err();
        assert!(matches!(err, FerrumError::ValueTooLarge { .. }));
    }

    #[test]
    fn append_persists_final_state_to_aof() {
        let path = tmp_aof_path("append");
        let (engine, writer) = engine_with_aof(&path);

        engine.append(b"k".to_vec(), b"hello ".to_vec()).unwrap();
        engine.append(b"k".to_vec(), b"world".to_vec()).unwrap();
        drop(engine);
        drop(writer);

        // Each APPEND is logged as a SET carrying the new full value so
        // replay converges regardless of the order records are applied in.
        let bytes = fs::read(&path).unwrap();
        let expected = concat!(
            "*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$6\r\nhello \r\n",
            "*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$11\r\nhello world\r\n",
        );
        assert_eq!(bytes, expected.as_bytes());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn strlen_returns_zero_for_missing_key() {
        let engine = KvEngine::new();
        assert_eq!(engine.strlen(b"missing").unwrap(), 0);
    }

    #[test]
    fn strlen_counts_raw_bytes() {
        let engine = KvEngine::new();
        engine
            .set(b"k".to_vec(), vec![0x00, 0xff, b'a', b'b'])
            .unwrap();
        assert_eq!(engine.strlen(b"k").unwrap(), 4);
    }

    #[test]
    fn set_nx_inserts_when_key_is_absent() {
        let engine = KvEngine::new();
        assert!(engine.set_nx(b"k".to_vec(), b"v1".to_vec()).unwrap());
        assert_eq!(engine.get(b"k").unwrap(), Some(b"v1".to_vec()));
    }

    #[test]
    fn set_nx_is_noop_when_key_exists() {
        let engine = KvEngine::new();
        engine.set(b"k".to_vec(), b"original".to_vec()).unwrap();
        assert!(!engine.set_nx(b"k".to_vec(), b"other".to_vec()).unwrap());
        assert_eq!(engine.get(b"k").unwrap(), Some(b"original".to_vec()));
    }

    #[test]
    fn set_nx_only_logs_successful_inserts_to_aof() {
        let path = tmp_aof_path("setnx");
        let (engine, writer) = engine_with_aof(&path);

        assert!(engine.set_nx(b"k".to_vec(), b"v".to_vec()).unwrap());
        assert!(!engine.set_nx(b"k".to_vec(), b"other".to_vec()).unwrap());
        drop(engine);
        drop(writer);

        let bytes = fs::read(&path).unwrap();
        assert_eq!(bytes, b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn mset_inserts_every_pair() {
        let engine = KvEngine::new();
        engine
            .mset(vec![
                (b"a".to_vec(), b"1".to_vec()),
                (b"b".to_vec(), b"2".to_vec()),
            ])
            .unwrap();
        assert_eq!(engine.get(b"a").unwrap(), Some(b"1".to_vec()));
        assert_eq!(engine.get(b"b").unwrap(), Some(b"2".to_vec()));
    }

    #[test]
    fn mset_rejects_batch_without_applying_any_pair() {
        let engine = KvEngine::new();
        engine.set(b"existing".to_vec(), b"keep".to_vec()).unwrap();

        let too_big = vec![b'x'; VALUE_MAX_BYTES + 1];
        let err = engine
            .mset(vec![
                (b"a".to_vec(), b"1".to_vec()),
                (b"b".to_vec(), too_big),
            ])
            .unwrap_err();
        assert!(matches!(err, FerrumError::ValueTooLarge { .. }));
        // Neither pair made it into the store because validation happens
        // before any mutation.
        assert_eq!(engine.get(b"a").unwrap(), None);
        assert_eq!(engine.get(b"b").unwrap(), None);
        assert_eq!(engine.get(b"existing").unwrap(), Some(b"keep".to_vec()));
    }

    #[test]
    fn mset_writes_batch_atomically_to_aof() {
        let path = tmp_aof_path("mset");
        let (engine, writer) = engine_with_aof(&path);

        engine
            .mset(vec![
                (b"a".to_vec(), b"1".to_vec()),
                (b"b".to_vec(), b"2".to_vec()),
            ])
            .unwrap();
        drop(engine);
        drop(writer);

        let bytes = fs::read(&path).unwrap();
        let expected = concat!(
            "*3\r\n$3\r\nSET\r\n$1\r\na\r\n$1\r\n1\r\n",
            "*3\r\n$3\r\nSET\r\n$1\r\nb\r\n$1\r\n2\r\n",
        );
        assert_eq!(bytes, expected.as_bytes());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn mget_returns_values_in_request_order() {
        let engine = KvEngine::new();
        engine.set(b"a".to_vec(), b"1".to_vec()).unwrap();
        engine.set(b"c".to_vec(), b"3".to_vec()).unwrap();

        let values = engine
            .mget(&[b"a".to_vec(), b"missing".to_vec(), b"c".to_vec()])
            .unwrap();
        assert_eq!(values, vec![Some(b"1".to_vec()), None, Some(b"3".to_vec())]);
    }

    #[test]
    fn incr_by_initialises_missing_key_from_zero() {
        let engine = KvEngine::new();
        assert_eq!(engine.incr_by(b"counter".to_vec(), 1).unwrap(), 1);
        assert_eq!(engine.incr_by(b"counter".to_vec(), 4).unwrap(), 5);
        assert_eq!(engine.get(b"counter").unwrap(), Some(b"5".to_vec()));
    }

    #[test]
    fn incr_by_supports_negative_delta() {
        let engine = KvEngine::new();
        engine.set(b"k".to_vec(), b"10".to_vec()).unwrap();
        assert_eq!(engine.incr_by(b"k".to_vec(), -3).unwrap(), 7);
    }

    #[test]
    fn incr_by_rejects_non_integer_value() {
        let engine = KvEngine::new();
        engine.set(b"k".to_vec(), b"not a number".to_vec()).unwrap();
        let err = engine.incr_by(b"k".to_vec(), 1).unwrap_err();
        assert!(matches!(err, FerrumError::ParseError(ref m) if m.contains("integer")));
    }

    #[test]
    fn incr_by_reports_overflow_as_parse_error() {
        let engine = KvEngine::new();
        engine
            .set(b"k".to_vec(), i64::MAX.to_string().into_bytes())
            .unwrap();
        let err = engine.incr_by(b"k".to_vec(), 1).unwrap_err();
        assert!(matches!(err, FerrumError::ParseError(ref m) if m.contains("integer")));
    }

    #[test]
    fn incr_by_persists_new_integer_to_aof() {
        let path = tmp_aof_path("incrby");
        let (engine, writer) = engine_with_aof(&path);

        engine.incr_by(b"counter".to_vec(), 5).unwrap();
        engine.incr_by(b"counter".to_vec(), -2).unwrap();
        drop(engine);
        drop(writer);

        let bytes = fs::read(&path).unwrap();
        let expected = concat!(
            "*3\r\n$3\r\nSET\r\n$7\r\ncounter\r\n$1\r\n5\r\n",
            "*3\r\n$3\r\nSET\r\n$7\r\ncounter\r\n$1\r\n3\r\n",
        );
        assert_eq!(bytes, expected.as_bytes());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_exists() {
        let engine = KvEngine::new();
        assert!(!engine.exists(b"key").unwrap());
        engine.set(b"key".to_vec(), b"value".to_vec()).unwrap();
        assert!(engine.exists(b"key").unwrap());
        engine.del(b"key").unwrap();
        assert!(!engine.exists(b"key").unwrap());
    }

    #[test]
    fn test_dbsize_empty() {
        let engine = KvEngine::new();
        assert_eq!(engine.dbsize().unwrap(), 0);
    }

    #[test]
    fn test_dbsize_after_operations() {
        let engine = KvEngine::new();
        engine.set(b"a".to_vec(), b"1".to_vec()).unwrap();
        engine.set(b"b".to_vec(), b"2".to_vec()).unwrap();
        assert_eq!(engine.dbsize().unwrap(), 2);
        engine.del(b"a").unwrap();
        assert_eq!(engine.dbsize().unwrap(), 1);
    }

    #[test]
    fn test_flushdb() {
        let engine = KvEngine::new();
        engine.set(b"a".to_vec(), b"1".to_vec()).unwrap();
        engine.set(b"b".to_vec(), b"2".to_vec()).unwrap();
        engine.flushdb().unwrap();
        assert_eq!(engine.dbsize().unwrap(), 0);
        assert_eq!(engine.get(b"a").unwrap(), None);
    }

    #[test]
    fn test_set_rejects_empty_key() {
        let engine = KvEngine::new();
        let err = engine.set(Vec::new(), b"v".to_vec()).unwrap_err();
        assert!(matches!(err, FerrumError::ParseError(_)));
    }

    #[test]
    fn test_set_rejects_oversized_key() {
        let engine = KvEngine::new();
        let big_key = vec![b'k'; KEY_MAX_BYTES + 1];
        let err = engine.set(big_key, b"v".to_vec()).unwrap_err();
        assert!(matches!(
            err,
            FerrumError::KeyTooLong {
                len,
                max: KEY_MAX_BYTES,
            } if len == KEY_MAX_BYTES + 1
        ));
    }

    #[test]
    fn test_set_accepts_boundary_key_length() {
        let engine = KvEngine::new();
        let key = vec![b'k'; KEY_MAX_BYTES];
        assert!(engine.set(key.clone(), b"v".to_vec()).is_ok());
        assert_eq!(engine.get(&key).unwrap(), Some(b"v".to_vec()));
    }

    #[test]
    fn test_set_rejects_oversized_value() {
        let engine = KvEngine::new();
        let big_value = vec![b'v'; VALUE_MAX_BYTES + 1];
        let err = engine.set(b"k".to_vec(), big_value).unwrap_err();
        assert!(matches!(
            err,
            FerrumError::ValueTooLarge {
                len,
                max: VALUE_MAX_BYTES,
            } if len == VALUE_MAX_BYTES + 1
        ));
    }

    #[test]
    fn binary_safe_key_and_value_round_trip() {
        let engine = KvEngine::new();
        let key: Vec<u8> = vec![0x00, 0x01, 0xff, 0xfe];
        let value: Vec<u8> = vec![0x80, 0x00, b'\r', b'\n', 0xc3, 0x28];
        engine.set(key.clone(), value.clone()).unwrap();
        assert_eq!(engine.get(&key).unwrap(), Some(value));
        assert!(engine.exists(&key).unwrap());
        assert!(engine.del(&key).unwrap());
        assert_eq!(engine.get(&key).unwrap(), None);
    }

    #[test]
    fn test_concurrent_access() {
        use std::thread;

        let engine = KvEngine::new();
        let mut handles = vec![];

        for i in 0..10 {
            let engine = engine.clone();
            handles.push(thread::spawn(move || {
                engine
                    .set(
                        format!("key{i}").into_bytes(),
                        format!("value{i}").into_bytes(),
                    )
                    .unwrap();
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        for i in 0..10 {
            assert_eq!(
                engine.get(format!("key{i}").as_bytes()).unwrap(),
                Some(format!("value{i}").into_bytes())
            );
        }
    }

    #[test]
    fn mutating_commands_are_appended_to_aof() {
        let path = tmp_aof_path("mutating");
        let (engine, writer) = engine_with_aof(&path);

        engine.set(b"a".to_vec(), b"1".to_vec()).unwrap();
        engine.del(b"a").unwrap();
        engine.flushdb().unwrap();
        drop(engine);
        drop(writer);

        let bytes = fs::read(&path).unwrap();
        let expected = concat!(
            "*3\r\n$3\r\nSET\r\n$1\r\na\r\n$1\r\n1\r\n",
            "*2\r\n$3\r\nDEL\r\n$1\r\na\r\n",
            "*1\r\n$7\r\nFLUSHDB\r\n",
        );
        assert_eq!(bytes, expected.as_bytes());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn read_only_commands_do_not_touch_aof() {
        let path = tmp_aof_path("readonly");
        let (engine, writer) = engine_with_aof(&path);

        engine.get(b"missing").unwrap();
        engine.exists(b"missing").unwrap();
        engine.dbsize().unwrap();
        drop(engine);
        drop(writer);

        let bytes = fs::read(&path).unwrap();
        assert!(bytes.is_empty());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn del_of_missing_key_is_not_logged() {
        let path = tmp_aof_path("del-missing");
        let (engine, writer) = engine_with_aof(&path);

        assert!(!engine.del(b"missing").unwrap());
        drop(engine);
        drop(writer);

        let bytes = fs::read(&path).unwrap();
        assert!(bytes.is_empty());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn expire_at_ms_sets_and_ttl_reports_remaining() {
        let engine = KvEngine::new();
        engine.set(b"k".to_vec(), b"v".to_vec()).unwrap();

        let now = current_epoch_ms();
        assert!(engine.expire_at_ms(b"k", now + 60_000).unwrap());

        match engine.ttl_ms(b"k").unwrap() {
            TtlStatus::Millis(ms) => assert!(ms > 0 && ms <= 60_000),
            other => panic!("expected Millis(..), got {other:?}"),
        }
    }

    #[test]
    fn expire_at_ms_returns_false_for_missing_key() {
        let engine = KvEngine::new();
        let now = current_epoch_ms();
        assert!(!engine.expire_at_ms(b"missing", now + 1000).unwrap());
    }

    #[test]
    fn expire_in_the_past_deletes_key_immediately() {
        let engine = KvEngine::new();
        engine.set(b"k".to_vec(), b"v".to_vec()).unwrap();

        let now = current_epoch_ms();
        assert!(engine.expire_at_ms(b"k", now - 1).unwrap());
        assert_eq!(engine.get(b"k").unwrap(), None);
        assert!(matches!(engine.ttl_ms(b"k").unwrap(), TtlStatus::Missing));
    }

    #[test]
    fn persist_strips_ttl_only_when_present() {
        let engine = KvEngine::new();
        engine.set(b"k".to_vec(), b"v".to_vec()).unwrap();
        assert!(!engine.persist(b"k").unwrap());

        let now = current_epoch_ms();
        engine.expire_at_ms(b"k", now + 10_000).unwrap();
        assert!(engine.persist(b"k").unwrap());
        assert!(matches!(engine.ttl_ms(b"k").unwrap(), TtlStatus::NoExpire));
        assert!(!engine.persist(b"k").unwrap());
    }

    #[test]
    fn ttl_status_for_missing_and_persistent_keys() {
        let engine = KvEngine::new();
        assert!(matches!(
            engine.ttl_ms(b"missing").unwrap(),
            TtlStatus::Missing
        ));
        engine.set(b"k".to_vec(), b"v".to_vec()).unwrap();
        assert!(matches!(engine.ttl_ms(b"k").unwrap(), TtlStatus::NoExpire));
    }

    #[test]
    fn lazy_expiration_drops_key_on_read() {
        let engine = KvEngine::new();
        engine.set(b"k".to_vec(), b"v".to_vec()).unwrap();
        let deadline = Instant::now() + Duration::from_millis(20);
        {
            let mut store = engine.store.write().unwrap();
            if let Some(entry) = store.get_mut(b"k".as_slice()) {
                entry.expire_at = Some(deadline);
            }
        }
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(engine.get(b"k").unwrap(), None);
        assert!(!engine.exists(b"k").unwrap());
        assert_eq!(engine.dbsize().unwrap(), 0);
    }

    #[test]
    fn set_overwrite_clears_existing_ttl() {
        let engine = KvEngine::new();
        engine.set(b"k".to_vec(), b"v".to_vec()).unwrap();
        let now = current_epoch_ms();
        engine.expire_at_ms(b"k", now + 60_000).unwrap();
        engine.set(b"k".to_vec(), b"v2".to_vec()).unwrap();
        assert!(matches!(engine.ttl_ms(b"k").unwrap(), TtlStatus::NoExpire));
    }

    #[test]
    fn incr_preserves_ttl() {
        let engine = KvEngine::new();
        engine.set(b"k".to_vec(), b"1".to_vec()).unwrap();
        let now = current_epoch_ms();
        engine.expire_at_ms(b"k", now + 60_000).unwrap();
        engine.incr_by(b"k".to_vec(), 5).unwrap();
        match engine.ttl_ms(b"k").unwrap() {
            TtlStatus::Millis(ms) => assert!(ms > 0),
            other => panic!("expected Millis(..), got {other:?}"),
        }
    }

    #[test]
    fn append_preserves_ttl() {
        let engine = KvEngine::new();
        engine.set(b"k".to_vec(), b"hi".to_vec()).unwrap();
        let now = current_epoch_ms();
        engine.expire_at_ms(b"k", now + 60_000).unwrap();
        engine.append(b"k".to_vec(), b"!".to_vec()).unwrap();
        match engine.ttl_ms(b"k").unwrap() {
            TtlStatus::Millis(ms) => assert!(ms > 0),
            other => panic!("expected Millis(..), got {other:?}"),
        }
    }

    #[test]
    fn expire_logs_pexpireat_record_to_aof() {
        let path = tmp_aof_path("expire");
        let (engine, writer) = engine_with_aof(&path);

        engine.set(b"k".to_vec(), b"v".to_vec()).unwrap();
        let abs_ms = current_epoch_ms() + 60_000;
        engine.expire_at_ms(b"k", abs_ms).unwrap();
        drop(engine);
        drop(writer);

        let bytes = fs::read(&path).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            text.contains("PEXPIREAT"),
            "missing PEXPIREAT record in {text:?}"
        );
        assert!(text.contains(&abs_ms.to_string()));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn persist_logs_persist_record_to_aof() {
        let path = tmp_aof_path("persist");
        let (engine, writer) = engine_with_aof(&path);

        engine.set(b"k".to_vec(), b"v".to_vec()).unwrap();
        let abs_ms = current_epoch_ms() + 60_000;
        engine.expire_at_ms(b"k", abs_ms).unwrap();
        assert!(engine.persist(b"k").unwrap());
        drop(engine);
        drop(writer);

        let bytes = fs::read(&path).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            text.contains("PERSIST"),
            "missing PERSIST record in {text:?}"
        );
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn sweep_removes_expired_entries_and_logs_del() {
        let path = tmp_aof_path("sweep");
        let (engine, writer) = engine_with_aof(&path);

        engine.set(b"k".to_vec(), b"v".to_vec()).unwrap();
        {
            let mut store = engine.store.write().unwrap();
            if let Some(entry) = store.get_mut(b"k".as_slice()) {
                entry.expire_at = Some(Instant::now() - Duration::from_millis(1));
            }
        }

        let stats = engine.sweep_expired(16).unwrap();
        assert_eq!(stats.evicted, 1);
        assert_eq!(engine.dbsize().unwrap(), 0);

        drop(engine);
        drop(writer);
        let bytes = fs::read(&path).unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("DEL"));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn sweep_respects_zero_sample() {
        let engine = KvEngine::new();
        let stats = engine.sweep_expired(0).unwrap();
        assert_eq!(stats.examined, 0);
        assert_eq!(stats.evicted, 0);
    }

    #[test]
    fn used_memory_starts_at_zero_and_grows_on_set() {
        let engine = KvEngine::new();
        assert_eq!(engine.used_memory(), 0);
        engine.set(b"k".to_vec(), b"v".to_vec()).unwrap();
        let after_set = engine.used_memory();
        assert!(after_set >= 2 + PER_ENTRY_OVERHEAD);
    }

    #[test]
    fn used_memory_adjusts_on_overwrite_and_delete() {
        let engine = KvEngine::new();
        engine.set(b"k".to_vec(), b"short".to_vec()).unwrap();
        let a = engine.used_memory();
        engine.set(b"k".to_vec(), vec![b'x'; 100]).unwrap();
        let b = engine.used_memory();
        assert!(b > a, "grow overwrite should increase used_memory");

        engine.del(b"k").unwrap();
        assert_eq!(engine.used_memory(), 0);
    }

    #[test]
    fn flushdb_resets_used_memory() {
        let engine = KvEngine::new();
        engine.set(b"a".to_vec(), b"1".to_vec()).unwrap();
        engine.set(b"b".to_vec(), b"2".to_vec()).unwrap();
        assert!(engine.used_memory() > 0);
        engine.flushdb().unwrap();
        assert_eq!(engine.used_memory(), 0);
    }

    #[test]
    fn memory_usage_returns_per_entry_bytes_or_none() {
        let engine = KvEngine::new();
        assert_eq!(engine.memory_usage(b"absent").unwrap(), None);
        engine.set(b"k".to_vec(), b"value".to_vec()).unwrap();
        let reported = engine.memory_usage(b"k").unwrap().unwrap();
        assert_eq!(reported, 1 + 5 + PER_ENTRY_OVERHEAD);
    }

    #[test]
    fn memory_usage_expires_key_lazily() {
        let engine = KvEngine::new();
        engine.set(b"k".to_vec(), b"v".to_vec()).unwrap();
        {
            let mut store = engine.store.write().unwrap();
            if let Some(entry) = store.get_mut(b"k".as_slice()) {
                entry.expire_at = Some(Instant::now() - Duration::from_millis(1));
            }
        }
        assert_eq!(engine.memory_usage(b"k").unwrap(), None);
        assert_eq!(engine.used_memory(), 0);
    }

    #[test]
    fn used_memory_tracks_mset_and_del_many() {
        let engine = KvEngine::new();
        engine
            .mset(vec![
                (b"a".to_vec(), b"1".to_vec()),
                (b"bb".to_vec(), b"22".to_vec()),
            ])
            .unwrap();
        assert_eq!(
            engine.used_memory(),
            (1 + 1 + PER_ENTRY_OVERHEAD) + (2 + 2 + PER_ENTRY_OVERHEAD),
        );
        let removed = engine
            .del_many(&[b"a".to_vec(), b"bb".to_vec(), b"missing".to_vec()])
            .unwrap();
        assert_eq!(removed, 2);
        assert_eq!(engine.used_memory(), 0);
    }

    #[test]
    fn noeviction_refuses_writes_past_max_memory() {
        let engine = KvEngine::new();
        engine
            .set_eviction_config(EvictionConfig {
                max_memory: PER_ENTRY_OVERHEAD + 8,
                policy: EvictionPolicy::NoEviction,
                samples: 5,
            })
            .unwrap();
        engine.set(b"k".to_vec(), b"12345".to_vec()).unwrap();
        // Writing anything now overshoots: expect OOM without touching the
        // existing key.
        let err = engine.set(b"x".to_vec(), b"y".to_vec()).unwrap_err();
        assert!(matches!(err, FerrumError::OutOfMemory));
        assert_eq!(engine.get(b"k").unwrap(), Some(b"12345".to_vec()));
    }

    #[test]
    fn allkeys_lru_evicts_least_recently_used_key() {
        let engine = KvEngine::new();
        // Room for exactly two entries.
        let max = 2 * (1 + 3 + PER_ENTRY_OVERHEAD);
        engine
            .set_eviction_config(EvictionConfig {
                max_memory: max,
                policy: EvictionPolicy::AllKeysLru,
                samples: 10,
            })
            .unwrap();

        engine.set(b"a".to_vec(), b"AAA".to_vec()).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        engine.set(b"b".to_vec(), b"BBB".to_vec()).unwrap();
        std::thread::sleep(Duration::from_millis(5));

        // Touch `a` so it becomes the newest.
        engine.get(b"a").unwrap();
        std::thread::sleep(Duration::from_millis(5));

        // Third insert must evict: `b` is now the least recently used.
        engine.set(b"c".to_vec(), b"CCC".to_vec()).unwrap();
        assert_eq!(
            engine.get(b"b").unwrap(),
            None,
            "b should have been evicted"
        );
        assert!(engine.exists(b"a").unwrap());
        assert!(engine.exists(b"c").unwrap());
    }

    #[test]
    fn volatile_lru_falls_back_to_oom_without_volatile_keys() {
        let engine = KvEngine::new();
        engine
            .set_eviction_config(EvictionConfig {
                max_memory: PER_ENTRY_OVERHEAD + 2,
                policy: EvictionPolicy::VolatileLru,
                samples: 5,
            })
            .unwrap();
        engine.set(b"k".to_vec(), b"v".to_vec()).unwrap();
        let err = engine.set(b"x".to_vec(), b"y".to_vec()).unwrap_err();
        assert!(matches!(err, FerrumError::OutOfMemory));
    }

    #[test]
    fn volatile_ttl_evicts_soonest_expiring() {
        let engine = KvEngine::new();
        let max = 2 * (1 + 1 + PER_ENTRY_OVERHEAD);
        engine
            .set_eviction_config(EvictionConfig {
                max_memory: max,
                policy: EvictionPolicy::VolatileTtl,
                samples: 10,
            })
            .unwrap();
        engine.set(b"a".to_vec(), b"1".to_vec()).unwrap();
        engine.set(b"b".to_vec(), b"2".to_vec()).unwrap();
        let now = current_epoch_ms();
        engine.expire_at_ms(b"a", now + 10_000).unwrap();
        engine.expire_at_ms(b"b", now + 60_000).unwrap();

        engine.set(b"c".to_vec(), b"3".to_vec()).unwrap();
        assert_eq!(
            engine.get(b"a").unwrap(),
            None,
            "shortest-TTL key should evict"
        );
        assert!(engine.exists(b"b").unwrap());
        assert!(engine.exists(b"c").unwrap());
    }

    #[test]
    fn allkeys_random_picks_some_victim_under_pressure() {
        let engine = KvEngine::new();
        let max = 2 * (1 + 1 + PER_ENTRY_OVERHEAD);
        engine
            .set_eviction_config(EvictionConfig {
                max_memory: max,
                policy: EvictionPolicy::AllKeysRandom,
                samples: 5,
            })
            .unwrap();
        engine.set(b"a".to_vec(), b"1".to_vec()).unwrap();
        engine.set(b"b".to_vec(), b"2".to_vec()).unwrap();
        engine.set(b"c".to_vec(), b"3".to_vec()).unwrap();
        // One of the three must have been evicted.
        let survivors = [b"a", b"b", b"c"]
            .iter()
            .filter(|k| engine.exists(k.as_slice()).unwrap())
            .count();
        assert_eq!(survivors, 2);
    }

    #[test]
    fn zero_max_memory_disables_enforcement() {
        let engine = KvEngine::new();
        for i in 0..100 {
            let k = format!("k{i}");
            engine.set(k.into_bytes(), vec![b'x'; 128]).unwrap();
        }
        assert_eq!(engine.dbsize().unwrap(), 100);
    }
}
