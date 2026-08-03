#!/usr/bin/env bash
# Bucket-size sweep (Mac/arm, direction). ONE fixed 180k real-vector pool re-partitioned into
# buckets of {500,2000,5000} → única variable = tamaño. Oracle tie-aware recall@10 AND @50.
# xyzDB=exacto=1.000 (canary/control) · rivales muestran el residuo HNSW (crece con tamaño+profundidad).
# scoped best-form todos · f32 lossless · dense frozen HNSW (mismo config que el 0.9429 de hoy, INV-2).
# @2c/8G holgado → todos sirven → aísla tamaño, no envelope. Engine-exclusive (1 contenedor a la vez).
set -uo pipefail
AG="$(cd "$(dirname "$0")" && pwd)"; cd "$AG"; PY="$AG/.venv/bin/python"
OUT=/tmp/lme_sizesweep.jsonl; : > "$OUT"
SIZES="500 2000 5000"
export BENCH_PG_M=48 BENCH_PG_EFC=200 BENCH_PG_EFS=200 \
       BENCH_QD_M=48 BENCH_QD_EFC=200 BENCH_QD_EF=512 \
       BENCH_CH_M=32 BENCH_CH_CEF=200 BENCH_CH_SEF=512

up(){ # $1=engine $2=image(xyzdb only)
  local e=$1 img=${2:-} c=bench-$1
  docker rm -f "$c" >/dev/null 2>&1; docker volume rm "bench_$1" >/dev/null 2>&1
  case "$e" in
    xyzdb)    docker run -d --name "$c" --cpus 2 --memory 8g -p 2505:2505 -v "bench_$1:/data" "$img" --port 2505 --path /data/bench --bind 0.0.0.0 --insecure-allow-no-auth --cache-size 512 >/dev/null 2>&1;;
    pgvector) docker run -d --name "$c" --cpus 2 --memory 8g -p 5432:5432 -e POSTGRES_PASSWORD=bench -v "bench_$1:/var/lib/postgresql" pgvector/pgvector:pg18 >/dev/null 2>&1;;
    qdrant)   docker run -d --name "$c" --cpus 2 --memory 8g -p 6333:6333 -v "bench_$1:/qdrant/storage" qdrant/qdrant:latest >/dev/null 2>&1;;
    chroma)   docker run -d --name "$c" --cpus 2 --memory 8g -p 8000:8000 -v "bench_$1:/data" chromadb/chroma:latest >/dev/null 2>&1;;
  esac
  local i=0; while [ $i -lt 120 ]; do
    case "$e" in
      xyzdb)    nc -z 127.0.0.1 2505 2>/dev/null && return 0;;
      pgvector) "$PY" -c "import psycopg2;psycopg2.connect(host='127.0.0.1',port=5432,user='postgres',password='bench',dbname='postgres').close()" 2>/dev/null && return 0;;
      qdrant)   curl -fsS http://127.0.0.1:6333/readyz >/dev/null 2>&1 && return 0;;
      chroma)   curl -fsS http://127.0.0.1:8000/api/v2/heartbeat >/dev/null 2>&1 && return 0;;
    esac; i=$((i+1)); sleep 1; done; return 1; }

sweep(){ # $1=engine $2=image
  local e=$1 img=${2:-}
  for s in $SIZES; do
    echo "[$(date +%H:%M:%S)] $e size=$s: up @8G"
    if ! up "$e" "$img"; then echo "  $e no arrancó"; docker rm -f bench-$e >/dev/null 2>&1; continue; fi
    echo "[$(date +%H:%M:%S)] $e size=$s: load 180k ($((180000/s)) buckets) + recall + latency/RAM/disk/CPU ..."
    "$PY" measure_sizesweep.py --engine "$e" --container "bench-$e" --volume "bench_$e" \
      --size "$s" --warmup 1 --repeats 5 --out "$OUT" || echo "  $e size=$s measure nonzero"
    docker rm -f bench-$e >/dev/null 2>&1
  done
}

# xyzDB first = the exactness canary (must be 1.000 at every size), then the HNSW rivals.
sweep xyzdb xyzdb:0.9-v3-arm64-dev
for e in pgvector qdrant chroma; do sweep "$e" ""; done
echo "[$(date +%H:%M:%S)] SIZE SWEEP DONE -> $OUT"
