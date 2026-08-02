#!/usr/bin/env python3
"""P6, the expensive half: does a two-level pg partition tree survive real data?

WHAT P6 LEFT OPEN
-----------------
P6 proved the DDL is buildable and prunes to one leaf — on EMPTY tables. It
measured 50,000 leaves in 145.7s of DDL and a plan that touched one relation.
That refuted "you cannot express sub-gravity in pg", and it left the half that
actually decides viability untouched: rows, indexes, and a plan over data.

Three things only appear once the data is in:

1. **The per-leaf index.** A partitioned table's vector index is created per leaf.
   `n` leaves means `n` HNSW builds — and an HNSW over 5 rows is not a graph, it is
   overhead with a name.
2. **The planner's choice.** Pruning to one leaf does not mean using its index; a
   leaf small enough is cheaper to seq-scan, which is a different mechanism from
   the one this arm credits pg with. Only EXPLAIN over real rows tells them apart.
3. **What it costs to get there.** Load time into `n` leaves is the number xyzDB's
   two declaration lines are compared against.

DIRECTION ONLY on this machine, like everything local. But the STRUCTURAL results
— does it build, does it prune, does it use the index — carry.
"""
import argparse
import json
import os
import re
import sys
import time

import numpy as np

sys.path.insert(0, "/bench")
sys.path.insert(0, "/client")
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import adapters  # noqa: E402
from bucket_axis import load_point  # noqa: E402


def connect(host, port):
    import psycopg2
    c = psycopg2.connect(host=host, port=port, user="postgres", password="bench",
                         dbname="postgres", connect_timeout=30)
    c.autocommit = True
    return c


def build_tree(cur, dim, card, ddl_timeout):
    """One `bucket` level over a `catN` level: the sub-gravity shape, in pg's terms.

    LIST partitioning on both levels. Every statement goes over the connection
    rather than as an argv element — an earlier version passed the DDL as a
    command-line argument and died at 5,000 leaves with "argument list too long",
    which the data itself gave away: the failure was FASTER than the success it was
    supposed to beat.
    """
    t0 = time.perf_counter()
    cur.execute("DROP TABLE IF EXISTS items CASCADE")
    cur.execute(f"""CREATE TABLE items (
        gid int, bucket int, cat int, emb vector({dim})
    ) PARTITION BY LIST (bucket)""")
    cur.execute("CREATE TABLE items_b0 PARTITION OF items FOR VALUES IN (0) "
                "PARTITION BY LIST (cat)")
    for v in range(card):
        cur.execute(f"CREATE TABLE items_b0_c{v} PARTITION OF items_b0 FOR VALUES IN ({v})")
    return round(time.perf_counter() - t0, 1)


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--host", default=adapters.DEFAULT_ENGINE_HOST)
    ap.add_argument("--port", type=int, default=5432)
    ap.add_argument("--store", default="corpora/lme/axis")
    ap.add_argument("--point", type=int, default=1)
    ap.add_argument("--cardinality", type=int, default=100)
    ap.add_argument("--n", type=int, default=0, help="rows; 0 = the whole point")
    ap.add_argument("--ddl-timeout", type=int, default=900)
    ap.add_argument("--out", default="")
    args = ap.parse_args()

    c = load_point(args.store, args.point)
    n = args.n or len(c["vecs"])
    vecs = np.asarray(c["vecs"][:n])
    cats = c["fields"][f"cat{args.cardinality}"][:n]
    dim = vecs.shape[1]

    conn = connect(args.host, args.port)
    cur = conn.cursor()
    cur.execute("CREATE EXTENSION IF NOT EXISTS vector")

    row = {"engine": "pgvector", "shape": "two-level LIST partition",
           "leaves": args.cardinality, "rows": int(n), "dim": dim,
           "direction_only": True}

    row["ddl_s"] = build_tree(cur, dim, args.cardinality, args.ddl_timeout)
    row["ddl_statements"] = 2 + args.cardinality   # table + b0 + one per leaf

    # Load. COPY would be faster, but the arm compares the OPERATOR cost of the
    # shape, and every engine here is loaded through its ordinary client path.
    t0 = time.perf_counter()
    from psycopg2.extras import execute_values
    B = 2000
    for s in range(0, n, B):
        rows = [(i, 0, int(cats[i]), "[" + ",".join(f"{x:.6f}" for x in vecs[i]) + "]")
                for i in range(s, min(s + B, n))]
        execute_values(cur, "INSERT INTO items (gid, bucket, cat, emb) VALUES %s", rows)
    row["load_s"] = round(time.perf_counter() - t0, 1)
    row["load_rows_per_s"] = int(n / max(row["load_s"], 1e-9))

    # THE PLAN BEFORE ANY INDEX EXISTS. At small leaf sizes the planner prefers a
    # seq scan + sort, which means the per-leaf HNSW buys nothing there — so the
    # honest strong form for pg at that point of the axis is to partition and NOT
    # index. Taking this plan first turns that from an inference into a
    # measurement: if it matches the post-index plan, the index provably changed
    # nothing.
    qv = "[" + ",".join(f"{float(x):.6f}" for x in c["qvecs"][0]) + "]"
    val = int(cats[0])
    sql = (f"SELECT gid FROM items WHERE bucket = 0 AND cat = {val} "
           f"ORDER BY emb <=> %s LIMIT 10")

    def plan_now():
        cur.execute("EXPLAIN (ANALYZE, BUFFERS, COSTS OFF) " + sql, (qv,))
        pl = "\n".join(r[0] for r in cur.fetchall())
        rels = set(re.findall(r"Scan on (\S+)", pl))
        m = re.search(r"actual time=[\d.]+\.\.([\d.]+)", pl)
        return {"relations": sorted(rels), "leaves": len(rels),
                "used_index": ("Index Scan" in pl) or ("Index Only Scan" in pl),
                "seq_scan": "Seq Scan" in pl,
                "top_ms": float(m.group(1)) if m else None,
                "head": "\n".join(pl.split("\n")[:3])[:200]}

    row["plan_without_index"] = plan_now()

    # One HNSW per leaf — the cost the empty-table probe could not see.
    t0 = time.perf_counter()
    try:
        cur.execute("SET maintenance_work_mem = '512MB'")
        cur.execute("CREATE INDEX ON items USING hnsw (emb vector_cosine_ops)")
        row["index_s"] = round(time.perf_counter() - t0, 1)
        row["index_note"] = f"one HNSW per leaf: {args.cardinality} graphs, " \
                            f"~{n // max(args.cardinality,1)} rows each"
    except Exception as e:                                   # noqa: BLE001
        row["index_s"] = None
        row["index_error"] = str(e)[:200]

    # A partitioned PARENT holds no data, so pg_total_relation_size('items') is 0
    # — the first run reported "0 bytes" for 50,000 rows and 100 HNSW graphs. The
    # size lives in the leaves; sum them.
    cur.execute("""SELECT pg_size_pretty(sum(pg_total_relation_size(c.oid))::bigint)
                   FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                   WHERE n.nspname = 'public' AND c.relname LIKE 'items_b0_c%'""")
    row["size"] = cur.fetchone()[0]

    row["plan_with_index"] = plan_now()
    row["rows_per_leaf"] = n // max(args.cardinality, 1)

    wi, wo = row["plan_with_index"], row["plan_without_index"]
    row["index_changed_the_plan"] = (wi["used_index"] != wo["used_index"]) or \
                                    (wi["seq_scan"] != wo["seq_scan"])
    row["mechanism"] = ("HNSW graph on one leaf" if wi["used_index"] else
                        "EXACT scan of one leaf (seq scan + sort)")
    row["verdict"] = (
        "OK — pruned to one leaf AND used its index"
        if wi["leaves"] == 1 and wi["used_index"] and not wi["seq_scan"] else
        f"pruned to {wi['leaves']} leaf/leaves and did NOT use the index — at "
        f"{row['rows_per_leaf']} rows per leaf pg scans exactly, so this cell is "
        f"exact-vs-exact, not filtered-ANN")

    print(json.dumps(row, indent=1), flush=True)
    if args.out:
        with open(args.out, "a") as f:
            f.write(json.dumps(row) + "\n")
    cur.close()
    conn.close()


if __name__ == "__main__":
    main()
