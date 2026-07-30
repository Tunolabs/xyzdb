#!/usr/bin/env bash
# S4 serverless wake — TTFQ (restart -> first query) + at-rest footprint, SAME probe
# in the 4 engines, swept across ALL tiers incl. the tightest (§6.10 — S4's reason
# to exist is the full envelope, where a rival may not wake). lib_docker.sh.
#
#   PY=<venv> ENGINES_SEL="xyzdb pgvector qdrant chroma" S4_ENVS="1c256-swap 2c512-swap 2c2g-swap 2c8g" \
#     BASE_N=0 OUTSUB=s4 bash run_s4.sh [engine ...]
set -uo pipefail
AG="$(cd "$(dirname "$0")" && pwd)"; cd "$AG"
# shellcheck source=lib_docker.sh
. ./lib_docker.sh
PY="${PY:-$AG/.venv/bin/python}"; export PY
OUTDIR="$AG/results/${OUTSUB:-s4}"; mkdir -p "$OUTDIR"; LOG="$OUTDIR/run.log"; touch "$LOG"
dense(){ export BENCH_PG_M=48 BENCH_PG_EFC=200 BENCH_PG_EFS=200 \
                BENCH_QD_M=48 BENCH_QD_EFC=200 BENCH_QD_EF=512 \
                BENCH_CH_M=32 BENCH_CH_CEF=200 BENCH_CH_SEF=512; }
tierspec(){ local l="$1" t; for t in "${TIERS_ALL[@]}"; do set -- $t; [ "$1" = "$l" ] && { echo "$t"; return; }; done; }
S4_ENVS="${S4_ENVS:-1c256-swap 2c512-swap 2c2g-swap 2c8g}"; BASE_N="${BASE_N:-0}"
ENGINES=(${ENGINES_SEL:-xyzdb pgvector qdrant chroma})
[ $# -gt 0 ] && ENGINES=("$@")
dense
for envl in $S4_ENVS; do
  spec="$(tierspec "$envl")"; [ -z "$spec" ] && { echo "unknown tier $envl" | tee -a "$LOG"; continue; }
  read -r label mem memswap cpus cache <<<"$spec"
  for e in "${ENGINES[@]}"; do
    out="$OUTDIR/${label}__${e}.jsonl"; donef="$out.done"
    [ -f "$donef" ] && { echo "[skip] $out" | tee -a "$LOG"; continue; }
    : > "$out"
    echo "[$(date +%H:%M:%S)] $label $e" | tee -a "$LOG"
    if ! up_engine "$e" "$mem" "$memswap" "$cpus" "$cache"; then
      st=$(dead_reason "$e")
      "$PY" -c "import json;open('$out','a').write(json.dumps({'kind':'s4','engine':'$e','envelope':'$label','status':'$st'})+chr(10))"
      echo "  -> $st" | tee -a "$LOG"; down_engine "$e"; touch "$donef"; continue
    fi
    "$PY" measure_s4.py --engine "$e" --container "bench-$e" $(diskarg_for "$e") \
        --storage "${STORAGE:-local}" --envelope "$label" --base_n "$BASE_N" --out "$out" >>"$LOG" 2>&1 \
      || echo "  (measure exited nonzero — see jsonl)" | tee -a "$LOG"
    touch "$donef"
    down_engine "$e"
  done
done
echo "[$(date +%H:%M:%S)] S4 DONE -> $OUTDIR" | tee -a "$LOG"
