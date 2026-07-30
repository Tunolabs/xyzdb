//! Flush: convert a sealed memtable into an SSTable on disk.

use crate::error::Result;
use crate::memtable::Memtable;
use crate::table::meta::SSTableMeta;
use crate::table::writer::{SSTableConfig, SSTableWriter};
use std::path::Path;
use std::sync::Arc;

/// Test-only (8b/8c): while armed, every memtable flush fails with ENOSPC,
/// simulating a disk-full that stalls background flush/compaction. Unlike the
/// one-shot WAL hook, this is PERSISTENT so the sealed-memtable backlog
/// builds and stays — exercising throttle back-pressure (Paused) and the
/// bounded `wait_compaction_settle` (no hang), then recovery when disarmed.
#[cfg(feature = "durability-test-hooks")]
pub static FORCE_FLUSH_ENOSPC: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Convenience wrapper for tests: defaults to a fresh Passthrough
/// scheduler. Production code MUST use [`flush_memtable_with_scheduler`]
/// with the engine's shared `Arc<Scheduler>` so flush writes register on
/// `Lane::Flush`.
pub fn flush_memtable(
    memtable: &Memtable,
    path: &Path,
    table_id: u64,
    config: &SSTableConfig,
) -> Result<Option<SSTableMeta>> {
    flush_memtable_with_scheduler(
        memtable,
        path,
        table_id,
        config,
        Arc::new(crate::io::Scheduler::passthrough()),
    )
}

/// Canonical flush: writes the memtable to an SSTable, instrumenting
/// every kernel write through the supplied scheduler on `Lane::Flush`.
pub fn flush_memtable_with_scheduler(
    memtable: &Memtable,
    path: &Path,
    table_id: u64,
    config: &SSTableConfig,
    scheduler: Arc<crate::io::Scheduler>,
) -> Result<Option<SSTableMeta>> {
    if memtable.is_empty() {
        return Ok(None);
    }

    // 8b/8c: simulate a disk-full stalling background flush.
    #[cfg(feature = "durability-test-hooks")]
    if FORCE_FLUSH_ENOSPC.load(std::sync::atomic::Ordering::Relaxed) {
        return Err(std::io::Error::from_raw_os_error(28).into());
    }

    let mut writer = SSTableWriter::new_with_scheduler(
        path,
        table_id,
        config.clone(),
        scheduler,
        crate::io::Lane::Flush,
    )?;
    for entry in memtable.iter() {
        writer.add(entry)?;
    }
    writer.finish()
}
