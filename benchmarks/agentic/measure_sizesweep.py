#!/usr/bin/env python3
"""Bucket-size / scan-size sweep with the full resource signature.

Two uses:
  1. Cross-engine (scoped best-form) — is "exact vs approximate" real? (recall stays ~1.0 to 5000).
  2. xyzDB before(0.8.13) vs after(0.9) — where did Fase 1 (G1a/G2/G3/v3) move the needle? Recall
     is the CANARY (must be identical before==after — Fase 1 is read-path, not recall); the signal
     is in latency/RAM/disk/CPU across the envelope ladder, esp. at tight envelopes where a bucket
     scan exceeds the cache (G2), reads sequentially from disk (G3), and decodes on the miss path
     (G1a). v3 (AVX2) shows only on x86 — on arm Mac before/after isolates G1a+G2+G3.

Isolates SIZE (or scan-size) as the sole variable and captures ALL resource axes:
  RECALL (oracle tie-aware, xyzDB exact = canary) · LATENCY warmed p50/p99 · RAM build-peak /
  query-peak / at-rest · DISK (du) · CPU%% (mean+peak during the burst) · SERVES/OOM.

Corpora (real bge vectors, INV-3):
  --corpus pool  → deterministic 180k sample of Corpus A cvec.npy.
  --corpus full  → all 189,514 real vecs.
  --size S       → bucket size; S>=N ⇒ ONE bucket (mono, scan-at-scale).
"""
import argparse
import json
import os
import signal
import subprocess
import threading
import time

import numpy as np

# A build that neither OOMs nor finishes (paged on-disk HNSW at a tight envelope) is UNVIABLE —
# a coverage result, not a hang to eat the run. Override via BUILD_TIMEOUT env (seconds).
BUILD_TIMEOUT = int(os.environ.get("BUILD_TIMEOUT", 1800))
from adapters import ADAPTERS
from measure_x import PeakSampler, hnsw_from_env, PORTS, disk_mb, wait_ready, docker_mem_mb
import recall_harness as rh

CORP = os.environ.get("BENCH_CORP",
                      os.path.join(os.path.dirname(os.path.abspath(__file__)), "corpora", "lme"))
POOL_N = 180_000
POOL_SEED = 20260706


def cpu_perc(container: str) -> float:
    try:
        out = subprocess.run(["docker", "stats", "--no-stream", "--format", "{{.CPUPerc}}", container],
                             capture_output=True, text=True, timeout=15).stdout.strip()
        return float(out.replace("%", "")) if out else -1.0
    except Exception:
        return -1.0


def container_alive(container: str) -> bool:
    try:
        out = subprocess.run(["docker", "inspect", "-f", "{{.State.Running}}", container],
                             capture_output=True, text=True, timeout=15).stdout.strip()
        return out == "true"
    except Exception:
        return False


def oom_killed(container: str) -> bool:
    try:
        out = subprocess.run(["docker", "inspect", "-f", "{{.State.OOMKilled}}", container],
                             capture_output=True, text=True, timeout=15).stdout.strip()
        return out == "true"
    except Exception:
        return False


class CpuSampler(threading.Thread):
    def __init__(self, container):
        super().__init__(daemon=True)
        self.container, self.samples = container, []
        self._ev = threading.Event()

    def run(self):
        while not self._ev.is_set():
            p = cpu_perc(self.container)
            if p >= 0: self.samples.append(p)
            self._ev.wait(0.3)

    def stop(self):
        self._ev.set(); self.join(timeout=3)
        if not self.samples: return -1.0, -1.0
        return round(sum(self.samples) / len(self.samples), 1), round(max(self.samples), 1)


def load_data(corpus, target_n=0):
    cvec = np.load(f"{CORP}/cvec.npy")
    qvec = np.load(f"{CORP}/qvec.npy")
    meta = json.load(open(f"{CORP}/meta.json"))
    if corpus == "pool":
        rng = np.random.default_rng(POOL_SEED)
        idx = rng.choice(cvec.shape[0], size=POOL_N, replace=False)
        data = cvec[idx]
    elif corpus == "tiled":
        # Superbucket scan-at-scale: tile the real 189K to target_n rows (distinct ids, repeated
        # REAL vectors — no synthetic/perturbed content). Scan cost is Θ(N·d), content-independent
        # (founder-accepted framing), so this honestly measures scan/stream/RAM/latency at scale.
        # Recall degrades to a before==after EXACTNESS canary only (dups → score ties, tie-aware=1.0).
        idx = np.arange(target_n) % cvec.shape[0]
        data = cvec[idx]
    else:                                     # full
        data = cvec
    held = [j for j, q in enumerate(meta["queries"]) if q["split"] == "held"]
    return data.astype(np.float32), qvec[held].astype(np.float32)


def base_record(args, s, n, n_buckets):
    return {"kind": "sizesweep", "engine": args.engine, "corpus": f"{args.corpus}-{n}",
            "envelope": args.envelope, "storage": args.storage, "image": args.image,
            "round": args.round, "bucket_size": s, "n_buckets": n_buckets, "data_n": n,
            "scoped": bool(args.scoped)}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--engine", required=True, choices=list(ADAPTERS))
    ap.add_argument("--container", required=True)
    ap.add_argument("--volume", default="")
    ap.add_argument("--disk_path", default="")
    ap.add_argument("--storage", default="local")
    ap.add_argument("--corpus", default="pool", choices=["pool", "full", "tiled"])
    ap.add_argument("--target_n", type=int, default=0)       # for corpus=tiled: total rows (tile real to N)
    ap.add_argument("--size", type=int, required=True)       # bucket size; >=N ⇒ mono
    ap.add_argument("--envelope", default="?")
    ap.add_argument("--image", default="?")                  # before / after (label only)
    ap.add_argument("--round", type=int, default=1)          # A/B/A/B round index
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--scoped", type=int, default=1)        # 1=per-bucket structure, 0=flat (one HNSW over N)
    ap.add_argument("--warmup", type=int, default=1)
    ap.add_argument("--repeats", type=int, default=3)
    ap.add_argument("--max_queries", type=int, default=0)    # cap (mono scan is O(N)·expensive)
    ap.add_argument("--out", required=True)
    args = ap.parse_args()
    port = PORTS[args.engine]

    data, qv = load_data(args.corpus)
    n = len(data)
    s = min(args.size, n)                                    # mono if size>=n
    n_buckets = (n + s - 1) // s
    bids = np.arange(n, dtype=np.int64) // s
    dim = data.shape[1]
    if args.max_queries > 0:
        qv = qv[:args.max_queries]

    Adapter = ADAPTERS[args.engine]
    hnsw = hnsw_from_env(args.engine)
    adapter = Adapter(host=args.host, port=port, dim=dim, hnsw=hnsw, scoped=bool(args.scoped))

    # BUILD — may OOM (docker kills) or hang paging (tight envelope) building the index. Both are
    # RESULTS (serves=False), separated: oom_during_load vs unviable_build_timeout vs crash.
    signal.signal(signal.SIGALRM, lambda *_: (_ for _ in ()).throw(TimeoutError(f"build > {BUILD_TIMEOUT}s")))
    bsamp = PeakSampler(args.container); bsamp.start()
    t0 = time.perf_counter()
    try:
        signal.alarm(BUILD_TIMEOUT)
        adapter.load(data, bids, hnsw)
        signal.alarm(0)
        load_s = round(time.perf_counter() - t0, 1)
        build_peak = bsamp.stop()
    except Exception as e:
        signal.alarm(0)
        at = round(time.perf_counter() - t0, 1)
        bpeak = bsamp.stop()
        killed = oom_killed(args.container)
        st = ("unviable_build_timeout" if isinstance(e, TimeoutError)
              else "oom_during_load" if killed else "crash_during_load")
        rec = base_record(args, s, n, n_buckets)
        rec.update({"serves": False, "status": st, "oom_killed": killed, "oom_at_s": at,
                    "build_ram_peak_mb": round(bpeak, 1), "phase": "build", "err": str(e)[:120]})
        open(args.out, "a").write(json.dumps(rec) + "\n"); print(json.dumps(rec)); return

    # Oracle cutoffs (exact f64) — engine-independent, once per query.
    cut = []
    for j in range(len(qv)):
        b = j % n_buckets
        bv = data[b * s:min((b + 1) * s, n)]
        cut.append((b, rh.kth_oracle_score(qv[j], bv, 10), rh.kth_oracle_score(qv[j], bv, 50)))

    # WARMUP (not timed).
    try:
        for _ in range(max(0, args.warmup)):
            for j in range(len(qv)):
                adapter.query(qv[j], cut[j][0], 50)
    except Exception:
        pass

    # QUERY burst: warmed latency + RAM-peak + CPU%. May die at a tight envelope (query balloon).
    qsamp = PeakSampler(args.container); qsamp.start()
    csamp = CpuSampler(args.container); csamp.start()
    r10, r50, lat = [], [], []
    served = True
    try:
        for _ in range(max(1, args.repeats)):
            for j in range(len(qv)):
                b, c10, c50 = cut[j]
                t = time.perf_counter()
                got = adapter.query(qv[j], b, 50)
                lat.append((time.perf_counter() - t) * 1e3)
                r10.append(rh.tie_aware_recall(qv[j], got, data, c10, 10))
                r50.append(rh.tie_aware_recall(qv[j], got, data, c50, 50))
    except Exception as e:
        served = False
        query_err = str(e)[:120]
    qpeak = qsamp.stop()
    cpu_mean, cpu_peak = csamp.stop()
    try: adapter.close()
    except Exception: pass

    if not served and not lat:
        rec = base_record(args, s, n, n_buckets)
        rec.update({"serves": False, "status": "oom_during_query", "phase": "query",
                    "oom_killed": oom_killed(args.container), "build_ram_peak_mb": round(build_peak, 1),
                    "load_s": load_s, "err": query_err})
        open(args.out, "a").write(json.dumps(rec) + "\n"); print(json.dumps(rec)); return

    # RAM-at-rest + disk after graceful restart (flushed steady state).
    ram_rest, disk = -1.0, -1.0
    if args.volume or args.disk_path:
        subprocess.run(["docker", "restart", args.container], capture_output=True, timeout=180)
        if wait_ready(args.engine, args.host, port):
            ram_rest = round(sorted(docker_mem_mb(args.container) for _ in range(3))[1], 1)
        disk = disk_mb(args.volume, args.disk_path)

    a = np.array(lat)
    rec = base_record(args, s, n, n_buckets)
    rec.update({
        "serves": True, "status": "ok" if served else "partial_then_died",
        "n_queries": len(qv), "repeats": args.repeats,
        "recall_at_10": round(float(np.mean(r10)), 4), "recall_at_50": round(float(np.mean(r50)), 4),
        "p50_ms": round(float(np.percentile(a, 50)), 3), "p99_ms": round(float(np.percentile(a, 99)), 3),
        "mean_ms": round(float(a.mean()), 3),
        "cpu_mean_pct": cpu_mean, "cpu_peak_pct": cpu_peak,
        "build_ram_peak_mb": round(build_peak, 1), "query_ram_peak_mb": round(qpeak, 1),
        "ram_rest_mb": ram_rest, "disk_mb": disk, "load_s": load_s,
        "setup": getattr(adapter, "setup_cost", None),
    })
    open(args.out, "a").write(json.dumps(rec) + "\n")
    print(json.dumps(rec))


if __name__ == "__main__":
    main()
