#!/usr/bin/env python3
"""Engine-generic measurement for the cross-engine agentic sweep (rebuilt, 0.9).

Dispatches to adapters.py (xyzdb/pgvector/qdrant/chroma). Modes:
  query    -> recall (vs brute-force oracle), p50/p99/mean latency, RAM-peak, CPU-ish
  footprint-> RAM-at-rest (median-3, post graceful restart) + disk (du on the volume)
  both     -> one load, then query, then restart+footprint

HNSW params come from BENCH_* env (dense=tuned / light=idiomatic); xyzDB ignores them.
Mac/OrbStack numbers are DIRECTION (page-cache mediated) — the publishable table is m6a.
"""
import argparse
import json
import os
import subprocess
import threading
import time

import numpy as np
from adapters import ADAPTERS

PORTS = {"xyzdb": 2505, "pgvector": 5432, "qdrant": 6333, "chroma": 8000}


def docker_mem_mb(container: str) -> float:
    try:
        out = subprocess.run(["docker", "stats", "--no-stream", "--format", "{{.MemUsage}}", container],
                             capture_output=True, text=True, timeout=15).stdout.strip()
        used = out.split("/")[0].strip()
        num = float("".join(c for c in used if (c.isdigit() or c == ".")))
        u = used.lower()
        if "gib" in u: num *= 1024
        elif "kib" in u: num /= 1024
        return num
    except Exception:
        return -1.0


class PeakSampler(threading.Thread):
    def __init__(self, container):
        super().__init__(daemon=True)
        self.container, self.peak = container, 0.0
        self._ev = threading.Event()

    def run(self):
        while not self._ev.is_set():
            m = docker_mem_mb(self.container)
            if m > self.peak: self.peak = m
            self._ev.wait(0.4)

    def stop(self):
        self._ev.set(); self.join(timeout=2); return self.peak


def wait_ready(engine, host, port, timeout=120) -> bool:
    import socket
    for _ in range(timeout):
        try:
            with socket.create_connection((host, port), timeout=1):
                time.sleep(2); return True
        except OSError:
            time.sleep(1)
    return False


# ── image / run stamp (change 6) ─────────────────────────────────────────────
# Each measured record carries the engine image and bench commit so a result can
# be traced to the exact binary it measured. Premise-20 (a 2026-07-05 stale xyz
# image measured as if it were 0.9.5) is only detectable if the image is on the
# record. Computed once at import; XYZDB_IMG is exported by lib_docker.sh.
def _git_short():
    try:
        r = subprocess.run(["git", "rev-parse", "--short", "HEAD"],
                           capture_output=True, text=True, timeout=5)
        return r.stdout.strip() or "?"
    except Exception:
        return "?"


_STAMP = {"bench_commit": _git_short(),
          "xyzdb_image": os.environ.get("XYZDB_IMG", "")}


def bench_stamp():
    """Provenance stamp for a measured record: {bench_commit, xyzdb_image}."""
    return dict(_STAMP)


# ── post-load settle to state parity (change 3) ──────────────────────────────
def _reconnect(a, engine, host, port):
    """Re-establish the adapter's transport after a container restart (the handles
    held before the restart are dead). Mirrors each adapter's __init__ connect."""
    if engine == "xyzdb":
        import xyzdb_minimal as xyzdb
        a.db = xyzdb.connect(host, port)
    elif engine == "pgvector":
        import psycopg2
        from pgvector.psycopg2 import register_vector
        a.conn = psycopg2.connect(host=host, port=port, user="postgres",
                                  password="bench", dbname="postgres")
        register_vector(a.conn)
    elif engine == "qdrant":
        from qdrant_client import QdrantClient
        a.client = QdrantClient(host=host, port=port, timeout=600)
    elif engine == "chroma":
        import chromadb
        a.client = chromadb.HttpClient(host=host, port=port)
        if getattr(a, "scoped", False):
            a._colls = {int(c.name.split("_", 1)[1]): c
                        for c in a.client.list_collections() if c.name.startswith("mem_")}
        else:
            a._flat = a.client.get_collection("mem")


def _probe(a, engine):
    """Cheap serving check — confirms the engine answers, not just that the port is
    open (pg initdb, chroma boot can lag the TCP listen)."""
    if engine == "xyzdb":
        a.db.execute('SHOW LOBES')
    elif engine == "pgvector":
        c = a.conn.cursor(); c.execute("SELECT 1"); c.fetchone()
    elif engine == "qdrant":
        a.client.get_collections()
    elif engine == "chroma":
        a.client.heartbeat()


def settle(container, engine, host, port, adapter, cap_s=180):
    """Post-load settle so every engine measures on SETTLED state (parity, not a
    trick): restart the container — for xyzDB this seals+flushes the memtable to
    disk (the native flush mechanism; the rivals already persisted their index in
    load()) — then reconnect and confirm serving. Declared in the report protocol.
    Returns the settle wall-time in ms."""
    t0 = time.perf_counter()
    # -t 60: under memory pressure (tight tiers) xyzDB's shutdown flush can exceed
    # docker's default 10s stop grace -> SIGKILL(137) mid-shutdown + WAL replay on
    # start (slow) + the cell watchdog seeing a "dead" container. 60s lets the
    # engine seal+flush gracefully; fast engines are unaffected (they exit sooner).
    subprocess.run(["docker", "restart", "-t", "60", container], capture_output=True, timeout=cap_s)
    last = None
    while time.perf_counter() - t0 < cap_s:
        try:
            _reconnect(adapter, engine, host, port)
            _probe(adapter, engine)
            return (time.perf_counter() - t0) * 1e3
        except Exception as e:
            last = e
            time.sleep(0.5)
    raise RuntimeError(f"{engine} did not settle within {cap_s}s: {last}")


def hnsw_from_env(engine: str) -> dict:
    g = os.environ.get
    if engine == "pgvector":
        return {"m": int(g("BENCH_PG_M", 16)), "efc": int(g("BENCH_PG_EFC", 64)), "efs": int(g("BENCH_PG_EFS", 100))}
    if engine == "qdrant":
        return {"m": int(g("BENCH_QD_M", 16)), "efc": int(g("BENCH_QD_EFC", 100)), "ef": int(g("BENCH_QD_EF", 128))}
    if engine == "chroma":
        return {"m": int(g("BENCH_CH_M", 16)), "cef": int(g("BENCH_CH_CEF", 100)), "sef": int(g("BENCH_CH_SEF", 128))}
    return {}


def disk_mb(volume: str, disk_path: str = "") -> float:
    """On-disk footprint. Bind-mount (AWS /mnt/ssd|/mnt/hdd) → host `du` directly;
    named volume (Mac default) → `du` inside a busybox mounting the volume."""
    try:
        if disk_path:
            out = subprocess.run(["du", "-sk", disk_path], capture_output=True, text=True, timeout=60).stdout.split()
        elif volume:
            out = subprocess.run(["docker", "run", "--rm", "-v", f"{volume}:/d", "busybox",
                                  "du", "-sk", "/d"], capture_output=True, text=True, timeout=60).stdout.split()
        else:
            return -1.0
        return round(int(out[0]) / 1024, 1) if out else -1.0
    except Exception:
        return -1.0


def do_query(adapter, args, c, load_s):
    qvecs, qbucket, oracle = c["qvecs"], c["q_bucket"], c["oracle"]
    k = int(c["meta"][3])
    nq = len(qvecs) if args.max_queries <= 0 else min(args.max_queries, len(qvecs))
    for j in range(nq):  # warmup
        adapter.query(qvecs[j], int(qbucket[j]), k)
    sampler = PeakSampler(args.container); sampler.start()
    lat, rec = [], []
    for _ in range(max(1, args.repeats)):
        for j in range(nq):
            oid = {int(x) for x in oracle[j]}
            t0 = time.perf_counter()
            got = adapter.query(qvecs[j], int(qbucket[j]), k)
            lat.append((time.perf_counter() - t0) * 1e3)
            rec.append(len(set(got) & oid) / k)
    peak = sampler.stop()
    a = np.array(lat)
    return {"kind": "query", "engine": args.engine, "envelope": args.envelope,
            "corpus": args.corpus_label, "pass": args.pass_label, "storage": args.storage,
            "round": args.round, "regime": args.regime, "k": k, "n_queries": nq,
            "recall": round(float(np.mean(rec)), 4),
            "p50_ms": round(float(np.percentile(a, 50)), 3),
            "p99_ms": round(float(np.percentile(a, 99)), 3),
            "mean_ms": round(float(a.mean()), 3),
            "ram_peak_mb": round(peak, 1), "load_s": round(load_s, 1), "status": None}


def do_footprint(args, load_s):
    subprocess.run(["docker", "restart", args.container], capture_output=True, timeout=180)
    if not wait_ready(args.engine, args.host, args.port):
        return {"kind": "footprint", "engine": args.engine, "envelope": args.envelope,
                "corpus": args.corpus_label, "pass": args.pass_label, "storage": args.storage,
                "round": args.round, "status": "restart_failed"}
    ram = sorted(docker_mem_mb(args.container) for _ in range(3))[1]
    return {"kind": "footprint", "engine": args.engine, "envelope": args.envelope,
            "corpus": args.corpus_label, "pass": args.pass_label, "storage": args.storage,
            "round": args.round, "regime": args.regime, "ram_rest_mb": round(ram, 1),
            "disk_total_mb": disk_mb(args.volume, args.disk_path), "load_s": round(load_s, 1), "status": None}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--engine", required=True, choices=list(ADAPTERS))
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=0)
    ap.add_argument("--container", required=True)
    ap.add_argument("--volume", default="")
    ap.add_argument("--disk_path", default="")   # host path when bind-mounted (AWS /mnt/ssd|/mnt/hdd)
    ap.add_argument("--storage", default="local")  # ssd | hdd | local — annotated in every record
    ap.add_argument("--corpus", required=True)
    ap.add_argument("--corpus_label", default="?")
    ap.add_argument("--pass_label", default="light")
    ap.add_argument("--mode", choices=["query", "footprint", "both"], default="query")
    ap.add_argument("--envelope", default="?")
    ap.add_argument("--regime", default="hot-cache")
    ap.add_argument("--round", type=int, default=1)
    ap.add_argument("--max_queries", type=int, default=0)
    ap.add_argument("--repeats", type=int, default=1)
    ap.add_argument("--out", required=True)
    args = ap.parse_args()
    if not args.port:
        args.port = PORTS[args.engine]

    c = np.load(args.corpus)
    dim = int(c["meta"][2])
    Adapter = ADAPTERS[args.engine]
    adapter = Adapter(host=args.host, port=args.port, dim=dim, hnsw=hnsw_from_env(args.engine))
    t0 = time.perf_counter()
    adapter.load(c["vecs"], c["bucket_ids"], hnsw_from_env(args.engine))
    load_s = time.perf_counter() - t0

    recs = []
    if args.mode in ("query", "both"):
        recs.append(do_query(adapter, args, c, load_s))
    adapter.close()
    if args.mode in ("footprint", "both"):
        recs.append(do_footprint(args, load_s))

    with open(args.out, "a") as f:
        for r in recs:
            f.write(json.dumps(r) + "\n")
    for r in recs:
        print(json.dumps(r))


if __name__ == "__main__":
    main()
