# 配置

FerrumKV 通过命令行参数进行配置。每个参数在配置文件（`ferrum.conf.example`）中都有对应的指令。

## 命令行参数

| 参数 | 默认值 | 说明 |
|------|---------|-------------|
| `--config PATH` | *(无)* | 加载配置文件（指令见下） |
| `--addr HOST:PORT` | `127.0.0.1:6380` | RESP 监听地址 |
| `--dashboard-addr ADDR\|off` | `127.0.0.1:6381` | Web 控制台地址，设为 `off` 可关闭 |
| `--aof-path PATH` | *(未启用)* | 启用 AOF 持久化 |
| `--appendfsync POLICY` | `everysec` | `always` / `everysec` / `no` |
| `--client-timeout SECONDS` | `0`（不限制） | 单连接空闲超时 |
| `--maxclients N` | *(不限制)* | 最大并发客户端数 |
| `--maxmemory BYTES` | `0`（不限制） | 内存上限（`512b` / `64kb` / `256mb` / `1gb`） |
| `--maxmemory-policy POLICY` | `noeviction` | 10 种策略中的任意一种 |
| `--maxmemory-samples N` | `5` | 每轮淘汰采样的 key 数量 |
| `--io-threads N` | `0`（自动） | Tokio 工作线程数 |
| `--loglevel LEVEL` | `info` | `off` / `error` / `warn` / `info` / `debug` / `trace` |

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

通过 `--config ferrum.conf` 传入配置文件。命令行参数会覆盖配置文件中的值。完整列表见 `ferrum-kv --help`。
