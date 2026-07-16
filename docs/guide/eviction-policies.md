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
| `allkeys-sieve` / `volatile-sieve` | SIEVE (NSDI'24) | x | | | |
| **`allkeys-sieves`** / **`volatile-sieves`** | SIEVE-S (FerrumKV) | x | | x | |
| **`allkeys-ahe`** / **`volatile-ahe`** | Adaptive | x | x | x | x |

## SIEVE — Simple, Efficient Eviction (NSDI'24)

SIEVE (Zhang et al., NSDI'24) is a modern eviction algorithm that is **simpler
than LRU** yet beats 9 state-of-the-art algorithms on 45%+ of 1,559 production
traces, with ~2× LRU throughput. It keeps one FIFO queue plus one *hand*:

- On insert, a key joins the tail of the queue with its `visited` bit cleared.
- On access, its `visited` bit is set.
- On eviction, the hand sweeps forward clearing `visited` bits until it finds a
  key whose bit is still `false` — that key is the victim.

The key insight is *quick demotion*: a single missed access is enough to evict
an object, whereas LRU gives every touch a "second chance". Eviction is O(1)
amortised and needs no random sampling.

### SIEVE-S — FerrumKV's TTL-aware variant

SIEVE-S adds a FerrumKV-original twist: keys within ~10% of expiry are
**force-demoted** — treated as if `visited` were `false` regardless of recent
access — so they become immediate eviction candidates and spare hotter,
long-lived keys. (The NSDI'24 paper does not discuss TTL integration.)

Switch at runtime, exactly like any other policy:

```text
CONFIG SET maxmemory-policy allkeys-sieve
CONFIG SET maxmemory-policy allkeys-sieves
```

## AHE — Adaptive Hybrid Eviction

AHE (Adaptive Hybrid Eviction) blends **recency**, **frequency**, and **TTL urgency**
into a single self-tuning *Eviction Priority Score*. Unlike static policies, AHE
continuously reweights each signal based on observed access patterns, so it adapts to
workloads that shift between cache-friendly and scan-heavy behavior — without manual
tuning.

Switch policies at runtime with `CONFIG SET maxmemory-policy allkeys-ahe`; no restart
required.
