#!/usr/bin/env bash
# Run ThreadSanitizer on faster-core compaction tests.
#
# Usage:
#   ./scripts/run-tsan.sh                   # run all TSan compaction tests
#   ./scripts/run-tsan.sh scanner_vs        # run tests matching 'scanner_vs'
#
# Prerequisites:
#   - Rust nightly toolchain: rustup toolchain install nightly
#   - TSan runtime (ships with nightly on x86_64-unknown-linux-gnu)
#
# TSan adds 5-15× overhead. Tests are designed to run in <30s each under TSan.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUST_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

cd "$RUST_DIR"

SUPPRESSIONS="crates/faster-core/tests/tsan_suppressions.txt"

if [ ! -f "$SUPPRESSIONS" ]; then
    echo "ERROR: suppression file not found: $SUPPRESSIONS" >&2
    exit 1
fi

export RUSTFLAGS="-Z sanitizer=thread"
export TSAN_OPTIONS="suppressions=$SUPPRESSIONS halt_on_error=0 history_size=4"
export RUST_TEST_THREADS=1  # TSan works best single-process

echo "=== ThreadSanitizer: faster-core compaction tests ==="
echo "RUSTFLAGS=$RUSTFLAGS"
echo "TSAN_OPTIONS=$TSAN_OPTIONS"
echo ""

cargo +nightly test -p faster-core --test tsan_compaction -- --ignored --nocapture "$@"
