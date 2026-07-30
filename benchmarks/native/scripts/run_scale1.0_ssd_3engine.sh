#!/bin/bash
# Run Bench A Scale 1.0 SSD on the three engines sequentially with
# cleanup between each. Designed for unattended overnight execution.
#
# Each engine's container is brought up, the bench is run end-to-end
# (Phase 0..5 incl. 60-min concurrent), then container + data dir are
# torn down before the next engine starts. Resource sampling enabled.
#
# Output:
#   results/<engine>-ssd-scale1.0-<ts>.{json,csv,md}  per engine
#   results/scale1.0-ssd-3engine.master.log           orchestrator log

set -u  # do not use -e: a single engine failing should not abort the rest

cd "$(dirname "$0")/.."   # benchmarks/native/

MASTER_LOG=./results/scale1.0-ssd-3engine.master.log
mkdir -p ./results data/xyzdata data/pgdata data/mongodata

echo "=== Bench A Scale 1.0 SSD 3-engine sequence ==="     | tee -a "$MASTER_LOG"
echo "=== started: $(date '+%Y-%m-%d %H:%M:%S %z') ==="    | tee -a "$MASTER_LOG"
echo "=== host: $(uname -n) / docker: $(docker --version | awk '{print $3}' | tr -d ',') ===" | tee -a "$MASTER_LOG"

# ---------------------------------------------------------------- xyzDB
echo "" | tee -a "$MASTER_LOG"
echo "=== [1/3] xyzDB Scale 1.0 SSD — start: $(date '+%H:%M:%S') ===" | tee -a "$MASTER_LOG"
rm -rf ./data/xyzdata
mkdir -p ./data/xyzdata
STORAGE_PROFILE=ssd docker compose --profile xyzdb up -d 2>&1 | tee -a "$MASTER_LOG"
until nc -z 127.0.0.1 2505 2>/dev/null; do sleep 1; done
echo "xyzdb listen ok at $(date '+%H:%M:%S')" | tee -a "$MASTER_LOG"
./target/release/native-bench \
    --engine xyzdb \
    --scale 1.0 \
    --storage ssd \
    --schema-mode full \
    --duration 3600 \
    --cold-runs 100 \
    --golden golden/golden-scale1-seed42.json \
    --output ./results 2>&1 | tee ./results/xyzdb-scale1.0-ssd.run.log >> "$MASTER_LOG"
XYZDB_RC=${PIPESTATUS[0]}
echo "xyzdb exit=$XYZDB_RC at $(date '+%H:%M:%S')" | tee -a "$MASTER_LOG"
docker compose --profile xyzdb down --remove-orphans 2>&1 | tee -a "$MASTER_LOG"
rm -rf ./data/xyzdata

# ---------------------------------------------------------------- Postgres
echo "" | tee -a "$MASTER_LOG"
echo "=== [2/3] PostgreSQL Scale 1.0 SSD — start: $(date '+%H:%M:%S') ===" | tee -a "$MASTER_LOG"
rm -rf ./data/pgdata
mkdir -p ./data/pgdata
STORAGE_PROFILE=ssd docker compose --profile postgres up -d 2>&1 | tee -a "$MASTER_LOG"
until docker exec native-postgres-1 pg_isready -U postgres -d bench 2>/dev/null | grep -q "accepting connections"; do sleep 2; done
echo "postgres ready at $(date '+%H:%M:%S')" | tee -a "$MASTER_LOG"
./target/release/native-bench \
    --engine postgres \
    --scale 1.0 \
    --storage ssd \
    --schema-mode full \
    --duration 3600 \
    --cold-runs 100 \
    --golden golden/golden-scale1-seed42.json \
    --output ./results 2>&1 | tee ./results/postgres-scale1.0-ssd.run.log >> "$MASTER_LOG"
PG_RC=${PIPESTATUS[0]}
echo "postgres exit=$PG_RC at $(date '+%H:%M:%S')" | tee -a "$MASTER_LOG"
docker compose --profile postgres down --remove-orphans 2>&1 | tee -a "$MASTER_LOG"
rm -rf ./data/pgdata

# ---------------------------------------------------------------- MongoDB
echo "" | tee -a "$MASTER_LOG"
echo "=== [3/3] MongoDB Scale 1.0 SSD — start: $(date '+%H:%M:%S') ===" | tee -a "$MASTER_LOG"
rm -rf ./data/mongodata
mkdir -p ./data/mongodata
STORAGE_PROFILE=ssd docker compose --profile mongo up -d 2>&1 | tee -a "$MASTER_LOG"
until [ "$(docker inspect --format '{{.State.Health.Status}}' native-mongodb-1 2>/dev/null)" = "healthy" ]; do sleep 2; done
echo "mongo healthy at $(date '+%H:%M:%S')" | tee -a "$MASTER_LOG"
./target/release/native-bench \
    --engine mongo \
    --scale 1.0 \
    --storage ssd \
    --schema-mode full \
    --duration 3600 \
    --cold-runs 100 \
    --golden golden/golden-scale1-seed42.json \
    --output ./results 2>&1 | tee ./results/mongo-scale1.0-ssd.run.log >> "$MASTER_LOG"
MONGO_RC=${PIPESTATUS[0]}
echo "mongo exit=$MONGO_RC at $(date '+%H:%M:%S')" | tee -a "$MASTER_LOG"
docker compose --profile mongo down --remove-orphans 2>&1 | tee -a "$MASTER_LOG"
rm -rf ./data/mongodata

# ---------------------------------------------------------------- summary
echo "" | tee -a "$MASTER_LOG"
echo "=== sequence finished: $(date '+%Y-%m-%d %H:%M:%S %z') ===" | tee -a "$MASTER_LOG"
echo "=== exit codes: xyzdb=$XYZDB_RC pg=$PG_RC mongo=$MONGO_RC ===" | tee -a "$MASTER_LOG"
echo "=== reports under ./results/ ===" | tee -a "$MASTER_LOG"
ls -1 ./results/*scale1.0-ssd-2026*.md 2>&1 | tee -a "$MASTER_LOG" || true
