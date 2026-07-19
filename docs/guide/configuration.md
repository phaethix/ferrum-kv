# Configuration

FerrumKV is configured through command-line flags. Every flag also has a matching
directive in the config file (`ferrum.conf.example`).

## Command-line flags

| Flag | Default | Description |
|------|---------|-------------|
| `--config PATH` | *(none)* | Load a config file (directives below) |
| `--addr HOST:PORT` | `127.0.0.1:6380` | RESP listening address |
| `--dashboard-addr ADDR\|off` | `127.0.0.1:6381` | Web dashboard address, or `off` to disable |
| `--aof-path PATH` | *(disabled)* | Enable AOF persistence |
| `--appendfsync POLICY` | `everysec` | `always` / `everysec` / `no` |
| `--client-timeout SECONDS` | `0` (disabled) | Per-connection idle timeout |
| `--maxclients N` | *(unlimited)* | Max concurrent client connections |
| `--maxmemory BYTES` | `0` (unlimited) | Memory cap (`512b` / `64kb` / `256mb` / `1gb`) |
| `--maxmemory-policy POLICY` | `noeviction` | Any of the 16 policies |
| `--maxmemory-samples N` | `5` | Keys sampled per eviction round |
| `--io-threads N` | `0` (auto) | Tokio worker threads |
| `--loglevel LEVEL` | `info` | `off` / `error` / `warn` / `info` / `debug` / `trace` |

## Config file

```bash
# ferrum.conf.example
bind 127.0.0.1
port 6380
dashboard-addr 127.0.0.1:6381
maxmemory 256mb
maxmemory-policy allkeys-ahe
appendonly yes
appendfsync everysec
```

Pass the file with `--config ferrum.conf`. Flags override config-file values.
See `ferrum-kv --help` for the full list.
