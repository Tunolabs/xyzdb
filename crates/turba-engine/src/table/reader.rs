//! SSTable reader: opens an SSTable file, reads index/bloom/meta, supports point reads and scans.

// SPDX-License-Identifier: BUSL-1.1
use crate::block;
use crate::bloom::{self, BloomFilter};
use crate::cache::{BlockCache, BlockHandle, DecodedBlock, MetaHandle, MetaKind, MetaSection};
use crate::error::{Error, Result};
use crate::table::meta::{FOOTER_SIZE, FOOTER_SIZE_V2, Footer, SSTableMeta};
use crate::types::{Entry, SeqNo};
use byteorder_lite::{LittleEndian, ReadBytesExt};
use std::fs::File;
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// An index entry pointing to a data block on disk.
#[derive(Debug, Clone)]
pub struct IndexEntry {
    pub last_key: Vec<u8>,
    pub last_seqno: u64,
    pub offset: u64,
    pub size: u32,
}

pub struct SSTableReader {
    path: PathBuf,
    meta: SSTableMeta,
    /// On-disk location `(offset, len)` of the block index. Like the bloom, the
    /// index is NOT retained resident (~53MB per keyspace at scale); it is parsed
    /// lazily and cached via the metadata cache, reloaded from here on a miss.
    /// `block_count` stays resident via `meta.block_count`. See [`Self::index_section`].
    index_loc: (u64, usize),
    /// On-disk location `(offset, len)` of the bloom-filter block. The bloom is
    /// NOT retained resident (~179MB per keyspace at scale); it is parsed lazily
    /// and cached via the metadata cache, reloaded from here on a miss. See
    /// [`Self::bloom_maybe_contains`].
    bloom_loc: (u64, usize),
    cache: Arc<BlockCache>,
    tree_id: u64,
    table_id: u64,
    /// On-disk location `(offset, len)` of the meta block. The zone-map blob
    /// lives inside it but is NOT retained resident (the 565MB-per-keyspace
    /// cost); it is fetched lazily via the metadata cache and reloaded from
    /// here on a cache miss. See [`Self::zone_maps`].
    meta_loc: (u64, usize),
    /// Lazy file handle for pread — opened on first block read, zero FDs during BULKMODE.
    file: parking_lot::Mutex<Option<Arc<std::fs::File>>>,
    /// Shared I/O scheduler for `before_op` / `after_op` instrumentation.
    /// Cloned from the Tree at table-open time. v0.3-cycle Spike A.2 c2.
    scheduler: Arc<crate::io::Scheduler>,
    /// Bytes read synchronously at open time: `index_len + bloom_len + meta_len`.
    /// Excludes the fixed-size footer. Aggregated by `Tree::open_with_scheduler`
    /// into `WarmupStats.bytes_loaded` for STATS reporting (H1.1).
    warmup_bytes: u64,
}

impl SSTableReader {
    /// Convenience for tests: defaults `tree_id=0` and a fresh
    /// Passthrough scheduler. Production code uses `open_with_tree_id`.
    pub fn open(path: &Path, cache: Arc<BlockCache>) -> Result<Self> {
        Self::open_with_tree_id(
            path,
            cache,
            0,
            Arc::new(crate::io::Scheduler::passthrough()),
        )
    }

    pub fn open_with_tree_id(
        path: &Path,
        cache: Arc<BlockCache>,
        tree_id: u64,
        scheduler: Arc<crate::io::Scheduler>,
    ) -> Result<Self> {
        let mut file = File::open(path)?;
        let file_len = file.metadata()?.len();

        if file_len < FOOTER_SIZE as u64 {
            return Err(Error::Corruption("file too small for footer".into()));
        }

        // Read footer. Pull up to the v2 (checksummed) footer size from the
        // end; Footer::decode auto-detects v2 vs legacy v1 and returns the
        // on-disk footer length so the meta block can be bounded.
        let read_len = file_len.min(FOOTER_SIZE_V2 as u64) as usize;
        file.seek(SeekFrom::End(-(read_len as i64)))?;
        let mut footer_buf = vec![0u8; read_len];
        file.read_exact(&mut footer_buf)?;
        let (footer, footer_size) = Footer::decode(&footer_buf)?;

        // Block index: NOT read/decoded here — fetched lazily through the
        // metadata cache (decoded on first use, evictable). Record its location.
        // `block_count` stays available via `meta.block_count`.
        let index_len = (footer.bloom_offset - footer.index_offset) as usize;
        let index_loc = (footer.index_offset, index_len);

        // Bloom filter: NOT read/parsed here — it is fetched lazily through the
        // metadata cache (parsed on first use, evictable). Record its location.
        let bloom_len = (footer.meta_offset - footer.bloom_offset) as usize;
        let bloom_loc = (footer.bloom_offset, bloom_len);

        // Read meta
        file.seek(SeekFrom::Start(footer.meta_offset))?;
        let meta_len = (file_len - footer_size as u64 - footer.meta_offset) as usize;
        let mut meta_data = vec![0u8; meta_len];
        file.read_exact(&mut meta_data)?;
        let mut meta = SSTableMeta::decode(&meta_data)?;

        // Drop the zone-map blob from the resident meta — it dominates resident
        // RAM (~2 MB/SST). It is fetched lazily through the metadata cache and
        // reloaded from `meta_loc` on a miss (see `zone_maps`). The small meta
        // fields (key bounds, counts) stay resident.
        meta.zone_maps = Vec::new();

        // No file handle opened here — lazy init on first block read.
        // During BULKMODE, thousands of SSTables exist with zero FDs.
        // Index + bloom are excluded (loaded lazily, not at open).
        let warmup_bytes = meta_len as u64;
        Ok(Self {
            path: path.to_path_buf(),
            tree_id,
            table_id: meta.table_id,
            meta,
            index_loc,
            bloom_loc,
            cache,
            meta_loc: (footer.meta_offset, meta_len),
            file: parking_lot::Mutex::new(None),
            scheduler,
            warmup_bytes,
        })
    }

    /// Force-open the lazy pread file handle. Call after open_with_tree_id
    /// when the file must remain accessible even if later deleted from disk
    /// (POSIX keeps open handles valid after unlink).
    pub fn warm_handle(&self) -> Result<()> {
        let mut guard = self.file.lock();
        if guard.is_none() {
            match File::open(&self.path) {
                Ok(f) => *guard = Some(Arc::new(f)),
                Err(e) => {
                    eprintln!(
                        "WARM HANDLE FAILED: path={:?}, exists={}, table_id={}, error={}",
                        self.path,
                        self.path.exists(),
                        self.table_id,
                        e
                    );
                    return Err(e.into());
                }
            }
        }
        Ok(())
    }

    pub fn meta(&self) -> &SSTableMeta {
        &self.meta
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn table_id(&self) -> u64 {
        self.table_id
    }

    /// Number of data blocks. Resident via `meta.block_count` (a single u32) —
    /// no index fetch, so iterators can bound their range without touching the
    /// (now cacheable) index.
    pub(crate) fn block_count(&self) -> usize {
        self.meta.block_count as usize
    }

    /// Fetch the (cacheable) block index, decoding it from disk on a miss.
    /// Immutable per SST → eviction is correctness-free (a reload yields the
    /// same entries). Unlike zone maps / bloom, the index is required to locate
    /// blocks, so callers propagate the error rather than degrading.
    pub(crate) fn index_section(&self) -> Result<Arc<MetaSection>> {
        let handle = MetaHandle {
            tree_id: self.tree_id,
            table_id: self.table_id,
            kind: MetaKind::Index,
        };
        self.cache.meta_get_or_load(handle, || {
            let (offset, len) = self.index_loc;
            let data = self.read_at(offset, len)?;
            Ok(MetaSection::Index(Self::decode_index(&data)?))
        })
    }

    /// Borrow the `IndexEntry` slice out of a section fetched via
    /// [`Self::index_section`]. Returns an empty slice for a non-Index section
    /// (never happens for an Index handle).
    fn index_entries(section: &MetaSection) -> &[IndexEntry] {
        match section {
            MetaSection::Index(v) => v.as_slice(),
            _ => &[],
        }
    }

    /// Verify every data block's on-disk checksum, reading raw bytes straight
    /// from the file (bypassing the decoded block cache) so the persisted bytes
    /// are actually exercised. Uses [`block::validate_checksum`], which checks
    /// the header and the XXH3-128 data checksum WITHOUT decompressing. Returns
    /// the indices of blocks that fail; a block that cannot even be read counts
    /// as failed. Alert-only — never mutates the file.
    pub fn verify_blocks(&self) -> Vec<usize> {
        let section = match self.index_section() {
            Ok(s) => s,
            Err(_) => return (0..self.block_count()).collect(),
        };
        let index = Self::index_entries(&section);
        let mut bad = Vec::new();
        let mut file = match File::open(&self.path) {
            Ok(f) => f,
            Err(_) => return (0..index.len()).collect(),
        };
        for (i, entry) in index.iter().enumerate() {
            let mut buf = vec![0u8; entry.size as usize];
            let ok = file.seek(SeekFrom::Start(entry.offset)).is_ok()
                && file.read_exact(&mut buf).is_ok()
                && block::validate_checksum(&buf).is_ok();
            if !ok {
                bad.push(i);
            }
        }
        bad
    }

    /// Resident bytes of the block index. Now `0` — the index is held in the
    /// evictable metadata cache, not resident in the reader. Diagnostic-only;
    /// cache-resident metadata is reported via `BlockCache::meta_current_weight`.
    pub fn index_bytes(&self) -> usize {
        0
    }

    /// Resident bytes of the bloom filter's bit array. Now `0` — the bloom is
    /// held in the evictable metadata cache, not resident in the reader.
    /// Diagnostic-only; cache-resident metadata is reported via
    /// `BlockCache::meta_current_weight`.
    pub fn bloom_bytes(&self) -> usize {
        0
    }

    /// Bytes read from disk at `open_with_tree_id` time: `index_len + bloom_len
    /// + meta_len`. Distinct from `index_bytes` / `bloom_bytes`, which report
    /// resident in-memory sizes — the on-disk bloom is encoded and the in-memory
    /// `IndexEntry` vec carries fixed-per-entry overhead. Aggregated by
    /// `Tree::open_with_scheduler` for `WarmupStats.bytes_loaded` (H1.1).
    pub fn warmup_bytes(&self) -> u64 {
        self.warmup_bytes
    }

    /// Point read: look up a single key. Uses bloom filter to skip if absent.
    pub fn get(&self, user_key: &[u8], visible_seqno: SeqNo) -> Result<Option<Entry>> {
        // Bloom filter check (fetched via the metadata cache, parsed once).
        let hash = bloom::key_hash(user_key);
        if !self.bloom_maybe_contains(hash)? {
            return Ok(None);
        }

        // Binary search on block index (fetched via the metadata cache).
        let section = self.index_section()?;
        let index = Self::index_entries(&section);
        let block_idx =
            match index.binary_search_by(|idx_entry| idx_entry.last_key.as_slice().cmp(user_key)) {
                Ok(i) => i,  // exact match on last_key
                Err(i) => i, // first block whose last_key > user_key
            };

        if block_idx < index.len() {
            let entries = self.load_block(crate::io::Lane::UserIORead, &index[block_idx])?;
            if let Some(entry) = block::point_read(&entries, user_key, visible_seqno) {
                return Ok(Some(entry));
            }
        }

        Ok(None)
    }

    /// Point read that SKIPS the bloom filter (block index + data block only).
    ///
    /// Crash-recovery read-path fallback ONLY. A correct bloom never false-negatives,
    /// so [`Self::get`]'s bloom skip is safe in normal operation. But after an unclean
    /// crash a post-recovery SSTable can carry a bloom that disagrees with its data
    /// (see the recovery-bloom root ticket); then `get` returns `None` for a key the
    /// scan path (which never consults the bloom) can see. This path trusts the index
    /// + block, never the bloom, so a caller that already knows the key is live (it
    /// came from a scan) can recover it. Never wire this into the hot point-get path —
    /// it defeats the bloom's purpose.
    pub fn get_no_bloom(&self, user_key: &[u8], visible_seqno: SeqNo) -> Result<Option<Entry>> {
        let section = self.index_section()?;
        let index = Self::index_entries(&section);
        let block_idx =
            match index.binary_search_by(|idx_entry| idx_entry.last_key.as_slice().cmp(user_key)) {
                Ok(i) => i,
                Err(i) => i,
            };
        if block_idx < index.len() {
            let entries = self.load_block(crate::io::Lane::UserIORead, &index[block_idx])?;
            if let Some(entry) = block::point_read(&entries, user_key, visible_seqno) {
                return Ok(Some(entry));
            }
        }
        Ok(None)
    }

    /// Scan all entries in sorted order (for compaction and range reads).
    pub fn scan(&self) -> Result<Vec<Entry>> {
        let section = self.index_section()?;
        let mut all_entries = Vec::new();
        for idx_entry in Self::index_entries(&section) {
            let entries = self.load_block(crate::io::Lane::UserIORead, idx_entry)?;
            all_entries.extend(entries);
        }
        Ok(all_entries)
    }

    /// Scan entries whose keys fall within [start, end) range.
    pub fn scan_range(&self, start: &[u8], end: &[u8]) -> Result<Vec<Entry>> {
        let section = self.index_section()?;
        let mut results = Vec::new();
        for idx_entry in Self::index_entries(&section) {
            // Skip blocks entirely before our range
            if idx_entry.last_key.as_slice() < start {
                continue;
            }

            let entries = self.load_block(crate::io::Lane::UserIORead, idx_entry)?;
            for entry in entries {
                if entry.key.as_slice() >= end {
                    return Ok(results);
                }
                if entry.key.as_slice() >= start {
                    results.push(entry);
                }
            }
        }
        Ok(results)
    }

    /// Scan entries with a given prefix.
    pub fn scan_prefix(&self, prefix: &[u8]) -> Result<Vec<Entry>> {
        let section = self.index_section()?;
        let mut results = Vec::new();
        for idx_entry in Self::index_entries(&section) {
            // Skip blocks that end before our prefix
            if idx_entry.last_key.len() >= prefix.len()
                && idx_entry.last_key[..prefix.len()] < *prefix
            {
                continue;
            }
            // Stop if block starts after our prefix range
            // (We'd need first_key to know for sure, so just load and filter)

            let entries = self.load_block(crate::io::Lane::UserIORead, idx_entry)?;
            for entry in entries {
                if entry.key.starts_with(prefix) {
                    results.push(entry);
                } else if entry.key.as_slice() > prefix && !entry.key.starts_with(prefix) {
                    // Past the prefix range — check if we've seen any matches
                    if !results.is_empty() {
                        return Ok(results);
                    }
                }
            }
        }
        Ok(results)
    }

    /// Load a single decoded block. The `lane` parameter routes the
    /// underlying kernel read through the I/O scheduler — UserIORead for
    /// point lookups and scans, Compaction { target_level } for k-way
    /// merge reads. Only the disk-bound path (cache miss) registers as an
    /// `OpKind::Read` with the scheduler; cache hits do not (they incur
    /// no kernel I/O and would skew per-lane service-time observations).
    pub(crate) fn load_block(
        &self,
        lane: crate::io::Lane,
        idx_entry: &IndexEntry,
    ) -> Result<Vec<Entry>> {
        let handle = BlockHandle {
            tree_id: self.tree_id,
            table_id: self.table_id,
            offset: idx_entry.offset,
        };

        let offset = idx_entry.offset;
        let size = idx_entry.size;
        let file_ref = {
            let mut guard = self.file.lock();
            if guard.is_none() {
                match File::open(&self.path) {
                    Ok(f) => *guard = Some(Arc::new(f)),
                    Err(e) => {
                        eprintln!(
                            "BLOCK READ FAILED: path={:?}, exists={}, table_id={}, error={}",
                            self.path,
                            self.path.exists(),
                            self.table_id,
                            e
                        );
                        return Err(e.into());
                    }
                }
            }
            Arc::clone(guard.as_ref().unwrap())
        };
        let scheduler = &self.scheduler;
        // Decode a block exactly ONCE per read. On a miss the loader reads + decodes
        // (validate checksums, decompress, parse) and hands back both the compressed
        // bytes to cache AND the parsed entries to return — no second decode. On a hit
        // the resident compressed block is decoded once here. (Previously the miss
        // decoded twice: once discarded just to count entries, once to return.)
        match self.cache.get_or_load_returning(handle, lane, || {
            // Cache miss → kernel pread happens. Scheduler observes here;
            // cache hits skip this closure entirely and never register
            // an OpKind::Read with the scheduler.
            scheduler.before_op(lane, crate::io::OpKind::Read { bytes: size });
            let read_start = std::time::Instant::now();
            // pread: thread-safe, no seek, shared file handle
            let mut raw = vec![0u8; size as usize];
            let read_result: std::io::Result<()> = {
                use std::os::unix::fs::FileExt;
                file_ref.read_exact_at(&mut raw, offset)
            };
            scheduler.after_op(
                lane,
                crate::io::OpKind::Read { bytes: size },
                read_start.elapsed().as_micros() as u64,
            );
            read_result?;
            // Validate + parse ONCE, before caching; the compressed bytes are cached.
            let entries = block::decode(&raw)?;
            Ok((DecodedBlock { data: raw }, entries))
        })? {
            crate::cache::Loaded::Miss(_block, entries) => Ok(entries),
            crate::cache::Loaded::Hit(block) => block::decode(&block.data),
        }
    }

    /// Configured block-cache capacity (bytes). Used by bulk scan iterators to
    /// decide, from their on-disk span, whether admitting their blocks could
    /// thrash the cache (span > capacity) and should route through `Lane::Scan`.
    pub(crate) fn cache_capacity(&self) -> u64 {
        self.cache.capacity()
    }

    /// G3 (0.9): advise the kernel to read `[offset, offset+len)` of this SST's
    /// data file sequentially. Called ONCE per bulk scan at iterator
    /// construction — never on point lookups. Best-effort and side-effect only:
    /// it allocates nothing and reads nothing into the process (the kernel does
    /// the read-ahead in its own page cache, keeping the app at O(block)); a
    /// failure only forfeits the optimisation. This is the OPPOSITE of
    /// compaction's `F_NOCACHE`/`FADV_DONTNEED`, which asks the kernel to DROP
    /// pages — a user scan wants them prefetched. Outcome is recorded on the
    /// shared cache for the deterministic G3 gate.
    pub(crate) fn hint_sequential_readahead(&self, offset: u64, len: u64) {
        if len == 0 {
            return;
        }
        // Reuse / lazily populate the shared file handle (same as load_block).
        let file_ref = {
            let mut guard = self.file.lock();
            if guard.is_none() {
                match File::open(&self.path) {
                    Ok(f) => *guard = Some(Arc::new(f)),
                    // The scan will surface any real open error on first read.
                    Err(_) => return,
                }
            }
            Arc::clone(guard.as_ref().unwrap())
        };
        let ok = platform_readahead_hint(&file_ref, offset, len).is_ok();
        self.cache.record_readahead_hint(offset, len, ok);
    }

    /// Read `len` bytes at `offset` via pread (thread-safe, shares the lazy file
    /// handle). Used to reload evictable metadata sections on a cache miss.
    fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; len];
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            let file_ref = {
                let mut guard = self.file.lock();
                if guard.is_none() {
                    *guard = Some(Arc::new(File::open(&self.path)?));
                }
                Arc::clone(guard.as_ref().unwrap())
            };
            file_ref.read_exact_at(&mut buf, offset)?;
        }
        #[cfg(not(unix))]
        {
            let mut f = File::open(&self.path)?;
            f.seek(SeekFrom::Start(offset))?;
            f.read_exact(&mut buf)?;
        }
        Ok(buf)
    }

    /// Lazily fetch this SST's zone-map blob via the metadata cache. On a miss
    /// it re-reads the meta block from `meta_loc` and re-extracts the zone maps,
    /// then caches the blob (a cold path that then serves many scans). Returns an
    /// empty blob when the SST has no zone maps. The blob is immutable per SST,
    /// so a reload is byte-identical and eviction never affects correctness —
    /// at worst a scan loses block pruning until the blob is back in cache.
    pub fn zone_maps(&self) -> Result<Arc<MetaSection>> {
        let handle = MetaHandle {
            tree_id: self.tree_id,
            table_id: self.table_id,
            kind: MetaKind::ZoneMaps,
        };
        self.cache.meta_get_or_load(handle, || {
            let (offset, len) = self.meta_loc;
            let meta_data = self.read_at(offset, len)?;
            Ok(MetaSection::ZoneMaps(
                SSTableMeta::decode(&meta_data)?.zone_maps,
            ))
        })
    }

    /// Bloom probe via the metadata cache: `true` if `hash` *may* be present.
    /// The bloom is parsed once on a miss (from `bloom_loc`) and cached; a hot
    /// SST hits the cache, a cold one reloads. Immutable per SST → eviction is
    /// correctness-free. On a load error, returns `true` (do not skip the SST).
    fn bloom_maybe_contains(&self, hash: u64) -> Result<bool> {
        let handle = MetaHandle {
            tree_id: self.tree_id,
            table_id: self.table_id,
            kind: MetaKind::Bloom,
        };
        let section = self.cache.meta_get_or_load(handle, || {
            let (offset, len) = self.bloom_loc;
            let bytes = self.read_at(offset, len)?;
            let bf = BloomFilter::from_bytes(&bytes)
                .ok_or_else(|| Error::Corruption("invalid bloom filter".into()))?;
            Ok(MetaSection::Bloom(bf))
        })?;
        match &*section {
            MetaSection::Bloom(bf) => Ok(bf.maybe_contains(hash)),
            _ => Ok(true),
        }
    }

    fn decode_index(data: &[u8]) -> Result<Vec<IndexEntry>> {
        let mut cursor = Cursor::new(data);
        let count = cursor.read_u32::<LittleEndian>()? as usize;
        let mut entries = Vec::with_capacity(count);

        for _ in 0..count {
            // u32 length — matches the write path in writer::encode_index.
            // v0.2.1 audit upgrade (user keys have no hard upper bound).
            let key_len = cursor.read_u32::<LittleEndian>()? as usize;
            let mut key = vec![0u8; key_len];
            cursor.read_exact(&mut key)?;
            let seqno = cursor.read_u64::<LittleEndian>()?;
            let offset = cursor.read_u64::<LittleEndian>()?;
            let size = cursor.read_u32::<LittleEndian>()?;

            entries.push(IndexEntry {
                last_key: key,
                last_seqno: seqno,
                offset,
                size,
            });
        }

        Ok(entries)
    }
}

// --- Streaming block-by-block iterator ---

use crate::tree::version::TableHandle;

/// Streaming iterator over an SSTable: reads one block at a time.
/// At any moment, only the current block's entries are in memory (~32KB).
/// Holds an Arc<TableHandle> to keep the reader alive.
///
/// The `lane` field is parameterised at construction (shape proposal §9
/// decision 1) — every BlockIter has a known caller, so the iter cannot
/// exist without a Lane. UserIORead for tree-level scans, Compaction for
/// k-way merge inputs.
pub struct SSTableBlockIter {
    table: Arc<TableHandle>,
    /// The SST's block index, fetched once at construction and held for the
    /// iteration so `next` indexes it without a per-block cache lookup. Holding
    /// the Arc also pins it in the metadata cache for the scan's duration.
    index: Arc<MetaSection>,
    block_idx: usize,
    end_block_idx: usize, // exclusive upper bound (block_count = scan all)
    current: std::vec::IntoIter<Entry>,
    /// Optional per-block skip predicate. Returns true = load, false = skip.
    /// Used by zone maps to skip blocks that can't contain matching records.
    block_filter: Option<Box<dyn Fn(usize) -> bool>>,
    /// Lane tag passed to `load_block` for every block read. Set at
    /// construction; cannot change mid-iteration.
    lane: crate::io::Lane,
}

/// Choose the block-read lane for a bulk scan from its on-disk span (G2, 0.9).
///
/// A user-facing scan whose span (summed size of the blocks it will touch)
/// exceeds the block-cache capacity cannot benefit from admission: admitting
/// its blocks would self-evict and evict the hot working set (it thrashes
/// either way). Such a scan routes through [`crate::io::Lane::Scan`], which
/// bypasses admission. A scan that FITS (`span <= capacity`, the gray zone)
/// keeps its incoming `UserIORead` lane and admits exactly as before — zero
/// regression by design. Non-user lanes (Compaction / Flush k-way merge inputs)
/// are never reclassified, so only bulk user scans are affected.
fn scan_lane_for_span(
    lane: crate::io::Lane,
    span_bytes: u64,
    cache_capacity: u64,
) -> crate::io::Lane {
    match lane {
        crate::io::Lane::UserIORead if span_bytes > cache_capacity => crate::io::Lane::Scan,
        other => other,
    }
}

/// Kernel sequential read-ahead hint over `[offset, offset+len)` of `file` (G3).
/// Linux: `posix_fadvise(POSIX_FADV_SEQUENTIAL)` (widens the read-ahead window).
/// macOS: `fcntl(F_RDADVISE)` (schedules read-ahead of the range). No-op on
/// other targets. Pure hint: no allocation, no read into the process.
#[cfg(target_os = "linux")]
fn platform_readahead_hint(file: &File, offset: u64, len: u64) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    // SAFETY: `file` owns a valid fd for the duration of the call; posix_fadvise
    // only updates kernel-side read-ahead state for that fd and neither reads nor
    // writes any of our process memory.
    let ret = unsafe {
        libc::posix_fadvise(
            file.as_raw_fd(),
            offset as libc::off_t,
            len.min(libc::off_t::MAX as u64) as libc::off_t,
            libc::POSIX_FADV_SEQUENTIAL,
        )
    };
    if ret == 0 {
        Ok(())
    } else {
        Err(std::io::Error::from_raw_os_error(ret))
    }
}

#[cfg(target_os = "macos")]
fn platform_readahead_hint(file: &File, offset: u64, len: u64) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let ra = libc::radvisory {
        ra_offset: offset as libc::off_t,
        ra_count: len.min(libc::c_int::MAX as u64) as libc::c_int,
    };
    // SAFETY: `file` owns a valid fd for the call; F_RDADVISE reads the
    // stack-local `radvisory` we pass by pointer and schedules kernel
    // read-ahead. The kernel does not retain the pointer or touch our memory.
    let ret = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_RDADVISE, &ra) };
    if ret == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn platform_readahead_hint(_file: &File, _offset: u64, _len: u64) -> std::io::Result<()> {
    Ok(())
}

impl SSTableBlockIter {
    /// Create a streaming iterator over ALL blocks. `lane` is the
    /// scheduler tag every block read carries — UserIORead for
    /// user-visible scans, Compaction for k-way merge. Fallible: the (cacheable)
    /// block index is fetched here and held for the iteration.
    pub fn new(table: Arc<TableHandle>, lane: crate::io::Lane) -> Result<Self> {
        let index = table.reader.index_section()?;
        let entries = SSTableReader::index_entries(&index);
        let end = entries.len();
        // G2: a full-table scan touches every block; if that span exceeds the
        // cache it routes through Lane::Scan (bypass admission).
        let span: u64 = entries.iter().map(|e| e.size as u64).sum();
        let lane = scan_lane_for_span(lane, span, table.reader.cache_capacity());
        // G3: hint the whole data extent for sequential read-ahead (bulk scan).
        let hint = match (entries.first(), entries.last()) {
            (Some(f), Some(l)) => Some((
                f.offset,
                (l.offset + l.size as u64).saturating_sub(f.offset),
            )),
            _ => None,
        };
        if let Some((off, len)) = hint {
            table.reader.hint_sequential_readahead(off, len);
        }
        Ok(Self {
            table,
            index,
            block_idx: 0,
            end_block_idx: end,
            current: Vec::new().into_iter(),
            block_filter: None,
            lane,
        })
    }

    /// Create a streaming iterator that only reads blocks overlapping [start, end).
    /// Uses binary search on the block index to skip irrelevant blocks. Fallible
    /// (fetches + holds the cacheable index).
    pub fn new_with_range(
        table: Arc<TableHandle>,
        start: &[u8],
        end: Option<&[u8]>,
        lane: crate::io::Lane,
    ) -> Result<Self> {
        let index = table.reader.index_section()?;
        let entries = SSTableReader::index_entries(&index);
        let first = entries.partition_point(|idx| idx.last_key.as_slice() < start);
        let last = match end {
            Some(end_key) => {
                let l = entries.partition_point(|idx| idx.last_key.as_slice() < end_key);
                // Include the block that might contain end_key
                (l + 1).min(entries.len())
            }
            None => entries.len(),
        };
        // G2: span = summed size of the blocks this iterator will touch. If it
        // exceeds the cache, route through Lane::Scan (bypass admission). An
        // empty/degenerate range (first >= last) yields span 0 → keeps
        // UserIORead (it reads nothing anyway).
        let span: u64 = entries
            .get(first..last)
            .map(|blocks| blocks.iter().map(|e| e.size as u64).sum())
            .unwrap_or(0);
        let lane = scan_lane_for_span(lane, span, table.reader.cache_capacity());
        // G3: hint the byte extent this iterator will sweep for read-ahead.
        let hint = if first < last {
            match (entries.get(first), entries.get(last - 1)) {
                (Some(f), Some(l)) => Some((
                    f.offset,
                    (l.offset + l.size as u64).saturating_sub(f.offset),
                )),
                _ => None,
            }
        } else {
            None
        };
        if let Some((off, len)) = hint {
            table.reader.hint_sequential_readahead(off, len);
        }
        Ok(Self {
            table,
            index,
            block_idx: first,
            end_block_idx: last,
            current: Vec::new().into_iter(),
            block_filter: None,
            lane,
        })
    }

    /// Attach a zone map filter. Blocks where the filter returns false are skipped.
    pub fn with_block_filter(mut self, filter: Box<dyn Fn(usize) -> bool>) -> Self {
        self.block_filter = Some(filter);
        self
    }
}

impl Iterator for SSTableBlockIter {
    type Item = Entry;

    fn next(&mut self) -> Option<Entry> {
        loop {
            if let Some(entry) = self.current.next() {
                return Some(entry);
            }

            if self.block_idx >= self.end_block_idx {
                return None;
            }

            // Zone map skip: check if this block should be loaded
            if let Some(ref filter) = self.block_filter {
                if !filter(self.block_idx) {
                    self.block_idx += 1;
                    continue;
                }
            }

            let idx_entry = SSTableReader::index_entries(&self.index).get(self.block_idx)?;
            match self.table.reader.load_block(self.lane, idx_entry) {
                Ok(entries) => {
                    self.block_idx += 1;
                    self.current = entries.into_iter();
                }
                Err(_) => return None,
            }
        }
    }
}
