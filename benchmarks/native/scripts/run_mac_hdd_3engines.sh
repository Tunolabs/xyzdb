#!/bin/bash
# Mac local HDD regime runner — Block 5 Phase O Mac variant.
#
# Drives 3 engines (xyzdb, postgres, mongo) sequentially on a single
# Mac-mounted HDD volume with §4.2 reduced cache caps so the
# working-set ≫ cache regime is in effect. Surreal excluded per
# caveat C-12.
#
# Mirrors `run_aws_4engines.sh` structure so AWS HDD scale 1.0 future
# run can reuse same env-var contract (PG_CONFIG_FILE,
# MONGO_CONFIG_FILE, CACHE_SIZE) through that script.
#
# Defaults (override via env):
#   MOUNT_ROOT=/Volumes/disco-d   (symlink without spaces — create
#                                  with: ln -sf "/Volumes/Disco D" /Volumes/disco-d)
#   SCALE=1.0
#   DURATION=900                  (Phase 3 sustained, seconds)
#   COLD_RUNS=50                  (vs AWS 100 — HDD slower, balance
#                                  statistical solidity vs wallclock)
#   ENGINES="xyzdb postgres mongo"
#
# HDD-cap §4.2 regime (C-13):
#   xyzdb     → CACHE_SIZE=512  (vs SSD default 1024)
#   postgres  → PG_CONFIG_FILE=postgresql-t6-hdd-cap.conf
#               (shared_buffers=256MB vs SSD default 2GB)
#   mongo     → MONGO_CONFIG_FILE=mongod-t6-hdd-cap.conf
#               (cacheSizeGB=0.5 vs SSD default 3.0)
#
# Output: benchmarks/native/results/mac-hdd-scale1.0-fase-O/
#   - mac_hdd_<engine>_a.{json,csv,md}   (renamed from orchestrator's
#                                          timestamped output)
#   - mac_hdd_<engine>_a.run.log         (per-cell stdout)
#   - mac_hdd_<engine>_a.disk.txt        (pre/post du)
#   - master.log                          (cell starts/exits)
#   - summary.md                          (verdict pivot)

set -u  # NOT -e: each cell exit code is part of the verdict surface.

# Refinement #15 silenced-cell gate.
# shellcheck source=./_lib_validate_cell.sh
source "$(dirname "$0")/_lib_validate_cell.sh"

cd "$(dirname "$0")/.."   # benchmarks/native/

MOUNT_ROOT="${MOUNT_ROOT:-/Volumes/disco-d}"
SCALE="${SCALE:-1.0}"
DURATION="${DURATION:-900}"
COLD_RUNS="${COLD_RUNS:-50}"
ENGINES="${ENGINES:-xyzdb postgres mongo}"

OUTPUT_DIR="./results/mac-hdd-scale1.0-fase-O"
MASTER_LOG="$OUTPUT_DIR/master.log"
SUMMARY_MD="$OUTPUT_DIR/summary.md"
mkdir -p "$OUTPUT_DIR"

if [ ! -d "$MOUNT_ROOT" ]; then
    echo "FATAL: MOUNT_ROOT=$MOUNT_ROOT does not exist" >&2
    exit 2
fi
if [ ! -w "$MOUNT_ROOT" ]; then
    echo "FATAL: MOUNT_ROOT=$MOUNT_ROOT is not writable" >&2
    exit 2
fi
if [ ! -x ./target/release/native-bench ]; then
    echo "FATAL: ./target/release/native-bench missing — run cargo build --release first" >&2
    exit 2
fi
if [ ! -f ./golden/golden-scale1-seed42.json ]; then
    echo "FATAL: ./golden/golden-scale1-seed42.json missing — run \`./target/release/golden_dump --scale 1.0 --seed 42 --out-dir ./golden\` first" >&2
    exit 2
fi

profile_for() {
    case "$1" in
        xyzdb)    echo "xyzdb" ;;
        postgres) echo "postgres" ;;
        mongo)    echo "mongo" ;;
    esac
}

container_for() {
    case "$1" in
        xyzdb)    echo "native-xyzdb-1" ;;
        postgres) echo "native-postgres-1" ;;
        mongo)    echo "native-mongodb-1" ;;
    esac
}

data_path_for() {
    local engine="$1"
    case "$engine" in
        xyzdb)    echo "$MOUNT_ROOT/xyzdata" ;;
        postgres) echo "$MOUNT_ROOT/pgdata" ;;
        mongo)    echo "$MOUNT_ROOT/mongodata" ;;
    esac
}

export_data_env() {
    local engine="$1" path="$2"
    case "$engine" in
        xyzdb)    export XYZ_DATA="$path" ;;
        postgres) export PG_DATA="$path" ;;
        mongo)    export MONGO_DATA="$path" ;;
    esac
}

# C-13: HDD-cap regime — reduce engine cache so working-set ≫ cache.
export_caps_env() {
    local engine="$1"
    # Clear any inherited override from prior cells.
    unset PG_CONFIG_FILE MONGO_CONFIG_FILE CACHE_SIZE
    case "$engine" in
        xyzdb)    export CACHE_SIZE=512 ;;
        postgres) export PG_CONFIG_FILE=postgresql-t6-hdd-cap.conf ;;
        mongo)    export MONGO_CONFIG_FILE=mongod-t6-hdd-cap.conf ;;
    esac
    export STORAGE_PROFILE=hdd
    export IO_SCHEDULER=hdd
}

wait_for_engine() {
    local engine="$1" timeout=120 i=0
    case "$engine" in
        xyzdb)
            until nc -z 127.0.0.1 2505 2>/dev/null; do
                i=$((i+1)); [ "$i" -ge "$timeout" ] && return 1; sleep 1
            done ;;
        postgres)
            until nc -z 127.0.0.1 5432 2>/dev/null; do
                i=$((i+1)); [ "$i" -ge "$timeout" ] && return 1; sleep 1
            done
            # PG initdb fast-shutdown + restart race; sleep 10 clears it
            # reliably on slower disks (Mac APFS USB, AWS HDD st1).
            sleep 10 ;;
        mongo)
            until nc -z 127.0.0.1 27017 2>/dev/null; do
                i=$((i+1)); [ "$i" -ge "$timeout" ] && return 1; sleep 1
            done
            sleep 10 ;;
    esac
    return 0
}

run_cell() {
    local engine="$1"
    local profile container data_path cell cell_log disk_log
    profile=$(profile_for "$engine")
    container=$(container_for "$engine")
    data_path=$(data_path_for "$engine")
    cell="mac_hdd_${engine}"
    cell_log="$OUTPUT_DIR/${cell}.run.log"
    disk_log="$OUTPUT_DIR/${cell}.disk.txt"

    echo ""                                                | tee -a "$MASTER_LOG"
    echo "=========================================="     | tee -a "$MASTER_LOG"
    echo "=== cell: $cell  start: $(date '+%H:%M:%S') ===" | tee -a "$MASTER_LOG"
    echo "=== data_path: $data_path ===" | tee -a "$MASTER_LOG"
    echo "==========================================" | tee -a "$MASTER_LOG"
    local t0; t0=$(date +%s)

    rm -rf "$data_path" 2>/dev/null
    mkdir -p "$data_path"
    chmod 1777 "$data_path"

    {
        echo "=== disk pre-load $(date '+%H:%M:%S') ==="
        du -sh "$data_path" 2>/dev/null || echo "du failed"
        df -h "$MOUNT_ROOT" 2>/dev/null
    } > "$disk_log"

    export_data_env "$engine" "$data_path"
    export_caps_env "$engine"

    docker compose --profile "$profile" up -d 2>&1 \
        | tee -a "$cell_log" >> "$MASTER_LOG"

    if ! wait_for_engine "$engine"; then
        echo "!!! $engine failed to listen within timeout" \
            | tee -a "$cell_log" "$MASTER_LOG"
        docker compose --profile "$profile" down -v --remove-orphans 2>&1 >> "$MASTER_LOG"
        rm -rf "$data_path" 2>/dev/null
        return 1
    fi
    echo "$engine ready at $(date '+%H:%M:%S')" | tee -a "$cell_log" "$MASTER_LOG"

    ./target/release/native-bench \
        --engine "$engine" \
        --scale "$SCALE" \
        --storage hdd \
        --schema-mode full \
        --duration "$DURATION" \
        --cold-runs "$COLD_RUNS" \
        --container-name "$container" \
        --data-path "$data_path" \
        --golden ./golden/golden-scale1-seed42.json \
        --output "$OUTPUT_DIR" 2>&1 | tee -a "$cell_log" >> "$MASTER_LOG"
    local rc=${PIPESTATUS[0]}

    {
        echo ""
        echo "=== disk post-run $(date '+%H:%M:%S') ==="
        du -sh "$data_path" 2>/dev/null || echo "du failed"
        df -h "$MOUNT_ROOT" 2>/dev/null
    } >> "$disk_log"

    # Rename orchestrator's timestamped output to cell-keyed name.
    for ext in json csv md; do
        local newest
        newest=$(ls -t "$OUTPUT_DIR/${engine}-hdd-scale${SCALE}-"*."$ext" 2>/dev/null | head -1)
        [ -n "${newest:-}" ] && mv "$newest" "$OUTPUT_DIR/${cell}.${ext}"
    done

    if ! validate_cell_queries "$engine" "$OUTPUT_DIR/${cell}.json" >/dev/null 2>&1; then
        rc=1
        echo "=== cell: $cell  GATE FAIL (silenced cold queries) ===" | tee -a "$MASTER_LOG"
    fi

    docker compose --profile "$profile" down -v --remove-orphans 2>&1 >> "$MASTER_LOG"
    rm -rf "$data_path" 2>/dev/null

    local t1; t1=$(date +%s)
    local dt=$((t1 - t0))
    echo "=== cell: $cell  exit=$rc  wall=${dt}s ===" | tee -a "$MASTER_LOG"
    return $rc
}

main_t0=$(date +%s)
{
    echo "=== Mac local HDD scale 1.0 regime runner (3 engines, C-12 Surreal excluded) ==="
    echo "=== started: $(date '+%Y-%m-%d %H:%M:%S %z') ==="
    echo "=== host: $(uname -n) / kernel: $(uname -r) ==="
    echo "=== docker: $(docker --version | awk '{print $3}' | tr -d ',') ==="
    echo "=== mount_root: $MOUNT_ROOT ==="
    echo "=== scale: $SCALE  duration: ${DURATION}s  cold_runs: $COLD_RUNS ==="
    echo "=== engines: $ENGINES ==="
    echo "=== HDD-cap §4.2 (C-13): xyzDB CACHE_SIZE=512, PG shared_buffers=256MB, Mongo cacheSizeGB=0.5 ==="
    df -h "$MOUNT_ROOT"
} | tee -a "$MASTER_LOG"

LABELS=()
VERDICTS=()
WALLS=()

for engine in $ENGINES; do
    cell_t0=$(date +%s)
    if run_cell "$engine"; then
        verdict="PASS"
    else
        verdict="FAIL"
    fi
    cell_t1=$(date +%s)
    LABELS+=("${engine}_a")
    VERDICTS+=("$verdict")
    WALLS+=($((cell_t1 - cell_t0)))
done

main_t1=$(date +%s)
total_dt=$((main_t1 - main_t0))

echo ""                                                | tee -a "$MASTER_LOG"
echo "=========================================="     | tee -a "$MASTER_LOG"
echo "=== Mac HDD scale 1.0 verdict summary ==="     | tee -a "$MASTER_LOG"
echo "=========================================="     | tee -a "$MASTER_LOG"
pass=0; fail=0
for ((i=0; i<${#LABELS[@]}; i++)); do
    label="${LABELS[$i]}"
    v="${VERDICTS[$i]}"
    w="${WALLS[$i]}"
    printf "  %-25s %-4s  wall=%ss\n" "$label" "$v" "$w" \
        | tee -a "$MASTER_LOG"
    [ "$v" = "PASS" ] && pass=$((pass+1)) || fail=$((fail+1))
done
n_cells=${#LABELS[@]}
echo ""                                                | tee -a "$MASTER_LOG"
echo "=== totals: ${pass}/${n_cells} PASS, ${fail}/${n_cells} FAIL ===" | tee -a "$MASTER_LOG"
echo "=== wall time: ${total_dt}s ===" | tee -a "$MASTER_LOG"
echo "=== finished: $(date '+%Y-%m-%d %H:%M:%S %z') ===" | tee -a "$MASTER_LOG"

# Pivot summary (markdown).
{
    echo "# Mac HDD scale 1.0 — verdict (Block 5 Phase O Mac variant)"
    echo ""
    echo "Generated: $(date '+%Y-%m-%d %H:%M:%S %z')"
    echo "Host: $(uname -n) · mount_root: $MOUNT_ROOT"
    echo "Scale: $SCALE · duration: ${DURATION}s · cold_runs: $COLD_RUNS"
    echo "HDD-cap §4.2 (C-13): xyzDB CACHE_SIZE=512, PG shared_buffers=256MB, Mongo cacheSizeGB=0.5"
    echo "Wall: ${total_dt}s · Cells: ${pass}/${n_cells} PASS, ${fail}/${n_cells} FAIL"
    echo ""
    echo "## Cell verdicts"
    echo ""
    echo "| engine | verdict | wall (s) | json | disk |"
    echo "|---|---|---|---|---|"
    for ((i=0; i<${#LABELS[@]}; i++)); do
        label="${LABELS[$i]}"
        v="${VERDICTS[$i]}"
        w="${WALLS[$i]}"
        eng="${label%_*}"
        echo "| $eng | $v | $w | mac_hdd_${label}.json | mac_hdd_${label}.disk.txt |"
    done
    echo ""
    echo "Per-cell artefacts in this directory."
    echo "Resource samples (CPU, memory, disk timeline) embedded in each \`.json\`."
    echo "Disk pre/post snapshots in \`*.disk.txt\`."
} > "$SUMMARY_MD"

[ "$fail" -eq 0 ] && exit 0 || exit 1
