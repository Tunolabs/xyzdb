//! TurbaEngine: the top-level storage engine with 5 fixed keyspaces.
//!
//! Wraps 5 Trees (spatial, identity, dictionary, ghosts, vectors) with a
//! shared block cache, WAL for durability, and background flush/compact
//! workers.

// SPDX-License-Identifier: BUSL-1.1
use crate::cache::BlockCache;
use crate::compaction::leveled::LeveledConfig;
use crate::compression::CompressionType;
use crate::config::{EngineConfig, IoSchedulerMode, StorageProfile};
use crate::error::Result;
use crate::journal::entry::BatchItem;
use crate::journal::recovery;
use crate::journal::writer::{JournalWriter, PersistMode};
use crate::table::writer::SSTableConfig;
use crate::tree::{Tree, TreeConfig};
use crate::types::{SeqNo, ValueType};
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Keyspace identifiers — fixed, not dynamic.
pub const KS_SPATIAL: u8 = 0;
pub const KS_IDENTITY: u8 = 1;
pub const KS_DICTIONARY: u8 = 2;
pub const KS_GHOSTS: u8 = 3;
/// 5th keyspace: per-record vector column, keyed by the same spatial key as
/// the record it belongs to.
pub const KS_VECTORS: u8 = 4;

const JOURNAL_FILE: &str = "journal.wal";

/// Group commit synchronization. Writers write to the WAL buffer and wait.
/// A sync thread periodically fsyncs and wakes all waiting writers.
struct GroupSync {
    /// Incremented by the sync thread after each fsync.
    synced_epoch: AtomicU64,
    /// Incremented by each writer after writing to buffer.
    pending_epoch: AtomicU64,
    /// Writers park here waiting for synced_epoch to advance.
    notify: std::sync::Condvar,
    lock: std::sync::Mutex<()>,
    /// Test-only: when set, the sync thread skips its fsync + advance
    /// cycle so tests can reproduce the crash-window scenario.
    /// `load(Ordering::Relaxed)` per sync-thread iteration; the thread
    /// already sleeps 1 ms per iteration, so the atomic read is
    /// imperceptible (~1 ns on a hot cache line).
    #[cfg(feature = "durability-test-hooks")]
    paused: AtomicBool,
    /// Unix timestamp (ms since epoch) of the last successful
    /// `journal.sync()`. 0 means "no sync has succeeded yet" — either
    /// the thread has not run, is not needed (non-Durable mode), or the
    /// last N cycles all failed. Consumers (/stats, operator dashboards)
    /// compare this to wall-clock time to detect a stalled sync thread:
    /// a value that stops advancing under write load implies writers
    /// are blocked on a broken durability path (see Finding 9).
    last_successful_sync_ts_ms: AtomicU64,
    /// Monotonic counter incremented at the top of every sync-thread
    /// iteration, regardless of whether the iteration did any work.
    /// Liveness signal distinct from `last_successful_sync_ts_ms`:
    /// heartbeat advancing while `last_successful_sync_ts_ms` stays
    /// flat means the thread is alive but every `j.sync()` is failing.
    heartbeat_count: AtomicU64,
    /// Set true if a WAL `sync()` ever returns `Err` (e.g. fsync EIO). On
    /// Linux a failed fsync clears the dirty-page error state, so a retry
    /// can return Ok WITHOUT the bytes reaching disk — acking a silently
    /// lost write (the fsyncgate class). Once set: the sync thread stops
    /// attempting fsync, waiting writers wake and return an error instead
    /// of a false success, and new commits fail fast. Never cleared at
    /// runtime; a poisoned WAL requires a restart, after which recovery
    /// replays whatever reached the WAL durably.
    poisoned: AtomicBool,
    /// Test-only: force the next WAL `sync()` to fail, driving the
    /// fsyncgate poison path deterministically (no real disk fault).
    #[cfg(feature = "durability-test-hooks")]
    force_sync_error: AtomicBool,
}

/// WAL size (bytes) above which the pruner forces a checkpoint so crash-recovery
/// replay stays within the memory envelope (deuda #10 intermediate). `open()`'s
/// pruner normally deletes archived segments once they are manifest-durable, but
/// a keyspace whose memtable never fills pins the prune watermark and the WAL
/// grows unbounded until a shutdown/COMPACT — a hard crash would then replay the
/// whole WAL into one memtable and OOM the restart. Derived from the cgroup
/// memory limit: recovery holds the WAL roughly twice (decoded batches + rebuilt
/// memtables), so a quarter of the limit keeps the replay peak comfortably under
/// it. Clamped to a sane band; an unconstrained host (no cgroup) uses the ceiling
/// (ample RAM → negligible crash-loop risk).
///
/// The PRODUCTION threshold is this cgroup-derived value. `TURBA_WAL_MAX_BYTES`
/// (read at `open()`) is a tuning/test escape hatch, NOT a production setting.
/// The 0.8.12 continuous-checkpoint work removes the need for this force-flush
/// entirely (`wal-state-machine.md`).
fn wal_reclaim_threshold_from_limit(cgroup_limit: Option<u64>) -> u64 {
    const FLOOR: u64 = 16 * 1024 * 1024;
    const CEIL: u64 = 512 * 1024 * 1024;
    match cgroup_limit {
        Some(limit) => (limit / 4).clamp(FLOOR, CEIL),
        None => CEIL,
    }
}

/// Flush-only checkpoint of every tree, then PRUNE the WAL (deuda #10
/// intermediate — the pruner's size-triggered bound). Each `tree.checkpoint_flush`
/// pauses bg compaction like `major_compact` (no new concurrency) but only flushes
/// memtables + persists the manifest — O(new data), fast enough to keep pace with
/// a high-scope load, unlike a full `major_compact` (hundreds of L0 SSTables) which
/// fell behind and let the WAL grow. After every keyspace is manifest-durable the
/// prune drops the now-durable ARCHIVED segments only; it never touches the active
/// segment or a not-yet-durable tail, so a concurrent writer's WAL entries survive
/// a crash (unlike `rotate`). Bounds the WAL to ~one active segment.
fn checkpoint_flush_and_prune<'a>(
    trees: impl IntoIterator<Item = &'a Arc<Tree>>,
    journal: &Mutex<JournalWriter>,
) -> Result<()> {
    let trees: Vec<&Arc<Tree>> = trees.into_iter().collect();
    for tree in &trees {
        tree.checkpoint_flush()?;
    }
    let watermark = wal_prune_watermark(trees.iter().copied());
    journal.lock().prune(watermark)?;
    Ok(())
}

/// WAL-safe prune watermark across keyspaces. A keyspace that has flushed AND
/// manifest-persisted everything it ever received (`manifest_durable >=
/// current_seqno`) is "caught up" and contributes `u64::MAX` — it holds back no
/// WAL entry, so idle keyspaces (e.g. ghosts) never pin the watermark. Any other
/// keyspace contributes its manifest-durable seqno (NEVER `flushed_seqno` — the
/// BULKMODE trap, `wal-state-machine.md` §6). Safe by construction: a segment
/// containing a non-durable entry X is never pruned, because the keyspace that
/// received X has `durable < X <= current_seqno` and pins the watermark below X.
fn wal_prune_watermark<'a>(trees: impl Iterator<Item = &'a Arc<Tree>>) -> u64 {
    trees
        .map(|t| {
            let received = t.current_seqno();
            let durable = t.manifest_durable_seqno();
            if durable >= received {
                u64::MAX
            } else {
                durable
            }
        })
        .min()
        .unwrap_or(0)
}

/// Test-only override for [`TurbaEngine::recovered_from_wal`]. No-op in production
/// (nothing sets it); it exists so a test can arm the post-recovery confirmation
/// without having to manufacture a real unclean crash, and — the part that matters
/// — so the same test can turn it OFF as a NEGATIVE CONTROL and watch the
/// unprotected path let a duplicate through. A guard that cannot be shown to be
/// load-bearing is decoration.
pub static FORCE_RECOVERED_FROM_WAL: AtomicBool = AtomicBool::new(false);

impl TurbaEngine {
    /// Whether this process replayed WAL entries at open — i.e. the previous run
    /// did not shut down cleanly, so the recovery flush ran.
    ///
    /// Read by paths that must not tolerate a SILENT point-get miss (the
    /// duplicate-anchor check above all): inside this window they confirm a miss
    /// bloom-lessly, outside it they do not pay for the confirmation. See the
    /// field doc for why the window is the right gate.
    ///
    /// # Returns
    /// `true` when WAL entries were replayed at open, or when the test override
    /// [`FORCE_RECOVERED_FROM_WAL`] is set.
    pub fn recovered_from_wal(&self) -> bool {
        self.recovered_from_wal || FORCE_RECOVERED_FROM_WAL.load(Ordering::Relaxed)
    }
}

pub struct TurbaEngine {
    pub spatial: Arc<Tree>,
    pub identity: Arc<Tree>,
    pub dictionary: Arc<Tree>,
    pub ghosts: Arc<Tree>,
    /// 5th keyspace: per-record vector column, keyed by the record's spatial
    /// key. A first-class LSM keyspace identical in treatment to the others.
    pub vectors: Arc<Tree>,

    /// True when this process replayed WAL entries at open, i.e. the previous run
    /// did NOT shut down cleanly (a graceful shutdown rotates the journal, so a
    /// clean restart replays nothing).
    ///
    /// This is exactly the window in which the recovery flush ran and wrote the
    /// post-recovery SSTables that the "survivor key vanished" class implicates —
    /// a bloom-gated point-get can false-negative a key a scan still sees. The
    /// root of that defect is still open (see the internal analysis), so consumers
    /// use this flag to arm a cheap confirmation on the read paths where a silent
    /// miss would be a CORRECTNESS bug rather than a slow answer — most
    /// importantly the duplicate-anchor check, where a false negative turns an
    /// idempotent insert into a duplicate record. Outside this window the same
    /// confirmation would be pure cost, since its common case is a legitimate
    /// miss; gating on recovery makes it free in normal operation.
    recovered_from_wal: bool,
    journal: Arc<Mutex<JournalWriter>>,
    seqno: AtomicU64,
    /// Group commit: writers wait here for the sync thread to fsync.
    group_sync: Arc<GroupSync>,
    wal_shutdown: Arc<AtomicBool>,
    wal_handle: Mutex<Option<std::thread::JoinHandle<()>>>,
    sync_handle: Mutex<Option<std::thread::JoinHandle<()>>>,
    #[allow(dead_code)]
    path: PathBuf,
    /// Resolved WAL path: either `config.wal_path` if provided, or
    /// `path.join(JOURNAL_FILE)`. Stored at open so subsequent operations
    /// (snapshot, restore, recovery) do not have to recompute it.
    wal_path: PathBuf,
    #[allow(dead_code)]
    cache: Arc<BlockCache>,
    #[allow(dead_code)]
    config: EngineConfig,
    /// Exclusive advisory lock on the data dir (`<path>/LOCK`), held for the
    /// engine's lifetime so a second opener of the same directory (e.g. a stray
    /// second `xyzdb-mcp --embed` on the same dir) fails fast instead of two
    /// writers corrupting one LSM (C7). Released on drop or on
    /// process death (flock semantics — no stale lock after a crash).
    ///
    /// Wrapped in a `Mutex` only so a crash-simulating test (which leaks the
    /// engine via `std::mem::forget`, never running `Drop`) can release JUST
    /// this fd through `&self` — mirroring the kernel reclaiming it on a real
    /// crash — without a clean shutdown. See `_test_release_dir_lock`.
    _dir_lock: std::sync::Mutex<Option<std::fs::File>>,
}

impl TurbaEngine {
    /// Acquire an exclusive, non-blocking advisory lock on `<path>/LOCK`.
    ///
    /// # Errors
    /// [`Error::Config`] if the lock is already held — another process has the
    /// data dir open. The single-writer guarantee (one LSM, one writer) would
    /// otherwise be violated and silently corrupt the data.
    #[cfg(unix)]
    fn acquire_dir_lock(path: &Path) -> Result<std::fs::File> {
        use std::os::unix::io::AsRawFd;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path.join("LOCK"))?;
        // SAFETY: `flock` is called on a valid fd owned by `file` (which
        // outlives the call); `LOCK_NB` makes it return immediately rather than
        // block. No memory is shared or aliased.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            return Err(crate::error::Error::Config(format!(
                "data dir {} is already open by another process (LSM single-writer lock held). \
                 Stop the other xyzdb/xyzdb-mcp instance, or use a different --path.",
                path.display()
            )));
        }
        Ok(file)
    }

    /// Non-unix fallback: no advisory lock (the deploy targets are unix).
    #[cfg(not(unix))]
    fn acquire_dir_lock(_path: &Path) -> Result<std::fs::File> {
        std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(_path.join("LOCK"))
            .map_err(Into::into)
    }
    /// Test-only: release the data-dir lock WITHOUT a clean shutdown, modelling
    /// the kernel reclaiming the fd on a real crash. Crash-simulation tests leak
    /// the engine via `std::mem::forget` (to skip `Drop`'s flush); that also
    /// leaks this fd, so the flock would stay held in-process and block the
    /// recovery reopen. Calling this first mirrors what a real SIGKILL does.
    pub fn _test_release_dir_lock(&self) {
        if let Ok(mut guard) = self._dir_lock.lock() {
            *guard = None;
        }
    }

    /// Test-only: current total WAL size in bytes (active + archived segments).
    /// Lets a crash test assert the WAL is BOUNDED after a prune — the durable
    /// prefix reclaimed — without asserting *which* pruner reclaimed it. The
    /// background `turba-wal-pruner` races an explicit `prune_wal()` under load,
    /// so `prune_wal()`'s own freed-byte count is not a stable observable; the
    /// resulting WAL size is.
    pub fn _test_wal_total_bytes(&self) -> u64 {
        self.journal.lock().total_bytes()
    }

    /// Test-only crash simulation: stop and join every background thread (WAL
    /// pruner, group-commit sync, each tree's flush/compact worker), then
    /// release the dir lock — WITHOUT the graceful `shutdown()` flush.
    ///
    /// This is what a real SIGKILL does that `std::mem::forget` alone does not:
    /// `forget` skips `Drop`, so the background threads would stay ALIVE and
    /// race the recovery reopen on the same (now-unlocked) directory. Stopping
    /// them here — while deliberately NOT sealing/flushing the active memtable —
    /// leaves an acked-but-unflushed tail only in the WAL, the exact crash state
    /// under test. Follow with `std::mem::forget(engine)` so `Drop` never flushes.
    pub fn _test_crash_stop(&self) {
        // Stop the WAL pruner + group-commit sync threads and join them, so no
        // ghost thread survives to prune/sync during the reopen.
        self.wal_shutdown.store(true, Ordering::Relaxed);
        if let Some(h) = self.wal_handle.lock().take() {
            let _ = h.join();
        }
        if let Some(h) = self.sync_handle.lock().take() {
            let _ = h.join();
        }
        // Stop each tree's bg flush/compact worker. Unlike `shutdown()`, this
        // does NOT `seal_active()`/`flush_sealed()` — the unflushed tail must
        // stay only in the WAL to model the crash.
        for tree in self.trees() {
            tree.shutdown_bg();
        }
        self._test_release_dir_lock();
    }

    /// Open the engine. `path` is the data directory; behaviour is the
    /// single-tier (fintech) layout — file layout, MANIFEST and WAL
    /// semantics are all driven from `path`.
    pub fn open(path: &Path, config: EngineConfig) -> Result<Self> {
        // Validation: surface configuration errors before any I/O.
        // The xyzdb-server CLI runs the same `validate` at startup, but
        // direct library callers (tests, downstream embedders) reach
        // open() without that gate.
        config
            .validate()
            .map_err(|e| crate::error::Error::Config(e.to_string()))?;

        Self::open_single_tier(path, config)
    }

    fn open_single_tier(path: &Path, config: EngineConfig) -> Result<Self> {
        std::fs::create_dir_all(path)?;

        // C7: take the exclusive data-dir lock before any LSM I/O, so a second
        // opener of the same dir fails here instead of corrupting the store.
        let dir_lock = Self::acquire_dir_lock(path)?;

        // v0.4 cp 4.2.1: pass lane-admission policy through to the cache
        // (default true; CLI `--block-cache-lane-admission disabled`
        // turns it off for A/B comparison).
        let cache = Arc::new(BlockCache::with_config(
            config.cache_size_bytes,
            config.block_cache_lane_admission,
        ));

        // I/O scheduler: shared by all trees. Profile-conditional
        // construction per cycle doc §6 D6 — `IoSchedulerMode::Ssd`
        // selects Passthrough (zero-overhead, default), `Hdd` selects
        // the lane-aware scheduler for observability (per-lane EWMA,
        // outstanding counters, SLO breach detection). v0.5 retired the
        // enforce ladder per DEC-V5-11; the Laned scheduler now provides
        // pure instrumentation without throttling.
        let scheduler = Arc::new(match config.io_scheduler {
            IoSchedulerMode::Ssd => crate::io::Scheduler::passthrough(),
            IoSchedulerMode::Hdd => crate::io::Scheduler::laned(),
        });

        // Build per-keyspace configs based on storage profile
        let spatial_config = tree_config(&config, KeyspaceKind::Spatial);
        let identity_config = tree_config(&config, KeyspaceKind::Identity);
        let dictionary_config = tree_config(&config, KeyspaceKind::Dictionary);
        let ghosts_config = tree_config(&config, KeyspaceKind::Ghosts);
        let vectors_config = tree_config(&config, KeyspaceKind::Vectors);

        // Open trees — production uses open_with_scheduler so all
        // trees share the same scheduler instance for cross-tree
        // coordination (cycle doc §6 D5).
        let spatial = Arc::new(Tree::open_with_scheduler(
            Arc::clone(&scheduler),
            &path.join("spatial"),
            spatial_config,
            Arc::clone(&cache),
        )?);
        let identity = Arc::new(Tree::open_with_scheduler(
            Arc::clone(&scheduler),
            &path.join("identity"),
            identity_config,
            Arc::clone(&cache),
        )?);
        let dictionary = Arc::new(Tree::open_with_scheduler(
            Arc::clone(&scheduler),
            &path.join("dictionary"),
            dictionary_config,
            Arc::clone(&cache),
        )?);
        let ghosts = Arc::new(Tree::open_with_scheduler(
            Arc::clone(&scheduler),
            &path.join("ghosts"),
            ghosts_config,
            Arc::clone(&cache),
        )?);
        let vectors = Arc::new(Tree::open_with_scheduler(
            Arc::clone(&scheduler),
            &path.join("vectors"),
            vectors_config,
            Arc::clone(&cache),
        )?);

        // Recover WAL: replay any batches from previous session. The WAL
        // path defaults to `<path>/journal.wal`; `--wal-path` overrides
        // to place the WAL on a separate device (v0.5.2 B.5).
        let journal_path = config
            .wal_path
            .clone()
            .unwrap_or_else(|| path.join(JOURNAL_FILE));
        if let Some(parent) = journal_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut max_seqno: u64 = 0;

        let recovered = recovery::recover_journal(&journal_path)?;
        for batch in &recovered {
            max_seqno = max_seqno.max(batch.seqno);
            for item in &batch.items {
                let tree = match item.keyspace_id {
                    KS_SPATIAL => &spatial,
                    KS_IDENTITY => &identity,
                    KS_DICTIONARY => &dictionary,
                    KS_GHOSTS => &ghosts,
                    KS_VECTORS => &vectors,
                    _ => continue,
                };
                tree.insert_with_seqno(&item.key, &item.value, batch.seqno, item.value_type);
            }
        }

        // Reconcile seqno: max of tree seqnos and recovered seqno
        let tree_max = [&spatial, &identity, &dictionary, &ghosts, &vectors]
            .iter()
            .map(|t| t.current_seqno())
            .max()
            .unwrap_or(0);
        let start_seqno = max_seqno.max(tree_max);

        // Whether this open replayed WAL entries — the exposure window for the
        // post-recovery bloom defect. Captured here because this is where the
        // condition already exists; see the `recovered_from_wal` field doc.
        let recovered_from_wal = !recovered.is_empty();

        // If we recovered data, flush to SSTables and start a fresh journal
        if recovered_from_wal {
            for tree in [&spatial, &identity, &dictionary, &ghosts, &vectors] {
                tree.seal_active();
                tree.flush_sealed()?;
            }
        }

        // Open fresh journal (truncate old one after recovery)
        let journal = Arc::new(Mutex::new(JournalWriter::create(
            &journal_path,
            config.persist_mode,
            Arc::clone(&scheduler),
            config.wal_segment_max_bytes,
        )?));

        // Start background workers for each tree
        for tree in [&spatial, &identity, &dictionary, &ghosts, &vectors] {
            tree.start_bg_worker();
        }

        // Group commit sync state
        let group_sync = Arc::new(GroupSync {
            synced_epoch: AtomicU64::new(0),
            pending_epoch: AtomicU64::new(0),
            notify: std::sync::Condvar::new(),
            lock: std::sync::Mutex::new(()),
            #[cfg(feature = "durability-test-hooks")]
            paused: AtomicBool::new(false),
            last_successful_sync_ts_ms: AtomicU64::new(0),
            heartbeat_count: AtomicU64::new(0),
            poisoned: AtomicBool::new(false),
            #[cfg(feature = "durability-test-hooks")]
            force_sync_error: AtomicBool::new(false),
        });

        // Start group sync thread (fsyncs WAL every 1ms when writers are waiting)
        let use_group_commit = config.persist_mode == PersistMode::SyncData;
        let wal_shutdown = Arc::new(AtomicBool::new(false));
        let sync_handle = if use_group_commit {
            let journal_ref = Arc::clone(&journal);
            let gs = Arc::clone(&group_sync);
            let shutdown = Arc::clone(&wal_shutdown);
            Some(
                std::thread::Builder::new()
                    .name("turba-wal-sync".into())
                    .spawn(move || {
                        while !shutdown.load(Ordering::Relaxed) {
                            std::thread::sleep(std::time::Duration::from_millis(1));
                            // Heartbeat BEFORE the pause check: even when
                            // the test hook pauses fsyncs, the thread is
                            // still alive and should prove it. Operators
                            // reading a flat heartbeat infer a dead
                            // thread regardless of pause state.
                            gs.heartbeat_count.fetch_add(1, Ordering::Relaxed);
                            #[cfg(feature = "durability-test-hooks")]
                            if gs.paused.load(Ordering::Relaxed) {
                                continue;
                            }
                            // Once poisoned, never attempt fsync again — a retry could
                            // false-succeed (fsyncgate). Keep heartbeating so liveness
                            // signals stay truthful; writers fail fast on their own.
                            if gs.poisoned.load(Ordering::Acquire) {
                                continue;
                            }
                            let pending = gs.pending_epoch.load(Ordering::Acquire);
                            let synced = gs.synced_epoch.load(Ordering::Acquire);
                            if pending > synced {
                                // Only advance synced_epoch if try_lock + sync
                                // actually succeeded. Advancing on a failed
                                // sync (try_lock contention, fsync Err) would
                                // wake writers on unsynced data. See Finding 9
                                // secondary fix.
                                if let Some(mut j) = journal_ref.try_lock() {
                                    #[cfg(feature = "durability-test-hooks")]
                                    if gs.force_sync_error.load(Ordering::Relaxed) {
                                        // Inject an fsync failure: drive the poison
                                        // path exactly as a real EIO would — prove we
                                        // never false-ack and never retry.
                                        gs.poisoned.store(true, Ordering::Release);
                                        {
                                            let _g = gs.lock.lock().unwrap();
                                            gs.notify.notify_all();
                                        }
                                        continue;
                                    }
                                    match j.sync() {
                                        Ok(()) => {
                                            gs.synced_epoch
                                                .store(pending, Ordering::Release);
                                            let now_ms = std::time::SystemTime::now()
                                                .duration_since(std::time::UNIX_EPOCH)
                                                .map(|d| d.as_millis() as u64)
                                                .unwrap_or(0);
                                            gs.last_successful_sync_ts_ms
                                                .store(now_ms, Ordering::Relaxed);
                                            gs.notify.notify_all();
                                        }
                                        Err(e) => {
                                            // fsyncgate: a failed fsync clears the
                                            // dirty-page error on Linux, so a retry can
                                            // return Ok without the bytes ever reaching
                                            // disk — false-acking a lost write. Poison
                                            // the WAL and NEVER retry. Acquire the lock
                                            // across notify so no waiter is lost between
                                            // its poison-check and its wait().
                                            gs.poisoned.store(true, Ordering::Release);
                                            {
                                                let _g = gs.lock.lock().unwrap();
                                                gs.notify.notify_all();
                                            }
                                            eprintln!(
                                                "turba-wal-sync: WAL fsync FAILED — WAL poisoned, writes rejected (not retried): {e}"
                                            );
                                        }
                                    }
                                }
                                // If try_lock failed, do nothing —
                                // next iteration retries.
                            }
                        }
                        // Final sync on shutdown — CHECKED: a failed final sync
                        // means buffered writes may be lost, so poison rather than
                        // let the next open trust a tail that never reached disk.
                        if let Some(mut j) = journal_ref.try_lock() {
                            if let Err(e) = j.sync() {
                                gs.poisoned.store(true, Ordering::Release);
                                eprintln!("turba-wal-sync: final shutdown sync FAILED: {e}");
                            }
                        }
                    })
                    .expect("failed to spawn WAL sync thread"),
            )
        } else {
            None
        };

        // WAL janitor thread: disabled in production (Finding 10).
        // The janitor called rotate() under a weaker precondition
        // (flushed_seqno advanced) than rotate() requires (all data
        // in SSTables). Since rotate() truncates the entire WAL,
        // any writes still in active memtables with seqno greater
        // than flushed_seqno were silently lost on subsequent crash.
        //
        // The WAL background thread differs by build, but BOTH builds spawn
        // one — production does NOT skip the spawn. Under the
        // durability-test-hooks feature it is the OLD janitor
        // (rotate-on-`flushed_seqno`), kept alive only for the Finding 10
        // regression test, which reproduces the pre-fix scenario end-to-end.
        // WITHOUT the feature — i.e. production — it is the safe successor
        // pruner spawned in the `cfg(not(...))` arm below, which deletes
        // archived, manifest-durable WAL segments and bounds the WAL
        // losslessly. WAL growth is additionally bounded by explicit COMPACT
        // (`Engine::major_compact` + `execute_compact`) and by graceful
        // shutdown (`Drop` for `TurbaEngine` seals active then flushes).
        let wal_handle: Option<std::thread::JoinHandle<()>> = {
            #[cfg(feature = "durability-test-hooks")]
            {
                let trees = [
                    Arc::clone(&spatial),
                    Arc::clone(&identity),
                    Arc::clone(&dictionary),
                    Arc::clone(&ghosts),
                    Arc::clone(&vectors),
                ];
                let journal_ref = Arc::clone(&journal);
                let shutdown = Arc::clone(&wal_shutdown);
                Some(
                    std::thread::Builder::new()
                        .name("turba-wal-janitor".into())
                        .spawn(move || {
                            let mut last_rotated: u64 = start_seqno;
                            while !shutdown.load(Ordering::Relaxed) {
                                std::thread::sleep(std::time::Duration::from_millis(500));
                                if shutdown.load(Ordering::Relaxed) {
                                    break;
                                }

                                // min(flushed_seqno) across all keyspaces
                                let min_flushed =
                                    trees.iter().map(|t| t.flushed_seqno()).min().unwrap_or(0);

                                if min_flushed > last_rotated {
                                    if let Some(mut j) = journal_ref.try_lock() {
                                        if j.rotate().is_ok() {
                                            last_rotated = min_flushed;
                                        }
                                    }
                                }
                            }
                        })
                        .expect("failed to spawn WAL janitor"),
                )
            }
            #[cfg(not(feature = "durability-test-hooks"))]
            {
                // Production WAL pruner (the safe successor to the Finding-10
                // janitor). Unlike the janitor — which called rotate() and
                // truncated the ENTIRE WAL on the `flushed_seqno` watermark,
                // losing active-memtable writes on crash — this thread only
                // DELETES archived WAL segments whose every entry is ≤
                // `min(manifest_durable_seqno)` across all trees. Those entries
                // are persisted in SSTables recorded by a durable manifest, so
                // dropping them is lossless on crash, and the active segment +
                // any not-yet-durable tail are never touched. This bounds the
                // WAL automatically and transparently — no operator COMPACT.
                let trees = [
                    Arc::clone(&spatial),
                    Arc::clone(&identity),
                    Arc::clone(&dictionary),
                    Arc::clone(&ghosts),
                    Arc::clone(&vectors),
                ];
                let journal_ref = Arc::clone(&journal);
                let shutdown = Arc::clone(&wal_shutdown);
                // WAL size that triggers a forced checkpoint (deuda #10 intermediate):
                // derived from the cgroup memory limit so a hard-crash replay fits the
                // envelope; overridable via TURBA_WAL_MAX_BYTES for tuning/tests.
                let wal_max = std::env::var("TURBA_WAL_MAX_BYTES")
                    .ok()
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or_else(|| {
                        wal_reclaim_threshold_from_limit(
                            crate::host_probes::probe_cgroup_memory_limit_bytes(),
                        )
                    });
                Some(
                    std::thread::Builder::new()
                        .name("turba-wal-pruner".into())
                        .spawn(move || {
                            while !shutdown.load(Ordering::Relaxed) {
                                std::thread::sleep(std::time::Duration::from_millis(1000));
                                if shutdown.load(Ordering::Relaxed) {
                                    break;
                                }
                                // Bound the WAL so a HARD crash cannot replay more than
                                // the envelope allows. The plain prune below only deletes
                                // manifest-durable archived segments, so a keyspace whose
                                // memtable never fills pins the watermark and the WAL grows
                                // unbounded (deuda #10). When it exceeds wal_max — and we
                                // are not in BULKMODE (where the WAL is not written, so it
                                // never grows) — force a FLUSH-ONLY checkpoint (flush every
                                // tree, persist manifests, then prune). A full major_compact
                                // was too slow under a high-scope load (hundreds of L0
                                // SSTables) to keep pace; flush-only is O(new data). The
                                // checkpoint prunes (never rotates), so a concurrent
                                // writer's not-yet-durable tail is never truncated.
                                let wal_bytes = journal_ref.lock().total_bytes();
                                let all_compacting = trees.iter().all(|t| t.compaction_enabled());
                                if wal_bytes > wal_max && all_compacting {
                                    let _ = checkpoint_flush_and_prune(trees.iter(), &journal_ref);
                                } else {
                                    let watermark = wal_prune_watermark(trees.iter());
                                    if let Some(mut j) = journal_ref.try_lock() {
                                        let _ = j.prune(watermark);
                                    }
                                }
                            }
                        })
                        .expect("failed to spawn WAL pruner"),
                )
            }
        };

        Ok(Self {
            spatial,
            identity,
            dictionary,
            ghosts,
            vectors,
            recovered_from_wal,
            journal,
            seqno: AtomicU64::new(start_seqno),
            group_sync,
            wal_shutdown,
            wal_handle: Mutex::new(wal_handle),
            sync_handle: Mutex::new(sync_handle),
            path: path.to_path_buf(),
            wal_path: journal_path,
            cache,
            config,
            _dir_lock: std::sync::Mutex::new(Some(dir_lock)),
        })
    }

    /// Resolved WAL path for this engine instance. Defaults to
    /// `<path>/journal.wal` or honours `EngineConfig::wal_path` when set
    /// (xyzdb-server `--wal-path` flag, v0.5.2 B.5).
    pub fn wal_path(&self) -> &Path {
        &self.wal_path
    }

    /// Total `turba-compact: error:` lines printed across all trees
    /// since boot. Zero under healthy load; nonzero indicates races in the
    /// flush/compact path, IO failures, or corrupted on-disk state.
    /// Primarily an observability hook for operators and regression tests
    /// for the atomic SSTable meta publish (v0.2.1 Finding 4).
    /// v0.6.1 D5 §4.7 — true iff any of the engine's trees is
    /// currently executing a compaction pass. Used by the heat
    /// allocator worker's compaction interlock: the pass skips
    /// emit when this returns true. Aggregates across the
    /// keyspaces (spatial, identity, dictionary, ghosts, vectors) — any
    /// in-flight compaction in any keyspace blocks heat moves.
    pub fn is_any_compaction_in_progress(&self) -> bool {
        self.trees().iter().any(|t| t.is_compacting())
    }

    pub fn total_compact_errors(&self) -> u64 {
        self.spatial.compact_error_count()
            + self.identity.compact_error_count()
            + self.dictionary.compact_error_count()
            + self.ghosts.compact_error_count()
    }

    /// Reference to the shared block cache. Used by diagnostic logs and
    /// the upcoming `/stats` endpoint to read weighted size, hits, misses,
    /// and len without needing to expose the field publicly.
    pub fn cache_ref(&self) -> &BlockCache {
        &self.cache
    }

    /// Create a new write batch.
    pub fn batch(&self) -> WriteBatch<'_> {
        WriteBatch {
            engine: self,
            items: Vec::new(),
        }
    }

    /// Flush all keyspaces and compact.
    ///
    /// # Durability
    ///
    /// - **Precondition**: no concurrent writer is mid-batch on a keyspace
    ///   in a state that has not yet been enrolled in the group-commit
    ///   barrier. In practice callers either hold an exclusive handle or
    ///   accept that any still-unacked batch racing `major_compact` may or
    ///   may not survive the WAL rotate.
    /// - **Postcondition**: every acknowledged write is materialised in an
    ///   SSTable, and the WAL is truncated to zero bytes. This is the
    ///   canonical caller that establishes invariant D1 for
    ///   `JournalWriter::rotate` — `seal_active()` plus the internal
    ///   `flush_sealed()` inside `tree.major_compact()` are what make the
    ///   subsequent `journal.rotate()` safe: they guarantee every acknowledged
    ///   write is already in an SSTable before the WAL is truncated, so a
    ///   crash between rotate and the next flush cannot lose an acked write.
    pub fn major_compact(&self) -> Result<()> {
        // Seal + full-compact every tree (each pauses its own bg passes and
        // flushes + manifest-persists), then rotate the WAL. The rotate goes
        // through the GUARDED `rotate_journal` (the same D1 precondition the
        // COMPACT verb uses): it refuses to truncate if any WAL-backed keyspace
        // still holds acked, unflushed data. The seal+major above already make
        // every acked write durable, so this is belt-and-suspenders — but it
        // anchors EVERY production-intended WAL truncation to the physical prune
        // watermark, so a future flush regression can never silently truncate
        // here (the unguarded `journal.rotate()` this replaced trusted the flush
        // blindly). Defense in depth for the compact-skips-vectors class.
        for tree in self.trees() {
            tree.seal_active();
            tree.major_compact()?;
        }
        self.rotate_journal()?;
        for tree in self.trees() {
            tree.set_compaction_enabled(true);
        }
        Ok(())
    }

    /// Create a hot snapshot of the database under
    /// `<data_dir>/snapshots/<name>/`. v0.4 cp 3.2.1.
    ///
    /// # Durability + writer-blocking contract
    ///
    /// - Acquires the journal mutex; new writers block here for the
    ///   duration of the snapshot lock window. The window includes
    ///   atomic compaction-disable flips, atomic memtable seals, one
    ///   `journal.sync()`, hard-linking SSTs (sub-ms each), copying
    ///   per-keyspace MANIFEST files, copying the WAL file, writing
    ///   `snapshot.meta`, and re-enabling compaction. The cycle plan
    ///   §3 Bloque 3 acceptance gate requires the lock window to stay
    ///   < 100 ms in normal mode. The window is recorded into
    ///   `snapshot.meta::lock_window_us` for observability.
    /// - **BULKMODE caveat**: if compaction was already disabled on
    ///   any tree at snapshot start (BULKMODE — `WriteBatch::commit`
    ///   skips the WAL write in that mode), this method forces
    ///   `flush_sealed()` on each affected tree before capture so
    ///   the snapshot is consistent. The lock window grows with the
    ///   sealed memtable size — operators should pause bulk loads
    ///   before snapshotting (documented in OPERATIONS.md §4).
    ///
    /// # Errors
    ///
    /// - [`Error::SnapshotExists`] if `snapshots/<name>/` already
    ///   exists. Pick a different name or delete the existing one.
    /// - [`Error::Io`] for filesystem failures (no space, permission).
    pub fn create_snapshot(&self, name: &str) -> Result<crate::snapshot::SnapshotMeta> {
        use crate::snapshot::{
            KeyspaceCapture, SNAPSHOT_WAL_FILE, SnapshotMeta, list_sst_files, snapshots_root,
            write_snapshot_meta,
        };
        use std::time::{Instant, SystemTime, UNIX_EPOCH};

        // Reject path-traversal in the name BEFORE any filesystem join (S3):
        // a crafted name (`../x`, `a/b`, `..`) must not escape `snapshots/`.
        crate::snapshot::validate_snapshot_name(name)?;

        // Refuse name collisions before acquiring the WAL lock so the
        // failure happens outside the writer-blocking window.
        let snap_dir = snapshots_root(&self.path).join(name);
        if snap_dir.exists() {
            return Err(crate::error::Error::SnapshotExists(name.to_string()));
        }
        std::fs::create_dir_all(&snap_dir)?;

        // Capture pre-state: was BULKMODE active on any tree? Read BEFORE we
        // disable compaction below, otherwise the check can't distinguish
        // bulk-disabled compaction from our own.
        let bulkmode_at_capture = self.trees().iter().any(|t| !t.compaction_enabled());

        // Stop new compactions and DRAIN any in-flight pass — BEFORE taking the
        // WAL lock, so the drain does NOT block writers. Setting the flag alone
        // only stops NEW background passes (H12); a pass already past the
        // `compaction_enabled` gate would still apply its version swap and
        // `delete_compacted_inputs`, unlinking an SSTable mid-hard-link (→ ENOENT)
        // or skewing the copied MANIFEST against the linked SST set. Acquiring
        // each tree's compaction lock drains the in-flight pass and blocks new
        // ones; the guards are held across the capture loop below. Draining HERE
        // (outside the WAL-lock window) keeps the writer-blocking window short
        // even when a major compaction is in flight — inside the window a long
        // drain stalls every writer for its full duration (soak: a 40 s stall on
        // a snapshot that landed during a post-bulk major compaction).
        for tree in self.trees() {
            tree.set_compaction_enabled(false);
        }
        let compaction_guards: Vec<_> = self.trees().iter().map(|t| t.lock_compaction()).collect();

        // ── Acquire the WAL lock; writers contending now block. ──
        // The SST set is already frozen (compaction drained + disabled above),
        // so this window now covers only seal + fsync + hard-link (milliseconds).
        let mut journal = self.journal.lock();
        let lock_start = Instant::now();

        // Seal active memtables. Atomic version swap — microseconds.
        // Subsequent writes go to a new active memtable; their WAL
        // entries land *after* the captured offset and are not part
        // of this snapshot.
        for tree in self.trees() {
            tree.seal_active();
        }

        // BULKMODE consistency: in BULKMODE the WAL was skipped on
        // writes, so sealed memtables hold data not present in either
        // the WAL or any SST. Force flush_sealed for those trees.
        // The lock window grows with sealed memtable size — operators
        // are warned in OPERATIONS.md to pause bulk loads first.
        if bulkmode_at_capture {
            for tree in self.trees() {
                tree.flush_sealed()?;
            }
        }

        // Force a WAL fsync. Without this, sealed-but-unflushed bytes
        // could be in the OS page cache only — a subsequent power-cut
        // restore from the snapshot dir would lose them.
        journal.sync()?;

        // Capture per-keyspace SST inventory + copy MANIFEST.
        let mut keyspaces_capture: Vec<KeyspaceCapture> = Vec::with_capacity(5);
        for tree in self.trees() {
            let tree_dir = tree.dir();
            let keyspace = tree_dir
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .to_string();

            let dst_ks_dir = snap_dir.join(&keyspace);
            std::fs::create_dir_all(&dst_ks_dir)?;

            // Hard-link every live SST into the snapshot dir. POSIX
            // link counting keeps the inode alive even if compaction
            // unlinks the source after the snapshot lock releases.
            let live_paths = tree.live_table_paths();
            let mut sst_filenames: Vec<String> = Vec::with_capacity(live_paths.len());
            for src in &live_paths {
                let fname = src
                    .file_name()
                    .and_then(|s| s.to_str())
                    .ok_or_else(|| {
                        crate::error::Error::Corruption(format!(
                            "non-utf8 SST path: {}",
                            src.display()
                        ))
                    })?
                    .to_string();
                std::fs::hard_link(src, dst_ks_dir.join(&fname))?;
                sst_filenames.push(fname);
            }
            sst_filenames.sort();

            // Copy MANIFEST. (Copy not hard-link: engine.open() on
            // restore writes to MANIFEST during recovery and that
            // would propagate back into the snapshot if hard-linked.)
            let src_manifest = tree_dir.join("MANIFEST");
            if src_manifest.exists() {
                std::fs::copy(&src_manifest, dst_ks_dir.join("MANIFEST"))?;
            }
            // Sanity: nothing missing from the directory walk.
            let _ = list_sst_files(tree_dir)?; // fail-fast if dir unreadable

            keyspaces_capture.push(KeyspaceCapture {
                keyspace,
                sst_filenames,
            });
        }

        // Capture done: every live SST is hard-linked and every MANIFEST
        // copied. Release the compaction locks so background passes can
        // resume while the cold-path WAL copy + meta write proceed.
        drop(compaction_guards);

        // Copy the WAL. Same reasoning as MANIFEST: must be a copy,
        // not a hard-link, because subsequent engine activity grows
        // the live WAL and would corrupt the snapshot's view.
        let src_wal = self.wal_path.clone();
        let dst_wal = snap_dir.join(SNAPSHOT_WAL_FILE);
        let wal_bytes = if src_wal.exists() {
            std::fs::copy(&src_wal, &dst_wal)?
        } else {
            0
        };

        // Re-enable compaction. Done before writing the meta sidecar
        // so the writer-blocking window closes as early as possible
        // (the meta write is on the cold path).
        for tree in self.trees() {
            tree.set_compaction_enabled(true);
        }

        let lock_window_us = lock_start.elapsed().as_micros() as u64;

        // Drop the journal guard *before* the meta write (cold path
        // outside the writer-blocking gate).
        drop(journal);

        let meta = SnapshotMeta {
            name: name.to_string(),
            created_at_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            manifest_version: 3,
            keyspaces: keyspaces_capture,
            wal_bytes,
            bulkmode_at_capture,
            lock_window_us,
        };
        write_snapshot_meta(&snap_dir, &meta)?;
        Ok(meta)
    }

    /// Disable/enable auto-compaction on all keyspaces.
    /// Disable during bulk load for maximum write throughput.
    /// Call major_compact() after re-enabling to compress everything.
    ///
    /// # Durability
    ///
    /// - **Precondition**: none.
    /// - **Postcondition**: bg compaction is on/off on every tree. This
    ///   function does NOT touch the WAL and does NOT flush memtables;
    ///   turning compaction back on does not make previously-buffered
    ///   writes durable on its own. BULKMODE (enabled == false) also
    ///   SKIPS the WAL write in `WriteBatch::commit` — a crash during
    ///   bulk load loses the in-progress load by design, and callers MUST
    ///   call `major_compact()` (or equivalent) after re-enabling to
    ///   re-establish durability.
    pub fn set_compaction_enabled(&self, enabled: bool) {
        for tree in self.trees() {
            tree.set_compaction_enabled(enabled);
        }
    }

    /// Wait until compaction pressure settles.
    pub fn wait_compaction_settle(&self) {
        for _ in 0..200 {
            let settled = self
                .trees()
                .iter()
                .all(|t| t.sealed_memtable_count() == 0 && t.l0_table_count() <= 8);
            if settled {
                return;
            }
            // Nudge bg workers
            for tree in self.trees() {
                tree.notify_bg();
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// Force sync the journal to disk.
    ///
    /// # Durability
    ///
    /// - **Precondition**: none.
    /// - **Postcondition**: every byte previously handed to
    ///   `write_batch_buffered` (or `write_batch`) is fsynced on disk.
    ///   This flushes data the group-commit thread has not yet picked up,
    ///   but it does NOT advance `synced_epoch` — writers blocked on the
    ///   group-commit condvar are released by the sync thread, not by
    ///   this call. Intended for graceful shutdown and for external
    ///   tooling that wants to force a barrier outside the hot path.
    pub fn persist(&self) -> Result<()> {
        let res = self.journal.lock().sync();
        if res.is_err() {
            // fsyncgate parity (5a/5b): a failed flush/fsync means the buffered
            // window is NOT durable. Poison the WAL — exactly as the group-commit
            // sync thread does on EIO (3a) — so every subsequent commit fails
            // fast instead of continuing to ack against an un-syncable journal.
            // This closes the gap where the Batched / periodic-persist fsync
            // path swallowed the error and kept acking (silent false durability).
            self.group_sync.poisoned.store(true, Ordering::Release);
            let _g = self.group_sync.lock.lock().unwrap();
            self.group_sync.notify.notify_all();
        }
        res
    }

    /// Unix timestamp (ms since epoch) of the last successful WAL
    /// `sync()` from the group-commit sync thread. 0 means "no sync
    /// has succeeded yet" — either group-commit is disabled
    /// (Batched/Async modes), the thread has not run, or the last
    /// cycles all failed. Operator health check: compare against
    /// wall-clock time. Flat value under write load implies writers
    /// are blocked on a broken durability path.
    pub fn sync_thread_last_successful_sync_ts_ms(&self) -> u64 {
        self.group_sync
            .last_successful_sync_ts_ms
            .load(Ordering::Relaxed)
    }

    /// Monotonic counter incremented at the top of every sync-thread
    /// iteration. Distinct from `last_successful_sync_ts_ms`:
    /// heartbeat advances while `last_successful_sync_ts_ms` stays
    /// flat means the thread is alive but every `j.sync()` is failing.
    /// Always 0 when group-commit is disabled (Batched/Async modes),
    /// since no sync thread is spawned.
    pub fn sync_thread_heartbeat_count(&self) -> u64 {
        self.group_sync.heartbeat_count.load(Ordering::Relaxed)
    }

    /// Test-only: pause or resume the group-commit sync thread. When paused,
    /// the sync thread does not advance `synced_epoch` even if writers are
    /// waiting. Used by the Finding 9 regression test to reproduce the
    /// crash-window scenario without subprocess infrastructure.
    #[cfg(feature = "durability-test-hooks")]
    pub fn _test_pause_sync(&self, paused: bool) {
        self.group_sync.paused.store(paused, Ordering::Release);
    }

    /// Test-only: force the WAL sync thread's next `sync()` to fail, to
    /// exercise the fsyncgate poison path. Once it fires the WAL is
    /// poisoned (writes rejected) until the engine is reopened.
    #[cfg(feature = "durability-test-hooks")]
    pub fn _test_force_sync_error(&self, on: bool) {
        self.group_sync
            .force_sync_error
            .store(on, Ordering::Release);
    }

    /// Test-only: read whether the WAL has been poisoned.
    #[cfg(feature = "durability-test-hooks")]
    pub fn _test_is_poisoned(&self) -> bool {
        self.group_sync.poisoned.load(Ordering::Acquire)
    }

    /// Test-only: read the current `synced_epoch` value. Paired with
    /// `_test_pause_sync` for invariant-level assertions in the Finding 9
    /// regression test.
    #[cfg(feature = "durability-test-hooks")]
    pub fn _test_synced_epoch(&self) -> u64 {
        self.group_sync.synced_epoch.load(Ordering::Acquire)
    }

    /// Truncate the WAL after all data has been flushed to SSTables.
    /// Call after major_compact() or when all trees are fully compacted.
    ///
    /// # Durability
    ///
    /// - **Precondition (invariant D1)**: every acknowledged write is
    ///   already in an SSTable. The caller is responsible for sealing
    ///   active memtables AND completing `flush_sealed()` on every tree
    ///   BEFORE invoking this. This function does NOT enforce the
    ///   precondition — it is a thin wrapper over `JournalWriter::rotate`.
    /// - **Postcondition**: the WAL file is zero bytes. Any future crash
    ///   recovery recovers solely from SSTables. Violating the
    ///   precondition silently loses acknowledged writes on crash.
    ///
    /// Prefer `major_compact()` for the full, safe path. This entry point
    ///  exists for callers that already establish the precondition by
    /// other means (e.g., the xyzdb-engine `execute_compact`).
    /// Truncate the WAL to reclaim disk after a full flush.
    ///
    /// # Precondition (verified)
    /// Every **WAL-backed** keyspace must be manifest-durable — `journal.rotate()`
    /// truncates the WAL UNCONDITIONALLY, so a WAL-backed keyspace still holding
    /// an acked but unflushed tail would lose it on the next crash. Checked here
    /// via the prune watermark over `spatial/identity/dictionary/vectors`
    /// (`== u64::MAX` iff all are caught up; idle keyspaces never pin it). If one
    /// lags, the rotate is REFUSED with [`crate::error::Error::WalRotatePrecondition`]
    /// rather than silently dropping its tail — the guard against the "flushed
    /// only a SUBSET of keyspaces before rotating" class (compact-skips-vectors).
    ///
    /// The `ghosts` keyspace is DELIBERATELY EXCLUDED: ghost writes go straight
    /// to its active memtable and bypass the WAL (`ghost.rs`), so truncating the
    /// WAL can never lose ghost data and its flush state must not gate the rotate.
    /// Ghost durability (a crash losing the unflushed ghost memtable → a stale
    /// index) is `GhostLobeManager`'s concern, orthogonal to the WAL. Correct
    /// callers (COMPACT, shutdown) seal + flush every WAL-backed tree first.
    pub fn rotate_journal(&self) -> Result<()> {
        let watermark = wal_prune_watermark(
            [
                &self.spatial,
                &self.identity,
                &self.dictionary,
                &self.vectors,
            ]
            .into_iter(),
        );
        if watermark != u64::MAX {
            return Err(crate::error::Error::WalRotatePrecondition(format!(
                "a WAL-backed keyspace has acked writes not yet in an SSTable (prune watermark \
                 {watermark}); seal + flush spatial/identity/dictionary/vectors before rotating the WAL"
            )));
        }
        self.journal.lock().rotate()
    }

    /// Prune archived WAL segments that are fully manifest-durable. This is the
    /// safe, automatic, lossless WAL bound (the background `turba-wal-pruner`
    /// runs it every ~1s). Unlike `rotate_journal`, it NEVER truncates the
    /// active segment or any segment holding a not-yet-durable entry, so it
    /// requires no quiescence and cannot lose acknowledged writes on crash.
    /// Returns bytes freed. Exposed for tests and explicit checkpoints.
    pub fn prune_wal(&self) -> Result<u64> {
        let watermark = wal_prune_watermark(
            [
                &self.spatial,
                &self.identity,
                &self.dictionary,
                &self.ghosts,
                &self.vectors,
            ]
            .into_iter(),
        );
        self.journal.lock().prune(watermark)
    }

    /// Graceful shutdown: stop bg workers, flush everything.
    ///
    /// # Durability
    ///
    /// - **Precondition**: callers have stopped issuing new writes.
    ///   Writes that are already enrolled in the group-commit barrier
    ///   will complete normally; writes issued after `shutdown()` begins
    ///   are racing the teardown and have undefined durability.
    /// - **Postcondition**: the WAL pruner and group-commit sync thread
    ///   are stopped, the journal is fsynced, every tree has sealed its
    ///   active memtable and drained its bg flush/compact workers. On
    ///   success, any recovery from this on-disk state is lossless for
    ///   every write that returned `Ok` from `commit`. When every keyspace
    ///   is manifest-durable (the compaction-enabled path), the WAL is then
    ///   truncated so the next `open()` recovers from SSTables alone rather
    ///   than replaying the full write history — see the WAL-reclaim step.
    pub fn shutdown(&self) -> Result<()> {
        // Stop WAL pruner + sync thread
        self.wal_shutdown.store(true, Ordering::Relaxed);
        if let Some(h) = self.wal_handle.lock().take() {
            let _ = h.join();
        }
        if let Some(h) = self.sync_handle.lock().take() {
            let _ = h.join();
        }

        // Sync journal
        self.journal.lock().sync()?;

        // Stop each tree's bg flush/compact workers so nothing races the
        // synchronous flush below.
        for tree in self.trees() {
            tree.shutdown_bg();
        }

        // Synchronously seal + flush every tree so all acked writes land in
        // SSTables. In the compaction-enabled path `flush_sealed` also persists
        // the manifest, advancing each keyspace's manifest-durable seqno to its
        // highest acked write.
        let mut all_manifest_durable = true;
        for tree in self.trees() {
            tree.seal_active();
            tree.flush_sealed()?;
            if !tree.compaction_enabled() {
                all_manifest_durable = false;
            }
        }

        // Reclaim the WAL once every keyspace is manifest-durable. Otherwise the
        // next open() replays the ENTIRE write history, which (a) leaves a
        // redundant full WAL copy on disk alongside the SSTables (~2x footprint
        // at rest) and (b) rebuilds the whole dataset in one memtable during
        // recovery — OOM-killing a restart at a tight memory envelope (deuda #10:
        // confirmed 100k @256M restart exit 137). rotate()'s precondition (every
        // acked write already in an SSTable) is met by the flush above. In
        // BULKMODE the WAL is skipped on write, so there is nothing to reclaim and
        // the replay path is left unchanged.
        if all_manifest_durable {
            self.journal.lock().rotate()?;
        }

        Ok(())
    }

    fn trees(&self) -> [&Arc<Tree>; 5] {
        [
            &self.spatial,
            &self.identity,
            &self.dictionary,
            &self.ghosts,
            &self.vectors,
        ]
    }

    fn tree_for_ks(&self, ks: u8) -> &Arc<Tree> {
        match ks {
            KS_SPATIAL => &self.spatial,
            KS_IDENTITY => &self.identity,
            KS_DICTIONARY => &self.dictionary,
            KS_GHOSTS => &self.ghosts,
            KS_VECTORS => &self.vectors,
            _ => panic!("invalid keyspace id: {ks}"),
        }
    }

    /// Check backpressure on a tree. Waits up to 50ms for flush/compaction
    /// to drain. Returns Err(Overloaded) if still blocked after timeout.
    /// Total in-memory memtable bytes (active + sealed) across every keyspace.
    /// A PUT touches several keyspaces, so the quantity ingest backpressure must
    /// bound is the SUM, not any single tree's. Five cheap size reads.
    pub fn global_memtable_bytes(&self) -> usize {
        [
            &self.spatial,
            &self.identity,
            &self.dictionary,
            &self.ghosts,
            &self.vectors,
        ]
        .into_iter()
        .map(|t| t.active_memtable_size() + t.sealed_memtable_bytes())
        .sum()
    }

    /// Kick every keyspace's background flush (used by the ingest stall).
    fn notify_all_bg(&self) {
        for t in [
            &self.spatial,
            &self.identity,
            &self.dictionary,
            &self.ghosts,
            &self.vectors,
        ] {
            t.notify_bg();
        }
    }

    fn check_backpressure(&self, tree: &Tree) -> crate::error::Result<()> {
        // Budget-governed GLOBAL ceiling. Stall the writer when the summed
        // active+sealed memtable bytes across all keyspaces reach the budget-
        // derived ceiling, until background flush drains below the low-water
        // mark. This is what lets a tight container bound its own ingest instead
        // of OOM-ing while building a large index. A holgado budget (>= ~755MiB)
        // derives the 264MiB cap, which the per-tree sealed-count guard below
        // reaches first -> this block never triggers and behaviour is unchanged.
        // STALL, not reject: only a flush that makes NO drain progress for
        // INGEST_STALL_STUCK escalates to Overloaded (disk-full etc.).
        const INGEST_STALL_POLL: std::time::Duration = std::time::Duration::from_millis(5);
        const INGEST_STALL_STUCK: std::time::Duration = std::time::Duration::from_secs(30);
        let ceiling =
            crate::memory_budget::memtable_ceiling_from_budget(self.config.memory_budget_bytes)
                as usize;
        if self.global_memtable_bytes() >= ceiling {
            let low_water = ceiling / 2;
            let mut last = self.global_memtable_bytes();
            let mut no_progress = std::time::Duration::ZERO;
            self.notify_all_bg();
            while self.global_memtable_bytes() > low_water {
                std::thread::sleep(INGEST_STALL_POLL);
                let now = self.global_memtable_bytes();
                if now < last {
                    no_progress = std::time::Duration::ZERO;
                } else {
                    no_progress += INGEST_STALL_POLL;
                    if no_progress >= INGEST_STALL_STUCK {
                        return Err(crate::error::Error::Overloaded);
                    }
                }
                last = now;
                self.notify_all_bg();
            }
        }

        let sealed_threshold = if tree.compaction_enabled() { 2 } else { 4 };

        // Wait up to 50ms (10 × 5ms) for sealed memtables to drain
        for _ in 0..10 {
            if tree.sealed_memtable_count() < sealed_threshold {
                break;
            }
            tree.notify_bg();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        if tree.sealed_memtable_count() >= sealed_threshold {
            return Err(crate::error::Error::Overloaded);
        }

        // Skip L0 check if compaction disabled (bulk load mode)
        if !tree.compaction_enabled() {
            return Ok(());
        }

        // Wait up to 50ms for L0 to drain
        for _ in 0..10 {
            if tree.l0_table_count() < 4 {
                return Ok(());
            }
            tree.notify_bg();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        if tree.l0_table_count() >= 4 {
            return Err(crate::error::Error::Overloaded);
        }
        Ok(())
    }

    /// Seal memtable if large enough, notify background worker.
    fn maybe_trigger_flush(&self, tree: &Arc<Tree>) {
        if tree.active_memtable_size() >= tree_memtable_limit(tree) {
            tree.seal_active();
            tree.notify_bg();
        }
    }
}

impl Drop for TurbaEngine {
    fn drop(&mut self) {
        // Stop WAL janitor
        self.wal_shutdown.store(true, Ordering::Relaxed);
        if let Some(h) = self.wal_handle.lock().take() {
            let _ = h.join();
        }

        // Best-effort: sync journal, seal all, stop bg workers (which
        // flush+compact).
        let _ = self.journal.lock().sync();
        for tree in [
            &self.spatial,
            &self.identity,
            &self.dictionary,
            &self.ghosts,
            &self.vectors,
        ] {
            tree.seal_active();
            tree.notify_bg();
        }
        for tree in [
            &self.spatial,
            &self.identity,
            &self.dictionary,
            &self.ghosts,
            &self.vectors,
        ] {
            tree.shutdown_bg();
        }
    }
}

fn tree_memtable_limit(tree: &Tree) -> usize {
    tree.max_memtable_size()
}

/// Atomic cross-keyspace write batch.
pub struct WriteBatch<'a> {
    engine: &'a TurbaEngine,
    items: Vec<BatchItem>,
}

impl<'a> WriteBatch<'a> {
    pub fn put_spatial(&mut self, key: &[u8], value: &[u8]) {
        self.items.push(BatchItem {
            keyspace_id: KS_SPATIAL,
            key: key.to_vec(),
            value: value.to_vec(),
            value_type: ValueType::Value,
        });
    }

    pub fn put_identity(&mut self, key: &[u8], value: &[u8]) {
        self.items.push(BatchItem {
            keyspace_id: KS_IDENTITY,
            key: key.to_vec(),
            value: value.to_vec(),
            value_type: ValueType::Value,
        });
    }

    pub fn put_dictionary(&mut self, key: &[u8], value: &[u8]) {
        self.items.push(BatchItem {
            keyspace_id: KS_DICTIONARY,
            key: key.to_vec(),
            value: value.to_vec(),
            value_type: ValueType::Value,
        });
    }

    pub fn put_ghosts(&mut self, key: &[u8], value: &[u8]) {
        self.items.push(BatchItem {
            keyspace_id: KS_GHOSTS,
            key: key.to_vec(),
            value: value.to_vec(),
            value_type: ValueType::Value,
        });
    }

    /// Stage a put into the `vectors` keyspace (5th keyspace: per-record
    /// vector column, keyed by the record's spatial key).
    pub fn put_vectors(&mut self, key: &[u8], value: &[u8]) {
        self.items.push(BatchItem {
            keyspace_id: KS_VECTORS,
            key: key.to_vec(),
            value: value.to_vec(),
            value_type: ValueType::Value,
        });
    }

    pub fn remove_spatial(&mut self, key: &[u8]) {
        self.items.push(BatchItem {
            keyspace_id: KS_SPATIAL,
            key: key.to_vec(),
            value: Vec::new(),
            value_type: ValueType::Tombstone,
        });
    }

    pub fn remove_identity(&mut self, key: &[u8]) {
        self.items.push(BatchItem {
            keyspace_id: KS_IDENTITY,
            key: key.to_vec(),
            value: Vec::new(),
            value_type: ValueType::Tombstone,
        });
    }

    pub fn remove_dictionary(&mut self, key: &[u8]) {
        self.items.push(BatchItem {
            keyspace_id: KS_DICTIONARY,
            key: key.to_vec(),
            value: Vec::new(),
            value_type: ValueType::Tombstone,
        });
    }

    /// Stage a tombstone for `key` in the `vectors` keyspace.
    pub fn remove_vectors(&mut self, key: &[u8]) {
        self.items.push(BatchItem {
            keyspace_id: KS_VECTORS,
            key: key.to_vec(),
            value: Vec::new(),
            value_type: ValueType::Tombstone,
        });
    }

    /// Commit the batch: WAL write → memtable inserts → backpressure check.
    /// In BULKMODE (compaction disabled), WAL is skipped — if the process crashes
    /// during bulk load, the partial load is lost and must be restarted.
    ///
    /// # Durability
    ///
    /// - **Precondition**: the engine is open and not mid-shutdown.
    /// - **Postcondition (Durable mode, compaction enabled)**: on `Ok`,
    ///   the batch is fsynced on disk. This is enforced by the
    ///   group-commit barrier: the writer enrolls its epoch after
    ///   `write_batch_buffered`, then blocks on the condvar until the
    ///   sync thread advances `synced_epoch` past its own epoch. The
    ///   `synced_epoch` advance happens ONLY after a successful
    ///   `j.sync()`, so condvar release implies the writer's bytes are
    ///   on disk. See Finding 9 for the prior `wait_timeout(5ms)` bug
    ///   that broke this invariant and the fix that replaced it with a
    ///   proper `while synced_epoch < epoch { wait }` loop.
    /// - **Postcondition (BULKMODE, compaction disabled)**: on `Ok`, the
    ///   batch is in the memtable but NOT in the WAL. A crash before the
    ///   next `major_compact()` loses the batch and any others issued
    ///   since bulk load started. This is intentional — callers opt into
    ///   this tradeoff by disabling compaction for ingest throughput.
    /// - Backpressure in step 4 is advisory and post-commit: data is
    ///   already committed before `check_backpressure` runs, so
    ///   `Err(Overloaded)` from it is swallowed.
    pub fn commit(self) -> Result<SeqNo> {
        if self.items.is_empty() {
            return Ok(0);
        }

        // fsyncgate guard: once the WAL is poisoned (a prior fsync failed),
        // refuse new writes rather than risk a false-durable ack.
        if self.engine.group_sync.poisoned.load(Ordering::Acquire) {
            return Err(crate::error::Error::Corruption(
                "WAL poisoned by a prior fsync failure; not durable — restart required".into(),
            ));
        }

        // 1. Assign seqno
        let seqno = self.engine.seqno.fetch_add(1, Ordering::AcqRel) + 1;

        // 2. Write to WAL (skip in BULKMODE — no reads, crash = restart load)
        if self.engine.spatial.compaction_enabled() {
            let mut journal = self.engine.journal.lock();
            if journal.persist_mode() == PersistMode::SyncData {
                // Group commit: write to buffer, release lock, block on
                // condvar until the sync thread advances synced_epoch past
                // this writer's epoch. Only then is our batch on disk.
                // See Finding 9 for the prior wait_timeout(5ms) bug.
                journal.write_batch_buffered(seqno, &self.items)?;
                let epoch = self
                    .engine
                    .group_sync
                    .pending_epoch
                    .fetch_add(1, Ordering::AcqRel)
                    + 1;
                drop(journal); // Release lock so other writers can append

                // Block until synced_epoch >= our epoch. The sync thread
                // only advances synced_epoch after a successful j.sync(),
                // so this condition implies our batch is on disk.
                let mut guard = self.engine.group_sync.lock.lock().unwrap();
                while self.engine.group_sync.synced_epoch.load(Ordering::Acquire) < epoch {
                    // A poison wakes us here: the fsync failed, our bytes are NOT
                    // durable. Return an error instead of a false-success ack.
                    if self.engine.group_sync.poisoned.load(Ordering::Acquire) {
                        drop(guard);
                        return Err(crate::error::Error::Corruption(
                            "WAL fsync failed before this write was durable; not acked".into(),
                        ));
                    }
                    guard = self.engine.group_sync.notify.wait(guard).unwrap();
                }
                drop(guard);
            } else {
                journal.write_batch(seqno, &self.items)?;
            }
        }

        // 3. Insert into memtables
        for item in &self.items {
            let tree = self.engine.tree_for_ks(item.keyspace_id);
            tree.insert_with_seqno(&item.key, &item.value, seqno, item.value_type);
        }

        // 4. Seal + notify bg if memtable is large, check backpressure.
        // In BULKMODE: seal + notify only, no backpressure stalls. Flush threads
        // drain sealed at their own pace. RAM headroom is ample (~6GB free).
        let bulkmode = !self.engine.spatial.compaction_enabled();
        // One slot per keyspace id (KS_SPATIAL..=KS_VECTORS); indexed by
        // `keyspace_id as usize`, so it must cover the 5th keyspace too.
        let mut affected_ks = [false; 5];
        for item in &self.items {
            affected_ks[item.keyspace_id as usize] = true;
        }
        for (ks_id, affected) in affected_ks.iter().enumerate() {
            if *affected {
                let tree = self.engine.tree_for_ks(ks_id as u8);
                self.engine.maybe_trigger_flush(tree);
                if !bulkmode {
                    // Backpressure after memtable insert: data is already committed.
                    // If overloaded, log warning but don't return error — the write succeeded.
                    let _ = self.engine.check_backpressure(tree);
                    // Overloaded is non-fatal here: data is already in memtable.
                }
            }
        }

        Ok(seqno)
    }
}

// --- Per-keyspace configuration ---

pub(crate) enum KeyspaceKind {
    Spatial,
    Identity,
    Dictionary,
    Ghosts,
    /// 5th keyspace: per-record vector column, keyed by the record's spatial
    /// key. Tuned identically to Dictionary (small-value point keyspace).
    Vectors,
}

pub(crate) fn tree_config(config: &EngineConfig, kind: KeyspaceKind) -> TreeConfig {
    // (block_size, memtable_size, final_compression, bloom_bpk, level_compressions)
    // level_compressions: [L0, L1, L2+]
    let (block_size, memtable_size, compression, bloom_bpk, level_comps) =
        match (&config.storage_profile, &kind) {
            (StorageProfile::Ssd, KeyspaceKind::Spatial) => (
                32 * 1024,
                32 << 20,
                CompressionType::Zstd(3),
                10.0,
                vec![
                    CompressionType::Lz4,
                    CompressionType::Lz4,
                    CompressionType::Zstd(3),
                ],
            ),
            (StorageProfile::Hdd, KeyspaceKind::Spatial) => (
                64 * 1024,
                32 << 20,
                CompressionType::Zstd(3),
                14.0,
                vec![
                    CompressionType::Lz4,
                    CompressionType::Lz4,
                    CompressionType::Zstd(3),
                ],
            ),
            (StorageProfile::Ssd, KeyspaceKind::Identity) => (
                4 * 1024,
                32 << 20,
                CompressionType::Lz4,
                10.0,
                vec![CompressionType::Lz4, CompressionType::Lz4],
            ),
            (StorageProfile::Hdd, KeyspaceKind::Identity) => (
                16 * 1024,
                16 << 20,
                CompressionType::Lz4,
                14.0,
                vec![CompressionType::Lz4, CompressionType::Lz4],
            ),
            (StorageProfile::Ssd, KeyspaceKind::Dictionary) => (
                4 * 1024,
                8 << 20,
                CompressionType::Lz4,
                10.0,
                vec![CompressionType::Lz4, CompressionType::Lz4],
            ),
            (StorageProfile::Hdd, KeyspaceKind::Dictionary) => (
                8 * 1024,
                8 << 20,
                CompressionType::Lz4,
                10.0,
                vec![CompressionType::Lz4, CompressionType::Lz4],
            ),
            // Vectors hold f32 embeddings (incompressible: LZ4 ratio ≈ 0.996×), so
            // LZ4 buys ~0 space (None is +0.4% disk) yet still runs its literal/match
            // copy loop on every warm-scan block decode (M1 flamegraph: ~22.6% CPU).
            // None skips the codec entirely; a plain block copy remains (only G5's
            // borrowed-slice removes that). Block size tuned as Dictionary (small-value).
            (StorageProfile::Ssd, KeyspaceKind::Vectors) => (
                4 * 1024,
                8 << 20,
                CompressionType::None,
                10.0,
                vec![CompressionType::None, CompressionType::None],
            ),
            (StorageProfile::Hdd, KeyspaceKind::Vectors) => (
                8 * 1024,
                8 << 20,
                CompressionType::None,
                10.0,
                vec![CompressionType::None, CompressionType::None],
            ),
            (StorageProfile::Ssd, KeyspaceKind::Ghosts) => (
                64 * 1024,
                8 << 20,
                CompressionType::Zstd(3),
                0.0,
                vec![CompressionType::Lz4, CompressionType::Zstd(3)],
            ),
            (StorageProfile::Hdd, KeyspaceKind::Ghosts) => (
                256 * 1024,
                8 << 20,
                CompressionType::Zstd(3),
                0.0,
                vec![CompressionType::Lz4, CompressionType::Zstd(3)],
            ),
        };

    // H2.3 §9.3 — per-storage-profile L0 batch size, with optional CLI
    // override from xyzdb-server's --l0-batch flag.
    let mut compaction = LeveledConfig::for_storage_profile(config.storage_profile);
    if let Some(n) = config.l0_batch_override {
        compaction.l0_compact_batch_size = n;
    }

    // Test/diagnostic hooks: shrink memtables so the at-scale code paths (deep
    // LSM levels after compaction, the multi-chunk ghost-build flush in
    // `GhostLobeManager::create`) are exercised on tiny datasets instead of
    // requiring hundreds of thousands of records. Both default (unset) to the
    // production size. `TURBA_TEST_MEMTABLE_BYTES` applies to EVERY keyspace
    // (forces deep spatial levels cheaply); `TURBA_GHOST_MEMTABLE_BYTES` applies
    // to the ghost keyspace only. The ghost-specific value wins for ghosts.
    fn env_bytes(var: &str) -> Option<usize> {
        std::env::var(var)
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|&n| n > 0)
    }
    // Budget-scale the tuned seal size DOWN for tight budgets so ingest self-
    // limits (downward-only: budgets >= ~755MiB keep today's size exactly). This
    // shrinks max_memtable_size, which also feeds tree_memtable_limit -> the flush
    // trigger. The explicit test hooks below still win (they force tiny memtables).
    let memtable_size =
        crate::memory_budget::scale_seal_size(memtable_size as u64, config.memory_budget_bytes)
            as usize;
    let memtable_size = env_bytes("TURBA_TEST_MEMTABLE_BYTES").unwrap_or(memtable_size);
    let memtable_size = if matches!(kind, KeyspaceKind::Ghosts) {
        env_bytes("TURBA_GHOST_MEMTABLE_BYTES").unwrap_or(memtable_size)
    } else {
        memtable_size
    };

    TreeConfig {
        sstable: SSTableConfig {
            data_block_size: block_size,
            compression,
            bloom_bits_per_key: bloom_bpk,
            ..Default::default()
        },
        max_memtable_size: memtable_size,
        compaction,
        level_compressions: Some(level_comps),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StorageProfile;

    #[test]
    fn tree_config_uses_storage_profile_l0_batch_when_no_override() {
        let mut cfg = EngineConfig {
            storage_profile: StorageProfile::Ssd,
            l0_batch_override: None,
            ..Default::default()
        };
        let tc = tree_config(&cfg, KeyspaceKind::Spatial);
        assert_eq!(tc.compaction.l0_compact_batch_size, 50, "SSD default");

        cfg.storage_profile = StorageProfile::Hdd;
        let tc = tree_config(&cfg, KeyspaceKind::Spatial);
        // HDD value is sweep-driven; the test only asserts it's sane and
        // tracks `for_storage_profile`.
        assert_eq!(
            tc.compaction.l0_compact_batch_size,
            crate::compaction::leveled::LeveledConfig::for_storage_profile(StorageProfile::Hdd)
                .l0_compact_batch_size,
            "HDD profile default flows through tree_config"
        );
    }

    #[test]
    fn tree_config_l0_batch_override_takes_precedence() {
        let mut cfg = EngineConfig {
            storage_profile: StorageProfile::Hdd,
            l0_batch_override: Some(15),
            ..Default::default()
        };
        let tc = tree_config(&cfg, KeyspaceKind::Spatial);
        assert_eq!(
            tc.compaction.l0_compact_batch_size, 15,
            "CLI/runtime override must take precedence over the storage-profile default"
        );

        // SSD profile + override also takes effect (advanced tuning is
        // profile-agnostic).
        cfg.storage_profile = StorageProfile::Ssd;
        cfg.l0_batch_override = Some(7);
        let tc = tree_config(&cfg, KeyspaceKind::Spatial);
        assert_eq!(tc.compaction.l0_compact_batch_size, 7);
    }
}
