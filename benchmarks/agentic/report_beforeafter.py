#!/usr/bin/env python3
"""Render the xyzDB before/after matrix per envelope, with per-cell gate applicability + Δ rows.

Founder's requirement (feedback_gate_bench_presentation): before==after where a gate CANNOT fire
is CORRECT, not a null result — so each cell is tagged with which Fase-1 gate is expected to bite,
and every metric carries a Δ = after − before so improvement (or its absence) is a number.

Gate→regime (computed, not eyeballed):
  scan_MB = min(bucket_size, N)·1024·4   ·   cache_MB = envelope/4   ·   ws_MB = N·1024·4
  G2 (span>cache bypass): FIRES iff serves and scan_MB > cache_MB; else N/A(fits) or OOM.
  G1a/G3 (miss decode / cold readahead): observable where ws_MB > cache_MB (eviction→cold scans).
  RAM-peak(query): balloon = G5/Fase-2, NOT this run → before≈after is expected, not a regression.
  v3 (AVX2): inert on arm Mac (both images arm64) → compute delta pending x86.
"""
import json
import sys

CACHE = {"128M": 32, "256M": 64, "512M": 128, "2G": 512, "8G": 2048}
ENV_ORDER = ["8G", "2G", "512M", "256M", "128M"]
SIZE_ORDER = [("pool", 500), ("pool", 2000), ("pool", 5000), ("full", 380), ("full", 189514)]
MB = 1024 * 4 / 1e6   # bytes per 1024-d f32 vector → MB


def size_label(corpus_pref, bs, data_n):
    if bs >= data_n:
        return f"mono-{data_n//1000}k"
    if corpus_pref == "full":
        return f"dense-{data_n//1000}k/{bs}"
    return f"pool/{bs}"


def gate_tag(serves, scan_mb, cache_mb, ws_mb):
    if not serves:
        return "OOM"
    g2 = "G2✓" if scan_mb > cache_mb else "G2·NA(fits)"
    thr = "cold/thrash→G1a·G3" if ws_mb > cache_mb else "warm"
    return f"{g2} · {thr}"


def fmt(b, a, unit="", pct=False):
    """before → after (Δ). '·' when a side is missing."""
    if b is None and a is None:
        return "· / ·"
    if b is None:
        return f"· → {a}{unit}"
    if a is None:
        return f"{b}{unit} → ·"
    d = a - b
    ds = f"{d:+.1f}" if abs(d) >= 0.05 or d == 0 else f"{d:+.4f}"
    if pct and b:
        ds += f" ({(a-b)/b*100:+.0f}%)"
    return f"{b}{unit} → {a}{unit}  Δ{ds}"


def get(rec, k, default=None):
    return rec.get(k, default) if rec else default


def main():
    path = sys.argv[1] if len(sys.argv) > 1 else "/tmp/xyz_beforeafter.jsonl"
    rows = [json.loads(l) for l in open(path) if l.strip()]
    idx = {}   # (env, corpus_pref, bucket_size, image) -> rec
    for r in rows:
        corpus_pref = r.get("corpus", "?-0").split("-")[0]
        idx[(r["envelope"], corpus_pref, r["bucket_size"], r.get("image"))] = r

    for env in ENV_ORDER:
        cache = CACHE[env]
        print(f"\n{'='*78}\n  ENVELOPE {env}  (cache {cache} MB, 2 cpu)\n{'='*78}")
        print(f"{'size':<15}{'gate expectation':<34}{'serv b/a':<12}{'recall@10 canary':<20}")
        for pref, bs in SIZE_ORDER:
            b = idx.get((env, pref, bs, "before"))
            a = idx.get((env, pref, bs, "after"))
            if not b and not a:
                continue
            data_n = get(a, "data_n") or get(b, "data_n") or bs
            lbl = size_label(pref, bs, data_n)
            scan_mb = min(bs, data_n) * MB
            ws_mb = data_n * MB
            sv_b, sv_a = get(b, "serves"), get(a, "serves")
            serves = bool(sv_a)
            tag = gate_tag(serves, scan_mb, cache, ws_mb)
            can = fmt(get(b, "recall_at_10"), get(a, "recall_at_10"))
            print(f"{lbl:<15}{tag:<34}{str(sv_b)+'/'+str(sv_a):<12}{can:<20}")
            if serves and sv_b:
                # Δ rows only for cells that served in BOTH images (a real before/after comparison).
                print(f"    p50 ms     : {fmt(get(b,'p50_ms'), get(a,'p50_ms'))}")
                print(f"    p99 ms     : {fmt(get(b,'p99_ms'), get(a,'p99_ms'))}")
                print(f"    load s     : {fmt(get(b,'load_s'), get(a,'load_s'))}")
                print(f"    build RAM  : {fmt(get(b,'build_ram_peak_mb'), get(a,'build_ram_peak_mb'),' MB')}")
                print(f"    qRAM peak  : {fmt(get(b,'query_ram_peak_mb'), get(a,'query_ram_peak_mb'),' MB')}  [G5/Fase2 — expect ≈]")
                print(f"    RAM rest   : {fmt(get(b,'ram_rest_mb'), get(a,'ram_rest_mb'),' MB')}  [G1a lives here]")
                print(f"    disk MB    : {fmt(get(b,'disk_mb'), get(a,'disk_mb'))}")
                print(f"    CPU%% mean  : {fmt(get(b,'cpu_mean_pct'), get(a,'cpu_mean_pct'))}")
            elif get(a, "status") or get(b, "status"):
                print(f"    status b/a : {get(b,'status')} / {get(a,'status')}  "
                      f"(build RAM b/a {get(b,'build_ram_peak_mb')}/{get(a,'build_ram_peak_mb')} MB, "
                      f"oom_at b/a {get(b,'oom_at_s')}/{get(a,'oom_at_s')} s)")
    print("\nNote: v3 (AVX2) inert on arm Mac — compute Δ pending x86. Δ shown only where BOTH "
          "images served (a real comparison). before==after where gate tag is N/A/warm is CORRECT.")


if __name__ == "__main__":
    main()
