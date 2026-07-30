# xyzDB — condensed pre-1.0 changelog (private development, 0.1 → 1.0)

This is the **condensed** record of xyzDB's private development between 0.1 and 1.0. The full per-version narrative — the reasoning, the measurements, and the decisions behind each release — is kept in a **private repository and is not published**; what survives here in the public repo is the changelog itself.

Its references to commit hashes, branches, run labels, and paths belong to that private repository and to earlier directory structures, so they **do not resolve here**. It includes exploratory directions that were deliberately abandoned — each with its decision — so it is a faithful account of how the engine was reached, not only of what shipped.

The public history begins at 1.0: see [`../../CHANGELOG.md`](../../CHANGELOG.md) and the v1.0.0 note indexed in [`README.md`](README.md).

> **Note (v0.2.5.1):** the language surface was renamed from `xyzQL` to **xyTalk** in v0.2.5.1. Entries below v0.2.5.1 retain the original `xyzQL` wording as a historical record. See the v0.2.5.1 entry for the rename rationale and the migration note.

---

## [0.8.13] — 2026-07-03 (streaming NEAREST bucket sweep)

The fused `NEAREST` path stops materialising the whole gravity bucket before scoring, so scan RAM decouples from bucket size.

### Added

- **`range_stream` — lazy inclusive range scan (`turba-engine`).** `range()` now delegates to `range_stream().collect()`, so the two are byte-identical by construction. `range_stream` yields the same visible entries over the inclusive `[start, end]` in the same order, but lazily — an O(block) working set instead of materialising the whole range. Distinct from `range_iter` (half-open `[start, end)`); the inclusive bound is load-bearing for gravity-bucket sweeps whose `key_max` is the saturated all-`0xFF` tail.

### Changed

- **Streamed fused `NEAREST` bucket sweep (`perf`).** Both fused loops (the V5 vector column and the V3/V4 inline-blob fallback) switch from collecting the bucket into a `Vec` to `range_stream`. The bounded top-k heap already caps retained state at k, so the scan working set is O(block), decoupling RAM from bucket size N — this removes the query balloon that pushed large buckets past a tight memory cap. Exactness and latency are unchanged (same comparisons, same order); only `try_prefix_scan_nearest` is touched, the unfused fallback is untouched.

## [0.8.12] — 2026-07-03 (flush-only WAL checkpoint)

### Fixed

- **Flush-only WAL checkpoint bounds the WAL under multi-scope load.** The 0.8.11 containment forced a full `major_compact` at the size threshold, which re-reads the whole dataset each trigger and cannot keep pace with a high-scope load (thousands of small SSTables) — the WAL reached ~785 MB on a 250k many-bucket load and a hard crash still OOM-looped at a tight envelope. Replaced with `Tree::checkpoint_flush`: pause bg compaction, seal + flush memtables, persist the manifest (O(new data), no full merge); the pruner runs it then **prunes** (never rotates) so a concurrent writer's not-yet-durable tail is never truncated. Galaxy 250k at 256M now ends ~122 MB and a SIGKILL restart recovers; verified graceful and SIGKILL across a 128M–8G envelope sweep.

## [0.8.11] — 2026-07-02 (WAL bounding under lagging keyspaces)

### Fixed

- **Bound the WAL under a lagging keyspace to survive a hard crash.** The prune watermark is `min(manifest_durable_seqno)` across keyspaces, so a keyspace whose memtable never fills pins it low and the pruner cannot drop already-durable archived segments; the WAL grows with the full write history and a hard crash replays it all into one memtable, OOM-killing the restart. The pruner now forces a checkpoint (`major_compact` + WAL rotate) once the WAL passes a memory-derived threshold (cgroup limit / 4, clamped). Skipped in BULKMODE.
- **Reclaim the WAL on graceful shutdown.** `shutdown()` sealed and flushed every tree but left the WAL intact, doubling the on-disk footprint and forcing the next `open()` to replay the entire history into one memtable (OOM at a tight envelope). It now flushes every tree synchronously — advancing each keyspace's manifest-durable seqno — then rotates the WAL, so a clean shutdown leaves only SSTables and recovery replays nothing.

## [0.8.10] — 2026-07-01 (graceful shutdown + visible SCAN truncation)

### Added

- **Graceful shutdown on SIGTERM/Ctrl-C (`xyzdb-server`).** The accept loop races `accept()` against a shutdown signal via `tokio::select!`. On signal, strictly in order: stop accepting, drain in-flight connections (tracked in a `JoinSet`, 5 s bounded), abort stragglers, run `engine.graceful_shutdown()` (clean-shutdown marker + seal/flush of all trees + WAL reclaim), then `std::process::exit(0)`. Committed writes are WAL-durable, so an aborted straggler loses only its in-flight reply. A real-process e2e test (`graceful_shutdown_e2e.rs`, Unix) proves the clean path. **Resolves finding H9.**
- **Vector dimension validated on PUT (`xyzdb-engine`).** A searchable vector field learns its dimension from the first embedding and enforces it on every later write — a mismatched-dimension vector is rejected at ingest instead of being silently dropped from every `NEAREST` top-k at query time. `VectorSpec` grows an optional `dim` (`SPEC_FORMAT` `0x01` → `0x02`); a legacy `0x01` slot opens with the dimension unknown and learns it on the next write. The learned dim is persisted and enforced across restart.
- **`SCAN` never truncates silently (`xyzdb-engine`).** A capped `SCAN` that overflows now returns `PaginatedRecords` with `has_more = true` (cursor `None`) instead of a bare truncated `Records`. Extended from the gravity fast path to every capped `SCAN` route so the signal does not depend on which route the router picks; `NEAREST`-feeding scans never signal (they lift the cap and need every candidate).

## [0.8.9] — 2026-07-01 (NEAREST hardening + Python SDK + toolchain)

### Added

- **`--nearest-budget-ms` airbag for runaway bucket scans**, default calibrated to 3000 ms; **M2.3 hydrate-until-k** for residual-filtered `NEAREST`; the `NEAREST`-feeding `SCAN` decoupled from the implicit cap; an **M3-A cross-bucket `NEAREST` equivalence gate** over the `FOLLOW` union.
- **Python SDK moved to `sdks/python`** (out of the engine workspace); 0.8.8 vectors exposed in the Python SDK and the MCP query tool; the fluent API documented and expanded; default `NEAREST` candidate window set to 10000 with a frame-size guard.

### Changed

- **f32 SIMD scorer for `NEAREST`** (`perf`), dropping the per-element f64 cast; candidates scored directly from the packed bytes.
- **Build aligned to Rust 1.96, edition 2024, BUSL-1.1** across the workspace, with fresh deps; `turba-engine` bumped to 0.2.0.
- **Docs realigned to v0.8.8** — quickstart/reference/MCP examples refreshed, dangling and obsolete markdown references fixed, past release notes reincorporated as the development record.

## [0.8.8] — 2026-06-29 (vector column — gravity-bounded exact NEAREST)

The lobe's searchable embedding becomes a first-class, RAM-cheap axis. A `NEAREST` over a gravity bucket no longer materialises a full record per candidate: the searchable vector is hoisted out of the record blob and ultimately moved into a dedicated **`vectors` keyspace** (the 5th LSM tree), so ranking ranges ~1 KB column entries instead of ~4 KB blobs and a full deserialize runs only for the surviving top-k. This is **not** an index: search stays the EXACT brute-force cosine/dot/l2 over the bucket — there is no ANN/HNSW/IVF anywhere. **No format break for V1/V2 data**; the record-format byte (V3/V4/V5) identifies each layout unambiguously and older blobs read unchanged.

### Added

- **`VECTOR <field> IN "<lobe>"` — declare a lobe's searchable embedding.** Names the single `Value::Vector` field hoisted to the record prefix / vector column and swept by `NEAREST`. A foundational axis sibling to (not part of) `GRAVITY BY`: gravity decides placement, the vector decides what is searched. Persisted in the dictionary slot `[VECTOR_FIELD][lobe_id]` with the same `[MAGIC][format][postcard]` envelope as `GravitySpec` (`vector_spec.rs`).
- **V3/V4/V5 on-disk record formats (`xyzdb-core/src/record.rs`).** **V3** (`0x03`) hoists the searchable vector OUT of `fields` to a front-of-blob prefix, so a `NEAREST` scan reads it without deserialising the whole record (a record with no designated vector serialises `vec: None` — content-identical to V2). **V4** (`0x04`) adds the vector's stored squared norm `‖v‖²` to the prefix for a Cauchy–Schwarz early-abort in cosine. **V5** (`0x05`) moves the vector OUT of the record blob entirely into the `vectors` keyspace (the blob is V4 minus the prefix; the column carries a V4-shaped mini-blob, byte-identical to a V4 prefix so it scores the same).
- **`vectors` keyspace — 5th LSM tree (`turba-engine`).** `TurbaEngine` now opens five fixed keyspaces — `spatial`, `identity`, `dictionary`, `ghosts`, **`vectors`** (`KS_VECTORS = 4`). The vector column is keyed by the same 22-byte spatial key as the record, treated as a first-class keyspace (own WAL/memtable/SSTable/compaction).
- **Fused `Scan`+`Nearest` path (`xyzdb-engine/src/ops/nearest.rs`, `execute_scan_nearest`).** For a `NEAREST` over a gravity bucket with no residual filter, the fast path ranges the `vectors` column (V5) / hoisted prefix (V3/V4) over the bucket key range, scores each candidate zero-copy (no full-record materialisation), keeps a bounded top-k min-heap, and fully hydrates only the survivors. Bit-identical top-k to the unfused reference path (`scan → execute_nearest`); validated by `tests/scan_nearest_fused.rs`.
- **Pruned cosine (`xyzdb-core/src/distance.rs`, `cosine_pruned`).** A Cauchy–Schwarz upper bound (`dot_partial + ‖a_tail‖·‖b_tail‖`) lets a candidate abort once it provably cannot reach the current k-th best score. Survivors score bit-identically to the unpruned `similarity_indexed`.

### Changed

- **Per-query transient RAM ~32 → ~8 MiB for `NEAREST`.** Ranking reads the ~1 KB vector column entries instead of the ~4 KB record blobs, at a ~+2.2% on-disk cost (the column duplicates the vector out of the blob). This is what lets a 128 MB-RAM agent container stay RAM-resident under concurrent vector queries.

### Fixed

- **Gravity placement honours a declared `GRAVITY BY` even without the `*` marker** (`xyzdb-engine/src/ops/put.rs`). A record whose gravity field is written as a plain field now lands in the same bucket the `SCAN`/`FIND` fast path resolves — write-time placement and query-time `detect_gravity_eq` no longer diverge (the spec falls back to anchor/LID gravity only when no spec pins the record).

### Removed

- **Remote embedding (Architecture B / SEXTANT).** The opt-in `EMBED "text"` keyword (in `PUT` and `NEAREST`), the `sextant_client.rs` HTTP client, the `EmbeddingMode` enum, and the server's `--embedding-service-url` / `--embedding-timeout-ms` flags are gone. The engine is now **purely agnostic**: the caller always supplies the vector — a bound `$param`, an inline list, or `REF "id"`, embedded with the same model the corpus used. Embedding is an application/dock concern, never the engine's. (The wiring added in 0.8.6 never became load-bearing; pulling it keeps the engine network-free.)

### Also since 0.8.0 (already shipped under earlier 0.8.x)

- **`NEAREST` / `ORBIT` (v0.8.0)** — semantic top-k over a gravity-bounded scan, `REF "id"` more-like-this, `$param`-bound query vectors.
- **`FOLLOW` (`858e47a`)** — cross-entity expansion across gravity buckets (`FOLLOW <field> TO "<lobe>" ON <target_field>`).
- **Re-gravitation (`6522d78`)** — `SET` on a gravity field moves the record to its new bucket.
- **Packed f32 vectors (`c960ddc`)** — dense float lists store as packed `f32`, lossless.

## [0.8.6] — 2026-06-21 (HSX removed — single-tier hardened)

HSX (the experimental multi-tier RAM/SSD/HDD feature) is **removed**. It had been gated off since the scale-1 Q7/Q8/Q9 regression — the cross-tier merge iterator was never wired — so the product is the single-tier hardened engine. **No on-disk format change**: `SnapshotMeta`'s tier fields, `TierCopyMode`, and `TierId` are kept as inert fossils, so existing 0.8.x data opens with no `migrate`. Verified at scale 0.1: `verify_golden` 0 diffs + content gate match; ghost/compaction/snapshot suites green.

### Removed

- **HSX runtime (turba):** heat allocator, placement detector, SSD→HDD materialise worker, write barrier, orphan registry, placement map, adaptive caps, tier state + multi-tier open/flush routing (`open_multi_tier`).
- **HSX xyTalk surface:** `PLACE GROUP` / `UNPLACE GROUP` / `DEMOTE GROUP` / `RELOAD TIERS` / `PRESERVE ORPHAN` / `RELEASE ORPHAN` verbs (+ their AST, parser, executor, and MCP policy arms).
- **HSX CLI + config:** `--hsx-mode`, `--tier-ram`/`--tier-ssd`/`--tier-hdd`, boot-time tier-budget derivation (`i4_derive`); `HsxConfig`, `TierConfig`, `HsxMode`, `ClassifierKind`, and `EngineConfig.tiers`/`.hsx`.
- **HSX read-path recording:** the per-bucket access counter + heat map (`record_spatial_access` hot-path glue, `bucket_access`/`heat_map`) and the `/stats.placement` telemetry subtree.
- Obsolete `sequential` + `concurrent` benchmark suites (superseded by `benchmarks/native`, the cross-engine xyzDB-vs-pg/mongo harness) and the v0.2.4 runner scripts; stale `scripts/`, `smoke-results/`, `docs/historical/`.

### Changed

- `EngineConfig::validate` is now a no-arg single-tier shell; `Engine::open` no longer dispatches on tier count.
- Ghosts and the spatial read path lost their multi-tree plumbing (single `&Tree` instead of a tree slice / dispatcher) — behaviour-identical; the ghost scan/rollup algorithm is unchanged.

### Kept (on-disk fossils — no format break)

- `SnapshotMeta.tiers` / `tier_copy_mode`, `TierCopyMode`, `TierId` — inert types, always single-tier values; preserved so older snapshots still deserialize.

### Also since 0.8.5

- S1 query parameter binding (`$param`, anti-injection) + opt-in remote-embedding wiring (SEXTANT client).

## [0.8.1] – [0.8.5] — 2026-06 (point releases, backfilled)

Tagged but not previously logged here:

- **0.8.5** — server carries bound `$params` over the wire protocol (S1 phase 2).
- **0.8.4** — parser accepts scientific-notation float literals.
- **0.8.3** — exclusive data-dir lock prevents double-open of a store (C7).
- **0.8.2** — drain in-flight compaction before taking the snapshot WAL lock.
- **0.8.1** — exclude projection ghosts from full-record auto-routing.

## [0.8.0] — 2026-06-13 (format cycle: gravity keel + grouped-ghost efficiency)

The format-break release (project convention: format breaks land in a single minor bump). Several threads land together so the on-disk format moves once, behind one box gate: the **GravitySpec keel + D1** (value-only canonical placement hash), the **rollup merge-operator** that finally makes **P0-2** practical, **cacheable per-SST metadata** (RAM ceiling fix), and the **3f-meta** footer checksum. A pre-0.8 data directory needs a one-time `migrate` — the engine refuses gravity reads/writes until then. Box-validated at scale 1.0 (149.9M records) on SSD and HDD: `verify_golden` 0 diffs, content gate match, identical SSD/HDD content hashes.

### Added

- **`GRAVITY BY <expr> IN "<lobe>"` — declarative placement (GravitySpec keel).** Placement intent is now a first-class declaration: `Raw` (a single field — `*field` is sugar), `Normalized` (`lower`/`trim`), or `Composite` (a tuple folded as one — kills the two-`*` footgun where multiple gravity fields hashed inconsistently between write and query). A single canonical hash module (`gravity_spec.rs`) owns the name→hash contract; `PUT`/`SCAN`/`FIND`/`PULL`/`PLACE` all route through it, so the write and query sides can no longer diverge by construction. The `[0xFF,0xFA][lobe_id]` slot widens to `[MAGIC][format][postcard(GravitySpec)]`; a legacy bare-string slot decodes as `Raw` (no migration for that field).
- **Rollup merge-operator (turba) — blind delta-append ghost rollups.** turba gains a per-key `MergeOperator`, attached to the dictionary tree. Grouped-ghost rollups are written as **blind signed deltas** (no read-modify-write) and folded into one value at compaction and on read. The fold deduplicates the same `(key, seqno)` that transiently lives in both a sealed memtable and the SSTable it is flushing into, so concurrent flush/compaction cannot double-count. This removes the O(groups) RMW that collapsed a high-cardinality `REFRESH GHOST` to **~8.8 h** at scale 1. The operator owns only the `[ROLLUP]` prefix; anchors, gravity specs, and pins keep last-writer-wins.
- **3f-meta — SSTable footer checksum.** A v2 footer appends an `xxh3_64` over the magic + the three block offsets, verified on open; a corrupted offset is now caught instead of silently mis-locating a block. Legacy v1 footers (no checksum) are still read, so an upgraded data directory keeps opening until its SSTables recompact.

### Changed — on-disk format (one-time `migrate` required)

- **D1 — value-only canonical gravity hash.** The placement hash now folds **values only**, not `name+value`, unifying the three historical conventions (`*`-path, anchor fallback, LINK/PLACE) that previously hashed the same logical key three different ways — the root cause of gravity reads missing records they had written. The gravity-spec slot marker moves `0x02 → 0x03`; a slot still at `0x02`/pre-D1 makes the engine **refuse gravity reads/writes until `migrate` runs**. `migrate` (`MIGRATE`) rehashes every gravity key to the value-only convention and rewrites legacy values to V1, with progress logging and an idempotent re-run.
- **Ghost entry key format `0x03 → 0x04`** (P0-2, see _Fixed_).

### Fixed

- **P0-2 · ghost entries no longer collapse records sharing an `ORDER BY` value.** The ghost entry sort-key gains a uniqueness tiebreak (the spatial key for covering ghosts, the group key for grouped ghosts) and a prefix-free `Text` encoding, so a covering ghost returns the full set instead of one record per `ORDER BY` value. Carried as a Known issue in 0.7.6/0.7.7 (correct but impractical to build); now cheap because grouped rollups append instead of RMW.

### Performance

- **Grouped-ghost build (Phase 0.5): ~8.8 h → ~36 min (SSD) / ~68 min (HDD)** at scale 1, via the delta-append rollups above.
- **Cacheable per-SST metadata — RAM ceiling fix.** Zone maps, bloom filters, and the block index were loaded eagerly at `SSTableReader::open` and held resident forever (~800 MB of metadata at scale 1, O(dataset)). All three are now fetched on demand through an evictable metadata cache and reloaded on a miss — **no on-disk format change** (they were already offset-addressable from the footer). Box-measured scale-1 RAM peak: **2.13 GB** (SSD) / **2.06 GB** (HDD), under the v0.1 4 GB baseline on the 2C/8G (T6) target and down from 5.16 GB at v0.6.2.

### Measured (AWS box, scale 1.0, T6 2C/8G)

- `verify_golden` **0 diffs** + content gate match on SSD and HDD; SSD/HDD content hashes identical (no double-count at scale).
- Spatial Q4 (TopExposure) ~1.3 ms; Q4/Q5 group counts correct (100/80).
- Load 74.9k rec/s SSD / 58.5k rec/s HDD; disk footprint ~11.5 GB (~77 B/rec).

## [0.7.7] — 2026-06-10 (post-0.7.6 hardening)

Crash-window and write-path hardening on top of v0.7.6. **No on-disk format change** (`MANIFEST_VERSION = 4`, ghost format unchanged). The audit's **P0-2** finding (ghost entry keys collapse records/groups sharing an `ORDER BY` value) is **deferred to v0.8**: the fix is correct (validated on the box — `verify_golden` exact, all gates pass) but at high group cardinality its build cost needs the v0.8 grouped-ghost efficiency redesign (rollup merge-operator) to be practical, so correctness and efficiency land together rather than shipping a correct-but-slow ingest in a patch.

### Fixed — robustness

- **L1+ level-overlap integrity guard (audit P1-1).** `Tree::get_at` binary-searches each L1+ level on the `[key_min, key_max]` range, which is correct only when the run is sorted **and** non-overlapping. v0.7.x re-sorts L1+ levels by `key_min` at both the compaction-apply and manifest-load sites, but sorting alone does not prove non-overlap — two SSTs from a buggy compaction can share a key range and silently reintroduce the L1+ point-read miss undetected. Both sites now assert non-overlap right after sorting (`Version::check_level_non_overlapping`): a panic in debug/test builds and a `tracing::error!` in release, since an overlap is a compaction bug to surface, not a recoverable data state to tolerate. The pure overlap core (`first_overlapping_index`) is unit-tested.
- **`cleanup_orphan_ssts` could unlink an in-flight flush's SST (audit P1-4).** The orphan sweep deleted any unreferenced SST with `id <= max_referenced`, assuming an in-flight flush always holds the highest id. It does not: the background flush worker is not paused by `major_compact`, so a compaction can install a higher id while a lower-id flush has written its SST but not yet installed it — and that file would be deleted out from under the flush (data loss in a narrow window). Flushes now register their allocated ids in `Tree::flushing_ids` (RAII-scoped, removed on every exit path including `?`), and the sweep skips any in-flight id. The deletion predicate (`orphan_is_deletable`) is unit-tested, including the lower-id-flush-vs-higher-id-compaction case.

### Performance

- **Cache per-ghost core filters across writes (audit P2-2).** `notify_write` rebuilt each ghost's filters into core form on every write — deep-cloning every `Text`/`List`/`Map` literal through `literal_to_value`, once per ghost per write — even though a ghost's filters are immutable after creation. The converted filters are now memoised on `GhostMeta` (`core_filters_cache`, a runtime-only field that is not part of `PersistedGhostMeta`, so the on-disk ghost format is unchanged) and reused. Hot-path win on bulk ingest, where the same invariant conversion was paid N×K times.

## [0.7.6] — 2026-06-10 (gravity read-path correctness + lightweight ghosts)

Engine correctness and memory work on top of v0.7.0. Folds the work scoped as 0.7.5 (gravity read-path fixes) and 0.7.6 (lightweight ghosts + the dictionary-prefix collision fix). No on-disk format change vs v0.7.0 (`MANIFEST_VERSION = 4`). An engine-wide read-only audit accompanies this work: its P0-1 finding is **fixed in this release** (see _Fixed_); the remaining P0-2 and P1 findings are tracked under _Known issues_.

### Fixed — correctness

- **LSM L1+ silent data-miss at scale.** Compaction left L1+ levels unsorted; `Tree::get_at`'s per-level `binary_search` then returned `Ok(None)` for keys that were present — a silent point-read miss that only surfaced at scale (deep levels). Levels are now re-sorted by `key_min` at both mutation sites (compaction apply + manifest load). The manifest-load sort also repairs data dirs written by the pre-fix compaction. Verified on the box: a gravity ghost that returned 0 records now returns the full bucket.
- **FIND / PULL by gravity were modelled as 1→1.** A per-value gravity dictionary entry mapped `(field, value) → one LID` and was overwritten on every `PUT`, so an unlimited `FIND` on a gravity field returned at most one of the bucket's N records, and `DELETE` leaked the entry (un-refcountable). Both retired: `FIND` now resolves a gravity-`Eq` predicate through the same bounded bucket range scan `SCAN` uses, and the dictionary entry is no longer written (nothing left to leak). Pre-existing `0xFE` entries are inert.
- **PULL hash-collision filter.** `PULL` scans the 48-bit gravity bucket; records of a different gravity value that collided into the same bucket are now dropped via a post-filter (the same guard `SCAN` already applied). `LINK TO` children that inherit the bucket are preserved.
- **Q2 fully-pinned group aggregate.** Serving a fully-pinned `GROUP BY` group from an aggregate ghost is now an `O(log N)` point lookup instead of an `O(N)` scan over every group (the ~335 ms → µs Q2 win).
- **Compaction non-convergence guard.** Byte-scaled level target count + a write-amplification ceiling (`max_compaction_amplification`, default 64) abort a runaway compaction with `Error::CompactionStalled` instead of looping.
- **Dictionary prefix collision PIN ↔ FIELD_REGISTRY (audit P0-1).** The earlier PIN-prefix move (away from the ghost-meta collision at `[0xFF,0xFD]`) landed on `[0xFF,0xFB]`, already owned by the V2 field registry; both key by `lobe_id` and share the value shape `[MAGIC][0x01][postcard]`, so a lobe with both a PIN and V2 records had the two writes silently clobber each other (corrupt field names → V2 records decoding with the wrong fields). PIN moved to the unused `[0xFF,0xF8]` with the `[0xFF,0xFD]` read-only boot migration retained. All seven reserved dictionary prefixes are now centralized in `reserved_keys.rs` with a test that enumerates them and fails the build on any future un-disambiguated prefix collision — closing the class that recurred twice. Regression was on `stage` only, never in a tagged release.

### Added

- **Lightweight ghosts — on-disk group rollups.** An aggregate (`GROUP BY … AGGREGATE …`) ghost kept every group's accumulator resident in RAM (`group_summaries`); a high-cardinality grouping key (one group per `rfc` → millions) made that map gigabytes (~2.3 GB measured at scale 1, the dominant share of engine RSS). Past `group_spill_limit()` (64k groups; `XYZ_GHOST_SUMMARIES_MAX_GROUPS` overrides for tests) the map spills to **one canonical rollup entry per group** in the dictionary keyspace and the in-RAM map stays empty. Reads of a fully-pinned group become a single bloom-backed exact `get`; incremental writes do a get-merge-put. Low-cardinality ghosts (by `empresa`, thousands of groups) stay in RAM, unchanged. RAM at scale-1 SSD validation dropped from 4.9 GB → 3.1 GB peak / 2.5 GB avg.
- **BULKMODE defers ghost aggregate maintenance.** Under `BULKMODE ON`, `notify_write` skips all aggregate maintenance (in-RAM and on-disk rollup RMW alike) — a per-record rollup RMW collapsed scale-1 ingest to tens of records/s. Covering-index entry inserts continue. The post-load `REFRESH` rebuilds every aggregate ghost (the contract was already documented; the engine now honours it).
- **`ram_budget.ghost_aggregates_bytes`.** `/stats` now models in-RAM ghost aggregate state as its own component, so the `ratio` no longer under-reports at scale (was 0.41 with `group_summaries` unmodelled).

### Known issues — from the engine audit, NOT yet fixed

- **P0-2 · ghost entry sort-key has no uniqueness suffix.** Records sharing an `ORDER BY` value overwrite each other in the ghost keyspace; covering ghosts return a subset. Pre-existing; scoped for the 0.8 ghost-key rework (changes covering-ghost on-disk format).
- **P1 · crash-window and frailty findings.** L1+ no-overlap invariant not asserted at install time; ghost maintenance runs post-commit outside the WAL group; `flush_sealed` swaps the in-memory version before persisting the manifest; `cleanup_orphan_ssts` can race a flush in flight. Robustness hardening, deferred to a 0.7.x follow-up. Full detail in the private engine audit.

## [0.7.0] — 2026-06-01

**Hardened base engine; HSX active mode shelved as experimental.** The v0.6.2 cycle built HSX active mode (tier migration, tier-aware reads), but the AWS HDD scale-1 production verdict was **negative** — the heat allocator regresses Q7/Q8/Q9 catastrophically at scale (root cause: the §4 cross-tier merge iterator is deferred/unwired). v0.7.0 ships the **single-tier base engine** as the product and keeps HSX **off by default, marked experimental**, pending a structural redesign. The HSX code is conserved, not removed.

### Highlights

- **Base engine is the product.** `--hsx-mode` defaults to `baseline` (single-tier); no allocator/detector thread is spawned in the default path (zero-cost, I-1). HSX heterogeneous mode remains available but experimental.
- **Fat LTO is now the default release profile** (`lto = "fat"`, `codegen-units = 1`). Resolves the codegen-differential overhead (~60 % slower `Q8MonthlyClose`) that previously required manual build flags.
- **Engine hardening (conserved from the v0.6.2 cycle, inert in baseline):** bulk-load heat-recording gate (removes a load-throughput slowdown) and bounded heat-map eviction + admission filter + entry-API race fix. Both run only on the HSX-active path; harmless in baseline.

### Measured base-engine evidence (AWS, scale 1.0)

Cross-engine matrix vs PostgreSQL / MongoDB:

- **Spatial Q4 (TopExposure):** xyzdb 1.14 ms vs PostgreSQL 4 887 ms vs MongoDB 162 260 ms — ~4 300× / ~142 000× faster.
- **Load throughput:** +34 % on HDD vs PostgreSQL (48.7k vs 36.4k rec/s); disk density ~2.6× (xyzdb ~10 GB vs PostgreSQL ~28-30 GB, ~76 B/rec).
- Honest trade-offs: PostgreSQL wins ad-hoc aggregates (Q2/Q8/Q9 via materialised views); xyzdb uses more RAM (~5.2 GB peak on the 2C/8G target).

### Experimental — HSX (heterogeneous) mode

- Off by default. When enabled (`--hsx-mode heterogeneous` + tier paths) it activates the v0.6.2 active path (tier migration, tier-aware reads, crash-safe move worker). **Not production-ready** — see the negative scale-1 verdict in the v0.6.2 entry. The cross-tier merge iterator (§4) remains unwired, the structural gap behind the Q7/Q8/Q9 regression.

### Versioning

- Jumped 0.6.x → **0.7.0** to mark a clean milestone distinct from the shelved HSX-active attempt. No on-disk format change vs v0.6.1. The `version` field (previously stuck at `0.1.0`) now tracks the release.

## [0.6.2] — 2026-05-27 — DEVELOPMENT ONLY (gated off, never tagged)

> **Status: DEVELOPMENT — gated off, never released as a tag.** This cycle implemented HSX active mode, but the AWS HDD scale-1 production verdict was NEGATIVE (see "Production verdict" below). The work is conserved behind the off-by-default `--hsx-mode` gate; the shippable base-engine parts were released as **v0.7.0**.

HSX **active mode** (implementation record). Where v0.6.1 shipped the heat-tracking + intent-publishing *foundation*, v0.6.2 implemented the active path: data physically ascends and descends across tiers, reads dispatch to where the data lives, and a bucket mid-migration stays consistent and crash-safe.

### Highlights

- **Read-path tier-aware dispatch** (D6 §4). Every read site resolves the bucket's `physical_tier` via the per-bucket dispatcher (`spatial_for_bucket_with_ctx`) instead of the default-write-tier alias. Single-tier (`--hsx-mode baseline`) bypasses the dispatcher entirely (`tier_state = None`) — no overhead leaks into the v0.6.0 path (guarded by G9).
- **Cross-tier merge iterator** (I-7). A bucket whose data straddles source and destination tiers during a migration is read through a merge iterator with a `visible_seqno` cutoff, so every reader observes either the pre-move or post-move snapshot — never a split state.
- **Move worker — 6-phase pipeline** `PLANNED → COPYING → COMPACTING → SEALED → PUBLISHED → CLEANED` (D6 §5b/§5c). The SEALED phase holds a per-bucket write barrier and delta-copies writes that raced the COPYING window, closing the writes-during-migration hole (I-7). `physical_tier` flips only at SEALED→PUBLISHED; the PlacementMap persists at every phase transition.
- **Crash-safety recovery state machine** (I-8, D6 §8.2). On open, each `pending_migration` is classified by phase and resolved deterministically: PLANNED/SEALED re-enqueue (worker re-runs the pipeline; idempotent via immutable `snapshot_seqno` + MVCC dedup), COPYING/COMPACTING abort (clear pending, `physical_tier` unchanged), PUBLISHED/CLEANED finalize. A `kill -9` mid-migration always resumes or rolls back to a single coherent tier.
- **Ghost path tier-aware** (D-HSX-16, D6 §9). `ghost_manager::read_topn` fallback + ghost rebuild iterate the source lobe through the per-bucket dispatcher; ghost SSTs stay on the source lobe's default tier.

### Performance

- **G3(d)**: hetero `Q1Point` P50 **−9.68 %** vs baseline on the promoted hot subset (canonical workload, scale 0.1, 3600 s, cold-runs 100, three physical media). ⚠️ **This held at scale 0.1 ONLY** (working set fit in cache); at scale 1 the allocator regresses Q7/Q8/Q9 — see "Production verdict" below.
- **G1 PASS** — 0 regressions vs `pre-v0.6.1-cycle @ ccc4b5d` under the absolute-or-relative band (see methodology disclosure below).
- **G9 PASS** — 0 regressions vs `v0.6.1 @ dc2a219`; the D6 dispatcher adds no measurable overhead to single-tier baseline mode.

### Build requirement

v0.6.2 **requires an LTO build for production deployment**:

```
CARGO_PROFILE_RELEASE_LTO=fat \
CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 \
cargo build --release
```

The non-LTO build exhibits a **codegen-differential overhead (~60 % slower on `Q8MonthlyClose` under mixed workload)** caused by Rust std / `flume` inlining decisions in the default release profile (lazy-init paths surface as `OnceBox::initialize` / `pthread_mutex_init` / elevated `flume::recv_timeout` in profiles). LTO inlining eliminates the differential; the overhead is **not** algorithmic. This was the root cause of the original G1 `Q8` halt and is resolved by the build flags above, confirmed across four independent canonical runs.

### Methodology disclosure

- **G1/G9 acceptance criterion** uses an **absolute-or-relative band**: a metric counts as a regression only when it exceeds **both** +5 % relative **and** +0.5 ms absolute. On sub-millisecond queries the Mac developer hardware noise floor (~0.5 ms scheduling jitter) exceeds the 5 % relative threshold, producing false-positive halts on tail percentiles of small queries (e.g. `Q1Point` P99 +0.03 ms). The band keeps gross regressions caught while ignoring sub-ms tail noise. Evidence: violations migrate between queries across runs (signature of noise, not regression); `Q8` — the real signal — resolves consistently under LTO. See cycle plan §8 review notes + §10.

### HSX runtime model + deferred work

- HSX active mode in v0.6.2 operates under the **§2.5 lazy write-through model**: the heat allocator publishes `canonical_tier` intent, and data migrates lazily as new mutations flush to the intended tier. The **spontaneous bulk-migration trigger** (the allocator emitting a `TierMigration` for cold on-disk data during normal operation) remains **§2.7-deferred** per `detector.rs` ("bulk migration of cold on-disk data is §2.7 work").
- **G7 acceptance is reformulated** to validate I-8 via the **recovery path** — the only `TierMigration` emitter in v0.6.2 runtime. It seeds a `pending_migration` at each phase, boots (recovery classifies + acts per D6 §8.2), then `kill -9` + re-boot asserts clean idempotent crash recovery (18/18: 6 phases × 3 cycles). G7 coverage expands to the spontaneous-trigger path when §2.7 lands.

### Production verdict — NEGATIVE (resolved 2026-05-31)

The AWS HDD scale-1 canonical run (per §12.10) came back **negative**: with the allocator active, cold-query p50 regresses **Q7 +2039 %, Q8 +80 %, Q9 +1301 %** (and Q3-p99 +598 %) vs the allocator-inert control — both sides 3-tier, isolating the regression to the allocator (burst confound ruled out: Q2/Q3-p50 equal across sides). Root cause: the allocator promotes buckets and fragments the lobe across tiers, but the §4 cross-tier merge iterator is deferred/unwired, so multi-bucket reads without a hint (Q9) and tail scans (Q3-p99) collapse; Q7 batch writes contend with the allocator. The Mac/scale-0.1 gates passed because the working set fit in cache. **HSX active mode is shelved as experimental, off by default** (shipped as v0.7.0).

### v0.7 preview

The v0.7 cycle is dedicated to a **true in-process RAM tier** (BTreeMap-backed, ~1000× vs SSD) replacing v0.6.2's ramdisk-backed RAM tier (~50× vs SSD). Design contract D7 + cycle plan pending post-tag.

### Acceptance evidence

7/7 gates PASS on canonical workload (`benchmarks/native/results/v0.6.2-acceptance/`):

- **G1 PASS** — absolute-or-relative band vs `pre-v0.6.1-cycle @ ccc4b5d`; `Q8` resolved under LTO across strict / swap / alternated / counterbalanced runs (`g1-cb-20260527T000244Z/G1-ACCEPTANCE-VERDICT.md`).
- **G2 PASS** — 6/6 hetero-activation sub-checks.
- **G3(d) PASS** — hetero `Q1Point` P50 −9.68 % vs baseline on the hot subset.
- **G4 PASS** — 15/15 kill-9 recovery sub-checks (3 cycles × 5).
- **G7 PASS** — 18/18 move-worker / I-8 sub-checks via recovery path (`v0.6.2-g7g9-20260527T142343Z`).
- **G8 PASS** — single-reader-per-phase + 8-concurrent-readers see no split snapshot (I-7).
- **G9 PASS** — 0 regressions vs `v0.6.1 @ dc2a219` (same band as G1).

### Breaking

- **Production deployments must build with the LTO flags above.** A default `cargo build --release` is functionally correct but carries the codegen-differential overhead; it is not a supported production artifact for v0.6.2.
- No on-disk format change vs v0.6.1: the PLACEMENT_MAP V2 schema (`physical_tier` + `pending_migration`) loads v0.6.1 (V1) maps cleanly via the documented upgrade path.

### Tests

- `cargo test --workspace` green. Recovery state-machine unit tests cover each `pending_migration` phase per D6 §8.2; the `seed_g7_migration` example (turba-engine) builds a phase-pinned PLACEMENT_MAP for the process-level G7 harness.

## [0.6.1] — 2026-05-22

HSX heterogeneous foundation. v0.6.1 is the MVP α+: **the heat-tracking + intent-publishing layer**, plus the surrounding telemetry, persistence, and adaptive-caps work. The **active path** (physical tier-to-tier migration + tier-aware read dispatch + cross-tier merge iterator + measurable Q1Point P50 improvement under the canonical workload) is **explicitly deferred to a dedicated v0.6.2 cycle** — see "Out of scope / deferred to v0.6.2" below for the honest framing.

### Highlights

- **`--hsx-mode {baseline,heterogeneous}` + `--hsx-classifier {default,detector,custom}` CLI** (§1, D5 §1). `baseline` is the v0.6.0 default carry-forward; `heterogeneous` activates the heat allocator + adaptive caps. Operators upgrading without flags keep v0.6.0 behaviour.
- **HeatMap (C3) + AccessPattern enum** (§1, D5 §3). Bounded `DashMap<u64, HeatEntry>` (cap 10000) with continuous-decay heat formula (λ ≈ 0.001155/s, half-life 10 min) + four access patterns (PointLookup, RangeScan, BatchIngest, Unknown). LRU-by-coldness eviction at cap. `BucketAccessCounter::record` extended with `pattern` arg.
- **HeatAllocatorWorker** (§2, D5 §4). Periodic pass (30 s default) on a dedicated `hsx-allocator` thread: ranks the top-K buckets by current heat, plans moves under a per-pass `MOVE_BUDGET = 8`, applies them to `PlacementMap.canonical_tier`. Compaction interlock via `Tree::compaction_in_progress` `Arc<AtomicU64>` + `CompactionGuard` RAII (D5 §4.7). Bootstrap warm-up (first pass observe-only). **MVP α+ deviation accepted retroactively as v0.6.1 scope**: intent-only — the allocator publishes `BucketPlacement.canonical_tier` updates; the physical move worker + tier-aware reads are v0.6.2.
- **PlacementMap C2 persistence after applying pass** (§2 V4, surfaced by §5 G4). `heat_allocator::run_pass` now calls `pm.save(placement_root)` after `apply_decisions` mutates the in-memory map (same envelope pattern materialise.rs uses: postcard + xxh3 + atomic rename + dir fsync). Pre-fix, allocator moves lived only in memory and disappeared on kill -9; G4 sub-check (c) caught it directly with `placement_map_entries = 0` post-restart.
- **Adaptive caps with Normal + Critical-freeze** (§3, D5 §5). `AdaptiveCapsState` runtime probes host pressure (cgroup memory + `/proc/meminfo` + statvfs); transitions across four levels (`Normal` / `Warning` / `Emergency` / `Critical`) with asymmetric hysteresis (entry 25 / 10 / 5; exit 30 / 12 / 7; 2 pp gap). Critical entry emits `tracing::warn!(target: "hsx.adaptive_caps", …)` ALERT log and the next allocator pass skips entirely (freeze). Level resets to `Normal` on restart — operational note in module doc.
- **xyzMC `/stats.placement.*` hooks** (§4, D5 §9). New nested sub-objects `placement.heat_map`, `placement.allocator_decisions` (ring N=16), `placement.adaptive_caps` (level + caps + transitions ring N=32). `serde(skip_serializing_if = "Option::is_none")` — fields are entirely absent in baseline mode, present-with-data in heterogeneous mode. Schema validation tests lock the I-1 contract.
- **G4 persistence safety gate** (§5, D5 §6.3). Three kill -9 cycles, five sub-checks each (WAL replay clean, C1 acked-writes queryable, C2 MANIFEST + PlacementMap loaded, C3 HeatMap zero-then-warms, C4 BlockCache effectively-reset-then-warms). 15/15 sub-checks PASS on the final acceptance run (tag `full-20260522T020306Z`). Sub-checks (b) and (e) carry documented operational notes — (b) C1 is one-sided `observed >= expected` because the workload is INSERT-only and concurrent writes durably commit before the kill; (e) C4 uses a `< max(64 KiB, pre_kill / 100)` threshold because WAL replay + MANIFEST load incidentally read a handful of blocks through the cache during recovery.
- **G1 canonical baseline-regression gate** (§6, scope added mid-cycle). One-sided regression check: `--hsx-mode baseline` (v0.6.1) vs `pre-v0.6.1-cycle` (commit `ccc4b5d`) on the canonical workload (scale 0.1, duration 3600 s, cold-runs 100). Final verdict (tag `canonical-v6-g1-20260522T194601Z`): **0 regressions, 11 improvements, 7 within ±5 %**. Q3FullHistory P99 candidate −98.5 % (40.9 ms → 0.55 ms), Q1Point P99 candidate −46 %, Q8MonthlyClose P50/P99 candidate −5 % / −9 % (down from +66 % / +168 % pre-V5+V6). Same `load.records_loaded = 14 993 830` on both sides; same `concurrent.reads_total / writes_total` within 0.13 %; verify diffs within 700 records on 12.3 M.
- **Two perf regressions caught + closed during §6 G1**: V5 hoisted the per-entry `record_spatial_access` HSX-active check out of every spatial-iteration loop (`find.rs::scan_lobe_filtered`, `pull.rs`, `scan.rs` × 5, `engine.rs` × 3) — LLVM did not LICM the inlined short-circuit; manual hoisting elides the body in baseline mode. V6 gated the `batch_spatial_keys: Vec<Vec<u8>>` collection in `ops/put.rs` on `hsx_active` so baseline-mode PUT BATCH paths no longer pay ~10k heap allocations per 10k-record batch.

### Out of scope / deferred to v0.6.2

These items are documented as a coherent dedicated cycle rather than partial-credit asterisks. v0.6.1 ships HSX *foundation*; v0.6.2 ships HSX *active*. Both are real product, but only the second one delivers the "data ascends and descends like background maintenance" promise from the design pitch.

- **Active tier-to-tier move worker** (D5 §4.2 originally). Currently allocator publishes intent on `BucketPlacement.canonical_tier`; data does not physically migrate. Standalone, this is not useful without the next item.
- **Read-path tier-aware dispatch** (D5 §4.4 corollary). Every read site (`find.rs::scan_lobe_filtered`, `pull.rs`, `scan.rs` × 5, `engine.rs` × 3, `ghost::read_topn`) currently consults `engine.turba.spatial` — an alias to the *default-write-tier's* spatial Tree. Reads never look at `PlacementMap.canonical_tier` to dispatch to a different tier's Tree. v0.6.2 rewires these.
- **Cross-tier merge iterator** for transition periods where a bucket's data exists on both source and destination tier.
- **`PlacementMap` hot-path lookup index** sized for per-query × per-record consultation.
- **G3 (d) `Q1Point P50` improvement signal**. Currently reformulated as a no-catastrophic-regression sanity check (`hetero ≤ baseline × 1.5`); v0.6.2 re-enables the original assertion (`hetero < baseline` on the promoted subset).
- **Warning / Emergency level cap formulas** (D5 §5.2). MVP runs at Normal caps under Warning/Emergency, logs the entry only.
- **`critical_actions` telemetry + `ssd_failure_state`** (D5 §9.4.1 / §9.4.2).
- **WAL on SSD as hetero default** (D5 §7.1). MVP keeps WAL on HDD; isolates allocator + heat-map evaluation from WAL throughput changes.
- **G5 (write throughput delta)** and **G6 (adversarial memory pressure)** bench gates — depend on the items above.
- Cooldown hysteresis (60 s window), heat-map persistence across restart, runtime `--hsx-mode` toggle.

The deferred-items table and the V1 → V7 review-round forensic trail (each closed gap with its evidence) are recorded in the v0.6.1 cycle notes.

### Acceptance evidence

- **G1 PASS**: 0 regressions on canonical workload (scale 0.1, duration 3600 s, cold-runs 100). Reference binary built from `pre-v0.6.1-cycle @ ccc4b5d`. Tag `canonical-v6-g1-20260522T194601Z` under `benchmarks/native/results/v0.6.1-acceptance/`.
- **G2 PASS**: 6/6 sub-checks on `--hsx-mode heterogeneous` boot — 3-tier configuration accepted, `/stats.placement` populated, `adaptive_caps.level = "Normal"` at boot, `allocator_passes_total > 0` after 60 s, `heat_map` field present.
- **G3 PASS**: (a/b/c) green on distinct-physical-media tiers (DRAM RAM-disk + internal NVMe + external HDD) — `heat_map.entries_total = 10000`, `placement_map_entries > 500`, `allocator_decisions > 0`. (d) PASS as no-catastrophic-regression with hetero overhead within +50 % of baseline (typically negative on the canonical workload — hetero faster).
- **G4 PASS**: 15/15 sub-checks (3 cycles × 5) across `WAL replay` / `C1 queryable` / `C2 loaded` / `C3 heat zero-then-warms` / `C4 cache effectively-reset-then-warms`. Allocator was actively promoting at every kill instant.

### Breaking

- **`BucketAccessCounter::record` signature** changed from `record(bucket, now_ms)` to `record(bucket, pattern, now_ms)`. Embedding callers must add an `AccessPattern` argument. No on-disk format change.
- `/stats.placement.candidates[].gravity_hash` continues to use the v0.6.0 `format!("0x{:012x}", bucket)` 12-hex padded lowercase formatting; new sibling arrays (`allocator_decisions[]`, `heat_map.top_k_by_heat[]`) use the same format so xyzMC can join them.
- All `/stats.placement.*` additions are additive sub-objects with `serde(skip_serializing_if = "Option::is_none")`: existing single-tier consumers see no schema change.

### Tests

- `cargo test --workspace` green; xyzdb-engine integration adds 5 schema-validation tests (strict omission in baseline, allocator-decision `estimated_latency_saved_ms_per_access` JSON null shape, top-K descending order). turba-engine heat_allocator tests grow to 26 (V4 added `applying_pass_persists_placement_map_across_simulated_restart` + `empty_pass_does_not_rewrite_placement_map`).

## [0.6.0-pre] — 2026-05-20

Mini-cycle C of the pre-HSX flatten brief. **Format-breaking release** — on-disk format incompatible with v0.5.x. Tag applied 2026-05-20 on HEAD of `pre-hsx-flatten-C` (commit `e0f5c05`) **without the brief's C.1 bench + C.2 soak gates**; validation consolidated into v0.6.0 final acceptance.

### Highlights

- **`gravity_hash` 21 → 48 bits (C.1)**. `SpatialKey` grows 18 → 22 bytes. Birthday-collision 50% threshold moves from ~1.4K groups to ~16M groups; covers >10 TB datasets with margin for HSX placement decisions. `MANIFEST_VERSION` 3 → 4; data dirs from v0.5.x are rejected at open with a clear migration message (recreate from source per D-MIGRATION).
- **`/stats.ram_budget` aggregate (C.2)**. Per-component RAM accounting (BlockCache, RecordCache, memtables, SST metadata) + ratio against `vmrss_bytes`. Pure observability — no enforcement. HSX (v0.6.0) adds the cap layer that consumes this signal.

### Validation deferred to v0.6.0 final acceptance

The C.1 bench thresholds (Q1 P50 ≤ +10%, SST metadata ≤ +25%, bloom FP unchanged) and C.2 24h soak (`compact_err=0`, ratio ∈ [0.85, 1.15] under humanrandom=9 + daily_erp + scale 0.1 + seed 42) are not run before this tag. Single integrated soak at v0.6.0 final validates format bump + HSX together with A/B HSX-on vs HSX-off on the same binary, against the v0.5.2 baseline.

### Breaking

- **On-disk format**: v0.5.x data dirs do not open. No incremental migration tool.
- **Rust API**: `SpatialKey::new`, `prefix_for_gravity`, `gravity_hash_from_bytes` move from `u32` to `u64`. `hash_to_21bits` → `hash_to_48bits`. `compute_gravity_hash` and `compute_record_gravity_hash` return `u64`. Embedding consumers must widen their hash types.
- **/stats schema additive**: `ram_budget` object is new; existing consumers ignoring unknown keys are unaffected. No removed fields.

### Tests

- Pre-bump: 596 (xyzdb 406 + turba 190).
- Post-bump: **599** (xyzdb 407 + turba 192; +3 net: spatial_key_size_is_22, gravity_hash_full_48_bits_roundtrip, v3_manifest_fails_with_incompatible_format, ram_budget_snapshot_populates_after_writes — turba +2 via spatial_key_size + manifest reject, xyzdb +1 via ram_budget). All green.

## [0.5.0] — 2026-05-19

Cycle close + xyDisk retirement. Engine remains backward-compatible on disk with v0.4.x; data dirs open cleanly. Bench harness fixes + multi-persona workload + matrix orchestrator land alongside the empirical retirement of xyDisk per DEC-V5-11.

### Highlights

- **xyDisk ladder retired (DEC-V5-11)**. Sub-A.5 canonical A/B on AWS HDD scale 1.0 showed unacceptable trade-offs (Phase 0.5 ×4.4 slower under enforce, write P50 +40% worse, P99 ×3-7 worse across queries, engine idle-bound at 17% CPU vs 63%). The `LanedScheduler` instrumentation stays as pure observability (per-lane EWMA, outstanding peak, SLO breach counter, cross-lane peak). The enforce ladder code (`current_n_max_compaction`, `before_op(Compaction)` blocking loop, `h1_realistic` preset, `XydiskMode` enum, `--xydisk-mode` flag) is gone.
- **`compaction_blocked_us_total` removed (DEC-V5-12)**. Counter for the retired ladder; additionally exhibited the H17 instrumentation bug. Other `/stats.scheduler.*` fields unchanged.
- **Bench harness dispatch fix (H16)**. `ErraticaPicker::next_event` Sleep state failed to emit Query after wake, leaving the harness at 0.025 ev/s pre-fix (idle test). Post-fix (`d977527`) the harness reaches 5.6 ev/s sustained local SSD, matching spec §6.1 warm-up.
- **Multi-persona workload + time-of-day schedule + anomaly injection**. `humanrandom=9` + `daily_erp` with peak/EOD anomaly injection of Q4/Q3/Q8/Q5 — first cycle that exercises Q4 cold queries (713 samples in Run #1 on AWS m5a.xlarge T6 SSD scale 0.1 1h).
- **Cross-engine matrix orchestrator on AWS**. Per-engine `run_engine.sh` with PG `pg_isready` + Mongo `mongosh ping` healthchecks, `/stats` captured before teardown, HDD scale 1.0 4-run sequential matrix driver.
- **Docs translation pass**. 28 docs + bench-comment sweep migrated from Spanish to English (commits `88e3ecf`, `dfd2887`, `4f83d10`, `38ec01d`). Repo is now 100% English per the project's global rules.

### Findings

- **H16** (closed in cycle): bench dispatch loop bug, fixed in `d977527`.
- **H16-bis** (closed in cycle): virtiofs cache layer interfering with bench reproducibility on OrbStack; superseded by DEC-V5-10 (AWS official setup).
- **H17**: `compaction_blocked_us_total` counter did not instrument the ladder it was supposed to track despite 12 unit tests covering its arithmetic. Eliminated naturally by DEC-V5-12.
- **H18**: cross-engine HDD scale 1.0 matrix on EBS `st1` not viable sequential under T6 cgroup — EBS burst credits exhaust after ~21h continuous I/O. Affects PG/Mongo HDD runs (left incomplete on the AWS instance).

### Decisions

DEC-V5-6 (EWMA α=0.3 sample-step fixed); DEC-V5-7 (no hysteresis — made moot by V5-11); DEC-V5-8 (sub-B B.7 scoped out); DEC-V5-9 (B.9 catalog close); DEC-V5-10 (AWS official setup supersedes USB+OrbStack); **DEC-V5-11 (xyDisk retired)**; **DEC-V5-12 (`compaction_blocked_us_total` removed)**. DEC-V5-1 (original enforce default) is superseded by V5-11.

### Migration

- **No on-disk format change.** v0.4.x data dirs open cleanly under v0.5.0.
- **Scripts must drop `--xydisk-mode`**. The binary rejects unknown flags; remove references entirely.
- **`/stats` consumers must drop `scheduler.compaction_blocked_us_total`** (xyzMC, scrape integrations, dashboards). Other lane fields unchanged.
- **`xyzdb-engine::keyspaces::XydiskMode` enum removed**; embedding consumers parameterizing engine open via this type must drop the parameter.

### Tests

589 total: `xyzdb` 402 + `turba-engine` 187 (was 197 in v0.4.0 — −10 deleted ladder + A.3 tests). All green.

### Cross-product notes

Engine v0.5.0 is the prerequisite Phase 0 close declared in the pre-HSX flatten brief at the mono-repo cross-product docs. Mini-cycles A (v0.5.1 documentation flatten), B (v0.5.2 naming + WAL path + RecordCache LRU), and C (v0.6.0-pre format bump + RAM budget observer) follow.

## [0.4.0] — 2026-05-14

Internal operability MVP. Engine v0.4.0 ships hot snapshot + offline restore, TLS + bearer-token auth on the wire, the `GET /` + `GET /stats` operator surface, and a 72h soak gate validated on T6 (2C / 8G). Blocks 0-6 closed. **No on-disk format change** vs v0.3.4; clients on v0.2.5.x wire protocol continue to work.

### Highlights

- **Hot snapshot + offline restore** (Block 3). `xyzdb-cli admin snapshot {create, restore}`; hard-link orchestration with sidecar `snapshot.meta`; writer lock window <100 ms (4-7 ms empirical). Known limit **H12** — race with concurrent compaction under sustained load (12/13 fails in the 72h soak); fix deferred to v0.5 sub-cycle A. See [`OPERATIONS.md §4`](../../OPERATIONS.md) + [`OPERATIONS.md §6.6`](../../OPERATIONS.md) for the mitigation playbook.
- **TLS + bearer-token auth** (Block 2). `--tls-cert` / `--tls-key` via `tokio-rustls`; `--auth-token` (`XYZDB_TOKEN` for CLI + Python SDK) on V1 / V2 / V3. `/health` + `/ready` exempt from auth (load-balancer requirement).
- **`/metrics` Prometheus + observability** (Block 2). Emits over wire V1 (Prometheus binary framing limitation — finding **H4**); operators use a 30-LOC TCP→HTTP sidecar (Pattern B in [`OPERATIONS.md §5`](../../OPERATIONS.md)) until the v0.5 sub-cycle D OTel push exporter lands. Per-keyspace `pread_service_time_us_histogram`, sync-thread heartbeat, reap-cycle log fallback.
- **Operator surface** (Block 5). Static HTML on `GET /` polling `GET /stats` every 5s; XSS-safe (server never interpolates state into HTML, `escapeHtml()` on all `/stats` values). Cookie / `?token=` / Bearer auth. `/health` / `/ready` / `/metrics` over HTTP omitted by binary-budget — covered via wire V1 (**H8**); v0.5 sub-cycle D restores them when OTel exporter lands.
- **BlockCache lane admission policy** (Block 4). `--block-cache-lane-admission` flag (default **disabled** per DEC-V4-3 + finding **H7** — empirical 0 pp delta vs ≥10 pp gate on quick_cache 0.6 S3-FIFO). Policy plumbing + admission counters operational.
- **Router fix C-16** (Block 1). Prefer primary anchor lookup over a ghost when the query has an `Eq` predicate on an anchored column; closes the Q2 380× regression.
- **OPERATIONS.md runbook** (Block 6). §1-9 complete: deployment topology, configuration, health checks, backup, monitoring, incident playbook (5 scenarios + §6.6 H12), tuning (lane admission + ANALYZE cadence H14), upgrade (in-place + rollback + format mismatch), decommission (H9 signal-handler semantics).
- **72h soak gate** (Block 6 cp 6.2.3b). AWS EC2 T6, MMPP 2-state Path A workload. All four blocking gates **PASS**: G1 (compact_err=0 across keyspaces), G2 (heartbeat advancing), G3 (peak VmRSS 62% of cgroup), G4 sustained_growth 5.87% (under 10% threshold per DEC-V4-6 dual-gate).

### Findings

15 findings catalogued in the v0.4.0 cycle notes. Closed in-cycle: H1, H10 (Dockerfile glibc), H11 (G2 misformulated), H13 (G4 baseline calibration). Deferred to v0.5: H2, H3 (MCP `--connect` auth), H4, H5, H6 (token rotation), H7, H8, H9 (signal handler), H12 (snapshot race), H14 (ANALYZE memory burst — new from soak), H15 (orchestrator reporter mis-label).

### Decisions

DEC-V4-1 (branch base from `37d9820`); DEC-V4-3 (lane admission ceiling override 150→200 LOC + flag default disabled); DEC-V4-4 (soak 72h not 168h, deferred to v0.5 sub-cycle B); DEC-V4-5 (push `v0.4-cycle` to origin pre-cycle-close); DEC-V4-6 (G4 dual-gate refinement — sustained-growth blocking + peak-growth monitored).

### Migration

- **No on-disk format bump.** `MANIFEST_VERSION = 3`, `GHOST_META_FORMAT = 0x03` unchanged from v0.3.4. v0.4.0 opens any v0.3.x data dir cleanly.
- **`xyzdb-cli` not bundled in the v0.4 runtime container.** `xyzdb/Dockerfile` copies only `xyzdb-server`. Operators running backup automation on the host must build the cli locally (`cargo build --release -p xyzdb-cli`) before scheduling snapshot crons. Folded into v0.5 sub-cycle A alongside the H12 race fix. Caveat documented in [`OPERATIONS.md §4`](../../OPERATIONS.md).
- **Token rotation requires server restart** (**H6**). v0.5 sub-cycle C upgrades to mTLS / JWT with graceful rotation.

## [0.3.4] — 2026-05-08

Bench harness cleanup + measurement-honesty cycle. **Engine version unchanged** (xyzDB engine remains at v0.2.5.x for this cycle). Sealed measurement quality across the cross-engine bench (xyzDB + PostgreSQL + MongoDB + Surreal partial) and produced the first apples-to-apples cross-engine report at Scale 1.0.

### Highlights

- **`verify_golden` Phase 1.5 gate.** Generator iterators emit a `GoldenFile` baseline (V1-V6 aggregates with `reference_now` + tolerance + caveats); each driver runs `verify_golden()` against the file post-`post_load()` and pre-cold. Methodology refinement: V-set work scales ~250 LOC **per engine**, not per cycle. C-9 closed (Phase 5 verify was masking a +500-record drift introduced by Q7 PUT BATCH).
- **19 cross-engine caveats consolidated** (C-1 to C-19) — bench / asymmetry / ingestion-gap surfaces with root cause + empirical evidence + fix paths.
- **Cross-engine wins at Scale 1.0** — bulk_load HDD-cap **xyzDB 4.48× vs Postgres**; Q1 SSD **3.7× vs PG, 2.1× vs Mongo**; Q1 HDD **67× vs PG**; Q3 HDD **3.86× vs PG**; sustained reads/s SSD **xyzDB 29× vs PG**, HDD **26.5× vs PG**.
- **DBA-less storyline reframed** via Session 3 AutoOnly run (Scale 1.0 SSD, 30 min sustained, no ghosts predeclared): Q2 = 0.93 ms with no ghost vs Full-mode 80-345 ms (router-policy regression **C-16**). Storyline: "competitive sub-ms latency without the DBA ritual", not "fastest at every query".

### Folds-in (engine micro-cycles, pre-v0.3.4 cleanup)

- **`v0.3.2-ghost-singleflight`** — single-flight gate against duplicate ghost-creation under sustained load. Mechanism shipped in turba-engine; C-19 family deferral to v0.5 sub-cycle A.

### Findings closed pre-v0.4

- **C-16** router policy bug — fix shipped in v0.4.0 Block 1.
- **C-19 family** ghost LRU thrashing — single-flight base in this release; full fix deferred to v0.5 sub-cycle A.

Full release notes in the v0.3.4 cycle archive.

## [0.3.2-ghost-singleflight] — 2026-05-02

Partial-delivery tag, engine-only micro-cycle. Path B mechanism class lands: the v0.3.2 Spike D dominator (`maybe_create_ephemeral_ghost` at 74.4 % of 8R CPU samples) is removed via a bounded pool + single-flight gate in `turba-engine`. Six atomic counters expose the gate state. Zero infrastructure overhead. **Engine-only release**: no wire protocol change, no on-disk format change, no xyTalk grammar change. Carry-overs (C-19 family of ghost LRU thrashing artifacts) deferred to v0.3.4 design + v0.5 sub-cycle A.

## [0.3.1] — 2026-05-01

v0.3 xyDisk cycle ships its empirically-validated subset. **Engine-only release**: no wire protocol change, no on-disk format change, no xyTalk grammar change.

### Added — H2 mechanism class (Phase 0.5 wall-time reduction)

- **H2.1 — Trivial-move promotion in `major_compact`** (`turba-engine`). Manifest-only promotion replaces real I/O when a single L0 SST can be moved to L1.
- **H2.2 — Pre-warm L0 data sections** in `major_compact`. Sequential read of L0 data before compaction iter 1.
- **H2.3 — L0 batch tunable per storage profile.** Sweep validated default 50; null-result honestly recorded.

**Acceptance** (Bench A Scale 1.0 HDD, 149 934 430 records): Phase 0.5 post-load drops from 7 320 s (122 min) to 3 516 s (59 min) — **−52% / −63 min**. Wall total **−30% / −56 min**. Phase 2 cold queries Q1/Q5 P50 invariant.

### Fixed — H1.3.1 reader-feedback ladder activation

`current_n_max_compaction` was reading `ewma_p50_ns[UserIORead]` directly via `AtomicU64::load`; the atomic was only updated as a side effect of `p50_us(lane)`, never invoked during query traffic. The EWMA stayed at 0 forever and the H1 ladder always returned the configured cap (silently inert). Fix: refresh the EWMA inside `current_n_max_compaction` before reading.

### Notes

- H1 ladder remains as a healthy default for compaction-dominated regimes (write-heavy bursts, Phase 0.5-style work). Carry-overs in v0.3.5+ backlog.

## [0.2.6] — 2026-04-28

The **AI-agent consumption surface** ships. xyzDB exposes a Model Context Protocol server (`xyzdb-mcp`) that lets Claude Desktop, Claude Code, and other MCP-aware clients query the engine without learning xyTalk syntax or implementing a TCP driver. **Zero breaking change at the engine layer** — `turba-engine`, `xyzdb-engine`, `xyzdb-core`, and `xytalk-parser` byte-identical to v0.2.5.2.

### Added

- **`xyzdb-mcp` server** using the [rmcp](https://github.com/modelcontextprotocol/rust-sdk) 1.5 SDK (JSON-RPC 2.0 over stdio). Two source modes — `--embed <PATH>` (single-process, canonical Claude Desktop subprocess) and `--connect <HOST:PORT>` (TCP client of an existing `xyzdb-server`).
- **Four tools**: `stats`, `query` (with cursor pagination), `list_lobes`, `describe_lobe` (composite schema with per-field `PartialResult` fallibility).
- **Three resources**: `xyzdb://lobes`, `xyzdb://stats`, `xyzdb://lobes/{name}` (template).
- **Privacy contract** — default-on redaction: statements never logged, only `xxh3-64 query_hash` + `query_kind`. `--log-statements` rejected on non-loopback `--connect`.
- **7 failure modes** documented + reproduced in `xyzdb-mcp/tests/uat_failure_modes.sh`.
- **`--query-timeout-ms` flag**.

### Notes

- No on-disk format change. No wire-protocol change. No grammar removal. No engine API change.
- **Auth caveat** (**H3**, raised v0.4 cp 2.2.2): `xyzdb-mcp --connect` does NOT propagate `XYZDB_TOKEN`. With a `--auth-token`-enabled server upstream, the handshake fails. Workarounds in `docs/mcp-integration.md`; full fix in v0.5 sub-cycle C.

Full release notes in the v0.2.6 cycle archive.

## [0.2.5.2] — 2026-04-27

Documentation reframe by usage tier (Quickstart / Common / Power user / Operator) plus cursor extension on FIND for the gravity-bounded path. **Zero breaking change**; every query that runs on v0.2.5.1 runs identically on v0.2.5.2.

### Added (Piece 2 — cursor on FIND for gravity-bounded paths)

- **`FIND ... [LIMIT n] [CURSOR "<token>"]` grammar extension**. `FindStmt` AST gains `limit: Option<u64>` and `cursor: Option<String>` fields. Parser consumes optional `LIMIT` and `CURSOR` clauses after `WHERE` (reuses the existing `parse_limit` / `parse_cursor` helpers introduced in v0.2.5.1 for SCAN).
- **Engine: `execute_find_paginated` + `try_first_page_paginated`** in `xyzdb-engine/src/ops/find.rs`. Cursor presence routes to the paginated path (subsequent page); LIMIT presence on a gravity-eligible predicate triggers the first-page paginated path automatically (emits `PaginatedRecords` with fresh cursor + `has_more`). Cursor is rejected explicitly on:
  - `FIND LID(...)` — single-record lookup, error `"cursor not applicable to FIND LID(...)"`.
  - `FIND WHERE anchor_field = X CURSOR ...` — single record, error `"cursor not applicable to anchor lookup"`.
  - `FIND WHERE non_fast_field = X CURSOR ...` — no fast path, error `"field has no anchor or gravity"` with SCAN suggestion.
- **`find_gravity_paginated`** range-scan helper bounded by `SpatialKey::prefix_for_entity`. Mirrors `scan_primary_paginated` (page tail tracking + overscan-by-one for `has_more` detection) but stays inside the gravity bucket. Hash-collision post-filter via `record_matches_opt_expr` preserves Finding 13 correctness.
- **Cursor format and filter-checksum unified** between FIND and SCAN. `Vec<Filter>` (FIND's AND-flat shape) is converted to `Option<FilterExpr>` via `FilterExpr::from_filters` before hashing — a cursor produced by SCAN-Primary on the same logical filter would have the same checksum (cross-verb resume is not v0.2.5.2 scope but the format leaves the door open).

### Changed (Piece 2)

- **`scan::detect_gravity_eq` exposed as `pub(crate)`** so `find.rs` can reuse the gravity-eligibility detector without duplicating the logic.

### Added (Piece 1 — docs reframe)

- **Tier categorization** in `docs/xytalk-spec.md` §2. Statements now grouped by `Tier 1 — Quickstart` (LOBE, ANCHOR, PUT, PUT BATCH, FIND, SCAN, SET, DELETE), `Tier 2 — Common` (LINK, INCACHE/OUTCACHE, AGGREGATE, SHOW introspection), `Tier 3 — Power user` (PULL, SCAN GHOST, CREATE/DROP/REFRESH GHOST, AUTOANCHOR APPLY, PIN/UNPIN, SHOW tuning), `Tier 4 — Operator` (deprecated, redirects to `xyzdb-cli admin`).
- **§2.1 LOBE statement** documented as proper sub-section. Previously the `LOBE "name" HINT="..."` DDL appeared only as an inline example inside `§1 Data Model`.
- **§2.2 ANCHOR (declarative) and §2.17 AUTOANCHOR APPLY (operational)** split. Previously merged in a single `§2.14`; the split clarifies which is Quickstart-tier (ANCHOR) and which is Power user (AUTOANCHOR APPLY).
- **§2.12 SHOW (introspection) and §2.19 SHOW (tuning)** split. `SHOW LOBES / ANCHORS / GHOSTS / CACHE` is Common-tier discovery; `SHOW SCAN STATS / PROFILE / THROTTLE` is Power user tuning.
- **§2.20 ADMIN** unified entry consolidating `COMPACT / ANALYZE / BULKMODE / MIGRATE` with per-verb behavior. Replaces the previous separate `§2.16-2.19` entries.
- **Tier categorization criterion** documented at the top of `§2 Statements` as the rule for future statement additions.

### Changed (Piece 1 — docs reframe)

- **`docs/usage/quickstart.md`** rewritten as a 6-step sequential tutorial (Connect → Declare your space → Add identity → Insert → Find one → List many). Added `## Common patterns` block with 7 recipes (gravity bucket fetch, top-N, cursor pagination, count by group, in-place update, filtered delete, HotCache). Cross-references to spec organized by tier.
- **Cross-refs updated** in `docs/architecture.md` §8 timeline and `docs/usage/reference.md` to reflect new spec numbering (CREATE GHOST §2.10 → §2.15; INCACHE §2.18 → §2.10; admin §2.19 → §2.20).

### Notes

- No language-surface change. The set of statements is identical to v0.2.5.1; only their grouping and the order in which a reader encounters them changes.
- `cargo check --release --all-targets` clean (defense-in-depth against accidental code change during the docs cycle).

## [0.2.5.1] — 2026-04-27

Cleanup release. The query language gains a safety net for unbounded SCANs (default LIMIT + opaque CURSOR pagination), closes four `xytalk-spec.md` divergences (WHERE on standalone `SET`/`DELETE`/`LINK`; `INCACHE`/`OUTCACHE` rewritten with nom + documented), introduces `xyzdb-cli admin` as the successor surface for operator commands, and renames the language from xyzQL to **xyTalk** with no semantic, on-disk, or wire-protocol change.

### Highlights

- **Cursor pagination on SCAN.** Postcard-encoded `CursorPayload` wrapped in URL-safe base64; filter checksum binds the cursor to its WHERE clause.
- **`SCAN_LIMIT_DEFAULT = 1000`** safety cap on unbounded SCAN (with `tracing::warn`); hard ceiling `SCAN_LIMIT_HARD_MAX = 10000`.
- **Language renamed `xyzQL` → `xyTalk`**, crate `xyzdb-parser` → `xytalk-parser`. Pure cosmetic — semantics, on-disk format, and wire protocol byte-identical.

### Added

- **`SCAN ... CURSOR "<token>"` clause** + AST extension (commit `8397909`). Parser captures the opaque token; engine decodes, validates lobe + filter checksum, and seeks the SCAN to the next page.
- **Engine safety net for SCAN** (`c82a6d9`). `SCAN_LIMIT_DEFAULT` applied when both `LIMIT` and `ORDER BY` are omitted; emits `tracing::warn`. `SCAN_LIMIT_HARD_MAX` rejects `LIMIT N > 10000` with a pointer to chunked-streaming SCAN. Aggregates do not apply the default.
- **Cursor module + `QueryResult::PaginatedRecords` variant + Primary SCAN seek** (`3ab93d1`). `xyzdb-engine/src/cursor.rs` (postcard + xxh3 + URL_SAFE_NO_PAD base64). New `PaginatedRecords { records, cursor, has_more }` variant emitted when a SCAN exceeds the page boundary; existing `Records` variant preserved for queries that fit completely. Cursor + ORDER BY and cursor + ghost routing rejected explicitly — paginated sort + ghost cursors are v0.3 scope.
- **`WHERE` on standalone `SET` / `DELETE` / `LINK`** (`67b7507`). Closes a documented spec-vs-parser divergence: pre-v0.2.5.1 the parser rejected `SET "lobe" field = value WHERE …` and equivalents. LINK now accepts WHERE on both source and target — multi-record LINK is addressable without going through `PUT … LINK TO`.
- **`xyzdb-cli admin <verb>` subcommand** (`28d549e`) covering `compact`, `analyze <lobe>`, `bulkmode <on|off>`, and `migrate <lobe> | --all`. Thin wrapper over the existing V1 protocol; no engine logic moved. Canonical operator surface from v0.3 onwards when the language-statement form is dropped.
- **`LANGUAGE_AND_INTERFACES.md`** root reference doc (`43f8ea2`) — exhaustive map of the surface area: every xyTalk statement, all wire protocols (V1/V2/V3 + STATS short-circuit), all clients, SDKs and drivers in the repo, and a catalog of forward-looking surfaces (MCP, PG wire shim).

### Changed

- **Default LIMIT 1000 on plain SCAN without LIMIT** (`c82a6d9`). Behaviour change vs prior unbounded scan: queries that previously returned all records now return at most 1000 + emit a `tracing::warn` on the server. Add explicit `LIMIT N ≤ 10000` or paginate via `CURSOR` for larger result sets. Aggregate paths (`SCAN | AGGREGATE`, `SCAN | GROUP BY | AGGREGATE`) are not affected.
- **`INCACHE` / `OUTCACHE` rewritten with nom** (`b9e684b`). Bare keywords now reject at parse time with a clear error; pre-fix hand-rolled byte-slicing produced `OutCache("")` / `InCache{lobe:""}` silently and surfaced the empty-lobe error only at engine resolution. Lobe identifier accepts both quoted and unquoted forms (consistency with PUT/SCAN/FIND); INCACHE WHERE uses the V4 boolean-expression grammar. FAIL-pre verified by revert+run on the two bare-keyword rejection tests.
- **`COMPACT` / `ANALYZE` / `BULKMODE` / `MIGRATE` deprecated as language statements** (`28d549e`). Statements still execute; engine emits `tracing::warn` pointing at `xyzdb-cli admin <verb>`. **v0.3 will retire the language form**; new code should target the admin CLI. No breakage for existing drivers, validation suites, or external clients in v0.2.5.x.
- **Language renamed `xyzQL` → `xyTalk`** (`c958dd9`). Crate `xyzdb-parser` → `xytalk-parser`. Spec file `docs/xyzql-spec.md` → `docs/xytalk-spec.md`. Wire protocol bytes unchanged (V1/V2/V3 framing identical). CHANGELOG entries pre-v0.2.5.1 retain the original wording (historical record).

### Spec additions

- **`docs/xytalk-spec.md` §2.18 — INCACHE / OUTCACHE**: grammar, examples, operational notes (cache budget, error shape, parser rejection of bare keywords).
- **`docs/xytalk-spec.md` §2.19 — Admin statement deprecation**: documents the `xyzdb-cli admin` successor surface; clarifies that INCACHE/OUTCACHE are operator-grade *workload tuning* (not admin), while COMPACT/ANALYZE/BULKMODE/MIGRATE migrate out of the language in v0.3.

### Internal

- **xyTalk parser tests**: 36 → 55 (3 cursor + 4 WHERE-standalone + 7 INCACHE/OUTCACHE + 5 historical batch coverage).
- **xyzdb-engine**: integration 107 → 119 (4 LIMIT/cursor reject + 4 cursor pagination + 3 WHERE-standalone + 1 admin regression guard); unit 64 → 69 (5 cursor module).
- **xyzdb-server e2e**: 2/2 unchanged.
- **`fuzz/Cargo.lock` removed** (orphan; the fuzz crate is a member of the root workspace and uses the root lockfile).
- **External-consumer audit pre-rename**: no internal project referenced `xyzdb-parser` or `xyzQL`, so the rename broke nothing. Not published on crates.io.

### Migration

- **In-tree consumers**: all updated. `xyzdb_parser::*` imports → `xytalk_parser::*`. Cargo dep paths updated in `xyzdb`, `xyzdb-engine`, `xyzdb-server`, `xyzdb-bench`, `validation`, `fuzz`, and bench drivers.
- **External consumers**: no public crates.io dep on `xyzdb-parser`. No external action needed.
- **Cursor tokens are version-bound**: the filter checksum is derived from `format!("{:?}", filter_expr)`, which depends on the parser AST's Debug impl. A future release that adds or renames a `FilterExpr` variant will invalidate in-flight cursors. Cursors are ephemeral pagination state — accept the trade-off; document any `FilterExpr` changes in the corresponding release entry.

## [0.2.3.1] — 2026-04-23

Consolidation release. Closes the Section 2 audit items 4-7 deferred from v0.2.3 and two D1 gaps discovered during the audit in operational command paths. No new feature surface; no on-disk format change.

### Added

- Subprocess-based crash recovery tests in `turba-engine/tests/crash_recovery.rs`: `crash_after_acked_writes_preserves_them` (always-on) and `finding_9_paused_sync_writer_blocks_before_ack` (feature-gated). Real fork + SIGKILL, complementary to the in-process `mem::forget` tests.
- WAL state-machine static reference doc — states, transitions, composite operations, invariant D1, sentinels, failure modes, compliance quick-reference. Section 2 item 7 output.

### Fixed

- **D1 ack gap in `execute_autoanchor_apply`** (commit `5d8b000`). The handler returned `Ok("X indexed")` to the client after `let _ = self.turba.dictionary.insert(...)` — errors discarded, no seal/flush before ack. Fix: propagate error via `?`, seal + flush the dictionary before returning. Discovered by the Section 2 item 5 writer-ack audit.
- **D1 ack gap in `persist_pinned` (PIN / UNPIN)** (commit `3160fe9`). Same shape: insert + caller-visible `Ok` without seal/flush. Fix: seal + flush inside `persist_pinned` before returning.

### Changed

- `set_thresholds` and `recent_count` in `scan_telemetry.rs` gated behind `#[cfg(test)]` (commit `ee7d0ad`). Test-only accessors; release builds no longer carry the symbols. `cargo check` now warning-free.

### Internal

- Section 2 audit items 4 (invariant test coverage map), 5 (writer ack paths), 6 (sync/replay advancement) closed at the documentation level. Item 7 closed with the state machine reference above. All deferred cluster closure items now done.

## [0.2.3] — 2026-04-22

Closes the durability cluster (Findings 8, 9, 10) discovered in the v0.2.2 diagnostic window, and ships the first operator-facing observability surface: `/stats` JSON endpoint, separable `major_ok` counter, in-loop progress log for long compactions, and sync-thread health fields. Tagged local, not published.

### Added

- **`/stats` JSON health endpoint** on `xyzdb-server` (port 2505, V1/V2 text protocol). Command `STATS` or `SHOW STATS`, short-circuited in the connection handler. Returns per-keyspace levels/memory/compact counters, block cache weight/hits/misses, ghost counts, Linux `VmRSS` / `VmData` / cgroup probes, and sync-thread health. Consume via `echo STATS | xyzdb-cli | jq`.
- **Sync-thread health fields** (`sync_thread.last_successful_sync_ts_ms`, `sync_thread.heartbeat_count`) under `/stats`. Heartbeat increments every sync-loop iteration; timestamp advances only on successful `journal.sync()`. Distinguishes "thread dead" from "thread alive, every fsync failing".
- **`Tree::major_compact_success_count()`** metric (separable from the existing `compact_success_count`). Surfaced in the `reap-cycle` log as `major_ok=N` alongside `compact_ok=N` and `compact_err=N`; also exposed via `/stats`.
- **In-loop progress log** inside `major_compact_with_observer`, emitted to stderr every 60 s during long-running compactions: `turba-compact: major_compact in progress: tree=<name> iteration=<n> inputs_consumed=<x> initial_inputs=<y> output_tables=<z>`.
- **`/// # Durability` rustdoc sections** on every public surface that mutates WAL state or relies on in-SSTable invariants: 11 functions across `turba-engine::{journal::writer, engine, tree}`. Each states precondition + postcondition. Closes the durability cluster static audit.

### Fixed

- **Finding 8 (path A):** `Engine::major_compact` rotated the WAL without sealing active memtables first, losing acknowledged writes still resident in the active memtable on a subsequent crash. Fix: `seal_active()` on every tree before `tree.major_compact()` (whose internal `flush_sealed()` then persists the sealed content) and only then `journal.rotate()`. Establishes durability cluster invariant **D1**: every acknowledged write is in an SSTable before any caller invokes `JournalWriter::rotate`.
- **Finding 8 (path B):** same bug class in `execute_compact` (xyzdb-engine), the entry point used by the server's `COMPACT` command. Missed in the initial Finding 8 triage; discovered during the Phase 5 empirical re-run on 2026-04-21 when the same drift signature reappeared through a different route. Fix: mirror the seal-before-rotate sequence in `execute_compact` for spatial, identity, and dictionary keyspaces.
- **Finding 9 (writer ack, primary):** `WriteBatch::commit` under Durable mode used `condvar.wait_timeout(5 ms)` as the group-commit barrier. On timeout, the writer returned `Ok` to the client without verifying `synced_epoch` had advanced past its own epoch — a crash in the remaining ~995 ms of a 1 s cycle could acknowledge writes that were not on disk. Fix: replace with `while synced_epoch.load(...) < epoch { notify.wait(...) }` until the sync thread signals advancement. The writer no longer returns without an on-disk guarantee.
- **Finding 9 (sync thread, secondary):** the sync thread advanced `synced_epoch` on every iteration where it acquired the journal lock, regardless of whether `j.sync()` actually returned `Ok`. A failed fsync still woke waiting writers and acknowledged their batches. Fix: match on `j.sync()` and advance + notify only on `Ok`; log and retry on `Err`.
- **Finding 10 (WAL janitor):** the background WAL janitor rotated the journal on a `min(flushed_seqno)` watermark across keyspaces without ever calling `seal_active()`. Any write still in an active memtable at janitor-fire time was lost on crash because the WAL truncation removed its recovery record. Fix: disable the janitor in production builds pending a correct implementation. The janitor now only runs under the `durability-test-hooks` feature for regression testing. Operators rely on explicit `COMPACT` for WAL truncation; tradeoffs documented under Changed. Alternative janitor designs (seal/flush precondition enforcement; full removal) tracked for v0.3 in `TODO-v0.2.3.md` §6.
- **`zone_maps` redecode per block filter call** (`tree/mod.rs:367`): `decode_zone_maps` was invoked inside the block-filter closure on every filtered block of every SSTable, re-parsing the same opaque blob O(N) times per scan. Fix: decode once into owned `Vec<Vec<u8>>` before the closure captures it. Unquantified wall-clock improvement; eliminates the unused-variable warning that originally exposed the bug.
- **Sequential benchmark generator** (`benchmarks/sequential/harness`): `build_config_records` emitted only 2 of the 5 configured config-entity types, biasing the workload shape in published numbers.

### Changed

- **Progress log field naming** in the `major_compact in progress:` line: `inputs_processed=N/M` replaced with `inputs_consumed=N initial_inputs=M` (no slash). N is cumulative across loop iterations and can exceed M because cascading leveled compaction re-consumes outputs from earlier iterations as inputs of later ones; the slash form misread as invalid fractional progress (observed at `iteration=1498 inputs_processed=2977/327`).
- **WAL janitor disabled in production.** The background thread that truncated the WAL on a `min(flushed_seqno)` watermark no longer runs in production builds; see Finding 10 above. Operational implication: long-running deployments without periodic `COMPACT` accumulate WAL bytes indefinitely until restart. Mitigation for v0.2.3: operators schedule `COMPACT` explicitly (same effect, safe path). A safer always-on janitor is deferred to v0.3.

### Internal

- **Finding 8 regression tests**, one per known path: `finding_8_major_compact_seals_active_before_wal_rotate` in `turba-engine/tests/durability_proptest.rs` (path A); and `finding_8_path_b_execute_compact_seals_active_before_rotate` in `xyzdb-engine/tests/integration.rs` (path B). Both simulate SIGKILL via `mem::forget` to bypass `Drop`'s graceful seal+flush.
- **Phase 5 `--phase verify`** in the sequential benchmark harness (`benchmarks/sequential/harness`). Deterministic per-lobe record count check after reopen. This is the harness that empirically surfaced Finding 8 path B during the v0.2.3 validation cycle.
- **`durability-test-hooks` Cargo feature** on `turba-engine`. Keeps the WAL janitor alive (otherwise disabled in production builds — see Fixed / Finding 10) and exposes `_test_pause_sync` / `_test_synced_epoch` hooks used by the Finding 9 and Finding 10 regression tests.

### Known limitations

- **Section 2 audit items 4-7** (per-caller invariant tests, writer ack path audit, sync advancement audit, WAL state-machine design doc) deferred to v0.2.3.1 / v0.2.4. The durability cluster is closed on the three known members (Findings 8, 9, 10); the deferred items widen the audit to catch future regressions statically. See `TODO-v0.2.3.md` §2.
- **Finding 9 subprocess regression test** (a real SIGKILL-after-ack scenario under a separate process, not `mem::forget` in-process simulation) is intentionally out of scope. The current proptest exercises the same invariant without subprocess infrastructure.
- **Finding 7** (HDD Scale 1.0 post-load `major_compact` asymptotic stall) remains OPEN; scope is v0.3. v0.2.3 ships the observability (`/stats`, `major_ok`, in-loop progress log) that will let the v0.3 investigation characterise the stall with first-minute signal instead of overnight forensics.

## [0.2.2] — 2026-04-20 — retroactive, do not use

**Retroactive entry, no published binary.** Local tag on commit `53767e3` snapshotting the point where Finding 6 (RAM regression — L1 starvation + cleanup lag + jemalloc + meta-dedup) closed and the durability cluster (Findings 8/9/10) was discovered but not yet fixed. The full cluster fix ships in v0.2.3 and supersedes v0.2.2 entirely.

**Do not use.** Any v0.2.2 build can lose acknowledged writes on crash through three different paths (Findings 8/9/10). Upgrade to v0.2.3 — all three are closed.

The tag exists in git history so anyone bisecting hits an identifiable snapshot rather than an unlabeled mid-cycle commit.
## [0.2.0-alpha] — 2026-04-17

Phase 1 of the v0.2 roadmap: auto-ghost lifecycle (Ephemeral / Promoted / Permanent), operator-aware router, TTL reaper, LRU eviction. `-alpha` signals that known items remain open for v0.2.0 stable — see "Known issues" below.

### Added

#### Auto-ghost lifecycle

- `GhostType` enum on `GhostMeta` / `PersistedGhostMeta`: `Permanent` (manual `CREATE GHOST`, no TTL), `Ephemeral` (auto-created from scan telemetry, 24h TTL, 10 per lobe), `Promoted` (Ephemeral accessed 7 consecutive days, 30d TTL, 5 per lobe).
- `maybe_create_ephemeral_ghost` consumes `AutoGhostCandidate` from `ScanTelemetryStore`, spawns a background worker that runs `ghost_manager.create()`, reclassifies the resulting ghost as `Ephemeral`, and registers it in the router with operator-aware filter tuples + `filter_desc`.
- Ghost name generation via `xxh3_64(filter_desc)` → `auto_{lobe}_{hash:016x}`. Deterministic across threads; concurrent scans racing on the same pattern converge on a single `create()` call.
- In-memory access tracking on every ghost-routed read: `last_accessed`, `access_count_total`, `daily_access_bitmap`. Not re-persisted per-read (dictionary write overhead avoided at ~150 ops/s of sustained read traffic).
- `GhostLobeManager::rotate_bitmaps_if_needed` slides the 7-day access bitmap on UTC-day boundaries. Used by promotion detection.
- `GhostLobeManager::identify_promotable` returns Ephemerals with `daily_access_bitmap & 0x7F == 0x7F` (seven consecutive days).
- `GhostLobeManager::promote_ghost` mutates in place: same `ghost_id` and keyspace entries, new name (`promoted_<suffix>`), `GhostType::Promoted`, `ttl_seconds = 30d`. No spatial re-scan, no gap in covering index.
- `GhostLobeManager::identify_lru` + `evict_lru_at_limit` + `Engine::enforce_ghost_type_limit`: LRU eviction with strict pre-limit check (count >= max → drop LRU before create). Enforced at 10 Ephemeral / 5 Promoted per lobe.

#### TTL reaper

- Background thread spawned by `Engine::into_arc`. Weak reference to `Engine`; exits automatically when the last `Arc<Engine>` is released. No explicit shutdown API.
- 60-tick loop with 1s sleeps + `Weak::strong_count()` check each tick — shutdown latency ≤ 1s.
- `Engine::reap_cycle(current_day_bucket, &mut last_rotation)` runs three phases per minute: drop expired → cascade unregister from router + clear `ghost_created` flag in telemetry → rotate bitmaps on UTC-day advance → promote Ephemerals with 7-day bitmap.
- Day bucket arithmetic via `now_micros() / MICROS_PER_DAY` (no `chrono` dep).

#### Router

- `GhostRoutingMeta.filter_fields`: `Vec<(String, Value)>` → `Vec<(String, FilterOp, Value)>`. Pre-v0.2 the router matched field + value only and hard-coded `FilterOp::Eq`, so ghosts built for Gt / Contains / OR never routed. `plan_scan` now compares operator as well.
- `GhostRouter::set_filter_desc` + `get_filter_desc` + `rename_ghost`. `plan_scan` tries `filter_desc` equality first for OR / complex expressions before falling through to tuple matching.
- Removed the OR early-exit in `execute_scan` that hard-wired OR queries to `ScanSource::Primary`. The router now decides internally.

#### Persistence

- `MANIFEST_VERSION` bumped 1 → 2. `read_manifest` returns the dedicated `Error::IncompatibleFormat { found, expected }` when opening a v0.1 data directory, with a message instructing the operator to delete and re-ingest.
- `GHOST_META_FORMAT` const at `0x03`. Single format byte at the head of every persisted ghost meta record. Postcard is sequential and does NOT respect `#[serde(default)]` on trailing fields — schema evolution handled by bumping this byte; `load_all` skips records with an unrecognized byte and logs a recreate-via-`CREATE GHOST` hint.
- `ScanTelemetryStore` fields for detection thresholds: `min_hits: u64` (default 5), `min_latency_ms: f64` (default 20.0, lowered from pre-v0.2 const 500.0). `pub(crate) fn set_thresholds` for test injection.
- Boot-time TTL check in `load_all`: ghosts whose persisted `last_accessed > ttl_seconds` are purged before entering the runtime map. Shared `purge_ghost_data` helper between `drop_ghost` (runtime) and `load_all` (boot).
- Sliding 10-minute pattern window with `RECENT_HITS_CAP = 100` per pattern. Cap-on-write + filter-on-read: bounded memory, accurate trigger gate.

#### Testing

- `fuzz/` crate at project root with three cargo-fuzz targets: `sst_parse_block`, `wal_parse_record`, `xytalk_parse`. Virtual workspace `Cargo.toml` at project root to let cargo-fuzz resolve the parent.
- `turba-engine/tests/durability_proptest.rs`: proptest durability invariant (random committed batches, drop engine, reopen, every pair still readable).
- `turba-engine/tests/supversion_loom.rs`: loom model gated by `--cfg loom`.
- `[workspace.lints.clippy]` in both workspaces: `unwrap_used = warn`, `expect_used = warn`, `undocumented_unsafe_blocks = deny`, `missing_safety_doc = deny`.
- 64 tests added across the Phase 1 commits (31 → 64 lib + 99 integration in `xyzdb-engine`, plus new turba-engine proptest + loom harnesses).

### Changed

- **Breaking on-disk format.** v0.1 data directories are not readable by v0.2.0-alpha. Opening one returns `Error::IncompatibleFormat`. No migration path — operators delete and re-ingest.
- **Router shape change.** `GhostRoutingMeta.filter_fields` is a three-tuple now. Ghosts persisted in v0.1 would not register correctly under v0.2.0-alpha's router, but this is moot because v0.1 data is rejected on open (see above).
- **Telemetry threshold default.** `AUTO_GHOST_MIN_LATENCY_MS = 500.0` → `20.0`. Chosen as the latency where automatic optimization starts paying off in mixed workloads.
- **`mark_ghost_exists` removed.** Replaced by symmetric `set_ghost_flag(filter_desc, exists: bool)` on `ScanTelemetryStore`.

### Fixed

- **WAL parser OOM (Phase 0 finding).** `journal::entry::parse_one_batch` pre-allocated `Vec::with_capacity(item_count)` where `item_count` came from untrusted bytes; a crafted `u32::MAX` value triggered a 4.3B-item reservation before any bounds check. Fix: drop the capacity hint; per-item bounds checks still gate growth. Regression test `malicious_item_count_does_not_oom`.

### Performance

**Reference hardware**: MacBook Pro M4 Pro · 24 GB RAM · 512 GB SSD · macOS 26.4.1 · Docker Desktop, T6 container (2 CPU, 8 GB RAM). Reproducible with seed 42.

**Scale 0.1 × 1h concurrent workload** (balanced: 4 readers + 2 writers, fintech suite), same generator as v0.1, apples-to-apples:

| Metric | v0.1 | v0.2.0-alpha | Delta |
|---|---|---|---|
| Writes/s (records) | 10,209 | 10,210 | +0% |
| Reads/s | 152 | 237 | **+56%** |
| Read P50 | 9.91 ms | 6.11 ms | -38% |
| Read P95 | 94.08 ms | 75.26 ms | -20% |
| Read P99 | 377.34 ms | 107.39 ms | **-72%** |
| Read max | 1,297 ms | 5,792 ms | regression (one outlier in 853K reads) |
| Read errors | 0 | 25 | regression (0.003%, see Known issues) |
| Peak RAM | 1,723 MB | 2,219 MB | +29% |
| CPU (avg) | 200% | 200% | = |
| Integrity | EXACT | EXACT | |
| Records written | 36,751,396 | 36,754,270 | |

Per-query P50:

| Query | v0.1 | v0.2.0-alpha | Delta |
|---|---|---|---|
| Q1  Point  | 2.57 ms | 1.28 ms | -50% |
| Q2  PULL   | 2.59 ms | 1.26 ms | -52% |
| Q3  Agg    | 2.64 ms | 2.75 ms | +4% (PreComputed, expected flat) |
| Q4  TopN   | 14.51 ms | 12.18 ms | -16% |
| Q6  Dot    | 20.75 ms | 17.73 ms | -15% |
| Q7  Contains | 52.70 ms | 15.89 ms | **-70%** |
| Q8  GroupBy | 2.64 ms | 1.51 ms | -43% |
| Q9  OR     | 21.90 ms | 21.77 ms | -0.6% (see Known limitations) |
| Q10 Nested | 28.51 ms | 18.85 ms | -34% |

### Known limitations (by design)

- **Auto-ghost creation targets AND-shaped filter expressions.** Pure-OR queries route to existing manual or promoted ghosts via `filter_desc` (Step 0 plumbing) when available, but do NOT trigger auto-ghost creation themselves. The creation gate requires non-empty flat filters for selectivity — a ghost with no filter would index the entire lobe and degenerate into a second copy of the spatial keyspace. For OR-heavy workloads, use manual `CREATE GHOST` with specific filter variants, or await v0.3 (multi-predicate ghosts with OR-native indexing).
- **Auto-ghost under high parameter variety shows churn.** The bundled benchmark randomizes Q6/Q7/Q10 parameters uniformly over 7/6/18 variants, producing 31 distinct filter_desc values per lobe. With the 10-Ephemeral cap, ghosts rotate via LRU (observed 243× create/evict ratio). Workloads with stable filter values — typical production: dashboards with fixed thresholds, reports with predefined segments — see full benefit without churn. v0.3 Promoted with range indexing closes the gap for parameterized workloads.
- **Lifecycle tracking is in-memory only.** `last_accessed`, `access_count_total`, `daily_access_bitmap` reset to zero on boot. Ephemerals get a fresh 24h lease from reboot, not from last real access. High restart frequency (<7 days) prevents promotion detection. Acceptable for v0.2 target (always-on servers).

### Known issues (fix planned for v0.2.0 stable)

- **Transient `Ghost not found` errors** under high auto-ghost churn. 25 occurrences in 853K reads (0.003%) during the 1h benchmark. TOCTOU race between `router.plan_scan` returning a ghost name and `ghost_manager.read_topn` executing with that name — the ghost can be LRU-evicted in between. All 25 errors on the same class. Fix: transparent fallback to Primary on `GhostNotFound`. Estimated ~10 lines.
- **2,083 `data corruption: bad X` stderr lines** from the compact thread during the 1h benchmark. Source: SSTable meta parser sanity checks (`turba-engine/src/table/meta.rs`) rejecting numeric fields whose byte-length doesn't match. Zero impact on data integrity — failed compactions retry, outputs not committed on error, and the run ended with EXACT record count (38,519,451 tracked = 38,519,451 in DB). Suspected race between concurrent SSTable meta reads (compact thread) and writes (flush thread) amplified by ghost keyspace churn. Rate ~35/min, uniform distribution across the run. Under investigation for v0.2.0 stable.
- **Max read latency regression.** 1,297 ms (v0.1) → 5,792 ms (v0.2.0-alpha). One outlier in 853,600 reads (0.0001%). P99 improved 72% globally; a single extreme case regressed. Likely shared-lock contention during concurrent ghost creation + compaction + bump_access. Under investigation.

### Tests

- `cargo test -p xyzdb-engine` green: 63 lib + 99 integration.
- `cargo test` on `turba-engine` green: all test files pass.
- `cargo +nightly fuzz run <target>` clean 60s smoke on each of the three fuzz targets (24h runs pending post-release).

### Migration from v0.1

No migration code. v0.1 data directories are rejected on open with `Error::IncompatibleFormat`. Operators:

1. Stop v0.1 server.
2. Delete data directory.
3. Start v0.2.0-alpha server with clean data directory.
4. Re-ingest.

Manual `CREATE GHOST` statements from v0.1 remain syntactically compatible and must be re-issued against the fresh data directory.

---

## [0.1.0] — 2026-04-16

First public release. Feature-complete materialized-view database over a custom LSM engine, with reproducible benchmarks against PostgreSQL 18, MongoDB 8, and SurrealDB 3.0.5.

### Added

#### Turba storage engine

- ArcSwap-based SuperVersion for zero-lock reads; writers coordinate through a version-update mutex without blocking readers.
- WAL group commit with a dedicated 1ms sync thread; batched fsyncs across concurrent writers. `PersistMode::SyncData` and `PersistMode::Buffer` modes.
- L0-based write throttle replacing the earlier read-latency model. Healthy → Degraded (L0 > 8) → Critical (L0 > 16) → Paused (sealed > 3).
- Backpressure with 50ms non-fatal timeout post-memtable; clients no longer lose records on transient pressure.
- Zone maps: per-block min/max via the `ZoneMapBuilder` trait. Scans skip blocks whose range can't satisfy the predicate via `SSTableBlockIter::with_block_filter`.
- Compaction rate limiter: 100 MB/s for background compactions, unlimited for manual `major_compact`.
- Direct I/O hints for compaction (`F_NOCACHE` on macOS for input eviction, `FADV_DONTNEED` on Linux containers).
- `cleanup_orphan_ssts` with a `max_referenced` guard — safe without locks against concurrent flushes.
- `ZstdDict` compression type variant (infrastructure only; not yet wired into the compaction pipeline).
- Four-keyspace model per database: `spatial`, `identity`, `dictionary`, `ghosts`. Each has independent memtables, SSTables, flush threads, and compaction.

#### xyzDB query layer

- Ghost V2 complete: filter + order_by + group_by + aggregate, with post-write hook maintenance.
- `EMBED` keyword projects source fields into ghost entries. Top-N reads return rows without a single point read back to the source lobe.
- Streaming `prefix_iter` in ghost reads (replaced the previous `Vec`-loading implementation).
- Ghost memtable flush in `notify_write` (the ghost memtable was previously never flushing, causing incremental maintenance to stall).
- Sort-before-ingest: entries are sorted before `ingest_sorted` to satisfy the LSM's sorted-input invariant.
- Seqno visibility fix in `ingest_sorted` (entries were being MVCC-hidden due to an incorrect sequence number baseline).
- Manifest always persisted after `ingest_sorted`, regardless of `compaction_enabled`.
- `SCAN GHOST` default limit removed (was 1000 — now unlimited).
- Major compaction of the ghost keyspace after create, draining L0 fragmentation that was slowing top-N reads.
- `AutoAnchor`: detects high-cardinality UNIQUE fields and promotes them to anchor status automatically.
- Full xyzQL grammar in the parser: `FIND`, `SCAN`, `PULL`, `CREATE GHOST` (with `AGGREGATE`, `EMBED`, `ORDER BY`, `GROUP BY`), `PUT`, `LINK`, `LOBE`, `ANCHOR`, `BULKMODE ON/OFF`, `COMPACT`, `INCACHE`/`OUTCACHE` (parser only, cache logic stubbed).

#### Protocols

- V1 text protocol: `[version=1:u8][length:u32 BE][utf8 xyzQL]`.
- V3 binary bulk protocol: frame-based, postcard-serialized records, supports CREATE BATCH for fast load.
- Connection idle timeout (300s) and `MAX_FRAME_SIZE` 16 MB.

#### Benchmarks

- Sequential harness (`benchmarks/sequential/harness/`): full ingestion + 10 queries × 10 runs, P50/P95/P99. Generates deterministic fintech dataset (seed 42) at configurable scale 0.001–1.0.
- Concurrent harness (`benchmarks/concurrent/`): autonomous closed-loop workload. N readers + M writers run in parallel, writers generate data while readers query. Reports sustained throughput + latency percentiles over windowed 30s intervals.
- Four engine drivers: xyzDB (V1 + V3), PostgreSQL 18 (tokio-postgres), MongoDB 8 (mongodb crate), SurrealDB 3.0.5 (direct `reqwest::blocking` HTTP, bypassing the SDK).
- Six workload profiles (`read-only`, `write-only`, `read-heavy`, `balanced`, `write-heavy`, `stress`) and two suites (`fintech`, `ai-context`).
- Docker Compose tier configuration (T1–T6) with CPU, memory, and cache-size limits per engine.
- Integrity verification per run: atomic counter vs DB count, reporting `EXACT` / `OK` (within 1% in-flight) / `MISMATCH`.

#### Documentation

- Master README with benchmark headline, architecture diagram, and design trade-offs.
- Per-crate READMEs for `turba-engine`, `xyzdb`, and `benchmarks`.
- Release notes.
- Architecture reference, xyzQL grammar, benchmark results, and v0.2 roadmap (see `docs/` in v0.2.1 and later — individual files listed in `docs/README.md`).

### Performance

**Reference hardware**: MacBook Pro M4 Pro · 24 GB RAM · 512 GB SSD · macOS 26.4.1 Tahoe · Docker Desktop, tier T6 container (2 CPU, 8 GB RAM). All numbers reproducible with seed 42.

**Scale 0.1 × 1h concurrent workload** (balanced: 4 readers + 2 writers):

- xyzDB: 10,209 writes/s, 152 reads/s, RAM peak 1.7 GB, EXACT integrity
- PostgreSQL 18: 6,594 writes/s, 14.7 reads/s
- MongoDB 8: 9,665 writes/s, 6.3 reads/s
- SurrealDB 3.0.5: 501 writes/s, 43.2 reads/s (via direct HTTP; SDK `.query()` hangs — see Known Issues)

**Scale 1.0 sequential** (406,899,663 records, SSD):

- Ingestion: 170K rec/s sustained over ~40 min
- Q3 aggregate overdue (42M rows): 0.58ms (PreComputed ghost)
- Q8 GROUP BY status: 0.29ms (PreComputed ghost)
- Q4 top-10 balance: 73ms (EMBED ghost)
- Q10 nested state filter: 13.8ms

### Tests

- 99 integration tests in `xyzdb-engine` covering ghost lifecycle, co-location invariants, null handling, streaming iterators, GROUP BY, OR compound filters, dot notation, honeycombed layouts.
- Full test coverage in `turba-engine` for MVCC visibility, compaction correctness, WAL recovery, zone map invariants, and block cache eviction (6 test files).
- 11 validation suites in `xyzdb/validation/` for bulk load, read patterns, write stress, mixed workload, connection pooling, durability + crash recovery, edge cases, autodiscovery, scale curve, and 24h endurance.

### Known issues

- **Scale 1.0 COMPACT duration**: 2h 47min on SSD due to fused observer path. Separate COMPACT from CREATE GHOST is landed but not yet validated at scale 1.0 — planned for the v0.2 validation pass.
- **Q6/Q7/Q9 sequential regression**: zone map deserialization overhead on SSD (scan queries that don't benefit from zone-map skipping). Per-storage-profile disable is landed but pending validation at scale 1.0.
- **SurrealDB SDK hang**: the official `surrealdb` Rust SDK v3.0.5 `.query()` method hangs indefinitely after extended use, reproduced with both WebSocket and HTTP engine paths and both surrealkv and rocksdb backends. Our driver uses `reqwest::blocking` directly against `/sql` as a workaround. Not a xyzDB issue; documented in `benchmarks/README.md` for transparency.

### Migration notes from internal development

These changes occurred during pre-v0.1 development and are listed for completeness — they do not affect anyone adopting v0.1.0 as their starting point:

- Ghost V1 (copy-based, ~200 bytes/record): removed, replaced by V2 (reference-based, 18 bytes + optional EMBED projection).
- `xyz-engine` crate renamed to `turba-engine`. Struct renamed from `XyzEngine` to `TurbaEngine`. Thread and error-message identifiers updated.

---

