#!/usr/bin/env bash
# Corpus A recall for the 3 rivals @ a roomy envelope (8G) so they SERVE and yield their
# HNSW recall (residual under the bge ceiling). Dense (tuned) config. Engine-exclusive.
# xyzDB's number is already in /tmp/lme_xyzdb.jsonl (the exact ceiling). OOM at tight
# envelopes is a separate coverage finding, not this run.
# Rival images: single pinned source (see images.env). require_pinned_images is
# the negative control — this runner dies if it is not sourced or if a moving
# tag creeps back in, instead of silently resolving `:latest`.
. "$(cd "$(dirname "$0")" && pwd)/images.env"
require_pinned_images || exit 1

set -uo pipefail
AG="$(cd "$(dirname "$0")" && pwd)"; cd "$AG"
PY="$AG/.venv/bin/python"
OUT=/tmp/lme_rivals.jsonl; : > "$OUT"
export BENCH_PG_M=48 BENCH_PG_EFC=200 BENCH_PG_EFS=200 \
       BENCH_QD_M=48 BENCH_QD_EFC=200 BENCH_QD_EF=512 \
       BENCH_CH_M=32 BENCH_CH_CEF=200 BENCH_CH_SEF=512

up(){ e=$1; c=bench-$e; docker rm -f "$c" >/dev/null 2>&1; docker volume rm "bench_$e" >/dev/null 2>&1
  case "$e" in
    pgvector) docker run -d --name "$c" --cpus 2 --memory 8g -p 5432:5432 -e POSTGRES_PASSWORD=bench -v "bench_$e:/var/lib/postgresql" "$IMG_PG" >/dev/null 2>&1;;
    qdrant)   docker run -d --name "$c" --cpus 2 --memory 8g -p 6333:6333 -v "bench_$e:/qdrant/storage" "$IMG_QDRANT" >/dev/null 2>&1;;
    chroma)   docker run -d --name "$c" --cpus 2 --memory 8g -p 8000:8000 -v "bench_$e:/data" "$IMG_CHROMA" >/dev/null 2>&1;;
  esac
  local i=0; while [ $i -lt 120 ]; do
    case "$e" in
      pgvector) "$PY" -c "import psycopg2;psycopg2.connect(host='127.0.0.1',port=5432,user='postgres',password='bench',dbname='postgres').close()" 2>/dev/null && return 0;;
      qdrant)   curl -fsS http://127.0.0.1:6333/readyz >/dev/null 2>&1 && return 0;;
      chroma)   curl -fsS http://127.0.0.1:8000/api/v2/heartbeat >/dev/null 2>&1 && return 0;;
    esac; i=$((i+1)); sleep 1; done; return 1; }

for e in pgvector qdrant chroma; do
  echo "[$(date +%H:%M:%S)] $e: up @8G"
  if ! up "$e"; then echo "  $e no arrancó"; docker rm -f bench-$e >/dev/null 2>&1; continue; fi
  echo "[$(date +%H:%M:%S)] $e: build HNSW 246k + recall held-out ..."
  "$PY" measure_lme.py --engine "$e" --container "bench-$e" --volume "bench_$e" \
    --envelope 2c8G --split held --pass_label dense --out "$OUT" || echo "  $e measure nonzero"
  docker rm -f bench-$e >/dev/null 2>&1
done
echo "[$(date +%H:%M:%S)] RIVALS LME DONE -> $OUT"
