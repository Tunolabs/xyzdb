#!/usr/bin/env bash
# Fetch the LongMemEval-S (cleaned) corpus that the agentic benchmark embeds.
#
# Source (official upstream release — NOT vendored or re-hosted here):
#   dataset : https://huggingface.co/datasets/xiaowu0162/longmemeval-cleaned
#   repo    : https://github.com/xiaowu0162/LongMemEval
#   paper   : Di Wu et al., "LongMemEval: Benchmarking Chat Assistants on Long-Term
#             Interactive Memory", ICLR 2025 — arXiv:2410.10813
#
# Variant: LongMemEval_S ("cleaned" = upstream removes noisy history sessions). This is
# the exact file build_lme.py consumes. The sha256 is pinned so a silent upstream change
# fails loudly instead of quietly tainting the numbers. Do not commit the corpus (it is
# gitignored) and do not re-host it.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
DEST="${LME_SRC:-$HERE/data/longmemeval_s_cleaned.json}"
URL="https://huggingface.co/datasets/xiaowu0162/longmemeval-cleaned/resolve/main/longmemeval_s_cleaned.json"
# Git LFS oid of longmemeval_s_cleaned.json (277,383,467 bytes).
SHA256="d6f21ea9d60a0d56f34a05b609c79c88a451d2ae03597821ea3d5a9678c3a442"

sha_of() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}';
  else shasum -a 256 "$1" | awk '{print $1}'; fi
}

mkdir -p "$(dirname "$DEST")"

if [ -f "$DEST" ] && [ "$(sha_of "$DEST")" = "$SHA256" ]; then
  echo "corpus already present and verified: $DEST"
  exit 0
fi

echo "downloading LongMemEval-S (cleaned), ~277 MB ..."
if command -v curl >/dev/null 2>&1; then
  curl -fL --progress-bar -o "$DEST" "$URL"
elif command -v wget >/dev/null 2>&1; then
  wget -q --show-progress -O "$DEST" "$URL"
else
  echo "need curl or wget on PATH" >&2; exit 1
fi

got="$(sha_of "$DEST")"
if [ "$got" != "$SHA256" ]; then
  echo "SHA256 MISMATCH — upstream changed or the download is corrupt." >&2
  echo "  expected $SHA256" >&2
  echo "  got      $got" >&2
  echo "Refusing to proceed: the published numbers were built from the pinned file." >&2
  rm -f "$DEST"
  exit 1
fi

echo "OK: $DEST (sha256 verified)"
echo "next: .venv/bin/python build_lme.py    # embeds turns with BAAI/bge-large-en-v1.5 -> corpora/lme/"
