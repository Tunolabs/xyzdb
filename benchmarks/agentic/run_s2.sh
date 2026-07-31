#!/usr/bin/env bash
# S2 live session — write<->search interleaved (~30W/70R), SAME loop in the 4 engines.
# Envelope 2c8g (§6.10). Engine-exclusive via lib_docker.sh. Durable-strict.
#
#   PY=<venv> ENGINES_SEL="xyzdb pgvector qdrant chroma" S2_ENV=2c8g CYCLES=2000 \
#     WFRAC=0.30 BASE_N=0 OUTSUB=s2 bash run_s2.sh [engine ...]
set -uo pipefail
AG="$(cd "$(dirname "$0")" && pwd)"; cd "$AG"
# shellcheck source=lib_docker.sh
. ./lib_docker.sh
PY="${PY:-$AG/.venv/bin/python}"; export PY
OUTDIR="$AG/results/${OUTSUB:-s2}"; mkdir -p "$OUTDIR"; LOG="$OUTDIR/run.log"; touch "$LOG"
dense(){ export BENCH_PG_M=48 BENCH_PG_EFC=200 BENCH_PG_EFS=200 \
                BENCH_QD_M=48 BENCH_QD_EFC=200 BENCH_QD_EF=512 \
                BENCH_CH_M=32 BENCH_CH_CEF=200 BENCH_CH_SEF=512; }
tierspec(){ local l="$1" t; for t in "${TIERS_ALL[@]}"; do set -- $t; [ "$1" = "$l" ] && { echo "$t"; return; }; done; }
S2_ENV="${S2_ENV:-2c8g}"; CYCLES="${CYCLES:-2000}"; WFRAC="${WFRAC:-0.30}"; BASE_N="${BASE_N:-0}"
ENGINES=(${ENGINES_SEL:-xyzdb pgvector qdrant chroma})
[ $# -gt 0 ] && ENGINES=("$@")
read -r label mem memswap cpus cache <<<"$(tierspec "$S2_ENV")"
dense
for e in "${ENGINES[@]}"; do
  out="$OUTDIR/${label}__${e}.jsonl"; donef="$out.done"
  [ -f "$donef" ] && { echo "[skip] $out" | tee -a "$LOG"; continue; }
  : > "$out"; fail_reason=""
  echo "[$(date +%H:%M:%S)] $label $e (cycles=$CYCLES wfrac=$WFRAC)" | tee -a "$LOG"
  if ! up_engine "$e" "$mem" "$memswap" "$cpus" "$cache"; then
    st=$(dead_reason "$e")
    "$PY" -c "import json;open('$out','a').write(json.dumps({'kind':'s2','engine':'$e','envelope':'$label','status':'$st'})+chr(10))"
    echo "  -> $st" | tee -a "$LOG"; down_engine "$e" "$st"; touch "$donef"; continue
  fi
  if "$PY" measure_s2.py --engine "$e" --container "bench-$e" $(diskarg_for "$e") \
       --storage "${STORAGE:-local}" --envelope "$label" --cycles "$CYCLES" --wfrac "$WFRAC" \
       --base_n "$BASE_N" --out "$out" >>"$LOG" 2>&1; then
    touch "$donef"
  elif [ "$(docker inspect -f '{{.State.Running}}' bench-$e 2>/dev/null)" != true ]; then
    st=$(dead_reason "$e")
    "$PY" -c "import json;open('$out','a').write(json.dumps({'kind':'s2','engine':'$e','envelope':'$label','status':'$st'})+chr(10))"
    echo "  -> measure died: $st" | tee -a "$LOG"; fail_reason="$st"; touch "$donef"
  else
    echo "  !! measure nonzero (engine alive) — retry on resume" | tee -a "$LOG"
  fi
  down_engine "$e" "${fail_reason:-}"
done
echo "[$(date +%H:%M:%S)] S2 DONE -> $OUTDIR" | tee -a "$LOG"
