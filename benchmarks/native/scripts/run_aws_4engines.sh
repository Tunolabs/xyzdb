#!/bin/bash
# AWS 4-engine sequential runner — Phase 6 / 7 SSD or HDD pass.
#
# Drives one cell per engine on a single storage-backed mount.
# Each cell:
#   1. Provisions a fresh data directory under MOUNT_ROOT/<engine>data
#      with world-writable permissions (PG/Mongo containers run as
#      non-root and need to write into the bind mount).
#   2. Brings up the engine container via the existing docker-compose
#      profile, with XYZ_DATA / PG_DATA / MONGO_DATA
#      env vars overridden so the volume mounts target MOUNT_ROOT,
#      plus STORAGE_PROFILE + IO_SCHEDULER env vars so PG picks the
#      right t6-{ssd,hdd}.conf and xyzdb's CLI flags match the
#      device class.
#   3. Drives the orchestrator (Phase 0-5 default = setup, load, cold,
#      concurrent, verify). Resource sampling stays ON — the
#      orchestrator's ResourceSampler resolves --container-name and
#      --data-path from the same env vars and runs `docker stats` +
#      `du -sk` per phase. Outputs go to results/<engine>-<storage>-
#      scale*-<ts>.{json,csv,md} (timestamped, then renamed to the
#      cell key).
#   4. Captures pre/post disk usage on the data dir as a sanity sample
#      independent of the sampler.
#   5. Tears the container down and removes the data dir before the
#      next cell — engines run strictly serial (memory
#      feedback_bench_engines_serial.md), the storage budget is reused
#      across cells.
#
# Defaults (override via env):
#   STORAGE=ssd                 (or hdd)
#   MOUNT_ROOT=/mnt/$STORAGE    (auto-derived from STORAGE)
#   SCALE=0.1
#   DURATION=1800               (Phase 3 sustained, seconds)
#   COLD_RUNS=100               (canonical AWS, NOT 5 like local smoke)
#   ENGINES="xyzdb postgres mongo"
#
# Example overrides:
#   SCALE=1.0 DURATION=3600 COLD_RUNS=100 \
#     bash scripts/run_aws_4engines.sh                          # SSD canonical
#   STORAGE=hdd SCALE=0.1 DURATION=1800 \
#     bash scripts/run_aws_4engines.sh                          # HDD pass
#   STORAGE=hdd MOUNT_ROOT=/mnt/spinning SCALE=0.1 \
#     bash scripts/run_aws_4engines.sh                          # custom mount
#   ENGINES="xyzdb postgres" \
#     bash scripts/run_aws_4engines.sh                          # quick smoke
#
# Output:
#   results/aws_<storage>_<engine>.{json,csv,md}                (renamed
#                                                                from
#                                                                orchestrator's
#                                                                timestamped
#                                                                output)
#   results/aws_<storage>_<engine>.run.log                      (per-cell
#                                                                stdout)
#   results/aws_<storage>_<engine>.disk.txt                     (pre/post
#                                                                du -sb)
#   results/aws_<storage>.master.log                            (cell
#                                                                starts/exits)
#   results/aws_<storage>.summary.md                            (verdict
#                                                                pivot)

set -u  # NOT -e: each cell exit code is part of the verdict surface.

# v0.3.3 Phase 5.b refinement #15 — silenced-cell gate.
# shellcheck source=./_lib_validate_cell.sh
source "$(dirname "$0")/_lib_validate_cell.sh"

cd "$(dirname "$0")/.."   # benchmarks/native/

STORAGE="${STORAGE:-ssd}"
case "$STORAGE" in
    ssd|hdd) ;;
    *) echo "FATAL: STORAGE must be 'ssd' or 'hdd' (got '$STORAGE')" >&2; exit 2 ;;
esac
MOUNT_ROOT="${MOUNT_ROOT:-/mnt/$STORAGE}"

SCALE="${SCALE:-0.1}"
DURATION="${DURATION:-1800}"
COLD_RUNS="${COLD_RUNS:-100}"
ENGINES="${ENGINES:-xyzdb postgres mongo}"
# verify_golden anchor. Note the file-name quirk: scale 1.0's golden is
# "scale1", every other scale keeps its literal (0.1 -> "scale0.1").
GOLDEN="${GOLDEN:-golden/golden-scale$([ "$SCALE" = "1.0" ] && echo 1 || echo "$SCALE")-seed42.json}"

MASTER_LOG="./results/aws_${STORAGE}.master.log"
SUMMARY_MD="./results/aws_${STORAGE}.summary.md"
mkdir -p ./results

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

# Path under MOUNT_ROOT for this engine's data dir. Each engine gets
# its own subdir so cross-cell leftovers cannot collide.
data_path_for() {
    local engine="$1"
    case "$engine" in
        xyzdb)    echo "$MOUNT_ROOT/xyzdata" ;;
        postgres) echo "$MOUNT_ROOT/pgdata" ;;
        mongo)    echo "$MOUNT_ROOT/mongodata" ;;
    esac
}

# Export the compose env var for this engine so docker-compose.yml
# bind-mounts the SSD path. Other engines' env vars stay unset (their
# profiles aren't up).
export_data_env() {
    local engine="$1" path="$2"
    case "$engine" in
        xyzdb)    export XYZ_DATA="$path" ;;
        postgres) export PG_DATA="$path" ;;
        mongo)    export MONGO_DATA="$path" ;;
    esac
}

# Wait for the engine's listening port. Same probes as smoke_phase5b.sh.
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
            # PG entrypoint runs initdb on first launch with fast-shutdown +
            # restart sequence. The TCP port opens ~700 ms before the
            # restart, then closes briefly, then re-opens. sleep 3 was a
            # race against the second listener bind on slower disks (Mac
            # APFS over USB, EBS st1 cold cache). 10 s clears it reliably.
            sleep 10 ;;
        mongo)
            until nc -z 127.0.0.1 27017 2>/dev/null; do
                i=$((i+1)); [ "$i" -ge "$timeout" ] && return 1; sleep 1
            done
            # Mongo similar pattern: WiredTiger restart on first init.
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
    cell="aws_${STORAGE}_${engine}"
    cell_log="./results/${cell}.run.log"
    disk_log="./results/${cell}.disk.txt"

    echo ""                                                | tee -a "$MASTER_LOG"
    echo "=========================================="     | tee -a "$MASTER_LOG"
    echo "=== cell: $cell  start: $(date '+%H:%M:%S') ===" | tee -a "$MASTER_LOG"
    echo "=== data_path: $data_path ===" | tee -a "$MASTER_LOG"
    echo "=========================================="     | tee -a "$MASTER_LOG"
    local t0; t0=$(date +%s)

    # Provision SSD data dir. World-writable + sticky so PG / Mongo
    # containers (non-root user) can write through the bind mount.
    rm -rf "$data_path" 2>/dev/null
    mkdir -p "$data_path"
    chmod 1777 "$data_path"

    # Pre-load disk size sample (should be near-zero — the dir is fresh).
    {
        echo "=== disk pre-load $(date '+%H:%M:%S') ==="
        du -sb "$data_path" 2>/dev/null || echo "du failed"
        df -h "$MOUNT_ROOT" 2>/dev/null
    } > "$disk_log"

    export_data_env "$engine" "$data_path"

    # Bring up only this engine's profile. STORAGE_PROFILE drives the
    # PG/xyzDB config-file selection (postgresql-t6-{ssd,hdd}.conf and
    # the xyzdb --storage-profile / --io-scheduler CLI flags wired in
    # docker-compose.yml). IO_SCHEDULER tracks STORAGE so xyzdb's
    # internal scheduler matches the device class.
    STORAGE_PROFILE="$STORAGE" IO_SCHEDULER="$STORAGE" \
        docker compose --profile "$profile" up -d 2>&1 \
        | tee -a "$cell_log" >> "$MASTER_LOG"

    if ! wait_for_engine "$engine"; then
        echo "!!! $engine failed to listen within timeout" \
            | tee -a "$cell_log" "$MASTER_LOG"
        docker compose --profile "$profile" down --remove-orphans 2>&1 >> "$MASTER_LOG"
        rm -rf "$data_path" 2>/dev/null
        return 1
    fi
    echo "$engine ready at $(date '+%H:%M:%S')" | tee -a "$cell_log" "$MASTER_LOG"

    # Drive the bench. Resource sampling ON (no --no-resources). The
    # orchestrator picks --container-name and --data-path defaults
    # matching this engine + the env var we exported.
    ./target/release/native-bench \
        --engine "$engine" \
        --scale "$SCALE" \
        --storage "$STORAGE" \
        --schema-mode full \
        --duration "$DURATION" \
        --cold-runs "$COLD_RUNS" \
        --golden "$GOLDEN" \
        --container-name "$container" \
        --data-path "$data_path" \
        --output ./results 2>&1 | tee -a "$cell_log" >> "$MASTER_LOG"
    local rc=${PIPESTATUS[0]}

    # Post-run disk size sample (engine has bulk-loaded + Phase 3 wrote).
    {
        echo ""
        echo "=== disk post-run $(date '+%H:%M:%S') ==="
        du -sb "$data_path" 2>/dev/null || echo "du failed"
        df -h "$MOUNT_ROOT" 2>/dev/null
    } >> "$disk_log"

    # Rename orchestrator's timestamped output to the cell-keyed name so
    # the summary pivot can group by cell. The orchestrator writes
    # <engine>-<storage>-scale<n>-<ts>.{json,csv,md} where <n> is float-
    # formatted — SCALE=1 comes out as "scale1.0", so glob both spellings
    # (the exact-match-only glob silently skipped the rename and the
    # summary lost the cell).
    for ext in json csv md; do
        local newest
        newest=$(ls -t "./results/${engine}-${STORAGE}-scale${SCALE}-"*."$ext" \
                       "./results/${engine}-${STORAGE}-scale${SCALE}.0-"*."$ext" 2>/dev/null | head -1)
        [ -n "${newest:-}" ] && mv "$newest" "./results/${cell}.${ext}"
    done

    # Refinement #15 silenced-cell gate: any cold-phase Q with n_runs=0
    # that is NOT in the declared-deferrals list FAILS the cell.
    if ! validate_cell_queries "$engine" "./results/${cell}.json" 2>&1 | tee -a "$cell_log" >> "$MASTER_LOG"; then
        true   # piped tee returns 0; we re-run validate to capture rc
    fi
    if ! validate_cell_queries "$engine" "./results/${cell}.json" >/dev/null 2>&1; then
        rc=1
        echo "=== cell: $cell  GATE FAIL (silenced cold queries) ===" | tee -a "$MASTER_LOG"
    fi

    docker compose --profile "$profile" down --remove-orphans 2>&1 >> "$MASTER_LOG"
    rm -rf "$data_path" 2>/dev/null

    local t1; t1=$(date +%s)
    local dt=$((t1 - t0))
    echo "=== cell: $cell  exit=$rc  wall=${dt}s ===" | tee -a "$MASTER_LOG"
    return $rc
}

main_t0=$(date +%s)
{
    echo "=== AWS 4-engine sequential runner ==="
    echo "=== started: $(date '+%Y-%m-%d %H:%M:%S %z') ==="
    echo "=== host: $(uname -n) / kernel: $(uname -r) ==="
    echo "=== docker: $(docker --version | awk '{print $3}' | tr -d ',') ==="
    echo "=== storage: $STORAGE  mount_root: $MOUNT_ROOT ==="
    echo "=== scale: $SCALE  duration: ${DURATION}s  cold_runs: $COLD_RUNS ==="
    echo "=== engines: $ENGINES ==="
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
    LABELS+=("${engine}")
    VERDICTS+=("$verdict")
    WALLS+=($((cell_t1 - cell_t0)))
done

main_t1=$(date +%s)
total_dt=$((main_t1 - main_t0))

echo ""                                                | tee -a "$MASTER_LOG"
echo "=========================================="     | tee -a "$MASTER_LOG"
echo "=== AWS ${STORAGE} verdict summary ==="          | tee -a "$MASTER_LOG"
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
    echo "# AWS ${STORAGE} 4-engine sequential — verdict"
    echo ""
    echo "Generated: $(date '+%Y-%m-%d %H:%M:%S %z')"
    echo "Host: $(uname -n) · storage: $STORAGE · mount_root: $MOUNT_ROOT"
    echo "Scale: $SCALE · duration: ${DURATION}s · cold_runs: $COLD_RUNS"
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
        echo "| $label | $v | $w | aws_${STORAGE}_${label}.json | aws_${STORAGE}_${label}.disk.txt |"
    done
    echo ""
    echo "Per-cell artefacts in \`benchmarks/native/results/\`."
    echo "Resource samples (CPU, memory, disk timeline) embedded in each \`.json\`."
    echo "Disk pre/post snapshots in \`*.disk.txt\`."
} > "$SUMMARY_MD"

[ "$fail" -eq 0 ] && exit 0 || exit 1
