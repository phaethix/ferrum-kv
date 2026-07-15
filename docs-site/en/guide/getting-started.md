# Getting Started

FerrumKV is a single static binary. Build it with Cargo and run it — no external services required.

## Build

```bash
cargo build --release
```

## Run

```bash
# In-memory mode (RESP on :6380, dashboard on :6381)
./target/release/ferrum-kv

# With AOF persistence and the adaptive AHE eviction policy
./target/release/ferrum-kv \
  --aof-path /tmp/ferrum.aof \
  --maxmemory 256mb \
  --maxmemory-policy allkeys-ahe
```

## Talk to it

FerrumKV speaks RESP2, so any Redis client works:

```text
$ redis-cli -p 6380
redis-cli> SET user:1000 '{"name":"Alice"}'
OK
redis-cli> GET user:1000
{"name":"Alice"}
redis-cli> INFO memory
# Memory
used_memory:184
maxmemory:268435456
...
```

Open **http://127.0.0.1:6381** in your browser to use the built-in dashboard.
