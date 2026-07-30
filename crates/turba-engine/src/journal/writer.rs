//! Journal writer: appends encoded batches to the WAL file with optional fsync.

use crate::error::Result;
use crate::journal::entry::{BatchItem, encode_batch};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Test-only (3e): arm a one-shot disk-full (ENOSPC) on the NEXT WAL write.
/// Mimics a real torn write — a partial record reaches disk, then the write
/// fails — so recovery must discard the partial batch (entry.rs checksum/End)
/// and `commit` must surface the error (never a false ack).
#[cfg(feature = "durability-test-hooks")]
pub static FORCE_WRITE_ENOSPC: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Test-only (5a/5b): while armed, every WAL fsync (`sync_data`) fails with
/// EIO. Drives the periodic/Batched `persist()` fsync down the failure path —
/// the path 3a's group-commit poison did NOT cover — to prove it now poisons
/// and surfaces instead of silently swallowing the error.
#[cfg(feature = "durability-test-hooks")]
pub static FORCE_SYNC_DATA_ERROR: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistMode {
    /// Buffer writes — OS decides when to flush. Fast but not crash-safe per batch.
    Buffer,
    /// fsync after each batch write. Slow but crash-safe.
    SyncData,
}

/// Default size at which the active WAL segment rolls over to a new file.
/// Bounds the WAL at roughly `segment_max_bytes × (segments holding
/// not-yet-manifest-durable data + active)` instead of the full write history.
pub const DEFAULT_SEGMENT_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Append-only WAL with seqno-ordered SEGMENTS. Writes go to the active
/// segment (`journal.wal`); when it exceeds `segment_max_bytes` it rolls to
/// an archived segment `journal.<max_seqno>.wal` and a fresh active file
/// starts. `prune(watermark)` deletes archived segments whose every entry is
/// already manifest-durable (max_seqno ≤ watermark) — lossless, delete-only,
/// never touching the active segment or unflushed tail (wal-state-machine.md
/// §2/§4; this is the "rotate_up_to(seqno)" architecture deferred from
/// Finding 10's gating).
pub struct JournalWriter {
    writer: BufWriter<File>,
    path: PathBuf,
    /// Directory holding the active + archived segments (parent of `path`).
    dir: PathBuf,
    persist_mode: PersistMode,
    /// Shared I/O scheduler. Every WAL write + fsync registers as
    /// `Lane::WriterDurable` — never preempted (cycle doc §6 D3).
    scheduler: Arc<crate::io::Scheduler>,
    /// Roll the active segment once it reaches this many bytes.
    segment_max_bytes: u64,
    /// Bytes written to the current active segment (rolls when ≥ segment_max_bytes).
    active_bytes: u64,
    /// Highest seqno written to the active segment (its name on roll).
    active_max_seqno: u64,
    /// Archived (rolled) segments as (path, max_seqno), in roll order.
    segments: Vec<(PathBuf, u64)>,
}

impl JournalWriter {
    fn dir_of(path: &Path) -> PathBuf {
        path.parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
    }

    pub fn create(
        path: &Path,
        persist_mode: PersistMode,
        scheduler: Arc<crate::io::Scheduler>,
        segment_max_bytes: u64,
    ) -> Result<Self> {
        let dir = Self::dir_of(path);
        // Fresh start: any archived segments left by a previous run have already
        // been recovered + flushed by the open path before this call, so they are
        // stale — remove them so the directory reflects only the new active WAL.
        Self::remove_archived_segments(&dir);
        let file = File::create(path)?;
        Ok(Self {
            writer: BufWriter::new(file),
            path: path.to_path_buf(),
            dir,
            persist_mode,
            scheduler,
            segment_max_bytes,
            active_bytes: 0,
            active_max_seqno: 0,
            segments: Vec::new(),
        })
    }

    pub fn open_append(
        path: &Path,
        persist_mode: PersistMode,
        scheduler: Arc<crate::io::Scheduler>,
        segment_max_bytes: u64,
    ) -> Result<Self> {
        let dir = Self::dir_of(path);
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let active_bytes = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            writer: BufWriter::new(file),
            path: path.to_path_buf(),
            dir,
            persist_mode,
            scheduler,
            segment_max_bytes,
            active_bytes,
            active_max_seqno: 0,
            segments: Vec::new(),
        })
    }

    /// Delete every archived segment file (`journal.<n>.wal`) in `dir`. Used on
    /// fresh `create()` (stragglers are stale post-recovery) — never touches the
    /// active `journal.wal`.
    fn remove_archived_segments(dir: &Path) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries.flatten() {
                let p = e.path();
                if let Some(name) = p.file_name().and_then(|s| s.to_str()) {
                    if name
                        .strip_prefix("journal.")
                        .and_then(|s| s.strip_suffix(".wal"))
                        .and_then(|m| m.parse::<u64>().ok())
                        .is_some()
                    {
                        let _ = std::fs::remove_file(&p);
                    }
                }
            }
        }
    }

    /// Roll the active segment to an archived file named by its max seqno and
    /// start a fresh active segment. Precondition: the buffer is flushed+synced
    /// (callers invoke this only right after a successful `sync()`/`write_batch`
    /// fsync, so `journal.wal` on disk is a complete segment).
    fn maybe_roll(&mut self) -> Result<()> {
        if self.active_bytes < self.segment_max_bytes || self.active_max_seqno == 0 {
            return Ok(());
        }
        let archived = self
            .dir
            .join(format!("journal.{:020}.wal", self.active_max_seqno));
        std::fs::rename(&self.path, &archived)?;
        let _ = crate::manifest::fsync_dir(&self.dir);
        let file = File::create(&self.path)?;
        self.writer = BufWriter::new(file);
        self.segments.push((archived, self.active_max_seqno));
        self.active_bytes = 0;
        Ok(())
    }

    /// Delete archived segments whose entries are ALL ≤ `watermark` (the
    /// min manifest-durable seqno across trees) — they are fully persisted in
    /// SSTables recorded by a durable manifest, so dropping them from the WAL
    /// loses nothing on crash. The active segment and any archived segment
    /// holding seqno > watermark are kept. Returns bytes freed.
    pub fn prune(&mut self, watermark: u64) -> Result<u64> {
        let mut freed = 0u64;
        let mut kept = Vec::new();
        for (path, max_seqno) in std::mem::take(&mut self.segments) {
            if max_seqno <= watermark {
                if let Ok(meta) = std::fs::metadata(&path) {
                    freed += meta.len();
                }
                std::fs::remove_file(&path)?;
            } else {
                kept.push((path, max_seqno));
            }
        }
        self.segments = kept;
        if freed > 0 {
            let _ = crate::manifest::fsync_dir(&self.dir);
        }
        Ok(freed)
    }

    /// Account a kernel write at WriterDurable lane.
    fn instrumented_write(&mut self, buf: &[u8]) -> Result<()> {
        let bytes = buf.len() as u32;
        self.scheduler.before_op(
            crate::io::Lane::WriterDurable,
            crate::io::OpKind::Write { bytes },
        );
        let start = std::time::Instant::now();
        #[cfg(feature = "durability-test-hooks")]
        if FORCE_WRITE_ENOSPC.swap(false, std::sync::atomic::Ordering::Relaxed) {
            // Disk-full mid-write: flush a PARTIAL record to disk (the torn
            // tail recovery must discard), then fail with ENOSPC. One-shot.
            let half = buf.len() / 2;
            let _ = self.writer.write_all(&buf[..half]);
            let _ = self.writer.flush();
            self.scheduler.after_op(
                crate::io::Lane::WriterDurable,
                crate::io::OpKind::Write { bytes },
                start.elapsed().as_micros() as u64,
            );
            return Err(std::io::Error::from_raw_os_error(28).into());
        }
        let res = self.writer.write_all(buf);
        self.scheduler.after_op(
            crate::io::Lane::WriterDurable,
            crate::io::OpKind::Write { bytes },
            start.elapsed().as_micros() as u64,
        );
        res?;
        Ok(())
    }

    /// Account a kernel fsync at WriterDurable lane.
    fn instrumented_sync_data(&mut self) -> std::io::Result<()> {
        self.scheduler
            .before_op(crate::io::Lane::WriterDurable, crate::io::OpKind::Fsync);
        let start = std::time::Instant::now();
        #[cfg(feature = "durability-test-hooks")]
        if FORCE_SYNC_DATA_ERROR.load(std::sync::atomic::Ordering::Relaxed) {
            self.scheduler.after_op(
                crate::io::Lane::WriterDurable,
                crate::io::OpKind::Fsync,
                start.elapsed().as_micros() as u64,
            );
            return Err(std::io::Error::from_raw_os_error(5)); // EIO
        }
        let res = self.writer.get_ref().sync_data();
        self.scheduler.after_op(
            crate::io::Lane::WriterDurable,
            crate::io::OpKind::Fsync,
            start.elapsed().as_micros() as u64,
        );
        res
    }

    /// Write a batch to the journal. Optionally fsync based on persist_mode.
    pub fn write_batch(&mut self, seqno: u64, items: &[BatchItem]) -> Result<()> {
        let encoded = encode_batch(seqno, items);
        self.instrumented_write(&encoded)?;
        self.active_bytes += encoded.len() as u64;
        self.active_max_seqno = self.active_max_seqno.max(seqno);

        if self.persist_mode == PersistMode::SyncData {
            self.writer.flush()?;
            self.instrumented_sync_data()?;
            self.maybe_roll()?;
        }

        Ok(())
    }

    /// Write a batch to the buffer WITHOUT fsync. Used by group commit path.
    /// Caller is responsible for calling sync() later.
    ///
    /// # Durability
    ///
    /// - **Precondition**: none on the writer side. The caller MUST enroll
    ///   its seqno into the group-commit epoch sequence after this returns
    ///   and then block until the sync thread advances `synced_epoch` past
    ///   that epoch (see `WriteBatch::commit`).
    /// - **Postcondition**: bytes are in the `BufWriter`. They are NOT on
    ///   disk and NOT acknowledgeable to the client until a subsequent
    ///   `sync()` completes successfully. Returning `Ok` here is therefore
    ///   NOT a durability guarantee; only the group-commit barrier in
    ///   `WriteBatch::commit` is.
    pub fn write_batch_buffered(&mut self, seqno: u64, items: &[BatchItem]) -> Result<()> {
        let encoded = encode_batch(seqno, items);
        self.instrumented_write(&encoded)?;
        self.active_bytes += encoded.len() as u64;
        self.active_max_seqno = self.active_max_seqno.max(seqno);
        Ok(())
    }

    pub fn persist_mode(&self) -> PersistMode {
        self.persist_mode
    }

    /// Force flush + fsync regardless of persist_mode. Called by the
    /// group-commit sync thread on its 1 ms cadence.
    pub fn sync(&mut self) -> Result<()> {
        self.writer.flush()?;
        self.instrumented_sync_data()?;
        self.maybe_roll()?;
        Ok(())
    }

    /// Rotate the journal: sync current, truncate and start fresh.
    /// Safe to call only when all data has been flushed to SSTables
    /// (i.e., after major_compact or full flush + compaction).
    ///
    /// # Durability
    ///
    /// - **Precondition (invariant D1)**: every batch that this journal has
    ///   ever acknowledged to a client MUST already be persisted in an
    ///   SSTable. In practice that means every tree has seen both
    ///   `seal_active()` AND `flush_sealed()` (or an equivalent
    ///   `major_compact` path that invokes both) BEFORE `rotate()` is
    ///   called. Violating this precondition loses acknowledged writes on
    ///   crash — the truncated WAL cannot replay them.
    /// - **Postcondition**: the WAL file is a zero-byte file. Any future
    ///   crash recovery will see an empty WAL and recover only from
    ///   SSTables. There is no way to undo this operation.
    ///
    /// Known callers that must establish the precondition:
    /// - `Engine::major_compact` (turba-engine) — seals + flushes first.
    /// - `execute_compact` (xyzdb-engine) — seals + flushes first.
    /// - WAL janitor — gated behind `durability-test-hooks` feature; NOT
    ///   production-safe without a seal/flush step, see Finding 10.
    ///
    /// **Compliance review**: any new caller must satisfy the checklist in
    /// `docs/wal-state-machine.md` §7 before merging.
    pub fn rotate(&mut self) -> Result<()> {
        // Sync any buffered data
        self.writer.flush()?;
        self.instrumented_sync_data()?;

        // Truncate: reopen the same path, creating a fresh empty file.
        // 3g note: this truncate's directory entry is not dir-fsynced. Benign
        // by the rotate() precondition — all acked data is already in SSTs, so
        // a lost truncate on power loss just leaves the old (already-flushed)
        // WAL, which recovery replays idempotently (MVCC dedup). No data loss.
        let file = File::create(&self.path)?;
        self.writer = BufWriter::new(file);
        self.active_bytes = 0;

        // The precondition (all acked data in SSTs) also makes every archived
        // segment stale — drop them so COMPACT still takes the WAL to zero.
        for (path, _) in std::mem::take(&mut self.segments) {
            let _ = std::fs::remove_file(&path);
        }
        let _ = crate::manifest::fsync_dir(&self.dir);

        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Total on-disk WAL size: the active segment plus every archived segment.
    /// The WAL pruner reads this to bound crash-recovery replay — a WAL grown
    /// past the memory-derived threshold (because a lagging keyspace pins the
    /// prune watermark) forces a checkpoint (`docs/wal-state-machine.md`;
    /// deuda #10 intermediate). Cheap: one `stat` per archived segment, called
    /// on the pruner's ~1 s cadence.
    ///
    /// # Returns
    ///
    /// Bytes across `journal.wal` and all `journal.<n>.wal` archived segments.
    pub fn total_bytes(&self) -> u64 {
        let archived: u64 = self
            .segments
            .iter()
            .filter_map(|(p, _)| std::fs::metadata(p).ok().map(|m| m.len()))
            .sum();
        self.active_bytes + archived
    }
}

impl Drop for JournalWriter {
    fn drop(&mut self) {
        let _ = self.writer.flush();
        let _ = self.writer.get_ref().sync_data();
    }
}
