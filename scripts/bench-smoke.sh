#!/usr/bin/env bash
# Smoke benchmark for ferrum-kv. Starts a release build of the server,
# hammers it with the Rust-native load generator in `examples/load_gen.rs`,
# then tears the server down on exit.
#
# A Rust driver is used instead of `redis-benchmark` or `redis-cli --pipe`
# because both of those probe the server with commands (inline `PING`,
# `ECHO <sentinel>`) that ferrum-kv does not yet implement; speaking RESP2
# directly from the driver keeps the benchmark honest without adding
# compatibility commands just for tooling.
#
# Prerequisites:
#   - a recent Rust toolchain (the driver is built via `cargo run --release`)
#
# Usage:
#   scripts/bench-redis.sh                           # defaults
#   PORT=7000 REQUESTS=500000 scripts/bench-redis.sh
#   CLIENTS=64 VALUE_SIZE=256 scripts/bench-redis.sh

set -euo pipefail

PORT="${PORT:-6399}"
REQUESTS="${REQUESTS:-100000}"
CLIENTS="${CLIENTS:-50}"
VALUE_SIZE="${VALUE_SIZE:-64}"

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${ROOT_DIR}/target/release/ferrum-kv"

echo "==> building release binary and load generator"
cargo build --release --manifest-path "${ROOT_DIR}/Cargo.toml" \
    --example load_gen --bin ferrum-kv >/dev/null

SERVER_PID=""
cleanup() {
    if [[ -n "${SERVER_PID}" ]] && kill -0 "${SERVER_PID}" 2>/dev/null; then
        kill "${SERVER_PID}" 2>/dev/null || true
        wait "${SERVER_PID}" 2>/dev/null || true
    fi
}
trap cleanup EXIT INT TERM

echo "==> starting ferrum-kv on 127.0.0.1:${PORT}"
"${BIN}" --addr "127.0.0.1:${PORT}" >/tmp/ferrum-kv-bench.log 2>&1 &
SERVER_PID=$!

# Poll until the server accepts TCP connections.
for _ in {1..50}; do
    if (exec 3<>/dev/tcp/127.0.0.1/"${PORT}") 2>/dev/null; then
        exec 3>&- 3<&-
        break
    fi
    sleep 0.1
done

if ! kill -0 "${SERVER_PID}" 2>/dev/null; then
    echo "error: ferrum-kv exited during startup; log:" >&2
    cat /tmp/ferrum-kv-bench.log >&2 || true
    exit 1
fi

"${ROOT_DIR}/target/release/examples/load_gen" \
    --addr "127.0.0.1:${PORT}" \
    --requests "${REQUESTS}" \
    --clients "${CLIENTS}" \
    --value-size "${VALUE_SIZE}"

echo "==> done"
