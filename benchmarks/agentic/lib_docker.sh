#!/usr/bin/env bash
# Shared docker-run harness for the agentic benchmark — THE single way engines are
# brought up (engine-exclusive: one container at a time). Replaces the retired
# compose path (run_signature.sh + engines/docker-compose*.yml). Sourced by every
# scenario runner (S1-S6) and, going forward, run_signature_after.sh.
#
# Hardware tiers. docker --memory-swap is the TOTAL (RAM + swap); when it equals
# --memory, swap is OFF. Tight tiers keep swap so the guest OS is not left without
# headroom (founder 2026-07-17). Build/smoke happen on the roomy 2c8g tier first,
# to separate hardware limits from harness bugs.
#
#   label        RAM   swap   --memory-swap(total)  cpus  xyz-cache
#   1c256-swap   256m  256m   512m                  1     64
#   2c512-swap   512m  512m   1g                    2     128
#   2c2g-swap    2g    512m   2560m                 2     512
#   2c8g         8g    0      8g (== RAM, no swap)  2     2048    <- dev/build tier
#
# Envelope spec string used by the runners: "label mem memswap cpus cache".
TIERS_ALL=(
  "1c256-swap 256m 512m  1 64"
  "2c512-swap 512m 1g    2 128"
  "2c2g-swap  2g   2560m 2 512"
  "2c8g       8g   8g    2 2048"
)
# Dev/build tier — "separate hardware from problems": build & smoke here first.
TIER_DEV="2c8g 8g 8g 2 2048"

# Default = the 0.9.6 build carrying fix A (crash-recovery read-path bloom-less
# fallback, commit b823070). Binary-identical to b823070 (the only tree diff is the
# test file, which does not change the server). Supersedes xyzdb:0.9.5-fb615b7 as the
# single measured image going forward ("what image do I measure?" — premise-20). The
# prior default (xyzdb:0.9-v3-arm64-dev, 2026-07-05) predated xyTalk v1 and the fix.
# Override via XYZDB_IMG.
# Engine image tag: DERIVED from the workspace manifest, never hardcoded. Four
# scripts here used to carry a literal default and they had drifted to two
# different stale versions (0.9.6-fixA and 0.9.8-x86v3) while the repo was at
# 1.1.0 — and this tag is baked into every record's `envelope` field, so the
# provenance stamp in the data named an engine that did not run. Deriving it
# cannot go stale.
xyz_manifest_version() {
  # Walk up from this script's directory until the workspace manifest appears.
  # No path arithmetic: the earlier version assumed ../.. and returned an empty
  # string in three of the four scripts, which would have tagged an image
  # `xyzdb:` with no version at all.
  local d; d="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
  while [ "$d" != "/" ]; do
    if [ -f "$d/Cargo.toml" ] && grep -q "^\[workspace\]" "$d/Cargo.toml"; then
      awk '/^\[workspace\.package\]/{f=1;next} /^\[/{f=0} f&&/^version[ 	]*=/{gsub(/[^0-9.]/,"");print;exit}' "$d/Cargo.toml"
      return 0
    fi
    d="$(dirname "$d")"
  done
  echo "FATAL: workspace Cargo.toml not found; refusing to tag an image without a version" >&2
  return 1
}
IMG_XYZDB="${XYZDB_IMG:-xyzdb:$(xyz_manifest_version)}"
export XYZDB_IMG="$IMG_XYZDB"   # so measure_*.py bench_stamp() records the exact image
IMG_PG="${PG_IMG:-pgvector/pgvector:pg18}"
IMG_QDRANT="${QDRANT_IMG:-qdrant/qdrant:latest}"
IMG_CHROMA="${CHROMA_IMG:-chromadb/chroma:latest}"

port_for(){ case "$1" in xyzdb) echo 2505;; pgvector) echo 5432;; qdrant) echo 6333;; chroma) echo 8000;; esac; }
datadir_for(){ case "$1" in xyzdb|chroma) echo /data;; pgvector) echo /var/lib/postgresql;; qdrant) echo /qdrant/storage;; esac; }
# Where measure_*.py should `du` the on-disk footprint: bind path (AWS) or named volume (Mac).
diskarg_for(){ if [ -n "${STORAGE_ROOT:-}" ]; then echo "--disk_path $STORAGE_ROOT/$1"; else echo "--volume bench_$1"; fi; }

# ─── Forensic mode (OFF by default) ──────────────────────────────────────────
# XYZ_FORENSIC=1 turns this harness into a trap for the "survivor key vanished"
# class. It exists because the event happened HERE, not in the test harness: the
# origin was a bench cell (S2@246k @cache128) in a release container, and the
# evidence was lost twice over — `down_engine` removes the container (taking the
# stderr where the engine's invariant-guard line WOULD have been printed, since
# the server DOES install a tracing subscriber) and `up_engine` wipes the datadir
# for the next cell. So the corpse and its last words were both deleted by design.
#
# NOT for a publishable run. It adds writes (log dump) and may archive the datadir,
# so a cell measured with it on must either be a hunt run, or declare forensic mode
# as a NAMED CONDITION in the reproducibility block. Reading the engine's invariant
# counters is NOT part of this flag — that is a single HTTP GET at a phase boundary
# in measure.py, always on, and it cannot move a number.
FORENSIC="${XYZ_FORENSIC:-0}"
FORENSIC_DIR="${XYZ_FORENSIC_DIR:-$PWD/forensics}"

forensic_banner(){
  [ "$FORENSIC" = 1 ] || return 0
  mkdir -p "$FORENSIC_DIR"
  echo "!!! FORENSIC MODE ON — container logs kept, datadir archived, failed containers NOT removed." >&2
  echo "!!! Numbers from this run are NOT publishable unless forensic mode is declared as a condition." >&2
  echo "!!! Artefacts: $FORENSIC_DIR" >&2
}

# preserve_forensics <engine> <reason> — freeze what a failing cell leaves behind.
# Dumps the container log FIRST (it dies with the container) and archives the
# datadir when it is a bind path. With a named/anonymous volume the datadir cannot
# be frozen without STORAGE_ROOT — that is reported as a LIMITATION, never faked.
preserve_forensics(){
  [ "$FORENSIC" = 1 ] || return 0
  local e=$1 reason=${2:-unknown} c=bench-$1 stamp
  stamp="$(date -u +%Y%m%dT%H%M%SZ)"
  local base="$FORENSIC_DIR/${stamp}-${e}-${reason}"
  mkdir -p "$base"
  docker logs "$c" > "$base/container.log" 2>&1 || echo "(no logs for $c)" > "$base/container.log"
  docker inspect "$c" > "$base/inspect.json" 2>/dev/null || true
  if [ -n "${STORAGE_ROOT:-}" ] && [ -d "$STORAGE_ROOT/$e" ]; then
    cp -a "$STORAGE_ROOT/$e" "$base/datadir" 2>/dev/null \
      && echo "preserved datadir → $base/datadir" >&2
  else
    echo "DATADIR NOT PRESERVED: no STORAGE_ROOT bind path (the engine data lives in a docker volume that up_engine wipes). Set STORAGE_ROOT to make the corpse survivable." \
      > "$base/DATADIR_NOT_PRESERVED.txt"
    echo "WARNING: datadir could not be preserved (no STORAGE_ROOT); see $base" >&2
  fi
  # Grep the dumped log for the invariant guard, so the hit is visible immediately
  # instead of waiting for someone to think of grepping months later.
  if grep -qE "overlap: table\[|silently miss present keys" "$base/container.log" 2>/dev/null; then
    echo "!!! INVARIANT GUARD FIRED in $c — see $base/container.log" >&2
    grep -nE "overlap: table\[|silently miss present keys" "$base/container.log" >&2 || true
  fi
  echo "forensics → $base" >&2
}

wait_ready(){  # $1=engine -> 0 ready / 1 died or timeout
  local e=$1 c=bench-$1 i=0 py="${PY:-python3}"
  while [ $i -lt 90 ]; do
    case "$e" in
      xyzdb)    nc -z 127.0.0.1 2505 2>/dev/null && return 0 ;;
      pgvector) "$py" -c "import psycopg2;psycopg2.connect(host='127.0.0.1',port=5432,user='postgres',password='bench',dbname='postgres').close()" 2>/dev/null && return 0 ;;
      qdrant)   curl -fsS http://127.0.0.1:6333/readyz >/dev/null 2>&1 && return 0 ;;
      chroma)   curl -fsS http://127.0.0.1:8000/api/v2/heartbeat >/dev/null 2>&1 && return 0 ;;
    esac
    # Bail early if the container already died (OOM during start/load) — a result, not a hang.
    [ "$(docker inspect -f '{{.State.Running}}' "$c" 2>/dev/null)" = false ] && return 1
    i=$((i+1)); sleep 1
  done
  return 1
}

up_engine(){  # $1=engine $2=mem $3=memswap $4=cpus $5=cache  -> 0 ready / 1 not
  local e=$1 mem=$2 memswap=$3 cpus=$4 cache=$5 c=bench-$1 dd src mnt; dd=$(datadir_for "$e")
  docker rm -f "$c" >/dev/null 2>&1 || true
  if [ -n "${STORAGE_ROOT:-}" ]; then                 # AWS bind mount: /mnt/ssd|/mnt/hdd
    case "$STORAGE_ROOT" in ""|/|/mnt) echo "FATAL: unsafe STORAGE_ROOT='$STORAGE_ROOT'" >&2; return 1;; esac
    src="$STORAGE_ROOT/$e"; mkdir -p "$src"; find "$src" -mindepth 1 -delete 2>/dev/null || true
  else                                                # Mac default: docker named volume
    src="bench_$e"; docker volume rm "$src" >/dev/null 2>&1 || true
  fi
  mnt="$src:$dd"
  local mflags="--cpus $cpus --memory $mem --memory-swap $memswap"
  case "$e" in
    # --insecure-allow-no-auth: 1.0 refuses a non-loopback bind without a token; this is a
    # throwaway benchmark container on a private host, wiped per cell. NOT for real use.
    xyzdb)    docker run -d --name "$c" $mflags -p 2505:2505 -v "$mnt" \
                "$IMG_XYZDB" --port 2505 --path /data/bench --bind 0.0.0.0 --insecure-allow-no-auth --cache-size "$cache" >/dev/null 2>&1 ;;
    pgvector) docker run -d --name "$c" $mflags -p 5432:5432 -e POSTGRES_PASSWORD=bench -v "$mnt" "$IMG_PG" >/dev/null 2>&1 ;;
    qdrant)   docker run -d --name "$c" $mflags -p 6333:6333 -v "$mnt" "$IMG_QDRANT" >/dev/null 2>&1 ;;
    chroma)   docker run -d --name "$c" $mflags -p 8000:8000 -v "$mnt" "$IMG_CHROMA" >/dev/null 2>&1 ;;
  esac
  wait_ready "$e"
}

# "OOM_or_failed_to_start" | "crash_or_oom_during_load" — a recorded result, never a silent skip.
dead_reason(){ local c=bench-$1; [ "$(docker inspect -f '{{.State.OOMKilled}}' "$c" 2>/dev/null)" = true ] && { echo crash_or_oom_during_load; return; }; echo OOM_or_failed_to_start; }
# down_engine <engine> [failure_reason] — tear the cell down.
# With a failure_reason AND forensic mode on, the artefacts are frozen and the
# container is LEFT IN PLACE for inspection (its logs are the only copy of what
# the engine said). Without a reason, or with forensics off, behaviour is the
# original unconditional removal so a normal run is byte-for-byte unchanged.
down_engine(){
  local e=$1 reason=${2:-}
  if [ -n "$reason" ] && [ "$FORENSIC" = 1 ]; then
    preserve_forensics "$e" "$reason"
    echo "forensic mode: leaving container bench-$e in place for inspection" >&2
    return 0
  fi
  docker rm -f "bench-$e" >/dev/null 2>&1 || true
}

# Announce forensic mode at source time (no-op when off).
forensic_banner
