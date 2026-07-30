#!/bin/bash
# xyzDB image matrix — x86-v3 (AVX2) vs arm, run as an explicit, recorded
# choice. Each cell builds + brings up ONE xyzdb image variant, runs the bench,
# and tears it down before the next (engine-exclusive: never two engine
# containers at once). The chosen variant is baked into the image as
# `org.xyzdb.image-variant` AND passed to native-bench (--engine-image) so the
# report states which architecture produced each run.
#
# The `target-cpu=x86-64-v3` flag is target-scoped to x86_64-unknown-linux-gnu
# (.cargo/config.toml), so it applies to the linux/amd64 build (x86-v3) and is
# inert on linux/arm64 (arm). Bit-identical recall is verified equal across both
# (the v2==v3 gate) — the architecture changes latency, never the result.
#
# Usage:
#   run_xyz_image_matrix.sh [x86-v3|arm|both]   (default: host-native variant)
#
# Cross-building a non-native variant needs buildx + qemu and is slow; prefer
# running each variant on its native box (x86 box → x86-v3, arm box → arm).
#
# Output (per cell): results/xyzdb-<storage>-scale<sc>-<ts>.{json,csv,md}
#                    results/xyz-image-matrix.master.log

set -u  # do NOT -e: surfacing each cell's exit code is the goal

cd "$(dirname "$0")/.."   # benchmarks/native/

SCALE=${SCALE:-0.1}
STORAGE_PROFILE=${STORAGE_PROFILE:-ssd}
DURATION=${DURATION:-3600}
COLD_RUNS=${COLD_RUNS:-100}
# verify_golden anchor. Note the file-name quirk: scale 1.0's golden is
# "scale1", every other scale keeps its literal (0.1 -> "scale0.1").
GOLDEN=${GOLDEN:-golden/golden-scale$([ "$SCALE" = "1.0" ] && echo 1 || echo "$SCALE")-seed42.json}
MASTER_LOG=./results/xyz-image-matrix.master.log
mkdir -p ./results data/xyzdata

host_variant() {
    case "$(uname -m)" in
        x86_64|amd64) echo "x86-v3" ;;
        aarch64|arm64) echo "arm" ;;
        *) echo "x86-v3" ;;  # default to the publish target
    esac
}

platform_for() {
    case "$1" in
        x86-v3) echo "linux/amd64" ;;
        arm)    echo "linux/arm64" ;;
    esac
}

# Resolve the requested variant list.
case "${1:-$(host_variant)}" in
    both) VARIANTS=(x86-v3 arm) ;;
    x86-v3) VARIANTS=(x86-v3) ;;
    arm) VARIANTS=(arm) ;;
    *) echo "usage: $0 [x86-v3|arm|both]"; exit 2 ;;
esac

echo "=== xyzDB image matrix ==="                                  | tee -a "$MASTER_LOG"
echo "=== started: $(date '+%Y-%m-%d %H:%M:%S %z') ==="            | tee -a "$MASTER_LOG"
echo "=== host: $(uname -n) ($(uname -m)) / docker: $(docker --version | awk '{print $3}' | tr -d ',') ===" | tee -a "$MASTER_LOG"
echo "=== variants: ${VARIANTS[*]} ==="                            | tee -a "$MASTER_LOG"

run_cell() {
    local variant="$1"
    local platform
    platform=$(platform_for "$variant")

    echo ""                                                        | tee -a "$MASTER_LOG"
    echo "=== cell: xyzdb / $variant ($platform)  start: $(date '+%H:%M:%S') ===" | tee -a "$MASTER_LOG"

    rm -rf ./data/xyzdata && mkdir -p ./data/xyzdata

    # DOCKER_DEFAULT_PLATFORM drives both build and run; XYZ_IMAGE_VARIANT is
    # baked as the image label and passed to the report. Both are read from the
    # same value, so the recorded arch matches the container that ran.
    export DOCKER_DEFAULT_PLATFORM="$platform"
    export XYZ_IMAGE_VARIANT="$variant"

    STORAGE_PROFILE="$STORAGE_PROFILE" docker compose --profile xyzdb up -d --build 2>&1 \
        | tee -a "$MASTER_LOG"

    echo "waiting for xyzdb to listen on 2505..."                  | tee -a "$MASTER_LOG"
    local i=0
    until nc -z 127.0.0.1 2505 2>/dev/null; do
        i=$((i+1)); [ "$i" -ge 120 ] && { echo "!!! xyzdb never listened"; break; }; sleep 1
    done

    ./target/release/native-bench \
        --engine xyzdb \
        --scale "$SCALE" \
        --storage "$STORAGE_PROFILE" \
        --schema-mode full \
        --duration "$DURATION" \
        --cold-runs "$COLD_RUNS" \
        --golden "$GOLDEN" \
        --engine-image "$variant" \
        --output ./results 2>&1 | tee "./results/xyzdb-${variant}.run.log" >> "$MASTER_LOG"
    local rc=${PIPESTATUS[0]}

    docker compose --profile xyzdb down --remove-orphans 2>&1      | tee -a "$MASTER_LOG"
    rm -rf ./data/xyzdata
    unset DOCKER_DEFAULT_PLATFORM XYZ_IMAGE_VARIANT

    echo "=== cell: xyzdb / $variant  exit=$rc  at $(date '+%H:%M:%S') ===" | tee -a "$MASTER_LOG"
}

for v in "${VARIANTS[@]}"; do
    run_cell "$v"
done

echo ""                                                            | tee -a "$MASTER_LOG"
echo "=== xyzDB image matrix done: $(date '+%Y-%m-%d %H:%M:%S %z') ===" | tee -a "$MASTER_LOG"
