---
title: 架构
---

# 架构

FerrumKV 是一个独立的静态二进制文件。Tokio 运行时同时驱动 RESP2 监听器和内置 Web 控制台，两个前端共享同一个 `KvEngine` 实例——因此你在控制台中做的任何操作都会立即对已连接的 Redis 客户端可见。

## 组件概览

<figure class="kv-diagram kv-architecture-diagram">
  <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 820 220" style="max-width:780px; width:100%; font-family:-apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif;">
    <defs>
      <linearGradient id="boxGrad" x1="0%" y1="0%" x2="0%" y2="100%">
        <stop offset="0%" stop-color="#4f46e5"/>
        <stop offset="100%" stop-color="#6366f1"/>
      </linearGradient>
      <linearGradient id="clientGrad" x1="0%" y1="0%" x2="0%" y2="100%">
        <stop offset="0%" stop-color="#6366f1"/>
        <stop offset="100%" stop-color="#818cf8"/>
      </linearGradient>
      <filter id="shadow" x="-10%" y="-10%" width="120%" height="130%">
        <feDropShadow dx="0" dy="3" stdDeviation="4" flood-color="#000" flood-opacity="0.15"/>
      </filter>
      <marker id="arrow" markerWidth="10" markerHeight="10" refX="9" refY="3" orient="auto">
        <path d="M0,0 L0,6 L9,3 z" fill="#94a3b8"/>
      </marker>
    </defs>
    <!-- Row 0 -->
    <rect x="310" y="10" width="200" height="42" rx="10" fill="url(#clientGrad)" filter="url(#shadow)"/>
    <text x="410" y="37" text-anchor="middle" fill="#fff" font-size="14" font-weight="600">外部客户端</text>
    <!-- Row 1 -->
    <rect x="90" y="85" width="210" height="46" rx="10" fill="url(#boxGrad)" filter="url(#shadow)"/>
    <text x="195" y="114" text-anchor="middle" fill="#fff" font-size="13.5" font-weight="600">网络与并发</text>
    <rect x="520" y="85" width="200" height="46" rx="10" fill="url(#boxGrad)" filter="url(#shadow)"/>
    <text x="620" y="114" text-anchor="middle" fill="#fff" font-size="13.5" font-weight="600">Web 控制台</text>
    <!-- Row 2 -->
    <rect x="90" y="164" width="210" height="46" rx="10" fill="url(#boxGrad)" filter="url(#shadow)"/>
    <text x="195" y="193" text-anchor="middle" fill="#fff" font-size="13.5" font-weight="600">处理流水线</text>
    <rect x="340" y="164" width="170" height="46" rx="10" fill="#0f172a" stroke="#334155" stroke-width="1.5" filter="url(#shadow)"/>
    <text x="425" y="193" text-anchor="middle" fill="#e2e8f0" font-size="13.5" font-weight="600">存储层</text>
    <rect x="550" y="164" width="170" height="46" rx="10" fill="#0f172a" stroke="#334155" stroke-width="1.5" filter="url(#shadow)"/>
    <text x="635" y="193" text-anchor="middle" fill="#e2e8f0" font-size="13.5" font-weight="600">AOF 持久化</text>
    <!-- Arrows -->
    <path d="M370 52 L230 82" stroke="#94a3b8" stroke-width="2" fill="none" marker-end="url(#arrow)"/>
    <path d="M450 52 L590 82" stroke="#94a3b8" stroke-width="2" fill="none" marker-end="url(#arrow)"/>
    <path d="M195 131 L195 161" stroke="#94a3b8" stroke-width="2" fill="none" marker-end="url(#arrow)"/>
    <path d="M600 131 L465 161" stroke="#94a3b8" stroke-width="2" fill="none" marker-end="url(#arrow)"/>
    <path d="M300 187 L337 187" stroke="#94a3b8" stroke-width="2" fill="none" marker-end="url(#arrow)"/>
    <path d="M510 187 L547 187" stroke="#94a3b8" stroke-width="2" fill="none" marker-end="url(#arrow)"/>
    <!-- Labels -->
    <text x="280" y="72" fill="#64748b" font-size="11">TCP / RESP2</text>
    <text x="520" y="72" fill="#64748b" font-size="11">HTTP</text>
    <text x="202" y="150" fill="#64748b" font-size="11">Tokio task</text>
    <text x="535" y="150" fill="#64748b" font-size="11">共享引擎</text>
  </svg>
</figure>

## 请求处理流水线

1. **网络层** — `TcpListener` 在 `:6380` 上接受连接；每个连接被交给一个 Tokio 任务处理。
2. **处理层** — RESP2 解析器将字节流转换为命令数组，执行器针对 `KvEngine` 运行命令，编码器序列化响应。
3. **存储层** — `KvEngine` 持有一个 `HashMap<Vec<u8>, ValueEntry>`，由 `Arc<RwLock<…>>` 保护。后台清理任务淘汰过期键。
4. **持久化层** — 启用 AOF 后，每个写命令会被追加（并根据 `--appendfsync` 策略 fsync）到 `ferrum.aof`；启动时回放该文件以恢复状态。
5. **控制台** — 独立的 HTTP 监听器在 `:6381` 上运行，共享同一引擎，展示键值、实时统计和命令终端。

## 并发模型

服务器基于 Tokio 构建。RESP 连接和控制台作为协作任务在同一运行时上运行，因此无需跨进程同步状态。存储层由单个 `RwLock` 保护，在保持代码简洁的同时仍能并发服务多个客户端。
