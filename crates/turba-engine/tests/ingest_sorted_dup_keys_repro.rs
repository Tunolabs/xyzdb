//! Repro for the Q5-empty-at-scale ghost bug.
//!
//! The ghost build (`GhostLobeManager::create`) streams matching records into
//! the ghost keyspace via repeated `ingest_sorted` calls — one per ~8 MB buffer
//! ("chunk"). For an aggregate ghost whose `ORDER BY` field == `GROUP BY` field
//! (the bench's `overdue_by_empresa`), EVERY record of one group encodes to the
//! SAME sort_key, so a group's records are DUPLICATE keys. At scale a group's
//! records split across multiple chunks ⇒ the same key lands in multiple L0
//! SSTs; within a chunk the entries are sorted by user-key ONLY (not seqno), so
//! duplicate keys sit in arbitrary seqno order inside one SST. A final
//! `major_compact` then merges them.
//!
//! At small scale (Scale 0.002) everything fits in ONE chunk, so this
//! multi-SST + arbitrary-seqno-order path never runs and the ghost reads fine
//! (Q5 = 80 rows). At Scale 1.0 it does run and Q5 comes back EMPTY. This test
//! reproduces that exact shape at miniature scale by calling `ingest_sorted`
//! several times with duplicate keys in scan order, then compacting and reading
//! back — no 8 MB of data needed.

use std::sync::Arc;
use turba_engine::cache::BlockCache;
use turba_engine::compression::CompressionType;
use turba_engine::table::writer::SSTableConfig;
use turba_engine::tree::{Tree, TreeConfig};
use turba_engine::types::{Entry, ValueType};

fn tree(dir: &std::path::Path) -> Tree {
    let cache = Arc::new(BlockCache::new(16 * 1024 * 1024));
    let config = TreeConfig {
        sstable: SSTableConfig {
            compression: CompressionType::Lz4,
            data_block_size: 4096,
            ..Default::default()
        },
        max_memtable_size: 8 << 20, // 8 MB — same as the ghost keyspace
        compaction: Default::default(),
        level_compressions: None,
    };
    Tree::open(&dir.join("ghosts"), config, cache).unwrap()
}

/// One "chunk" the build would flush: entries sorted by USER KEY only (exactly
/// `entry_buf.sort_by(|a,b| a.key.cmp(&b.key))` in ghost.rs), duplicates kept.
fn ingest_chunk(t: &Tree, mut entries: Vec<Entry>) {
    entries.sort_by(|a, b| a.key.cmp(&b.key)); // user-key only — mirrors the build
    t.ingest_sorted(entries.into_iter()).unwrap();
}

fn e(key: &str, val: &str, seqno: u64) -> Entry {
    Entry {
        key: key.as_bytes().to_vec(),
        value: val.as_bytes().to_vec(),
        seqno,
        value_type: ValueType::Value,
    }
}

#[test]
fn aggregate_ghost_survives_multi_chunk_duplicate_keys() {
    let dir = tempfile::tempdir().unwrap();
    let t = tree(dir.path());

    // 5 groups ("empresas"), each appearing across THREE chunks (its records
    // split across buffer flushes). Global, increasing seqno in scan order —
    // exactly `seqno_base + index_count + 1`. Within a chunk, several records of
    // the same group → duplicate keys; the chunk is sorted by key only, so those
    // duplicates sit in scan-order (not seqno-order).
    let groups = ["emp0", "emp1", "emp2", "emp3", "emp4"];
    let mut seqno = 1u64;

    for _chunk in 0..3 {
        let mut batch = Vec::new();
        // Two records per group per chunk → duplicate keys within the chunk.
        for rep in 0..2 {
            for g in groups {
                batch.push(e(g, &format!("v{seqno}_{rep}"), seqno));
                seqno += 1;
            }
        }
        ingest_chunk(&t, batch);
    }

    // The build ends with a final ghost-keyspace major_compact.
    t.major_compact().unwrap();

    // Read every distinct group back (this is what SCAN GHOST / read_topn does
    // via prefix_iter over the ghost_id prefix). Each must be present — empty is
    // the Q5 bug.
    let found: Vec<String> = t
        .prefix_iter(b"emp")
        .unwrap()
        .map(|entry| String::from_utf8_lossy(&entry.key).into_owned())
        .collect();
    let distinct: std::collections::BTreeSet<String> = found.iter().cloned().collect();

    eprintln!(
        "prefix_iter returned {} entries, distinct keys = {distinct:?}",
        found.len()
    );
    assert_eq!(
        distinct.len(),
        groups.len(),
        "every group must be readable after multi-chunk ingest + compact; \
         got {distinct:?} (empty/short = the Q5-at-scale bug)"
    );

    // And a point read must return the LATEST value per group, not nothing.
    for g in groups {
        let v = t.get(g.as_bytes()).unwrap();
        assert!(v.is_some(), "group {g} must be readable, got None (Q5 bug)");
    }
}
