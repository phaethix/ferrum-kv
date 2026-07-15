---
layout: home

hero:
  name: FerrumKV
  text: Eviction algorithm laboratory for RESP2-compatible KV stores
  tagline: 10 cache eviction policies, built-in web dashboard, written in Rust.
  actions:
    - theme: brand
      text: Get Started
      link: /guide/getting-started
    - theme: alt
      text: View on GitHub
      link: https://github.com/phaethix/ferrum-kv

features:
  - title: 10 Eviction Policies
    details: LRU, LFU, Random, TTL, and AHE — an adaptive, self-tuning policy. Swap at runtime with no restart.
  - title: Built-in Dashboard
    details: Browse keys, edit inline, watch live stats, and run RESP commands straight from the browser. No extra dependencies.
  - title: RESP2 Compatible
    details: Drop-in for any Redis client — redis-cli, Redis Insight, or your own application.
  - title: Readable Rust
    details: ~8,500 lines of layered, well-commented code. No macro magic, no custom allocators.
---
