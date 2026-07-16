# Adaptive Hybrid Eviction (AHE)

> A self-tuning cache eviction algorithm that blends recency, frequency, and TTL urgency into a single score — and adapts its own weights from live hit-ratio feedback.

- **Status:** Adaptive hybrid algorithm, shipped since `v0.3`
- **Source:** [`src/storage/eviction.rs`](https://github.com/phaethix/ferrum-kv/blob/master/src/storage/eviction.rs)
- **Related:** [Whitepaper §9.5](https://github.com/phaethix/ferrum-kv/blob/master/docs/design/whitepaper.md)

---

## Abstract

Classic cache eviction policies force a fixed trade-off. **LRU** reacts fast to
bursty traffic but is defenceless against scan pollution and cannot recognise
long-lived hotspots. **LFU** protects stable hot keys but suffers a *cold-start*
penalty: a freshly arrived hot key has a low frequency and is evicted before it
ever warms up. Neither adapts when the access pattern shifts mid-run.

**AHE (Adaptive Hybrid Eviction)** resolves this by scoring every candidate with
a single **Eviction Priority Score (EPS)** that fuses three signals:

- **recency** — how long since the key was last touched (LRU intuition),
- **infrequency** — how rarely the key is accessed (LFU intuition),
- **TTL urgency** — whether the key is about to expire anyway.

The blend weight `alpha` between recency and frequency is **not a tunable knob the
operator must set** — it is driven by a feedback controller that watches the
engine's observed hit ratio and nudges `alpha` toward whatever the current
workload rewards. The result is one eviction policy that behaves like LRU under
bursts and like LFU under stable热度, without any human intervention.

---

## 1. Motivation

| Workload | LRU | LFU | Problem |
| -------- | :-: | :-: | ------- |
| Bursty hotspot (flash sale) | ✅ | ❌ | LFU cold-starts the new hot key and evicts it |
| Stable hot key | ❌ | ✅ | LRU drops a long-lived hot key after one idle read |
| Scan pollution (full table scan) | ❌ | ✅ | LRU lets cold scan keys evict hot data |
| Key about to expire | ❌ | ❌ | Neither is TTL-aware; a dying key wastes a slot |
| Shifting access pattern | ❌ | ❌ | A fixed policy cannot follow the change |

**Core idea.** For each sampled candidate compute a unified score; the key with
the **highest score is the most evictable**. Separately, a controller adjusts the
recency/frequency weight from observed hit ratio.

---

## 2. The Eviction Priority Score (EPS)

```text
EPS = alpha · recency + (1 − alpha) · infrequency + ttl_penalty
```

Higher EPS ⇒ more evictable. Each term is normalised to `[0, 1]` so the three
signals are directly comparable.

| Term | Definition | Meaning |
| ---- | ---------- | ------- |
| `recency` | `min((now − last_access) / 600s, 1.0)` | Higher for keys untouched for longer |
| `infrequency` | `1 − lfu_counter / 255` | Higher for colder keys (shared Morris counter) |
| `ttl_penalty` | `+0.2` if remaining TTL ≤ 30s **or** already expired; else `0` | Lets AHE prefer keys that are doomed anyway |
| `alpha` | adaptive weight, clamped to `[0.05, 0.95]` | `→ 1.0` favours LRU, `→ 0.0` favours LFU |

`recency` uses a 600-second horizon (`RECENCY_HORIZON`): a key idle for 10 minutes
is maximally "stale" and scores `1.0` regardless of how much longer it waits.
`infrequency` reuses the **same** `lfu_counter` that powers the `*-lfu` policies
(Morris counter, `0..=255`), so AHE adds **zero** extra per-key memory.

> **Design note.** An earlier draft used a `log2` frequency normalisation and a
> `1/(1+elapsed)` time decay. The shipped version instead shares the LFU Morris
> counter and a linear recency gradient — this keeps AHE's metadata footprint at
> exactly zero and ties it directly to Redis-compatible structures.

---

## 3. Adaptive Weight Controller (alpha)

`alpha` lives in `AdaptiveHybridState`, a 1-dimensional gradient search driven by
the engine's hit ratio. The engine calls `observe(hits, misses)` after each
successful eviction; once `window_size` (default **64**) samples accumulate, the
controller updates `alpha`.

| Field | Default | Role |
| ----- | ------- | ---- |
| `alpha` | `0.5` | Current recency/frequency blend |
| `step` | `0.05` | Magnitude of each adjustment |
| `direction` | `+1.0` | Sign of the last move; flipped on regression |
| `last_hit_ratio` | `0.0` | Previous window's hit ratio (regression detector) |
| `window_size` | `64` | Samples between adjustments (noise filter) |
| `window_count` | `0` | Samples seen in the current window |

```text
observe(hits, misses):
    window_count += 1
    if window_count < window_size:
        return                       # still gathering samples

    ratio = hits / (hits + misses)
    if ratio + 1e-4 < last_hit_ratio:
        direction = -direction       # hit ratio dropped → reverse course
    alpha = clamp(alpha + direction · step, 0.05, 0.95)
    last_hit_ratio = ratio
    window_count = 0
```

**Properties**

- **Objective is hit ratio, not access distribution.** No per-key skew metrics
  are needed; `keyspace_hits / (keyspace_hits + keyspace_misses)` is the target.
- **Flip-on-regression gradient search.** Only the last move's sign is stored;
  when the hit ratio retreats, the sign flips — a lightweight stand-in for
  estimating the gradient.
- **Guard rails.** `alpha` is clamped to `[0.05, 0.95]`, so AHE never degenerates
  into pure LRU or pure LFU.
- **Self-paced.** Because `observe` fires per eviction, the adaptation rate tracks
  write pressure naturally.

<figure class="kv-diagram">
  <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 860 360" role="img" aria-label="AHE adaptive alpha controller flow">
    <defs>
      <linearGradient id="aheFlowPrimary" x1="0" y1="0" x2="1" y2="1">
        <stop offset="0%" stop-color="#6366f1"/>
        <stop offset="100%" stop-color="#8b5cf6"/>
      </linearGradient>
      <marker id="aheFlowArrow" markerWidth="10" markerHeight="10" refX="8" refY="3" orient="auto">
        <path d="M0,0 L0,6 L9,3 z" fill="#94a3b8"/>
      </marker>
    </defs>
    <rect width="860" height="360" rx="18" fill="#0f172a"/>
    <text x="430" y="34" text-anchor="middle" fill="#e2e8f0" font-size="17" font-weight="700">Adaptive alpha controller</text>
    <text x="430" y="58" text-anchor="middle" fill="#94a3b8" font-size="12">one lightweight feedback loop, updated every eviction window</text>
    <rect x="58" y="122" width="170" height="54" rx="14" fill="url(#aheFlowPrimary)"/>
    <text x="143" y="144" text-anchor="middle" fill="#fff" font-size="12" font-weight="700">observe(hits, misses)</text>
    <text x="143" y="162" text-anchor="middle" fill="#ddd6fe" font-size="11">per eviction</text>
    <path d="M288 120 L348 149 L288 178 L228 149 Z" fill="#111827" stroke="#475569"/>
    <text x="288" y="145" text-anchor="middle" fill="#e2e8f0" font-size="12" font-weight="700">window</text>
    <text x="288" y="161" text-anchor="middle" fill="#e2e8f0" font-size="12" font-weight="700">&gt;= 64?</text>
    <rect x="408" y="122" width="164" height="54" rx="14" fill="#111827" stroke="#475569"/>
    <text x="490" y="144" text-anchor="middle" fill="#e2e8f0" font-size="12" font-weight="700">ratio = hits /</text>
    <text x="490" y="162" text-anchor="middle" fill="#e2e8f0" font-size="12" font-weight="700">(hits + misses)</text>
    <path d="M632 120 L704 149 L632 178 L560 149 Z" fill="#111827" stroke="#475569"/>
    <text x="632" y="145" text-anchor="middle" fill="#e2e8f0" font-size="12" font-weight="700">ratio</text>
    <text x="632" y="161" text-anchor="middle" fill="#e2e8f0" font-size="12" font-weight="700">regressed?</text>
    <rect x="594" y="236" width="160" height="46" rx="13" fill="#1e1b4b" stroke="#6366f1"/>
    <text x="674" y="263" text-anchor="middle" fill="#ddd6fe" font-size="12" font-weight="700">flip direction</text>
    <rect x="284" y="236" width="230" height="46" rx="13" fill="#111827" stroke="#475569"/>
    <text x="399" y="263" text-anchor="middle" fill="#e2e8f0" font-size="12" font-weight="700">alpha = clamp(alpha + direction·step)</text>
    <rect x="76" y="236" width="150" height="46" rx="13" fill="#111827" stroke="#475569"/>
    <text x="151" y="263" text-anchor="middle" fill="#94a3b8" font-size="12" font-weight="700">keep alpha</text>
    <path d="M228 149 L238 149" stroke="#94a3b8" stroke-width="2" marker-end="url(#aheFlowArrow)"/>
    <path d="M348 149 L408 149" stroke="#94a3b8" stroke-width="2" marker-end="url(#aheFlowArrow)"/>
    <path d="M572 149 L560 149" stroke="#94a3b8" stroke-width="2" marker-end="url(#aheFlowArrow)"/>
    <path d="M632 178 L662 232" stroke="#94a3b8" stroke-width="2" marker-end="url(#aheFlowArrow)"/>
    <path d="M594 259 L518 259" stroke="#94a3b8" stroke-width="2" marker-end="url(#aheFlowArrow)"/>
    <path d="M632 178 C620 216 512 216 468 235" stroke="#94a3b8" stroke-width="2" fill="none" marker-end="url(#aheFlowArrow)"/>
    <path d="M288 178 C278 214 205 216 170 235" stroke="#94a3b8" stroke-width="2" fill="none" marker-end="url(#aheFlowArrow)"/>
    <text x="372" y="140" fill="#94a3b8" font-size="11">yes</text>
    <text x="249" y="211" fill="#94a3b8" font-size="11">no</text>
    <text x="654" y="210" fill="#94a3b8" font-size="11">yes</text>
    <text x="520" y="218" fill="#94a3b8" font-size="11">no</text>
  </svg>
</figure>

---

## 4. Complexity & Memory

| Step | Cost | Notes |
| ---- | ---- | ----- |
| Per-candidate score | `O(1)` | recency + infrequency + ttl_penalty, all inline |
| Victim selection | `O(k)` | `k = maxmemory-samples` (default 5) |
| Extra per-key memory | **0 bytes** | reuses `lfu_counter` and `last_access` |
| Controller update | `O(1)` amortised | once per `window_size` evictions |

This matches Redis' sampling-based approach (Redis 6+ also samples
`maxmemory-samples` random keys instead of maintaining a global heap/list) while
adding adaptivity at no memory cost.

<figure class="kv-diagram">
  <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 860 300" role="img" aria-label="AHE eviction path">
    <defs>
      <linearGradient id="aheEvictPrimary" x1="0" y1="0" x2="1" y2="1">
        <stop offset="0%" stop-color="#6366f1"/>
        <stop offset="100%" stop-color="#8b5cf6"/>
      </linearGradient>
      <marker id="aheEvictArrow" markerWidth="10" markerHeight="10" refX="8" refY="3" orient="auto">
        <path d="M0,0 L0,6 L9,3 z" fill="#94a3b8"/>
      </marker>
    </defs>
    <rect width="860" height="300" rx="18" fill="#0f172a"/>
    <text x="430" y="34" text-anchor="middle" fill="#e2e8f0" font-size="17" font-weight="700">Sampling-based eviction path</text>
    <text x="430" y="58" text-anchor="middle" fill="#94a3b8" font-size="12">Redis-style random sampling, plus adaptive scoring</text>
    <rect x="46" y="112" width="132" height="54" rx="13" fill="url(#aheEvictPrimary)"/>
    <text x="112" y="135" text-anchor="middle" fill="#fff" font-size="12" font-weight="700">maxmemory</text>
    <text x="112" y="153" text-anchor="middle" fill="#ddd6fe" font-size="11">exceeded</text>
    <rect x="226" y="112" width="132" height="54" rx="13" fill="#111827" stroke="#475569"/>
    <text x="292" y="135" text-anchor="middle" fill="#e2e8f0" font-size="12" font-weight="700">sample k</text>
    <text x="292" y="153" text-anchor="middle" fill="#e2e8f0" font-size="12" font-weight="700">random keys</text>
    <rect x="406" y="112" width="158" height="54" rx="13" fill="#111827" stroke="#475569"/>
    <text x="485" y="135" text-anchor="middle" fill="#e2e8f0" font-size="12" font-weight="700">score candidates</text>
    <text x="485" y="153" text-anchor="middle" fill="#94a3b8" font-size="11">EPS(alpha, key, now)</text>
    <rect x="612" y="112" width="150" height="54" rx="13" fill="#1e1b4b" stroke="#6366f1"/>
    <text x="687" y="135" text-anchor="middle" fill="#ddd6fe" font-size="12" font-weight="700">argmax(EPS)</text>
    <text x="687" y="153" text-anchor="middle" fill="#ddd6fe" font-size="11">victim key</text>
    <rect x="432" y="214" width="158" height="46" rx="13" fill="#111827" stroke="#475569"/>
    <text x="511" y="241" text-anchor="middle" fill="#e2e8f0" font-size="12" font-weight="700">evict + observe</text>
    <rect x="224" y="214" width="146" height="46" rx="13" fill="#111827" stroke="#475569"/>
    <text x="297" y="241" text-anchor="middle" fill="#94a3b8" font-size="12" font-weight="700">adjust if full</text>
    <path d="M178 139 L226 139" stroke="#94a3b8" stroke-width="2" marker-end="url(#aheEvictArrow)"/>
    <path d="M358 139 L406 139" stroke="#94a3b8" stroke-width="2" marker-end="url(#aheEvictArrow)"/>
    <path d="M564 139 L612 139" stroke="#94a3b8" stroke-width="2" marker-end="url(#aheEvictArrow)"/>
    <path d="M687 166 C687 215 628 237 594 237" stroke="#94a3b8" stroke-width="2" fill="none" marker-end="url(#aheEvictArrow)"/>
    <path d="M432 237 L370 237" stroke="#94a3b8" stroke-width="2" marker-end="url(#aheEvictArrow)"/>
    <path d="M224 237 C160 237 126 196 112 169" stroke="#475569" stroke-width="2" fill="none" stroke-dasharray="5 5" marker-end="url(#aheEvictArrow)"/>
  </svg>
</figure>

---

## 5. Comparison with Classic Policies

| Dimension | LRU | LFU | **AHE (FerrumKV)** |
| --------- | :-: | :-: | :----------------: |
| Score compute | `O(1)` | `O(1)` (Morris) | `O(1)` |
| Selection | `O(k)` sample | `O(k)` sample | **`O(k)`**, `k = samples` |
| Extra memory | — | — | — (reuses LFU fields) |
| Burst hotspot | ✅ | ❌ | ✅ alpha → LRU |
| Stable hotspot | ❌ | ✅ | ✅ alpha → LFU |
| Scan resistance | ❌ | ✅ | ✅ frequency floors it |
| TTL-aware | ❌ | ❌ | ✅ `ttl_penalty` |
| Pattern shift | ❌ | ❌ | ✅ adapts live |
| Tunable params | none | `lfu-log-factor`, `lfu-decay-time` | alpha init, step, window, samples |

---

## 6. Convergence Behaviour

Under a workload that switches from a **burst** (favouring recency) to a
**stable hotspot** (favouring frequency), the controller walks `alpha` toward
LRU first, then reverses toward LFU, oscillating in a shrinking band around the
value the current mix rewards. The trace below is **illustrative** — the exact
path depends on the hit-ratio signal, which you can watch live via
`INFO memory` (`ahe_alpha`, `ahe_last_hit_ratio`).

<figure class="kv-diagram">
  <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 860 360" role="img" aria-label="AHE alpha under a shifting workload chart">
    <rect width="860" height="360" rx="18" fill="#0f172a"/>
    <text x="430" y="34" text-anchor="middle" fill="#e2e8f0" font-size="17" font-weight="700">AHE alpha under a shifting workload</text>
    <text x="430" y="58" text-anchor="middle" fill="#94a3b8" font-size="12">illustrative convergence path: LRU bias first, then LFU bias</text>
    <line x1="90" y1="286" x2="790" y2="286" stroke="#475569"/>
    <line x1="90" y1="86" x2="90" y2="286" stroke="#475569"/>
    <g stroke="#1f2937" stroke-width="1">
      <line x1="90" y1="246" x2="790" y2="246"/>
      <line x1="90" y1="206" x2="790" y2="206"/>
      <line x1="90" y1="166" x2="790" y2="166"/>
      <line x1="90" y1="126" x2="790" y2="126"/>
      <line x1="90" y1="86" x2="790" y2="86"/>
    </g>
    <text x="58" y="290" fill="#94a3b8" font-size="11">0.0</text>
    <text x="58" y="190" fill="#94a3b8" font-size="11">0.5</text>
    <text x="58" y="90" fill="#94a3b8" font-size="11">1.0</text>
    <text x="430" y="326" text-anchor="middle" fill="#94a3b8" font-size="12">Eviction window (×64 evictions)</text>
    <text x="24" y="190" text-anchor="middle" fill="#94a3b8" font-size="12" transform="rotate(-90 24 190)">alpha</text>
    <polyline points="90,186 168,176 246,162 324,144 402,126 480,130 558,148 636,170 714,192 790,200" fill="none" stroke="#8b5cf6" stroke-width="4" stroke-linecap="round" stroke-linejoin="round"/>
    <polyline points="90,186 168,186 246,186 324,186 402,186 480,186 558,186 636,186 714,186 790,186" fill="none" stroke="#64748b" stroke-width="2" stroke-dasharray="6 6"/>
    <g fill="#c4b5fd">
      <circle cx="90" cy="186" r="4"/><circle cx="168" cy="176" r="4"/><circle cx="246" cy="162" r="4"/><circle cx="324" cy="144" r="4"/><circle cx="402" cy="126" r="4"/><circle cx="480" cy="130" r="4"/><circle cx="558" cy="148" r="4"/><circle cx="636" cy="170" r="4"/><circle cx="714" cy="192" r="4"/><circle cx="790" cy="200" r="4"/>
    </g>
    <rect x="604" y="90" width="150" height="54" rx="12" fill="#111827" stroke="#475569"/>
    <circle cx="624" cy="110" r="4" fill="#8b5cf6"/><text x="638" y="114" fill="#e2e8f0" font-size="12">AHE self-tuning</text>
    <line x1="620" y1="130" x2="632" y2="130" stroke="#64748b" stroke-width="2" stroke-dasharray="6 6"/><text x="638" y="134" fill="#e2e8f0" font-size="12">fixed weight</text>
  </svg>
</figure>

The flat line is a fixed-weight policy for reference; AHE's line shows the
self-tuning excursion and settle.

---

## 7. Configuration & Observability

Enable AHE with a single flag — no algorithm-specific tuning required:

```bash
./ferrum-kv --maxmemory 256mb --maxmemory-policy allkeys-ahe
# or the TTL-scoped variant:
./ferrum-kv --maxmemory 256mb --maxmemory-policy volatile-ahe
```

| Knob | Default | Notes |
| ---- | ------- | ----- |
| `maxmemory-policy` | `noeviction` | set `allkeys-ahe` / `volatile-ahe` to enable |
| `maxmemory-samples` | `5` | candidates sampled per eviction (shared with `*-lru`/`*-lfu`) |
| `step` / `window_size` / `direction` | `0.05` / `64` / `+1` | internal, via `AdaptiveHybridState::default()` |

Watch the controller converge in real time:

```text
$ redis-cli -p 6380 INFO memory
# Memory
used_memory:268435456
maxmemory:268435456
maxmemory_policy:allkeys-ahe
ahe_alpha:0.71
ahe_last_hit_ratio:0.94
```

---

## 8. Further Reading

- Implementation: [`src/storage/eviction.rs`](https://github.com/phaethix/ferrum-kv/blob/master/src/storage/eviction.rs) — `eps_score`, `pick_victim_ahe`, `AdaptiveHybridState`
- Full design discussion: [Whitepaper §9.5](https://github.com/phaethix/ferrum-kv/blob/master/docs/design/whitepaper.md)
- Try it live: the [built-in dashboard](/guide/dashboard) plots `ahe_alpha` as the workload changes.
