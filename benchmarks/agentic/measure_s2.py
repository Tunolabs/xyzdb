#!/usr/bin/env python3
"""S2 — live session: write<->search interleaved (design §1 S2).

Sustained loop, ~30W/70R (a declared single mix — an agent that saves one memory
and searches 2-3x per turn). The write stream is a DETERMINISTIC replay of corpus
A turns (real similarity distribution, not synthetic). Measures:
  - insert latency (online write),
  - query p50/p99 under concurrent writes,
  - degradation: query p50 late-window vs early-window (where a maintenance cycle
    shows — xyz LSM compaction, qdrant optimizer, chroma compaction; all measured
    the same),
  - visibility: a write at t is a candidate at t+1 (incremental oracle — the just
    written memory is the self-match top hit).

Durability: durable-strict, equalised with the native bench (xyz --durability
durable / pg synchronous_commit=on / qdrant wait=True); chroma is labelled with
its real guarantee (no per-write fsync knob — fleco §6.2).

N cycles: a fixed bound here (dry-run-calibrated). The design stop condition
(>=1 full maintenance cycle per engine) needs a per-engine maintenance signal
(fleco §6.2); until wired, --cycles bounds it and the windows expose degradation.
Mac/OrbStack = DIRECTION.
"""
import argparse
import json
import signal
import time

import numpy as np
from adapters import ADAPTERS
from measure_x import PeakSampler, PORTS, hnsw_from_env, disk_mb, settle, bench_stamp
from measure_lme import load_corpus_a, BUILD_TIMEOUT

DURABILITY = {"xyzdb": "durable", "pgvector": "synchronous_commit=on",
              "qdrant": "wait=true", "chroma": "engine-default (no per-write fsync knob)"}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--engine", required=True, choices=list(ADAPTERS))
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--container", required=True)
    ap.add_argument("--volume", default="")
    ap.add_argument("--disk_path", default="")
    ap.add_argument("--storage", default="local")
    ap.add_argument("--envelope", default="?")
    ap.add_argument("--cycles", type=int, default=2000)
    ap.add_argument("--wfrac", type=float, default=0.30)
    ap.add_argument("--base_n", type=int, default=0, help="base index size (0=full corpus)")
    ap.add_argument("--k", type=int, default=10)
    ap.add_argument("--round", type=int, default=1)
    ap.add_argument("--out", required=True)
    args = ap.parse_args()
    port = PORTS[args.engine]

    qvec, queries, qid2int, turn_vecs, turn_bucket, turn_session = load_corpus_a()
    dim = turn_vecs.shape[1]
    if args.base_n > 0:
        turn_vecs = turn_vecs[:args.base_n]; turn_bucket = turn_bucket[:args.base_n]

    Adapter = ADAPTERS[args.engine]
    hnsw = hnsw_from_env(args.engine)
    adapter = Adapter(host=args.host, port=port, dim=dim, hnsw=hnsw)

    signal.signal(signal.SIGALRM, lambda *_: (_ for _ in ()).throw(TimeoutError(f"build>{BUILD_TIMEOUT}s")))
    bsamp = PeakSampler(args.container); bsamp.start(); t0 = time.perf_counter()
    try:
        signal.alarm(BUILD_TIMEOUT)
        adapter.load(turn_vecs, turn_bucket, hnsw)
        signal.alarm(0); load_s = time.perf_counter() - t0; build_peak = bsamp.stop()
    except Exception as e:
        signal.alarm(0); at = round(time.perf_counter() - t0, 1); bpeak = bsamp.stop()
        st = "unviable_build_timeout" if isinstance(e, TimeoutError) else "crash_or_oom_during_load"
        rec = {"kind": "s2", "engine": args.engine, "envelope": args.envelope, "round": args.round,
               "status": st, "oom_at_s": at, "build_ram_peak_mb": round(bpeak, 1),
               "err": str(e)[:120], "stamp": bench_stamp()}
        open(args.out, "a").write(json.dumps(rec) + "\n"); print(json.dumps(rec)); return

    # Post-load settle to state parity (change 3): restart+reconnect so the BASE index
    # is settled before the live write<->read loop (the loop's own writes are the
    # workload, legitimately in RAM). Declared in the report protocol.
    try:
        settle_ms = settle(args.container, args.engine, args.host, port, adapter)
    except Exception as e:
        rec = {"kind": "s2", "engine": args.engine, "envelope": args.envelope, "round": args.round,
               "status": "settle_failed", "err": str(e)[:120], "stamp": bench_stamp()}
        open(args.out, "a").write(json.dumps(rec) + "\n"); print(json.dumps(rec)); return

    rng = np.random.default_rng(20260701)
    nq = len(qvec)
    n_base = len(turn_vecs)
    qsamp = PeakSampler(args.container); qsamp.start()
    w_lat, q_by_cycle, vis = [], [], []
    wcount = 0
    for c in range(args.cycles):
        if rng.random() < args.wfrac:                       # WRITE
            src = wcount % n_base
            gid = n_base + wcount
            b = int(turn_bucket[src])
            t = time.perf_counter()
            adapter.insert_one(gid, b, turn_vecs[src])
            w_lat.append((time.perf_counter() - t) * 1e3)
            # visibility (t+1): the just-written memory should be its own top hit
            got = adapter.query(turn_vecs[src], b, args.k)
            vis.append(1.0 if gid in set(int(x) for x in got) else 0.0)
            wcount += 1
        else:                                               # READ
            qi = int(rng.integers(0, nq))
            b = qid2int[queries[qi]["qid"]]
            t = time.perf_counter()
            adapter.query(qvec[qi], b, args.k)
            q_by_cycle.append((c, (time.perf_counter() - t) * 1e3))
    qpeak = qsamp.stop()
    adapter.close()

    early = [ql for c, ql in q_by_cycle if c < args.cycles * 0.2]
    late = [ql for c, ql in q_by_cycle if c > args.cycles * 0.8]
    qall = [ql for _, ql in q_by_cycle]
    med = lambda a: float(np.median(a)) if a else 0.0
    degr = round(med(late) / med(early), 3) if early and late and med(early) > 0 else 1.0
    a = np.array(qall) if qall else np.array([0.0])
    wa = np.array(w_lat) if w_lat else np.array([0.0])
    rec = {
        "kind": "s2", "engine": args.engine, "envelope": args.envelope, "storage": args.storage,
        "round": args.round, "cycles": args.cycles, "wfrac": args.wfrac,
        "durability": DURABILITY[args.engine], "n_writes": wcount, "n_reads": len(qall),
        "insert_p50_ms": round(float(np.percentile(wa, 50)), 3),
        "insert_p99_ms": round(float(np.percentile(wa, 99)), 3),
        "query_p50_ms": round(float(np.percentile(a, 50)), 3),
        "query_p99_ms": round(float(np.percentile(a, 99)), 3),
        "query_p50_early_ms": round(med(early), 3), "query_p50_late_ms": round(med(late), 3),
        "degradation_late_over_early": degr,
        "visibility": round(float(np.mean(vis)), 4) if vis else 0.0,   # want 1.0 (durable + fresh)
        "build_ram_peak_mb": round(build_peak, 1), "query_ram_peak_mb": round(qpeak, 1),
        "disk_total_mb": disk_mb(args.volume, args.disk_path), "load_s": round(load_s, 1),
        "settle_ms": round(settle_ms, 1),
        "maint_signal": "not-wired (fleco 6.2); windows expose degradation", "status": None,
        "stamp": bench_stamp(),
    }
    open(args.out, "a").write(json.dumps(rec) + "\n")
    print(json.dumps(rec))


if __name__ == "__main__":
    main()
