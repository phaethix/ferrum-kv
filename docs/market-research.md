# Market Research Findings — FerrumKV

> Date: 2026-07-04
> Scope: Rust embedded KV landscape, eviction algorithm SOTA, Redis ecosystem 2025–2026
> Purpose: Inform product strategy revision

---

## 1. Rust Embedded KV Landscape

### Direct Competitor: kevy

**[kevy](https://github.com/goliajp/kevy)** (v1.3.0, June 2026) is the most relevant project to FerrumKV:

| Dimension | kevy | FerrumKV (current) |
|-----------|------|--------------------|
| Dependencies | **Zero** (pure `std` + hand-bound syscalls) | 6 (tokio, signal-hook, log, env_logger, indexmap) |
| Architecture | Thread-per-core, shared-nothing, io_uring | Tokio multi-threaded, Arc<RwLock<HashMap>> |
| Binary size | 768 KB | ~2.0 MB |
| Commands | **98** (all 5 Redis types + pub/sub + WATCH) | 15 (string only) |
| Embedded GET latency | **54ns** | ~400μs (network round-trip) |
| Server throughput (GET, c=50, P=16) | **4.4M ops/s** | ~350K ops/s |
| Eviction policies | TTL-based only | **10 policies (incl. AHE)** |
| Code readability | Complex (custom syscalls, slab allocators) | **Clean, layered, documented** |
| Learning value | Low (too optimized) | **High** |

**Strategic implication**: FerrumKV cannot and should not compete on throughput, command count, or binary size. kevy has already won those dimensions. The counter-positioning is: **eviction algorithm depth + code readability + teaching value**.

### Other Rust Embedded KV Projects

| Project | Positioning | Key Stat | FerrumKV Advantage |
|---------|------------|----------|-------------------|
| emdb v1.0 | Fastest embedded KV | 3.2x redb bulk load | No Redis compatibility |
| redb v4.1 | Mature ACID embedded DB | MVCC, stable format | No RESP2, different use case |
| FeOx DB v0.1 | Ultra-low latency | 180ns GET | Very new, no ecosystem |
| fast-cache v0.1 | Thread-per-core, GPU | 31M ops/s FCNP | Very new, AI/ML focused |
| SurrealKV | Versioned LSM-tree | Time-travel queries | Not a cache, different segment |

### Server-Side Landscape

| System | Throughput (GET, c=50, P=16) | Best For |
|--------|------------------------------|----------|
| Dragonfly 1.37 | ~3.5M ops/s | Multi-core throughput, memory efficiency |
| Valkey 9.1 | ~2.5M ops/s | Open-source governance, drop-in Redis |
| Redis 8.4 | ~2.3M ops/s | Ecosystem maturity, managed cloud |
| KeyDB 6.3 | ~2.0M ops/s | Active-active replication |
| **FerrumKV** | **~350K ops/s** | **Embedding, eviction research, teaching** |

---

## 2. Eviction Algorithm State of the Art

### The Key Papers (2024–2025)

#### SIEVE (NSDI'24) — THE NEW BASELINE

- **Venue**: NSDI 2024 (USENIX)
- **Authors**: Zhang et al., CMU
- **Algorithm**: One FIFO queue + one pointer ("hand") + one boolean per entry
- **Results**: Beats 9 SOTA algorithms on 45%+ of 1,559 production traces; 2x LRU throughput
- **Complexity**: ~20 lines of code
- **Key insight**: Quick demotion — one missed access and you're out. Counter-intuitively effective because most real-world workloads have strong temporal locality where "visited recently = will be visited again soon" holds.
- **FerrumKV action**: Implement as `allkeys-sieve` + TTL-aware variant `allkeys-sieve-s`

#### AdaptiveClimb (arXiv 2511.21235, 2025) — CONTROL-THEORETIC ADAPTIVE

- **Algorithm**: Single parameter `jump` — hit → decrease (promote faster), miss → increase (delay promotion)
- **DynamicAdaptiveClimb** extension: auto-resizes cache based on `jump` vs capacity ratio
- **Results**: Outperforms ARC, CACHEUS, SIEVE, LIRS, TinyLFU on 1,067 production traces; 35M ops/s @ 16 threads
- **Comparison to AHE**: AdaptiveClimb balances recency vs frequency with one parameter. AHE balances recency + frequency + **TTL urgency** with adaptive `alpha`. The TTL dimension is AHE's differentiator.
- **FerrumKV action**: Implement as additional policy; benchmark AHE vs AdaptiveClimb on TTL-heavy workloads

#### Cold-RL (arXiv 2508.12485, 2025) — RL IN PRODUCTION

- **Algorithm**: Dueling DQN served via ONNX sidecar, integrated into NGINX
- **Results**: 146% hit ratio improvement at 25MB cache; <2% CPU overhead; <500μs inference
- **Significance**: RL-based eviction is now practical for production. Not for FerrumKV v0.5, but validates the "eviction algorithm experimentation" positioning.
- **FerrumKV action**: Long-term aspirational. Could be a v0.9+ research project.

#### FreqRec (IEEE CICN 2025) — CPU CACHE FOCUSED

- **Algorithm**: Lightweight LFU-LRU hybrid for CPU cache blocks
- **Relevance**: Low for KV stores. CPU cache eviction has different constraints (fixed associativity, hardware complexity budget).

### Comparative Summary

| Algorithm | Recency | Frequency | TTL-Aware | Adaptive | Complexity | Year |
|-----------|---------|-----------|-----------|----------|------------|------|
| LRU | ✅ | ❌ | ❌ | ❌ | Medium | 1970s |
| LFU | ❌ | ✅ | ❌ | ❌ | Medium | 1970s |
| ARC | ✅ | ✅ | ❌ | ✅ | High | 2003 |
| SIEVE | ✅ | ❌ | ❌ | ❌ | **Trivial** | 2024 |
| AdaptiveClimb | ✅ | ✅ | ❌ | ✅ | Low | 2025 |
| **AHE (FerrumKV)** | ✅ | ✅ | ✅ | ✅ | Medium | 2025 |
| Cold-RL (DQN) | ✅ | ✅ | ✅ | ✅ | Very High | 2025 |

**Gap analysis**: SIEVE and AdaptiveClimb both lack TTL-awareness. AHE's unique contribution is integrating TTL into the adaptive scoring function. The "SIEVE-S" variant (SIEVE + TTL-aware demotion) is another FerrumKV original.

---

## 3. RESP Protocol Evolution

### RESP3 Adoption Timeline

| Date | Event |
|------|-------|
| 2018 | RESP3 spec first proposed (Redis 6.0) |
| 2024 | Valkey forks Redis, continues RESP3 support |
| May 2025 | Redis 8.0 released, stable RESP3 |
| Early 2026 | redis-py 8.0: RESP3 becomes **default** |
| June 2026 | node-redis 6.0: RESP3 becomes **default** |

### RESP3 Features Relevant to FerrumKV

| Feature | Benefit | Priority |
|---------|---------|----------|
| Typed replies (maps, sets, doubles, booleans, nulls) | Cleaner API for multi-type data | High |
| Push messages | Pub/Sub without dedicated connection | Medium |
| `HELLO 3` negotiation | Graceful fallback to RESP2 | High |
| Client-side caching | Lower latency for repeated reads | Medium |
| Streaming replies | Large dataset iteration | Low |

**Implication**: RESP2 is not going away (it's the universal fallback), but RESP3 is becoming the default in major client libraries. FerrumKV should add RESP3 support by v0.6 to remain relevant.

---

## 4. Key Strategic Insights

### 4.1 The embedded Redis-compatible space has a winner

kevy has solved the "fast, embeddable, Redis-compatible Rust KV" problem comprehensively. Any project positioning against kevy on performance or command count will lose.

### 4.2 Eviction algorithms are having a moment

Three major papers in 2024–2025 (SIEVE, AdaptiveClimb, Cold-RL) each with a different approach (simple heuristic, control theory, deep RL). No production KV offers all three for comparison. **This is FerrumKV's gap.**

### 4.3 TTL-awareness is the underexploited dimension

Neither SIEVE nor AdaptiveClimb handles TTL. Redis's volatile-* policies handle TTL but as a binary filter (only consider TTL keys), not as a continuous signal in the eviction score. AHE and SIEVE-S are genuinely novel in this dimension.

### 4.4 The teaching/readability niche is real and unoccupied

kevy is too optimized to learn from. Redis source is 100K+ lines of C. Dragonfly is complex C++. No project in the KV space explicitly optimizes for code readability and pedagogical value. This is a niche FerrumKV can own.

---

## 5. Revised Positioning (One-Pager)

```
FERRUMKV IS:
  - An eviction algorithm laboratory (13 policies + benchmark suite)
  - The most readable RESP2 server implementation in Rust
  - The home of AHE — the TTL-aware adaptive eviction algorithm
  - An embeddable KV library for Rust applications

FERRUMKV IS NOT:
  - The fastest RESP2 server (kevy is 2.7x faster)
  - The most command-complete (kevy has 98 commands)
  - A distributed cache (use Valkey/Dragonfly)
  - A Redis replacement for large-scale deployments

THE BET:
  The market has enough fast KV stores. It doesn't have one that
  helps you understand how KV stores work, or lets you experiment
  with eviction algorithms in a real server environment.
```

---

*Sources: see Appendix A in docs/product-strategy.md*
