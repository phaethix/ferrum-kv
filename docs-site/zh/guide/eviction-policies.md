# 淘汰策略

当设置了 `maxmemory` 后，FerrumKV 会按照所选策略淘汰 key。策略分为两类：

- `allkeys-*` —— 考量所有 key。
- `volatile-*` —— 仅考量设置了 TTL 的 key。

| 策略 | 类型 | 最近度 | 频次 | 感知 TTL | 自调优 |
|--------|------|:-------:|:---------:|:---------:|:-----------:|
| `noeviction` | — | | | | |
| `allkeys-lru` / `volatile-lru` | LRU | x | | x | |
| `allkeys-lfu` / `volatile-lfu` | LFU | | x | x | |
| `allkeys-random` / `volatile-random` | Random | | | x | |
| `volatile-ttl` | TTL | | | x | |
| **`allkeys-ahe`** / **`volatile-ahe`** | 自适应 | x | x | x | x |

## AHE —— 自适应混合淘汰

AHE（Adaptive Hybrid Eviction）将 **最近度**、**访问频次** 与 **TTL 紧迫度**
融合为一个自调优的 *淘汰优先级评分*。与静态策略不同，AHE 会根据观测到的访问模式
持续重新加权各项信号，从而自适应地在「缓存友好」与「扫描密集」等不同负载间切换，
无需人工调参。

使用 `CONFIG SET maxmemory-policy allkeys-ahe` 即可在运行时切换策略，无需重启。
