#!/usr/bin/env bash
# v3-verify (AWS x86, run BEFORE trusting after-image numbers) — GUARANTEE verification, not
# magnitude (ESTADO §Bloque-x86). v3 (x86-64-v3 / AVX2) must produce BIT-IDENTICAL scores to the
# v2 (SSE2) baseline; it only changes SPEED, never results. Two gates:
#   (a) `score_bit_identity` test PASSES under BOTH a v2 and a v3 build — the 1400-score fingerprint
#       must equal the reference 0xa74b71bdc019be72 in both. RED = FMA/reassoc leaked = a BUG to
#       hunt, NOT a tradeoff. v3 is NOT closed until this is green.
#   (b) objdump of `dot_acc`: the v3 build emits 256-bit ymm VMULPS/VADDPS; the v2 build does not
#       (128-bit xmm only) — proves v3 actually widened the kernel.
# Run on the m6a x86 box, from the engine crate root (ENGINE_DIR, default: repo root).
set -euo pipefail
ENGINE_DIR="${ENGINE_DIR:-$(cd "$(dirname "$0")/../../xyzdb" && pwd)}"; cd "$ENGINE_DIR"
REF=0xa74b71bdc019be72
echo "engine dir: $ENGINE_DIR · reference fingerprint: $REF"

echo "=== (a1) v3 build (config.toml applies target-cpu=x86-64-v3 on this target) ==="
cargo test -p xyzdb-core --release score_bit_identity 2>&1 | tee /tmp/v3_bitid_v3.log
echo "=== (a2) v2 baseline (RUSTFLAGS overrides config.toml rustflags entirely) ==="
RUSTFLAGS="-C target-cpu=x86-64-v2" cargo test -p xyzdb-core --release score_bit_identity 2>&1 | tee /tmp/v3_bitid_v2.log
echo "  → both suites must be GREEN. The test asserts the $REF fingerprint internally;"
echo "    a failure here = v3 changed the bits = FMA/reassoc bug (RED), not a tradeoff."

echo "=== (b) objdump of dot_acc — v3 should show 256-bit ymm, v2 xmm-only ==="
# locate the freshest release artifact carrying dot_acc (engine rlib / test bin)
V3BIN=$(find target/release -name 'xyzdb_engine-*' -type f 2>/dev/null | head -1)
if [ -n "${V3BIN:-}" ]; then
  echo "-- v3 dot_acc (expect vmulps/vaddps with %ymm) --"
  objdump -d "$V3BIN" 2>/dev/null | awk '/dot_acc/{f=1} f{print} /ret/{if(f)c++} c>1{exit}' \
    | grep -iE 'vmulps|vaddps|vfmadd|%ymm|%xmm' | head -20 || echo "  (símbolo no encontrado — buscar en turba-engine)"
else
  echo "  release binary con dot_acc no localizado — ubicar manualmente (grep -r 'fn dot_acc' src)"
fi
echo "  RECORDATORIO: si aparece VFMADD (fused multiply-add) → FMA colada → ROJO, cazar el bug."
echo "v3-verify DONE — solo si (a) verde en ambos + (b) v3=ymm/v2=xmm, v3 queda CERRADO."
