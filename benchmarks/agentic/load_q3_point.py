#!/usr/bin/env python3
"""Load one axis point of the frozen corpus into whichever engine is up.

WHY IT IS ITS OWN SCRIPT
------------------------
The first Q3 sweep loaded its lobes from a throwaway script in /tmp that spoke
xyTalk directly. That put the DDL — which lobes, which axis, which fields — in a
file nobody would ever read again, and it drifted from `adapters.py`, where the
same decisions are made for every other scenario. Everything here goes through
the adapters, so the sweep measures the same load the rest of the matrix does.

WHAT A "POINT" IS
-----------------
The locality axis (`bucket_axis.py`): 500 → 50 → 5 → 1 users per bucket. Point 1
is the pool — one bucket holding the whole corpus, which is the shape Q3-pool
asks about ("of everything, filtered to this category, what is closest?").

For xyzDB the satellite axis is a per-lobe declaration and cannot be changed
after the first write, so a sweep over four cardinalities is FOUR loads into four
lobes. The rivals take one load with four payload fields — that asymmetry is a
finding, not an accident, and it is what `setup_cost` records.
"""
import argparse
import json
import sys
import time

import numpy as np

sys.path.insert(0, "/Applications/Projects/Tunolabs/xyz/xyzdb/examples/client/python")
sys.path.insert(0, "/Applications/Projects/Tunolabs/xyz/xyzdb/benchmarks/agentic")

import adapters  # noqa: E402
import metadata_gen as mg  # noqa: E402
from bucket_axis import load_point  # noqa: E402


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--engine", required=True,
                    choices=["xyzdb", "qdrant", "pgvector", "chroma"])
    ap.add_argument("--store", default="corpora/lme/axis")
    ap.add_argument("--point", type=int, default=1, help="users per bucket (1 = pool)")
    ap.add_argument("--n", type=int, default=0, help="rows to load; 0 = all")
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=0, help="0 = the engine's default port")
    ap.add_argument("--out", default="")
    args = ap.parse_args()

    c = load_point(args.store, args.point)
    n = args.n or len(c["vecs"])
    vecs = np.asarray(c["vecs"][:n])
    bids = c["bucket_ids"][:n]
    meta = {k: v[:n] for k, v in c["fields"].items()}

    cls = {"xyzdb": adapters.XyzdbAdapter, "qdrant": adapters.QdrantAdapter,
           "pgvector": adapters.PgvectorAdapter, "chroma": adapters.ChromaAdapter}[args.engine]
    kw = {"host": args.host, "dim": vecs.shape[1]}
    if args.port:
        kw["port"] = args.port

    rows = []
    if args.engine == "xyzdb":
        # One lobe per cardinality: the axis is declared, and a declaration made
        # before the first write cannot be revised afterwards.
        for card in mg.CARDINALITIES:
            field = f"cat{card}"
            t0 = time.perf_counter()
            a = cls(satellite=field, lobe=f"mem_{field}", **kw)
            a.load(vecs, bids, meta=meta)
            dt = time.perf_counter() - t0
            a.close()
            rows.append({"engine": args.engine, "lobe": f"mem_{field}", "axis": field,
                         "rows": int(n), "load_s": round(dt, 1),
                         "rows_per_s": int(n / dt), "setup_cost": a.setup_cost})
            print(json.dumps(rows[-1]), flush=True)
    else:
        # One load; every catN travels as an ordinary payload/column field.
        t0 = time.perf_counter()
        a = cls(**kw)
        a.load(vecs, bids, hnsw=None, meta=meta)
        dt = time.perf_counter() - t0
        a.close()
        rows.append({"engine": args.engine, "rows": int(n), "load_s": round(dt, 1),
                     "rows_per_s": int(n / dt),
                     "setup_cost": getattr(a, "setup_cost", None)})
        print(json.dumps(rows[-1]), flush=True)

    if args.out:
        with open(args.out, "a") as f:
            for r in rows:
                f.write(json.dumps(r) + "\n")


if __name__ == "__main__":
    main()
