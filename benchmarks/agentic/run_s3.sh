#!/usr/bin/env bash
# S3 fleet lifecycle — N tenants born/grown/purged, SAME op in the 4 engines.
# Steps 10/100/1000 (chroma OOM at 1000 in the tier = the result, not a failure).
# Envelope 2c8g (§6.10; add a mid tier via S3_ENVS for chroma's curve). lib_docker.sh.
#
#   PY=<venv> ENGINES_SEL="xyzdb pgvector qdrant chroma" S3_ENVS="2c8g" \
#     STEPS=10,100,1000 OUTSUB=s3 bash run_s3.sh [engine ...]
set -uo pipefail
AG="$(cd "$(dirname "$0")" && pwd)"; cd "$AG"
# shellcheck source=lib_docker.sh
. ./lib_docker.sh
PY="${PY:-$AG/.venv/bin/python}"; export PY
OUTDIR="$AG/results/${OUTSUB:-s3}"; mkdir -p "$OUTDIR"; LOG="$OUTDIR/run.log"; touch "$LOG"
dense(){ export BENCH_PG_M=48 BENCH_PG_EFC=200 BENCH_PG_EFS=200 \
                BENCH_QD_M=48 BENCH_QD_EFC=200 BENCH_QD_EF=512 \
                BENCH_CH_M=32 BENCH_CH_CEF=200 BENCH_CH_SEF=512; }
tierspec(){ local l="$1" t; for t in "${TIERS_ALL[@]}"; do set -- $t; [ "$1" = "$l" ] && { echo "$t"; return; }; done; }
S3_ENVS="${S3_ENVS:-2c8g}"; STEPS="${STEPS:-10,100,1000}"
ENGINES=(${ENGINES_SEL:-xyzdb pgvector qdrant chroma})
[ $# -gt 0 ] && ENGINES=("$@")
dense
for envl in $S3_ENVS; do
  read -r label mem memswap cpus cache <<<"$(tierspec "$envl")"
  for e in "${ENGINES[@]}"; do
    out="$OUTDIR/${label}__${e}.jsonl"; donef="$out.done"
    [ -f "$donef" ] && { echo "[skip] $out" | tee -a "$LOG"; continue; }
    : > "$out"
    echo "[$(date +%H:%M:%S)] $label $e steps=$STEPS" | tee -a "$LOG"
    if ! up_engine "$e" "$mem" "$memswap" "$cpus" "$cache"; then
      st=$(dead_reason "$e")
      "$PY" -c "import json;open('$out','a').write(json.dumps({'kind':'s3','engine':'$e','envelope':'$label','status':'$st'})+chr(10))"
      echo "  -> $st" | tee -a "$LOG"; down_engine "$e"; touch "$donef"; continue
    fi
    # S3 tolerates measure nonzero (OOM growing = a recorded result inside the jsonl).
    "$PY" measure_s3.py --engine "$e" --container "bench-$e" --envelope "$label" \
        --steps "$STEPS" --out "$out" >>"$LOG" 2>&1 || echo "  (measure exited nonzero — see jsonl for OOM record)" | tee -a "$LOG"
    touch "$donef"
    down_engine "$e"
  done
done
echo "[$(date +%H:%M:%S)] S3 DONE -> $OUTDIR" | tee -a "$LOG"
