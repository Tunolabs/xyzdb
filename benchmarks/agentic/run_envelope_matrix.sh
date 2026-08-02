#!/usr/bin/env bash
# Envelope capacity matrix: 4 tiers × scales × engines × {S1,S3}. Does each engine's
# WORKING PEAK fit the tier's DRAM (fits) / survive on swap degraded (fits-degraded) /
# die (OOM)? Judged on the PEAK (PeakSampler, measure_x.py), not the mean.
#
# Tiers — docker: --cpus + --memory (=DRAM) + --memory-swap (=DRAM+SWAP; == --memory => no swap).
#   tier  cpus  DRAM   swap    --memory-swap   xyz --cache-size (~40-50% DRAM)
#   T1    1     256m   256m    512m            103   (measured working-set, saturates there)
#   T2    2     512m   512m    1024m           200
#   T3    2     2g     512m    2560m           800
#   T4    2     4g     512m    4608m            1600
#
# "Proporcional" = each engine in its best config for the envelope, DECLARED per cell
# (silent config between cells = the Q8 sin). xyz block-cache is elastic/compressible;
# pg shared_buffers ~25% DRAM + maintenance_work_mem scaled; qdrant/chroma default (a
# resident HNSW that does not fit the DRAM => OOM is a RESULT, its architecture vs the
# envelope, not a handicap). Config is encoded into the record's `envelope` string.
# Engine-exclusive. Image stamped in every record (premise-20).
#
# xyz IMAGE PER ARCH (AVX2). The Dockerfile applies `target-cpu=x86-64-v3` (AVX2) only
# on amd64 builds. Mac (arm64) builds are the arm baseline. So:
#   Mac (arm64, direction): XYZDB_IMG=xyzdb:0.9.6-fixA          (default here)
#   AWS  (amd64, publishable): build x86-v3 ON the x86 box, then point XYZDB_IMG at it:
#       docker build --build-arg XYZ_IMAGE_VARIANT=x86-v3 -t xyzdb:0.9.6-fixA-x86v3 .
#       XYZDB_IMG=xyzdb:0.9.6-fixA-x86v3 XYZ_ARCH=x86-v3 bash run_envelope_matrix.sh
#   XYZ_ARCH just labels the arch in each record; XYZDB_IMG selects the actual image.
# Rival images: single pinned source (see images.env). require_pinned_images is
# the negative control — this runner dies if it is not sourced or if a moving
# tag creeps back in, instead of silently resolving `:latest`.
. "$(cd "$(dirname "$0")" && pwd)/images.env"
require_pinned_images || exit 1

set -uo pipefail
AG="$(cd "$(dirname "$0")" && pwd)"; cd "$AG"; PY="${PY:-$AG/.venv/bin/python}"
export PYTHONPATH="${PYTHONPATH:-$(cd "$AG/../.." && pwd)/examples/client/python}:$AG"
export XYZDB_IMG="${XYZDB_IMG:-xyzdb:0.9.6-fixA}"
XYZ_ARCH="${XYZ_ARCH:-arm}"   # 'x86-v3' on AWS (AVX2), 'arm' on Mac. Recorded per cell.
IMG_XYZ="$XYZDB_IMG"
export BENCH_PG_M=48 BENCH_PG_EFC=200 BENCH_PG_EFS=200 \
       BENCH_QD_M=48 BENCH_QD_EFC=200 BENCH_QD_EF=512 \
       BENCH_CH_M=32 BENCH_CH_CEF=200 BENCH_CH_SEF=512 \
       BUILD_TIMEOUT="${BUILD_TIMEOUT:-900}"
OUT="${OUT:-$AG/results/envelope_matrix}"; mkdir -p "$OUT"

# tier -> "cpus dram memswap xyzcache pgsb pgmwm"
tier_spec(){ case "$1" in
  T1) echo "1 256m 512m  103 64MB  96MB";;
  T2) echo "2 512m 1024m 200 128MB 192MB";;
  T3) echo "2 2g   2560m 800 512MB 1GB";;
  T4) echo "2 4g   4608m 1600 1GB  1536MB";;
esac; }

up(){ # $1=engine $2=tier ; sets container bench-<engine> with tier envelope + per-engine config
  local e=$1 t=$2 c=bench-$1; read -r cpus dram memswap xyzc pgsb pgmwm <<<"$(tier_spec "$t")"
  docker rm -f "$c" >/dev/null 2>&1; docker volume rm "bench_$e" >/dev/null 2>&1; docker volume create "bench_$e" >/dev/null
  local mf="--cpus $cpus --memory $dram --memory-swap $memswap"
  case "$e" in
    xyzdb)    docker run -d --name "$c" $mf -p 2505:2505 -v "bench_$e:/data" \
                "$IMG_XYZ" --port 2505 --path /data/bench --bind 0.0.0.0 --cache-size "$xyzc" >/dev/null 2>&1;;
    pgvector) docker run -d --name "$c" $mf -p 5432:5432 -e POSTGRES_PASSWORD=bench -v "bench_$e:/var/lib/postgresql" \
                "$IMG_PG" -c shared_buffers=$pgsb -c maintenance_work_mem=$pgmwm >/dev/null 2>&1;;
    qdrant)   docker run -d --name "$c" $mf -p 6333:6333 -v "bench_$e:/qdrant/storage" "$IMG_QDRANT" >/dev/null 2>&1;;
    chroma)   docker run -d --name "$c" $mf -p 8000:8000 -v "bench_$e:/data" "$IMG_CHROMA" >/dev/null 2>&1;;
  esac
  local i=0; while [ $i -lt 120 ]; do
    case "$e" in
      xyzdb)    nc -z 127.0.0.1 2505 2>/dev/null && { sleep 2; return 0; };;
      pgvector) "$PY" -c "import psycopg2;psycopg2.connect(host='127.0.0.1',port=5432,user='postgres',password='bench',dbname='postgres').close()" 2>/dev/null && return 0;;
      qdrant)   curl -fsS http://127.0.0.1:6333/readyz >/dev/null 2>&1 && return 0;;
      chroma)   curl -fsS http://127.0.0.1:8000/api/v2/heartbeat >/dev/null 2>&1 && return 0;;
    esac
    [ "$(docker inspect -f '{{.State.Running}}' "$c" 2>/dev/null)" = false ] && return 1
    i=$((i+1)); sleep 1; done; return 1; }

oomkilled(){ [ "$(docker inspect -f '{{.State.OOMKilled}}' "bench-$1" 2>/dev/null)" = true ] && echo 1 || echo 0; }

cell(){ # $1=engine $2=tier $3=scenario(s1|s3) $4=N
  local e=$1 t=$2 sc=$3 N=$4; read -r cpus dram memswap xyzc pgsb pgmwm <<<"$(tier_spec "$t")"
  # config declared in the envelope string -> lands in every record
  local cfg; case "$e" in
    xyzdb) cfg="cache${xyzc}";; pgvector) cfg="sb${pgsb}_mwm${pgmwm}";; *) cfg="default";; esac
  local arch=""; [ "$e" = xyzdb ] && arch="_${XYZ_ARCH}"
  local envlbl="${t}_${cpus}c_${dram}_sw${memswap}_${e}:${cfg}${arch}"
  local out="$OUT/${t}_${sc}_n${N}_${e}.jsonl"; : > "$out"
  echo "[$(date +%H:%M:%S)] $t $sc n=$N $e  (dram=$dram swap=$memswap cpus=$cpus cfg=$cfg)"
  if ! up "$e" "$t"; then
    "$PY" -c "import json;open('$out','a').write(json.dumps({'kind':'$sc','engine':'$e','tier':'$t','envelope':'$envlbl','base_n':$N,'status':'container_did_not_start','oomkilled':$(oomkilled "$e"),'verdict':'OOM','phase':'boot'})+chr(10))"
    docker rm -f "bench-$e" >/dev/null 2>&1; echo "  -> OOM (base no arranca)"; return; fi
  # HARD per-cell wall-timeout. Lesson: tight envelope + swap => SLOW THRASH, not fast
  # OOM. BUILD_TIMEOUT (SIGALRM inside the S1 build) does NOT cover the S3 fleet loop,
  # which hung chroma@T1 for 5h. A cell past WALL = OOM-thrash (a RESULT: "did not
  # complete"). `timeout` is missing on this Mac -> perl alarm. SIGALRM => exit 142.
  local WALL="${CELL_WALL:-1200}" rc=0
  if [ "$sc" = s1 ]; then
    local qd=""; [ "$e" = qdrant ] && qd="--qd_variant scroll"
    perl -e 'alarm shift; exec @ARGV' "$WALL" env "BENCH_PG_MWM=$pgmwm" "$PY" measure_s1.py \
      --engine "$e" --container "bench-$e" --volume "bench_$e" --envelope "$envlbl" \
      --base_n "$N" --max_queries 30 $qd --out "$out" >"$out.stdout" 2>&1 || rc=$?
  else
    perl -e 'alarm shift; exec @ARGV' "$WALL" env "BENCH_PG_MWM=$pgmwm" "$PY" measure_s3.py \
      --engine "$e" --container "bench-$e" --envelope "$envlbl" --steps 10,100,1000 \
      --out "$out" >"$out.stdout" 2>&1 || rc=$?
  fi
  if [ "$rc" -ge 142 ]; then
    # SIGALRM: the cell exceeded WALL. The CAUSE is read, not assumed.
    #
    # This used to record `OOM-thrash` and "sustained swap thrash" unconditionally,
    # because that was the failure it was written for (a tight tier plus swap grinds
    # instead of dying). But the adjusted grid has cells that are slow by ARITHMETIC:
    # pool x cat2 scores ~123k vectors exactly, ~500 MB of vector column per query,
    # with the airbag off. If one of those exceeds the wall, calling it thrash names
    # the wrong cause and reads as an engine failure.
    #
    # So the verdict distinguishes what the container reports (OOM-killed, or a
    # memory limit low enough for the working set to thrash) from a cell that simply
    # did not finish in time. Both are recorded — never a gap — but they are not the
    # same finding.
    local killed; killed=$(oomkilled "$e")
    local verdict note
    if [ "$killed" = "true" ]; then
      verdict="OOM-thrash"; note="exceeded wall-timeout and the container was OOM-killed"
    else
      verdict="wall-timeout"
      note="exceeded the ${WALL}s wall without completing; container not OOM-killed, so the cause is NOT established as memory — an exact scan of a large bounded set can legitimately exceed this wall"
    fi
    "$PY" -c "import json;open('$out','a').write(json.dumps({'kind':'${sc}-verdict','engine':'$e','tier':'$t','envelope':'$envlbl','base_n':$N,'verdict':'$verdict','wall_s':$WALL,'oomkilled':$killed,'note':'$note'})+chr(10))"
    docker kill "bench-$e" >/dev/null 2>&1; echo "  -> $verdict (>${WALL}s wall)"
  fi
  local ko=$(oomkilled "$e"); echo "  oomkilled(post)=$ko"
  "$PY" -c "import json;open('$out','a').write(json.dumps({'kind':'$sc-oomcheck','engine':'$e','tier':'$t','oomkilled_post':$ko})+chr(10))"
  docker rm -f "bench-$e" >/dev/null 2>&1; }

# --- driver ---
TIERS="${TIERS:-T1 T2 T3 T4}"; SCALES="${SCALES:-30000 100000 246738}"
ENGINES="${ENGINES:-xyzdb pgvector qdrant chroma}"; SCENARIOS="${SCENARIOS:-s1 s3}"
if [ "${DRY:-0}" = 1 ]; then                 # dry-run: exactly one cell (cost probe)
  cell "${DRY_ENGINE:-xyzdb}" "${DRY_TIER:-T1}" "${DRY_SC:-s1}" "${DRY_N:-246738}"
  echo "DRY DONE"; exit 0; fi
echo "=== ENVELOPE MATRIX  $(date '+%F %H:%M:%S')  img=$IMG_XYZ ==="
for t in $TIERS; do for sc in $SCENARIOS; do for N in $SCALES; do
  [ "$sc" = s3 ] && { [ "$N" = 30000 ] || continue; }   # S3 is corpus-independent -> run once per tier
  for e in $ENGINES; do cell "$e" "$t" "$sc" "$N"; done
done; done; done
echo "=== MATRIX DONE  $(date '+%F %H:%M:%S') ==="
