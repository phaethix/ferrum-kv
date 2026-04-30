//! Microbenchmarks for the in-memory KV engine.
//!
//! These benchmarks call the engine's public API directly, bypassing the
//! RESP2 wire layer, so any change to the storage fast path shows up as a
//! clear regression. They are intentionally narrow: no network, no AOF,
//! no contention. End-to-end numbers should be gathered with
//! `scripts/bench-redis.sh` against a running server instead.
//!
//! Run with `cargo bench --bench engine_bench`.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

use ferrum_kv::storage::engine::KvEngine;

fn bench_set(c: &mut Criterion) {
    let mut group = c.benchmark_group("engine/set");
    for &value_len in &[8usize, 64, 512, 4096] {
        let value = vec![b'x'; value_len];
        group.throughput(Throughput::Bytes(value_len as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(value_len),
            &value,
            |b, value| {
                let engine = KvEngine::new();
                let mut n: u64 = 0;
                b.iter(|| {
                    n = n.wrapping_add(1);
                    let key = format!("k{n}").into_bytes();
                    engine
                        .set(black_box(key), black_box(value.clone()))
                        .unwrap();
                });
            },
        );
    }
    group.finish();
}

fn bench_get_hit(c: &mut Criterion) {
    let engine = KvEngine::new();
    for i in 0..1_000 {
        engine
            .set(format!("k{i}").into_bytes(), b"value".to_vec())
            .unwrap();
    }

    c.bench_function("engine/get/hit", |b| {
        let mut n: u64 = 0;
        b.iter(|| {
            n = n.wrapping_add(1);
            let key = format!("k{}", n % 1_000);
            let v = engine.get(black_box(key.as_bytes())).unwrap();
            black_box(v);
        });
    });
}

fn bench_get_miss(c: &mut Criterion) {
    let engine = KvEngine::new();
    c.bench_function("engine/get/miss", |b| {
        let mut n: u64 = 0;
        b.iter(|| {
            n = n.wrapping_add(1);
            let key = format!("absent{n}");
            let v = engine.get(black_box(key.as_bytes())).unwrap();
            black_box(v);
        });
    });
}

fn bench_incr(c: &mut Criterion) {
    c.bench_function("engine/incr", |b| {
        let engine = KvEngine::new();
        b.iter(|| {
            let v = engine
                .incr_by(black_box(b"counter".to_vec()), black_box(1))
                .unwrap();
            black_box(v);
        });
    });
}

criterion_group!(
    benches,
    bench_set,
    bench_get_hit,
    bench_get_miss,
    bench_incr
);
criterion_main!(benches);
