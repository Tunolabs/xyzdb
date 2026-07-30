#!/usr/bin/env bash
# Soak sidecar B — periodic hot snapshot every $SNAPSHOT_INTERVAL_SEC
# (default 21600 = 6h) via `xyzdb-cli admin snapshot create`. After each
# successful create, retention prunes the snapshots dir back to the last
# $SNAPSHOT_KEEP entries (default 4 = 24h coverage at 6h cadence).
#
# Snapshots land at $DATA_DIR/snapshots/soak-<UTC-iso>/ per the v0.4
# Backup contract (OPERATIONS.md §4). The dir name is sortable so
# `ls -1 | sort` orders chronologically.
#
# Env knobs:
#   XYZDB_CLI               default ./target/release/xyzdb-cli
#                           (resolved relative to the repo root the
#                           master runner cd's into).
#   HOST                    default 127.0.0.1
#   PORT                    default 2505
#   SNAPSHOT_INTERVAL_SEC   default 21600 (6h)
#   SNAPSHOT_KEEP           default 4
#   DATA_DIR                default /var/lib/xyzdb-soak/data
#   LOG                     default /var/lib/xyzdb-soak/reports/snapshot_cron.log
set -euo pipefail

XYZDB_CLI="${XYZDB_CLI:-./target/release/xyzdb-cli}"
HOST="${HOST:-127.0.0.1}"
PORT="${PORT:-2505}"
INTERVAL="${SNAPSHOT_INTERVAL_SEC:-21600}"
KEEP="${SNAPSHOT_KEEP:-4}"
DATA_DIR="${DATA_DIR:-/var/lib/xyzdb-soak/data}"
# Derive LOG from REPORT_DIR (exported by run_soak.sh) so the master
# runner's path overrides take effect without per-sidecar env juggling.
LOG="${LOG:-${REPORT_DIR:-/var/lib/xyzdb-soak/reports}/snapshot_cron.log}"

mkdir -p "$(dirname "$LOG")"
log() { echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) snapshot_cron: $*" | tee -a "$LOG" >&2; }

log "starting (interval=${INTERVAL}s keep=${KEEP} data_dir=${DATA_DIR})"
while :; do
  sleep "$INTERVAL"
  name="soak-$(date -u +%Y%m%dT%H%M%SZ)"
  log "creating $name"
  if "$XYZDB_CLI" --host "$HOST" --port "$PORT" admin snapshot create "$name" >> "$LOG" 2>&1; then
    log "created $name OK"
  else
    log "WARN snapshot create failed (server may be restarting); will retry next cycle"
    continue
  fi

  if [ -d "${DATA_DIR}/snapshots" ]; then
    # Sort by name (UTC iso → chronological), keep last $KEEP, delete rest.
    # `head -n -N` (drop-last-N) is GNU-only; compute count first for
    # portability with BSD coreutils (macOS smoke).
    total=$(ls -1 "${DATA_DIR}/snapshots" 2>/dev/null | grep -c '^soak-' || true)
    to_drop=$(( total - KEEP ))
    if [ "$to_drop" -gt 0 ]; then
      ls -1 "${DATA_DIR}/snapshots" | grep '^soak-' | sort | head -n "$to_drop" \
        | while IFS= read -r d; do
            [ -z "$d" ] && continue
            log "pruning ${DATA_DIR}/snapshots/${d}"
            rm -rf "${DATA_DIR}/snapshots/${d}"
          done
    fi
  fi
done
