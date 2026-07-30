"""Unit test: tie-aware recall gives 1.0 on a hand-built score-tie case where
id-set overlap would falsely report < 1.0. Run: ./.venv/bin/python test_tie_aware.py
"""
import numpy as np
from recall_harness import tie_aware_recall, kth_oracle_score


def main():
    # 2-D bucket, query = e0. Two distinct ids share the EXACT cutoff score.
    q = np.array([1.0, 0.0], dtype=np.float32)
    vecs = np.array([
        [1.0, 0.0],    # id 0: cos 1.0
        [0.8, 0.6],    # id 1: cos 0.8   ┐ tie at the k=2 cutoff
        [0.8, -0.6],   # id 2: cos 0.8   ┘
    ], dtype=np.float32)
    k = 2
    cut = kth_oracle_score(q, vecs, k)          # = 0.8 (2nd largest)
    assert abs(cut - 0.8) < 1e-6, cut

    # Oracle (argsort) top-2 ids = [0, 1]. The engine legitimately returns the
    # OTHER tied id: [0, 2] — identical scores, different id.
    engine_returned = [0, 2]

    # id-set overlap would be |{0,2} ∩ {0,1}| / 2 = 0.5 — a FALSE miss.
    idset = len(set(engine_returned) & {0, 1}) / k
    assert abs(idset - 0.5) < 1e-9, f"id-set sanity {idset}"

    # tie-aware: both returned scores (1.0, 0.8) reach the 0.8 cutoff → 1.0.
    ta = tie_aware_recall(q, engine_returned, vecs, cut, k)
    assert abs(ta - 1.0) < 1e-9, f"tie-aware recall must be 1.0, got {ta}"

    # And a genuine miss still scores < 1.0: return id 0 + a below-cutoff phantom.
    vecs2 = np.vstack([vecs, [[0.5, 0.866]]]).astype(np.float32)  # id 3: cos 0.5 < 0.8
    miss = tie_aware_recall(q, [0, 3], vecs2, cut, k)
    assert abs(miss - 0.5) < 1e-9, f"a real miss must score 0.5, got {miss}"

    print(f"tie-aware OK: cut={cut:.3f}  id-set(false)={idset}  tie-aware={ta}  real-miss={miss}")


if __name__ == "__main__":
    main()
