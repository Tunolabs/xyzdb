#!/bin/bash
# Run Bench A Scale 1.0 HDD on PostgreSQL only.
# Uses the external HDD mounted at /Volumes/Disco D as the data dir
# (1 TB USB drive, physically rotational).
#
# Designed for unattended overnight execution. Estimated wall clock
# 2-3h based on Scale 0.1 HDD results extrapolated.
#
# Output:
#   results/postgres-hdd-scale1.0-<UTC-ts>.{json,csv,md}
#   results/scale1.0-hdd-postgres.master.log
#
# Companion scripts (not yet created): run_scale1.0_hdd_xyzdb.sh,
# run_scale1.0_hdd_mongo.sh — copy this and adjust the engine block.

set -u  # do not use -e: surfacing the bench exit code is the goal

cd "$(dirname "$0")/.."   # benchmarks/native/

# HDD data path. Override via PG_DATA_HDD env var if your HDD lives elsewhere.
PG_DATA_HDD="${PG_DATA_HDD:-/Volumes/Disco D/pgdata}"

# Sanity: ensure the HDD path's parent exists and is mounted.
HDD_PARENT="$(dirname "$PG_DATA_HDD")"
if [ ! -d "$HDD_PARENT" ]; then
    echo "ERROR: HDD parent path does not exist: $HDD_PARENT"
    echo "  Mount the HDD first or override with PG_DATA_HDD=<path>"
    exit 1
fi

MASTER_LOG=./results/scale1.0-hdd-postgres.master.log
mkdir -p ./results

echo "=== Bench A Scale 1.0 HDD — PostgreSQL ==="                              | tee -a "$MASTER_LOG"
echo "=== started: $(date '+%Y-%m-%d %H:%M:%S %z') ==="                        | tee -a "$MASTER_LOG"
echo "=== host: $(uname -n) / docker: $(docker --version | awk '{print $3}' | tr -d ',') ===" | tee -a "$MASTER_LOG"
echo "=== HDD data path: $PG_DATA_HDD ==="                                     | tee -a "$MASTER_LOG"
echo "=== HDD free space: $(df -h "$HDD_PARENT" | tail -1 | awk '{print $4}') ===" | tee -a "$MASTER_LOG"

# Fresh data dir on HDD
echo ""                                                                         | tee -a "$MASTER_LOG"
echo "=== cleanup HDD data dir ==="                                             | tee -a "$MASTER_LOG"
rm -rf "$PG_DATA_HDD"
mkdir -p "$PG_DATA_HDD"

# Bring up postgres with HDD storage profile + data dir on the HDD volume.
echo ""                                                                         | tee -a "$MASTER_LOG"
echo "=== bringing up postgres (STORAGE_PROFILE=hdd, PG_DATA=$PG_DATA_HDD) ===" | tee -a "$MASTER_LOG"
PG_DATA="$PG_DATA_HDD" STORAGE_PROFILE=hdd docker compose --profile postgres up -d 2>&1 | tee -a "$MASTER_LOG"

# Wait for postgres to accept connections.
echo "waiting for postgres to be ready..."                                      | tee -a "$MASTER_LOG"
until docker exec native-postgres-1 pg_isready -U postgres -d bench 2>/dev/null | grep -q "accepting connections"; do sleep 2; done
echo "postgres ready at $(date '+%H:%M:%S')"                                    | tee -a "$MASTER_LOG"

# Run the bench.
echo ""                                                                         | tee -a "$MASTER_LOG"
echo "=== launching native-bench ==="                                           | tee -a "$MASTER_LOG"
./target/release/native-bench \
    --engine postgres \
    --scale 1.0 \
    --storage hdd \
    --schema-mode full \
    --duration 3600 \
    --cold-runs 100 \
    --golden golden/golden-scale1-seed42.json \
    --output ./results 2>&1 | tee ./results/postgres-scale1.0-hdd.run.log >> "$MASTER_LOG"
PG_RC=${PIPESTATUS[0]}

echo ""                                                                         | tee -a "$MASTER_LOG"
echo "=== bench exit=$PG_RC at $(date '+%H:%M:%S') ==="                         | tee -a "$MASTER_LOG"

# Tear down + cleanup HDD.
docker compose --profile postgres down --remove-orphans 2>&1                   | tee -a "$MASTER_LOG"
rm -rf "$PG_DATA_HDD"

# Summary.
echo ""                                                                         | tee -a "$MASTER_LOG"
echo "=== finished: $(date '+%Y-%m-%d %H:%M:%S %z') ==="                       | tee -a "$MASTER_LOG"
echo "=== exit code: postgres=$PG_RC ==="                                       | tee -a "$MASTER_LOG"
echo "=== reports: ===" | tee -a "$MASTER_LOG"
ls -1 ./results/postgres-hdd-scale1.0-2026*.md 2>/dev/null | tee -a "$MASTER_LOG" || true

exit $PG_RC
