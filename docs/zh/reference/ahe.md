# 自适应混合淘汰算法 AHE

> 一种自调优的缓存淘汰算法：将「时间局部性、访问频率、TTL 紧急度」融合为单一分数，并依据实时命中率反馈自动调整自身权重。

- **状态：** 原创算法，自 `v0.3` 起提供
- **源码：** [`src/storage/eviction.rs`](https://github.com/phaethix/ferrum-kv/blob/master/src/storage/eviction.rs)
- **相关：** [白皮书 §9.5](https://github.com/phaethix/ferrum-kv/blob/master/docs/design/whitepaper.md)

---

## 摘要

经典的缓存淘汰策略被迫做固定取舍：**LRU** 对突发流量反应快，却无力抵抗扫描污染，也无法识别长期热点；**LFU** 能保护稳定热点，但存在*冷启动*缺陷——刚到达的热点键频率低，还没热起来就被淘汰。当访问模式在运行中切换时，两者都无法自适应。

**AHE（Adaptive Hybrid Eviction，自适应混合淘汰）** 用一个统一的 **淘汰优先级分数（EPS, Eviction Priority Score）** 融合三个信号来解决：

- **recency（最近度）** —— 键距上次访问有多久（LRU 直觉），
- **infrequency（稀访问度）** —— 键被访问得有多稀少（LFU 直觉），
- **TTL 紧急度** —— 键是否即将过期。

recency 与 frequency 之间的混合权重 `alpha` **不是需要运维人员手调的旋钮**——它由反馈控制器驱动，监视引擎观测到的命中率，并朝当前负载所"奖励"的方向微调 `alpha`。最终效果是：同一个淘汰策略在突发时表现得像 LRU，在稳定热点时表现得像 LFU，且无需任何人工干预。

---

## 1. 设计动机

| 负载场景 | LRU | LFU | 问题 |
| -------- | :-: | :-: | ---- |
| 突发热点（秒杀） | ✅ | ❌ | LFU 冷启动，新热点频率低被误淘汰 |
| 稳定热点 | ❌ | ✅ | LRU 在闲置一次读取后就丢弃长期热点 |
| 扫描污染（全表遍历） | ❌ | ✅ | LRU 让冷扫描键驱逐热数据 |
| 即将过期的键 | ❌ | ❌ | 两者都不感知 TTL，将死的键白白占槽 |
| 访问模式切换 | ❌ | ❌ | 固定策略无法跟随变化 |

**核心思想。** 为每个采样候选计算一个统一分数，分数**最高者最应被淘汰**。此外，由一个控制器依据观测命中率调整 recency/frequency 权重。

---

## 2. 淘汰优先级分数（EPS）

```text
EPS = alpha · recency + (1 − alpha) · infrequency + ttl_penalty
```

EPS 越高 ⇒ 越该被淘汰。每一项都归一化到 `[0, 1]`，使三个信号可直接比较。

| 项 | 定义 | 含义 |
| -- | ---- | ---- |
| `recency` | `min((now − last_access) / 600s, 1.0)` | 越久未访问得分越高 |
| `infrequency` | `1 − lfu_counter / 255` | 越冷得分越高（复用共享的 Morris 计数器） |
| `ttl_penalty` | 剩余 TTL ≤ 30s **或** 已过期时 `+0.2`；否则 `0` | 让 AHE 优先清理将死的键 |
| `alpha` | 自适应权重，夹在 `[0.05, 0.95]` | `→ 1.0` 偏向 LRU，`→ 0.0` 偏向 LFU |

`recency` 采用 600 秒窗口（`RECENCY_HORIZON`）：闲置 10 分钟的键即为"最陈旧"，无论再等多久都记 `1.0`。`infrequency` 复用驱动 `*-lfu` 策略的**同一个** `lfu_counter`（Morris 计数器，`0..=255`），因此 AHE 的**每键额外内存为零**。

> **设计注记。** 早期草稿使用 `log2` 频率归一化与 `1/(1+elapsed)` 时间衰减。最终版本改为共享 LFU 的 Morris 计数器加线性 recency 梯度——这让 AHE 的元数据存储开销恰好为零，并直接与 Redis 兼容的数据结构挂钩。

---

## 3. 自适应权重控制器（alpha）

`alpha` 存于 `AdaptiveHybridState`，是一个由引擎命中率驱动的 1 维梯度搜索。引擎在每次成功淘汰后调用 `observe(hits, misses)`；当积累满 `window_size`（默认 **64**）个样本时，控制器更新 `alpha`。

| 字段 | 默认值 | 作用 |
| ---- | ------ | ---- |
| `alpha` | `0.5` | 当前 recency/frequency 混合比 |
| `step` | `0.05` | 每次调整幅度 |
| `direction` | `+1.0` | 上次移动的符号；回归时翻转 |
| `last_hit_ratio` | `0.0` | 上一窗口命中率（回归检测器） |
| `window_size` | `64` | 两次调整间的样本数（噪声过滤） |
| `window_count` | `0` | 当前窗口已积累样本数 |

```text
observe(hits, misses):
    window_count += 1
    if window_count < window_size:
        return                       # 仍在积累样本

    ratio = hits / (hits + misses)
    if ratio + 1e-4 < last_hit_ratio:
        direction = -direction       # 命中率回退 → 反向
    alpha = clamp(alpha + direction · step, 0.05, 0.95)
    last_hit_ratio = ratio
    window_count = 0
```

**特性**

- **目标函数是命中率，而非访问分布。** 无需每键偏斜度指标，直接以 `keyspace_hits / (keyspace_hits + keyspace_misses)` 为目标。
- **回归翻转式梯度搜索。** 仅记录上次移动的符号；命中率回退时翻转符号——以轻量方式替代梯度估计。
- **护栏保护。** `alpha` 夹在 `[0.05, 0.95]`，AHE 永不退化为纯 LRU 或纯 LFU。
- **自适应节奏。** 因 `observe` 每次淘汰触发，自适应速率自然跟随写入压力。

<figure class="kv-diagram">
  <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 860 360" role="img" aria-label="AHE 自适应 alpha 控制器流程图">
    <defs>
      <linearGradient id="aheZhFlowPrimary" x1="0" y1="0" x2="1" y2="1">
        <stop offset="0%" stop-color="#6366f1"/>
        <stop offset="100%" stop-color="#8b5cf6"/>
      </linearGradient>
      <marker id="aheZhFlowArrow" markerWidth="10" markerHeight="10" refX="8" refY="3" orient="auto">
        <path d="M0,0 L0,6 L9,3 z" fill="#94a3b8"/>
      </marker>
    </defs>
    <rect width="860" height="360" rx="18" fill="#0f172a"/>
    <text x="430" y="34" text-anchor="middle" fill="#e2e8f0" font-size="17" font-weight="700">自适应 alpha 控制器</text>
    <text x="430" y="58" text-anchor="middle" fill="#94a3b8" font-size="12">每个淘汰窗口更新一次的轻量反馈回路</text>
    <rect x="58" y="122" width="170" height="54" rx="14" fill="url(#aheZhFlowPrimary)"/>
    <text x="143" y="144" text-anchor="middle" fill="#fff" font-size="12" font-weight="700">observe(hits, misses)</text>
    <text x="143" y="162" text-anchor="middle" fill="#ddd6fe" font-size="11">每次淘汰调用</text>
    <path d="M288 120 L348 149 L288 178 L228 149 Z" fill="#111827" stroke="#475569"/>
    <text x="288" y="145" text-anchor="middle" fill="#e2e8f0" font-size="12" font-weight="700">窗口是否</text>
    <text x="288" y="161" text-anchor="middle" fill="#e2e8f0" font-size="12" font-weight="700">&gt;= 64?</text>
    <rect x="408" y="122" width="164" height="54" rx="14" fill="#111827" stroke="#475569"/>
    <text x="490" y="144" text-anchor="middle" fill="#e2e8f0" font-size="12" font-weight="700">ratio = hits /</text>
    <text x="490" y="162" text-anchor="middle" fill="#e2e8f0" font-size="12" font-weight="700">(hits + misses)</text>
    <path d="M632 120 L704 149 L632 178 L560 149 Z" fill="#111827" stroke="#475569"/>
    <text x="632" y="145" text-anchor="middle" fill="#e2e8f0" font-size="12" font-weight="700">命中率</text>
    <text x="632" y="161" text-anchor="middle" fill="#e2e8f0" font-size="12" font-weight="700">是否回退?</text>
    <rect x="594" y="236" width="160" height="46" rx="13" fill="#1e1b4b" stroke="#6366f1"/>
    <text x="674" y="263" text-anchor="middle" fill="#ddd6fe" font-size="12" font-weight="700">翻转方向</text>
    <rect x="284" y="236" width="230" height="46" rx="13" fill="#111827" stroke="#475569"/>
    <text x="399" y="263" text-anchor="middle" fill="#e2e8f0" font-size="12" font-weight="700">alpha = clamp(alpha + direction·step)</text>
    <rect x="76" y="236" width="150" height="46" rx="13" fill="#111827" stroke="#475569"/>
    <text x="151" y="263" text-anchor="middle" fill="#94a3b8" font-size="12" font-weight="700">保持 alpha</text>
    <path d="M228 149 L238 149" stroke="#94a3b8" stroke-width="2" marker-end="url(#aheZhFlowArrow)"/>
    <path d="M348 149 L408 149" stroke="#94a3b8" stroke-width="2" marker-end="url(#aheZhFlowArrow)"/>
    <path d="M572 149 L560 149" stroke="#94a3b8" stroke-width="2" marker-end="url(#aheZhFlowArrow)"/>
    <path d="M632 178 L662 232" stroke="#94a3b8" stroke-width="2" marker-end="url(#aheZhFlowArrow)"/>
    <path d="M594 259 L518 259" stroke="#94a3b8" stroke-width="2" marker-end="url(#aheZhFlowArrow)"/>
    <path d="M632 178 C620 216 512 216 468 235" stroke="#94a3b8" stroke-width="2" fill="none" marker-end="url(#aheZhFlowArrow)"/>
    <path d="M288 178 C278 214 205 216 170 235" stroke="#94a3b8" stroke-width="2" fill="none" marker-end="url(#aheZhFlowArrow)"/>
    <text x="372" y="140" fill="#94a3b8" font-size="11">是</text>
    <text x="249" y="211" fill="#94a3b8" font-size="11">否</text>
    <text x="654" y="210" fill="#94a3b8" font-size="11">是</text>
    <text x="520" y="218" fill="#94a3b8" font-size="11">否</text>
  </svg>
</figure>

---

## 4. 复杂度与内存

| 步骤 | 开销 | 说明 |
| ---- | ---- | ---- |
| 单候选打分 | `O(1)` | recency + infrequency + ttl_penalty，全内联 |
| 选 victim | `O(k)` | `k = maxmemory-samples`（默认 5） |
| 每键额外内存 | **0 字节** | 复用 `lfu_counter` 与 `last_access` |
| 控制器更新 | `O(1)` 摊还 | 每 `window_size` 次淘汰一次 |

这与 Redis 的采样法一致（Redis 6+ 同样采样 `maxmemory-samples` 个随机键，而非维护全局堆/链表），并以零内存代价叠加了自适应能力。

<figure class="kv-diagram">
  <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 860 300" role="img" aria-label="AHE 淘汰路径">
    <defs>
      <linearGradient id="aheZhEvictPrimary" x1="0" y1="0" x2="1" y2="1">
        <stop offset="0%" stop-color="#6366f1"/>
        <stop offset="100%" stop-color="#8b5cf6"/>
      </linearGradient>
      <marker id="aheZhEvictArrow" markerWidth="10" markerHeight="10" refX="8" refY="3" orient="auto">
        <path d="M0,0 L0,6 L9,3 z" fill="#94a3b8"/>
      </marker>
    </defs>
    <rect width="860" height="300" rx="18" fill="#0f172a"/>
    <text x="430" y="34" text-anchor="middle" fill="#e2e8f0" font-size="17" font-weight="700">基于采样的淘汰路径</text>
    <text x="430" y="58" text-anchor="middle" fill="#94a3b8" font-size="12">Redis 风格随机采样，加上自适应评分</text>
    <rect x="46" y="112" width="132" height="54" rx="13" fill="url(#aheZhEvictPrimary)"/>
    <text x="112" y="135" text-anchor="middle" fill="#fff" font-size="12" font-weight="700">maxmemory</text>
    <text x="112" y="153" text-anchor="middle" fill="#ddd6fe" font-size="11">超出限制</text>
    <rect x="226" y="112" width="132" height="54" rx="13" fill="#111827" stroke="#475569"/>
    <text x="292" y="135" text-anchor="middle" fill="#e2e8f0" font-size="12" font-weight="700">采样 k 个</text>
    <text x="292" y="153" text-anchor="middle" fill="#e2e8f0" font-size="12" font-weight="700">随机键</text>
    <rect x="406" y="112" width="158" height="54" rx="13" fill="#111827" stroke="#475569"/>
    <text x="485" y="135" text-anchor="middle" fill="#e2e8f0" font-size="12" font-weight="700">候选逐个打分</text>
    <text x="485" y="153" text-anchor="middle" fill="#94a3b8" font-size="11">EPS(alpha, key, now)</text>
    <rect x="612" y="112" width="150" height="54" rx="13" fill="#1e1b4b" stroke="#6366f1"/>
    <text x="687" y="135" text-anchor="middle" fill="#ddd6fe" font-size="12" font-weight="700">argmax(EPS)</text>
    <text x="687" y="153" text-anchor="middle" fill="#ddd6fe" font-size="11">选出 victim</text>
    <rect x="432" y="214" width="158" height="46" rx="13" fill="#111827" stroke="#475569"/>
    <text x="511" y="241" text-anchor="middle" fill="#e2e8f0" font-size="12" font-weight="700">淘汰 + observe</text>
    <rect x="224" y="214" width="146" height="46" rx="13" fill="#111827" stroke="#475569"/>
    <text x="297" y="241" text-anchor="middle" fill="#94a3b8" font-size="12" font-weight="700">窗口满则调整</text>
    <path d="M178 139 L226 139" stroke="#94a3b8" stroke-width="2" marker-end="url(#aheZhEvictArrow)"/>
    <path d="M358 139 L406 139" stroke="#94a3b8" stroke-width="2" marker-end="url(#aheZhEvictArrow)"/>
    <path d="M564 139 L612 139" stroke="#94a3b8" stroke-width="2" marker-end="url(#aheZhEvictArrow)"/>
    <path d="M687 166 C687 215 628 237 594 237" stroke="#94a3b8" stroke-width="2" fill="none" marker-end="url(#aheZhEvictArrow)"/>
    <path d="M432 237 L370 237" stroke="#94a3b8" stroke-width="2" marker-end="url(#aheZhEvictArrow)"/>
    <path d="M224 237 C160 237 126 196 112 169" stroke="#475569" stroke-width="2" fill="none" stroke-dasharray="5 5" marker-end="url(#aheZhEvictArrow)"/>
  </svg>
</figure>

---

## 5. 与经典策略对比

| 维度 | LRU | LFU | **AHE（FerrumKV）** |
| ---- | :-: | :-: | :-----------------: |
| 打分计算 | `O(1)` | `O(1)`（Morris） | `O(1)` |
| 选择 | `O(k)` 采样 | `O(k)` 采样 | **`O(k)`**，`k = samples` |
| 额外内存 | — | — | —（复用 LFU 字段） |
| 突发热点 | ✅ | ❌ | ✅ alpha → LRU |
| 稳定热点 | ❌ | ✅ | ✅ alpha → LFU |
| 抗扫描 | ❌ | ✅ | ✅ 频率兜底 |
| 感知 TTL | ❌ | ❌ | ✅ `ttl_penalty` |
| 模式切换 | ❌ | ❌ | ✅ 实时自适应 |
| 可调参数 | 无 | `lfu-log-factor`、`lfu-decay-time` | alpha 初值、step、window、samples |

---

## 6. 收敛行为

在负载从**突发**（偏向 recency）切换到**稳定热点**（偏向 frequency）时，控制器先将 `alpha` 推向 LRU，再反向推向 LFU，在逐渐收窄的区间内围绕当前负载所奖励的值振荡。下图路径为**示意**——真实轨迹取决于命中率信号，可通过 `INFO memory` 的 `ahe_alpha`、`ahe_last_hit_ratio` 实时观察。

<figure class="kv-diagram">
  <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 860 360" role="img" aria-label="AHE alpha 在切换负载下的收敛曲线">
    <rect width="860" height="360" rx="18" fill="#0f172a"/>
    <text x="430" y="34" text-anchor="middle" fill="#e2e8f0" font-size="17" font-weight="700">AHE alpha 在切换负载下的收敛</text>
    <text x="430" y="58" text-anchor="middle" fill="#94a3b8" font-size="12">示意路径：先偏向 LRU，再回调到 LFU</text>
    <line x1="90" y1="286" x2="790" y2="286" stroke="#475569"/>
    <line x1="90" y1="86" x2="90" y2="286" stroke="#475569"/>
    <g stroke="#1f2937" stroke-width="1">
      <line x1="90" y1="246" x2="790" y2="246"/>
      <line x1="90" y1="206" x2="790" y2="206"/>
      <line x1="90" y1="166" x2="790" y2="166"/>
      <line x1="90" y1="126" x2="790" y2="126"/>
      <line x1="90" y1="86" x2="790" y2="86"/>
    </g>
    <text x="58" y="290" fill="#94a3b8" font-size="11">0.0</text>
    <text x="58" y="190" fill="#94a3b8" font-size="11">0.5</text>
    <text x="58" y="90" fill="#94a3b8" font-size="11">1.0</text>
    <text x="430" y="326" text-anchor="middle" fill="#94a3b8" font-size="12">淘汰窗口（×64 次淘汰）</text>
    <text x="24" y="190" text-anchor="middle" fill="#94a3b8" font-size="12" transform="rotate(-90 24 190)">alpha</text>
    <polyline points="90,186 168,176 246,162 324,144 402,126 480,130 558,148 636,170 714,192 790,200" fill="none" stroke="#8b5cf6" stroke-width="4" stroke-linecap="round" stroke-linejoin="round"/>
    <polyline points="90,186 168,186 246,186 324,186 402,186 480,186 558,186 636,186 714,186 790,186" fill="none" stroke="#64748b" stroke-width="2" stroke-dasharray="6 6"/>
    <g fill="#c4b5fd">
      <circle cx="90" cy="186" r="4"/><circle cx="168" cy="176" r="4"/><circle cx="246" cy="162" r="4"/><circle cx="324" cy="144" r="4"/><circle cx="402" cy="126" r="4"/><circle cx="480" cy="130" r="4"/><circle cx="558" cy="148" r="4"/><circle cx="636" cy="170" r="4"/><circle cx="714" cy="192" r="4"/><circle cx="790" cy="200" r="4"/>
    </g>
    <rect x="604" y="90" width="150" height="54" rx="12" fill="#111827" stroke="#475569"/>
    <circle cx="624" cy="110" r="4" fill="#8b5cf6"/><text x="638" y="114" fill="#e2e8f0" font-size="12">AHE 自调优</text>
    <line x1="620" y1="130" x2="632" y2="130" stroke="#64748b" stroke-width="2" stroke-dasharray="6 6"/><text x="638" y="134" fill="#e2e8f0" font-size="12">固定权重</text>
  </svg>
</figure>

平直线为固定权重策略作参照；AHE 的曲线展示了自调优的偏移与收敛。

---

## 7. 配置与可观测性

一行参数即可启用 AHE——无需任何算法专属调参：

```bash
./ferrum-kv --maxmemory 256mb --maxmemory-policy allkeys-ahe
# 或仅限带 TTL 的键：
./ferrum-kv --maxmemory 256mb --maxmemory-policy volatile-ahe
```

| 旋钮 | 默认值 | 说明 |
| ---- | ------ | ---- |
| `maxmemory-policy` | `noeviction` | 设为 `allkeys-ahe` / `volatile-ahe` 启用 |
| `maxmemory-samples` | `5` | 每次淘汰采样的候选数（与 `*-lru`/`*-lfu` 共用） |
| `step` / `window_size` / `direction` | `0.05` / `64` / `+1` | 内部常量，见 `AdaptiveHybridState::default()` |

实时观察控制器收敛：

```text
$ redis-cli -p 6380 INFO memory
# Memory
used_memory:268435456
maxmemory:268435456
maxmemory_policy:allkeys-ahe
ahe_alpha:0.71
ahe_last_hit_ratio:0.94
```

---

## 8. 延伸阅读

- 实现：[`src/storage/eviction.rs`](https://github.com/phaethix/ferrum-kv/blob/master/src/storage/eviction.rs) —— `eps_score`、`pick_victim_ahe`、`AdaptiveHybridState`
- 完整设计讨论：[白皮书 §9.5](https://github.com/phaethix/ferrum-kv/blob/master/docs/design/whitepaper.md)
- 动手实验：[内置控制台](/zh/guide/dashboard) 会在负载变化时绘出 `ahe_alpha` 曲线。
