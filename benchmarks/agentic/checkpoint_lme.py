#!/usr/bin/env python3
"""Cheap checkpoints on Corpus A before any full run (prompt §Checkpoints).

1. Geometry / retrieval signal: query→GT-turn cosine vs query→distractor cosine. If the GT
   turns aren't clearly more similar than distractors, the embed/corpus is broken. (For
   LongMemEval a question bucket is a haystack, NOT a tight cluster — so the meaningful check
   is query-vs-answer SNR, not bucket intra/inter.)
2. Exact-retrieval recall@{1,5,10} (session-level, INV-1): the ceiling xyzDB-exact achieves;
   should land near the doc's ~0.96 — validates corpus + recall_harness together.
3. Precision gap f32-reduce vs f64-reduce → the size TIE_TOL must clear.
"""
import json
import os
import numpy as np
from collections import defaultdict
import recall_harness as rh

# Corpus dir — same default and override (BENCH_CORP) as measure_lme.py.
D = os.environ.get("BENCH_CORP",
                   os.path.join(os.path.dirname(os.path.abspath(__file__)), "corpora", "lme"))


def main():
    cvec = np.load(f"{D}/cvec.npy")
    qvec = np.load(f"{D}/qvec.npy")
    meta = json.load(open(f"{D}/meta.json"))
    T = meta["turns"]
    n = len(T["qid"])
    # index turns by question bucket
    by_q = defaultdict(list)   # qid -> list of turn-row indices
    for i in range(n):
        by_q[T["qid"][i]].append(i)
    queries = meta["queries"]

    rec = {k: [] for k in (1, 5, 10)}
    gt_cos, distr_cos, gaps = [], [], []
    for qi, q in enumerate(queries):
        rows = by_q[q["qid"]]
        vidx = np.array([T["vec_idx"][r] for r in rows])
        bvecs = cvec[vidx]
        qv = qvec[qi]
        s = rh.exact_scores(qv, bvecs)
        gt = set(q["gt"])
        is_gt = np.array([T["sid"][r] in gt for r in rows])
        if is_gt.any():
            gt_cos.append(float(s[is_gt].max()))          # best GT-turn similarity
        if (~is_gt).any():
            distr_cos.append(float(s[~is_gt].mean()))     # mean distractor similarity
        # exact top-k → session recall
        order = np.argsort(-s)
        turn_session = {r: T["sid"][r] for r in rows}
        ordered_rows = [rows[j] for j in order]
        for k in (1, 5, 10):
            rec[k].append(rh.session_recall_at_k(ordered_rows, turn_session, gt, k))
        if qi < 40:
            gaps.append(rh.measure_precision_gap(qv, bvecs)[0])

    print("=== Corpus A (LongMemEval) checkpoints ===")
    print(f"buckets(question_id)={len(by_q)}  unique_vecs={len(cvec)}  queries={len(queries)}")
    nan_buckets = int(np.isnan(gt_cos).sum() + np.isnan(distr_cos).sum())
    print(f"[1] geometry: query→best-GT-turn cos median={np.nanmedian(gt_cos):.3f}  "
          f"query→distractor cos median={np.nanmedian(distr_cos):.3f}  "
          f"(SNR ok if GT >> distractor)  [nan/degenerate embeds: {nan_buckets}]")
    print(f"[2] EXACT session-recall (xyzDB ceiling): "
          + "  ".join(f"@{k}={np.mean(rec[k]):.3f}" for k in (1, 5, 10)))
    mx = max(gaps)
    print(f"[3] precision gap f32-vs-f64: max over {len(gaps)} buckets = {mx:.2e}  "
          f"→ TIE_TOL debe ser ≥ {mx:.1e} (actual {rh.TIE_TOL:.0e})")


if __name__ == "__main__":
    main()
