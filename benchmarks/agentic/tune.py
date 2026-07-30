#!/usr/bin/env python3
"""Step 1 (spec 04): per-engine optimization on the DEV tuning set.

Sweeps each rival's HNSW params toward the shared objective — highest recall
while p99 <= 50 ms — and reports, per engine: the best config within the 50 ms
budget AND the best at any p99 (informative, spec 04 §A). recall@10 AND recall@50
(exact-vs-approx gap widens at depth). xyzDB is exact (recall 1.0, no dial): it
reports its recall + latency with cache + gravity co-location, no HNSW to tune.

Build params are swept by monkeypatching the adapter module constants before
each setup()+load() (no adapter edits); search params via the adapter's own arg.
Runs against ONE already-up engine container (the orchestrator brings it up at
8G, exclusive); loops every config inside (each does drop+rebuild+load+query).
"""
import argparse
import importlib
import json
import time

import numpy as np

import recall_harness as rh

BUDGET_MS = 50.0
KS = (10, 50)
KMAX = 50

GRIDS = {
    "pgvector": {"mod": "engines.pgvector_engine", "cls": "PgvectorEngine",
                 "build": {"HNSW_M": [16, 32, 48], "HNSW_EF_CONSTRUCTION": [64, 128, 200]},
                 "search": ("ef_search", [40, 100, 200, 400])},
    "qdrant":   {"mod": "engines.qdrant_engine", "cls": "QdrantEngine",
                 "build": {"HNSW_M": [16, 32, 48], "HNSW_EF_CONSTRUCT": [100, 200]},
                 "search": ("hnsw_ef", [128, 256, 512])},
    "chroma":   {"mod": "engines.chroma_engine", "cls": "ChromaEngine",
                 "build": {"HNSW_M": [16, 32], "HNSW_CONSTRUCTION_EF": [100, 200]},
                 "search": ("search_ef", [128, 256, 512])},
    "xyzdb":    {"mod": "engines.xyzdb_engine", "cls": "XyzdbEngine",
                 "build": {}, "search": (None, [None])},
}


def load_artifact(d):
    corpus = np.load(f"{d}/corpus_vectors.npy")
    meta = [json.loads(l) for l in open(f"{d}/corpus_meta.jsonl")]
    qvec = np.load(f"{d}/query_vectors.npy")
    qmeta = [json.loads(l) for l in open(f"{d}/query_meta.jsonl")]
    return corpus, meta, qvec, qmeta


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--engine", required=True)
    ap.add_argument("--emb_dir", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--dsn_pg", default="host=localhost port=5433 user=postgres password=bench dbname=mem")
    args = ap.parse_args()

    corpus, meta, qvec, qmeta = load_artifact(args.emb_dir)
    rid = [m["record_id"] for m in meta]
    scope_of = [m.get("scope", m.get("question_id")) for m in meta]
    by_scope = {}
    for i, s in enumerate(scope_of):
        by_scope.setdefault(s, []).append(i)
    by_scope = {s: np.array(v, dtype=np.int64) for s, v in by_scope.items()}
    q_scope = [m.get("scope", m.get("question_id")) for m in qmeta]
    Q = qvec.shape[0]
    # oracle top-KMAX per query (exact), over the query's scope
    ora = [[rid[int(r)] for r in rh.exact_topk(qvec[qi], corpus, by_scope[q_scope[qi]], KMAX)]
           for qi in range(Q)]
    load_meta = [{"record_id": rid[i], "scope": scope_of[i]} for i in range(len(meta))]

    g = GRIDS[args.engine]
    mod = importlib.import_module(g["mod"])
    cls = getattr(mod, g["cls"])
    build_keys = list(g["build"].keys())
    build_vals = [g["build"][k] for k in build_keys]
    sparam, svals = g["search"]

    def build_combos():
        import itertools
        if build_keys:
            return list(itertools.product(*build_vals))
        return [()]

    results = []
    for combo in build_combos():
        for k, v in zip(build_keys, combo):
            setattr(mod, k, v)                      # monkeypatch build constant
        for sv in svals:
            kw = {}
            if sparam and sv is not None:
                kw[sparam] = sv
            if args.engine == "pgvector":
                eng = cls(args.dsn_pg, **kw)
            else:
                eng = cls(**kw)
            try:
                eng.setup()
                t0 = time.perf_counter(); eng.load(corpus, load_meta); load_s = time.perf_counter() - t0
                got = [None] * Q
                lat = []
                for rep in range(2):                # 1 warmup + 1 measured
                    for qi in range(Q):
                        t = time.perf_counter()
                        res = eng.query(qvec[qi], q_scope[qi], KMAX)
                        dt = time.perf_counter() - t
                        if rep == 1:
                            lat.append(dt)
                            if got[qi] is None:
                                got[qi] = res
                L = np.array(lat) * 1000.0
                rec = {k: float(np.mean([len(set(got[i][:k]) & set(ora[i][:k])) / k for i in range(Q)])) for k in KS}
                row = {"engine": args.engine,
                       "build": dict(zip(build_keys, combo)),
                       sparam if sparam else "search": sv,
                       "recall@10": round(rec[10], 4), "recall@50": round(rec[50], 4),
                       "p50_ms": round(float(np.percentile(L, 50)), 2),
                       "p99_ms": round(float(np.percentile(L, 99)), 2),
                       "load_s": round(load_s, 1)}
                results.append(row)
                print(f"  {row['build']} {sparam}={sv}: r@10={rec[10]:.4f} r@50={rec[50]:.4f} "
                      f"p50={row['p50_ms']} p99={row['p99_ms']}ms", flush=True)
            except Exception as ex:
                print(f"  {dict(zip(build_keys, combo))} {sparam}={sv}: FAIL {type(ex).__name__}: {ex}", flush=True)
            finally:
                if hasattr(eng, "close"):
                    try: eng.close()
                    except Exception: pass

    with open(args.out, "a") as f:
        for r in results:
            f.write(json.dumps(r) + "\n")

    # pick: best recall@10 within budget, and best recall@10 at any p99
    within = [r for r in results if r["p99_ms"] <= BUDGET_MS]
    def best(rows):
        return max(rows, key=lambda r: (r["recall@10"], r["recall@50"], -r["p99_ms"])) if rows else None
    b_budget = best(within); b_any = best(results)
    print(f"\n=== {args.engine} PICK ===")
    print(f"  best within p99<={BUDGET_MS}ms: {b_budget}")
    print(f"  best at any p99:               {b_any}")


if __name__ == "__main__":
    main()
