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
pub(crate) mod write_guard;

#[cfg(test)]
mod tests;

// Re-export the public ancillary types and the crate-visible time helper so
// existing paths (`storage::engine::SweepStats`, `::current_epoch_ms`, …)
// keep resolving for callers in `network`, `persistence`, and the
// integration tests.
pub use types::{SweepStats, TtlStatus};
pub(crate) use util::current_epoch_ms;

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{
    AtomicBool, AtomicI64, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering,
};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::error::FerrumError;
use crate::network::monitor::MonitorRegistry;
use crate::persistence::AofWriter;
use crate::storage::adaptive_climb::AdaptiveClimbState;
use crate::storage::engine::entry::ValueEntry;
use crate::storage::engine::util::{
    log_aof_result, rng_seed, sample_candidates, validate_key, validate_value, xorshift32,
};
use crate::storage::engine::write_guard::WriteGuard;
use crate::storage::eviction::{
    self, AdaptiveHybridState, EvictionConfig, EvictionPolicy, EvictionScope,
};
use crate::storage::sieve::SieveState;

/// Maximum allowed key size in bytes (64 KiB).
pub const KEY_MAX_BYTES: usize = 64 * 1024;

/// Maximum allowed value size in bytes (16 MiB).
pub const VALUE_MAX_BYTES: usize = 16 * 1024 * 1024;

/// Approximate per-entry overhead charged on top of `key.capacity() +
/// value.capacity()`.
///
/// Derived from the real in-memory layout instead of a hand-waved constant:
///
/// | Component | Bytes |
/// |---|---|
/// | `size_of::<ValueEntry>()` (Vec header + 2 × `Instant` + flags) | ~72 |
/// | `HashMap` bucket (`Vec` header + `ValueEntry`, amortised over a 7/8 load factor) | ~96 |
/// | malloc header / size-class rounding for the key and value allocations | ~32 |
///
/// Rounded up to 144. `Vec::capacity()` (not `len()`) is charged separately so
/// oversized allocations are reflected. This is still an *estimate* — the
/// allocator's exact size classes are unknowable here — but the previous value
/// of 48 under-reported real usage by ~5×, which made `maxmemory` a poor
/// safety bound.
///
/// Note: the stateful SIEVE / AdaptiveClimb indexes keep their own copy of every
/// tracked key. Those structures are only populated while their policy is
/// active (see `index_kind`), and their footprint is *not* included in this
/// per-entry constant; `INFO memory` exposes `used_memory_policy_indexes` so
/// operators can see the extra cost.
pub const PER_ENTRY_OVERHEAD: u64 = 144;

/// Approximate cost of one key tracked in a stateful eviction index
/// (`IndexMap` bucket + the cloned key's heap allocation).
pub(crate) const PER_INDEX_ENTRY_BYTES: u64 = 48;

/// Which stateful eviction index (if any) must be maintained.
///
/// The engine keeps SIEVE and AdaptiveClimb queues in sync with the keyspace so
/// a runtime policy switch is immediately consistent — but maintaining both on
/// every access is pure overhead for the other 12 policies. This enum is stored
/// in a lock-free atomic and consulted on the hot path so only the active index
/// (if any) is touched.
pub(crate) mod index_kind {
    pub(crate) const NONE: u8 = 0;
    pub(crate) const SIEVE_ALL: u8 = 1;
    pub(crate) const SIEVE_VOLATILE: u8 = 2;
    pub(crate) const SIEVE_S_ALL: u8 = 3;
    pub(crate) const SIEVE_S_VOLATILE: u8 = 4;
    pub(crate) const AC_ALL: u8 = 5;
    pub(crate) const AC_VOLATILE: u8 = 6;

    #[inline]
    pub(crate) fn is_sieve(kind: u8) -> bool {
        matches!(
            kind,
            SIEVE_ALL | SIEVE_VOLATILE | SIEVE_S_ALL | SIEVE_S_VOLATILE
        )
    }

    #[inline]
    pub(crate) fn is_sieve_ttl(kind: u8) -> bool {
        matches!(kind, SIEVE_S_ALL | SIEVE_S_VOLATILE)
    }

    #[inline]
    pub(crate) fn is_ac(kind: u8) -> bool {
        matches!(kind, AC_ALL | AC_VOLATILE)
    }
}

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
    /// Slow-log tuning: a command is recorded only when its execution time
    /// (microseconds) strictly exceeds `slowlog_slower_than_us`.
    ///
    /// `-1` means "log everything"; `0` disables logging. Held as a
    /// `Lock`-free atomic so the per-command hot path can read it without
    /// touching any engine lock. Default 10_000µs (10ms), matching
    /// Redis' `slowlog-log-slower-than`.
    pub(crate) slowlog_slower_than_us: Arc<AtomicI64>,
    /// Maximum number of entries retained in the slow-log ring. When the
    /// ring overflows, the oldest entry is dropped. Default 128, matching
    /// Redis' `slowlog-max-len`. Lock-free atomic for the same reason as
    /// the threshold above.
    pub(crate) slowlog_max_len: Arc<AtomicU64>,
    /// The global slow-log ring. Guarded by a `Mutex` because entries are
    /// appended from many concurrent connection tasks; a `Mutex` is ample
    /// since only genuinely-slow commands ever touch it.
    pub(crate) slowlog: Arc<Mutex<VecDeque<SlowLogEntry>>>,
    /// Monotonic source for slow-log entry ids, so every recorded command
    /// gets a stable, ever-increasing identifier regardless of which
    /// connection logged it.
    pub(crate) slowlog_seq: Arc<AtomicU64>,
    /// Cumulative commands processed (all commands, all connections).
    /// Exposed as `total_commands_processed` in `INFO stats`.
    pub(crate) total_commands: Arc<AtomicU64>,
    /// Cumulative keys evicted by the maxmemory eviction policy.
    /// Exposed as `evicted_keys` in `INFO stats`.
    pub(crate) evicted_keys: Arc<AtomicU64>,
    /// Cumulative keys removed by TTL expiration (active + lazy sweeps).
    /// Exposed as `expired_keys` in `INFO stats`.
    pub(crate) expired_keys: Arc<AtomicU64>,
    /// Registry of active MONITOR clients. Shared via `Arc` so every
    /// engine clone broadcasts to the same set of subscribers.
    pub(crate) monitor_registry: MonitorRegistry,
    /// Which stateful eviction index is currently active, as an
    /// [`index_kind`] constant. Consulted lock-free on every access so the
    /// inactive index is never touched (previously both were maintained
    /// unconditionally, which made every read O(n) under AdaptiveClimb).
    pub(crate) index_kind: Arc<AtomicU8>,
    /// `true` when a `requirepass` password is configured. Read once per
    /// command instead of taking the password `RwLock`.
    pub(crate) auth_required: Arc<AtomicBool>,
    /// Cumulative AOF append failures. Surfaced via
    /// `INFO stats:aof_write_errors` so a failing disk is visible instead of
    /// only appearing in the log.
    pub(crate) aof_errors: Arc<AtomicU64>,
    /// Rotating start offset for the active-expiry sweep. Without it the
    /// sweeper kept sampling the same `HashMap` bucket prefix forever, so
    /// expired keys outside that prefix were never collected.
    pub(crate) sweep_cursor: Arc<AtomicUsize>,
}

/// One recorded slow command.
///
/// Mirrors Redis' slow-log entry shape: a monotonic `id`, the execution
/// `duration_us`, the reconstructed `args` (so operators see exactly what
/// was sent), the client `peer` address, and the server `ts_secs`. The
/// client name is intentionally omitted — FerrumKV has no `CLIENT
/// SETNAME` yet (YAGNI for v0.5).
#[derive(Clone)]
pub(crate) struct SlowLogEntry {
    pub(crate) id: u64,
    pub(crate) ts_secs: u64,
    pub(crate) duration_us: u64,
    pub(crate) args: Vec<Vec<u8>>,
    pub(crate) peer: SocketAddr,
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
            // Slow-log defaults mirror Redis: 10ms threshold, 128-entry cap.
            slowlog_slower_than_us: Arc::new(AtomicI64::new(10_000)),
            slowlog_max_len: Arc::new(AtomicU64::new(128)),
            slowlog: Arc::new(Mutex::new(VecDeque::new())),
            slowlog_seq: Arc::new(AtomicU64::new(0)),
            total_commands: Arc::new(AtomicU64::new(0)),
            evicted_keys: Arc::new(AtomicU64::new(0)),
            expired_keys: Arc::new(AtomicU64::new(0)),
            monitor_registry: MonitorRegistry::default(),
            index_kind: Arc::new(AtomicU8::new(index_kind::NONE)),
            auth_required: Arc::new(AtomicBool::new(false)),
            aof_errors: Arc::new(AtomicU64::new(0)),
            sweep_cursor: Arc::new(AtomicUsize::new(0)),
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
        // Only one stateful index is maintained at a time. Rebuilding is a
        // full keyspace pass, so it is done *only* when the index class
        // actually changes: a plain `CONFIG SET maxmemory` must not wipe the
        // SIEVE `visited` bits / AdaptiveClimb `jump` state.
        let new_kind = cfg.policy.index_kind();
        let old_kind = self.index_kind.load(Ordering::Acquire);
        if new_kind != old_kind {
            // Holding the store read lock blocks writers for the duration of
            // the transition, so no write can slip in between "index rebuilt"
            // and "kind published" (or observe the old kind while the index is
            // being cleared).
            let store = self.store.read()?;
            if index_kind::is_sieve(new_kind) {
                let mut sieve = self.sieve.lock()?;
                sieve.rebuild(&store, cfg.policy.scope());
                let mut ac = self.ac.lock()?;
                ac.clear();
            } else if index_kind::is_ac(new_kind) {
                let mut ac = self.ac.lock()?;
                ac.rebuild(&store, cfg.policy.scope());
                let mut sieve = self.sieve.lock()?;
                sieve.clear();
            } else {
                if let Ok(mut sieve) = self.sieve.lock() {
                    sieve.clear();
                }
                if let Ok(mut ac) = self.ac.lock() {
                    ac.clear();
                }
            }
            self.index_kind.store(new_kind, Ordering::Release);
            drop(store);
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
        // Redis semantics: an empty password disables authentication, it does
        // not mean "require the empty password".
        let pw = pw.filter(|p| !p.is_empty());
        let required = pw.is_some();
        let mut guard = self.auth_password.write()?;
        *guard = pw;
        // Publish the hot-path flag while still holding the lock so a
        // connection can never observe "password set" before the password
        // itself is visible (or vice versa).
        self.auth_required.store(required, Ordering::Release);
        Ok(())
    }

    /// `true` when a password is configured. Lock-free: the connection hot
    /// path calls this for every command.
    #[inline]
    pub fn auth_required(&self) -> bool {
        self.auth_required.load(Ordering::Acquire)
    }

    /// Returns a copy of the current `requirepass` password, if any.
    pub fn requirepass(&self) -> Result<Option<Vec<u8>>, FerrumError> {
        Ok(self.auth_password.read()?.clone())
    }

    // === Slow-log (F-03) ====================================================
    //
    // A bounded, global ring of the server's slowest commands. The tunables
    // (`slowlog_slower_than_us`, `slowlog_max_len`) are lock-free atomics so
    // the per-command hot path reads them without touching any engine lock.

    /// True when slow-logging is configured at all (`threshold != 0`).
    ///
    /// Sampled *before* a command runs so that the command which enables
    /// logging (`CONFIG SET slowlog-log-slower-than -1`) is not itself
    /// recorded — matching the historical behaviour and, more importantly,
    /// keeping the ring contents predictable for operators.
    pub(crate) fn slowlog_armed(&self) -> bool {
        self.slowlog_slower_than_us.load(Ordering::Relaxed) != 0
    }

    /// True when a command of `duration_us` should be recorded.
    ///
    /// Lets the dispatcher decide *before* serialising the argv for the
    /// slow-log, so a fast command never pays the snapshot cost.
    pub(crate) fn slowlog_would_log(&self, duration_us: u64) -> bool {
        match self.slowlog_slower_than_us.load(Ordering::Relaxed) {
            0 => false,
            n if n < 0 => true,
            n => duration_us > n as u64,
        }
    }

    /// Records `command_args` as a slow entry when its `duration_us` crosses
    /// the configured threshold. A no-op (and no lock taken) when logging is
    /// disabled, or when the command is fast enough.
    pub(crate) fn maybe_push_slowlog(
        &self,
        command_args: Vec<Vec<u8>>,
        peer: Option<SocketAddr>,
        duration_us: u64,
    ) {
        let threshold = self.slowlog_slower_than_us.load(Ordering::Relaxed);
        // `0` disables logging; `> 0` requires *strictly* exceeding the
        // threshold (matching Redis); `< 0` logs everything.
        let log_it = match threshold {
            0 => false,
            n if n < 0 => true,
            n => duration_us > n as u64,
        };
        if !log_it {
            return;
        }
        let id = self.slowlog_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let ts_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let entry = SlowLogEntry {
            id,
            ts_secs,
            duration_us,
            args: command_args,
            peer: peer.unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 0))),
        };
        let max_len = self.slowlog_max_len.load(Ordering::Relaxed) as usize;
        let mut ring = match self.slowlog.lock() {
            Ok(g) => g,
            // Poisoned: drop this sample rather than panic the connection.
            Err(_) => return,
        };
        ring.push_back(entry);
        // Trim from the front (oldest) down to the cap.
        while ring.len() > max_len.max(1) {
            ring.pop_front();
        }
    }

    /// Returns up to `count` slow-log entries, newest-first. `None` returns
    /// the whole ring (which the encoder will serialise in the same order).
    pub(crate) fn slowlog_get(&self, count: Option<usize>) -> Vec<SlowLogEntry> {
        let ring = match self.slowlog.lock() {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };
        let mut entries: Vec<SlowLogEntry> = ring.iter().rev().cloned().collect();
        if let Some(n) = count {
            entries.truncate(n);
        }
        entries
    }

    /// Number of entries currently retained in the slow-log ring.
    pub(crate) fn slowlog_len(&self) -> usize {
        self.slowlog.lock().map(|g| g.len()).unwrap_or(0)
    }

    /// Clears the slow-log ring.
    pub(crate) fn slowlog_reset(&self) {
        if let Ok(mut g) = self.slowlog.lock() {
            g.clear();
        }
    }

    /// Slow-log tunables surfaced to `CONFIG GET/SET`.
    pub(crate) fn slowlog_threshold_us(&self) -> i64 {
        self.slowlog_slower_than_us.load(Ordering::Relaxed)
    }
    pub(crate) fn slowlog_max_len(&self) -> u64 {
        self.slowlog_max_len.load(Ordering::Relaxed)
    }
    pub(crate) fn set_slowlog_threshold_us(&self, v: i64) {
        self.slowlog_slower_than_us.store(v, Ordering::Relaxed);
    }
    pub(crate) fn set_slowlog_max_len(&self, v: u64) {
        self.slowlog_max_len.store(v, Ordering::Relaxed);
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
            // Constant-time compare: a plain `==` lets a remote attacker learn
            // the correct prefix length from response timing.
            Some(expected) => Ok(util::constant_time_eq(provided, expected)),
            None => Err(FerrumError::ParseError(
                "Client sent AUTH, but no password is set".into(),
            )),
        }
    }

    /// Records an AOF append outcome, counting failures for `INFO`.
    ///
    /// Persistence stays best-effort (the in-memory state is authoritative and
    /// a failed append does not fail the client command), but the error is now
    /// observable via `INFO stats:aof_write_errors` instead of only appearing
    /// in the log.
    pub(crate) fn note_aof_result(&self, cmd: &str, result: Result<(), FerrumError>) {
        if result.is_err() {
            self.aof_errors.fetch_add(1, Ordering::Relaxed);
        }
        log_aof_result(cmd, result);
    }

    /// Cumulative number of failed AOF appends.
    pub fn aof_write_errors(&self) -> u64 {
        self.aof_errors.load(Ordering::Relaxed)
    }

    /// Number of fsync barriers issued by the AOF writer (0 when disabled).
    pub fn aof_fsync_count(&self) -> u64 {
        self.aof.as_ref().map(|w| w.fsync_count()).unwrap_or(0)
    }

    /// Approximate bytes held by the active stateful eviction index.
    ///
    /// SIEVE / AdaptiveClimb keep a second copy of every tracked key. Those
    /// bytes are deliberately *not* part of [`Self::used_memory`] (they are
    /// policy-dependent), but operators need to see them because `maxmemory`
    /// only accounts for the keyspace. `INFO memory` reports this as
    /// `used_memory_policy_indexes`.
    pub fn policy_index_memory(&self) -> u64 {
        if self.index_kind.load(Ordering::Relaxed) == index_kind::NONE {
            return 0;
        }
        match self.store.read() {
            Ok(store) => store.len() as u64 * PER_INDEX_ENTRY_BYTES,
            Err(_) => 0,
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
        let mut g = WriteGuard::begin(self)?;
        g.insert(key, value)
    }

    /// Records a (re)insertion in auxiliary eviction structures.
    ///
    /// Kept separate so the public command methods stay terse; callers must
    /// already hold the write lock or own the engine reference. Both helpers
    /// are gated on the lock-free [`Self::index_kind`] so the 14 policies that
    /// do not use a stateful index pay nothing for it (previously every write
    /// cloned the key into two `IndexMap`s regardless of the active policy).
    #[inline]
    fn sieve_notify_insert(&self, key: &[u8], has_ttl: bool) {
        if !index_kind::is_sieve(self.index_kind.load(Ordering::Relaxed)) {
            return;
        }
        if let Ok(mut s) = self.sieve.lock() {
            s.observe_insert(key, has_ttl);
        }
    }

    /// Mirrors a key access into the SIEVE `visited` bit.
    #[inline]
    fn sieve_notify_access(&self, key: &[u8]) {
        if !index_kind::is_sieve(self.index_kind.load(Ordering::Relaxed)) {
            return;
        }
        if let Ok(mut s) = self.sieve.lock() {
            s.observe_access(key);
        }
    }

    /// Mirrors a key removal from the SIEVE FIFO queue.
    #[inline]
    fn sieve_notify_remove(&self, key: &[u8]) {
        if !index_kind::is_sieve(self.index_kind.load(Ordering::Relaxed)) {
            return;
        }
        if let Ok(mut s) = self.sieve.lock() {
            s.observe_remove(key);
        }
    }

    /// Mirrors a (re)insertion into the AdaptiveClimb ordered list.
    #[inline]
    fn ac_notify_insert(&self, key: &[u8], has_ttl: bool) {
        if !index_kind::is_ac(self.index_kind.load(Ordering::Relaxed)) {
            return;
        }
        if let Ok(mut s) = self.ac.lock() {
            s.observe_insert(key, has_ttl);
        }
    }

    /// Mirrors a hit into the AdaptiveClimb promotion rule.
    #[inline]
    fn ac_notify_access(&self, key: &[u8]) {
        if !index_kind::is_ac(self.index_kind.load(Ordering::Relaxed)) {
            return;
        }
        if let Ok(mut s) = self.ac.lock() {
            s.observe_access(key);
        }
    }

    /// Mirrors a removal from the AdaptiveClimb ordered list.
    #[inline]
    fn ac_notify_remove(&self, key: &[u8]) {
        if !index_kind::is_ac(self.index_kind.load(Ordering::Relaxed)) {
            return;
        }
        if let Ok(mut s) = self.ac.lock() {
            s.observe_remove(key);
        }
    }

    /// Sets `key` to `value` only if the key is not already present.
    ///
    /// Returns `true` when the insert happened and `false` when the key was
    /// already set. The AOF records a `SET` only on a successful insert,
    /// mirroring the Redis semantics of `SETNX`.
    pub fn set_nx(&self, key: Vec<u8>, value: Vec<u8>) -> Result<bool, FerrumError> {
        let mut g = WriteGuard::begin(self)?;
        g.insert_nx(key, value)
    }

    /// Sets every `(key, value)` pair in `pairs` atomically.
    ///
    /// Each pair is validated before any mutation happens, so either the
    /// whole batch is applied or none of it is. The AOF log records the
    /// batch in a single write so concurrent appenders never observe a
    /// half-committed MSET.
    pub fn mset(&self, pairs: Vec<(Vec<u8>, Vec<u8>)>) -> Result<(), FerrumError> {
        let mut g = WriteGuard::begin(self)?;
        g.insert_many(pairs)
    }

    /// Returns the stored value for every key in `keys`, preserving order.
    ///
    /// Missing keys map to `None` so the caller can serialise them as
    /// null bulk strings without ambiguity. Expired entries are dropped in
    /// the same pass so the result reflects live state only.
    pub fn mget(&self, keys: &[Vec<u8>]) -> Result<Vec<Option<Vec<u8>>>, FerrumError> {
        let mut g = WriteGuard::begin(self)?;
        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            out.push(g.fetch_live(key));
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
        let mut g = WriteGuard::begin(self)?;
        let (current, existing_deadline) = match g.read_state(&key) {
            Some((data, ttl)) => {
                let text = std::str::from_utf8(&data).map_err(|_| {
                    FerrumError::ParseError("value is not an integer or out of range".into())
                })?;
                let n = text.parse::<i64>().map_err(|_| {
                    FerrumError::ParseError("value is not an integer or out of range".into())
                })?;
                (n, ttl)
            }
            None => (0i64, None),
        };

        let new_value = current.checked_add(delta).ok_or_else(|| {
            FerrumError::ParseError("value is not an integer or out of range".into())
        })?;
        let serialised = new_value.to_string().into_bytes();
        g.insert_keep_ttl(key, serialised, existing_deadline)?;
        Ok(new_value)
    }

    /// Atomically adds `delta` to the floating-point value at `key` and
    /// returns the new value, matching Redis' `INCRBYFLOAT` semantics.
    ///
    /// A missing key is treated as starting from zero. The existing value,
    /// if any, must be a decimal ASCII float that parses into an [`f64`];
    /// values that fail to parse produce the Redis-standard reply
    /// `-ERR value is not a valid float`.
    ///
    /// The key's existing TTL, if any, is preserved.
    pub fn incr_by_float(&self, key: Vec<u8>, delta: f64) -> Result<f64, FerrumError> {
        validate_key(&key)?;
        let mut g = WriteGuard::begin(self)?;
        let (current, existing_deadline) = match g.read_state(&key) {
            Some((data, ttl)) => {
                let text = std::str::from_utf8(&data)
                    .map_err(|_| FerrumError::ParseError("value is not a valid float".into()))?;
                let n = text
                    .parse::<f64>()
                    .map_err(|_| FerrumError::ParseError("value is not a valid float".into()))?;
                (n, ttl)
            }
            None => (0f64, None),
        };

        let new_value = current + delta;
        // Reject non-finite results the same way Redis does (the sum
        // overflowed to infinity / NaN).
        if !new_value.is_finite() {
            return Err(FerrumError::ParseError(
                "increment would produce NaN or Infinity".into(),
            ));
        }
        let serialised = new_value.to_string().into_bytes();
        g.insert_keep_ttl(key, serialised, existing_deadline)?;
        Ok(new_value)
    }

    /// Returns the value for `key`, or `None` if the key does not exist.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, FerrumError> {
        let mut g = WriteGuard::begin(self)?;
        Ok(g.fetch_live(key))
    }

    /// Deletes `key` and returns `true` if it existed.
    pub fn del(&self, key: &[u8]) -> Result<bool, FerrumError> {
        let mut g = WriteGuard::begin(self)?;
        Ok(g.remove(key))
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
        let mut g = WriteGuard::begin(self)?;
        Ok(g.remove_many(keys))
    }

    /// Returns `true` if `key` exists and has not expired.
    pub fn exists(&self, key: &[u8]) -> Result<bool, FerrumError> {
        let mut g = WriteGuard::begin(self)?;
        Ok(g.contains_live(key))
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
        let mut g = WriteGuard::begin(self)?;
        let mut count = 0usize;
        for key in keys {
            if g.contains_live(key) {
                count += 1;
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
        let mut g = WriteGuard::begin(self)?;
        let (new_value, existing_deadline) = match g.read_state(&key) {
            Some((data, ttl)) => {
                let mut buf = Vec::with_capacity(data.len() + suffix.len());
                buf.extend_from_slice(&data);
                buf.extend_from_slice(&suffix);
                (buf, ttl)
            }
            None => (suffix, None),
        };
        validate_value(&new_value)?;

        let new_len = new_value.len();
        g.insert_keep_ttl(key, new_value, existing_deadline)?;
        Ok(new_len)
    }

    /// Returns the byte length of the value at `key`, or `0` if absent.
    pub fn strlen(&self, key: &[u8]) -> Result<usize, FerrumError> {
        let mut g = WriteGuard::begin(self)?;
        Ok(g.strlen_of(key))
    }

    /// Removes all keys from the store.
    pub fn flushdb(&self) -> Result<(), FerrumError> {
        let mut g = WriteGuard::begin(self)?;
        g.clear()
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
        let mut g = WriteGuard::begin(self)?;
        g.expire_at(key, abs_epoch_ms)
    }

    /// Removes any TTL from `key`.
    ///
    /// Returns `true` only when the key existed **and** had a TTL before
    /// the call — matching Redis' `PERSIST` return semantics.
    pub fn persist(&self, key: &[u8]) -> Result<bool, FerrumError> {
        let mut g = WriteGuard::begin(self)?;
        g.persist(key)
    }

    /// Triggers a background AOF rewrite (the `BGREWRITEAOF` command).
    ///
    /// Returns `+OK` to the caller immediately; the heavy compaction runs on a
    /// dedicated `ferrum-aof-rewrite` OS thread so the server never pauses. If
    /// AOF is not enabled the caller is told so. A rewrite already in progress
    /// is a no-op (Redis returns `+OK` and ignores the second request), which
    /// keeps the delta-buffer invariant intact.
    pub fn rewrite_aof(&self) -> Result<(), FerrumError> {
        let Some(writer) = self.aof.as_ref() else {
            return Err(FerrumError::AofNotEnabled);
        };
        // Claim the rewrite slot *before* spawning. Previously the check and
        // the flag update were separated by the thread spawn, so two
        // concurrent BGREWRITEAOF requests could both pass the guard and
        // spawn two workers racing on the same temp file.
        if !writer.try_begin_rewrite() {
            // A rewrite is already in flight: Redis replies +OK and ignores
            // the second request, which keeps the delta invariant intact.
            return Ok(());
        }
        let writer = Arc::clone(writer);
        let engine = self.clone();
        match std::thread::Builder::new()
            .name("ferrum-aof-rewrite".to_string())
            .spawn(move || perform_aof_rewrite(engine, writer))
        {
            Ok(_) => Ok(()),
            Err(e) => {
                // The slot was claimed but no worker exists: release it so a
                // later attempt can succeed.
                if let Some(w) = self.aof.as_ref() {
                    w.abort_rewrite();
                }
                Err(FerrumError::Internal(format!(
                    "failed to spawn aof rewrite thread: {e}"
                )))
            }
        }
    }

    /// Returns the remaining TTL for `key` in milliseconds.
    ///
    /// * `Ok(TtlStatus::Missing)` — key does not exist (Redis reports `-2`).
    /// * `Ok(TtlStatus::NoExpire)` — key exists without a TTL (Redis `-1`).
    /// * `Ok(TtlStatus::Millis(n))` — `n` milliseconds remaining.
    pub fn ttl_ms(&self, key: &[u8]) -> Result<TtlStatus, FerrumError> {
        let mut g = WriteGuard::begin(self)?;
        Ok(g.ttl_of(key))
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
    ///
    /// Sampling starts at a rotating cursor rather than at the map's first
    /// bucket. `HashMap` iteration order is stable between mutations, so
    /// always taking the first `sample` keys meant keys outside that prefix
    /// were never actively expired. The cursor advances by the number of keys
    /// examined, so the whole keyspace is covered after `len / sample` ticks.
    /// The `skip` costs O(cursor) pointer steps (a few milliseconds for a
    /// million keys, every sweep interval) — the price of guaranteed coverage
    /// without maintaining a second TTL index.
    pub fn sweep_expired(&self, sample: usize) -> Result<SweepStats, FerrumError> {
        if sample == 0 {
            return Ok(SweepStats {
                examined: 0,
                evicted: 0,
            });
        }
        let mut store = self.store.write()?;
        let now = Instant::now();

        let len = store.len();
        let start = if len == 0 {
            0
        } else {
            self.sweep_cursor.load(Ordering::Relaxed) % len
        };

        let mut victims: Vec<Vec<u8>> = Vec::new();
        let mut examined = 0usize;
        for (key, entry) in store.iter().skip(start).chain(store.iter().take(start)) {
            if examined >= sample {
                break;
            }
            examined += 1;
            if entry.is_expired(now) {
                victims.push(key.clone());
            }
        }
        if len > 0 {
            self.sweep_cursor
                .store(start.wrapping_add(examined) % len, Ordering::Relaxed);
        }

        let evicted = victims.len();
        for key in &victims {
            self.track_remove(&mut store, key.as_slice());
            self.expired_keys.fetch_add(1, Ordering::Relaxed);
            if let Some(aof) = &self.aof {
                self.note_aof_result("DEL", aof.append_del(key));
            }
        }

        Ok(SweepStats { examined, evicted })
    }

    /// Logs a DEL record caused by lazy expiration.
    ///
    /// Kept separate from `del()` so call sites stay terse; callers must
    /// already hold the write lock.
    pub(crate) fn log_expire_drop(&self, key: &[u8]) {
        if let Some(aof) = &self.aof {
            self.note_aof_result("DEL", aof.append_del(key));
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
        let mut g = WriteGuard::begin(self)?;
        g.memory_of(key)
    }

    /// Inserts `entry` into `store` and updates the memory counter,
    /// returning the old entry (if any) so callers can reason about
    /// overwrite semantics.
    pub(crate) fn track_insert(
        &self,
        store: &mut HashMap<Vec<u8>, ValueEntry>,
        key: Vec<u8>,
        entry: ValueEntry,
    ) -> Option<ValueEntry> {
        let incoming = util::entry_bytes(&key, &entry);
        let has_ttl = entry.expire_at.is_some();
        let previous = store.insert(key.clone(), entry);
        let outgoing = previous
            .as_ref()
            .map(|p| util::entry_bytes(&key, p))
            .unwrap_or(0);
        self.apply_delta(incoming as i64 - outgoing as i64);
        self.sieve_notify_insert(&key, has_ttl);
        self.ac_notify_insert(&key, has_ttl);
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
                    let ttl_aware = index_kind::is_sieve_ttl(cfg.policy.index_kind());
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
            self.evicted_keys.fetch_add(1, Ordering::Relaxed);
            if let Some(aof) = &self.aof {
                self.note_aof_result("DEL", aof.append_del(&victim_key));
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
    pub(crate) fn touch_access(&self, store: &mut HashMap<Vec<u8>, ValueEntry>, key: &[u8]) {
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
    pub(crate) fn enforce_for_write(
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
    pub(crate) fn track_remove(
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
    pub(crate) fn track_clear(&self, store: &mut HashMap<Vec<u8>, ValueEntry>) {
        store.clear();
        // `FLUSHDB` is rare, so clearing both indexes unconditionally is
        // cheaper than branching on the active policy kind.
        if let Ok(mut sieve) = self.sieve.lock() {
            sieve.clear();
        }
        if let Ok(mut ac) = self.ac.lock() {
            ac.clear();
        }
        self.used_memory.store(0, Ordering::Relaxed);
    }

    /// Untracks an entry that was already removed by some other path
    /// (e.g. a caller that held onto the returned `Option<ValueEntry>`).
    pub(crate) fn untrack(&self, key: &[u8], entry: &ValueEntry) {
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

    /// Total commands processed (all commands, all connections).
    pub fn total_commands_processed(&self) -> u64 {
        self.total_commands.load(Ordering::Relaxed)
    }

    /// Cumulative evictions by the maxmemory policy.
    pub fn evicted_keys(&self) -> u64 {
        self.evicted_keys.load(Ordering::Relaxed)
    }

    /// Cumulative keys removed by TTL expiration.
    pub fn expired_keys(&self) -> u64 {
        self.expired_keys.load(Ordering::Relaxed)
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

/// Background AOF rewrite worker.
///
/// Snapshots the live keyspace, serialises a compact final-state AOF to a temp
/// file, then atomically swaps it in and replays any writes that landed during
/// the rewrite (buffered in [`AofWriter`]'s delta). Runs on the dedicated
/// `ferrum-aof-rewrite` OS thread; it never blocks the connection that issued
/// `BGREWRITEAOF`.
fn perform_aof_rewrite(engine: KvEngine, writer: Arc<AofWriter>) {
    // The caller has already claimed the slot via `try_begin_rewrite`.

    // Snapshot live entries: (key, value, optional absolute-TTL epoch ms).
    // Holding only the read lock keeps the writer pause brief.
    let snapshot: Vec<(Vec<u8>, Vec<u8>, Option<i64>)> = {
        let store = match engine.store.read() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let now = Instant::now();
        let mut out = Vec::with_capacity(store.len());
        for (k, entry) in store.iter() {
            if entry.is_expired(now) {
                continue;
            }
            let ttl = entry
                .expire_at
                .and_then(|d| util::deadline_to_epoch_ms(d, now));
            out.push((k.clone(), entry.data.clone(), ttl));
        }
        out
    };

    // The rewrite may have been aborted (delta overflow) while we held the
    // read lock; the old AOF is intact, so there is nothing left to do.
    if !writer.is_rewriting() {
        return;
    }

    let temp = writer.rewrite_temp_path();
    if let Err(e) = serialise_compact_aof(&temp, &snapshot) {
        log::warn!("aof rewrite aborted: {e}");
        let _ = std::fs::remove_file(&temp);
        writer.abort_rewrite();
        return;
    }

    if let Err(e) = writer.finish_rewrite(&temp) {
        log::warn!("aof rewrite finish failed: {e}");
        let _ = std::fs::remove_file(&temp);
    }
}

/// Serialises the compact final-state AOF for `snapshot` into `temp`.
///
/// Each live key becomes exactly one `SET`, plus a `PEXPIREAT` when it carries
/// a TTL. The bytes are identical to the wire protocol (方案 A：记最终态), so
/// the existing `replay` loader consumes them unchanged.
fn serialise_compact_aof(
    temp: &Path,
    snapshot: &[(Vec<u8>, Vec<u8>, Option<i64>)],
) -> Result<(), FerrumError> {
    use std::io::{BufWriter, Write};

    let f = std::fs::File::create(temp).map_err(|e| {
        FerrumError::PersistenceError(format!("aof rewrite create temp failed: {e}"))
    })?;
    // Stream through a buffered writer instead of materialising the whole
    // compact file in one `Vec`. The previous approach held a second full copy
    // of the dataset in memory, so a rewrite needed roughly 3× the live
    // keyspace (snapshot + serialised buffer + file cache).
    let mut w = BufWriter::with_capacity(64 * 1024, f);
    for (k, v, ttl) in snapshot {
        w.write_all(&crate::persistence::resp::encode_command(&[
            b"SET",
            k.as_slice(),
            v.as_slice(),
        ]))
        .map_err(|e| {
            FerrumError::PersistenceError(format!("aof rewrite write temp failed: {e}"))
        })?;
        if let Some(abs_ms) = ttl {
            let ts = abs_ms.to_string();
            w.write_all(&crate::persistence::resp::encode_command(&[
                b"PEXPIREAT",
                k.as_slice(),
                ts.as_bytes(),
            ]))
            .map_err(|e| {
                FerrumError::PersistenceError(format!("aof rewrite write temp failed: {e}"))
            })?;
        }
    }
    w.flush().map_err(|e| {
        FerrumError::PersistenceError(format!("aof rewrite flush temp failed: {e}"))
    })?;
    w.get_ref().sync_all().map_err(|e| {
        FerrumError::PersistenceError(format!("aof rewrite fsync temp failed: {e}"))
    })?;
    Ok(())
}

#[cfg(test)]
mod slowlog_tests {
    use super::*;
    use std::net::SocketAddr;

    /// A deterministic `SocketAddr` for tests (loopback:6391).
    fn peer() -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], 6391))
    }

    /// Builds a `SET k v` arg vector as the slow-log records it.
    fn set_args() -> Vec<Vec<u8>> {
        vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]
    }

    #[test]
    fn disabled_threshold_logs_nothing() {
        let e = KvEngine::new();
        e.set_slowlog_threshold_us(0);
        e.maybe_push_slowlog(set_args(), Some(peer()), 1_000_000);
        assert_eq!(e.slowlog_len(), 0);
    }

    #[test]
    fn equal_to_threshold_is_not_logged() {
        let e = KvEngine::new();
        e.set_slowlog_threshold_us(10_000);
        e.maybe_push_slowlog(set_args(), Some(peer()), 10_000);
        assert_eq!(e.slowlog_len(), 0, "exactly-at-threshold must not log");
    }

    #[test]
    fn strictly_exceeding_threshold_is_logged() {
        let e = KvEngine::new();
        e.set_slowlog_threshold_us(10_000);
        e.maybe_push_slowlog(set_args(), Some(peer()), 10_001);
        assert_eq!(e.slowlog_len(), 1);
    }

    #[test]
    fn negative_threshold_logs_everything() {
        let e = KvEngine::new();
        e.set_slowlog_threshold_us(-1);
        // duration 0 is still logged when threshold is forced on.
        e.maybe_push_slowlog(set_args(), None, 0);
        assert_eq!(e.slowlog_len(), 1);
    }

    #[test]
    fn ids_are_monotonic_and_newest_first() {
        let e = KvEngine::new();
        e.set_slowlog_threshold_us(-1);
        for _ in 0..3 {
            e.maybe_push_slowlog(set_args(), Some(peer()), 0);
        }
        let entries = e.slowlog_get(None);
        assert_eq!(entries.len(), 3);
        // Newest-first: the last push (id 3) comes first.
        assert_eq!(entries[0].id, 3);
        assert_eq!(entries[1].id, 2);
        assert_eq!(entries[2].id, 1);
    }

    #[test]
    fn ring_trims_to_max_len_dropping_oldest() {
        let e = KvEngine::new();
        e.set_slowlog_threshold_us(-1);
        e.set_slowlog_max_len(2);
        for _ in 0..5 {
            e.maybe_push_slowlog(set_args(), Some(peer()), 0);
        }
        assert_eq!(e.slowlog_len(), 2, "ring must cap at max_len");
        // Newest-first: surviving ids are the last two pushes (4, 5).
        let entries = e.slowlog_get(None);
        assert_eq!(entries[0].id, 5);
        assert_eq!(entries[1].id, 4);
    }

    #[test]
    fn get_returns_newest_first_and_respects_count() {
        let e = KvEngine::new();
        e.set_slowlog_threshold_us(-1);
        for _ in 0..3 {
            e.maybe_push_slowlog(set_args(), Some(peer()), 0);
        }
        let all = e.slowlog_get(None);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].id, 3);
        assert_eq!(all[2].id, 1);
        let limited = e.slowlog_get(Some(2));
        assert_eq!(limited.len(), 2);
        assert_eq!(limited[0].id, 3);
    }

    #[test]
    fn reset_clears_ring() {
        let e = KvEngine::new();
        e.set_slowlog_threshold_us(-1);
        e.maybe_push_slowlog(set_args(), Some(peer()), 0);
        assert_eq!(e.slowlog_len(), 1);
        e.slowlog_reset();
        assert_eq!(e.slowlog_len(), 0);
    }

    #[test]
    fn recorded_entry_captures_args_and_peer() {
        let e = KvEngine::new();
        e.set_slowlog_threshold_us(-1);
        e.maybe_push_slowlog(set_args(), Some(peer()), 42);
        let entries = e.slowlog_get(None);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].args, set_args());
        assert_eq!(entries[0].duration_us, 42);
        assert_eq!(entries[0].peer, peer());
    }

    #[test]
    fn missing_peer_falls_back_to_unspecified() {
        let e = KvEngine::new();
        e.set_slowlog_threshold_us(-1);
        e.maybe_push_slowlog(set_args(), None, 1);
        let entries = e.slowlog_get(None);
        assert_eq!(entries[0].peer, SocketAddr::from(([0, 0, 0, 0], 0)));
    }
}

#[cfg(test)]
mod info_fields_tests {
    use super::*;
    use crate::storage::eviction::{EvictionConfig, EvictionPolicy};
    use std::time::{Duration, Instant};

    #[test]
    fn total_commands_initial_value_is_zero() {
        let e = KvEngine::new();
        assert_eq!(e.total_commands_processed(), 0);
    }

    #[test]
    fn evicted_keys_initial_value_is_zero() {
        let e = KvEngine::new();
        assert_eq!(e.evicted_keys(), 0);
    }

    #[test]
    fn expired_keys_initial_value_is_zero() {
        let e = KvEngine::new();
        assert_eq!(e.expired_keys(), 0);
    }

    /// Evict a key by setting maxmemory very low, then assert the counter
    /// reflects the eviction.
    #[test]
    fn evicted_keys_increments_on_maxmemory_eviction() {
        let e = KvEngine::new();
        // Room for exactly one entry (`PER_ENTRY_OVERHEAD` dominates): the
        // second insert must evict the first.
        e.set_eviction_config(EvictionConfig {
            max_memory: PER_ENTRY_OVERHEAD + 4,
            policy: EvictionPolicy::AllKeysRandom,
            samples: 1,
        })
        .unwrap();
        e.set(b"a".to_vec(), b"1".to_vec()).unwrap();
        assert_eq!(e.evicted_keys(), 0);
        // Second insert must evict "a" because maxmemory is too tight.
        e.set(b"b".to_vec(), b"2".to_vec()).unwrap();
        assert!(
            e.evicted_keys() >= 1,
            "evicted_keys should be at least 1 after a forced eviction"
        );
    }

    /// Mark a key as manually expired, then sweep and assert
    /// expired_keys is incremented.
    #[test]
    fn expired_keys_increments_on_ttl_expiry() {
        let e = KvEngine::new();
        e.set(b"a".to_vec(), b"1".to_vec()).unwrap();
        // Manually mark the key as expired (expire_at_ms would delete
        // it immediately when the epoch is in the past).
        {
            let mut store = e.store.write().unwrap();
            if let Some(entry) = store.get_mut("a".as_bytes()) {
                entry.expire_at = Some(Instant::now() - Duration::from_secs(60));
            }
        }
        // The background sweeper samples up to 32 keys; with only one
        // key in the store it must find it.
        let s = e.sweep_expired(32).unwrap();
        assert_eq!(s.evicted, 1, "sweep must remove the manually-expired key");
        assert_eq!(
            e.expired_keys(),
            1,
            "expired_keys must reflect the sweeper removal"
        );
    }
}

#[cfg(test)]
mod aof_rewrite_tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use crate::persistence::AofWriter;
    use crate::persistence::config::{AofConfig, FsyncPolicy};
    use crate::persistence::replay;
    use crate::storage::engine::TtlStatus;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tmp_path(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("ferrum-rewrite-{label}-{nanos}-{n}.aof"))
    }

    /// Waits for an in-progress rewrite to finish.
    ///
    /// Callers that need to wait for the rewrite to *start* as well (to handle
    /// the spawn/start race) should poll `is_rewriting()` before calling this.
    /// In very fast environments the rewrite may already be done when the
    /// caller arrives; the downstream file-content assertions serve as the
    /// ultimate correctness check in that case.
    fn wait_for_rewrite(writer: &AofWriter, timeout: Duration) -> bool {
        let start = Instant::now();
        while writer.is_rewriting() {
            if start.elapsed() > timeout {
                return false;
            }
            thread::sleep(Duration::from_millis(5));
        }
        true
    }

    /// Extracts every `SET` key from a compact/raw AOF byte stream.
    fn parse_set_keys(bytes: &[u8]) -> Vec<Vec<u8>> {
        let prefix = b"*3\r\n$3\r\nSET\r\n";
        let mut keys = Vec::new();
        let mut i = 0usize;
        while i + prefix.len() <= bytes.len() {
            if &bytes[i..i + prefix.len()] == prefix {
                let mut j = i + prefix.len();
                let mut num = 0usize;
                j += 1; // skip '$'
                while bytes[j] != b'\r' {
                    num = num * 10 + (bytes[j] - b'0') as usize;
                    j += 1;
                }
                j += 2; // skip CRLF
                keys.push(bytes[j..j + num].to_vec());
                i = j + num + 2;
            } else {
                i += 1;
            }
        }
        keys
    }

    fn count_set_records(bytes: &[u8]) -> usize {
        bytes
            .windows(b"*3\r\n$3\r\nSET\r\n".len())
            .filter(|w| *w == b"*3\r\n$3\r\nSET\r\n")
            .count()
    }

    #[test]
    fn rewrite_compacts_aof_to_one_set_per_key() {
        let path = tmp_path("compact");
        let cfg = AofConfig::new(path.clone(), FsyncPolicy::Always);
        let writer = Arc::new(AofWriter::open(&cfg).unwrap());
        let engine = KvEngine::new().with_aof(Arc::clone(&writer));

        for i in 0..40u32 {
            engine
                .set(format!("k{i}").into_bytes(), format!("v{i}").into_bytes())
                .unwrap();
        }
        // Overwrite a prefix to guarantee pre-rewrite redundancy on disk.
        for i in 0..10u32 {
            engine
                .set(format!("k{i}").into_bytes(), b"new".to_vec())
                .unwrap();
        }

        engine.rewrite_aof().unwrap();
        // Wait for the rewrite to start and finish. In fast environments
        // the rewrite may complete before we reach here; the file-content
        // assertions below are the ultimate correctness check.
        let start = Instant::now();
        while !writer.is_rewriting() && start.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(1));
        }
        if writer.is_rewriting() {
            assert!(
                wait_for_rewrite(&writer, Duration::from_secs(5)),
                "rewrite did not finish"
            );
        }

        let bytes = fs::read(&path).unwrap();
        let keys = parse_set_keys(&bytes);
        assert_eq!(
            keys.len(),
            40,
            "compact AOF must contain exactly one SET per live key"
        );
        let distinct: std::collections::HashSet<Vec<u8>> = keys.iter().cloned().collect();
        assert_eq!(
            distinct.len(),
            40,
            "no redundant duplicate SET frames for the same key"
        );

        // Replaying the compact file must restore the full keyspace.
        drop(engine);
        drop(writer);
        let restored = KvEngine::new();
        replay(&path, &restored).unwrap();
        for i in 0..40u32 {
            assert_eq!(
                restored.get(format!("k{i}").as_bytes()).unwrap(),
                if i < 10 {
                    Some(b"new".to_vec())
                } else {
                    Some(format!("v{i}").into_bytes())
                }
            );
        }
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn rewrite_preserves_ttl_as_pexpireat_and_drops_expired() {
        let path = tmp_path("ttl");
        let cfg = AofConfig::new(path.clone(), FsyncPolicy::Always);
        let writer = Arc::new(AofWriter::open(&cfg).unwrap());
        let engine = KvEngine::new().with_aof(Arc::clone(&writer));

        for i in 0..5u32 {
            engine
                .set(format!("k{i}").into_bytes(), format!("v{i}").into_bytes())
                .unwrap();
        }
        // k0 gets a TTL; k1 gets a TTL that is already in the past (expired).
        engine
            .expire_at_ms(b"k0", current_epoch_ms() + 60_000)
            .unwrap();
        engine
            .expire_at_ms(b"k1", current_epoch_ms() - 1_000)
            .unwrap();

        engine.rewrite_aof().unwrap();
        // Wait for the rewrite to start and finish. In fast environments
        // the rewrite may complete before we reach here; the file-content
        // assertions below are the ultimate correctness check.
        let start = Instant::now();
        while !writer.is_rewriting() && start.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(1));
        }
        if writer.is_rewriting() {
            assert!(
                wait_for_rewrite(&writer, Duration::from_secs(5)),
                "rewrite did not finish"
            );
        }

        let bytes = fs::read(&path).unwrap();
        // One SET per live key (k1 expired and must be excluded).
        assert_eq!(count_set_records(&bytes), 4);
        // Exactly one PEXPIREAT, for the live TTL key k0.
        let pexpireat = bytes
            .windows(b"$9\r\nPEXPIREAT\r\n".len())
            .filter(|w| *w == b"$9\r\nPEXPIREAT\r\n")
            .count();
        assert_eq!(pexpireat, 1, "exactly one PEXPIREAT for the live TTL key");

        drop(engine);
        drop(writer);
        let restored = KvEngine::new();
        replay(&path, &restored).unwrap();
        assert!(matches!(
            restored.ttl_ms(b"k0").unwrap(),
            TtlStatus::Millis(_)
        ));
        assert_eq!(restored.get(b"k1").unwrap(), None, "expired key dropped");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn writes_during_rewrite_survive_restart() {
        let path = tmp_path("during");
        let cfg = AofConfig::new(path.clone(), FsyncPolicy::Always);
        let writer = Arc::new(AofWriter::open(&cfg).unwrap());
        let engine = KvEngine::new().with_aof(Arc::clone(&writer));

        for i in 0..500u32 {
            engine
                .set(format!("k{i}").into_bytes(), format!("v{i}").into_bytes())
                .unwrap();
        }

        engine.rewrite_aof().unwrap();

        // Wait for the rewrite to start, then issue a write that must land in
        // the delta buffer (or, if it finished first, as a normal post-rewrite
        // append — both survive replay). A larger keyspace keeps the rewrite
        // window open long enough for the in-flight write to be captured.
        let wait = Instant::now();
        while !writer.is_rewriting() && wait.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(5));
        }
        let mut wrote_during = false;
        for _ in 0..400 {
            if !writer.is_rewriting() {
                break;
            }
            engine.set(b"during".to_vec(), b"yes".to_vec()).unwrap();
            wrote_during = true;
            thread::sleep(Duration::from_millis(1));
        }
        if !wrote_during {
            engine.set(b"during".to_vec(), b"yes".to_vec()).unwrap();
        }

        // The rewrite may have already finished while we were writing
        // "during" keys. Only wait if it is still in progress.
        if writer.is_rewriting() {
            // At this point we already observed is_rewriting==true earlier,
            // so the rewrite has started; just wait for it to finish.
            let wait_start = Instant::now();
            while writer.is_rewriting() {
                assert!(
                    wait_start.elapsed() < Duration::from_secs(5),
                    "rewrite did not complete in time"
                );
                thread::sleep(Duration::from_millis(5));
            }
        }

        drop(engine);
        drop(writer);
        let restored = KvEngine::new();
        replay(&path, &restored).unwrap();
        for i in 0..500u32 {
            assert_eq!(
                restored.get(format!("k{i}").as_bytes()).unwrap(),
                Some(format!("v{i}").into_bytes())
            );
        }
        assert_eq!(
            restored.get(b"during").unwrap(),
            Some(b"yes".to_vec()),
            "write issued during the rewrite must be preserved after restart"
        );
        let _ = fs::remove_file(&path);
    }
}
