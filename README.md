# FerrumKV 🦀

A lightweight, multi-threaded KV storage server written in Rust — built from scratch for systems programming practice.

FerrumKV implements a simple text-based protocol over TCP, supporting basic key-value operations like `SET`, `GET`, `DEL`, and `PING`.

## Architecture

```mermaid
flowchart LR
    Client(["🖥️ Client"])

    Client -- "TCP :6380" --> Network

    subgraph FerrumKV ["⚙️ FerrumKV Server"]
        Network["📡 Network\nTCP listener\nthread-per-connection"]
        Protocol["📜 Protocol\ncommand parsing\nresponse formatting"]
        Storage["💾 Storage\nKvEngine\nArc&lt;RwLock&lt;HashMap&gt;&gt;"]
        Error["⚠️ Error\nFerrumError"]

        Network -- "raw input" --> Protocol
        Protocol -- "Command" --> Storage
        Storage -- "result" --> Network
        Protocol -.-> Error
    end

    Network -- "response" --> Client

    style FerrumKV fill:#1a1a2e,stroke:#e94560,stroke-width:2px,color:#eee
    style Client fill:#0f3460,stroke:#53d8fb,stroke-width:2px,color:#fff
    style Network fill:#16213e,stroke:#0f3460,stroke-width:1px,color:#eee
    style Protocol fill:#1a1a2e,stroke:#e94560,stroke-width:1px,color:#eee
    style Storage fill:#1a1a2e,stroke:#53d8fb,stroke-width:1px,color:#eee
    style Error fill:#1a1a2e,stroke:#f5a623,stroke-width:1px,color:#eee
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

Commands are **case-insensitive**. Values can contain spaces (e.g. `SET msg hello world`).

## Roadmap

- [ ] TTL (key expiration)
- [ ] AOF persistence
- [ ] RESP protocol support
- [ ] Async I/O (tokio)

## License

MIT
