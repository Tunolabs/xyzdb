#!/usr/bin/env bash
# Fase-1 before/after — the CLEAN (no-tradeoff, Mac-observable) comparisons the 50-cell matrix
# didn't isolate. Real 189K, f32, --nearest-budget-ms 0. Two blocks:
#   1. G2 ISOLATION: same mono-189k bucket @8G, vary ONLY cache (64MB→G2 fires span>cache,
#      2048MB→fits→G2 off), before vs after. Separates G2's bypass from G1a/G3.
#   2. A/B/A/B: pool/5000 + mono-189k @8G, 5 interleaved rounds per (size,image) → pooled-σ
#      significance on the ~20% p50 win (turns DIRECTION into a real claim).
# Engine-exclusive. v3 inert on arm Mac (both images arm64). Resumable (append).
set -uo pipefail
AG="$(cd "$(dirname "$0")" && pwd)"; cd "$AG"; PY="$AG/.venv/bin/python"
OUT=/tmp/xyz_fase1_clean.jsonl; : > "$OUT"
AFTER=xyzdb:0.9-v3-arm64-dev
BEFORE=xyzdb:0.8.13-before

up(){ # $1=image $2=cache_mb
  local img=$1 cache=$2 c=bench-xyzdb
  docker rm -f "$c" >/dev/null 2>&1; docker volume rm bench_xyzdb >/dev/null 2>&1
  docker run -d --name "$c" --cpus 2 --memory 8g -p 2505:2505 -v bench_xyzdb:/data "$img" \
    --port 2505 --path /data/bench --bind 0.0.0.0 --insecure-allow-no-auth --cache-size "$cache" --nearest-budget-ms 0 >/dev/null 2>&1
  local i=0; while [ $i -lt 120 ]; do nc -z 127.0.0.1 2505 2>/dev/null && return 0; i=$((i+1)); sleep 1; done
  return 1; }

run(){ # $1=image $2=imglabel $3=cache $4=envlbl $5=corpus $6=size $7=round $8=extra
  local img=$1 lbl=$2 cache=$3 env=$4 corp=$5 size=$6 rnd=$7 extra=$8
  echo "[$(date +%H:%M:%S)] $env $corp/$size $lbl r$rnd (cache=$cache): up"
  if ! up "$img" "$cache"; then echo "  no arrancó"; docker rm -f bench-xyzdb >/dev/null 2>&1; return; fi
  "$PY" measure_sizesweep.py --engine xyzdb --container bench-xyzdb --volume bench_xyzdb \
    --corpus "$corp" --size "$size" --envelope "$env" --image "$lbl" --round "$rnd" $extra --out "$OUT" \
    || echo "  measure nonzero"
  docker rm -f bench-xyzdb >/dev/null 2>&1; }

MONO="--corpus full --size 200000 --warmup 0 --repeats 1 --max_queries 30"
BIG="--corpus pool --size 5000 --warmup 1 --repeats 3"

# ---- Block 1: G2 isolation (mono@8G, cache 64 vs 2048) ----
echo "[$(date +%H:%M:%S)] === BLOQUE 1: G2 isolation ==="
for cache in 64 2048; do
  run "$AFTER"  after  "$cache" "8G-c${cache}" full 200000 1 "$MONO"
  run "$BEFORE" before "$cache" "8G-c${cache}" full 200000 1 "$MONO"
done

# ---- Block 2: A/B/A/B (5 rounds, interleaved per size) ----
echo "[$(date +%H:%M:%S)] === BLOQUE 2: A/B/A/B 5 rondas ==="
for r in 1 2 3 4 5; do
  run "$AFTER"  after  2048 8G pool 5000   "$r" "$BIG"
  run "$BEFORE" before 2048 8G pool 5000   "$r" "$BIG"
  run "$AFTER"  after  2048 8G full 200000 "$r" "$MONO"
  run "$BEFORE" before 2048 8G full 200000 "$r" "$MONO"
done
echo "[$(date +%H:%M:%S)] FASE1 CLEAN DONE -> $OUT"
