#!/usr/bin/env bash


# Dumb test wrapper that checks memory usage on macos.
# Expects the toolchain as an argument. Any others are forwarded to
# the underlying cargo-test invocation.

# Actual test invocation, toolchain is $1
function run_test() {
    TOOLCHAIN="$1"
    shift
    PATH="$PATH:$PWD/cockroachdb/bin:$PWD/clickhouse" \
        RUSTFLAGS="-D warnings" \
        RUSTDOCFLAGS="-D warnings" \
        cargo "+$TOOLCHAIN" test \
        --workspace --locked --verbose \
        "$@"
}

./tools/watch-memory-pressure.sh &
WATCH_PID="$!"
run_test "$@"

if [ "$WATCH_PID" -ne 0 ]; then
    kill "$WATCH_PID"
    wait "$WATCH_PID"
fi
