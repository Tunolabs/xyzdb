#!/usr/bin/env python3
"""S4 — serverless wake: time-to-first-query (design §1 S4).

Build the index, restart the container cleanly, then measure TTFQ = restart -> first
SUCCESSFUL query. xyzDB serves from the LSM (little to warm); the rivals load the
graph / collections into RAM on first access. Composes with the at-rest footprint
(RAM median-3 + disk). Swept across ALL tiers incl. the tightest (§6.10) — where a
rival may not even wake is a recorded result. Mac/OrbStack = DIRECTION.
"""
import argparse
import json
import signal
import subprocess
import time

import numpy as np
from adapters import ADAPTERS
from measure_x import docker_mem_mb, PORTS, hnsw_from_env, disk_mb, bench_stamp
from measure_lme import load_corpus_a, BUILD_TIMEOUT


def _reconnect(a, engine, host, port):
    """Re-establish the adapter's transport after `docker restart`, KEEPING its
    load-time query state (_efs/_ef/_flat/_colls). A fresh adapter would lack that
    state and its query() would raise — which is what made the rivals time out."""
    if engine == "xyzdb":
        import xyzdb_minimal as xyzdb
        a.db = xyzdb.connect(host, port)
    elif engine == "pgvector":
        import psycopg2
        from pgvector.psycopg2 import register_vector
        a.conn = psycopg2.connect(host=host, port=port, user="postgres",
                                  password="bench", dbname="postgres")
        a.conn.autocommit = True
        register_vector(a.conn)
    elif engine == "qdrant":
        from qdrant_client import QdrantClient
        a.client = QdrantClient(host=host, port=port, timeout=600)
    else:  # chroma — re-fetch the flat collection handle (stale after restart)
        import chromadb
        a.client = chromadb.HttpClient(host=host, port=port)
        a._flat = a.client.get_collection("mem")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--engine", required=True, choices=list(ADAPTERS))
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--container", required=True)
    ap.add_argument("--volume", default="")
    ap.add_argument("--disk_path", default="")
    ap.add_argument("--storage", default="local")
    ap.add_argument("--envelope", default="?")
    ap.add_argument("--base_n", type=int, default=0)
    ap.add_argument("--k", type=int, default=10)
    ap.add_argument("--ttfq_cap_s", type=int, default=300)
    ap.add_argument("--round", type=int, default=1)
    ap.add_argument("--out", required=True)
    args = ap.parse_args()
    port = PORTS[args.engine]

    qvec, queries, qid2int, turn_vecs, turn_bucket, turn_session = load_corpus_a()
    dim = turn_vecs.shape[1]
    if args.base_n > 0:
        turn_vecs = turn_vecs[:args.base_n]; turn_bucket = turn_bucket[:args.base_n]
    probe_b = qid2int[queries[0]["qid"]]
    probe_q = qvec[0]

    Adapter = ADAPTERS[args.engine]
    hnsw = hnsw_from_env(args.engine)

    signal.signal(signal.SIGALRM, lambda *_: (_ for _ in ()).throw(TimeoutError(f"build>{BUILD_TIMEOUT}s")))
    try:
        a = Adapter(host=args.host, port=port, dim=dim, hnsw=hnsw)
        signal.alarm(BUILD_TIMEOUT)
        a.load(turn_vecs, turn_bucket, hnsw)   # keep `a` (its load-state) for the probe
        signal.alarm(0)
    except Exception as e:
        signal.alarm(0)
        st = "unviable_build_timeout" if isinstance(e, TimeoutError) else "crash_or_oom_during_load"
        open(args.out, "a").write(json.dumps({"kind": "s4", "engine": args.engine,
            "envelope": args.envelope, "status": st, "err": str(e)[:120]}) + "\n"); return

    # Clean restart, then time the first successful query. Reuse the loaded adapter
    # (its _efs/_ef/_flat query state) and just reconnect the transport per attempt.
    subprocess.run(["docker", "restart", args.container], capture_output=True, timeout=180)
    t0 = time.perf_counter()
    ttfq = None
    while time.perf_counter() - t0 < args.ttfq_cap_s:
        try:
            _reconnect(a, args.engine, args.host, port)
            got = a.query(probe_q, probe_b, args.k)
            if got is not None:
                ttfq = (time.perf_counter() - t0) * 1e3
                break
        except Exception:
            time.sleep(0.2)
    if ttfq is None:
        open(args.out, "a").write(json.dumps({"kind": "s4", "engine": args.engine,
            "envelope": args.envelope, "status": "ttfq_timeout", "ttfq_cap_s": args.ttfq_cap_s}) + "\n")
        print(f"  {args.engine} TTFQ timeout > {args.ttfq_cap_s}s"); return

    ram = sorted(docker_mem_mb(args.container) for _ in range(3))[1]
    rec = {"kind": "s4", "engine": args.engine, "envelope": args.envelope, "storage": args.storage,
           "round": args.round, "ttfq_ms": round(ttfq, 1), "ram_rest_mb": round(ram, 1),
           "disk_total_mb": disk_mb(args.volume, args.disk_path), "status": None, "stamp": bench_stamp(adapter)}
    open(args.out, "a").write(json.dumps(rec) + "\n")
    print(json.dumps(rec))


if __name__ == "__main__":
    main()
