#!/usr/bin/env bash
# v0.5 sub-A.5 — HDD scale 1.0 matrix (4 runs, ~12-16h wall-clock).
#
# Overnight, unattended. Reuses run_engine.sh per run; failures in one
# run do NOT abort the matrix.
#
# Runs:
#   04 xyzdb     HDD scale 1.0 --xydisk-mode observe   (sin ladder)
#   05 xyzdb     HDD scale 1.0 --xydisk-mode enforce   (con ladder, A.5 A/B)
#   06 postgres  HDD scale 1.0 (config postgresql-t6-hdd.conf)
#   07 mongo     HDD scale 1.0 (config mongod-t6.conf)
#
# Scale 1.0 = ~150M records (10× scale 0.1) — produces enough
# compaction churn for the ladder to actually act (cycle plan A.5
# expected workload).
#
# Required:
#   /mnt/hdd          HDD volume mounted, writable, ≥100 GiB free
#   d4c3068 or newer  run_engine.sh with PG/Mongo fixes
#
# Usage (tmux for disconnect survival):
#   tmux new-session -d -s hdd1 \
#     "$REPO_ROOT/benchmarks/native/scripts/matrix/run_matrix_hdd_scale1.sh \
#        2>&1 | tee /tmp/matrix-hdd-scale1-full.log"
#
# Per-run output: /mnt/hdd/xyzdb-bench/<run_tag>/{data,reports,logs}/
# Summary: /tmp/matrix-hdd-scale1-summary.log

set -uo pipefail

REPO_ROOT="${REPO_ROOT:-$(cd "$(dirname "$0")/../../../.." && pwd)}"
RUNNER="${REPO_ROOT}/benchmarks/native/scripts/matrix/run_engine.sh"
SUMMARY="${MATRIX_SUMMARY:-/tmp/matrix-hdd-scale1-summary.log}"

# Scale 1.0 specific tuning (override-able via env at invocation).
SCALE="${SCALE:-1.0}"
DURATION_SEC="${DURATION_SEC:-3600}"
PERSONAS="${PERSONAS:-humanrandom=9}"
SCHEDULE="${SCHEDULE:-daily_erp}"
SEED="${SEED:-42}"
CPUS="${CPUS:-2}"
MEMORY="${MEMORY:-8G}"
SCRAPE_INTERVAL_SEC="${SCRAPE_INTERVAL_SEC:-30}"
DRAIN_DELAY_SEC="${DRAIN_DELAY_SEC:-60}"   # more drain on scale 1.0

# Per-run config: tag | engine | soak_root | storage_profile | io_scheduler | xydisk_mode.
# storage_profile=hdd para que PG monte postgresql-t6-hdd.conf y
# xyzdb tune ghost_block_size=256KB + bloom 14 bits.
# io_scheduler y xydisk_mode solo afectan xyzdb (PG/Mongo los ignoran).
read -r -d '' MATRIX <<'EOF' || true
04-xyzdb-hdd-observe-scale1 |xyzdb    |/mnt/hdd/xyzdb-bench |hdd|hdd|observe
05-xyzdb-hdd-enforce-scale1 |xyzdb    |/mnt/hdd/xyzdb-bench |hdd|hdd|enforce
06-postgres-hdd-scale1      |postgres |/mnt/hdd/xyzdb-bench |hdd|-  |-
07-mongo-hdd-scale1         |mongo    |/mnt/hdd/xyzdb-bench |hdd|-  |-
EOF

log() { echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) hdd_s1: $*" | tee -a "$SUMMARY"; }

# Pre-flight checks.
if ! [ -d /mnt/hdd ]; then
  log "FAIL: /mnt/hdd not mounted or not a directory"
  exit 1
fi
[ -x "$RUNNER" ] || { log "FAIL: $RUNNER missing or not executable (git pull?)"; exit 1; }
docker info >/dev/null 2>&1 || { log "FAIL: docker not reachable"; exit 1; }

# Disk space sanity — scale 1.0 needs ~20 GiB per engine; ≥100 GiB free
# allows margin for the 4 runs plus xyzdb temp during bulk.
hdd_avail_gb=$(df -BG /mnt/hdd | awk 'NR==2 {gsub("G","",$4); print $4}')
if [ "${hdd_avail_gb:-0}" -lt 100 ]; then
  log "WARN: /mnt/hdd has only ${hdd_avail_gb}G free (recommended ≥100G for 4 runs)"
fi

# Stale-container teardown.
log "pre-flight: tearing down any stale containers"
( cd "$REPO_ROOT/benchmarks/native" \
  && docker compose --profile all down --remove-orphans \
) >/dev/null 2>&1 || true

# Ensure native-bench is built.
if [ ! -x "$REPO_ROOT/benchmarks/native/target/release/native-bench" ]; then
  log "building native-bench..."
  ( cd "$REPO_ROOT/benchmarks/native" && cargo build --release --bin native-bench ) \
    >>"${SUMMARY}.build" 2>&1
fi
if [ ! -x "$REPO_ROOT/xyzdb/target/release/xyzdb-cli" ]; then
  log "building xyzdb-cli..."
  ( cd "$REPO_ROOT/xyzdb" && cargo build --release -p xyzdb-cli ) \
    >>"${SUMMARY}.build" 2>&1
fi

# Pre-pull images so the first compose-up does not block on registry.
log "pre-pulling postgres:18 + mongo:7.0"
docker pull postgres:18 >/dev/null 2>&1 || log "WARN: postgres pull failed"
docker pull mongo:7.0   >/dev/null 2>&1 || log "WARN: mongo pull failed"

log "================================================================"
log "MATRIX BEGIN — HDD scale 1.0, 4 runs"
log "common: duration=${DURATION_SEC}s personas=$PERSONAS schedule=$SCHEDULE seed=$SEED"
log "envelope: ${CPUS}C / ${MEMORY} cgroup, hdd_avail=${hdd_avail_gb}G"
log "================================================================"

T_START=$(date +%s)
declare -a RESULTS=()

while IFS='|' read -r TAG ENG ROOT SP IS XM; do
  TAG=$(echo "$TAG"   | xargs)
  ENG=$(echo "$ENG"   | xargs)
  ROOT=$(echo "$ROOT" | xargs)
  SP=$(echo "$SP"     | xargs)
  IS=$(echo "$IS"     | xargs)
  XM=$(echo "$XM"     | xargs)
  [ -z "$TAG" ] && continue
  [[ "$TAG" == \#* ]] && continue

  log "----------------------------------------------------------------"
  log "RUN $TAG · engine=$ENG · storage=$SP · io_sched=$IS · xydisk_mode=$XM"
  log "----------------------------------------------------------------"

  # Clean stale dir if previous attempt left something
  rm -rf "$ROOT/$TAG"

  # Resolve dashes (n/a) to safe defaults for run_engine.sh env.
  SP_ARG="$SP"; [ "$SP" = "-" ] && SP_ARG="ssd"
  IS_ARG="$IS"; [ "$IS" = "-" ] && IS_ARG="ssd"
  XM_ARG="$XM"; [ "$XM" = "-" ] && XM_ARG="enforce"

  RUN_START=$(date +%s)
  ENGINE="$ENG" \
  RUN_TAG="$TAG" \
  SOAK_ROOT="$ROOT" \
  SCALE="$SCALE" \
  DURATION_SEC="$DURATION_SEC" \
  PERSONAS="$PERSONAS" \
  SCHEDULE="$SCHEDULE" \
  SEED="$SEED" \
  STORAGE_PROFILE="$SP_ARG" \
  IO_SCHEDULER="$IS_ARG" \
  XYDISK_MODE="$XM_ARG" \
  CPUS="$CPUS" \
  MEMORY="$MEMORY" \
  SCRAPE_INTERVAL_SEC="$SCRAPE_INTERVAL_SEC" \
  "$RUNNER"
  RC=$?
  ELAPSED=$(( $(date +%s) - RUN_START ))
  RESULTS+=("$TAG rc=$RC elapsed=${ELAPSED}s ($(( ELAPSED / 60 ))m)")
  log "RUN $TAG complete rc=$RC elapsed=${ELAPSED}s"

  log "drain ${DRAIN_DELAY_SEC}s"
  sleep "$DRAIN_DELAY_SEC"
done <<< "$MATRIX"

TOTAL=$(( $(date +%s) - T_START ))
log "================================================================"
log "MATRIX COMPLETE total=${TOTAL}s ($(( TOTAL / 3600 ))h $(( (TOTAL % 3600) / 60 ))m)"
log "================================================================"
for r in "${RESULTS[@]}"; do
  log "  $r"
done
log "summary: $SUMMARY"
log "reports under /mnt/hdd/xyzdb-bench/"
