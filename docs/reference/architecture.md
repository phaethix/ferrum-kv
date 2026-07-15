---
title: Architecture
---

# Architecture

FerrumKV is a single static binary. A Tokio runtime drives both the RESP2
listener and the built-in web dashboard, and the two front-ends share one
`KvEngine` instance — so anything you do in the dashboard is instantly visible
to connected Redis clients.

## Component overview

```mermaid
flowchart TB
    classDef runtime fill:#f8fafc,stroke:#94a3b8,stroke-width:2px,color:#334155,stroke-dasharray:5 5
    classDef engine fill:#eff6ff,stroke:#3b82f6,stroke-width:2px,color:#1e3a8a
    classDef entity fill:#fef2f2,stroke:#ef4444,stroke-width:2px,color:#7f1d1d
    classDef resource fill:#f0fdf4,stroke:#22c55e,stroke-width:2px,color:#14532d
    classDef config fill:#faf5ff,stroke:#a855f7,stroke-width:2px,color:#581c87
    classDef ext fill:#fff7ed,stroke:#ea580c,stroke-width:2px,color:#9a3412

    ClientLayer["🌐 External Clients"]
    NetLayer["🔌 Network & Concurrency"]
    DashLayer["🖥️ Built-in Web Dashboard"]
    ProcessLayer["⚙️ Processing Pipeline"]
    StoreLayer["💾 Storage Layer"]
    PersistLayer["🗄️ Persistence (AOF)"]

    ClientLayer --> NetLayer
    ClientLayer --> DashLayer
    NetLayer --> ProcessLayer
    DashLayer --> StoreLayer
    ProcessLayer --> StoreLayer
    StoreLayer --> PersistLayer
```

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
