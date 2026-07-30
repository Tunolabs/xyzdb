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
use std::path::PathBuf;
use std::sync::Arc;

pub const MAX_LEVELS: usize = 7;

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
    /// it panics in debug/test builds and logs a loud error in release, because
    /// overlap is a compaction bug to surface, not a recoverable data state to
    /// tolerate silently.
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
            debug_assert!(false, "{msg}");
            tracing::error!("{msg}");
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
}
