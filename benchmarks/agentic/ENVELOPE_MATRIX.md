# Envelope capacity matrix — agentic bench

Comparative **capacity** (not just latency): for each hardware envelope, does an
engine's **working peak** fit? Runner: [`run_envelope_matrix.sh`](run_envelope_matrix.sh).
Image stamped in every record (premise-20). Mac/OrbStack = **direction**, not publishable.

## Why peak, not mean
An engine OOMs (or pages) when its **peak** working set exceeds the envelope, not its
average. So every verdict is judged on `query_ram_peak_mb` — the `PeakSampler` in
[`measure_x.py`](measure_x.py) that samples `docker stats` every 0.4 s — **never** the
mean. Two RAM numbers are tracked and must never be conflated (mixing them caused two
false alarms already):

| number | source | meaning |
|---|---|---|
| `query_ram_peak_mb` | `PeakSampler` (measure_x.py) | **TRANSIENT** peak during the query loop |
| `vmrss` / `ram_budget` | `STATS` (engine, `stats.rs:260`) | **RESIDENT** at the moment of the call |

## Tiers
docker: `--cpus` + `--memory` (= DRAM) + `--memory-swap` (= DRAM + SWAP; `== --memory` ⇒ no swap).

| tier | cpus | DRAM (`--memory`) | swap | `--memory-swap` | xyz `--memory-budget-mb` |
|---|---|---|---|---|---|
| T1 | 1 | 256M | 256M | 512M | 256 |
| T2 | 2 | 512M | 512M | 1024M | 512 |
| T3 | 2 | 2G | 512M | 2560M | 2048 |
| T4 | 2 | 4G | 512M | 4608M | 4096 |

- **All four tiers carry swap** → they reveal degrade-vs-thrash under paging.
- **T4's swap is a small 512M cushion** → anything overflowing 4G by more than
  512M still OOM-kills quickly (a near-clean binary OOM with a brief paging window),
  matching a realistic 4G box that carries a little swap.

## Scales
Corpus A (LongMemEval, bge-large 1024d): **30k / 100k / 246k** turns. S1's per-query
bucket grows with N (buckets fill toward 246k); S3 is corpus-independent (200 vecs/tenant).

## "Proportional" config — DEFINED EXPLICITLY (declared per cell)
"Proportional" does **not** mean "same MB of cache for everyone" — engines use RAM
differently. xyz's block cache is **elastic and compressible** (shrinking it loses no
correctness); qdrant/chroma's HNSW graph is **resident and non-compressible**. So each
engine runs in its **best config for the envelope**, declared in every record's
`envelope` string (silent config between cells = the Q8 sin):

| engine | per-tier config | rationale |
|---|---|---|
| **xyz** | `--memory-budget-mb` = tier DRAM (T1→256 … T4→4096) | 0.9.6 governs memory via the budget (`--cache-size` deprecated); the engine self-limits to DRAM and returns memory under pressure. Passed EXPLICITLY because the engine's cgroup auto-detect fails on the AWS box (else it assumes 1024MB → OOM at tight tiers) |
| **pgvector** | `shared_buffers` ~25% DRAM (T1→64MB … T4→1GB) + `maintenance_work_mem` scaled ([`adapters.py:21`](adapters.py) `BENCH_PG_MWM`), rest → OS cache | standard PG tuning; honest build budget per envelope, not a fixed 2GB that OOMs trivially |
| **qdrant / chroma** | default memory config; container `--memory` bounds them | if the resident HNSW does not fit the DRAM ⇒ **OOM is a RESULT** (its architecture vs the envelope, not a handicap) |

## OOM criterion — uniform for all 4
A cell **fits** only if the working **peak** fits the DRAM. Per cell we record one of:

- **`fits`** — peak ≤ DRAM.
- **`fits-degraded`** — peak > DRAM but survives on swap (degrades but lives); the
  latency penalty of paging is measured.
- **`OOM`** — thrashes/dies (`OOMKilled`, container-did-not-start, or unviable build).

The peak is: **xyz** = the transient NEAREST materialization (~560 MiB @246k, measured);
**rivals** = resident HNSW graph + query. `OOMKilled` is read via `docker inspect` before
and after each cell.

## Hypotheses under test (the potential finding)
1. **Transient pages better than resident under swap pressure.** xyz's peak is a
   materialization that decays after the query; an HNSW graph is re-touched by every
   query → sustained thrash. T1–T3 (with swap) should show xyz *degrade-and-recover*
   while HNSW engines *degrade-and-stall*. T4 (4G + 512M cushion) is the
   near-clean binary OOM: a brief page-window, then the kill.
2. **S1-vs-S3 isolates bucket-fullness (closes the open question).** S1's peak scaled
   with N because LME buckets fill toward 246k. Run S1 (bucket grows with N) **and** S3
   (bounded 200 vecs/tenant). If S3's peak is flat while S1's scales ⇒ the 246k ceiling
   is **LME corpus density, not an xyz limit**. That contrast is worth more than any
   single cell.

## Measured inputs (already established, feed the predictions)
- **Cold/idle floor**: ~9 MiB, **N-independent** (STATS vmrss, 30k=100k=246k).
- **Steady serving** (settled, warm): ~189 MiB @246k (128 cache-capped + ~61 non-cache,
  sub-linear in N).
- **Transient query peak (P1 — scales with N)**: `query_ram_peak_mb` @cache-128 =
  139 (30k) / 144 (100k) / **563 (246k)**. Flat to 100k, ~4× at full corpus
  (bucket-fullness). NOT flat → real scale ceiling.
- **P2 — CORRECTED (the peak is elastic to container pressure, NOT rigid).** In an 8G
  container the @246k peak read 563/614/586 at cache 128/256/512 — which *looked* like a
  rigid materialization floor. It was not: in a **256M** container (tier T1, cache 103)
  the same @246k S1 peak **collapses to 222 MiB** and fits DRAM with no swap
  (recall 1.0, p50 7.8ms). The 563 was the **allocator holding freed memory** because
  8G gave it no reason to return it. Under real pressure the working set is ~222 MiB.
- **PRODUCT PROPERTY — xyz returns memory under pressure (elastic RAM).** Its footprint
  adapts to the envelope you give it and serves correctly within it. This is the third
  independent confirmation that "RAM measured in abundance ≠ xyz's real RAM": (1) the
  2.6 GB was block cache, (2) the 438 MiB "floor" was a transient peak, (3) the 563 MiB
  peak was allocator slack. Each time, "how much xyz uses" turned out to be "how much we
  let it use", not "how much it needs". This is the **opposite of HNSW**, whose resident
  graph does not compress under pressure — it fits or dies.
- **Envelope implication (revised)**: xyz likely **fits DRAM at every tier at every
  scale** (elastic peak collapses under pressure); the density moat is not "uses little"
  but "uses what you give it and serves correct, while a resident HNSW either fits or
  OOMs". Whether xyz ever needs swap at these 4 tiers is what the matrix confirms —
  with a 222 MiB peak it fits T1's 256M DRAM, so swap may never be exercised.

## Full deployment matrix — `run_envelope_full.sh`
The single-engine runner above measures S1+S3 per binary, memory only. The **full**
runner measures the honest **deployment** (what a real stack carries) across ALL
scenarios, with **combined CPU + memory** and live fall-detection.

### Deployments (4)
| deployment | containers | who does what |
|---|---|---|
| `xyz` | 1 (`bench-xyzdb`) | one engine: vector + structured |
| `pg` | 1 (`bench-pgvector`) | one system: pgvector does vector + structured |
| `qdrant+pg` | 2 (`bench-qdrant` + `bench-store` :5433) | qdrant vector + Postgres store |
| `chroma+pg` | 2 (`bench-chroma` + `bench-store` :5433) | chroma vector + Postgres store |

In a two-system deployment the **vector engine gets the tier envelope** and the **+pg
store gets a FIXED lean envelope — 1 core / 256M (+256 swap), `shared_buffers` 64MB** —
"does the stack fit with a minimal second DB". Combined budget = tier DRAM + 256M; the
watchdog tracks each container against its OWN DRAM cap. S1/S2/S4/S5 exercise the vector
side (the store is resident; its structured metadata is KB-scale so its RAM ≈ base +
shared_buffers); **S6 loads AND exercises the store** (double write, inconsistency
window, SQL aggregate).

**No skip-cascade — every cell is a real test.** With the watchdog making falls cheap
(OOM-kill instant, thrash ~5 min), running 100k/246k even after 30k fell yields a real
per-cell verdict and the **fit-gradient** (fits-degraded → thrash → OOM-kill = where the
wall is), not an assumed skip. Each record also carries `boot_s` (launch), `load_s`
(build), `settle_ms` (recovery/restart), `session_recall` (retrieval), `n_engines` /
`one_system`, and the combined + per-container CPU/mem.

### Scenarios — all 6
S1 (retrieve+expand), S2 (live session), S4 (TTFQ), S5 (hybrid), S6 (composite turn)
per scale; S3 (fleet 10/100/1000 tenants) once per tier (corpus-independent).

### Combined resources — `cell_watchdog.py`
One process polls every container of the deployment and reports the **combined**
footprint (sum of all containers), peak + avg, for BOTH memory and CPU, plus a
per-container breakdown (so qdrant's own footprint inside qdrant+pg is recoverable
without a solo run). Merged into each scenario record as `combined_mem_peak_mb` /
`combined_cpu_peak_pct` / `combined_*_avg_*` / `per_container`.

### Fall detection — how a cell is declared dead, and when
The same watchdog decides, in bounded time, whether a cell fell and **why** — three
modes (see the fits/thrash/OOM criterion above):

| mode | signal | time-to-verdict | verdict |
|---|---|---|---|
| **OOM-kill** | container `State.OOMKilled==true` (kernel killed it: working set > DRAM+swap) | instant (~1 poll) | `OOM-kill` |
| **crashed** | container exited non-zero, not OOM | instant | `crashed` |
| **thrash** | mem pinned ≥92% DRAM **and** cpu <20% (paging = iowait, not compute) sustained 300 s | ~5 min | `OOM-thrash` |
| **wall** | hard per-scale backstop | 30k→10 min / 100k→20 min / 246k→30 min | `OOM-thrash` |

Thrash is the "swap can't save it" case (alive but unusable); the signature fires
conservatively (a false kill = a wrong verdict, worse than waiting) — pinned≥92% AND
cpu<20% held a full 300 s. On a fall the watchdog writes the verdict+reason and
`docker kill`s the containers (aborting the measure), so the reason lands without the
full wall. **Boot** is ~0-2 s even at T1 (an empty engine always fits) — OOM happens
at LOAD, not boot. **Resumable**: a cell whose out-file already holds a completed main
record is skipped. Thresholds via env: `WD_PIN_FRAC` (0.92), `WD_CPU_LOW` (20),
`WD_STALL` (300), `CELL_WALL` per-scale.

### Usage — full runner
```bash
# Mac (arm64, direction) — docker named volumes
bash run_envelope_full.sh   # 4 deployments × 4 tiers × 3 scales × 6 scenarios
DEPLOYMENTS_RUN="xyz qdrant+pg" TIERS="T1 T4" SCALES="246738" SCENARIOS="s1 s6" bash run_envelope_full.sh
DRY=1 DRY_DEP=qdrant+pg DRY_TIER=T3 DRY_N=30000 DRY_SC=s1 bash run_envelope_full.sh  # one cell
```
Records land in `results/envelope_full/<tier>_<sc>_n<N>_<depslug>.jsonl`.

**AWS (x86-v3, publishable)** — bind-mounts the real SSD via `STORAGE_ROOT` so the data
lands on `/mnt/ssd` (93 GB), not the ~14 GB root disk (pg 246k ≈ 28 GB/cell). Use the
launcher `run_envelope_aws.sh` (preflight + AWS env), or set the vars by hand:
```bash
cd ~/xyzdb/benchmarks/agentic && ./run_envelope_aws.sh          # STORAGE_ROOT=/mnt/ssd, x86-v3 image
STORAGE_ROOT=/mnt/ssd XYZ_ARCH=x86-v3 XYZDB_IMG=xyzdb:0.9.6-x86v3 bash run_envelope_full.sh
```
`STORAGE_ROOT` set ⇒ each cell bind-mounts `/mnt/ssd/bench_<engine>` (wiped between cells —
xyzDB has no drop_lobe) and the measure reads footprint via host `du`. Prereqs (build the
x86-v3 server image, pull rival images, rsync the gitignored corpus, `.venv`) are documented
in `run_envelope_aws.sh`.

## Usage — single-engine runner
```bash
# Mac (arm64, direction) — full matrix
bash run_envelope_matrix.sh
# scope it
TIERS="T1 T2" SCALES="246738" SCENARIOS="s1" ENGINES="xyzdb qdrant" bash run_envelope_matrix.sh
# dry-run: one cell (cost probe)
DRY=1 DRY_TIER=T1 DRY_N=246738 DRY_ENGINE=xyzdb DRY_SC=s1 bash run_envelope_matrix.sh
```
Records land in `results/envelope_matrix/<tier>_<sc>_n<N>_<engine>.jsonl`.

### xyz image per arch (AVX2)
The Dockerfile applies `target-cpu=x86-64-v3` (AVX2) **only on amd64 builds**; Mac
(arm64) is the arm baseline.
- **Mac (arm64)**: `XYZDB_IMG=xyzdb:0.9.6-fixA` (default).
- **AWS (amd64, publishable)**: build x86-v3 **on the x86 box**, then point at it:
  ```bash
  docker build --build-arg XYZ_IMAGE_VARIANT=x86-v3 -t xyzdb:0.9.6-fixA-x86v3 .
  XYZDB_IMG=xyzdb:0.9.6-fixA-x86v3 XYZ_ARCH=x86-v3 bash run_envelope_matrix.sh
  ```
  `XYZ_ARCH` only labels the arch in each record; `XYZDB_IMG` selects the actual image.

## Out of scope
Fine temporal decay of the peak over long idle; envelopes other than these 4; AWS runs
(the 4 tiers, Mac, direction only). Do not over-characterize — it does not change the
envelope decision.
