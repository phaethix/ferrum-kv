# 快速开始

FerrumKV 是一个单独的静态二进制文件。使用 Cargo 编译后直接运行，无需任何外部服务。

## 编译

```bash
cargo build --release
```

## 运行

```bash
# 内存模式（RESP 监听 :6380，控制台监听 :6381）
./target/release/ferrum-kv

# 启用 AOF 持久化，并使用自适应的 AHE 淘汰策略
./target/release/ferrum-kv \
  --aof-path /tmp/ferrum.aof \
  --maxmemory 256mb \
  --maxmemory-policy allkeys-ahe
```

## 体验一下

FerrumKV 使用 RESP2 协议，因此任意 Redis 客户端都能连接：

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

在浏览器中打开 **http://127.0.0.1:6381** 即可使用内置控制台。
