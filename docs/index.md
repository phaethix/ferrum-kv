---
layout: home

hero:
  name: FerrumKV
  text: A readable KV store with 10 eviction policies
  actions:
    - theme: brand
      text: Get Started
      link: /guide/getting-started
    - theme: alt
      text: View on GitHub
      link: https://github.com/phaethix/ferrum-kv

features:
  - title: Original AHE Algorithm
    details: Self-tuning eviction that blends recency, frequency, and TTL into one score, then adapts its own weights from live hit-ratio feedback. No tuning required.
  - title: Built-in Dashboard
    details: Browse keys, edit inline, watch live stats, and run RESP commands straight from the browser. Zero extra dependencies to install.
  - title: Readable Rust
    details: ~8,500 lines of layered, well-commented code. No macro magic, no custom allocators — built to be read and learned from.
---
