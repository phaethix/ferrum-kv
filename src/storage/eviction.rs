//! Eviction configuration and candidate selection.
//!
//! Separated from [`crate::storage::engine`] so the policy logic can be
//! unit-tested in isolation. Candidate selection is approximate, matching
//! Redis' design: instead of maintaining a global priority structure, we
//! sample a small number of keys and pick the least desirable one.

use std::time::Instant;

/// How the engine should decide which key to drop when it runs out of memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvictionPolicy {
    /// Refuse writes that would grow the dataset past `maxmemory`.
    NoEviction,
    /// Approximate LRU over every key.
    AllKeysLru,
    /// Approximate LRU over keys that have a TTL; falls back to
    /// [`EvictionPolicy::NoEviction`] if no volatile keys exist.
    VolatileLru,
    /// Approximate LFU (Morris counter with logarithmic increment and
    /// minute-granularity decay) over every key.
    AllKeysLfu,
    /// Approximate LFU restricted to keys with a TTL.
    VolatileLfu,
    /// Pick a random key.
    AllKeysRandom,
    /// Pick a random volatile key.
    VolatileRandom,
    /// Pick the volatile key with the shortest remaining TTL.
    VolatileTtl,
    /// Adaptive Hybrid Eviction: blends recency, frequency, and TTL into
    /// a single score (`EPS`) and adapts its recency/frequency weight
    /// (`alpha`) based on the engine's observed hit ratio. See
    /// [`AdaptiveHybridState`] for the feedback loop.
    AllKeysAhe,
    /// AHE restricted to keys with a TTL.
    VolatileAhe,
}

impl EvictionPolicy {
    /// Parses the wire name used by the config file and by `CONFIG SET`.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "noeviction" => Some(Self::NoEviction),
            "allkeys-lru" => Some(Self::AllKeysLru),
            "volatile-lru" => Some(Self::VolatileLru),
            "allkeys-lfu" => Some(Self::AllKeysLfu),
            "volatile-lfu" => Some(Self::VolatileLfu),
            "allkeys-random" => Some(Self::AllKeysRandom),
            "volatile-random" => Some(Self::VolatileRandom),
            "volatile-ttl" => Some(Self::VolatileTtl),
            "allkeys-ahe" => Some(Self::AllKeysAhe),
            "volatile-ahe" => Some(Self::VolatileAhe),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::NoEviction => "noeviction",
            Self::AllKeysLru => "allkeys-lru",
            Self::VolatileLru => "volatile-lru",
            Self::AllKeysLfu => "allkeys-lfu",
            Self::VolatileLfu => "volatile-lfu",
            Self::AllKeysRandom => "allkeys-random",
            Self::VolatileRandom => "volatile-random",
            Self::VolatileTtl => "volatile-ttl",
            Self::AllKeysAhe => "allkeys-ahe",
            Self::VolatileAhe => "volatile-ahe",
        }
    }

    pub fn scope(self) -> EvictionScope {
        match self {
            Self::NoEviction => EvictionScope::AllKeys,
            Self::AllKeysLru | Self::AllKeysLfu | Self::AllKeysRandom | Self::AllKeysAhe => {
                EvictionScope::AllKeys
            }
            Self::VolatileLru
            | Self::VolatileLfu
            | Self::VolatileRandom
            | Self::VolatileTtl
            | Self::VolatileAhe => EvictionScope::Volatile,
        }
    }
}

/// Which subset of the keyspace is eligible for eviction under a policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvictionScope {
    /// Every live key is a candidate.
    AllKeys,
    /// Only keys with an active TTL are candidates.
    Volatile,
}

/// User-facing eviction configuration.
#[derive(Debug, Clone, Copy)]
pub struct EvictionConfig {
    /// Soft memory ceiling, in bytes. Zero means unlimited.
    pub max_memory: u64,
    /// Strategy used when the ceiling is exceeded.
    pub policy: EvictionPolicy,
    /// How many random candidates are inspected per eviction round.
    ///
    /// Redis' default is 5; higher values give better approximations at
    /// the cost of doing more work per write that overflows memory.
    pub samples: usize,
}

impl Default for EvictionConfig {
    fn default() -> Self {
        Self {
            max_memory: 0,
            policy: EvictionPolicy::NoEviction,
            samples: 5,
        }
    }
}

/// Metadata forwarded by the engine for each sampling candidate.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// Owned copy of the key, returned so the caller can drop it from the
    /// store without having to re-borrow the map.
    pub key: Vec<u8>,
    /// `Instant` of the most recent access; `None` if the key has never
    /// been touched (should not happen in practice).
    pub last_access: Option<Instant>,
    /// TTL deadline, if any.
    pub expire_at: Option<Instant>,
    /// Morris-style access frequency counter (0..=255). Higher means
    /// hotter. Sampled as-is (no further decay applied at pick time).
    pub lfu_counter: u8,
}

/// Picks the "worst" candidate according to `policy`.
///
/// Returns `None` when the iterator was empty — typically because no key
/// matched the policy's scope (for example `volatile-lru` on a dataset
/// without any TTL). Callers should treat that the same way they'd treat
/// [`EvictionPolicy::NoEviction`].
///
/// For [`EvictionPolicy::AllKeysAhe`] / [`EvictionPolicy::VolatileAhe`]
/// callers should use [`pick_victim_ahe`] instead so the adaptive weight
/// is threaded through; this function falls back to pure LFU in that case.
pub fn pick_victim(policy: EvictionPolicy, candidates: Vec<Candidate>) -> Option<Candidate> {
    if candidates.is_empty() {
        return None;
    }
    match policy {
        EvictionPolicy::NoEviction => None,
        EvictionPolicy::AllKeysRandom | EvictionPolicy::VolatileRandom => {
            // Pick the last candidate: the engine supplies them in random
            // iteration order (HashMap + indexmap are already unordered
            // from the client's point of view).
            candidates.into_iter().next()
        }
        EvictionPolicy::AllKeysLru | EvictionPolicy::VolatileLru => candidates
            .into_iter()
            .min_by_key(|c| c.last_access.unwrap_or(Instant::now())),
        EvictionPolicy::AllKeysLfu | EvictionPolicy::VolatileLfu => {
            // Lowest counter wins; break ties by oldest access to avoid
            // repeatedly evicting freshly inserted keys (which all start
            // at LFU_INIT_VAL).
            candidates
                .into_iter()
                .min_by_key(|c| (c.lfu_counter, c.last_access.unwrap_or_else(Instant::now)))
        }
        EvictionPolicy::VolatileTtl => candidates
            .into_iter()
            .filter(|c| c.expire_at.is_some())
            .min_by_key(|c| c.expire_at.unwrap()),
        EvictionPolicy::AllKeysAhe | EvictionPolicy::VolatileAhe => {
            pick_victim_ahe(AHE_DEFAULT_ALPHA, candidates)
        }
    }
}

/// Default blend weight used when AHE has no feedback history yet: equal
/// weight on recency and frequency.
pub const AHE_DEFAULT_ALPHA: f32 = 0.5;

/// Picks the AHE victim, i.e. the candidate with the highest eviction
/// priority score (EPS).
///
/// `alpha` controls the blend between recency and frequency and is
/// expected to live in `[0.0, 1.0]`; values outside the range are clamped
/// so a misconfigured feedback loop cannot break the policy. See
/// [`eps_score`] for the exact formula.
pub fn pick_victim_ahe(alpha: f32, candidates: Vec<Candidate>) -> Option<Candidate> {
    if candidates.is_empty() {
        return None;
    }
    let alpha = alpha.clamp(0.0, 1.0);
    let now = Instant::now();
    candidates
        .into_iter()
        .map(|c| {
            let score = eps_score(alpha, &c, now);
            (score, c)
        })
        // Highest score wins (i.e. most evictable). `f32` has no total
        // order, but our score is always finite so `partial_cmp` is safe.
        .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(_, c)| c)
}

/// Computes the Eviction Priority Score for `candidate` under `alpha`.
///
/// ```text
/// EPS = alpha * recency + (1 - alpha) * infrequency + ttl_penalty
/// ```
///
/// * `recency` — `(now - last_access) / RECENCY_HORIZON` clamped to `[0, 1]`.
///   Older accesses score higher.
/// * `infrequency` — `1 - lfu_counter / 255`. Colder keys score higher.
/// * `ttl_penalty` — `+TTL_PENALTY` when the key expires in under
///   [`TTL_SHORT_HORIZON`]; `0` otherwise. Lets AHE prefer doomed keys
///   without crowding out the main signal.
///
/// Higher is more evictable.
pub fn eps_score(alpha: f32, candidate: &Candidate, now: Instant) -> f32 {
    use std::time::Duration;
    const RECENCY_HORIZON: Duration = Duration::from_secs(600);
    const TTL_SHORT_HORIZON: Duration = Duration::from_secs(30);
    const TTL_PENALTY: f32 = 0.2;

    let recency = match candidate.last_access {
        Some(ts) => {
            let age = now.saturating_duration_since(ts).as_secs_f32();
            (age / RECENCY_HORIZON.as_secs_f32()).min(1.0)
        }
        None => 1.0,
    };
    let infrequency = 1.0 - (candidate.lfu_counter as f32) / 255.0;
    let ttl_penalty = match candidate.expire_at {
        Some(deadline) if deadline > now => {
            let remaining = deadline.saturating_duration_since(now);
            if remaining <= TTL_SHORT_HORIZON {
                TTL_PENALTY
            } else {
                0.0
            }
        }
        // Already expired (or will be on next sweep): treat as very evictable.
        Some(_) => TTL_PENALTY,
        None => 0.0,
    };
    alpha * recency + (1.0 - alpha) * infrequency + ttl_penalty
}

/// Minute bucket extracted from [`SystemTime`], wrapping into a `u16` so
/// it fits alongside [`Candidate::lfu_counter`] in the stored entry.
///
/// Wrapping is fine because the decay logic only cares about the
/// *difference* between two nearby minute marks (typically < 60).
pub fn current_minute_stamp() -> u16 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let minutes = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() / 60)
        .unwrap_or(0);
    (minutes & 0xFFFF) as u16
}

/// Default Morris counter assigned to freshly inserted keys, matching
/// Redis' `LFU_INIT_VAL`. Giving new keys a small head start protects
/// them from being evicted immediately after insertion.
pub const LFU_INIT_VAL: u8 = 5;

/// Returns the decayed counter for an entry whose last decay tick was
/// recorded at `last_stamp` (minute granularity).
///
/// `decay_period_minutes` is the number of minutes required to remove
/// one unit from the counter; values <= 0 disable decay entirely. This
/// mirrors Redis' `lfu-decay-time` (default: 1 minute).
pub fn decayed_counter(
    counter: u8,
    last_stamp: u16,
    now_stamp: u16,
    decay_period_minutes: u16,
) -> u8 {
    if decay_period_minutes == 0 {
        return counter;
    }
    // `now_stamp - last_stamp` with `u16` wrapping covers ~45 days, well
    // beyond any realistic idle window between accesses.
    let elapsed = now_stamp.wrapping_sub(last_stamp);
    let ticks = elapsed / decay_period_minutes;
    counter.saturating_sub(ticks.min(u8::MAX as u16) as u8)
}

/// Probabilistically increments a Morris counter.
///
/// The step size shrinks as the counter grows, which keeps hot keys from
/// saturating at `255` after only a few accesses and gives lukewarm keys
/// room to distinguish themselves. `rand01` must produce a value in
/// `[0.0, 1.0)`; in production the engine supplies an atomic LCG sample,
/// tests pass a deterministic sequence.
///
/// Constants follow Redis' defaults (`lfu-log-factor = 10`).
pub fn probabilistic_increment(counter: u8, rand01: f32) -> u8 {
    const LFU_LOG_FACTOR: f32 = 10.0;
    if counter == u8::MAX {
        return counter;
    }
    let baseline = counter.saturating_sub(LFU_INIT_VAL) as f32;
    let p = 1.0 / (baseline * LFU_LOG_FACTOR + 1.0);
    if rand01 < p { counter + 1 } else { counter }
}

/// Adaptive feedback loop controlling the AHE `alpha` parameter.
///
/// The engine feeds a rolling hit ratio into [`AdaptiveHybridState::observe`]
/// every few evictions. If the ratio drops relative to the previous
/// window, the controller nudges `alpha` in the opposite direction of
/// the last adjustment (simple 1D gradient search). Otherwise it keeps
/// heading the same way. This keeps the policy self-tuning without
/// requiring workload-specific configuration.
#[derive(Debug, Clone, Copy)]
pub struct AdaptiveHybridState {
    /// Current blend weight; `1.0` = pure recency, `0.0` = pure frequency.
    pub alpha: f32,
    /// Step size applied per adjustment.
    pub step: f32,
    /// Sign (`+1.0` or `-1.0`) of the last adjustment. Flipped when the
    /// hit ratio regresses.
    pub direction: f32,
    /// Last hit ratio observed; used to detect regressions.
    pub last_hit_ratio: f32,
    /// Minimum samples between adjustments to filter out noise.
    pub window_size: u64,
    /// Samples accumulated in the current window.
    pub window_count: u64,
}

impl Default for AdaptiveHybridState {
    fn default() -> Self {
        Self {
            alpha: AHE_DEFAULT_ALPHA,
            step: 0.05,
            direction: 1.0,
            last_hit_ratio: 0.0,
            window_size: 64,
            window_count: 0,
        }
    }
}

impl AdaptiveHybridState {
    /// Feeds one eviction sample `(hits, misses)` into the controller.
    ///
    /// Returns `true` when the window completed and `alpha` was adjusted,
    /// `false` otherwise. Callers typically ignore the return value; it
    /// exists so tests can assert that adjustments fire.
    pub fn observe(&mut self, hits: u64, misses: u64) -> bool {
        self.window_count += 1;
        if self.window_count < self.window_size {
            return false;
        }
        let total = hits + misses;
        let ratio = if total == 0 {
            self.last_hit_ratio
        } else {
            hits as f32 / total as f32
        };
        if ratio + 1e-4 < self.last_hit_ratio {
            // Regressed — flip direction.
            self.direction = -self.direction;
        }
        self.alpha = (self.alpha + self.direction * self.step).clamp(0.05, 0.95);
        self.last_hit_ratio = ratio;
        self.window_count = 0;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn candidate(name: &str, age_ms: u64, ttl_ms: Option<u64>) -> Candidate {
        candidate_lfu(name, age_ms, ttl_ms, 0)
    }

    fn candidate_lfu(name: &str, age_ms: u64, ttl_ms: Option<u64>, counter: u8) -> Candidate {
        let now = Instant::now();
        Candidate {
            key: name.as_bytes().to_vec(),
            last_access: Some(now - Duration::from_millis(age_ms)),
            expire_at: ttl_ms.map(|t| now + Duration::from_millis(t)),
            lfu_counter: counter,
        }
    }

    #[test]
    fn lru_picks_oldest_access() {
        let pick = pick_victim(
            EvictionPolicy::AllKeysLru,
            vec![
                candidate("young", 10, None),
                candidate("old", 1_000, None),
                candidate("mid", 100, None),
            ],
        )
        .unwrap();
        assert_eq!(pick.key, b"old");
    }

    #[test]
    fn volatile_ttl_picks_shortest_remaining() {
        let pick = pick_victim(
            EvictionPolicy::VolatileTtl,
            vec![
                candidate("long", 0, Some(60_000)),
                candidate("short", 0, Some(500)),
                candidate("mid", 0, Some(5_000)),
            ],
        )
        .unwrap();
        assert_eq!(pick.key, b"short");
    }

    #[test]
    fn no_eviction_never_picks() {
        assert!(pick_victim(EvictionPolicy::NoEviction, vec![candidate("a", 0, None)]).is_none());
    }

    #[test]
    fn empty_candidates_return_none() {
        assert!(pick_victim(EvictionPolicy::AllKeysLru, vec![]).is_none());
    }

    #[test]
    fn policy_name_roundtrip() {
        for name in [
            "noeviction",
            "allkeys-lru",
            "volatile-lru",
            "allkeys-lfu",
            "volatile-lfu",
            "allkeys-random",
            "volatile-random",
            "volatile-ttl",
            "allkeys-ahe",
            "volatile-ahe",
        ] {
            let p = EvictionPolicy::from_name(name).unwrap();
            assert_eq!(p.name(), name);
        }
        assert!(EvictionPolicy::from_name("bogus").is_none());
    }

    #[test]
    fn lfu_picks_lowest_counter() {
        let pick = pick_victim(
            EvictionPolicy::AllKeysLfu,
            vec![
                candidate_lfu("hot", 0, None, 200),
                candidate_lfu("cold", 0, None, 3),
                candidate_lfu("warm", 0, None, 50),
            ],
        )
        .unwrap();
        assert_eq!(pick.key, b"cold");
    }

    #[test]
    fn lfu_breaks_ties_by_oldest_access() {
        let pick = pick_victim(
            EvictionPolicy::AllKeysLfu,
            vec![
                candidate_lfu("new", 10, None, 5),
                candidate_lfu("old", 10_000, None, 5),
            ],
        )
        .unwrap();
        assert_eq!(pick.key, b"old");
    }

    #[test]
    fn probabilistic_increment_never_exceeds_max() {
        let mut c = u8::MAX;
        c = probabilistic_increment(c, 0.0);
        assert_eq!(c, u8::MAX);
    }

    #[test]
    fn probabilistic_increment_fresh_key_advances_on_low_draw() {
        let c = probabilistic_increment(LFU_INIT_VAL, 0.0);
        assert_eq!(c, LFU_INIT_VAL + 1);
    }

    #[test]
    fn probabilistic_increment_hot_key_rarely_advances() {
        // At counter 255 - 1, the probability is ~1/(250*10+1) ≈ 0.0004;
        // a mid-range draw must not advance the counter.
        let c = probabilistic_increment(u8::MAX - 1, 0.5);
        assert_eq!(c, u8::MAX - 1);
    }

    #[test]
    fn decay_subtracts_one_per_period() {
        assert_eq!(decayed_counter(10, 0, 3, 1), 7);
        assert_eq!(decayed_counter(10, 0, 5, 2), 8);
        assert_eq!(decayed_counter(10, 0, 60, 0), 10); // disabled
        assert_eq!(decayed_counter(2, 0, 100, 1), 0); // saturates at 0
    }

    #[test]
    fn ahe_prefers_cold_and_stale() {
        let hot_recent = candidate_lfu("hot", 10, None, 200);
        let cold_old = candidate_lfu("cold", 10_000, None, 2);
        let pick = pick_victim_ahe(0.5, vec![hot_recent.clone(), cold_old.clone()]).unwrap();
        assert_eq!(pick.key, b"cold");
    }

    #[test]
    fn ahe_ttl_penalty_promotes_doomed_keys() {
        // Same LFU / recency, but one has a short TTL.
        let healthy = candidate_lfu("healthy", 100, Some(600_000), 50);
        let doomed = candidate_lfu("doomed", 100, Some(5_000), 50);
        let pick = pick_victim_ahe(0.5, vec![healthy, doomed]).unwrap();
        assert_eq!(pick.key, b"doomed");
    }

    #[test]
    fn adaptive_state_flips_on_regression() {
        let mut state = AdaptiveHybridState {
            window_size: 1,
            ..AdaptiveHybridState::default()
        };
        // First window establishes a baseline hit ratio of 0.9.
        state.observe(9, 1);
        let alpha_after_first = state.alpha;
        let dir_after_first = state.direction;
        // Ratio drops to 0.5 — controller flips direction next time.
        state.observe(5, 5);
        assert_eq!(state.direction, -dir_after_first);
        assert!((state.alpha - alpha_after_first).abs() > f32::EPSILON);
    }
}
