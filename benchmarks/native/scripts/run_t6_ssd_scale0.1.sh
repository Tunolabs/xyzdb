#!/bin/bash
# T6 (2C/8G) SSD comparative — xyzDB / PostgreSQL / MongoDB, scale 0.1.
# Sequential + engine-exclusive: per-cell up -> bench -> down (never two engine
# containers at once). Pick the engine set via ENGINES (default all three).
#
# Env knobs (all overridable):
#   ENGINES="xyzdb postgres mongo"   which cells, in order
#   SCALE=0.1  COLD_RUNS=100  DURATION=300
#   DATA_ROOT=./data     engine data dirs (bind-mounted); point at a big mount
#                        when root is tight, e.g. DATA_ROOT=/mnt/ssd/xyz-bench
#   PULL=missing         set PULL=never for a no-Docker-Hub run (images must be
#                        cached: rust:slim-bookworm, distroless/cc, postgres:18,
#                        mongo:7.0). Fails loudly instead of pulling.
#   IMG=<auto>           image/arch label recorded in the report (x86-v3 | arm),
#                        auto-detected from the host arch.
#
# WARNING (mongo): mongo Q4 is the known outlier — a runtime $group over fat
# embedded docs (pre-agg dropped). At scale 0.1 it has run ~7 min/query, so
# COLD_RUNS=100 for the mongo cell is hours. Probe mongo separately with a low
# count, e.g.  ENGINES=mongo COLD_RUNS=3 DURATION=30 bash scripts/run_t6_ssd_scale0.1.sh
#
# Output: results/{engine}-<profile>-scale<SCALE>-<ts>.{json,csv,md} and the
#         master + per-cell logs are named by profile+SCALE:
#         t6-<profile>-scale<SCALE>.master.log and {engine}-t6-<profile>-scale<SCALE>.run.log
#         (profile = ssd|hdd from STORAGE_PROFILE).

set -u  # do NOT -e: surface each cell's exit code

cd "$(dirname "$0")/.."   # benchmarks/native/

export CPUS=2
export MEMORY=8G
# ssd (default) | hdd — tunes the pg config (postgresql-t6-<profile>.conf) and the
# xyzDB engine (--storage-profile) via docker-compose, feeds native-bench
# `--storage`, and names the logs/reports by profile. Point DATA_ROOT at the
# matching mount (e.g. DATA_ROOT=/mnt/hdd/xyz-bench STORAGE_PROFILE=hdd).
export STORAGE_PROFILE=${STORAGE_PROFILE:-ssd}
# warn (not the compose default `info`): the xyzDB router logs plan_scan at INFO
# for every query, which adds per-query I/O and contaminates latency. Bench must
# run quiet. Overridable for debugging.
export RUST_LOG=${RUST_LOG:-warn}
SCALE=${SCALE:-0.1}
COLD_RUNS=${COLD_RUNS:-100}
DURATION=${DURATION:-300}
GOLDEN=${GOLDEN:-golden/golden-scale0.1-seed42.json}
ENGINES=${ENGINES:-"xyzdb postgres mongo"}
DATA_ROOT=${DATA_ROOT:-./data}
export XYZ_DATA="$DATA_ROOT/xyzdata"
export PG_DATA="$DATA_ROOT/pgdata"
export MONGO_DATA="$DATA_ROOT/mongodata"
PULL=${PULL:-missing}
case "$(uname -m)" in
    x86_64|amd64) IMG=${IMG:-x86-v3} ;;
    aarch64|arm64) IMG=${IMG:-arm} ;;
    *) IMG=${IMG:-x86-v3} ;;
esac

MASTER_LOG=./results/t6-${STORAGE_PROFILE}-scale${SCALE}.master.log
mkdir -p ./results

datadir () { case "$1" in xyzdb) echo "$XYZ_DATA";; postgres) echo "$PG_DATA";; mongo) echo "$MONGO_DATA";; esac; }

# Probe xyzDB with a real read-only statement (SHOW GHOSTS): status byte 0x00 =
# ready. `nc -z` only proves the port is open, which raced against the server
# still opening the DB / a restart (restart: unless-stopped) and reset Phase 0.
xyz_ready () {
    python3 -c 'import socket,struct,sys
try:
 s=socket.create_connection(("127.0.0.1",2505),timeout=3)
 p=b"SHOW GHOSTS"; s.sendall(bytes([1])+struct.pack(">I",len(p))+p)
 sys.exit(0 if s.recv(1)==b"\x00" else 1)
except Exception: sys.exit(1)' 2>/dev/null
}

wait_ready () {
    case "$1" in
        xyzdb)
            until nc -z 127.0.0.1 2505 2>/dev/null; do sleep 1; done
            local i=0
            until xyz_ready; do
                i=$((i+1)); [ "$i" -ge 60 ] && { echo "!!! xyzdb not serving after 60 tries" | tee -a "$MASTER_LOG"; break; }
                sleep 1
            done
            ;;
        postgres) until docker exec native-postgres-1 pg_isready -U postgres -d bench 2>/dev/null | grep -q "accepting connections"; do sleep 2; done ;;
        mongo)    until [ "$(docker inspect --format '{{.State.Health.Status}}' native-mongodb-1 2>/dev/null)" = "healthy" ]; do sleep 2; done ;;
    esac
}

run_cell () {
    local eng="$1" dd rc extra=""
    dd=$(datadir "$eng")
    [ "$eng" = "xyzdb" ] && extra="--build"   # only xyzDB is built locally
    echo ""                                                | tee -a "$MASTER_LOG"
    echo "=== cell: $eng  up: $(date '+%H:%M:%S') ==="     | tee -a "$MASTER_LOG"
    rm -rf "$dd" && mkdir -p "$dd"
    XYZ_IMAGE_VARIANT="$IMG" docker compose --profile "$eng" up -d $extra --pull "$PULL" 2>&1 | tee -a "$MASTER_LOG"
    echo "waiting for $eng..."                             | tee -a "$MASTER_LOG"
    wait_ready "$eng"
    ./target/release/native-bench \
        --engine "$eng" \
        --scale "$SCALE" \
        --storage "$STORAGE_PROFILE" \
        --schema-mode full \
        --cold-runs "$COLD_RUNS" \
        --duration "$DURATION" \
        --golden "$GOLDEN" \
        --engine-image "$IMG" \
        --data-path "$dd" \
        --output ./results 2>&1 | tee "./results/${eng}-t6-${STORAGE_PROFILE}-scale${SCALE}.run.log" >> "$MASTER_LOG"
    rc=${PIPESTATUS[0]}
    docker compose --profile "$eng" down --remove-orphans 2>&1 | tee -a "$MASTER_LOG"
    rm -rf "$dd"
    echo "=== cell: $eng  exit=$rc  down: $(date '+%H:%M:%S') ===" | tee -a "$MASTER_LOG"
}

echo "=== T6 (2C/8G) SSD — engines: $ENGINES, scale $SCALE ===" | tee -a "$MASTER_LOG"
echo "=== started $(date '+%Y-%m-%d %H:%M:%S %z') / host $(uname -n) ($(uname -m)) / img=$IMG mongo=${MONGO_IMAGE:-mongo:7.0} data=$DATA_ROOT pull=$PULL ===" | tee -a "$MASTER_LOG"
echo "=== cold-runs=$COLD_RUNS concurrent=${DURATION}s CPUS=$CPUS MEMORY=$MEMORY ===" | tee -a "$MASTER_LOG"

for e in $ENGINES; do
    run_cell "$e"
done

echo ""                                                    | tee -a "$MASTER_LOG"
echo "=== done: $(date '+%Y-%m-%d %H:%M:%S %z') ==="       | tee -a "$MASTER_LOG"
