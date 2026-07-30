#!/usr/bin/env bash
# Master runner — orquesta el soak v0.4 (cycle plan §3.6.2.3).
#
# Lanza: docker compose up -d xyzdb (T6 cgroup 2C/8G) + 4 sidecars
# (scrape A, snapshot B, analyze C, gate D) + orchestrator native-bench
# Phase 3 sustained. Espera al primero de tres eventos:
#   - orchestrator termina por sí solo (duración alcanzada).
#   - sidecar D (gate_monitor) escribe $GATE_FAIL_FLAG → abort.
#   - usuario envía SIGINT/SIGTERM → cleanup gracioso.
#
# Por defecto: 72h (DEC-V4-4). Override con DURATION_SEC=600 para smoke.
#
# Uso: ./run_soak.sh [smoke|full]
#   smoke  → DURATION_SEC=600, intervalos sidecar reducidos
#   full   → defaults (72h, intervalos producción)

set -euo pipefail

# ── Defaults ───────────────────────────────────────────────────────────
MODE="${1:-full}"
REPO_ROOT="${REPO_ROOT:-$(cd "$(dirname "$0")/../../../.." && pwd)}"
BENCH_DIR="${BENCH_DIR:-${REPO_ROOT}/benchmarks/native}"
SOAK_ROOT="${SOAK_ROOT:-/var/lib/xyzdb-soak}"
DATA_DIR="${DATA_DIR:-${SOAK_ROOT}/data}"
REPORT_DIR="${REPORT_DIR:-${SOAK_ROOT}/reports}"
DISK_HEADROOM_GIB="${DISK_HEADROOM_GIB:-50}"

case "$MODE" in
  smoke)
    DURATION_SEC="${DURATION_SEC:-600}"          # 10 min
    SCRAPE_INTERVAL_SEC="${SCRAPE_INTERVAL_SEC:-10}"
    SNAPSHOT_INTERVAL_SEC="${SNAPSHOT_INTERVAL_SEC:-120}"  # 2 min
    ANALYZE_INTERVAL_SEC="${ANALYZE_INTERVAL_SEC:-300}"    # 5 min
    GATE_POLL_SEC="${GATE_POLL_SEC:-15}"
    ;;
  full)
    DURATION_SEC="${DURATION_SEC:-259200}"        # 72h, DEC-V4-4
    SCRAPE_INTERVAL_SEC="${SCRAPE_INTERVAL_SEC:-30}"
    SNAPSHOT_INTERVAL_SEC="${SNAPSHOT_INTERVAL_SEC:-21600}" # 6h
    ANALYZE_INTERVAL_SEC="${ANALYZE_INTERVAL_SEC:-43200}"   # 12h
    GATE_POLL_SEC="${GATE_POLL_SEC:-60}"
    ;;
  *) echo "usage: $0 [smoke|full]" >&2; exit 1 ;;
esac

export REPORT_DIR DATA_DIR
export OUT_CSV="${REPORT_DIR}/scrape_stats.csv"
export GATE_LOG="${REPORT_DIR}/gate_monitor.log"
export GATE_FAIL_FLAG="${REPORT_DIR}/gate.failed"
export SCRAPE_CSV="$OUT_CSV"
export SCRAPE_INTERVAL_SEC SNAPSHOT_INTERVAL_SEC ANALYZE_INTERVAL_SEC GATE_POLL_SEC
export XYZDB_CLI="${XYZDB_CLI:-${REPO_ROOT}/xyzdb/target/release/xyzdb-cli}"

mkdir -p "$REPORT_DIR" "$DATA_DIR"

log() { echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) run_soak: $*" | tee -a "${REPORT_DIR}/run_soak.log" >&2; }

# ── Pre-flight ─────────────────────────────────────────────────────────
log "MODE=$MODE DURATION_SEC=$DURATION_SEC"
log "REPO_ROOT=$REPO_ROOT BENCH_DIR=$BENCH_DIR"
log "DATA_DIR=$DATA_DIR REPORT_DIR=$REPORT_DIR"

# Disk headroom — portable: `-k` (1 KiB blocks) on both BSD (macOS) and GNU
# coreutils. `df -BG` is GNU-only and silently misreports on BSD.
avail_kib=$(df -k "$SOAK_ROOT" 2>/dev/null | awk 'NR==2 {print $4}' || echo "0")
avail_gib=$(( ${avail_kib:-0} / 1024 / 1024 ))
if [ "${avail_gib:-0}" -lt "$DISK_HEADROOM_GIB" ]; then
  log "FAIL pre-flight: $SOAK_ROOT only ${avail_gib}GiB free (need ≥${DISK_HEADROOM_GIB})"
  exit 1
fi
log "pre-flight OK: ${avail_gib}GiB free at $SOAK_ROOT"

# Stale fail-flag from a previous run would short-circuit the wait loop.
rm -f "$GATE_FAIL_FLAG"

# ── Background-process bookkeeping ─────────────────────────────────────
declare -a CHILD_PIDS=()
ORCH_PID=""

cleanup() {
  log "cleanup begins"
  for pid in "${CHILD_PIDS[@]:-}" "${ORCH_PID:-}"; do
    [ -z "$pid" ] && continue
    if kill -0 "$pid" 2>/dev/null; then
      log "  killing pid=$pid"
      kill -TERM "$pid" 2>/dev/null || true
    fi
  done
  # Give children 5s to exit, then SIGKILL stragglers.
  sleep 5
  for pid in "${CHILD_PIDS[@]:-}" "${ORCH_PID:-}"; do
    [ -z "$pid" ] && continue
    kill -KILL "$pid" 2>/dev/null || true
  done
  log "cleanup done"
}
trap 'log "received signal; cleaning up"; cleanup; exit 130' INT TERM

# ── Bring up the engine ────────────────────────────────────────────────
log "docker compose up -d xyzdb (T6 cgroup 2C/8G via env)"
( cd "$BENCH_DIR" && CPUS="${CPUS:-2}" MEMORY="${MEMORY:-8G}" XYZ_DATA="$DATA_DIR" docker compose up -d xyzdb )

log "waiting for /ready"
for _ in $(seq 1 30); do
  if curl -sS --max-time 2 "http://127.0.0.1:2505/stats" >/dev/null 2>&1; then
    log "engine reachable"
    break
  fi
  sleep 2
done

# ── Launch sidecars ────────────────────────────────────────────────────
launch() {
  local name="$1"; shift
  log "launching sidecar: $name"
  "$@" >>"${REPORT_DIR}/${name}.stdout" 2>>"${REPORT_DIR}/${name}.stderr" &
  CHILD_PIDS+=("$!")
}

launch scrape_stats   "$BENCH_DIR/scripts/soak/scrape_stats.sh"
launch snapshot_cron  "$BENCH_DIR/scripts/soak/snapshot_cron.sh"
launch analyze_cron   "$BENCH_DIR/scripts/soak/analyze_cron.sh"
launch gate_monitor   "$BENCH_DIR/scripts/soak/gate_monitor.sh"

# ── Launch orchestrator (Phase 3 sustained) ────────────────────────────
log "launching orchestrator (Phase 3 sustained, duration=${DURATION_SEC}s)"
(
  cd "$BENCH_DIR"
  ERRATICA_PHASE3_DURATION_SEC="$DURATION_SEC" \
    cargo run --release --bin native-bench -- \
      --engine xyzdb --scale 0.1 --schema-mode full \
      --phase setup,load,concurrent --duration "$DURATION_SEC" \
      --output "$REPORT_DIR" \
      --no-resources
) >>"${REPORT_DIR}/orchestrator.stdout" 2>>"${REPORT_DIR}/orchestrator.stderr" &
ORCH_PID="$!"
log "orchestrator pid=$ORCH_PID"

# ── Wait loop ──────────────────────────────────────────────────────────
log "soak running; tail ${REPORT_DIR}/orchestrator.stdout for progress"
while :; do
  if [ -f "$GATE_FAIL_FLAG" ]; then
    log "GATE FAIL detected: $(cat "$GATE_FAIL_FLAG" 2>/dev/null)"
    cleanup
    exit 2
  fi
  if ! kill -0 "$ORCH_PID" 2>/dev/null; then
    log "orchestrator exited; soak complete"
    break
  fi
  sleep 10
done

# ── Post-process ───────────────────────────────────────────────────────
cleanup
log "soak finished cleanly; running post-process"
"$BENCH_DIR/scripts/soak/postprocess.py" \
  --scrape "$OUT_CSV" \
  --orchestrator-results "$REPORT_DIR" \
  --output "$REPORT_DIR/v0.4-soak.md" \
  >>"${REPORT_DIR}/postprocess.log" 2>&1 || log "WARN postprocess failed (see log)"

log "DONE. Report: $REPORT_DIR/v0.4-soak.md"
