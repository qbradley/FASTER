#!/usr/bin/env bash
#
# bench-ci-smoke.sh — Quick benchmark smoke test for CI
#
# Verifies that all benchmark suites compile and execute without error.
# Does NOT measure performance — runs each benchmark for a single iteration
# using criterion's --quick mode or the custom smoke test path.
#
# Purpose: catch compilation failures, data setup panics, and configuration
# errors before they block a full benchmark run.
#
# Usage:
#   rust/scripts/bench-ci-smoke.sh
#
# Exit codes:
#   0  All benchmark suites compiled and ran successfully
#   1  One or more suites failed
#
# Target: complete in <30 seconds on a typical CI runner.

set -euo pipefail

# ── Colors ──────────────────────────────────────────────────────────────────
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BOLD='\033[1m'
DIM='\033[2m'
RESET='\033[0m'

# ── Resolve paths ───────────────────────────────────────────────────────────
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

cd "$REPO_ROOT"

echo -e "${BOLD}╔══════════════════════════════════════════════════════════════╗${RESET}"
echo -e "${BOLD}║       FASTER — Benchmark Smoke Test (CI)                    ║${RESET}"
echo -e "${BOLD}╚══════════════════════════════════════════════════════════════╝${RESET}"
echo ""

FAILURES=0
PASSES=0
START_TIME=$(date +%s)

# ── Helper ──────────────────────────────────────────────────────────────────
run_smoke() {
    local suite_name="$1"
    shift
    local cmd=("$@")

    echo -ne "  ${suite_name}... "
    local suite_start
    suite_start=$(date +%s)

    if "${cmd[@]}" > /dev/null 2>&1; then
        local suite_elapsed=$(( $(date +%s) - suite_start ))
        echo -e "${GREEN}OK${RESET} (${suite_elapsed}s)"
        PASSES=$((PASSES + 1))
    else
        local suite_elapsed=$(( $(date +%s) - suite_start ))
        echo -e "${RED}FAIL${RESET} (${suite_elapsed}s)"
        FAILURES=$((FAILURES + 1))
    fi
}

# ── Suite 1: faster-core criterion benchmarks ──────────────────────────────
# The custom main() in each bench file runs a minimal smoke test when invoked
# without --bench. `cargo test --benches` triggers this path via nextest/cargo.
echo -e "${BOLD}▶ Checking faster-core benchmark suites...${RESET}"

run_smoke "core_benchmarks (compile+smoke)" \
    cargo test -p faster-core --bench core_benchmarks

run_smoke "ycsb (compile+smoke)" \
    cargo test -p faster-core --bench ycsb

run_smoke "hash_layout_bench (compile+smoke)" \
    cargo test -p faster-core --bench hash_layout_bench

echo ""

# ── Suite 2: faster-bench criterion benchmarks ─────────────────────────────
echo -e "${BOLD}▶ Checking faster-bench benchmark suite...${RESET}"

run_smoke "faster-bench/main (compile+smoke)" \
    cargo test -p faster-bench --bench main

echo ""

# ── Suite 3: disk-io-bench sample binary ────────────────────────────────────
echo -e "${BOLD}▶ Checking disk-io-bench sample...${RESET}"

run_smoke "disk-io-bench (compile)" \
    cargo build -p disk-io-bench

echo ""

# ── Summary ─────────────────────────────────────────────────────────────────
TOTAL_ELAPSED=$(( $(date +%s) - START_TIME ))
TOTAL=$((PASSES + FAILURES))

echo -e "${BOLD}── Summary ──${RESET}"
echo -e "  Passed:  ${GREEN}${PASSES}${RESET} / ${TOTAL}"
echo -e "  Failed:  ${RED}${FAILURES}${RESET} / ${TOTAL}"
echo -e "  Time:    ${TOTAL_ELAPSED}s"
echo ""

if [[ $FAILURES -gt 0 ]]; then
    echo -e "${RED}${BOLD}❌ FAIL: ${FAILURES} benchmark suite(s) failed smoke test${RESET}"
    exit 1
else
    echo -e "${GREEN}${BOLD}✅ PASS: All ${TOTAL} benchmark suites healthy${RESET}"
    exit 0
fi
