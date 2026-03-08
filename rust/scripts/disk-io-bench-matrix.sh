#!/usr/bin/env bash
#
# Disk I/O benchmark matrix runner for FASTER.
#
# Runs all device × client × workload × thread combinations and
# collects results into a single CSV file.
#
# Usage:
#   ./scripts/disk-io-bench-matrix.sh [OPTIONS]
#
# Options:
#   --data-dir DIR      Storage directory (default: /tmp/faster-bench-data)
#   --output FILE       Output CSV file (default: disk-io-results.csv)
#   --duration SECS     Measurement duration per run (default: 30)
#   --warmup SECS       Warmup duration per run (default: 10)
#   --num-keys N        Number of keys (default: 10000000)
#   --quick             Quick mode: 5s runs, 2s warmup, fewer combos
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Defaults
DATA_DIR="/tmp/faster-bench-data"
OUTPUT="disk-io-results.csv"
DURATION=30
WARMUP=10
NUM_KEYS=10000000
QUICK=0

# Parse args
while [[ $# -gt 0 ]]; do
    case $1 in
        --data-dir)  DATA_DIR="$2"; shift 2 ;;
        --output)    OUTPUT="$2"; shift 2 ;;
        --duration)  DURATION="$2"; shift 2 ;;
        --warmup)    WARMUP="$2"; shift 2 ;;
        --num-keys)  NUM_KEYS="$2"; shift 2 ;;
        --quick)     QUICK=1; shift ;;
        *)           echo "Unknown option: $1"; exit 1 ;;
    esac
done

if [[ $QUICK -eq 1 ]]; then
    DURATION=5
    WARMUP=2
    NUM_KEYS=1000000
fi

# Build
echo "╔══════════════════════════════════════════════════════════════╗"
echo "║          FASTER Disk I/O Benchmark Matrix                   ║"
echo "╚══════════════════════════════════════════════════════════════╝"
echo ""
echo "Building disk-io-bench (release)..."
cd "$REPO_ROOT"
cargo build --release -p disk-io-bench 2>&1 | tail -5

BENCH_BIN="$REPO_ROOT/target/release/disk-io-bench"
if [[ ! -x "$BENCH_BIN" ]]; then
    echo "ERROR: benchmark binary not found at $BENCH_BIN"
    exit 1
fi

# Prepare storage
mkdir -p "$DATA_DIR"
echo ""
echo "Configuration:"
echo "  Data dir:   $DATA_DIR"
echo "  Output:     $OUTPUT"
echo "  Duration:   ${DURATION}s (warmup: ${WARMUP}s)"
echo "  Keys:       $NUM_KEYS"
echo ""

# Detect available devices
DEVICES="sync tokio"
if [[ "$(uname -s)" == "Linux" ]]; then
    # Check kernel supports io_uring (5.10+)
    KERNEL_MAJOR=$(uname -r | cut -d. -f1)
    KERNEL_MINOR=$(uname -r | cut -d. -f2)
    if [[ $KERNEL_MAJOR -gt 5 ]] || [[ $KERNEL_MAJOR -eq 5 && $KERNEL_MINOR -ge 10 ]]; then
        DEVICES="sync uring tokio"
        echo "  io_uring:   available (kernel $(uname -r))"
    else
        echo "  io_uring:   unavailable (kernel $(uname -r), need 5.10+)"
    fi
else
    echo "  io_uring:   unavailable (not Linux)"
fi

CLIENTS="sync tokio"
WORKLOADS="overcommit write-heavy mixed scan"

if [[ $QUICK -eq 1 ]]; then
    THREAD_COUNTS="1 4 8"
else
    THREAD_COUNTS="1 4 8 16"
fi

echo ""

# Drop page cache before starting (if root)
if [[ $EUID -eq 0 ]]; then
    echo 1 > /proc/sys/vm/drop_caches
    echo "  Page cache: cleared"
fi

# Start iostat in background if available
IOSTAT_PID=""
IOSTAT_LOG="$DATA_DIR/iostat.log"
if command -v iostat &>/dev/null; then
    iostat -xz 5 > "$IOSTAT_LOG" 2>&1 &
    IOSTAT_PID=$!
    echo "  iostat:     recording to $IOSTAT_LOG (PID $IOSTAT_PID)"
fi

echo ""

# CSV header
echo "device,client,workload,threads,num_keys,value_size,buffer_pages,ops_per_sec,throughput_mb_s,p50_ns,p99_ns,p999_ns,pending_rate,total_ops,total_reads,total_writes,total_pending,elapsed_secs" > "$OUTPUT"

# Run matrix
TOTAL=0
for d in $DEVICES; do
    for c in $CLIENTS; do
        for w in $WORKLOADS; do
            for t in $THREAD_COUNTS; do
                TOTAL=$((TOTAL + 1))
            done
        done
    done
done

RUN=0
for device in $DEVICES; do
    for client in $CLIENTS; do
        for workload in $WORKLOADS; do
            for threads in $THREAD_COUNTS; do
                RUN=$((RUN + 1))
                echo "── [$RUN/$TOTAL] $device + $client | $workload | ${threads}T ──"

                # Run benchmark with CSV output, append to file
                $BENCH_BIN \
                    --device "$device" \
                    --client "$client" \
                    --workload "$workload" \
                    --threads "$threads" \
                    --num-keys "$NUM_KEYS" \
                    --duration "$DURATION" \
                    --warmup "$WARMUP" \
                    --data-dir "$DATA_DIR" \
                    --output csv 2>&1 | tail -1 >> "$OUTPUT"

                echo ""
            done
        done
    done
done

# Stop iostat
if [[ -n "$IOSTAT_PID" ]]; then
    kill "$IOSTAT_PID" 2>/dev/null || true
    echo "iostat log: $IOSTAT_LOG"
fi

echo ""
echo "════════════════════════════════════════════════════════════════"
echo "  Benchmark matrix complete: $RUN runs"
echo "  Results: $OUTPUT"
echo "════════════════════════════════════════════════════════════════"
