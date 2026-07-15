---
title: Architecture
---

# Architecture

FerrumKV is a single static binary. A Tokio runtime drives both the RESP2
listener and the built-in web dashboard, and the two front-ends share one
`KvEngine` instance — so anything you do in the dashboard is instantly visible
to connected Redis clients.

## Component overview

<figure class="kv-diagram kv-architecture-diagram">
  <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 820 220" style="max-width:780px; width:100%; font-family:-apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif;">
    <defs>
      <!-- Gradient for boxes -->
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
    <!-- Row 0: External Clients -->
    <rect x="310" y="10" width="200" height="42" rx="10" fill="url(#clientGrad)" filter="url(#shadow)"/>
    <text x="410" y="37" text-anchor="middle" fill="#fff" font-size="14" font-weight="600">External Clients</text>
    <!-- Row 1: Network + Dashboard -->
    <rect x="90" y="85" width="210" height="46" rx="10" fill="url(#boxGrad)" filter="url(#shadow)"/>
    <text x="195" y="114" text-anchor="middle" fill="#fff" font-size="13.5" font-weight="600">Network &amp; Concurrency</text>
    <rect x="520" y="85" width="200" height="46" rx="10" fill="url(#boxGrad)" filter="url(#shadow)"/>
    <text x="620" y="114" text-anchor="middle" fill="#fff" font-size="13.5" font-weight="600">Web Dashboard</text>
    <!-- Row 2: Processing Pipeline -->
    <rect x="90" y="164" width="210" height="46" rx="10" fill="url(#boxGrad)" filter="url(#shadow)"/>
    <text x="195" y="193" text-anchor="middle" fill="#fff" font-size="13.5" font-weight="600">Processing Pipeline</text>
    <!-- Row 2: Storage Layer -->
    <rect x="340" y="164" width="170" height="46" rx="10" fill="#0f172a" stroke="#334155" stroke-width="1.5" filter="url(#shadow)"/>
    <text x="425" y="193" text-anchor="middle" fill="#e2e8f0" font-size="13.5" font-weight="600">Storage Layer</text>
    <!-- Row 2: AOF Persistence -->
    <rect x="550" y="164" width="170" height="46" rx="10" fill="#0f172a" stroke="#334155" stroke-width="1.5" filter="url(#shadow)"/>
    <text x="635" y="193" text-anchor="middle" fill="#e2e8f0" font-size="13.5" font-weight="600">AOF Persistence</text>
    <!-- Arrows -->
    <!-- Client → Network -->
    <path d="M370 52 L230 82" stroke="#94a3b8" stroke-width="2" fill="none" marker-end="url(#arrow)"/>
    <!-- Client → Dashboard -->
    <path d="M450 52 L590 82" stroke="#94a3b8" stroke-width="2" fill="none" marker-end="url(#arrow)"/>
    <!-- Network → Pipeline -->
    <path d="M195 131 L195 161" stroke="#94a3b8" stroke-width="2" fill="none" marker-end="url(#arrow)"/>
    <!-- Dashboard → Storage -->
    <path d="M600 131 L465 161" stroke="#94a3b8" stroke-width="2" fill="none" marker-end="url(#arrow)"/>
    <!-- Pipeline → Storage -->
    <path d="M300 187 L337 187" stroke="#94a3b8" stroke-width="2" fill="none" marker-end="url(#arrow)"/>
    <!-- Storage → AOF -->
    <path d="M510 187 L547 187" stroke="#94a3b8" stroke-width="2" fill="none" marker-end="url(#arrow)"/>
    <!-- Labels on arrows -->
    <text x="280" y="72" fill="#64748b" font-size="11">TCP / RESP2</text>
    <text x="520" y="72" fill="#64748b" font-size="11">HTTP</text>
    <text x="202" y="150" fill="#64748b" font-size="11">Tokio task</text>
    <text x="535" y="150" fill="#64748b" font-size="11">shared engine</text>
  </svg>
</figure>

## Request pipeline

1. **Network** — a `TcpListener` on `:6380` accepts connections; each one is handed to a Tokio task.
2. **Processing** — the RESP2 parser turns the byte stream into a command array, the executor runs it against the `KvEngine`, and the encoder serialises the reply.
3. **Storage** — `KvEngine` owns a `HashMap<Vec<u8>, ValueEntry>` behind an `Arc<RwLock<…>>`. A background sweeper evicts expired keys.
4. **Persistence** — when AOF is enabled, every mutating command is appended (and fsync'd per `--appendfsync`) to `ferrum.aof`; on boot the file is replayed to restore state.
5. **Dashboard** — a separate HTTP listener on `:6381` shares the same engine and renders keys, live stats, and a command console.

## Concurrency model

The server is built on Tokio. RESP connections and the dashboard run as
cooperative tasks on the same runtime, so there is no cross-process state to
keep in sync. The storage layer is guarded by a single `RwLock`, which keeps
the code easy to reason about while still serving many clients concurrently.
