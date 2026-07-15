# Eviction Policies

When `maxmemory` is set, FerrumKV evicts keys according to the selected policy.
Policies come in two families:

- `allkeys-*` — consider every key.
- `volatile-*` — consider only keys with a TTL set.

| Policy | Type | Recency | Frequency | TTL-Aware | Self-Tuning |
|--------|------|:-------:|:---------:|:---------:|:-----------:|
| `noeviction` | — | | | | |
| `allkeys-lru` / `volatile-lru` | LRU | x | | x | |
| `allkeys-lfu` / `volatile-lfu` | LFU | | x | x | |
| `allkeys-random` / `volatile-random` | Random | | | x | |
| `volatile-ttl` | TTL | | | x | |
| **`allkeys-ahe`** / **`volatile-ahe`** | Adaptive | x | x | x | x |

## AHE — Adaptive Hybrid Eviction

AHE (Adaptive Hybrid Eviction) blends **recency**, **frequency**, and **TTL urgency**
into a single self-tuning *Eviction Priority Score*. Unlike static policies, AHE
continuously reweights each signal based on observed access patterns, so it adapts to
workloads that shift between cache-friendly and scan-heavy behavior — without manual
tuning.

Switch policies at runtime with `CONFIG SET maxmemory-policy allkeys-ahe`; no restart
required.
