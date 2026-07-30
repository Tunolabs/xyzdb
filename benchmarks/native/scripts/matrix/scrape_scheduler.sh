#!/usr/bin/env bash
# v0.5 sidecar — scrape scheduler.* section of xyzdb `/stats` every
# $SCRAPE_INTERVAL_SEC and append to CSV.
#
# The official `scrape_stats.sh` predates v0.5 and ignores scheduler
# fields; this sidecar captures the per-lane EWMA + breach + outstanding
# peak + compaction_blocked + cross_lane_peak + vmrss + cgroup anon
# needed for A.5 ladder validation.
#
# xyzdb-only — PG and Mongo do not expose a compatible `/stats`. The
# matrix runner skips launching this sidecar for non-xyzdb engines.
#
# Env knobs:
#   HOST, PORT             default 127.0.0.1:2505
#   SCRAPE_INTERVAL_SEC    default 30
#   REPORT_DIR             output dir (required)

set -euo pipefail
HOST="${HOST:-127.0.0.1}"
PORT="${PORT:-2505}"
INTERVAL="${SCRAPE_INTERVAL_SEC:-30}"
OUT="${REPORT_DIR:?REPORT_DIR required}/scrape_scheduler.csv"

mkdir -p "$(dirname "$OUT")"
[ -s "$OUT" ] || echo "ts_ms,mode,uior_p50_us,uior_ewma_p50_us,uior_outstanding_peak,uior_slo_breach,wd_p50_us,wd_ewma_p50_us,wd_slo_breach,flush_p50_us,flush_ewma_p50_us,comp_p50_us,comp_ewma_p50_us,comp_outstanding_peak,comp_blocked_us_total,cross_lane_peak,vmrss_b,cg_anon_b" > "$OUT"

while :; do
  ts=$(python3 -c 'import time; print(int(time.time()*1000))')
  body=$(curl -sS --max-time 5 "http://${HOST}:${PORT}/stats" 2>/dev/null || true)
  if [ -z "$body" ] || ! echo "$body" | jq -e '.scheduler.mode' >/dev/null 2>&1; then
    sleep "$INTERVAL"
    continue
  fi
  row=$(echo "$body" | jq -r '[
    '"$ts"', .scheduler.mode,
    (.scheduler.user_io_read.p50_us//0),(.scheduler.user_io_read.ewma_p50_us//0),
    (.scheduler.user_io_read.outstanding_peak//0),(.scheduler.user_io_read.slo_breach_count//0),
    (.scheduler.writer_durable.p50_us//0),(.scheduler.writer_durable.ewma_p50_us//0),
    (.scheduler.writer_durable.slo_breach_count//0),
    (.scheduler.flush.p50_us//0),(.scheduler.flush.ewma_p50_us//0),
    (.scheduler.compaction.p50_us//0),(.scheduler.compaction.ewma_p50_us//0),
    (.scheduler.compaction.outstanding_peak//0),
    (.scheduler.compaction_blocked_us_total//0),
    (.scheduler.cross_lane_outstanding_peak//0),
    (.process.vmrss_bytes//0),(.cgroup.anon_bytes//0)
  ] | @csv' 2>/dev/null || true)
  [ -n "$row" ] && echo "$row" >> "$OUT"
  sleep "$INTERVAL"
done
