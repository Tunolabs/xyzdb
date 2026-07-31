//! The duplicate-anchor check must not be defeated by a post-recovery bloom.
//!
//! Why this exists, and why it does not wait for a diagnosis: the root of the
//! post-recovery bloom false-negative is still OPEN (see the internal analysis),
//! but its worst consequence does not depend on the root. The duplicate-anchor
//! check is a bloom-gated point-get; if it false-negatives, `PUT` writes a SECOND
//! record under a key declared UNIQUE — a duplicate ledger entry, silently, and
//! plausibly. The consumer of the uniqueness guarantee cannot detect any of it.
//!
//! The fix is cheap because the check is asymmetric. A false POSITIVE costs
//! nothing (the bloom emits those by design: the block is read, the key is not
//! there, the insert proceeds). Only the MISS branch needs armouring, and only
//! inside the window where the defect is reachable — a process that replayed WAL,
//! i.e. whose previous run did not shut down cleanly. There, a miss is confirmed
//! bloom-lessly before being trusted. Outside that window nothing is paid, which
//! matters because the common case of this check IS a legitimate miss.
//!
//! These tests forge the exact on-disk state an unclean crash produces (an
//! all-zero bloom bit-array with `num_bits > 0`, so `maybe_contains` answers false
//! for every key) and pin BOTH directions:
//!   - armed   → the duplicate is caught, the second insert is refused;
//!   - unarmed → the duplicate GETS THROUGH.
//! The second is a negative control. It asserts the bug on purpose: without it,
//! the first test could pass for reasons unrelated to the armouring, and a guard
//! that cannot be shown to be load-bearing is decoration.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::Ordering::Relaxed;
use turba_engine::engine::FORCE_RECOVERED_FROM_WAL;
use turba_engine::table::meta::{FOOTER_SIZE_V2, Footer};
use xyzdb_engine::engine::{Engine, QueryResult};

/// `FORCE_RECOVERED_FROM_WAL` is process-global, so these two tests must not
/// interleave: one arming while the other expects the unarmed path would make the
/// negative control read the neighbour's state. (Same trap already met by the
/// invariant-counter tests in turba.)
static ARMED_TESTS: Mutex<()> = Mutex::new(());

struct KnobReset;
impl Drop for KnobReset {
    fn drop(&mut self) {
        FORCE_RECOVERED_FROM_WAL.store(false, Relaxed);
    }
}

fn serial() -> (std::sync::MutexGuard<'static, ()>, KnobReset) {
    let g = ARMED_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    FORCE_RECOVERED_FROM_WAL.store(false, Relaxed);
    (g, KnobReset)
}

fn run(engine: &Engine, q: &str) -> Result<QueryResult, String> {
    engine.run(q).map_err(|e| format!("{q:?}: {e:?}"))
}

/// The single flushed SSTable under a keyspace directory.
fn find_sst(tree_path: &Path) -> PathBuf {
    let mut ssts: Vec<PathBuf> = std::fs::read_dir(tree_path)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            let n = p.to_string_lossy();
            n.ends_with(".sst") && !n.ends_with(".sst.tmp")
        })
        .collect();
    assert_eq!(ssts.len(), 1, "expected one flushed SSTable, got {ssts:?}");
    ssts.pop().unwrap()
}

/// Zero the bloom's bit array on disk, keeping the 5-byte trailer (`k` +
/// `num_bits`) so `num_bits > 0`: a well-formed, all-zero bloom whose
/// `maybe_contains` returns false for EVERY key. This is the bloom↔data
/// divergence the post-recovery defect produces, forged deterministically —
/// the live crash reproducer is flaky (11 events in one run, then 0 in ~66).
fn zero_bloom_bits(sst: &Path) {
    let mut f = OpenOptions::new().read(true).write(true).open(sst).unwrap();
    let len = f.metadata().unwrap().len();
    let read_len = FOOTER_SIZE_V2.min(len as usize);
    f.seek(SeekFrom::End(-(read_len as i64))).unwrap();
    let mut tail = vec![0u8; read_len];
    f.read_exact(&mut tail).unwrap();
    let (footer, _) = Footer::decode(&tail).unwrap();
    let bits_end = footer.meta_offset - 5;
    let n = (bits_end - footer.bloom_offset) as usize;
    assert!(n > 0, "bloom must have a non-empty bit array (got {n})");
    f.seek(SeekFrom::Start(footer.bloom_offset)).unwrap();
    f.write_all(&vec![0u8; n]).unwrap();
    f.sync_all().unwrap();
}

/// Build a lobe with a UNIQUE anchor holding one record, get the anchor entry
/// flushed to an SSTable, then forge that SSTable's bloom so every lookup in the
/// `dictionary` keyspace false-negatives. Returns the reopened engine.
fn engine_with_blinded_anchor_bloom(dir: &Path) -> Engine {
    {
        let engine = Engine::open(dir).unwrap();
        run(&engine, r#"LOBE "ledger""#).unwrap();
        run(&engine, r#"ANCHOR "txid" UNIQUE IN "ledger""#).unwrap();
        run(&engine, r#"PUT {txid: "TX-1", amount: 100} IN "ledger""#).unwrap();
        // The anchor lives in the `dictionary` keyspace; flush it so it sits in an
        // SSTable whose bloom can be forged (a memtable has no bloom to blind).
        //
        // Deliberately the plain seal+flush and NOT `COMPACT`: `major_compact`
        // pauses compaction while it runs, and `flush_sealed` writes
        // `bloom_bits_per_key = 0.0` whenever compaction is disabled — so a flush
        // taken during COMPACT produces an SSTable with NO bloom bits at all, which
        // cannot be blinded (there is nothing to zero). Verified the hard way: the
        // first draft used COMPACT and the forging step failed with an empty bloom.
        let dict = &engine.turba().dictionary;
        dict.seal_active();
        dict.flush_sealed().expect("flush dictionary");

        // Then shut down GRACEFULLY, and this step is load-bearing: a graceful
        // shutdown rotates the journal, so the reopen below replays nothing and the
        // anchor exists ONLY in the (about to be blinded) SSTable.
        //
        // Without it the reopen replays the WAL, the anchor lands back in the active
        // memtable, and `get_at` finds it there BEFORE any bloom is consulted — so
        // the duplicate gets caught with the bloom never involved. That is not a
        // theoretical worry: the first draft of these tests did exactly that, the
        // "armed" case passed for a reason unrelated to the armouring, and the
        // negative control is what exposed it.
        engine.turba().shutdown().expect("graceful shutdown");
    }
    zero_bloom_bits(&find_sst(&dir.join("dictionary")));
    Engine::open(dir).unwrap()
}

/// Armed (the process replayed WAL): a bloom that hides the existing anchor must
/// NOT let a second record be written under it.
#[test]
fn armed_after_recovery_the_duplicate_anchor_is_still_caught() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let engine = engine_with_blinded_anchor_bloom(dir.path());

    FORCE_RECOVERED_FROM_WAL.store(true, Relaxed);
    let err = run(&engine, r#"PUT {txid: "TX-1", amount: 999} IN "ledger""#)
        .expect_err("the duplicate anchor must be refused even with a blinded bloom");
    assert!(
        err.contains("DuplicateAnchor") || err.to_lowercase().contains("duplicate"),
        "expected a duplicate-anchor rejection, got: {err}"
    );

    // And the ledger still holds exactly one TX-1 — the point of the whole guard.
    let recs = match run(&engine, r#"SCAN "ledger""#).unwrap() {
        QueryResult::Records(r) => r,
        QueryResult::PaginatedRecords { records, .. } => records,
        other => panic!("unexpected: {other:?}"),
    };
    let tx1 = recs
        .iter()
        .filter(|r| r.fields.get("txid").and_then(|v| v.as_text()) == Some("TX-1"))
        .count();
    assert_eq!(tx1, 1, "exactly one TX-1 must exist, found {tx1}");
}

/// NEGATIVE CONTROL — unarmed (no recovery window): the same forged state lets the
/// duplicate through. This asserts the DEFECT deliberately, to prove the armouring
/// above is what prevents it. If this test ever starts failing, the exposure closed
/// by some other means and the armouring's justification must be re-checked, not
/// the test relaxed.
#[test]
fn unarmed_the_blinded_bloom_lets_a_duplicate_anchor_through() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let engine = engine_with_blinded_anchor_bloom(dir.path());

    // Not armed: the miss is trusted, exactly as it was before this change.
    FORCE_RECOVERED_FROM_WAL.store(false, Relaxed);
    run(&engine, r#"PUT {txid: "TX-1", amount: 999} IN "ledger""#)
        .expect("unarmed, the blinded bloom makes the duplicate look absent");

    let recs = match run(&engine, r#"SCAN "ledger""#).unwrap() {
        QueryResult::Records(r) => r,
        QueryResult::PaginatedRecords { records, .. } => records,
        other => panic!("unexpected: {other:?}"),
    };
    let tx1 = recs
        .iter()
        .filter(|r| r.fields.get("txid").and_then(|v| v.as_text()) == Some("TX-1"))
        .count();
    assert_eq!(
        tx1, 2,
        "negative control: unarmed, the unique anchor must have been DUPLICATED \
         (found {tx1}). If this is 1, the armouring is not what catches the \
         duplicate and the armed test proves nothing."
    );
}
