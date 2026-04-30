//! Microbenchmarks for the RESP2 parser and encoder.
//!
//! These benchmarks isolate the wire-format fast path from the rest of the
//! server: every iteration parses (or encodes) a pre-built byte buffer, so
//! any regression in parsing or encoding shows up immediately without TCP
//! or engine noise.
//!
//! Run with `cargo bench --bench resp2_bench`.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

use ferrum_kv::protocol::encoder;
use ferrum_kv::protocol::parser::{FrameParse, parse_frame};

fn build_set_frame(key: &[u8], value: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"*3\r\n");
    encoder::encode_bulk_string(&mut out, b"SET");
    encoder::encode_bulk_string(&mut out, key);
    encoder::encode_bulk_string(&mut out, value);
    out
}

fn build_get_frame(key: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"*2\r\n");
    encoder::encode_bulk_string(&mut out, b"GET");
    encoder::encode_bulk_string(&mut out, key);
    out
}

fn bench_parse_set(c: &mut Criterion) {
    let mut group = c.benchmark_group("resp2/parse/set");
    for &value_len in &[8usize, 64, 512, 4096] {
        let value = vec![b'x'; value_len];
        let frame = build_set_frame(b"some-key", &value);
        group.throughput(Throughput::Bytes(frame.len() as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(value_len),
            &frame,
            |b, frame| {
                b.iter(|| {
                    let parsed = parse_frame(black_box(frame)).unwrap();
                    assert!(matches!(parsed, FrameParse::Complete { .. }));
                });
            },
        );
    }
    group.finish();
}

fn bench_parse_get(c: &mut Criterion) {
    let frame = build_get_frame(b"some-key");
    c.bench_function("resp2/parse/get", |b| {
        b.iter(|| {
            let parsed = parse_frame(black_box(&frame)).unwrap();
            assert!(matches!(parsed, FrameParse::Complete { .. }));
        });
    });
}

fn bench_encode_bulk(c: &mut Criterion) {
    let mut group = c.benchmark_group("resp2/encode/bulk");
    for &value_len in &[8usize, 64, 512, 4096] {
        let payload = vec![b'x'; value_len];
        group.throughput(Throughput::Bytes(value_len as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(value_len),
            &payload,
            |b, payload| {
                b.iter(|| {
                    let mut out = Vec::with_capacity(payload.len() + 16);
                    encoder::encode_bulk_string(&mut out, black_box(payload));
                    black_box(out);
                });
            },
        );
    }
    group.finish();
}

fn bench_encode_integer(c: &mut Criterion) {
    c.bench_function("resp2/encode/integer", |b| {
        b.iter(|| {
            let mut out = Vec::with_capacity(16);
            encoder::encode_integer(&mut out, black_box(1_234_567));
            black_box(out);
        });
    });
}

criterion_group!(
    benches,
    bench_parse_set,
    bench_parse_get,
    bench_encode_bulk,
    bench_encode_integer
);
criterion_main!(benches);
