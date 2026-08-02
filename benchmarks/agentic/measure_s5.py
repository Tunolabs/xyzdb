#!/usr/bin/env python3
"""S5 — hybrid search: exact structured filter + NEAREST (design §1 S5).

Same business question in the 4 engines: "of the memories matching `topic < T`,
give the k nearest". Sweeps selectivity 50% -> 0.1% (metadata_gen.SELECTIVITIES).
Metadata is deterministic (metadata_gen), so all engines filter the SAME universe
(the parity gate) and the oracle scores over exactly that filtered set.

Arms (each its best, declared): xyzdb WHERE+NEAREST exact · qdrant filterable-HNSW
with an integer payload index (its STRONG arm — the filter prunes the graph during
traversal) · pgvector iterative_scan / partition · chroma metadata pre-filter.

Recall@k is vs the f64 oracle OVER THE FILTERED SET (tie-aware). Mac = DIRECTION.
"""
import argparse
import json
import signal
import time

import numpy as np
from adapters import ADAPTERS
from measure_x import PeakSampler, PORTS, hnsw_from_env, disk_mb, settle, bench_stamp
from measure_lme import load_corpus_a, BUILD_TIMEOUT
import metadata_gen as mg
import recall_harness as rh


def build_bucket_turns(turn_bucket):
    idx = {}
    for tid in range(len(turn_bucket)):
        idx.setdefault(int(turn_bucket[tid]), []).append(tid)
    return idx


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--engine", required=True, choices=list(ADAPTERS))
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--container", required=True)
    ap.add_argument("--volume", default="")
    ap.add_argument("--disk_path", default="")
    ap.add_argument("--storage", default="local")
    ap.add_argument("--envelope", default="?")
    ap.add_argument("--scoped", action="store_true")
    ap.add_argument("--k", type=int, default=10)
    ap.add_argument("--split", default="held", choices=["held", "dev", "all"])
    ap.add_argument("--max_queries", type=int, default=0)
    ap.add_argument("--base_n", type=int, default=0, help="index size (0=full corpus)")
    ap.add_argument("--round", type=int, default=1)
    ap.add_argument("--out", required=True)
    args = ap.parse_args()
    port = PORTS[args.engine]

    qvec, queries, qid2int, turn_vecs, turn_bucket, turn_session = load_corpus_a()
    if args.base_n > 0:
        turn_vecs = turn_vecs[:args.base_n]
        turn_bucket = turn_bucket[:args.base_n]
    dim = turn_vecs.shape[1]
    md = mg.gen(len(turn_vecs))                 # deterministic — same universe for all engines
    topic = md["topic"]
    bturns = build_bucket_turns(turn_bucket)

    Adapter = ADAPTERS[args.engine]
    hnsw = hnsw_from_env(args.engine)
    adapter = Adapter(host=args.host, port=port, dim=dim, hnsw=hnsw, scoped=args.scoped)

    signal.signal(signal.SIGALRM, lambda *_: (_ for _ in ()).throw(TimeoutError(f"build>{BUILD_TIMEOUT}s")))
    bsamp = PeakSampler(args.container); bsamp.start(); t0 = time.perf_counter()
    try:
        signal.alarm(BUILD_TIMEOUT)
        adapter.load(turn_vecs, turn_bucket, hnsw, meta=md)
        signal.alarm(0)
        load_s = time.perf_counter() - t0
        build_peak = bsamp.stop()
    except Exception as e:
        signal.alarm(0)
        at = round(time.perf_counter() - t0, 1); bpeak = bsamp.stop()
        st = "unviable_build_timeout" if isinstance(e, TimeoutError) else "crash_or_oom_during_load"
        rec = {"kind": "s5", "engine": args.engine, "corpus": "lme", "envelope": args.envelope,
               "scoped": args.scoped, "storage": args.storage, "round": args.round,
               "status": st, "oom_at_s": at, "build_ram_peak_mb": round(bpeak, 1),
               "setup": getattr(adapter, "setup_cost", None), "err": str(e)[:120], "stamp": bench_stamp(adapter)}
        open(args.out, "a").write(json.dumps(rec) + "\n"); print(json.dumps(rec)); return

    # Post-load settle to state parity (change 3): restart+reconnect so the query &
    # footprint below are measured on SETTLED state, uniformly for every engine.
    try:
        settle_ms = settle(args.container, args.engine, args.host, port, adapter)
    except Exception as e:
        rec = {"kind": "s5", "engine": args.engine, "corpus": "lme", "envelope": args.envelope,
               "scoped": args.scoped, "storage": args.storage, "round": args.round,
               "status": "settle_failed", "err": str(e)[:120], "stamp": bench_stamp(adapter)}
        open(args.out, "a").write(json.dumps(rec) + "\n"); print(json.dumps(rec)); return

    sel = [(i, q) for i, q in enumerate(queries) if args.split == "all" or q["split"] == args.split]
    if args.max_queries > 0:
        sel = sel[:args.max_queries]

    qsamp = PeakSampler(args.container); qsamp.start()
    # one record per (engine, selectivity) — the curve is the result.
    for s in mg.SELECTIVITIES:
        T = mg.threshold(s)
        lat, rec_at = [], []
        for qi, q in sel:
            b = qid2int[q["qid"]]
            if b not in bturns:
                continue
            filt = [t for t in bturns[b] if int(topic[t]) < T]
            if not filt:
                continue
            # keff = min(k, n_filtered): at low selectivity the filtered set is < k, so
            # the achievable recall denominator is n_filtered, not k — otherwise the
            # curve shows a symmetric artifact (all engines drop to n/k), not a real
            # degradation.
            keff = min(args.k, len(filt))
            cut = rh.kth_oracle_score(qvec[qi], turn_vecs[np.asarray(filt)], keff)
            t = time.perf_counter()
            got = adapter.query_filtered(qvec[qi], b, args.k, T)
            lat.append((time.perf_counter() - t) * 1e3)
            rec_at.append(rh.tie_aware_recall(qvec[qi], got, turn_vecs, cut, keff))
        a = np.array(lat) if lat else np.array([0.0])
        rec = {
            "kind": "s5", "engine": args.engine, "corpus": "lme", "envelope": args.envelope,
            "scoped": args.scoped, "storage": args.storage, "round": args.round, "split": args.split,
            "k": args.k, "selectivity": s, "topic_lt": T, "n_queries": len(rec_at),
            "recall": round(float(np.mean(rec_at)), 4) if rec_at else 0.0,
            "p50_ms": round(float(np.percentile(a, 50)), 3),
            "p99_ms": round(float(np.percentile(a, 99)), 3),
            "mean_ms": round(float(a.mean()), 3),
            "load_s": round(load_s, 1), "settle_ms": round(settle_ms, 1),
            "setup": getattr(adapter, "setup_cost", None), "status": None, "stamp": bench_stamp(adapter),
        }
        open(args.out, "a").write(json.dumps(rec) + "\n")
        print(json.dumps(rec))

    qpeak = qsamp.stop()
    adapter.close()
    open(args.out, "a").write(json.dumps({
        "kind": "s5_footprint", "engine": args.engine, "envelope": args.envelope,
        "scoped": args.scoped, "storage": args.storage, "round": args.round,
        "build_ram_peak_mb": round(build_peak, 1), "query_ram_peak_mb": round(qpeak, 1),
        "disk_total_mb": disk_mb(args.volume, args.disk_path), "status": None,
        "stamp": bench_stamp(adapter)}) + "\n")


if __name__ == "__main__":
    main()
