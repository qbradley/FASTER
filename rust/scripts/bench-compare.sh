#!/usr/bin/env bash
#
# bench-compare.sh — Performance regression detection for FASTER Rust
#
# Runs criterion benchmarks against a saved baseline and flags regressions
# that exceed a configurable threshold. Designed for VM execution only.
#
# Usage:
#   rust/scripts/bench-compare.sh [OPTIONS]
#
# Options:
#   --baseline NAME   Baseline to compare against (default: main)
#   --threshold N     Regression tolerance percentage (default: 5)
#   --bench FILTER    Run only benchmarks matching FILTER
#   --quick           Use criterion --quick mode (fewer samples)
#   --json            Output results as JSON instead of table
#   --help            Show this help message
#
# Exit codes:
#   0  All benchmarks within threshold
#   1  One or more regressions exceed threshold
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

# ── Defaults ────────────────────────────────────────────────────────────────
BASELINE="main"
THRESHOLD=5
BENCH_FILTER=""
QUICK=false
JSON_OUTPUT=false

# ── Resolve paths ───────────────────────────────────────────────────────────
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CRITERION_DIR="$REPO_ROOT/target/criterion"

# ── Parse arguments ─────────────────────────────────────────────────────────
show_help() {
    sed -n '2,/^$/s/^# \?//p' "$0"
    exit 0
}

while [[ $# -gt 0 ]]; do
    case $1 in
        --baseline)   BASELINE="$2"; shift 2 ;;
        --threshold)  THRESHOLD="$2"; shift 2 ;;
        --bench)      BENCH_FILTER="$2"; shift 2 ;;
        --quick)      QUICK=true; shift ;;
        --json)       JSON_OUTPUT=true; shift ;;
        --help|-h)    show_help ;;
        *)            echo -e "${RED}Unknown option: $1${RESET}"; exit 2 ;;
    esac
done

# Validate threshold is a number
if ! [[ "$THRESHOLD" =~ ^[0-9]+(\.[0-9]+)?$ ]]; then
    echo -e "${RED}❌ --threshold must be a number, got: $THRESHOLD${RESET}" >&2
    exit 2
fi

# ── Header ──────────────────────────────────────────────────────────────────
echo -e "${BOLD}╔══════════════════════════════════════════════════════════════╗${RESET}"
echo -e "${BOLD}║       FASTER — Performance Regression Detection             ║${RESET}"
echo -e "${BOLD}╚══════════════════════════════════════════════════════════════╝${RESET}"
echo ""
echo -e "  Baseline:   ${CYAN}${BASELINE}${RESET}"
echo -e "  Threshold:  ${CYAN}${THRESHOLD}%${RESET}"
echo -e "  Commit:     ${DIM}$(cd "$REPO_ROOT" && git rev-parse --short HEAD 2>/dev/null || echo 'unknown')${RESET}"
echo -e "  Date:       ${DIM}$(date -u '+%Y-%m-%d %H:%M:%S UTC')${RESET}"
echo ""

# ── Check baseline exists ──────────────────────────────────────────────────
check_baseline() {
    local found=0
    if [[ -d "$CRITERION_DIR" ]]; then
        # Criterion stores baselines inside each benchmark's directory
        # Look for any benchmark dir that has a baseline with the given name
        while IFS= read -r -d '' dir; do
            if [[ -d "$dir/$BASELINE" ]]; then
                found=1
                break
            fi
        done < <(find "$CRITERION_DIR" -mindepth 1 -maxdepth 1 -type d -print0 2>/dev/null)
    fi

    if [[ $found -eq 0 ]]; then
        echo -e "${YELLOW}⚠  No saved baseline '${BASELINE}' found in target/criterion/${RESET}"
        echo -e "${DIM}   Run bench-baseline.sh first, or let CI save a baseline on main push.${RESET}"
        echo -e "${DIM}   Proceeding — criterion will report absolute times only (no comparison).${RESET}"
        echo ""
        return 1
    fi
    return 0
}

HAS_BASELINE=true
check_baseline || HAS_BASELINE=false

# ── Build criterion args ───────────────────────────────────────────────────
CARGO_BENCH_ARGS=(bench -p faster-core --no-fail-fast)
CRITERION_ARGS=(--save-baseline current)

if [[ "$HAS_BASELINE" == true ]]; then
    CRITERION_ARGS+=(--baseline "$BASELINE")
fi

if [[ "$QUICK" == true ]]; then
    CRITERION_ARGS+=(--quick)
fi

if [[ -n "$BENCH_FILTER" ]]; then
    CRITERION_ARGS+=("$BENCH_FILTER")
fi

# ── Run benchmarks ──────────────────────────────────────────────────────────
echo -e "${BOLD}▶ Running benchmarks...${RESET}"
echo -e "  ${DIM}$ cd $REPO_ROOT && cargo ${CARGO_BENCH_ARGS[*]} -- ${CRITERION_ARGS[*]}${RESET}"
echo ""

BENCH_OUTPUT_FILE=$(mktemp "/tmp/faster-bench-compare-XXXXXX.txt")
trap 'rm -f "$BENCH_OUTPUT_FILE"' EXIT

cd "$REPO_ROOT"

if ! cargo "${CARGO_BENCH_ARGS[@]}" -- "${CRITERION_ARGS[@]}" 2>&1 | tee "$BENCH_OUTPUT_FILE"; then
    echo -e "\n${RED}❌ Benchmark run failed.${RESET}" >&2
    exit 2
fi

# ── Parse criterion output ──────────────────────────────────────────────────
# Criterion comparison output looks like:
#   <benchmark_name>   time:   [1.234 ns 1.256 ns 1.278 ns]
#                      change: [-2.3456% +0.1234% +2.5678%] (p = 0.12 < 0.05)
#                      Change within noise threshold.
# or:
#                      Performance has regressed.
# or:
#                      Performance has improved.

parse_results() {
    local output_file="$1"
    local current_bench=""

    # Arrays to hold parsed results
    declare -a bench_names=()
    declare -a bench_changes=()
    declare -a bench_ci_low=()
    declare -a bench_ci_high=()
    declare -a bench_status=()  # improved / regressed / nochange / new

    while IFS= read -r line; do
        # Match benchmark name line: "benchmark_name   time:   [..."
        if [[ "$line" =~ ^([a-zA-Z_][a-zA-Z0-9_/:.]*)[[:space:]]+time:[[:space:]]+\[ ]]; then
            current_bench="${BASH_REMATCH[1]}"
        fi

        # Match change line: "change: [-2.34% +0.12% +2.56%]"
        if [[ -n "$current_bench" && "$line" =~ change:[[:space:]]+\[([+-]?[0-9]+\.[0-9]+)%[[:space:]]+([+-]?[0-9]+\.[0-9]+)%[[:space:]]+([+-]?[0-9]+\.[0-9]+)% ]]; then
            bench_names+=("$current_bench")
            bench_ci_low+=("${BASH_REMATCH[1]}")
            bench_changes+=("${BASH_REMATCH[2]}")
            bench_ci_high+=("${BASH_REMATCH[3]}")

            local pct="${BASH_REMATCH[2]}"
            # Remove leading + for comparison
            local abs_pct="${pct#+}"
            abs_pct="${abs_pct#-}"

            # Check next line for status
            bench_status+=("pending")
            current_bench=""
        fi

        # Check for regression/improvement status
        if [[ "$line" == *"Performance has regressed"* ]]; then
            if [[ ${#bench_status[@]} -gt 0 ]]; then
                bench_status[${#bench_status[@]}-1]="regressed"
            fi
        elif [[ "$line" == *"Performance has improved"* ]]; then
            if [[ ${#bench_status[@]} -gt 0 ]]; then
                bench_status[${#bench_status[@]}-1]="improved"
            fi
        elif [[ "$line" == *"within noise threshold"* || "$line" == *"No change"* ]]; then
            if [[ ${#bench_status[@]} -gt 0 ]]; then
                bench_status[${#bench_status[@]}-1]="nochange"
            fi
        fi
    done < "$output_file"

    # Also count benchmarks that had no comparison (new benchmarks)
    local new_count=0
    while IFS= read -r line; do
        if [[ "$line" =~ ^([a-zA-Z_][a-zA-Z0-9_/:.]*)[[:space:]]+time: ]] && \
           ! grep -q "change:" <(grep -A3 "^${BASH_REMATCH[1]}" "$output_file"); then
            new_count=$((new_count + 1))
        fi
    done < <(grep "time:" "$output_file")

    # ── Determine regressions exceeding threshold ───────────────────────
    local regression_count=0
    local improvement_count=0
    local nochange_count=0
    local threshold_violations=()

    for i in "${!bench_names[@]}"; do
        local pct="${bench_changes[$i]}"
        local status="${bench_status[$i]}"

        # Positive change% = regression (slower), negative = improvement (faster)
        # Compare absolute value of positive changes against threshold
        if [[ "$pct" == +* ]] || [[ "$pct" =~ ^[0-9] ]]; then
            local raw="${pct#+}"
            # Use awk for floating point comparison
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
        print_json_results
    else
        print_table_results
    fi

    # Return exit code based on threshold violations
    if [[ ${#threshold_violations[@]} -gt 0 ]]; then
        return 1
    fi
    return 0
}

print_table_results() {
    echo ""
    echo -e "${BOLD}═══ Regression Analysis ═══${RESET}"
    echo ""

    if [[ ${#bench_names[@]} -eq 0 ]]; then
        if [[ "$HAS_BASELINE" == false ]]; then
            echo -e "${YELLOW}No baseline to compare against — first run establishes baseline.${RESET}"
        else
            echo -e "${YELLOW}No comparison data found in criterion output.${RESET}"
        fi
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
        if [[ ${#name} -gt 50 ]]; then
            name="${name:0:47}..."
        fi

        # Color based on status and threshold
        local status_label color
        local raw="${change#+}"
        raw="${raw#-}"

        # Check if this is a threshold violation
        local is_violation=false
        for vi in "${threshold_violations[@]}"; do
            if [[ "$vi" == "$i" ]]; then
                is_violation=true
                break
            fi
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
    echo -e "  Total compared:       ${#bench_names[@]}"
    echo -e "  Threshold violations: ${RED}${#threshold_violations[@]}${RESET} (>${THRESHOLD}% regression)"
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
        echo -e "${GREEN}${BOLD}✅ PASS: All benchmarks within ${THRESHOLD}% threshold${RESET}"
    fi
}

print_json_results() {
    echo "{"
    echo "  \"threshold\": $THRESHOLD,"
    echo "  \"baseline\": \"$BASELINE\","
    echo "  \"commit\": \"$(cd "$REPO_ROOT" && git rev-parse --short HEAD 2>/dev/null || echo 'unknown')\","
    echo "  \"timestamp\": \"$(date -u '+%Y-%m-%dT%H:%M:%SZ')\","
    echo "  \"pass\": $([ ${#threshold_violations[@]} -eq 0 ] && echo true || echo false),"
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
}

# ── Run analysis ────────────────────────────────────────────────────────────
if [[ "$HAS_BASELINE" == true ]]; then
    if parse_results "$BENCH_OUTPUT_FILE"; then
        exit 0
    else
        exit 1
    fi
else
    echo ""
    echo -e "${GREEN}${BOLD}✅ Baseline run complete.${RESET} Saved as '${CYAN}current${RESET}'."
    echo -e "${DIM}   Future runs will compare against this baseline.${RESET}"
    exit 0
fi
