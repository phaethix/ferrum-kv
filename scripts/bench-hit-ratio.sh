#!/usr/bin/env bash
#
# Reproduces the hit-ratio benchmark matrix for FerrumKV's eviction policies.
#
# For every (policy x access-pattern) pair the harness starts a fresh,
# memory-capped server and replays a read-through workload with realistic
# inter-request spacing, then reads the server's own keyspace_hits /
# keyspace_misses counters. See examples/hit_ratio_bench.rs for the method.
#
# The full matrix (5 patterns x 4 policies) takes a few minutes. Tune with:
#   OPS=12000 PACE_MS=1 ./scripts/bench-hit-ratio.sh
set -euo pipefail

OPS="${OPS:-12000}"
PACE_MS="${PACE_MS:-1}"
WORKING_SET="${WORKING_SET:-100000}"
CAPACITY="${CAPACITY:-5000}"
PATTERNS="${PATTERNS:-zipf,shift,mixed,scan}"
POLICIES="${POLICIES:-lru,lfu,ahe,random,sieve,sieves}"

cd "$(dirname "$0")/.."

echo ">> building release binary"
cargo build --release --bin ferrum-kv

echo ">> running hit-ratio benchmark (ops=${OPS} pace=${PACE_MS}ms)"
echo "   patterns=${PATTERNS}"
echo "   policies=${POLICIES}"
cargo run --release --example hit_ratio_bench -- \
  --working-set "${WORKING_SET}" \
  --capacity "${CAPACITY}" \
  --ops "${OPS}" \
  --pace-ms "${PACE_MS}" \
  --patterns "${PATTERNS}" \
  --policies "${POLICIES}"
