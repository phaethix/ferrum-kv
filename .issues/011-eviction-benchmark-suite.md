---
id: FERRUM-011
title: "Build eviction algorithm benchmark suite with standard workloads"
severity: medium
status: open
component: storage
found_date: 2026-07-04
reporter: PM Research
---

## Summary

FerrumKV currently has no standardized way to compare eviction policy performance. The `redis-benchmark` smoke test measures QPS but doesn't measure miss ratio — the actual metric eviction policies optimize for.

This issue tracks building a benchmark harness that runs each eviction policy against standard workloads (Zipfian, temporal hotspots, production traces) and produces a comparison table. This is the centerpiece of FerrumKV's "eviction algorithm platform" positioning.

## Design

### Workload Profiles

| Workload | Description | Parameters |
|----------|-------------|------------|
| **Zipfian-static** | Static key popularity following Zipf distribution | `α` ∈ {0.7, 0.9, 1.0, 1.2}, key space = 10× cache size |
| **Zipfian-dynamic** | Popularity shifts every N requests (simulates trend changes) | `α=1.0`, shift interval = 100K ops, 5 shifts |
| **Temporal hotspots** | 80% of requests to 20% of keys, hot set changes over time | Hot set size = 1% of key space, rotates every 10K ops |
| **Mixed R/W** | Configurable read/write ratio with Zipfian reads and uniform writes | R:W ∈ {90:10, 50:50, 10:90} |
| **Production trace replay** | Replay pre-recorded key access sequences from libCacheSim | MetaCDN, Twitter KV subsets |
| **TTL stress** | Keys with varying TTLs, testing TTL-aware policies (AHE, SIEVE-S, VolatileTTL) | TTL distribution: 10% short (1s), 40% medium (60s), 50% long (3600s) |

### Benchmark Harness

A Rust binary or integration test that:
1. Starts a FerrumKV instance with a specific eviction policy + memory cap
2. Generates/replays a workload trace
3. Records: miss ratio, hit ratio, eviction count, expired count, avg latency, p99 latency
4. Produces a markdown comparison table

```rust
// Proposed public API for the benchmark harness:
pub struct EvictionBenchmark {
    policy: EvictionPolicy,
    cache_size_bytes: u64,
    workload: Box<dyn WorkloadGenerator>,
    warmup_ops: u64,
    measure_ops: u64,
}

pub struct BenchmarkReport {
    pub policy_name: String,
    pub hit_ratio: f64,
    pub miss_ratio: f64,
    pub evictions: u64,
    pub expired: u64,
    pub avg_latency_us: f64,
    pub p99_latency_us: f64,
    pub ops_per_second: f64,
}

pub fn run_suite(policies: &[EvictionPolicy], workloads: &[Box<dyn WorkloadGenerator>])
    -> Vec<Vec<BenchmarkReport>>;
```

### Output Format

The harness produces a markdown table suitable for README inclusion:

```
## Eviction Policy Comparison (Zipfian α=1.0, 10MB cache, 100M key space)

| Policy | Hit Ratio | Miss Ratio | Evictions | Avg Latency | Throughput |
|--------|-----------|------------|-----------|-------------|------------|
| noeviction | N/A | N/A | 0 | 0.42ms | 62K ops/s |
| allkeys-lru | 78.2% | 21.8% | 1.2M | 0.45ms | 60K ops/s |
| allkeys-lfu | 76.8% | 23.2% | 1.3M | 0.47ms | 58K ops/s |
| allkeys-sieve | 80.1% | 19.9% | 1.1M | 0.44ms | 61K ops/s |
| allkeys-ahe | 81.3% | 18.7% | 0.9M | 0.46ms | 59K ops/s |
```

## Impact

- **Credibility**: Eviction claims backed by reproducible benchmarks.
- **Research value**: A researcher can run the same suite against their custom policy and get a comparison table.
- **Marketing**: The comparison table is the most scannable evidence of FerrumKV's value prop.

## Suggested Fix

Files to create:
- `benches/eviction_bench/` — benchmark harness crate
- `benches/eviction_bench/workloads/` — workload generators (Zipfian, hotspots, etc.)
- `benches/eviction_bench/report.rs` — markdown table generation
- `scripts/run-eviction-suite.sh` — one-command benchmark runner

This is NOT a Criterion benchmark (those measure micro-ops). This is a system-level benchmark that runs the full server stack against generated workloads.

## Verification

```bash
# Run the full suite
./scripts/run-eviction-suite.sh

# Produces:
# - docs/benchmarks/eviction-comparison.md  (markdown table)
# - docs/benchmarks/eviction-comparison.json (raw data)
# - docs/benchmarks/eviction-comparison.png  (chart, optional)
```

## Metadata

```yaml
id: FERRUM-011
title: "Build eviction algorithm benchmark suite with standard workloads"
severity: medium
status: open
component: storage
found_date: 2026-07-04
reporter: PM Research
```
