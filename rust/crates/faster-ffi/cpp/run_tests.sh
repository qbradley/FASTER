#!/usr/bin/env bash
#
# run_tests.sh — Build the Rust FFI library and run C++ integration tests.
#
# Usage:
#   ./run_tests.sh          # Build everything and run tests
#   ./run_tests.sh --skip-rust  # Skip Rust build (use existing library)
#
# Exit code 0 = all tests pass, non-zero = failure.
#
# Copyright (c) Microsoft Corporation. Licensed under the MIT License.

set -euo pipefail

# ── Colors ──────────────────────────────────────────────────────────
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BOLD='\033[1m'
RESET='\033[0m'

# ── Resolve directories ────────────────────────────────────────────
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CPP_DIR="$SCRIPT_DIR"
RUST_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"

BUILD_DIR="$CPP_DIR/build"

# ── Parse flags ─────────────────────────────────────────────────────
SKIP_RUST=false
for arg in "$@"; do
    case "$arg" in
        --skip-rust) SKIP_RUST=true ;;
        --help|-h)
            echo "Usage: $0 [--skip-rust]"
            echo "  --skip-rust  Skip building the Rust FFI library"
            exit 0
            ;;
    esac
done

# ── Step 1: Build Rust FFI library ──────────────────────────────────
if [[ "$SKIP_RUST" == false ]]; then
    echo -e "${BOLD}▶ Building Rust FFI library (release)...${RESET}"
    (cd "$RUST_ROOT" && cargo build --release -p faster-ffi 2>&1)
    echo -e "${GREEN}  ✅ Rust FFI library built${RESET}"
else
    echo -e "${YELLOW}  ⚡ Skipping Rust build (--skip-rust)${RESET}"
fi

# ── Step 2: Configure and build C++ tests ───────────────────────────
echo -e "\n${BOLD}▶ Configuring C++ tests with CMake...${RESET}"
cmake -S "$CPP_DIR" -B "$BUILD_DIR" -DCMAKE_BUILD_TYPE=Release 2>&1

echo -e "\n${BOLD}▶ Building C++ tests...${RESET}"
cmake --build "$BUILD_DIR" --parallel 2>&1
echo -e "${GREEN}  ✅ C++ tests built${RESET}"

# ── Step 3: Run tests ───────────────────────────────────────────────
echo -e "\n${BOLD}▶ Running C++ integration tests...${RESET}"
echo ""

TOTAL_SUITES=0
PASSED_SUITES=0
FAILED_SUITES=0
FAILED_NAMES=()

# Detect Rust release directory for LD_LIBRARY_PATH
RUST_TARGET="$(rustc -vV | sed -n 's/host: //p')"
RELEASE_DIR="$RUST_ROOT/target/$RUST_TARGET/release"
if [[ ! -f "$RELEASE_DIR/libfaster_ffi.so" ]]; then
    RELEASE_DIR="$RUST_ROOT/target/release"
fi
export LD_LIBRARY_PATH="${RELEASE_DIR}:${LD_LIBRARY_PATH:-}"

for test_bin in "$BUILD_DIR"/test_*; do
    [[ -x "$test_bin" ]] || continue
    test_name="$(basename "$test_bin")"
    TOTAL_SUITES=$((TOTAL_SUITES + 1))

    echo -e "${BOLD}  ● ${test_name}${RESET}"
    if "$test_bin" 2>&1 | sed 's/^/    /'; then
        echo -e "  ${GREEN}✅ ${test_name} PASSED${RESET}"
        PASSED_SUITES=$((PASSED_SUITES + 1))
    else
        echo -e "  ${RED}❌ ${test_name} FAILED${RESET}"
        FAILED_SUITES=$((FAILED_SUITES + 1))
        FAILED_NAMES+=("$test_name")
    fi
    echo ""
done

# ── Summary ─────────────────────────────────────────────────────────
echo -e "${BOLD}════════════════════════════════════════${RESET}"
echo -e "${BOLD}Test Suites: ${PASSED_SUITES}/${TOTAL_SUITES} passed${RESET}"

if [[ $FAILED_SUITES -gt 0 ]]; then
    echo -e "${RED}Failed: ${FAILED_NAMES[*]}${RESET}"
    exit 1
else
    echo -e "${GREEN}All integration tests passed!${RESET}"
    exit 0
fi
