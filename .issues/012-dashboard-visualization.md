---
id: FERRUM-012
title: "Build interactive visual dashboard for real-time internals visualization"
severity: medium
status: open
component: network
found_date: 2026-07-04
reporter: PM Design
---

## Summary

A visual teaching dashboard that makes FerrumKV's internals visible in real time. v0.5.0 delivers the Live Overview. Sandbox (v0.6.0) and Animations (v0.5.1) are deferred.

See [`docs/dashboard-design.md`](../docs/dashboard-design.md) for the complete product spec (v2.1, aligned with engineering review).

## v0.5.0 — Live Overview

Replace `redis-cli INFO` with a real-time visual dashboard.

**What it ships:**
- Four core metric cards (memory, hit rate, evictions, ops/s-lifetime-avg) with educational tooltips, **no sparklines**
- AHE Adaptive Alpha gauge — live-updating slider from `ahe_snapshot()` (data source ✅ exists)
- Eviction Log — **simplified**: key + policy + TTL remaining. **No EPS breakdown** (data source 🔧 new ring buffer, 3 fields)
- RESP2 Stream — raw wire protocol bytes flowing in and out (data source 🔧 optional tap in `handle_client`)
- Keyspace Distribution — sampled TTL, key size, LFU heatmap (data source 🔧 extends existing `expire_stats` scan)
- Sandbox and Animations tabs visible but grayed out with "coming soon"

**Key UX principles:**
- Everything has a tooltip. Hover to learn what any number means.
- Real-time push via SSE (not polling). No refresh button.
- Dark theme, monospace for data, system fonts.
- Same process, second tokio listener on `127.0.0.1:6381`. Zero new dependencies.

## v0.5.1 — Animations + Sparklines [deferred]

- ops/s rolling window + sparklines on all 4 metric cards
- Eviction Log upgraded to full EPS component breakdown
- 4 animations (RESP2, AOF, AHE, Expiration Sweeper) — all use **static example data**

## v0.6.0 — Sandbox [deferred]

- Dual `KvEngine` instances, identical workload, side-by-side comparison
- 6 workload profiles (Zipfian, Zipfian+shift, hotspot, mixed R/W, sequential scan, burst)
- Post-experiment auto-analysis + one-click Markdown export
- SIEVE animation (blocked by FERRUM-009)

## Scope Boundaries

- ❌ No write access — read-only metrics, no command execution from browser
- ❌ No auth — localhost dev tool, binds 127.0.0.1 by default
- ❌ No persistent history — eviction log is a fixed-size ring buffer
- ❌ No alerting — not a monitoring tool
- ❌ No multi-server — single node only
- ❌ No WebSocket or JS framework — SSE + vanilla JS, zero new crates

## Verification (v0.5.0)

```bash
open http://localhost:6381
# Dashboard loads, 4 metric cards show live data, tooltips work
# AHE alpha slider updates (when policy is allkeys-ahe or volatile-ahe)
# Eviction Log populates when memory cap is hit
# RESP2 Stream shows wire bytes during redis-benchmark run
# Keyspace distribution shows sampled data
```

## Metadata

```yaml
id: FERRUM-012
title: "Build interactive visual dashboard for real-time internals visualization"
severity: medium
status: open
component: network
found_date: 2026-07-04
reporter: PM Design
engineering_review: 2026-07-05 — v0.5.0 scope confirmed feasible, no hard gaps. Two implementation notes:
  1. expire_stats() kept unchanged; new keyspace_distribution() method added instead
  2. Add bench case in resp2_bench.rs to verify tap-inactive zero overhead
```
