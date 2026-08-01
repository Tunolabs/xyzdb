#!/usr/bin/env bash
# Corpus A recall run (Mac/arm, direction). Envelope holgado (8G) → todos SIRVEN → recall.
# Motores: xyzDB before(0.8.13) + after(0.9) [recall = exacto, debe coincidir] + rivales
# pgvector/qdrant/chroma × {scoped, flat}. Dense config. STOP tras recall (cobertura/OOM aparte).
# Canario: xyzdb-scoped debe dar recall@10 = 0.9429 exacto (el techo). f32 lossless en todos.
# Rival images: single pinned source (see images.env). require_pinned_images is
# the negative control — this runner dies if it is not sourced or if a moving
# tag creeps back in, instead of silently resolving `:latest`.
. "$(cd "$(dirname "$0")" && pwd)/images.env"
require_pinned_images || exit 1

set -uo pipefail
AG="$(cd "$(dirname "$0")" && pwd)"; cd "$AG"; PY="$AG/.venv/bin/python"
OUT=/tmp/lme_recall.jsonl; : > "$OUT"
export BENCH_PG_M=48 BENCH_PG_EFC=200 BENCH_PG_EFS=200 \
       BENCH_QD_M=48 BENCH_QD_EFC=200 BENCH_QD_EF=512 \
       BENCH_CH_M=32 BENCH_CH_CEF=200 BENCH_CH_SEF=512 \
       BUILD_TIMEOUT=1800

up(){ # $1=engine $2=image(xyzdb only)
  local e=$1 img=${2:-} c=bench-$1
  docker rm -f "$c" >/dev/null 2>&1; docker volume rm "bench_$1" >/dev/null 2>&1
  case "$e" in
    xyzdb)    docker run -d --name "$c" --cpus 2 --memory 8g -p 2505:2505 -v "bench_$1:/data" "$img" --port 2505 --path /data/bench --bind 0.0.0.0 --cache-size 512 >/dev/null 2>&1;;
    pgvector) docker run -d --name "$c" --cpus 2 --memory 8g -p 5432:5432 -e POSTGRES_PASSWORD=bench -v "bench_$1:/var/lib/postgresql" "$IMG_PG" >/dev/null 2>&1;;
    qdrant)   docker run -d --name "$c" --cpus 2 --memory 8g -p 6333:6333 -v "bench_$1:/qdrant/storage" "$IMG_QDRANT" >/dev/null 2>&1;;
    chroma)   docker run -d --name "$c" --cpus 2 --memory 8g -p 8000:8000 -v "bench_$1:/data" "$IMG_CHROMA" >/dev/null 2>&1;;
  esac
  local i=0; while [ $i -lt 120 ]; do
    case "$e" in
      xyzdb)    nc -z 127.0.0.1 2505 2>/dev/null && return 0;;
      pgvector) "$PY" -c "import psycopg2;psycopg2.connect(host='127.0.0.1',port=5432,user='postgres',password='bench',dbname='postgres').close()" 2>/dev/null && return 0;;
      qdrant)   curl -fsS http://127.0.0.1:6333/readyz >/dev/null 2>&1 && return 0;;
      chroma)   curl -fsS http://127.0.0.1:8000/api/v2/heartbeat >/dev/null 2>&1 && return 0;;
    esac; i=$((i+1)); sleep 1; done; return 1; }

cell(){ # $1=engine $2=label $3=scopedflag(--scoped|"") $4=image(xyzdb)
  local e=$1 lbl=$2 sc=$3 img=${4:-}
  echo "[$(date +%H:%M:%S)] $lbl: up @8G"
  if ! up "$e" "$img"; then echo "  $lbl no arrancó"; docker rm -f bench-$e >/dev/null 2>&1; return; fi
  echo "[$(date +%H:%M:%S)] $lbl: load 246k + recall held-out ..."
  "$PY" measure_lme.py --engine "$e" --container "bench-$e" --volume "bench_$e" \
    --envelope 2c8G --split held --pass_label dense $sc --out "$OUT" || echo "  $lbl measure nonzero"
  docker rm -f bench-$e >/dev/null 2>&1
}

# xyzDB before/after (recall exacto — deben coincidir; el canario)
cell xyzdb xyzdb-after  "" xyzdb:0.9-v3-arm64-dev
cell xyzdb xyzdb-before "" xyzdb:0.8.13-before
# rivales × {scoped, flat}
for e in pgvector qdrant chroma; do
  cell "$e" "$e-scoped" "--scoped" ""
  cell "$e" "$e-flat"   ""         ""
done
echo "[$(date +%H:%M:%S)] LME RECALL DONE -> $OUT"
