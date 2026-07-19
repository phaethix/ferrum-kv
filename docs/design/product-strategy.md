# FerrumKV Product Strategy v2

> Author: Product Strategy — 2026-07-04
> Based on: deep market research (Rust embedded KV landscape, eviction algorithm SOTA, Redis/Valkey/Dragonfly ecosystem)
> Target: v0.5 → v0.7
> Status: **revised after research — sharpened positioning**

---

## Executive Summary

**The original strategy positioned FerrumKV as "the embeddable RESP2-compatible KV for Rust." Market research reveals this position is already occupied by [kevy](https://github.com/goliajp/kevy) (zero-dep, 98-command parity, 54ns embedded GET, 768KB binary, 2.3x Valkey throughput). Competing head-to-head on command parity or raw throughput is a losing game.**

**Revised positioning: FerrumKV is the *eviction algorithm laboratory* and *readable systems-programming reference* — the KV that ships 10+ eviction policies with reproducible benchmarks, and the codebase you actually read to learn how a RESP2 server works.**

---

## 1. Market Landscape (Research Synthesis)

### 1.1 The Embedded Rust KV Field (2025–2026)

| Project | Strengths | Why FerrumKV Doesn't Compete Directly |
|---------|-----------|--------------------------------------|
| **[kevy](https://github.com/goliajp/kevy)** | Zero-dep, 98-command parity, all 5 Redis types, 54ns embedded GET, 768KB binary, pub/sub, cluster routing, 2.3–2.7x Valkey | kevy has already won the "fast embedded Redis replacement" space. Thread-per-core + io_uring architecture far more complex than FerrumKV's model. |
| **[emdb](https://github.com/jamesgober/emdb-rs)** | Fastest embedded KV (Bitcask-style), AES-256 encryption, io_uring, 3.2x redb on bulk load | Pure embedded DB, no RESP2, no Redis compatibility. Different segment. |
| **[redb](https://github.com/cberner/redb)** | Mature ACID embedded DB, MVCC, copy-on-write B-trees, stable format | No RESP2. Different segment (OLTP embedded DB vs cache). |
| **[FeOx DB](https://github.com/mehrantsi/feoxdb)** | 180ns GET, JSON Patch, CAS, CLOCK eviction | Very new (v0.1), no ecosystem. Interesting but unproven. |
| **[fast-cache](https://github.com/d-tietjen/fast-cache)** | 31M ops/s via FCNP protocol, thread-per-core, GPU transfer layer | Very new (v0.1, May 2026), AI/ML focused (vLLM/LMCache). |
| **Garnet** (Microsoft) | .NET-based, RESP-compatible, VLDB 2026 paper, tiered storage | Heavyweight (.NET runtime), server-only, not embeddable. |
| **Dragonfly** | 7x Valkey on sorted sets, fully multi-threaded, 28% less memory | Server-only, C++ codebase. Not embeddable. |

### 1.2 The Eviction Algorithm Frontier (2024–2025)

Key discovery: the eviction algorithm space is undergoing a renaissance. Four major developments:

| Algorithm | Year | Venue | Key Claim | Relevance to FerrumKV |
|-----------|------|-------|-----------|----------------------|
| **SIEVE** | 2024 | NSDI'24 | Simpler than LRU, beats 9 SOTA algorithms on 45%+ of 1,559 production traces, 2x LRU throughput | **Must implement.** This is the new baseline. FerrumKV currently has LRU/LFU/AHE but not SIEVE. |
| **AdaptiveClimb** | 2025 | arXiv:2511.21235 | Control-theoretic self-tuning, single parameter `jump`, outperforms ARC/CACHEUS/SIEVE on 1,067 traces, 35M ops/s @ 16 threads | **AHE's closest academic cousin.** Validates the adaptive direction. AHE needs differentiation from this. |
| **Cold-RL** | 2025 | arXiv:2508.12485 | Dueling DQN for NGINX cache eviction, 146% hit ratio improvement at 25MB, <2% CPU overhead, <500μs inference | **Long-term aspirational.** RL-based eviction now practical. Far future for FerrumKV. |
| **FreqRec** | 2025 | IEEE CICN | Lightweight LFU-LRU hybrid, 54% IPC improvement on SPEC CPU 2017 | CPU cache focused, less relevant for KV stores. |

**Critical finding for AHE**: AdaptiveClimb is functionally very similar — it adapts a single weight parameter based on observed hit/miss patterns, exactly as AHE's `alpha` adapts based on hit ratio. For AHE to be academically credible, it needs a clear differentiator. **AHE's unique angle: it incorporates TTL-awareness (time-to-live based scoring), which neither AdaptiveClimb nor SIEVE do.** This is the narrative hook.

### 1.3 RESP3 Adoption

- **redis-py 8.0** (Python): RESP3 is now the **default** protocol
- **node-redis 6.0** (Node.js): RESP3 is now the **default** (June 2026)
- **Valkey Swift 1.3**: Full RESP3 support including client-side caching
- Both Redis 8 and Valkey 9 speak byte-identical RESP2/RESP3 — no client changes needed

**Implication**: FerrumKV being RESP2-only is increasingly a limitation. RESP3 support (at minimum: `HELLO 3`, typed replies) should move up in priority.

### 1.4 Server-Side Landscape (Context)

| System | Best For | Key Number |
|--------|----------|------------|
| Redis 8 | Ecosystem maturity, managed cloud | Best p95 tail latency |
| Valkey 9 | Open-source purity, BSD license | ~1M RPS on high-core HW |
| Dragonfly | Multi-core throughput, memory efficiency | 7x Valkey on ZADD |
| KeyDB | Active-active replication | Niche use case |

These are all server-side beasts. FerrumKV does not compete here — and shouldn't try.

---

## 2. Revised Positioning

### The New One-Liner

**FerrumKV is the eviction algorithm laboratory for RESP2-compatible KV stores — and the most readable systems-programming reference implementation in Rust.**

### The Three-Pillar Differentiation

#### Pillar 1: Eviction Algorithm Platform

FerrumKV is the only KV that ships **16 eviction policies** (including SIEVE, SIEVE-S, AHE, and AdaptiveClimb) as first-class, benchmarked features. You can A/B test eviction policies on your workload, add your own policy by implementing a trait, and see reproducible benchmarks against standard baselines.

**This is a genuine gap.** No Redis fork, no embedded KV, and no academic simulator offers this combination: production-grade RESP2 server + pluggable eviction algorithms + reproducible benchmark suite.

#### Pillar 2: Readable Reference Implementation

kevy may be 2.7x faster, but its internal architecture (thread-per-core, shared-nothing, hand-bound Linux syscalls, io_uring, custom slab allocators) is **not readable** for someone learning how a KV store works. FerrumKV's ~8,500 lines of clean Rust with explicit design documents is the codebase you read to *understand*:

- How a RESP2 parser works (byte-level incremental parsing)
- How AOF persistence works (append + fsync policies + replay)
- How eviction interacts with expiration and memory tracking
- How a tokio-based network server handles connection lifecycle

This is not a consolation prize — it's a **deliberate product decision**. The market has fast KV stores. It doesn't have *teachable* ones.

#### Pillar 3: AHE — TTL-Aware Adaptive Eviction

AHE's differentiation from AdaptiveClimb: **AHE incorporates TTL as a first-class signal in the eviction score (EPS)**. AdaptiveClimb only balances recency vs frequency. AHE balances recency + frequency + **time-to-live urgency**. This is the academic hook.

### What FerrumKV Is NOT (Reaffirmed)

- NOT a high-throughput production cache (use kevy or Dragonfly)
- NOT a Redis replacement for large-scale deployments
- NOT chasing command parity (kevy already has 98 commands)
- NOT a distributed database

### Target Audience (Refined)

1. **Eviction algorithm researchers** — plug in a policy, run the benchmark suite, get a paper
2. **Systems programming learners** — read the code, understand the internals, modify and experiment
3. **Rust developers with unusual caching needs** — need a specific eviction policy that Redis/Dragonfly don't offer
4. **Educators** — teaching KV store design, use FerrumKV as course material

---

## 3. Revised Roadmap

### Phase 0: Foundation Hardening (v0.5.0) — 3 weeks

**Goal**: Table stakes for operational credibility. No positioning change — these are hygiene features any server must have.

| ID | Feature | Effort | Rationale |
|----|---------|--------|-----------|
| ✅ F-01 | CONFIG SET/GET | 3 days | Runtime config. Blocks all operational maturity. **(done v0.5.1)** |
| ✅ F-02 | AUTH requirepass | 1 day | Security baseline. `AUTH` + `requirepass` via CLI flag, config file, and runtime `CONFIG SET`. **(done v0.5.1)** |
| F-03 | SLOWLOG | 2 days | Latency observability. |
| F-04 | AOF REWRITE | 5 days | AOF compaction. Critical for any non-toy AOF user. |
| F-05 | MONITOR command | 1 day | Debugging tool. |
| F-06 | INFO fields expansion | 1 day | `instantaneous_ops_per_sec`, `evicted_keys`, `expired_keys`, `total_commands_processed`. |
| F-07 | WriteGuard pipeline refactor | 3 days | Eliminate write-path boilerplate. Foundation for clean eviction integration. |
| ✅ F-08 | Version bump + badge fix | 1 hour | Housekeeping. **(done v0.5.1)** |

**Exit criteria**: All CI green. AOF rewrite tested with 1M keys. No benchmark regression.

### Phase 1: Eviction Algorithm Platform (v0.5.1) — 3 weeks

**Goal**: Deliver on Pillar 1. This is where FerrumKV becomes unique.

| ID | Feature | Effort | Rationale |
|----|---------|--------|-----------|
| E-01 | **SIEVE implementation** | 2 days | The NSDI'24 algorithm that beats LRU/LFU on 45%+ of production traces. One queue + one pointer. Implementation is trivial. |
| E-02 | **SIEVE-S (SIEVE with TTL)** | 2 days | FerrumKV's contribution: SIEVE + TTL-awareness. Items near expiry get demoted faster. |
| E-03 | **AdaptiveClimb implementation** | 2 days | The control-theoretic policy from arXiv 2511.21235. Single parameter `jump`. |
| E-04 | **EvictionPolicy trait** | 2 days | Extract eviction behind a trait so new policies can be added without touching engine internals. Trait methods: `on_hit`, `on_miss`, `pick_victim`, `on_insert`, `on_evict`. |
| E-05 | **Eviction benchmark suite** | 4 days | Scripted workloads: Zipfian (α=0.7, 0.9, 1.0, 1.2), temporal hotspots, mixed R/W, production traces (MetaCDN, Twitter KV subsets from libCacheSim). Produce a comparison table across all policies. |
| E-06 | **AHE differentiation paper** | 3 days | Document how AHE differs from AdaptiveClimb: TTL signal in EPS, multi-dimensional scoring, adaptive α via hit-ratio feedback. Benchmarks vs LRU, LFU, SIEVE, AdaptiveClimb. |

**This phase is the pivot.** After v0.5.1, FerrumKV's README should lead with the eviction algorithm comparison table, not the command list.

### Phase 2: Protocol Modernization (v0.6.0) — 3 weeks

**Goal**: RESP3 support (since the ecosystem is moving there).

| ID | Feature | Effort | Rationale |
|----|---------|--------|-----------|
| P-01 | `HELLO 3` handshake | 2 days | RESP3 negotiation. Fall back to RESP2 for clients that don't request it. |
| P-02 | RESP3 typed replies | 3 days | Map types, set types, double, boolean, null, push messages. The encoder gains typed variants. |
| P-03 | RESP3 push for Pub/Sub | 2 days | If/when Pub/Sub is implemented, RESP3 push messages enable multiplexed subscriptions on a single connection. |
| P-04 | Client-side caching (basic) | 3 days | RESP3 `CLIENT TRACKING` — basic invalidation push. |
| P-05 | RESP2 backward compat | 1 day | Ensure all existing RESP2 clients continue to work. |

**Why RESP3 before data types**: The protocol is foundational. Adding List/Hash types over RESP2 is fine, but RESP3 enables client-side caching, push messages, and richer type representations (native doubles, booleans, maps) that make the type system coherent. Do protocol first, then types.

### Phase 3: Data Model Expansion (v0.7.0) — 4 weeks

**Goal**: List + Hash + Set types. (Sorted Sets deferred to v0.8, Streams deferred indefinitely.)

Note: This is *not* about competing with kevy on command count. It's about having enough types to run realistic eviction benchmarks (different types produce different access patterns) and to be useful for real embedded caching.

| ID | Feature | Effort | Notes |
|----|---------|--------|-------|
| D-01 | Value enum migration | 4 days | Architectural prerequisite. Type-per-key semantics. |
| D-02 | List type + commands | 4 days | LPUSH, RPUSH, LPOP, RPOP, LLEN, LRANGE, LINDEX. BLPOP/BRPOP deferred. |
| D-03 | Hash type + commands | 3 days | HSET, HGET, HDEL, HGETALL, HEXISTS, HLEN, HKEYS, HVALS. |
| D-04 | Set type + commands | 3 days | SADD, SREM, SMEMBERS, SISMEMBER, SCARD. SINTER/SUNION/SDIFF deferred. |
| D-05 | TYPE command | 0.5 day | Prerequisite for all multi-type operations. |
| D-06 | RENAME / RENAMENX / COPY | 1 day | High utility, trivial implementation. |
| D-07 | Multi-type AOF | 5 days | Extend AOF format for typed operations. Backward compat with string-only AOF. |

### Phase 4: Embedding Experience & Education (v0.7.1) — 2 weeks

| ID | Feature | Effort | Notes |
|----|---------|--------|-------|
| L-01 | FerrumKVBuilder | 2 days | `FerrumKV::builder().aof_path(p).policy(SIEVE).max_memory(64*MB).build()` |
| L-02 | `ferrum-kv` as library docs | 2 days | rustdoc examples, getting-started guide, API reference |
| L-03 | Course material: "Build a KV Store" | 5 days | 8-chapter walkthrough: parser → engine → AOF → eviction → async → benchmarks. Each chapter has exercises. |
| L-04 | Architecture tour video script | 2 days | Companion to the codebase: visual walkthrough of the data flow. |

### Beyond v0.8 (Not Yet Scheduled)

- Sorted Set (skip-list)
- Pub/Sub (tokio broadcast)
- AHE vs AdaptiveClimb formal benchmark paper
- Pluggable eviction trait with runtime policy switching
- Prometheus metrics endpoint
- TLS (rustls)
- Primary-Replica replication (academic exercise)

---

## 4. Competitive Positioning Map

```
                    HIGH throughput
                         │
                    kevy │  Dragonfly
                    (2.7x │  (7x Valkey
                    Valkey)│  on ZADD)
                         │
    Embedded ────────────┼──────────── Server-only
    library              │              process
                         │
               FerrumKV  │  Redis 8
               (eviction │  Valkey 9
               lab +     │
               readable) │
                         │
                    LOW throughput / TEACHING focus
```

FerrumKV occupies the **bottom-left** quadrant: embedded library, but optimized for algorithm transparency and readability rather than raw throughput. This quadrant is **empty** — no other project combines embeddability with a teaching/algorithm-experimentation focus.

---

## 5. Success Metrics (Revised)

### Quantitative

| Metric | Baseline (v0.4.1) | v0.5.0 Target | v0.5.1 Target |
|--------|-------------------|---------------|---------------|
| Eviction policies | 10 | 10 (unchanged) | **13** (+SIEVE, +SIEVE-S, +AdaptiveClimb) |
| Eviction benchmark coverage | 0 formal workloads | 0 | **5 workloads** × 13 policies |
| Commands | 15 | 22 | 22 |
| RESP3 support | ❌ | ❌ | ❌ (v0.6) |
| `redis-benchmark` SET QPS | 62,189 | ≥ 60,000 | ≥ 58,000 (acceptable for eviction overhead) |
| Binary size (release, stripped) | ~2.0MB | ≤ 2.2MB | ≤ 2.5MB |
| Test count | 293 | ≥ 350 | ≥ 400 |
| AHE differentiation documented | ❌ | ❌ | ✅ (paper) |

### Qualitative

- **Eviction credibility**: A researcher can read `design/whitepaper.md`, reproduce the benchmarks, and understand how AHE differs from AdaptiveClimb.
- **Library experience**: A developer adds `ferrum-kv = "0.5"` and has an embedded cache with their choice of 13 eviction policies.
- **Educational value**: A student can read the codebase from `main.rs` → `parser.rs` → `engine/mod.rs` → `eviction.rs` and understand every layer.

---

## 6. Risk Register (Updated)

| # | Risk | Probability | Impact | Mitigation |
|---|------|------------|--------|------------|
| R1 | kevy captures the "embedded Rust RESP2 KV" mindshare entirely | High | Medium | **Don't fight it. Pivot hard to eviction lab + education.** kevy can't do 13 eviction policies with reproducible benchmarks. |
| R2 | AHE not sufficiently differentiated from AdaptiveClimb | Medium | High | The TTL-awareness angle is real (AdaptiveClimb paper explicitly does not handle TTL). Lead with this. |
| R3 | SIEVE implementation is trivial (one queue + one pointer) — why would anyone use FerrumKV for this? | Low | Medium | The value is the *comparison platform*, not any single algorithm. Plug your own policy, run the benchmark suite, get a comparison table. |
| R4 | Single-maintainer bus factor | Present | High | Each module now has a design doc. Educational material doubles as onboarding docs for contributors. |
| R5 | RESP3 adoption accelerates, making RESP2-only servers look legacy | Medium | Medium | v0.6 adds RESP3. Until then, RESP2 is still the universal fallback — every Redis client speaks it. |

---

## 7. Immediate Actions (This Week)

1. ✅ ~~Version bump to 0.5.0-dev~~ (done)
2. ✅ ~~Create FERRUM-006/007/008~~ (done)
3. ✅ ~~Implement F-01 (CONFIG SET/GET)~~ (done) — supports `maxmemory`, `maxmemory-policy`, `maxmemory-samples` over RESP; `CONFIG GET *` and single-param lookups.
4. ✅ ~~Implement F-02 (AUTH)~~ (done) — `AUTH password`, `requirepass` via `--requirepass` flag, `requirepass` config-file directive, and runtime `CONFIG SET requirepass`. Unauthenticated commands are gated with `-NOAUTH`; wrong passwords return `-WRONGPASS`.
5. **Begin F-07 (WriteGuard refactor)** — foundation for E-04 (EvictionPolicy trait)
6. **Read SIEVE paper thoroughly** (NSDI'24) — the implementation is ~20 lines, but understand the design decisions
7. **Read AdaptiveClimb paper** (arXiv:2511.21235) — understand how it differs from AHE

---

## Appendix A: Research Sources

- [kevy — zero-dep Rust RESP2 KV](https://github.com/goliajp/kevy) (v1.3.0, June 2026)
- [SIEVE — NSDI'24 Best Paper](https://www.usenix.org/conference/nsdi24/presentation/zhang-yazhuo)
- [AdaptiveClimb — arXiv:2511.21235](https://arxiv.org/abs/2511.21235) (2025)
- [Cold-RL — arXiv:2508.12485](https://arxiv.org/abs/2508.12485) (2025)
- [FreqRec — IEEE CICN 2025](https://ieeexplore.ieee.org/document/11367833)
- [Dragonfly vs Valkey threading models](https://www.dragonflydb.io/blog/why-threading-models-matter-dragonfly-vs-valkey) (Nov 2025)
- [Redis 8 vs Valkey 8.1 technical comparison](https://www.dragonflydb.io/blog/redis-8-0-vs-valkey-8-1-a-technical-comparison) (May 2025)
- [In-memory data landscape end of 2025](https://www.dragonflydb.io/blog/in-memory-data-landscape-at-the-end-of-2025)
- [centminmod/redis-comparison-benchmarks](https://github.com/centminmod/redis-comparison-benchmarks) (v5, 2026)
- [redis-py 8.0 RESP3 default](https://newreleases.io/project/github/redis/redis-py/release/v8.0.0b2)
- [node-redis 6.0 RESP3 default](https://github.com/redis/node-redis/releases/tag/redis%406.0.0)
- [Garnet VLDB 2026](https://github.com/microsoft/garnet)
- [UCSC CacheBench](https://ucsc-ospo.github.io/report/osre25/harvard/cachebench/2025-08-06-haochengxia/)
- [SIEVE vs LRU bachelor thesis](https://www.cs.vu.nl/~wanf/theses/stampf-bscthesis.pdf) (VU Amsterdam, 2025)
