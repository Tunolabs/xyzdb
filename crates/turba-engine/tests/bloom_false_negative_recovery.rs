//! Regression teeth for the crash-recovery read-path fix (fix A).
//!
//! The defect: after an unclean crash, a post-recovery SSTable can carry a bloom
//! that disagrees with its data. The bloom-gated point-get ([`Tree::get`]) then
//! false-negatives a key the bloom-less scan still sees — surfacing as the NEAREST
//! hydration's "survivor key vanished". Durability is intact (measured: 0 ack'd loss
//! across many crashes); only the read path errs, and it self-heals once compaction
//! rewrites the table.
//!
//! The live crash race is rare and not reliably reproducible, so this test forges
//! the exact bloom↔data divergence deterministically: flush a key into an SSTable
//! that carries a real bloom, then zero the on-disk bloom bit-array (keeping
//! `num_bits > 0`, so `maybe_contains` returns false for every key). [`Tree::get`]
//! then false-negatives the key, while [`Tree::get_no_bloom`] — the fix's fallback,
//! which never consults the bloom — recovers it. That co-observation (same key:
//! bloom-gated get misses, bloom-less get finds) is the read-path/not-durability
//! proof and the permanent gate for the `ops/nearest` hydration fallback.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use turba_engine::cache::BlockCache;
use turba_engine::compaction::leveled::LeveledConfig;
use turba_engine::compression::CompressionType;
use turba_engine::table::meta::{FOOTER_SIZE_V2, Footer};
use turba_engine::table::writer::SSTableConfig;
use turba_engine::tree::{Tree, TreeConfig};

fn test_config() -> TreeConfig {
    TreeConfig {
        sstable: SSTableConfig {
            compression: CompressionType::Lz4,
            data_block_size: 4096,
            ..Default::default() // bloom_bits_per_key = 10.0 — a real bloom
        },
        max_memtable_size: 32 * 1024,
        compaction: LeveledConfig {
            max_l0_tables: 4,
            base_size_bytes: 64 * 1024,
            size_ratio: 10,
            ..Default::default()
        },
        level_compressions: None,
    }
}

/// The single visible `.sst` under `tree_path` (a visible `.sst` is always complete;
/// `.sst.tmp` writer debris is excluded).
fn find_sst(tree_path: &Path) -> PathBuf {
    let mut ssts: Vec<PathBuf> = std::fs::read_dir(tree_path)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            let n = p.to_string_lossy();
            n.ends_with(".sst") && !n.ends_with(".sst.tmp")
        })
        .collect();
    assert_eq!(
        ssts.len(),
        1,
        "expected exactly one flushed SSTable, got {ssts:?}"
    );
    ssts.pop().unwrap()
}

/// Zero the bloom's bit array on disk, keeping the 5-byte trailer (`k` + `num_bits`)
/// so `num_bits > 0`: a well-formed, all-zero bloom whose `maybe_contains` returns
/// false for every key. Forges the bloom↔data divergence an unclean crash produces.
fn zero_bloom_bits(sst: &Path) {
    let mut f = OpenOptions::new().read(true).write(true).open(sst).unwrap();
    let len = f.metadata().unwrap().len();
    let read_len = FOOTER_SIZE_V2.min(len as usize);
    f.seek(SeekFrom::End(-(read_len as i64))).unwrap();
    let mut tail = vec![0u8; read_len];
    f.read_exact(&mut tail).unwrap();
    let (footer, _) = Footer::decode(&tail).unwrap();

    // On-disk bloom layout: [bits ..][k: u8][num_bits: u32 LE]. Zero only the bits.
    let bits_end = footer.meta_offset - 5;
    let n = (bits_end - footer.bloom_offset) as usize;
    assert!(
        n > 0,
        "bloom must have a non-empty bit array to corrupt (got {n})"
    );
    f.seek(SeekFrom::Start(footer.bloom_offset)).unwrap();
    f.write_all(&vec![0u8; n]).unwrap();
    f.sync_all().unwrap();
}

#[test]
fn get_no_bloom_recovers_key_a_false_negative_bloom_hides() {
    let dir = tempfile::tempdir().unwrap();
    let tree_path = dir.path().join("tree");
    let key = b"the_survivor_key".as_slice();
    let value = b"survivor_value_payload".as_slice();

    // 1. Flush the key into an SSTable that carries a correct bloom.
    {
        let cache = Arc::new(BlockCache::new(64 * 1024 * 1024));
        let tree = Tree::open(&tree_path, test_config(), cache).unwrap();
        for i in 0..200u32 {
            tree.insert(format!("filler_{i:05}").as_bytes(), b"x")
                .unwrap();
        }
        tree.insert(key, value).unwrap();
        tree.seal_active();
        tree.flush_sealed().unwrap();
        // With the correct bloom, the bloom-gated get finds the key (mechanism sane).
        assert_eq!(tree.get(key).unwrap().as_deref(), Some(value));
    }

    // 2. Forge the crash-recovery defect: an all-zero bloom (still num_bits > 0).
    zero_bloom_bits(&find_sst(&tree_path));

    // 3. Re-open fresh: a raw Tree has no WAL, so the key lives ONLY in the SSTable,
    //    and a fresh block cache re-reads the corrupted bloom from disk.
    let cache = Arc::new(BlockCache::new(64 * 1024 * 1024));
    let tree = Tree::open(&tree_path, test_config(), cache).unwrap();

    // 4. Defect reproduced deterministically: the bloom-gated get false-negatives
    //    the present key (bloom says "absent" though the data block holds it).
    assert_eq!(
        tree.get(key).unwrap(),
        None,
        "precondition: a false-negative bloom must hide the key from the bloom-gated get",
    );

    // 5. The FIX — co-observation, read-path NOT durability: the bloom-less fallback
    //    recovers the SAME key the bloom-gated get missed. A `Some` here proves the
    //    data was in the store all along (the SSTable was re-opened and read), so the
    //    vanish is a read-path miss, not a durability loss.
    assert_eq!(
        tree.get_no_bloom(key).unwrap().as_deref(),
        Some(value),
        "fix A: get_no_bloom must recover a key a false-negative bloom hides from get",
    );
}
