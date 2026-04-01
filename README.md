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
        Client[/"TCP Client (telnet/netcat)"/]
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
        Parser["Protocol Parser (Text)"]
        Exec("Command Executor (SET/GET/DEL/PING/DBSIZE/FLUSHDB)")
        Fmt["Response Formatter"]
        
        Parser -->|"yield command"| Exec
        Exec -->|"return result"| Fmt
    end

    %% 5. Storage Engine
    subgraph StoreLayer ["💾 Storage Layer"]
        direction LR
        Engine[("KvEngine")]
        State{{"Shared State (Arc<RwLock<HashMap>>)"}}
        
        Engine -->|"manages"| State
    end

    %% 6. Cross-System Data Flow
    ClientLayer == "TCP Stream" === NetLayer
    WorkerThread -.->|"delegates stream"| ProcessLayer
    ProcessLayer == "read & write data" === StoreLayer
    Fmt -.->|"flush to socket"| Client

    %% 7. Apply Styles
    class ClientLayer,NetLayer,ProcessLayer,StoreLayer runtime
    class Client ext
    class Listener,WorkerThread engine
    class Parser,Exec,Fmt entity
    class Engine resource
    class State config
```

## Quick Start

```bash
# Build
cargo build

# Run server (listens on 127.0.0.1:6380)
cargo run

# Connect with telnet or netcat
telnet 127.0.0.1 6380
```

## Supported Commands

| Command           | Description                  | Response          |
|--------------------|------------------------------|-------------------|
| `SET key value`   | Store a key-value pair       | `OK`              |
| `GET key`         | Retrieve value by key        | value or `NULL`   |
| `DEL key`         | Delete a key                 | `OK` or `NULL`    |
| `PING`            | Health check                 | `PONG`            |
| `DBSIZE`          | Return number of keys        | count             |
| `FLUSHDB`         | Remove all keys              | `OK`              |

Commands are **case-insensitive**. Values can contain spaces (e.g. `SET msg hello world`).

## Error Handling

All operations return structured error responses instead of panicking:

- Parse errors: `ERR wrong number of arguments for 'SET' command`
- Unknown commands: `ERR unknown command: FOOBAR`
- Internal errors: `ERR internal error: lock poisoned`

## Roadmap

- [x] Core KV engine (SET/GET/DEL/PING/DBSIZE/FLUSHDB)
- [x] Unified error handling with Result propagation
- [ ] AOF persistence
- [ ] TTL (key expiration)
- [ ] RESP protocol support
- [ ] Async I/O (tokio)

## License

MIT
