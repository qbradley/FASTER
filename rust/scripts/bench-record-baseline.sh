#!/usr/bin/env bash
#
# bench-record-baseline.sh — Record a full benchmark baseline for a FASTER release
#
# Runs ALL benchmark suites and saves results in a structured, version-controlled
# format under rust/baselines/{version}/. Captures machine info, Rust version,
# git SHA, timestamp, and produces both machine-readable JSON and human-readable
# markdown reports.
#
# Usage:
#   rust/scripts/bench-record-baseline.sh <VERSION_TAG> [OPTIONS]
#
# Arguments:
#   VERSION_TAG       Release version (e.g., v0.1.0)
#
# Options:
#   --quick           Fewer criterion samples (faster, noisier)
#   --bench FILTER    Only run benchmarks matching FILTER
#   --help            Show this help message
#
# Output:
#   rust/baselines/{version}/
#     metadata.json        Machine info, git SHA, Rust version, timestamp
#     summary.json         Per-benchmark timing results (machine-readable)
#     report.md            Human-readable markdown report
#     criterion/           Raw criterion output for detailed comparison
#
# ⚠️  Run on a dedicated VM only — never on local dev machines.
#     Noisy environments produce unreliable results.

set -euo pipefail

# ── Colors ──────────────────────────────────────────────────────────────────
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
BOLD='\033[1m'
DIM='\033[2m'
RESET='\033[0m'

# ── Resolve paths ───────────────────────────────────────────────────────────
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CRITERION_DIR="$REPO_ROOT/target/criterion"

# ── Defaults ────────────────────────────────────────────────────────────────
VERSION_TAG=""
QUICK=false
BENCH_FILTER=""

# ── Parse arguments ─────────────────────────────────────────────────────────
show_help() {
    sed -n '2,/^$/s/^# \?//p' "$0"
    exit 0
}

while [[ $# -gt 0 ]]; do
    case $1 in
        --quick)       QUICK=true; shift ;;
        --bench)       BENCH_FILTER="$2"; shift 2 ;;
        --help|-h)     show_help ;;
        -*)            echo -e "${RED}Unknown option: $1${RESET}" >&2; exit 2 ;;
        *)
            if [[ -z "$VERSION_TAG" ]]; then
                VERSION_TAG="$1"; shift
            else
                echo -e "${RED}Unexpected argument: $1${RESET}" >&2; exit 2
            fi
            ;;
    esac
done

if [[ -z "$VERSION_TAG" ]]; then
    echo -e "${RED}❌ Missing required VERSION_TAG argument.${RESET}" >&2
    echo -e "${DIM}Usage: bench-record-baseline.sh <VERSION_TAG> [OPTIONS]${RESET}" >&2
    exit 2
fi

# ── Setup output directory ──────────────────────────────────────────────────
BASELINE_DIR="$REPO_ROOT/baselines/$VERSION_TAG"

if [[ -d "$BASELINE_DIR" ]]; then
    echo -e "${YELLOW}⚠  Baseline directory already exists: baselines/${VERSION_TAG}/${RESET}"
    echo -e "${YELLOW}   Overwriting existing baseline.${RESET}"
    rm -rf "$BASELINE_DIR"
fi

mkdir -p "$BASELINE_DIR/criterion"

# ── Collect metadata ────────────────────────────────────────────────────────
collect_metadata() {
    local cpu_model cores mem_gb os_info kernel rust_version cargo_version
    local git_sha git_sha_short git_branch git_dirty

    cpu_model=$(grep -m1 'model name' /proc/cpuinfo 2>/dev/null | cut -d: -f2 | sed 's/^ //' || echo "unknown")
    cores=$(nproc 2>/dev/null || echo "unknown")
    mem_gb=$(awk '/MemTotal/{printf "%.1f", $2/1024/1024}' /proc/meminfo 2>/dev/null || echo "unknown")
    os_info=$(grep PRETTY_NAME /etc/os-release 2>/dev/null | cut -d'"' -f2 || uname -s)
    kernel=$(uname -r 2>/dev/null || echo "unknown")
    rust_version=$(rustc --version 2>/dev/null || echo "unknown")
    cargo_version=$(cargo --version 2>/dev/null || echo "unknown")

    cd "$REPO_ROOT"
    git_sha=$(git rev-parse HEAD 2>/dev/null || echo "unknown")
    git_sha_short=$(git rev-parse --short HEAD 2>/dev/null || echo "unknown")
    git_branch=$(git branch --show-current 2>/dev/null || echo "unknown")
    git_dirty=$(git diff --quiet 2>/dev/null && echo "false" || echo "true")

    cat <<EOF
{
  "version": "$VERSION_TAG",
  "timestamp": "$(date -u '+%Y-%m-%dT%H:%M:%SZ')",
  "git": {
    "sha": "$git_sha",
    "sha_short": "$git_sha_short",
    "branch": "$git_branch",
    "dirty": $git_dirty
  },
  "rust": {
    "rustc": "$rust_version",
    "cargo": "$cargo_version"
  },
  "machine": {
    "cpu": "$cpu_model",
    "cores": $cores,
    "memory_gb": $mem_gb,
    "os": "$os_info",
    "kernel": "$kernel"
  },
  "options": {
    "quick_mode": $QUICK,
    "filter": $(if [[ -n "$BENCH_FILTER" ]]; then echo "\"$BENCH_FILTER\""; else echo "null"; fi)
  }
}
EOF
}

# ── Header ──────────────────────────────────────────────────────────────────
echo -e "${BOLD}╔══════════════════════════════════════════════════════════════╗${RESET}"
echo -e "${BOLD}║       FASTER — Record Benchmark Baseline                    ║${RESET}"
echo -e "${BOLD}╚══════════════════════════════════════════════════════════════╝${RESET}"
echo ""
echo -e "  Version:    ${CYAN}${VERSION_TAG}${RESET}"
echo -e "  Output:     ${DIM}rust/baselines/${VERSION_TAG}/${RESET}"
echo -e "  Commit:     ${DIM}$(cd "$REPO_ROOT" && git rev-parse --short HEAD 2>/dev/null)${RESET}"
echo -e "  Rust:       ${DIM}$(rustc --version 2>/dev/null)${RESET}"
echo -e "  Date:       ${DIM}$(date -u '+%Y-%m-%d %H:%M:%S UTC')${RESET}"
echo ""

# ── Save metadata ───────────────────────────────────────────────────────────
echo -e "${BOLD}▶ Collecting machine metadata...${RESET}"
collect_metadata > "$BASELINE_DIR/metadata.json"
echo -e "  ${DIM}Saved: baselines/${VERSION_TAG}/metadata.json${RESET}"
echo ""

# ── Build criterion args ────────────────────────────────────────────────────
CRITERION_ARGS=(--save-baseline "$VERSION_TAG")

if [[ "$QUICK" == true ]]; then
    CRITERION_ARGS+=(--quick)
fi

if [[ -n "$BENCH_FILTER" ]]; then
    CRITERION_ARGS+=("$BENCH_FILTER")
fi

# ── Run faster-core criterion benchmarks ────────────────────────────────────
# This runs all 3 bench files: core_benchmarks.rs, ycsb.rs, hash_layout_bench.rs
echo -e "${BOLD}▶ Running faster-core benchmark suites...${RESET}"
echo -e "  ${DIM}Suites: core_benchmarks, ycsb, hash_layout_bench${RESET}"
echo -e "  ${DIM}$ cd $REPO_ROOT && cargo bench -p faster-core --no-fail-fast -- ${CRITERION_ARGS[*]}${RESET}"
echo ""

cd "$REPO_ROOT"
BENCH_LOG="$BASELINE_DIR/criterion/faster-core.log"

START_TIME=$(date +%s)

if ! cargo bench -p faster-core --no-fail-fast -- "${CRITERION_ARGS[@]}" 2>&1 | tee "$BENCH_LOG"; then
    echo -e "\n${RED}❌ faster-core benchmarks failed.${RESET}" >&2
    exit 2
fi

CORE_ELAPSED=$(( $(date +%s) - START_TIME ))
echo -e "  ${DIM}faster-core completed in ${CORE_ELAPSED}s${RESET}"
echo ""

# ── Run faster-bench criterion benchmarks ───────────────────────────────────
echo -e "${BOLD}▶ Running faster-bench pipeline benchmarks...${RESET}"
echo -e "  ${DIM}$ cd $REPO_ROOT && cargo bench -p faster-bench --no-fail-fast -- ${CRITERION_ARGS[*]}${RESET}"
echo ""

BENCH_LOG2="$BASELINE_DIR/criterion/faster-bench.log"

BENCH_START=$(date +%s)

if ! cargo bench -p faster-bench --no-fail-fast -- "${CRITERION_ARGS[@]}" 2>&1 | tee "$BENCH_LOG2"; then
    echo -e "\n${RED}❌ faster-bench benchmarks failed.${RESET}" >&2
    exit 2
fi

BENCH_ELAPSED=$(( $(date +%s) - BENCH_START ))
echo -e "  ${DIM}faster-bench completed in ${BENCH_ELAPSED}s${RESET}"
echo ""

TOTAL_ELAPSED=$(( $(date +%s) - START_TIME ))

# ── Copy criterion raw data ────────────────────────────────────────────────
echo -e "${BOLD}▶ Archiving criterion data...${RESET}"

if [[ -d "$CRITERION_DIR" ]]; then
    # Copy the baseline data for each benchmark directory
    BENCH_COUNT=0
    while IFS= read -r -d '' bench_dir; do
        bench_name=$(basename "$bench_dir")
        # Skip internal criterion directories
        [[ "$bench_name" == ".baselines" || "$bench_name" == "report" ]] && continue

        baseline_data="$bench_dir/$VERSION_TAG"
        if [[ -d "$baseline_data" ]]; then
            mkdir -p "$BASELINE_DIR/criterion/$bench_name"
            cp -r "$baseline_data"/* "$BASELINE_DIR/criterion/$bench_name/" 2>/dev/null || true
            BENCH_COUNT=$((BENCH_COUNT + 1))
        fi
    done < <(find "$CRITERION_DIR" -mindepth 1 -maxdepth 1 -type d -print0 2>/dev/null)

    echo -e "  ${DIM}Archived criterion data for ${BENCH_COUNT} benchmarks${RESET}"
else
    echo -e "  ${YELLOW}⚠  No criterion output directory found${RESET}"
fi
echo ""

# ── Parse results into summary JSON ─────────────────────────────────────────
echo -e "${BOLD}▶ Generating summary...${RESET}"

# Parse criterion output logs to extract timing data
# Criterion lines look like:
#   benchmark_name   time:   [1.234 ns 1.256 ns 1.278 ns]
generate_summary() {
    python3 -c "
import json, re, os, sys, glob
from datetime import datetime, timezone

version = '$VERSION_TAG'
baseline_dir = '$BASELINE_DIR'
total_elapsed = $TOTAL_ELAPSED

benchmarks = []
# Criterion time line regex:
#   benchmark_name   time:   [lo unit mid unit hi unit]
time_re = re.compile(
    r'^(\S.*?)\s+time:\s+\[(\d+(?:\.\d+)?)\s+(ns|us|ms|s|µs)\s+'
    r'(\d+(?:\.\d+)?)\s+(ns|us|ms|s|µs)\s+'
    r'(\d+(?:\.\d+)?)\s+(ns|us|ms|s|µs)\]'
)

for log_file in sorted(glob.glob(os.path.join(baseline_dir, 'criterion', '*.log'))):
    suite_name = os.path.splitext(os.path.basename(log_file))[0]
    with open(log_file) as f:
        for line in f:
            m = time_re.match(line.strip())
            if m:
                benchmarks.append({
                    'name': m.group(1).strip(),
                    'suite': suite_name,
                    'time_low': float(m.group(2)),
                    'time_mid': float(m.group(4)),
                    'time_high': float(m.group(6)),
                    'unit': m.group(5),
                })

summary = {
    'version': version,
    'timestamp': datetime.now(timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ'),
    'total_benchmarks': len(benchmarks),
    'total_elapsed_seconds': total_elapsed,
    'benchmarks': benchmarks,
}
json.dump(summary, sys.stdout, indent=2)
print()
" 2>/dev/null || {
        # Fallback: minimal summary if python3 fails
        cat <<EOF
{
  "version": "$VERSION_TAG",
  "timestamp": "$(date -u '+%Y-%m-%dT%H:%M:%SZ')",
  "total_benchmarks": 0,
  "total_elapsed_seconds": $TOTAL_ELAPSED,
  "benchmarks": []
}
EOF
    }
}

generate_summary > "$BASELINE_DIR/summary.json"
echo -e "  ${DIM}Saved: baselines/${VERSION_TAG}/summary.json${RESET}"

# ── Generate markdown report ────────────────────────────────────────────────
generate_report() {
    local meta="$BASELINE_DIR/metadata.json"
    local summary="$BASELINE_DIR/summary.json"

    cat <<'HEADER'
# Benchmark Baseline Report
HEADER

    echo ""
    echo "**Version:** \`$VERSION_TAG\`"
    echo "**Date:** $(date -u '+%Y-%m-%d %H:%M:%S UTC')"
    echo "**Total duration:** ${TOTAL_ELAPSED}s"
    echo ""

    echo "## Environment"
    echo ""

    # Parse metadata with python3 for reliable JSON handling
    python3 -c "
import json, sys
m = json.load(open('$meta'))
print(f\"| Property | Value |\")
print(f\"|----------|-------|\")
print(f\"| CPU | {m['machine']['cpu']} |\")
print(f\"| Cores | {m['machine']['cores']} |\")
print(f\"| Memory | {m['machine']['memory_gb']} GB |\")
print(f\"| OS | {m['machine']['os']} |\")
print(f\"| Kernel | {m['machine']['kernel']} |\")
print(f\"| Rust | {m['rust']['rustc']} |\")
print(f\"| Git SHA | \`{m['git']['sha_short']}\` ({m['git']['branch']}) |\")
print(f\"| Dirty tree | {m['git']['dirty']} |\")
" 2>/dev/null || echo "(metadata parse error)"

    echo ""
    echo "## Results"
    echo ""

    # Parse summary to build results table
    python3 -c "
import json
s = json.load(open('$summary'))
print(f\"Total benchmarks: **{s['total_benchmarks']}**\")
print()
print('| Benchmark | Time (mid) | Unit | CI Low | CI High |')
print('|-----------|------------|------|--------|---------|')
for b in s['benchmarks']:
    print(f\"| {b['name']} | {b['time_mid']} | {b['unit']} | {b['time_low']} | {b['time_high']} |\")
" 2>/dev/null || echo "(summary parse error)"

    echo ""
    echo "## Comparison"
    echo ""
    echo "To compare against this baseline:"
    echo ""
    echo '```bash'
    echo "rust/scripts/bench-release-compare.sh $VERSION_TAG"
    echo '```'
    echo ""
    echo "---"
    echo "*Generated by bench-record-baseline.sh*"
}

generate_report > "$BASELINE_DIR/report.md"
echo -e "  ${DIM}Saved: baselines/${VERSION_TAG}/report.md${RESET}"
echo ""

# ── Summary ─────────────────────────────────────────────────────────────────
TOTAL_BENCHMARKS=$(python3 -c "import json; print(json.load(open('$BASELINE_DIR/summary.json'))['total_benchmarks'])" 2>/dev/null || echo "?")

echo -e "${GREEN}${BOLD}╔══════════════════════════════════════════════════════════════╗${RESET}"
echo -e "${GREEN}${BOLD}║  ✅ Baseline recorded: ${VERSION_TAG}${RESET}"
echo -e "${GREEN}${BOLD}╚══════════════════════════════════════════════════════════════╝${RESET}"
echo ""
echo -e "  Benchmarks:  ${CYAN}${TOTAL_BENCHMARKS}${RESET}"
echo -e "  Duration:    ${CYAN}${TOTAL_ELAPSED}s${RESET}"
echo -e "  Output:      ${DIM}rust/baselines/${VERSION_TAG}/${RESET}"
echo ""
echo -e "  ${DIM}Files:${RESET}"
echo -e "    ${DIM}metadata.json     Machine info, git SHA, Rust version${RESET}"
echo -e "    ${DIM}summary.json      Per-benchmark timing data${RESET}"
echo -e "    ${DIM}report.md         Human-readable report${RESET}"
echo -e "    ${DIM}criterion/        Raw criterion data for re-comparison${RESET}"
echo ""
echo -e "  Next steps:"
echo -e "    1. Review: ${CYAN}cat rust/baselines/${VERSION_TAG}/report.md${RESET}"
echo -e "    2. Commit: ${CYAN}git add rust/baselines/${VERSION_TAG} && git commit${RESET}"
echo -e "    3. Compare later: ${CYAN}rust/scripts/bench-release-compare.sh ${VERSION_TAG}${RESET}"
