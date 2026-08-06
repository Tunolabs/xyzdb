//! Block cache: concurrent LRU cache for decoded data blocks.
//!
//! Wraps quick_cache with weight-based eviction (block byte size).
//! Shared across all keyspaces.
//!
//! Per-tree counters (v0.3 cycle Day 0-1, Spike 0 ampliado): in addition to
//! the global hit/miss counts inherited from quick_cache, the cache tracks
//! per-tree (`tree_id`) hit/miss counts and accumulated time spent on cache
//! hits vs disk-bound misses. The accounting attributes each `get_or_load`
//! call to one of two paths and adds the wall-clock delta to the matching
//! counter. Surfaced via `KeyspaceStats.block_cache` in `STATS`. Used to
//! attribute query latency to (cache miss / disk service).

// SPDX-License-Identifier: BUSL-1.1
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Key for the block cache: (tree_id, table_id, block offset).
/// tree_id disambiguates tables across different keyspaces that share the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockHandle {
    pub tree_id: u64,
    pub table_id: u64,
    pub offset: u64,
}

/// A data block's raw ON-DISK (compressed) bytes, cached to avoid repeated disk
/// reads. `block::decode` is applied per access to validate checksums, decompress,
/// and parse into entries — so the cache holds the compressed form and pays decode
/// on read (a cache HIT still decodes once; a MISS decodes once in the loader).
pub struct DecodedBlock {
    pub data: Vec<u8>,
}

impl DecodedBlock {
    pub fn weight(&self) -> u64 {
        (self.data.len() + 64) as u64
    }
}

/// Result of [`BlockCache::get_or_load_returning`]. On a cache HIT the block was
/// already resident (the loader did not run); on a MISS the loader ran and its
/// freshly-derived value `R` is returned alongside the now-cached block — so a
/// caller that must derive `R` from the block (e.g. decode it into entries) does
/// that work exactly once on a miss instead of decoding a second time.
pub enum Loaded<R> {
    Hit(Arc<DecodedBlock>),
    Miss(Arc<DecodedBlock>, R),
}

#[derive(Clone)]
struct BlockWeighter;

impl quick_cache::Weighter<BlockHandle, Arc<DecodedBlock>> for BlockWeighter {
    fn weight(&self, _key: &BlockHandle, val: &Arc<DecodedBlock>) -> u64 {
        val.weight()
    }
}

type InnerCache = quick_cache::sync::Cache<BlockHandle, Arc<DecodedBlock>, BlockWeighter>;

/// Which per-SST metadata section a [`MetaHandle`] addresses. Pre-0.8 the engine
/// held every SST's zone maps / index / bloom resident for the SST's lifetime
/// (O(dataset) RAM). The metadata cache lets cold SSTs' sections evict so
/// resident RAM tracks the working set instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MetaKind {
    /// Per-block zone-map blob (scan pruning).
    ZoneMaps,
    /// Block index (key-range → block offset).
    Index,
    /// Bloom filter bits.
    Bloom,
}

/// Cache key for an evictable per-SST metadata section.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MetaHandle {
    pub tree_id: u64,
    pub table_id: u64,
    pub kind: MetaKind,
}

/// A cached metadata section. Parsed forms (not raw bytes) are stored so the
/// hot `get`/scan paths never re-parse per lookup. ZoneMaps stays raw — it is
/// decoded per scan anyway.
pub enum MetaSection {
    ZoneMaps(Vec<u8>),
    Bloom(crate::bloom::BloomFilter),
    Index(Vec<crate::table::reader::IndexEntry>),
}

impl MetaSection {
    fn weight(&self) -> u64 {
        let inner = match self {
            MetaSection::ZoneMaps(v) => v.len(),
            MetaSection::Bloom(b) => b.size_bytes(),
            // Mirror SSTableReader::index_bytes: dynamic key + ~48 B/entry
            // (last_seqno + offset + size + Vec header + enum discriminant).
            MetaSection::Index(entries) => entries.iter().map(|e| e.last_key.len() + 48).sum(),
        };
        (inner + 64) as u64
    }
}

#[derive(Clone)]
struct MetaWeighter;

impl quick_cache::Weighter<MetaHandle, Arc<MetaSection>> for MetaWeighter {
    fn weight(&self, _key: &MetaHandle, val: &Arc<MetaSection>) -> u64 {
        val.weight()
    }
}

type MetaInner = quick_cache::sync::Cache<MetaHandle, Arc<MetaSection>, MetaWeighter>;

/// Per-tree counters tracked atomically. Hot path uses only `Relaxed` ordering
/// because no cross-counter invariant is enforced; consumers read each field
/// independently in `STATS` snapshots.
#[derive(Default)]
struct PerTreeCounters {
    hits: AtomicU64,
    misses: AtomicU64,
    /// Microseconds accumulated on cache-miss paths (`get_or_load` ran the
    /// loader closure — i.e. disk read happened).
    disk_read_us_total: AtomicU64,
    /// Microseconds accumulated on cache-hit paths (no disk I/O).
    cache_read_us_total: AtomicU64,
    /// Per-bucket count of `pread` service-time samples (cache-miss closure
    /// elapsed_us). Buckets are HDD-physics-aligned and concentrate
    /// resolution on the diagnostic 3–20 ms zone. See [`pread_bucket`] for
    /// the boundaries. Added in v0.3.2 Spike B.
    pread_service_time_buckets: [AtomicU64; PREAD_HISTOGRAM_BUCKETS],
}

/// Number of buckets in the `pread` service-time histogram.
pub const PREAD_HISTOGRAM_BUCKETS: usize = 10;

/// Bucketize a `pread` service-time sample (microseconds) into the
/// 10-bucket histogram. Buckets are HDD-physics-aligned: page cache hits
/// (b0), SSD-like fast paths (b1), HDD seek bands (b2-b5), queueing tax
/// (b6-b7), pathological (b8-b9).
#[inline]
pub fn pread_bucket(elapsed_us: u64) -> usize {
    match elapsed_us {
        0..=999 => 0,           // < 1 ms — page cache hit
        1_000..=2_999 => 1,     // 1–3 ms — page cache slow / SSD-like
        3_000..=4_999 => 2,     // 3–5 ms — NCQ optimal / fast HDD
        5_000..=7_999 => 3,     // 5–8 ms — HDD inner-track seek
        8_000..=11_999 => 4,    // 8–12 ms — HDD typical seek
        12_000..=19_999 => 5,   // 12–20 ms — HDD worst-case seek
        20_000..=49_999 => 6,   // 20–50 ms — queueing tax onset
        50_000..=99_999 => 7,   // 50–100 ms — heavy queueing
        100_000..=299_999 => 8, // 100–300 ms — pathological
        _ => 9,                 // ≥ 300 ms — outlier
    }
}

/// Public, plain-old-data snapshot of a single tree's counters. Returned by
/// [`BlockCache::per_tree_snapshot`] and consumed by the STATS serializer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PerTreeBlockCacheSnapshot {
    pub hits: u64,
    pub misses: u64,
    pub disk_read_us_total: u64,
    pub cache_read_us_total: u64,
    /// Cumulative `pread` service-time histogram. Index = bucket (see
    /// [`pread_bucket`] for boundaries). Snapshotted from
    /// `PerTreeCounters.pread_service_time_buckets` at STATS query time.
    /// Spike B uses post-phase deltas to derive phase-local distributions.
    pub pread_service_time_buckets: [u64; PREAD_HISTOGRAM_BUCKETS],
}

pub struct BlockCache {
    inner: InnerCache,
    /// Evictable per-SST metadata sections (zone maps / index / bloom). Separate
    /// budget from `inner` so metadata (high reuse, large items) and data blocks
    /// don't entangle eviction. Lets resident metadata RAM track the working set
    /// instead of O(dataset). Always-admit (no lane policy — metadata is hot).
    meta: MetaInner,
    per_tree: RwLock<HashMap<u64, Arc<PerTreeCounters>>>,
    /// v0.4 cp 4.2.1: when true, the lane-aware admission policy is
    /// active — Compaction + Flush block-misses do NOT insert into
    /// the cache (they still benefit from cache hits if a previous
    /// UserIORead path warmed the same block). UserIORead and
    /// WriterDurable always admit. When false, every miss admits
    /// regardless of lane (legacy v0.3.x behaviour). Toggled via
    /// `--block-cache-lane-admission {enabled, disabled}` server flag.
    lane_admission_enabled: bool,
    /// Per-lane admission counters: `admitted[lane]` increments on a
    /// miss that proceeded to insert; `skipped[lane]` increments on a
    /// miss that the policy declined to admit. Indexed by
    /// [`crate::io::Lane::index`]. Surfaced in `/metrics` as
    /// `xyzdb_block_cache_admission_total{lane,outcome}`.
    admitted_total: [AtomicU64; crate::io::Lane::COUNT],
    skipped_total: [AtomicU64; crate::io::Lane::COUNT],
    /// G3 (0.9): observability for the sequential read-ahead hint. `ok`/`err`
    /// count successful/failed hint syscalls; `last_offset`/`last_len` record
    /// the byte range of the most recent successful hint. Behavioural surface
    /// for the deterministic G3 gate (the hint's latency/page-cache effect is
    /// pending-x86). Read-path only — no bearing on correctness.
    readahead_hint_ok: AtomicU64,
    readahead_hint_err: AtomicU64,
    readahead_last_offset: AtomicU64,
    readahead_last_len: AtomicU64,
}

impl BlockCache {
    /// Build a cache with the v0.3.x default admission policy
    /// (`lane_admission_enabled = true`). Legacy callers + tests use
    /// this; production code goes through [`BlockCache::with_config`].
    pub fn new(capacity_bytes: u64) -> Self {
        Self::with_config(capacity_bytes, true)
    }

    /// v0.4 cp 4.2.1: build a cache with explicit lane-admission
    /// configuration. `lane_admission_enabled = true` enables the
    /// Compaction/Flush admit-only-if-already-present policy.
    pub fn with_config(capacity_bytes: u64, lane_admission_enabled: bool) -> Self {
        // Metadata budget: a quarter of the data-block budget, floored at 64 MiB.
        // Sized internally for now; `--metadata-cache-size` is wired in a later
        // increment. Metadata sections are large (zone maps ~2 MB/SST) so the
        // estimated-items hint is lower than the data cache's.
        let meta_budget = (capacity_bytes / 4).max(64 * 1024 * 1024);
        Self {
            inner: InnerCache::with_weighter(10_000, capacity_bytes, BlockWeighter),
            meta: MetaInner::with_weighter(4_000, meta_budget, MetaWeighter),
            per_tree: RwLock::new(HashMap::new()),
            lane_admission_enabled,
            admitted_total: std::array::from_fn(|_| AtomicU64::new(0)),
            skipped_total: std::array::from_fn(|_| AtomicU64::new(0)),
            readahead_hint_ok: AtomicU64::new(0),
            readahead_hint_err: AtomicU64::new(0),
            readahead_last_offset: AtomicU64::new(0),
            readahead_last_len: AtomicU64::new(0),
        }
    }

    /// Record the outcome of a G3 sequential read-ahead hint. `ok = false`
    /// means the hint syscall returned an error (the scan still proceeds
    /// correctly; it just misses the read-ahead). On success the hinted byte
    /// range is stored for the behavioural gate.
    pub(crate) fn record_readahead_hint(&self, offset: u64, len: u64, ok: bool) {
        if ok {
            self.readahead_hint_ok.fetch_add(1, Ordering::Relaxed);
            self.readahead_last_offset.store(offset, Ordering::Relaxed);
            self.readahead_last_len.store(len, Ordering::Relaxed);
        } else {
            self.readahead_hint_err.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// `(ok, err)` counts of read-ahead hint syscalls since construction (G3).
    pub fn readahead_hint_stats(&self) -> (u64, u64) {
        (
            self.readahead_hint_ok.load(Ordering::Relaxed),
            self.readahead_hint_err.load(Ordering::Relaxed),
        )
    }

    /// `(offset, len)` of the most recent successful read-ahead hint (G3).
    pub fn last_readahead_range(&self) -> (u64, u64) {
        (
            self.readahead_last_offset.load(Ordering::Relaxed),
            self.readahead_last_len.load(Ordering::Relaxed),
        )
    }

    /// Admission predicate per the v0.4 policy. Returns `true` when a
    /// miss for `lane` may insert a fresh block; `false` when the
    /// policy declines (Compaction / Flush with admission enabled).
    #[inline]
    fn should_admit(&self, lane: crate::io::Lane) -> bool {
        if !self.lane_admission_enabled {
            return true;
        }
        match lane {
            crate::io::Lane::UserIORead | crate::io::Lane::WriterDurable => true,
            // Scan joins Flush/Compaction on the non-admitting side: a bulk
            // sweep larger than the cache (the only case routed here — see
            // `Lane::Scan`) benefits from hits but must not admit on miss, or
            // it self-evicts and evicts the hot working set. Scans that FIT
            // stay on UserIORead and admit as before (zero regression).
            crate::io::Lane::Flush | crate::io::Lane::Compaction { .. } | crate::io::Lane::Scan => {
                false
            }
        }
    }

    /// Snapshot per-lane admission counters: `[(admitted, skipped); 4]`
    /// indexed by lane discriminant. Cheap (8 atomic loads). Consumed
    /// by the `/metrics` Prometheus emitter.
    pub fn admission_snapshot(&self) -> [(u64, u64); crate::io::Lane::COUNT] {
        std::array::from_fn(|i| {
            (
                self.admitted_total[i].load(Ordering::Relaxed),
                self.skipped_total[i].load(Ordering::Relaxed),
            )
        })
    }

    /// True if the lane-aware admission policy is active.
    pub fn lane_admission_enabled(&self) -> bool {
        self.lane_admission_enabled
    }

    /// Fast path lookup or lazy insert of the per-tree counters.
    fn per_tree_entry(&self, tree_id: u64) -> Arc<PerTreeCounters> {
        if let Some(s) = self.per_tree.read().get(&tree_id).cloned() {
            return s;
        }
        let mut guard = self.per_tree.write();
        guard
            .entry(tree_id)
            .or_insert_with(|| Arc::new(PerTreeCounters::default()))
            .clone()
    }

    pub fn get(&self, handle: &BlockHandle) -> Option<Arc<DecodedBlock>> {
        self.inner.get(handle)
    }

    pub fn insert(&self, handle: BlockHandle, block: Arc<DecodedBlock>) {
        self.inner.insert(handle, block);
    }

    pub fn get_or_load<F>(
        &self,
        handle: BlockHandle,
        lane: crate::io::Lane,
        loader: F,
    ) -> crate::error::Result<Arc<DecodedBlock>>
    where
        F: FnOnce() -> crate::error::Result<DecodedBlock>,
    {
        // Delegate to the returning form with no side value; recover the Arc from
        // either outcome (identical bookkeeping, single source of truth).
        Ok(
            match self.get_or_load_returning(handle, lane, || loader().map(|b| (b, ())))? {
                Loaded::Hit(b) | Loaded::Miss(b, ()) => b,
            },
        )
    }

    /// Like [`Self::get_or_load`] but the loader returns `(DecodedBlock, R)`; on a
    /// MISS the caller-derived `R` (e.g. the block's decoded entries) is returned
    /// so it need not be re-derived, and on a HIT the resident block is returned.
    /// This is what lets `load_block` decode a block exactly once on a miss.
    pub fn get_or_load_returning<F, R>(
        &self,
        handle: BlockHandle,
        lane: crate::io::Lane,
        loader: F,
    ) -> crate::error::Result<Loaded<R>>
    where
        F: FnOnce() -> crate::error::Result<(DecodedBlock, R)>,
    {
        let counters = self.per_tree_entry(handle.tree_id);
        let start = std::time::Instant::now();
        if let Some(block) = self.inner.get(&handle) {
            counters.hits.fetch_add(1, Ordering::Relaxed);
            counters
                .cache_read_us_total
                .fetch_add(start.elapsed().as_micros() as u64, Ordering::Relaxed);
            return Ok(Loaded::Hit(block));
        }
        // Per-tree counters track read INTENT (including loader failures),
        // not successful completions. This aligns with quick_cache's global
        // counter semantics: `inner.get()` returning None already bumped
        // the global miss counter regardless of whether the loader
        // subsequently succeeds. The miss counter and disk_read_us_total
        // must reflect the same event, on both success AND failure paths,
        // or operator-facing per-tree sums drift below global. (Drift was
        // observed at 0.0013 % during a 162 s SSD smoke with concurrent
        // compaction unlinking files mid-load — small but real.)
        counters.misses.fetch_add(1, Ordering::Relaxed);
        let (block, side) = match loader() {
            Ok((b, r)) => (Arc::new(b), r),
            Err(e) => {
                // Capture time spent on the failed disk attempt.
                let elapsed_us = start.elapsed().as_micros() as u64;
                counters
                    .disk_read_us_total
                    .fetch_add(elapsed_us, Ordering::Relaxed);
                counters.pread_service_time_buckets[pread_bucket(elapsed_us)]
                    .fetch_add(1, Ordering::Relaxed);
                return Err(e);
            }
        };
        // v0.4 cp 4.2.1: lane-aware admission. UserIORead + WriterDurable
        // always admit; Flush + Compaction skip the insert when the policy
        // is enabled (they still benefit from cache hits warmed by other
        // lanes). The block is returned to the caller either way; only
        // the cache-population side effect differs.
        if self.should_admit(lane) {
            self.inner.insert(handle, Arc::clone(&block));
            self.admitted_total[lane.index()].fetch_add(1, Ordering::Relaxed);
        } else {
            self.skipped_total[lane.index()].fetch_add(1, Ordering::Relaxed);
        }
        let elapsed_us = start.elapsed().as_micros() as u64;
        counters
            .disk_read_us_total
            .fetch_add(elapsed_us, Ordering::Relaxed);
        counters.pread_service_time_buckets[pread_bucket(elapsed_us)]
            .fetch_add(1, Ordering::Relaxed);
        Ok(Loaded::Miss(block, side))
    }

    /// Fetch an evictable metadata section, loading + caching it on a miss.
    /// Metadata is immutable per SST, so a reload yields identical bytes — a
    /// miss costs only a re-read, never correctness. Always admits (no lane
    /// policy): a bloom/index/zone-map serves many reads, unlike a data block.
    pub fn meta_get_or_load<F>(
        &self,
        handle: MetaHandle,
        loader: F,
    ) -> crate::error::Result<Arc<MetaSection>>
    where
        F: FnOnce() -> crate::error::Result<MetaSection>,
    {
        if let Some(section) = self.meta.get(&handle) {
            return Ok(section);
        }
        let arc = Arc::new(loader()?);
        self.meta.insert(handle, Arc::clone(&arc));
        Ok(arc)
    }

    /// Current total weight of the metadata cache (bytes resident in cache).
    /// The resident metadata RAM the partitioning shrinks vs the old
    /// always-resident model.
    pub fn meta_current_weight(&self) -> u64 {
        self.meta.weight()
    }

    /// Current total weight of cached entries (bytes). Diagnostic: if this
    /// exceeds `capacity()` the weighter or cap enforcement is broken.
    pub fn current_weight(&self) -> u64 {
        self.inner.weight()
    }

    /// Configured maximum weight (bytes).
    pub fn capacity(&self) -> u64 {
        self.inner.capacity()
    }

    /// Total number of entries currently resident.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// True if no entries are resident.
    pub fn is_empty(&self) -> bool {
        self.inner.len() == 0
    }

    /// Monotonic lifetime cache hits (global, all trees).
    pub fn hits(&self) -> u64 {
        self.inner.hits()
    }

    /// Monotonic lifetime cache misses (global, all trees).
    pub fn misses(&self) -> u64 {
        self.inner.misses()
    }

    /// Snapshot the per-tree counters for `tree_id`. Returns the zero
    /// snapshot if the tree has not yet performed any `get_or_load`. Cheap
    /// (one read lock + N atomic loads); safe to call from STATS path.
    pub fn per_tree_snapshot(&self, tree_id: u64) -> PerTreeBlockCacheSnapshot {
        let guard = self.per_tree.read();
        let Some(c) = guard.get(&tree_id) else {
            return PerTreeBlockCacheSnapshot::default();
        };
        let mut buckets = [0u64; PREAD_HISTOGRAM_BUCKETS];
        for (i, slot) in buckets.iter_mut().enumerate() {
            *slot = c.pread_service_time_buckets[i].load(Ordering::Relaxed);
        }
        PerTreeBlockCacheSnapshot {
            hits: c.hits.load(Ordering::Relaxed),
            misses: c.misses.load(Ordering::Relaxed),
            disk_read_us_total: c.disk_read_us_total.load(Ordering::Relaxed),
            cache_read_us_total: c.cache_read_us_total.load(Ordering::Relaxed),
            pread_service_time_buckets: buckets,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_block(byte: u8, len: usize) -> DecodedBlock {
        DecodedBlock {
            data: vec![byte; len],
        }
    }

    /// Loader that simulates measurable disk-read latency. `Instant::elapsed`
    /// resolves to ≥ 1 µs, so a brief sleep guarantees the disk_read_us
    /// counter increments observably; production loaders trivially exceed
    /// this threshold.
    fn slow_loader(byte: u8) -> impl FnOnce() -> crate::error::Result<DecodedBlock> {
        move || {
            std::thread::sleep(std::time::Duration::from_micros(50));
            Ok(make_block(byte, 256))
        }
    }

    #[test]
    fn get_or_load_miss_increments_misses_and_disk_us() {
        let cache = BlockCache::new(1024 * 1024);
        let handle = BlockHandle {
            tree_id: 1,
            table_id: 1,
            offset: 0,
        };
        let _ = cache
            .get_or_load(handle, crate::io::Lane::UserIORead, slow_loader(0xab))
            .unwrap();
        let snap = cache.per_tree_snapshot(1);
        assert_eq!(snap.hits, 0);
        assert_eq!(snap.misses, 1);
        assert!(
            snap.disk_read_us_total >= 50,
            "disk_read_us should reflect loader latency, got {}",
            snap.disk_read_us_total
        );
        assert_eq!(snap.cache_read_us_total, 0, "no cache_read_us on miss");
    }

    #[test]
    fn get_or_load_hit_increments_hits_and_cache_us() {
        let cache = BlockCache::new(1024 * 1024);
        let handle = BlockHandle {
            tree_id: 7,
            table_id: 2,
            offset: 0,
        };
        // first call: miss (50 µs simulated disk)
        let _ = cache
            .get_or_load(handle, crate::io::Lane::UserIORead, slow_loader(0xcd))
            .unwrap();
        // second call: hit — loader MUST NOT run
        let _ = cache
            .get_or_load(handle, crate::io::Lane::UserIORead, || {
                panic!("loader must not run on hit")
            })
            .unwrap();
        let snap = cache.per_tree_snapshot(7);
        assert_eq!(snap.hits, 1);
        assert_eq!(snap.misses, 1);
        // cache_read_us is hard to assert tight bounds on a fast machine; check
        // it is strictly less than the disk path (which paid the 50 µs sleep).
        assert!(snap.disk_read_us_total >= 50, "disk path took ≥ 50 µs");
        assert!(
            snap.cache_read_us_total < snap.disk_read_us_total,
            "cache hit must be faster than disk miss; cache={} disk={}",
            snap.cache_read_us_total,
            snap.disk_read_us_total
        );
    }

    #[test]
    fn per_tree_isolation() {
        let cache = BlockCache::new(1024 * 1024);
        let h_a = BlockHandle {
            tree_id: 10,
            table_id: 1,
            offset: 0,
        };
        let h_b = BlockHandle {
            tree_id: 20,
            table_id: 1,
            offset: 0,
        };
        // tree 10: 1 miss + 1 hit
        let _ = cache
            .get_or_load(h_a, crate::io::Lane::UserIORead, || Ok(make_block(1, 64)))
            .unwrap();
        let _ = cache
            .get_or_load(h_a, crate::io::Lane::UserIORead, || panic!("hit"))
            .unwrap();
        // tree 20: 1 miss only
        let _ = cache
            .get_or_load(h_b, crate::io::Lane::UserIORead, || Ok(make_block(2, 64)))
            .unwrap();
        let s10 = cache.per_tree_snapshot(10);
        let s20 = cache.per_tree_snapshot(20);
        assert_eq!(s10.hits, 1);
        assert_eq!(s10.misses, 1);
        assert_eq!(s20.hits, 0);
        assert_eq!(s20.misses, 1);
    }

    #[test]
    fn per_tree_snapshot_unknown_tree_is_zero() {
        let cache = BlockCache::new(1024);
        let snap = cache.per_tree_snapshot(99);
        assert_eq!(snap, PerTreeBlockCacheSnapshot::default());
    }

    #[test]
    fn timing_accumulates_across_calls() {
        let cache = BlockCache::new(1024 * 1024);
        let h = BlockHandle {
            tree_id: 42,
            table_id: 1,
            offset: 0,
        };
        let _ = cache
            .get_or_load(h, crate::io::Lane::UserIORead, || Ok(make_block(0, 64)))
            .unwrap(); // miss
        let _ = cache
            .get_or_load(h, crate::io::Lane::UserIORead, || panic!("hit"))
            .unwrap(); // hit
        let _ = cache
            .get_or_load(h, crate::io::Lane::UserIORead, || panic!("hit"))
            .unwrap(); // hit
        let snap = cache.per_tree_snapshot(42);
        assert_eq!(snap.hits, 2);
        assert_eq!(snap.misses, 1);
    }

    #[test]
    fn miss_counter_increments_on_loader_failure() {
        // Regression: previously `loader()?` early-returned before per-tree
        // counters incremented, causing per-tree Σ misses to drift below
        // the quick_cache global miss counter. After the v0.3-cycle fix,
        // per-tree miss is incremented BEFORE the loader runs and
        // disk_read_us_total is captured on both success and failure paths.
        let cache = BlockCache::new(1024 * 1024);
        let h = BlockHandle {
            tree_id: 99,
            table_id: 1,
            offset: 0,
        };

        // Loader fails with a slow simulation (50 µs sleep so disk_read_us
        // is observably non-zero on fast machines).
        let result = cache.get_or_load(h, crate::io::Lane::UserIORead, || {
            std::thread::sleep(std::time::Duration::from_micros(50));
            Err(crate::error::Error::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "simulated unlink during compaction",
            )))
        });
        assert!(result.is_err(), "loader failure must propagate");

        let snap = cache.per_tree_snapshot(99);
        assert_eq!(
            snap.misses, 1,
            "per-tree miss must count even on loader failure"
        );
        assert_eq!(snap.hits, 0);
        assert!(
            snap.disk_read_us_total >= 50,
            "disk_read_us_total must capture loader-failure attempt time, got {}",
            snap.disk_read_us_total
        );
        // No block ended up in cache:
        assert!(
            cache.get(&h).is_none(),
            "failed loader must not insert a block"
        );
    }

    #[test]
    fn per_tree_misses_match_global_under_mixed_success_and_failure() {
        // Validates per-tree Σ misses = global misses across a mixed stream.
        // In the original implementation, this would drift by N (the count
        // of failing loaders) — exactly the 12-miss drift observed in the
        // 162 s SSD smoke.
        let cache = BlockCache::new(1024 * 1024);

        // 5 successful misses (each on a fresh handle so it's a real miss)
        for i in 0..5u64 {
            let h = BlockHandle {
                tree_id: 1,
                table_id: 100,
                offset: i * 64,
            };
            let _ = cache
                .get_or_load(h, crate::io::Lane::UserIORead, || Ok(make_block(0xab, 64)))
                .expect("success path");
        }
        // 3 failing misses
        for i in 0..3u64 {
            let h = BlockHandle {
                tree_id: 1,
                table_id: 200,
                offset: i * 64,
            };
            let _ = cache.get_or_load(h, crate::io::Lane::UserIORead, || {
                Err(crate::error::Error::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "fail",
                )))
            });
        }

        let per_tree = cache.per_tree_snapshot(1);
        // Per-tree miss must equal global miss (8 = 5 + 3).
        assert_eq!(per_tree.misses, 8);
        assert_eq!(cache.misses(), 8);
        assert_eq!(
            per_tree.misses,
            cache.misses(),
            "per-tree must equal global"
        );
    }

    #[test]
    fn pread_bucket_boundaries_are_exact() {
        // Verify boundary mapping for the 10-bucket pread histogram
        // (v0.3.2 Spike B). Boundaries must match the design doc §2.3.
        assert_eq!(pread_bucket(0), 0); // floor
        assert_eq!(pread_bucket(999), 0); // < 1 ms
        assert_eq!(pread_bucket(1_000), 1); // 1 ms exact -> b1
        assert_eq!(pread_bucket(2_999), 1);
        assert_eq!(pread_bucket(3_000), 2); // NCQ band entry
        assert_eq!(pread_bucket(4_999), 2);
        assert_eq!(pread_bucket(5_000), 3); // HDD inner-track entry
        assert_eq!(pread_bucket(7_999), 3);
        assert_eq!(pread_bucket(8_000), 4); // HDD typical entry
        assert_eq!(pread_bucket(11_999), 4);
        assert_eq!(pread_bucket(12_000), 5); // HDD worst-case entry
        assert_eq!(pread_bucket(19_999), 5);
        assert_eq!(pread_bucket(20_000), 6); // queueing onset
        assert_eq!(pread_bucket(49_999), 6);
        assert_eq!(pread_bucket(50_000), 7); // heavy queueing
        assert_eq!(pread_bucket(99_999), 7);
        assert_eq!(pread_bucket(100_000), 8); // pathological
        assert_eq!(pread_bucket(299_999), 8);
        assert_eq!(pread_bucket(300_000), 9); // outlier
        assert_eq!(pread_bucket(u64::MAX), 9);
    }

    // ─── v0.4 cp 4.2.1: lane admission policy ──────────────────────────

    /// Policy on: Compaction miss does NOT insert; user can later read
    /// the same block from disk (the closure runs again). The skipped
    /// counter increments for the Compaction lane.
    #[test]
    fn lane_admission_compaction_miss_does_not_insert() {
        let cache = BlockCache::with_config(1024 * 1024, /*lane_admission_enabled=*/ true);
        let h = BlockHandle {
            tree_id: 1,
            table_id: 1,
            offset: 0,
        };
        // Compaction miss: loader runs, but block is NOT cached.
        let _ = cache
            .get_or_load(h, crate::io::Lane::Compaction { target_level: 1 }, || {
                Ok(make_block(0xaa, 64))
            })
            .unwrap();
        // Subsequent get() must miss because the policy declined to admit.
        assert!(cache.get(&h).is_none(), "compaction miss must NOT insert");
        let snap = cache.admission_snapshot();
        // Compaction lane = index 3.
        assert_eq!(snap[3], (0, 1), "compaction skipped++");
    }

    /// Policy on: Flush miss skips admission identically to Compaction.
    #[test]
    fn lane_admission_flush_miss_does_not_insert() {
        let cache = BlockCache::with_config(1024 * 1024, true);
        let h = BlockHandle {
            tree_id: 1,
            table_id: 1,
            offset: 0,
        };
        let _ = cache
            .get_or_load(h, crate::io::Lane::Flush, || Ok(make_block(0xbb, 64)))
            .unwrap();
        assert!(cache.get(&h).is_none(), "flush miss must NOT insert");
        let snap = cache.admission_snapshot();
        // Flush lane = index 2.
        assert_eq!(snap[2], (0, 1), "flush skipped++");
    }

    /// Policy on: UserIORead always admits, regardless of policy state.
    /// The admitted counter increments for the UserIORead lane.
    #[test]
    fn lane_admission_user_read_admits() {
        let cache = BlockCache::with_config(1024 * 1024, true);
        let h = BlockHandle {
            tree_id: 1,
            table_id: 1,
            offset: 0,
        };
        let _ = cache
            .get_or_load(h, crate::io::Lane::UserIORead, || Ok(make_block(0xcc, 64)))
            .unwrap();
        assert!(cache.get(&h).is_some(), "user read must admit");
        let snap = cache.admission_snapshot();
        // UserIORead lane = index 0.
        assert_eq!(snap[0], (1, 0), "user_io_read admitted++");
    }

    /// Policy off: every miss admits, regardless of lane. Sanity for the
    /// `--block-cache-lane-admission disabled` toggle.
    #[test]
    fn lane_admission_disabled_admits_all_lanes() {
        let cache = BlockCache::with_config(1024 * 1024, /*enabled=*/ false);
        let h_compact = BlockHandle {
            tree_id: 1,
            table_id: 1,
            offset: 0,
        };
        let h_flush = BlockHandle {
            tree_id: 1,
            table_id: 1,
            offset: 64,
        };
        let _ = cache
            .get_or_load(
                h_compact,
                crate::io::Lane::Compaction { target_level: 0 },
                || Ok(make_block(0xdd, 64)),
            )
            .unwrap();
        let _ = cache
            .get_or_load(h_flush, crate::io::Lane::Flush, || Ok(make_block(0xee, 64)))
            .unwrap();
        // Both blocks are now cached because the policy is off.
        assert!(cache.get(&h_compact).is_some(), "policy off → admit");
        assert!(cache.get(&h_flush).is_some(), "policy off → admit");
        let snap = cache.admission_snapshot();
        assert_eq!(snap[3].0, 1, "compaction admitted (policy off)");
        assert_eq!(snap[2].0, 1, "flush admitted (policy off)");
        // Skipped never increments when policy is off.
        assert_eq!(snap[3].1, 0);
        assert_eq!(snap[2].1, 0);
    }

    /// Regression gate: with no concurrent Compaction/Flush activity,
    /// the user-side hit-rate is identical between policy=on and
    /// policy=off. Cycle plan §3 Bloque 4 R4.1 — the policy must NOT
    /// degrade single-workload behaviour.
    #[test]
    fn lane_admission_no_regression_when_no_compaction_concurrent() {
        for &policy_on in &[true, false] {
            let cache = BlockCache::with_config(1024 * 1024, policy_on);
            // 100 user reads, alternating fresh + repeated handles to
            // produce a known hit/miss ratio (50 misses + 50 hits).
            for i in 0..100u64 {
                let key_offset = i % 50; // each handle hit twice
                let h = BlockHandle {
                    tree_id: 1,
                    table_id: 1,
                    offset: key_offset * 64,
                };
                let _ = cache
                    .get_or_load(h, crate::io::Lane::UserIORead, || {
                        Ok(make_block(key_offset as u8, 64))
                    })
                    .unwrap();
            }
            let snap = cache.per_tree_snapshot(1);
            assert_eq!(
                snap.misses, 50,
                "user-only workload should miss 50× (policy_on={policy_on})"
            );
            assert_eq!(
                snap.hits, 50,
                "user-only workload should hit 50× (policy_on={policy_on})"
            );
        }
    }

    #[test]
    fn get_or_load_increments_pread_histogram_on_success_and_failure() {
        // Verifies the histogram fires in BOTH cache-miss code paths
        // (success + loader failure). Spike B relies on this invariant —
        // failure-path miss times still represent disk service time.
        let cache = BlockCache::new(1024 * 1024);

        // Successful load.
        let h_ok = BlockHandle {
            tree_id: 7,
            table_id: 1,
            offset: 0,
        };
        let _ = cache
            .get_or_load(h_ok, crate::io::Lane::UserIORead, || {
                Ok(make_block(0xcd, 64))
            })
            .expect("success");

        // Failing load on a different handle.
        let h_err = BlockHandle {
            tree_id: 7,
            table_id: 1,
            offset: 64,
        };
        let _ = cache.get_or_load(h_err, crate::io::Lane::UserIORead, || {
            Err(crate::error::Error::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "fail",
            )))
        });

        let snap = cache.per_tree_snapshot(7);
        let total_buckets: u64 = snap.pread_service_time_buckets.iter().sum();
        assert_eq!(
            total_buckets, 2,
            "histogram must capture both success and failure samples"
        );
        assert_eq!(snap.misses, 2);
    }
}
