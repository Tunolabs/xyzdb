#!/bin/bash
# Phase 5.b functional smoke runner — v0.3.3 cycle gate.
#
# Drives 3 cells: one per engine (xyzdb, postgres, mongo). Each cell
# runs the full Phase 0..5 pipeline at scale 0.1, duration 30 s,
# cold-runs 5, schema-mode full.
#
# Acceptance gates (per design §13.1 Phase 5):
#   1. Functional Q1-Q10 — no crash, expected result returned.
#   2. Cadence dispatch correctness — 3 REFRESH / $merge threads spawn.
#
# This is functional smoke — green/red, not measurement. Numbers
# from this script are NOT authoritative bench output (Phase 7
# AWS matrix is). Per cell teardown of containers + data volumes
# guarantees clean schema state for the next cell.
#
# Output:
#   results/phase5b_<engine>.{json,csv,md}
#   results/phase5b_<engine>.run.log
#   results/phase5b.master.log

set -u  # do NOT -e: surfacing each cell's exit code is the goal

# v0.3.3 Phase 5.b refinement #15 — silenced-cell gate.
# shellcheck source=./_lib_validate_cell.sh
source "$(dirname "$0")/_lib_validate_cell.sh"

cd "$(dirname "$0")/.."   # benchmarks/native/

MASTER_LOG=./results/phase5b.master.log
mkdir -p ./results

declare -a ENGINES=(xyzdb postgres mongo)

# Map engine → compose profile + container probe
profile_for() {
    case "$1" in
        xyzdb)    echo "xyzdb" ;;
        postgres) echo "postgres" ;;
        mongo)    echo "mongo" ;;
    esac
}

datadir_for() {
    case "$1" in
        xyzdb)    echo "./data/xyzdata" ;;
        postgres) echo "./data/pgdata" ;;
        mongo)    echo "./data/mongodata" ;;
    esac
}

# Wait for engine to accept connections after `up -d`. Each engine
# has a different probe; PG + Mongo also have docker healthchecks
# but we double-check from host because compose health doesn't
# guarantee in-container app readiness for our driver path.
wait_for_engine() {
    local engine="$1"
    local timeout=60
    local i=0
    case "$engine" in
        xyzdb)
            until nc -z 127.0.0.1 2505 2>/dev/null; do
                i=$((i+1)); [ "$i" -ge "$timeout" ] && return 1; sleep 1
            done
            ;;
        postgres)
            until nc -z 127.0.0.1 5432 2>/dev/null; do
                i=$((i+1)); [ "$i" -ge "$timeout" ] && return 1; sleep 1
            done
            sleep 2  # PG accepts TCP a moment before it accepts queries
            ;;
        mongo)
            until nc -z 127.0.0.1 27017 2>/dev/null; do
                i=$((i+1)); [ "$i" -ge "$timeout" ] && return 1; sleep 1
            done
            sleep 2
            ;;
    esac
    return 0
}

run_cell() {
    local engine="$1"
    local profile data_dir
    profile=$(profile_for "$engine")
    data_dir=$(datadir_for "$engine")
    local cell="phase5b_${engine}"
    local cell_log="./results/${cell}.run.log"
    local cell_t0 cell_t1 cell_dt

    echo ""                                              | tee -a "$MASTER_LOG"
    echo "================================================" | tee -a "$MASTER_LOG"
    echo "=== cell: $cell  start: $(date '+%H:%M:%S') ===" | tee -a "$MASTER_LOG"
    echo "================================================" | tee -a "$MASTER_LOG"
    cell_t0=$(date +%s)

    # Fresh data dir for this cell.
    rm -rf "$data_dir" 2>/dev/null
    mkdir -p "$data_dir"

    # Bring up just this engine's profile.
    STORAGE_PROFILE=ssd docker compose --profile "$profile" up -d 2>&1 \
        | tee -a "$cell_log" >> "$MASTER_LOG"

    if ! wait_for_engine "$engine"; then
        echo "!!! $engine failed to come up within timeout" | tee -a "$cell_log" "$MASTER_LOG"
        docker compose --profile "$profile" down --remove-orphans 2>&1 >> "$MASTER_LOG"
        return 1
    fi

    echo "$engine ready at $(date '+%H:%M:%S')" | tee -a "$cell_log" "$MASTER_LOG"

    # Drive bench. --no-resources because Phase 5.b is functional
    # smoke, not measurement: skip docker stats sampling overhead.
    ./target/release/native-bench \
        --engine "$engine" \
        --scale 0.1 \
        --storage ssd \
        --schema-mode full \
        --duration 30 \
        --cold-runs 5 \
        --no-resources \
        --output ./results 2>&1 | tee -a "$cell_log" >> "$MASTER_LOG"
    local rc=${PIPESTATUS[0]}

    # Refinement #15 silenced-cell gate.
    local cell_json
    cell_json=$(ls -t ./results/${engine}-ssd-scale0.1-*.json 2>/dev/null | head -1)
    if [ -n "$cell_json" ]; then
        if ! validate_cell_queries "$engine" "$cell_json" 2>&1 | tee -a "$cell_log" >> "$MASTER_LOG"; then
            true
        fi
        if ! validate_cell_queries "$engine" "$cell_json" >/dev/null 2>&1; then
            rc=1
            echo "=== cell: $cell  GATE FAIL (silenced cold queries) ===" | tee -a "$MASTER_LOG"
        fi
    fi

    docker compose --profile "$profile" down --remove-orphans 2>&1 >> "$MASTER_LOG"
    rm -rf "$data_dir" 2>/dev/null

    cell_t1=$(date +%s)
    cell_dt=$((cell_t1 - cell_t0))
    echo "=== cell: $cell  exit=$rc  wall=${cell_dt}s ===" | tee -a "$MASTER_LOG"
    return $rc
}

main_t0=$(date +%s)
echo "=== Phase 5.b smoke runner ===" | tee -a "$MASTER_LOG"
echo "=== started: $(date '+%Y-%m-%d %H:%M:%S %z') ===" | tee -a "$MASTER_LOG"
echo "=== host: $(uname -n) / docker: $(docker --version | awk '{print $3}' | tr -d ',') ===" | tee -a "$MASTER_LOG"

# macOS bash is 3.2 (no associative arrays); use parallel indexed
# arrays keyed by enumeration order. `LABELS[i]` is the cell name,
# `VERDICTS[i]` is PASS/FAIL, `WALLS[i]` is the cell duration.
LABELS=()
VERDICTS=()
WALLS=()

for engine in "${ENGINES[@]}"; do
    cell_t0=$(date +%s)
    if run_cell "$engine"; then
        verdict="PASS"
    else
        verdict="FAIL"
    fi
    cell_t1=$(date +%s)
    LABELS+=("${engine}")
    VERDICTS+=("$verdict")
    WALLS+=($((cell_t1 - cell_t0)))
done

main_t1=$(date +%s)
total_dt=$((main_t1 - main_t0))

echo ""                                                | tee -a "$MASTER_LOG"
echo "================================================" | tee -a "$MASTER_LOG"
echo "=== Phase 5.b verdict summary ==="                | tee -a "$MASTER_LOG"
echo "================================================" | tee -a "$MASTER_LOG"
pass=0; fail=0
for ((i=0; i<${#LABELS[@]}; i++)); do
    label="${LABELS[$i]}"
    v="${VERDICTS[$i]}"
    w="${WALLS[$i]}"
    printf "  %-20s %-4s  wall=%ss\n" "$label" "$v" "$w" \
        | tee -a "$MASTER_LOG"
    [ "$v" = "PASS" ] && pass=$((pass+1)) || fail=$((fail+1))
done
n_cells=${#LABELS[@]}
echo ""                                                | tee -a "$MASTER_LOG"
echo "=== totals: ${pass}/${n_cells} PASS, ${fail}/${n_cells} FAIL ===" | tee -a "$MASTER_LOG"
echo "=== wall time: ${total_dt}s ===" | tee -a "$MASTER_LOG"
echo "=== finished: $(date '+%Y-%m-%d %H:%M:%S %z') ===" | tee -a "$MASTER_LOG"

[ "$fail" -eq 0 ] && exit 0 || exit 1
