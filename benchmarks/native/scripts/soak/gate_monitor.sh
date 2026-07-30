#!/usr/bin/env bash
# Soak sidecar D — fail-fast acceptance-gate monitor. Polls the latest
# row of $SCRAPE_CSV every $GATE_POLL_SEC (default 60) and the docker
# container restart counter, evaluating four gates from cycle plan
# §3.6.2.3:
#
#   G1  any keyspace `compact_err` > 0
#   G2  sync_thread heartbeat_count flat between two consecutive scrapes
#       (real "thread dead" detection — `last_successful_sync_ts_ms`
#       only advances on actual fsyncs, so it stays flat during MMPP
#       Idle stretches even on a healthy engine; that's not a failure)
#   G3  vmrss > $CGROUP_LIMIT_BYTES * 0.95
#   G4  crash-loop: container RestartCount delta > 3 in 5 min window
#
# When a gate trips, the monitor (a) writes a structured reason line to
# $GATE_LOG, (b) writes a marker file at $GATE_FAIL_FLAG so the master
# runner notices on its own polling loop, (c) exits non-zero. The
# master runner is responsible for killing siblings.
#
# Env knobs:
#   SCRAPE_CSV          default /var/lib/xyzdb-soak/reports/scrape_stats.csv
#   GATE_LOG            default /var/lib/xyzdb-soak/reports/gate_monitor.log
#   GATE_FAIL_FLAG      default /var/lib/xyzdb-soak/reports/gate.failed
#   CGROUP_LIMIT_BYTES  default 8589934592 (8 GiB; matches compose MEMORY=8G)
#   GATE_POLL_SEC       default 60
#   CONTAINER           default native-xyzdb-1 (compose-default name)
#   HEARTBEAT_STALE_MS  default 5000
#   CRASH_WINDOW_SEC    default 300 (5 min)
#   CRASH_MAX           default 3
set -euo pipefail

SCRAPE_CSV="${SCRAPE_CSV:-/var/lib/xyzdb-soak/reports/scrape_stats.csv}"
GATE_LOG="${GATE_LOG:-/var/lib/xyzdb-soak/reports/gate_monitor.log}"
GATE_FAIL_FLAG="${GATE_FAIL_FLAG:-/var/lib/xyzdb-soak/reports/gate.failed}"
CGROUP_LIMIT_BYTES="${CGROUP_LIMIT_BYTES:-8589934592}"
POLL="${GATE_POLL_SEC:-60}"
CONTAINER="${CONTAINER:-native-xyzdb-1}"
HEARTBEAT_STALE_MS="${HEARTBEAT_STALE_MS:-5000}"
CRASH_WINDOW_SEC="${CRASH_WINDOW_SEC:-300}"
CRASH_MAX="${CRASH_MAX:-3}"
RSS_THRESHOLD_BYTES=$(( CGROUP_LIMIT_BYTES * 95 / 100 ))

mkdir -p "$(dirname "$GATE_LOG")"
log() { echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) gate_monitor: $*" | tee -a "$GATE_LOG" >&2; }
fail() {
  log "GATE FAILED: $*"
  echo "$*" > "$GATE_FAIL_FLAG"
  exit 2
}

# Crash-loop bookkeeping: ring buffer of (timestamp, restart_count) deltas.
declare -a restart_history=()
last_restart_count=-1

# G2 bookkeeping — track previous (ts_ms, heartbeat_count) to detect a
# flat (non-advancing) sync thread between *distinct* scrapes. We only
# compare consecutive rows of the scrape CSV, identified by ts_ms; if
# the gate_monitor wakes up between scrapes it sees the same row twice,
# and "same heartbeat" is expected (not a failure). A 1 ms heartbeat
# loop produces ~500-1000 heartbeats/sec, so once ts_ms advances we
# expect the heartbeat to advance with it on a healthy thread.
prev_ts=""
prev_heartbeat=""

restart_count() {
  docker inspect --format '{{.RestartCount}}' "$CONTAINER" 2>/dev/null || echo "0"
}

log "starting (poll=${POLL}s rss_threshold=${RSS_THRESHOLD_BYTES} container=${CONTAINER})"

while :; do
  sleep "$POLL"

  # Read the latest scrape row. Tail in case the file grew; skip if empty.
  if [ ! -s "$SCRAPE_CSV" ]; then
    log "scrape CSV empty; waiting another cycle"
    continue
  fi
  last=$(tail -n 1 "$SCRAPE_CSV")
  if [[ "$last" == ts_ms* ]]; then
    # Header only — scraper has not produced data yet.
    continue
  fi
  IFS=',' read -r ts_ms vmrss_b _cg_anon _cg_active _sync_last_ts sync_hb \
                ce_spatial ce_identity ce_dictionary ce_ghosts \
                _bc_w _bc_c _bc_h _bc_m _gh_total _l0_s _l0_i <<<"$last"

  # G1 — compact_err in any keyspace
  for v in "$ce_spatial" "$ce_identity" "$ce_dictionary" "$ce_ghosts"; do
    if [ "${v:-0}" -gt 0 ]; then
      fail "G1 compact_err > 0 (spatial=$ce_spatial identity=$ce_identity dictionary=$ce_dictionary ghosts=$ce_ghosts)"
    fi
  done

  # G2 — sync_thread liveness via heartbeat_count freshness. The
  # heartbeat advances on every iteration of the 1 ms WAL sync loop
  # in turba-engine/src/engine.rs:215, independent of whether any
  # fsync was performed. A flat heartbeat across two *distinct*
  # scrape rows (different ts_ms) means the thread is actually
  # dead — Finding 9-class durability outage. We do NOT compare
  # last_successful_sync_ts_ms because it only advances when
  # pending_epoch > synced_epoch; under low-write MMPP Idle
  # stretches it legitimately stays flat for minutes on a healthy
  # engine (cp 6.2.3a smoke, finding H11).
  #
  # Only evaluate when ts_ms has advanced since the last poll —
  # otherwise we are reading the same row twice (poll cadence
  # faster than scrape cadence) and "same heartbeat" is expected.
  if [ -n "$prev_ts" ] && [ "${ts_ms:-0}" -gt "$prev_ts" ] \
     && [ -n "$prev_heartbeat" ] \
     && [ "${sync_hb:-0}" -le "$prev_heartbeat" ]; then
    fail "G2 sync_thread heartbeat flat: count=${sync_hb} unchanged across scrapes (prev_ts=${prev_ts} prev_hb=${prev_heartbeat} ts=${ts_ms})"
  fi
  if [ -z "$prev_ts" ] || [ "${ts_ms:-0}" -gt "$prev_ts" ]; then
    prev_ts="$ts_ms"
    prev_heartbeat="$sync_hb"
  fi

  # G3 — VmRSS over 95% cgroup limit
  if [ "${vmrss_b:-0}" -gt "$RSS_THRESHOLD_BYTES" ]; then
    fail "G3 vmrss=${vmrss_b}B > 0.95 * ${CGROUP_LIMIT_BYTES}B (=${RSS_THRESHOLD_BYTES}B)"
  fi

  # G4 — crash-loop window
  rc=$(restart_count)
  if [ "$last_restart_count" -ge 0 ] && [ "$rc" -gt "$last_restart_count" ]; then
    now_s=$(python3 -c 'import time; print(int(time.time()))')
    restart_history+=("$now_s")
    log "container restart observed (count=${rc})"
  fi
  last_restart_count="$rc"
  # Drop entries older than CRASH_WINDOW_SEC.
  if [ "${#restart_history[@]}" -gt 0 ]; then
    cutoff=$(( $(python3 -c 'import time; print(int(time.time()))') - CRASH_WINDOW_SEC ))
    new=()
    for t in "${restart_history[@]}"; do
      [ "$t" -ge "$cutoff" ] && new+=("$t")
    done
    restart_history=("${new[@]:-}")
    if [ "${#restart_history[@]}" -gt "$CRASH_MAX" ]; then
      fail "G4 crash-loop: ${#restart_history[@]} restarts in ${CRASH_WINDOW_SEC}s (>${CRASH_MAX})"
    fi
  fi
done
