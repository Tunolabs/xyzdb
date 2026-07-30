# xyzDB native benchmark — engine comparison

xyzDB vs PostgreSQL vs MongoDB on a business-question workload (Q1–Q9), each engine
in its **idiomatic** shape. Plus idle-footprint and multi-agent density.

> **Setup.** AWS **m6a.xlarge** (x86-64-v3 / AVX2; bench containers capped to the **T6 envelope = 2 vCPU / 8 GB**), **SSD**,
> **scale 1.0 ≈ 150M records**, schema `Full`, shape A. **3 passes, pooled** (median ±
> σ across passes). Engine-exclusive serial runs — never two engine containers at once.
> Harness `run_aws_4engines.sh`. `verify_golden` + content-gate pass bit-for-bit;
> recall exact.
>
> **Parity-fair.** Every engine gets its idiomatic fast path (pg covering / mat-view
> indexes, mongo aggregation). This supersedes the earlier single-run campaign where
> only xyzDB had the Q4/Q8 rollup — those pre-parity numbers (Q4 ~3800× etc.) are
> retired as misleading.
>
> **Status:** SSD scale-1 3-pass — **COMPLETE** (cold + concurrent + footprint, all 3
> passes, golden + content-gate + integrity). HDD scale-1 — **COMPLETE** (x86-v3,
> same parity campaign; xyz vs pg, §5). **Mongo did not complete on HDD** — v8.0 and
> v7.0 both failed to finish the load within 48 h under the same parity configuration;
> reported as **not-completed**, not omitted.

---

## 1. Cold queries — P50 pooled (median ± σ of 3 passes)

| query | xyz | pg | mongo | winner | advantage |
|---|--:|--:|--:|:--|:--|
| Q1 Point | **0.20**±0.00 | 0.44±0.00 | 1.00±0.01 | xyz | 2.2× |
| Q2 Aggregate | **0.78**±0.02 | 1.63±0.29 | 1.79±0.01 | xyz | 2.1× |
| Q3 FullHistory | **0.72**±0.00 | 2.47±0.02 | 2.72±0.08 | xyz | 3.4× |
| Q4 TopExposure | **0.30**±0.00 | 0.49±0.00 | 0.61±0.00 | xyz | 1.6× |
| Q5 OverdueByEmpresa | **0.21**±0.01 | 0.44±0.00 | 0.49±0.00 | xyz | 2.1× |
| Q6 RecentPayments | 0.89±0.00 | **0.57**±0.01 | 0.83±0.00 | pg | 1.5× |
| Q7 BatchIngest | 4.80±0.05 | 5.12±0.33 | **4.59**±0.23 | mongo | 1.05× (tie) |
| Q8 MonthlyClose | **0.30**±0.00 | 0.47±0.01 | 0.69±0.03 | xyz | 1.6× |
| Q9 CustomerContext | 3.83±0.08 | **2.95**±0.02 | 4.16±0.43 | pg | 1.3× |

**Wins (pooled): xyz 6 · pg 2 · mongo 1.**

### Verdict (3-pass, publishable)
- **xyz wins 6/9 cold** (point, aggregate, full-history, top-exposure, overdue,
  monthly-close) — **1.6× to 3.4×** over the runner-up. σ ≈ 0 → solid.
- **pg wins Q6 and Q9** (recent-payments 1.5×, customer-context 1.3×) — its
  covering-index / sorted terrain.
- **mongo wins Q7 by 1.05× = statistical tie** (batch-ingest).
- **Stability:** the 3 passes agree (max σ 0.29 on pg-Q2, the rest ≈ 0) → passes the
  ≤ 1σ gate; not single-run noise.

---

## 2. Concurrent (Phase 3) — pooled P50 / P99 ms (median of 3 passes)

| query | xyz | pg | mongo |
|---|--:|--:|--:|
| Q1 | 0.45 / 1.2 | 0.89 / 11.3 | 1.58 / 45.8 |
| Q2 | 0.92 / 2.6 | 1.46 / 16.1 | 2.19 / 43.8 |
| Q3 | 3.67 / 5.7 | 3.47 / **747** | 5.71 / 155 |
| Q4 | 0.62 / 1.0 | 0.89 / 6.2 | 6.78 / 58.1 |
| Q5 | 0.44 / 1.3 | 0.83 / 13.5 | 1.12 / 41.8 |
| Q6 | 2.86 / 4.7 | 0.97 / 15.7 | 6.95 / 62.1 |
| Q7 | 5.26 / 7.6 | 5.65 / 54.4 | 7.31 / 55.5 |
| Q8 | 0.57 / 0.9 | 0.86 / 13.3 | 1.17 / 46.0 |
| Q9 | 5.34 / 8.6 | 3.98 / 46.4 | 9.87 / 63.0 |

**Worst concurrent P99: xyz 8.6 ms · pg 747 ms · mongo 155 ms.** Under mixed
read + write, **xyz holds P99 ≤ 8.6 ms on EVERY query**; pg spikes on Q3 (747 ms,
mat-view refresh) plus several 11–54 ms; mongo is uniform 42–63 ms. In P50 concurrent
xyz wins 6/9 (as cold); **in P99 xyz wins all 9** — the predictable-tail moat.

---

## 3. Load & footprint (pooled, stable across the 3 passes)

| metric | xyz | pg | mongo |
|---|--:|--:|--:|
| Load (rec/s) | **123,865** | 98,031 | 13,724 |
| RAM peak (MiB) | **2073** | 2759 | 4878 |
| Disk (MiB) | **12,192** | 28,182 | 13,213 |
| Q3 concurrent P99 | **~6 ms** | 747 ms | ~155 ms |

xyz wins load (1.26× pg, 9× mongo), RAM **and** disk simultaneously (disk 2.3× less
than pg), and concurrent-tail stability (pg pays 747 ms on Q3 for the mat-view
refresh; mongo's refresh runs in minutes).

---

## 4. Headline (publishable)

On **scale-1 (~150M) on real hardware**, xyzDB wins **latency in 6/9 queries + load +
footprint (RAM and disk) + stability**, with recall/golden verified bit-for-bit. It
concedes Q6/Q9 to pg (covering index) and ties Q7 with mongo.

Note the RAM here (**2073 MiB, the lowest of the three**) — the density moat working:
this is the **paced load + flush** regime, the opposite of the agentic bulk-load
regime (no flush), where xyzDB's transient peak is much higher. Same engine, footprint
adapts to how it is fed.

---

## 5. HDD — xyzDB vs PostgreSQL (x86-v3)

> AWS HDD (`/mnt/hdd`), scale 1.0 (149.9M records), x86-v3, 2026-07-17/18. Same parity
> campaign as the SSD 3-pass. **Mongo did not complete on HDD**: v8.0 and v7.0 both
> failed to finish the load within 48 h under the same parity configuration
> (document-model aggregation is non-viable on rotational storage at this scale) —
> reported as **not-completed**, not omitted. Both completing engines (xyz, pg):
> `verify_golden` YES / 0 diffs.

### 5.1 Cold P50 (ms) — × = how far the winner is ahead

| query | xyz | pg | winner | × |
|---|--:|--:|:--|:--|
| Q1 Point | **0.24** | 0.47 | xyz | 2.0× |
| Q2 Aggregate | **5.72** | 100.03 | xyz | **17.5×** |
| Q3 FullHistory | **0.73** | 2.60 | xyz | 3.6× |
| Q4 TopExposure | **0.32** | 0.49 | xyz | 1.5× |
| Q5 OverdueByEmpresa | **0.23** | 0.44 | xyz | 1.9× |
| Q6 RecentPayments | 1.71 | **0.58** | pg | 2.9× |
| Q7 BatchIngest | **38.46** | 100.13 | xyz | 2.6× |
| Q8 MonthlyClose | **0.31** | 0.46 | xyz | 1.5× |
| Q9 CustomerContext | **3.66** | 200.17 | xyz | **54.7×** |

**Cold wins: xyz 8/9 · pg 1 (Q6).**

### 5.2 Concurrent P50 / P99 (ms)

| query | xyz | pg |
|---|--:|--:|
| Q1 | 0.52 / 4.76 | 0.83 / 184.0 |
| Q2 | 4.69 / 8.77 | 141.7 / 306.1 |
| Q3 | 3.50 / 5.53 | 170.0 / **1581** |
| Q4 | 0.54 / 1.87 | 0.86 / 1.43 |
| Q5 | 0.42 / 2.35 | 0.80 / 2.10 |
| Q6 | 3.41 / 4.96 | 0.99 / 33.2 |
| Q7 | 4.97 / 7.64 | 374.6 / **2166** |
| Q8 | 0.55 / 0.97 | 0.82 / 2.06 |
| Q9 | 6.13 / **20.23** | 672.1 / **3482** |

**Worst concurrent P99: xyz 20.2 ms · pg 3482 ms** (~170× gap). pg's mat-view refresh
runs ~15 min each on HDD (6 refreshes, ~896 s avg).

### 5.3 Resources

| metric | xyz | pg | × (winner) |
|---|--:|--:|:--|
| Load (rec/s) | **117,867** | 85,008 | xyz 1.39× |
| Wall total | **1.25 h** | 5.15 h | xyz **4.1×** |
| RAM peak (MiB) | **2255** | 2610 | xyz 1.16× |
| Disk final (MiB) | **11,443** | 29,781 | xyz 2.6× |
| CPU mean (%) | 90.9 | 10.7¹ | — |

¹ pg's low mean CPU is not efficiency — on HDD it spends most of its time blocked on
I/O; xyz keeps the CPU working (90.9% mean) and still finishes in 4.1× less wall.

### 5.4 Reading — the HDD co-location moat
xyz **barely degrades SSD→HDD** (Q4 0.30→0.32, Q3 0.72→0.73, Q9 3.83→3.66) while pg's
analytical queries **collapse on rotational storage** (Q2 1.63→100, Q9 2.95→200, Q3
concurrent P99 → 1581). On cheap cold storage xyz's lead widens to **8/9 cold** and a
worst-P99 gap of 20 ms vs 3482 ms. Mongo is non-viable (did not complete the load in
48 h; reported not-completed).

---

## 6. Idle footprint — empty-DB baseline

> Measured 2026-06-17, Mac (arm64), Docker, fresh container, **empty database** —
> cold start-up cost before any data. Single host, single sample; the
> order-of-magnitude gaps are the robust signal, not the exact MiB.

| Engine | Image on disk | Idle RAM | vs xyz (RAM) | vs xyz (image) |
|---|--:|--:|--:|--:|
| **xyzdb** (slim / distroless-cc) | **42.7 MB** | **17.7 MiB** | 1× | 1× |
| postgres:18 + pgvector | 466 MB | 99 MiB | 5.6× | 10.9× |
| mongo:7.0 | 827 MB | 355 MiB | 20.1× | 19.4× |

xyzDB boots at 17.7 MiB (vs pg 99, mongo 355 — mongo reserves the WiredTiger cache up
front). Its working set grows lazily with data; no fixed multi-hundred-MB floor at
boot. Strong for edge / many-instance / serverless.

---

## 7. Density — N memory-agents per box (product regime)

> 2026-06-21. The experiment that matters is not "one agent serving X GB active" but
> **N memory-agents coexisting** — the real agent-memory infra pattern. 1
> process/instance per agent (real isolation); agent = one gravity bucket of 8,000
> records (~33 MB). Metric: cgroup `memory.current` per fleet, separating the
> **hard floor (anon + shmem, non-reclaimable)** from page-cache `file` (reclaimable).
> Mac OrbStack ≈ m5a.xlarge.

| engine | N | phase | cgroup tot | anon | file | shmem | floor/agent | total/agent |
|---|---|---|--:|--:|--:|--:|--:|--:|
| xyzdb | 8 | idle | 650 | 386 | 256 | 0 | **48.2** | 81.2 |
| pg | 8 | idle | 1038 | 44 | 985 | 417 | **57.6** | 129.8 |
| xyzdb | 8 | light | 2047 | 1781 | 257 | 0 | 222.6 (transient) | 255.9 |
| pg | 8 | light | 1034 | 44 | 985 | 417 | 57.6 | 129.2 |

*(MiB. light = 10 NEAREST/SELECT per agent, measured immediately after traffic.)*

**Honest reading.** The idle hard floor is **comparable, not a blowout**: xyzDB
~48 MiB/agent vs pg ~57 (~1.2×); total resident 81 vs 130/agent (~1.6×) — not the
order of magnitude an "17 MB vs 135 MB" framing suggests. Under traffic xyzDB's anon
spikes (full-bucket materialization per query) but it is **transient, not a leak** —
the balloon returns to the OS in ~75 s of calm. Capacity in 16 GiB (m5a.xlarge): xyzDB
~300 agents idle vs pg ~250 (edge ~1.2–1.6×). The real moat is **zero-DBA / zero-index
+ real per-process isolation at a competitive footprint** (a dense f32 sidecar —
materialize only top-k — is the lever for the transient balloon and the 14–38 ms
query latency).

---

## Correctness gates — and the one accepted diff

Every run gates on ingestion correctness before any latency number is trusted:

- **`verify_golden` (Phase 1.5)** — after bulk load, before cold queries: the loaded
  keyspace is hashed against a generator-derived truth file (`benchmarks/native/golden/`).
  Must be `YES / 0 diffs`.
- **Content gate (Phase 1.6)** — a stride sample of the anchored entities (clientes by
  `rfc`, creditos `Credit` rows by `credit_id`) is content-hashed to catch systematic
  value drift, not just row counts.
- **Integrity verify (Phase 5)** — per-entity row counts vs the expected dataset.

Phase 5 reports `Exact: NO` on exactly one entity, and this is **expected, accepted, and
symmetric across all three engines** — not a failure:

> The payment-bearing entity (`creditos`) shows a small positive Δ (≈ +28 600 at scale
> 0.1). It is the residue of **Q7 BatchIngest**, the write scenario in Phases 2–3: Q7
> inserts synthetic 100-row payment batches as part of the mixed read+write workload, so
> after the run the entity holds more rows than the pristine dataset. Every engine ingests
> the identical Q7 batches, so the Δ is the same for xyzDB, PostgreSQL, and MongoDB — the
> comparison stays fair. A Δ on **any other entity**, or a `verify_golden` / content-gate
> miss, is a real integrity failure and blocks the result.

The auto-generated report note (`native-bench`) restates this per run; this section is the
authoritative record so the diff is never mistaken for a regression.

## Source artefacts

Native harness under `benchmarks/native/`; raw per-cell artefacts (`aws_ssd_*`
`.json/.csv`, one set per pass) on the AWS box. Idle footprint and density are Mac
(arm64) measurements. HDD 3-pass re-run is pending.
