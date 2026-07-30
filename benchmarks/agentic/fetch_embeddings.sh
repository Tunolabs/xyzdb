#!/usr/bin/env bash
# Download the PREBUILT LongMemEval bge-large embeddings so you can skip the slow, GPU-hungry
# local embed step (build_lme.py on CPU is ~2 days). Assets are GitHub release assets
# (unmetered public download), sha256-pinned. This is the default corpus path.
#
# To build the corpus from source instead, set BENCH_BUILD_CORPUS=1 (the runner then uses
# fetch_corpus.sh + build_lme.py; needs torch + sentence-transformers, GPU recommended).
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
DEST="${BENCH_CORP:-$HERE/corpora/lme}"
BASE="https://github.com/Tunolabs/xyzdb-agentic-embeddings/releases/download/v1-lme-bge-large"

sha_of() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}';
  else shasum -a 256 "$1" | awk '{print $1}'; fi
}
sha_for() {
  case "$1" in
    cvec.npy)  echo 02264623434b93147a9236f842d619f0fb05cfa4205b5f6e332901050a53fa7a ;;
    qvec.npy)  echo 547bd823cb6892e4ebaa0f167c47b915bf853eec921e574f692b607dfafa6b58 ;;
    meta.json) echo 8020964230570976ad06ec8839ddecc31bdc8ea81f9d45f80a1c3a058ad2b8c9 ;;
  esac
}

mkdir -p "$DEST"
for f in cvec.npy qvec.npy meta.json; do
  want="$(sha_for "$f")"
  if [ -f "$DEST/$f" ] && [ "$(sha_of "$DEST/$f")" = "$want" ]; then
    echo "ok (already present, verified): $f"; continue
  fi
  echo "downloading $f ..."
  if command -v curl >/dev/null 2>&1; then curl -fL --progress-bar -o "$DEST/$f" "$BASE/$f"
  elif command -v wget >/dev/null 2>&1; then wget -q --show-progress -O "$DEST/$f" "$BASE/$f"
  else echo "need curl or wget on PATH" >&2; exit 1; fi
  got="$(sha_of "$DEST/$f")"
  if [ "$got" != "$want" ]; then
    echo "SHA256 MISMATCH for $f — expected $want, got $got" >&2
    rm -f "$DEST/$f"; exit 1
  fi
done
echo "prebuilt embeddings ready in $DEST"
