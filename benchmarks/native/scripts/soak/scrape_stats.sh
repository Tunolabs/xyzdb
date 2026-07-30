#!/usr/bin/env bash
# Soak sidecar A — scrape `/stats` from xyzdb-server every $SCRAPE_INTERVAL_SEC
# (default 30) and append a CSV row to $OUT_CSV. Designed to run unattended
# under the master `run_soak.sh`.
#
# Endpoint: HTTP `/stats` (shipped in v0.4 cp 5.1.1, multiplexed on port 2505).
# CSV is appended; header written iff the file is empty.
#
# Env knobs:
#   HOST                 default 127.0.0.1
#   PORT                 default 2505
#   SCRAPE_INTERVAL_SEC  default 30
#   OUT_CSV              default /var/lib/xyzdb-soak/reports/scrape_stats.csv
set -euo pipefail

HOST="${HOST:-127.0.0.1}"
PORT="${PORT:-2505}"
INTERVAL="${SCRAPE_INTERVAL_SEC:-30}"
OUT="${OUT_CSV:-/var/lib/xyzdb-soak/reports/scrape_stats.csv}"

mkdir -p "$(dirname "$OUT")"
if [ ! -s "$OUT" ]; then
  echo "ts_ms,vmrss_b,cg_anon_b,cg_active_file_b,sync_last_ts_ms,sync_hb,ce_spatial,ce_identity,ce_dictionary,ce_ghosts,bc_weight_b,bc_capacity_b,bc_hits,bc_misses,ghost_total,l0_spatial,l0_identity" > "$OUT"
fi

while :; do
  ts=$(python3 -c 'import time; print(int(time.time()*1000))')
  body=$(curl -sS --max-time 5 "http://${HOST}:${PORT}/stats" 2>/dev/null || true)
  # Skip the row entirely if `/stats` did not return a valid body —
  # writing a row of zeros would intoxicate the gate monitor (sync_hb
  # would appear flat between a real-valued row and a zeroed row,
  # producing a false G2 trip during a transient HTTP hiccup).
  if [ -z "$body" ] || ! echo "$body" | jq -e '.sync_thread.heartbeat_count' >/dev/null 2>&1; then
    sleep "$INTERVAL"
    continue
  fi
  row=$(echo "$body" | jq -r '
    [
      '"$ts"',
      (.process.vmrss_bytes // 0),
      (.cgroup.anon_bytes // 0),
      (.cgroup.active_file_bytes // 0),
      (.sync_thread.last_successful_sync_ts_ms // 0),
      (.sync_thread.heartbeat_count // 0),
      (.keyspaces.spatial.compact.compact_err // 0),
      (.keyspaces.identity.compact.compact_err // 0),
      (.keyspaces.dictionary.compact.compact_err // 0),
      (.keyspaces.ghosts.compact.compact_err // 0),
      (.block_cache.weight_bytes // 0),
      (.block_cache.capacity_bytes // 0),
      (.block_cache.hits // 0),
      (.block_cache.misses // 0),
      (.ghosts.total // 0),
      (.keyspaces.spatial.levels.l0 // 0),
      (.keyspaces.identity.levels.l0 // 0)
    ] | @csv
  ' 2>/dev/null || true)
  if [ -n "$row" ]; then
    echo "$row" >> "$OUT"
  fi
  sleep "$INTERVAL"
done
