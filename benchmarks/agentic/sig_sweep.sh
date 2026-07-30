#!/usr/bin/env bash
# Signature-grade publishable sweep. xyzdb 0.8.13 (streaming) + pgvector/qdrant/chroma.
# Envelopes {1c128M,1c256M,2c512M,2c2G,2c8G} × corpora {galaxy, super 30k/100k/250k/500k}
# × modes {query,footprint} × config-passes {dense=tuned rivals+xyzdb, light=idiomatic rivals}.
# ≥5 rounds A/B/A/B per query cell (run_signature). Resumable via per-cell .done sentinels.
# OOM = recorded result. Multi-day; run autonomous.
set -uo pipefail
SB="${SB:-/tmp/xyz-sig-sweep-scratch}"
AG="$(cd "$(dirname "$0")" && pwd)"
VENV="$SB/lme/venv/bin/python"
DATADIR=/tmp/xyzdb-bench-enginedata; export DATADIR
RESDIR="$AG/results/sig"; mkdir -p "$RESDIR"
LOG="$SB/sig_sweep.log"; touch "$LOG"
cd "$AG"

# envelope: label mem cpus cache
ENVELOPES=("1c128M 128M 1 32" "1c256M 256M 1 64" "2c512M 512M 2 128" "2c2G 2G 2 512" "2c8G 8G 2 2048")
# corpus: label relpath maxq
CORPORA=("galaxy synth_galaxy 0" "30k synth_super/30000 100" "100k synth_by_size/100000 50" "250k synth_by_size/250000 30" "500k synth_super/500000 20")

# tuned dense config (step-1 winners); light = adapter defaults (unset)
dense_env(){ export BENCH_PG_M=48 BENCH_PG_EFC=200 BENCH_PG_EFS=200 \
                    BENCH_QD_M=48 BENCH_QD_EFC=200 BENCH_QD_EF=512 \
                    BENCH_CH_M=32 BENCH_CH_CEF=200 BENCH_CH_SEF=512; }
light_env(){ unset BENCH_PG_M BENCH_PG_EFC BENCH_PG_EFS BENCH_QD_M BENCH_QD_EFC BENCH_QD_EF \
                   BENCH_CH_M BENCH_CH_CEF BENCH_CH_SEF; }

run_cell(){   # $1=envlabel $2=mem $3=cpus $4=cache $5=corpuslabel $6=emb $7=maxq $8=mode $9=pass $10=engines $11=rounds
  local envl=$1 mem=$2 cpus=$3 cache=$4 corp=$5 emb=$6 maxq=$7 mode=$8 pass=$9 engines=${10} rounds=${11}
  local out="$RESDIR/${envl}__${corp}__${mode}__${pass}.jsonl"
  local done="$out.done"
  if [ -f "$done" ]; then echo "[skip] $out (done)" | tee -a "$LOG"; return; fi
  echo "[$(date +%H:%M:%S)] CELL env=$envl corpus=$corp mode=$mode pass=$pass engines='$engines' rounds=$rounds" | tee -a "$LOG"
  if [ "$pass" = dense ]; then dense_env; else light_env; fi
  export CPUS="$cpus" MEMORY="$mem" CACHE_SIZE="$cache"
  EMB="$SB/$emb" VENV="$VENV" ENGINES="$engines" ROUNDS="$rounds" MODE="$mode" \
    ENVELOPE="$envl" DATADIR="$DATADIR" MAXQ="$maxq" KS="10,50" REPEATS=1 WARMUP=1 OUT="$out" \
    bash run_signature.sh >>"$LOG" 2>&1 && touch "$done" \
    || echo "!!! cell $out exited nonzero — no .done, will retry on resume" | tee -a "$LOG"
}

for envspec in "${ENVELOPES[@]}"; do
  read -r envl mem cpus cache <<<"$envspec"
  for corpspec in "${CORPORA[@]}"; do
    read -r corp emb maxq <<<"$corpspec"
    for mode in query footprint; do
      rounds=5; [ "$mode" = footprint ] && rounds=3   # disk/at-rest are stable; 3 suffices
      # dense pass: tuned rivals + xyzdb
      run_cell "$envl" "$mem" "$cpus" "$cache" "$corp" "$emb" "$maxq" "$mode" dense "xyzdb pgvector qdrant chroma" "$rounds"
      # light pass: idiomatic rivals (the frontier's other point) + xyzdb again as a
      # variance control (xyzdb has no dense/light dial → xyzdb-light must equal
      # xyzdb-dense within measurement noise; any gap is inter-round variance to explain).
      run_cell "$envl" "$mem" "$cpus" "$cache" "$corp" "$emb" "$maxq" "$mode" light "xyzdb pgvector qdrant chroma" "$rounds"
    done
  done
done
echo "[$(date +%H:%M:%S)] SIG SWEEP DONE — results in $RESDIR" | tee -a "$LOG"
