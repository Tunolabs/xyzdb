#!/usr/bin/env python3
"""xyzdb-only Fase-1 direction measurement (query + footprint modes).

Reimplements the agentic harness's measure step against the current 0.9 image
(NOT a restore of the lost scratchpad tool). Single engine, no rivals — the
A/B is xyzdb-before (v0.8.13) vs xyzdb-after (post-Fase-1), same harness, same
box. Numbers are Mac/ARM DIRECTION only (OrbStack mediates page cache; absolute
magnitude is the m6a block). Record schema follows docs/agentic-bench/03.

Metrics (never merged): query → recall, p50/p99/mean latency, RAM-peak (balloon).
footprint → RAM-at-rest (post graceful restart), disk-at-rest (du on the mount).
"""
import argparse
import json
import subprocess
import threading
import time

import numpy as np
import xyzdb_minimal as xyzdb

CHUNK = 600  # records per PUT BATCH; full-precision 1024-d floats ~22 KB/rec → ~13 MB < 16 MiB frame


def _nearest(db, lobe, bucket_val, qvec, k):
    """Bucket-scoped exact NEAREST via the minimal client; returns raw record dicts.

    `$q` is a bound V4 param; k is a literal (the parser rejects `$k` in NEAREST(...)).
    Same statement the fluent SDK builder emitted — identical engine work."""
    out = db.execute(
        f'SCAN "{lobe}" WHERE bucket = $b | NEAREST(emb, $q, {int(k)}, cosine)',
        {"b": str(bucket_val), "q": qvec.tolist()})
    return out.get("records", []) or []


def docker_mem_mb(container: str) -> float:
    """Container RSS in MiB via `docker stats` (the number OOM is judged on)."""
    try:
        out = subprocess.run(
            ["docker", "stats", "--no-stream", "--format", "{{.MemUsage}}", container],
            capture_output=True, text=True, timeout=10,
        ).stdout.strip()
        used = out.split("/")[0].strip()  # e.g. "123.4MiB"
        num = float("".join(c for c in used if (c.isdigit() or c == ".")))
        u = used.lower()
        if "gib" in u:
            num *= 1024
        elif "kib" in u:
            num /= 1024
        return num
    except Exception:
        return -1.0


class PeakSampler(threading.Thread):
    """Poll container RSS every 0.4 s, track the peak (the query balloon)."""

    def __init__(self, container: str):
        super().__init__(daemon=True)
        self.container = container
        self.peak = 0.0
        # NB: NOT `self._stop` — that shadows threading.Thread's internal _stop().
        self._stopev = threading.Event()

    def run(self):
        while not self._stopev.is_set():
            m = docker_mem_mb(self.container)
            if m > self.peak:
                self.peak = m
            self._stopev.wait(0.4)

    def stop(self) -> float:
        self._stopev.set()
        self.join(timeout=2)
        return self.peak


def wait_ready(host: str, port: int, timeout: int = 60) -> bool:
    import socket
    for _ in range(timeout):
        try:
            with socket.create_connection((host, port), timeout=1):
                time.sleep(1)
                return True
        except OSError:
            time.sleep(1)
    return False


def load_corpus(db, lobe: str, c) -> float:
    """Create schema + PUT the galaxy. Returns load wall-seconds."""
    db.execute(f'LOBE "{lobe}" HINT="fase1 galaxy"')
    db.execute(f'VECTOR emb IN "{lobe}"')
    db.execute(f'GRAVITY BY bucket IN "{lobe}"')
    vecs, bids = c["vecs"], c["bucket_ids"]
    n = len(vecs)
    t0 = time.perf_counter()
    for start in range(0, n, CHUNK):
        end = min(start + CHUNK, n)
        recs = [
            {"*bucket": f"b{int(bids[i])}", "id": f"g{i}", "emb": vecs[i].tolist()}
            for i in range(start, end)
        ]
        db.put_batch(lobe, recs)
    return time.perf_counter() - t0


def run_query(args, c) -> dict:
    db = xyzdb.connect(args.host, args.port)
    load_s = load_corpus(db, args.lobe, c)
    qvecs, qbucket, oracle = c["qvecs"], c["q_bucket"], c["oracle"]
    k = int(c["meta"][3])
    nq = len(qvecs) if args.max_queries <= 0 else min(args.max_queries, len(qvecs))

    # warmup pass (discarded), also checks recall correctness early
    for j in range(nq):
        _nearest(db, args.lobe, f"b{int(qbucket[j])}", qvecs[j], k)

    sampler = PeakSampler(args.container)
    sampler.start()
    lat_ms, recalls = [], []
    for _ in range(max(1, args.repeats)):
        for j in range(nq):
            oid = {f"g{int(x)}" for x in oracle[j]}
            t0 = time.perf_counter()
            recs = _nearest(db, args.lobe, f"b{int(qbucket[j])}", qvecs[j], k)
            lat_ms.append((time.perf_counter() - t0) * 1e3)
            got = {r["id"] for r in recs if "id" in r}
            recalls.append(len(got & oid) / k)
    peak = sampler.stop()
    db.close()

    a = np.array(lat_ms)
    return {
        "kind": "query", "engine": "xyzdb", "image": args.image, "round": args.round,
        "envelope": args.envelope, "regime": args.regime, "k": k, "n_queries": nq,
        "recall": round(float(np.mean(recalls)), 4),
        "p50_ms": round(float(np.percentile(a, 50)), 3),
        "p99_ms": round(float(np.percentile(a, 99)), 3),
        "mean_ms": round(float(a.mean()), 3),
        "std_ms": round(float(a.std()), 3),
        "ram_peak_mb": round(peak, 1), "load_s": round(load_s, 1), "status": None,
    }


def run_footprint(args, c) -> dict:
    db = xyzdb.connect(args.host, args.port)
    load_s = load_corpus(db, args.lobe, c)
    db.close()
    # graceful restart = the uniform flush mechanism (seals+flushes all trees)
    subprocess.run(["docker", "restart", args.container], capture_output=True, timeout=120)
    if not wait_ready(args.host, args.port):
        return {"kind": "footprint", "engine": "xyzdb", "image": args.image,
                "round": args.round, "envelope": args.envelope, "status": "restart_failed"}
    ram = sorted(docker_mem_mb(args.container) for _ in range(3))[1]  # median-3
    du = subprocess.run(["du", "-sk", f"{args.datadir}/xyzdb"],
                        capture_output=True, text=True).stdout.split()
    disk_mb = round(int(du[0]) / 1024, 1) if du else -1.0
    return {
        "kind": "footprint", "engine": "xyzdb", "image": args.image, "round": args.round,
        "envelope": args.envelope, "regime": args.regime,
        "ram_rest_mb": round(ram, 1), "disk_total_mb": disk_mb,
        "load_s": round(load_s, 1), "status": None,
    }


def run_both(args, c) -> list:
    """One load → query metrics → graceful restart → footprint metrics.
    Halves load cost vs running query and footprint as separate loads."""
    db = xyzdb.connect(args.host, args.port)
    load_s = load_corpus(db, args.lobe, c)
    qvecs, qbucket, oracle = c["qvecs"], c["q_bucket"], c["oracle"]
    k = int(c["meta"][3])
    nq = len(qvecs) if args.max_queries <= 0 else min(args.max_queries, len(qvecs))
    for j in range(nq):  # warmup
        _nearest(db, args.lobe, f"b{int(qbucket[j])}", qvecs[j], k)
    sampler = PeakSampler(args.container)
    sampler.start()
    lat_ms, recalls = [], []
    for _ in range(max(1, args.repeats)):
        for j in range(nq):
            oid = {f"g{int(x)}" for x in oracle[j]}
            t0 = time.perf_counter()
            recs = _nearest(db, args.lobe, f"b{int(qbucket[j])}", qvecs[j], k)
            lat_ms.append((time.perf_counter() - t0) * 1e3)
            got = {r["id"] for r in recs if "id" in r}
            recalls.append(len(got & oid) / k)
    peak = sampler.stop()
    db.close()
    a = np.array(lat_ms)
    qrec = {
        "kind": "query", "engine": "xyzdb", "image": args.image, "round": args.round,
        "envelope": args.envelope, "regime": args.regime, "k": k, "n_queries": nq,
        "recall": round(float(np.mean(recalls)), 4),
        "p50_ms": round(float(np.percentile(a, 50)), 3),
        "p99_ms": round(float(np.percentile(a, 99)), 3),
        "mean_ms": round(float(a.mean()), 3), "std_ms": round(float(a.std()), 3),
        "ram_peak_mb": round(peak, 1), "load_s": round(load_s, 1), "status": None,
    }
    # graceful restart → footprint
    subprocess.run(["docker", "restart", args.container], capture_output=True, timeout=180)
    frec = {"kind": "footprint", "engine": "xyzdb", "image": args.image, "round": args.round,
            "envelope": args.envelope, "regime": args.regime, "status": "restart_failed"}
    if wait_ready(args.host, args.port):
        ram = sorted(docker_mem_mb(args.container) for _ in range(3))[1]
        du = subprocess.run(["du", "-sk", f"{args.datadir}/xyzdb"],
                            capture_output=True, text=True).stdout.split()
        disk_mb = round(int(du[0]) / 1024, 1) if du else -1.0
        frec = {"kind": "footprint", "engine": "xyzdb", "image": args.image, "round": args.round,
                "envelope": args.envelope, "regime": args.regime,
                "ram_rest_mb": round(ram, 1), "disk_total_mb": disk_mb,
                "load_s": round(load_s, 1), "status": None}
    return [qrec, frec]


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=2505)
    ap.add_argument("--container", required=True)
    ap.add_argument("--corpus", required=True)
    ap.add_argument("--mode", choices=["query", "footprint", "both"], default="query")
    ap.add_argument("--image", default="unknown")     # before | after (label)
    ap.add_argument("--envelope", default="?")
    ap.add_argument("--regime", default="hot-cache")
    ap.add_argument("--round", type=int, default=1)
    ap.add_argument("--lobe", default="mem")
    ap.add_argument("--max_queries", type=int, default=0)
    ap.add_argument("--repeats", type=int, default=3)
    ap.add_argument("--datadir", default="/tmp/xyzdb-bench-enginedata")
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    c = np.load(args.corpus)
    if args.mode == "both":
        recs = run_both(args, c)
    elif args.mode == "query":
        recs = [run_query(args, c)]
    else:
        recs = [run_footprint(args, c)]
    with open(args.out, "a") as f:
        for rec in recs:
            f.write(json.dumps(rec) + "\n")
    for rec in recs:
        print(json.dumps(rec))


if __name__ == "__main__":
    main()
