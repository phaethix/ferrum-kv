---
layout: home

hero:
  name: FerrumKV
  text: 面向 RESP2 兼容 KV 存储的淘汰算法实验室
  tagline: 用易读的 Rust、零依赖控制台写就，提供 10 种淘汰策略 —— 含原创自调优算法 AHE。
  actions:
    - theme: brand
      text: 快速开始
      link: /zh/guide/getting-started
    - theme: alt
      text: GitHub 仓库
      link: https://github.com/phaethix/ferrum-kv

features:
  - title: 原创 AHE 算法
    details: 自调优策略将最近度、访问频率与 TTL 紧急度融合为单一分数，并依据实时命中率反馈自适应调整权重。无需手调。
  - title: 10 种淘汰策略
    details: LRU、LFU、Random、TTL，以及 AHE —— 运行时即可切换，无需重启，与 Redis 的 maxmemory-policy 一致。
  - title: 内置控制台
    details: 在浏览器中浏览 key、就地编辑、查看实时指标，并直接执行 RESP 命令，无需安装任何额外依赖。
  - title: 易读的 Rust
    details: 约 8,500 行分层清晰、注释充分的代码。没有宏魔法，也没有自定义分配器 —— 为阅读与学习而生。
---
