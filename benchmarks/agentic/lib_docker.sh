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
# Rival images come from images.env, digest-pinned, with NO fallback default on
# purpose: the old `${PG_IMG:-pgvector/pgvector:pg18}` form meant a caller who set
# nothing silently got a moving tag. require_pinned_images turns that silence into
# a failure.
. "$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)/images.env"
require_pinned_images || exit 1

# ─── Tripwires ───────────────────────────────────────────────────────────────
# Three rules that were WRITTEN DOWN and got broken anyway, on 2026-08-01, by
# someone who had all three in front of them. A rule you have to remember is a
# rule you will skip the day the task changes shape under you. These make them
# impossible to skip instead, in the pattern `require_pinned_images` already set:
# the runner that does not satisfy one DIES, it does not warn.
#
# Each carries the negative control that proves it can fail, because a tripwire
# nobody has ever seen fire is indistinguishable from a comment.

# T3 — a modified engine does not measure.
#
# THE ONE THAT WOULD HAVE STOPPED IT. The session that broke the other two began
# as benchmark work, found an engine bug, fixed it, and kept measuring — with an
# engine tree that no longer matched any built artefact. Numbers from that tree
# name a binary nobody can rebuild.
#
# Diagnosing an engine bug from a bench is legitimate and expected; what is not
# is carrying the modified tree into a measurement. So the check is at the
# runner's front door, not in the client.
#
# Negative control: `touch crates/engine/src/lib.rs` (or edit anything under
# crates/) and run any runner — it must die here.
require_clean_engine_tree(){
    command -v git >/dev/null 2>&1 || return 0        # not a checkout: nothing to assert
    local root dirty
    root=$(git rev-parse --show-toplevel 2>/dev/null) || return 0
    dirty=$(git -C "$root" status --porcelain -- crates/ 2>/dev/null)
    if [ -n "$dirty" ]; then
        echo "FATAL: the engine tree is modified — a benchmark cannot measure it." >&2
        echo "$dirty" | sed 's/^/       /' >&2
        echo "       If the work changed the engine, this is no longer a bench session:" >&2
        echo "       commit or stash it, rebuild the image, and measure that image." >&2
        echo "       (Diagnosing with a local build is fine — measuring with one is not.)" >&2
        return 1
    fi
}

# T1 — what is measured must be a container.
#
# The engine under measurement has to be the artefact that ships, held to the
# same `--cpus`/`--memory` bound as its rivals. A host process is neither. This
# asserts the port is published by a RUNNING container whose name is the one
# `up_engine` creates — not merely that something answers, which a native binary
# on the same port satisfies just as well.
#
# Negative control: start `target/release/xyzdb-server --port 2505` on the host
# with no container up, then call this — it must fail.
require_containerised_engine(){   # $1=engine
    local e=$1 c=bench-$1 port; port=$(port_for "$e")
    local state; state=$(docker inspect -f '{{.State.Running}}' "$c" 2>/dev/null)
    if [ "$state" != "true" ]; then
        echo "FATAL: no running container '$c' — refusing to measure." >&2
        echo "       Something may be answering on port $port, but a host process is" >&2
        echo "       not the artefact under test and carries none of the cell's limits." >&2
        return 1
    fi
    docker port "$c" "$port" >/dev/null 2>&1 || {
        echo "FATAL: container '$c' does not publish port $port." >&2
        return 1
    }
}

# ─── The harness runs in its own image too ───────────────────────────────────
#
# `bench_py <script> [args…]` runs a harness step inside `Dockerfile.bench`
# instead of against whatever Python the host has. The engines are pinned by
# digest; the clients that talk to them are pinned by this image. Before it
# existed the qdrant client sat two minors behind its own server and chroma could
# not be installed at all, because the host venv was Python 3.9.
#
# The repo is mounted, not baked: editing a runner must not mean rebuilding an
# image, and an image that carries no benchmark code cannot drift from the repo.
BENCH_IMG="${BENCH_IMG:-xyzdb-bench:local}"
BENCH_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"

bench_build(){
    docker build -q -f "$BENCH_DIR/Dockerfile.bench" -t "$BENCH_IMG" "$BENCH_DIR" >/dev/null || {
        echo "FATAL: could not build $BENCH_IMG" >&2; return 1; }
}

bench_py(){   # $1=script (repo-relative to benchmarks/agentic), rest=args
    local repo; repo=$(cd "$BENCH_DIR/../.." && pwd)
    docker run --rm \
        --add-host=host.docker.internal:host-gateway \
        -v "$BENCH_DIR":/bench \
        -v "$repo/examples/client/python":/client:ro \
        -e BENCH_ENGINE_HOST=host.docker.internal \
        -e XYZDB_IMG="${XYZDB_IMG:-}" \
        -w /bench \
        "$BENCH_IMG" "$@"
}

# T2 — full capture, and the exit code of the thing that mattered.
#
# Twice a pipe hid a failure (`cmd | head` reporting head's status); once `tail`
# ate the evidence of which test failed; and once a trailing `grep -c` found zero
# failures, exited 1 for "no match", and turned a GREEN tree into a reported red.
# All four are the same root: the status of a compound is the status of its LAST
# command, and a filter is a lossy witness.
#
# So: everything goes to a file (never `tail`, never `head`), `REAL_EXIT` is
# captured on its very next line, and nothing runs between the command and that
# capture. Grep the FILE afterwards, as much as you like — the code is already
# safe in `REAL_EXIT`.
#
# Negative control: `run_step /tmp/x.log false` must return 1.
run_step(){   # $1=logfile, rest=command -> the command's own exit code
    local log=$1; shift
    "$@" > "$log" 2>&1
    local REAL_EXIT=$?
    return $REAL_EXIT
}

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
