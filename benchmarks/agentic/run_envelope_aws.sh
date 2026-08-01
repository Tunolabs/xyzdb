#!/usr/bin/env bash
# AWS m6a.xlarge (x86-64-v3 / AVX2) ONE-COMMAND launcher for the full envelope matrix on
# the real SSD. Bind-mounts /mnt/ssd so per-cell data does not fill the ~14 GB root disk.
#
# SELF-PROVISIONING. It checks every prerequisite and, if one is missing, sets it up for
# you before running — then launches the matrix:
#   • Python venv + runtime deps        (created + pip-installed if absent)
#   • LongMemEval corpus                 (prebuilt embeddings downloaded if corpora/lme/ is
#                                         absent; BENCH_BUILD_CORPUS=1 builds from source)
#   • xyzDB engine image                 (docker build from this checkout if absent)
#   • pgvector / qdrant / chroma images  (docker pull if absent)
# Re-running is cheap: anything already in place is detected and skipped. Force a fresh
# engine image after a `git pull` with REBUILD_IMG=1.
#
# The one thing it cannot create for you is the SSD mount: /mnt/ssd must already be a
# mounted, writable block device — that is the reason to run on the box at all.
#
# Run:  cd ~/xyzdb/benchmarks/agentic && ./run_envelope_aws.sh
# Overrides: XYZDB_IMG, REBUILD_IMG, TIERS, SCALES, DEPLOYMENTS_RUN, SCENARIOS,
#            BUILD_TIMEOUT, WD_STALL, OUT.
# Rival images: single pinned source (see images.env). require_pinned_images is
# the negative control — this runner dies if it is not sourced or if a moving
# tag creeps back in, instead of silently resolving `:latest`.
. "$(cd "$(dirname "$0")" && pwd)/images.env"
require_pinned_images || exit 1

set -uo pipefail
AG="$(cd "$(dirname "$0")" && pwd)"; cd "$AG"
REPO="$(cd "$AG/../.." && pwd)"
PY="$AG/.venv/bin/python"

export STORAGE_ROOT="${STORAGE_ROOT:-/mnt/ssd}" STORAGE="${STORAGE:-ssd}"
export XYZ_ARCH="${XYZ_ARCH:-x86-v3}"
export XYZDB_IMG="${XYZDB_IMG:-xyzdb:0.9.8-x86v3}"
export BUILD_TIMEOUT="${BUILD_TIMEOUT:-300}"   # raise (e.g. 1200) for a definitive pg-246k build verdict

say(){ echo "[provision] $*"; }
fail(){ echo "FATAL: $1" >&2; exit 1; }

command -v docker >/dev/null 2>&1 || fail "docker not found on PATH"

# --- SSD mount (cannot be auto-created) ---
case "$STORAGE_ROOT" in /mnt/ssd|/mnt/hdd) : ;; *) fail "STORAGE_ROOT must be /mnt/ssd or /mnt/hdd (got '$STORAGE_ROOT')";; esac
[ -d "$STORAGE_ROOT" ] || fail "$STORAGE_ROOT does not exist — mount the block device first"
touch "$STORAGE_ROOT/.wtest" 2>/dev/null && rm -f "$STORAGE_ROOT/.wtest" || fail "$STORAGE_ROOT is not writable"
avail=$(df -PBG "$STORAGE_ROOT" 2>/dev/null | awk 'NR==2{gsub("G","",$4);print $4}')
[ "${avail:-0}" -ge 40 ] || say "WARN: only ${avail}G free on $STORAGE_ROOT (pg 246k ≈ 28G/cell)"

# --- swap: the tight tiers need it. docker's --memory-swap is a SILENT no-op without a host
#     swapfile, so T1/T2 at 246k OOM during load. We warn (not create — swap is a host change). ---
if ! swapon --show --noheadings 2>/dev/null | grep -q .; then
  say "WARN: no swap on this host — the tight tiers (T1/T2) at 246k will OOM during load."
  say "      docker --memory-swap is a no-op without host swap. To enable 4 GB on the SSD:"
  say "        fallocate -l 4G $STORAGE_ROOT/swapfile && chmod 600 $STORAGE_ROOT/swapfile && mkswap $STORAGE_ROOT/swapfile && swapon $STORAGE_ROOT/swapfile"
fi

# --- Python venv + runtime deps ---
if [ ! -x "$PY" ]; then
  say "creating venv at .venv"
  python3 -m venv "$AG/.venv" || fail "could not create venv (install it: sudo apt-get install -y python3-venv)"
  "$PY" -m pip install -q --upgrade pip
  say "installing runtime deps (requirements.txt)"
  "$PY" -m pip install -q -r "$AG/requirements.txt" || fail "pip install -r requirements.txt failed"
fi

# --- Corpus: prebuilt download by default; build from source with BENCH_BUILD_CORPUS=1 ---
if [ ! -f "$AG/corpora/lme/cvec.npy" ]; then
  if [ "${BENCH_BUILD_CORPUS:-0}" = 1 ]; then
    say "building the corpus from source (BENCH_BUILD_CORPUS=1)"
    say "installing corpus-build deps (requirements-corpus.txt: torch + sentence-transformers, heavy)"
    "$PY" -m pip install -q -r "$AG/requirements-corpus.txt" || fail "pip install -r requirements-corpus.txt failed"
    say "downloading LongMemEval-S (fetch_corpus.sh, sha256-pinned)"
    bash "$AG/fetch_corpus.sh" || fail "fetch_corpus.sh failed"
    say "embedding turns with BAAI/bge-large-en-v1.5 (build_lme.py) — slow on CPU (~2 days), GPU recommended"
    "$PY" "$AG/build_lme.py" || fail "build_lme.py failed"
  else
    say "downloading prebuilt embeddings (set BENCH_BUILD_CORPUS=1 to build from source instead)"
    bash "$AG/fetch_embeddings.sh" || fail "fetch_embeddings.sh failed"
  fi
fi
[ -f "$AG/corpora/lme/cvec.npy" ] || fail "corpus still missing after provisioning"

# --- in-repo minimal client must be importable (it ships in the checkout, no install) ---
PYTHONPATH="$REPO/examples/client/python" "$PY" -c "import xyzdb_minimal" 2>/dev/null \
  || fail "cannot import the in-repo minimal client (examples/client/python/xyzdb_minimal.py)"

# --- xyzDB engine image: build from this checkout if absent (or if REBUILD_IMG=1) ---
if [ "${REBUILD_IMG:-0}" = 1 ] || ! docker image inspect "$XYZDB_IMG" >/dev/null 2>&1; then
  say "building engine image $XYZDB_IMG from $REPO (x86-v3 / AVX2)"
  docker build -t "$XYZDB_IMG" --build-arg XYZ_IMAGE_VARIANT=x86-v3 "$REPO" || fail "docker build of $XYZDB_IMG failed"
fi

# --- rival images: pull if absent ---
for img in "$IMG_PG" "$IMG_QDRANT" "$IMG_CHROMA"; do
  docker image inspect "$img" >/dev/null 2>&1 || { say "pulling $img"; docker pull "$img" || fail "docker pull $img failed"; }
done

echo "=== ENVELOPE AWS  img=$XYZDB_IMG  arch=$XYZ_ARCH  storage=$STORAGE_ROOT  build_timeout=${BUILD_TIMEOUT}s  $(date '+%F %H:%M:%S') ==="
docker rm -f bench-xyzdb bench-pgvector bench-qdrant bench-chroma bench-store >/dev/null 2>&1
exec bash run_envelope_full.sh
