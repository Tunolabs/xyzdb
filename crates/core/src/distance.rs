//! Vector distance / similarity for `NEAREST` retrieval.
//!
//! Embeddings are stored f32 (`Value::Vector`); scoring is **f32 end-to-end**.
//! The query is an aligned `&[f32]`; each candidate is read **directly from its
//! packed little-endian bytes** (a V5 column / V3 prefix) via unaligned `f32x8`
//! loads — no per-candidate decode into an aligned `Vec`. The per-element
//! products run over `wide::f32x8` lanes (eight f32 partial sums); the cross-lane
//! reduction, the norms, the suffix-norm table, and the Cauchy–Schwarz bound are
//! all **f64** — so the lane arithmetic is fast and vectorisable while the
//! accumulation stays numerically safe. There is no per-element `as f64` cast in
//! the hot loop (that cast both doubled the work and blocked auto-vectorisation
//! in the old scalar-f64 path).
//!
//! A **higher score always means "more similar"** — `NEAREST` keeps the top-k
//! by score with one bounded min-heap regardless of metric. For `L2` the score
//! is the *negated* distance (closer ⇒ higher ⇒ nearer), keeping the rule
//! uniform.
//!
//! **Bit-identity contract.** [`similarity`] and a surviving [`cosine_pruned`]
//! must return the *same bits* for the same inputs: both accumulate the dot over
//! the identical `f32x8` chunk sequence and reduce it the same way, and both
//! divide by `‖a‖·‖b‖` computed via [`norm`]. The pruning only inserts a bound
//! check between blocks; it never alters the accumulator. The SIMD kernel is
//! within-ULP of a naïve f64 fold (different accumulation order), not
//! bit-identical to it — that is by design.

// SPDX-License-Identifier: BUSL-1.1
use crate::value::Value;
use wide::f32x8;

const LANES: usize = 8;

// The scorer reads a stored candidate's f32 directly from its packed
// little-endian bytes via unaligned loads interpreted in native order — a
// big-endian target would misread them. Every supported target (x86_64,
// aarch64) is little-endian; fail the build loudly anywhere else.
#[cfg(target_endian = "big")]
compile_error!("xyzdb-core scoring requires a little-endian target");

/// Similarity metric for `NEAREST`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    /// Cosine similarity `dot(a,b) / (‖a‖·‖b‖)`, in `[-1, 1]`. Magnitude-invariant.
    Cosine,
    /// Raw dot product. Use when embeddings are already L2-normalized.
    Dot,
    /// Negated Euclidean distance `-‖a-b‖` (so higher ⇒ nearer).
    L2,
}

impl Metric {
    /// Parse a metric name, case-insensitive.
    ///
    /// # Arguments
    /// * `name` - one of `cosine`/`cos`, `dot`/`inner`, `l2`/`euclidean`.
    ///
    /// # Returns
    /// The metric, or `None` if the name is unknown.
    pub fn parse(name: &str) -> Option<Metric> {
        match name.to_ascii_lowercase().as_str() {
            "cosine" | "cos" => Some(Metric::Cosine),
            "dot" | "inner" => Some(Metric::Dot),
            "l2" | "euclidean" => Some(Metric::L2),
            _ => None,
        }
    }
}

/// Extract an f32 vector from a [`Value`].
///
/// `Value::Vector` (the stored packed form) is returned directly; a
/// `Value::List` of `Float`/`Int` is coerced element-wise to `f32` (so a
/// query literal parsed as `f64`/`Int` is narrowed once, here, not per
/// candidate in the hot loop).
///
/// # Returns
/// The vector, or `None` if `v` is not a list/vector or holds a non-numeric
/// element.
pub fn as_vector(v: &Value) -> Option<Vec<f32>> {
    match v {
        Value::Vector(packed) => Some(packed.clone()),
        Value::List(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    Value::Float(f) => out.push(*f as f32),
                    Value::Int(i) => out.push(*i as f32),
                    _ => return None,
                }
            }
            Some(out)
        }
        _ => None,
    }
}

/// Load eight contiguous f32 (the query side, always an aligned slice) into an
/// `f32x8`; `s` must have at least 8 elements.
#[inline]
fn load8(s: &[f32]) -> f32x8 {
    let mut a = [0.0f32; LANES];
    a.copy_from_slice(&s[..LANES]);
    f32x8::from(a)
}

/// Load eight stored f32 (the candidate side: packed little-endian bytes from a
/// V5 column / V3 prefix, at any alignment) starting at element `off`, via one
/// unaligned 32-byte read — no copy into an aligned buffer. This is the win over
/// decoding each candidate into a scratch `Vec<f32>` first: on a stored vector
/// scan the candidate bytes feed the lanes directly. `b` must hold 8 floats from
/// `off`. Little-endian only (guarded at the module top).
#[inline]
fn load8_bytes(b: &[u8], off: usize) -> f32x8 {
    let arr: [f32; LANES] = bytemuck::pod_read_unaligned(&b[off * 4..off * 4 + LANES * 4]);
    f32x8::from(arr)
}

/// Read one stored little-endian f32 at element index `i` (the ragged tail).
#[inline]
fn f32_at(b: &[u8], i: usize) -> f32 {
    f32::from_le_bytes([b[4 * i], b[4 * i + 1], b[4 * i + 2], b[4 * i + 3]])
}

/// Σ `a_i·b_i` — the canonical dot of a query `a` (aligned f32) against a stored
/// candidate `b` (packed little-endian bytes, `a.len()*4` of them). Eight f32
/// lane accumulators across full chunks, reduced once in f64, plus an f64 tail.
/// Both `similarity` and `cosine_pruned` reduce through this exact shape, so a
/// survivor is bit-identical.
#[inline]
fn dot_acc(a: &[f32], b: &[u8]) -> f64 {
    let n = a.len();
    let chunks = n / LANES;
    let mut acc = f32x8::ZERO;
    for c in 0..chunks {
        let off = c * LANES;
        acc += load8(&a[off..off + LANES]) * load8_bytes(b, off);
    }
    let mut s: f64 = acc.to_array().iter().map(|&x| x as f64).sum();
    // scalar f64 tail; indexed to stay bit-identical to the SIMD path above
    #[allow(clippy::needless_range_loop)]
    for i in (chunks * LANES)..n {
        s += a[i] as f64 * f32_at(b, i) as f64;
    }
    s
}

/// Σ `b_i²` over a stored candidate's packed bytes — `‖b‖²` via the same
/// lane/reduce shape as [`dot_acc`], so it is bit-identical to `dot_acc(b, b)`
/// (and to the `norm_sq` persisted at write time). Used when the cosine path has
/// no pre-stored norm.
#[inline]
fn dot_self_bytes(b: &[u8]) -> f64 {
    let n = b.len() / 4;
    let chunks = n / LANES;
    let mut acc = f32x8::ZERO;
    for c in 0..chunks {
        let v = load8_bytes(b, c * LANES);
        acc += v * v;
    }
    let mut s: f64 = acc.to_array().iter().map(|&x| x as f64).sum();
    for i in (chunks * LANES)..n {
        let x = f32_at(b, i) as f64;
        s += x * x;
    }
    s
}

/// Σ `(a_i − b_i)²` — the canonical squared L2 (query `a` f32, candidate `b`
/// bytes), same lane/reduce shape as [`dot_acc`].
#[inline]
fn sqdist_acc(a: &[f32], b: &[u8]) -> f64 {
    let n = a.len();
    let chunks = n / LANES;
    let mut acc = f32x8::ZERO;
    for c in 0..chunks {
        let off = c * LANES;
        let d = load8(&a[off..off + LANES]) - load8_bytes(b, off);
        acc += d * d;
    }
    let mut s: f64 = acc.to_array().iter().map(|&x| x as f64).sum();
    // scalar f64 tail; indexed to stay bit-identical to the SIMD path above
    #[allow(clippy::needless_range_loop)]
    for i in (chunks * LANES)..n {
        let d = a[i] as f64 - f32_at(b, i) as f64;
        s += d * d;
    }
    s
}

/// `‖a‖` — the L2 norm, computed via the canonical [`dot_acc`] so callers feed
/// `cosine_pruned` an `na` that matches [`similarity`]'s denominator bit-for-bit.
///
/// # Returns
/// The Euclidean norm of `a` (0.0 for an empty or all-zero vector).
pub fn norm(a: &[f32]) -> f64 {
    dot_acc(a, bytemuck::cast_slice(a)).sqrt()
}

/// `‖a‖²` — the RAW squared norm via the canonical [`dot_acc`] reduction (no
/// `sqrt`). Write paths persist this (V4 prefix / V5 column) so a cosine NEAREST
/// can skip the per-candidate norm pass. It is **bit-identical** to the `nb2`
/// [`cosine_pruned`] computes live and to `norm(a)`'s radicand, so the stored
/// and live cosine scores agree to the bit — which the fused fast path relies on.
///
/// # Returns
/// `Σ a_i²` accumulated through the same `f32x8` lanes as the dot (0.0 when empty).
pub fn norm_sq(a: &[f32]) -> f64 {
    dot_acc(a, bytemuck::cast_slice(a))
}

/// Score a query against a stored candidate so a **higher score means more
/// similar**.
///
/// # Arguments
/// * `metric` - the similarity metric.
/// * `a` - the query vector (aligned f32), non-empty.
/// * `b` - the candidate's packed little-endian f32 bytes (`a.len()*4` of them),
///   read directly from storage — no decode into an aligned `Vec` first.
///
/// # Returns
/// The score, or `None` if the lengths disagree (`b.len() != a.len()*4`), `a` is
/// empty, or a cosine norm is zero (cosine is undefined for a zero vector).
pub fn similarity(metric: Metric, a: &[f32], b: &[u8]) -> Option<f64> {
    if a.is_empty() || b.len() != a.len() * 4 {
        return None;
    }
    match metric {
        Metric::Dot => Some(dot_acc(a, b)),
        Metric::Cosine => {
            let na = norm(a);
            let nb2 = dot_self_bytes(b);
            if na == 0.0 || nb2 == 0.0 {
                None
            } else {
                Some(dot_acc(a, b) / (na * nb2.sqrt()))
            }
        }
        Metric::L2 => Some(-sqdist_acc(a, b).sqrt()),
    }
}

/// Reverse-cumulative squared norm of `a`: `out[i] = Σ_{j>=i} a[j]²` (f64), with
/// `out.len() == a.len() + 1` and `out[n] == 0`. Precomputed once per query so
/// [`cosine_pruned`] reads `‖a_tail‖²` in O(1) at each early-abort check.
///
/// # Returns
/// The suffix sum-of-squares table.
pub fn suffix_norm2(a: &[f32]) -> Vec<f64> {
    let n = a.len();
    let mut out = vec![0.0f64; n + 1];
    for i in (0..n).rev() {
        out[i] = out[i + 1] + a[i] as f64 * a[i] as f64;
    }
    out
}

/// Cosine similarity with a Cauchy–Schwarz **early-abort** for top-k pruning.
///
/// The dot is accumulated in `f32x8` blocks; after each full block the largest
/// value the final dot can still reach is `dot_partial + ‖a_tail‖·‖b_tail‖`
/// (Cauchy–Schwarz, computed in f64). If even that, scored, **strictly** falls
/// below `threshold` (the current k-th best similarity), the candidate cannot
/// enter the top-k and we return `None` without finishing the dot.
///
/// A surviving score is **bit-identical** to [`similarity`]`(Cosine, a, b)`: the
/// dot reduces through [`dot_acc`]'s shape and the denominator is `na · √nb2`,
/// the same factors [`similarity`] uses.
///
/// # Arguments
/// * `a` - query vector (aligned f32).
/// * `na` - `‖a‖` from [`norm`] (f64), precomputed once per query.
/// * `a_suffix_norm2` - [`suffix_norm2(a)`](suffix_norm2); `len == a.len() + 1`.
/// * `b` - candidate's packed little-endian f32 bytes (`a.len()*4` of them).
/// * `nb2` - `‖b‖²` if pre-computed (V4 prefix / V5 column stores it), else
///   `None` to compute it live; a supplied value must equal `Σ b_i²` reduced as
///   [`dot_acc`] (i.e. the persisted `norm_sq`).
/// * `threshold` - current k-th best cosine score; `None` ⇒ heap not full (never abort).
/// * `block` - check the bound every ~`block` dims (rounded down to a multiple
///   of the lane width); amortises the per-check `sqrt`s.
///
/// # Returns
/// The exact cosine score (bit-identical to [`similarity`] for a survivor), or
/// `None` if aborted or undefined (zero norm).
#[allow(clippy::too_many_arguments)]
pub fn cosine_pruned(
    a: &[f32],
    na: f64,
    a_suffix_norm2: &[f64],
    b: &[u8],
    nb2: Option<f64>,
    threshold: Option<f64>,
    block: usize,
) -> Option<f64> {
    let n = a.len();
    if b.len() != n * 4 || n == 0 {
        return None;
    }
    // ‖b‖²: supplied by the V4 prefix (lever C), else computed live via the
    // canonical reduction so the live and stored paths agree bit-for-bit.
    let nb2: f64 = match nb2 {
        Some(s) => s,
        None => dot_self_bytes(b),
    };
    if na == 0.0 || nb2 == 0.0 {
        return None;
    }
    let denom = na * nb2.sqrt();
    // score > threshold ⟺ dot > threshold·denom (denom > 0). Strict, so a tie at
    // the threshold is never pruned (the caller's lid tiebreak decides it).
    let t_dot = threshold.map(|t| t * denom);
    let chunks = n / LANES;
    let blk_chunks = (block.max(LANES) / LANES).max(1); // full f32x8 chunks per block
    let mut acc = f32x8::ZERO;
    let mut bsq = f32x8::ZERO;
    let mut c = 0usize;
    while c < chunks {
        let cend = (c + blk_chunks).min(chunks);
        for cc in c..cend {
            let off = cc * LANES;
            let bv = load8_bytes(b, off);
            acc += load8(&a[off..off + LANES]) * bv;
            bsq += bv * bv;
        }
        c = cend;
        if let Some(td) = t_dot
            && c < chunks
        {
            // Upper bound on the FINAL dot, in f64: dot_partial + ‖a_tail‖·‖b_tail‖.
            let dot_partial: f64 = acc.to_array().iter().map(|&x| x as f64).sum();
            let bhead2: f64 = bsq.to_array().iter().map(|&x| x as f64).sum();
            let a_tail = a_suffix_norm2[c * LANES].max(0.0).sqrt();
            let b_tail = (nb2 - bhead2).max(0.0).sqrt();
            if dot_partial + a_tail * b_tail < td {
                return None;
            }
        }
    }
    // Finalise the dot exactly as dot_acc does (reduce the same acc, same f64 tail)
    // so a survivor is bit-identical to similarity(Cosine, a, b).
    let mut dot: f64 = acc.to_array().iter().map(|&x| x as f64).sum();
    // scalar f64 tail; indexed to stay bit-identical to the SIMD path above
    #[allow(clippy::needless_range_loop)]
    for i in (chunks * LANES)..n {
        dot += a[i] as f64 * f32_at(b, i) as f64;
    }
    Some(dot / denom)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    /// Naïve f64 fold — the within-epsilon oracle (a different accumulation
    /// order than the SIMD kernel, so equality is approximate, not bit-exact).
    fn dot_naive_f64(a: &[f32], b: &[f32]) -> f64 {
        (0..a.len()).map(|i| a[i] as f64 * b[i] as f64).sum()
    }

    /// View an f32 slice as the packed little-endian bytes the scorer consumes
    /// for a candidate (zero-copy).
    fn as_bytes(v: &[f32]) -> &[u8] {
        bytemuck::cast_slice(v)
    }

    #[test]
    fn metric_parse_is_case_insensitive() {
        assert_eq!(Metric::parse("Cosine"), Some(Metric::Cosine));
        assert_eq!(Metric::parse("DOT"), Some(Metric::Dot));
        assert_eq!(Metric::parse("euclidean"), Some(Metric::L2));
        assert_eq!(Metric::parse("hamming"), None);
    }

    #[test]
    fn as_vector_extracts_numbers_and_coerces_int() {
        let v = Value::List(vec![Value::Float(1.0), Value::Int(2), Value::Float(-0.5)]);
        assert_eq!(as_vector(&v), Some(vec![1.0f32, 2.0, -0.5]));
    }

    #[test]
    fn as_vector_rejects_non_numeric_and_non_list() {
        let bad = Value::List(vec![Value::Float(1.0), Value::Text("x".into())]);
        assert_eq!(as_vector(&bad), None);
        assert_eq!(as_vector(&Value::Int(3)), None);
    }

    #[test]
    fn as_vector_reads_packed_f32_vector() {
        let v = Value::Vector(vec![1.0, 2.0, -0.5]);
        assert_eq!(as_vector(&v), Some(vec![1.0f32, 2.0, -0.5]));
    }

    #[test]
    fn cosine_identical_orthogonal_opposite() {
        assert!(close(
            similarity(Metric::Cosine, &[1.0, 0.0], as_bytes(&[2.0, 0.0])).unwrap(),
            1.0
        ));
        assert!(close(
            similarity(Metric::Cosine, &[1.0, 0.0], as_bytes(&[0.0, 1.0])).unwrap(),
            0.0
        ));
        assert!(close(
            similarity(Metric::Cosine, &[1.0, 0.0], as_bytes(&[-1.0, 0.0])).unwrap(),
            -1.0
        ));
    }

    #[test]
    fn dot_is_raw_inner_product() {
        assert!(close(
            similarity(Metric::Dot, &[1.0, 2.0], as_bytes(&[3.0, 4.0])).unwrap(),
            11.0
        ));
    }

    #[test]
    fn l2_is_negated_distance_so_higher_is_nearer() {
        assert!(close(
            similarity(Metric::L2, &[1.0, 1.0], as_bytes(&[1.0, 1.0])).unwrap(),
            0.0
        ));
        let near = similarity(Metric::L2, &[0.0, 0.0], as_bytes(&[1.0, 0.0])).unwrap();
        let far = similarity(Metric::L2, &[0.0, 0.0], as_bytes(&[3.0, 4.0])).unwrap();
        assert!(near > far);
    }

    /// SIMD kernel is within-ULP of a naïve f64 fold across dims that exercise
    /// full chunks + a ragged tail (catches a wrong index or a dropped tail).
    #[test]
    fn simd_dot_is_within_epsilon_of_naive_f64() {
        for n in [1usize, 7, 8, 9, 64, 100, 384, 1000] {
            let a: Vec<f32> = (0..n).map(|i| ((i * 7 + 3) % 13) as f32 - 6.0).collect();
            let b: Vec<f32> = (0..n).map(|i| ((i * 5 + 1) % 11) as f32 - 5.0).collect();
            let simd = similarity(Metric::Dot, &a, as_bytes(&b)).unwrap();
            let naive = dot_naive_f64(&a, &b);
            let tol = 1e-3 * (naive.abs().max(1.0)); // relative; error grows with dim
            assert!(
                (simd - naive).abs() <= tol,
                "n={n}: simd={simd} naive={naive}"
            );
        }
    }

    #[test]
    fn cosine_pruned_survivor_is_bit_identical_and_aborts_only_losers() {
        let n = 64usize;
        let q: Vec<f32> = (0..n).map(|i| ((i * 7 + 3) % 13) as f32 - 6.0).collect();
        let na = norm(&q);
        let suf = suffix_norm2(&q);
        let corpus: Vec<Vec<f32>> = (0..200)
            .map(|c| {
                (0..n)
                    .map(|i| (((i * c + c) % 17) as f32 - 8.0) * 0.5)
                    .collect()
            })
            .collect();
        let mut scores: Vec<f64> = corpus
            .iter()
            .map(|b| similarity(Metric::Cosine, &q, as_bytes(b)).unwrap())
            .collect();
        scores.sort_by(|a, b| b.partial_cmp(a).unwrap());
        let threshold = scores[9]; // 10th best
        for b in &corpus {
            let full = similarity(Metric::Cosine, &q, as_bytes(b)).unwrap();
            match cosine_pruned(&q, na, &suf, as_bytes(b), None, Some(threshold), 8) {
                Some(s) => assert_eq!(s.to_bits(), full.to_bits(), "survivor not bit-identical"),
                None => assert!(full < threshold, "pruned a winner: {full} >= {threshold}"),
            }
        }
        // No threshold ⇒ never aborts, stays exact (bit-identical).
        for b in &corpus {
            let full = similarity(Metric::Cosine, &q, as_bytes(b)).unwrap();
            let p = cosine_pruned(&q, na, &suf, as_bytes(b), None, None, 8).unwrap();
            assert_eq!(p.to_bits(), full.to_bits());
        }
        // Lever C: a pre-computed ‖b‖² (as norm_sq(b), what the V4 prefix / V5
        // column store) is bit-identical to computing it live — the prefix-path
        // equivalence.
        for b in &corpus {
            let live = cosine_pruned(&q, na, &suf, as_bytes(b), None, None, 8).unwrap();
            let nb2 = norm_sq(b);
            let stored = cosine_pruned(&q, na, &suf, as_bytes(b), Some(nb2), None, 8).unwrap();
            assert_eq!(stored.to_bits(), live.to_bits());
        }
    }

    #[test]
    fn rejects_dim_mismatch_empty_and_zero_norm() {
        assert_eq!(
            similarity(Metric::Cosine, &[1.0, 2.0], as_bytes(&[1.0])),
            None
        );
        assert_eq!(similarity(Metric::Dot, &[], as_bytes(&[])), None);
        assert_eq!(
            similarity(Metric::Cosine, &[0.0, 0.0], as_bytes(&[1.0, 1.0])),
            None
        );
    }

    /// M1 attribution micro-bench (NOT a correctness test): isolate "remove the
    /// `as f64` cast" from "explicit SIMD" by timing three dot kernels over the
    /// same synthetic corpus — the OLD scalar-f64-via-cast, a scalar-f32 (cast
    /// removed, which LLVM may auto-vectorise on its own), and the production
    /// `f32x8` [`dot_acc`]. Run release so the codegen matches production:
    ///   cargo test --release -p xyzdb-core micro_scorer_attribution -- --ignored --nocapture
    #[test]
    #[ignore]
    fn micro_scorer_attribution() {
        use std::hint::black_box;
        use std::time::Instant;

        // splitmix64 — deterministic synthetic vectors (fixed seed = stable).
        fn mix(s: &mut u64) -> u64 {
            *s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = *s;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
        fn comp(s: &mut u64) -> f32 {
            ((mix(s) >> 40) as f32) / ((1u64 << 23) as f32) - 1.0 // ~uniform [-1,1)
        }
        // OLD scorer: scalar dot with a per-element f64 cast (doubled the work and
        // blocked auto-vectorisation).
        fn dot_f64_cast(a: &[f32], b: &[f32]) -> f64 {
            let mut s = 0f64;
            for i in 0..a.len() {
                s += a[i] as f64 * b[i] as f64;
            }
            s
        }
        // Cast removed: products in f32, no explicit SIMD. Whether this matches
        // f32x8 tells us if LLVM auto-vectorises once the cast is gone.
        fn dot_f32_scalar(a: &[f32], b: &[f32]) -> f64 {
            let mut s = 0f32;
            for i in 0..a.len() {
                s += a[i] * b[i];
            }
            s as f64
        }
        // Generic (monomorphised → inlined, no dyn dispatch) best-of-`reps` timer
        // for one full N-vector scan; best-of filters scheduler noise.
        fn bench<F: Fn(&[f32], &[f32]) -> f64>(
            reps: usize,
            n: usize,
            dim: usize,
            corpus: &[f32],
            q: &[f32],
            f: F,
        ) -> f64 {
            let mut best = f64::MAX;
            for _ in 0..reps {
                let t = Instant::now();
                let mut acc = 0f64;
                for i in 0..n {
                    let b = &corpus[i * dim..(i + 1) * dim];
                    acc += f(black_box(q), black_box(b));
                }
                black_box(acc);
                best = best.min(t.elapsed().as_secs_f64());
            }
            best
        }

        let (reps, n) = (50usize, 10_000usize);
        for &dim in &[384usize, 1536] {
            let mut st = 0xDEAD_BEEF_u64 ^ (dim as u64);
            let corpus: Vec<f32> = (0..n * dim).map(|_| comp(&mut st)).collect();
            let q: Vec<f32> = (0..dim).map(|_| comp(&mut st)).collect();

            let t_cast = bench(reps, n, dim, &corpus, &q, dot_f64_cast);
            let t_f32 = bench(reps, n, dim, &corpus, &q, dot_f32_scalar);
            // The production kernel: candidate read straight from packed bytes
            // (the byte-native scorer's unaligned f32x8 load — no scratch).
            let t_simd = bench(reps, n, dim, &corpus, &q, |a, b| {
                super::dot_acc(a, bytemuck::cast_slice(b))
            });

            let gflops = |t: f64| (n as f64 * 2.0 * dim as f64) / t / 1e9;
            eprintln!("\n=== scorer micro (dim={dim}, N={n}, best-of-{reps}) ===");
            eprintln!(
                "  f64+cast (old): {:>6.2} GFLOP/s  {:.3} ms/scan",
                gflops(t_cast),
                t_cast * 1e3
            );
            eprintln!(
                "  f32 scalar    : {:>6.2} GFLOP/s  {:.3} ms/scan   cast-removal {:.2}x",
                gflops(t_f32),
                t_f32 * 1e3,
                t_cast / t_f32
            );
            eprintln!(
                "  f32x8 (wide)  : {:>6.2} GFLOP/s  {:.3} ms/scan   SIMD-over-scalar {:.2}x",
                gflops(t_simd),
                t_simd * 1e3,
                t_f32 / t_simd
            );
            eprintln!("  >>> total f64+cast -> f32x8: {:.2}x", t_cast / t_simd);
        }
    }
}
