#!/usr/bin/env python3
"""S1 — retrieve-and-expand (design §1 S1, PRIORITY 1).

Same business question in the 4 engines: NEAREST top-k in the bucket, then expand
each hit to its FULL session (the real agent-memory pattern — the conversation, not
a lone turn). One engine at a time (container already up). Reuses adapters.py
`retrieve_expand` and corpus A (bucket = question_id, session = sid).

Two gates (do not conflate):
  - session_recall — fraction of the ORACLE top-k sessions the engine's NEAREST found
    (f64 oracle over the bucket). HNSW rivals < 1.0; xyzDB exact = the ceiling.
  - expand_complete_frac — CORRECTNESS, all engines must hit 1.0: given the engine's
    OWN hit sessions, it returns EXACTLY those sessions' turns in the bucket (no drop,
    no bleed). Independent of recall; a config/JOIN bug shows here.

Also reports end-to-end latency, roundtrips (xyz 2 / pg 1 / qdrant scroll 2 or
payload-dup 1 / chroma 2), and disk (the payload-dup tax). Every config Δ (scoped,
qd_variant) is in the record. Mac/OrbStack = DIRECTION only.
"""
import argparse
import json
import signal
import time

import numpy as np
from adapters import ADAPTERS
from measure_x import PeakSampler, PORTS, hnsw_from_env, disk_mb, settle, bench_stamp
from measure_lme import load_corpus_a, BUILD_TIMEOUT
import recall_harness as rh


def build_bucket_index(turn_bucket, turn_session):
    """bucket_int -> {'turns': [tid...], 'by_sid': {sid: set(tid)}} for oracle + gate."""
    idx = {}
    for tid in range(len(turn_bucket)):
        b = int(turn_bucket[tid])
        s = str(turn_session[tid])
        e = idx.setdefault(b, {"turns": [], "by_sid": {}})
        e["turns"].append(tid)
        e["by_sid"].setdefault(s, set()).add(tid)
    return idx


def oracle_sessions(qvec, bucket_turns, turn_vecs, turn_session, k):
    """The k exact-NEAREST turns' sessions = the gold session set to expand."""
    bt = np.asarray(bucket_turns)
    sc = rh.exact_scores(qvec, turn_vecs[bt])
    topk = bt[np.argsort(-sc)[:k]]
    return list(dict.fromkeys(str(turn_session[int(t)]) for t in topk))


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
    ap.add_argument("--qd_variant", default="scroll", choices=["scroll", "payload-dup"],
                    help="qdrant S1 arm: scroll (2 RT) or payload-dup (1 RT, disk tax)")
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
        turn_session = turn_session[:args.base_n]
    dim = turn_vecs.shape[1]
    bidx = build_bucket_index(turn_bucket, turn_session)

    Adapter = ADAPTERS[args.engine]
    hnsw = hnsw_from_env(args.engine)
    kw = {"scoped": args.scoped}
    if args.engine == "qdrant":
        kw["s1_variant"] = args.qd_variant
    adapter = Adapter(host=args.host, port=port, dim=dim, hnsw=hnsw, **kw)
    qd = args.qd_variant if args.engine == "qdrant" else None

    # BUILD (load with co-located sid). Unviable build = recorded result, not a hang.
    signal.signal(signal.SIGALRM, lambda *_: (_ for _ in ()).throw(TimeoutError(f"build>{BUILD_TIMEOUT}s")))
    bsamp = PeakSampler(args.container); bsamp.start(); t0 = time.perf_counter()
    try:
        signal.alarm(BUILD_TIMEOUT)
        adapter.load(turn_vecs, turn_bucket, hnsw, sids=turn_session)
        signal.alarm(0)
        load_s = time.perf_counter() - t0
        build_peak = bsamp.stop()
    except Exception as e:
        signal.alarm(0)
        at = round(time.perf_counter() - t0, 1)
        bpeak = bsamp.stop()
        st = "unviable_build_timeout" if isinstance(e, TimeoutError) else "crash_or_oom_during_load"
        rec = {"kind": "s1", "engine": args.engine, "corpus": "lme", "envelope": args.envelope,
               "scoped": args.scoped, "qd_variant": qd, "storage": args.storage, "round": args.round,
               "status": st, "oom_at_s": at, "build_ram_peak_mb": round(bpeak, 1),
               "setup": getattr(adapter, "setup_cost", None), "stamp": bench_stamp(),
               "note": f"died BUILDING over {len(turn_vecs)} vectors (not serving)", "err": str(e)[:120]}
        open(args.out, "a").write(json.dumps(rec) + "\n"); print(json.dumps(rec)); return

    # Post-load settle to state parity (change 3): restart -> xyz seals+flushes its
    # memtable to disk, then reconnect. query_ram_peak below is then measured on
    # SETTLED state (the R1 fix), uniformly for every engine. Declared in the report.
    try:
        settle_ms = settle(args.container, args.engine, args.host, port, adapter)
    except Exception as e:
        rec = {"kind": "s1", "engine": args.engine, "corpus": "lme", "envelope": args.envelope,
               "scoped": args.scoped, "qd_variant": qd, "storage": args.storage, "round": args.round,
               "status": "settle_failed", "err": str(e)[:120], "stamp": bench_stamp()}
        open(args.out, "a").write(json.dumps(rec) + "\n"); print(json.dumps(rec)); return

    sel = [(i, q) for i, q in enumerate(queries) if args.split == "all" or q["split"] == args.split]
    if args.max_queries > 0:
        sel = sel[:args.max_queries]
    qsamp = PeakSampler(args.container); qsamp.start()
    lat, rts, srec = [], [], []
    expand_ok = 0
    n_trunc = 0   # queries whose NEAREST found no session (a flat/config red flag, not thesis)
    for qi, q in sel:
        b = qid2int[q["qid"]]
        if b not in bidx:
            continue
        osids = oracle_sessions(qvec[qi], bidx[b]["turns"], turn_vecs, turn_session, args.k)
        t = time.perf_counter()
        turns, hit_sids, nrt = adapter.retrieve_expand(qvec[qi], b, args.k)
        lat.append((time.perf_counter() - t) * 1e3)
        rts.append(nrt)
        srec.append(len(set(osids) & set(hit_sids)) / max(1, len(osids)))
        # expand-completeness: engine returned EXACTLY its hit sessions' turns in bucket b
        expected = set()
        for s in hit_sids:
            expected |= bidx[b]["by_sid"].get(s, set())
        if set(int(x) for x in turns) == expected and expected:
            expand_ok += 1
        if not hit_sids:
            n_trunc += 1
    qpeak = qsamp.stop()
    adapter.close()
    a = np.array(lat) if lat else np.array([0.0])
    rec = {
        "kind": "s1", "engine": args.engine, "corpus": "lme", "envelope": args.envelope,
        "scoped": args.scoped, "qd_variant": qd, "storage": args.storage, "round": args.round,
        "split": args.split, "k": args.k, "n_queries": len(sel),
        "session_recall": round(float(np.mean(srec)), 4) if srec else 0.0,
        "expand_complete_frac": round(expand_ok / max(1, len(sel)), 4),   # GATE: expect 1.0
        "roundtrips": int(round(float(np.mean(rts)))) if rts else 0,
        "p50_ms": round(float(np.percentile(a, 50)), 3),
        "p99_ms": round(float(np.percentile(a, 99)), 3),
        "mean_ms": round(float(a.mean()), 3),
        "build_ram_peak_mb": round(build_peak, 1), "query_ram_peak_mb": round(qpeak, 1),
        "disk_total_mb": disk_mb(args.volume, args.disk_path),   # payload-dup tax shows here
        "load_s": round(load_s, 1), "settle_ms": round(settle_ms, 1), "n_truncated": n_trunc,
        "setup": getattr(adapter, "setup_cost", None), "status": None, "stamp": bench_stamp(),
    }
    open(args.out, "a").write(json.dumps(rec) + "\n")
    print(json.dumps(rec))


if __name__ == "__main__":
    main()
