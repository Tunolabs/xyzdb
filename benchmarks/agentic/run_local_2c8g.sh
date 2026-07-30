#!/usr/bin/env bash
# Local first-results pass: ALL six scenarios on the 2c8g tier, reduced scale
# (BASE_N / MAXQ), in Docker, sequential + engine-exclusive per scenario. Mac /
# OrbStack = DIRECTION only (page-cache mediated) — not publishable numbers, a
# first look that every scenario runs green and the shape is sane.
#
#   PY=<venv/python> PYTHONPATH=<xyzdb sdk> BASE_N=30000 MAXQ=50 bash run_local_2c8g.sh
set -uo pipefail
AG="$(cd "$(dirname "$0")" && pwd)"; cd "$AG"
export PY="${PY:-$AG/.venv/bin/python}"
: "${PYTHONPATH:?set PYTHONPATH to the xyzdb python SDK dir}"
export BASE_N="${BASE_N:-30000}"; export MAXQ="${MAXQ:-50}"
echo "=== LOCAL 2c8g first-results pass  BASE_N=$BASE_N MAXQ=$MAXQ  $(date '+%F %H:%M:%S') ==="

run(){ echo ""; echo ">>> $1  $(date '+%H:%M:%S')"; shift; "$@" || echo "  (scenario runner exited nonzero — see its jsonl)"; }

run "S1 retrieve-expand" env S1_ENVS=2c8g bash run_s1.sh
run "S5 hybrid"          env S5_ENVS=2c8g bash run_s5.sh
run "S6 one-engine"      env S6_ENV=2c8g  bash run_s6.sh
run "S2 live session"    env S2_ENV=2c8g  bash run_s2.sh
run "S3 fleet"           env S3_ENVS=2c8g bash run_s3.sh
run "S4 TTFQ"            env S4_ENVS=2c8g bash run_s4.sh

echo ""; echo "=== LOCAL PASS DONE  $(date '+%F %H:%M:%S') ==="
echo "--- report ---"
"$PY" report_agentic.py ./results || true
