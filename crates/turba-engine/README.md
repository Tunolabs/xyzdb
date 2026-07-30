# turba-engine

Custom LSM-tree storage engine that powers xyzDB. Single crate, library-only, no network, single-tier.

**Version 0.2.0.** Headline deltas over 0.1.0: a fifth `vectors` keyspace, an
automatically-bounded WAL, cacheable per-SST metadata, an SSTable footer
checksum, a per-key merge operator, `SCRUB` integrity verification plus a round
of crash-safety hardening, and the removal of the multi-tier tiered-placement
experiment (Turba is now single-tier). Details below.

## What's in it

- **Five independent keyspaces** per database: `spatial`, `identity`, `dictionary`, `ghosts`, and `vectors` (the v0.8 per-record vector column). Each is a first-class LSM tree — own memtables, SSTables, compaction worker, bloom filters (`KS_SPATIAL = 0 … KS_VECTORS = 4`).
- **ArcSwap SuperVersion** for zero-lock reads. Readers acquire an `Arc<SuperVersion>` atomically; writers swap versions through a `Mutex` without blocking readers.
- **Atomic write batches.** `engine.batch()` collects per-keyspace `put_*` / `remove_*` and `commit()`s them as one group with a single durability point, returning the assigned `SeqNo`.
- **WAL group commit.** A dedicated sync thread runs every 1 ms, batching fsyncs across concurrent writers. `PersistMode::SyncData` waits on a Condvar until its epoch is fsynced; `PersistMode::Buffer` does not. Per-writer epoch tracking closes the ack-before-fsync gap (v0.2.3).
- **Bounded WAL.** The active segment rolls to an archived `journal.<seqno>.wal` at `wal_segment_max_bytes` (default 64 MB); segments fully below the cross-keyspace manifest-durable watermark are pruned in the background, so the WAL stays at ~a couple of segments instead of the whole write history. The watermark is the minimum manifest-durable seqno across keyspaces — idle keyspaces never pin it.
- **WAL D1 invariant.** `rotate_journal()` and any advance of a durability sentinel (`synced_epoch`, `flushed_seqno`, `last_rotated`) must establish — before the call — that every acked write is already in an SSTable. Every D1 caller, ack path, and sentinel advance is audited (v0.2.3.1).
- **Cacheable per-SST metadata.** Zone maps (per-block min/max, SSTable meta tag 12), bloom filters, and the block index are loaded on demand into the block cache rather than held resident, so RAM scales with the working set instead of the table count. Scans consult zone maps via `SSTableBlockIter::with_block_filter` and skip blocks whose range can't satisfy the predicate.
- **SSTable footer checksum** (3f-meta). The footer carries an xxh3 checksum, so a torn or corrupt footer is detected at open instead of being silently mis-read.
- **Per-key merge operator.** Read-fold and compaction-collapse halves let a key accumulate partial updates that fold to a single value, with each distinct seqno applied exactly once (the merge double-count race is closed and compaction is serialized per tree).
- **Compaction.** Leveled, dual-criterion overflow scheduling: each level's overflow ratio = `max(bytes_ratio, count_ratio)`; the highest-ratio level compacts next. The L0 emergency cap (`L0_EMERGENCY_RATIO = 3.0 × max_l0_tables = 4 → 12`) drains L0 first to bound point-read latency. The background worker rate-limits at 100 MB/s; `major_compact` runs unrate-limited and converges with bounded write-amplification.
- **MVCC** with 64-bit sequence numbers. Snapshot isolation for readers; tombstones evicted at the last level.
- **Compression** per-level: LZ4 at L0 (fast), Zstd-3 at L2+ (dense). A `ZstdDict` trained-dictionary variant exists in `compression.rs` but is not yet wired into the flush/compaction path.
- **Storage profile.** `StorageProfile::Hdd` widens block sizes (e.g. spatial 32 KB → 64 KB) and increases bloom bits per key (10 → 14) to amortise seeks on rotational media. An independent `IoSchedulerMode` opts into a lane-aware I/O scheduler.
- **Crash-safety hardening.** Exclusive data-dir lock (a second open fails clean instead of corrupting the database); path-traversal rejection in snapshot names; ENOSPC mid-write fails clean, discards the partial, and back-pressures without wedging; fsync `EIO` poisons the engine rather than acking a lost write; directory-fsync failures propagate instead of being swallowed. `SCRUB` verifies on-disk integrity (block checksums + MANIFEST) and alerts — read-only, never repairs.

The multi-tier tiered-placement experiment (RAM/SSD/HDD) was removed in v0.8.6 — Turba is a single-tier hardened engine. The global allocator (jemalloc) is set by the **server** binary (`xyzdb-server`), not by this library.

## Standalone usage

```rust
use turba_engine::config::{EngineConfig, StorageProfile};
use turba_engine::engine::TurbaEngine;
use std::path::Path;

// Sensible defaults; override only what you need.
let config = EngineConfig {
    storage_profile: StorageProfile::Ssd,
    ..EngineConfig::default()
};
let engine = TurbaEngine::open(Path::new("/tmp/db"), config)?;

// Writes go through an atomic batch — one call per keyspace, one durability point.
let mut batch = engine.batch();
batch.put_spatial(b"acct:42/tx:001", b"...record bytes...");
batch.put_vectors(b"acct:42/tx:001", b"...packed f32 column...");
let _seqno = batch.commit()?;

// Reads are per-keyspace, lock-free via the ArcSwap SuperVersion.
let v: Option<Vec<u8>> = engine.spatial.get(b"acct:42/tx:001")?;

// Range scan over a key prefix — where Turba's co-location pays off.
for entry in engine.spatial.range(b"acct:42/", b"acct:42:")? {
    let (_k, _v) = (entry.key, entry.value); // sorted by key
}
```

Turba is a pure key-value engine. The xyzDB query layer (xyTalk parser, lobes, anchors, gravity, ghosts, vector `NEAREST`, write throttle) is the sibling `xyzdb-engine` crate, [`../engine/`](../engine/).

## Tests

`cargo test -p turba-engine` runs the default suite — **272 test functions** (counted as `#[test]` / `#[tokio::test]` attributes) across 29 integration files plus in-crate unit tests. Run it for the authoritative pass count:

```
engine / tree / memtable        end-to-end ops, MVCC, per-keyspace reads
block / sstable / bloom         block format, zone maps, footer checksum, FP rate
compaction (+ convergence)      leveled correctness, major_compact convergence
crash_recovery                  subprocess kill + WAL replay
snapshot (basic / drain / load) hard-link snapshots, compaction-drain, under load
durability_modes / fsync        Buffer-window flush, fsync-EIO poison
enospc (flush / torn write)     clean ENOSPC failure + back-pressure recovery
engine_dir_lock                 exclusive data-dir lock (double-open rejected)
```

Two extra targets are gated: `--features durability-test-hooks` (the group-commit proptest) and the `loom` cfg (SuperVersion model-checking).

## Dependencies

- `crossbeam-skiplist` — lock-free memtable
- `arc-swap` — zero-lock SuperVersion swap
- `parking_lot` — fast mutexes for version updates
- `quick_cache` — block cache (with stats)
- `zstd` + `lz4_flex` — per-level compression
- `xxhash-rust` (xxh3) — block & footer checksums
- `varint-rs` + `byteorder-lite` + `byteview` — on-disk encoding
- `interval-heap` — compaction overflow scheduling
- `flume` — sync-thread channel
- `serde` + `serde_json` + `postcard` — `snapshot.meta` sidecar (human-readable + compact binary)
- `tracing` — operator-facing logs
- `self_cell`, `libc` — internal plumbing

No async, no network, no external database. `tokio` is not a dependency of `turba-engine`.

---

For project context, benchmarks, and the full xyzDB query layer, see the [root README](../../README.md) and the per-version notes under [`../../docs/releases/`](../../docs/releases/).

Created by **Iván Moreno Mendoza** · BUSL-1.1
