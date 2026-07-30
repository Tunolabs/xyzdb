# native cross-engine benchmark

The native cross-engine benchmark.

Each engine runs in its idiomatic data layout, across **three engines**: xyzDB
heterogeneous lobes, PostgreSQL with declarative partitioning + materialised
views, and MongoDB with selective embedding + `$merge` pre-aggregation. This is
the current cross-engine harness — numbers current through v0.8.x (see
[`docs/benchmark-native.md`](../../docs/benchmark-native.md)).

MongoDB image: `mongo:7.0.31`.
`mongo:8.0` and `mongo:8.2` segfault on aarch64 + OrbStack under
sustained concurrent workload — distinct subsystems, same platform
class. 7.0 LTS is the most recent line without SBE-by-default and is
empirically stable in our test environment.

The deterministic dataset generator guarantees unique RFCs across
1.5 M+ ordinals (Finding 15, base-36 homoclave bijection); this
matters at Scale 1.0 where the previous splitmix homoclave produced
duplicates on the 218th client.

## Layout

```
benchmarks/native/
├── Cargo.toml                ← workspace
├── docker-compose.yml        ← xyzdb + postgres services
├── configs/
│   ├── postgresql-t6-ssd.conf
│   └── postgresql-t6-hdd.conf
├── generator/                ← deterministic dataset crate (seed=42)
├── drivers/
│   ├── xyzdb/                ← V1 text protocol; PUT BATCH bulk; xyTalk queries
│   └── postgres/             ← tokio-postgres; COPY bulk; SQL queries; refresh thread
├── orchestrator/             ← phase sequencer + reports (JSON/CSV/MD)
├── results/                  ← gitignored — per-run outputs
└── README.md                 ← you are here
```

## Build

```bash
cd benchmarks/native
cargo build --release
```

The orchestrator binary is `target/release/native-bench`.

## Run

### 1. Bring engines up

```bash
# SSD profile (default) — all three engines.
mkdir -p data/xyzdata data/pgdata data/mongodata
STORAGE_PROFILE=ssd docker compose --profile all up -d

# A single engine — bring up only what you need:
# docker compose --profile xyzdb    up -d
# docker compose --profile postgres up -d
# docker compose --profile mongo    up -d

# HDD physical (mount your spinning disk first):
# STORAGE_PROFILE=hdd \
# XYZ_DATA="/Volumes/HDD/xyzdata" PG_DATA="/Volumes/HDD/pgdata" \
# MONGO_DATA="/Volumes/HDD/mongodata" \
# docker compose --profile all up -d
```

### 2. Run the bench

#### Bench A — xyzDB

```bash
./target/release/native-bench \
  --engine xyzdb \
  --scale 0.1 \
  --storage ssd \
  --schema-mode full \
  --duration 3600 \
  --output ./results
```

#### Bench A — PostgreSQL

```bash
./target/release/native-bench \
  --engine postgres \
  --scale 0.1 \
  --storage ssd \
  --schema-mode full \
  --duration 3600 \
  --pg-conn 'host=127.0.0.1 port=5432 user=postgres password=bench dbname=bench' \
  --output ./results
```

#### Bench A — MongoDB

```bash
./target/release/native-bench \
  --engine mongo \
  --scale 0.1 \
  --storage ssd \
  --schema-mode full \
  --duration 3600 \
  --mongo-uri 'mongodb://127.0.0.1:27017' \
  --mongo-db bench \
  --output ./results
```

### 3. Phase 6 — auto-ghost validation pass (xyzDB only)

Second run on Scale 0.1 SSD with ghosts NOT pre-created. Phase 3
exercises the scan-telemetry promotion path organically.

```bash
./target/release/native-bench \
  --engine xyzdb \
  --scale 0.1 \
  --storage ssd \
  --schema-mode auto-only \
  --duration 1800 \
  --output ./results
```

### 4. Phase selectors

`--phase` accepts `all` (default) or a comma-separated subset of
`setup,load,cold,concurrent,verify`. `--phase setup,load,cold,verify`
runs everything except the 60-min concurrent workload.

### 5. Resource sampling (CPU% / mem MiB / disk MiB)

The orchestrator polls `docker stats` every 5 s and `du -sk` every
30 s during a run, tags samples per phase, and writes peak / avg /
final values into the JSON / MD report. Defaults pick the standard
container name + bind-mount source per engine; override only when
running outside the supplied `docker-compose.yml`:

```bash
./target/release/native-bench --engine xyzdb --scale 0.1 --storage ssd \
    --container-name native-xyzdb-1 \
    --data-path ./data/xyzdata \
    --output ./results

# Disable sampling entirely:
#   --no-resources
```

The `XYZ_DATA` / `PG_DATA` / `MONGO_DATA` env vars (used by Compose
to point bind mounts at e.g. an external HDD) flow through to the
sampler's `--data-path` default automatically — HDD runs sample the
physical disk, not a stale local placeholder.

### 6. Sequence script (xyzdb → postgres → mongo)

`scripts/run_scale1.0_ssd_3engine.sh` runs the full Bench A Scale 1.0
SSD sequence one engine after the other, brings each container up
fresh, runs end-to-end (Phase 0..5 incl. 60-min concurrent), tears
down, and wipes the data dir before the next engine starts. Resource
sampling is enabled. Designed for unattended overnight execution
(~5–6 h wall clock at Scale 1.0).

```bash
caffeinate -d ./scripts/run_scale1.0_ssd_3engine.sh \
  > results/scale1.0-launch.log 2>&1 &
```

Master log: `results/scale1.0-ssd-3engine.master.log`. Per-engine
exit codes are captured via `${PIPESTATUS[0]}`.

### 7. Tear down

```bash
docker compose --profile all down --remove-orphans -v   # WARNING: -v wipes volumes
```

## Reports

Each run emits three files into `--output`:

- `<run-id>.json` — full structured report (drives the aggregate
  cross-engine results).
- `<run-id>.csv` — flat per-query latency rows for spreadsheet
  analysis.
- `<run-id>.md` — human-readable summary.

`<run-id>` = `<engine>-<storage>-scale<X>-<UTC-timestamp>`.

## Dataset

- Seed: 42 by default (`--seed`). Same seed → byte-identical dataset.
- Volumes: ~14.7 M records at scale 0.1; ~149 M at scale 1.0.
- Volumes per entity calibrated to realistic Mexican fintech operator
  proportions.

## Tests

```bash
cargo test -p native-generator --release
```

Determinism test: same seed → byte-identical hash on all 12 entity
streams. Different seeds → divergent client streams.

## Bulk-load protocol notes

- **xyzDB**: V1 text protocol with `PUT BATCH` (5 K records per batch).
  V3 binary bulk is an optimisation tracked as v0.2.5.1 follow-up; if
  Phase 1 throughput is the bottleneck, the V3 path from
  `benchmarks/sequential/harness/src/drivers/xyzdb.rs` (legacy
  reference) is the starting point.
- **PostgreSQL**: `COPY ... FROM STDIN` via `tokio-postgres`. Standard
  bulk-load primitive; faster than INSERT-batches by 5-10×.
- **MongoDB**: `insert_many` with `ordered=false`, write concern
  `{w:1, j:true}`. The credits collection bulk-loads with embedding
  strategy A: a streaming merge of credits + installments +
  collections + collection_actions assembles one embedded credit doc
  at a time without ever holding more than the current credit's
  fanout in memory.

## Refresh thread (PG + Mongo)

Phase 3 spawns refresh threads per cadence configured by the
orchestrator (defaults: 30 s → `top_active_balance`, 60 s →
`credits_by_rfc`). Wall-clock + count of refreshes accumulate as
the **maintenance tax** in the run report. PG runs `REFRESH
MATERIALIZED VIEW CONCURRENTLY`; Mongo runs the equivalent
aggregation pipeline with `$merge`. xyzDB has no equivalent thread
— its ghosts auto-update incrementally via post-write hooks.

## Reference

- Engine: [`docs/architecture.md`](../../docs/architecture.md).
- Language: [`docs/xytalk-spec.md`](../../docs/xytalk-spec.md).
- Consolidated results: [`docs/benchmark-native.md`](../../docs/benchmark-native.md).
