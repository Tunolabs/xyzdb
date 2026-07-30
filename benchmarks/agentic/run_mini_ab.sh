#!/usr/bin/env bash
# xyzdb-only Fase-1 A/B grid (Mac/ARM, DIRECTION; m6a rehearsal). NOT the rival
# signature sweep — this isolates the Fase-1 delta: before (v0.8.13-streaming)
# vs after (0.9: G1a+G4+G2+G3). Same synthetic corpus, same box/session, engine-
# exclusive, before/after alternated per cell (A/B drift control). 1-round SIGNAL
# (like the streaming-sweep doc). Resumable via per-cell .done. Mac numbers are
# direction only; absolute magnitude is the m6a block.
#
# Grid: 5 envelopes @2c x 5 corpora x {before,after}, mode=both (one load ->
# query metrics -> graceful restart -> footprint metrics).
set -uo pipefail
AG="$(cd "$(dirname "$0")" && pwd)"; cd "$AG"
PY="$AG/.venv/bin/python"
DATADIR=/tmp/xyzdb-bench-enginedata
OUTDIR="$AG/results/mini"; mkdir -p "$OUTDIR"
LOG="$OUTDIR/run.log"; touch "$LOG"
CONT=xyzmini
BEFORE=xyzdb:0.8.13-before
AFTER=xyzdb:0.9-v3-arm64-dev

# envelope: "label mem cpus cacheMB"  (all 2c — the real T6 core count)
ENVELOPES=("128M 128m 2 32" "256M 256m 2 64" "512M 512m 2 128" "2G 2g 2 512" "8G 8g 2 2048")
# corpus: "label file maxq"  — fast-first so early cells report quickly
CORPORA=("30k /tmp/s30k.npz 100" "100k /tmp/s100k.npz 50" "galaxy /tmp/galaxy250k.npz 0" \
         "250k /tmp/s250k.npz 30" "500k /tmp/s500k.npz 20")

img_for(){ [ "$1" = before ] && echo "$BEFORE" || echo "$AFTER"; }

up(){   # $1=image $2=mem $3=cpus $4=cache
  docker rm -f "$CONT" >/dev/null 2>&1 || true
  find "$DATADIR/xyzdb" -mindepth 1 -delete 2>/dev/null || true; mkdir -p "$DATADIR/xyzdb"
  docker run -d --name "$CONT" --cpus "$3" --memory "$2" -p 2505:2505 \
    -v "$DATADIR/xyzdb:/data" "$1" \
    --port 2505 --path /data/bench --bind 0.0.0.0 --cache-size "$4" >/dev/null 2>&1
  local i=0; while [ $i -lt 40 ]; do nc -z 127.0.0.1 2505 2>/dev/null && { sleep 1; return 0; }; i=$((i+1)); sleep 1; done
  return 1
}

for envspec in "${ENVELOPES[@]}"; do
  read -r envl mem cpus cache <<<"$envspec"
  for corpspec in "${CORPORA[@]}"; do
    read -r corp file maxq <<<"$corpspec"
    for lbl in before after; do   # A/B alternated per corpus×envelope
      out="$OUTDIR/${envl}__${corp}__${lbl}.jsonl"; done_f="$out.done"
      [ -f "$done_f" ] && { echo "[skip] $out" | tee -a "$LOG"; continue; }
      : > "$out"
      echo "[$(date +%H:%M:%S)] ${envl} ${corp} ${lbl}" | tee -a "$LOG"
      if ! up "$(img_for "$lbl")" "$mem" "$cpus" "$cache"; then
        "$PY" -c "import json;open('$out','a').write(json.dumps({'kind':'query','engine':'xyzdb','image':'$lbl','envelope':'$envl','corpus':'$corp','status':'OOM_or_failed_to_start'})+chr(10))"
        docker rm -f "$CONT" >/dev/null 2>&1 || true; touch "$done_f"; continue
      fi
      if "$PY" measure.py --container "$CONT" --corpus "$file" --mode both \
           --image "$lbl" --envelope "$envl" --round 1 --datadir "$DATADIR" \
           --max_queries "$maxq" --repeats 3 --out "$out" >>"$LOG" 2>&1; then
        # tag the corpus into every record for the pivot
        "$PY" -c "import json,sys; L=[json.loads(x) for x in open('$out')]; [d.update(corpus='$corp') for d in L]; open('$out','w').write(''.join(json.dumps(d)+chr(10) for d in L))"
        touch "$done_f"
      else
        echo "  !! measure nonzero for $out — no .done, will retry on resume" | tee -a "$LOG"
      fi
      docker rm -f "$CONT" >/dev/null 2>&1 || true
    done
  done
done
echo "[$(date +%H:%M:%S)] MINI A/B GRID DONE -> $OUTDIR" | tee -a "$LOG"
