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
IMG_XYZDB="${XYZDB_IMG:-xyzdb:0.9.6-fixA}"
export XYZDB_IMG="$IMG_XYZDB"   # so measure_*.py bench_stamp() records the exact image
IMG_PG="${PG_IMG:-pgvector/pgvector:pg18}"
IMG_QDRANT="${QDRANT_IMG:-qdrant/qdrant:latest}"
IMG_CHROMA="${CHROMA_IMG:-chromadb/chroma:latest}"

port_for(){ case "$1" in xyzdb) echo 2505;; pgvector) echo 5432;; qdrant) echo 6333;; chroma) echo 8000;; esac; }
datadir_for(){ case "$1" in xyzdb|chroma) echo /data;; pgvector) echo /var/lib/postgresql;; qdrant) echo /qdrant/storage;; esac; }
# Where measure_*.py should `du` the on-disk footprint: bind path (AWS) or named volume (Mac).
diskarg_for(){ if [ -n "${STORAGE_ROOT:-}" ]; then echo "--disk_path $STORAGE_ROOT/$1"; else echo "--volume bench_$1"; fi; }

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
down_engine(){ docker rm -f "bench-$1" >/dev/null 2>&1 || true; }
