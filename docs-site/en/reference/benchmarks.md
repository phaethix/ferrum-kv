# Benchmarks

Measured on an Apple M5 (10 cores), loopback, with
`redis-benchmark -n 100000 -c 50`.

| Scenario | SET QPS | GET QPS | p50 Latency |
|----------|--------:|--------:|------------:|
| Baseline (no eviction) | 62,189 | 65,231 | 0.42ms |
| Pipelined `-P 16` | 350,877 | 378,787 | 1.06ms |
| LFU (16MB cap) | 57,339 | 61,690 | 0.42ms |
| AHE (16MB cap) | 59,559 | 50,787 | 0.42ms |

## Reading the numbers

- **Pipelining** is the biggest lever — batching commands cuts per-request overhead and
  multiplies throughput.
- Under a **memory cap**, AHE trades a little GET throughput for noticeably better
  hit-rate stability than LFU on mixed workloads.

The full methodology and raw output live in
[`benches/redis-benchmark.md`](https://github.com/phaethix/ferrum-kv/blob/master/benches/redis-benchmark.md).
