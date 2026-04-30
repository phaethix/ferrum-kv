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
    /// Pick a random key.
    AllKeysRandom,
    /// Pick a random volatile key.
    VolatileRandom,
    /// Pick the volatile key with the shortest remaining TTL.
    VolatileTtl,
}

impl EvictionPolicy {
    /// Parses the wire name used by the config file and by `CONFIG SET`.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "noeviction" => Some(Self::NoEviction),
            "allkeys-lru" => Some(Self::AllKeysLru),
            "volatile-lru" => Some(Self::VolatileLru),
            "allkeys-random" => Some(Self::AllKeysRandom),
            "volatile-random" => Some(Self::VolatileRandom),
            "volatile-ttl" => Some(Self::VolatileTtl),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::NoEviction => "noeviction",
            Self::AllKeysLru => "allkeys-lru",
            Self::VolatileLru => "volatile-lru",
            Self::AllKeysRandom => "allkeys-random",
            Self::VolatileRandom => "volatile-random",
            Self::VolatileTtl => "volatile-ttl",
        }
    }

    pub fn scope(self) -> EvictionScope {
        match self {
            Self::NoEviction => EvictionScope::AllKeys,
            Self::AllKeysLru | Self::AllKeysRandom => EvictionScope::AllKeys,
            Self::VolatileLru | Self::VolatileRandom | Self::VolatileTtl => EvictionScope::Volatile,
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
}

/// Picks the "worst" candidate according to `policy`.
///
/// Returns `None` when the iterator was empty — typically because no key
/// matched the policy's scope (for example `volatile-lru` on a dataset
/// without any TTL). Callers should treat that the same way they'd treat
/// [`EvictionPolicy::NoEviction`].
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
        EvictionPolicy::VolatileTtl => candidates
            .into_iter()
            .filter(|c| c.expire_at.is_some())
            .min_by_key(|c| c.expire_at.unwrap()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn candidate(name: &str, age_ms: u64, ttl_ms: Option<u64>) -> Candidate {
        let now = Instant::now();
        Candidate {
            key: name.as_bytes().to_vec(),
            last_access: Some(now - Duration::from_millis(age_ms)),
            expire_at: ttl_ms.map(|t| now + Duration::from_millis(t)),
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
            "allkeys-random",
            "volatile-random",
            "volatile-ttl",
        ] {
            let p = EvictionPolicy::from_name(name).unwrap();
            assert_eq!(p.name(), name);
        }
        assert!(EvictionPolicy::from_name("bogus").is_none());
    }
}
