//! Tree: a single LSM-tree instance.
//!
//! Coordinates memtable, version management, flush, and reads.
//! No WAL — durability comes in Phase 6 (Engine layer).

// SPDX-License-Identifier: BUSL-1.1
pub mod version;

use crate::cache::BlockCache;
use crate::compaction::{leveled, worker as compaction_worker};
use crate::compression::CompressionType;
use crate::error::Result;
use crate::flush;
use crate::manifest;
use crate::memtable::Memtable;
use crate::merge::MergeIterator;
use crate::mvcc::MvccStream;
use crate::table::reader::SSTableBlockIter;
use crate::table::writer::SSTableConfig;
use crate::types::{Entry, SeqNo, ValueType, prefix_to_range};
use arc_swap::ArcSwap;
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use version::{SuperVersion, Version};

#[derive(Debug, Clone)]
pub struct TreeConfig {
    pub sstable: SSTableConfig,
    pub max_memtable_size: usize,
    pub compaction: leveled::LeveledConfig,
    /// Per-level compression override. Index = level number.
    /// If set, overrides sstable.compression for that level.
    /// Levels beyond the array length use the last entry.
    pub level_compressions: Option<Vec<CompressionType>>,
}

impl Default for TreeConfig {
    fn default() -> Self {
        Self {
            sstable: SSTableConfig::default(),
            max_memtable_size: 16 * 1024 * 1024, // 16MB
            compaction: leveled::LeveledConfig::default(),
            level_compressions: None,
        }
    }
}

impl TreeConfig {
    /// Get the compression type for a given level.
    /// Falls back to sstable.compression if level_compressions is not set.
    pub fn compression_for_level(&self, level: usize) -> CompressionType {
        match &self.level_compressions {
            Some(lc) if !lc.is_empty() => lc.get(level).copied().unwrap_or(*lc.last().unwrap()),
            _ => self.sstable.compression,
        }
    }
}

/// Per-level resident-metadata byte breakdown for a tree's SSTables.
/// Returned by `Tree::memory_breakdown` and consumed by the `reap-cycle`
/// diagnostic log. Each vector has `MAX_LEVELS` entries; index 0 is L0.
#[derive(Debug, Clone)]
pub struct TreeMemoryBreakdown {
    pub zone_maps_per_level: Vec<usize>,
    pub index_per_level: Vec<usize>,
    pub bloom_per_level: Vec<usize>,
}

/// Telemetry captured during `Tree::open_with_scheduler` while iterating the
/// manifest and eagerly opening every visible SSTable. Each call to
/// `SSTableReader::open_with_tree_id` reads bloom + index + meta synchronously
/// into struct fields; this struct just measures that work. Write-once at open
/// time; readable at any time via `Tree::warmup_stats`. See design doc §8.4
/// (H1.1).
#[derive(Debug, Clone, Default)]
pub struct WarmupStats {
    /// Total wall time of the manifest-replay loop, in milliseconds. Includes
    /// every `Version::open_table_eager` call but not the parent
    /// `Tree::open_with_scheduler` overhead (debris cleanup, channel setup).
    pub wall_ms: u64,
    /// Sum of `SSTableReader::warmup_bytes` across every opened SSTable —
    /// the index + bloom + meta bytes read off disk during the replay.
    /// Excludes data blocks (lazy via `BlockCache`) and the fixed footer.
    pub bytes_loaded: u64,
    /// Number of SSTables actually opened (skips manifest entries whose path
    /// no longer exists on disk).
    pub sstables_opened: usize,
}

impl TreeMemoryBreakdown {
    pub fn zone_maps_total(&self) -> usize {
        self.zone_maps_per_level.iter().sum()
    }
    pub fn index_total(&self) -> usize {
        self.index_per_level.iter().sum()
    }
    pub fn bloom_total(&self) -> usize {
        self.bloom_per_level.iter().sum()
    }
    pub fn total(&self) -> usize {
        self.zone_maps_total() + self.index_total() + self.bloom_total()
    }
}

pub struct Tree {
    path: PathBuf,
    tree_id: u64,
    config: TreeConfig,
    cache: Arc<BlockCache>,

    current: ArcSwap<SuperVersion>,
    next_table_id: AtomicU64,
    seqno: AtomicU64,

    /// Highest seqno that has been flushed to SSTables.
    /// Used by the Engine to know when it's safe to truncate the WAL.
    flushed_seqno: AtomicU64,

    /// Highest seqno that is both flushed to an SSTable AND recorded in a
    /// **persisted manifest**. This is the watermark below which the WAL is
    /// truly safe to discard. It is strictly more conservative than
    /// `flushed_seqno`: in BULKMODE (compaction disabled) `flush_sealed`
    /// advances `flushed_seqno` WITHOUT persisting the manifest, so the
    /// manifest can lag the flush — discarding the WAL on `flushed_seqno`
    /// alone would lose data on crash (`wal-state-machine.md` §6). Only
    /// advanced after a successful `persist_manifest()`. WAL segment pruning
    /// must gate on `min(manifest_durable_seqno)` across all trees.
    manifest_durable_seqno: AtomicU64,

    // Background workers: separate flush + compact threads
    flush_notify: flume::Sender<()>,
    flush_receiver: parking_lot::Mutex<Option<flume::Receiver<()>>>,
    compact_notify: flume::Sender<()>,
    compact_receiver: parking_lot::Mutex<Option<flume::Receiver<()>>>,
    bg_shutdown: Arc<AtomicBool>,
    bg_handles: parking_lot::Mutex<Vec<std::thread::JoinHandle<()>>>,
    compaction_enabled: Arc<AtomicBool>,
    /// Serializes manifest writes to prevent MANIFEST.tmp race between
    /// bg flush thread and synchronous flush_sealed / maybe_compact calls.
    manifest_lock: Mutex<()>,
    /// Serializes Version mutations (ArcSwap store) to prevent lost updates
    /// when flush_sealed and ingest_sorted run concurrently.
    version_update_lock: Mutex<()>,
    /// Table ids allocated by a flush that has written its SST but not yet
    /// installed it in the live `Version`. `cleanup_orphan_ssts` must never
    /// delete one of these: its `id <= max_referenced` guard assumes the
    /// in-flight flush always holds the highest id, but a concurrent
    /// compaction can install a *higher* id while a lower-id flush is still in
    /// flight (the bg flush worker is not paused by `major_compact`), which
    /// would otherwise unlink an SST about to become live.
    flushing_ids: Mutex<std::collections::HashSet<u64>>,
    /// Optional zone map builder for compaction output SSTables.
    zone_map_builder: parking_lot::RwLock<Option<Arc<dyn crate::table::writer::ZoneMapBuilder>>>,
    /// Optional per-key merge operator. When set, an owned key's versions are
    /// folded (at compaction and read) instead of last-writer-wins — e.g. ghost
    /// rollup delta-append. Only the keyspace that needs it (dictionary) gets one.
    merge_operator: parking_lot::RwLock<Option<Arc<dyn crate::merge_op::MergeOperator>>>,
    /// Count of Err returns from the background compact worker. Bumped on
    /// every failed `maybe_compact` cycle; an operator-visible signal that
    /// something is racing or going wrong. Zero under healthy load.
    compact_error_count: AtomicU64,
    /// Count of successful compact cycles (`maybe_compact` returning
    /// `Ok(true)` — i.e. a compaction round actually consolidated tables).
    /// Paired with `compact_error_count` as a health signal: under sustained
    /// write load this should climb; if writes arrive and this stays flat,
    /// compaction isn't keeping up (L0 grows, reader state accumulates).
    compact_success_count: AtomicU64,
    /// v0.6.1 D5 §4.7 — number of compaction passes currently
    /// executing on this tree. Incremented on `maybe_compact` /
    /// `major_compact_with_observer` entry, decremented on exit
    /// via the RAII [`CompactionGuard`]. Read by the heat
    /// allocator worker to honour the "no moves during compaction"
    /// interlock from D5 §4.7. `Arc<AtomicU64>` so the engine can
    /// expose a stable handle without copying the entire Tree.
    compaction_in_progress: Arc<AtomicU64>,
    /// Serializes compaction WORK on this tree (not just the version swap):
    /// `maybe_compact` `try_lock`s and skips if held; `major_compact` blocks on
    /// it. Without this, a background `maybe_compact` already past the
    /// `compaction_enabled` gate runs concurrently with a manual `major_compact`
    /// (or another bg pass): both can choose overlapping inputs and apply both
    /// merged outputs. Under last-writer-wins that's benign (same key+seqno
    /// dedups); under a merge operator it DOUBLE-COUNTS the folded value. The
    /// pre-existing `compaction_enabled.swap(false)` only stops NEW bg passes,
    /// not the in-flight one — this lock closes that window.
    compaction_lock: parking_lot::Mutex<()>,
    /// Count of successful `major_compact_with_observer` cycles. Distinct
    /// from `compact_success_count`, which tracks background
    /// `maybe_compact` cycles. Exposed via `major_compact_success_count()`
    /// and surfaced in the reap-cycle log as `major_ok=N`. No paired
    /// error counter: failures propagate to the caller
    /// (`execute_compact`, `shutdown`), which distinguishes noise-level
    /// bg telemetry from explicit-operation failure paths.
    major_compact_success_count: AtomicU64,
    /// Count of trivial-move compactions applied (H2.1). Exposed via
    /// `trivial_move_count()`.
    trivial_move_count: AtomicU64,
    /// Sum of `input.meta().file_size` across applied trivial-moves —
    /// the bytes that would have been re-written under non-trivial
    /// compaction. Useful for empirical sizing of the H2.1 savings.
    trivial_move_bytes_saved: AtomicU64,
    /// Count of `prewarm_l0_data_sections` invocations (H2.2). Increments
    /// once per `major_compact_with_observer` call that has at least one
    /// L0 input on iter 0 of its loop. See §9.2.
    prewarm_l0_invocations: AtomicU64,
    /// Total bytes read by pre-warm sequential `std::io::copy` calls.
    /// Sum should approximate Σ file_size of L0 inputs across invocations.
    prewarm_l0_bytes_read: AtomicU64,
    /// Cumulative wall-clock microseconds spent in `prewarm_l0_data_sections`.
    /// Includes the open + sequential read for every L0 input. The
    /// cost-model line item the §9.2 verdict criteria evaluate against
    /// the iter-1 saving.
    prewarm_l0_wall_us: AtomicU64,
    /// Count of per-file pre-warm errors. SOFT-FAIL contract: errors are
    /// counted + logged via `tracing::warn!`, never propagated. A
    /// climbing counter without a climbing wall is an operational signal
    /// (file races, eviction concurrency, transient HDD hiccups).
    prewarm_l0_errors: AtomicU64,

    /// Shared I/O scheduler. Owned by the engine, cloned into every tree.
    /// At Spike A.2 checkpoint 1 the scheduler is **dormant** — readers
    /// and writers do not yet consult it. Checkpoint 2 activates the
    /// instrumentation hooks.
    scheduler: Arc<crate::io::Scheduler>,
    /// Telemetry from the manifest-replay loop in `open_with_scheduler`.
    /// Write-once at open; read at any time via `warmup_stats()` (H1.1).
    warmup_stats: WarmupStats,
}

static NEXT_TREE_ID: AtomicU64 = AtomicU64::new(1);

/// RAII guard tracking the table ids a single `flush_sealed` call has
/// allocated but not yet installed in the live `Version`. Registered ids are
/// held in `Tree::flushing_ids` so `cleanup_orphan_ssts` will not unlink their
/// SST files, and are removed on drop — which fires on every exit path,
/// including the `?` early returns in `flush_sealed`. By the time it drops the
/// ids are already referenced by the installed version (or the flush failed and
/// abandoned them), so protection is continuous: in-flight set → version.
struct FlushIdGuard<'a> {
    set: &'a Mutex<std::collections::HashSet<u64>>,
    ids: Vec<u64>,
}

impl<'a> FlushIdGuard<'a> {
    fn new(set: &'a Mutex<std::collections::HashSet<u64>>) -> Self {
        Self {
            set,
            ids: Vec::new(),
        }
    }

    /// Mark `id` as in-flight for the lifetime of this guard.
    fn register(&mut self, id: u64) {
        self.set.lock().insert(id);
        self.ids.push(id);
    }
}

impl Drop for FlushIdGuard<'_> {
    fn drop(&mut self) {
        if self.ids.is_empty() {
            return;
        }
        let mut set = self.set.lock();
        for id in &self.ids {
            set.remove(id);
        }
    }
}

/// Decision for `cleanup_orphan_ssts`: may the SST file with `id` be unlinked?
///
/// True only when the file is (a) unreferenced by the live version, (b) at or
/// below the highest referenced id — so a freshly written higher-numbered file
/// the version has not caught up to is never touched — and (c) not in flight
/// from a flush. The in-flight check (c) is what makes a lower-id flush safe
/// against a concurrent higher-id compaction: without it, a flush that wrote
/// `100.sst` but has not installed it would be deleted the moment a compaction
/// installs `101`, because `100 <= 101` and 100 is not yet referenced.
fn orphan_is_deletable(
    id: u64,
    referenced: &std::collections::HashSet<u64>,
    max_referenced: u64,
    in_flight: &std::collections::HashSet<u64>,
) -> bool {
    !referenced.contains(&id) && id <= max_referenced && !in_flight.contains(&id)
}

/// Per-SSTable scrub result: the indices of data blocks that failed their
/// on-disk checksum. Clean tables are omitted from the report.
#[derive(Debug, Clone)]
pub struct SstScrubReport {
    pub path: std::path::PathBuf,
    pub table_id: u64,
    pub bad_blocks: Vec<usize>,
}

/// Scrub result for one tree (keyspace): how much was verified and what failed.
#[derive(Debug, Clone, Default)]
pub struct TreeScrubReport {
    pub ssts_scanned: usize,
    pub blocks_scanned: usize,
    pub corrupt_ssts: Vec<SstScrubReport>,
    /// False if the MANIFEST failed its checksum or could not be read.
    pub manifest_ok: bool,
}

/// Which gate decided a point lookup — the answer to "did the request actually
/// reach the door this test claims to exercise?".
///
/// A test that asserts only on the RESULT can pass without ever touching the gate
/// it targets. That is not hypothetical: a duplicate-anchor test with a forged
/// (blinded) bloom passed because reopening replayed the WAL, the key was back in
/// the active memtable, and `get_at` answers from there BEFORE consulting any
/// bloom. Green, and never touched the bloom. This enum is how such a test states
/// its precondition instead of hoping for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LookupGate {
    /// Answered by the active memtable.
    ActiveMemtable { tombstone: bool },
    /// Answered by a sealed (not yet flushed) memtable.
    SealedMemtable { tombstone: bool },
    /// Answered by an SSTable at `level`.
    Table {
        level: usize,
        table_id: u64,
        tombstone: bool,
    },
    /// Nothing held the key: every memtable and every consulted table said absent.
    NotFound,
}

/// Why a lookup ended where it did, plus the two anomalies worth catching.
///
/// Only produced by [`Tree::get_at_traced`]; the untraced path builds none of this
/// and performs no extra I/O.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LookupTrace {
    /// The gate that produced the answer.
    pub decided_by: Option<LookupGate>,
    /// Levels where the L1+ `binary_search` found no covering table **but a linear
    /// scan of that level would have**. Non-empty is the smoking gun for an
    /// unsorted or overlapping L1+ run — the state the engine's own guard warns
    /// "may silently miss present keys", and historically a cause of exactly this
    /// symptom that had nothing to do with blooms.
    pub positional_misses_with_covering_table: Vec<usize>,
    /// `(level, table_id)` for each table that reported the key ABSENT through the
    /// bloom-gated read while a bloom-less read of the same table found it — i.e.
    /// a bloom false negative, caught in the act.
    pub bloom_false_negatives: Vec<(usize, u64)>,
}

impl LookupTrace {
    /// True when the lookup hit no anomaly: no positional miss over a covering
    /// table, no bloom false negative.
    pub fn is_clean(&self) -> bool {
        self.positional_misses_with_covering_table.is_empty()
            && self.bloom_false_negatives.is_empty()
    }
}

impl Tree {
    /// Process-unique identifier assigned at `open()`. Stable for the
    /// lifetime of this `Tree` instance and used as the lookup key into the
    /// shared `BlockCache`'s per-tree counter map (Spike 0 ampliado, v0.3
    /// cycle Day 0-1).
    pub fn tree_id(&self) -> u64 {
        self.tree_id
    }

    /// Aggregate OS page-cache residency across this tree's SSTables.
    /// Linux only; returns `Default` on other targets. Per-SSTable errors
    /// (e.g. file unlinked between version-load and measure) are silently
    /// dropped — STATS is diagnostic, not transactional. Spike 0 ampliado
    /// (v0.3 cycle Day 2-3); see `crate::page_cache` for the mechanism.
    pub fn page_cache_residency(&self) -> crate::page_cache::PageCacheResidency {
        let sv = self.current.load();
        let mut total = crate::page_cache::PageCacheResidency::default();
        for level in sv.version.levels.iter() {
            for handle in level {
                if let Ok(r) = crate::page_cache::measure_residency(&handle.path) {
                    total.add(&r);
                }
            }
        }
        total
    }

    /// Borrow the tree's I/O scheduler. Used by readers / writers / the
    /// flush + compact background workers to invoke `before_op` /
    /// `after_op` around their kernel I/O calls. At Spike A.2 checkpoint
    /// 1 callers do not yet use this — the scheduler is dormant.
    pub fn scheduler(&self) -> &Arc<crate::io::Scheduler> {
        &self.scheduler
    }

    /// Create or open a tree at the given path. Convenience wrapper that
    /// defaults to the [`crate::io::Scheduler::Passthrough`] scheduler.
    ///
    /// **Production code MUST use [`Tree::open_with_scheduler`]** with the
    /// engine's shared `Arc<Scheduler>`. This wrapper exists for tests
    /// and historical compatibility only — it constructs a fresh
    /// passthrough each call, which would defeat per-tree scheduler
    /// coordination across the engine's keyspaces.
    pub fn open(path: &Path, config: TreeConfig, cache: Arc<BlockCache>) -> Result<Self> {
        Self::open_with_scheduler(
            Arc::new(crate::io::Scheduler::passthrough()),
            path,
            config,
            cache,
        )
    }

    /// Canonical entry point: create or open a tree, threading the
    /// engine's shared `Arc<Scheduler>` so every I/O on this tree is
    /// observed by the same scheduler instance.
    pub fn open_with_scheduler(
        scheduler: Arc<crate::io::Scheduler>,
        path: &Path,
        config: TreeConfig,
        cache: Arc<BlockCache>,
    ) -> Result<Self> {
        std::fs::create_dir_all(path)?;

        // Crash debris cleanup: any `.sst.tmp` files present at open time are
        // leftovers from a previous session that died mid-flush. The atomic
        // publish invariant (writer writes to .tmp, fsyncs, renames to .sst)
        // means a visible .sst file is always complete; a visible .sst.tmp
        // file is always incomplete and should be discarded. Runtime doesn't
        // touch .tmp files because they may belong to an in-flight writer.
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                if name.to_string_lossy().ends_with(".sst.tmp") {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }

        let tree_id = NEXT_TREE_ID.fetch_add(1, Ordering::Relaxed);

        // H1.1: time the manifest-replay loop and accumulate per-SST warmup
        // bytes (bloom + index + meta read by SSTableReader::open). Surfaced
        // via `warmup_stats()` and the engine's STATS schema.
        let warmup_start = std::time::Instant::now();
        let mut warmup_bytes_acc: u64 = 0;
        let mut sstables_opened_acc: usize = 0;

        let (version, next_table_id, next_seqno) = if let Some(mdata) =
            manifest::read_manifest(path)?
        {
            let mut version = Version::new();
            for (level_idx, level_entries) in mdata.levels.into_iter().enumerate() {
                for entry in level_entries {
                    let tpath = PathBuf::from(&entry.path);
                    if tpath.exists() {
                        let handle = Version::open_table_eager(
                            tpath,
                            Arc::clone(&cache),
                            tree_id,
                            Arc::clone(&scheduler),
                        )?;
                        warmup_bytes_acc += handle.reader.warmup_bytes();
                        sstables_opened_acc += 1;
                        version.levels[level_idx].push(handle);
                    }
                }
            }
            // Defensive: L1+ MUST be sorted by key_min for `get_at`'s
            // per-level binary_search to find present keys. Sort on load
            // regardless of the manifest's stored order — manifests written
            // before the compaction-sort fix (Version::with_compaction_applied)
            // persisted levels left unsorted by `extend`, which made point
            // reads silently miss keys at scale.
            for level_idx in 1..version.levels.len() {
                version.levels[level_idx].sort_by(|a, b| a.meta().key_min.cmp(&b.meta().key_min));
                Version::check_level_non_overlapping(level_idx, &version.levels[level_idx]);
            }
            (version, mdata.next_table_id, mdata.next_seqno)
        } else {
            (Version::new(), 1, 0)
        };

        // Table ids must be monotonic ACROSS restarts, not just within a process.
        //
        // `next_table_id` is only made durable by `persist_manifest`, which runs
        // AFTER the tables are installed in the live Version and not at all while
        // compaction is disabled (BULKMODE). So a crash in that window — or a bulk
        // load that exits without a major compaction — leaves `NNNNNN.sst` on disk
        // while the manifest still says `next_table_id = NNNNNN`. Orphan `.sst`
        // files are not swept at open (only `.sst.tmp` is), so the next flush mints
        // the same id and `sst_path` resolves to the SAME FILENAME.
        //
        // Nothing silently corrupts today: a reusable id is by construction absent
        // from every persisted manifest, so it is never live in a Version and the
        // caches (keyed by `(tree_id, table_id)`, with no generation) cannot serve
        // one table's blocks for another. But the safety rests on that argument
        // rather than on an invariant, and the same identity being handed out twice
        // is a hazard for anything keyed by it — the block cache, the meta cache,
        // orphan cleanup, `FlushIdGuard`. So make it impossible instead of merely
        // unreachable, mirroring the `max` reconciliation `seqno` already gets in
        // `TurbaEngine::open`. Cost is one `read_dir` at open, on a directory that
        // is already enumerated for `.sst.tmp` cleanup.
        let next_table_id = next_table_id.max(Self::max_on_disk_table_id(path) + 1);

        let warmup_stats = WarmupStats {
            wall_ms: warmup_start.elapsed().as_millis() as u64,
            bytes_loaded: warmup_bytes_acc,
            sstables_opened: sstables_opened_acc,
        };

        let active = Arc::new(Memtable::new());
        let sv = SuperVersion::new(active, Arc::new(version));

        let (flush_notify, flush_recv) = flume::bounded(4);
        let (compact_notify, compact_recv) = flume::bounded(4);
        let bg_shutdown = Arc::new(AtomicBool::new(false));
        let compaction_enabled = Arc::new(AtomicBool::new(true));

        Ok(Self {
            path: path.to_path_buf(),
            tree_id,
            config,
            cache,
            current: ArcSwap::from_pointee(sv),
            next_table_id: AtomicU64::new(next_table_id),
            seqno: AtomicU64::new(next_seqno),
            flushed_seqno: AtomicU64::new(next_seqno),
            manifest_durable_seqno: AtomicU64::new(next_seqno),
            flush_notify,
            flush_receiver: parking_lot::Mutex::new(Some(flush_recv)),
            compact_notify,
            compact_receiver: parking_lot::Mutex::new(Some(compact_recv)),
            bg_shutdown,
            bg_handles: parking_lot::Mutex::new(Vec::new()),
            compaction_enabled,
            manifest_lock: Mutex::new(()),
            version_update_lock: Mutex::new(()),
            flushing_ids: Mutex::new(std::collections::HashSet::new()),
            zone_map_builder: parking_lot::RwLock::new(None),
            merge_operator: parking_lot::RwLock::new(None),
            compact_error_count: AtomicU64::new(0),
            compact_success_count: AtomicU64::new(0),
            compaction_in_progress: Arc::new(AtomicU64::new(0)),
            compaction_lock: parking_lot::Mutex::new(()),
            major_compact_success_count: AtomicU64::new(0),
            trivial_move_count: AtomicU64::new(0),
            trivial_move_bytes_saved: AtomicU64::new(0),
            prewarm_l0_invocations: AtomicU64::new(0),
            prewarm_l0_bytes_read: AtomicU64::new(0),
            prewarm_l0_wall_us: AtomicU64::new(0),
            prewarm_l0_errors: AtomicU64::new(0),
            scheduler,
            warmup_stats,
        })
    }

    /// Telemetry from the manifest-replay loop at `open_with_scheduler`. The
    /// returned struct is populated once at open and never mutates afterwards
    /// — re-opening the tree on a fresh `Tree` instance is the only way to
    /// observe a change. See design doc §8.4 (H1.1).
    pub fn warmup_stats(&self) -> WarmupStats {
        self.warmup_stats.clone()
    }

    // --- Write path ---

    /// Insert a key-value pair. Returns the assigned seqno.
    pub fn insert(&self, user_key: &[u8], value: &[u8]) -> Result<SeqNo> {
        let seqno = self.seqno.fetch_add(1, Ordering::AcqRel) + 1;
        let sv = self.current.load();
        sv.active.insert(user_key, value, seqno, ValueType::Value);
        Ok(seqno)
    }

    /// Delete a key (write a tombstone). Returns the assigned seqno.
    pub fn remove(&self, user_key: &[u8]) -> Result<SeqNo> {
        let seqno = self.seqno.fetch_add(1, Ordering::AcqRel) + 1;
        let sv = self.current.load();
        sv.active.insert(user_key, &[], seqno, ValueType::Tombstone);
        Ok(seqno)
    }

    /// Insert with an externally assigned seqno (used by Engine batch path).
    pub fn insert_with_seqno(&self, user_key: &[u8], value: &[u8], seqno: SeqNo, vtype: ValueType) {
        // Advance the tree's seqno counter so get() sees this write
        self.seqno.fetch_max(seqno, Ordering::AcqRel);
        let sv = self.current.load();
        sv.active.insert(user_key, value, seqno, vtype);
    }

    /// Verify the on-disk integrity of every live SSTable (data-block
    /// checksums) and the MANIFEST, without repairing anything. Reads raw bytes
    /// from disk so it surfaces silent bit-rot before a query hits it.
    /// Alert-only.
    pub fn scrub(&self) -> TreeScrubReport {
        let sv = self.current.load();
        let mut report = TreeScrubReport::default();
        for level in &sv.version.levels {
            for table in level {
                report.ssts_scanned += 1;
                report.blocks_scanned += table.reader.block_count();
                let bad = table.reader.verify_blocks();
                if !bad.is_empty() {
                    report.corrupt_ssts.push(SstScrubReport {
                        path: table.reader.path().to_path_buf(),
                        table_id: table.reader.table_id(),
                        bad_blocks: bad,
                    });
                }
            }
        }
        // Re-verify the MANIFEST checksum: read_manifest errors on a checksum
        // mismatch or an unreadable file.
        report.manifest_ok = manifest::read_manifest(&self.path).is_ok();
        report
    }

    // --- Read path ---

    /// Point lookup: returns the latest visible value, or None.
    /// Tombstones return None (key is deleted).
    pub fn get(&self, user_key: &[u8]) -> Result<Option<Vec<u8>>> {
        let visible_seqno = self.seqno.load(Ordering::Acquire);
        self.get_at(user_key, visible_seqno)
    }

    /// Point lookup at a specific seqno.
    pub fn get_at(&self, user_key: &[u8], visible_seqno: SeqNo) -> Result<Option<Vec<u8>>> {
        self.get_at_inner(user_key, visible_seqno, None)
    }

    /// Point lookup that also reports WHICH GATE decided it, plus the two
    /// anomalies that make a point read miss a key a scan can still see: an L1+
    /// positional miss over a covering table, and a bloom false negative.
    ///
    /// For tests and diagnosis. It shares ONE implementation with [`Self::get_at`]
    /// — the untraced path passes `None` and is byte-for-byte the same work, with
    /// no extra I/O — so the traced view can never drift from the real one.
    ///
    /// Tracing DOES perform extra reads: on a table-level absent answer it repeats
    /// the read bloom-lessly to tell "genuinely absent" from "the bloom lied", and
    /// on an L1+ positional miss it linearly scans that level's bounds. Both are
    /// exactly the confirmations a diagnosis needs and exactly what production must
    /// not pay, which is why they live behind this entry point.
    ///
    /// # Errors
    /// Propagates storage errors from the underlying readers.
    pub fn get_at_traced(
        &self,
        user_key: &[u8],
        visible_seqno: SeqNo,
    ) -> Result<(Option<Vec<u8>>, LookupTrace)> {
        let mut trace = LookupTrace::default();
        let out = self.get_at_inner(user_key, visible_seqno, Some(&mut trace))?;
        Ok((out, trace))
    }

    fn get_at_inner(
        &self,
        user_key: &[u8],
        visible_seqno: SeqNo,
        mut trace: Option<&mut LookupTrace>,
    ) -> Result<Option<Vec<u8>>> {
        let sv = self.current.load();

        // 1. Active memtable
        if let Some((vtype, value)) = sv.active.get(user_key, visible_seqno) {
            let tombstone = vtype == ValueType::Tombstone;
            if let Some(t) = trace.as_deref_mut() {
                t.decided_by = Some(LookupGate::ActiveMemtable { tombstone });
            }
            return Ok(if tombstone { None } else { Some(value) });
        }

        // 2. Sealed memtables (newest first)
        for sealed in sv.sealed.iter().rev() {
            if let Some((vtype, value)) = sealed.get(user_key, visible_seqno) {
                let tombstone = vtype == ValueType::Tombstone;
                if let Some(t) = trace.as_deref_mut() {
                    t.decided_by = Some(LookupGate::SealedMemtable { tombstone });
                }
                return Ok(if tombstone { None } else { Some(value) });
            }
        }

        // 3. SSTable levels
        for (level_idx, level) in sv.version.levels.iter().enumerate() {
            if level_idx == 0 {
                // L0: check all tables (may overlap)
                for table in level.iter().rev() {
                    if user_key < table.meta().key_min.as_slice()
                        || user_key > table.meta().key_max.as_slice()
                    {
                        continue;
                    }
                    if let Some(entry) = table.reader.get(user_key, visible_seqno)? {
                        let tombstone = entry.value_type == ValueType::Tombstone;
                        if let Some(t) = trace.as_deref_mut() {
                            t.decided_by = Some(LookupGate::Table {
                                level: level_idx,
                                table_id: table.meta().table_id,
                                tombstone,
                            });
                        }
                        return Ok(if tombstone { None } else { Some(entry.value) });
                    }
                    if let Some(t) = trace.as_deref_mut() {
                        Self::note_absent_table(t, level_idx, table, user_key, visible_seqno)?;
                    }
                }
            } else {
                // L1+: binary search by key range (tables are sorted, non-overlapping)
                let pos = level.binary_search_by(|t| {
                    if user_key > t.meta().key_max.as_slice() {
                        std::cmp::Ordering::Less
                    } else if user_key < t.meta().key_min.as_slice() {
                        std::cmp::Ordering::Greater
                    } else {
                        std::cmp::Ordering::Equal
                    }
                });
                match pos {
                    Ok(idx) => {
                        if let Some(entry) = level[idx].reader.get(user_key, visible_seqno)? {
                            let tombstone = entry.value_type == ValueType::Tombstone;
                            if let Some(t) = trace.as_deref_mut() {
                                t.decided_by = Some(LookupGate::Table {
                                    level: level_idx,
                                    table_id: level[idx].meta().table_id,
                                    tombstone,
                                });
                            }
                            return Ok(if tombstone { None } else { Some(entry.value) });
                        }
                        if let Some(t) = trace.as_deref_mut() {
                            Self::note_absent_table(
                                t,
                                level_idx,
                                &level[idx],
                                user_key,
                                visible_seqno,
                            )?;
                        }
                    }
                    Err(_) => {
                        // The positional gate said no table covers this key. If a
                        // linear pass finds one that does, the level is not the
                        // sorted, non-overlapping run the search assumes — and the
                        // key is present but unreachable by a point read.
                        if let Some(t) = trace.as_deref_mut()
                            && level.iter().any(|tbl| {
                                user_key >= tbl.meta().key_min.as_slice()
                                    && user_key <= tbl.meta().key_max.as_slice()
                            })
                        {
                            t.positional_misses_with_covering_table.push(level_idx);
                        }
                    }
                }
            }
        }

        if let Some(t) = trace.as_deref_mut() {
            t.decided_by = Some(LookupGate::NotFound);
        }
        Ok(None)
    }

    /// Record whether a table's "absent" answer was the bloom lying. Trace-only:
    /// it costs an extra bloom-less read, which production must not pay.
    fn note_absent_table(
        trace: &mut LookupTrace,
        level: usize,
        table: &version::TableHandle,
        user_key: &[u8],
        visible_seqno: SeqNo,
    ) -> Result<()> {
        if table
            .reader
            .get_no_bloom(user_key, visible_seqno)?
            .is_some()
        {
            trace
                .bloom_false_negatives
                .push((level, table.meta().table_id));
        }
        Ok(())
    }

    /// Point lookup that BYPASSES SSTable bloom filters (block index + data block).
    ///
    /// Same active → sealed → SSTable layering and MVCC visibility as [`Self::get_at`]
    /// (active/sealed are skiplists with no bloom, so they behave identically); only
    /// the SSTable reads switch to [`super::table::reader::Reader::get_no_bloom`].
    ///
    /// Crash-recovery read-path fallback ONLY (never the hot path). After an unclean
    /// crash a post-recovery SSTable may carry a bloom that disagrees with its data,
    /// making [`Self::get`] miss a key the scan path can see. A caller that already
    /// observed the key via a scan (e.g. NEAREST hydration) uses this to recover it.
    pub fn get_no_bloom(&self, user_key: &[u8]) -> Result<Option<Vec<u8>>> {
        let visible_seqno = self.seqno.load(Ordering::Acquire);
        let sv = self.current.load();

        if let Some((vtype, value)) = sv.active.get(user_key, visible_seqno) {
            return Ok(if vtype == ValueType::Tombstone {
                None
            } else {
                Some(value)
            });
        }
        for sealed in sv.sealed.iter().rev() {
            if let Some((vtype, value)) = sealed.get(user_key, visible_seqno) {
                return Ok(if vtype == ValueType::Tombstone {
                    None
                } else {
                    Some(value)
                });
            }
        }
        for (level_idx, level) in sv.version.levels.iter().enumerate() {
            if level_idx == 0 {
                for table in level.iter().rev() {
                    if user_key < table.meta().key_min.as_slice()
                        || user_key > table.meta().key_max.as_slice()
                    {
                        continue;
                    }
                    if let Some(entry) = table.reader.get_no_bloom(user_key, visible_seqno)? {
                        return Ok(if entry.value_type == ValueType::Tombstone {
                            None
                        } else {
                            Some(entry.value)
                        });
                    }
                }
            } else {
                let pos = level.binary_search_by(|t| {
                    if user_key > t.meta().key_max.as_slice() {
                        std::cmp::Ordering::Less
                    } else if user_key < t.meta().key_min.as_slice() {
                        std::cmp::Ordering::Greater
                    } else {
                        std::cmp::Ordering::Equal
                    }
                });
                if let Ok(idx) = pos {
                    if let Some(entry) = level[idx].reader.get_no_bloom(user_key, visible_seqno)? {
                        return Ok(if entry.value_type == ValueType::Tombstone {
                            None
                        } else {
                            Some(entry.value)
                        });
                    }
                }
            }
        }

        Ok(None)
    }

    /// Prefix scan: returns all visible entries matching the prefix, MVCC-filtered.
    /// Tombstones are excluded from results.
    pub fn prefix(&self, prefix: &[u8]) -> Result<Vec<Entry>> {
        let visible_seqno = self.seqno.load(Ordering::Acquire);
        self.prefix_at(prefix, visible_seqno)
    }

    /// Streaming prefix scan: returns an iterator instead of Vec.
    /// Memory-efficient for large scans (ghost creation, full lobe scans).
    pub fn prefix_iter(&self, prefix: &[u8]) -> Result<Box<dyn Iterator<Item = Entry> + '_>> {
        self.prefix_iter_filtered(prefix, None)
    }

    /// Streaming prefix scan with optional per-table block filter.
    /// The filter factory receives the SSTable metadata's zone_maps blob and the block index,
    /// and returns true if the block should be loaded.
    // block_filter factory type; a type alias is a design change, deferred (not a lint fix).
    #[allow(clippy::type_complexity)]
    pub fn prefix_iter_filtered(
        &self,
        prefix: &[u8],
        block_filter: Option<Arc<dyn Fn(&[u8], usize) -> bool + Send + Sync>>,
    ) -> Result<Box<dyn Iterator<Item = Entry> + '_>> {
        let visible_seqno = self.seqno.load(Ordering::Acquire);
        let sv = self.current.load();
        let (lower, upper) = prefix_to_range(prefix);

        let mut sources: Vec<Box<dyn Iterator<Item = Entry>>> = Vec::new();

        // Active memtable
        let active_entries: Vec<Entry> = sv
            .active
            .iter()
            .filter(|e| e.key.starts_with(prefix))
            .collect();
        if !active_entries.is_empty() {
            sources.push(Box::new(active_entries.into_iter()));
        }

        // Sealed memtables
        for sealed in &sv.sealed {
            let entries: Vec<Entry> = sealed
                .iter()
                .filter(|e| e.key.starts_with(prefix))
                .collect();
            if !entries.is_empty() {
                sources.push(Box::new(entries.into_iter()));
            }
        }

        // SSTables — streaming block-by-block
        for level in &sv.version.levels {
            for table in level {
                if let Some(ref upper) = upper {
                    if table.meta().key_min.as_slice() >= upper.as_slice() {
                        continue;
                    }
                }
                if table.meta().key_max.as_slice() < lower.as_slice() {
                    continue;
                }
                let prefix_owned = prefix.to_vec();
                let mut iter = SSTableBlockIter::new_with_range(
                    Arc::clone(table),
                    &lower,
                    upper.as_deref(),
                    crate::io::Lane::UserIORead,
                )?;

                // Attach block filter if zone maps exist for this SSTable.
                // Decode zone maps ONCE into an owned Vec<Vec<u8>> before
                // constructing the closure, so the closure does not
                // re-decode on every block filter invocation.
                if let Some(ref bf) = block_filter {
                    // Zone maps are no longer resident — fetch the blob via the
                    // metadata cache (reloaded from disk on a miss). On any load
                    // error or a non-zone-map section, fall back to no pruning.
                    let section = table.reader.zone_maps().ok();
                    let zone_bytes: &[u8] = match section.as_deref() {
                        Some(crate::cache::MetaSection::ZoneMaps(v)) => v.as_slice(),
                        _ => &[],
                    };
                    if !zone_bytes.is_empty() {
                        let zone_maps: Vec<Vec<u8>> =
                            crate::table::meta::decode_zone_maps(zone_bytes)
                                .into_iter()
                                .map(|s| s.to_vec())
                                .collect();
                        let bf = bf.clone();
                        iter = iter.with_block_filter(Box::new(move |block_idx| {
                            if let Some(zm_bytes) = zone_maps.get(block_idx) {
                                bf(zm_bytes.as_slice(), block_idx)
                            } else {
                                true // No zone map for this block → load it
                            }
                        }));
                    }
                }

                let iter = iter.filter(move |e| e.key.starts_with(&prefix_owned));
                sources.push(Box::new(iter));
            }
        }

        let merged = MergeIterator::new(sources);
        let mvcc =
            MvccStream::new_with_merge(merged, visible_seqno, self.merge_operator.read().clone());
        let iter = mvcc.filter(|e| e.value_type == ValueType::Value);
        Ok(Box::new(iter))
    }

    /// Streaming range scan over `[start, end)` (`end = None` → unbounded above).
    ///
    /// Mirrors [`Self::prefix_iter`] but with explicit byte bounds, for ordered
    /// secondary-index seeks (the ghost range-seek). Binary-searches each
    /// SSTable's block index down to `start` and stops at `end`, prunes tables
    /// whose key range cannot overlap, and applies the SAME MVCC visibility +
    /// tombstone filtering as `prefix_iter` — so it inherits the production read
    /// semantics rather than introducing new ones.
    pub fn range_iter(
        &self,
        start: &[u8],
        end: Option<&[u8]>,
    ) -> Result<Box<dyn Iterator<Item = Entry> + '_>> {
        let visible_seqno = self.seqno.load(Ordering::Acquire);
        let sv = self.current.load();
        let start_v = start.to_vec();
        let end_v: Option<Vec<u8>> = end.map(|e| e.to_vec());

        // Membership test for `[start, end)`. Cloned into each source so a block
        // (which may carry keys outside the bounds) is filtered exactly.
        let make_pred = || {
            let s = start_v.clone();
            let e = end_v.clone();
            move |k: &[u8]| k >= s.as_slice() && e.as_ref().is_none_or(|e| k < e.as_slice())
        };

        let mut sources: Vec<Box<dyn Iterator<Item = Entry>>> = Vec::new();

        let pred = make_pred();
        let active: Vec<Entry> = sv.active.iter().filter(|en| pred(&en.key)).collect();
        if !active.is_empty() {
            sources.push(Box::new(active.into_iter()));
        }

        for sealed in &sv.sealed {
            let pred = make_pred();
            let es: Vec<Entry> = sealed.iter().filter(|en| pred(&en.key)).collect();
            if !es.is_empty() {
                sources.push(Box::new(es.into_iter()));
            }
        }

        for level in &sv.version.levels {
            for table in level {
                // Prune SSTables whose key range cannot overlap [start, end).
                if let Some(ref e) = end_v {
                    if table.meta().key_min.as_slice() >= e.as_slice() {
                        continue;
                    }
                }
                if table.meta().key_max.as_slice() < start_v.as_slice() {
                    continue;
                }
                let iter = SSTableBlockIter::new_with_range(
                    Arc::clone(table),
                    &start_v,
                    end_v.as_deref(),
                    crate::io::Lane::UserIORead,
                )?;
                let pred = make_pred();
                sources.push(Box::new(iter.filter(move |en| pred(&en.key))));
            }
        }

        let merged = MergeIterator::new(sources);
        let mvcc =
            MvccStream::new_with_merge(merged, visible_seqno, self.merge_operator.read().clone());
        let iter = mvcc.filter(|e| e.value_type == ValueType::Value);
        Ok(Box::new(iter))
    }

    pub fn prefix_at(&self, prefix: &[u8], visible_seqno: SeqNo) -> Result<Vec<Entry>> {
        let sv = self.current.load();
        let (lower, upper) = prefix_to_range(prefix);

        let mut sources: Vec<Box<dyn Iterator<Item = Entry>>> = Vec::new();

        let active_entries: Vec<Entry> = sv
            .active
            .iter()
            .filter(|e| e.key.starts_with(prefix))
            .collect();
        if !active_entries.is_empty() {
            sources.push(Box::new(active_entries.into_iter()));
        }

        for sealed in &sv.sealed {
            let entries: Vec<Entry> = sealed
                .iter()
                .filter(|e| e.key.starts_with(prefix))
                .collect();
            if !entries.is_empty() {
                sources.push(Box::new(entries.into_iter()));
            }
        }

        for level in &sv.version.levels {
            for table in level {
                if let Some(ref upper) = upper {
                    if table.meta().key_min.as_slice() >= upper.as_slice() {
                        continue;
                    }
                }
                if table.meta().key_max.as_slice() < lower.as_slice() {
                    continue;
                }

                let prefix_owned = prefix.to_vec();
                let iter = SSTableBlockIter::new_with_range(
                    Arc::clone(table),
                    &lower,
                    upper.as_deref(),
                    crate::io::Lane::UserIORead,
                )?
                .filter(move |e| e.key.starts_with(&prefix_owned));
                sources.push(Box::new(iter));
            }
        }

        let merged = MergeIterator::new(sources);
        let mvcc =
            MvccStream::new_with_merge(merged, visible_seqno, self.merge_operator.read().clone());
        let results: Vec<Entry> = mvcc.filter(|e| e.value_type == ValueType::Value).collect();

        Ok(results)
    }

    /// Full scan of all entries (for testing/debugging).
    pub fn scan_all(&self) -> Result<Vec<Entry>> {
        let visible_seqno = self.seqno.load(Ordering::Acquire);
        let sv = self.current.load();

        let mut sources: Vec<Box<dyn Iterator<Item = Entry>>> = Vec::new();

        let active_entries: Vec<Entry> = sv.active.iter().collect();
        if !active_entries.is_empty() {
            sources.push(Box::new(active_entries.into_iter()));
        }
        for sealed in &sv.sealed {
            let entries: Vec<Entry> = sealed.iter().collect();
            if !entries.is_empty() {
                sources.push(Box::new(entries.into_iter()));
            }
        }
        for level in &sv.version.levels {
            for table in level {
                let iter = SSTableBlockIter::new(Arc::clone(table), crate::io::Lane::UserIORead)?;
                sources.push(Box::new(iter));
            }
        }

        let merged = MergeIterator::new(sources);
        let mvcc =
            MvccStream::new_with_merge(merged, visible_seqno, self.merge_operator.read().clone());
        let results: Vec<Entry> = mvcc.filter(|e| e.value_type == ValueType::Value).collect();
        Ok(results)
    }

    /// Range scan: returns all visible entries in [start, end], MVCC-filtered.
    ///
    /// Exactly `range_stream(start, end)?.collect()` — the eager form. Prefer
    /// [`Self::range_stream`] for a large range (e.g. a big gravity bucket) so the
    /// working set stays at O(block) instead of materializing the whole range.
    pub fn range(&self, start: &[u8], end: &[u8]) -> Result<Vec<Entry>> {
        Ok(self.range_stream(start, end)?.collect())
    }

    /// Streaming form of [`Self::range`]: the SAME visible entries in the SAME
    /// order over the INCLUSIVE `[start, end]`, MVCC-filtered, but yielded lazily
    /// so the caller never holds the whole range at once. Because `range` is
    /// defined as `range_stream(..).collect()`, the two are byte-identical by
    /// construction (the gate in `nearest.rs` tests it end-to-end).
    ///
    /// Distinct from [`Self::range_iter`], which is HALF-OPEN `[start, end)` for
    /// ordered secondary-index seeks — inclusivity matters here: NEAREST's
    /// `key_max` is the saturated all-`0xFF` tail of a gravity bucket, so a
    /// half-open bound would silently drop an entry sitting exactly on it.
    pub fn range_stream(
        &self,
        start: &[u8],
        end: &[u8],
    ) -> Result<Box<dyn Iterator<Item = Entry> + '_>> {
        let visible_seqno = self.seqno.load(Ordering::Acquire);
        let sv = self.current.load();

        let mut sources: Vec<Box<dyn Iterator<Item = Entry>>> = Vec::new();

        let active_entries: Vec<Entry> = sv
            .active
            .iter()
            .filter(|e| e.key.as_slice() >= start && e.key.as_slice() <= end)
            .collect();
        if !active_entries.is_empty() {
            sources.push(Box::new(active_entries.into_iter()));
        }
        for sealed in &sv.sealed {
            let entries: Vec<Entry> = sealed
                .iter()
                .filter(|e| e.key.as_slice() >= start && e.key.as_slice() <= end)
                .collect();
            if !entries.is_empty() {
                sources.push(Box::new(entries.into_iter()));
            }
        }
        for level in &sv.version.levels {
            for table in level {
                if table.meta().key_max.as_slice() < start || table.meta().key_min.as_slice() > end
                {
                    continue;
                }
                let start_owned = start.to_vec();
                let end_owned = end.to_vec();
                let iter = SSTableBlockIter::new_with_range(
                    Arc::clone(table),
                    start,
                    Some(end),
                    crate::io::Lane::UserIORead,
                )?
                .filter(move |e| {
                    e.key.as_slice() >= start_owned.as_slice()
                        && e.key.as_slice() <= end_owned.as_slice()
                });
                sources.push(Box::new(iter));
            }
        }

        let merged = MergeIterator::new(sources);
        let mvcc =
            MvccStream::new_with_merge(merged, visible_seqno, self.merge_operator.read().clone());
        let iter = mvcc.filter(|e| e.value_type == ValueType::Value);
        Ok(Box::new(iter))
    }

    // --- Flush path ---

    /// Seal the active memtable: rotate it to sealed, create a new active.
    /// Returns true if a memtable was sealed (false if active was empty).
    pub fn seal_active(&self) -> bool {
        let _guard = self.version_update_lock.lock();
        let sv = self.current.load();
        if sv.active.is_empty() {
            return false;
        }

        let old_active = Arc::clone(&sv.active);
        let new_active = Arc::new(Memtable::new());

        let mut new_sealed = sv.sealed.clone();
        new_sealed.push(old_active);

        self.current.store(Arc::new(SuperVersion {
            active: new_active,
            sealed: new_sealed,
            version: Arc::clone(&sv.version),
        }));
        true
    }

    /// Flush all sealed memtables to SSTables in L0.
    /// Returns the number of SSTables created.
    pub fn flush_sealed(&self) -> Result<usize> {
        let sealed_memtables = {
            let sv = self.current.load();
            if sv.sealed.is_empty() {
                return Ok(0);
            }
            sv.sealed.clone()
        };

        let max_flushed = sealed_memtables
            .iter()
            .map(|m| m.highest_seqno())
            .max()
            .unwrap_or(0);

        let mut flush_config = self.config.sstable.clone();
        flush_config.compression = self.config.compression_for_level(0);
        if !self.compaction_enabled.load(Ordering::Relaxed) {
            flush_config.bloom_bits_per_key = 0.0;
        }

        // Protect this flush's SSTs from cleanup_orphan_ssts between writing
        // them and installing them in the live Version (see FlushIdGuard).
        let mut id_guard = FlushIdGuard::new(&self.flushing_ids);

        let mut new_tables = Vec::new();
        for memtable in &sealed_memtables {
            let table_id = self.next_table_id.fetch_add(1, Ordering::AcqRel);
            id_guard.register(table_id);
            let sst_path = self.sst_path(table_id);

            if let Some(_meta) = flush::flush_memtable_with_scheduler(
                memtable,
                &sst_path,
                table_id,
                &flush_config,
                Arc::clone(&self.scheduler),
            )? {
                let handle = if self.compaction_enabled.load(Ordering::Relaxed) {
                    Version::open_table_eager(
                        sst_path,
                        Arc::clone(&self.cache),
                        self.tree_id,
                        Arc::clone(&self.scheduler),
                    )?
                } else {
                    Version::open_table(
                        sst_path,
                        Arc::clone(&self.cache),
                        self.tree_id,
                        Arc::clone(&self.scheduler),
                    )?
                };
                new_tables.push(handle);
            }
        }

        let count = new_tables.len();

        let processed_count = sealed_memtables.len();
        {
            let _guard = self.version_update_lock.lock();
            let sv = self.current.load();
            let new_version = if count > 0 {
                Arc::new(sv.version.with_new_l0_tables(new_tables))
            } else {
                Arc::clone(&sv.version)
            };

            let drop_count = processed_count.min(sv.sealed.len());
            self.current.store(Arc::new(SuperVersion {
                active: Arc::clone(&sv.active),
                sealed: sv.sealed[drop_count..].to_vec(),
                version: new_version,
            }));
        }

        // Persist manifest (skip in BULKMODE — major_compact persists at the end)
        let manifest_persisted = self.compaction_enabled.load(Ordering::Relaxed);
        if manifest_persisted {
            self.persist_manifest()?;
        }

        // Update flushed_seqno — all entries up to this seqno are safely in SSTables
        if max_flushed > 0 {
            self.flushed_seqno.fetch_max(max_flushed, Ordering::AcqRel);
            // The WAL-prune watermark only advances once the manifest that REFERENCES
            // these SSTs is itself durable. In BULKMODE the manifest is deferred to
            // major_compact, so claiming WAL-safety here would lose data on crash
            // (wal-state-machine.md §6, the BULKMODE sentinel-leads-manifest trap).
            if manifest_persisted {
                self.manifest_durable_seqno
                    .fetch_max(max_flushed, Ordering::AcqRel);
            }
        }

        Ok(count)
    }

    // --- Compaction ---

    /// v0.6.1 D5 §4.7 — handle to the per-Tree compaction counter.
    /// Heat allocator worker consults `count.load(Acquire) > 0` to
    /// decide whether to skip its pass. The counter is incremented
    /// by [`CompactionGuard::new`] at the entry of `maybe_compact`
    /// and `major_compact_with_observer` and decremented on Drop.
    pub fn compaction_in_progress_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.compaction_in_progress)
    }

    /// True iff any compaction pass is currently executing on this
    /// tree (i.e. the RAII guard is alive somewhere). Surfaced for
    /// the heat allocator's compaction interlock (D5 §4.7).
    pub fn is_compacting(&self) -> bool {
        self.compaction_in_progress.load(Ordering::Acquire) > 0
    }

    /// Acquire this tree's compaction lock, **blocking until any in-flight
    /// compaction pass finishes** and releasing it only when the returned
    /// guard drops.
    ///
    /// While held, `maybe_compact` `try_lock`s and skips and
    /// `major_compact_with_observer` blocks — so no compaction can apply a
    /// version swap or call `delete_compacted_inputs`. [`Engine::create_snapshot`]
    /// holds one guard per tree across its hard-link window: setting
    /// `compaction_enabled = false` alone only stops *new* background passes,
    /// not the one already past that gate, which would otherwise unlink an
    /// SSTable mid-hard-link (→ `ENOENT`) or persist a MANIFEST skewed against
    /// the linked SST set (H12). This drains that pass before capture.
    pub fn lock_compaction(&self) -> parking_lot::MutexGuard<'_, ()> {
        self.compaction_lock.lock()
    }

    /// Run one round of compaction if needed. Returns true if compaction happened.
    pub fn maybe_compact(&self) -> Result<bool> {
        // Serialize compaction work per tree: if another compaction (bg or a
        // manual major) holds the lock, skip this pass (it will be retried on
        // the next bg tick). Prevents two compactions from folding overlapping
        // inputs and applying both merged outputs (a double-count under a merge
        // operator). Held across choose + fold + version swap.
        let _cl = match self.compaction_lock.try_lock() {
            Some(g) => g,
            None => return Ok(false),
        };
        let _guard = CompactionGuard::new(Arc::clone(&self.compaction_in_progress));
        let task = {
            let sv = self.current.load();
            leveled::choose_compaction(&sv.version, &self.config.compaction)
        };

        let task = match task {
            Some(t) => t,
            None => return Ok(false),
        };

        // H2.1 trivial-move fast path: maybe_compact has no observer, so
        // observer_allows_trivial is always true.
        {
            let sv = self.current.load();
            if leveled::is_trivial_move_candidate(&task, &sv.version, &self.config.compaction) {
                self.apply_trivial_move(&task)?;
                return Ok(true);
            }
        }

        let mut compact_config = self.config.sstable.clone();
        compact_config.compression = self.config.compression_for_level(task.target_level);

        let zm_builder = self.zone_map_builder.read().clone();
        let result = compaction_worker::execute_with_observer(
            &task,
            &self.path,
            &self.next_table_id,
            &compact_config,
            Arc::clone(&self.cache),
            self.tree_id,
            self.config.compaction.target_table_size,
            None,
            zm_builder,
            self.merge_operator.read().clone(),
            100 * 1024 * 1024, // background: 100 MB/s
            Arc::clone(&self.scheduler),
        )?;

        // Update version atomically
        {
            let _guard = self.version_update_lock.lock();
            let sv = self.current.load();
            let new_version = sv.version.with_compaction_applied(
                &result.old_ids,
                result.new_tables,
                result.target_level,
            );
            self.current.store(Arc::new(SuperVersion {
                active: Arc::clone(&sv.active),
                sealed: sv.sealed.clone(),
                version: Arc::new(new_version),
            }));
        }

        self.persist_manifest()?;
        self.delete_compacted_inputs(&result.old_ids);

        Ok(true)
    }

    /// Apply the H2.1 trivial-move on `task`: migrate the single input
    /// handle from `task.source_level` to `task.target_level` via a
    /// manifest-only update. The SSTable file stays in place with the
    /// same `table_id`. NO call to `delete_compacted_inputs` — the file
    /// is still in use at the new level.
    ///
    /// Caller must have validated `is_trivial_move_candidate` AND the
    /// observer rule (`observer.is_none() || target_level >= 2`).
    ///
    /// Wraps `persist_manifest` with `scheduler.before_op + after_op`
    /// so the §8.2 reader-feedback ladder can throttle trivial-moves
    /// under reader pressure (the manifest fsync still contends for
    /// the disk queue). The normal compaction path does not currently
    /// emit a per-manifest-fsync hook (its block-level hooks dwarf the
    /// manifest cost); this asymmetry is intentional — trivial-move,
    /// lacking block-level hooks, would be invisible to the ladder
    /// without the explicit wrap.
    fn apply_trivial_move(&self, task: &leveled::CompactionTask) -> Result<()> {
        let input = Arc::clone(&task.input_tables[0]);
        // The file_size in the encoded SSTableMeta is always 0 (writer
        // line 316: "filled after footer" — the field is set on the
        // returned `final_meta`, but the on-disk meta block was already
        // serialised with 0 before bytes_written was known). Query the
        // filesystem directly for the accurate byte count of the moved
        // SSTable.
        let bytes_saved = std::fs::metadata(&input.path).map(|m| m.len()).unwrap_or(0);
        let target_level = task.target_level;
        let lane = crate::io::Lane::Compaction {
            target_level: target_level as u8,
        };
        let kind = crate::io::OpKind::Fsync;

        self.scheduler.before_op(lane, kind);
        let t0 = std::time::Instant::now();

        {
            let _guard = self.version_update_lock.lock();
            let sv = self.current.load();
            let new_version = sv.version.with_compaction_applied(
                &task.input_ids,
                vec![Arc::clone(&input)],
                target_level,
            );
            self.current.store(Arc::new(SuperVersion {
                active: Arc::clone(&sv.active),
                sealed: sv.sealed.clone(),
                version: Arc::new(new_version),
            }));
        }
        self.persist_manifest()?;

        let elapsed_us = t0.elapsed().as_micros() as u64;
        self.scheduler.after_op(lane, kind, elapsed_us);

        self.trivial_move_count.fetch_add(1, Ordering::Relaxed);
        self.trivial_move_bytes_saved
            .fetch_add(bytes_saved, Ordering::Relaxed);
        // NO delete_compacted_inputs — the file is still in use at the
        // new level under the same table_id.
        Ok(())
    }

    /// Major compaction: compact all levels until everything is merged.
    /// Drains L0 in batches of 50 to avoid OOM with large L0 counts.
    /// Temporarily disables bg compact thread to prevent race conditions.
    ///
    /// IMPORTANT: orphan cleanup runs only ONCE at the end, not inside the loop.
    /// Running it mid-loop deletes SSTable files that are still referenced by the
    /// Version for non-overlapping key ranges (e.g., different lobe_ids).
    ///
    /// # Durability
    ///
    /// - **Precondition**: caller has sealed any active memtable whose
    ///   contents it wants compaction to see. `flush_sealed()` is
    ///   invoked internally at the top of the loop, so any sealed
    ///   memtable is drained, but an ACTIVE memtable at call time is
    ///   NOT automatically sealed here — the active handle is preserved.
    /// - **Postcondition**: every sealed memtable and every SSTable at
    ///   call time is merged into the deepest level. `flushed_seqno` is
    ///   advanced to reflect what is durably in SSTables, which is the
    ///   signal the WAL janitor reads. This function does NOT rotate
    ///   the WAL — callers that want to truncate the journal must do
    ///   so themselves AFTER calling this, and only after sealing their
    ///   active memtable first (see Finding 8).
    pub fn major_compact(&self) -> Result<()> {
        self.major_compact_with_observer(None)
    }

    /// Flush-only checkpoint (deuda #10 intermediate): seal the active memtable,
    /// flush every sealed memtable to L0, and persist the manifest so
    /// `manifest_durable_seqno` advances — WITHOUT a full compaction. The WAL
    /// pruner calls this to bound crash-recovery replay: a keyspace whose memtable
    /// never fills pins the prune watermark and the WAL grows unbounded, and a
    /// full `major_compact` is too slow under a high-scope load (hundreds of L0
    /// SSTables) to keep pace, so the WAL still grew. This does only the flush +
    /// manifest persist the pruner needs (O(new data), not O(whole dataset)).
    ///
    /// Pauses background compaction EXACTLY like `major_compact_with_observer`
    /// (acquire the compaction lock to drain any in-flight bg pass, then disable
    /// new passes) so no bg compaction races the flush — the same safety property
    /// as the `major_compact` path it replaces in the pruner. The active memtable
    /// sealed here means concurrent writes land in a fresh memtable + fresh WAL
    /// tail; the pruner PRUNES (never rotates) afterwards, so that not-yet-durable
    /// tail is never truncated.
    ///
    /// # Errors
    /// Propagates flush and manifest-persist I/O errors.
    pub fn checkpoint_flush(&self) -> Result<()> {
        // Drain any in-flight bg compaction, then stop new passes — identical to
        // major_compact_with_observer's pause.
        let _cl = self.compaction_lock.lock();
        let _guard = CompactionGuard::new(Arc::clone(&self.compaction_in_progress));
        let was_enabled = self.compaction_enabled.swap(false, Ordering::AcqRel);

        self.seal_active();
        let flush_res = self.flush_sealed();
        // `flush_sealed` skips the manifest persist while compaction is disabled
        // (the BULKMODE gate at its tail), so persist it here and advance the
        // WAL-prune watermark to the now-durable flushed seqno.
        let persist_res = self.persist_manifest();
        if flush_res.is_ok() && persist_res.is_ok() {
            self.manifest_durable_seqno
                .fetch_max(self.flushed_seqno(), Ordering::AcqRel);
        }

        self.compaction_enabled
            .store(was_enabled, Ordering::Release);
        flush_res?;
        persist_res?;
        Ok(())
    }

    /// Major compaction with an optional observer that sees every surviving entry.
    /// Used by xyzdb-engine to piggyback ghost creation on the compaction scan.
    pub fn major_compact_with_observer(
        &self,
        observer: Option<&dyn compaction_worker::CompactionObserver>,
    ) -> Result<()> {
        // Acquire the per-tree compaction lock: this BLOCKS until any in-flight
        // background `maybe_compact` finishes (the `compaction_enabled.swap`
        // below only stops NEW bg passes, not the one already running), so the
        // major compaction never folds the same inputs concurrently with a bg
        // pass and double-applies the merged output.
        let _cl = self.compaction_lock.lock();
        let _guard = CompactionGuard::new(Arc::clone(&self.compaction_in_progress));
        // H2.3 §9.3 — L0 batch size now config-driven per storage profile.
        // SSD preserves the pre-H2.3 value of 50; HDD value is sweep-driven
        // and frozen via `LeveledConfig::for_storage_profile`. Operators
        // can override at runtime via xyzdb-server's --l0-batch CLI flag,
        // which propagates through EngineConfig to this field.
        let l0_batch: usize = self.config.compaction.l0_compact_batch_size;
        const PROGRESS_LOG_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

        // Pause bg compact thread to avoid concurrent compaction races
        let was_enabled = self.compaction_enabled.swap(false, Ordering::AcqRel);

        self.flush_sealed()?;

        // Progress tracking for the in-loop log (Finding 7 Bug B1 fix c):
        // on long-running major compacts, operators need a sign of life
        // every 60 s. `initial_inputs` is the SSTable count at entry;
        // `inputs_consumed` is cumulative across iterations and CAN
        // exceed `initial_inputs` when cascading compaction re-consumes
        // outputs of prior iterations as inputs (L0→L1 output feeds into
        // an L1→L2 compaction in a later iteration). The two are
        // intentionally reported without a slash so operators do not
        // read them as a fraction.
        let mut last_log_at = std::time::Instant::now();
        let mut iteration: u64 = 0;
        let mut inputs_consumed: u64 = 0;
        let mut output_tables: u64 = 0;
        let initial_inputs: u64 = {
            let sv = self.current.load();
            sv.version.levels.iter().map(|l| l.len() as u64).sum()
        };
        // Baseline for the amplification guard: the larger of the entry table
        // count and the table count the data NATURALLY occupies at
        // target_table_size. A handful of large L0 tables legitimately explode
        // into many small output tables (target_table_size ≪ memtable), so
        // dividing inputs_consumed by the raw entry count (e.g. 4) would
        // false-flag a healthy split as runaway. Dividing by the natural table
        // count makes the ratio a true re-read multiplier.
        let amp_baseline: u64 = {
            // Scale the amplification denominator by the table count the data
            // NATURALLY occupies, derived from block_count (reliable on disk —
            // unlike file_size, which a freshly written SSTable cannot record
            // since the size includes its own footer, so it round-trips as 0).
            // A few large L0 tables legitimately explode into many small output
            // tables; dividing by the raw entry count would false-flag that
            // healthy split as runaway.
            let sv = self.current.load();
            let total_blocks: u64 = sv
                .version
                .levels
                .iter()
                .flat_map(|l| l.iter())
                .map(|t| t.meta().block_count as u64)
                .sum();
            let block_sz = self.config.sstable.data_block_size.max(1) as u64;
            let tgt = self.config.compaction.target_table_size.max(1) as u64;
            let blocks_per_target = (tgt / block_sz).max(1);
            initial_inputs.max(total_blocks / blocks_per_target)
        };
        let tree_label = self
            .path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string());

        // H2.2 §9.2 — one-shot guard. Pre-warm the L0 batch's data
        // sections into OS page cache only on the first L0 force task
        // of this major_compact_with_observer call. Subsequent
        // iterations inherit warm cache from iter 1's reads;
        // re-prewarming would just waste I/O.
        let mut pre_warmed = false;

        let max_amp = self.config.compaction.max_compaction_amplification;
        loop {
            iteration += 1;
            if last_log_at.elapsed() >= PROGRESS_LOG_INTERVAL {
                eprintln!(
                    "turba-compact: major_compact in progress: tree={} iteration={} inputs_consumed={} initial_inputs={} output_tables={}",
                    tree_label, iteration, inputs_consumed, initial_inputs, output_tables,
                );
                last_log_at = std::time::Instant::now();
            }

            // Write-amplification guard. A converging major compaction re-reads
            // each input only a handful of times; runaway re-merging (the
            // scale-1 spatial COMPACT churn) drives inputs_consumed without
            // bound. Abort with a per-level diagnosis rather than spinning for
            // hours — the engine fails knowing WHY, not silently.
            if max_amp > 0
                && amp_baseline > 0
                && inputs_consumed > amp_baseline.saturating_mul(max_amp)
            {
                // Restore the prior compaction_enabled state before bailing so
                // background compaction is not left wedged off.
                if was_enabled {
                    self.compaction_enabled.store(true, Ordering::Release);
                }
                let levels: Vec<usize> = self
                    .current
                    .load()
                    .version
                    .levels
                    .iter()
                    .map(|l| l.len())
                    .collect();
                return Err(crate::error::Error::CompactionStalled(format!(
                    "tree={tree_label} exceeded {max_amp}× write-amplification ceiling \
                     (inputs_consumed={inputs_consumed} > baseline={amp_baseline} × {max_amp}; \
                     initial_inputs={initial_inputs}) after {iteration} iterations, \
                     output_tables={output_tables}, level_table_counts={levels:?} — \
                     the level structure is not draining"
                )));
            }

            let force_l0 = {
                let sv = self.current.load();
                if sv.version.l0_table_count() > 0 {
                    Some(leveled::build_l0_task_batched(&sv.version, l0_batch))
                } else {
                    None
                }
            };

            if let Some(task) = force_l0 {
                // H2.2 §9.2 pre-warm: one-shot, before any merge work
                // touches data blocks via SSTableBlockIter. Soft-fail —
                // errors counted, never propagated.
                if !pre_warmed {
                    self.prewarm_l0_data_sections(&task.input_tables);
                    pre_warmed = true;
                }
                // H2.1 trivial-move: only when no observer (target_level=1
                // here, observer rule requires target_level >= 2 with obs).
                let trivial_eligible = observer.is_none() && {
                    let sv = self.current.load();
                    leveled::is_trivial_move_candidate(&task, &sv.version, &self.config.compaction)
                };
                if trivial_eligible {
                    let bytes_saved = task.input_tables[0].meta().file_size;
                    self.apply_trivial_move(&task)?;
                    inputs_consumed += 1;
                    // bytes_saved attributed to "outputs in the manifest"
                    // narrative — same Arc, new level.
                    output_tables += 1;
                    let _ = bytes_saved; // already accumulated in counter.
                    continue;
                }

                let mut compact_config = self.config.sstable.clone();
                compact_config.compression = self.config.compression_for_level(task.target_level);

                let result = compaction_worker::execute_with_observer(
                    &task,
                    &self.path,
                    &self.next_table_id,
                    &compact_config,
                    Arc::clone(&self.cache),
                    self.tree_id,
                    self.config.compaction.target_table_size,
                    observer,
                    None,
                    self.merge_operator.read().clone(),
                    u64::MAX, // manual: no rate limit
                    Arc::clone(&self.scheduler),
                )?;
                inputs_consumed += result.old_ids.len() as u64;
                output_tables += result.new_tables.len() as u64;

                {
                    let _guard = self.version_update_lock.lock();
                    let sv = self.current.load();
                    let new_version = sv.version.with_compaction_applied(
                        &result.old_ids,
                        result.new_tables,
                        result.target_level,
                    );
                    self.current.store(Arc::new(SuperVersion {
                        active: Arc::clone(&sv.active),
                        sealed: sv.sealed.clone(),
                        version: Arc::new(new_version),
                    }));
                }
                self.persist_manifest()?;
                self.delete_compacted_inputs(&result.old_ids);
                continue;
            }

            // L1+ compaction: same as maybe_compact but without rate limiter
            // and without zone map builder. Zone maps are built gradually by
            // background compaction after the manual COMPACT finishes.
            let task = {
                let sv = self.current.load();
                leveled::choose_compaction(&sv.version, &self.config.compaction)
            };
            match task {
                None => break,
                Some(task) => {
                    // H2.1 trivial-move: observer rule = target_level >= 2
                    // when an observer is wired in. The L1+ branch always
                    // has target_level >= 2 (L_n -> L_{n+1} with n >= 1),
                    // so trivial-move is always observer-safe here.
                    let trivial_eligible = (observer.is_none() || task.target_level >= 2) && {
                        let sv = self.current.load();
                        leveled::is_trivial_move_candidate(
                            &task,
                            &sv.version,
                            &self.config.compaction,
                        )
                    };
                    if trivial_eligible {
                        self.apply_trivial_move(&task)?;
                        inputs_consumed += 1;
                        output_tables += 1;
                        continue;
                    }

                    let mut compact_config = self.config.sstable.clone();
                    compact_config.compression =
                        self.config.compression_for_level(task.target_level);

                    let result = compaction_worker::execute_with_observer(
                        &task,
                        &self.path,
                        &self.next_table_id,
                        &compact_config,
                        Arc::clone(&self.cache),
                        self.tree_id,
                        self.config.compaction.target_table_size,
                        observer,
                        None, // no zone maps during manual COMPACT
                        self.merge_operator.read().clone(),
                        u64::MAX, // no rate limit
                        Arc::clone(&self.scheduler),
                    )?;
                    inputs_consumed += result.old_ids.len() as u64;
                    output_tables += result.new_tables.len() as u64;

                    {
                        let _guard = self.version_update_lock.lock();
                        let sv = self.current.load();
                        let new_version = sv.version.with_compaction_applied(
                            &result.old_ids,
                            result.new_tables,
                            result.target_level,
                        );
                        self.current.store(Arc::new(SuperVersion {
                            active: Arc::clone(&sv.active),
                            sealed: sv.sealed.clone(),
                            version: Arc::new(new_version),
                        }));
                    }
                    self.persist_manifest()?;
                    self.delete_compacted_inputs(&result.old_ids);
                }
            }
        }

        // Final sweep for any stragglers missed by direct deletion (e.g. crash recovery)
        self.cleanup_orphan_ssts()?;

        // Restore previous compaction_enabled state
        if was_enabled {
            self.compaction_enabled.store(true, Ordering::Release);
        }

        // major_compact sealed + flushed every memtable and persisted the manifest
        // above, so all flushed data is now manifest-durable. Catch the WAL-prune
        // watermark up to flushed_seqno (this is how it advances out of a BULKMODE
        // run, where flush_sealed deferred the manifest).
        self.manifest_durable_seqno
            .fetch_max(self.flushed_seqno.load(Ordering::Acquire), Ordering::AcqRel);

        self.major_compact_success_count
            .fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    // --- Manifest persistence ---

    fn persist_manifest(&self) -> Result<()> {
        let _guard = self.manifest_lock.lock();
        let sv = self.current.load();
        manifest::write_manifest(
            &self.path,
            &sv.version,
            self.next_table_id.load(Ordering::Acquire),
            self.seqno.load(Ordering::Acquire),
        )
    }

    /// Delete the SSTable files produced as inputs to a completed compaction.
    ///
    /// Called immediately after `persist_manifest` in `maybe_compact` and
    /// `major_compact`, once the new version is durable and the inputs are
    /// no longer reachable by new readers. POSIX `unlink` is safe even when
    /// concurrent readers hold open file descriptors: the kernel keeps the
    /// inode alive until the last descriptor closes, so in-flight reads
    /// complete correctly. Errors from `remove_file` are silently swallowed —
    /// a missing file simply means a prior cycle already cleaned it up, and
    /// `cleanup_orphan_ssts` will sweep any genuinely missed files on the next
    /// MAJOR COMPACTION — not at startup: it is only called from
    /// `major_compact_with_observer`, and open cleans `.sst.tmp` debris only. An
    /// engine that never runs a major compaction therefore accumulates orphans.
    /// They are inert (absent from every persisted manifest, so never opened) and
    /// since table ids are reconciled at open they can no longer have their id
    /// reused either — but they do occupy disk until a major compaction sweeps them.
    fn delete_compacted_inputs(&self, ids: &[u64]) {
        for id in ids {
            let path = self.path.join(format!("{id:06}.sst"));
            let _ = std::fs::remove_file(&path);
        }
    }

    /// Remove SSTable files that are no longer referenced by the current version.
    /// A file is deleted only when it is unreferenced, has `id <= max_referenced`,
    /// AND is not currently in flight from a flush (`flushing_ids`). The
    /// `max_referenced` bound alone is not sufficient: it assumes an in-flight
    /// flush always holds the highest id, but a concurrent compaction can install
    /// a higher id while a lower-id flush is mid-write (the bg flush worker is not
    /// paused by `major_compact`), so the in-flight set is consulted explicitly.
    /// Unlink is safe for files no live reader can reach: POSIX keeps the inode
    /// alive for any open descriptor until it closes.
    fn cleanup_orphan_ssts(&self) -> Result<()> {
        let sv = self.current.load();
        let referenced: std::collections::HashSet<u64> = sv
            .version
            .levels
            .iter()
            .flat_map(|l| l.iter())
            .map(|t| t.meta().table_id)
            .collect();
        let max_referenced = referenced.iter().copied().max().unwrap_or(0);
        drop(sv);

        // Snapshot ids of flushes that have written an SST but not yet
        // installed it. The `id <= max_referenced` guard alone is unsafe: a
        // concurrent compaction can install a higher id while a lower-id flush
        // is still in flight, and that flush's SST must not be unlinked.
        let in_flight = self.flushing_ids.lock().clone();

        if let Ok(entries) = std::fs::read_dir(&self.path) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str.ends_with(".sst") {
                    if let Ok(id) = name_str.trim_end_matches(".sst").parse::<u64>() {
                        if orphan_is_deletable(id, &referenced, max_referenced, &in_flight) {
                            let _ = std::fs::remove_file(entry.path());
                        }
                    }
                }
            }
        }

        Ok(())
    }

    // --- Metrics ---

    pub fn sealed_memtable_count(&self) -> usize {
        self.current.load().sealed.len()
    }

    /// Sum of `approximate_size()` across all sealed memtables currently held
    /// by the live `SuperVersion`. Non-zero values indicate flush backlog:
    /// seals are queued but the L0 writer has not caught up yet. Under bulk
    /// load each sealed memtable is ~16 MB by default, so tens of them add
    /// up to the GB range quickly.
    pub fn sealed_memtable_bytes(&self) -> usize {
        self.current
            .load()
            .sealed
            .iter()
            .map(|m| m.approximate_size())
            .sum()
    }

    pub fn l0_table_count(&self) -> usize {
        self.current.load().version.l0_table_count()
    }

    pub fn active_memtable_size(&self) -> usize {
        self.current.load().active.approximate_size()
    }

    pub fn max_memtable_size(&self) -> usize {
        self.config.max_memtable_size
    }

    /// Enable/disable background compaction on this tree.
    ///
    /// # Durability
    ///
    /// - **Precondition**: none.
    /// - **Postcondition**: the `compaction_enabled` flag is set; on
    ///   transition to `true` the compact worker is notified. This does
    ///   NOT flush memtables and does NOT touch the WAL. The flag is
    ///   also read by `WriteBatch::commit` in the parent engine to
    ///   decide whether to skip the WAL write (BULKMODE); callers that
    ///   flip this to `false` accept loss of not-yet-flushed writes on
    ///   crash until they re-enable and run `major_compact()`.
    pub fn set_compaction_enabled(&self, enabled: bool) {
        self.compaction_enabled.store(enabled, Ordering::Relaxed);
        if enabled {
            let _ = self.compact_notify.try_send(());
        }
    }

    pub fn compaction_enabled(&self) -> bool {
        self.compaction_enabled.load(Ordering::Relaxed)
    }

    /// Set the zone map builder for compaction output SSTables.
    /// Called by xyzdb-engine after boot to enable per-block zone maps.
    pub fn set_zone_map_builder(&self, builder: Arc<dyn crate::table::writer::ZoneMapBuilder>) {
        *self.zone_map_builder.write() = Some(builder);
    }

    /// Set the per-key merge operator (folds an owned key's versions at
    /// compaction and read). Called by xyzdb-engine for the dictionary tree to
    /// enable ghost rollup delta-append.
    pub fn set_merge_operator(&self, op: Arc<dyn crate::merge_op::MergeOperator>) {
        *self.merge_operator.write() = Some(op);
    }

    pub fn current_seqno(&self) -> SeqNo {
        self.seqno.load(Ordering::Acquire)
    }

    /// On-disk directory backing this tree. Used by the snapshot module
    /// (v0.4 cp 3.2.1) to discover the per-keyspace MANIFEST file path
    /// and the SSTable file directory.
    pub fn dir(&self) -> &std::path::Path {
        &self.path
    }

    /// Snapshot the paths of every live SSTable in the current
    /// SuperVersion. Cheap: an `ArcSwap::load` + a flat traversal of
    /// `version.levels`. Used by `Engine::create_snapshot` (v0.4 cp
    /// 3.2.1) to enumerate the files to hard-link before releasing
    /// the snapshot lock; subsequent compaction unlinks-by-id are
    /// safe because POSIX retains the inode while the hard-link in
    /// the snapshot directory still references it.
    pub fn live_table_paths(&self) -> Vec<std::path::PathBuf> {
        let sv = self.current.load();
        sv.version
            .levels
            .iter()
            .flat_map(|level| level.iter().map(|t| t.path.clone()))
            .collect()
    }

    /// Highest seqno that has been flushed to SSTables in this tree.
    ///
    /// # Durability
    ///
    /// - **Precondition**: none.
    /// - **Postcondition**: returns a lower bound on what is in SSTables
    ///   for this tree. Advanced by `flush_sealed()` after a successful
    ///   memtable → SSTable write, and by L0-seed compactions on the
    ///   bulk-load path. Consumers (notably the WAL janitor) use
    ///   `min(flushed_seqno)` across all five trees as the watermark
    ///   below which the WAL is safe to discard. NOTE: in BULKMODE
    ///   (compaction disabled, WAL skipped on writes) this value can
    ///   lead the WAL's view — see `WriteBatch::commit`.
    pub fn flushed_seqno(&self) -> SeqNo {
        self.flushed_seqno.load(Ordering::Acquire)
    }

    /// Highest seqno that is flushed to an SSTable AND recorded in a persisted
    /// manifest — the watermark below which this tree's WAL entries are safe to
    /// discard. Strictly `<= flushed_seqno()`; in BULKMODE it lags until
    /// `major_compact` persists the manifest. WAL segment pruning gates on
    /// `min(manifest_durable_seqno)` across all trees (never `flushed_seqno`).
    pub fn manifest_durable_seqno(&self) -> SeqNo {
        self.manifest_durable_seqno.load(Ordering::Acquire)
    }

    /// Number of failed background compact cycles since boot. Each increment
    /// corresponds to one line printed to stderr prefixed with
    /// `turba-compact: error:`. Intended as a health metric — zero under
    /// normal operation; nonzero indicates a race, IO failure, or corrupted
    /// on-disk state that deserves investigation.
    pub fn compact_error_count(&self) -> u64 {
        self.compact_error_count.load(Ordering::Relaxed)
    }

    /// Number of successful compact cycles since boot. Paired with
    /// `compact_error_count`: under sustained write load this should climb
    /// at a rate proportional to writes. Flat counter + growing L0 is the
    /// signature of "compact not keeping up" (v0.2.2 Finding 6 candidate).
    pub fn compact_success_count(&self) -> u64 {
        self.compact_success_count.load(Ordering::Relaxed)
    }

    /// Number of successful major compact cycles since boot. Distinct
    /// from `compact_success_count` (background `maybe_compact` cycles).
    /// Expected to be low cardinality — operators trigger major compact
    /// via `COMPACT` or engine shutdown, not per-write. Flat counter
    /// across a run that issued a COMPACT is a regression signal.
    pub fn major_compact_success_count(&self) -> u64 {
        self.major_compact_success_count.load(Ordering::Relaxed)
    }

    /// Number of trivial-move compactions applied since boot (H2.1).
    pub fn trivial_move_count(&self) -> u64 {
        self.trivial_move_count.load(Ordering::Relaxed)
    }

    /// Sum of `input.meta().file_size` across applied trivial-moves —
    /// the bytes that would have been re-written under non-trivial
    /// compaction. Useful for empirical sizing of the H2.1 savings.
    pub fn trivial_move_bytes_saved(&self) -> u64 {
        self.trivial_move_bytes_saved.load(Ordering::Relaxed)
    }

    /// Count of `prewarm_l0_data_sections` invocations (H2.2 §9.2).
    pub fn prewarm_l0_invocations(&self) -> u64 {
        self.prewarm_l0_invocations.load(Ordering::Relaxed)
    }

    /// Total bytes read by pre-warm sequential `std::io::copy` calls.
    pub fn prewarm_l0_bytes_read(&self) -> u64 {
        self.prewarm_l0_bytes_read.load(Ordering::Relaxed)
    }

    /// Cumulative wall-clock microseconds spent in pre-warm.
    pub fn prewarm_l0_wall_us(&self) -> u64 {
        self.prewarm_l0_wall_us.load(Ordering::Relaxed)
    }

    /// Count of per-file pre-warm errors (soft-fail; never propagated).
    pub fn prewarm_l0_errors(&self) -> u64 {
        self.prewarm_l0_errors.load(Ordering::Relaxed)
    }

    /// H2.2 §9.2 — sequentially read each L0 input file into the OS page
    /// cache before the k-way merge starts. The reads are discarded via
    /// `std::io::sink`; the optimisation is the kernel-level cache state,
    /// not in-process buffering. `BlockCache` is intentionally untouched
    /// (preserves capacity for concurrent readers; avoids upfront decode
    /// + decompression CPU cost).
    ///
    /// **Soft-fail contract** (per §9.2): per-file errors are logged via
    /// `eprintln!` (matches existing major_compact stderr discipline) and
    /// counted in `prewarm_l0_errors`, but NEVER propagated. Pre-warm is
    /// best-effort optimisation, NOT contract. Major compact proceeds
    /// regardless of how many files succeeded.
    fn prewarm_l0_data_sections(&self, l0_tables: &[Arc<crate::tree::version::TableHandle>]) {
        let t0 = std::time::Instant::now();
        let mut bytes_total: u64 = 0;
        for input in l0_tables {
            match std::fs::File::open(&input.path) {
                Ok(file) => {
                    let mut reader = std::io::BufReader::new(file);
                    match std::io::copy(&mut reader, &mut std::io::sink()) {
                        Ok(n) => bytes_total = bytes_total.saturating_add(n),
                        Err(e) => {
                            eprintln!(
                                "turba-compact: prewarm_l0 read failed for {:?}: {}",
                                input.path, e
                            );
                            self.prewarm_l0_errors.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                Err(e) => {
                    eprintln!(
                        "turba-compact: prewarm_l0 open failed for {:?}: {}",
                        input.path, e
                    );
                    self.prewarm_l0_errors.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        self.prewarm_l0_invocations.fetch_add(1, Ordering::Relaxed);
        self.prewarm_l0_bytes_read
            .fetch_add(bytes_total, Ordering::Relaxed);
        self.prewarm_l0_wall_us
            .fetch_add(t0.elapsed().as_micros() as u64, Ordering::Relaxed);
    }

    /// Per-level SSTable count from the current SuperVersion.
    /// `result[0]` = L0 count, `result[i]` = Li count. Length is
    /// `MAX_LEVELS`. Reads the ArcSwap-current version via `load`, no lock.
    pub fn level_table_counts(&self) -> Vec<usize> {
        let sv = self.current.load();
        sv.version.levels.iter().map(|l| l.len()).collect()
    }

    /// Per-level resident-metadata byte breakdown across every SSTable in
    /// the current SuperVersion. Returns three vectors aligned by level
    /// (`MAX_LEVELS` entries each): `zone_maps`, `index`, `bloom`.
    ///
    /// Diagnostic-only: the `reap-cycle` log prints these to attribute RSS
    /// growth to SSTable-derived residents vs. the block cache vs. memtables.
    /// A 64 MB SST carries ~2 MB of zone maps by design (see
    /// `SSTableMeta::zone_maps` in `table/meta.rs`); if that term grows
    /// faster than the configured block-cache cap, the residency budget is
    /// dominated by metadata, not cached blocks.
    pub fn memory_breakdown(&self) -> TreeMemoryBreakdown {
        let sv = self.current.load();
        let mut zone_maps = vec![0usize; version::MAX_LEVELS];
        let mut index = vec![0usize; version::MAX_LEVELS];
        let mut bloom = vec![0usize; version::MAX_LEVELS];
        for (lvl, tables) in sv.version.levels.iter().enumerate() {
            if lvl >= version::MAX_LEVELS {
                break;
            }
            for t in tables {
                zone_maps[lvl] += t.meta().zone_maps.len();
                index[lvl] += t.reader.index_bytes();
                bloom[lvl] += t.reader.bloom_bytes();
            }
        }
        TreeMemoryBreakdown {
            zone_maps_per_level: zone_maps,
            index_per_level: index,
            bloom_per_level: bloom,
        }
    }

    /// Count of `.sst` files visible on disk (excludes `.sst.tmp` in-flight
    /// writes). Used alongside `level_table_counts()` to detect stale files:
    /// if `disk_sst_count > sum(level_table_counts())`, old SSTables are
    /// being retained after compaction — either by an old SuperVersion that
    /// readers haven't dropped, or by a broken cleanup path.
    pub fn disk_sst_count(&self) -> usize {
        match std::fs::read_dir(&self.path) {
            Ok(entries) => entries
                .flatten()
                .filter(|e| {
                    e.file_name()
                        .to_str()
                        .map(|s| s.ends_with(".sst") && !s.ends_with(".sst.tmp"))
                        .unwrap_or(false)
                })
                .count(),
            Err(_) => 0,
        }
    }

    /// Highest table id present as `NNNNNN.sst` in `path`, or 0 if none.
    ///
    /// Used at open to keep table ids monotonic across restarts even when the
    /// manifest that would have recorded them never became durable. Ignores
    /// `.sst.tmp` (crash debris, cleaned separately) and unparseable names.
    fn max_on_disk_table_id(path: &Path) -> u64 {
        std::fs::read_dir(path)
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .filter_map(|e| {
                        let name = e.file_name();
                        let name = name.to_str()?;
                        name.strip_suffix(".sst")?.parse::<u64>().ok()
                    })
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0)
    }

    fn sst_path(&self, table_id: u64) -> PathBuf {
        self.path.join(format!("{table_id:06}.sst"))
    }

    // --- Background workers ---

    /// Start separate flush and compaction threads.
    /// Flush thread drains sealed memtables to L0 (fast).
    /// Compact thread merges L0→L1→L2+ (slow, runs in parallel).
    pub fn start_bg_worker(self: &Arc<Self>) {
        let flush_rx = self
            .flush_receiver
            .lock()
            .take()
            .expect("flush worker already started");
        let compact_rx = self
            .compact_receiver
            .lock()
            .take()
            .expect("compact worker already started");

        // --- Flush thread ---
        let tree_f = Arc::clone(self);
        let shutdown_f = Arc::clone(&self.bg_shutdown);
        let compact_tx = self.compact_notify.clone();

        let flush_handle = std::thread::Builder::new()
            .name(format!("turba-flush-{}", self.tree_id))
            .spawn(move || {
                while !shutdown_f.load(Ordering::Relaxed) {
                    let _ = flush_rx.recv_timeout(std::time::Duration::from_millis(250));
                    if shutdown_f.load(Ordering::Relaxed) {
                        break;
                    }

                    match tree_f.flush_sealed() {
                        Ok(n) if n > 0 => {
                            // New L0 tables — wake compact thread
                            let _ = compact_tx.try_send(());
                        }
                        Err(e) => eprintln!("turba-flush: error: {e}"),
                        _ => {}
                    }
                }
                // Final drain
                let _ = tree_f.flush_sealed();
                let _ = compact_tx.try_send(());
            })
            .expect("failed to spawn flush worker");

        // --- Compact thread ---
        let tree_c = Arc::clone(self);
        let shutdown_c = Arc::clone(&self.bg_shutdown);
        let compact_enabled = Arc::clone(&self.compaction_enabled);

        let compact_handle = std::thread::Builder::new()
            .name(format!("turba-compact-{}", self.tree_id))
            .spawn(move || {
                while !shutdown_c.load(Ordering::Relaxed) {
                    let _ = compact_rx.recv_timeout(std::time::Duration::from_millis(500));
                    if shutdown_c.load(Ordering::Relaxed) {
                        break;
                    }
                    if !compact_enabled.load(Ordering::Relaxed) {
                        continue;
                    }

                    // Run all pending compaction rounds
                    loop {
                        match tree_c.maybe_compact() {
                            Ok(true) => {
                                tree_c.compact_success_count.fetch_add(1, Ordering::Relaxed);
                                continue;
                            }
                            Ok(false) => break,
                            Err(e) => {
                                tree_c.compact_error_count.fetch_add(1, Ordering::Relaxed);
                                eprintln!("turba-compact: error: {e}");
                                break;
                            }
                        }
                    }
                }
                // Final drain
                while let Ok(true) = tree_c.maybe_compact() {}
            })
            .expect("failed to spawn compact worker");

        let mut handles = self.bg_handles.lock();
        handles.push(flush_handle);
        handles.push(compact_handle);
    }

    /// Ingest pre-sorted entries directly into L0 as SSTables, bypassing the memtable.
    /// Entries MUST be in sorted order. Splits output at max_memtable_size boundaries.
    /// Memory: O(block_size) constant — only one data block in RAM at a time.
    /// Used by ghost creation where entries are already sorted by sort_key.
    pub fn ingest_sorted(&self, entries: impl Iterator<Item = Entry>) -> Result<u64> {
        let mut config = self.config.sstable.clone();
        config.compression = self.config.compression_for_level(0);
        config.bloom_bits_per_key = 0.0; // ghost SSTables don't need bloom

        let target_size = self.config.max_memtable_size;
        let mut new_tables = Vec::new();
        let mut current_writer: Option<(crate::table::writer::SSTableWriter, PathBuf)> = None;
        let mut current_size = 0usize;
        let mut total = 0u64;
        let mut max_seqno = 0u64;

        for entry in entries {
            let entry_size = entry.key.len() + entry.value.len() + 20;
            if entry.seqno > max_seqno {
                max_seqno = entry.seqno;
            }

            if current_writer.is_none() || current_size >= target_size {
                if let Some((writer, path)) = current_writer.take() {
                    if let Some(_meta) = writer.finish()? {
                        let handle = Version::open_table_eager(
                            path,
                            Arc::clone(&self.cache),
                            self.tree_id,
                            Arc::clone(&self.scheduler),
                        )?;
                        new_tables.push(handle);
                    }
                }
                let table_id = self.next_table_id.fetch_add(1, Ordering::AcqRel);
                let path = self.sst_path(table_id);
                let writer = crate::table::writer::SSTableWriter::new_with_scheduler(
                    &path,
                    table_id,
                    config.clone(),
                    Arc::clone(&self.scheduler),
                    crate::io::Lane::Flush,
                )?;
                current_writer = Some((writer, path));
                current_size = 0;
            }

            if let Some((ref mut writer, _)) = current_writer {
                writer.add(entry)?;
                current_size += entry_size;
                total += 1;
            }
        }

        if let Some((writer, path)) = current_writer.take() {
            if let Some(_meta) = writer.finish()? {
                let handle = Version::open_table_eager(
                    path,
                    Arc::clone(&self.cache),
                    self.tree_id,
                    Arc::clone(&self.scheduler),
                )?;
                new_tables.push(handle);
            }
        }

        // Advance seqno past all ingested entries so reads see them (MVCC visibility)
        if max_seqno > 0 {
            self.seqno.fetch_max(max_seqno, Ordering::AcqRel);
        }

        if !new_tables.is_empty() {
            let _guard = self.version_update_lock.lock();
            let sv = self.current.load();
            let new_version = Arc::new(sv.version.with_new_l0_tables(new_tables));
            self.current.store(Arc::new(SuperVersion {
                active: Arc::clone(&sv.active),
                sealed: sv.sealed.clone(),
                version: new_version,
            }));
        }

        // Always persist manifest after ingest — the new SSTables must be recorded
        // for recovery, regardless of compaction state.
        self.persist_manifest()?;

        Ok(total)
    }

    /// Notify flush thread (sealed memtable ready).
    pub fn notify_bg(&self) {
        let _ = self.flush_notify.try_send(());
    }

    /// Shutdown both background threads.
    pub fn shutdown_bg(&self) {
        self.bg_shutdown.store(true, Ordering::Relaxed);
        let _ = self.flush_notify.try_send(());
        let _ = self.compact_notify.try_send(());
        for handle in self.bg_handles.lock().drain(..) {
            let _ = handle.join();
        }
    }
}

/// v0.6.1 D5 §4.7 — RAII guard for the per-Tree compaction counter.
///
/// Constructed at the entry of `Tree::maybe_compact` and
/// `Tree::major_compact_with_observer`. Increments the counter on
/// construction, decrements on Drop. Even if the compaction returns
/// `Err(_)`, the Drop runs and the counter returns to zero.
///
/// The heat allocator worker (v0.6.1 §2) reads
/// `compaction_in_progress.load(Acquire) > 0` once per pass and
/// skips emit when any compaction is active — the "no moves during
/// compaction" rule from D5 §4.7.
pub struct CompactionGuard {
    counter: Arc<AtomicU64>,
}

impl CompactionGuard {
    pub fn new(counter: Arc<AtomicU64>) -> Self {
        counter.fetch_add(1, Ordering::AcqRel);
        Self { counter }
    }
}

impl Drop for CompactionGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::orphan_is_deletable;
    use std::collections::HashSet;

    fn set(ids: &[u64]) -> HashSet<u64> {
        ids.iter().copied().collect()
    }

    /// The P1-4 regression: a flush wrote `100.sst` and registered it in
    /// `flushing_ids` but has not installed it; a concurrent compaction then
    /// installed `101`, so `max_referenced = 101` and `100 <= 101`. Without the
    /// in-flight check the file would be unlinked out from under the flush. With
    /// it, the in-flight id is protected.
    #[test]
    fn in_flight_flush_id_below_a_compacted_id_is_not_deleted() {
        let referenced = set(&[101]);
        let in_flight = set(&[100]);
        assert!(
            !orphan_is_deletable(100, &referenced, 101, &in_flight),
            "an in-flight flush SST must never be deleted, even when a higher \
             compacted id has raised max_referenced past it"
        );
    }

    #[test]
    fn genuinely_orphaned_id_is_deleted() {
        // 50 is unreferenced, below max_referenced (101), and not in flight.
        let referenced = set(&[100, 101]);
        let in_flight = HashSet::new();
        assert!(orphan_is_deletable(50, &referenced, 101, &in_flight));
    }

    #[test]
    fn referenced_id_is_never_deleted() {
        let referenced = set(&[100, 101]);
        let in_flight = HashSet::new();
        assert!(!orphan_is_deletable(100, &referenced, 101, &in_flight));
    }

    #[test]
    fn id_above_max_referenced_is_left_alone() {
        // A freshly written higher id the version has not caught up to.
        let referenced = set(&[100]);
        let in_flight = HashSet::new();
        assert!(!orphan_is_deletable(150, &referenced, 100, &in_flight));
    }
}
