#!/usr/bin/env bash
# v0.5 cross-engine matrix runner — generic. Bring up one engine via
# docker compose, run Phase 0/1/3 of native-bench with personas +
# schedule, capture report bundle + scheduler scrape (xyzdb only) +
# resource sampler. Tear down cleanly.
#
# Required env:
#   ENGINE              xyzdb | postgres | mongo
#   RUN_TAG             e.g. v0.5-run1-ssd-xyzdb-1h (output dir name)
#   SOAK_ROOT           base path, e.g. /mnt/ssd/xyzdb-bench
#
# Optional env (with defaults):
#   SCALE                       0.1
#   DURATION_SEC                3600
#   PERSONAS                    humanrandom=9
#   SCHEDULE                    daily_erp
#   SEED                        42
#   STORAGE_PROFILE             ssd
#   IO_SCHEDULER                ssd (xyzdb only)
#   XYDISK_MODE                 enforce (xyzdb only)
#   CPUS                        2
#   MEMORY                      8G
#   SCRAPE_INTERVAL_SEC         30
#
# Engine-specific:
#   xyzdb     — XYZ_DATA  = ${SOAK_ROOT}/${RUN_TAG}/data
#   postgres  — PG_DATA   = idem
#   mongo     — MONGO_DATA = idem
#
# Output: ${SOAK_ROOT}/${RUN_TAG}/{data,reports,logs}/

set -euo pipefail

ENGINE="${ENGINE:?ENGINE required: xyzdb|postgres|mongo}"
RUN_TAG="${RUN_TAG:?RUN_TAG required}"
SOAK_ROOT="${SOAK_ROOT:?SOAK_ROOT required}"

case "$ENGINE" in
  xyzdb|postgres|mongo) : ;;
  *) echo "unsupported ENGINE: $ENGINE" >&2; exit 2 ;;
esac

SCALE="${SCALE:-0.1}"
DURATION_SEC="${DURATION_SEC:-3600}"
PERSONAS="${PERSONAS:-humanrandom=9}"
SCHEDULE="${SCHEDULE:-daily_erp}"
SEED="${SEED:-42}"

# Engine env (read by compose-override.yml).
export STORAGE_PROFILE="${STORAGE_PROFILE:-ssd}"
export IO_SCHEDULER="${IO_SCHEDULER:-ssd}"
export XYDISK_MODE="${XYDISK_MODE:-enforce}"
export DURABILITY="${DURABILITY:-durable}"
export CACHE_SIZE="${CACHE_SIZE:-1024}"
export THROTTLE="${THROTTLE:-bulk}"
export BATCH_INTERVAL="${BATCH_INTERVAL:-100}"
export CPUS="${CPUS:-2}"
export MEMORY="${MEMORY:-8G}"

RUN_DIR="${SOAK_ROOT}/${RUN_TAG}"
DATA_DIR="${RUN_DIR}/data"
REPORT_DIR="${RUN_DIR}/reports"
LOG_DIR="${RUN_DIR}/logs"
mkdir -p "$DATA_DIR" "$REPORT_DIR" "$LOG_DIR"

case "$ENGINE" in
  xyzdb)    export XYZ_DATA="$DATA_DIR" ;;
  postgres) export PG_DATA="$DATA_DIR" ;;
  mongo)    export MONGO_DATA="$DATA_DIR" ;;
esac

export REPORT_DIR DATA_DIR
export SCRAPE_INTERVAL_SEC="${SCRAPE_INTERVAL_SEC:-30}"
export OUT_CSV="${REPORT_DIR}/scrape_stats.csv"

REPO_ROOT="${REPO_ROOT:-$(cd "$(dirname "$0")/../../../.." && pwd)}"
BENCH_DIR="${REPO_ROOT}/benchmarks/native"
COMPOSE_BASE="${BENCH_DIR}/docker-compose.yml"
COMPOSE_OVERRIDE="${BENCH_DIR}/scripts/matrix/compose-override.yml"
SCHEDULER_SCRAPE="${BENCH_DIR}/scripts/matrix/scrape_scheduler.sh"

# Map matrix ENGINE name → docker compose service / container / profile.
# Crucially the Mongo service is `mongodb` in compose.yml (not `mongo`).
case "$ENGINE" in
  xyzdb)
    COMPOSE_SERVICE=xyzdb
    CONTAINER_NAME=native-xyzdb-1
    COMPOSE_PROFILE=xyzdb
    HEALTH_URL="http://127.0.0.1:2505/stats"
    ;;
  postgres)
    COMPOSE_SERVICE=postgres
    CONTAINER_NAME=native-postgres-1
    COMPOSE_PROFILE=postgres
    HEALTH_PORT=5432
    ;;
  mongo)
    COMPOSE_SERVICE=mongodb
    CONTAINER_NAME=native-mongodb-1
    COMPOSE_PROFILE=mongo
    HEALTH_PORT=27017
    ;;
esac

log() { echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) run_engine: $*" | tee -a "${LOG_DIR}/run.log"; }
log "ENGINE=$ENGINE RUN_TAG=$RUN_TAG storage=${STORAGE_PROFILE} scale=${SCALE} duration=${DURATION_SEC}s"
log "personas=${PERSONAS} schedule=${SCHEDULE} seed=${SEED} cgroup=${CPUS}C/${MEMORY}"

CHILD_PIDS=()
ENGINE_UP=0
STATS_CAPTURED=0

cleanup() {
  log "cleanup begins"
  for pid in "${CHILD_PIDS[@]:-}"; do
    [ -z "$pid" ] && continue
    kill -TERM "$pid" 2>/dev/null || true
  done
  sleep 3
  for pid in "${CHILD_PIDS[@]:-}"; do
    [ -z "$pid" ] && continue
    kill -KILL "$pid" 2>/dev/null || true
  done
  # Capture /stats from xyzdb BEFORE tearing down the container. Only
  # overwrite stats_final.json if curl succeeds — never blow away a
  # previously-captured snapshot with an empty file (Run #1 lesson).
  if [ "$ENGINE" = "xyzdb" ] && [ "$ENGINE_UP" -eq 1 ] && [ "$STATS_CAPTURED" -eq 0 ]; then
    log "capturing /stats final snapshot before teardown"
    if body=$(curl -sS --max-time 5 "$HEALTH_URL" 2>/dev/null) && [ -n "$body" ]; then
      echo "$body" > "${REPORT_DIR}/stats_final.json"
      STATS_CAPTURED=1
    fi
  fi
  log "compose down -v $COMPOSE_SERVICE"
  ( cd "$BENCH_DIR" \
    && docker compose -f "$COMPOSE_BASE" -f "$COMPOSE_OVERRIDE" --profile "$COMPOSE_PROFILE" stop "$COMPOSE_SERVICE" \
  ) >>"${LOG_DIR}/compose.log" 2>&1 || true
  ( cd "$BENCH_DIR" \
    && docker compose -f "$COMPOSE_BASE" -f "$COMPOSE_OVERRIDE" --profile "$COMPOSE_PROFILE" rm -f "$COMPOSE_SERVICE" \
  ) >>"${LOG_DIR}/compose.log" 2>&1 || true
  log "cleanup done"
}
trap 'cleanup; exit 130' INT TERM

# 1. Bring up engine.
log "docker compose up -d $COMPOSE_SERVICE (profile=$COMPOSE_PROFILE)"
( cd "$BENCH_DIR" \
  && docker compose -f "$COMPOSE_BASE" -f "$COMPOSE_OVERRIDE" --profile "$COMPOSE_PROFILE" up -d "$COMPOSE_SERVICE" \
) >>"${LOG_DIR}/compose.log" 2>&1
COMPOSE_RC=$?
if [ "$COMPOSE_RC" -ne 0 ]; then
  log "FAIL: docker compose up returned rc=$COMPOSE_RC; see ${LOG_DIR}/compose.log"
  cleanup
  exit 1
fi

# 2. Wait for engine to be **truly** ready (not just port-bound). For PG
# `nc -z` succeeds in ~1 s but PG is still finishing initdb and rejects
# queries — the Run #2/#6 failure mode. For Mongo same story. We use
# engine-specific semantic probes:
#   xyzdb     curl /stats
#   postgres  docker exec ... pg_isready -d bench -U postgres
#   mongo     docker exec ... mongosh --quiet --eval ping
# Timeout up to 180 s (90 retries × 2 s).
log "waiting for $ENGINE semantic health (max 180s)"
ready=0
for i in $(seq 1 90); do
  case "$ENGINE" in
    xyzdb)
      if curl -sS --max-time 2 "$HEALTH_URL" >/dev/null 2>&1; then
        ready=1
        break
      fi
      ;;
    postgres)
      if docker exec "$CONTAINER_NAME" pg_isready -h 127.0.0.1 -p 5432 -d bench -U postgres -q 2>/dev/null; then
        ready=1
        break
      fi
      ;;
    mongo)
      if docker exec "$CONTAINER_NAME" mongosh --quiet --eval 'db.runCommand({ping:1}).ok' >/dev/null 2>&1; then
        ready=1
        break
      fi
      ;;
  esac
  sleep 2
done
if [ "$ready" -ne 1 ]; then
  log "FAIL: $ENGINE not ready after 180 s"
  cleanup
  exit 1
fi
ENGINE_UP=1
log "$ENGINE reachable (semantic health passed in ${i}*2s)"

# 3. Sidecars (xyzdb-only get scheduler scrape).
launch() {
  local name="$1"; shift
  log "sidecar $name"
  "$@" >>"${LOG_DIR}/${name}.stdout" 2>>"${LOG_DIR}/${name}.stderr" &
  CHILD_PIDS+=("$!")
}
if [ "$ENGINE" = "xyzdb" ]; then
  launch scrape_stats     "$BENCH_DIR/scripts/soak/scrape_stats.sh"
  launch scheduler_scrape "$SCHEDULER_SCRAPE"
fi

# 4. Run the bench. ($CONTAINER_NAME already set in the ENGINE→service map.)
log "native-bench $ENGINE scale=$SCALE duration=$DURATION_SEC personas=$PERSONAS schedule=$SCHEDULE container=$CONTAINER_NAME"
( cd "$BENCH_DIR" \
  && ERRATICA_PHASE3_DURATION_SEC="$DURATION_SEC" \
     ./target/release/native-bench \
       --engine "$ENGINE" --scale "$SCALE" --schema-mode full \
       --phase setup,load,concurrent --duration "$DURATION_SEC" --seed "$SEED" \
       --personas "$PERSONAS" --schedule "$SCHEDULE" \
       --output "$REPORT_DIR" \
       --container-name "$CONTAINER_NAME" \
       --data-path "$DATA_DIR" \
) >>"${LOG_DIR}/bench.stdout" 2>>"${LOG_DIR}/bench.stderr"
RC=$?
log "bench rc=$RC"

# 5. Capture final /stats while container is still up (xyzdb only).
if [ "$ENGINE" = "xyzdb" ]; then
  log "capturing /stats final snapshot"
  if body=$(curl -sS --max-time 5 "$HEALTH_URL" 2>/dev/null) && [ -n "$body" ]; then
    echo "$body" > "${REPORT_DIR}/stats_final.json"
    STATS_CAPTURED=1
    log "stats_final.json saved"
  else
    log "WARN: curl /stats failed; stats_final.json not written"
  fi
fi

cleanup
log "DONE reports=$REPORT_DIR rc=$RC"
exit $RC
