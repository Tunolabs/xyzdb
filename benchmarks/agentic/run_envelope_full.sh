#!/usr/bin/env bash
# Full envelope matrix: 4 DEPLOYMENTS × 4 tiers × 3 scales × 6 scenarios (S1,S2,S4,
# S5,S6 per scale; S3 once per tier — corpus-independent). Unlike run_envelope_matrix.sh
# (single engine, memory-only, S1+S3) this measures the HONEST DEPLOYMENT:
#
#   xyz        1 container  (xyzdb does vector + structured)
#   pg         1 container  (pgvector does vector + structured)
#   qdrant+pg  2 containers (qdrant vector + Postgres store on :5433)
#   chroma+pg  2 containers (chroma vector + Postgres store on :5433)
#
# and the COMBINED CPU + memory of all a deployment's containers (cell_watchdog.py),
# peak + avg + per-container breakdown, plus live fall-detection (OOM-kill / crashed /
# OOM-thrash) so a dead cell gets its reason without waiting the full wall. In a
# two-system deployment the vector engine gets the tier envelope and the +pg store gets
# a FIXED lean envelope (1 core / 256M) — "does the stack fit with a minimal second DB";
# combined budget = tier DRAM + 256M. S1/S2/S4/S5 exercise the vector side; the store is
# resident (structured metadata is KB-scale so its RAM ≈ base + shared_buffers). S6 loads
# AND functionally exercises the store (double write, inconsistency window, SQL aggregate).
#
# NO SKIP-CASCADE: every (deployment, tier, scale, scenario) is a REAL test. The watchdog
# makes falls cheap (OOM-kill instant, thrash ~5min), so measuring 100k/246k even after
# 30k fell gives a real per-cell verdict + the fit-gradient (fits-degraded -> thrash ->
# OOM-kill), not an assumed skip. Every cell: bring up -> watchdog(sampler+fall-detect)
# -> run scenario -> merge combined resources + boot_s + n_engines. RESUMABLE: a cell
# whose out-file already holds a completed main record is skipped.
#
# Image stamped per record (premise-20). Mac/OrbStack = DIRECTION, not publishable.
# xyz image per arch (AVX2 x86-v3 on AWS): XYZDB_IMG / XYZ_ARCH — see run_envelope_matrix.sh.
set -uo pipefail
AG="$(cd "$(dirname "$0")" && pwd)"; cd "$AG"; PY="${PY:-$AG/.venv/bin/python}"
# xyzdb minimal client ships in the repo at examples/client/python — no external SDK.
_CLIENTS="$(cd "$AG/../.." 2>/dev/null && pwd)/examples/client/python"
export PYTHONPATH="${PYTHONPATH:-$_CLIENTS}:$AG"
# AWS: STORAGE_ROOT=/mnt/ssd bind-mounts the real block device (data on the 93G SSD, host `du` for
# footprint, wiped between cells — xyzDB has no drop_lobe so the dir MUST start empty). Empty = Mac
# named volumes (docker-managed). STORAGE just labels ssd/hdd in the disk record.
STORAGE_ROOT="${STORAGE_ROOT:-}"; STORAGE="${STORAGE:-ssd}"
export XYZDB_IMG="${XYZDB_IMG:-xyzdb:0.9.6-fixA}"
XYZ_ARCH="${XYZ_ARCH:-arm}"
IMG_XYZ="$XYZDB_IMG"
export BENCH_PG_M=48 BENCH_PG_EFC=200 BENCH_PG_EFS=200 \
       BENCH_QD_M=48 BENCH_QD_EFC=200 BENCH_QD_EF=512 \
       BENCH_CH_M=32 BENCH_CH_CEF=200 BENCH_CH_SEF=512 \
       BUILD_TIMEOUT="${BUILD_TIMEOUT:-900}"
OUT="${OUT:-$AG/results/envelope_full}"; mkdir -p "$OUT"
IMG_PG=pgvector/pgvector:pg18

# tier -> "cpus dram memswap xyzcache pgsb pgmwm"
tier_spec(){ case "$1" in
  T1) echo "1 256m 512m  103 64MB  96MB";;
  T2) echo "2 512m 1024m 200 128MB 192MB";;
  T3) echo "2 2g   2560m 800 512MB 1GB";;
  T4) echo "2 4g   4608m 1600 1GB  1536MB";;
esac; }
dram_mib(){ case "$1" in *g) echo $(( ${1%g} * 1024 ));; *m) echo "${1%m}";; *) echo "$1";; esac; }
scale_wall(){ case "$1" in 30000) echo 420;; 100000) echo 540;; *) echo 660;; esac; }  # per-scale hard wall (backstop; BUILD_TIMEOUT cuts build-thrash first)

# deployment -> "vec_engine  vec_container  vec_port  containers..."  (containers = all to sample)
dep_spec(){ case "$1" in
  xyz)        echo "xyzdb    bench-xyzdb    2505 bench-xyzdb";;
  pg)         echo "pgvector bench-pgvector 5432 bench-pgvector";;
  qdrant+pg)  echo "qdrant   bench-qdrant   6333 bench-qdrant bench-store";;
  chroma+pg)  echo "chroma   bench-chroma   8000 bench-chroma bench-store";;
esac; }
dep_slug(){ echo "$1" | tr -d '+'; }   # qdrant+pg -> qdrantpg (filename-safe)

wait_port(){ local host=127.0.0.1 port=$1 c=$2 e=$3 i=0
  while [ $i -lt 150 ]; do
    case "$e" in
      xyzdb)    nc -z "$host" "$port" 2>/dev/null && { sleep 2; return 0; };;
      pgvector|store) "$PY" -c "import psycopg2;psycopg2.connect(host='$host',port=$port,user='postgres',password='bench',dbname='postgres').close()" 2>/dev/null && return 0;;
      qdrant)   curl -fsS "http://$host:$port/readyz" >/dev/null 2>&1 && return 0;;
      chroma)   curl -fsS "http://$host:$port/api/v2/heartbeat" >/dev/null 2>&1 && return 0;;
    esac
    [ "$(docker inspect -f '{{.State.Running}}' "$c" 2>/dev/null)" = false ] && return 1
    i=$((i+1)); sleep 1; done; return 1; }

# per-engine data dir inside the container
datadir_for(){ case "$1" in xyzdb|chroma) echo /data;; pgvector|store) echo /var/lib/postgresql;; qdrant) echo /qdrant/storage;; esac; }
# prep a data volume -> sets global MNT to the `-v` arg. AWS: bind-mount /mnt/ssd (wiped each cell).
# Mac: docker named volume. Guarded to a bench-only path.
prep_vol(){ local v=$1 dd=$2
  case "$v" in bench_*) : ;; *) echo "FATAL: bad vol '$v'" >&2; return 1;; esac
  if [ -n "$STORAGE_ROOT" ]; then
    case "$STORAGE_ROOT" in ""|/|/mnt) echo "FATAL: unsafe STORAGE_ROOT='$STORAGE_ROOT'" >&2; return 1;; esac
    rm -rf "$STORAGE_ROOT/$v"; mkdir -p "$STORAGE_ROOT/$v"; MNT="-v $STORAGE_ROOT/$v:$dd"
  else
    docker volume rm "$v" >/dev/null 2>&1; docker volume create "$v" >/dev/null; MNT="-v $v:$dd"
  fi; }
# disk arg for the measure scripts: host --disk_path on AWS, --volume on Mac
disk_arg(){ if [ -n "$STORAGE_ROOT" ]; then echo "--disk_path $STORAGE_ROOT/$1 --storage $STORAGE"; else echo "--volume $1"; fi; }
# S6 needs both the vector and the store disk locations
s6_disk_args(){ if [ -n "$STORAGE_ROOT" ]; then echo "--vec_disk_path $STORAGE_ROOT/bench_$1 --store_disk_path $STORAGE_ROOT/bench_store --storage $STORAGE"; else echo "--vec_volume bench_$1 --store_volume bench_store"; fi; }

# bring up a whole deployment with the tier envelope; each container gets tier DRAM (store fixed 256M)
up_deploy(){ local dep=$1 t=$2; read -r cpus dram memswap xyzc pgsb pgmwm <<<"$(tier_spec "$t")"
  local mf="--cpus $cpus --memory $dram --memory-swap $memswap"
  local ve vc vp; read -r ve vc vp _ <<<"$(dep_spec "$dep")"
  for c in bench-xyzdb bench-pgvector bench-qdrant bench-chroma bench-store; do docker rm -f "$c" >/dev/null 2>&1; done
  # vector side (MNT set by prep_vol: bind-mount /mnt/ssd on AWS, named volume on Mac)
  case "$ve" in
    xyzdb)    prep_vol bench_xyzdb /data || return 1
              # 0.9.6: --cache-size deprecated; memory is governed by --memory-budget-mb. The engine's
              # cgroup auto-detect FAILS on this AWS box (assumes 1024MB -> OOM at tight tiers), so we
              # pass the tier DRAM explicitly = the engine self-limits to DRAM (the elastic-RAM moat).
              # --insecure-allow-no-auth: from 1.0 the engine refuses a non-loopback bind without a
              # token; this is a throwaway single-cell container on a private benchmark host, wiped
              # between cells. DO NOT copy this into any real deployment — set --auth-token instead.
              docker run -d --name bench-xyzdb $mf -p 2505:2505 $MNT \
                "$IMG_XYZ" --port 2505 --path /data/bench --bind 0.0.0.0 --insecure-allow-no-auth --memory-budget-mb "$(dram_mib "$dram")" >/dev/null 2>&1;;
    pgvector) prep_vol bench_pgvector /var/lib/postgresql || return 1
              docker run -d --name bench-pgvector $mf -p 5432:5432 -e POSTGRES_PASSWORD=bench $MNT \
                "$IMG_PG" -c shared_buffers=$pgsb -c maintenance_work_mem=$pgmwm >/dev/null 2>&1;;
    qdrant)   prep_vol bench_qdrant /qdrant/storage || return 1
              docker run -d --name bench-qdrant $mf -p 6333:6333 $MNT qdrant/qdrant:latest >/dev/null 2>&1;;
    chroma)   prep_vol bench_chroma /data || return 1
              docker run -d --name bench-chroma $mf -p 8000:8000 $MNT chromadb/chroma:latest >/dev/null 2>&1;;
  esac
  wait_port "$vp" "$vc" "$ve" || return 1
  # structured store (2-system deployments only), on :5433. FIXED minimal envelope —
  # 1 core / 256M (+256 swap) / shared_buffers 64MB — NOT the tier envelope: "does the
  # stack fit with a lean second DB". The combined budget = vector's tier DRAM + 256M.
  case "$dep" in
    qdrant+pg|chroma+pg)
      prep_vol bench_store /var/lib/postgresql || return 1
      docker run -d --name bench-store --cpus 1 --memory 256m --memory-swap 512m -p 5433:5432 \
        -e POSTGRES_PASSWORD=bench $MNT \
        "$IMG_PG" -c shared_buffers=64MB -c maintenance_work_mem=64MB >/dev/null 2>&1
      wait_port 5433 bench-store store || return 1;;
  esac
  return 0; }

down_all(){ for c in bench-xyzdb bench-pgvector bench-qdrant bench-chroma bench-store; do docker rm -f "$c" >/dev/null 2>&1; done; }
oomkilled(){ [ "$(docker inspect -f '{{.State.OOMKilled}}' "$1" 2>/dev/null)" = true ] && echo 1 || echo 0; }

# has this cell already produced a completed main record? (resume)
done_cell(){ local out=$1 sc=$2
  [ -s "$out" ] || return 1
  "$PY" -c "import json,sys
sc='$sc'
try:
    ok=any(json.loads(l).get('kind')==sc for l in open('$out') if l.strip())
except Exception: ok=False
sys.exit(0 if ok else 1)"; }

cell(){ # $1=deployment $2=tier $3=scenario(s1|s2|s3|s4|s5|s6) $4=N
  local dep=$1 t=$2 sc=$3 N=$4
  read -r cpus dram memswap xyzc pgsb pgmwm <<<"$(tier_spec "$t")"
  local -a df; read -r -a df <<<"$(dep_spec "$dep")"   # read -a collapses the multi-space padding
  local ve="${df[0]}" vc="${df[1]}" vp="${df[2]}"; local -a conts=("${df[@]:3}")
  local slug; slug="$(dep_slug "$dep")"
  local arch=""; [ "$dep" = xyz ] && arch="_${XYZ_ARCH}"
  local cfg; case "$ve" in xyzdb) cfg="mbudget$(dram_mib "$dram")";; pgvector) cfg="sb${pgsb}";; *) cfg="default+pg${pgsb}";; esac
  local envlbl="${t}_${cpus}c_${dram}_sw${memswap}_${dep}:${cfg}${arch}"
  local out="$OUT/${t}_${sc}_n${N}_${slug}.jsonl"
  if done_cell "$out" "$sc"; then echo "[$(date +%H:%M:%S)] SKIP (done) $t $sc n=$N $dep"; return; fi
  : > "$out"
  local n_eng=${#conts[@]}
  echo "[$(date +%H:%M:%S)] $t $sc n=$N $dep  (dram=$dram cpus=$cpus engines=$n_eng conts=${conts[*]})"
  local tb; tb=$(date +%s)
  if ! up_deploy "$dep" "$t"; then
    "$PY" -c "import json;open('$out','a').write(json.dumps({'kind':'$sc','deployment':'$dep','tier':'$t','envelope':'$envlbl','base_n':$N,'n_engines':$n_eng,'status':'deployment_did_not_start','verdict':'OOM','phase':'boot'})+chr(10))"
    down_all; echo "  -> OOM (deployment no arranca)"; return; fi
  local boot_s=$(( $(date +%s) - tb ))

  # combined watchdog+sampler over ALL the deployment's containers: live CPU+mem
  # peak/avg AND fall-detection (OOM-kill / crashed / OOM-thrash) with a per-scale
  # wall. On a fall it writes the verdict+reason and kills the containers (which
  # aborts the measure), so the reason lands without waiting the full wall.
  local stop="$out.stop" ress="$out.ress.json" verdict="$out.verdict.json"
  rm -f "$stop" "$ress" "$verdict"
  local dmib wwall; dmib="$(dram_mib "$dram")"; wwall="${CELL_WALL:-$(scale_wall "$N")}"
  local STALL="${WD_STALL:-300}"
  local -a pairs=()   # per-container DRAM cap: store fixed 256M, vector engine = tier DRAM
  for c in "${conts[@]}"; do
    if [ "$c" = bench-store ]; then pairs+=("$c:256"); else pairs+=("$c:$dmib"); fi
  done
  "$PY" cell_watchdog.py "$stop" "$ress" "$verdict" "$STALL" "$wwall" "${pairs[@]}" &
  local rpid=$!
  local WALL=$((wwall + 240)) rc=0   # perl-alarm OUTER backstop (only if the watchdog itself dies)
  case "$sc" in
    s1) local qd=""; [ "$ve" = qdrant ] && qd="--qd_variant scroll"
        perl -e 'alarm shift; exec @ARGV' "$WALL" env "BENCH_PG_MWM=$pgmwm" "$PY" measure_s1.py \
          --engine "$ve" --container "$vc" $(disk_arg "bench_$ve") --envelope "$envlbl" \
          --base_n "$N" --max_queries 30 $qd --out "$out" >"$out.stdout" 2>&1 || rc=$?;;
    s2) perl -e 'alarm shift; exec @ARGV' "$WALL" env "BENCH_PG_MWM=$pgmwm" "$PY" measure_s2.py \
          --engine "$ve" --container "$vc" $(disk_arg "bench_$ve") --envelope "$envlbl" \
          --base_n "$N" --out "$out" >"$out.stdout" 2>&1 || rc=$?;;
    s3) perl -e 'alarm shift; exec @ARGV' "$WALL" env "BENCH_PG_MWM=$pgmwm" "$PY" measure_s3.py \
          --engine "$ve" --container "$vc" --envelope "$envlbl" --steps 10,100,1000 \
          --out "$out" >"$out.stdout" 2>&1 || rc=$?;;
    s4) perl -e 'alarm shift; exec @ARGV' "$WALL" env "BENCH_PG_MWM=$pgmwm" "$PY" measure_s4.py \
          --engine "$ve" --container "$vc" $(disk_arg "bench_$ve") --envelope "$envlbl" \
          --base_n "$N" --out "$out" >"$out.stdout" 2>&1 || rc=$?;;
    s5) perl -e 'alarm shift; exec @ARGV' "$WALL" env "BENCH_PG_MWM=$pgmwm" "$PY" measure_s5.py \
          --engine "$ve" --container "$vc" $(disk_arg "bench_$ve") --envelope "$envlbl" \
          --base_n "$N" --max_queries 30 --out "$out" >"$out.stdout" 2>&1 || rc=$?;;
    s6) perl -e 'alarm shift; exec @ARGV' "$WALL" env "BENCH_PG_MWM=$pgmwm" "$PY" measure_s6.py \
          --deployment "$dep" --container "$vc" --envelope "$envlbl" --base_n "$N" \
          $(s6_disk_args "$ve") --out "$out" >"$out.stdout" 2>&1 || rc=$?;;
  esac

  touch "$stop"; wait "$rpid" 2>/dev/null   # stop + collect the watchdog

  # Fall verdict: the watchdog wrote one (OOM-kill / crashed / OOM-thrash) and already
  # killed the containers. Otherwise a SIGALRM (rc>=142) = the OUTER backstop fired
  # (watchdog died) -> still record it as thrash.
  if [ -s "$verdict" ]; then
    "$PY" - "$out" "$verdict" "$dep" "$t" "$envlbl" "$N" "$sc" <<'PYV'
import json, sys
out, vf, dep, t, env, N, sc = sys.argv[1:8]
v = json.load(open(vf))
open(out, "a").write(json.dumps({
    "kind": sc + "-verdict", "deployment": dep, "tier": t, "envelope": env,
    "base_n": int(N), "verdict": v.get("verdict"), "reason": v.get("reason"),
    "container": v.get("container")}) + "\n")
print("  -> %s (%s)" % (v.get("verdict"), v.get("reason")))
PYV
  elif [ "$rc" -ge 142 ]; then
    "$PY" -c "import json;open('$out','a').write(json.dumps({'kind':'${sc}-verdict','deployment':'$dep','tier':'$t','envelope':'$envlbl','base_n':$N,'verdict':'OOM-thrash','wall_s':$WALL,'note':'outer wall backstop (watchdog died): sustained thrash'})+chr(10))"
    for c in "${conts[@]}"; do docker kill "$c" >/dev/null 2>&1; done
    echo "  -> OOM-thrash (outer wall ${WALL}s)"
  fi
  # merge combined resources into the scenario's main record(s) + append a resources record
  "$PY" - "$out" "$ress" "$sc" "$boot_s" "$n_eng" "$dep" <<'PYEOF'
import json, sys, os
out, ress, sc, boot_s, n_eng, dep = sys.argv[1:7]
r = json.load(open(ress)) if os.path.exists(ress) else {}
lines = [json.loads(l) for l in open(out) if l.strip()]
for rec in lines:
    if rec.get("kind") == sc:
        rec["combined_mem_peak_mb"] = r.get("combined_mem_peak_mb")
        rec["combined_cpu_peak_pct"] = r.get("combined_cpu_peak_pct")
        rec["combined_mem_avg_mb"] = r.get("combined_mem_avg_mb")
        rec["combined_cpu_avg_pct"] = r.get("combined_cpu_avg_pct")
        rec["per_container"] = r.get("per_container")
        rec["boot_s"] = int(boot_s)
        rec["n_engines"] = int(n_eng)
        rec["one_system"] = (int(n_eng) == 1)
        rec["deployment"] = dep
with open(out, "w") as f:
    for rec in lines:
        f.write(json.dumps(rec) + "\n")
    f.write(json.dumps({"kind": sc + "-resources", "boot_s": int(boot_s),
                        "n_engines": int(n_eng), **r}) + "\n")
PYEOF
  local oom=0; for c in "${conts[@]}"; do [ "$(oomkilled "$c")" = 1 ] && oom=1; done
  "$PY" -c "import json;open('$out','a').write(json.dumps({'kind':'$sc-oomcheck','oomkilled_post':$oom})+chr(10))"
  echo "  oom=$oom  $(${PY} -c "import json,os;r=json.load(open('$ress')) if os.path.exists('$ress') else {};print('mem_peak=%s cpu_peak=%s' % (r.get('combined_mem_peak_mb'), r.get('combined_cpu_peak_pct')))" 2>/dev/null)"
  rm -f "$stop" "$ress" "$verdict"; down_all; }

# --- driver ---
TIERS="${TIERS:-T1 T2 T3 T4}"; SCALES="${SCALES:-30000 100000 246738}"
DEPLOYMENTS_RUN="${DEPLOYMENTS_RUN:-xyz pg qdrant+pg chroma+pg}"
SCENARIOS="${SCENARIOS:-s1 s2 s3 s4 s5 s6}"
if [ "${DRY:-0}" = 1 ]; then
  cell "${DRY_DEP:-xyz}" "${DRY_TIER:-T3}" "${DRY_SC:-s1}" "${DRY_N:-30000}"
  echo "DRY DONE"; exit 0; fi
echo "=== ENVELOPE FULL  $(date '+%F %H:%M:%S')  img=$IMG_XYZ ==="
for t in $TIERS; do for sc in $SCENARIOS; do for N in $SCALES; do
  [ "$sc" = s3 ] && { [ "$N" = 30000 ] || continue; }   # S3 corpus-independent -> once per tier
  for dep in $DEPLOYMENTS_RUN; do cell "$dep" "$t" "$sc" "$N"; done
done; done; done
echo "=== FULL MATRIX DONE  $(date '+%F %H:%M:%S') ==="
