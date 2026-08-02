#!/usr/bin/env bash
# Q3 selectivity sweep — DIRECTION ONLY, one containerised engine at a time.
#
# WHY THIS WRAPPER EXISTS
# -----------------------
# `run_q3_direction.py` takes a port and assumes something is already listening.
# The first attempt at this sweep satisfied that by starting a natively-built
# `target/release/xyzdb-server` on the host, which breaks the two rules this
# harness is built around: every engine under measurement is a pinned CONTAINER,
# and exactly one runs at a time. A native binary is not the artefact anyone
# ships, and a host process has neither the cpu nor the memory bound its rivals
# are held to — so its numbers are not comparable to theirs by construction.
#
# `lib_docker.sh` already does it right for every engine including xyzDB
# (per-cell up → measure → down, `--cpus`/`--memory` applied uniformly). This
# script routes the sweep through it, so the engine-exclusive property is
# structural instead of remembered.
#
# USAGE
#   XYZDB_IMG=xyzdb:<sha> ./run_q3_direction.sh [extra args for the .py]
#
# The image is NOT defaulted here on purpose: `lib_docker.sh` carries the single
# default and overriding it silently is how a matrix ends up measuring a binary
# nobody named. Rival images come digest-pinned from images.env.
set -u
cd "$(dirname "$0")"

. ./images.env
require_pinned_images || exit 1
. ./lib_docker.sh

# T3 at the front door: a modified engine tree cannot be measured. This is the
# check that would have stopped the 2026-08-01 drift, where bench work found an
# engine bug, fixed it, and kept measuring a tree matching no built artefact.
require_clean_engine_tree || exit 1

read -r LABEL MEM MEMSWAP CPUS CACHE <<<"$TIER_DEV"
OUT="${OUT:-/tmp/q3_direction_$(date +%H%M%S).jsonl}"
PY="${PY:-python3}"
POINT="${POINT:-1}"   # 1 = pool: one bucket holding the whole corpus (the Q3-pool shape)
N="${N:-}"            # empty = the whole corpus; set for a smoke run

echo "== tier $LABEL (mem=$MEM cpus=$CPUS cache=$CACHE) · image $IMG_XYZDB · out $OUT"

# One engine at a time, in the order the .py sweeps them. Each cell brings its
# own container up on a wiped datadir and takes it down before the next starts,
# so no two engines are ever resident together.
for ENGINE in xyzdb qdrant pgvector; do
    echo "-- $ENGINE: up"
    if ! up_engine "$ENGINE" "$MEM" "$MEMSWAP" "$CPUS" "$CACHE"; then
        echo "{\"engine\":\"$ENGINE\",\"error\":\"$(dead_reason "$ENGINE")\"}" | tee -a "$OUT"
        down_engine "$ENGINE"
        continue
    fi
    # T1: prove what answers on the port is the container we just started, not a
    # host process that happens to be listening. `up_engine` returning 0 says the
    # port answers; it does not say who answered.
    require_containerised_engine "$ENGINE" || { down_engine "$ENGINE"; continue; }

    # The container starts empty (up_engine wipes the datadir / volume), so each
    # cell loads its own copy of the frozen corpus through the same adapters the
    # rest of the matrix uses. `$?` is captured on its own line: after a pipe it
    # would report the last stage's status, not the runner's.
    "$PY" load_q3_point.py --engine "$ENGINE" --point "$POINT" ${N:+--n "$N"} --out "$OUT"
    REAL_EXIT=$?
    echo "-- $ENGINE: load exit $REAL_EXIT"
    if [ "$REAL_EXIT" -ne 0 ]; then down_engine "$ENGINE"; continue; fi

    "$PY" run_q3_direction.py --engines "$ENGINE" --xyz-port "$(port_for xyzdb)" \
        ${N:+--n "$N"} --exclusive --out "$OUT" "$@"
    REAL_EXIT=$?
    echo "-- $ENGINE: sweep exit $REAL_EXIT"
    down_engine "$ENGINE"
done

echo "== done · $OUT"
