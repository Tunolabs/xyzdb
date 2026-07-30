#!/bin/bash
# Run Bench A Scale 1.0 HDD on xyzdb only.
# Uses the external HDD mounted at /Volumes/Disco D as the data dir
# (1 TB USB drive, physically rotational). Storage profile "hdd" =
# 256 KB ghost blocks + bloom 14 bits per the engine's HDD-tuned config.
#
# Wall-clock estimate: 1.5-3 h based on:
#  - xyzdb SSD Scale 1.0 wall ≈ 1h 30min
#  - HDD penalty for incremental-ghost-update path is bounded
#    (no REFRESH MATVIEW lock contention like postgres);
#    expected -50 % to -70 % of postgres-HDD wall (which was 2h 38m).
#
# Output:
#   results/xyzdb-hdd-scale1.0-<UTC-ts>.{json,csv,md}
#   results/scale1.0-hdd-xyzdb.master.log
#
# Companions:
#  - scripts/run_scale1.0_hdd_postgres.sh — already run in the
#    v0.2.5.2 cycle (synthesis at docs/reports/benchmark-v0.2.5.2-
#    scale1.0-hdd-postgres.md, untracked pending mongo HDD finish).
#  - scripts/run_scale1.0_hdd_mongo.sh — pending re-launch
#    (driver client-timeout fix at 3758058 already in build).

set -u  # do not use -e: surfacing the bench exit code is the goal

cd "$(dirname "$0")/.."   # benchmarks/native/

# HDD data path. Override via XYZ_DATA_HDD env var if the HDD is mounted elsewhere.
XYZ_DATA_HDD="${XYZ_DATA_HDD:-/Volumes/Disco D/xyzdata}"

# Sanity: ensure the HDD path's parent exists and is mounted.
HDD_PARENT="$(dirname "$XYZ_DATA_HDD")"
if [ ! -d "$HDD_PARENT" ]; then
    echo "ERROR: HDD parent path does not exist: $HDD_PARENT"
    echo "  Mount the HDD first or override with XYZ_DATA_HDD=<path>"
    exit 1
fi

MASTER_LOG=./results/scale1.0-hdd-xyzdb.master.log
mkdir -p ./results

echo "=== Bench A Scale 1.0 HDD — xyzdb ==="                                     | tee -a "$MASTER_LOG"
echo "=== started: $(date '+%Y-%m-%d %H:%M:%S %z') ==="                          | tee -a "$MASTER_LOG"
echo "=== host: $(uname -n) / docker: $(docker --version | awk '{print $3}' | tr -d ',') ===" | tee -a "$MASTER_LOG"
echo "=== HDD data path: $XYZ_DATA_HDD ==="                                      | tee -a "$MASTER_LOG"
echo "=== HDD free space: $(df -h "$HDD_PARENT" | tail -1 | awk '{print $4}') ===" | tee -a "$MASTER_LOG"

# Ensure a clean docker state (stop any previously running xyzdb container
# that might still hold the prior data dir's LSM lock).
echo ""                                                                          | tee -a "$MASTER_LOG"
echo "=== teardown any prior container ==="                                      | tee -a "$MASTER_LOG"
docker compose --profile xyzdb down --remove-orphans 2>&1                        | tee -a "$MASTER_LOG"

# Fresh data dir on HDD
echo ""                                                                          | tee -a "$MASTER_LOG"
echo "=== cleanup HDD data dir ==="                                              | tee -a "$MASTER_LOG"
rm -rf "$XYZ_DATA_HDD"
mkdir -p "$XYZ_DATA_HDD"

# Bring up xyzdb with data dir on the HDD volume + HDD storage profile.
# STORAGE_PROFILE=hdd selects the HDD-tuned config (256 KB ghost blocks,
# bloom 14 bits). XYZ_DATA points the volume mount at the external HDD.
echo ""                                                                          | tee -a "$MASTER_LOG"
echo "=== bringing up xyzdb (XYZ_DATA=$XYZ_DATA_HDD, STORAGE_PROFILE=hdd) ===" | tee -a "$MASTER_LOG"
XYZ_DATA="$XYZ_DATA_HDD" STORAGE_PROFILE=hdd docker compose --profile xyzdb up -d 2>&1 | tee -a "$MASTER_LOG"

# Wait for xyzdb-server to be FULLY READY (not just port-bound).
# nc -z only validates the TCP listener — but xyzdb-server binds the
# port early in startup, before block-cache allocation, WAL replay,
# and keyspace mount finish. On HDD physical (slow USB-mounted volumes)
# the gap between port-bound and request-handler-ready can be several
# seconds; on SSD it is sub-100ms. A premature first frame in that
# gap gets "Connection reset by peer" from the server's accept loop.
#
# Robust probe: loop SHOW LOBES via xyzdb-cli until it succeeds. This
# only returns after the server has fully processed a real frame.
XYZDB_CLI="$(dirname "$0")/../../../xyzdb/target/release/xyzdb-cli"
echo "waiting for xyzdb to be ready (real query probe)..."                       | tee -a "$MASTER_LOG"
for i in $(seq 1 120); do
    if echo 'SHOW LOBES' | "$XYZDB_CLI" --port 2505 > /dev/null 2>&1; then
        echo "xyzdb ready at $(date '+%H:%M:%S') (attempt $i)"                    | tee -a "$MASTER_LOG"
        break
    fi
    sleep 1
done

# Run the bench. Pass --data-path explicitly so the resource sampler
# probes the actual HDD path, not the default ./data/xyzdata. This
# closes the bug observed in the postgres HDD run where Disk peak
# reported 0.0 MiB.
echo ""                                                                          | tee -a "$MASTER_LOG"
echo "=== launching native-bench (with explicit --data-path for HDD sampler) === " | tee -a "$MASTER_LOG"
./target/release/native-bench \
    --engine xyzdb \
    --scale 1.0 \
    --storage hdd \
    --schema-mode full \
    --duration 3600 \
    --cold-runs 100 \
    --golden golden/golden-scale1-seed42.json \
    --data-path "$XYZ_DATA_HDD" \
    --output ./results 2>&1 | tee ./results/xyzdb-scale1.0-hdd.run.log >> "$MASTER_LOG"
XYZDB_RC=${PIPESTATUS[0]}

echo ""                                                                          | tee -a "$MASTER_LOG"
echo "=== bench exit=$XYZDB_RC at $(date '+%H:%M:%S') ==="                       | tee -a "$MASTER_LOG"

# Tear down + cleanup HDD.
docker compose --profile xyzdb down --remove-orphans 2>&1                        | tee -a "$MASTER_LOG"
rm -rf "$XYZ_DATA_HDD"

# Summary.
echo ""                                                                          | tee -a "$MASTER_LOG"
echo "=== finished: $(date '+%Y-%m-%d %H:%M:%S %z') ==="                         | tee -a "$MASTER_LOG"
echo "=== exit code: xyzdb=$XYZDB_RC ==="                                        | tee -a "$MASTER_LOG"
echo "=== reports: ===" | tee -a "$MASTER_LOG"
ls -1 ./results/xyzdb-hdd-scale1.0-2026*.md 2>/dev/null | tee -a "$MASTER_LOG" || true

exit $XYZDB_RC
