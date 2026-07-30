"""Shared recall kernel for the cross-engine bench (per prompt-agente-bench-0.9 + INV-1/5).

TWO recall regimes, do not conflate:
  - Corpus B (scale, brute-force oracle): xyzDB is EXACT ⇒ must score 1.000 vs the oracle
    (tie-aware). Rivals show their HNSW residual. Uses `exact_scores` + `tie_aware_recall`.
  - Corpus A (LongMemEval, academic GT): recall@k is SESSION-level (a session is recalled if
    ANY of its turns is in the top-k) vs `answer_session_ids`. ALL engines < 1.0 (retrieval
    quality); xyzDB-exact is the ceiling, rivals ≤ it. Uses `session_recall_at_k`.

The oracle kernel is f32-products / f64-reduction — NOT numpy BLAS on f32 (which reduces in
f32 and flips sub-ULP ties). But xyzDB accumulates the dot in f32 SIMD, so oracle(f64) and
xyzDB(f32) differ at sub-ULP; `measure_precision_gap` quantifies that gap so `TIE_TOL` is set
ABOVE it — otherwise the exactness gate fails on benign cross-implementation rounding.
"""
import numpy as np

# Set from measure_precision_gap on the real corpus (see checkpoint). Default is a safe
# upper bound for 1024-d f32-vs-f64 dot accumulation (~n·eps·|x| ≈ 1024·6e-8 ≈ 6e-5).
TIE_TOL = 2e-4


def exact_scores(q: np.ndarray, vecs: np.ndarray) -> np.ndarray:
    """Oracle cosine: f32 element-wise products, f64 reduction. Unit-norm ⇒ dot==cosine."""
    q32 = q.astype(np.float32)
    v32 = vecs.astype(np.float32)
    return (v32 * q32).astype(np.float64).sum(axis=1)


def _f32_reduced_scores(q: np.ndarray, vecs: np.ndarray) -> np.ndarray:
    """Same products but reduced in f32 — a proxy for an engine's f32-SIMD accumulation."""
    q32 = q.astype(np.float32)
    v32 = vecs.astype(np.float32)
    return (v32 * q32).sum(axis=1, dtype=np.float32).astype(np.float64)


def measure_precision_gap(q: np.ndarray, vecs: np.ndarray):
    """Max/mean |f64-reduce − f32-reduce| over the bucket — the size the tie tolerance
    must clear so an exact engine is not falsely marked < 1.0."""
    d = np.abs(exact_scores(q, vecs) - _f32_reduced_scores(q, vecs))
    return float(d.max()), float(d.mean())


def kth_oracle_score(q: np.ndarray, bucket_vecs: np.ndarray, k: int) -> float:
    """Cutoff = the k-th largest oracle cosine within the bucket (or the min if bucket<k)."""
    s = exact_scores(q, bucket_vecs)
    s.sort()
    return float(s[max(0, len(s) - k)])


def tie_aware_recall(q, returned_ids, corpus_vecs, cut, k, tol=None) -> float:
    """Corpus B oracle recall: fraction of the engine's top-k whose oracle score reaches the
    cutoff. Ties at the boundary all count (compare by score, not id) ⇒ exact engine = 1.0."""
    if k <= 0:
        return 0.0
    tol = TIE_TOL if tol is None else tol
    ids = [int(i) for i in list(returned_ids)[:k]]
    if not ids:
        return 0.0
    sc = exact_scores(q, corpus_vecs[ids])
    return int((sc >= cut - tol).sum()) / k


def tie_aware_equal(ids_a, ids_b, score_of, tol=None) -> bool:
    """INV-5: two rankings are equal iff same top-k order EXCEPT among exactly-equal scores.
    `score_of` maps an id → its (oracle) score. Position i may differ only if the two ids'
    scores are within `tol`."""
    tol = TIE_TOL if tol is None else tol
    if len(ids_a) != len(ids_b):
        return False
    for a, b in zip(ids_a, ids_b):
        if a != b and abs(score_of[a] - score_of[b]) > tol:
            return False
    return True


def session_recall_at_k(returned_turn_ids, turn_session, gt_sessions, k) -> float:
    """Corpus A academic recall@k (INV-1): a GT session counts as recalled if ANY of its turns
    is in the engine's top-k. `turn_session` maps turn-id → session-id."""
    gt = set(gt_sessions)
    if not gt:
        return 0.0
    got = {turn_session[int(i)] for i in list(returned_turn_ids)[:k]}
    return len(gt & got) / len(gt)
