#!/usr/bin/env python3
"""Corpus A (LongMemEval) recall run — session-level academic recall (INV-1).

Distinct from measure_x (Corpus B oracle recall): here recall@k is vs the academic GT
(answer_session_ids), a session counts as recalled if ANY of its turns is in the top-k, and
ALL engines score < 1.0 (retrieval quality) — xyzDB-exact is the CEILING, rivals ≤ it.

One engine at a time (the container is already up). Reuses adapters.py (load/query) and
recall_harness.session_recall_at_k. Separates BUILD RAM-peak from QUERY RAM-peak, and
`load_s` (build completed) from `oom_at_s`/`status` (died building the index).
"""
import argparse
import json
import os
import signal
import time

import numpy as np
from adapters import ADAPTERS
from measure_x import docker_mem_mb, PeakSampler, wait_ready, PORTS
import recall_harness as rh

CORP = os.environ.get("BENCH_CORP",
                      os.path.join(os.path.dirname(os.path.abspath(__file__)), "corpora", "lme"))
# Ceiling on the index build. An unviable build (e.g. chroma-500-collections) is a RESULT,
# not a hang that eats the session. Override via BUILD_TIMEOUT env (seconds).
BUILD_TIMEOUT = int(os.environ.get("BUILD_TIMEOUT", 1800))


def load_corpus_a():
    cvec = np.load(f"{CORP}/cvec.npy")
    qvec = np.load(f"{CORP}/qvec.npy")
    meta = json.load(open(f"{CORP}/meta.json"))
    T = meta["turns"]
    qids = list(dict.fromkeys(T["qid"]))          # stable unique question ids
    qid2int = {q: i for i, q in enumerate(qids)}
    n = len(T["qid"])
    # turn i → its bge vector, its gravity bucket (question_id int), its session id
    turn_vecs = cvec[np.asarray(T["vec_idx"])]     # (n_turns, 1024)
    turn_bucket = np.asarray([qid2int[q] for q in T["qid"]], dtype=np.int64)
    turn_session = T["sid"]                         # list, index = turn id
    return qvec, meta["queries"], qid2int, turn_vecs, turn_bucket, turn_session


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--engine", required=True, choices=list(ADAPTERS))
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--container", required=True)
    ap.add_argument("--volume", default="")
    ap.add_argument("--disk_path", default="")
    ap.add_argument("--storage", default="local")
    ap.add_argument("--envelope", default="?")
    ap.add_argument("--pass_label", default="dense")
    ap.add_argument("--scoped", action="store_true", help="rival scoped (partition/tenant/coll-per-bucket) vs flat")
    ap.add_argument("--round", type=int, default=1)
    ap.add_argument("--split", default="held", choices=["held", "dev", "all"])
    ap.add_argument("--out", required=True)
    args = ap.parse_args()
    port = PORTS[args.engine]

    qvec, queries, qid2int, turn_vecs, turn_bucket, turn_session = load_corpus_a()
    dim = turn_vecs.shape[1]

    # BUILD (load) with its own RAM-peak sampler; capture time-to-OOM if it dies.
    Adapter = ADAPTERS[args.engine]
    hnsw = __import__("measure_x").hnsw_from_env(args.engine)
    adapter = Adapter(host=args.host, port=port, dim=dim, hnsw=hnsw, scoped=args.scoped)
    signal.signal(signal.SIGALRM, lambda *_: (_ for _ in ()).throw(TimeoutError(f"build > {BUILD_TIMEOUT}s")))
    bsamp = PeakSampler(args.container); bsamp.start()
    t0 = time.perf_counter()
    try:
        signal.alarm(BUILD_TIMEOUT)
        adapter.load(turn_vecs, turn_bucket, hnsw)
        signal.alarm(0)
        load_s = time.perf_counter() - t0
        build_peak = bsamp.stop()
    except Exception as e:
        signal.alarm(0)
        at = round(time.perf_counter() - t0, 1)
        bpeak = bsamp.stop()
        # The rival dies BUILDING the HNSW over N vectors, not serving — the coverage headline.
        st = "unviable_build_timeout" if isinstance(e, TimeoutError) else "crash_or_oom_during_load"
        rec = {"kind": "recall", "engine": args.engine, "corpus": "lme", "envelope": args.envelope,
               "pass": args.pass_label, "scoped": args.scoped, "storage": args.storage, "round": args.round,
               "status": st, "oom_at_s": at, "build_ram_peak_mb": round(bpeak, 1),
               "setup": getattr(adapter, "setup_cost", None),
               "note": f"died BUILDING the index over {len(turn_vecs)} vectors (not serving)",
               "err": str(e)[:120]}
        open(args.out, "a").write(json.dumps(rec) + "\n"); print(json.dumps(rec)); return

    # QUERY: session-level recall@{1,5,10}, separated by split; own RAM-peak sampler.
    sel = [(i, q) for i, q in enumerate(queries)
           if args.split == "all" or q["split"] == args.split]
    qsamp = PeakSampler(args.container); qsamp.start()
    rec_at = {1: [], 5: [], 10: []}
    lat = []; n_trunc = 0   # queries returning < k (a flat-config red flag, not a thesis result)
    for qi, q in sel:
        b = qid2int[q["qid"]]
        gt = q["gt"]
        t = time.perf_counter()
        got = adapter.query(qvec[qi], b, 10)             # top-10 turn ids
        lat.append((time.perf_counter() - t) * 1e3)
        if len(got) < 10:
            n_trunc += 1
        for k in (1, 5, 10):
            rec_at[k].append(rh.session_recall_at_k(got, turn_session, gt, k))
    qpeak = qsamp.stop()
    adapter.close()
    a = np.array(lat)
    rec = {
        "kind": "recall", "engine": args.engine, "corpus": "lme", "envelope": args.envelope,
        "pass": args.pass_label, "scoped": args.scoped, "storage": args.storage, "round": args.round, "split": args.split,
        "n_queries": len(sel),
        "recall_at_1": round(float(np.mean(rec_at[1])), 4),
        "recall_at_5": round(float(np.mean(rec_at[5])), 4),
        "recall_at_10": round(float(np.mean(rec_at[10])), 4),
        "p50_ms": round(float(np.percentile(a, 50)), 3),
        "p99_ms": round(float(np.percentile(a, 99)), 3),
        "build_ram_peak_mb": round(build_peak, 1), "query_ram_peak_mb": round(qpeak, 1),
        "load_s": round(load_s, 1), "oom_at_s": None, "status": None,
        "setup": getattr(adapter, "setup_cost", None),   # operational cost of scoping = the moat
        "n_truncated": n_trunc,                          # >0 on flat = config red flag to fix
    }
    open(args.out, "a").write(json.dumps(rec) + "\n")
    print(json.dumps(rec))


if __name__ == "__main__":
    main()
