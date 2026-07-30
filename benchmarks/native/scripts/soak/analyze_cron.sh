#!/usr/bin/env bash
# Soak sidecar C — `xyzdb-cli admin analyze <lobe>` every
# $ANALYZE_INTERVAL_SEC (default 43200 = 12h). On first run, discovers
# the live lobe set via `SHOW LOBES` so the schema-shape is not hard-
# coded in the script.
#
# Failures are logged but never abort the soak (analyze is opportunistic,
# not load-bearing); gate-failure detection is the gate_monitor's job.
#
# Env knobs:
#   XYZDB_CLI              default ./target/release/xyzdb-cli
#   HOST                   default 127.0.0.1
#   PORT                   default 2505
#   ANALYZE_INTERVAL_SEC   default 43200 (12h)
#   LOG                    default /var/lib/xyzdb-soak/reports/analyze_cron.log
set -euo pipefail

XYZDB_CLI="${XYZDB_CLI:-./target/release/xyzdb-cli}"
HOST="${HOST:-127.0.0.1}"
PORT="${PORT:-2505}"
INTERVAL="${ANALYZE_INTERVAL_SEC:-43200}"
# Derive LOG from REPORT_DIR (exported by run_soak.sh) so master runner
# overrides take effect without per-sidecar env juggling.
LOG="${LOG:-${REPORT_DIR:-/var/lib/xyzdb-soak/reports}/analyze_cron.log}"

mkdir -p "$(dirname "$LOG")"
log() { echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) analyze_cron: $*" | tee -a "$LOG" >&2; }

discover_lobes() {
  # `SHOW LOBES` returns rows like `  0. <lobe_name> (N anchors)` after
  # a `Lobes:` header. Extract the second whitespace-separated token of
  # rows whose first non-space char is a digit.
  echo "SHOW LOBES" \
    | "$XYZDB_CLI" --host "$HOST" --port "$PORT" 2>/dev/null \
    | awk '/^[[:space:]]+[0-9]+\./ { print $2 }' \
    || true
}

log "starting (interval=${INTERVAL}s)"
while :; do
  sleep "$INTERVAL"
  log "discovering lobes via SHOW LOBES"
  # Portable to bash 3.2 (macOS) — avoid `mapfile`.
  lobes=()
  while IFS= read -r line; do
    [ -n "$line" ] && lobes+=("$line")
  done < <(discover_lobes)
  if [ "${#lobes[@]}" -eq 0 ]; then
    log "WARN no lobes returned; skipping cycle"
    continue
  fi
  log "analyze targets: ${lobes[*]}"
  for lobe in "${lobes[@]}"; do
    if "$XYZDB_CLI" --host "$HOST" --port "$PORT" admin analyze "$lobe" >> "$LOG" 2>&1; then
      log "analyze $lobe OK"
    else
      log "WARN analyze $lobe failed"
    fi
  done
done
