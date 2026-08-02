#!/usr/bin/env bash
# The adjusted grid, one containerised engine at a time.
#
# Cells come from `grid.py` (MEASURED), not from a nested loop: the full cross
# product asks a question fifteen of its sixteen cells cannot answer. See that
# file for why, and for what is deliberately not measured.
#
# Designed to survive the operator walking away. Every step writes to the mounted
# results directory with its own log, `REAL_EXIT` is captured on the line after the
# command, and a failed cell is RECORDED and skipped rather than aborting the run —
# a run that dies on cell two teaches less than one that finishes with two holes
# named.
set -u
cd "$(dirname "$0")"
. ./images.env
require_pinned_images || exit 1
. ./lib_docker.sh
require_clean_engine_tree || exit 1

read -r LABEL MEM MEMSWAP CPUS CACHE <<<"$TIER_DEV"
OUT="${OUT:-/out/grid.jsonl}"
LOGS="${BENCH_OUT}/logs"; mkdir -p "$LOGS"
QUERIES="${QUERIES:-20}"; REPEATS="${REPEATS:-3}"

echo "== grid · tier $LABEL · image $IMG_XYZDB · out $BENCH_OUT"
bench_py grid.py | tee "$LOGS/grid-cells.jsonl"

# (point, cardinality) pairs, coarse first: the flagship cell is the one that can
# fail, and finding that out in minute five beats finding it out in hour two.
CELLS="1:2 1:10 5:2 5:10 1:100"

for ENGINE in xyzdb qdrant pgvector; do
  for CELL in $CELLS; do
    POINT="${CELL%%:*}"; CARD="${CELL##*:}"
    TAG="$ENGINE-p$POINT-c$CARD"
    echo "-- $TAG: up"
    if ! up_engine "$ENGINE" "$MEM" "$MEMSWAP" "$CPUS" "$CACHE"; then
      echo "{\"cell\":\"$TAG\",\"error\":\"$(dead_reason "$ENGINE")\"}" >> "$BENCH_OUT/grid.jsonl"
      down_engine "$ENGINE"; continue
    fi
    require_containerised_engine "$ENGINE" || { down_engine "$ENGINE"; continue; }

    run_step "$LOGS/$TAG-load.log" bench_py load_q3_point.py \
        --engine "$ENGINE" --point "$POINT" --out "$OUT"
    REAL_EXIT=$?
    echo "-- $TAG: load exit $REAL_EXIT"
    if [ "$REAL_EXIT" -ne 0 ]; then
      echo "{\"cell\":\"$TAG\",\"phase\":\"load\",\"exit\":$REAL_EXIT}" >> "$BENCH_OUT/grid.jsonl"
      down_engine "$ENGINE"; continue
    fi

    run_step "$LOGS/$TAG-sweep.log" bench_py run_q3_direction.py \
        --engines "$ENGINE" --point "$POINT" --cardinalities "$CARD" \
        --queries "$QUERIES" --repeats "$REPEATS" --exclusive --out "$OUT"
    REAL_EXIT=$?
    echo "-- $TAG: sweep exit $REAL_EXIT"
    [ "$REAL_EXIT" -ne 0 ] && echo "{\"cell\":\"$TAG\",\"phase\":\"sweep\",\"exit\":$REAL_EXIT}" >> "$BENCH_OUT/grid.jsonl"
    down_engine "$ENGINE"
  done
done

echo "== done · $BENCH_OUT/grid.jsonl"
