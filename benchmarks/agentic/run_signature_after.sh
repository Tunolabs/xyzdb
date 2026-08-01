#!/usr/bin/env bash
# Cross-engine agentic sweep, AFTER image (xyzDB 0.9) vs pgvector·qdrant·chroma.
# Rebuilt harness (measure_x.py + adapters.py). Engine-EXCLUSIVE, one container up
# at a time. Mode=both (query recall/latency/RAM-peak + footprint RAM-rest/disk).
# Dense pass (rivals at step-1 tuned config; xyzDB exact, no dial). Resumable via
# per-cell .done. Synthetic corpus. **Mac/OrbStack = DIRECTION only** (page-cache
# mediated) — the publishable table is m6a native x86. OOM/no-start = recorded result.
# Rival images: single pinned source (see images.env). require_pinned_images is
# the negative control — this runner dies if it is not sourced or if a moving
# tag creeps back in, instead of silently resolving `:latest`.
. "$(cd "$(dirname "$0")" && pwd)/images.env"
require_pinned_images || exit 1

set -uo pipefail
AG="$(cd "$(dirname "$0")" && pwd)"; cd "$AG"
PY="$AG/.venv/bin/python"
OUTDIR="$AG/results/${OUTSUB:-xeng}"; mkdir -p "$OUTDIR"; LOG="$OUTDIR/run.log"; touch "$LOG"
XYZDB_IMG="${XYZDB_IMG:-xyzdb:0.9-v3-arm64-dev}"

# PROFILE preset (small | fast | full). Sets the envelope/corpus lists unless
# ENV_SPECS / CORP_SPECS override them explicitly.
#   small = 1 corpus (galaxy 30k) × 1 envelope (2c256M) = the minimum RELIABLE
#           cross-engine snapshot at the product envelope (recall + footprint +
#           latency + serves, all four engines). ~10-15 min / ~3 min per engine.
#   fast  = galaxy 30k + super 30k × {2c256M, 2c2G}. ~20-30 min. Adds a superbucket
#           and a roomy envelope for a latency/RAM contrast.
#   full  = 6 envelopes × 5 corpora (incl. 250k/500k) = the multi-hour publishable-shape run.
PROFILE="${PROFILE:-full}"
case "$PROFILE" in
  small) DEF_ENV=("2c256M 256m 2 64"); DEF_CORP=("galaxy /tmp/galaxy_mini.npz 0");;
  fast)  DEF_ENV=("2c256M 256m 2 64" "2c2G 2g 2 512")
         DEF_CORP=("galaxy /tmp/galaxy_mini.npz 0" "30k /tmp/s30k.npz 100");;
  full)  DEF_ENV=("1c128M 128m 1 32" "1c256M 256m 1 64" "2c512M 512m 2 128" \
                  "2c1G 1g 2 256" "2c2G 2g 2 512" "2c8G 8g 2 2048")
         DEF_CORP=("30k /tmp/s30k.npz 100" "100k /tmp/s100k.npz 50" "galaxy /tmp/galaxy250k.npz 0" \
                   "250k /tmp/s250k.npz 30" "500k /tmp/s500k.npz 20");;
  *) echo "unknown PROFILE=$PROFILE (use small|fast|full)"; exit 2;;
esac
# envelope: "label mem cpus xyzdbcache" · corpus: "label file maxq" — ';'-separated overrides
if [ -n "${ENV_SPECS:-}" ]; then IFS=';' read -ra ENVELOPES <<<"$ENV_SPECS"; else ENVELOPES=("${DEF_ENV[@]}"); fi
if [ -n "${CORP_SPECS:-}" ]; then IFS=';' read -ra CORPORA <<<"$CORP_SPECS"; else CORPORA=("${DEF_CORP[@]}"); fi
# shellcheck disable=SC2206
ENGINES=(${ENGINES_SEL:-xyzdb pgvector qdrant chroma})
# Positional args select engines to run (e.g. `run_signature_after.sh xyzdb qdrant`).
# No args → all four. Lets you sweep all now, then re-validate just one later.
[ $# -gt 0 ] && ENGINES=("$@")

# Storage: label annotated in every record + WHERE the engine data lives.
#   STORAGE=ssd|hdd|local   (default local)         — the label
#   STORAGE_ROOT=/mnt/ssd   (empty → named volumes) — host path for bind mounts on AWS
# On AWS: STORAGE=ssd STORAGE_ROOT=/mnt/ssd ... (and a second run STORAGE=hdd STORAGE_ROOT=/mnt/hdd).
# On Mac: leave STORAGE_ROOT unset → docker named volumes (virtiofs bind is flaky here).
STORAGE="${STORAGE:-local}"
STORAGE_ROOT="${STORAGE_ROOT:-}"
BENCH_SENTINEL="xyzdb-bench"   # guards the destructive clean of a bind dir

datadir_for(){ case "$1" in xyzdb|chroma) echo /data;; pgvector) echo /var/lib/postgresql;; qdrant) echo /qdrant/storage;; esac; }
diskarg_for(){ if [ -n "$STORAGE_ROOT" ]; then echo "--disk_path $STORAGE_ROOT/$1"; else echo "--volume bench_$1"; fi; }

# dense (tuned rival) config — the fair "rivals at their best" pass
dense(){ export BENCH_PG_M=48 BENCH_PG_EFC=200 BENCH_PG_EFS=200 \
                BENCH_QD_M=48 BENCH_QD_EFC=200 BENCH_QD_EF=512 \
                BENCH_CH_M=32 BENCH_CH_CEF=200 BENCH_CH_SEF=512; }

port_for(){ case "$1" in xyzdb) echo 2505;; pgvector) echo 5432;; qdrant) echo 6333;; chroma) echo 8000;; esac; }

up(){   # $1=engine $2=mem $3=cpus $4=cache  -> 0 ready / 1 not
  local e=$1 mem=$2 cpus=$3 cache=$4 c=bench-$1 dd src mnt; dd=$(datadir_for "$e")
  docker rm -f "$c" >/dev/null 2>&1 || true
  if [ -n "$STORAGE_ROOT" ]; then                    # AWS bind mount: /mnt/ssd|/mnt/hdd
    case "$STORAGE_ROOT" in ""|/|/mnt) echo "FATAL: unsafe STORAGE_ROOT='$STORAGE_ROOT'" | tee -a "$LOG"; return 1;; esac
    src="$STORAGE_ROOT/$e"; mkdir -p "$src"; find "$src" -mindepth 1 -delete 2>/dev/null || true
  else                                               # Mac default: docker named volume
    src="bench_$e"; docker volume rm "$src" >/dev/null 2>&1 || true
  fi
  mnt="$src:$dd"
  case "$e" in
    xyzdb)    docker run -d --name "$c" --cpus "$cpus" --memory "$mem" -p 2505:2505 -v "$mnt" \
                "$XYZDB_IMG" --port 2505 --path /data/bench --bind 0.0.0.0 --cache-size "$cache" >/dev/null 2>&1 ;;
    pgvector) docker run -d --name "$c" --cpus "$cpus" --memory "$mem" -p 5432:5432 -e POSTGRES_PASSWORD=bench \
                -v "$mnt" "$IMG_PG" >/dev/null 2>&1 ;;
    qdrant)   docker run -d --name "$c" --cpus "$cpus" --memory "$mem" -p 6333:6333 -v "$mnt" \
                "$IMG_QDRANT" >/dev/null 2>&1 ;;
    chroma)   docker run -d --name "$c" --cpus "$cpus" --memory "$mem" -p 8000:8000 -v "$mnt" \
                "$IMG_CHROMA" >/dev/null 2>&1 ;;
  esac
  local i=0
  while [ $i -lt 90 ]; do
    case "$e" in
      xyzdb)    nc -z 127.0.0.1 2505 2>/dev/null && return 0 ;;
      pgvector) "$PY" -c "import psycopg2;psycopg2.connect(host='127.0.0.1',port=5432,user='postgres',password='bench',dbname='postgres').close()" 2>/dev/null && return 0 ;;
      qdrant)   curl -fsS http://127.0.0.1:6333/readyz >/dev/null 2>&1 && return 0 ;;
      chroma)   curl -fsS http://127.0.0.1:8000/api/v2/heartbeat >/dev/null 2>&1 && return 0 ;;
    esac
    # bail early if the container already died (OOM during start/load)
    [ "$(docker inspect -f '{{.State.Running}}' "$c" 2>/dev/null)" = false ] && return 1
    i=$((i+1)); sleep 1
  done
  return 1
}

dead_reason(){  # $1=engine -> "OOM_or_failed_to_start" | "crash_or_oom_during_load"
  local c=bench-$1
  [ "$(docker inspect -f '{{.State.OOMKilled}}' "$c" 2>/dev/null)" = true ] && { echo crash_or_oom_during_load; return; }
  echo OOM_or_failed_to_start
}

dense
for corpspec in "${CORPORA[@]}"; do
  read -r corp file maxq <<<"$corpspec"
  for envspec in "${ENVELOPES[@]}"; do
    read -r envl mem cpus cache <<<"$envspec"
    for e in "${ENGINES[@]}"; do
      out="$OUTDIR/${envl}__${corp}__${e}.jsonl"; donef="$out.done"
      [ -f "$donef" ] && { echo "[skip] $out" | tee -a "$LOG"; continue; }
      : > "$out"; fail_reason=""
      echo "[$(date +%H:%M:%S)] $envl $corp $e" | tee -a "$LOG"
      if ! up "$e" "$mem" "$cpus" "$cache"; then
        st=$(dead_reason "$e")
        "$PY" -c "import json;open('$out','a').write(json.dumps({'kind':'query','engine':'$e','envelope':'$envl','corpus':'$corp','pass':'dense','status':'$st'})+chr(10))"
        echo "  -> $st" | tee -a "$LOG"; docker rm -f bench-$e >/dev/null 2>&1; touch "$donef"; continue
      fi
      if "$PY" measure_x.py --engine "$e" --port "$(port_for "$e")" --container "bench-$e" $(diskarg_for "$e") \
           --storage "$STORAGE" --corpus "$file" --corpus_label "$corp" --pass_label dense --envelope "$envl" \
           --mode both --max_queries "$maxq" --repeats 1 --out "$out" >>"$LOG" 2>&1; then
        touch "$donef"
      elif [ "$(docker inspect -f '{{.State.Running}}' bench-$e 2>/dev/null)" != true ]; then
        st=$(dead_reason "$e")
        "$PY" -c "import json;open('$out','a').write(json.dumps({'kind':'query','engine':'$e','envelope':'$envl','corpus':'$corp','pass':'dense','status':'$st'})+chr(10))"
        echo "  -> measure died: $st" | tee -a "$LOG"; fail_reason="$st"; touch "$donef"
      else
        echo "  !! measure nonzero (engine alive) — no .done, retry on resume" | tee -a "$LOG"
      fi
      docker rm -f bench-$e >/dev/null 2>&1
    done
  done
done
echo "[$(date +%H:%M:%S)] XENG SWEEP DONE -> $OUTDIR" | tee -a "$LOG"
