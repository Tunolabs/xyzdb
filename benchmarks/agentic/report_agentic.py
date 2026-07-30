#!/usr/bin/env python3
"""Agentic benchmark report — pools rounds and renders one markdown table per
scenario (S1-S6) plus the P7 frontier. Reads results/<scenario>/*.jsonl written by
the measure_s*.py / run_s*.sh. Every config Δ (scoped, qd_variant, deployment,
selectivity, envelope) stays a distinct row — never merged silently.

P7 (design §1 P7): the celda where xyzDB loses is kept VISIBLE as a declared
frontier ("with a natural scope xyzDB wins; in the global blob the ANN win"),
read off the existing size sweep (30k/100k/250k/500k) if present. The report
verifies the ANN>exact cross-over falls inside the swept range.

Usage: report_agentic.py [results_dir]  (default ./results) > report.md
"""
import glob
import json
import os
import sys
from collections import defaultdict


def load(results_dir):
    recs = []
    for f in glob.glob(os.path.join(results_dir, "**", "*.jsonl"), recursive=True):
        for line in open(f):
            line = line.strip()
            if line:
                try: recs.append(json.loads(line))
                except Exception: pass
    return recs


def mean(xs):
    xs = [x for x in xs if isinstance(x, (int, float))]
    return round(sum(xs) / len(xs), 3) if xs else None


def pool(recs, keyfields, valfields):
    """Group by keyfields (pool rounds), average valfields."""
    groups = defaultdict(list)
    for r in recs:
        groups[tuple(r.get(k) for k in keyfields)].append(r)
    out = []
    for key, g in sorted(groups.items(), key=lambda kv: [str(x) for x in kv[0]]):
        row = dict(zip(keyfields, key))
        for v in valfields:
            row[v] = mean([r.get(v) for r in g])
        row["_n"] = len(g)
        # carry a status if every round failed
        sts = [r.get("status") for r in g if r.get("status")]
        if sts and len(sts) == len(g):
            row["status"] = sts[0]
        out.append(row)
    return out


def table(rows, cols, headers=None):
    headers = headers or cols
    out = ["| " + " | ".join(headers) + " |", "|" + "|".join(["---"] * len(cols)) + "|"]
    for r in rows:
        out.append("| " + " | ".join(str(r.get(c, "")) if r.get(c) is not None else "—" for c in cols) + " |")
    return "\n".join(out)


def main():
    rd = sys.argv[1] if len(sys.argv) > 1 else "./results"
    recs = load(rd)
    by = defaultdict(list)
    for r in recs:
        by[r.get("kind")].append(r)

    print("# Agentic benchmark — report\n")
    print(f"_Pooled from {len(recs)} records in `{rd}`. Mac/OrbStack = DIRECTION; the "
          "publishable table is the box run. Every config Δ is a distinct row._\n")

    if by["s1"]:
        print("## S1 — retrieve-and-expand\n")
        rows = pool(by["s1"], ["engine", "qd_variant", "envelope"],
                    ["session_recall", "expand_complete_frac", "roundtrips", "p50_ms", "p99_ms", "disk_total_mb"])
        print(table(rows, ["engine", "qd_variant", "envelope", "session_recall",
                           "expand_complete_frac", "roundtrips", "p50_ms", "p99_ms", "disk_total_mb"]))
        print("\n> Gate: `expand_complete_frac` must be 1.0 for all (correctness). `session_recall` "
              "is retrieval quality (HNSW rivals < 1.0). Roundtrips + disk expose the expand trade "
              "(xyz/scroll/chroma 2 RT; pg-JOIN/payload-dup 1 RT; payload-dup pays disk).\n")

    if by["s5"]:
        print("## S5 — hybrid search (recall vs selectivity)\n")
        rows = pool(by["s5"], ["engine", "envelope", "selectivity"], ["recall", "p50_ms", "p99_ms"])
        print(table(rows, ["engine", "envelope", "selectivity", "recall", "p50_ms", "p99_ms"]))
        print("\n> qdrant comes best-armed here (filterable-HNSW). Recall is vs the oracle over the "
              "filtered set; same universe across engines (deterministic metadata).\n")

    if by["s6"]:
        print("## S6 — one engine for the agent (per deployment)\n")
        rows = pool(by["s6"], ["deployment", "envelope"],
                    ["write_p50_ms", "update_p50_ms", "near_p50_ms", "incons_p50_ms", "total_disk_mb"])
        print(table(rows, ["deployment", "envelope", "write_p50_ms", "update_p50_ms",
                           "near_p50_ms", "incons_p50_ms", "total_disk_mb"]))
        print("\n> PG ties on one-system (conceded). The +store deployments pay the stack tax: "
              "`incons_p50_ms` (inconsistency window) and summed `total_disk_mb`.\n")

    if by["s2"]:
        print("## S2 — live session (write<->search)\n")
        rows = pool(by["s2"], ["engine", "envelope"],
                    ["insert_p50_ms", "query_p50_ms", "query_p99_ms", "degradation_late_over_early", "visibility"])
        print(table(rows, ["engine", "envelope", "insert_p50_ms", "query_p50_ms", "query_p99_ms",
                           "degradation_late_over_early", "visibility"]))
        print("\n> Durability equalised durable-strict (chroma labelled). `degradation` = query p50 "
              "late/early (maintenance shows here); `visibility` should be 1.0 (fresh + durable).\n")

    if by["s3"]:
        print("## S3 — fleet lifecycle (RAM vs #tenants)\n")
        rows = pool(by["s3"], ["engine", "envelope", "step"],
                    ["tenants", "ram_mb", "ram_over_base_mb", "create_nth_ms", "destroy_nth_ms"])
        print(table(rows, ["engine", "envelope", "step", "tenants", "ram_mb",
                           "ram_over_base_mb", "create_nth_ms", "destroy_nth_ms"]))
        print("\n> The RAM curve is the result. An OOM record while growing (e.g. chroma at 1000) is "
              "the frontier the scenario exists to show, not a failure.\n")

    if by["s4"]:
        print("## S4 — serverless wake (TTFQ)\n")
        rows = pool(by["s4"], ["engine", "envelope"], ["ttfq_ms", "ram_rest_mb", "disk_total_mb"])
        print(table(rows, ["engine", "envelope", "ttfq_ms", "ram_rest_mb", "disk_total_mb"]))
        print("\n> TTFQ = restart -> first successful query, across all tiers incl. the tightest. "
              "`ttfq_timeout` = a rival that never woke in the tier.\n")

    # P7 — the frontier, read off the size sweep (kind=query with a size corpus label).
    sweep = [r for r in by.get("query", []) if str(r.get("corpus")) in ("30k", "100k", "250k", "500k")]
    print("## P7 — the frontier (kept visible)\n")
    if sweep:
        rows = pool(sweep, ["engine", "corpus"], ["p50_ms", "recall"])
        print(table(rows, ["engine", "corpus", "p50_ms", "recall"]))
        # cross-over: first size where an ANN rival's p50 beats xyzdb's
        xyz = {r["corpus"]: r["p50_ms"] for r in rows if r["engine"] == "xyzdb"}
        cross = None
        for sz in ("30k", "100k", "250k", "500k"):
            others = [r["p50_ms"] for r in rows if r["corpus"] == sz and r["engine"] != "xyzdb"
                      and isinstance(r.get("p50_ms"), (int, float))]
            if sz in xyz and others and min(others) < xyz[sz]:
                cross = sz; break
        print(f"\n> Declared frontier: with a natural scope xyzDB wins; in the global blob the ANN "
              f"win on latency. ANN>exact cross-over first seen at **{cross or 'not within 30k-500k'}**. "
              f"{'Inside the swept range — no 1M run needed.' if cross else 'Cross not captured — a 1M point may be warranted (verify before running).'}\n")
    else:
        print("_No size-sweep records found. Run the signature sweep "
              "(run_signature_after.sh / run_rival_coverage.sh) to populate the P7 frontier, then "
              "re-run this report._\n")


if __name__ == "__main__":
    main()
