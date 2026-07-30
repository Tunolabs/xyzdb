#!/bin/bash
# Run Bench A Scale 1.0 HDD on MongoDB only.
# Uses the external HDD mounted at /Volumes/Disco D as the data dir
# (1 TB USB drive, physically rotational).
#
# Designed for unattended overnight execution. Estimated wall clock
# 5-7h based on Mongo SSD Scale 1.0 (4h 22min wall) extrapolated with
# typical 30-60% HDD penalty on WiredTiger random I/O.
#
# Output:
#   results/mongo-hdd-scale1.0-<UTC-ts>.{json,csv,md}
#   results/scale1.0-hdd-mongo.master.log
#
# Companion: scripts/run_scale1.0_hdd_postgres.sh — same pattern.
# xyzdb HDD script lands when needed (gravity bench is faster path).

set -u  # do not use -e: surfacing the bench exit code is the goal

cd "$(dirname "$0")/.."   # benchmarks/native/

# HDD data path. Override via MONGO_DATA_HDD env var if your HDD lives elsewhere.
MONGO_DATA_HDD="${MONGO_DATA_HDD:-/Volumes/Disco D/mongodata}"

# Sanity: ensure the HDD path's parent exists and is mounted.
HDD_PARENT="$(dirname "$MONGO_DATA_HDD")"
if [ ! -d "$HDD_PARENT" ]; then
    echo "ERROR: HDD parent path does not exist: $HDD_PARENT"
    echo "  Mount the HDD first or override with MONGO_DATA_HDD=<path>"
    exit 1
fi

MASTER_LOG=./results/scale1.0-hdd-mongo.master.log
mkdir -p ./results

echo "=== Bench A Scale 1.0 HDD — MongoDB ==="                                  | tee -a "$MASTER_LOG"
echo "=== started: $(date '+%Y-%m-%d %H:%M:%S %z') ==="                         | tee -a "$MASTER_LOG"
echo "=== host: $(uname -n) / docker: $(docker --version | awk '{print $3}' | tr -d ',') ===" | tee -a "$MASTER_LOG"
echo "=== HDD data path: $MONGO_DATA_HDD ==="                                   | tee -a "$MASTER_LOG"
echo "=== HDD free space: $(df -h "$HDD_PARENT" | tail -1 | awk '{print $4}') ===" | tee -a "$MASTER_LOG"

# Fresh data dir on HDD
echo ""                                                                          | tee -a "$MASTER_LOG"
echo "=== cleanup HDD data dir ==="                                              | tee -a "$MASTER_LOG"
rm -rf "$MONGO_DATA_HDD"
mkdir -p "$MONGO_DATA_HDD"

# Bring up mongo with data dir on the HDD volume.
# STORAGE_PROFILE is consumed by docker-compose.yml only by the postgres
# service (selects the .conf); mongo uses a single mongod-t6.conf so the
# var is informational here. Pass --storage hdd to native-bench so the
# report records the storage class correctly.
echo ""                                                                          | tee -a "$MASTER_LOG"
echo "=== bringing up mongo (MONGO_DATA=$MONGO_DATA_HDD) ==="                   | tee -a "$MASTER_LOG"
MONGO_DATA="$MONGO_DATA_HDD" STORAGE_PROFILE=hdd docker compose --profile mongo up -d 2>&1 | tee -a "$MASTER_LOG"

# Wait for mongo to be healthy (docker-compose healthcheck pings).
echo "waiting for mongo healthcheck..."                                          | tee -a "$MASTER_LOG"
until [ "$(docker inspect --format '{{.State.Health.Status}}' native-mongodb-1 2>/dev/null)" = "healthy" ]; do sleep 2; done
echo "mongo healthy at $(date '+%H:%M:%S')"                                      | tee -a "$MASTER_LOG"

# Run the bench. Pass --data-path explicitly so the resource sampler
# probes the actual HDD path, not the default ./data/mongodata. This
# closes the bug observed in the postgres HDD run where Disk peak
# reported 0.0 MiB.
echo ""                                                                          | tee -a "$MASTER_LOG"
echo "=== launching native-bench (with explicit --data-path for HDD sampler) ===" | tee -a "$MASTER_LOG"
./target/release/native-bench \
    --engine mongo \
    --scale 1.0 \
    --storage hdd \
    --schema-mode full \
    --duration 3600 \
    --cold-runs 100 \
    --golden golden/golden-scale1-seed42.json \
    --data-path "$MONGO_DATA_HDD" \
    --output ./results 2>&1 | tee ./results/mongo-scale1.0-hdd.run.log >> "$MASTER_LOG"
MONGO_RC=${PIPESTATUS[0]}

echo ""                                                                          | tee -a "$MASTER_LOG"
echo "=== bench exit=$MONGO_RC at $(date '+%H:%M:%S') ==="                       | tee -a "$MASTER_LOG"

# Tear down + cleanup HDD.
docker compose --profile mongo down --remove-orphans 2>&1                        | tee -a "$MASTER_LOG"
rm -rf "$MONGO_DATA_HDD"

# Summary.
echo ""                                                                          | tee -a "$MASTER_LOG"
echo "=== finished: $(date '+%Y-%m-%d %H:%M:%S %z') ==="                        | tee -a "$MASTER_LOG"
echo "=== exit code: mongo=$MONGO_RC ==="                                        | tee -a "$MASTER_LOG"
echo "=== reports: ===" | tee -a "$MASTER_LOG"
ls -1 ./results/mongo-hdd-scale1.0-2026*.md 2>/dev/null | tee -a "$MASTER_LOG" || true

exit $MONGO_RC
