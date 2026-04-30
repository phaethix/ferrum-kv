# FerrumKV 🦀

A lightweight, multi-threaded KV storage server written in Rust — built from scratch for systems programming practice.

## Architecture

```mermaid
flowchart TB
    %% 1. Global Tailwind Colors
    classDef runtime fill:#f8fafc,stroke:#94a3b8,stroke-width:2px,color:#334155,stroke-dasharray: 5 5
    classDef engine fill:#eff6ff,stroke:#3b82f6,stroke-width:2px,color:#1e3a8a
    classDef entity fill:#fef2f2,stroke:#ef4444,stroke-width:2px,color:#7f1d1d
    classDef resource fill:#f0fdf4,stroke:#22c55e,stroke-width:2px,color:#14532d
    classDef config fill:#faf5ff,stroke:#a855f7,stroke-width:2px,color:#581c87
    classDef ext fill:#fff7ed,stroke:#ea580c,stroke-width:2px,color:#9a3412

    %% 2. External Clients
    subgraph ClientLayer ["🌐 External Input"]
        direction LR
        Client[/"redis-cli / any RESP2 client"/]
    end

    %% 3. Network & Concurrency
    subgraph NetLayer ["🔌 Network & Concurrency"]
        direction LR
        Listener(("TcpListener (Port 6380)"))
        WorkerThread[["Worker Thread (thread::spawn)"]]

        Listener -->|"accept connection"| WorkerThread
    end

    %% 4. Command Processing Pipeline
    subgraph ProcessLayer ["⚙️ Processing Pipeline"]
        direction LR
        Parser["RESP2 Parser (Array of Bulk Strings)"]
        Exec("Command Executor (SET/GET/DEL/EXISTS/PING/DBSIZE/FLUSHDB)")
        Encoder["RESP2 Encoder (+OK / $n / :n / -ERR)"]

        Parser -->|"yield command"| Exec
        Exec -->|"return result"| Encoder
    end

    %% 5. Storage Engine
    subgraph StoreLayer ["💾 Storage Layer"]
        direction LR
        Engine[("KvEngine")]
        State{{"Shared State (Arc<RwLock<HashMap<Vec<u8>, Vec<u8>>>>)"}}

        Engine -->|"manages"| State
    end

    %% 6. Persistence Layer
    subgraph PersistLayer ["🗄️ Persistence (AOF)"]
        direction LR
        AofWriter["AofWriter (Mutex<File>)"]
        AofFile[/"ferrum.aof (RESP2 on disk)"/]
        Replay["Startup Replay"]

        AofWriter -->|"append + fsync"| AofFile
        AofFile -->|"restore on boot"| Replay
    end

    %% 7. Cross-System Data Flow
    ClientLayer == "TCP Stream" === NetLayer
    WorkerThread -.->|"delegates stream"| ProcessLayer
    ProcessLayer == "read & write data" === StoreLayer
    StoreLayer -.->|"log write ops"| PersistLayer
    Replay -.->|"apply commands"| Engine
    Encoder -.->|"flush to socket"| Client

    %% 8. Apply Styles
    class ClientLayer,NetLayer,ProcessLayer,StoreLayer,PersistLayer runtime
    class Client ext
    class Listener,WorkerThread engine
    class Parser,Exec,Encoder entity
    class Engine,AofWriter,Replay resource
    class State,AofFile config
```

## Quick Start

```bash
# Build
cargo build --release

# Run without persistence (in-memory only)
cargo run --release

# Run with AOF persistence (survives restarts)
cargo run --release -- --aof-path /tmp/ferrum.aof

# Run with explicit fsync policy: always | everysec (default) | no
cargo run --release -- --aof-path /tmp/ferrum.aof --appendfsync always

# Connect with the official Redis CLI
redis-cli -p 6380
```

### CLI Flags

| Flag | Default | Description |
|---|---|---|
| `--addr HOST:PORT` | `127.0.0.1:6380` | Listening address |
| `--aof-path PATH` | *(disabled)* | Enables AOF persistence at the given path |
| `--appendfsync POLICY` | `everysec` | Fsync policy when AOF is enabled (`always` / `everysec` / `no`) |

## Supported Commands

All commands are spoken over **RESP2** (the same wire protocol as Redis), so any Redis client works out of the box.

| Command              | Description                              | RESP2 Response                        |
|----------------------|------------------------------------------|----------------------------------------|
| `SET key value`      | Store a key-value pair                   | `+OK`                                  |
| `GET key`            | Retrieve value by key                    | Bulk string, or nil (`$-1`)            |
| `DEL key [key ...]`  | Delete one or more keys                  | `:N` — number of keys actually deleted |
| `EXISTS key [key ...]` | Count how many of the given keys exist  | `:N`                                   |
| `PING [message]`     | Health check (echoes `message` if given) | `+PONG` or bulk string                 |
| `DBSIZE`             | Return number of keys                    | `:N`                                   |
| `FLUSHDB`            | Remove all keys                          | `+OK`                                  |

Command names are **case-insensitive**.

### Binary Safety

Keys and values are stored as raw `Vec<u8>`, so arbitrary bytes — including `NUL`, `\r\n`, and non‑UTF‑8 sequences — round-trip unchanged through both the network layer and the AOF file.

## Error Handling

All operations return structured RESP2 errors (`-ERR ...`) instead of panicking:

- Parse errors: `-ERR wrong number of arguments for 'SET' command`
- Unknown commands: `-ERR unknown command 'FOOBAR'`
- Internal errors: `-ERR internal error: lock poisoned`

## Persistence (AOF)

When `--aof-path` is set, every write command (`SET` / `DEL` / `FLUSHDB`) is appended to the AOF file **in RESP2 format** — the exact same bytes a client would send over the wire. On startup, FerrumKV replays the file to rebuild state; a half-written tail record is safely truncated.

Fsync policies follow Redis semantics:

- `always` — fsync after every write (safest, slowest)
- `everysec` — fsync once per second on a background tick (default)
- `no` — let the OS decide (fastest, least durable)

## Roadmap

- [x] Core KV engine (`SET` / `GET` / `DEL` / `EXISTS` / `PING` / `DBSIZE` / `FLUSHDB`)
- [x] Unified error handling with `Result` propagation
- [x] RESP2 protocol (binary-safe, compatible with `redis-cli`)
- [x] AOF persistence with configurable fsync + replay on startup
- [ ] Graceful shutdown (SIGINT / SIGTERM) + structured logging
- [ ] TTL (key expiration) & memory eviction (LRU / LFU / AHE)
- [ ] Async I/O (Tokio)

## License

MIT
