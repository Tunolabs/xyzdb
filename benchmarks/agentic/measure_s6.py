#!/usr/bin/env python3
"""S6 — one engine for the whole agent (design §1 S6). Runs the composite turn
(write vector+fields -> update structured -> NEAREST) N times per DEPLOYMENT, then
the AGGREGATE ("count topic X active + avg importance"). Cells are deployments
(deployments_s6.py), not binaries. Reports per-op latency, the +store inconsistency
window (0 for one-system xyz/pg), the AGGREGATE, and summed footprint.

Honesty (design §1 S6): PG ties on one-system; vector-pure-at-scale is the
specialist's turf (P7). Every deployment answers the same business turn.
"""
import argparse
import json
import time

import numpy as np
from deployments_s6 import DEPLOYMENTS
from measure_lme import load_corpus_a
from measure_x import disk_mb, bench_stamp
import metadata_gen as mg


def pct(a, p):
    return round(float(np.percentile(np.array(a) if a else [0.0], p)), 3)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--deployment", required=True, choices=list(DEPLOYMENTS))
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--container", default="")   # accepted for runner symmetry (RAM via disk only)
    ap.add_argument("--envelope", default="?")
    ap.add_argument("--n_turns", type=int, default=200)
    ap.add_argument("--topic", type=int, default=1)
    ap.add_argument("--k", type=int, default=10)
    ap.add_argument("--base_n", type=int, default=0, help="index size (0=full corpus)")
    ap.add_argument("--storage", default="local")
    ap.add_argument("--vec_disk_path", default="")
    ap.add_argument("--vec_volume", default="")
    ap.add_argument("--store_disk_path", default="")
    ap.add_argument("--store_volume", default="")
    ap.add_argument("--round", type=int, default=1)
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    qvec, queries, qid2int, turn_vecs, turn_bucket, turn_session = load_corpus_a()
    if args.base_n > 0:
        turn_vecs = turn_vecs[:args.base_n]
        turn_bucket = turn_bucket[:args.base_n]
    dim = turn_vecs.shape[1]
    md = mg.gen(len(turn_vecs))

    dep = DEPLOYMENTS[args.deployment](host=args.host, dim=dim)
    t0 = time.perf_counter()
    dep.setup(turn_vecs, turn_bucket, md)
    load_s = time.perf_counter() - t0

    # Post-setup settle to state parity (change 3): restart+reconnect the (vector)
    # container so the turns and the AGGREGATE read from settled state (xyz's ghost
    # persists across the restart). Skipped only if no container was passed.
    settle_ms = 0.0
    if args.container:
        settle_ms = dep.settle(args.container)

    base = len(turn_vecs)
    tmd = mg.gen(base + args.n_turns)   # deterministic metadata for the new-turn writes
    nq = len(qvec)
    W, U, Nn, Inc = [], [], [], []
    for j in range(args.n_turns):
        gid = base + j
        b = qid2int[queries[j % len(queries)]["qid"]]
        r = dep.turn(gid, b, int(tmd["topic"][gid]), str(tmd["status"][gid]),
                     float(tmd["importance"][gid]), qvec[j % nq], args.k, "archived")
        W.append(r["write_ms"]); U.append(r["update_ms"]); Nn.append(r["near_ms"]); Inc.append(r["incons_ms"])
    ta = time.perf_counter()
    agg = dep.aggregate(args.topic)
    agg_ms = (time.perf_counter() - ta) * 1e3   # ghost-routed (xyz) -> expect sub-ms
    dep.close()

    vec_disk = disk_mb(args.vec_volume, args.vec_disk_path)
    store_disk = disk_mb(args.store_volume, args.store_disk_path) if not DEPLOYMENTS[args.deployment].one_system else 0.0
    total_disk = round((vec_disk if vec_disk > 0 else 0) + (store_disk if store_disk > 0 else 0), 1)

    rec = {
        "kind": "s6", "deployment": args.deployment, "one_system": DEPLOYMENTS[args.deployment].one_system,
        "envelope": args.envelope, "storage": args.storage, "round": args.round, "n_turns": args.n_turns,
        "write_p50_ms": pct(W, 50), "write_p99_ms": pct(W, 99),
        "update_p50_ms": pct(U, 50), "update_p99_ms": pct(U, 99),
        "near_p50_ms": pct(Nn, 50), "near_p99_ms": pct(Nn, 99),
        "incons_p50_ms": pct(Inc, 50), "incons_p99_ms": pct(Inc, 99),   # 0 for one-system
        "aggregate_raw": str(agg)[:200], "aggregate_topic": args.topic,
        "aggregate_ms": round(agg_ms, 3),   # xyz ghost-routed -> sub-ms
        "vec_disk_mb": vec_disk, "store_disk_mb": store_disk, "total_disk_mb": total_disk,
        "load_s": round(load_s, 1), "settle_ms": round(settle_ms, 1), "status": None,
        "stamp": bench_stamp(adapter),
    }
    open(args.out, "a").write(json.dumps(rec) + "\n")
    print(json.dumps(rec))


if __name__ == "__main__":
    main()
