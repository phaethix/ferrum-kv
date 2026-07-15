# FerrumKV Dashboard — Product Design

> Version: 2.1
> Date: 2026-07-05
> Owner: Product
> Status: spec revised after engineering review — v0.5.0 scope locked

---

## 0. Version Roadmap

| Version | Content | Status |
|---------|---------|--------|
| **v0.5.0** | Level 1 (Live Overview) — 4 cards + AHE gauge + Eviction Log (simplified) + RESP2 Stream + Keyspace snapshot | **this spec** |
| **v0.5.1** | ops/s sparkline, Eviction Log EPS breakdown, 4 animations (static data) | deferred |
| **v0.6.0** | Sandbox (Level 2), SIEVE animation (blocked by FERRUM-009) | deferred |

This spec describes the **v0.5.0** deliverable. Sections marked `[v0.5.1]` or `[v0.6.0]` are included for design continuity but are explicitly out of scope for the current milestone.

---

## 1. Product Positioning

### What This Is

A **visual teaching dashboard** that makes FerrumKV's internals visible in real time. It's the visual proof of our core positioning claim: *"the most readable KV in existence."*

### What This Is NOT

| Competitor | What They Built | Why It's Different |
|-----------|----------------|---------------------|
| RedisInsight | Production monitoring for Redis Ops | Ops tool, not a learning tool. Shows KPIs, not internals. |
| Grafana + Redis Exporter | Metric dashboards | "What's the QPS?" — not "Why did that key get evicted?" |
| `redis-cli MONITOR` | Raw command stream | Indecipherable to non-experts. Teaches nothing. |
| Academic simulators (libCacheSim) | Eviction replay | Powerful but CLI-only, no live visualization |

### Unique Value Proposition

> Open `http://localhost:6381` and see what no other KV database shows you: **your cache's internals, alive.** Every metric has a plain-language explanation. No login. No config. No external dependencies.

### Target Users (in priority order)

1. **Learners** — "I want to understand how a KV store works by watching it run."
2. **Contributors** — "I want to see the impact of my code change on cache behavior in real time."
3. **Researchers** — "I want to see eviction decisions as they happen and understand why the algorithm chose each victim." (Full A/B sandbox deferred to v0.6.0)
4. **Power users** — "I want to understand my workload's access pattern to tune the eviction policy."

---

## 2. Design System

### 2.1 Visual Identity

| Element | Choice | Rationale |
|---------|--------|-----------|
| Theme | Dark (GitHub-dark palette) | Dev tool convention, reduces eye strain for long sessions |
| Primary accent | Blue (`#58a6ff`) | Data, trust, neutral |
| AHE accent | Purple (`#a371f7`) | Distinct visual identity for our unique algorithm |
| Success/winner | Green (`#3fb950`) | Eviction comparison winners |
| Typography | System monospace for data, system sans-serif for labels | Zero web fonts, loads instantly |
| Spacing | 8px grid | Clean, scannable |

### 2.2 Core UI Principles

1. **Everything has a tooltip.** Hover over any number or widget to see a plain-language explanation of what it means and why it matters. A beginner should be able to understand the entire dashboard without leaving the page.

2. **Show, then explain.** The default view is a clean overview. Click any section to expand a detailed view with inline educational content.

3. **Live, not stale.** All data updates in real time. Nothing requires a refresh button.

4. **Honest about what you're seeing.** If a number is sampled or approximate, the UI says so. No fake precision.

### 2.3 Viewing Modes

| Mode | Audience | v0.5.0 |
|------|----------|--------|
| **Overview** (default) | Everyone | ✅ delivered |
| **Deep Dive** | Learners, contributors | ✅ delivered (tooltips + inline explanations) |
| **Sandbox** | Researchers, power users | ❌ deferred to v0.6.0 |

### 2.4 Architecture Note (for Engineering)

The dashboard runs as a **second tokio listener in the same process**, sharing the existing `KvEngine` via its `Arc`-based `Clone`. Default port `127.0.0.1:6381`. Data is pushed via **Server-Sent Events (SSE)** over HTTP/1.1 — no WebSocket, no long-polling, zero new dependencies. The frontend is a single HTML file with embedded CSS and vanilla JavaScript, compiled into the binary.

---

## 3. Level 1 — Live Overview (v0.5.0)

### Purpose

Replace `redis-cli INFO` with a live dashboard that tells the story of your cache's health, performance, and behavior.

### Viewing Experience

The user opens `http://localhost:6381`. No login. No config. They immediately see their cache, alive.

### Top Bar

```
┌─────────────────────────────────────────────────────────────┐
│  🦀 FerrumKV v0.5.0         ▲ 6.2K ops/s     ⬆ 2h 14m      │
│  allkeys-ahe · 256MB cap    ● connected       since restart  │
└─────────────────────────────────────────────────────────────┘
```

Always visible: version, active eviction policy, memory cap, current ops/s, uptime, green dot when server is reachable.

> **Data source note for ops/s**: v0.5.0 uses a **lifetime average** (`total_commands / uptime_seconds`). Requires a new `AtomicU64` command counter ticked in `execute_command`. Rolling-window sparkline is deferred to v0.5.1.

### Four Core Metrics (Top Row)

```
┌─────────────────┐ ┌─────────────────┐ ┌─────────────────┐ ┌──────────────────┐
│    MEMORY        │ │   HIT RATE       │ │   EVICTIONS      │ │   OPS/SEC         │
│                 │ │                  │ │                 │ │                  │
│   45.3 MB       │ │     78.3%        │ │    1,247        │ │    6,241         │
│   / 256 MB      │ │                  │ │    total        │ │  (lifetime avg)  │
│   ███░░░░░░░   │ │                  │ │                 │ │                  │
│                 │ │   [?] How hit    │ │   [?] What is   │ │   [?] How ops/s  │
│   [?] About     │ │   rate is       │ │   counted as    │ │   is calculated  │
│   memory usage  │ │   calculated     │ │   an eviction   │ │                  │
└─────────────────┘ └─────────────────┘ └─────────────────┘ └──────────────────┘
```

**Data sources (all exist in current codebase):**

| Card | Data Source | Status |
|------|------------|--------|
| Memory | `engine.used_memory()` (AtomicU64) + `eviction_config().max_memory` | ✅ exists |
| Hit Rate | `engine.keyspace_stats()` → `hits / (hits + misses)` (two AtomicU64) | ✅ exists |
| Evictions | Needs a new `AtomicU64` counter ticked in `enforce_memory_limit` | 🔧 new, 1 line |
| Ops/sec | Needs a new `AtomicU64` counter ticked in `execute_command`; display as `total / uptime_secs` | 🔧 new, 2 lines |

Each card shows:
- **Primary value** (large, monospace)
- **Context** (percentage of max, or "lifetime avg" label)
- **Memory bar** (visual fill of max capacity — only memory has this)
- **Tooltip icon** `[?]` that reveals educational text on hover

> **v0.5.0 vs v0.5.1**: v0.5.0 ships with **no sparklines** on any card. Sparklines (rolling 60-second window charts) arrive in v0.5.1 after the ops/s rolling window infrastructure is built.

**Tooltip examples:**

> **How is hit rate calculated?**
> `hits / (hits + misses)` over the lifetime of the server.
> A "hit" means a key was found and was not expired.
> A "miss" means the key didn't exist or had already expired.
> This is the core metric that eviction policies try to optimize.
> Higher is better. 100% means every read found its key.

### AHE Adaptive Alpha (Only Visible When AHE Is Active)

```
┌─────────────────────────────────────────────────────────────────┐
│  🧠 AHE Adaptive Alpha                          [?] What is α?  │
│                                                                 │
│   0.0  ──────────────●──────────────  1.0                       │
│                      0.62                                        │
│                                                                 │
│   ← Frequency matters more        Recency matters more →        │
│                                                                 │
│   α = 0.62 means AHE currently weights recency 62% and          │
│   frequency 38% in its eviction decisions. α self-tunes         │
│   based on the observed hit ratio via the feedback loop         │
│   in AdaptiveHybridState::observe.                              │
│                                                                 │
│   ["How AHE tunes α" → opens animation (v0.5.1)]                │
└─────────────────────────────────────────────────────────────────┘
```

**Data source**: `engine.ahe_snapshot()` returns `{alpha, last_hit_ratio}`. Both fields already exist on `AdaptiveHybridState`. ✅

This is the signature widget. **No other KV database shows you its eviction algorithm thinking.** The slider is live-updating. When it moves, users can literally see the algorithm adapting to their workload.

> **v0.5.0 note**: The "How AHE tunes α" link opens a placeholder page. The full AHE EPS animation ships in v0.5.1.

### Dual Live Feed (Bottom Row, Side by Side)

```
┌──────────────────────────────┐ ┌──────────────────────────────────┐
│  📋 Eviction Log             │ │  ⚡ RESP2 Stream (live)            │
│                              │ │                                    │
│  14:03:21.432                │ │  → *3\r\n$3\r\nSET\r\n$4\r\n     │
│  ┌─────────────────────────┐ │ │       user\r\n$5\r\nhello\r\n    │
│  │ key: "user:921"         │ │ │  ← +OK\r\n                       │
│  │ policy: allkeys-ahe     │ │ │                                    │
│  │ TTL remaining: 12s      │ │ │  → *2\r\n$3\r\nGET\r\n$4\r\n     │
│  └─────────────────────────┘ │ │       user\r\n                    │
│                              │ │  ← $5\r\nhello\r\n               │
│  14:03:19.188                │ │                                    │
│  ┌─────────────────────────┐ │ │  → *3\r\n$6\r\nEXPIRE\r\n        │
│  │ key: "session:45"       │ │ │       $4\r\nuser\r\n$4\r\n60\r\n │
│  │ policy: allkeys-ahe     │ │ │  ← :1\r\n                        │
│  │ TTL remaining: 8s       │ │ │                                    │
│  └─────────────────────────┘ │ │  [?] This is the actual wire      │
│                              │ │  protocol. Every Redis client     │
│  14:03:18.021                │ │  speaks these bytes.              │
│  ┌─────────────────────────┐ │ │                                    │
│  │ key: "cache:332"        │ │ └──────────────────────────────────┘
│  │ policy: allkeys-ahe     │ │
│  │ TTL remaining: 45s      │ │
│  └─────────────────────────┘ │
│                              │
│  [?] Each eviction shows    │
│  which policy made the       │
│  decision and how much TTL   │
│  the key had left.           │
└──────────────────────────────┘
```

**Eviction Log (v0.5.0 simplified)**:

| Field | v0.5.0 | v0.5.1 |
|-------|--------|--------|
| key | ✅ (from `Candidate.key` in `enforce_memory_limit`) | ✅ |
| policy | ✅ (from `EvictionConfig::policy.name()`) | ✅ |
| TTL remaining | ✅ (from `Candidate.expire_at - now`) | ✅ |
| EPS score + component breakdown | ❌ deferred | ✅ |
| recency/frequency/TTL urgency sub-scores | ❌ deferred | ✅ |

**Data source**: `enforce_memory_limit` currently calls `pick_victim` and receives a `Candidate` but immediately drops all metadata (only `victim.key` is used). v0.5.0 adds a fixed-size `VecDeque<EvictionEvent>` ring buffer to the engine, populated with 3 fields per eviction. This is a ~3 line addition in `enforce_memory_limit`.

**RESP2 Stream**: Raw bytes flowing in and out. Right side shows `→` (client request) and `←` (server response). This demystifies the wire protocol.

**Data source**: `handle_client` in `server.rs` reads from and writes to a `TcpStream`. v0.5.0 adds an optional ring buffer tap in the read/write path, gated by an `AtomicBool` flag — enabled only when the dashboard is active, zero overhead (single `if` branch correctly predicted as "not taken") otherwise. 🔧

### Keyspace Distribution

```
┌─────────────────────────────────────────────────────────────────┐
│  📊 Keyspace Snapshot (sampled, ~200 keys)        [?]           │
│                                                                 │
│  TTL Distribution                                               │
│  [1s]   ▏                                                       │
│  [10s]  ▎                                                       │
│  [1m]   ████████████                                            │
│  [5m]   ████████████████████                                    │
│  [1h]   ██████████                                              │
│  [∞]    ████████████████████████████  ← no TTL set              │
│                                                                 │
│  63% of keys have no TTL. 22% expire within 5 minutes.          │
│  This matters for eviction: only 37% of keys are eligible       │
│  for volatile-* policies.                                       │
│                                                                 │
│  Key Size Distribution                    LFU Heatmap           │
│  [≤64B]  ████████████████████            cool ░░▒▒▓▓██ hot     │
│  [256B]  ████████                        [?] LFU counter       │
│  [1KB]   ████                             distribution         │
│  [4KB+]  ▏                                                     │
└─────────────────────────────────────────────────────────────────┘
```

**Data source**: New method `engine.keyspace_distribution()` — separate from the existing `expire_stats()`. `expire_stats` keeps its current `(expires_count, avg_ttl)` return type so the INFO command's `db0:keys=...,expires=...,avg_ttl=...` output is unaffected. The new method does the same O(n) scan under the read lock but returns bucketed distribution data for the dashboard. 🔧

**Why this matters**: The distributions explain *why* your eviction policy behaves a certain way. "Oh, most of my keys have no TTL — `volatile-lru` is useless for my workload." This turns a passive dashboard into an active learning experience.

### Navigation Bar

```
┌─────────────────────────────────────────────────────────────────┐
│  [📊 Dashboard]  [🧪 Sandbox →]  [🎬 Animations →]              │
│                        coming soon       coming soon              │
└─────────────────────────────────────────────────────────────────┘
```

v0.5.0 ships with **Dashboard only**. Sandbox and Animations tabs are visible but grayed out with "coming soon" labels. This sets user expectation and avoids a dead-end launch.

---

## 4. Level 2 — Eviction Sandbox [v0.6.0]

> **This entire section is deferred to v0.6.0.** Included here for design continuity. Do not implement for v0.5.0.

### Purpose

Let users compare two eviction policies side-by-side, on the same workload, and see which wins.

### User Journey

1. User clicks **🧪 Sandbox** tab
2. Configures: Policy A, Policy B, workload profile, cache size, duration, warmup
3. Clicks **▶ Run**
4. Watches two engines process the same traffic side-by-side in real time
5. After completion: summary table with automated plain-language analysis

### Scope Warning

The Sandbox requires:
- Two independent `KvEngine` instances in the same process
- A workload abstraction trait with 6 implementations (Zipfian, Zipfian+shift, hotspot, mixed R/W, sequential scan, burst)
- A driver that feeds both engines via direct API calls (not TCP)
- Dual metric snapshot channels

This is the largest single piece of the dashboard roadmap. It should be planned as its own milestone, not squeezed into a v0.5.x patch.

---

## 5. Level 3 — Teaching Animations [v0.5.1]

> **v0.5.0 ships zero animations.** The Animations tab is a "coming soon" placeholder. v0.5.1 delivers 4 static-data animations. SIEVE is blocked by FERRUM-009.

### Purpose

Step-by-step visual walkthroughs of core KV internals. Abstract concepts made concrete.

### Navigation

Accessible from the **🎬 Animations** tab (v0.5.1+), or from inline links throughout the dashboard.

### Animation Inventory

| # | Animation | What It Teaches | Data Source | v0.5.1 |
|---|-----------|----------------|-------------|--------|
| 1 | **RESP2: Bytes to Command** | How `*3\r\n$3\r\nSET\r\n...` becomes a parsed command | Static example data | ✅ |
| 2 | **AOF: Replay on Startup** | How the server rebuilds state from the append-only file | Can replay user's **actual AOF file** | ✅ |
| 3 | **AHE: How EPS Is Computed** | Recency × frequency × TTL urgency → eviction priority score | **Static example data** (real `eps_score` formula and constants, synthetic candidate values) | ✅ |
| 4 | **Expiration Sweeper** | TTL sampling cycle, lazy deletion, adaptive loop | Static example data + live sweeper metrics for context | ✅ |
| — | **SIEVE: Quick Demotion** | FIFO queue + hand pointer + visited flags | ❌ **blocked by FERRUM-009** — SIEVE not yet implemented | v0.6.0 |

> **v0.5.1 data model**: Animations #1, #3, #4 use **static, hardcoded example data** that illustrates the algorithm correctly. Animation #2 (AOF) can optionally replay the user's real AOF file from disk. No animation requires live EPS component capture from `enforce_memory_limit` — that infrastructure arrives in v0.5.1 and feeds both the upgraded Eviction Log and the AHE animation's live mode.

### Animation Player UX (applies to all 4)

```
┌─────────────────────────────────────────────────────────────────┐
│  🎬 [Animation Title]                            Step 3 of 8      │
│                                                                   │
│  ┌─────────────────────────────────────────────────────────────┐ │
│  │                                                             │ │
│  │              [visual state — boxes, arrows, colors]         │ │
│  │                                                             │ │
│  └─────────────────────────────────────────────────────────────┘ │
│                                                                   │
│  ┌─────────────────────────────────────────────────────────────┐ │
│  │  Step N: [one-paragraph explanation, jargon-light]          │ │
│  │                                                             │ │
│  │  💡 [One-sentence insight: "why this matters"]              │ │
│  └─────────────────────────────────────────────────────────────┘ │
│                                                                   │
│  [⏮ Prev]  [⏸ Pause]  [⏭ Next]  [🔄 Auto-play]  Speed: ●●○    │
└─────────────────────────────────────────────────────────────────┘
```

**Player controls:**
- **Step forward/back**: Manual walkthrough for deep reading
- **Auto-play**: Runs through all steps with configurable speed (0.5× / 1× / 2×)
- **Pause**: Freezes the current step for discussion or screenshot
- **Counter**: "Step 3 of 8" shows progress

### Animation 1: RESP2 — Bytes to Command

```
Step 1: Raw bytes arrive: *3\r\n$3\r\nSET\r\n$4\r\nuser\r\n$5\r\nhello\r\n
Step 2: Parser reads the first byte: '*' → this is an array
Step 3: Parser reads '3' → the array has 3 elements
Step 4: '$3' → first element is a bulk string of 3 bytes
Step 5: 'SET' → the command name
Step 6: '$4' + 'user' → first argument (the key)
Step 7: '$5' + 'hello' → second argument (the value)
Step 8: Complete: Command::Set { key: "user", value: "hello" }
```

### Animation 2: AOF — Replay on Startup

```
Step 1: Server starts. ferrum.aof exists. Opening file...
Step 2: Record 1: *3\r\n$3\r\nSET\r\n$3\r\nfoo\r\n$3\r\nbar\r\n
Step 3: Engine SET foo → bar. HashMap now has 1 key.
Step 4: Record 2: *3\r\n$3\r\nSET\r\n$4\r\nuser\r\n$5\r\nAlice\r\n
Step 5: Engine SET user → Alice. HashMap now has 2 keys.
...
Step N: End of file reached. Replay complete. Applied 42,381 records.
         HashMap contains 42,381 keys.
```

> **v0.5.1 enhancement**: This animation can read the user's actual `ferrum.aof` file and display real records from it. The replay logic already exists in `persistence/replay.rs`.

### Animation 3: AHE — How EPS Is Computed

Uses the **real `eps_score` function's formula and constants** (`RECENCY_HORIZON`, `TTL_SHORT_HORIZON`, `TTL_PENALTY`) from `eviction.rs`, with synthetic example candidates that demonstrate the trade-offs clearly:

```
Candidate A: "hot-recent-no-ttl"
  Recency: 0.91 (accessed 2s ago — very recent)
  Frequency: 0.88 (accessed 500+ times — very hot)
  TTL urgency: 0.00 (no TTL set — never expires)
  ─────────────────
  EPS = α(0.62)·0.91 + (1-α)(0.38)·0.88 + 0.00·TTL_PENALTY
      = 0.564 + 0.334 + 0.000
      = 0.898  ← HIGHEST → this key gets evicted!

💡 A hot, recent key with no TTL gets evicted because memory is full.
  AHE has to pick SOMETHING. Without TTL urgency, recency+frequency
  alone decide — and other candidates had higher EPS.

Candidate B: "warm-old-about-to-expire"
  Recency: 0.45 (accessed 30s ago — moderately recent)
  Frequency: 0.55 (accessed 50 times — moderately hot)
  TTL urgency: 0.97 (TTL: 3s remaining — about to expire!)
  ─────────────────
  EPS = α(0.62)·0.45 + (1-α)(0.38)·0.55 + 0.97·TTL_PENALTY
      = 0.279 + 0.209 + TTL_penalty
      = 0.488 + TTL_penalty

💡 Notice how TTL urgency can push a "warm" key's score above a
  "hot" key's. This is AHE's TTL-awareness in action: why keep
  something that's about to die naturally?
```

> **Engineering note**: The exact numeric values, constants, and formula MUST match `eps_score` in `src/storage/eviction.rs:223-251`. The animation hardcodes the formula with the real constants. Do not use the illustrative numbers from this spec — dereference the source code.

### Animation 4: Expiration Sweeper

```
Step 1: Sweeper wakes up (100ms tick). Samples 20 random keys.
Step 2: Key "a" → TTL: 0s remaining → EXPIRED → remove.   (1/20)
Step 3: Key "b" → TTL: 5m remaining → keep.
Step 4: Key "c" → TTL: 0s remaining → EXPIRED → remove.   (2/20)
...
Step 20: 6/20 expired (30%). 30% > 25% threshold → run another round immediately!
💡 Redis' adaptive strategy: if >25% of the sample is expired, the sweeper
doesn't sleep — it runs again. This handles the case where a large batch
of keys all expire around the same time.
```

---

## 6. User Journeys

### Journey 1: First-Time Learner (v0.5.0)

```
1. Clones repo, runs `cargo run --release`
2. Terminal says: "Dashboard: http://localhost:6381"
3. Opens browser → sees the live overview
4. Thinks: "Cool, I see my cache's stats"
5. Runs `redis-benchmark -n 10000 -t set,get` in another terminal
6. Watches the dashboard respond — memory fills, hit rate climbs, ops/s rises
7. Hovers over "Hit Rate" → reads the tooltip, learns what hit/miss mean
8. Notices AHE alpha slider moving → thinks "wait, it's self-tuning?"
9. Reads the alpha tooltip → learns how adaptive eviction works
10. Understands the basics of how a KV cache behaves. Takes 5 minutes.
```

### Journey 2: Contributor (v0.5.0)

```
1. Makes a change to the eviction code
2. Runs ferrum-kv locally, opens dashboard
3. Runs `redis-benchmark` to generate load
4. Watches eviction log — sees which keys are getting evicted under their change
5. Checks hit rate — did it improve or regress?
6. Iterates on the code with immediate visual feedback
```

### Journey 3: Researcher (v0.6.0, Sandbox required)

```
1. Starts ferrum-kv with --maxmemory 64mb
2. Navigates to Sandbox tab
3. Selects Policy A: allkeys-lru, Policy B: allkeys-ahe
4. Workload: Zipfian α=1.0, 80/20 R/W, 60s
5. Clicks Run
6. Watches side-by-side comparison
7. After 60s: sees automated analysis explaining why AHE won
8. Clicks "Copy Results (Markdown)"
9. Pastes into their paper draft
```

---

## 7. Non-Functional Requirements

### Architecture
- Dashboard HTTP listener runs **in-process**, sharing the existing tokio runtime and `KvEngine` (via `Arc` clone)
- Default bind: `127.0.0.1:6381`
- Data push model: **SSE (Server-Sent Events)** over HTTP/1.1 — uses `tokio::io` (already a dependency), zero new crates
- Frontend: single HTML file, embedded CSS and vanilla JS, compiled into binary

### Performance
- Dashboard must not affect the RESP2 server's throughput by more than 2%
- Page load time <100ms on localhost
- Metrics refresh at 2Hz (500ms interval)
- RESP2 Stream tap: zero overhead when dashboard is not active; ring buffer bounded to 200 entries when active
- Eviction event ring buffer: fixed 200 entries, no allocation after init

### Security
- Dashboard binds to `127.0.0.1` by default — no network exposure
- If user explicitly binds to `0.0.0.0`, show a warning in the logs
- Dashboard is read-only. No endpoints that modify engine state.

### Accessibility
- All text is actual text (not images of text) — screen-reader friendly
- Color is never the sole indicator
- Sufficient contrast ratio (WCAG AA for text)

### Compatibility
- Works in Chrome, Firefox, Safari (last 2 major versions)
- Responsive down to 1024px wide (not mobile — this is a desktop dev tool)

---

## 8. Competitive Comparison

| Feature | `redis-cli INFO` | RedisInsight | Grafana + Redis Exporter | FerrumKV Dashboard |
|---------|-----------------|--------------|--------------------------|-------------------|
| Real-time metrics | ❌ (one-shot) | ✅ | ✅ (polling) | ✅ (SSE push) |
| Eviction event log | ❌ | ❌ | ❌ | ✅ (v0.5.0, simplified) |
| AHE alpha visualization | ❌ | ❌ | ❌ | ✅ (v0.5.0, live) |
| RESP2 wire-level visibility | ❌ | ❌ | ❌ | ✅ (v0.5.0) |
| Keyspace distribution | ❌ | ❌ | ❌ | ✅ (v0.5.0, sampled) |
| Algorithm animations | ❌ | ❌ | ❌ | ✅ (v0.5.1, static data) |
| Policy A/B testing | ❌ | ❌ | ❌ | ✅ (v0.6.0) |
| Built-in educational content | ❌ | ❌ | ❌ | ✅ (tooltips everywhere) |
| Requires external process | N/A | ✅ (separate app) | ✅ (Grafana + Prometheus) | ❌ (built-in, same process) |

---

## 9. Success Metrics

### v0.5.0
- A first-time user opens the dashboard and stays for >3 minutes
- All 4 metric cards display live data from a running ferrum-kv instance
- Eviction Log shows actual eviction events when memory cap is hit
- RESP2 Stream shows actual wire bytes during a `redis-benchmark` run
- Zero new crate dependencies added
- `resp2_bench.rs` confirms: dashboard tap inactive → throughput unchanged within ±1%

### v0.5.1
- A user steps through at least 1 animation
- A user clicks "How AHE tunes α" and watches the EPS computation walkthrough

### v0.6.0
- A user runs the sandbox and compares at least 2 policies
- A contributor includes a dashboard screenshot in a PR

---

## 10. What We're NOT Building (Scope Boundaries)

- ❌ **No write access.** The dashboard reads metrics. It does not execute commands against the engine.
- ❌ **No authentication.** This is a localhost dev tool.
- ❌ **No persistent history.** Sparklines reset on page refresh. Eviction log is a ring buffer — old entries are dropped.
- ❌ **No alerting.** No webhooks, no notifications.
- ❌ **No multi-server view.** Single node only.
- ❌ **No WebSocket.** SSE over HTTP/1.1 keeps the dependency count at 6.
- ❌ **No JavaScript framework.** Single HTML file, vanilla JS.

---

*End of product spec v2.1. Aligned with codebase reality as of 2026-07-05.*
