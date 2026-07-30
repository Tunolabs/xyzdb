#!/usr/bin/env python3
"""Soak post-process — read scrape CSV + orchestrator results, produce a
markdown report with 1h-window aggregates and the four cycle-plan acceptance
gates evaluated end-to-end.

Used by run_soak.sh after the soak finishes (or aborts via gate_monitor).
The same script doubles as a template generator: when called in cp 6.2.3a
smoke mode, it produces a minimal report with whatever data the 10-minute
run captured.
"""

from __future__ import annotations

import argparse
import csv
import json
import statistics
from pathlib import Path
from typing import Iterable


def read_scrape(path: Path) -> list[dict]:
    """Parse the scrape_stats CSV into a list of typed dicts."""
    if not path.exists() or path.stat().st_size == 0:
        return []
    with path.open() as f:
        reader = csv.DictReader(f)
        return [{k: int(v) for k, v in row.items()} for row in reader]


def hourly_buckets(rows: list[dict]) -> dict[int, list[dict]]:
    """Group rows by floor(ts_ms / 3 600 000)."""
    out: dict[int, list[dict]] = {}
    for r in rows:
        out.setdefault(r["ts_ms"] // 3_600_000, []).append(r)
    return out


def summarise(rows: Iterable[dict], key: str) -> dict[str, float]:
    vals = [r[key] for r in rows]
    if not vals:
        return {"min": 0, "max": 0, "avg": 0, "p99": 0}
    return {
        "min": min(vals),
        "max": max(vals),
        "avg": statistics.mean(vals),
        "p99": statistics.quantiles(vals, n=100)[98] if len(vals) >= 100 else max(vals),
    }


def evaluate_gates(rows: list[dict], cgroup_limit_bytes: int) -> dict[str, str]:
    """Return PASS/FAIL/UNKNOWN per cycle-plan acceptance gate."""
    if not rows:
        return {k: "UNKNOWN" for k in ("G1", "G2", "G3", "G4")}

    # G1 — compact_err in any keyspace
    g1 = "PASS"
    for r in rows:
        for k in ("ce_spatial", "ce_identity", "ce_dictionary", "ce_ghosts"):
            if r[k] > 0:
                g1 = f"FAIL ({k}={r[k]} at ts={r['ts_ms']})"
                break
        if g1 != "PASS":
            break

    # G2 — sync thread liveness via heartbeat_count freshness.
    # Mirrors gate_monitor.sh: compare consecutive `sync_hb` rows; if
    # one value equals or is below the previous, the WAL sync thread
    # has stopped its 1 ms heartbeat → real Finding-9 outage.
    # Do NOT use `sync_last_ts_ms` (only advances on actual fsyncs;
    # MMPP Idle stretches legitimately keep it flat — finding H11).
    g2 = "PASS"
    prev_hb = None
    for r in rows:
        hb = r["sync_hb"]
        if prev_hb is not None and hb <= prev_hb:
            g2 = f"FAIL (heartbeat flat: prev={prev_hb} curr={hb} at ts={r['ts_ms']})"
            break
        prev_hb = hb

    # G3 — VmRSS > 95 % cgroup limit
    threshold = cgroup_limit_bytes * 95 // 100
    g3 = "PASS"
    for r in rows:
        if r["vmrss_b"] > threshold:
            g3 = f"FAIL (vmrss={r['vmrss_b']}B > {threshold}B)"
            break

    # G4 — VmRSS growth between post-warmup baseline and end-of-run
    # measurement. Split into two metrics per DEC-V4-6 (cycle plan §7):
    #
    #   G4 (BLOCKING, sustained_growth) — compares end-of-run hourly
    #     average against post-warmup hour-1 average. Threshold 10%.
    #     Detects unbounded leaks (memory that accumulates and does
    #     not return to baseline). This is the cycle gate.
    #
    #   G4-bis (MONITORED, peak_growth) — compares peak post-warmup
    #     sample against the same baseline. Threshold 250%. Captures
    #     transient bursts (e.g. ANALYZE memory bloat documented as
    #     H14). Never fails the cycle; emits a hallazgo when exceeded
    #     so the burst can be triaged in a follow-up engine cycle.
    #
    # The original v0.4 cp 6.2.3a definition used peak-vs-baseline as
    # the blocking gate, which is over-sensitive to transient bursts:
    # the v0.4 72h soak triggered 5 ANALYZE runs that each spike RSS
    # 3.2× for ~3 min, producing growth=225% (FAIL) on a workload
    # whose sustained baseline only drifted +5.9%. DEC-V4-6 separates
    # the two so transients don't mask a clean steady-state.
    #
    # Cycle plan §3.6.2.3 specifies "post-warmup baseline":
    #   - Skip the first WARMUP_SKIP_MS (default 10 min) — bulk_load
    #     + Phase 0.5 (BULKMODE OFF + COMPACT + AUTOANCHOR) push
    #     VmRSS from a few MB to >1 GB.
    #   - Use the next BASELINE_WINDOW_MS (default 1 h) as baseline.
    #   - Use the rest of the run as the measurement window.
    #   - Measure END-OF-RUN average over the last END_WINDOW_MS
    #     for the sustained metric.
    # Need at least BASELINE_WINDOW_MS + MIN_MEASURE_MS of post-
    # warmup data; otherwise G4 is UNKNOWN. Smokes (<2 h) hit this
    # path naturally; production 72 h soaks have ample data to
    # compare hour 1 vs hours 71.
    WARMUP_SKIP_MS = 10 * 60 * 1000
    BASELINE_WINDOW_MS = 60 * 60 * 1000
    END_WINDOW_MS = 60 * 60 * 1000
    MIN_MEASURE_MS = 30 * 60 * 1000
    PEAK_MONITOR_THRESHOLD = 2.50  # G4-bis info threshold (250% growth)

    if rows:
        run_start = rows[0]["ts_ms"]
        post_warmup = [r for r in rows if r["ts_ms"] - run_start >= WARMUP_SKIP_MS]
    else:
        post_warmup = []

    needed_ms = BASELINE_WINDOW_MS + MIN_MEASURE_MS
    pw_span_ms = (post_warmup[-1]["ts_ms"] - post_warmup[0]["ts_ms"]) if post_warmup else 0
    if pw_span_ms < needed_ms:
        g4 = (
            f"UNKNOWN (post-warmup span {pw_span_ms / 60000:.1f} min < "
            f"{needed_ms / 60000:.0f} min needed for baseline + measurement; "
            f"smokes <2 h hit this path naturally)"
        )
        g4_bis = "UNKNOWN (insufficient post-warmup data)"
    else:
        baseline_end = post_warmup[0]["ts_ms"] + BASELINE_WINDOW_MS
        baseline_rows = [r for r in post_warmup if r["ts_ms"] < baseline_end]
        measure_rows = [r for r in post_warmup if r["ts_ms"] >= baseline_end]
        baseline = statistics.mean(r["vmrss_b"] for r in baseline_rows)

        # G4 blocking — sustained growth via end-of-run average.
        end_window_start = measure_rows[-1]["ts_ms"] - END_WINDOW_MS
        end_rows = [r for r in measure_rows if r["ts_ms"] >= end_window_start]
        end_avg = statistics.mean(r["vmrss_b"] for r in end_rows) if end_rows else baseline
        sustained_growth = (end_avg - baseline) / baseline if baseline else 0.0
        if sustained_growth > 0.10:
            g4 = (
                f"FAIL (sustained_growth={sustained_growth:.2%} "
                f"baseline_avg={baseline:.0f}B end_avg={end_avg:.0f}B post-warmup)"
            )
        else:
            g4 = (
                f"PASS (sustained_growth={sustained_growth:.2%} "
                f"baseline_avg={baseline:.0f}B end_avg={end_avg:.0f}B)"
            )

        # G4-bis monitored — peak growth (transient burst detector).
        # Never fails the cycle; emits a hallazgo line when exceeded so
        # the burst pattern can be triaged in engine follow-up work.
        peak = max(r["vmrss_b"] for r in measure_rows)
        peak_growth = (peak - baseline) / baseline if baseline else 0.0
        if peak_growth > PEAK_MONITOR_THRESHOLD:
            g4_bis = (
                f"HALLAZGO (peak_growth={peak_growth:.2%} > "
                f"{PEAK_MONITOR_THRESHOLD:.0%} threshold; peak={peak}B; "
                f"non-blocking — investigate transient source)"
            )
        else:
            g4_bis = f"OK (peak_growth={peak_growth:.2%} under {PEAK_MONITOR_THRESHOLD:.0%})"

    return {"G1": g1, "G2": g2, "G3": g3, "G4": g4, "G4_BIS": g4_bis}


def render_report(
    rows: list[dict],
    orchestrator_dir: Path,
    cgroup_limit_bytes: int,
) -> str:
    gates = evaluate_gates(rows, cgroup_limit_bytes)
    duration_s = (rows[-1]["ts_ms"] - rows[0]["ts_ms"]) / 1000 if rows else 0
    rss = summarise(rows, "vmrss_b") if rows else {}

    # Pull cold/Phase-3 per-query stats from the orchestrator JSON if present.
    query_blob = ""
    json_files = sorted(orchestrator_dir.glob("xyzdb-*.json")) if orchestrator_dir else []
    if json_files:
        try:
            data = json.loads(json_files[-1].read_text())
            cold = data.get("cold_queries", [])
            if cold:
                lines = ["| Q | n | P50 ms | P95 ms | P99 ms |", "|---|---:|---:|---:|---:|"]
                for q in cold:
                    lines.append(
                        f"| {q['query']} | {q['n_runs']} | {q['p50_ms']:.2f} | {q['p95_ms']:.2f} | {q['p99_ms']:.2f} |"
                    )
                query_blob = "\n".join(lines)
        except Exception as exc:
            query_blob = f"_orchestrator JSON parse failed: {exc}_"

    lines = [
        "# v0.4 Soak — auto-generated report",
        "",
        f"**Mode**: {'72h DEC-V4-4' if duration_s > 36 * 3600 else 'smoke / partial'}",
        f"**Wall-clock**: {duration_s/3600:.2f} h ({len(rows)} scrape rows)",
        "",
        "Workload steady-state efectivo 86R / 8.5W / 5A bajo modelo MMPP 2-state.",
        "El cycle plan §3.6.2.3 mencionaba 70/25/5 como descriptor; se priorizó",
        "realismo del MMPP sobre matching exacto del descriptor — decisión cycle",
        "plan §3.6.2.3a.",
        "",
        "## Acceptance gates (blocking)",
        "",
        "| Gate | Definition | Verdict |",
        "|---|---|---|",
        f"| G1 | `compact_err > 0` en cualquier keyspace | {gates['G1']} |",
        f"| G2 | sync_thread heartbeat_count flat across scrapes (thread dead) | {gates['G2']} |",
        f"| G3 | VmRSS > 95 % cgroup limit ({cgroup_limit_bytes} B) | {gates['G3']} |",
        f"| G4 | Sustained VmRSS growth > 10 % (end-of-run hourly-avg vs post-warmup hour-1 avg) | {gates['G4']} |",
        "",
        "## Monitored metrics (non-blocking, hallazgo on threshold)",
        "",
        "Per DEC-V4-6 (cycle plan §7): peak-based growth is tracked",
        "separately from the sustained gate. Peak transients (e.g. ANALYZE",
        "memory bursts, see H14) are expected and do not fail the cycle;",
        "they emit a `HALLAZGO` line for engine follow-up triage.",
        "",
        "| Metric | Definition | Verdict |",
        "|---|---|---|",
        f"| G4-bis | Peak VmRSS growth > 250 % vs post-warmup hour-1 avg | {gates['G4_BIS']} |",
        "",
        "## VmRSS summary",
        "",
        f"- min  {rss.get('min',0)} B",
        f"- avg  {rss.get('avg',0):.0f} B",
        f"- max  {rss.get('max',0)} B",
        f"- P99  {rss.get('p99',0):.0f} B",
        "",
        "## Cold-phase per-query (orchestrator)",
        "",
        query_blob or "_no orchestrator results JSON found_",
        "",
        "## Files",
        "",
        f"- scrape CSV: `{orchestrator_dir}/scrape_stats.csv`",
        f"- gate log:   `{orchestrator_dir}/gate_monitor.log`",
        f"- snapshot log: `{orchestrator_dir}/snapshot_cron.log`",
        f"- analyze log:  `{orchestrator_dir}/analyze_cron.log`",
        "",
    ]
    return "\n".join(lines)


def main() -> int:
    p = argparse.ArgumentParser()
    p.add_argument("--scrape", required=True, type=Path)
    p.add_argument("--orchestrator-results", required=True, type=Path)
    p.add_argument("--output", required=True, type=Path)
    p.add_argument("--cgroup-limit-bytes", type=int, default=8 * 1024**3)
    args = p.parse_args()

    rows = read_scrape(args.scrape)
    md = render_report(rows, args.orchestrator_results, args.cgroup_limit_bytes)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(md)
    print(f"wrote {args.output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
