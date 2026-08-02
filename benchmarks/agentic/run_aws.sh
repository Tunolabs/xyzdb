#!/usr/bin/env bash
# AWS m6a x86 — the PUBLISHABLE block (real disk, x86 images with v3/AVX2 active). New harness
# (measure_sizesweep.py). Bind-mounts real block devices; run ssd and hdd as SEPARATE sequential
# passes (never both at once — the T6 envelope must be engine-exclusive):
#   STORAGE=ssd STORAGE_ROOT=/mnt/ssd ./run_aws.sh
#   STORAGE=hdd STORAGE_ROOT=/mnt/hdd ./run_aws.sh
#
# PREREQS on the box:
#   1. Build BOTH x86 images: `xyzdb:0.9-v3-x86` (v3 rustflags active) + `xyzdb:0.8.13-before-x86`.
#      Override tags via AFTER_IMG / BEFORE_IMG.
#   2. rsync the corpus: benchmarks/agentic/corpora/lme/{cvec,qvec}.npy + meta.json to the box.
#   3. .venv present (qdrant-client/chromadb/psycopg2+pgvector) — same as Mac.
#   4. v3-verify is SEPARATE (objdump AVX2/FMA + result fingerprint 0xa74b71bdc019be72) — do it
#      BEFORE trusting after-image numbers. See `v3_verify.sh`.
#
# Phases: A = rival coverage (pg/qdrant/chroma, 189K flat, ladder, serves/OOM build-vs-query).
#         B = xyzDB before/after (pool/5000 + mono-189k × ladder, canary + latency/RAM/disk/CPU).
# G3 (readahead) finally observable here: real block device, not OrbStack virtiofs.
set -uo pipefail
AG="$(cd "$(dirname "$0")" && pwd)"; cd "$AG"; PY="$AG/.venv/bin/python"
STORAGE="${STORAGE:-ssd}"; STORAGE_ROOT="${STORAGE_ROOT:-/mnt/$STORAGE}"
AFTER_IMG="${AFTER_IMG:-xyzdb:0.9-v3-x86}"; BEFORE_IMG="${BEFORE_IMG:-xyzdb:0.8.13-before-x86}"
PHASE="${PHASE:-all}"     # all | coverage | beforeafter
ROUNDS="${ROUNDS:-1}"     # A/B/A/B interleaved rounds for Phase B (5 = publishable, pooled-σ)
MONO_Q="${MONO_Q:-30}"    # queries for the O(N) mono-189k scan (kept low; each ~hundreds of ms)
OUT="$AG/results/aws_${STORAGE}.jsonl"; mkdir -p "$AG/results"; [ "${APPEND:-0}" = 1 ] || : > "$OUT"
ENVS="${ENVS:-8G 2G 512M 256M 128M}"
export BENCH_PG_M=48 BENCH_PG_EFC=200 BENCH_PG_EFS=200 \
       BENCH_QD_M=48 BENCH_QD_EFC=200 BENCH_QD_EF=512 \
       BENCH_CH_M=32 BENCH_CH_CEF=200 BENCH_CH_SEF=512 BUILD_TIMEOUT=600

env_mem(){ case "$1" in 128M) echo 128m;; 256M) echo 256m;; 512M) echo 512m;; 2G) echo 2g;; 8G) echo 8g;; esac; }
env_cache(){ case "$1" in 128M) echo 32;; 256M) echo 64;; 512M) echo 128;; 2G) echo 512;; 8G) echo 2048;; esac; }
pg_sb(){  case "$1" in 128M) echo 16MB;; 256M) echo 32MB;; 512M) echo 64MB;; 2G) echo 256MB;; 8G) echo 512MB;; esac; }
pg_mwm(){ case "$1" in 128M) echo 48MB;; 256M) echo 96MB;; 512M) echo 192MB;; 2G) echo 1GB;; 8G) echo 2GB;; esac; }
datadir_for(){ case "$1" in xyzdb|chroma) echo /data;; pgvector) echo /var/lib/postgresql;; qdrant) echo /qdrant/storage;; esac; }
port_for(){ case "$1" in xyzdb) echo 2505;; pgvector) echo 5432;; qdrant) echo 6333;; chroma) echo 8000;; esac; }
# REAL disk: bind-mount the block device. host `du` on that path = the publishable footprint.
mount_arg(){ local e=$1 d; d=$(datadir_for "$e"); mkdir -p "$STORAGE_ROOT/bench_$e"; echo "-v $STORAGE_ROOT/bench_$e:$d"; }
disk_args(){ echo "--disk_path $STORAGE_ROOT/bench_$1 --storage $STORAGE"; }
# Wipe the per-engine data dir BETWEEN cells. rm -rf + recreate (find -mindepth -delete left the
# xyzDB lobe behind on real disk → "Lobe 'mem' already exists"; xyzDB has no drop_lobe, so the dir
# MUST start empty). Guarded to a bench-only path.
clean_disk(){ [ -n "$STORAGE_ROOT" ] || return 0; case "$STORAGE_ROOT/bench_$1" in
    */bench_*) rm -rf "$STORAGE_ROOT/bench_$1"; mkdir -p "$STORAGE_ROOT/bench_$1";; esac; }

up_xyzdb(){ # $1=image $2=mem $3=cache
  local img=$1 mem=$2 cache=$3 c=bench-xyzdb; docker rm -f "$c" >/dev/null 2>&1; clean_disk xyzdb
  docker run -d --name "$c" --cpus 2 --memory "$mem" -p 2505:2505 $(mount_arg xyzdb) "$img" \
    --port 2505 --path /data/bench --bind 0.0.0.0 --cache-size "$cache" --nearest-budget-ms 0 >/dev/null 2>&1
  local i=0; while [ $i -lt 120 ]; do nc -z 127.0.0.1 2505 2>/dev/null && return 0; i=$((i+1)); sleep 1; done; return 1; }

up_rival(){ # $1=engine $2=env
  local e=$1 env=$2 mem c=bench-$1; mem=$(env_mem "$env"); docker rm -f "$c" >/dev/null 2>&1; clean_disk "$e"
  case "$e" in
    pgvector) docker run -d --name "$c" --cpus 2 --memory "$mem" --shm-size "$mem" -p 5432:5432 -e POSTGRES_PASSWORD=bench \
                $(mount_arg pgvector) pgvector/pgvector:pg18 \
                -c shared_buffers=$(pg_sb "$env") -c maintenance_work_mem=$(pg_mwm "$env") >/dev/null 2>&1;;
    qdrant)   docker run -d --name "$c" --cpus 2 --memory "$mem" -p 6333:6333 $(mount_arg qdrant) qdrant/qdrant:latest >/dev/null 2>&1;;
    chroma)   docker run -d --name "$c" --cpus 2 --memory "$mem" -p 8000:8000 $(mount_arg chroma) chromadb/chroma:latest >/dev/null 2>&1;;
  esac
  local i=0; while [ $i -lt 90 ]; do
    case "$e" in
      pgvector) "$PY" -c "import psycopg2;psycopg2.connect(host='127.0.0.1',port=5432,user='postgres',password='bench',dbname='postgres').close()" 2>/dev/null && return 0;;
      qdrant)   curl -fsS http://127.0.0.1:6333/readyz >/dev/null 2>&1 && return 0;;
      chroma)   curl -fsS http://127.0.0.1:8000/api/v2/heartbeat >/dev/null 2>&1 && return 0;;
    esac; i=$((i+1)); sleep 1; done; return 1; }

meas(){ "$PY" measure_sizesweep.py --engine "$1" --container "bench-$1" $(disk_args "$1") "${@:2}" --out "$OUT" || echo "  measure nonzero"; }

phase_coverage(){
  # ALL FOUR engines, same corpus (189K real), same sizes, engine-EXCLUSIVE (one container at a
  # time) → the head-to-head + coverage on real disk. xyzDB = AVX2/v3 image (mono, gravity native,
  # exact); rivals = flat one-HNSW-over-189K, each tuned fairly per envelope. Rivals need nothing
  # special for v3 — HNSW is HNSW; the AVX2 work is xyzDB's scorer only.
  echo "[$(date +%H:%M:%S)] === PHASE A: 4-engine head-to-head / coverage, 189K × ladder ($STORAGE) ==="
  for env in $ENVS; do local mem cache; mem=$(env_mem "$env"); cache=$(env_cache "$env")
    for e in xyzdb pgvector qdrant chroma; do
      echo "[$(date +%H:%M:%S)] $env $e: up"
      if [ "$e" = xyzdb ]; then
        up_xyzdb "$AFTER_IMG" "$mem" "$cache" || { echo "  xyzdb no arrancó @$env"; docker rm -f bench-xyzdb >/dev/null 2>&1; continue; }
        meas xyzdb --corpus full --size 200000 --scoped 1 --envelope "$env" --image after-v3 --warmup 0 --repeats 1 --max_queries "$MONO_Q"
        docker rm -f bench-xyzdb >/dev/null 2>&1
      else
        if ! up_rival "$e" "$env"; then echo "  $e no arrancó @$env"; docker rm -f bench-$e >/dev/null 2>&1; continue; fi
        BENCH_PG_MWM=$(pg_mwm "$env") meas "$e" --corpus full --size 200000 --scoped 0 --envelope "$env" --image rival --warmup 0 --repeats 1 --max_queries "$MONO_Q"
        docker rm -f bench-$e >/dev/null 2>&1
      fi
    done
  done; }

phase_beforeafter(){
  # xyzDB before(0.8.13) vs after(0.9-v3), A/B/A/B interleaved rounds → Fase-1 magnitude on real
  # disk (G3 readahead finally observable). pool/5000 (warm p50) + mono-189k (scan-at-scale).
  echo "[$(date +%H:%M:%S)] === PHASE B: xyzDB before/after ×${ROUNDS} rounds (5000 + mono) × ladder ($STORAGE) ==="
  for env in $ENVS; do local mem cache; mem=$(env_mem "$env"); cache=$(env_cache "$env")
    for r in $(seq 1 "$ROUNDS"); do
      for spec in "after $AFTER_IMG" "before $BEFORE_IMG"; do set -- $spec; local lbl=$1 img=$2
        up_xyzdb "$img" "$mem" "$cache" && meas xyzdb --corpus pool --size 5000 --envelope "$env" --image "$lbl" --round "$r" --warmup 1 --repeats 3
        docker rm -f bench-xyzdb >/dev/null 2>&1
        up_xyzdb "$img" "$mem" "$cache" && meas xyzdb --corpus full --size 200000 --envelope "$env" --image "$lbl" --round "$r" --warmup 0 --repeats 1 --max_queries "$MONO_Q"
        docker rm -f bench-xyzdb >/dev/null 2>&1
      done
    done
  done; }

case "$PHASE" in
  coverage)    phase_coverage;;
  beforeafter) phase_beforeafter;;
  all)         phase_coverage; phase_beforeafter;;
esac
echo "[$(date +%H:%M:%S)] AWS $STORAGE DONE -> $OUT"
