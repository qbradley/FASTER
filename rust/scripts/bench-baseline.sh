#!/usr/bin/env bash
#
# bench-baseline.sh — Save and manage benchmark baselines for FASTER Rust
#
# Runs the full criterion benchmark suite and saves the result as a named
# baseline with metadata (git commit, timestamp, machine info).
#
# Usage:
#   rust/scripts/bench-baseline.sh [COMMAND] [OPTIONS]
#
# Commands:
#   save [NAME]       Run benchmarks and save as named baseline (default: main)
#   list              List all saved baselines with metadata
#   compare A B       Compare two named baselines (prints criterion output)
#   info NAME         Show metadata for a baseline
#   delete NAME       Delete a named baseline
#
# Options:
#   --bench FILTER    Run only benchmarks matching FILTER
#   --quick           Use criterion --quick mode (fewer samples)
#   --help            Show this help message
#
# The 'save' command also writes metadata to:
#   target/criterion/.baselines/<name>.meta.json
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
META_DIR="$CRITERION_DIR/.baselines"

# ── Defaults ────────────────────────────────────────────────────────────────
COMMAND="save"
BASELINE_NAME="main"
COMPARE_A=""
COMPARE_B=""
BENCH_FILTER=""
QUICK=false

# ── Parse arguments ─────────────────────────────────────────────────────────
show_help() {
    sed -n '2,/^$/s/^# \?//p' "$0"
    exit 0
}

if [[ $# -gt 0 ]]; then
    case $1 in
        save)     COMMAND="save"; shift; [[ $# -gt 0 && "$1" != --* ]] && { BASELINE_NAME="$1"; shift; } ;;
        list)     COMMAND="list"; shift ;;
        compare)  COMMAND="compare"; shift
                  [[ $# -ge 2 ]] || { echo -e "${RED}Usage: bench-baseline.sh compare A B${RESET}"; exit 2; }
                  COMPARE_A="$1"; COMPARE_B="$2"; shift 2 ;;
        info)     COMMAND="info"; shift
                  [[ $# -ge 1 ]] || { echo -e "${RED}Usage: bench-baseline.sh info NAME${RESET}"; exit 2; }
                  BASELINE_NAME="$1"; shift ;;
        delete)   COMMAND="delete"; shift
                  [[ $# -ge 1 ]] || { echo -e "${RED}Usage: bench-baseline.sh delete NAME${RESET}"; exit 2; }
                  BASELINE_NAME="$1"; shift ;;
        --*)      ;; # fall through to option parsing
        *)        BASELINE_NAME="$1"; shift ;;
    esac
fi

while [[ $# -gt 0 ]]; do
    case $1 in
        --bench)   BENCH_FILTER="$2"; shift 2 ;;
        --quick)   QUICK=true; shift ;;
        --help|-h) show_help ;;
        *)         echo -e "${RED}Unknown option: $1${RESET}"; exit 2 ;;
    esac
done

# ── Helpers ─────────────────────────────────────────────────────────────────

collect_machine_info() {
    local cpu_model cores mem_gb os_info kernel
    cpu_model=$(grep -m1 'model name' /proc/cpuinfo 2>/dev/null | cut -d: -f2 | sed 's/^ //' || echo "unknown")
    cores=$(nproc 2>/dev/null || echo "unknown")
    mem_gb=$(awk '/MemTotal/{printf "%.1f", $2/1024/1024}' /proc/meminfo 2>/dev/null || echo "unknown")
    os_info=$(cat /etc/os-release 2>/dev/null | grep PRETTY_NAME | cut -d'"' -f2 || uname -s)
    kernel=$(uname -r 2>/dev/null || echo "unknown")

    echo "{\"cpu\": \"$cpu_model\", \"cores\": $cores, \"memory_gb\": $mem_gb, \"os\": \"$os_info\", \"kernel\": \"$kernel\"}"
}

save_metadata() {
    local name="$1"
    mkdir -p "$META_DIR"

    local commit commit_date branch
    commit=$(cd "$REPO_ROOT" && git rev-parse HEAD 2>/dev/null || echo "unknown")
    commit_date=$(cd "$REPO_ROOT" && git log -1 --format=%cI 2>/dev/null || echo "unknown")
    branch=$(cd "$REPO_ROOT" && git branch --show-current 2>/dev/null || echo "unknown")

    local machine_info
    machine_info=$(collect_machine_info)

    cat > "$META_DIR/${name}.meta.json" <<EOF
{
  "baseline_name": "$name",
  "timestamp": "$(date -u '+%Y-%m-%dT%H:%M:%SZ')",
  "git": {
    "commit": "$commit",
    "commit_short": "${commit:0:12}",
    "commit_date": "$commit_date",
    "branch": "$branch"
  },
  "machine": $machine_info,
  "criterion": {
    "quick_mode": $QUICK,
    "filter": "${BENCH_FILTER:-null}"
  }
}
EOF
    echo -e "  ${DIM}Metadata saved: target/criterion/.baselines/${name}.meta.json${RESET}"
}

# ── Commands ────────────────────────────────────────────────────────────────

cmd_save() {
    echo -e "${BOLD}╔══════════════════════════════════════════════════════════════╗${RESET}"
    echo -e "${BOLD}║          FASTER — Save Benchmark Baseline                   ║${RESET}"
    echo -e "${BOLD}╚══════════════════════════════════════════════════════════════╝${RESET}"
    echo ""
    echo -e "  Baseline:   ${CYAN}${BASELINE_NAME}${RESET}"
    echo -e "  Commit:     ${DIM}$(cd "$REPO_ROOT" && git rev-parse --short HEAD 2>/dev/null)${RESET}"
    echo -e "  Branch:     ${DIM}$(cd "$REPO_ROOT" && git branch --show-current 2>/dev/null)${RESET}"
    echo -e "  Date:       ${DIM}$(date -u '+%Y-%m-%d %H:%M:%S UTC')${RESET}"
    echo ""

    # Build criterion args
    local cargo_args=(bench -p faster-core --no-fail-fast)
    local criterion_args=(--save-baseline "$BASELINE_NAME")

    if [[ "$QUICK" == true ]]; then
        criterion_args+=(--quick)
    fi

    if [[ -n "$BENCH_FILTER" ]]; then
        criterion_args+=("$BENCH_FILTER")
    fi

    echo -e "${BOLD}▶ Running full benchmark suite...${RESET}"
    echo -e "  ${DIM}$ cd $REPO_ROOT && cargo ${cargo_args[*]} -- ${criterion_args[*]}${RESET}"
    echo ""

    cd "$REPO_ROOT"
    local start_time
    start_time=$(date +%s)

    if ! cargo "${cargo_args[@]}" -- "${criterion_args[@]}"; then
        echo -e "\n${RED}❌ Benchmark run failed.${RESET}" >&2
        exit 2
    fi

    local end_time elapsed
    end_time=$(date +%s)
    elapsed=$((end_time - start_time))

    # Save metadata
    save_metadata "$BASELINE_NAME"

    echo ""
    echo -e "${GREEN}${BOLD}✅ Baseline '${BASELINE_NAME}' saved${RESET} (${elapsed}s)"
    echo ""
    echo -e "  ${DIM}Criterion data:  target/criterion/*/new/ (linked to baseline '${BASELINE_NAME}')${RESET}"
    echo -e "  ${DIM}Metadata:        target/criterion/.baselines/${BASELINE_NAME}.meta.json${RESET}"
    echo ""
    echo -e "  Compare against this baseline:"
    echo -e "  ${CYAN}  rust/scripts/bench-compare.sh --baseline ${BASELINE_NAME}${RESET}"
}

cmd_list() {
    echo -e "${BOLD}═══ Saved Baselines ═══${RESET}"
    echo ""

    if [[ ! -d "$META_DIR" ]]; then
        echo -e "${YELLOW}  No baselines saved yet.${RESET}"
        echo -e "${DIM}  Run: rust/scripts/bench-baseline.sh save [name]${RESET}"
        exit 0
    fi

    local count=0
    printf "${BOLD}%-15s  %-12s  %-20s  %-25s  %s${RESET}\n" \
        "Name" "Commit" "Date" "Machine" "Quick?"
    printf "%-15s  %-12s  %-20s  %-25s  %s\n" \
        "───────────────" "────────────" "────────────────────" "─────────────────────────" "──────"

    for meta_file in "$META_DIR"/*.meta.json; do
        [[ -f "$meta_file" ]] || continue
        count=$((count + 1))

        local name commit_short timestamp cpu quick
        name=$(python3 -c "import json; d=json.load(open('$meta_file')); print(d['baseline_name'])" 2>/dev/null || echo "?")
        commit_short=$(python3 -c "import json; d=json.load(open('$meta_file')); print(d['git']['commit_short'])" 2>/dev/null || echo "?")
        timestamp=$(python3 -c "import json; d=json.load(open('$meta_file')); print(d['timestamp'][:19])" 2>/dev/null || echo "?")
        cpu=$(python3 -c "import json; d=json.load(open('$meta_file')); print(d['machine']['cpu'][:25])" 2>/dev/null || echo "?")
        quick=$(python3 -c "import json; d=json.load(open('$meta_file')); print('yes' if d['criterion']['quick_mode'] else 'no')" 2>/dev/null || echo "?")

        printf "%-15s  %-12s  %-20s  %-25s  %s\n" "$name" "$commit_short" "$timestamp" "$cpu" "$quick"
    done

    if [[ $count -eq 0 ]]; then
        echo -e "${YELLOW}  No baselines saved yet.${RESET}"
    fi

    # Also list criterion baselines without metadata (e.g., from CI)
    echo ""
    echo -e "${DIM}Criterion baselines in target/criterion/ (may not have metadata):${RESET}"

    if [[ -d "$CRITERION_DIR" ]]; then
        local baseline_names=()
        while IFS= read -r -d '' bench_dir; do
            for sub in "$bench_dir"/*/; do
                local sub_name
                sub_name=$(basename "$sub")
                # Skip 'new', 'base', 'report' — these are criterion internals
                if [[ "$sub_name" != "new" && "$sub_name" != "base" && "$sub_name" != "report" ]]; then
                    # Check if it looks like a baseline (has estimates.json)
                    if [[ -f "$sub/estimates.json" || -d "$sub" ]]; then
                        baseline_names+=("$sub_name")
                    fi
                fi
            done
        done < <(find "$CRITERION_DIR" -mindepth 1 -maxdepth 1 -type d -not -name '.baselines' -print0 2>/dev/null)

        # Deduplicate and print
        if [[ ${#baseline_names[@]} -gt 0 ]]; then
            printf '%s\n' "${baseline_names[@]}" | sort -u | while read -r bn; do
                echo -e "  ${CYAN}• $bn${RESET}"
            done
        else
            echo -e "  ${DIM}(none found)${RESET}"
        fi
    fi
}

cmd_info() {
    local meta_file="$META_DIR/${BASELINE_NAME}.meta.json"

    if [[ ! -f "$meta_file" ]]; then
        echo -e "${RED}❌ No metadata found for baseline '${BASELINE_NAME}'${RESET}" >&2
        echo -e "${DIM}   File: $meta_file${RESET}" >&2
        exit 2
    fi

    echo -e "${BOLD}═══ Baseline: ${BASELINE_NAME} ═══${RESET}"
    echo ""
    python3 -c "
import json, sys
d = json.load(open('$meta_file'))
print(f\"  Name:      {d['baseline_name']}\")
print(f\"  Date:      {d['timestamp']}\")
print(f\"  Commit:    {d['git']['commit']} ({d['git']['branch']})\")
print(f\"  Machine:   {d['machine']['cpu']}\")
print(f\"  Cores:     {d['machine']['cores']}\")
print(f\"  Memory:    {d['machine']['memory_gb']} GB\")
print(f\"  OS:        {d['machine']['os']}\")
print(f\"  Kernel:    {d['machine']['kernel']}\")
print(f\"  Quick:     {d['criterion']['quick_mode']}\")
filter_val = d['criterion']['filter']
if filter_val and filter_val != 'null':
    print(f\"  Filter:    {filter_val}\")
" 2>/dev/null || {
        echo -e "${DIM}  Raw metadata:${RESET}"
        cat "$meta_file"
    }
}

cmd_compare() {
    echo -e "${BOLD}╔══════════════════════════════════════════════════════════════╗${RESET}"
    echo -e "${BOLD}║       FASTER — Baseline Comparison: ${COMPARE_A} vs ${COMPARE_B}${RESET}"
    echo -e "${BOLD}╚══════════════════════════════════════════════════════════════╝${RESET}"
    echo ""

    # Show metadata for both baselines if available
    for name in "$COMPARE_A" "$COMPARE_B"; do
        local meta_file="$META_DIR/${name}.meta.json"
        if [[ -f "$meta_file" ]]; then
            local commit_short timestamp
            commit_short=$(python3 -c "import json; d=json.load(open('$meta_file')); print(d['git']['commit_short'])" 2>/dev/null || echo "?")
            timestamp=$(python3 -c "import json; d=json.load(open('$meta_file')); print(d['timestamp'][:19])" 2>/dev/null || echo "?")
            echo -e "  ${CYAN}${name}${RESET}: commit ${commit_short} (${timestamp})"
        else
            echo -e "  ${CYAN}${name}${RESET}: ${DIM}(no metadata)${RESET}"
        fi
    done
    echo ""

    # Criterion doesn't have a direct "compare A vs B" mode.
    # The workaround: run with --load-baseline A --baseline B
    local cargo_args=(bench -p faster-core --no-fail-fast)
    local criterion_args=(--load-baseline "$COMPARE_A" --baseline "$COMPARE_B")

    if [[ -n "$BENCH_FILTER" ]]; then
        criterion_args+=("$BENCH_FILTER")
    fi

    echo -e "${BOLD}▶ Comparing baselines...${RESET}"
    echo -e "  ${DIM}$ cargo ${cargo_args[*]} -- ${criterion_args[*]}${RESET}"
    echo ""

    cd "$REPO_ROOT"
    cargo "${cargo_args[@]}" -- "${criterion_args[@]}" || {
        echo -e "\n${RED}❌ Comparison failed. Ensure both baselines exist.${RESET}" >&2
        exit 2
    }
}

cmd_delete() {
    echo -e "${BOLD}Deleting baseline: ${BASELINE_NAME}${RESET}"

    local deleted=false

    # Delete metadata
    local meta_file="$META_DIR/${BASELINE_NAME}.meta.json"
    if [[ -f "$meta_file" ]]; then
        rm -f "$meta_file"
        echo -e "  ${DIM}Removed metadata: $meta_file${RESET}"
        deleted=true
    fi

    # Delete criterion data for this baseline in all benchmark dirs
    if [[ -d "$CRITERION_DIR" ]]; then
        while IFS= read -r -d '' bench_dir; do
            local baseline_dir="$bench_dir/$BASELINE_NAME"
            if [[ -d "$baseline_dir" ]]; then
                rm -rf "$baseline_dir"
                echo -e "  ${DIM}Removed: $baseline_dir${RESET}"
                deleted=true
            fi
        done < <(find "$CRITERION_DIR" -mindepth 1 -maxdepth 1 -type d -not -name '.baselines' -print0 2>/dev/null)
    fi

    if [[ "$deleted" == true ]]; then
        echo -e "${GREEN}✅ Baseline '${BASELINE_NAME}' deleted.${RESET}"
    else
        echo -e "${YELLOW}⚠  No data found for baseline '${BASELINE_NAME}'.${RESET}"
    fi
}

# ── Dispatch ────────────────────────────────────────────────────────────────
case "$COMMAND" in
    save)    cmd_save ;;
    list)    cmd_list ;;
    compare) cmd_compare ;;
    info)    cmd_info ;;
    delete)  cmd_delete ;;
    *)       echo -e "${RED}Unknown command: $COMMAND${RESET}"; exit 2 ;;
esac
