//! Version management for the LSM tree.
//!
//! - `Version`: immutable snapshot of the SSTable hierarchy (levels + tables).
//! - `SuperVersion`: the complete read state = active memtable + sealed memtables + Version.
//!
//! Each flush or compaction creates a new Version. SuperVersion is swapped atomically
//! under write lock so concurrent readers see a consistent state.

use crate::cache::BlockCache;
use crate::memtable::Memtable;
use crate::table::meta::SSTableMeta;
use crate::table::reader::SSTableReader;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

pub const MAX_LEVELS: usize = 7;

/// Count of L1+ non-overlap invariant violations observed in this process.
///
/// Bumped by [`Version::check_level_non_overlapping`] whenever an overlapping
/// L1+ run is detected — the state that makes `get_at`'s per-level binary search
/// able to silently miss present keys.
///
/// Process-global on purpose: the guard runs from two static-ish contexts (tree
/// open, before a `Tree` exists; and `Version::with_compaction_applied`, which
/// returns a fresh `Version`), so threading a per-tree field through them would
/// be plumbing without extra signal — there is exactly one such invariant today.
/// Read it via [`level_overlap_violations`]; it is surfaced in the engine's stats
/// so a health query sees it in EVERY configuration, subscriber or not.
static LEVEL_OVERLAP_VIOLATIONS: AtomicU64 = AtomicU64::new(0);

/// Per-keyspace breakdown of the same violations.
///
/// The total answers *"did it happen"*; this answers *"where"* — which is half the
/// diagnosis, because the keyspace decides the blast radius. An overlap in
/// `dictionary` means a point-get can miss an anchor key, so a duplicate-anchor
/// check can fail to detect a duplicate and an idempotent insert stops being
/// idempotent; the same overlap in `spatial` degrades a record read instead. The
/// label is derived from the table's own path (`…/<keyspace>/<id>.sst`), so no
/// call site had to grow a parameter.
///
/// A `Mutex<BTreeMap>` and not atomics: the guard fires approximately never, so
/// lock cost is irrelevant, and a map keeps the keyspace set open (a sixth
/// keyspace needs no code change here).
static LEVEL_OVERLAP_BY_KEYSPACE: std::sync::LazyLock<std::sync::Mutex<BTreeMap<String, u64>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(BTreeMap::new()));

/// Number of L1+ non-overlap invariant violations seen since process start.
///
/// **Non-zero means a correctness-relevant invariant fired**: an overlapping L1+
/// run breaks the read path's binary search, so point reads can miss keys that a
/// scan still finds. It never resets and never decreases.
///
/// # Returns
/// The running count across every keyspace. `0` in a healthy process.
pub fn level_overlap_violations() -> u64 {
    LEVEL_OVERLAP_VIOLATIONS.load(AtomicOrdering::Relaxed)
}

/// Per-keyspace counts behind [`level_overlap_violations`], keyed by the
/// keyspace directory name (`spatial`, `identity`, `dictionary`, `ghosts`,
/// `vectors`, or `unknown` if the path shape is unexpected).
///
/// # Returns
/// A snapshot of the map. Empty in a healthy process.
pub fn level_overlap_by_keyspace() -> BTreeMap<String, u64> {
    LEVEL_OVERLAP_BY_KEYSPACE
        .lock()
        .map(|m| m.clone())
        .unwrap_or_default()
}

/// The keyspace a table belongs to, from its path: `…/<keyspace>/<id>.sst`.
fn keyspace_of(table: &TableHandle) -> String {
    table
        .path
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Handle to an SSTable on disk with its pre-loaded reader.
///
/// `meta` is borrowed from the reader via `meta()` rather than owned here.
/// An earlier design kept a cloned `SSTableMeta` alongside the reader; since
/// `SSTableMeta.zone_maps` can run ~2 MB per 64 MB SSTable (see `meta.rs`),
/// the duplicate doubled the RSS cost per table with zero benefit — the
/// reader already owns the authoritative copy and the handle always lives
/// with its reader.
pub struct TableHandle {
    pub reader: SSTableReader,
    pub path: PathBuf,
}

impl TableHandle {
    /// Borrow the SSTable metadata from the underlying reader. Prefer this
    /// over accessing `reader.meta()` directly at call sites that previously
    /// used the removed `meta` field.
    pub fn meta(&self) -> &SSTableMeta {
        self.reader.meta()
    }
}

/// Returns the index `i` of the first table whose key range overlaps its
/// predecessor in a level sorted ascending by `key_min`, or `None` if the run is
/// non-overlapping. `bounds[k] = (key_min, key_max)`. Tables in one L1+ level
/// hold disjoint key sets, so a shared boundary key (`prev_max == this_min`)
/// counts as overlap. Pure and allocation-free so the `get_at` binary-search
/// precondition can be unit-tested without building SSTables; see
/// [`Version::check_level_non_overlapping`].
fn first_overlapping_index(bounds: &[(&[u8], &[u8])]) -> Option<usize> {
    (1..bounds.len()).find(|&i| {
        let prev_max = bounds[i - 1].1;
        let this_min = bounds[i].0;
        prev_max >= this_min
    })
}

/// Immutable snapshot of the LSM tree's on-disk state.
pub struct Version {
    /// levels[0] = L0 (may have overlapping tables), levels[1..] = L1+ (sorted, non-overlapping).
    pub levels: Vec<Vec<Arc<TableHandle>>>,
}

impl Version {
    pub fn new() -> Self {
        Self {
            levels: (0..MAX_LEVELS).map(|_| Vec::new()).collect(),
        }
    }

    /// Add tables to L0 (flush result).
    pub fn with_new_l0_tables(&self, tables: Vec<Arc<TableHandle>>) -> Self {
        let mut levels = self.levels.clone();
        levels[0].extend(tables);
        Self { levels }
    }

    /// Replace tables at specific levels after compaction.
    /// Removes `old_ids` and adds `new_tables` at `target_level`.
    pub fn with_compaction_applied(
        &self,
        old_ids: &[u64],
        new_tables: Vec<Arc<TableHandle>>,
        target_level: usize,
    ) -> Self {
        let mut levels = self.levels.clone();

        // Remove old tables from all levels
        for level in &mut levels {
            level.retain(|t| !old_ids.contains(&t.meta().table_id));
        }

        // Add new tables to target level.
        if target_level < levels.len() {
            levels[target_level].extend(new_tables);
            // L1+ MUST stay sorted by key_min: `Tree::get_at` point-lookups
            // binary-search each level on the [key_min, key_max] range, which is
            // only correct on a sorted, non-overlapping run (the level invariant
            // documented at the top of this file). `extend` appends the freshly
            // merged output — whose key range belongs in the MIDDLE of the level
            // — at the END, so after the first mid-range compaction the level is
            // out of order and binary_search starts MISSING keys that are
            // present. Range scans (`prefix_iter`) don't binary-search, so they
            // kept finding the data — which is why this surfaced only as silent
            // empty ghost reads (the no-projection ghost fallback point-reads)
            // at scale, never as a scan error. L0 is exempt: it may overlap and
            // `get_at` scans it linearly.
            if target_level >= 1 {
                levels[target_level].sort_by(|a, b| a.meta().key_min.cmp(&b.meta().key_min));
                Self::check_level_non_overlapping(target_level, &levels[target_level]);
            }
        }

        Self { levels }
    }

    /// Integrity guard: verify an L1+ level is a non-overlapping run after it has
    /// been sorted by `key_min`.
    ///
    /// `Tree::get_at` binary-searches each L1+ level on the `[key_min, key_max]`
    /// range, which is correct only when the run is both sorted AND
    /// non-overlapping. Sorting alone does not guarantee the latter — two SSTs
    /// from a buggy compaction can share a key range — so a violation lets point
    /// reads silently miss present keys (the L1+ data-miss class fixed at the
    /// data level in v0.7.x). This asserts the invariant the read path assumes:
    /// it panics in debug/test builds, logs a loud error, AND bumps a counter,
    /// because overlap is a compaction bug to surface, not a recoverable data
    /// state to tolerate silently.
    ///
    /// ## Why a counter and not only a log (2026-07-31)
    ///
    /// The log alone made this guard **invisible to library consumers**. Three
    /// configurations, only two of which could ever speak:
    /// - debug/test: `debug_assert!` panics → visible;
    /// - release WITH a `tracing` subscriber installed (e.g. `xyzdb-server`) →
    ///   the `error!` prints → visible;
    /// - release WITHOUT a subscriber — **every embedder that links this crate as
    ///   a library, because installing a subscriber is the caller's job and most
    ///   callers do not** → the assert is compiled out and `error!` is a no-op, so
    ///   a fired invariant left **no trace anywhere**.
    ///
    /// An invariant owned by the engine must not delegate its visibility to the
    /// caller's logging plumbing. [`level_overlap_violations`] makes it observable
    /// as **state**, readable in every configuration, and lets a harness assert on
    /// a deterministic counter instead of scraping text (fragile to filter level,
    /// format, and buffering around a process that is about to die). Same shape as
    /// the SCRUB findings, which are attended precisely because the caller reads
    /// them.
    ///
    /// # Arguments
    /// * `level_idx` - The level being checked. Must be `>= 1`; L0 is exempt (it
    ///   may overlap and `get_at` scans it linearly).
    /// * `level` - The level's tables, already sorted ascending by `key_min`.
    pub(crate) fn check_level_non_overlapping(level_idx: usize, level: &[Arc<TableHandle>]) {
        let bounds: Vec<(&[u8], &[u8])> = level
            .iter()
            .map(|t| (t.meta().key_min.as_slice(), t.meta().key_max.as_slice()))
            .collect();
        if let Some(i) = first_overlapping_index(&bounds) {
            let msg = format!(
                "L{level_idx} overlap: table[{}].key_max >= table[{}].key_min — \
                 overlapping run breaks get_at's binary search (point reads may \
                 silently miss present keys)",
                i - 1,
                i
            );
            // Record BEFORE the assert: in debug the panic must not swallow the
            // observation, so a harness that catches the panic still sees the count.
            LEVEL_OVERLAP_VIOLATIONS.fetch_add(1, AtomicOrdering::Relaxed);
            let keyspace = level
                .first()
                .map(|t| keyspace_of(t))
                .unwrap_or_else(|| "unknown".to_string());
            if let Ok(mut by_ks) = LEVEL_OVERLAP_BY_KEYSPACE.lock() {
                *by_ks.entry(keyspace.clone()).or_insert(0) += 1;
            }
            debug_assert!(false, "{keyspace}/{msg}");
            tracing::error!(keyspace = %keyspace, "{msg}");
        }
    }

    pub fn l0_table_count(&self) -> usize {
        self.levels[0].len()
    }

    pub fn total_table_count(&self) -> usize {
        self.levels.iter().map(|l| l.len()).sum()
    }

    /// Approximate disk space across all levels.
    pub fn disk_space(&self) -> u64 {
        self.levels
            .iter()
            .flat_map(|l| l.iter())
            .map(|t| t.meta().file_size)
            .sum()
    }

    /// Open a table from disk (lazy pread handle — zero FDs until first block read).
    pub fn open_table(
        path: PathBuf,
        cache: Arc<BlockCache>,
        tree_id: u64,
        scheduler: Arc<crate::io::Scheduler>,
    ) -> crate::error::Result<Arc<TableHandle>> {
        let reader = SSTableReader::open_with_tree_id(&path, cache, tree_id, scheduler)?;
        Ok(Arc::new(TableHandle { reader, path }))
    }

    /// Open a table with eager pread handle — used for compaction output where
    /// the file may be deleted by cleanup_orphan_ssts while still being read.
    /// POSIX keeps open handles valid after unlink.
    pub fn open_table_eager(
        path: PathBuf,
        cache: Arc<BlockCache>,
        tree_id: u64,
        scheduler: Arc<crate::io::Scheduler>,
    ) -> crate::error::Result<Arc<TableHandle>> {
        let reader = SSTableReader::open_with_tree_id(&path, cache, tree_id, scheduler)?;
        reader.warm_handle()?;
        Ok(Arc::new(TableHandle { reader, path }))
    }
}

impl Default for Version {
    fn default() -> Self {
        Self::new()
    }
}

/// The complete read state at a point in time.
pub struct SuperVersion {
    pub active: Arc<Memtable>,
    pub sealed: Vec<Arc<Memtable>>,
    pub version: Arc<Version>,
}

impl SuperVersion {
    pub fn new(active: Arc<Memtable>, version: Arc<Version>) -> Self {
        Self {
            active,
            sealed: Vec::new(),
            version,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::first_overlapping_index;

    /// The `get_at` L1+ binary-search precondition: a level sorted by `key_min`
    /// must also be non-overlapping. `first_overlapping_index` is the pure core
    /// of the integrity guard installed at every compaction-apply / manifest-load
    /// site; these cases pin its contract.
    #[test]
    fn non_overlapping_runs_are_accepted() {
        // Disjoint, ascending: the well-formed L1+ run.
        let bounds: &[(&[u8], &[u8])] = &[(b"a", b"c"), (b"d", b"f"), (b"g", b"k"), (b"m", b"z")];
        assert_eq!(first_overlapping_index(bounds), None);
    }

    #[test]
    fn empty_and_singleton_levels_never_overlap() {
        assert_eq!(first_overlapping_index(&[]), None);
        assert_eq!(first_overlapping_index(&[(b"a", b"z")]), None);
    }

    #[test]
    fn true_range_overlap_is_detected() {
        // table[1] starts at "e", inside table[0]'s [a, k] range.
        let bounds: &[(&[u8], &[u8])] = &[(b"a", b"k"), (b"e", b"q")];
        assert_eq!(first_overlapping_index(bounds), Some(1));
    }

    #[test]
    fn shared_boundary_key_counts_as_overlap() {
        // Adjacent tables touching on "f": same user key in two L1+ SSTs is the
        // forbidden state (prev.key_max == this.key_min), so this is overlap.
        let bounds: &[(&[u8], &[u8])] = &[(b"a", b"f"), (b"f", b"z")];
        assert_eq!(first_overlapping_index(bounds), Some(1));
    }

    #[test]
    fn reports_the_first_offending_pair() {
        // First two are disjoint; the overlap is between index 2 and 3.
        let bounds: &[(&[u8], &[u8])] = &[(b"a", b"c"), (b"d", b"f"), (b"g", b"p"), (b"k", b"z")];
        assert_eq!(first_overlapping_index(bounds), Some(3));
    }

    // ─── Negative control for the invariant counter ──────────────────────────
    //
    // The cases above pin the pure core. They do NOT prove the COUNTER is wired:
    // if `LEVEL_OVERLAP_VIOLATIONS` never incremented, every run would report a
    // healthy zero and the harness gate built on it would be decoration. A
    // detector that cannot fire is not evidence — so this drives the real guard,
    // through real SSTables, and asserts the counter moves.

    use super::{Version, level_overlap_violations};
    use crate::cache::BlockCache;
    use crate::table::writer::{SSTableConfig, SSTableWriter};
    use crate::types::{Entry, ValueType};
    use std::sync::{Arc, Mutex};

    /// The violation counter is process-global, so the two tests below — which
    /// assert on a DELTA — must not run concurrently: one bumping while the other
    /// samples its baseline makes the healthy case read the neighbour's increment.
    /// (Found by this very control failing under the default parallel runner; the
    /// harness gate in `crash_recovery.rs` is immune because it asserts `== 0`
    /// absolutely, not a delta.)
    static COUNTER_TESTS: Mutex<()> = Mutex::new(());

    /// Write a one-per-key SSTable spanning `keys` and open it as a handle.
    fn table_spanning(dir: &std::path::Path, id: u64, keys: &[&[u8]]) -> Arc<super::TableHandle> {
        let path = dir.join(format!("{id:06}.sst"));
        let mut w = SSTableWriter::new(&path, id, SSTableConfig::default()).expect("writer");
        for (i, k) in keys.iter().enumerate() {
            w.add(Entry::new(
                k.to_vec(),
                b"v".to_vec(),
                (i + 1) as u64,
                ValueType::Value,
            ))
            .expect("add");
        }
        w.finish().expect("finish").expect("non-empty table");
        Version::open_table(
            path,
            Arc::new(BlockCache::new(1 << 20)),
            0,
            Arc::new(crate::io::Scheduler::passthrough()),
        )
        .expect("open table")
    }

    #[test]
    fn overlap_guard_increments_the_counter() {
        let _serial = COUNTER_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        // Two REAL tables whose key ranges overlap: [a..k] and [e..q]. This is the
        // forbidden L1+ state that makes get_at's binary search able to miss keys.
        let t1 = table_spanning(dir.path(), 1, &[b"a", b"k"]);
        let t2 = table_spanning(dir.path(), 2, &[b"e", b"q"]);

        let before = level_overlap_violations();
        // In debug the guard's `debug_assert!` panics, so catch it — the counter is
        // bumped BEFORE the assert precisely so the observation survives the panic.
        // The hook is silenced so the expected panic does not look like a failure.
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            Version::check_level_non_overlapping(1, &[t1, t2]);
        }));
        std::panic::set_hook(prev_hook);

        assert_eq!(
            level_overlap_violations(),
            before + 1,
            "the overlap guard must record the violation as STATE; without this the \
             harness gate and the stats field would report a healthy zero forever"
        );
        // …and it must say WHERE. The tables live in a directory named after the
        // keyspace, which is how the label is derived; a wrong label would make the
        // breakdown worse than useless (it would point the next investigation at
        // the wrong keyspace).
        let ks = dir
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert_eq!(
            super::level_overlap_by_keyspace().get(&ks).copied(),
            Some(1),
            "the violation must be attributed to the keyspace the tables live in ({ks})"
        );
        // Debug builds additionally panic (the assert); release builds return Ok.
        // Both are acceptable — the counter is the configuration-independent signal.
        let _ = outcome;
    }

    #[test]
    fn healthy_level_leaves_the_counter_untouched() {
        let _serial = COUNTER_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        // The other half of the control: the counter must NOT drift on a sane
        // level, or "non-zero means a real violation" would be a lie.
        let dir = tempfile::tempdir().expect("tempdir");
        let t1 = table_spanning(dir.path(), 11, &[b"a", b"c"]);
        let t2 = table_spanning(dir.path(), 12, &[b"m", b"z"]);
        let before = level_overlap_violations();
        Version::check_level_non_overlapping(1, &[t1, t2]);
        assert_eq!(
            level_overlap_violations(),
            before,
            "a non-overlapping level must not bump the violation counter"
        );
    }
}
