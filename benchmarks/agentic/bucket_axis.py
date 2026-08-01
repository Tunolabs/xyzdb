#!/usr/bin/env python3
"""The locality-granularity axis: tenant isolation vs shared pool.

WHAT THE AXIS ACTUALLY IS (read this before reading the numbers)
---------------------------------------------------------------
Regrouping does **not** make users bigger — it pools users into a shared bucket.
500 buckets is one user per bucket; 50 buckets is **ten users sharing one**. So
"50 users with 5k memories each" would be a lie: it is 500 users, ten to a bucket.

The query keeps naming **the user**, which is the honest choice, so pooling turns
the user into a **residual filter inside a larger bucket**. That gives the axis its
real business name: the decision every SaaS architect makes — schema per tenant vs
one shared schema with a `tenant_id` — measured end to end.

    500 buckets   ~493/bucket    USER        total isolation; the query names its gravity
     50 buckets  ~4,935/bucket   GROUP       ten tenants per partition; user goes residual
      5 buckets ~49,348/bucket   BIG GROUP   a hundred tenants per partition
      1 bucket  246,738          POOL        one shared space; tenant_id is just a field

**There is deliberately no "session" point**, though the corpus has 19,195 `sid` of
~13 turns and it would have saved fabricating the fine end. It does not hold, and
not for size: a query's answer is spread across **2 to 6 sessions in 59% of cases**
(196 queries have one answer session, 283 have several). With gravity on the
session, a query pins one and loses the rest *by construction* — recall capped
below 1.0 for a reason that has nothing to do with any engine. LongMemEval is built
that way on purpose; it is a long-term-memory benchmark and the needle is spread.
(Size would have been survivable: 90.2% of queries have all their answer sessions
holding >=10 turns.) A session is a co-location unit, not a query scope, and an axis
of localities needs scopes. The fine end is **user** — equally natural, being the
corpus's own 500 question-ids.


WHY THIS EXISTS
---------------
The v1 matrix varied tier and N but held bucket size constant at ~493 records by
accident of the corpus: LongMemEval-S has 500 questions and `GRAVITY BY bucket`
uses `question_id`, so 246,738 turns land in 500 buckets (measured here: min 396,
p50 491, mean 493.5, max 616). At 1024d that is a ~2 MB working set per query — it
fits any cache of any tier, which is why S1 reads flat across T1..T4. That
flatness *demonstrates* gravity works, and it also means the matrix never visits
the regime where sub-gravity, bounded aggregates or `budget_stop` can appear.

This module turns the accident into a declared axis: the same 246,738 vectors, the
same 500 queries, regrouped into 500 / 50 / 5 / 1 buckets. Only the co-location
changes. No new corpus, no re-embedding.

TWO THINGS THAT WOULD SILENTLY RUIN THE AXIS
--------------------------------------------
1. **The truth is per point, never reused.** The oracle is the top-k *within the
   query's bucket* (`recall_harness.kth_oracle_score` cuts against `bucket_vecs`).
   Merge buckets and the bucket holds different vectors, so the true top-k changes.
   Reusing the 500-bucket oracle at another point would mis-score every engine
   equally — and equally wrong is exactly the failure nobody notices, because the
   comparison still looks self-consistent. `verify_oracle_is_per_point()` is the
   negative control for this.

2. **The axis changes DIFFICULTY, not just cost.** With 10 questions merged into
   one bucket, a query hunts its needle among nine other conversations' haystacks.
   That is harder *for every engine equally*, so each point remains apples to
   apples — but a report may not say "latency grows with bucket size" without also
   saying "and so does difficulty". Signed prediction, before running: **recall@k
   falls along the axis for all four engines, and by similar amounts**, because it
   is a property of the corpus, not of any engine. If it falls *unevenly*, that is
   a finding, and an interesting one: someone's bounding mechanism degrades as the
   haystack gets denser.
"""
import argparse
import hashlib
import json
import os

import numpy as np

import recall_harness as rh

_HERE = os.path.dirname(os.path.abspath(__file__))
CORP = os.environ.get("BENCH_CORP", os.path.join(_HERE, "corpora", "lme"))

# The declared points of the axis: how many buckets the 500 real users collapse into.
# 500 = one bucket per user (the v1 accident, ~493/bucket); 1 = every user pooled.
AXIS_POINTS = (500, 50, 5, 1)

# The business name of each point — carried into the record so a number is never
# published without the architecture it describes.
POINT_NAMES = {500: "user", 50: "group", 5: "big_group", 1: "pool"}
POINT_MEANING = {
    500: "total isolation: one tenant, one scope; the query names its gravity",
    50: "ten tenants per partition; the user becomes a residual filter",
    5: "a hundred tenants per partition",
    1: "one shared space; tenant_id is just a field",
}


def regroup(qids, n_buckets: int) -> np.ndarray:
    """Map each turn's question-id to a merged bucket index. Pure and deterministic.

    Distinct qids are sorted (so the result does not depend on turn order or on
    dict iteration) and cut into `n_buckets` contiguous groups of near-equal size.
    Contiguous rather than interleaved keeps group sizes even; either would do, but
    the choice must be fixed, not incidental.

    Args:
        qids: Per-turn question-id, length = number of turns.
        n_buckets: Target bucket count. Must not exceed the distinct qid count.

    Returns:
        int64 array, length = number of turns, values in [0, n_buckets).

    Raises:
        ValueError: If `n_buckets` exceeds the number of distinct qids, which would
            silently produce empty buckets and an oracle over nothing.
    """
    uniq = sorted(set(qids))
    if n_buckets > len(uniq):
        raise ValueError(f"n_buckets={n_buckets} > distinct qids={len(uniq)}")
    per = -(-len(uniq) // n_buckets)  # ceil, so the last group is the short one
    index = {q: min(i // per, n_buckets - 1) for i, q in enumerate(uniq)}
    return np.array([index[q] for q in qids], dtype=np.int64)


def build_oracle(vecs, bucket_ids, qvecs, q_bucket, k: int) -> np.ndarray:
    """True top-k row indices for each query, computed WITHIN that query's bucket.

    This is the per-point truth of the module docstring: it is a function of the
    bucket assignment, so it must be rebuilt whenever the assignment changes.

    Args:
        vecs: (n, dim) f32 corpus vectors, one row per turn.
        bucket_ids: (n,) bucket index per row.
        qvecs: (nq, dim) f32 query vectors.
        q_bucket: (nq,) the bucket each query searches.
        k: Depth of the truth.

    Returns:
        (nq, k) int64 array of row indices into `vecs`, best first. Short buckets
        are padded by repeating the last hit so the array stays rectangular; the
        consumers read it as a set, so a repeat is harmless.
    """
    members = {}
    for b in np.unique(bucket_ids):
        members[int(b)] = np.flatnonzero(bucket_ids == b)
    out = np.zeros((len(qvecs), k), dtype=np.int64)
    for j in range(len(qvecs)):
        rows = members[int(q_bucket[j])]
        scores = rh.exact_scores(qvecs[j], vecs[rows])
        take = min(k, len(rows))
        top = rows[np.argsort(-scores, kind="stable")[:take]]
        if take < k:  # pad a short bucket rather than emit a ragged array
            top = np.concatenate([top, np.repeat(top[-1:], k - take)])
        out[j] = top
    return out


def verify_oracle_is_per_point(vecs, qvecs, qids, k: int = 10) -> dict:
    """Negative control: prove the truth actually moves when the buckets move.

    Builds the oracle at two axis points and asserts they differ. If they did not,
    the per-point rebuild would be ceremony — and reusing one oracle everywhere
    would be undetectable, because every engine would be mis-scored the same way.
    """
    b_fine = regroup(qids, 500)
    b_coarse = regroup(qids, 5)
    q_fine = np.array([b_fine[np.flatnonzero(b_fine == i)[0]] for i in range(500)])
    # Query j searches the bucket its own question fell into.
    uniq = sorted(set(qids))
    qpos = {q: i for i, q in enumerate(uniq)}
    first_turn = {q: np.flatnonzero(np.asarray(qids) == q)[0] for q in uniq}
    q_fine = np.array([b_fine[first_turn[q]] for q in uniq])
    q_coarse = np.array([b_coarse[first_turn[q]] for q in uniq])
    o_fine = build_oracle(vecs, b_fine, qvecs, q_fine, k)
    o_coarse = build_oracle(vecs, b_coarse, qvecs, q_coarse, k)
    same = int(sum(set(o_fine[j]) == set(o_coarse[j]) for j in range(len(qvecs))))
    return {"queries": len(qvecs), "identical_top_k": same,
            "changed": len(qvecs) - same,
            "verdict": "OK — truth moves with the buckets" if same < len(qvecs)
                       else "BROKEN — oracle did not change; the axis is not an axis"}


def assemble(n_buckets: int, k: int, limit: int, out_path: str) -> dict:
    """Build one axis point and write the .npz the measure_*.py scripts consume.

    Keys written: ``vecs``, ``bucket_ids``, ``qvecs``, ``q_bucket``, ``oracle``,
    ``meta`` (``meta[3]`` is k, as `measure_x.py` reads it), plus ``axis`` for
    provenance so a result can be traced to the point it came from.
    """
    meta = json.load(open(os.path.join(CORP, "meta.json")))
    cvec = np.load(os.path.join(CORP, "cvec.npy"), mmap_mode="r")
    qvec = np.load(os.path.join(CORP, "qvec.npy"))
    turns = meta["turns"]
    qids = turns["qid"]
    vec_idx = np.asarray(turns["vec_idx"], dtype=np.int64)

    bucket_ids = regroup(qids, n_buckets)
    if limit and limit < len(qids):
        # Subsample turns for the N axis, keeping bucket proportions.
        rng = np.random.default_rng(meta["seed"])
        sel = np.sort(rng.choice(len(qids), size=limit, replace=False))
        vec_idx, bucket_ids = vec_idx[sel], bucket_ids[sel]
        qids = [qids[i] for i in sel]

    vecs = np.ascontiguousarray(cvec[vec_idx])
    uniq = sorted(set(qids))
    first_turn = {q: int(np.flatnonzero(np.asarray(qids) == q)[0]) for q in uniq}
    order = [qq["qid"] for qq in meta["queries"]]
    keep = [i for i, q in enumerate(order) if q in first_turn]
    qvecs = qvec[keep]
    q_bucket = np.array([bucket_ids[first_turn[order[i]]] for i in keep], dtype=np.int64)

    oracle = build_oracle(vecs, bucket_ids, qvecs, q_bucket, k)
    sizes = np.bincount(bucket_ids)
    np.savez(out_path, vecs=vecs, bucket_ids=bucket_ids, qvecs=qvecs,
             q_bucket=q_bucket, oracle=oracle,
             meta=np.array([len(vecs), int(meta["dim"]), n_buckets, k]),
             axis=np.array([n_buckets, int(sizes.mean()), len(vecs)]))
    return {"n_buckets": n_buckets,
            "point": POINT_NAMES.get(n_buckets, f"b{n_buckets}"),
            "means": POINT_MEANING.get(n_buckets, ""),
            "tenants_per_bucket": round(500 / n_buckets, 1),
            "turns": int(len(vecs)), "queries": int(len(qvecs)),
            "bucket_min": int(sizes.min()), "bucket_mean": round(float(sizes.mean()), 1),
            "bucket_max": int(sizes.max()), "k": k, "out": out_path,
            "corpus_sha": hashlib.sha256(vecs[:64].tobytes()).hexdigest()[:16]}


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--buckets", type=int, help="one axis point; omit for all of AXIS_POINTS")
    ap.add_argument("--k", type=int, default=10)
    ap.add_argument("--limit", type=int, default=0, help="subsample turns (N axis)")
    ap.add_argument("--outdir", default=os.path.join(CORP, "axis"))
    ap.add_argument("--verify", action="store_true", help="run the per-point negative control only")
    args = ap.parse_args()

    if args.verify:
        meta = json.load(open(os.path.join(CORP, "meta.json")))
        cvec = np.load(os.path.join(CORP, "cvec.npy"), mmap_mode="r")
        qvec = np.load(os.path.join(CORP, "qvec.npy"))
        turns = meta["turns"]
        vecs = np.ascontiguousarray(cvec[np.asarray(turns["vec_idx"], dtype=np.int64)])
        print(json.dumps(verify_oracle_is_per_point(vecs, qvec, turns["qid"], args.k), indent=1))
        return

    os.makedirs(args.outdir, exist_ok=True)
    points = [args.buckets] if args.buckets else list(AXIS_POINTS)
    for nb in points:
        out = os.path.join(args.outdir, f"lme_b{nb}" + (f"_n{args.limit}" if args.limit else "") + ".npz")
        print(json.dumps(assemble(nb, args.k, args.limit, out)))


if __name__ == "__main__":
    main()
