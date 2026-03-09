#!/usr/bin/env bash
#
# bench-release-compare.sh — Compare current performance against a stored baseline
#
# Runs the full benchmark suite and compares results against a previously
# recorded baseline (from bench-record-baseline.sh). Produces a regression
# report with per-benchmark comparison, flags regressions >5%, and exits
# non-zero if any regressions are found.
#
# Usage:
#   rust/scripts/bench-release-compare.sh <BASELINE_VERSION> [OPTIONS]
#
# Arguments:
#   BASELINE_VERSION  Version to compare against (e.g., v0.1.0)
#
# Options:
#   --threshold N     Regression tolerance percentage (default: 5)
#   --bench FILTER    Only run benchmarks matching FILTER
#   --quick           Fewer criterion samples (faster, noisier)
#   --json            Output JSON instead of table
#   --help            Show this help message
#
# Exit codes:
#   0  All benchmarks within threshold — no regressions
#   1  One or more benchmarks regressed beyond threshold
#   2  Script error (missing baseline, build failure, etc.)
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
BASELINE_VERSION=""
THRESHOLD=5
BENCH_FILTER=""
QUICK=false
JSON_OUTPUT=false

# ── Parse arguments ─────────────────────────────────────────────────────────
show_help() {
    sed -n '2,/^$/s/^# \?//p' "$0"
    exit 0
}

while [[ $# -gt 0 ]]; do
    case $1 in
        --threshold)  THRESHOLD="$2"; shift 2 ;;
        --bench)      BENCH_FILTER="$2"; shift 2 ;;
        --quick)      QUICK=true; shift ;;
        --json)       JSON_OUTPUT=true; shift ;;
        --help|-h)    show_help ;;
        -*)           echo -e "${RED}Unknown option: $1${RESET}" >&2; exit 2 ;;
        *)
            if [[ -z "$BASELINE_VERSION" ]]; then
                BASELINE_VERSION="$1"; shift
            else
                echo -e "${RED}Unexpected argument: $1${RESET}" >&2; exit 2
            fi
            ;;
    esac
done

if [[ -z "$BASELINE_VERSION" ]]; then
    echo -e "${RED}❌ Missing required BASELINE_VERSION argument.${RESET}" >&2
    echo -e "${DIM}Usage: bench-release-compare.sh <BASELINE_VERSION> [OPTIONS]${RESET}" >&2
    exit 2
fi

# Validate threshold
if ! [[ "$THRESHOLD" =~ ^[0-9]+(\.[0-9]+)?$ ]]; then
    echo -e "${RED}❌ --threshold must be a number, got: $THRESHOLD${RESET}" >&2
    exit 2
fi

# ── Verify baseline exists ──────────────────────────────────────────────────
BASELINE_DIR="$REPO_ROOT/baselines/$BASELINE_VERSION"

if [[ ! -d "$BASELINE_DIR" ]]; then
    echo -e "${RED}❌ Baseline not found: rust/baselines/${BASELINE_VERSION}/${RESET}" >&2
    echo -e "${DIM}Available baselines:${RESET}" >&2
    if [[ -d "$REPO_ROOT/baselines" ]]; then
        for d in "$REPO_ROOT/baselines"/*/; do
            [[ -d "$d" ]] && echo -e "  ${CYAN}$(basename "$d")${RESET}" >&2
        done
    else
        echo -e "  ${DIM}(none)${RESET}" >&2
    fi
    exit 2
fi

if [[ ! -f "$BASELINE_DIR/summary.json" ]]; then
    echo -e "${RED}❌ Baseline incomplete — missing summary.json${RESET}" >&2
    exit 2
fi

# ── Header ──────────────────────────────────────────────────────────────────
if [[ "$JSON_OUTPUT" != true ]]; then
    echo -e "${BOLD}╔══════════════════════════════════════════════════════════════╗${RESET}"
    echo -e "${BOLD}║       FASTER — Release Benchmark Comparison                 ║${RESET}"
    echo -e "${BOLD}╚══════════════════════════════════════════════════════════════╝${RESET}"
    echo ""
    echo -e "  Baseline:   ${CYAN}${BASELINE_VERSION}${RESET}"
    echo -e "  Threshold:  ${CYAN}${THRESHOLD}%${RESET}"
    echo -e "  Commit:     ${DIM}$(cd "$REPO_ROOT" && git rev-parse --short HEAD 2>/dev/null || echo 'unknown')${RESET}"
    echo -e "  Date:       ${DIM}$(date -u '+%Y-%m-%d %H:%M:%S UTC')${RESET}"
    echo ""

    # Show baseline metadata
    if [[ -f "$BASELINE_DIR/metadata.json" ]]; then
        local_sha=$(python3 -c "import json; print(json.load(open('$BASELINE_DIR/metadata.json'))['git']['sha_short'])" 2>/dev/null || echo "?")
        local_ts=$(python3 -c "import json; print(json.load(open('$BASELINE_DIR/metadata.json'))['timestamp'][:19])" 2>/dev/null || echo "?")
        echo -e "  ${DIM}Baseline recorded: ${local_sha} @ ${local_ts}${RESET}"
        echo ""
    fi
fi

# ── Restore criterion baseline data ────────────────────────────────────────
# Copy the stored criterion data into target/criterion so criterion can compare
restore_baseline_data() {
    if [[ "$JSON_OUTPUT" != true ]]; then
        echo -e "${BOLD}▶ Restoring baseline criterion data...${RESET}"
    fi

    local restored=0
    for bench_data_dir in "$BASELINE_DIR/criterion"/*/; do
        [[ -d "$bench_data_dir" ]] || continue
        local bench_name
        bench_name=$(basename "$bench_data_dir")

        # Skip log files
        [[ "$bench_name" == *.log ]] && continue

        local target_dir="$CRITERION_DIR/$bench_name/$BASELINE_VERSION"
        mkdir -p "$target_dir"
        cp -r "$bench_data_dir"/* "$target_dir/" 2>/dev/null || true
        restored=$((restored + 1))
    done

    if [[ "$JSON_OUTPUT" != true ]]; then
        echo -e "  ${DIM}Restored ${restored} benchmark baseline(s) into target/criterion/${RESET}"
        echo ""
    fi
}

restore_baseline_data

# ── Run benchmarks ──────────────────────────────────────────────────────────
CRITERION_ARGS=(--save-baseline current --baseline "$BASELINE_VERSION")

if [[ "$QUICK" == true ]]; then
    CRITERION_ARGS+=(--quick)
fi

if [[ -n "$BENCH_FILTER" ]]; then
    CRITERION_ARGS+=("$BENCH_FILTER")
fi

BENCH_OUTPUT=$(mktemp "/tmp/faster-release-compare-XXXXXX.txt")
trap 'rm -f "$BENCH_OUTPUT"' EXIT

if [[ "$JSON_OUTPUT" != true ]]; then
    echo -e "${BOLD}▶ Running benchmarks against baseline...${RESET}"
fi

cd "$REPO_ROOT"

# Run faster-core benchmarks
if [[ "$JSON_OUTPUT" != true ]]; then
    echo -e "  ${DIM}Suite: faster-core${RESET}"
fi
if ! cargo bench -p faster-core --no-fail-fast -- "${CRITERION_ARGS[@]}" 2>&1 | tee -a "$BENCH_OUTPUT"; then
    echo -e "\n${RED}❌ faster-core benchmarks failed.${RESET}" >&2
    exit 2
fi

# Run faster-bench benchmarks
if [[ "$JSON_OUTPUT" != true ]]; then
    echo ""
    echo -e "  ${DIM}Suite: faster-bench${RESET}"
fi
if ! cargo bench -p faster-bench --no-fail-fast -- "${CRITERION_ARGS[@]}" 2>&1 | tee -a "$BENCH_OUTPUT"; then
    echo -e "\n${RED}❌ faster-bench benchmarks failed.${RESET}" >&2
    exit 2
fi

# ── Parse criterion comparison output ───────────────────────────────────────
# Criterion output format:
#   benchmark_name   time:   [lo mid hi]
#                    change: [lo% mid% hi%] (p = ...)
#                    Performance has regressed / improved / Change within noise threshold.

parse_and_report() {
    local output_file="$1"
    local current_bench=""

    # Result arrays
    declare -a bench_names=()
    declare -a bench_changes=()
    declare -a bench_ci_low=()
    declare -a bench_ci_high=()
    declare -a bench_status=()

    while IFS= read -r line; do
        # Benchmark name line
        if [[ "$line" =~ ^([a-zA-Z_][a-zA-Z0-9_/:.]*)[[:space:]]+time:[[:space:]]+\[ ]]; then
            current_bench="${BASH_REMATCH[1]}"
        fi

        # Change line
        if [[ -n "$current_bench" && "$line" =~ change:[[:space:]]+\[([+-]?[0-9]+\.[0-9]+)%[[:space:]]+([+-]?[0-9]+\.[0-9]+)%[[:space:]]+([+-]?[0-9]+\.[0-9]+)% ]]; then
            bench_names+=("$current_bench")
            bench_ci_low+=("${BASH_REMATCH[1]}")
            bench_changes+=("${BASH_REMATCH[2]}")
            bench_ci_high+=("${BASH_REMATCH[3]}")
            bench_status+=("pending")
            current_bench=""
        fi

        # Status lines
        if [[ "$line" == *"Performance has regressed"* ]]; then
            [[ ${#bench_status[@]} -gt 0 ]] && bench_status[${#bench_status[@]}-1]="regressed"
        elif [[ "$line" == *"Performance has improved"* ]]; then
            [[ ${#bench_status[@]} -gt 0 ]] && bench_status[${#bench_status[@]}-1]="improved"
        elif [[ "$line" == *"within noise threshold"* || "$line" == *"No change"* ]]; then
            [[ ${#bench_status[@]} -gt 0 ]] && bench_status[${#bench_status[@]}-1]="nochange"
        fi
    done < "$output_file"

    # ── Classify results ────────────────────────────────────────────────
    local regression_count=0
    local improvement_count=0
    local nochange_count=0
    local threshold_violations=()

    for i in "${!bench_names[@]}"; do
        local pct="${bench_changes[$i]}"
        local status="${bench_status[$i]}"

        # Positive % = slower (regression), negative = faster (improvement)
        if [[ "$pct" == +* ]] || [[ "$pct" =~ ^[0-9] ]]; then
            local raw="${pct#+}"
            if awk "BEGIN{exit(!($raw > $THRESHOLD))}"; then
                threshold_violations+=("$i")
                regression_count=$((regression_count + 1))
            elif [[ "$status" == "regressed" ]]; then
                regression_count=$((regression_count + 1))
            else
                nochange_count=$((nochange_count + 1))
            fi
        else
            improvement_count=$((improvement_count + 1))
        fi
    done

    # ── Output ──────────────────────────────────────────────────────────
    if [[ "$JSON_OUTPUT" == true ]]; then
        echo "{"
        echo "  \"baseline_version\": \"$BASELINE_VERSION\","
        echo "  \"threshold\": $THRESHOLD,"
        echo "  \"commit\": \"$(git rev-parse --short HEAD 2>/dev/null || echo 'unknown')\","
        echo "  \"timestamp\": \"$(date -u '+%Y-%m-%dT%H:%M:%SZ')\","
        echo "  \"pass\": $([ ${#threshold_violations[@]} -eq 0 ] && echo true || echo false),"
        echo "  \"summary\": {"
        echo "    \"total\": ${#bench_names[@]},"
        echo "    \"regressions\": $regression_count,"
        echo "    \"improvements\": $improvement_count,"
        echo "    \"threshold_violations\": ${#threshold_violations[@]},"
        echo "    \"unchanged\": $nochange_count"
        echo "  },"
        echo "  \"benchmarks\": ["

        for i in "${!bench_names[@]}"; do
            local comma=""
            [[ $i -lt $((${#bench_names[@]} - 1)) ]] && comma=","

            local is_violation=false
            for vi in "${threshold_violations[@]}"; do
                [[ "$vi" == "$i" ]] && is_violation=true
            done

            echo "    {"
            echo "      \"name\": \"${bench_names[$i]}\","
            echo "      \"change_pct\": ${bench_changes[$i]},"
            echo "      \"ci_low_pct\": ${bench_ci_low[$i]},"
            echo "      \"ci_high_pct\": ${bench_ci_high[$i]},"
            echo "      \"status\": \"${bench_status[$i]}\","
            echo "      \"exceeds_threshold\": $is_violation"
            echo "    }$comma"
        done

        echo "  ]"
        echo "}"
    else
        # Human-readable table output
        echo ""
        echo -e "${BOLD}═══ Release Regression Analysis: current vs ${BASELINE_VERSION} ═══${RESET}"
        echo ""

        if [[ ${#bench_names[@]} -eq 0 ]]; then
            echo -e "${YELLOW}No comparison data found in criterion output.${RESET}"
            echo -e "${DIM}This can happen if the baseline was recorded with different benchmarks.${RESET}"
            return 0
        fi

        # Table header
        printf "${BOLD}%-50s  %8s  %8s  %8s  %s${RESET}\n" \
            "Benchmark" "Change" "CI Low" "CI High" "Status"
        printf "%-50s  %8s  %8s  %8s  %s\n" \
            "$(printf '%0.s─' {1..50})" "────────" "────────" "────────" "──────────"

        for i in "${!bench_names[@]}"; do
            local name="${bench_names[$i]}"
            local change="${bench_changes[$i]}"
            local ci_low="${bench_ci_low[$i]}"
            local ci_high="${bench_ci_high[$i]}"
            local status="${bench_status[$i]}"

            # Truncate long names
            [[ ${#name} -gt 50 ]] && name="${name:0:47}..."

            local status_label color
            local is_violation=false
            for vi in "${threshold_violations[@]}"; do
                [[ "$vi" == "$i" ]] && is_violation=true
            done

            if [[ "$is_violation" == true ]]; then
                color="$RED"
                status_label="🔴 REGRESSED"
            elif [[ "$status" == "improved" ]]; then
                color="$GREEN"
                status_label="🟢 improved"
            elif [[ "$status" == "regressed" ]]; then
                color="$YELLOW"
                status_label="🟡 regressed"
            else
                color="$DIM"
                status_label="⚪ no change"
            fi

            printf "${color}%-50s  %+7.2f%%  %+7.2f%%  %+7.2f%%  %s${RESET}\n" \
                "$name" "$change" "$ci_low" "$ci_high" "$status_label"
        done

        # Summary
        echo ""
        echo -e "${BOLD}── Summary ──${RESET}"
        echo -e "  Baseline:             ${CYAN}${BASELINE_VERSION}${RESET}"
        echo -e "  Total compared:       ${#bench_names[@]}"
        echo -e "  Threshold violations: ${RED}${#threshold_violations[@]}${RESET} (>${THRESHOLD}% regression)"
        echo -e "  Regressions:          ${regression_count}"
        echo -e "  Improvements:         ${GREEN}${improvement_count}${RESET}"
        echo -e "  Within noise:         ${nochange_count}"

        if [[ ${#threshold_violations[@]} -gt 0 ]]; then
            echo ""
            echo -e "${RED}${BOLD}❌ FAIL: ${#threshold_violations[@]} benchmark(s) regressed beyond ${THRESHOLD}% threshold${RESET}"
            echo ""
            echo -e "${DIM}Regressions exceeding threshold:${RESET}"
            for vi in "${threshold_violations[@]}"; do
                echo -e "  ${RED}• ${bench_names[$vi]} (${bench_changes[$vi]}%)${RESET}"
            done
        else
            echo ""
            echo -e "${GREEN}${BOLD}✅ PASS: All benchmarks within ${THRESHOLD}% threshold vs ${BASELINE_VERSION}${RESET}"
        fi
    fi

    # Exit code: 1 if any threshold violations
    [[ ${#threshold_violations[@]} -gt 0 ]] && return 1
    return 0
}

# ── Run analysis ────────────────────────────────────────────────────────────
if parse_and_report "$BENCH_OUTPUT"; then
    exit 0
else
    exit 1
fi
