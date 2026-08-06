//! A ghost-routed read must not lose rows when the bloom lies.
//!
//! `ghost/read.rs` resolves each ghost entry with a point-read on the `spatial`
//! keyspace, and its miss branch is `_ => continue`: the record is skipped. Every
//! point lookup is bloom-gated, and a post-recovery SSTable can carry a bloom that
//! disagrees with its data (KNOWN-ISSUES.md), so a false negative there **drops
//! rows from a result with no error and no flag** — the query simply returns fewer.
//!
//! It is the sharpest of the remaining exposures for a second reason: ghosts are
//! materialised by the engine itself from scan telemetry, so the exposure can
//! appear without anyone writing a query differently.
//!
//! This test measures whether that is reachable, so the public file can say
//! "demonstrated" or "not measured" rather than guessing between them.
//!
//! IT DOES NOT MEASURE IT YET, and is `#[ignore]`d for that reason rather than
//! deleted. Its first version passed — and the pass was worthless: the
//! discriminator added afterwards shows the forge never blinded a point lookup on
//! `spatial`. The likely cause is that the reopen replays the journal, so the
//! records come back through the memtable, which has no bloom to blind. The
//! dictionary test avoids this because a graceful shutdown rotates the journal
//! after the DDL; the same has to be arranged here for record writes.
//!
//! Left in place with both discriminators armed, because the next person to touch
//! this needs the trap, not a blank page. Until it runs, `ghost/read.rs` is "not
//! measured" in KNOWN-ISSUES.md — which is not the same as safe.

// SPDX-License-Identifier: BUSL-1.1
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use turba_engine::table::meta::{FOOTER_SIZE_V2, Footer};
use xyzdb_engine::engine::{Engine, QueryResult};

fn find_ssts(tree_path: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(tree_path)
        .map(|rd| {
            rd.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    let n = p.to_string_lossy();
                    n.ends_with(".sst") && !n.ends_with(".sst.tmp")
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Zero the bloom's bit array, keeping the trailer so `num_bits > 0`: every
/// `maybe_contains` answers false while the data stays intact.
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
    if n == 0 {
        return; // nothing to blind in this table
    }
    f.seek(SeekFrom::Start(footer.bloom_offset)).unwrap();
    f.write_all(&vec![0u8; n]).unwrap();
    f.sync_all().unwrap();
}

const N: usize = 60;

#[test]
#[ignore = "needs records that live ONLY in an SSTable: the close must rotate the \
            journal (a graceful shutdown that seals, flushes and leaves the manifest \
            durable), not a drop or a kill — otherwise the reopen replays and \
            repopulates the memtable, which has no bloom to blind"]
fn a_ghost_routed_read_keeps_its_rows_under_a_blinded_bloom() {
    let dir = tempfile::tempdir().unwrap();
    let spatial_dir = dir.path().join("spatial");

    {
        let e = Engine::open(dir.path()).unwrap();
        e.run(r#"LOBE "g""#).unwrap();
        for i in 0..N {
            e.run(&format!(
                r#"PUT {{_type:"R", id:"g{i}", x:{i}, tag:"t"}} IN "g""#
            ))
            .unwrap();
        }
        // A covering ordered ghost so the filter SCAN routes through the ghost's
        // point-read path rather than the primary scan.
        e.run(r#"CREATE GHOST "gc" FROM "g" ORDER BY x"#).unwrap();
        // The records must live in an SSTable whose bloom can be forged — a
        // memtable has no bloom at all.
        let sp = &e.turba().spatial;
        sp.seal_active();
        sp.flush_sealed().expect("flush spatial");
    }

    let ssts = find_ssts(&spatial_dir);
    assert!(
        !ssts.is_empty(),
        "no spatial SSTable to forge in {spatial_dir:?}"
    );
    for sst in &ssts {
        zero_bloom_bits(sst);
    }

    let e = Engine::open(dir.path()).unwrap();

    // DISCRIMINATOR 1 — the forge must have blinded real point lookups. A direct
    // point-get on a key that exists must come back absent; if it does not, the
    // records are being served from somewhere the bloom never gated and this test
    // is measuring nothing.
    let probe = e.turba().spatial.prefix_iter(&[]).unwrap().next();
    let probe_key = probe.expect("spatial has entries").key;
    assert!(
        e.turba().spatial.get(&probe_key).unwrap().is_none(),
        "the forge did not blind point lookups on `spatial` — the green below \
         would prove nothing about the defect this test exists for"
    );

    // DISCRIMINATOR 2 — the query must actually route through the ghost. Without
    // this the scan could be answered by the primary path, which does not use the
    // point-read under test.
    let ghosts = match e.run("SHOW GHOSTS").unwrap() {
        QueryResult::Info(l) => l.join(" "),
        other => panic!("expected Info, got {other:?}"),
    };
    assert!(
        ghosts.contains("gc"),
        "the ghost is not registered, so the read cannot have routed through it: \
         {ghosts}"
    );

    let rows = match e.run(r#"SCAN "g" WHERE tag="t""#).unwrap() {
        QueryResult::Records(r) => r.len(),
        QueryResult::PaginatedRecords { records, .. } => records.len(),
        other => panic!("expected records, got {other:?}"),
    };
    assert_eq!(
        rows, N,
        "a ghost-routed read lost rows under a blinded bloom: {rows} of {N} came \
         back, with no error and no flag"
    );
}
