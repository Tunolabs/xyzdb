#!/usr/bin/env bash
# xyzDB before(0.8.13) vs after(0.9) — Fase 1 evolution. Rivals set aside; this measures MY OWN
# improvement. Envelope ladder {128M,256M,512M,2G,8G} × sizes {pool-500/2000/5000, full-dense
# (≈246k product-at-scale, 499 buckets), mono-189k (scan-at-scale, 1 bucket)} × {before, after}.
# Per cell: recall (CANARY: before==after, Fase 1 is read-path not recall) · p50/p99 · build/load ·
# RAM peak+rest · disk · CPU% · serves/OOM.  cache=envelope/4 so a scan can exceed cache (G2) at
# tight envelopes; --nearest-budget-ms 0 forces exact (clean canary, mono never truncates).
# NOTE: both images are arm64 → v3/AVX2 is inert on Mac; before/after here isolates G1a+G2+G3.
# v3 (compute) shows only on x86/AWS (pending). Engine-exclusive. OOM = result. Resumable (append).
set -uo pipefail
AG="$(cd "$(dirname "$0")" && pwd)"; cd "$AG"; PY="$AG/.venv/bin/python"
OUT=/tmp/xyz_beforeafter.jsonl; : > "$OUT"
AFTER=xyzdb:0.9-v3-arm64-dev
BEFORE=xyzdb:0.8.13-before

# envelope -> "mem cache_mb"  (cache≈envelope/4). case, not `declare -A` (macOS bash 3.2 lacks it).
env_params(){ case "$1" in
    128M) echo "128m 32";; 256M) echo "256m 64";; 512M) echo "512m 128";;
    2G) echo "2g 512";; 8G) echo "8g 2048";; esac; }
ENVS="8G 2G 512M 256M 128M"     # serves-first, then tighten

up(){ # $1=image $2=mem $3=cache
  local img=$1 mem=$2 cache=$3 c=bench-xyzdb
  docker rm -f "$c" >/dev/null 2>&1; docker volume rm bench_xyzdb >/dev/null 2>&1
  docker run -d --name "$c" --cpus 2 --memory "$mem" -p 2505:2505 -v bench_xyzdb:/data "$img" \
    --port 2505 --path /data/bench --bind 0.0.0.0 --insecure-allow-no-auth --cache-size "$cache" --nearest-budget-ms 0 >/dev/null 2>&1
  local i=0; while [ $i -lt 120 ]; do nc -z 127.0.0.1 2505 2>/dev/null && return 0; i=$((i+1)); sleep 1; done
  return 1; }

cell(){ # $1=image $2=imglabel $3=mem $4=cache $5=envlbl $6=corpus $7=size $8=extra-args
  local img=$1 lbl=$2 mem=$3 cache=$4 env=$5 corp=$6 size=$7 extra=$8
  echo "[$(date +%H:%M:%S)] $env $corp/$size $lbl: up (mem=$mem cache=$cache)"
  if ! up "$img" "$mem" "$cache"; then
    echo "  no arrancó (envelope demasiado ajustado para el proceso base) -> registro serves=false"
    "$PY" - "$OUT" "$env" "$lbl" "$corp" "$size" <<'PY'
import json,sys
o,env,lbl,corp,size=sys.argv[1:6]
open(o,"a").write(json.dumps({"kind":"sizesweep","engine":"xyzdb","image":lbl,"envelope":env,
  "corpus":f"{corp}-?","bucket_size":int(size),"serves":False,"status":"container_did_not_start"})+"\n")
PY
    docker rm -f bench-xyzdb >/dev/null 2>&1; return
  fi
  echo "[$(date +%H:%M:%S)] $env $corp/$size $lbl: load + recall + latency/RAM/disk/CPU ..."
  "$PY" measure_sizesweep.py --engine xyzdb --container bench-xyzdb --volume bench_xyzdb \
    --corpus "$corp" --size "$size" --envelope "$env" --image "$lbl" $extra --out "$OUT" \
    || echo "  measure nonzero"
  docker rm -f bench-xyzdb >/dev/null 2>&1
}

for env in $ENVS; do
  read mem cache <<<"$(env_params "$env")"
  # sizes: pool 500/2000/5000 (450q, warm+r3) · full-dense 380 (~499 buckets, 450q) · mono-189k (30q, O(N) scan)
  for size in 500 2000 5000; do
    cell "$AFTER"  after  "$mem" "$cache" "$env" pool "$size" "--warmup 1 --repeats 3"
    cell "$BEFORE" before "$mem" "$cache" "$env" pool "$size" "--warmup 1 --repeats 3"
  done
  cell "$AFTER"  after  "$mem" "$cache" "$env" full 380 "--warmup 1 --repeats 3"
  cell "$BEFORE" before "$mem" "$cache" "$env" full 380 "--warmup 1 --repeats 3"
  cell "$AFTER"  after  "$mem" "$cache" "$env" full 200000 "--warmup 0 --repeats 1 --max_queries 30"
  cell "$BEFORE" before "$mem" "$cache" "$env" full 200000 "--warmup 0 --repeats 1 --max_queries 30"
  echo "[$(date +%H:%M:%S)] === envelope $env COMPLETO ==="
done
echo "[$(date +%H:%M:%S)] BEFORE/AFTER SWEEP DONE -> $OUT"
