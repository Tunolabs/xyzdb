#!/usr/bin/env bash
# Rival coverage/OOM — the counterpart to "xyzDB streams 189K to 128M". Can pg/qdrant/chroma serve
# 189K real vectors across the envelope ladder, or do they OOM BUILDING the HNSW? Mac/SSD (local),
# ladder {8G,2G,512M,256M,128M}. FLAT: one HNSW over all 189K (where the build balloons). Each
# rival tuned FAIRLY per envelope (pg shared_buffers+maintenance_work_mem scaled — not a fixed 2GB
# that OOMs trivially). serves/OOM recorded, and WHERE: build (oom_during_load / unviable_build_
# timeout) vs query (oom_during_query) — the headline is "OOMs BUILDING the index", separated.
# xyzDB is NOT run here (already serves all envelopes streaming, from the before/after matrix).
# Engine-exclusive. BUILD_TIMEOUT 600s (a >10min on-disk build over 189K = unviable for the point).
set -uo pipefail
AG="$(cd "$(dirname "$0")" && pwd)"; cd "$AG"; PY="$AG/.venv/bin/python"
OUT="${OUT:-/tmp/xyz_rival_coverage.jsonl}"; [ "${APPEND:-0}" = 1 ] || : > "$OUT"
ENVS="${ENVS:-8G 2G 512M 256M 128M}"
export BENCH_PG_M=48 BENCH_PG_EFC=200 BENCH_PG_EFS=200 \
       BENCH_QD_M=48 BENCH_QD_EFC=200 BENCH_QD_EF=512 \
       BENCH_CH_M=32 BENCH_CH_CEF=200 BENCH_CH_SEF=512 \
       BUILD_TIMEOUT=${BUILD_TIMEOUT:-600}

env_mem(){ case "$1" in 128M) echo 128m;; 256M) echo 256m;; 512M) echo 512m;; 2G) echo 2g;; 8G) echo 8g;; esac; }
# pg needs shared_buffers to boot + maintenance_work_mem to build — both scaled to the envelope.
pg_sb(){  case "$1" in 128M) echo 16MB;; 256M) echo 32MB;; 512M) echo 64MB;; 2G) echo 256MB;; 8G) echo 512MB;; esac; }
pg_mwm(){ case "$1" in 128M) echo 48MB;; 256M) echo 96MB;; 512M) echo 192MB;; 2G) echo 1GB;; 8G) echo 2GB;; esac; }

up(){ # $1=engine $2=envlbl
  local e=$1 env=$2 mem c=bench-$1; mem=$(env_mem "$env")
  docker rm -f "$c" >/dev/null 2>&1; docker volume rm "bench_$1" >/dev/null 2>&1
  case "$e" in
    pgvector) docker run -d --name "$c" --cpus 2 --memory "$mem" -p 5432:5432 -e POSTGRES_PASSWORD=bench \
                -v "bench_$1:/var/lib/postgresql" pgvector/pgvector:pg18 \
                -c shared_buffers=$(pg_sb "$env") -c maintenance_work_mem=$(pg_mwm "$env") >/dev/null 2>&1;;
    qdrant)   docker run -d --name "$c" --cpus 2 --memory "$mem" -p 6333:6333 -v "bench_$1:/qdrant/storage" qdrant/qdrant:latest >/dev/null 2>&1;;
    chroma)   docker run -d --name "$c" --cpus 2 --memory "$mem" -p 8000:8000 -v "bench_$1:/data" chromadb/chroma:latest >/dev/null 2>&1;;
  esac
  local i=0; while [ $i -lt 90 ]; do
    case "$e" in
      pgvector) "$PY" -c "import psycopg2;psycopg2.connect(host='127.0.0.1',port=5432,user='postgres',password='bench',dbname='postgres').close()" 2>/dev/null && return 0;;
      qdrant)   curl -fsS http://127.0.0.1:6333/readyz >/dev/null 2>&1 && return 0;;
      chroma)   curl -fsS http://127.0.0.1:8000/api/v2/heartbeat >/dev/null 2>&1 && return 0;;
    esac; i=$((i+1)); sleep 1; done; return 1; }

cell(){ # $1=engine $2=envlbl
  local e=$1 env=$2
  echo "[$(date +%H:%M:%S)] $env $e: up ($(env_mem "$env"))"
  if ! up "$e" "$env"; then
    echo "  $e no arrancó @$env → serves=false (proceso base no cabe en el envelope)"
    BENCH_PG_MWM=$(pg_mwm "$env") "$PY" - "$OUT" "$env" "$e" <<'PY'
import json,sys; o,env,e=sys.argv[1:4]
open(o,"a").write(json.dumps({"kind":"sizesweep","engine":e,"image":"rival","envelope":env,
  "corpus":"full-189514","scoped":False,"serves":False,"status":"container_did_not_start",
  "phase":"boot","note":"base process does not fit the envelope"})+"\n")
PY
    docker rm -f bench-$e >/dev/null 2>&1; return
  fi
  echo "[$(date +%H:%M:%S)] $env $e: build 1×HNSW/189K + serve/OOM ..."
  BENCH_PG_MWM=$(pg_mwm "$env") "$PY" measure_sizesweep.py --engine "$e" --container "bench-$e" --volume "bench_$e" \
    --corpus full --size 200000 --scoped 0 --envelope "$env" --image rival --storage ssd \
    --warmup 0 --repeats 1 --max_queries 20 --out "$OUT" || echo "  $e measure nonzero"
  docker rm -f bench-$e >/dev/null 2>&1; }

for env in $ENVS; do
  for e in pgvector qdrant chroma; do cell "$e" "$env"; done
  echo "[$(date +%H:%M:%S)] === envelope $env COMPLETO ==="
done
echo "[$(date +%H:%M:%S)] RIVAL COVERAGE DONE -> $OUT"
