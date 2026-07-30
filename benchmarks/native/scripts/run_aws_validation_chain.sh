#!/bin/bash
# Enriched AWS validation chain: verify-golden gate + SSD→HDD sequential pass.
#
# Two enrichments over a bare `run_aws_4engines.sh` invocation, both general
# (they benefit every engine, not just xyzdb):
#
#   1. Generates the verify-golden file the orchestrator looks for
#      (`results/golden-scale<X>-seed<Y>.json`). When that file is present the
#      orchestrator runs Phase 1.5 verify_golden — the V1-V6 data-integrity
#      gate (counts + sums by lobe/_type, distinct rfc, config catalogue)
#      computed straight from the deterministic generator. Prior box runs
#      skipped it ("no golden file"). The golden is engine-agnostic: it gates
#      Postgres and Mongo cells the same way.
#
#   2. Runs the SSD campaign, then the HDD campaign, sequentially — engines
#      stay strictly serial per the T6 envelope rule
#      (feedback_bench_engines_serial.md). Lets the whole pass run overnight.
#
# This script does NOT modify run_aws_4engines.sh; it only generates the golden
# and calls the existing runner twice. Per-phase RAM/CPU is already in each
# cell's `.md` report (the per-phase peak table) — look there to attribute the
# Phase 0.5 post-load cost, not the master.log.
#
# Defaults (override via env):
#   SCALE=1.0  SEED=42  DURATION=3600  COLD_RUNS=100  ENGINES=xyzdb
#   STORAGES="ssd hdd"   (space-separated; order is honoured)
#
# Example:
#   bash scripts/run_aws_validation_chain.sh                 # xyzdb, SSD then HDD, golden on
#   STORAGES=ssd bash scripts/run_aws_validation_chain.sh
#
# Prereqs (same as run_aws_4engines.sh): after `git pull`, rebuild BOTH the
# host binaries (`cargo build --release` → native-bench + golden_dump) AND the
# xyzdb docker image (`docker compose build xyzdb`). This script builds the
# host binaries it needs; the docker image is the operator's responsibility.

set -u
cd "$(dirname "$0")/.."   # benchmarks/native/

SCALE="${SCALE:-1.0}"
SEED="${SEED:-42}"
DURATION="${DURATION:-3600}"
COLD_RUNS="${COLD_RUNS:-100}"
ENGINES="${ENGINES:-xyzdb}"
STORAGES="${STORAGES:-ssd hdd}"

mkdir -p ./results

# The orchestrator resolves the golden path as
# `golden-scale{scale}-seed{seed}.json` where {scale} is the f64 Display of
# --scale: 1.0 → "1", 0.1 → "0.1". Mirror that here so the file we generate is
# the one it loads.
scale_tag() {
    case "$1" in
        1.0|1)   echo "1" ;;
        *)       echo "${1%.0}" ;;  # strip a trailing .0; 0.1 stays 0.1
    esac
}
GOLDEN_TAG="$(scale_tag "$SCALE")"
GOLDEN_FILE="./results/golden-scale${GOLDEN_TAG}-seed${SEED}.json"

echo "=== 0.7.x validation chain ==="
echo "=== scale=$SCALE seed=$SEED engines='$ENGINES' storages='$STORAGES' ==="

# ── 1. Verify-golden file ────────────────────────────────────────────────
if [ -f "$GOLDEN_FILE" ]; then
    echo "=== golden present: $GOLDEN_FILE (Phase 1.5 will run) ==="
else
    echo "=== generating golden ($GOLDEN_FILE) ==="
    # Prefer the prebuilt binary; fall back to cargo run.
    if [ -x ./target/release/golden_dump ]; then
        ./target/release/golden_dump --scale "$SCALE" --seed "$SEED" --out-dir ./results
    else
        cargo run --release -p native-generator --bin golden_dump -- \
            --scale "$SCALE" --seed "$SEED" --out-dir ./results
    fi
    # Be robust to golden_dump's own filename formatting: if it wrote a
    # differently-tagged name (e.g. scale1.0), put a copy at the exact path the
    # orchestrator resolves, so Phase 1.5 is guaranteed to find it.
    if [ ! -f "$GOLDEN_FILE" ]; then
        newest="$(ls -t ./results/golden-scale*-seed${SEED}.json 2>/dev/null | head -1)"
        if [ -n "${newest:-}" ]; then
            echo "=== golden written as $newest → copying to $GOLDEN_FILE ==="
            cp "$newest" "$GOLDEN_FILE"
        else
            echo "!!! golden_dump produced no golden-scale*-seed${SEED}.json — Phase 1.5 will skip" >&2
        fi
    fi
fi

# ── 2. SSD → HDD sequential campaigns ────────────────────────────────────
overall=0
for storage in $STORAGES; do
    echo ""
    echo "############################################################"
    echo "### storage cell group: $storage  ($(date '+%Y-%m-%d %H:%M:%S %z'))"
    echo "############################################################"
    STORAGE="$storage" SCALE="$SCALE" DURATION="$DURATION" COLD_RUNS="$COLD_RUNS" \
        ENGINES="$ENGINES" \
        bash scripts/run_aws_4engines.sh
    rc=$?
    echo "### $storage group exit=$rc"
    [ "$rc" -ne 0 ] && overall=1
done

echo ""
echo "=== validation chain done: overall=$overall ($(date '+%Y-%m-%d %H:%M:%S %z')) ==="
echo "=== per-storage verdicts: results/aws_<storage>.summary.md ==="
echo "=== Phase 1.5 verify_golden + per-phase resources: in each cell's .md ==="
exit "$overall"
