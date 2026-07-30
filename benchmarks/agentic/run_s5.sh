#!/usr/bin/env bash
# S5 hybrid search — exact filter (topic<T) + NEAREST, SAME question in the 4 engines,
# sweeping selectivity 50%->0.1%. qdrant comes best-armed here (filterable-HNSW).
# Engine-exclusive via lib_docker.sh. Envelopes: tight + roomy (§6.10 "two points").
#
#   PY=<venv> ENGINES_SEL="xyzdb pgvector qdrant chroma" S5_ENVS="2c8g" \
#     SPLIT=held K=10 MAXQ=0 OUTSUB=s5 bash run_s5.sh [engine ...]
set -uo pipefail
AG="$(cd "$(dirname "$0")" && pwd)"; cd "$AG"
# shellcheck source=lib_docker.sh
. ./lib_docker.sh
PY="${PY:-$AG/.venv/bin/python}"; export PY
OUTDIR="$AG/results/${OUTSUB:-s5}"; mkdir -p "$OUTDIR"; LOG="$OUTDIR/run.log"; touch "$LOG"

dense(){ export BENCH_PG_M=48 BENCH_PG_EFC=200 BENCH_PG_EFS=200 \
                BENCH_QD_M=48 BENCH_QD_EFC=200 BENCH_QD_EF=512 \
                BENCH_CH_M=32 BENCH_CH_CEF=200 BENCH_CH_SEF=512; }
tierspec(){ local l="$1" t; for t in "${TIERS_ALL[@]}"; do set -- $t; [ "$1" = "$l" ] && { echo "$t"; return; }; done; }

S5_ENVS="${S5_ENVS:-2c512-swap 2c8g}"
ENGINES=(${ENGINES_SEL:-xyzdb pgvector qdrant chroma})
[ $# -gt 0 ] && ENGINES=("$@")
SPLIT="${SPLIT:-held}"; K="${K:-10}"; MAXQ="${MAXQ:-0}"

dense
for envl in $S5_ENVS; do
  spec="$(tierspec "$envl")"; [ -z "$spec" ] && { echo "unknown tier $envl" | tee -a "$LOG"; continue; }
  read -r label mem memswap cpus cache <<<"$spec"
  for e in "${ENGINES[@]}"; do
    out="$OUTDIR/${label}__${e}.jsonl"; donef="$out.done"
    [ -f "$donef" ] && { echo "[skip] $out" | tee -a "$LOG"; continue; }
    : > "$out"
    echo "[$(date +%H:%M:%S)] $label $e" | tee -a "$LOG"
    if ! up_engine "$e" "$mem" "$memswap" "$cpus" "$cache"; then
      st=$(dead_reason "$e")
      "$PY" -c "import json;open('$out','a').write(json.dumps({'kind':'s5','engine':'$e','envelope':'$label','status':'$st'})+chr(10))"
      echo "  -> $st" | tee -a "$LOG"; down_engine "$e"; touch "$donef"; continue
    fi
    if "$PY" measure_s5.py --engine "$e" --container "bench-$e" $(diskarg_for "$e") \
         --storage "${STORAGE:-local}" --envelope "$label" --split "$SPLIT" --k "$K" \
         --base_n "${BASE_N:-0}" --max_queries "$MAXQ" --out "$out" >>"$LOG" 2>&1; then
      touch "$donef"
    elif [ "$(docker inspect -f '{{.State.Running}}' bench-$e 2>/dev/null)" != true ]; then
      st=$(dead_reason "$e")
      "$PY" -c "import json;open('$out','a').write(json.dumps({'kind':'s5','engine':'$e','envelope':'$label','status':'$st'})+chr(10))"
      echo "  -> measure died: $st" | tee -a "$LOG"; touch "$donef"
    else
      echo "  !! measure nonzero (engine alive) — no .done, retry on resume" | tee -a "$LOG"
    fi
    down_engine "$e"
  done
done
echo "[$(date +%H:%M:%S)] S5 DONE -> $OUTDIR" | tee -a "$LOG"
