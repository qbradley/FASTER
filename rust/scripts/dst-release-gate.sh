#!/usr/bin/env bash
#
# dst-release-gate — DST-focused verification for FASTER Rust
#
# Runs the full DST verification suite: build checks, test suites,
# loom concurrency, clippy lint. Use before merging DST-related changes.
#
# Usage: dst-release-gate [--quick]
#
#   --quick   Skip extended campaign (tier-1 + tier-2 canary only)
#
# Exit codes:
#   0  All checks passed
#   1  One or more checks failed

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BOLD='\033[1m'
DIM='\033[2m'
RESET='\033[0m'

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUST_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

if [[ ! -f "$RUST_DIR/Cargo.toml" ]]; then
    echo -e "${RED}❌ Cannot find rust/Cargo.toml${RESET}" >&2
    exit 1
fi

cd "$RUST_DIR"

QUICK=false
[[ "${1:-}" == "--quick" ]] && QUICK=true

PASSED=0
FAILED=0
TOTAL_START=$(date +%s)

run_step() {
    local name="$1"
    shift
    echo -e "\n${BOLD}▸ ${name}${RESET}"
    echo -e "${DIM}  $*${RESET}"
    local start
    start=$(date +%s)
    if "$@" > /tmp/dst-gate-output.txt 2>&1; then
        local elapsed=$(( $(date +%s) - start ))
        echo -e "  ${GREEN}✓ passed${RESET} ${DIM}(${elapsed}s)${RESET}"
        ((PASSED++)) || true
    else
        local elapsed=$(( $(date +%s) - start ))
        echo -e "  ${RED}✗ FAILED${RESET} ${DIM}(${elapsed}s)${RESET}"
        tail -20 /tmp/dst-gate-output.txt | sed 's/^/    /'
        ((FAILED++)) || true
    fi
}

echo -e "${BOLD}╔══════════════════════════════════════════╗${RESET}"
echo -e "${BOLD}║   DST Release Gate Verification Suite    ║${RESET}"
echo -e "${BOLD}╚══════════════════════════════════════════╝${RESET}"

# ── Step 1: Build without simulation (zero-cost gate) ──
run_step "Build faster-core (no simulation)" \
    cargo build -p faster-core

# ── Step 2: Build with simulation feature ──
run_step "Build faster-core (simulation)" \
    cargo build -p faster-core --features simulation

# ── Step 3: faster-core tests ──
run_step "faster-core tests (nextest)" \
    cargo nextest run -p faster-core

# ── Step 4: faster-dst tests (smoke — excludes extended campaign) ──
run_step "faster-dst smoke tests" \
    cargo nextest run -p faster-dst -E 'not test(campaign_expanded)'

# ── Step 5: DST concurrency scenarios (canary seeds) ──
run_step "DST concurrency scenarios (canary seeds)" \
    cargo nextest run -p faster-dst --test dst_concurrency

# ── Step 6: Loom concurrency tests ──
run_step "Loom concurrency tests" \
    env RUSTFLAGS="--cfg loom" cargo test --release --features loom -p faster-core --test loom_tests

# ── Step 7: Clippy (std) ──
run_step "Clippy: faster-core" \
    cargo clippy -p faster-core -- -D warnings

# ── Step 8: Clippy (simulation) ──
run_step "Clippy: faster-core (simulation)" \
    cargo clippy -p faster-core --features simulation -- -D warnings

# ── Step 9: Clippy (faster-dst) ──
run_step "Clippy: faster-dst" \
    cargo clippy -p faster-dst -- -D warnings

# ── Step 10: Extended campaign (optional) ──
if [[ "$QUICK" == false ]]; then
    run_step "DST extended campaign (all scenarios × canary seeds)" \
        cargo nextest run -p faster-dst --test dst_campaign -- --ignored
fi

# ── Summary ──
TOTAL_ELAPSED=$(( $(date +%s) - TOTAL_START ))
echo ""
echo -e "${BOLD}════════════════════════════════════════════${RESET}"
echo -e "  ${GREEN}Passed:${RESET} $PASSED"
if [[ $FAILED -gt 0 ]]; then
    echo -e "  ${RED}Failed:${RESET} $FAILED"
fi
echo -e "  ${DIM}Total time: ${TOTAL_ELAPSED}s${RESET}"
echo -e "${BOLD}════════════════════════════════════════════${RESET}"

rm -f /tmp/dst-gate-output.txt

if [[ $FAILED -gt 0 ]]; then
    echo -e "\n${RED}❌ $FAILED check(s) failed${RESET}"
    exit 1
else
    echo -e "\n${GREEN}✅ All checks passed — DST release gate clear${RESET}"
    exit 0
fi
