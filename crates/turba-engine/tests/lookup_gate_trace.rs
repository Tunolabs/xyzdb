//! The gate discriminator: which door decided a point lookup.
//!
//! Built because a test that asserts only on the RESULT can pass without ever
//! touching the gate it targets. That happened here: a duplicate-anchor test with
//! a deliberately blinded bloom passed green because reopening replayed the WAL,
//! the key was back in the active memtable, and `get_at` answers from there BEFORE
//! any bloom is consulted. The armouring under test was never exercised.
//!
//! So this is not only forensic equipment for a rare event — it is the primitive
//! that lets a gate-targeted test state its precondition instead of hoping for it,
//! and it would have caught that false green automatically.
//!
//! It also names the two ways a point read can miss a key a SCAN still finds:
//!   - an L1+ positional miss over a table that does cover the key (an unsorted or
//!     overlapping level: the state the engine's guard calls "may silently miss
//!     present keys", historically this symptom with no bloom involved);
//!   - a bloom false negative (bloom-gated read absent, bloom-less read present).

use std::path::Path;
use std::sync::Arc;
use turba_engine::cache::BlockCache;
use turba_engine::compaction::leveled::LeveledConfig;
use turba_engine::compression::CompressionType;
use turba_engine::table::writer::SSTableConfig;
use turba_engine::tree::{LookupGate, Tree, TreeConfig};

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

/// A key still in the active memtable is answered THERE — before any bloom. This
/// is the exact confusion that made a bloom test pass without touching a bloom.
#[test]
fn a_memtable_hit_is_attributed_to_the_memtable_not_a_bloom() {
    let dir = tempfile::tempdir().unwrap();
    let tree = open(dir.path());
    tree.insert(b"k1", b"v1").expect("write");

    let (val, trace) = tree.get_at_traced(b"k1", tree.current_seqno()).unwrap();
    assert_eq!(val.as_deref(), Some(&b"v1"[..]));
    assert_eq!(
        trace.decided_by,
        Some(LookupGate::ActiveMemtable { tombstone: false }),
        "a test that meant to exercise the bloom would have to see this and fail"
    );
    assert!(trace.is_clean(), "no anomaly expected: {trace:?}");
}

/// After a flush the same key is answered by an SSTable, so a bloom IS on the path.
/// Pinning both sides is what makes the discriminator usable as a precondition:
/// a bloom-targeted test asserts `Table`, and fails loudly if a memtable shadowed it.
#[test]
fn after_a_flush_the_same_key_is_attributed_to_a_table() {
    let dir = tempfile::tempdir().unwrap();
    let tree = open(dir.path());
    tree.insert(b"k1", b"v1").expect("write");
    tree.seal_active();
    tree.flush_sealed().expect("flush");

    let (val, trace) = tree.get_at_traced(b"k1", tree.current_seqno()).unwrap();
    assert_eq!(val.as_deref(), Some(&b"v1"[..]));
    match trace.decided_by {
        Some(LookupGate::Table {
            level, tombstone, ..
        }) => {
            assert!(!tombstone);
            assert_eq!(level, 0, "a fresh flush lands in L0");
        }
        other => panic!("expected a table gate, got {other:?}"),
    }
    assert!(trace.is_clean(), "healthy tree: {trace:?}");
}

/// A genuinely absent key reports `NotFound` and NO anomaly. Without this, a clean
/// trace would be indistinguishable from a trace the instrument failed to fill —
/// the same "did not happen vs did not look" problem the invariant counters solve.
#[test]
fn a_genuine_absence_is_not_reported_as_an_anomaly() {
    let dir = tempfile::tempdir().unwrap();
    let tree = open(dir.path());
    for i in 0..200 {
        tree.insert(format!("k{i:04}").as_bytes(), b"v")
            .expect("write");
    }
    tree.seal_active();
    tree.flush_sealed().expect("flush");

    let (val, trace) = tree.get_at_traced(b"absent", tree.current_seqno()).unwrap();
    assert!(val.is_none());
    assert_eq!(trace.decided_by, Some(LookupGate::NotFound));
    assert!(
        trace.is_clean(),
        "a legitimate miss must NOT look like a bloom false negative or a \
         positional miss, or every real diagnosis would drown in noise: {trace:?}"
    );
}

/// A tombstone is an answer, not an absence: the gate must say which door produced
/// it, so "deleted" is never mistaken for "the bloom lost it".
#[test]
fn a_tombstone_is_attributed_and_marked() {
    let dir = tempfile::tempdir().unwrap();
    let tree = open(dir.path());
    tree.insert(b"k1", b"v1").expect("write");
    tree.remove(b"k1").expect("write");

    let (val, trace) = tree.get_at_traced(b"k1", tree.current_seqno()).unwrap();
    assert!(val.is_none(), "a tombstoned key reads as absent");
    assert_eq!(
        trace.decided_by,
        Some(LookupGate::ActiveMemtable { tombstone: true }),
        "the absence must be attributed to a tombstone, not to a missing key"
    );
    assert!(trace.is_clean());
}

/// The traced and untraced paths must agree on the ANSWER — they share one
/// implementation precisely so the traced view cannot drift from the real one.
#[test]
fn tracing_does_not_change_the_answer() {
    let dir = tempfile::tempdir().unwrap();
    let tree = open(dir.path());
    for i in 0..500 {
        tree.insert(format!("k{i:04}").as_bytes(), format!("v{i}").as_bytes())
            .expect("write");
    }
    tree.seal_active();
    tree.flush_sealed().expect("flush");
    for i in 500..700 {
        tree.insert(format!("k{i:04}").as_bytes(), format!("v{i}").as_bytes())
            .expect("write");
    }

    let seq = tree.current_seqno();
    for probe in [
        "k0000", "k0250", "k0499", "k0600", "k0699", "absent", "k9999",
    ] {
        let plain = tree.get_at(probe.as_bytes(), seq).unwrap();
        let (traced, _) = tree.get_at_traced(probe.as_bytes(), seq).unwrap();
        assert_eq!(plain, traced, "traced answer diverged for {probe}");
    }
}
