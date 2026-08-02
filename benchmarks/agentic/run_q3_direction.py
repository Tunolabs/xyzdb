#!/usr/bin/env python3
"""Q3-pool selectivity sweep across engines — DIRECTION ONLY, not publishable numbers.

WHAT THIS IS
------------
The flagship question: "of this pool's memories about billing, which is most like
this?" Equality filter plus similarity, swept from 50% down to 0.1% selectivity by
varying the field's cardinality (cat2 / cat10 / cat100 / cat1000).

WHAT IT IS NOT
--------------
Publishable latency. The image here is the arm64 variant — a first-class build, not
a crippled one: the `target-cpu=x86-64-v3` flag is target-scoped to the x86 Linux
triple precisely so it never reaches aarch64, and on ARM there is nothing for it to
widen (the scorer's `f32x8` maps to AVX2 on x86 and to NEON here). What does not
carry across is the COMPARISON: absolute milliseconds from one ISA cannot be quoted
against the other, and Docker on this Mac runs in a VM with 16 KB pages against the
x86 box's 4 KB, which shifts RSS. **The SHAPE of the curve mostly does carry**, and
the shape is the claim: does cost fall as the filter tightens, or rise?

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
import os
import statistics
import sys
import time

import numpy as np

_HERE = os.path.dirname(os.path.abspath(__file__))
for _p in ("/bench", "/client", _HERE,
           os.path.join(_HERE, "..", "..", "examples", "client", "python")):
    if os.path.isdir(_p) and _p not in sys.path:
        sys.path.insert(0, _p)

import adapters                    # noqa: E402
import metadata_gen as mg          # noqa: E402
import recall_harness as rh        # noqa: E402
from bucket_axis import load_point  # noqa: E402

QDRANT_FULL_SCAN_DEFAULT = 10_000
# The collection `adapters.QdrantAdapter` actually creates. It was hardcoded to
# "bench" here and in the route gate, so every qdrant cell died on a 404 that read
# as "qdrant is down". When a default is wrong in one place, the same name is worth
# grepping for across the harness before closing it — this is the second copy.
QDRANT_COLLECTION = os.environ.get("BENCH_QDRANT_COLLECTION", "mem")


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


def run_xyz(port, lobe, field, value, k, qvecs, truths, vecs, repeats, bucket="0"):
    from xyzdb_minimal import connect
    db = connect(adapters.DEFAULT_ENGINE_HOST, port, timeout=300.0)
    lat, rec = [], []
    for j, qv in enumerate(qvecs):
        qs = json.dumps([float(x) for x in qv])
        # `| SHAPE {id}` projects the 1024-float embedding OUT of the RESPONSE.
        # Without it xyzDB serialises ~20 KB of JSON per returned record while the
        # qdrant and pg clients hand back ids only — the first run measured that
        # serialisation and called it search latency.
        stmt = (f'SCAN "{lobe}" WHERE bucket = "{bucket}" AND {field} = {value} '
                f'| NEAREST {k} BY emb TO {qs} USING cosine | SHAPE {{id}}')
        r, ms = timed(lambda: db.execute(stmt), repeats)
        ids = [int(x["id"][1:]) for x in r.get("records", []) if "id" in x]
        cut = float(rh.exact_scores(qv, vecs[truths[j]]).min())
        lat.append(ms)
        rec.append(rh.tie_aware_recall(qv, ids, vecs, cut, k))
    db.close()
    return lat, rec, "gravity+satellite (exact, bounded)"


def run_qdrant(coll, field, value, k, qvecs, truths, vecs, repeats, bucket="0"):
    from qdrant_client import QdrantClient, models
    cl = QdrantClient(host=adapters.DEFAULT_ENGINE_HOST, port=6333)
    # Both predicates, or the bounded set is not the cell's: at coarse points the
    # bucket is what makes gravity comparable across engines.
    flt = models.Filter(must=[
        models.FieldCondition(key=field, match=models.MatchValue(value=int(value))),
        models.FieldCondition(key="bucket", match=models.MatchValue(value=str(bucket)))])
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


def run_pg(host, field, value, k, qvecs, truths, vecs, repeats, port=5432, bucket="0"):
    """pgvector over a PERSISTENT psycopg2 connection.

    The first version shelled out to `docker exec … psql` once per timed repeat,
    so every measurement included spawning a container exec and a fresh psql
    process — tens of milliseconds of process startup reported as query latency,
    against rivals measured over a connection that was already open. The warmup
    discard did not help: the cost was paid on every repeat, not just the first.

    It also could not run from inside the harness image, which has no docker CLI —
    the port to a pinned client image is what surfaced it.
    """
    import psycopg2
    conn = psycopg2.connect(host=host, port=port, user="postgres",
                            password="bench", dbname="postgres", connect_timeout=30)
    conn.autocommit = True
    cur = conn.cursor()
    lat, rec = [], []
    mech = None
    for j, qv in enumerate(qvecs):
        vec = "[" + ",".join(f"{float(x):.6f}" for x in qv) + "]"
        sql = (f"SELECT gid FROM items WHERE bucket = {int(bucket)} AND {field} = {value} "
               f"ORDER BY emb <=> %s LIMIT {k}")

        def call():
            cur.execute(sql, (vec,))
            return [int(r[0]) for r in cur.fetchall()]

        ids, ms = timed(call, repeats)
        if mech is None:
            cur.execute("EXPLAIN (COSTS OFF) " + sql, (vec,))
            plan = "\n".join(r[0] for r in cur.fetchall())
            mech = ("index scan" if "Index Scan" in plan else
                    "seq scan" if "Seq Scan" in plan else "other")
            rels = {ln.split()[-1] for ln in plan.splitlines() if "Scan on" in ln}
            mech += f" [{len(rels)} relation(s)]"
        cut = float(rh.exact_scores(qv, vecs[truths[j]]).min())
        lat.append(ms)
        rec.append(rh.tie_aware_recall(qv, ids, vecs, cut, k))
    cur.close()
    conn.close()
    return lat, rec, mech


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--store", default="corpora/lme/axis")
    ap.add_argument("--point", type=int, default=1,
                    help="locality point (1=pool, 5=big_group, 50=group, 500=user)")
    ap.add_argument("--cardinalities", default="",
                    help="comma list; empty = every catN (the old full sweep)")
    # 0 = the whole point, matching `load_q3_point.py`'s own default. They MUST
    # agree: the oracle is computed here over the first `n` rows while the engine
    # holds whatever the loader put in, so two different defaults would score a
    # 246,738-row engine against a 50,000-row truth and report the gap as recall.
    ap.add_argument("--n", type=int, default=0, help="rows of the slice; 0 = all")
    ap.add_argument("--queries", type=int, default=20)
    ap.add_argument("--repeats", type=int, default=3)
    ap.add_argument("--k", type=int, default=10)
    # The container port. A host-native binary on some other port is not the
    # artefact under test — `run_q3_direction.sh` brings the engine up through
    # lib_docker.sh so every engine in the sweep is a pinned container.
    ap.add_argument("--xyz-port", type=int, default=2505)
    ap.add_argument("--engines", default="xyzdb,qdrant,pgvector")
    ap.add_argument("--out", default="")
    ap.add_argument("--exclusive", action="store_true",
                    help="assert the caller left ONLY this engine running")
    args = ap.parse_args()

    c = load_point(args.store, args.point)
    n = args.n or len(c["vecs"])
    vecs = np.asarray(c["vecs"][:n])
    fields = {k2: v[:n] for k2, v in c["fields"].items()}
    qvecs = c["qvecs"][:args.queries]
    engines = args.engines.split(",")
    rows = []

    cards = ([int(x) for x in args.cardinalities.split(",") if x.strip()]
             or list(mg.CARDINALITIES))
    for card in cards:
        field = f"cat{card}"
        rows_per_sat = n / card
        if mg.is_degenerate(n, card, args.k):
            rows.append({"axis": field, "skipped": "degenerate",
                         "rows_per_satellite": round(rows_per_sat, 1)})
            continue
        # At coarse points there is more than one bucket, so the cell must ask
        # inside the bucket its query vector lives in — otherwise the bounded set is
        # not the one grid.py computed and the cell measures a different question.
        bucket = str(int(c["q_bucket"][0])) if "q_bucket" in c else "0"
        in_bucket = np.flatnonzero(c["bucket_ids"][:n] == int(bucket))
        value = int(fields[field][in_bucket[0]]) if len(in_bucket) else int(fields[field][0])
        sel = np.array([i for i in in_bucket if fields[field][i] == value], dtype=int)
        truths = [truth_for(qv, vecs, sel, args.k) for qv in qvecs]
        for eng in engines:
            fn = {"xyzdb": lambda: run_xyz(args.xyz_port, f"mem_{field}", field, value, args.k,
                                           qvecs, truths, vecs, args.repeats, bucket),
                  "qdrant": lambda: run_qdrant(QDRANT_COLLECTION, field, value, args.k,
                                               qvecs, truths, vecs, args.repeats, bucket),
                  "pgvector": lambda: run_pg(adapters.DEFAULT_ENGINE_HOST, field, value,
                                             args.k, qvecs, truths, vecs,
                                             args.repeats, bucket=bucket)}[eng]
            try:
                lat, rec, mech = fn()
                rows.append({
                    "axis": field, "selectivity": round(1 / card, 4),
                    "rows_per_satellite": round(rows_per_sat, 1),
                    "engine": eng, "mechanism": mech,
                    "p50_ms": round(statistics.median(lat), 2),
                    "recall": round(float(np.mean(rec)), 4),
                    "n": n, "queries": len(qvecs), "point": args.point,
                    "bounded_set": int(len(sel)),
                    "direction_only": True, "engine_exclusive": args.exclusive,
                    "why": "arm64 image + 16KB pages; the publishable one is x86-64-v3",
                    "xyzdb_image": os.environ.get("XYZDB_IMG", "unset"),
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
