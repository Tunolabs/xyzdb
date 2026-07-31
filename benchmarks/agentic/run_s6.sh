#!/usr/bin/env bash
# S6 one-engine-for-the-agent — cells are DEPLOYMENTS, not binaries. xyz / pg are
# one system (1 container); qdrant+pg / chroma+pg are the real stack (vector engine
# + a Postgres store on 5433) — the two-container deployment IS the point (measures
# the stack tax). Envelope 2c8g (§6.10). Reuses lib_docker.sh for the vector side.
#
#   PY=<venv> DEPLOYS="xyz pg qdrant+pg chroma+pg" S6_ENV=2c8g N_TURNS=200 TOPIC=1 \
#     OUTSUB=s6 bash run_s6.sh [deployment ...]
set -uo pipefail
AG="$(cd "$(dirname "$0")" && pwd)"; cd "$AG"
# shellcheck source=lib_docker.sh
. ./lib_docker.sh
PY="${PY:-$AG/.venv/bin/python}"; export PY
OUTDIR="$AG/results/${OUTSUB:-s6}"; mkdir -p "$OUTDIR"; LOG="$OUTDIR/run.log"; touch "$LOG"
tierspec(){ local l="$1" t; for t in "${TIERS_ALL[@]}"; do set -- $t; [ "$1" = "$l" ] && { echo "$t"; return; }; done; }
S6_ENV="${S6_ENV:-2c8g}"; N_TURNS="${N_TURNS:-200}"; TOPIC="${TOPIC:-1}"
DEPLOYS=(${DEPLOYS:-xyz pg qdrant+pg chroma+pg})
[ $# -gt 0 ] && DEPLOYS=("$@")
read -r label mem memswap cpus cache <<<"$(tierspec "$S6_ENV")"

up_store(){  # a Postgres structured store on 5433 for the +store deployments
  docker rm -f bench-store >/dev/null 2>&1 || true
  docker volume rm bench_store >/dev/null 2>&1 || true
  docker run -d --name bench-store --cpus 1 --memory 512m --memory-swap 1g \
    -p 5433:5432 -e POSTGRES_PASSWORD=bench -v bench_store:/var/lib/postgresql \
    "$IMG_PG" >/dev/null 2>&1
  local i=0; while [ $i -lt 90 ]; do
    "$PY" -c "import psycopg2;psycopg2.connect(host='127.0.0.1',port=5433,user='postgres',password='bench',dbname='postgres').close()" 2>/dev/null && return 0
    [ "$(docker inspect -f '{{.State.Running}}' bench-store 2>/dev/null)" = false ] && return 1
    i=$((i+1)); sleep 1; done; return 1
}
down_store(){ docker rm -f bench-store >/dev/null 2>&1 || true; }

vec_engine(){ case "$1" in xyz) echo xyzdb;; pg) echo pgvector;; qdrant+pg) echo qdrant;; chroma+pg) echo chroma;; esac; }

for d in "${DEPLOYS[@]}"; do
  e="$(vec_engine "$d")"; needstore=""; case "$d" in *+pg) needstore=1;; esac
  out="$OUTDIR/${label}__$(echo "$d" | tr '+' '-').jsonl"; donef="$out.done"
  [ -f "$donef" ] && { echo "[skip] $out" | tee -a "$LOG"; continue; }
  : > "$out"; fail_reason=""
  echo "[$(date +%H:%M:%S)] $label $d (vector=$e store=${needstore:-no})" | tee -a "$LOG"
  if ! up_engine "$e" "$mem" "$memswap" "$cpus" "$cache"; then
    st=$(dead_reason "$e")
    "$PY" -c "import json;open('$out','a').write(json.dumps({'kind':'s6','deployment':'$d','envelope':'$label','status':'$st'})+chr(10))"
    echo "  -> $st" | tee -a "$LOG"; down_engine "$e" "$st"; touch "$donef"; continue
  fi
  storeargs=""
  if [ -n "$needstore" ]; then
    if ! up_store; then echo "  -> store failed to start" | tee -a "$LOG"; down_engine "$e"; continue; fi
    storeargs="--store_volume bench_store"
  fi
  if "$PY" measure_s6.py --deployment "$d" --container "bench-$e" --envelope "$label" \
       --n_turns "$N_TURNS" --topic "$TOPIC" --base_n "${BASE_N:-0}" --storage "${STORAGE:-local}" \
       $(echo $(diskarg_for "$e") | sed 's/--volume/--vec_volume/;s/--disk_path/--vec_disk_path/') \
       $storeargs --out "$out" >>"$LOG" 2>&1; then
    touch "$donef"
  else
    echo "  !! measure_s6 nonzero" | tee -a "$LOG"
  fi
  down_engine "$e"; [ -n "$needstore" ] && down_store
done
echo "[$(date +%H:%M:%S)] S6 DONE -> $OUTDIR" | tee -a "$LOG"
