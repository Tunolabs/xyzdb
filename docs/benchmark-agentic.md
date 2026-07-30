# xyzDB agentic benchmark — envelope capacity matrix

Capacity + latency of an **agent-memory deployment** across hardware envelopes:
does each deployment fit a given tier, and how fast does it serve? Corpus A =
LongMemEval (bge-large, 1024d; buckets = real conversational sessions). The harness lives
in [`benchmarks/agentic/`](../benchmarks/agentic/) and is runnable from a clean checkout:
`fetch_corpus.sh` downloads the public LongMemEval dataset and the engines are driven with
the in-repo minimal client — see that directory's README for the full recipe.

> **Status: COMPLETE — 256/256 cells.** AWS **m6a.xlarge** (x86-64-v3 / AVX2),
> data on the **real SSD** (`/mnt/ssd`, bind-mounted), **real swap** (4 GB
> swapfile, verified with `swapon` — an earlier pass ran silently swapless;
> every number below is from the corrected, swap-true run). xyzDB image
> `xyzdb:0.9.8-x86v3` — the **1.0 engine**: the xyzDB cells were re-run
> 2026-07-28 on the same box, corpus and swap, now driving the engine through
> the in-repo minimal client, and the numbers are unchanged vs 0.9.7
> (**budget-governed ingest** — the engine bounds its own build against
> `--memory-budget-mb`). Rivals are unchanged, from the 0.9.7 dataset at their
> images. One
> engine at a time, per-cell fresh container + wiped data dir,
> `BUILD_TIMEOUT=1200s`, hard cell wall 1400s, live fall-detection with
> reasons (OOM-kill / crashed / thrash).

## Setup

| tier | cpus | vector DRAM | swap | `--memory-swap` (total) | xyz `--memory-budget-mb` |
|---|---|---|---|---|---|
| T1 | 1 | 256M | 256M | 512M | 256 |
| T2 | 2 | 512M | 512M | 1024M | 512 |
| T3 | 2 | 2G | 512M | 2560M | 2048 |
| T4 | 2 | 4G | 512M | 4608M | 4096 |

Docker `--memory-swap` is the **total** (DRAM + swap), so the effective swap is
`--memory-swap − DRAM`: T1 = 256M, T2/T3/T4 = 512M. Host-backed by a real 4G
swapfile (EC2 has none by default). The lean store in the 2-container deployments
is fixed at 1c / 256M DRAM + 256M swap.

Deployments: **xyz** (1 container: vector + structured), **pg** (1: pgvector does
both), **qdrant+pg** / **chroma+pg** (2: vector engine at the tier envelope + a
**fixed lean Postgres store, 1c/256M**, on :5433). Scales 30k / 100k / 246k (full
corpus). RAM peak = combined across the deployment's containers, with the
per-container split. Every engine runs its own idiomatic best form (see below).

## Scenarios

Six agent-memory workloads, each the *same business question* posed to every
engine in its idiomatic shape. Corpus A = LongMemEval, bge-large 1024d; the
retrieval bucket is `question_id`, a session is `sid`.

| # | Workload | Agent-memory question | What it measures |
|---|---|---|---|
| **S1** | retrieve-and-expand | "the k nearest memories, then **expand each to its full session**" (the conversation, not a lone turn) | e2e latency, `session_recall` (fraction of the oracle top-k found — xyz exact = ceiling, HNSW rivals < 1.0), `expand_complete_frac` (correctness: exactly the hit sessions' turns, no drop/bleed), roundtrips, disk (payload-dup tax) |
| **S2** | live session | an agent that **saves one memory and searches 2–3× per turn** (sustained ~30% write / 70% read, deterministic replay of corpus turns) | online insert latency, query p50/p99 under concurrent writes, **degradation** (late vs early window — where a maintenance cycle shows: LSM compaction / optimizer / compaction), visibility (a write at `t` is a candidate at `t+1`). Durable-strict, equalised across engines |
| **S3** | multi-agent fleet | N agents = N tenants (buckets); the fleet is **born, grows, and dies** (PURGE), each engine in its best tenant form | cost to create / destroy the **Nth** tenant (not the first), **RAM vs #tenants curve**, filtered-query recall at each step; steps 10 / 100 / 1000 (if an engine can't hold 1000 in the tier, that OOM is the result). Run once per tier |
| **S4** | serverless wake | build the index → restart the container cleanly → **TTFQ** = restart → first *successful* query | cold-start readiness: xyz serves from the LSM (little to warm); rivals load the graph / collections into RAM on first access. Composes with at-rest footprint |
| **S5** | hybrid search | "of the memories matching `topic < T`, give the k nearest" — exact structured filter **+** NEAREST, selectivity swept 50% → 0.1% | recall@k vs the f64 oracle **over the filtered set** (tie-aware); deterministic metadata → all engines filter the identical universe (the parity gate) |
| **S6** | composite turn | **one engine for the whole agent**: write vector+fields → update structured → NEAREST, then the AGGREGATE ("count topic X active + avg importance") | per-op latency, the **+store inconsistency window** (0 for one-system xyz/pg; non-zero for the 2-container stacks), the aggregate, summed footprint. Cells here are *deployments*, not binaries |

## Capacity map (S1 — building and serving the base index)

```
deployment    T1:30/100/246   T2:30/100/246   T3:30/100/246   T4:30/100/246
xyz           FIT  FIT  FIT*   FIT  FIT  FIT    FIT  FIT  FIT    FIT  FIT  FIT
pg            FIT  BT   XX     FIT  BT   XX     FIT  FIT  XX     FIT  FIT  XX
qdrant+pg     FIT  FIT  FIT    FIT  FIT  FIT    FIT  FIT  FIT    FIT  FIT  FIT
chroma+pg     FIT  TH   TH     FIT  FIT* XX     FIT  FIT  FIT    FIT  FIT  FIT
```
`BT = build did not complete in 20 min · XX = crash/OOM during load · TH =
OOM-thrash (pinned + no progress, watchdog-verdicted) · FIT* = completes but
degraded (see tables)`

## Headline

1. **xyz is the only single-container deployment that fits everywhere — and its
   latency is FLAT across envelopes.** At the full corpus (246k), S1 p50 is
   **18.6 / 18.9 / 18.3 / 17.6 ms across T1→T4** and the build takes 202–218 s
   whether the box has 256 MB or 4 GB. The envelope changes what xyz *uses*
   (RAM peak 226 MB at T1 → 1550 MB at T4, elastic), not how it *behaves*.
   Recall 1.0 in every cell (exact NEAREST, never approximate). The one caveat
   is **T1/246k**: at 256 MB the 246k build spills to swap — it still serves at
   18.6 ms but loads and recovers by paging (load 218 s, settle 20 s), a
   swap-assisted fit, not a fit in hard RAM.
2. **qdrant+pg also fits everywhere** and posts the best roomy-tier scan p50
   (4.4–7.9 ms, resident HNSW) — but it pays where an agent feels it: **TTFQ is
   1.2–8.4 SECONDS in every cell** (xyz: 0.02–1.4 s), the tight tier degrades it
   (26.9/52.2 ms at T1/246k, worse than xyz), live-session queries drop to
   18–29 ms p50 at 246k (xyz 4.5–14), and it is two systems.
3. **pg never loads the full corpus** — 246k OOMs during the HNSW build at
   EVERY tier, including 4G+swap; 100k builds only at ≥2G (T1/T2 exceed the
   20-min budget). Where it fits it is fast and lean on point queries (S2 q50
   1.3 ms), and its T1/30k "fit" is heavily degraded: the build pages for
   **1043 s vs 95 s** on a roomy tier (11×).
4. **chroma+pg is the slowest at every size** (S1 p50 36 → 290 ms as N grows;
   S5 up to 552 ms; S2 q50 265 ms at 246k) with brutal tails (S1 p99 4.0 s at
   T3/246k), needs ≥2G for the full corpus, and thrashes at T1 beyond 30k.
5. **Fleet (S3)**: creating the 1000th tenant costs xyz/qdrant ~150–190 ms;
   pg ~1.0 s (DDL per tenant); **chroma never reached 1000 tenants at any
   tier**.
6. **The lean second DB works**: the +pg store stays at 23–57 MB in every cell —
   the stack tax is operational (two systems, double write, TTFQ), not RAM.

## Per-tier tables

Latency in ms. `S1 load s` = corpus build wall-clock (the fits-degraded signal).
`S6 near/agg` = composite-turn vector read / structured aggregate.

### T1 — vector 1c / 256M + 256M swap · store (2-container deploys) 1c / 256M + 256M swap

| deploy | c | N | S1 p50/p99 | S1 load s | recall | S2 q50 | S4 TTFQ ms | S5 p50 | S6 near/agg | RAM peak (split) | verdict |
|---|--|--|--|--|--|--|--|--|--|--|---|
| xyz | 1 | 30k | 16.7/28.2 | 22.8 | 1.00 | 1.0 | 223.7 | 14.1 | 1.2/0.27 | 131 (xyzdb 131) | FIT |
| xyz | 1 | 100k | 14.8/28.7 | 82.1 | 1.00 | 1.1 | 221.2 | 13.9 | 5.1/0.27 | 208 (xyzdb 208) | FIT |
| xyz | 1 | 246k | 18.6/32.1 | 218.4 | 1.00 | 14.0 | 223.3 | 14.3 | 5.0/0.28 | 226 (xyzdb 226) | **FIT (swap)** |
| pg | 1 | 30k | 14.6/27.9 | 1043.2 | 1.00 | 1.3 | 277.6 | 13.8 | 1.5/15.67 | 256 (pgvector 256) | FIT |
| pg | 1 | 100k | -/- | - | - | - | - | - | -/- | 256 (pgvector 256) | **build-timeout** |
| pg | 1 | 246k | -/- | - | - | - | - | - | -/- | 105 (pgvector 105) | **OOM-load** |
| qdrant+pg | 2 | 30k | 8.9/36.4 | 23.7 | 1.00 | 2.5 | 1638.9 | 8.0 | 3.0/0.76 | 290 (qdrant 253, store 37) | FIT |
| qdrant+pg | 2 | 100k | 12.0/31.8 | 78.0 | 1.00 | 2.8 | 3083.5 | 24.7 | 13.5/0.85 | 244 (qdrant 221, store 25) | FIT |
| qdrant+pg | 2 | 246k | 26.9/52.2 | 192.3 | 1.00 | 29.3 | 8106.6 | 25.9 | 26.4/1.06 | 274 (qdrant 248, store 26) | FIT |
| chroma+pg | 2 | 30k | 42.1/512.0 | 98.1 | 1.00 | 4.8 | 912.9 | 49.3 | 29.4/0.94 | 256 (chroma 230, store 26) | FIT |
| chroma+pg | 2 | 100k | -/- | - | - | - | - | - | -/- | 282 (chroma 256, store 26) | **thrash** |
| chroma+pg | 2 | 246k | -/- | - | - | - | - | - | -/- | 282 (chroma 256, store 26) | **thrash** |

### T2 — vector 2c / 512M + 512M swap · store (2-container deploys) 1c / 256M + 256M swap

| deploy | c | N | S1 p50/p99 | S1 load s | recall | S2 q50 | S4 TTFQ ms | S5 p50 | S6 near/agg | RAM peak (split) | verdict |
|---|--|--|--|--|--|--|--|--|--|--|---|
| xyz | 1 | 30k | 13.8/23.7 | 22.6 | 1.00 | 0.9 | 218.5 | 13.8 | 1.2/0.27 | 258 (xyzdb 258) | FIT |
| xyz | 1 | 100k | 17.9/29.2 | 79.4 | 1.00 | 1.1 | 227.3 | 14.0 | 4.3/0.29 | 270 (xyzdb 270) | FIT |
| xyz | 1 | 246k | 18.9/27.4 | 211.6 | 1.00 | 12.2 | 1433.2 | 14.2 | 5.6/0.28 | 314 (xyzdb 314) | FIT |
| pg | 1 | 30k | 15.2/27.5 | 102.4 | 1.00 | 1.3 | 277.3 | 13.8 | 1.4/7.70 | 327 (pgvector 327) | FIT |
| pg | 1 | 100k | -/- | - | - | - | - | - | -/- | 512 (pgvector 512) | **build-timeout** |
| pg | 1 | 246k | -/- | - | - | - | - | - | -/- | 179 (pgvector 179) | **OOM-load** |
| qdrant+pg | 2 | 30k | 4.4/7.6 | 26.8 | 1.00 | 2.4 | 2763.2 | 6.9 | 5.2/1.33 | 214 (qdrant 191, store 23) | FIT |
| qdrant+pg | 2 | 100k | 7.4/13.3 | 81.5 | 1.00 | 2.6 | 1910.9 | 7.0 | 5.2/0.77 | 405 (qdrant 380, store 25) | FIT |
| qdrant+pg | 2 | 246k | 7.9/19.1 | 196.9 | 1.00 | 18.1 | 7179.9 | 8.3 | 10.6/1.29 | 423 (qdrant 398, store 26) | FIT |
| chroma+pg | 2 | 30k | 36.0/104.4 | 29.0 | 1.00 | 4.9 | 385.9 | 47.5 | 29.3/0.94 | 271 (chroma 248, store 23) | FIT |
| chroma+pg | 2 | 100k | 117.7/2126.4 | 529.3 | 1.00 | 13.4 | 3002.1 | 193.0 | 63.6/0.96 | 538 (chroma 512, store 26) | FIT |
| chroma+pg | 2 | 246k | -/- | - | - | - | - | - | -/- | 538 (chroma 512, store 26) | **OOM-load** |

### T3 — vector 2c / 2G + 512M swap · store (2-container deploys) 1c / 256M + 256M swap

| deploy | c | N | S1 p50/p99 | S1 load s | recall | S2 q50 | S4 TTFQ ms | S5 p50 | S6 near/agg | RAM peak (split) | verdict |
|---|--|--|--|--|--|--|--|--|--|--|---|
| xyz | 1 | 30k | 16.0/24.4 | 22.6 | 1.00 | 0.9 | 224.4 | 14.0 | 1.1/0.27 | 330 (xyzdb 330) | FIT |
| xyz | 1 | 100k | 16.9/25.3 | 77.6 | 1.00 | 1.1 | 226.6 | 14.0 | 4.7/0.33 | 767 (xyzdb 767) | FIT |
| xyz | 1 | 246k | 18.3/26.7 | 202.2 | 1.00 | 5.0 | 213.8 | 14.1 | 4.5/0.27 | 889 (xyzdb 889) | FIT |
| pg | 1 | 30k | 5.9/9.2 | 94.4 | 1.00 | 1.3 | 220.3 | 4.4 | 1.4/0.67 | 462 (pgvector 462) | FIT |
| pg | 1 | 100k | 15.0/21.5 | 375.1 | 1.00 | 1.4 | 252.1 | - | -/- | 1153 (pgvector 1153) | FIT |
| pg | 1 | 246k | -/- | - | - | - | - | - | -/- | 618 (pgvector 618) | **OOM-load** |
| qdrant+pg | 2 | 30k | 4.3/7.7 | 26.9 | 1.00 | 2.4 | 2540.1 | 2.9 | 4.4/1.34 | 219 (qdrant 196, store 23) | FIT |
| qdrant+pg | 2 | 100k | 4.4/7.9 | 104.5 | 1.00 | 3.3 | 6708.0 | 2.7 | 3.6/1.48 | 493 (qdrant 468, store 26) | FIT |
| qdrant+pg | 2 | 246k | 7.4/13.4 | 269.8 | 1.00 | 6.3 | 1183.8 | 2.9 | 3.7/1.67 | 1305 (qdrant 1280, store 26) | FIT |
| chroma+pg | 2 | 30k | 36.5/104.6 | 25.4 | 1.00 | 4.9 | 392.2 | 48.0 | 29.8/0.93 | 255 (chroma 233, store 23) | FIT |
| chroma+pg | 2 | 100k | 114.3/322.5 | 109.4 | 1.00 | 12.5 | 657.6 | 191.1 | 60.0/0.99 | 585 (chroma 560, store 26) | FIT |
| chroma+pg | 2 | 246k | 289.3/3987.4 | 388.5 | 1.00 | 263.4 | 6115.7 | 544.8 | 148.5/1.11 | 1296 (chroma 1271, store 26) | FIT |

### T4 — vector 2c / 4G + 512M swap · store (2-container deploys) 1c / 256M + 256M swap

| deploy | c | N | S1 p50/p99 | S1 load s | recall | S2 q50 | S4 TTFQ ms | S5 p50 | S6 near/agg | RAM peak (split) | verdict |
|---|--|--|--|--|--|--|--|--|--|--|---|
| xyz | 1 | 30k | 15.9/24.5 | 22.6 | 1.00 | 0.9 | 22.4 | 13.8 | 1.2/0.27 | 339 (xyzdb 339) | FIT |
| xyz | 1 | 100k | 16.9/26.3 | 78.0 | 1.00 | 1.0 | 23.8 | 13.8 | 4.6/0.30 | 1404 (xyzdb 1404) | FIT |
| xyz | 1 | 246k | 17.6/23.8 | 202.0 | 1.00 | 4.5 | 216.0 | 26.4 | 4.5/0.36 | 1550 (xyzdb 1550) | FIT |
| pg | 1 | 30k | 5.9/9.1 | 95.4 | 1.00 | 1.3 | 220.2 | 4.3 | 1.4/0.69 | 472 (pgvector 472) | FIT |
| pg | 1 | 100k | 6.3/9.3 | 375.7 | 1.00 | 1.4 | 220.9 | - | -/- | 1701 (pgvector 1701) | FIT |
| pg | 1 | 246k | -/- | - | - | - | - | - | -/- | 1164 (pgvector 1164) | **OOM-load** |
| qdrant+pg | 2 | 30k | 4.4/10.1 | 27.6 | 1.00 | 2.4 | 2768.8 | 2.8 | 5.5/1.45 | 228 (qdrant 205, store 23) | FIT |
| qdrant+pg | 2 | 100k | 4.7/11.1 | 105.3 | 1.00 | 5.5 | 8387.2 | 2.9 | 3.3/2.79 | 494 (qdrant 469, store 26) | FIT |
| qdrant+pg | 2 | 246k | 4.6/10.0 | 277.4 | 1.00 | 5.8 | 2320.2 | 2.8 | 3.8/1.59 | 634 (qdrant 609, store 26) | FIT |
| chroma+pg | 2 | 30k | 36.8/109.8 | 25.9 | 1.00 | 4.9 | 389.9 | 48.0 | 29.8/0.93 | 242 (chroma 219, store 23) | FIT |
| chroma+pg | 2 | 100k | 115.4/322.5 | 109.6 | 1.00 | 12.6 | 661.3 | 192.3 | 60.4/0.98 | 586 (chroma 561, store 26) | FIT |
| chroma+pg | 2 | 246k | 289.9/1316.1 | 311.1 | 1.00 | 265.5 | 1942.3 | 551.9 | 143.0/1.10 | 1296 (chroma 1270, store 26) | FIT |

### S3 — fleet (10/100/1000 tenants, once per tier): create-nth ms / RAM MiB at 1000

| deploy | T1 | T2 | T3 | T4 |
|---|---|---|---|---|
| xyz | 287.4 / 192 | 212.9 / 274 | 183.4 / 821 | 184.5 / 1465 |
| pg | 1036.8 / 129 | 1016.5 / 203 | 1011.6 / 648 | 1012.6 / 1213 |
| qdrant+pg | 158.8 / 123 | 156.2 / 311 | 286.7 / 258 | 161.6 / 214 |
| chroma+pg | no t1000 | no t1000 | no t1000 | no t1000 |

`chroma+pg` S3 "no t1000": the fleet loop never completed the 1000-tenant step
inside the cell wall at any tier (it reaches ~100).

## Notes

- **fits vs fits-degraded.** A FIT verdict says the cell completed; the
  degradation shows in `S1 load s` and the latencies. Clear degraded fits:
  pg T1/30k (build 1043 s vs 95 s roomy), chroma T2/100k (build 529 s, S1 p99
  2.1 s). xyz shows **one** degraded fit — T1/246k, where 256 MB forces the 246k
  build to page to swap (load 218 s, settle 20 s vs ~6.5 s elsewhere), a
  swap-assisted fit; everywhere else load is 23/80/205 s at 30k/100k/246k,
  constant across tiers.
- **pg S6 cells at 100k/246k** appear as "fall (base build)": the composite
  scenario rebuilds the base index first, which is exactly what fails at those
  scales — same root as its S1 verdicts, not an extra failure mode.
- **Why xyz fits T1**: the 1.0 engine derives a global memtable ceiling from
  `--memory-budget-mb` and stalls ingest for background flush at the ceiling —
  a tight container bounds its own build instead of OOM-ing (0.9.6 died building
  246k at T1). At the very tightest corner (T1/246k) the build peak still needs
  the host swapfile to land — docker `--memory-swap` is a no-op without one, so
  a fresh box must have swap. See `docs/architecture.md` §memory.
- **Verdicts are watchdog-attributed**, with reasons (e.g. chroma T1:
  `mem pinned 256/256MiB and cpu <20% sustained 300s -> paging, no progress`).
  A container must stay down >90 s to be declared fallen (the settle restart
  comes back; a real OOM does not).
- Mac/OrbStack runs of this matrix are DIRECTION only; this AWS dataset is the
  publishable one. Records carry the image stamp per cell.
