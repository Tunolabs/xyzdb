//! Table ids stay monotonic across restarts, not just within a process.
//!
//! `next_table_id` is only made durable by `persist_manifest`, which runs AFTER
//! the tables are installed in the live Version, and not at all while compaction
//! is disabled. So a bulk load that exits without a major compaction leaves
//! `NNNNNN.sst` on disk while the manifest still says `next_table_id = NNNNNN` —
//! and since orphan `.sst` files are not swept at open (only `.sst.tmp` is), the
//! next flush used to mint the same id and write the SAME filename.
//!
//! No crash is needed to reproduce it, which is what makes this a cheap test
//! instead of a race: BULKMODE skips the manifest on flush, `shutdown()` skips the
//! journal rotation while compaction is disabled, and open takes the id from the
//! manifest alone.
//!
//! Nothing was silently corrupting: a reusable id is by construction absent from
//! every persisted manifest, so it is never live in a Version and the caches —
//! keyed by `(tree_id, table_id)` with no generation — cannot serve one table's
//! blocks for another. This pins the invariant anyway, because that safety rested
//! on an argument rather than on a rule, and a reused identity is a hazard for
//! everything keyed by it (block cache, meta cache, orphan cleanup, FlushIdGuard).

// SPDX-License-Identifier: BUSL-1.1
use std::path::Path;
use std::sync::Arc;
use turba_engine::cache::BlockCache;
use turba_engine::compaction::leveled::LeveledConfig;
use turba_engine::compression::CompressionType;
use turba_engine::table::writer::SSTableConfig;
use turba_engine::tree::{Tree, TreeConfig};

fn config() -> TreeConfig {
    TreeConfig {
        sstable: SSTableConfig {
            compression: CompressionType::None,
            ..Default::default()
        },
        max_memtable_size: 32 * 1024,
        compaction: LeveledConfig::default(),
        level_compressions: None,
    }
}

fn open(dir: &Path) -> Tree {
    Tree::open(dir, config(), Arc::new(BlockCache::new(1 << 20))).expect("open")
}

/// Ids of the `NNNNNN.sst` files currently on disk, ascending.
fn sst_ids(dir: &Path) -> Vec<u64> {
    let mut ids: Vec<u64> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let n = e.file_name();
            n.to_str()?.strip_suffix(".sst")?.parse::<u64>().ok()
        })
        .collect();
    ids.sort_unstable();
    ids
}

/// Write + flush with compaction DISABLED, so no manifest is persisted: the SST
/// lands on disk while the manifest keeps pointing at its id.
fn flush_without_manifest(dir: &Path, keys: &[&str]) {
    let tree = open(dir);
    tree.set_compaction_enabled(false);
    for k in keys {
        tree.insert(k.as_bytes(), b"v").expect("write");
    }
    tree.seal_active();
    tree.flush_sealed().expect("flush");
}

#[test]
fn a_flush_after_an_unpersisted_manifest_does_not_reuse_the_id() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();

    flush_without_manifest(path, &["a", "b"]);
    let first = sst_ids(path);
    assert_eq!(first.len(), 1, "expected one orphan SST, got {first:?}");

    // Reopen: the manifest never recorded the flush above, so before the fix the
    // id counter still pointed at `first[0]` and the next flush overwrote it.
    flush_without_manifest(path, &["c", "d"]);
    let after = sst_ids(path);

    assert_eq!(
        after.len(),
        2,
        "the second flush must create a NEW file, not overwrite the orphan \
         (ids on disk: {after:?}) — a reused id means one identity for two \
         different table contents, and every cache keyed by (tree_id, table_id) \
         would then be able to serve the wrong bytes"
    );
    assert!(
        after[1] > after[0],
        "ids must be monotonic across restarts: {after:?}"
    );
}

/// The reconciliation must not renumber anything on a normal reopen: a clean
/// manifest already carries an id greater than everything it lists, so the `max`
/// is a no-op there. Without this, the fix could "pass" by inflating ids on every
/// open, quietly burning the id space.
#[test]
fn a_clean_reopen_does_not_advance_the_id() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();

    {
        let tree = open(path);
        for i in 0..50 {
            tree.insert(format!("k{i:04}").as_bytes(), b"v")
                .expect("write");
        }
        tree.seal_active();
        tree.flush_sealed().expect("flush"); // compaction enabled ⇒ manifest persisted
    }
    let before = sst_ids(path);
    assert!(!before.is_empty());

    // Reopen and flush nothing: no new file may appear, and the next id must be
    // exactly one past the highest existing one rather than jumping.
    {
        let tree = open(path);
        tree.insert(b"tail", b"v").expect("write");
        tree.seal_active();
        tree.flush_sealed().expect("flush");
    }
    let after = sst_ids(path);
    assert_eq!(
        after.len(),
        before.len() + 1,
        "exactly one new SST expected (before {before:?}, after {after:?})"
    );
    assert_eq!(
        *after.last().unwrap(),
        before.last().unwrap() + 1,
        "the new id must follow the highest existing one with no gap \
         (before {before:?}, after {after:?})"
    );
}
