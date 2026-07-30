#!/usr/bin/env bash
# v0.5 cross-engine matrix — orchestrate all 7 runs sequentially in
# background, unattended.
#
# Runs (in order):
#   1. xyzdb     SSD     (--io-scheduler ssd, --xydisk-mode enforce)
#   2. postgres  SSD
#   3. mongo     SSD
#   4. xyzdb     HDD     (--io-scheduler hdd, --xydisk-mode observe)  ← SIN ladder
#   5. xyzdb     HDD     (--io-scheduler hdd, --xydisk-mode enforce)  ← CON ladder
#   6. postgres  HDD
#   7. mongo     HDD
#
# Each run goes through run_engine.sh (engine up → bulk load → Phase 3
# sustained 1h → /stats snapshot → engine down). Runs are strictly
# serial — never two engines up at once (per memory bench_engines_serial:
# "T6 envelope must be engine-exclusive"). Failures in one run do NOT
# abort the matrix; the next run starts after a brief drain delay.
#
# Required:
#   /mnt/ssd  (SSD volume mounted, writable by container)
#   /mnt/hdd  (HDD volume mounted, writable by container)
#
# Usage (in tmux for disconnect survival):
#   tmux new-session -d -s matrix \
#     "$REPO_ROOT/benchmarks/native/scripts/matrix/run_matrix_all.sh 2>&1 | tee /tmp/matrix-full.log"
#   tmux attach -t matrix   # to peek; Ctrl-B D to detach
#
# Output: each run lands in $SOAK_ROOT/<run_tag>/ with its own data,
# reports, logs subdirs. Top-level summary written to /tmp/matrix-summary.log
# at completion.

set -uo pipefail   # NOT -e: keep going even if one run errors out

REPO_ROOT="${REPO_ROOT:-$(cd "$(dirname "$0")/../../../.." && pwd)}"
MATRIX_SCRIPT="${REPO_ROOT}/benchmarks/native/scripts/matrix/run_engine.sh"
SUMMARY="${MATRIX_SUMMARY:-/tmp/matrix-summary.log}"

# Common params (override via env at invocation if needed).
SCALE="${SCALE:-0.1}"
DURATION_SEC="${DURATION_SEC:-3600}"
PERSONAS="${PERSONAS:-humanrandom=9}"
SCHEDULE="${SCHEDULE:-daily_erp}"
SEED="${SEED:-42}"
CPUS="${CPUS:-2}"
MEMORY="${MEMORY:-8G}"
SCRAPE_INTERVAL_SEC="${SCRAPE_INTERVAL_SEC:-30}"
DRAIN_DELAY_SEC="${DRAIN_DELAY_SEC:-30}"   # pause between runs

# Per-run config: name | engine | soak_root | storage_profile | io_scheduler | xydisk_mode.
# Lines starting with # are comments. Pipe-separated. Use - for n/a.
read -r -d '' MATRIX <<'EOF' || true
01-xyzdb-ssd-1h           |xyzdb    |/mnt/ssd/xyzdb-bench |ssd|ssd|enforce
02-postgres-ssd-1h        |postgres |/mnt/ssd/xyzdb-bench |- |- |-
03-mongo-ssd-1h           |mongo    |/mnt/ssd/xyzdb-bench |- |- |-
04-xyzdb-hdd-observe-1h   |xyzdb    |/mnt/hdd/xyzdb-bench |hdd|hdd|observe
05-xyzdb-hdd-enforce-1h   |xyzdb    |/mnt/hdd/xyzdb-bench |hdd|hdd|enforce
06-postgres-hdd-1h        |postgres |/mnt/hdd/xyzdb-bench |- |- |-
07-mongo-hdd-1h           |mongo    |/mnt/hdd/xyzdb-bench |- |- |-
EOF

log() { echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) matrix: $*" | tee -a "$SUMMARY"; }

# Sanity: required mounts.
for mp in /mnt/ssd /mnt/hdd; do
  if ! mountpoint -q "$mp" 2>/dev/null && ! [ -d "$mp" ]; then
    log "FAIL: $mp is not mounted or not a directory"
    exit 1
  fi
done

# Sanity: engine scripts present + Docker available.
[ -x "$MATRIX_SCRIPT" ] || { log "FAIL: $MATRIX_SCRIPT missing or not executable"; exit 1; }
docker info >/dev/null 2>&1 || { log "FAIL: docker not reachable"; exit 1; }

# Cleanup any stale containers from prior runs (paranoid).
log "pre-flight: tearing down any stale xyzdb / postgres / mongo containers"
( cd "$REPO_ROOT/benchmarks/native" \
  && docker compose --profile all down --remove-orphans \
) >/dev/null 2>&1 || true

# Build native-bench + xyzdb-cli host binaries if missing.
if [ ! -x "$REPO_ROOT/benchmarks/native/target/release/native-bench" ]; then
  log "building native-bench..."
  ( cd "$REPO_ROOT/benchmarks/native" && cargo build --release --bin native-bench ) \
    >>"$SUMMARY.build" 2>&1
fi
if [ ! -x "$REPO_ROOT/xyzdb/target/release/xyzdb-cli" ]; then
  log "building xyzdb-cli..."
  ( cd "$REPO_ROOT/xyzdb" && cargo build --release -p xyzdb-cli ) \
    >>"$SUMMARY.build" 2>&1
fi

log "matrix begins (7 runs, expected ~10-15h wall-clock)"
log "common params: scale=$SCALE duration=${DURATION_SEC}s personas=$PERSONAS schedule=$SCHEDULE seed=$SEED cgroup=${CPUS}C/${MEMORY}"

T_START=$(date +%s)
declare -a RUN_RESULTS=()

# Parse & run each line.
while IFS='|' read -r RUN_TAG ENGINE SOAK_ROOT SP IS XM; do
  RUN_TAG=$(echo "$RUN_TAG" | xargs)
  ENGINE=$(echo "$ENGINE" | xargs)
  SOAK_ROOT=$(echo "$SOAK_ROOT" | xargs)
  SP=$(echo "$SP" | xargs)
  IS=$(echo "$IS" | xargs)
  XM=$(echo "$XM" | xargs)
  [ -z "$RUN_TAG" ] && continue
  [[ "$RUN_TAG" == \#* ]] && continue

  log "================================================================"
  log "RUN $RUN_TAG · engine=$ENGINE · soak_root=$SOAK_ROOT"
  log "  storage=$SP io_scheduler=$IS xydisk_mode=$XM"
  log "================================================================"

  RUN_START=$(date +%s)
  # Engine-specific env overrides.
  STORAGE_PROFILE_ARG="$SP"
  IO_SCHEDULER_ARG="$IS"
  XYDISK_MODE_ARG="$XM"
  # For non-xyzdb engines we still pass the env but run_engine.sh
  # ignores them (only xyzdb uses the flags via compose-override).
  [ "$SP" = "-" ] && STORAGE_PROFILE_ARG="ssd"
  [ "$IS" = "-" ] && IO_SCHEDULER_ARG="ssd"
  [ "$XM" = "-" ] && XYDISK_MODE_ARG="enforce"

  ENGINE="$ENGINE" \
  RUN_TAG="$RUN_TAG" \
  SOAK_ROOT="$SOAK_ROOT" \
  SCALE="$SCALE" \
  DURATION_SEC="$DURATION_SEC" \
  PERSONAS="$PERSONAS" \
  SCHEDULE="$SCHEDULE" \
  SEED="$SEED" \
  STORAGE_PROFILE="$STORAGE_PROFILE_ARG" \
  IO_SCHEDULER="$IO_SCHEDULER_ARG" \
  XYDISK_MODE="$XYDISK_MODE_ARG" \
  CPUS="$CPUS" \
  MEMORY="$MEMORY" \
  SCRAPE_INTERVAL_SEC="$SCRAPE_INTERVAL_SEC" \
  "$MATRIX_SCRIPT"
  RC=$?
  RUN_END=$(date +%s)
  RUN_ELAPSED=$(( RUN_END - RUN_START ))
  RUN_RESULTS+=("$RUN_TAG rc=$RC elapsed=${RUN_ELAPSED}s")
  log "RUN $RUN_TAG complete rc=$RC elapsed=${RUN_ELAPSED}s"

  # Drain pause — let docker release resources + filesystem flush.
  log "drain ${DRAIN_DELAY_SEC}s before next run"
  sleep "$DRAIN_DELAY_SEC"
done <<< "$MATRIX"

T_END=$(date +%s)
TOTAL=$(( T_END - T_START ))
log "================================================================"
log "MATRIX COMPLETE total=${TOTAL}s ($(( TOTAL / 3600 ))h $(( (TOTAL % 3600) / 60 ))m)"
log "================================================================"
for r in "${RUN_RESULTS[@]}"; do
  log "  $r"
done
log "summary: $SUMMARY"
log "reports: under /mnt/ssd/xyzdb-bench/ and /mnt/hdd/xyzdb-bench/"
