---
layout: home

hero:
  name: FerrumKV
  text: 面向 RESP2 兼容 KV 存储的淘汰算法实验室
  tagline: 10 种缓存淘汰策略、内置 Web 控制台，使用 Rust 编写。
  actions:
    - theme: brand
      text: 快速开始
      link: /zh/guide/getting-started
    - theme: alt
      text: GitHub 仓库
      link: https://github.com/phaethix/ferrum-kv

features:
  - title: 10 种淘汰策略
    details: LRU、LFU、Random、TTL，以及自适应、自调优的 AHE。运行时即可切换，无需重启。
  - title: 内置控制台
    details: 在浏览器中浏览 key、就地编辑、查看实时指标，并直接执行 RESP 命令，无需任何额外依赖。
  - title: 兼容 RESP2
    details: 可无缝对接任意 Redis 客户端 —— redis-cli、Redis Insight，或你自己的应用。
  - title: 易读的 Rust
    details: 约 8,500 行分层清晰、注释充分的代码。没有宏魔法，也没有自定义分配器。
---
