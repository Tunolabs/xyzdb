# xyzDB — Architecture

**State:** current as of `1.0` (2026-07-30). The core engine architecture below is accurate; sections describing pre-v0.6 surfaces remain valid where the engine is unchanged — see the pre-1.0 changelog in `docs/releases/CHANGELOG-pre-1.0.md` for per-version deltas. Notable changes since v0.5.0: `gravity_hash` widened 21 → 48 bits with `MANIFEST_VERSION` 4 (v0.6.0-pre); high-cardinality aggregate ghosts spill their group rollups to disk (**lightweight ghosts**, §4.3, v0.7.6); the **GravitySpec keel + D1** value-only canonical placement hash and cacheable per-SST metadata landed in v0.8.0. **The engine is a single-tier hardened LSM.** v0.8.8 added the **vector subsystem** (§3.7): a 5th `vectors` keyspace and a fused, gravity-bounded EXACT `NEAREST`. **1.0 widened `SpatialKey` to 24 bytes** with a `sat` satellite axis and `MANIFEST_VERSION` 5 (§2.3); **the axis is now live** — `SATELLITE BY <field> IN "lobe"` sub-divides one gravity bucket so a query pinning both fields reads only the matching rows (§2.3.1). Opt-in per lobe: a lobe that does not declare it keeps every record at satellite 0 and behaves exactly as before: a pre-1.0 (0.8.x, 22-byte / v4) data dir is rejected on open — there is no in-place migration, the upgrade path is re-ingestion from source.
**Scope:** layered architecture from wire protocol down to disk; data-model rationale, durability contract, and component responsibilities.

This document describes engine state. Per-version narrative lives in `docs/releases/<version>.md`. Forensic history of closed findings lives in the corresponding release notes.

---

## 0. Pillars

xyzDB is a single-node LSM database designed around physical co-location of semantically related records. Where a relational engine normalises by type and recovers relationships through joins, xyzDB co-locates by domain and exposes traversal as a primitive. The pillars below are organised by what each contributes to that thesis.

### Tier 1 — Data model (the radical idea)

- **Heterogeneous lobes.** A lobe co-locates records of multiple `_type` values that share a domain (`creditos` aloja `Credit + Installment + Payment + Collection + CollectionAction`). One physical bucket, one ordering, one scan.
- **Semantic gravity.** Fields prefixed with `*` in a `PUT` define `gravity_hash`. Records that share a gravity value cluster into contiguous block ranges.
- **Z-order co-location.** Physical key is `SpatialKey(lobe_id: u16, gravity_hash: u48, sat: u16, z_order_2d: u48, seq: u64)` — 24 bytes big-endian. Same gravity → adjacent on disk. (`sat` is the satellite / sub-gravity axis: 0 unless the lobe declares `SATELLITE BY`, in which case it holds `hash16` of the declared field — §2.3.1.)
- **`PULL`.** Graph traversal exposed as a language primitive: range scan over a co-located subtree, not a JOIN engine.

### Tier 2 — Query + correctness

- **xyTalk pipeline-first.** `FIND ... | PULL | AGGREGATE` composes operations explicitly. No implicit joins, no query optimiser rewriting intent.
- **Anchors.** `UNIQUE` constraint + dictionary O(1) lookup integrated as one primitive. `FIND` resolves anchor → gravity → scan in that order.
- **Durability D1.** Every write the engine acknowledges in `Durable` mode is in an SSTable, not just buffered in the WAL. Comment-asserted invariants are not enough; D1 is the cluster-wide test discipline (§9).
- **Schemaless.** Records carry dynamic fields. Schema lives in code or convention, not in DDL.

### Tier 3 — Self-tuning (what makes "no maintenance" defensible)

- **Ghosts.** Materialised secondary indexes with automatic projection — only fields needed by the matching query shape are stored.
- **Auto-ghost.** `ScanTelemetry` observes scan patterns and promotes hot ones (default: 5 hits within a 10-min window, average latency ≥ 20 ms). No DBA, no `CREATE INDEX`.
- **Anchor auto-routing.** `FIND` chooses anchor → gravity → scan based on what is populated, not on a hint. The query text does not change between modes.

### Tier 4 — Operational

- **HDD-friendly LSM.** Storage profiles tune block size (32 KB SSD / 64 KB HDD), bloom bits per key (10 / 14), and compression (LZ4 hot, zstd cold). Bulk-mode load converts random writes into sequential ones.
- **Single binary, no external dependencies.** No ZooKeeper, no etcd, no JVM. One process, one TCP port, one volume.
- **`/stats` observability built-in.** JSON endpoint over the same port; reap-cycle log emits the same data to stderr. No Grafana add-on required to see what the engine is doing.

---

## 1. Stack

```
┌─────────────────────────────────────────────────────────────┐
│ Client: xyzdb-cli (rustyline REPL) │ TCP sockets │ SDKs TBD  │
└─────────────────────────────────────────────────────────────┘
                       │
                       │ V1 text / V2 formatted / V3 binary bulk
                       ▼
┌─────────────────────────────────────────────────────────────┐
│ xyzdb-server (tokio, :2505)                                  │
│   Connection handler → parse → Engine::execute               │
│   /stats JSON snapshot endpoint on the same port             │
└─────────────────────────────────────────────────────────────┘
                       │
                       ▼
┌─────────────────────────────────────────────────────────────┐
│ xyzdb-engine                                                 │
│   • Engine: lifecycle, execute, ops dispatch                 │
│   • GhostLobeManager: Permanent/Ephemeral/Promoted lifecycle │
│   • GhostRouter: Primary / Ghost / GhostPreComputed routing  │
│   • ScanTelemetry: auto-ghost trigger                        │
│   • AnchorRegistry: UNIQUE + dictionary O(1)                 │
│   • Planner: statement → ops                                 │
│   • ops/ : scan, put, find, pull, aggregate, group, delete   │
│   • RecordCache, throttle, field_dict                           │
└─────────────────────────────────────────────────────────────┘
                       │
                       ▼
┌─────────────────────────────────────────────────────────────┐
│ turba-engine — LSM storage                                   │
│   Trees: spatial / identity / dictionary / ghosts / vectors  │
│   WAL group commit, MemTable, SSTables with                  │
│   block + bloom + zone map, leveled compaction with          │
│   dual-criterion scheduling, ArcSwap SuperVersion, MVCC,     │
│   jemalloc allocator (Linux)                                 │
└─────────────────────────────────────────────────────────────┘
                       │
                       ▼
                Filesystem (APFS / ext4, SSD or HDD)
```

| crate | LOC | role |
|---|---:|---|
| `xyzdb-core` | 1 409 | types (LID, Value, FilterOp, Literal, Record, `PaginatedRecords` result variant) |
| `xytalk-parser` | 1 964 | xyTalk → AST (nom-based; renamed from `xyzdb-parser` in v0.2.5.1) |
| `xyzdb-engine` (dir `crates/engine/`) | 10 576 | ghost lifecycle + router + ops + telemetry + anchors + cursor. The former god-files are now directory modules: `engine/{boot,dispatch,ghosts,gravity,lifecycle,maintenance,stats,vectors,verbs,mod}.rs` and `ghost/{build,lifecycle,notify,persist,read,mod}.rs`. Non-contract modules are `pub(crate)`; the public contract surface is `engine` / `keyspaces` / `ops` / `throttle` / `stats` |
| `xyzdb-server` | 1 123 | TCP `:2505` (V1 text + V2 formatted + V3 binary bulk + `/stats`) |
| `xyzdb-cli` | 120 | rustyline REPL + `xyzdb-cli admin <verb>` subcommand |
| `xyzdb-bench` | 941 | internal bench harness (legacy) |
| `turba-engine` | 6 068 | LSM storage (standalone crate, 5 keyspaces) |

LOC are point-in-time. Run `tokei` against the workspace for an authoritative count.

---

## 2. Data model

### 2.1 Heterogeneous lobes

A lobe is a logical bucket for records that share a domain. Unlike a relational table (one type per table), a lobe admits multiple `_type` values. The `creditos` lobe in the fintech reference dataset, for instance, contains the entire credit lifecycle for a client:

```
Relational shape:                   xyzDB shape:
  table credits (FK clients)          lobe creditos:
  table installments (FK credits)       _type=Credit
  table payments (FK credits)           _type=Installment
  table collections (FK credits)        _type=Payment
  table coll_actions (FK collections)   _type=Collection
                                        _type=CollectionAction
                                      all sharing gravity=rfc
```

A SQL query asking "all credit-related history for client X" is a 4-table JOIN. The xyzDB query is a single `PULL depth=N` over a contiguous range — no JOIN engine, no hash table, no sort merge. The cost paid up front is the discipline of choosing what counts as a domain; the cost saved at read time is every cross-table seek.

**Heterogeneous lobes are the radical idea.** Gravity is the mechanism that delivers it physically; PULL is the language primitive that exposes it.

### 2.2 Semantic gravity

Fields prefixed with `*` in a `PUT` are gravity fields. Their values are hashed into the record's `gravity_hash`. Records with the same gravity_hash cluster into the same block range on the `spatial` keyspace.

```text
PUT {*rfc: "AAA", _type: "Credit", monto: 10000} IN "creditos"
PUT {*rfc: "AAA", _type: "Installment", monto: 1000} IN "creditos"
-- Both records share gravity_hash. PULL reads them in one block range.
```

Gravity is set per-record at write time. The V3 binary bulk insert protocol takes a `gravity_fields: &[String]` parameter per batch, so a bulk loader can co-locate large datasets without per-record syntax overhead.

### 2.3 Z-order co-location

A record's physical key is `SpatialKey { lobe_id: u16, gravity_hash: u64 (48 bits used), sat: u16, type_id: u16, timestamp_norm: u32 (21 bits used), seq: u64 }` ([`xyzdb-core/src/key.rs`](../crates/core/src/key.rs)). On disk the key serialises to **24 bytes big-endian** (0.9.0 layout, `MANIFEST_VERSION = 5`) as `[lobe_id u16 = 2 bytes][gravity_hash u48 = 6 bytes][sat u16 = 2 bytes][z_order_2d(type_id, timestamp_norm) u48 = 6 bytes][seq u64 = 8 bytes]`. The 48-bit `gravity_hash` is the primary co-location key — records sharing it share the leading **8 bytes** (`lobe_id` + `gravity_hash`, `GRAVITY_PREFIX_SIZE = 8`) of every serialised SpatialKey and therefore sort contiguously. The `sat` field is the **satellite / sub-gravity axis**, sitting between the gravity prefix and the Z-Order component. It is 0 unless the lobe declares `SATELLITE BY <field>` (§2.3.1); with every record at satellite 0 the physical ordering is byte-for-byte identical to the old 22-byte layout, and a lobe that declares the axis instead groups records by `hash16(field)` **within** its gravity bucket — the emission order of a parent-bucket sweep becomes `sat → z_order → seq`, which is why declaring the axis is a per-lobe opt-in on an empty lobe rather than a global change. The Z-Order 2D component (42 useful bits today — 21-bit `type_id` interleaved with 21-bit `timestamp_norm`, inside a 48-bit field whose slack is reserved for future widening) gives intra-bucket locality across `(type, time)`. The trailing `seq` u64 is a monotonic tiebreaker so two records with identical entity, type, and normalised timestamp don't collide on the spatial keyspace. The on-disk format is **incompatible with the pre-0.9 22-byte layout** (and with the pre-v0.6 18-byte / 21-bit layout before it); engines on `MANIFEST_VERSION = 5` reject earlier data dirs on open — there is no in-place migration, so recreate the dataset from source by re-ingestion (D-MIGRATION).

The bit-level packing is an encoding detail of `to_bytes` / `from_bytes`; the public API operates on the 5-field struct. Block-level zone maps and bloom filters filter further within a co-located range, so even gravity buckets larger than a block stay efficient.

### 2.4 Identifiers and references

- **LID** (Local ID) — 128-bit, encodes node, lobe, timestamp, and sequence. Globally unique, monotonic, sortable.
- **Anchor** — a UNIQUE constraint on a lobe field. Anchor values are stored in the `dictionary` keyspace mapping value → LID. See §4.6.
- **LINK** — a parent-child reference. `PUT {...} LINK TO "parents" WHERE rfc = "X"` writes the child with a `_link_<rel>` field encoding the parent's LID, enabling `PULL` traversal across non-co-located records.

### 2.5 Query model

Four read shapes, each routed differently:

- **`FIND`** — anchor → gravity → scan. The first populated path wins. With an applied anchor, point lookup is O(1) on the `dictionary` keyspace.
- **`PULL`** — fetch a seed record plus its co-located subtree up to `depth=N`. Range scan on `spatial` plus optional `_link_*` traversal via `identity`.
- **`SCAN`** — lobe iteration with `WHERE`. Routes to one of three sources (§4.4): `Primary`, `Ghost(name)`, or `GhostPreComputed(name)`. Optionally paginated via opaque `CURSOR "<token>"` (postcard + URL-safe base64; filter-checksum bound — v0.2.5.1).
- **`AGGREGATE` / `GROUP BY`** — streaming when sourced from `Primary` or `Ghost`; **zero-scan** when a `GhostPreComputed` exists whose grouping and metric set *cover* the query (same group-by, and it precomputes every requested metric — op, field, per-metric filter, and alias). A query asking a metric the ghost lacks (or with a different per-metric filter) falls back to a correct primary scan rather than returning another metric's value.

---

## 3. turba-engine — LSM storage

The storage layer is its own crate (`turba-engine`), used exclusively by `xyzdb-engine` today but carrying no xyzDB-specific types. The separation lets us treat the two as independent contracts.

### 3.1 Five parallel keyspaces

Each `TurbaEngine` opens five `Tree` instances, each with its own WAL, memtable, SSTable hierarchy, and compaction thread:

| keyspace | content | key shape |
|---|---|---|
### 2.3.1 Sub-gravity: the satellite axis

Gravity decides *which bucket* a record lands in. The satellite axis decides *how one bucket is sub-divided*. `SATELLITE BY <field> IN "lobe"` names a single field whose value maps through `hash16` to the `sat` slot, so records sharing that value group together **inside** their gravity bucket.

**Read path.** A query pinning both the gravity field and the satellite field resolves a sub-range (`prefix_for_satellite`: bytes 0..10 fixed, tail saturated) instead of the whole bucket. This bounds `SCAN`, `AGGREGATE`, `GROUP BY`, and the fused `NEAREST` of §3.7 — for `NEAREST` the candidate set *is* the satellite, so scoring inside it yields the exact top-k of the filtered set rather than an approximation.

**Correctness does not depend on the hash.** `hash16` collides by design (a `u16` axis), so the read path always re-applies the field predicate as an anti-collision residual. The bounded scan is therefore a **pure optimisation**: same rows, same order as the parent-bucket scan, gated row-for-row.

**Constraints, and why.** One axis per lobe (a single `u16` cannot carry two fields; a two-level split is deferred). Declared on an **empty** lobe: declaring it over live records would strand them at satellite 0 where a bounded query cannot see them, and re-packing existing data is a later path. Leaving is free — the parent sweep covers every satellite, so retracting the axis costs speed, never data or exactness. Records missing the field share satellite 0, so the axis only pays when the field is near-universal in the lobe.

**Emission order is a declared consequence.** A parent-bucket sweep emits `sat → z_order → seq` once the axis is live, so surfaces that read "the first N emitted" see a different N. This is why the axis is opt-in per lobe rather than a global format change — `docs/xytalk-spec.md` §2.2.2 carries the user-facing rules, including that `SET` re-places a record whose satellite field changed while `ON CONFLICT UPDATE` does not.

| `spatial` | serialised records, one per write | `SpatialKey` (24 bytes) |
| `identity` | pre-resolved link traversals (accelerates PULL) | `LID` (16 bytes) |
| `dictionary` | anchor values, field-name dictionary, ghost metadata, LobeRegistry, pinned fields, gravity-field registry, vector-field registry, lightweight-ghost group rollups | tagged keys under reserved `[0xFF,0xF7..0xFE]` prefixes (enumerated and collision-checked in `reserved_keys.rs`) |
| `ghosts` | materialised secondary indexes | `(ghost_id: u16, rank: N)` |
| `vectors` | per-record searchable f32 vector (V5 column), one per record with a declared `VECTOR` field | `SpatialKey` (24 bytes — same key as the record) |

All five run leveled compaction. `config.rs::StorageProfile::{Ssd, Hdd}` tunes per-keyspace block size, bloom bits, and compression: zstd on cold levels, LZ4 on hot — **except the `vectors` keyspace, which uses `None` (no compression) at every level on both SSD and HDD** (f32 embeddings are effectively incompressible, so the CPU cost buys nothing and would only slow the `NEAREST` scan path). The `vectors` keyspace (`KS_VECTORS = 4`) is the 5th LSM tree, added in v0.8.8 — see §3.7.

### 3.2 Durability

The CLI flag `--durability {durable, batched, async}` selects an engine-level `DurabilityMode` ([`engine/mod.rs:58`](../crates/engine/src/engine/mod.rs#L58)) which maps to one of two underlying journal `PersistMode` variants ([`crates/turba-engine/src/journal/writer.rs:10-16`](../crates/turba-engine/src/journal/writer.rs#L10)) plus an optional periodic flush:

| CLI value | DurabilityMode | journal PersistMode | Periodic flush |
|---|---|---|---|
| `durable` (default) | `Durable` | `SyncData` (fsync per group-commit batch) | n/a |
| `batched` | `Batched` | `Buffer` | server spawns a `tokio::time::interval` task that calls `engine.persist_journal()` every `--batch-interval` ms (default 100 ms) |
| `async` | `Async` | `Buffer` | none — OS-scheduled writeback only |

Production runs use `durable`. `batched` trades a bounded write window of last data against ~10× HDD throughput; `async` is for bulk-load only and is the only mode that may lose more than `--batch-interval` ms of acknowledged writes on crash.

**`durable` mode is group-commit.** A writer:

1. Appends its batch to the WAL buffer.
2. Increments `pending_epoch` and captures its local `epoch`.
3. **Blocks on the condition variable until `synced_epoch >= epoch`.**

A dedicated sync thread (`turba-wal-sync`) fsyncs the WAL on a 1 ms cadence and advances `synced_epoch` **only on `journal.sync()` `Ok`**. Failed `try_lock` or fsync `Err` leaves the sentinel in place for the next iteration. This is the post-Finding-9 contract: the writer returns `Ok` only when its specific batch has been fsynced, not on a 5 ms timeout.

Throughput under `durable`: ~10 K records/s on SSD, ~10 K on HDD (after the v0.2.1 atomic-publish + v0.2.2 jemalloc fixes).

**Segment-based WAL.** The WAL is not one growing file. Writes append to an active segment (`journal.wal`); when it crosses `segment_max_bytes` (default 64 MiB, `DEFAULT_SEGMENT_MAX_BYTES`) it is renamed to an archived segment `journal.<max_seqno>.wal` and a fresh active file starts (`JournalWriter::maybe_roll`). Production builds spawn a `turba-wal-pruner` thread **by default** that, on a ~1 s cadence, calls `JournalWriter::prune(watermark)` to delete only the archived segments whose every entry is already manifest-durable — i.e. `max_seqno ≤ min(manifest_durable_seqno)` across all trees (`crates/turba-engine/src/engine.rs`, `wal_prune_watermark`). Prune is lossless and delete-only: it never touches the active segment or a not-yet-durable tail, so a concurrent writer's WAL entries always survive a crash. If a lagging keyspace pins the watermark and the total WAL crosses `wal_max` (a cgroup-limit-derived threshold, `TURBA_WAL_MAX_BYTES` overrides for tuning/tests), the pruner forces a flush-only checkpoint (`checkpoint_flush_and_prune`) — flush every tree, persist manifests, then prune — rather than a `rotate`. `JournalWriter::rotate()`, which truncates the entire WAL to zero, is the exceptional path used only by the quiescent `major_compact` / `execute_compact` (§3.4), never by steady-state reclamation. It is no longer called blindly: the engine wrapper `TurbaEngine::rotate_journal()` now **verifies a runtime precondition** before truncating — if any WAL-backed keyspace (`spatial`, `identity`, `dictionary`, `vectors`) still holds acknowledged writes not yet flushed to SSTables, it **refuses with `Error::WalRotatePrecondition`** rather than dropping them (§9).

The full state machine for every WAL transition — the BULKMODE `persist_manifest` skip, the `flushed_seqno` advancement nuance, and the crash-recovery routes — lives in `docs/wal-state-machine.md`.

### 3.3 SSTable format

Each SSTable on disk:

```
[data blocks] [index block] [bloom filter] [meta block] [footer (36 B)]
```

- **Data blocks** — each block carries a **34-byte header** (magic `XYZB`, block type, compression type, a 16-byte **XXH3-128 checksum over the compressed bytes**, compressed + decompressed lengths, and an XXH3-32 header checksum) followed by the compressed payload. Inside, entries are prefix-truncated against the previous key with restart points every 16 entries; the decompressed length is cross-checked on read. 32 KB target (SSD) / 64 KB (HDD).
- **Index block** — one entry per data block, pointing to its byte range. Keys inside the index use u32 length prefixes (post v0.2.1 Finding 4).
- **Bloom** — 10 bits/key (SSD), 14 (HDD). Skipped for `ghosts` keyspace.
- **Meta** — tagged fields (table_id, item_count, seqno range, zone maps, …). Variable-length fields use u32 lengths; fixed-size scalars still u16.
- **Footer** — v2 (`XZT2`), final **36 bytes**: magic + 3 offsets + an xxh3_64 checksum over the 28-byte head, so a corrupted offset is caught at open rather than silently mis-locating a block. The legacy 28-byte v1 footer (`XYZT`, no checksum) is still read for pre-0.8 SSTables until they are recompacted; new tables always write v2.

**Atomic publish invariant.** Writer emits to `{path}.sst.tmp`, calls `sync_all`, then `rename(2)`. The final path never contains partial bytes. Crash debris (`*.sst.tmp` files) is swept on `Tree::open`.

### 3.4 Compaction

Each tree has a dedicated `turba-compact-{tree_id}` thread. L0 → L1 is triggered by L0 table count. L1+ uses **dual-criterion overflow** (`bytes_ratio.max(count_ratio)`), plus a per-level cap (`max_tables_per_level = 20`). The dual criterion catches the many-tiny-tables pathology that bytes-only scheduling missed: 59 L1 tables × ~1 MB stays under a 64 MB byte target but blows the count. An L0 emergency cap (`L0_EMERGENCY_RATIO = 3.0`) prevents L0 starvation when later levels never overflow.

**Two tree identifiers coexist; neither is renamed.** `tree_id: u64` is a runtime monotonic counter assigned by `NEXT_TREE_ID.fetch_add` in `crates/turba-engine/src/tree/mod.rs` — used for compaction-thread naming, log scoping, and debug introspection only. `keyspace_id: u8` is the stable 0..4 constant identifying the five engine keyspaces (`spatial`, `identity`, `dictionary`, `ghosts`, `vectors`) and appears in the on-disk SpatialKey + WAL framing. They are independent: `tree_id` changes across reopens, `keyspace_id` is part of the storage format. Code paths that reference one never substitute the other.

Compacted inputs are **deleted directly by ID** after `persist_manifest`, not via an orphan-scan sweep over the data directory. POSIX unlink is safe with concurrent readers (the inode lives until the last FD closes); the orphan-scan path is preserved as a final sweep in `major_compact` for crash-recovery stragglers only.

**WAL reclamation: `turba-wal-pruner` by default, `rotate` only on compaction.** Production builds spawn a `turba-wal-pruner` thread **by default** (the safe successor to the Finding-10 janitor). It auto-bounds the WAL by calling segment `prune()` on the **manifest-durable watermark** — `min(manifest_durable_seqno)` across all trees, **never `flushed_seqno`** (the code explicitly forbids `flushed_seqno` for prune — the BULKMODE trap, §3.2 and `wal-state-machine.md` §6) — deleting only fully-durable *archived* segments and never the active tail. On top of that, when the WAL exceeds `wal_max` and a lagging keyspace pins the watermark, the pruner runs a flush-and-prune checkpoint. Full-WAL `rotate()` is now the exceptional path: only `Engine::major_compact` and `execute_compact` rotate, both sealing + flushing **every WAL-backed keyspace (`spatial`, `identity`, `dictionary`, `vectors`)** first (Finding 8 paths A and B), only when writers are quiescent, and both route through the precondition-checked `rotate_journal()` (§3.2). Sealing `vectors` alongside the rest closes a **compact-skips-vectors durability bug**: a hoisted searchable vector could be lost on a `COMPACT`-then-crash sequence because the `vectors` keyspace was not flushed before the WAL truncation. The old Finding-10 janitor survives under `#[cfg(feature = "durability-test-hooks")]` so the regression test can exercise the pre-fix scenario without reverting production code.

**Health signal.** Three separate counters per tree:

- `compact_ok` — bg compactions completed.
- `major_ok` — explicit `major_compact` cycles (post Finding 7 bug B1, separate from `compact_ok`).
- `compact_err` — failed compact cycles. Zero under healthy load.

### 3.5 ArcSwap SuperVersion

Readers get a consistent snapshot via a single `ArcSwap<SuperVersion>` load. Flush and compaction publish new SuperVersions atomically; readers holding the old version see the old state until the last reference drops. No locks on the read path; MVCC sequences prevent reading uncommitted writes.

### 3.6 Memory model

**One budget knob.** Memory is governed by a single flag, `--memory-budget-mb`. The block cache size is **derived**, not configured directly: `cache = budget / 4`, clamped to `[32 MiB, 2 GiB]` (the 2 GiB ceiling caps the cache even on very large budgets). The budget itself resolves by precedence: explicit `--memory-budget-mb` first, else the **cgroup** memory limit on Linux (the cgroup limit *only* — never physical RAM, so a container's budget tracks its quota rather than the host), else a 1 GiB default. The older `--cache-size` flag is deprecated and hidden; use `--memory-budget-mb`.

**The budget also governs ingest.** The summed memtable footprint (active + sealed, across all five keyspaces — a PUT fans out to several) is bounded by a derived ceiling: `35 % of the budget`, clamped to `[24 MiB, 264 MiB]`, where the 264 MiB cap is exactly the pre-budget worst case, so any budget ≥ ~755 MiB behaves byte-for-byte as before (downward-only scaling). Each keyspace's memtable seal size is scaled by the same factor (floored at 2 MiB), which also advances the flush trigger, and the writer **stalls** — waiting for background flush to drain to half the ceiling — when the global sum reaches it, escalating to `Overloaded` only if flush makes no progress for 30 s (disk full). The upshot: a tight container (e.g. 256 MiB) bounds its own build instead of OOM-ing, trading load speed for fitting the envelope. Bulk mode (`BULKMODE`) intentionally bypasses this backpressure.

`xyzdb-server` uses **jemalloc** as `#[global_allocator]` on Linux (`tikv-jemallocator`, features `background_threads` + `unprefixed_malloc_on_supported_platforms`). The default glibc `malloc` arena retention pattern adds 15-30 % to RSS on long-running compaction workloads; jemalloc decay returns pages to the OS during read-dominated idle windows. Empirical impact at T6 (2 CPU / 8 GB) SSD Scale 1.0: RAM peak −29 %, COMPACT time −52 %, Q9 P50 −61 %.

`TableHandle` does not own its `SSTableMeta`; it delegates via `reader.meta()`. Each handle would otherwise carry a per-handle clone of `zone_maps: Vec<u8>` (~2 MB per 64 MB SSTable), which compounds across levels under sustained load.

The reap-cycle log (every 60 s by default) emits VmRSS, cgroup `memory.stat` (`anon`, `file`, `active_file`, `inactive_file`), per-tree `mem_active` and sealed-memtable bytes, and per-keyspace `compact_ok` / `major_ok` / `compact_err`. A warning fires when `VmRSS * 100 ≥ limit_mb * 85` so the budget wall is observable before OOM.

### 3.7 Vector subsystem (v0.8.8)

A lobe can declare one **searchable embedding field** with `VECTOR <field> IN "<lobe>"` (the vector-field registry slot in `dictionary`, `vector_spec.rs`). This is a foundational axis orthogonal to gravity: **gravity decides placement** (which bucket a record lands in), **the vector decides what `NEAREST` sweeps**. It is *not* an index — there is no ANN/HNSW/IVF anywhere. `NEAREST` is an **EXACT brute-force** cosine/dot/l2 scan bounded to a gravity bucket; the subsystem only makes that exact scan cheap to materialise.

**Record-format evolution (`xyzdb-core/src/record.rs`).** The first blob byte names the record layout. The engine emits only **V1, V2, and V5** today; **V3 and V4 are retired record formats** (superseded by the split V5 layout). Each blob still reads with the decoder its version byte selects:

| format | byte | layout |
|---|---|---|
| V1 | `0x01` | **Emitted.** field names as strings (ghosts stay V1) |
| V2 | `0x02` | **Emitted.** field IDs (u16) instead of strings |
| V3 | `0x03` | **Retired record format** (no longer emitted). Historically: V2 + the searchable vector **hoisted** out of `fields` to a front-of-blob prefix, so a `NEAREST` scan read it without a full deserialize. A record with no declared vector serialised `vec: None` (content-identical to V2). Superseded by V5 |
| V4 | `0x04` | **Retired as a record format**, but the `0x04` byte lives on as the **vector-column marker**. Historically: V3 + the vector's stored squared norm `‖v‖²` in the prefix, enabling a Cauchy–Schwarz early-abort in cosine. Today the byte tags a V5 column value (a V4-shaped mini-blob — see V5) |
| V5 | `0x05` | **Emitted.** Split layout — the searchable vector lives in the `vectors` keyspace, **not** the record blob (the blob is V2-shaped with the searchable vector excluded). The paired column entry is a `0x04`-marked, V4-shaped mini-blob, byte-identical to an old inline V4 prefix, so it scores the same |

**Fused `Scan`+`Nearest` (`xyzdb-engine/src/ops/nearest.rs`, `execute_scan_nearest`).** For a `NEAREST` over a gravity bucket with no residual filter, the fast path ranges the `vectors` column (V5) / hoisted prefix (V3/V4) over the bucket key range, scores each candidate **zero-copy** (no full-record materialisation per candidate), keeps a bounded top-k min-heap, and fully hydrates only the surviving top-k. The result is **bit-identical** to the unfused reference path (`scan → execute_nearest`), checked by `tests/scan_nearest_fused.rs`. The pruned-cosine core (`xyzdb-core/src/distance.rs`, `cosine_pruned`) uses a Cauchy–Schwarz upper bound (`dot_partial + ‖a_tail‖·‖b_tail‖`) to abort a candidate that provably cannot reach the current k-th best; survivors score bit-identically to the unpruned `similarity_indexed`.

**RAM / disk trade-off.** Moving the vector into the V5 column means `NEAREST` ranking reads ~1 KB column entries instead of ~4 KB record blobs, cutting the per-query transient from ~32 MiB to ~8 MiB at a ~+2.2% on-disk cost (the column duplicates the vector out of the blob). This is what lets a 128 MB-RAM agent container stay RAM-resident under concurrent vector queries.

**The engine never embeds.** `NEAREST` always works on a vector the caller supplies — a bound `$param` (preferred), an inline list literal, or `REF "id"` (the embedding of another record in the bucket). The corpus and the query must be embedded with the *same* model; choosing and running that model is an application/dock concern the engine stays out of. No network call ever happens on the query or write path — the engine is purely agnostic.

---

## 4. xyzdb-engine — query layer

### 4.1 Engine struct

`Engine` is the single aggregate that owns the storage, the ghost machinery, the telemetry store, the anchor registry, and the field dictionary. Its lifecycle:

1. `Engine::open(path)` — opens `TurbaEngine`, replays WAL, loads ghost metadata, starts the zone-map builder.
2. `Engine::into_arc()` — wraps in `Arc`, spawns the background ghost-TTL reaper, returns `Arc<Engine>`.
3. Drop — propagates through `Arc` → last-ref-release → reaper exits (via `weak_self.upgrade() == None`) → `TurbaEngine::shutdown` seals and flushes **all five keyspaces** (`spatial`, `identity`, `dictionary`, `ghosts`, `vectors`) to SSTables before returning, so a graceful shutdown leaves no acknowledged write in WAL-only state.

No explicit `shutdown()` API on `Engine`. Drop is the single lifecycle contract.

### 4.2 Ops

`Engine::execute(statement)` dispatches to the right `ops/*` module:

| op | file | one-line |
|---|---|---|
| PUT | `ops/put.rs` | gravity hash, anchor check, single write or batch |
| FIND | `ops/find.rs` | anchor → gravity → scan resolution |
| PULL | `ops/pull.rs` | subtree reconstruction via identity range scan |
| SCAN | `ops/scan.rs` | router decision; primary / ghost / ghost-precomputed branches |
| AGGREGATE / GROUP BY | `ops/aggregate.rs` + scan dispatch | streaming or PreComputed-ghost short-circuit |
| SET | `ops/set.rs` | partial field update |
| DELETE | `ops/delete.rs` | tombstone of the matched record(s) (no cascade) |

### 4.3 Ghost system

Ghosts are materialised secondary indexes stored in the `ghosts` keyspace. Every ghost has:

- `filters: Vec<Filter>` — AND-flat selector applied to the source lobe.
- Optional `order_by`, `projection`, `aggregate_specs`, `group_fields`.
- `ghost_id: u16` — prefix in the `ghosts` keyspace.
- `filter_desc: String` — serialised `FilterExpr` used for OR / complex-expression routing.

Ghosts created with `GROUP BY <field> AGGREGATE func()` clauses persist per-group accumulator state (`group_summaries`) in their meta. This is the data structure `ScanSource::GhostPreComputed` reads — see §4.4.

**Lightweight ghosts (on-disk group rollups).** Keeping `group_summaries` in RAM is fine for a low-cardinality grouping (by `empresa_id`: thousands of groups, a few MB) but unbounded for a high-cardinality one (by `rfc`: one group per client → millions, gigabytes at scale). When a ghost's group count crosses `group_spill_limit()` (default 64k; `XYZ_GHOST_SUMMARIES_MAX_GROUPS` overrides for tests) the engine spills it to disk: **exactly one canonical rollup entry per group** lives in the `dictionary` keyspace (`[0xFF,0xF9][ghost_id][group_key]`) and the in-RAM map is cleared and stays empty — that empty map *is* the "lightweight" discriminator (no persisted flag; pre-spill ghosts load unchanged). A fully-pinned group read is then a single bloom-backed exact `get`; incremental maintenance is a get-merge-put against the canonical entry (every aggregate here — count, sum, min/max, avg — is exactly mergeable). Low-cardinality ghosts are untouched. RAM becomes the block cache, not `O(groups)`.

**Three lifecycle classes:**

- **Permanent** — created by `CREATE GHOST`. No TTL. Survives reboots.
- **Ephemeral** — auto-created by `ScanTelemetry` (§4.5). 24 h TTL. **Max 20 per lobe** (active runtime cap, [`ghost_pool.rs:251-259`](../crates/engine/src/ghost_pool.rs#L251)). LRU-evicted at the cap.
- **Promoted** — Ephemeral that accumulated ≥ 7 daily access bits within 30 days. Renamed in place with no data re-scan; 30 d TTL. **Max 5 per lobe** ([`engine/ghosts.rs:139`](../crates/engine/src/engine/ghosts.rs#L139), enforced by `enforce_ghost_type_limit`).

> The pre-Finding 5 cap of "10 Ephemeral per lobe" was a stale rustdoc string in the old monolithic `ghost.rs`. The `ghost.rs` → `ghost/{build,lifecycle,notify,persist,read}.rs` split reconciled it: [`ghost/lifecycle.rs:87`](../crates/engine/src/ghost/lifecycle.rs#L87) now documents the correct "20 Ephemeral, 5 Promoted" caps. The runtime cap is **20**, set in `ghost_pool.rs` and enforced via `enforce_ghost_type_limit`. arch.md is the source of truth.

**Writes notify ghosts.** Every `PUT` invokes `ghost_manager.notify_write(record)` — each matching ghost appends/updates an entry, keeping the index live. Ghosts with `group_summaries` (or on-disk rollups) update their accumulators incrementally; no `REFRESH` step is required in steady state. **Exception — `BULKMODE`:** while bulk load is active, aggregate maintenance is deferred (a per-record rollup read-modify-write would collapse ingest throughput); the post-load `REFRESH GHOST` rebuilds aggregate ghosts from the loaded data. Covering-index entry inserts continue during bulk.

### 4.4 Router

`GhostRouter::plan_scan(filters, filter_desc, order_by, has_aggregates, group_fields, has_limit)` returns one of three `ScanSource` variants:

- `Primary` — scan the spatial keyspace.
- `Ghost(name)` — iterate the named ghost's keyspace and hydrate from spatial.
- `GhostPreComputed(name)` — serve aggregates directly from `group_summaries`, **zero scan**.

**PreComputed precondition (post Finding 11).** A ghost is eligible for `GhostPreComputed` routing only when every query predicate is either:

- a ghost-constant filter (already in `meta.filter_fields`), or
- an `Eq` predicate on a field in `meta.group_fields`.

Any other predicate disqualifies the ghost from PreComputed; the router falls back to `Primary` (or to `Ghost` if a non-PreComputed match exists). This guarantees the pre-computed group entries returned to the caller satisfy every `WHERE` clause in the query, not just those covered by the ghost definition.

**Operator support today**: PreComputed honours `=` only on group keys. Non-Eq predicates on group keys (`!=`, `<`, `<=`, `>`, `>=`, `IN`) disqualify the ghost from PreComputed. Range support on group keys is v0.3 scope.

**Two matching passes** still run in sequence:

1. **`filter_desc` equality** — direct hit if a ghost was created for exactly this expression shape (OR / NOT / complex).
2. **Flat-filter tuple match** — `(field, op, value)` subset match; falls back to the best ghost whose filters are a subset of the query.

**Transparent fallback (Finding 1).** If the ghost the router selected has been evicted between `plan_scan` and `read_topn`, the scan path catches `Err(XyzError::GhostNotFound)`, unregisters the stale entry, and re-executes against `Primary`. Invisible to the caller.

### 4.5 Scan telemetry

`ScanTelemetryRegistry` tracks every scan by `filter_desc`. When a pattern accumulates `min_hits` within a 10-minute sliding window AND its rolling average latency ≥ `min_latency_ms`, the next scan returns an `AutoGhostCandidate`; the engine spawns a background worker to materialise an Ephemeral ghost.

Defaults (`scan_telemetry.rs`):

- `DEFAULT_MIN_HITS = 5`
- `DEFAULT_MIN_LATENCY_MS = 20.0`

Operators tune via the server CLI:

- `--auto-ghost-min-hits N`
- `--auto-ghost-min-latency-ms F`

Setting `--auto-ghost-min-latency-ms 1e9` effectively disables auto-ghost creation (existing manual ghosts and Promoted survivors continue to work).

**Note on the "painful" threshold concept.** The 20 ms gate is the active operational threshold. The doc-comment cluster at [`scan_telemetry.rs:5-24`](../crates/engine/src/scan_telemetry.rs#L5) discusses 20 ms as the line below which ghosts cost more in `notify_write` overhead than they save on read; a separate "painful = ≥ 100 ms" concept exists as a future env override (`XYZDB_TELEMETRY_PAINFUL_MS`) that would lower the gate for empirical auto-promotion validation runs. The 100 ms threshold is **not** yet a runtime gate; only the 20 ms default + CLI override exist at v0.3.4.

### 4.6 Anchors

`ANCHOR <field> UNIQUE IN <lobe>` is **declarative**: it registers the uniqueness constraint and creates an empty entry in the dictionary keyspace. Subsequent inserts populate the dictionary on write.

`AUTOANCHOR APPLY <field> IN <lobe>` is **operational**: it iterates existing primary records and indexes them into the dictionary. This is the entry point that retroactively populates the anchor for a lobe loaded before the constraint was declared (e.g. after a bulk import).

**Idempotency contract (post Finding 12).** APPLY succeeds when the anchor was previously declared via `ANCHOR ... UNIQUE IN`. Registration is conditional inside the APPLY handler (`is_anchor` check); the populate step always runs. The declarative `ANCHOR` path retains its original strict semantics: duplicate declarations of the same field still error.

After APPLY, `FIND <lobe> WHERE <field> = X` resolves via the anchor path (O(1) dictionary lookup) instead of falling through to scan + bloom.

---

## 5. xyzdb-server and the wire protocol

Tokio-based TCP server bound to `:2505` (default). Three protocol versions coexist on the same port:

- **V1** (text): `[version=1][length: u32 BE][UTF-8 xyTalk]`. Responses are plaintext tables. REPL default.
- **V2** (formatted): JSON responses.
- **V3** (binary bulk): framed chunked streaming for high-throughput ingest paths. Carries `gravity_fields: &[String]` per batch.

Frame length prefixes are u32. Client disconnects propagate gracefully; per-connection state is minimal.

A side `/stats` HTTP-style endpoint on the same port emits a JSON snapshot of engine internals — see §10.

### 5.1 MCP dispatch model (v0.2.6)

`xyzdb-mcp` is a separate binary that exposes the engine to MCP-compatible clients over JSON-RPC 2.0 framed by line-delimited messages on stdin/stdout. It runs as a subprocess of the agent's host process.

Two source modes, mutually exclusive at the CLI:

- `--embed <PATH>`: the MCP process opens the data dir directly (LSM lock holder). `Engine::open(path).into_arc()` produces an `Arc<Engine>` shared across all tool-call futures via `tokio::task::spawn_blocking` — keeps parking_lot mutexes off the async reactor. Single-process deployment; the canonical single-process subprocess pattern.
- `--connect <HOST:PORT>`: the MCP process is a TCP V2 client of an existing `xyzdb-server`. Per-call connection (no pool); the upstream server JSON is forwarded verbatim into the MCP `CallToolResult.content[0].text` slot. Single MCP per data dir is still the contract; the multi-process shape is for when the data dir is owned by a long-running server.

Tools exposed (Pillar 1-4): `stats`, `query`, `list_lobes`, `describe_lobe`. Resources (Pillar 5): two concrete URIs (`xyzdb://lobes`, `xyzdb://stats`) plus one URI template (`xyzdb://lobes/{name}`). Both surfaces share the same `*_json` helper functions on `XyzdbServer`, so the engine path is identical across tools and resources.

Privacy contract (Pillar 6): default-on redaction means the per-tool-call `tracing::info` event carries `query_hash` (xxh3-64, first 8 hex) + `query_kind` (first verb) instead of statement text; `cursor_present: bool` instead of the cursor token; `records_returned: u64` instead of result content. A `--log-statements` flag adds full statement + cursor logging at TRACE level, gated by a startup guard that rejects `--connect` to non-loopback hosts (loopback recognised: `127.0.0.0/8`, `::1`, `[::1]`, `localhost`). The cross-actor leak guard prevents statements from other actors sharing the same upstream xyzdb-server from landing in the local MCP process's stderr.

Concurrent dispatch: rmcp 1.5 dispatches incoming `tools/call` requests concurrently. Production MCP clients await each response before issuing the next; the per-call `request_id` (UUIDv7) in telemetry pairs requests with completions for out-of-order observation.

Surface contract: [`docs/mcp-integration.md`](mcp-integration.md).

---

## 6. Format versions and migration

On-disk format bytes and the pagination token, bumped on breaking format changes:

- `MANIFEST_VERSION` (turba-engine manifest): **v5** (current, since 0.9.0 — the 24-byte `SpatialKey` with the `sat` satellite axis, §2.3; the axis went live with `SATELLITE BY` and needed no format change, which is why it was reserved while there were no users). v4 (v0.6.0-pre, 22-byte / 48-bit `gravity_hash`) and v1/v2/v3 (≤ v0.5.x, 18-byte / 21-bit) are rejected by current binaries with `Error::IncompatibleFormat`; there is no in-place migration — recreate the data dir from source by re-ingestion (D-MIGRATION).
- `GHOST_META_FORMAT` (xyzdb-engine ghost persistence): **0x03** (current). Bumped when `GhostMeta` gained lifecycle fields (`ghost_type`, `ttl_seconds`, `daily_access_bitmap`, `access_count_total`).
- **Cursor pagination token format** (`CURSOR_FORMAT_V2`): **v2** (current, since 0.9.0). The opaque `SCAN ... CURSOR` token (postcard + URL-safe base64, §2.5) was bumped 1 → 2 because it embeds a `SpatialKey` scan position and that key widened 22 → 24 bytes; **v1 tokens are rejected** (`cursor invalid: unsupported format version`). Unlike the two above this is an in-flight protocol token, not a byte on disk.

Rejection returns a clear operator-facing message explaining *why* and pointing at re-ingestion. There is no in-place migration code; v0.2.x has no production users.

---

## 7. Tests

~1050 unit + integration test functions across the engine, parser, server, and storage crates (v0.7.6 snapshot: `cargo test --workspace` ⇒ 580 passing in the `xyzdb/` workspace + 477 in `turba-engine`; subject to drift). v0.7.6 additions: `gravity_bucket_lifecycle` (FIND-returns-full-bucket + PULL collision filter), `lightweight_ghosts` (build-spill parity, incremental RMW, bulk+refresh, DROP purge), the L1+ `get_at` regression test, the PIN-prefix migration tests, and `reserved_keys::reserved_prefixes_do_not_collide` (build-time prefix-collision guard). Highlights:

- D1 cluster regression tests for Findings 8 / 9 / 10 (durability_proptest) — see §9.
- Read-path correctness tests for Findings 11 (PreComputed WHERE filter) and 12 (`AUTOANCHOR APPLY` idempotency).
- Subprocess-based crash-recovery tests that exercise real SIGKILL paths the in-process `mem::forget` simulation cannot reach.
- `loom` concurrency tests (gated) for the WAL group-commit path.
- Property-based tests on durability and SSTable round-trip.

For the exact figure, run `cargo test --release --no-run 2>&1 | grep "test result"` on the workspace.

### 7.1 Validation suites (operational)

Separate from `cargo test` — under [`tools/validation/src/suites/`](../tools/validation/src/suites/) — there are ten operational suites (`s01_data_load` through `s10_endurance`) that drive a **running** `xyzdb-server` over the V2 wire protocol and assert end-to-end behaviour (data load, read patterns, write stress, mixed workload, connection pool, durability, edge cases, auto-discovery, scale curve, endurance). Suites are scenario-style integration runners, not part of `cargo test`; they require an engine binary and a path. Surfaces flagged WIP at v0.3.4:

- **`s08_autodiscovery`** — tests `SHOW AUTOLINK` and `AUTOLINK APPLY`. The xyTalk parser does not accept the `AUTOLINK` keyword at v0.3.4 (`xytalk-parser` has no `parse_autolink`; only `parse_autoanchor_apply`). The suite's AUTOLINK tests are expected to fail parse-time against a v0.3.4 engine. The scaffolding exists in [`xyzdb-server/src/connection.rs:289`](../crates/server/src/connection.rs#L289) (write classifier) and the Python SDK (`show_autolink`) but has no parser/runtime backing. Cleanup or completion is a v0.5 decision — see [`docs/xytalk-spec.md` §2 AUTOLINK status note](xytalk-spec.md).

Clippy lints in both workspaces:

- `unwrap_used = warn`
- `expect_used = warn`
- `undocumented_unsafe_blocks = deny`
- `missing_safety_doc = deny`

---

## 8. Evolution timeline

Per-version narrative lives in `docs/releases/`. One-line summary per published tag:

- **v0.1.0** — first public; single keyspace, fjall-backed storage.
- **v0.2.0-alpha** — turba-engine LSM (replaces fjall); Phase-1 ghost lifecycle (Permanent / Ephemeral / Promoted); `filter_desc` routing.
- **v0.2.1** — stabilization. Findings 1 (transparent ghost fallback), 4 (SSTable atomic publish + u32 length widening), 5 (Ephemeral cap 10 → 20). 4-run matrix + 8 h smoke.
- **v0.2.2** — T6 RAM budget. Finding 6 cluster (dual-criterion compaction, direct deletion of compacted inputs, jemalloc, `TableHandle::meta()` dedup). RAM peak −29 % at Scale 1.0.
- **v0.2.3** — durability cluster D1 closed. Findings 8 / 9 / 10. `/stats` endpoint. Section 2 audit kicked off.
- **v0.2.3.1** — Section 2 audit closed. Ack-path coverage (`execute_autoanchor_apply`, `persist_pinned`), subprocess crash-recovery tests, `wal-state-machine.md`.
- **v0.2.5.1** — query-language cleanup. SCAN cursor pagination (postcard + URL-safe base64 token, filter checksum binding), default `LIMIT 1000` safety cap on unbounded SCAN with hard ceiling 10000, `WHERE` on standalone `SET`/`DELETE`/`LINK`, INCACHE/OUTCACHE rewritten with nom and documented (spec §2.10 in current numbering, was §2.18 at the time of this release), `xyzdb-cli admin <verb>` subcommand for COMPACT / ANALYZE / BULKMODE / MIGRATE (those statements are deprecated as language-level, retired in v0.3 — spec §2.21 current, was §2.19), and the language renamed from xyzQL to **xyTalk** (semantics, on-disk format, and wire bytes byte-identical).

---

## 9. Durability contract D1

**Statement.** Every caller of `JournalWriter::rotate()` — and every operation that advances a durability sentinel such as `synced_epoch`, `flushed_seqno`, or `last_rotated` — must establish, before the call, the precondition: **"all writes acknowledged to callers are in SSTables, not in active memtables, sealed-but-unflushed memtables, or WAL-only state"**.

Sentinels like `flushed_seqno` partially capture this; they do not fully capture it, because active memtables hold acknowledged writes whose seqnos exceed `flushed_seqno`. Sealing active memtables and flushing them to SSTables is the only operation that reliably establishes the precondition.

A comment asserting the precondition is necessary but not sufficient. Each caller is audited against the documented precondition in code review, and a regression test exercises an adversarial ordering (active memtable holds an unflushed write at the moment of rotate / advance, then crash) for every D1 caller.

**Cluster — three closed Findings, three different mechanisms:**

- **Finding 8** — `Engine::major_compact` (path A) and `execute_compact` (path B) truncated the WAL via `journal.rotate()` on the assumption "all data is in SSTables", but active memtables were not sealed. Closed in v0.2.3 (path A and path B). Empirically validated via Phase 5 Scale 1.0 SSD re-run: 5/5 lobes EXACT post-restart, `mem_active = 0 MB` at SIGTERM.
- **Finding 9** — group-commit writer returned `Ok(seqno)` after `wait_timeout(5 ms)` without verifying its own epoch was synced; sync thread advanced `synced_epoch` even on `try_lock` failure or `j.sync()` `Err`. Closed in v0.2.3 (primary writer-side fix + secondary sync-thread fix + regression test under the `durability-test-hooks` feature flag).
- **Finding 10** — WAL janitor thread rotated every 500 ms when `flushed_seqno` advanced, bypassing the precondition. Closed by disabling the janitor thread in production builds (option b); the janitor remains alive under `#[cfg(feature = "durability-test-hooks")]` so the regression test can exercise the pre-fix scenario.

**0.9.0 hardening — the precondition is now runtime-enforced.** What was previously audit-only (code review + regression test) is now also checked at run time: `TurbaEngine::rotate_journal()` inspects every **WAL-backed keyspace** — `spatial`, `identity`, `dictionary`, and the new `vectors` keyspace (§3.7) — and **refuses to truncate with `Error::WalRotatePrecondition`** if any still holds acknowledged writes above its flushed watermark, instead of silently dropping them. Correspondingly, `execute_compact` and `major_compact` now seal + flush `vectors` alongside the other keyspaces before rotating; adding `vectors` to that set closed a **compact-skips-vectors** bug where a hoisted searchable vector could be lost on a `COMPACT`-then-crash sequence (§3.4). Graceful shutdown likewise seals + flushes all five keyspaces (§4.1).

**Test coverage map** — three D1 callers, three regression tests:

| Caller | Test | File |
|---|---|---|
| `Engine::major_compact` | `finding_8_major_compact_seals_active_before_wal_rotate` | `crates/turba-engine/tests/durability_proptest.rs` |
| `execute_compact` | `finding_8_path_b_execute_compact_seals_active_before_rotate` | `crates/engine/tests/integration.rs` |
| WAL janitor (feature-gated) | `finding_10_wal_janitor_rotate_does_not_lose_active_memtable_writes` | `crates/turba-engine/tests/durability_proptest.rs` |

A subprocess-based harness in `crates/turba-engine/tests/` exercises real SIGKILL paths the `mem::forget` simulation cannot reach. The full state machine for every WAL operation lives in `docs/wal-state-machine.md`.

A fourth D1 caller added in any future commit without a matching test row is a violation of the cluster's response plan.

---

## 10. Observability

`/stats` is an HTTP-style endpoint on the server's TCP port that emits a JSON snapshot of:

- Per-lobe ghost counts (Permanent / Ephemeral / Promoted) and lifecycle states.
- Auto-ghost telemetry — recent patterns, hit counts, rolling latencies, trigger thresholds.
- `compact_ok` / `major_ok` / `compact_err` per keyspace.
- Sync-thread health — `pending_epoch`, `synced_epoch`, last sync time, last sync `Result`.
- Per-tree `mem_active`, `sealed`, `flushed_seqno`, `disk_sst_count`, `version_sum`.
- Process memory: `VmRSS`, cgroup `anon` / `file` / `active_file` / `inactive_file`.

The reap-cycle log (~60 s cadence) emits the same data to stderr in a compact text format, including the 85 % cgroup-limit warning when triggered. Operators can wire either surface into Prometheus / Grafana via standard scraping.

The auto-ghost telemetry section above surfaces the active threshold (`min_latency_ms = 20.0` default, tunable via `--auto-ghost-min-latency-ms`) and the rolling latency per scan pattern. See [§4.5](#45-scan-telemetry) for the full gate specification, including the historical "painful 100 ms" concept that lives as comment-level reference (not as runtime gate at v0.3.4) and the planned `XYZDB_TELEMETRY_PAINFUL_MS` env override for empirical validation runs (caveat C-19, backlog Entry 24).

This surface is what makes the "no maintenance" pillar defensible. Without observable internals, any incident becomes a guess; with them, an operator can distinguish "stuck compactor", "active memtable backlog", "ghost LRU thrashing", and "RAM ceiling approached" from the same log without reaching for new tooling.

---

## 11. Design principles

Non-obvious choices worth stating:

1. **Heterogeneous lobes over normalisation.** A relational schema forces one table per type and recovers the domain through joins. xyzDB co-locates by domain in one lobe and exposes traversal as a primitive (`PULL`). The cost paid up front is the discipline of choosing what a domain is; the cost saved at read time is every cross-table seek. This is the most consequential design choice in the engine; everything else either supports it or is orthogonal.

2. **D1 over comment-asserted invariants.** Every comment of the form *"safe because X"* in the durability path must be convertible to a code-enforced assertion. If it cannot be, either the comment is wrong or the code is incomplete. Findings 8 / 9 / 10 each violated a comment-asserted invariant; the cluster response is the testing discipline that makes the invariants observable.

3. **Drop-driven shutdown.** No `Engine::shutdown()`. The `Weak<Engine>` held by the reaper thread is the signal: when the last `Arc` is gone, `upgrade()` returns `None` and the thread exits. Guarantees eventual cleanup without an extra cross-thread channel.

4. **Format byte bumping over `#[serde(default)]`.** Postcard does not honour `serde(default)` on trailing fields. Schema evolution of on-disk records means an explicit version byte in the record header; load paths match-and-skip on unknown versions. Discovered the hard way during Phase 1 ghost schema changes.

5. **Atomic publish everywhere.** Both the MANIFEST and SSTables go through `.tmp + fsync + rename`. After v0.2.1 Finding 4, this is an invariant the entire storage layer relies on.

6. **In-memory ghost lifecycle metrics.** `last_accessed`, `access_count_total`, `daily_access_bitmap` are not re-persisted on every read. Would cost 150+ WAL writes/second under the target workload for zero durability benefit. Trade-off: Ephemerals get a fresh 24 h lease on reboot; Permanents care only about their flag.

7. **Single `Arc<Engine>` with `weak_self` cell.** Background threads need `Arc<Engine>` but can only be spawned from `&self`. `OnceLock<Weak<Engine>>` populated by `into_arc()` lets any `&self` method `self_arc()` when it needs to spawn. Cleaner than a public `Arc<Engine>` constructor that fights with `set_record_cache_size(&mut self)`.

---

## 12. What is deliberately not here

- **No replication.** Single-node by design; distribution is v1.0+ territory.
- ~~**No snapshots / restore.** Stop engine, copy data dir.~~ **Retired in v0.4.0** (Block 3): `Engine::create_snapshot` + `Engine::restore_snapshot` land as `xyzdb-cli admin snapshot {create, restore}` over the V1 wire — hard-link orchestration with sidecar `snapshot.meta`. **Caveat H12**: snapshot creation under sustained load with concurrent compaction races (race between `set_compaction_enabled(false)` and `live_table_paths()` in `crates/turba-engine/src/engine.rs:442`). 12/13 attempts failed in the v0.4 72h soak; retries succeed intermittently. See [`OPERATIONS.md §6.6`](../OPERATIONS.md) for the mitigation playbook; root-cause fix deferred to v0.5 sub-cycle A.
- **No secondary query languages.** xyTalk is the only surface. SQL shim, Cypher shim — deferred.
- **No transactions across lobes.** Per-lobe writes are atomic through WAL batch commits; cross-lobe is best-effort.
- **No UDF / custom filter functions.** `WHERE` is a closed grammar. Keeps query-planning tractable.
- **No encryption at rest.** None of the workspace `Cargo.toml`s pull `argon2` / `aes-gcm` / `hkdf` / equivalent crates. Operators wanting at-rest confidentiality should run xyzDB on a LUKS / dm-crypt / FileVault-protected volume. The `xyzDB_0_1_V3_2_Final.docx` design draft of March 2026 declared AES-256-GCM "active by default"; that aspiration was not implemented in v0.2.x or v0.3.x.
- **No coordinate-level / dimensional security.** Same draft introduced "CLS" (policies as 3D regions); no module of that name exists in `xyzdb-engine`. Access control is delegated to the network layer.
- **No GNG / online auto-organization.** The pre-MVP V3.2 design positioned a Growing Neural Gas thread as a third pillar (background BMU + reposicionamiento + spring layout). It was never implemented. Background self-tuning at v0.3.x is **leveled compaction + ghost auto-promotion** only; there is no online learning of record placement.
- **No deferred cracking / Hilbert post-compaction.** V3.2 §3.3 described Z-Order → Hilbert reorganisation in compaction filters as a future "refinement layer". Compaction at v0.3.x is leveled merge with dual-criterion overflow ([§3.4](#34-compaction)); no curve transform happens.
- **No HyperLogLog cardinality / MinHash AUTO-LINK runtime.** The `xyzdb-cli admin analyze` path uses an exact `HashSet<u64>` over value hashes ([`xyzdb-engine/src/analyze.rs:54`](../crates/engine/src/analyze.rs#L54)) — not HLL. The `AUTOLINK` keyword has scaffolding in the server write classifier and the Python SDK but no parser binding; suite `s08_autodiscovery` covers an aspirational surface, not a v0.3.x feature.

Each is an explicit non-goal at v0.2 / v0.3, not an accident: replication, cross-lobe transactions, a PG-wire bridge, and a broader SDK matrix are explicitly out of scope.

---

*References: `docs/wal-state-machine.md` for WAL state transitions; `docs/releases/` for per-version narratives; the git history for commit-level rationale.*
