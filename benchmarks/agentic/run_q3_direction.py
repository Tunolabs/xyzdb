#!/usr/bin/env python3
"""Q3-pool selectivity sweep across engines — DIRECTION ONLY, not publishable numbers.

WHAT THIS IS
------------
The flagship question: "of this pool's memories about billing, which is most like
this?" Equality filter plus similarity, swept from 50% down to 0.1% selectivity by
varying the field's cardinality (cat2 / cat10 / cat100 / cat1000).

WHAT IT IS NOT
--------------
Publishable latency. This machine is arm64 and the publishable image builds with
`target-cpu=x86-64-v3` only on x86, so xyzDB here is literally a different binary.
Docker also runs on a VM with 16 KB pages against 4 KB on the x86 box, which shifts
RSS. **Absolute milliseconds do not transfer. The SHAPE of the curve mostly does**,
and the shape is the claim: does cost fall as the filter tightens, or rise?

Every row is stamped `direction_only: true` so a number from here can never be
mistaken for a matrix cell.

THE MECHANISM COLUMN IS NOT DECORATION
--------------------------------------
qdrant abandons its HNSW graph below `full_scan_threshold` (10,000 by default) and
scans exactly. Measured: this sweep CROSSES that threshold between cat10 (24,673
points at full scale) and cat100 (2,467). So in the tight cells qdrant is not a
degrading ANN — it is an exact scanner, and reporting those cells as "filtered HNSW"
would name the wrong mechanism. Each row therefore carries which mechanism actually
ran, and a comparison that ignores that column is comparing two different things.
"""
import argparse
import json
import statistics
import subprocess
import sys
import time

import numpy as np

sys.path.insert(0, "/Applications/Projects/Tunolabs/xyz/xyzdb/examples/client/python")
sys.path.insert(0, "/Applications/Projects/Tunolabs/xyz/xyzdb/benchmarks/agentic")

import metadata_gen as mg          # noqa: E402
import recall_harness as rh        # noqa: E402
from bucket_axis import load_point  # noqa: E402

QDRANT_FULL_SCAN_DEFAULT = 10_000


def truth_for(qvec, vecs, rows, k):
    """Independent oracle over the filtered subset — the same one the gate uses."""
    scores = rh.exact_scores(qvec, vecs[rows])
    return [int(rows[i]) for i in np.argsort(-scores, kind="stable")[:k]]


def timed(fn, repeats, warmup=1):
    """Median-of-repeats wall time in ms, plus the last result.

    The warmup passes are run and DISCARDED. The first call to any of the three
    engines pays cold cache and, for pg, a fresh psql process; including it turns a
    latency comparison into a startup comparison.
    """
    for _ in range(warmup):
        fn()
    out, lat = None, []
    for _ in range(repeats):
        t0 = time.perf_counter()
        out = fn()
        lat.append((time.perf_counter() - t0) * 1e3)
    return out, statistics.median(lat)


def run_xyz(port, lobe, field, value, k, qvecs, truths, vecs, repeats):
    from xyzdb_minimal import connect
    db = connect("127.0.0.1", port, timeout=300.0)
    lat, rec = [], []
    for j, qv in enumerate(qvecs):
        qs = json.dumps([float(x) for x in qv])
        # `| SHAPE {id}` projects the 1024-float embedding OUT of the RESPONSE.
        # Without it xyzDB serialises ~20 KB of JSON per returned record while the
        # qdrant and pg clients hand back ids only — the first run measured that
        # serialisation and called it search latency.
        stmt = (f'SCAN "{lobe}" WHERE bucket = "0" AND {field} = {value} '
                f'| NEAREST {k} BY emb TO {qs} USING cosine | SHAPE {{id}}')
        r, ms = timed(lambda: db.execute(stmt), repeats)
        ids = [int(x["id"][1:]) for x in r.get("records", []) if "id" in x]
        cut = float(rh.exact_scores(qv, vecs[truths[j]]).min())
        lat.append(ms)
        rec.append(rh.tie_aware_recall(qv, ids, vecs, cut, k))
    db.close()
    return lat, rec, "gravity+satellite (exact, bounded)"


def run_qdrant(coll, field, value, k, qvecs, truths, vecs, repeats):
    from qdrant_client import QdrantClient, models
    cl = QdrantClient(host="127.0.0.1", port=6333)
    flt = models.Filter(must=[models.FieldCondition(
        key=field, match=models.MatchValue(value=int(value)))])
    matched = cl.count(collection_name=coll, count_filter=flt, exact=True).count
    thr = getattr(cl.get_collection(coll).config.hnsw_config,
                  "full_scan_threshold", QDRANT_FULL_SCAN_DEFAULT) or QDRANT_FULL_SCAN_DEFAULT
    mech = ("BRUTE FORCE (exact — below its own full_scan_threshold)"
            if matched < thr else "HNSW graph (filtered traversal)")
    lat, rec = [], []
    for j, qv in enumerate(qvecs):
        r, ms = timed(lambda: cl.query_points(
            collection_name=coll, query=qv.tolist(), query_filter=flt, limit=k).points, repeats)
        ids = [int(p.id) for p in r]
        cut = float(rh.exact_scores(qv, vecs[truths[j]]).min())
        lat.append(ms)
        rec.append(rh.tie_aware_recall(qv, ids, vecs, cut, k))
    return lat, rec, f"{mech} [{matched} pts vs threshold {thr}]"


def run_pg(container, field, value, k, qvecs, truths, vecs, repeats, dim):
    lat, rec = [], []
    mech = None
    for j, qv in enumerate(qvecs):
        vec = "[" + ",".join(f"{float(x):.6f}" for x in qv) + "]"
        sql = (f"SELECT gid FROM items WHERE bucket = 0 AND {field} = {value} "
               f"ORDER BY emb <=> '{vec}' LIMIT {k};")
        def call():
            r = subprocess.run(
                ["docker", "exec", "-i", "-e", "PGPASSWORD=bench", container,
                 "psql", "-U", "postgres", "-tAq", "-f", "-"],
                input=sql, capture_output=True, text=True, timeout=300)
            return [int(x) for x in r.stdout.split() if x.strip().isdigit()]
        ids, ms = timed(call, repeats)
        if mech is None:
            ex = subprocess.run(
                ["docker", "exec", "-i", "-e", "PGPASSWORD=bench", container,
                 "psql", "-U", "postgres", "-tAq", "-f", "-"],
                input="EXPLAIN (COSTS OFF) " + sql, capture_output=True, text=True, timeout=300)
            plan = ex.stdout
            mech = ("index scan" if "Index Scan" in plan else
                    "seq scan" if "Seq Scan" in plan else "other")
            mech += f" [{len({l.split()[-1] for l in plan.splitlines() if 'Scan on' in l})} relation(s)]"
        cut = float(rh.exact_scores(qv, vecs[truths[j]]).min())
        lat.append(ms)
        rec.append(rh.tie_aware_recall(qv, ids, vecs, cut, k))
    return lat, rec, mech


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--store", default="corpora/lme/axis")
    ap.add_argument("--n", type=int, default=50_000, help="rows of the slice")
    ap.add_argument("--queries", type=int, default=20)
    ap.add_argument("--repeats", type=int, default=3)
    ap.add_argument("--k", type=int, default=10)
    ap.add_argument("--xyz-port", type=int, default=25000)
    ap.add_argument("--engines", default="xyzdb,qdrant,pgvector")
    ap.add_argument("--out", default="")
    ap.add_argument("--exclusive", action="store_true",
                    help="assert the caller left ONLY this engine running")
    args = ap.parse_args()

    c = load_point(args.store, 1)          # pool: one bucket, the whole corpus
    vecs = np.asarray(c["vecs"][:args.n])
    fields = {k2: v[:args.n] for k2, v in c["fields"].items()}
    qvecs = c["qvecs"][:args.queries]
    engines = args.engines.split(",")
    rows = []

    for card in mg.CARDINALITIES:
        field = f"cat{card}"
        rows_per_sat = args.n / card
        if mg.is_degenerate(args.n, card, args.k):
            rows.append({"axis": field, "skipped": "degenerate",
                         "rows_per_satellite": round(rows_per_sat, 1)})
            continue
        value = int(fields[field][0])
        sel = np.flatnonzero(fields[field] == value)
        truths = [truth_for(qv, vecs, sel, args.k) for qv in qvecs]
        for eng in engines:
            fn = {"xyzdb": lambda: run_xyz(args.xyz_port, f"mem_{field}", field, value, args.k,
                                           qvecs, truths, vecs, args.repeats),
                  "qdrant": lambda: run_qdrant("bench", field, value, args.k,
                                               qvecs, truths, vecs, args.repeats),
                  "pgvector": lambda: run_pg("bench-pg", field, value, args.k,
                                             qvecs, truths, vecs, args.repeats,
                                             vecs.shape[1])}[eng]
            try:
                lat, rec, mech = fn()
                rows.append({
                    "axis": field, "selectivity": round(1 / card, 4),
                    "rows_per_satellite": round(rows_per_sat, 1),
                    "engine": eng, "mechanism": mech,
                    "p50_ms": round(statistics.median(lat), 2),
                    "recall": round(float(np.mean(rec)), 4),
                    "n": args.n, "queries": len(qvecs),
                    "direction_only": True, "engine_exclusive": args.exclusive,
                    "why": "arm64 + 16KB pages; the publishable image is x86-64-v3",
                })
            except Exception as e:
                rows.append({"axis": field, "engine": eng, "error": str(e)[:200]})
            print(json.dumps(rows[-1]), flush=True)

    if args.out:
        with open(args.out, "a") as f:
            for r in rows:
                f.write(json.dumps(r) + "\n")


if __name__ == "__main__":
    main()
