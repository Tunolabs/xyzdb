# xyzDB

A semantic gravity database. Related records live together on disk by identity — graph traversal is a range scan, aggregates are metadata reads, pre-aggregations build themselves from observed query telemetry. A single-tier hardened LSM engine; semantic vector search (`NEAREST`) runs as an exact, gravity-bounded scan with no ANN index — embeddings are always supplied by the caller; the engine never embeds.

```
Bench — native cross-engine · AWS m6a.xlarge · T6 envelope (2C/8GB) · scale 1.0 ≈ 150 M · 3-pass pooled · parity-fair

  Cold P50:        xyzDB wins 6/9 queries, 1.6–3.4× over the runner-up
  Concurrent P99:  worst of all 9 queries — xyzDB 8.6 ms · PostgreSQL 747 ms · MongoDB 155 ms
  Load:            xyzDB 124 K rec/s · pg 98 K · mongo 14 K
  Footprint:       RAM peak 2.1 GiB (lowest) · disk 12.2 GiB (pg 28.2 · mongo 13.2)

  HDD (same scale): xyzDB 8/9 cold · worst P99 20 ms vs pg 3 482 ms
                    mongo (v8.0 / v7.0) did not complete the load in 48 h — reported not-completed

Agentic memory — LongMemEval (bge-large 1024d) · vs pgvector / qdrant / chroma · 256 MB→4 GB envelopes

  Fit:            xyzDB the only single-container memory that fits every envelope · pg never loads 246k
  S1 p50 @246k:   flat 17.6–18.9 ms across 256 MB→4 GB · recall 1.0 (exact NEAREST)
```

Same machine, same dataset, same questions — each engine in its native data layout (xyzDB heterogeneous lobes, PG normalised + materialised views, Mongo selective embedding + `$merge`). The HDD physical pass preserves the ordering. Full data and methodology: [`docs/benchmark-native.md`](docs/benchmark-native.md).

---

## Quick start

```bash
# Run the server
cargo run -p xyzdb-server --release -- --path ./data --port 2505

# Connect with the REPL
cargo run -p xyzdb-cli -- --port 2505
```

```text
-- A LOBE is a semantic data space. An ANCHOR is a unique field that
-- xyzDB uses as entity identity — records sharing an anchor value
-- physically co-locate on disk (see "What makes xyzDB different" below).

LOBE "workspace"
ANCHOR "code" UNIQUE IN "workspace"

PUT {_type: "Company", code: "ACME", name: "Acme Corp"} IN "workspace"
PUT {_type: "Project", project_id: "P1", budget: 50000} IN "workspace"
    LINK TO "workspace" WHERE code = "ACME" AS "owner"

FIND "workspace" WHERE code = "ACME" | PULL depth=2
```

One query. One range scan. Entire entity graph.

### Clients

The engine repo ships a single-file, stdlib-only **reference client**,
[`examples/client/python/xyzdb_minimal.py`](examples/client/python/xyzdb_minimal.py)
(`connect` / `execute` with `$param` binding / `put_batch`). It illustrates the
wire protocol specified in [`PROTOCOL.md`](PROTOCOL.md) — which anyone may
implement freely, in any language, under any license. Like the rest of this
repository it is BUSL-1.1, so it is for illustration; for a production client,
implement straight from `PROTOCOL.md` or use the Apache-2.0 client packages
(`xyzdb` on PyPI, npm, and crates.io) that ship from a separate repository. See
[`examples/client/python/README.md`](examples/client/python/README.md).

---

## MCP integration

xyzDB ships an MCP (Model Context Protocol) server that lets MCP-compatible clients call into the database without learning xyTalk syntax or implementing a TCP driver. The server exposes five tools (`stats`, `query`, `snapshot`, `list_lobes`, `describe_lobe`) and three resources (`xyzdb://lobes`, `xyzdb://stats`, `xyzdb://lobes/{name}`).

```bash
cargo build --release -p xyzdb-mcp
```

```jsonc
// ~/Library/Application Support/Claude/claude_desktop_config.json (macOS)
{
  "mcpServers": {
    "xyzdb": {
      "command": "/abs/path/to/xyzdb/target/release/xyzdb-mcp",
      "args": ["--embed", "/abs/path/to/your/data"]
    }
  }
}
```

Two modes: `--embed <PATH>` (canonical single-process subprocess pattern) and `--connect <HOST:PORT>` (TCP client of an existing `xyzdb-server`, canonical for a multi-process deployment). Privacy-clean by default — statement text never appears in logs, only an xxh3-64 fingerprint plus first verb. A `--log-statements` development flag adds full TRACE logging behind a cross-actor leak guard that refuses non-loopback `--connect` targets.

Prefer not to build from source? A prebuilt image ships `xyzdb-mcp` and is listed on the [MCP registry](https://registry.modelcontextprotocol.io) as `io.github.Tunolabs/xyzdb`:

```bash
docker run -i --rm -v /abs/path/to/your/data:/data \
  ghcr.io/tunolabs/xyzdb-mcp:1.0.1 --embed /data
```

Run with `-i` (stdio transport); see the [Docker section](docs/mcp-integration.md#docker-image) for `--connect` and MCP-client config.

Full reference: [`docs/mcp-integration.md`](docs/mcp-integration.md). Templates and an annotated wire transcript: [`examples/mcp/`](examples/mcp/).

---

## What makes xyzDB different

**Semantic gravity.** You mark a field with `*` (or declare an `ANCHOR`) and every record sharing that value is stored in the same physical block. A customer and all their orders, payments, and sessions — one sequential read, no JOINs. Records inherit their parent's location through `LINK`, transitively. Co-location is not indexing after the fact; it's the layout.

**Auto-detected ghosts.** Materialised views that maintain themselves incrementally through post-write hooks. A ghost filtered by `status = 'overdue'` with `AGGREGATE count(), sum(monto)` turns Q5 from a multi-million-row scan into a counter read. `EMBED` projects fields into ghost entries so top-N queries return without a single point read. No cron jobs, no refresh windows — hooks fire on every write that affects the ghost. Scan telemetry promotes hot query shapes into ghosts automatically; the DBA does not have to anticipate them.

**Gravity-as-index.** When a SCAN's `WHERE` clause matches the lobe's gravity field, the executor consults the gravity bucket directly instead of scanning the lobe. A point-by-anchor query on an 11.7 M-record lobe returns in 0.55 ms cold — same order of magnitude as a B-tree index seek, without any explicit index declaration.

---

## Benchmarks — native cross-engine

**xyzDB vs PostgreSQL vs MongoDB**, each engine in its idiomatic shape and
**parity-fair** (every engine gets its idiomatic fast path — pg covering /
mat-view indexes, mongo aggregation pipelines). AWS m6a.xlarge (x86-64-v3), T6
envelope (2 vCPU / 8 GB), **scale 1.0 ≈ 150 M records**, 3 passes pooled,
ingestion golden-verified bit-for-bit. Full tables in
[`docs/benchmark-native.md`](docs/benchmark-native.md).

Headline (SSD): xyzDB wins **6/9 cold queries (1.6–3.4×)**, **load**
(124 K rec/s), **RAM** (2.1 GiB peak, lowest) **and disk** (12.2 GiB vs pg
28.2) simultaneously — and under mixed read+write holds **worst-case P99 at
8.6 ms across all nine queries** (pg spikes to 747 ms on its mat-view refresh;
mongo sits at 42–63 ms). It concedes two covering-index point queries to pg
and ties batch-ingest with mongo. On **HDD** the gap widens to 8/9 cold and a
20 ms vs 3 482 ms worst concurrent P99; mongo (v8.0 and v7.0) did not complete
the load in 48 h under the same parity configuration — reported not-completed.

### Reproduce

```bash
cd benchmarks/native
cargo build --release

# Bring the engines up (SSD profile is default; HDD profile reads
# postgresql-t6-hdd.conf and configures wider xyzDB block sizes)
mkdir -p data/xyzdata data/pgdata data/mongodata
STORAGE_PROFILE=ssd docker compose --profile all up -d

# Run a single engine end-to-end (Phase 0..5, 60-min concurrent)
./target/release/native-bench --engine xyzdb --scale 0.1 --storage ssd \
    --duration 3600 --output ./results

# Or the full sequence (xyzdb → postgres → mongo) in one shot
./scripts/run_scale1.0_ssd_3engine.sh
```

Reports land at `benchmarks/native/results/<engine>-<storage>-scale<X>-<UTC-ts>.{json,csv,md}`. Every run captures CPU% / memory MiB / disk MiB samples per phase via the resource sampler.

### Methodology gate

The bench includes a `verify_golden` step (Phase 1.5, post-bulk-load pre-cold-queries) that checks ingestion correctness against a generator-derived truth file ([`benchmarks/native/golden/`](benchmarks/native/golden/)). For harness configuration and the query set see [`docs/usage/reference.md`](docs/usage/reference.md) §4.3.

### Native results (full data)

Consolidated cross-engine measurements (scale 1.0, SSD 3-pass + HDD, vs PostgreSQL and MongoDB on the T6 envelope) live in [`docs/benchmark-native.md`](docs/benchmark-native.md), kept current with the engine.

DBA-less storyline (load-bearing claim): xyzDB resolves anchor-supported queries sub-ms at scale 1.0 with **zero schema work beyond the anchor declaration**. PostgreSQL and MongoDB reach better latency on some aggregate queries — but only via DBA-declared mat-views / `$merge` collections with maintenance cadence. xyzDB's value proposition is competitive sub-ms latency without the DBA ritual, not "fastest at every query".

---

## Benchmarks — agentic memory

**xyzDB vs pgvector vs qdrant vs chroma** as an agent-memory store, each in its
idiomatic best form, on the **same** corpus and queries. Dataset: **LongMemEval**
(`bge-large-en-v1.5`, 1024-d; retrieval bucket = conversational session). Six
agent-memory workloads (retrieve-and-expand, live session, multi-agent fleet,
serverless wake, hybrid filter + `NEAREST`, composite turn) run across four
hardware envelopes — **256 MB → 4 GB** DRAM, 1–2 vCPU — on AWS m6a.xlarge
(x86-64-v3), one engine at a time, fresh container + wiped data dir per cell.
Complete: 256/256 cells. Full tables in
[`docs/benchmark-agentic.md`](docs/benchmark-agentic.md).

Headline:

- **xyzDB is the only single-container deployment that fits every envelope, and its
  latency is flat across them.** At the full corpus (246k memories) S1 p50 is
  **18.6 / 18.9 / 18.3 / 17.6 ms from 256 MB (T1) to 4 GB (T4)** — the envelope
  changes what it *uses* (RAM peak 226 MB → 1550 MB, elastic), not how it *behaves*.
  **Recall 1.0 in every cell** (exact `NEAREST`, never approximate).
- **Cold wake (TTFQ): 0.02–1.4 s** for xyzDB versus **1.2–8.4 s** for qdrant+pg in
  every cell — xyzDB serves straight from the LSM; the rivals load the
  graph/collections into RAM on first access.
- **PostgreSQL/pgvector never loads the full corpus** — 246k OOMs during the HNSW
  build at every tier, including 4 GB + swap. **chroma+pg is the slowest at every
  size** (S1 p50 36 → 290 ms as N grows) and needs ≥ 2 GB for the full corpus.
- **Fleet (S3):** creating the 1000th tenant costs xyzDB/qdrant ~150–190 ms, pg
  ~1.0 s (DDL per tenant); chroma never reached 1000 tenants at any tier.

Reproduce from a clean checkout — the harness fetches the public dataset itself:

```bash
cd benchmarks/agentic
./fetch_corpus.sh       # downloads LongMemEval (public dataset)
./run_envelope_aws.sh   # the envelope matrix; see the directory README for options
```

---

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│  xyzdb-server (TCP :2505)                                   │
│  ├── V1 protocol  (text xyTalk)                             │
│  ├── V2 protocol  (text xyTalk + format byte → JSON)        │
│  ├── V3 protocol  (binary bulk-load frames)                 │
│  └── V4 protocol  (bound params → injection-safe)           │
└─────────────────────────────────────────────────────────────┘
       │
       ▼
┌─────────────────────────────────────────────────────────────┐
│  xyzdb-engine  ·  query layer                               │
│  ├── xytalk-parser      nom-based xyTalk → AST              │
│  ├── planner + router   ghost selection, query plan         │
│  ├── ops                FIND, SCAN, PULL, GROUP BY, ghost   │
│  ├── auto-anchor        ANALYZE-driven anchor promotion     │
│  ├── auto-ghost         scan-telemetry ghost promotion      │
│  └── post-write hooks   incremental ghost maintenance       │
└─────────────────────────────────────────────────────────────┘
       │
       ▼
┌─────────────────────────────────────────────────────────────┐
│  turba-engine  ·  custom LSM storage                        │
│  ├── 5 keyspaces        spatial, identity, dictionary,      │
│  │                      ghosts, vectors                     │
│  ├── ArcSwap version    zero-lock reads                     │
│  ├── WAL group commit   1 ms sync thread, batched fsyncs    │
│  ├── Zone maps          per-block min/max, skip on filter   │
│  └── Compaction         leveled, dual-criterion overflow,   │
│                         L0 emergency cap, jemalloc          │
└─────────────────────────────────────────────────────────────┘
```

**Co-location mechanism.** Every record carries a `SpatialKey` whose prefix encodes a 48-bit `gravity_hash` derived from its gravity field / anchor (or from a linked parent's). Records sort contiguously in SSTable files by `gravity_hash`, so `FIND … | PULL depth=N` becomes a single range iterator instead of N random point reads. Gravity-as-index extends the same primitive to SCAN equality predicates on the gravity field — and since the post-v0.7.0 work, `FIND` and `PULL` resolve those predicates through the same bounded bucket range scan (the older single-LID gravity dictionary entry, which truncated multi-record buckets, was retired). The key is now 24 bytes (`MANIFEST_VERSION 5`), carries a `sat` axis for satellite placement (sub-gravity: `SATELLITE BY <field>` sub-divides one gravity bucket so a query pinning both fields reads only the matching rows — opt-in per lobe, exact, no format change), and pairs with a v2 cursor token — widened from the 22-byte `MANIFEST_VERSION 4` layout (v0.6.0-pre) and the pre-v0.6 18-byte / 21-bit key.

**Ghost system.** A `CREATE GHOST` statement defines a filter + order + aggregate. Post-write hooks on the source lobe maintain it incrementally: new records matching the filter get inserted into the ghost's keyspace, aggregates update in place. `EMBED field1, field2` projects fields into ghost entries — top-N queries read the ghost and return rows without touching the source lobe. Auto-promotion (the v0.2.0-alpha lifecycle: Permanent / Ephemeral / Promoted) creates ghosts from observed query shapes once latency or frequency thresholds trigger. **Lightweight ghosts (post-v0.7.0):** an aggregate ghost grouped on a high-cardinality key (one group per client → millions) would keep millions of accumulators in RAM; past a threshold it spills to one canonical rollup entry per group on disk (bloom-backed exact `get`), so RAM stays bounded while low-cardinality ghosts stay fully in-memory.

**Read path.** Readers acquire an `Arc<SuperVersion>` via `ArcSwap` — zero locks, zero coordination with writers. MVCC sequence numbers provide snapshot isolation.

**Write throttle (xyzdb-engine).** Adaptive based on L0 table count and sealed memtable count. `balanced` profile (default): Healthy → Degraded (L0 > 8, throttled to 8 K writes/s) → Critical (L0 > 16, 2 K writes/s) → Paused (sealed memtables > 3). Other profiles: `transactional`, `analytical`, `bulk` (disabled), `maintenance`.

Deep dive in [`docs/architecture.md`](docs/architecture.md). Language surface in [`docs/xytalk-spec.md`](docs/xytalk-spec.md).

---

## Testing

`cargo test --workspace -- --list` registers **981 tests** on the Linux x86-64 CI runner. (The source carries 989 `#[test]` / `#[tokio::test]` attributes; the difference is tests behind a platform `cfg` that do not compile there.) Run `cargo test --workspace` for the authoritative pass count. Notable suites:

| Layer | Where |
|---|---|
| `xyzdb-engine` integration (ghost lifecycle, co-location, GROUP BY, OR filters, dot-notation, gravity-as-index, cursor pagination) | `crates/engine/tests/integration.rs` |
| gravity read-path (FIND-returns-full-bucket, PULL hash-collision filter) · lightweight ghosts (build-spill parity, incremental RMW, bulk+refresh, DROP purge) | `crates/engine/tests/gravity_bucket_lifecycle.rs`, `crates/engine/tests/lightweight_ghosts.rs` |
| `xyzdb-engine` unit (~140) incl. cursor encode/decode, ghost-prefix purge, PIN-prefix migration | `crates/engine/src/` |
| `xytalk-parser` unit (~67) · `xyzdb-core` unit (~63) | `crates/{xytalk-parser,core}/` |
| `turba-engine` (block, compaction, crash recovery, durability proptest, memtable, sstable, supversion loom, tree, flush+compact race, L1+ `get_at` regression) | `crates/turba-engine/tests/` + `src/` |

`cargo test --workspace` from each workspace runs the relevant suite; run it for the authoritative count. The `validation/` suites under `tools/validation/` are operational (require a running server). `cargo clippy --workspace` exits with zero errors and a tracked set of ~83 production `unwrap`/`expect` warnings (post-launch debt, `warn`-level by policy). Counts here are measured on the Linux x86-64 CI runner; earlier macOS-only figures understated the platform-gated code.

---

## What xyzDB is not

xyzDB is designed for a specific problem: applications with graph-shaped data where co-location pays off. What it doesn't do:

- **Not multi-node — yet.** Single-host only. No replication, no sharding, no HA. On the Beyond-1.0 roadmap, not v0.x.
- **Bearer-token auth + TLS 1.3, but no authorization model.** The server binds loopback by default; a non-loopback bind requires a bearer token (`--auth-token`) or an explicit `--insecure-allow-no-auth` override. TLS 1.3 is available (since v0.4). There is no per-user authorization, RBAC, or multi-tenancy — deploy behind your own access layer.
- **Not ACID across entities — yet.** Writes are atomic per keyspace + WAL group commit. Cross-entity transactions are not supported; cross-lobe transactions are a Beyond-1.0 item.
- **No encryption at rest — yet.** None of the workspace `Cargo.toml`s pull `argon2` / `aes-gcm` / `hkdf`. Deploy on an encrypted volume (LUKS / dm-crypt / FileVault) if at-rest confidentiality is required. A design draft from March 2026 declared AES-256-GCM "active by default"; that aspiration was not implemented — encryption at rest is a Beyond-1.0 item.
- **No GNG / online auto-organization.** Background work is leveled compaction + auto-ghost lifecycle (Permanent / Ephemeral / Promoted) only. The "Growing Neural Gas" thread positioned in an early design as a third pillar was never implemented; there is no online learning of record placement.
- **Ad-hoc scans without a matching ghost are slower than indexed databases.** Once auto-ghost has observed a recurring query, latency drops to ghost-class. Until then a `SCAN WHERE field > X` runs 1–25 ms (xyzDB, Scale 0.1) versus PostgreSQL 0.2–0.7 ms with a B-tree index.
- **No full-text search, no global ANN index.** Vector similarity (`NEAREST`, v0.8) is an **exact** cosine/dot/l2 scan **bounded to a gravity bucket** — there is no HNSW/IVF/ANN index for whole-lobe nearest-neighbour, and no full-text search. The engine never embeds: the caller always supplies the vector.
- **Ghosts go stale after an `ON CONFLICT UPDATE` upsert.** The upsert path does not notify the ghost layer, so a covering ghost keeps the pre-upsert record and an aggregate ghost keeps stale `count` / `sum` until the ghost is rebuilt. **Mitigation:** run `REFRESH GHOST "<name>"` after a load that upserts.
- **`NEAREST` cannot be served from a ghost.** Routed explicitly through one (`SCAN GHOST "<name>" … | NEAREST`), it returns the ghost's index entries — null LIDs, only the embedded fields — not resolved records. Use a plain `SCAN … | NEAREST`: a filter that matches a ghost still routes through it for the filter but falls back to primary point-reads for the vector and returns correct records.
- **`FIND` and `PULL` don't self-limit — yet.** Without an explicit `LIMIT`, `FIND` returns every matching record and `PULL` every linked record — there is no default row cap (unlike `SCAN`, capped at `SCAN_LIMIT_DEFAULT = 1000`) and no time budget (unlike `NEAREST`'s `--nearest-budget-ms`); `PULL` bounds only traversal depth (`MAX_PULL_DEPTH = 10`), not cardinality. Pass a `LIMIT` for interactive queries over large lobes. A default cap and query budget are a roadmap item.
- **Not battle-tested.** PostgreSQL has 30 years of production. xyzDB has the published benchmarks in [`docs/benchmark-native.md`](docs/benchmark-native.md) and the regression tests above.

---

## Compatibility

- **Rust** the build toolchain is pinned to **1.96 / edition 2024** in `rust-toolchain.toml` — the only toolchain built and CI-tested. Edition 2024 sets the language floor at 1.85, but earlier toolchains are not tested and are not a supported MSRV.
- **OS** Linux (Docker-tested) and macOS (development + benchmarking on Docker Desktop / OrbStack).
- **Storage** SSD gives the best *absolute* latency and is the default. HDD is a **first-class profile, not a degraded mode**: a dedicated storage profile widens block sizes and raises bloom bits per key, and it is where xyzDB's *relative* lead is largest — **8/9 cold wins** and a worst concurrent P99 of **20.2 ms vs PostgreSQL's 3 482 ms (~170×)**, versus 6/9 and 8.6 ms vs 747 ms on SSD. Run on cheap disk and stay fast.
- **Memory** runs in 2 GB (T2) for light workloads; 8 GB (T6) is the benchmark reference.

Core dependencies: `crossbeam-skiplist` (lock-free memtable), `arc-swap` (zero-lock version swap), `parking_lot`, `zstd` + `lz4_flex`, `xxhash-rust/xxh3`, `tikv-jemallocator` (Linux global allocator). No network database dependencies — `turba-engine` is a from-scratch LSM, not a wrapper.

---

## Roadmap

The full release history — every version, format break, and design decision — lives in [`CHANGELOG.md`](CHANGELOG.md) and [`docs/releases/`](docs/releases/).

### 1.0 — first public release

**1.0 is the first public, source-available (BUSL-1.1) release** of the hardened single-tier engine. It publishes the line that stabilised across `0.8.x` — the last pre-1.0 public series: the vector column and gravity-bounded exact `NEAREST` (0.8.8), then `NEAREST` hardening, WAL bounding, and the streaming bucket sweep (0.8.9–0.8.13) — now under a license.

### Beyond 1.0

The full roadmap — what's next on the 1.0.x line, what's planned for later, and what xyzDB is **deliberately not doing** — lives in [`ROADMAP.md`](ROADMAP.md).

Cross-engine bench evidence (xyzDB vs PostgreSQL / MongoDB, AWS scale 1): [`docs/benchmark-native.md`](docs/benchmark-native.md).

---

## Components

| Directory | Description |
|---|---|
| [`crates/turba-engine/`](crates/turba-engine/) | Custom LSM storage engine (library, no network). |
| [`crates/`](crates/) | Parser, query engine, server, CLI, MCP server. |
| [`examples/`](examples/) | Reference client and MCP integration examples (BUSL-1.1, like the rest of the repo). |
| [`benchmarks/native/`](benchmarks/native/) | Native cross-engine benchmark (xyzDB + PG + Mongo). |
| [`benchmarks/agentic/`](benchmarks/agentic/) | Agentic-memory benchmark (xyzDB + pgvector + qdrant + chroma) on LongMemEval. |
| [`docs/`](docs/) | Versioned documentation: architecture, language spec, usage, releases. |

---

## License / Licencia

**EN — xyzDB is source-available, not open source.** The engine is published
under the [Business Source License 1.1](./LICENSE) (BUSL-1.1).
Copyright (c) 2026 Iván Moreno Mendoza.

Free, without asking us and without paying us:

- Downloading, reading, modifying and redistributing the source.
- Any non-production use: evaluation, development, testing, CI, benchmarking.
- Running it in production for the internal purposes of your own organization,
  at any scale and on as many deployments as you want.

A commercial license is required for any production use that makes xyzDB, or
what it does, available to third parties — **whether or not you charge for it**:

- Offering it to third parties as a hosted or managed service (DBaaS, PaaS,
  IaaS).
- Using it as a component of a product, application or service you make
  available to third parties.
- Delivering it to third parties for their production use, on a commercial
  basis, standalone or embedded.

**Consultants and integrators**: you may deploy, operate, support or maintain
xyzDB on behalf of a client, and build bespoke work for them, for a fee — as long
as the client's own use is itself permitted. What needs a license is offering it
as your own service to many clients, or supplying the same xyzDB-based product
to several of them.

The line is internal versus third parties, not free versus paid.

xyzDB 1.0 becomes available under the Apache License 2.0 on **2029-08-01**, three
years after publication. Every version carries its own date, three years or less
— see [docs/license-change-dates.md](./docs/license-change-dates.md). Additional
permissions, if any, are recorded in [PERMISSIONS.md](./PERMISSIONS.md).

If you redistribute copies, the license requires you to display the license text
conspicuously with them. Everything in this repository, including the reference
client under `examples/`, is BUSL-1.1; the installable client packages (`xyzdb` on
crates.io, PyPI and npm) are Apache 2.0. The wire protocol in
[PROTOCOL.md](./PROTOCOL.md) may be implemented freely by anyone.
Trademark policy: [TRADEMARKS.md](./TRADEMARKS.md).

xyzDB is provided **"as is"**, without warranty of any kind, express or
implied — see the [`LICENSE`](./LICENSE) file for the full disclaimer. You are
responsible for validating it against your own workload before relying on it.

Contributions: issues, bug reports and benchmark reproductions are welcome now.
Code contributions are not being accepted yet — xyzDB is also licensed
commercially, so we need a contributor agreement in place first. It is being
drafted; we are targeting the last week of August 2026 to open pull requests — a target, not a guarantee, and if it moves it will be announced here.

Commercial licensing: **licensing@tuno.bar**

---

**ES — xyzDB es source-available, no open source.** El motor se publica bajo la
[Business Source License 1.1](./LICENSE) (BUSL-1.1).
Copyright (c) 2026 Iván Moreno Mendoza.

Gratis, sin pedirnos permiso y sin pagarnos:

- Descargar, leer, modificar y redistribuir el código.
- Cualquier uso no productivo: evaluación, desarrollo, pruebas, CI, benchmarks.
- Usarlo en producción para los fines internos de tu propia organización, a
  cualquier escala y en tantos despliegues como quieras.

Requiere licencia comercial cualquier uso en producción que ponga xyzDB, o lo
que hace, a disposición de terceros — **cobres o no**:

- Ofrecerlo a terceros como servicio alojado o gestionado (DBaaS, PaaS, IaaS).
- Usarlo como componente de un producto, aplicación o servicio que pongas a
  disposición de terceros.
- Entregarlo a terceros para su uso en producción, con ánimo comercial,
  suelto o embebido.

**Consultoras e integradores**: puedes desplegar, operar, dar soporte o mantener
xyzDB por cuenta de un cliente, y desarrollarle trabajo a medida, cobrando por
ello — siempre que el uso del propio cliente esté permitido. Lo que requiere
licencia es ofrecerlo como servicio propio a varios clientes, o suministrar el
mismo producto basado en xyzDB a varios de ellos.

La línea es interno frente a terceros, no gratis frente a pago.

xyzDB 1.0 pasa a Apache License 2.0 el **2029-08-01**, tres años después de su
publicación. Cada versión lleva su propia fecha, de tres años o menos — ver
[docs/license-change-dates.md](./docs/license-change-dates.md). Los permisos
adicionales, si los hay, se recogen en [PERMISSIONS.md](./PERMISSIONS.md).

Si redistribuyes copias, la licencia te obliga a mostrar su texto de forma
visible junto a ellas. Todo lo que hay en este repositorio, incluido el cliente
de referencia en `examples/`, es BUSL-1.1; los paquetes instalables (`xyzdb` en
crates.io, PyPI y npm) son Apache 2.0. El protocolo de
[PROTOCOL.md](./PROTOCOL.md) puede implementarlo cualquiera.
Política de marca: [TRADEMARKS.md](./TRADEMARKS.md).

xyzDB se proporciona **"tal cual"**, sin garantía de ningún tipo, expresa o
implícita — el descargo completo está en el fichero [`LICENSE`](./LICENSE).
Validarlo contra tu propia carga antes de depender de él es tu
responsabilidad.

Contribuciones: las issues, los reportes de fallo y las reproducciones de
benchmark son bienvenidas ya. Las contribuciones de código todavía no se aceptan:
xyzDB también se licencia comercialmente, así que primero necesitamos un acuerdo
de contribución. Se está redactando; nuestro objetivo es abrir los pull requests
la última semana de agosto de 2026 — es un objetivo, no una garantía, y si la
fecha se mueve se anunciará aquí.

Licenciamiento comercial: **licensing@tuno.bar**

> This summary has no legal value; the LICENSE file prevails. / Este resumen no
> tiene valor legal; prevalece el fichero LICENSE.

## Author & stewardship

Created and designed by **Iván Moreno Mendoza**. xyzDB is his personal research project; he holds the copyright and continues to develop it. The "xyzDB" trademark is held by Tuno Labs, which also handles commercial licensing.

Official site: [xyzdb.bar](https://xyzdb.bar) · Contact: `ivan@tuno.bar` · [tuno.bar](https://tuno.bar)

---

*"Related data should live together. Everything else follows from there."*
