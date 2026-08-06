//! Compaction worker: reads input SSTables, merges, writes output SSTables.

// SPDX-License-Identifier: BUSL-1.1
use crate::cache::BlockCache;
use crate::compaction::leveled::CompactionTask;
use crate::compaction::stream::CompactionStream;
use crate::error::Result;
use crate::merge::MergeIterator;
use crate::merge_op::MergeOperator;
use crate::table::reader::SSTableBlockIter;
use crate::table::writer::{SSTableConfig, SSTableWriter, ZoneMapBuilder};
use crate::tree::version::{TableHandle, Version};
use crate::types::Entry;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Advise the OS to release page cache for a file after compaction reads.
/// On Linux (Docker), uses posix_fadvise(FADV_DONTNEED).
/// No-op on other platforms.
/// Advise the OS to release page cache for a file after compaction.
/// Linux: posix_fadvise(FADV_DONTNEED). macOS: fcntl(F_NOCACHE).
/// Advise the OS to release page cache for a file after compaction.
/// Linux (Docker): FADV_DONTNEED — container has limited RAM, eviction helps.
/// macOS: no-op — page cache accelerates multi-pass compaction by keeping
/// output SSTables in RAM for the next round's input reads.
fn drop_page_cache(path: &Path) {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        if let Ok(file) = std::fs::File::open(path) {
            // SAFETY: `file` owns a valid open fd for the duration of this block;
            // `posix_fadvise` takes only integers (offset 0, len 0 = the whole
            // file) and touches no user memory, so a live fd is its only
            // precondition. The advisory return code is intentionally ignored.
            unsafe {
                libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
    }
}

/// Rate limiter for compaction I/O. Limits bytes written per second to leave
/// disk bandwidth for foreground reads and writes.
pub struct RateLimiter {
    bytes_per_sec: u64,
    bytes_since_check: u64,
    last_check: std::time::Instant,
}

impl RateLimiter {
    pub fn new(bytes_per_sec: u64) -> Self {
        Self {
            bytes_per_sec,
            bytes_since_check: 0,
            last_check: std::time::Instant::now(),
        }
    }

    /// Call after writing `n` bytes. Sleeps if rate exceeded.
    pub fn consume(&mut self, n: usize) {
        self.bytes_since_check += n as u64;
        // Check every 1MB to avoid syscall overhead
        if self.bytes_since_check < 1_048_576 {
            return;
        }
        let elapsed = self.last_check.elapsed();
        let expected = std::time::Duration::from_secs_f64(
            self.bytes_since_check as f64 / self.bytes_per_sec as f64,
        );
        if expected > elapsed {
            std::thread::sleep(expected - elapsed);
        }
        self.bytes_since_check = 0;
        self.last_check = std::time::Instant::now();
    }
}

pub struct CompactionResult {
    pub new_tables: Vec<Arc<TableHandle>>,
    pub old_ids: Vec<u64>,
    pub target_level: usize,
}

/// Observer called for each surviving entry during compaction.
/// Allows higher layers (e.g., ghost creation) to piggyback on the compaction scan
/// without additional I/O. Called after MVCC cleanup — only live entries are observed.
pub trait CompactionObserver: Send + Sync {
    fn observe(&self, key: &[u8], value: &[u8]);
}

/// Execute a compaction task: merge inputs → write outputs.
/// Background compaction: rate-limited to 100 MB/s.
// Compaction inputs; bundling into a struct is a design change, deferred (not a lint fix).
#[allow(clippy::too_many_arguments)]
pub fn execute(
    task: &CompactionTask,
    tables_dir: &Path,
    next_table_id: &AtomicU64,
    config: &SSTableConfig,
    cache: Arc<BlockCache>,
    tree_id: u64,
    target_table_size: usize,
    scheduler: Arc<crate::io::Scheduler>,
) -> Result<CompactionResult> {
    execute_with_observer(
        task,
        tables_dir,
        next_table_id,
        config,
        cache,
        tree_id,
        target_table_size,
        None,
        None,
        None,
        100 * 1024 * 1024,
        scheduler,
    )
}

/// Execute compaction with an optional observer, zone map builder, and merge
/// operator (the latter folds an owned key's versions during the compaction).
#[allow(clippy::too_many_arguments)]
pub fn execute_with_observer(
    task: &CompactionTask,
    tables_dir: &Path,
    next_table_id: &AtomicU64,
    config: &SSTableConfig,
    cache: Arc<BlockCache>,
    tree_id: u64,
    target_table_size: usize,
    observer: Option<&dyn CompactionObserver>,
    zone_map_builder: Option<Arc<dyn ZoneMapBuilder>>,
    merge_operator: Option<Arc<dyn MergeOperator>>,
    rate_limit_bytes_per_sec: u64,
    scheduler: Arc<crate::io::Scheduler>,
) -> Result<CompactionResult> {
    // Compaction lane carries the target_level for sub-priority routing.
    // SILK's "low-level critical, high-level background" principle is
    // applied at dispatch time (H1); accounting collapses all
    // Compaction { .. } onto a single per-lane slot.
    let compact_lane = crate::io::Lane::Compaction {
        target_level: task.target_level as u8,
    };

    // 1. Create streaming block-by-block iterators (no full SSTable load)
    let mut sources: Vec<Box<dyn Iterator<Item = Entry>>> = Vec::new();
    for table in &task.input_tables {
        let iter = SSTableBlockIter::new(Arc::clone(table), compact_lane)?;
        sources.push(Box::new(iter));
    }

    // 2. K-way merge
    let merged = MergeIterator::new(sources);

    // 3. Compaction stream: MVCC cleanup + tombstone eviction + optional merge fold
    let compaction_stream =
        CompactionStream::new_with_merge(merged, task.is_last_level, 0, merge_operator);

    // 4. Write output SSTables (rotate when exceeding target size)
    let mut rate_limiter = RateLimiter::new(rate_limit_bytes_per_sec);
    let mut new_tables = Vec::new();
    let mut current_writer: Option<(SSTableWriter, PathBuf, u64)> = None;
    let mut current_size = 0usize;

    for entry in compaction_stream {
        // Notify observer of each surviving entry (ghost creation piggybacks here)
        if let Some(obs) = observer {
            if entry.value_type == crate::types::ValueType::Value {
                obs.observe(&entry.key, &entry.value);
            }
        }

        let entry_size = entry.key.len() + entry.value.len() + 20;

        if current_writer.is_none() || current_size >= target_table_size {
            // Finish current writer
            if let Some((writer, path, _tid)) = current_writer.take() {
                if let Some(_meta) = writer.finish()? {
                    drop_page_cache(&path);
                    let handle = Version::open_table_eager(
                        path,
                        Arc::clone(&cache),
                        tree_id,
                        Arc::clone(&scheduler),
                    )?;
                    new_tables.push(handle);
                }
            }

            // Start new writer
            let table_id = next_table_id.fetch_add(1, Ordering::AcqRel);
            let path = tables_dir.join(format!("{table_id:06}.sst"));
            let writer = SSTableWriter::with_zone_map_builder(
                &path,
                table_id,
                config.clone(),
                zone_map_builder.clone(),
                Arc::clone(&scheduler),
                compact_lane,
            )?;
            current_writer = Some((writer, path, table_id));
            current_size = 0;
        }

        if let Some((ref mut writer, _, _)) = current_writer {
            writer.add(entry)?;
            current_size += entry_size;
            rate_limiter.consume(entry_size);
        }
    }

    // Finish last writer
    if let Some((writer, path, _tid)) = current_writer.take() {
        if let Some(_meta) = writer.finish()? {
            drop_page_cache(&path);
            let handle = Version::open_table_eager(
                path,
                Arc::clone(&cache),
                tree_id,
                Arc::clone(&scheduler),
            )?;
            new_tables.push(handle);
        }
    }

    // Evict input SSTable pages from OS cache — they've been read once for compaction
    for table in &task.input_tables {
        drop_page_cache(&table.path);
    }

    Ok(CompactionResult {
        new_tables,
        old_ids: task.input_ids.clone(),
        target_level: task.target_level,
    })
}
