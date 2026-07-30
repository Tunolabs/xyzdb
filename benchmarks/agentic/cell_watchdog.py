#!/usr/bin/env python3
"""Live watchdog + combined resource sampler for one deployment cell.

Polls every container of a deployment and BOTH (a) accumulates the combined
CPU + memory footprint (peak/avg + per-container breakdown, same JSON shape the
runner merges) AND (b) decides, in bounded time, whether the cell "fell" and why —
so the reason lands in the record without waiting the full wall.

Three fall modes (see ENVELOPE_MATRIX.md — fits / thrash / OOM):
  * OOM-kill  container State.Running==false with OOMKilled==true — the kernel
              killed it (working set > DRAM+swap). Detected instantly.
  * crashed   container died non-zero, not OOM (a real bug/error). Instant.
  * OOM-thrash container ALIVE but drowning in swap: memory pinned at the DRAM cap
              AND cpu collapsed (paging = iowait, not compute) sustained for
              STALL_WINDOW. This is the "swap can't save it" case — alive but
              unusable. Detected in ~STALL_WINDOW, not the full wall.

The wall is the hard backstop (a thrash the signature missed still gets caught).
On any fall the watchdog writes the verdict file AND `docker kill`s the containers
so the measure process aborts fast; the runner then records the verdict+reason.
Bias is conservative (a false kill = a wrong verdict/data gap, worse than waiting
the wall): thrash fires only on pinned≥92% AND cpu<20% held a full STALL_WINDOW.

Each container carries its OWN DRAM cap (the vector engine gets the tier DRAM, the
+pg store gets its fixed 256M) so the thrash "pinned" check is per-container correct.

Usage:
  cell_watchdog.py <stop_file> <out_json> <verdict_file> <stall_window_s> <wall_s> \
                   <container1:dram_mib> [<container2:dram_mib> ...]
Env: WD_PIN_FRAC (default 0.92), WD_CPU_LOW (default 20), WD_POLL_S (default 3).
"""
import sys
import os
import json
import time
import subprocess

PIN_FRAC = float(os.environ.get("WD_PIN_FRAC", "0.92"))
CPU_LOW = float(os.environ.get("WD_CPU_LOW", "20"))
POLL_S = float(os.environ.get("WD_POLL_S", "3"))
# A not-running container must STAY dead this long before it is declared fallen.
# The measures' settle step intentionally `docker restart`s the engine (up to 60s
# stop grace under memory pressure) — without this grace the watchdog mistook that
# restart window for a crash and killed the fresh instance (false settle_failed).
# A real OOM-kill/crash stays dead, so its verdict just arrives this much later.
DEATH_GRACE_S = float(os.environ.get("WD_DEATH_GRACE", "90"))


def _mem_to_mib(tok: str) -> float:
    tok = tok.strip()
    for unit, mult in (("GiB", 1024.0), ("MiB", 1.0), ("KiB", 1.0 / 1024.0),
                       ("GB", 953.674), ("MB", 0.953674), ("kB", 0.000953674),
                       ("B", 1.0 / (1024.0 * 1024.0))):
        if tok.endswith(unit):
            try:
                return float(tok[:-len(unit)]) * mult
            except ValueError:
                return 0.0
    return 0.0


def _inspect(container: str):
    """Return (running: bool, oomkilled: bool, exit_code: int) or None."""
    try:
        r = subprocess.run(
            ["docker", "inspect", "-f",
             "{{.State.Running}}|{{.State.OOMKilled}}|{{.State.ExitCode}}", container],
            capture_output=True, text=True, timeout=15)
    except Exception:
        return None
    out = r.stdout.strip()
    if r.returncode != 0 or "|" not in out:
        return None
    run, oom, code = out.split("|")
    try:
        code = int(code)
    except ValueError:
        code = -1
    return run.strip() == "true", oom.strip() == "true", code


def _stats(container: str):
    """Return (mem_mib, cpu_pct) or None."""
    try:
        r = subprocess.run(
            ["docker", "stats", "--no-stream", "--format", "{{.MemUsage}}|{{.CPUPerc}}", container],
            capture_output=True, text=True, timeout=15)
    except Exception:
        return None
    out = r.stdout.strip()
    if r.returncode != 0 or "|" not in out:
        return None
    mu, cp = out.split("|", 1)
    mem = _mem_to_mib(mu.split("/")[0])
    try:
        cpu = float(cp.strip().rstrip("%"))
    except ValueError:
        cpu = 0.0
    return mem, cpu


def main() -> None:
    stop, outj, verdictf = sys.argv[1], sys.argv[2], sys.argv[3]
    stall_window = float(sys.argv[4])
    wall = float(sys.argv[5])
    # remaining args are "container:dram_mib" pairs — each container's own DRAM cap
    conts = []
    dram_of = {}
    for tok in sys.argv[6:]:
        name, _, d = tok.rpartition(":")
        conts.append(name)
        dram_of[name] = float(d)

    per = {c: {"mem_peak": 0.0, "cpu_peak": 0.0, "mem_sum": 0.0, "cpu_sum": 0.0, "n": 0} for c in conts}
    comb = {"mem_peak": 0.0, "cpu_peak": 0.0, "mem_sum": 0.0, "cpu_sum": 0.0, "n": 0}
    stall_since = {c: None for c in conts}   # monotonic time each container entered the thrash signature
    dead_since = {c: None for c in conts}    # monotonic time each container was first seen not-running
    t0 = time.monotonic()
    verdict = None

    while not os.path.exists(stop):
        now = time.monotonic()
        # (a) hard death — container gone AND stayed gone past the grace window
        # (a settle restart comes back; an OOM-kill/crash does not).
        for c in conts:
            ins = _inspect(c)
            if ins is None:
                continue
            running, oom, code = ins
            if running:
                dead_since[c] = None
                continue
            if dead_since[c] is None:
                dead_since[c] = now
                continue
            if now - dead_since[c] < DEATH_GRACE_S:
                continue
            if oom:
                verdict = {"verdict": "OOM-kill", "container": c,
                           "reason": f"{c}: kernel OOM-killed (working set > DRAM+swap)"}
            else:
                verdict = {"verdict": "crashed", "container": c,
                           "reason": (f"{c}: exited (code {code}), not OOM, and stayed down "
                                      f">{DEATH_GRACE_S:.0f}s (settle restarts come back)")}
            break
        if verdict:
            break
        # (b) sample + thrash signature
        cm = cc = 0.0
        for c in conts:
            s = _stats(c)
            if s is None:
                continue
            m, cpu = s
            cm += m
            cc += cpu
            a = per[c]
            a["mem_peak"] = max(a["mem_peak"], m)
            a["cpu_peak"] = max(a["cpu_peak"], cpu)
            a["mem_sum"] += m
            a["cpu_sum"] += cpu
            a["n"] += 1
            dram_c = dram_of.get(c, 0.0)
            pinned = dram_c > 0 and m >= PIN_FRAC * dram_c
            collapsed = cpu < CPU_LOW
            if pinned and collapsed:
                if stall_since[c] is None:
                    stall_since[c] = now
                elif now - stall_since[c] >= stall_window:
                    verdict = {"verdict": "OOM-thrash", "container": c,
                               "reason": (f"{c}: mem pinned {m:.0f}/{dram_c:.0f}MiB (>={PIN_FRAC:.0%} DRAM) "
                                          f"and cpu {cpu:.0f}% (<{CPU_LOW:.0f}%) sustained {stall_window:.0f}s "
                                          f"-> paging, no progress (swap cannot save it)")}
                    break
            else:
                stall_since[c] = None
        if cm or cc:
            comb["mem_peak"] = max(comb["mem_peak"], cm)
            comb["cpu_peak"] = max(comb["cpu_peak"], cc)
            comb["mem_sum"] += cm
            comb["cpu_sum"] += cc
            comb["n"] += 1
        if verdict:
            break
        # (c) hard wall backstop
        if now - t0 >= wall:
            verdict = {"verdict": "OOM-thrash", "container": ",".join(conts),
                       "reason": f"exceeded wall {wall:.0f}s: sustained thrash, did not complete"}
            break
        time.sleep(POLL_S)

    def avg(sm, n):
        return round(sm / n, 1) if n else 0.0

    res = {
        "combined_mem_peak_mb": round(comb["mem_peak"], 1),
        "combined_mem_avg_mb": avg(comb["mem_sum"], comb["n"]),
        "combined_cpu_peak_pct": round(comb["cpu_peak"], 1),
        "combined_cpu_avg_pct": avg(comb["cpu_sum"], comb["n"]),
        "n_samples": comb["n"],
        "containers": conts,
        "per_container": {c: {
            "mem_peak_mb": round(a["mem_peak"], 1), "mem_avg_mb": avg(a["mem_sum"], a["n"]),
            "cpu_peak_pct": round(a["cpu_peak"], 1), "cpu_avg_pct": avg(a["cpu_sum"], a["n"]),
        } for c, a in per.items()},
    }
    with open(outj, "w") as f:
        json.dump(res, f)

    if verdict:
        with open(verdictf, "w") as f:
            json.dump(verdict, f)
        for c in conts:
            subprocess.run(["docker", "kill", c], capture_output=True)


if __name__ == "__main__":
    main()
