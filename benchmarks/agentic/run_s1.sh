#!/usr/bin/env bash
# S1 retrieve-and-expand — the 4 engines, SAME business question (NEAREST top-k in
# the bucket, then expand each hit to its full session). Engine-exclusive via
# lib_docker.sh. qdrant runs BOTH arms (scroll 2 RT + payload-dup 1 RT, disk tax) —
# one motor, two points. Envelopes: tight + roomy (§6.10 "two points").
#
#   PY=<venv> ENGINES_SEL="xyzdb pgvector qdrant chroma" S1_ENVS="2c8g" \
#     SPLIT=held K=10 MAXQ=0 OUTSUB=s1 bash run_s1.sh [engine ...]
set -uo pipefail
AG="$(cd "$(dirname "$0")" && pwd)"; cd "$AG"
# shellcheck source=lib_docker.sh
. ./lib_docker.sh
PY="${PY:-$AG/.venv/bin/python}"; export PY
OUTDIR="$AG/results/${OUTSUB:-s1}"; mkdir -p "$OUTDIR"; LOG="$OUTDIR/run.log"; touch "$LOG"

# dense (tuned rival) config — the fair "rivals at their best" pass (matches signature).
dense(){ export BENCH_PG_M=48 BENCH_PG_EFC=200 BENCH_PG_EFS=200 \
                BENCH_QD_M=48 BENCH_QD_EFC=200 BENCH_QD_EF=512 \
                BENCH_CH_M=32 BENCH_CH_CEF=200 BENCH_CH_SEF=512; }

tierspec(){ local l="$1" t; for t in "${TIERS_ALL[@]}"; do set -- $t; [ "$1" = "$l" ] && { echo "$t"; return; }; done; }

S1_ENVS="${S1_ENVS:-2c512-swap 2c8g}"   # tight + roomy
ENGINES=(${ENGINES_SEL:-xyzdb pgvector qdrant chroma})
[ $# -gt 0 ] && ENGINES=("$@")
SPLIT="${SPLIT:-held}"; K="${K:-10}"; MAXQ="${MAXQ:-0}"

dense
for envl in $S1_ENVS; do
  spec="$(tierspec "$envl")"; [ -z "$spec" ] && { echo "unknown tier $envl" | tee -a "$LOG"; continue; }
  read -r label mem memswap cpus cache <<<"$spec"
  for e in "${ENGINES[@]}"; do
    variants=("scroll"); [ "$e" = qdrant ] && variants=("scroll" "payload-dup")
    for v in "${variants[@]}"; do
      tag="$e"; [ "$e" = qdrant ] && tag="qdrant-$v"
      out="$OUTDIR/${label}__${tag}.jsonl"; donef="$out.done"
      [ -f "$donef" ] && { echo "[skip] $out" | tee -a "$LOG"; continue; }
      : > "$out"; fail_reason=""
      echo "[$(date +%H:%M:%S)] $label $tag" | tee -a "$LOG"
      if ! up_engine "$e" "$mem" "$memswap" "$cpus" "$cache"; then
        st=$(dead_reason "$e")
        "$PY" -c "import json,sys;open('$out','a').write(json.dumps({'kind':'s1','engine':'$e','envelope':'$label','qd_variant':('$v' if '$e'=='qdrant' else None),'status':'$st'})+chr(10))"
        echo "  -> $st" | tee -a "$LOG"; down_engine "$e" "$st"; touch "$donef"; continue
      fi
      qdarg=""; [ "$e" = qdrant ] && qdarg="--qd_variant $v"
      if "$PY" measure_s1.py --engine "$e" --container "bench-$e" $(diskarg_for "$e") \
           --storage "${STORAGE:-local}" --envelope "$label" --split "$SPLIT" --k "$K" \
           --base_n "${BASE_N:-0}" --max_queries "$MAXQ" $qdarg --out "$out" >>"$LOG" 2>&1; then
        touch "$donef"
      elif [ "$(docker inspect -f '{{.State.Running}}' bench-$e 2>/dev/null)" != true ]; then
        st=$(dead_reason "$e")
        "$PY" -c "import json;open('$out','a').write(json.dumps({'kind':'s1','engine':'$e','envelope':'$label','status':'$st'})+chr(10))"
        echo "  -> measure died: $st" | tee -a "$LOG"; fail_reason="$st"; touch "$donef"
      else
        echo "  !! measure nonzero (engine alive) — no .done, retry on resume" | tee -a "$LOG"
      fi
      down_engine "$e" "${fail_reason:-}"
    done
  done
done
echo "[$(date +%H:%M:%S)] S1 DONE -> $OUTDIR" | tee -a "$LOG"
