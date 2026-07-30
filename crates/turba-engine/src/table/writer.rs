//! SSTable writer: accumulates entries, writes data blocks, index, bloom, meta, footer.
//!
//! Layout:
//! ```text
//! [Data Block 0][Data Block 1]...[Data Block N]
//! [Index Block]                  ← block handles pointing to data blocks
//! [Bloom Filter]                 ← raw bloom bytes
//! [Meta Block]                   ← SSTableMeta encoded
//! [Footer (28 bytes)]            ← magic + section offsets
//! ```

use crate::block::{self, BlockType};
use crate::bloom::{self, BloomBuilder};
use crate::compression::CompressionType;
use crate::error::Result;
use crate::table::meta::{FORMAT_VERSION, Footer, SSTableMeta};
use crate::types::{Entry, ValueType};
use byteorder_lite::{LittleEndian, WriteBytesExt};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::Arc;

/// Callback for building per-block zone maps. Turba passes raw entries;
/// the implementor (xyzdb-engine) interprets values and returns opaque bytes.
pub trait ZoneMapBuilder: Send + Sync {
    fn build_block_zone_map(&self, entries: &[Entry]) -> Vec<u8>;
}

/// Configuration for writing an SSTable.
#[derive(Debug, Clone)]
pub struct SSTableConfig {
    pub data_block_size: usize, // target bytes per data block (before compression)
    pub compression: CompressionType,
    pub bloom_bits_per_key: f64, // 0.0 = no bloom filter
    pub restart_interval: usize, // prefix restart interval
}

impl Default for SSTableConfig {
    fn default() -> Self {
        Self {
            data_block_size: 32 * 1024, // 32KB
            compression: CompressionType::Lz4,
            bloom_bits_per_key: 10.0,
            restart_interval: 16,
        }
    }
}

/// Index entry: last key in a data block + block position.
#[derive(Debug, Clone)]
struct IndexEntry {
    last_key: Vec<u8>,
    last_seqno: u64,
    offset: u64,
    size: u32,
}

pub struct SSTableWriter {
    writer: BufWriter<File>,
    config: SSTableConfig,
    table_id: u64,

    /// Shared I/O scheduler. Cloned from the owning Tree (Flush) or from
    /// the compaction worker (Compaction). Wraps every kernel write_all
    /// + sync_all with `before_op` / `after_op`. v0.3-cycle Spike A.2 c2.
    scheduler: Arc<crate::io::Scheduler>,
    /// Lane this writer reports under: `Lane::Flush` for memtable flush
    /// outputs, `Lane::Compaction { target_level }` for k-way merge outputs.
    /// Set at construction; the writer never serves more than one lane.
    lane: crate::io::Lane,

    // Atomic publish: writes land in `tmp_path` (e.g. 00042.sst.tmp), get
    // fsynced in `finish`, and only then rename(2)'d into `final_path`
    // (00042.sst). Readers (SSTableReader::open) never see partial bytes
    // because they open by final path, which only appears post-rename.
    //
    // This guards against the interaction with `drop_page_cache` in the
    // compact worker: on Linux, `posix_fadvise(FADV_DONTNEED)` right after
    // an unsynced write could evict dirty pages before the kernel writes
    // them back, producing garbage reads. Fsync-then-rename removes both
    // the partial-visibility and the unsynced-evict failure modes.
    final_path: std::path::PathBuf,
    tmp_path: std::path::PathBuf,

    // Current data block being built
    current_block: Vec<Entry>,
    current_block_size: usize,

    // Index: one entry per data block
    index_entries: Vec<IndexEntry>,

    // Bloom filter builder
    bloom_builder: BloomBuilder,

    // Per-block zone maps (opaque, built by callback)
    zone_map_builder: Option<Arc<dyn ZoneMapBuilder>>,
    block_zone_maps: Vec<Vec<u8>>,

    // Stats
    item_count: u64,
    tombstone_count: u64,
    key_min: Option<Vec<u8>>,
    key_max: Option<Vec<u8>>,
    seqno_min: u64,
    seqno_max: u64,
    block_count: u32,
    bytes_written: u64,
}

/// Build the `.tmp` sibling for a given final SSTable path. Kept as a
/// free function so crash-recovery / orphan cleanup can derive it the
/// same way the writer does.
pub fn tmp_path_for(final_path: &Path) -> std::path::PathBuf {
    let mut s = final_path.as_os_str().to_owned();
    s.push(".tmp");
    std::path::PathBuf::from(s)
}

impl SSTableWriter {
    /// Convenience wrapper for tests: defaults scheduler to Passthrough
    /// and lane to Flush. Production callers MUST use
    /// [`SSTableWriter::new_with_scheduler`] / [`Self::with_zone_map_builder`]
    /// with the engine's shared `Arc<Scheduler>` so writes register on
    /// the correct lane.
    pub fn new(path: &Path, table_id: u64, config: SSTableConfig) -> Result<Self> {
        Self::new_with_scheduler(
            path,
            table_id,
            config,
            Arc::new(crate::io::Scheduler::passthrough()),
            crate::io::Lane::Flush,
        )
    }

    pub fn new_with_scheduler(
        path: &Path,
        table_id: u64,
        config: SSTableConfig,
        scheduler: Arc<crate::io::Scheduler>,
        lane: crate::io::Lane,
    ) -> Result<Self> {
        Self::with_zone_map_builder(path, table_id, config, None, scheduler, lane)
    }

    pub fn with_zone_map_builder(
        path: &Path,
        table_id: u64,
        config: SSTableConfig,
        zone_map_builder: Option<Arc<dyn ZoneMapBuilder>>,
        scheduler: Arc<crate::io::Scheduler>,
        lane: crate::io::Lane,
    ) -> Result<Self> {
        let final_path = path.to_path_buf();
        let tmp_path = tmp_path_for(path);
        let file = File::create(&tmp_path)?;
        Ok(Self {
            writer: BufWriter::new(file),
            bloom_builder: BloomBuilder::new(config.bloom_bits_per_key),
            config,
            table_id,
            final_path,
            tmp_path,
            current_block: Vec::new(),
            current_block_size: 0,
            index_entries: Vec::new(),
            zone_map_builder,
            block_zone_maps: Vec::new(),
            item_count: 0,
            tombstone_count: 0,
            key_min: None,
            key_max: None,
            seqno_min: u64::MAX,
            seqno_max: 0,
            block_count: 0,
            bytes_written: 0,
            scheduler,
            lane,
        })
    }

    /// Wrap a kernel-bound write through the I/O scheduler. Records
    /// `OpKind::Write { bytes }` on `lane` with the elapsed time of the
    /// underlying `BufWriter::write_all`.
    ///
    /// Free-function shape (vs `&mut self` method) so call sites can
    /// invoke this concurrently with partial-move state inside `finish`,
    /// where `self.bloom_builder.finish()` and `self.key_max.unwrap_*`
    /// have already taken sub-fields by value.
    fn write_with_sched(
        writer: &mut BufWriter<File>,
        scheduler: &crate::io::Scheduler,
        lane: crate::io::Lane,
        buf: &[u8],
    ) -> Result<()> {
        let bytes = buf.len() as u32;
        scheduler.before_op(lane, crate::io::OpKind::Write { bytes });
        let start = std::time::Instant::now();
        let res = writer.write_all(buf);
        scheduler.after_op(
            lane,
            crate::io::OpKind::Write { bytes },
            start.elapsed().as_micros() as u64,
        );
        res?;
        Ok(())
    }

    /// Add an entry. Entries MUST be added in sorted order (by InternalKey).
    pub fn add(&mut self, entry: Entry) -> Result<()> {
        // Track bloom
        if self.config.bloom_bits_per_key > 0.0 {
            self.bloom_builder.insert(bloom::key_hash(&entry.key));
        }

        // Track stats
        self.item_count += 1;
        if entry.value_type == ValueType::Tombstone {
            self.tombstone_count += 1;
        }
        if self.key_min.is_none() {
            self.key_min = Some(entry.key.clone());
        }
        self.key_max = Some(entry.key.clone());
        self.seqno_min = self.seqno_min.min(entry.seqno);
        self.seqno_max = self.seqno_max.max(entry.seqno);

        // Estimate entry size for block splitting
        let entry_size = entry.key.len() + entry.value.len() + 20; // approx overhead
        self.current_block_size += entry_size;
        self.current_block.push(entry);

        // Flush block if it exceeds target size
        if self.current_block_size >= self.config.data_block_size {
            self.flush_data_block()?;
        }

        Ok(())
    }

    fn flush_data_block(&mut self) -> Result<()> {
        if self.current_block.is_empty() {
            return Ok(());
        }

        let entries = std::mem::take(&mut self.current_block);
        self.current_block_size = 0;

        // Build zone map for this block before encoding
        if let Some(ref builder) = self.zone_map_builder {
            let zm = builder.build_block_zone_map(&entries);
            self.block_zone_maps.push(zm);
        }

        let last_key = entries.last().unwrap().key.clone();
        let last_seqno = entries.last().unwrap().seqno;

        let encoded = block::encode_with_restart_interval(
            &entries,
            self.config.compression,
            BlockType::Data,
            self.config.restart_interval,
        );

        let offset = self.bytes_written;
        let size = encoded.len() as u32;

        Self::write_with_sched(&mut self.writer, &self.scheduler, self.lane, &encoded)?;
        self.bytes_written += encoded.len() as u64;
        self.block_count += 1;

        self.index_entries.push(IndexEntry {
            last_key,
            last_seqno,
            offset,
            size,
        });

        Ok(())
    }

    /// Finish writing: flush remaining entries, write index, bloom, meta, footer.
    /// Returns the SSTable metadata.
    pub fn finish(mut self) -> Result<Option<SSTableMeta>> {
        // Flush any remaining entries
        self.flush_data_block()?;

        if self.item_count == 0 {
            return Ok(None);
        }

        // --- Write index block ---
        let index_offset = self.bytes_written;
        let index_data = self.encode_index();
        Self::write_with_sched(&mut self.writer, &self.scheduler, self.lane, &index_data)?;
        self.bytes_written += index_data.len() as u64;

        // --- Write bloom filter ---
        let bloom_offset = self.bytes_written;
        let bloom_data = self.bloom_builder.finish();
        Self::write_with_sched(&mut self.writer, &self.scheduler, self.lane, &bloom_data)?;
        self.bytes_written += bloom_data.len() as u64;

        // --- Write meta block ---
        let meta_offset = self.bytes_written;
        let meta = SSTableMeta {
            table_id: self.table_id,
            block_count: self.block_count,
            item_count: self.item_count,
            key_min: self.key_min.unwrap_or_default(),
            key_max: self.key_max.unwrap_or_default(),
            seqno_min: self.seqno_min,
            seqno_max: self.seqno_max,
            compression: self.config.compression.to_u8(),
            format_version: FORMAT_VERSION,
            file_size: 0, // filled after footer
            tombstone_count: self.tombstone_count,
            zone_maps: Self::encode_zone_maps(&self.block_zone_maps),
        };
        let meta_data = meta.encode();
        Self::write_with_sched(&mut self.writer, &self.scheduler, self.lane, &meta_data)?;
        self.bytes_written += meta_data.len() as u64;

        // --- Write footer ---
        let footer = Footer {
            index_offset,
            bloom_offset,
            meta_offset,
        };
        // Instrument the footer write at the same lane. footer.encode
        // writes through self.writer; we account for the FOOTER_SIZE_V2
        // bytes via a single before_op/after_op pair around the call.
        self.scheduler.before_op(
            self.lane,
            crate::io::OpKind::Write {
                bytes: crate::table::meta::FOOTER_SIZE_V2 as u32,
            },
        );
        let footer_start = std::time::Instant::now();
        footer.encode(&mut self.writer)?;
        self.scheduler.after_op(
            self.lane,
            crate::io::OpKind::Write {
                bytes: crate::table::meta::FOOTER_SIZE_V2 as u32,
            },
            footer_start.elapsed().as_micros() as u64,
        );
        self.bytes_written += crate::table::meta::FOOTER_SIZE_V2 as u64;

        // Flush BufWriter → underlying File (kernel page cache on Unix).
        self.writer.flush()?;

        // fsync: force all written bytes to stable storage before publish.
        // Without this, `drop_page_cache(path)` in the compact worker
        // (posix_fadvise FADV_DONTNEED on Linux) could evict dirty pages
        // that haven't been written back, leaving the about-to-be-opened
        // SSTable file with truncated / zero bytes at arbitrary offsets.
        // The meta parser then reads partial values and returns
        // `Error::Corruption("bad X")`. See v0.2.1 Finding 4.
        self.scheduler
            .before_op(self.lane, crate::io::OpKind::Fsync);
        let fsync_start = std::time::Instant::now();
        let fsync_res = self.writer.get_ref().sync_all();
        self.scheduler.after_op(
            self.lane,
            crate::io::OpKind::Fsync,
            fsync_start.elapsed().as_micros() as u64,
        );
        fsync_res?;

        // Atomic publish: rename from `00042.sst.tmp` to `00042.sst`.
        // On POSIX local filesystems this is atomic; readers that
        // enumerate the directory (e.g. Tree::cleanup_orphan_ssts) or
        // open by final path (SSTableReader::open) see a complete file
        // or no file. Combined with fsync above, this closes both the
        // unsynced-page-evict and mid-write visibility failure modes.
        std::fs::rename(&self.tmp_path, &self.final_path)?;
        // 3g/q4: no directory fsync here. The SST shares its directory with the
        // MANIFEST, and the manifest rewrite that publishes this SST runs a
        // propagated `manifest::fsync_dir` (3g) which persists THIS rename too
        // (one dir-fsync covers all pending renames in that dir). An SST
        // renamed but not yet referenced by a durable manifest is an orphan,
        // cleaned up on open — never a corruption.

        let final_meta = SSTableMeta {
            file_size: self.bytes_written,
            ..meta
        };

        Ok(Some(final_meta))
    }

    /// Encode per-block zone maps into a single blob.
    /// Format: [block_count: u32 LE] [zm_len_0: u16 LE][zm_data_0] [zm_len_1: u16 LE][zm_data_1] ...
    ///
    /// Per-block zone maps are produced by `ZoneMapBuilder::build_block_zone_map`
    /// and are intentionally small — typically 16–64 bytes encoding min/max
    /// ranges per indexed field. The u16 length here covers per-block entries,
    /// NOT the aggregate blob written as meta tag 12 (which uses u32; see
    /// `SSTableMeta::encode`). Verified safe by construction: the builder
    /// interface is a fixed struct → varint serialization bounded by the
    /// number of indexed fields per ghost, which is small by design.
    fn encode_zone_maps(block_zone_maps: &[Vec<u8>]) -> Vec<u8> {
        if block_zone_maps.is_empty() {
            return Vec::new();
        }
        let mut buf =
            Vec::with_capacity(4 + block_zone_maps.iter().map(|z| z.len() + 2).sum::<usize>());
        buf.write_u32::<LittleEndian>(block_zone_maps.len() as u32)
            .unwrap();
        for zm in block_zone_maps {
            buf.write_u16::<LittleEndian>(zm.len() as u16).unwrap();
            buf.extend_from_slice(zm);
        }
        buf
    }

    /// Encode the index as a flat binary: [entry_count: u32][entries...]
    /// Each entry: [key_len: u32][key][seqno: u64][offset: u64][size: u32]
    fn encode_index(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(self.index_entries.len() * 64);
        buf.write_u32::<LittleEndian>(self.index_entries.len() as u32)
            .unwrap();

        for entry in &self.index_entries {
            // u32 length for index-entry key. User keys (dictionary anchor
            // values in particular) have no hard upper bound in xyzDB, so
            // the length must accommodate > 65 535 bytes. Paired with the
            // matching u32 read in `reader::decode_index`. v0.2.1 audit.
            buf.write_u32::<LittleEndian>(entry.last_key.len() as u32)
                .unwrap();
            buf.extend_from_slice(&entry.last_key);
            buf.write_u64::<LittleEndian>(entry.last_seqno).unwrap();
            buf.write_u64::<LittleEndian>(entry.offset).unwrap();
            buf.write_u32::<LittleEndian>(entry.size).unwrap();
        }

        buf
    }
}
