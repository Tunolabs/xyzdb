# Benchmarks

Reproducible cross-engine benchmark harnesses for xyzDB.

## Active harness — native cross-engine

[`native/`](native/) — each engine runs in its **idiomatic data layout** (xyzDB heterogeneous lobes, PostgreSQL normalised tables + materialised views, MongoDB selective embedding + `$merge`). Comparison is on **business questions** rather than shape-shared queries. Ships with four engines: **xyzDB + PostgreSQL 18 + MongoDB 7.0 LTS + SurrealDB**.

The native bench is the source of the cross-engine numbers. Per-question native form is the business-questions table below; consolidated cross-engine results live in [`docs/benchmark-native.md`](../docs/benchmark-native.md).

```bash
cd benchmarks/native
cargo build --release

# SSD profile (default) — three engines.
mkdir -p data/xyzdata data/pgdata data/mongodata
STORAGE_PROFILE=ssd docker compose --profile all up -d

# Single engine (Phase 0..5, 60-min concurrent included)
./target/release/native-bench --engine xyzdb --scale 0.1 --storage ssd \
    --duration 3600 --output ./results

# Or the full xyzdb → postgres → mongo sequence in one shot
./scripts/run_scale1.0_ssd_3engine.sh

docker compose --profile all down --remove-orphans
```

For HDD physical runs, point bind mounts at the spinning disk:

```bash
STORAGE_PROFILE=hdd \
XYZ_DATA="/Volumes/HDD/xyzdata" PG_DATA="/Volumes/HDD/pgdata" \
MONGO_DATA="/Volumes/HDD/mongodata" \
docker compose --profile all up -d
```

Reports land at `results/<engine>-<storage>-scale<X>-<UTC-ts>.{json,csv,md}`. Each run captures CPU% / memory MiB / disk MiB samples per phase via the resource sampler (commit `ab3c88b`).

Full operational guide: [`native/README.md`](native/README.md).

## Datasets and scales

The native bench generator (`benchmarks/native/generator/`) emits a deterministic Mexican-fintech dataset keyed by a single seed (default 42).

| Scale | Clients | Total records | Approx. on-disk (SSD, post-load) |
|---:|---:|---:|---|
| 0.001 | 1 500 | ~150 K | xyzDB ~ 12 MB · PG ~ 70 MB · Mongo ~ 16 MB |
| 0.01 | 15 000 | ~1.5 M | xyzDB ~ 120 MB · PG ~ 700 MB · Mongo ~ 160 MB |
| **0.1** | **150 000** | **~15 M** | **xyzDB 1.1 GB · PG 6.8 GB · Mongo 1.4 GB** |
| 1.0 | 1 500 000 | ~150 M | xyzDB ~ 11 GB · PG ~ 70 GB · Mongo ~ 14 GB |

Scale 0.1 and 1.0 (SSD + HDD) have all been run; the consolidated cross-engine results are in [`docs/benchmark-native.md`](../docs/benchmark-native.md).

## Hardware reference

| Tier | CPU | RAM | xyzDB cache | PG `shared_buffers` | Mongo WT cache |
|---|---:|---:|---:|---:|---:|
| **T6** | **2** | **8 GB** | **1 GB** | **2 GB** | **3 GB** |

T6 is the reference cgroup for all published numbers — small CPU, generous RAM. The configuration rewards engines that keep their working set in memory rather than those that throw cores at the problem.

## Business questions

The native bench measures **nine cold-query** business questions (Q1–Q9), a transactional-cascade query (Q10), and a sustained 60-min concurrent workload. Definitions and per-engine native form are in the table below.

| # | Question | xyzDB native form | PG native form | Mongo native form |
|---|---|---|---|---|
| Q1 | Point lookup by RFC | `FIND WHERE rfc =` (anchor) | `SELECT WHERE rfc =` (B-tree) | `findOne({_id: rfc})` |
| Q2 | Total credit exposure | ghost pre-aggregated | mat view direct read | pre-agg collection lookup |
| Q3 | Complete portfolio history | `FIND \| PULL` (gravity range scan) | 7-source `UNION ALL` | embedded credits + 3 collection finds |
| Q4 | Top 100 by exposure | ghost ORDER BY + LIMIT | mat view `top_active_balance` | `top_active_balance` collection |
| Q5 | Overdue installments by branch | ghost auto-detected from telemetry | runtime `GROUP BY` | `$unwind` over embedded installments |
| Q6 | Recent payments above threshold | covering ghost | covering compound index | covering compound index |
| Q7 | Batch ingest (100 payments) | `PUT BATCH` | multi-row `INSERT` | `insertMany` |
| Q8 | Monthly close aggregation | ghost / scan | `monthly_close_mat` mat view | aggregation pipeline |
| Q9 | Full customer context | multi-bucket gravity read | stored `FUNCTION` (1 round-trip) | aggregation pipeline |
| Q10 | Transactional cascade | deferred (n=0) | transaction | deferred (n=0) |
| — | Sustained 60-min concurrent | 8 readers + 1 writer + ghost auto-update | + REFRESH MATERIALIZED VIEW thread | + `$merge` refresh thread |

## Results

Per-run reports live under `native/results/`. The consolidated, citable cross-engine results are in [`docs/benchmark-native.md`](../docs/benchmark-native.md).

---

For project context and the design narrative, see the [root README](../README.md).

Created by **Iván Moreno Mendoza** (I.V.M.), CTO & Co-founder at **TUNO Labs** · BUSL-1.1
