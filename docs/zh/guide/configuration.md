# 配置

FerrumKV 通过命令行参数进行配置。每个参数在配置文件（`ferrum.conf.example`）中都有对应的指令。

## 命令行参数

| 参数 | 默认值 | 说明 |
|------|---------|-------------|
| `--addr HOST:PORT` | `127.0.0.1:6380` | RESP 监听地址 |
| `--dashboard-addr ADDR\|off` | `127.0.0.1:6381` | Web 控制台地址，设为 `off` 可关闭 |
| `--aof-path PATH` | *(未启用)* | 启用 AOF 持久化 |
| `--appendfsync POLICY` | `everysec` | `always` / `everysec` / `no` |
| `--maxmemory BYTES` | `0`（不限制） | 内存上限（`512b` / `64kb` / `256mb` / `1gb`） |
| `--maxmemory-policy POLICY` | `noeviction` | 10 种策略中的任意一种 |
| `--io-threads N` | `0`（自动） | Tokio 工作线程数 |

## 配置文件

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

通过 `--config-file ferrum.conf` 传入配置文件（或使用对应参数）。命令行参数会覆盖配置文件中的值。完整列表见 `ferrum-kv --help`。
