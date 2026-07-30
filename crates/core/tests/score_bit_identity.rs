//! Paso 0 gate (0.9 read-path phase 1) — bit-identity of the SCORE, not just top-k order.
//!
//! The product guarantee is EXACT, bit-a-bit vector search. The fused NEAREST scores
//! cosine via `cosine_pruned` (Cauchy–Schwarz early-abort); the reference/brute-force
//! path scores via `similarity(Cosine, ..)`. A surviving score MUST be bit-identical
//! between the two — a sub-ULP drift that doesn't flip the top-k would pass a
//! top-k-order gate (`scan_nearest_fused.rs`) yet still violate "bit a bit".
//!
//! This gate asserts the f64 score bit patterns match EXACTLY over many random vectors,
//! including non-lane-multiple dims (exercises the f64 tail) and the stored-vs-live
//! `norm_sq` paths. It is the hard prerequisite for every read-path change in this phase
//! (G1a, G2, G3, G4) and for the v2==v3 build-widening check: run it before and after,
//! it must stay green.

use xyzdb_core::distance::{Metric, cosine_pruned, norm, norm_sq, similarity, suffix_norm2};

const PRUNE_BLOCK: usize = 32; // matches ops/nearest.rs

/// Deterministic LCG → reproducible f32 in [-1, 1). No rng dependency.
fn lcg(state: &mut u64) -> f32 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    let u = ((*state >> 40) & 0xFF_FFFF) as f32 / (1u64 << 24) as f32; // [0,1)
    u * 2.0 - 1.0
}

fn rand_vec(state: &mut u64, n: usize) -> Vec<f32> {
    (0..n).map(|_| lcg(state)).collect()
}

#[test]
fn cosine_pruned_score_is_bit_identical_to_similarity() {
    let mut st = 0x1234_5678_9abc_def0u64;
    // include non-multiples of 8 (65, 999) to exercise the f64 tail loop
    let dims = [8usize, 65, 128, 384, 999, 1024, 1536];
    let mut checked = 0usize;
    // Order-sensitive fold of every surviving score's bit pattern. This is the
    // v2==v3 gate made MECHANICAL: run this test under the x86 SSE2 (v2) build
    // and the x86 AVX2 (v3) build — the fingerprints MUST be identical. A
    // mismatch is NOT "AVX2 is less precise"; it means an FMA/contraction crept
    // in (a stray `mul_add`, or fp-contraction enabled). Red = bug to hunt, not
    // a tradeoff to accept. (On ARM this is a stable reference value; the
    // load-bearing comparison is x86 v2 vs x86 v3 — see docs §v3.)
    let mut fingerprint = 0u64;
    for &n in &dims {
        for _ in 0..200 {
            let a = rand_vec(&mut st, n);
            let b = rand_vec(&mut st, n);
            let bb: &[u8] = bytemuck::cast_slice(&b);

            let reference = similarity(Metric::Cosine, &a, bb);
            let na = norm(&a);
            let suf = suffix_norm2(&a);
            let nb2 = norm_sq(&b);
            // threshold=None ⇒ never abort ⇒ must finalize exactly like similarity
            let fused_stored = cosine_pruned(&a, na, &suf, bb, Some(nb2), None, PRUNE_BLOCK);
            let fused_live = cosine_pruned(&a, na, &suf, bb, None, None, PRUNE_BLOCK);

            match (reference, fused_stored, fused_live) {
                (Some(r), Some(fs), Some(fl)) => {
                    assert_eq!(
                        r.to_bits(),
                        fs.to_bits(),
                        "n={n}: cosine_pruned(stored nb2) score {fs:?} != similarity {r:?}"
                    );
                    assert_eq!(
                        r.to_bits(),
                        fl.to_bits(),
                        "n={n}: cosine_pruned(live nb2) score {fl:?} != similarity {r:?}"
                    );
                    // stored-norm_sq path == live path (the V5 column trusts this)
                    assert_eq!(
                        fs.to_bits(),
                        fl.to_bits(),
                        "n={n}: stored vs live norm_sq differ"
                    );
                    fingerprint = fingerprint.rotate_left(1) ^ r.to_bits();
                    checked += 1;
                }
                (None, None, None) => {} // both undefined (zero norm) — consistent
                other => panic!("n={n}: definedness mismatch fused vs reference: {other:?}"),
            }
        }
    }
    assert!(checked > 800, "too few defined comparisons: {checked}");
    eprintln!(
        "score-bit gate: {checked} cosine scores bit-identical (fused ≡ reference); \
         v2==v3 fingerprint={fingerprint:#018x} (MUST match across x86 SSE2 and AVX2 builds)"
    );
}

#[test]
fn dot_and_l2_scores_are_deterministic() {
    // Dot/L2 go through `similarity` on BOTH the fused and reference paths (nearest.rs
    // only uses cosine_pruned for Cosine), so they are the same call — assert the kernel
    // is deterministic (same inputs → same score bits) so the build-widening check has a
    // stable baseline for these metrics too.
    let mut st = 0xdead_beef_cafe_1234u64;
    for &n in &[65usize, 384, 1024, 1536] {
        for _ in 0..100 {
            let a = rand_vec(&mut st, n);
            let b = rand_vec(&mut st, n);
            let bb: &[u8] = bytemuck::cast_slice(&b);
            for m in [Metric::Dot, Metric::L2] {
                let s1 = similarity(m, &a, bb).unwrap();
                let s2 = similarity(m, &a, bb).unwrap();
                assert_eq!(s1.to_bits(), s2.to_bits(), "n={n} {m:?} non-deterministic");
            }
        }
    }
}
