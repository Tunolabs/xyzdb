#!/usr/bin/env python3
"""Prove each RIVAL took the mechanism we credit it with — the fairness half.

WHY THIS EXISTS
---------------
xyzDB has a route proof: the query bounded by the satellite completes while the same
query over the parent bucket is cut short, so the bounded path demonstrably ran. The
rivals need the same standard, and for a sharper reason than symmetry.

**qdrant switches to brute force below `full_scan_threshold`.** It is documented and
automatic: when a filter narrows the candidate set below that many points, qdrant
stops traversing the HNSW graph and scans exactly. At the pool point with cat1000 the
filter yields ~247 of 246,738 points — three orders of magnitude under the 10,000
default. Measuring that and publishing it as "qdrant's filtered HNSW at 0.1%
selectivity" would report its fallback under the name of its graph. It is the exact
mirror of the strawman we refused to build for ourselves, aimed the other way: a
number that flatters or damns the wrong mechanism.

**pg can prune to one partition and still seq-scan the leaf.** Pruning was verified
on empty tables in the P6 probe; with data resident the planner may prefer a
sequential scan over the leaf's HNSW index, which is a different mechanism from the
one the arm claims. EXPLAIN says which, and only EXPLAIN with data in says it truly.

**chroma has no second level to prove.** Its only co-location mechanism is more
collections, so the honest move is to state what it does rather than test for
something that is not there.

WHAT THIS FILE WILL AND WILL NOT DECIDE ON THIS MACHINE
-------------------------------------------------------
Structural results transfer: which mechanism ran, whether an index was used, whether
a filter fell back. Threshold verdicts do not. This is arm64 with Docker on a VM
using 16 KB pages against 4 KB on the publishable x86 box, which shifts RSS
measurably — so FIT/OOM near an envelope edge and build times near a timeout are
re-run there, never settled here. Latency likewise. Nothing in this file reports a
duration as a verdict.
"""
import argparse
import json


def qdrant_route(host, port, collection, field, value, expected_points):
    """Did qdrant use the payload index + graph, or fall back to an exact scan?

    Read from the collection's OWN configured threshold rather than assumed: the
    check is `filtered_points < full_scan_threshold`, which is qdrant's documented
    rule for abandoning the graph. Reporting the two numbers side by side makes the
    verdict checkable instead of asserted.
    """
    from qdrant_client import QdrantClient, models
    cl = QdrantClient(host=host, port=port)
    info = cl.get_collection(collection)
    hnsw = info.config.hnsw_config
    threshold = getattr(hnsw, "full_scan_threshold", None)
    flt = models.Filter(must=[models.FieldCondition(
        key=field, match=models.MatchValue(value=value))])
    matched = cl.count(collection_name=collection, count_filter=flt, exact=True).count
    indexed = {f: str(s) for f, s in (info.payload_schema or {}).items()}
    brute = threshold is not None and matched < threshold
    return {
        "engine": "qdrant", "field": field, "value": value,
        "points_matching_filter": matched,
        "full_scan_threshold": threshold,
        "payload_indexes": indexed,
        "field_is_indexed": field in indexed,
        "mechanism": "BRUTE FORCE (exact scan)" if brute else "HNSW graph traversal",
        "verdict": (
            "ATTENTION — below the collection's own full_scan_threshold, so qdrant "
            "scans exactly and does NOT traverse the graph. Reporting this cell as "
            "'filtered HNSW' would name the wrong mechanism."
            if brute else
            "OK — above the threshold, the graph is traversed with the filter applied"),
    }


def pg_route(container, table, bucket, field, value, dim=1024):
    """Does pg prune to one leaf AND use its index, with data resident?

    P6 proved pruning on empty tables. A plan that prunes correctly and then
    sequentially scans the leaf is a different mechanism from the one this arm
    credits pg with, and only EXPLAIN over real rows can tell them apart.
    """
    import subprocess
    q = (f"EXPLAIN (ANALYZE, BUFFERS, COSTS OFF) SELECT gid FROM {table} "
         f"WHERE bucket = {bucket} AND {field} = {value} "
         f"ORDER BY emb <=> '[{','.join(['0.1'] * dim)}]' LIMIT 10;")
    r = subprocess.run(["docker", "exec", "-i", "-e", "PGPASSWORD=bench", container,
                        "psql", "-U", "postgres", "-tAq", "-f", "-"],
                       input=q, capture_output=True, text=True, timeout=300)
    plan = (r.stdout or r.stderr)
    leaves = {ln.split()[-1] for ln in plan.splitlines() if "Scan on" in ln}
    used_index = "Index Scan" in plan or "Index Only Scan" in plan
    seq = "Seq Scan" in plan
    return {
        "engine": "pgvector", "field": field, "bucket": bucket,
        "relations_scanned": sorted(leaves), "leaves_scanned": len(leaves),
        "used_index": used_index, "seq_scan": seq,
        "verdict": ("OK — pruned to one relation and used an index"
                    if len(leaves) == 1 and used_index and not seq else
                    "ATTENTION — the plan is not the mechanism this arm credits pg with"),
        "plan": plan.strip()[:800],
    }


def chroma_declaration():
    """Chroma has no second level. Declared, not tested — there is nothing to observe."""
    return {
        "engine": "chroma", "mechanism": "collection per bucket + metadata filter",
        "second_level": None,
        "verdict": (
            "DECLARED LIMITATION — chroma's only co-location mechanism is multiplying "
            "collections. A second level would need |buckets| x |values| collections, "
            "which is combinatorial and not something anyone would deploy. It runs "
            "with collection-per-bucket plus a metadata filter, and that is stated as "
            "a finding rather than disguised as a weaker result."),
    }


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--engine", required=True, choices=["qdrant", "pgvector", "chroma"])
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=6333)
    ap.add_argument("--collection", default="mem")  # what QdrantAdapter creates
    ap.add_argument("--container", default="bench-pg")
    ap.add_argument("--table", default="items")
    ap.add_argument("--bucket", default="0")
    ap.add_argument("--field", default="cat1000")
    ap.add_argument("--value", default="0")
    ap.add_argument("--expected-points", type=int, default=0)
    args = ap.parse_args()

    if args.engine == "qdrant":
        out = qdrant_route(args.host, args.port, args.collection, args.field,
                           int(args.value) if args.value.isdigit() else args.value,
                           args.expected_points)
    elif args.engine == "pgvector":
        out = pg_route(args.container, args.table, args.bucket, args.field, args.value)
    else:
        out = chroma_declaration()
    print(json.dumps(out, indent=1))


if __name__ == "__main__":
    main()
