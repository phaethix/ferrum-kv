# 性能基准

测试环境：Apple M5（10 核），本地回环，使用 `redis-benchmark -n 100000 -c 50`。

| 场景 | SET QPS | GET QPS | p50 延迟 |
|----------|--------:|--------:|------------:|
| 基线（不淘汰） | 62,189 | 65,231 | 0.42ms |
| 流水线 `-P 16` | 350,877 | 378,787 | 1.06ms |
| LFU（16MB 上限） | 57,339 | 61,690 | 0.42ms |
| AHE（16MB 上限） | 59,559 | 50,787 | 0.42ms |

## 如何解读

- **流水线** 是提升吞吐最显著的杠杆 —— 批量发送命令可降低单请求开销，成倍提升吞吐。
- 在 **内存上限** 下，相比 LFU，AHE 会以少量 GET 吞吐为代价，在混合负载下获得明显更稳定的命中率。

完整的方法说明与原始输出见
[`benches/redis-benchmark.md`](https://github.com/phaethix/ferrum-kv/blob/master/benches/redis-benchmark.md)。
