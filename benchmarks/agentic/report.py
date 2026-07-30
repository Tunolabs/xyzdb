#!/usr/bin/env python3
"""Pivot the cross-engine sweep results into markdown tables.

Reads results/<sub>/*.jsonl and prints, per storage label:
  - a QUERY table (serves / recall / p50 / p99 / RAM-peak), envelope×corpus × engine
  - a FOOTPRINT table (RAM-at-rest / disk), envelope×corpus × engine

Usage: report.py results/small   (or results/m6a-ssd, etc.)
"""
import json
import sys
import glob
import os
from collections import defaultdict

ENGINES = ["xyzdb", "pgvector", "qdrant", "chroma"]


def load(resdir):
    q, f = {}, {}   # (storage, env, corpus, engine) -> record
    for path in glob.glob(os.path.join(resdir, "*.jsonl")):
        for line in open(path):
            line = line.strip()
            if not line:
                continue
            d = json.loads(line)
            key = (d.get("storage", "local"), d.get("envelope", "?"), d.get("corpus", "?"), d.get("engine", "?"))
            (q if d.get("kind") == "query" else f)[key] = d
    return q, f


def cell_q(d):
    if d is None:
        return "—"
    if d.get("status"):
        return "OOM" if "OOM" in d["status"] or "oom" in d["status"] else "FAIL"
    return f"{d['recall']:.3f} · {d['p50_ms']:.1f}/{d['p99_ms']:.1f}ms · {d['ram_peak_mb']:.0f}MB"


def cell_f(d):
    if d is None or d.get("status"):
        return "—"
    return f"{d.get('ram_rest_mb','?')}MB · {d.get('disk_total_mb','?')}MB"


def table(records, cellfn, resdir):
    storages = sorted({k[0] for k in records})
    out = []
    for st in storages:
        rows = sorted({(k[1], k[2]) for k in records if k[0] == st})
        out.append(f"\n### storage = {st}\n")
        out.append("| envelope · corpus | " + " | ".join(ENGINES) + " |")
        out.append("|" + "---|" * (len(ENGINES) + 1))
        for env, corp in rows:
            cells = [cellfn(records.get((st, env, corp, e))) for e in ENGINES]
            out.append(f"| {env} · {corp} | " + " | ".join(cells) + " |")
    return "\n".join(out)


def main():
    resdir = sys.argv[1] if len(sys.argv) > 1 else "results/small"
    q, f = load(resdir)
    if not q and not f:
        print(f"no records in {resdir}")
        return
    print(f"# Cross-engine sweep — {resdir}")
    print("\nMac/OrbStack = DIRECTION (page-cache mediated); publishable table = AWS m6a native x86.")
    print("\n## Query — recall · p50/p99 · RAM-peak")
    print(table(q, cell_q, resdir))
    print("\n## Footprint — RAM-at-rest · disk")
    print(table(f, cell_f, resdir))


if __name__ == "__main__":
    main()
