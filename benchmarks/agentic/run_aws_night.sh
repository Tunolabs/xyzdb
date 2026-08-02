#!/usr/bin/env bash
# Overnight UNATTENDED run on the AWS m6a box — SSD ONLY, all engines, then STOP.
# HDD is a SEPARATE later run (STORAGE=hdd STORAGE_ROOT=/mnt/hdd ./run_aws.sh) — NOT done here.
#
# Launch and walk away:
#     nohup ./run_aws_night.sh > night.out 2>&1 &
#
# What it does, in order (each engine sequential / exclusive, same 189K real corpus):
#   0. Preflight — abort BEFORE the night if SSD/corpus/images/venv aren't ready (don't waste hours).
#   1. v3-verify (gate) — logged; the run CONTINUES either way, but a RED is shouted in the summary.
#   2. PHASE A — 4-engine head-to-head + coverage (xyzDB AVX2/v3 + pg/qdrant/chroma), 189K, ladder.
#   3. PHASE B — xyzDB before/after ×3 A/B/A/B rounds (Fase-1 magnitude + G3 on real SSD).
#   4. Summary (serves/OOM per engine, v3-verify verdict) + DONE sentinel. STOPS (no HDD).
#
# xyzDB runs the AVX2 (x86-64-v3) image — that is what Fase-1 built. Rivals need nothing special
# (HNSW is HNSW; the AVX2 work is xyzDB's scorer only).
set -uo pipefail
AG="$(cd "$(dirname "$0")" && pwd)"; cd "$AG"; PY="$AG/.venv/bin/python"
export STORAGE=ssd STORAGE_ROOT="${STORAGE_ROOT:-/mnt/ssd}"
export AFTER_IMG="${AFTER_IMG:-xyzdb:0.9-v3-x86}" BEFORE_IMG="${BEFORE_IMG:-xyzdb:0.8.13-before-x86}"
export ROUNDS="${ROUNDS:-3}" MONO_Q="${MONO_Q:-50}"
# pgvector's tuned HNSW build (efc200/m48 over 189K) takes >600s at 8G on this box → give the
# build 30min so roomy envelopes COMPLETE; tight ones still hit it = a legit "unviable" result.
export BUILD_TIMEOUT="${BUILD_TIMEOUT:-1800}"
LOG="$AG/results/night_ssd.log"; mkdir -p "$AG/results"; : > "$LOG"
say(){ echo "[$(date '+%F %T')] $*" | tee -a "$LOG"; }

say "=== 0. PREFLIGHT ==="
fail=0
mkdir -p "$STORAGE_ROOT/.wtest" 2>/dev/null && rmdir "$STORAGE_ROOT/.wtest" 2>/dev/null || { say "  ✗ $STORAGE_ROOT no escribible"; fail=1; }
[ -f "$AG/corpora/lme/cvec.npy" ] || { say "  ✗ falta corpora/lme/cvec.npy (rsync desde Mac)"; fail=1; }
[ -x "$PY" ] || { say "  ✗ falta .venv (python)"; fail=1; }
for img in "$AFTER_IMG" "$BEFORE_IMG" pgvector/pgvector:pg18 qdrant/qdrant:latest chromadb/chroma:latest; do
  docker image inspect "$img" >/dev/null 2>&1 || { say "  ✗ falta imagen docker: $img"; fail=1; }
done
if [ "$fail" = 1 ]; then say "PREFLIGHT FALLÓ — corrige y relanza. No arranco la noche."; exit 1; fi
say "  ✓ preflight OK (ssd=$STORAGE_ROOT · after=$AFTER_IMG · before=$BEFORE_IMG · rounds=$ROUNDS)"

say "=== 1. v3-verify (gate de garantía; RED = FMA colada = bug) ==="
V3=RED
if ./v3_verify.sh >>"$LOG" 2>&1; then
  # v3_verify uses set -e: exit 0 = both bit-identity suites green
  V3=GREEN; say "  ✓ v3-verify GREEN (bits v2==v3, fingerprint OK)"
else
  say "  ✗ v3-verify NO-verde — revisar $LOG. Continúo la noche pero los números 'after' quedan EN DUDA hasta resolverlo."
fi

say "=== 2+3. run_aws.sh (PHASE A head-to-head + PHASE B before/after) — SSD ==="
PHASE=all ./run_aws.sh 2>&1 | tee -a "$LOG"

say "=== 4. SUMMARY ==="
"$PY" - "$AG/results/aws_ssd.jsonl" "$V3" <<'PY' 2>&1 | tee -a "$LOG"
import json,sys
path,v3=sys.argv[1],sys.argv[2]
try: rows=[json.loads(l) for l in open(path) if l.strip()]
except FileNotFoundError: rows=[]
serves=sum(1 for r in rows if r.get("serves"))
oom=[r for r in rows if r.get("serves") is False]
print(f"  celdas: {len(rows)} · sirven: {serves} · no-sirven: {len(oom)}")
from collections import Counter
c=Counter((r["engine"],r.get("status")) for r in oom)
for (e,st),n in sorted(c.items()): print(f"    OOM/fail: {e} × {st} = {n}")
print(f"  v3-verify: {v3}" + ("  <-- ATENCIÓN: after en duda" if v3!="GREEN" else ""))
print(f"  resultados: {path}")
PY
say "=== NIGHT SSD DONE. HDD es una corrida aparte: STORAGE=hdd STORAGE_ROOT=/mnt/hdd ./run_aws.sh ==="
