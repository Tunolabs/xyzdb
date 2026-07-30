//! Leveled compaction strategy.
//!
//! Level sizes: base_size × ratio^level
//!   L0: triggers when >= max_l0_tables (default 4)
//!   L1: base_size (default 64MB)
//!   L2: base_size × ratio (default 640MB, ratio=10)
//!   ...
//!
//! L0 → L1: merge ALL L0 tables with overlapping L1 tables.
//! L(n) → L(n+1): pick the table with most overlap, merge with overlapping tables at L(n+1).

use crate::tree::version::{MAX_LEVELS, TableHandle, Version};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct LeveledConfig {
    pub max_l0_tables: usize,     // trigger L0→L1 compaction (default 4)
    pub base_size_bytes: u64,     // L1 target size (default 64MB)
    pub size_ratio: u64,          // multiplier per level (default 10)
    pub target_table_size: usize, // max bytes per output SSTable during compaction (default 64MB)
    /// Trigger L(n)→L(n+1) compaction when L(n) holds more than this many
    /// tables, even if total bytes remain under the level's byte target.
    /// Guards against the "many-tiny-tables" pathology: an L0 compaction
    /// with 4 small L0 tables (from partial memtable flushes) produces a
    /// small L1 table. 50 such compactions leave L1 with 50 × ~1 MB
    /// tables totalling 50 MB — below the 64 MB byte target — but each
    /// table still holds its own reader state (bloom + index + meta
    /// ≈ 380 KB in memory) and SSTable file on disk. With 50 tables
    /// that's ~19 MB of process RSS just for reader state, plus OS page
    /// cache over the files. Pure-byte scheduling never triggers the
    /// L1→L2 compaction that would consolidate them. Default 20 (= 5×
    /// the default max_l0_tables).
    pub max_tables_per_level: usize,
    /// Master kill-switch for the H2.1 trivial-move optimisation. When
    /// `true`, single-input no-target-overlap compactions become a
    /// manifest-only update (the SSTable file stays in place, the
    /// handle migrates between levels). When `false`, every compaction
    /// goes through the rewrite path — the pre-H2.1 behaviour.
    ///
    /// Default `true`. Kill-switch: disable if a future workload
    /// violates the tombstone-rarity assumption (heavy DELETE traffic
    /// at the engine layer would accumulate tombstones in trivial-moved
    /// files, deferring their elision indefinitely).
    pub enable_trivial_move: bool,
    /// L0 batch size for `major_compact_with_observer`'s force-L0 path
    /// — the maximum number of L0 input tables consumed in a single
    /// compaction iteration. Pre-H2.3 was a `const L0_BATCH = 50`
    /// hard-coded across all storage profiles; H2.3 makes it
    /// per-profile config and bench-driven for HDD via
    /// [`Self::for_storage_profile`].
    ///
    /// Smaller batch → more iterations → more manifest persists +
    /// scheduler hops + cleanup_orphan calls per major compact, but
    /// each iteration's cold-load cost is smaller. The per-storage-profile
    /// value is bench-frozen at the point where that curve bends.
    pub l0_compact_batch_size: usize,
    /// Write-amplification ceiling for a single `major_compact` run, as a
    /// multiple of the table count present at entry. The loop aborts with
    /// [`crate::error::Error::CompactionStalled`] once
    /// `inputs_consumed > initial_inputs × this` — converting a
    /// non-convergent churn (the scale-1 spatial COMPACT that re-merged for
    /// 12 h: `initial_inputs ~1049`, `inputs_consumed` climbing past 5 800
    /// with `output_tables ~4917` and no termination) into a fast, diagnosed
    /// failure instead of a silent multi-hour hang. A healthy leveled major
    /// compaction re-reads each input only a handful of times, so the default
    /// (64×) leaves wide headroom; `0` disables the guard.
    pub max_compaction_amplification: u64,
}

impl Default for LeveledConfig {
    fn default() -> Self {
        Self {
            max_l0_tables: 4,
            base_size_bytes: 64 * 1024 * 1024,
            size_ratio: 10,
            target_table_size: 64 * 1024 * 1024,
            max_tables_per_level: 20,
            enable_trivial_move: true,
            l0_compact_batch_size: 50,
            max_compaction_amplification: 64,
        }
    }
}

impl LeveledConfig {
    /// Construct a `LeveledConfig` whose `l0_compact_batch_size` reflects
    /// the per-storage-profile bench-frozen value. SSD preserves the
    /// pre-H2.3 batch size of 50; HDD is bench-driven via the H2.3 sweep
    /// protocol.
    ///
    /// During the H2.3 sweep itself, both arms hold `50` (preserving the
    /// pre-H2.3 behaviour); the `feat(engine): freeze HDD L0 batch to <N>
    /// (H2.3 sweep result)` commit updates the HDD arm post-sweep.
    pub fn for_storage_profile(profile: crate::config::StorageProfile) -> Self {
        let l0_batch = match profile {
            crate::config::StorageProfile::Ssd => 50,
            // Pre-sweep placeholder. The H2.3 sweep + freeze commit
            // updates this to the empirical winner. NOT 25 by default —
            // committing a number a-priori contradicts §9.3's
            // bench-driven discipline.
            crate::config::StorageProfile::Hdd => 50,
        };
        Self {
            l0_compact_batch_size: l0_batch,
            ..Self::default()
        }
    }
}

pub struct CompactionTask {
    pub input_tables: Vec<Arc<TableHandle>>,
    pub input_ids: Vec<u64>,
    pub source_level: usize,
    pub target_level: usize,
    pub is_last_level: bool,
}

/// Hard cap on L0 overflow ratio before higher-level compaction is
/// considered. With L0 at ≥ `L0_EMERGENCY_RATIO × max_l0_tables` the
/// compactor always drains L0, ignoring any higher-level backlog.
/// Protects against point-read latency blowing up during a long
/// L1→L2 catch-up while writes keep adding L0 tables.
///
/// At `max_l0_tables = 4` and ratio 3.0, L0 is capped at 12 tables.
/// That bound is empirical: more than ~12 L0 tables starts degrading
/// read latency meaningfully because each point read has to consult
/// every L0 table.
const L0_EMERGENCY_RATIO: f64 = 3.0;

/// Choose which compaction to perform, if any.
///
/// Ratio-based priority (v0.2.2 Finding 6 fix). Each level's overflow
/// ratio is computed against its target (L0: table count; L1+: bytes),
/// and the level with the highest overflow is compacted. Two exceptions:
///
///   1. **L0 emergency**: if L0 overflow ratio ≥ `L0_EMERGENCY_RATIO`,
///      drain L0 first regardless of higher-level backlog. Prevents
///      the point-read latency spike that would occur if L0 grew
///      unbounded while a long L1→L2 compaction runs.
///
///   2. **No overflow anywhere**: returns `None`.
///
/// This replaces the previous strict "L0 first, L1+ second" priority,
/// which under sustained write load left L0 perpetually at its cap
/// and starved L1+ levels. Observed in v0.2.1 matrix OFF runs:
/// L1 grew to 104 tables over 10 min with L2 = 0 throughout.
pub fn choose_compaction(version: &Version, config: &LeveledConfig) -> Option<CompactionTask> {
    let l0_ratio = if config.max_l0_tables == 0 {
        0.0
    } else {
        version.levels[0].len() as f64 / config.max_l0_tables as f64
    };

    // L0 emergency: always drain first once ratio crosses the hard cap.
    if l0_ratio >= L0_EMERGENCY_RATIO {
        return Some(build_l0_compaction(version));
    }

    // Otherwise compact whichever level is most over its target.
    let mut best: Option<(f64, usize)> = None;
    if l0_ratio >= 1.0 {
        best = Some((l0_ratio, 0));
    }

    for level_idx in 1..MAX_LEVELS - 1 {
        let level_tables = &version.levels[level_idx];
        let level_count = level_tables.len();
        let level_size: u64 = level_tables.iter().map(|t| t.meta().file_size).sum();

        let bytes_target = config
            .base_size_bytes
            .saturating_mul(config.size_ratio.saturating_pow(level_idx as u32 - 1));

        // Count target scales with the level's byte budget. A level whose byte
        // budget legitimately holds N = bytes_target / target_table_size
        // correctly-sized tables must not be flagged as count-overflowing at a
        // flat 20: deep levels (L3+: budget ≫ 20 × target_table_size) would
        // then perpetually report overflow and cascade their entire contents
        // downward on every compaction — O(levels × data) write amplification.
        // That is the scale-1 spatial COMPACT churn (initial_inputs ~1049 →
        // output_tables ~4917, hours of re-merging 64 MB tables). The
        // `max_tables_per_level` floor still guards shallow levels (L1/L2,
        // whose budget implies < 20 tables) against the many-tiny-tables RSS
        // pathology that motivated the count criterion in the first place.
        let natural_tables = if config.target_table_size > 0 {
            (bytes_target / config.target_table_size as u64).max(1) as usize
        } else {
            config.max_tables_per_level
        };
        let count_target = config.max_tables_per_level.max(natural_tables);

        // Dual-criterion overflow: byte-based OR count-based. Whichever
        // is higher wins. Prevents many-tiny-tables from being invisible
        // to the byte-based scheduler (v0.2.2 Finding 6 second-iteration
        // root cause: L0→L1 with small L0 tables produced tiny L1 tables
        // that accumulated without ever exceeding the byte target).
        let bytes_ratio = if bytes_target > 0 {
            level_size as f64 / bytes_target as f64
        } else {
            0.0
        };
        let count_ratio = if count_target > 0 {
            level_count as f64 / count_target as f64
        } else {
            0.0
        };
        let ratio = bytes_ratio.max(count_ratio);

        if ratio > 1.0 && best.is_none_or(|(r, _)| ratio > r) {
            best = Some((ratio, level_idx));
        }
    }

    match best {
        Some((_, 0)) => Some(build_l0_compaction(version)),
        Some((_, level)) => Some(build_level_compaction(version, level)),
        None => None,
    }
}

/// Build an L0 → L1 compaction task (public for major_compact force path).
pub fn build_l0_task(version: &Version) -> CompactionTask {
    build_l0_compaction(version)
}

/// Build an L0 → L1 compaction task limited to `batch_size` L0 tables.
/// Used for incremental compaction after bulk load.
pub fn build_l0_task_batched(version: &Version, batch_size: usize) -> CompactionTask {
    build_l0_compaction_batched(version, batch_size)
}

/// L0 → L1: merge a batch of L0 tables with overlapping L1 tables.
fn build_l0_compaction_batched(version: &Version, batch_size: usize) -> CompactionTask {
    let l0_tables = &version.levels[0];
    // Take the oldest (first) batch_size tables
    let batch: Vec<_> = l0_tables.iter().take(batch_size).cloned().collect();

    let (range_min, range_max) = l0_key_range(&batch);

    let l1_overlapping: Vec<_> = version.levels[1]
        .iter()
        .filter(|t| overlaps(&t.meta().key_min, &t.meta().key_max, &range_min, &range_max))
        .cloned()
        .collect();

    let mut input_tables: Vec<Arc<TableHandle>> = batch;
    input_tables.extend(l1_overlapping);

    let input_ids: Vec<u64> = input_tables.iter().map(|t| t.meta().table_id).collect();
    let is_last_level = version.levels[2..].iter().all(|l| l.is_empty());

    CompactionTask {
        input_tables,
        input_ids,
        source_level: 0,
        target_level: 1,
        is_last_level,
    }
}

/// L0 → L1: merge all L0 tables with overlapping L1 tables.
fn build_l0_compaction(version: &Version) -> CompactionTask {
    let l0_tables = &version.levels[0];

    // Find key range of all L0 tables
    let (range_min, range_max) = l0_key_range(l0_tables);

    // Find overlapping L1 tables
    let l1_overlapping: Vec<_> = version.levels[1]
        .iter()
        .filter(|t| overlaps(&t.meta().key_min, &t.meta().key_max, &range_min, &range_max))
        .cloned()
        .collect();

    let mut input_tables: Vec<Arc<TableHandle>> = l0_tables.to_vec();
    input_tables.extend(l1_overlapping);

    let input_ids: Vec<u64> = input_tables.iter().map(|t| t.meta().table_id).collect();

    let is_last_level = version.levels[2..].iter().all(|l| l.is_empty());

    CompactionTask {
        input_tables,
        input_ids,
        source_level: 0,
        target_level: 1,
        is_last_level,
    }
}

/// L(n) → L(n+1): pick the largest table at L(n), merge with overlapping at L(n+1).
fn build_level_compaction(version: &Version, source_level: usize) -> CompactionTask {
    let target_level = source_level + 1;

    // Pick the table with the largest file_size at source level
    let picked = version.levels[source_level]
        .iter()
        .max_by_key(|t| t.meta().file_size)
        .cloned()
        .expect("level is non-empty");

    let key_min = &picked.meta().key_min;
    let key_max = &picked.meta().key_max;

    // Find overlapping tables at target level
    let overlapping: Vec<_> = if target_level < version.levels.len() {
        version.levels[target_level]
            .iter()
            .filter(|t| overlaps(&t.meta().key_min, &t.meta().key_max, key_min, key_max))
            .cloned()
            .collect()
    } else {
        Vec::new()
    };

    let mut input_tables = vec![picked];
    input_tables.extend(overlapping);

    let input_ids: Vec<u64> = input_tables.iter().map(|t| t.meta().table_id).collect();

    let is_last_level = target_level >= MAX_LEVELS - 1
        || version.levels[target_level + 1..]
            .iter()
            .all(|l| l.is_empty());

    CompactionTask {
        input_tables,
        input_ids,
        source_level,
        target_level,
        is_last_level,
    }
}

fn l0_key_range(tables: &[Arc<TableHandle>]) -> (Vec<u8>, Vec<u8>) {
    let min = tables
        .iter()
        .map(|t| t.meta().key_min.as_slice())
        .min()
        .unwrap_or(&[])
        .to_vec();
    let max = tables
        .iter()
        .map(|t| t.meta().key_max.as_slice())
        .max()
        .unwrap_or(&[])
        .to_vec();
    (min, max)
}

pub(crate) fn overlaps(a_min: &[u8], a_max: &[u8], b_min: &[u8], b_max: &[u8]) -> bool {
    a_min <= b_max && b_min <= a_max
}

/// Returns true if the compaction task qualifies for the H2.1 trivial-move
/// optimisation: a single source-level input with no target-level overlap,
/// and the bounded-depth guard against amplification at target_level + 1.
///
/// Caller must ALSO validate the observer rule separately
/// (`observer.is_none() || target_level >= 2`) — that varies per call site
/// (background `maybe_compact` has no observer; `major_compact_with_observer`
/// may have one wired in for spatial-tree ghost build).
pub fn is_trivial_move_candidate(
    task: &CompactionTask,
    version: &Version,
    config: &LeveledConfig,
) -> bool {
    if !config.enable_trivial_move {
        return false;
    }
    // build_level_compaction produces input_tables = [picked] + overlapping;
    // input_tables.len() == 1 implies no target overlap. build_l0_compaction*
    // can also produce a single-input task when only 1 L0 table exists with
    // no L1 overlap (rare, but possible post-bulk).
    if task.input_tables.len() != 1 {
        return false;
    }
    let input = &task.input_tables[0];
    // Bounded depth: input range must not overlap > 25 × target_table_size
    // of files in target_level + 1. RocksDB's `max_compaction_bytes` guard
    // (HDD-Investigation1 §4.1). Without it, trivial-move just defers
    // amplification one level deeper.
    let tp1 = task.target_level + 1;
    if tp1 < MAX_LEVELS {
        let key_min = &input.meta().key_min;
        let key_max = &input.meta().key_max;
        let overlap_bytes: u64 = version.levels[tp1]
            .iter()
            .filter(|t| overlaps(&t.meta().key_min, &t.meta().key_max, key_min, key_max))
            .map(|t| t.meta().file_size)
            .sum();
        let max_overlap = (config.target_table_size as u64).saturating_mul(25);
        if overlap_bytes > max_overlap {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StorageProfile;

    #[test]
    fn leveled_config_default_l0_batch_is_50() {
        let c = LeveledConfig::default();
        assert_eq!(c.l0_compact_batch_size, 50);
    }

    #[test]
    fn leveled_config_for_storage_profile_ssd_l0_batch_is_50() {
        let c = LeveledConfig::for_storage_profile(StorageProfile::Ssd);
        assert_eq!(
            c.l0_compact_batch_size, 50,
            "SSD must preserve pre-H2.3 L0 batch behaviour"
        );
    }

    #[test]
    fn leveled_config_for_storage_profile_hdd_uses_frozen_value() {
        // The HDD value is bench-driven by the H2.3 sweep + freeze
        // protocol. Pre-sweep placeholder is 50 (preserves pre-H2.3
        // behaviour); the freeze commit updates it to the empirical
        // winner. The test asserts the value is sane (positive,
        // <= SSD baseline) — the exact frozen value is data, not
        // contract.
        let c = LeveledConfig::for_storage_profile(StorageProfile::Hdd);
        assert!(c.l0_compact_batch_size > 0);
        assert!(
            c.l0_compact_batch_size <= 50,
            "HDD L0 batch must not exceed the SSD baseline; freeze commit updates this"
        );
    }
}
