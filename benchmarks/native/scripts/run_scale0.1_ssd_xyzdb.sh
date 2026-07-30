#!/bin/bash
# Run Bench A Scale 0.1 SSD on xyzdb only — v0.2.6 cross-check.
#
# Lighter variant of run_scale1.0_ssd_3engine.sh: single engine
# (xyzdb), single scale (0.1 = 14.99M records), single storage
# (SSD), full Phase 0..5 incl. 60-min concurrent. Wall-clock
# ~75 min — enough to surface macro-level regression vs the
# v0.2.5.2 baseline at the same scale (per docs/reports/
# benchmark-v0.2.5.2-* — the SSD synthesis report has Scale 0.1
# rows under the cold-query table that this run can be diffed
# against).
#
# This is the v0.2.6 tag-readiness gate when a full Scale 1.0
# overnight is not warranted (engine layer byte-identical to
# v0.2.5.2; the cheap variant is sufficient signal).
#
# Output:
#   results/xyzdb-ssd-scale0.1-<ts>.{json,csv,md}
#   results/scale0.1-ssd-xyzdb.master.log

set -u  # do not use -e: surfacing the bench exit code is the goal

cd "$(dirname "$0")/.."   # benchmarks/native/

MASTER_LOG=./results/scale0.1-ssd-xyzdb.master.log
mkdir -p ./results data/xyzdata

echo "=== Bench A Scale 0.1 SSD — xyzdb (v0.2.6 cross-check) ===" | tee -a "$MASTER_LOG"
echo "=== started: $(date '+%Y-%m-%d %H:%M:%S %z') ==="           | tee -a "$MASTER_LOG"
echo "=== host: $(uname -n) / docker: $(docker --version | awk '{print $3}' | tr -d ',') ===" | tee -a "$MASTER_LOG"

# Fresh data dir
echo ""                                                           | tee -a "$MASTER_LOG"
echo "=== cleanup ==="                                            | tee -a "$MASTER_LOG"
rm -rf ./data/xyzdata
mkdir -p ./data/xyzdata

# Bring up xyzdb container
echo ""                                                           | tee -a "$MASTER_LOG"
echo "=== bringing up xyzdb (SSD profile) ==="                    | tee -a "$MASTER_LOG"
STORAGE_PROFILE=ssd docker compose --profile xyzdb up -d 2>&1     | tee -a "$MASTER_LOG"

# Wait for the TCP listener (no docker healthcheck on xyzdb-server).
echo "waiting for xyzdb to listen on 2505..."                     | tee -a "$MASTER_LOG"
until nc -z 127.0.0.1 2505 2>/dev/null; do sleep 1; done
echo "xyzdb listen ok at $(date '+%H:%M:%S')"                     | tee -a "$MASTER_LOG"

# Run the bench. Phase 3 sustained = 3600 s (60 min); cold runs = 100.
# Same parameters as the 3-engine script's xyzdb leg, only --scale
# differs (0.1 vs 1.0).
echo ""                                                           | tee -a "$MASTER_LOG"
echo "=== launching native-bench ==="                             | tee -a "$MASTER_LOG"
./target/release/native-bench \
    --engine xyzdb \
    --scale 0.1 \
    --storage ssd \
    --schema-mode full \
    --duration 3600 \
    --cold-runs 100 \
    --golden golden/golden-scale0.1-seed42.json \
    --output ./results 2>&1 | tee ./results/xyzdb-scale0.1-ssd.run.log >> "$MASTER_LOG"
XYZDB_RC=${PIPESTATUS[0]}

echo ""                                                           | tee -a "$MASTER_LOG"
echo "=== bench exit=$XYZDB_RC at $(date '+%H:%M:%S') ==="        | tee -a "$MASTER_LOG"

# Tear down + cleanup
docker compose --profile xyzdb down --remove-orphans 2>&1         | tee -a "$MASTER_LOG"
rm -rf ./data/xyzdata

# Summary
echo ""                                                           | tee -a "$MASTER_LOG"
echo "=== finished: $(date '+%Y-%m-%d %H:%M:%S %z') ==="          | tee -a "$MASTER_LOG"
echo "=== exit code: xyzdb=$XYZDB_RC ==="                         | tee -a "$MASTER_LOG"
echo "=== reports: ===" | tee -a "$MASTER_LOG"
ls -1 ./results/xyzdb-ssd-scale0.1-2026*.md 2>/dev/null | tee -a "$MASTER_LOG" || true

exit $XYZDB_RC
