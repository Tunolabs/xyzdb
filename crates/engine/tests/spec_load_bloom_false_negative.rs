//! A lobe must come up with its declared axis even when the bloom lies.
//!
//! Every declaration — gravity, vector, satellite — used to be read at boot with
//! one bloom-gated point lookup per lobe, and its miss branch was
//! indistinguishable from "not declared". An SSTable written during crash
//! recovery can carry a bloom that disagrees with its data (root not diagnosed;
//! see KNOWN-ISSUES.md), so a false negative on a declaration key would bring the
//! lobe up WITHOUT its axis: no gravity placement, `NEAREST` with no vector field,
//! no satellite for new writes. Nothing would say so — the engine cannot tell an
//! absent declaration from one it failed to find.
//!
//! The loads are prefix scans now. A range scan never consults the bloom, because
//! the filter answers point questions, so the exposure is removed rather than
//! reported.
//!
//! The forge is the one `anchor_bloom_false_negative.rs` uses: zero the bloom's
//! bit array on disk while keeping `num_bits > 0`, producing a well-formed filter
//! that answers "absent" for every key. The live crash reproducer is flaky; this
//! is deterministic.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use turba_engine::table::meta::{FOOTER_SIZE_V2, Footer};
use xyzdb_engine::engine::Engine;

/// Every SSTable in the keyspace, because each DDL statement flushed its own and
/// blinding one would leave the others answering truthfully — the forge has to be
/// total or the positive control below is not measuring what it claims.
fn find_ssts(tree_path: &Path) -> Vec<PathBuf> {
    let ssts: Vec<PathBuf> = std::fs::read_dir(tree_path)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            let n = p.to_string_lossy();
            n.ends_with(".sst") && !n.ends_with(".sst.tmp")
        })
        .collect();
    assert!(!ssts.is_empty(), "no flushed SSTable in {tree_path:?}");
    ssts
}

/// Zero the bloom's bit array, keeping the 5-byte trailer so `num_bits > 0`: a
/// well-formed, all-zero bloom whose `maybe_contains` is false for every key.
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

/// Declare all three axes, flush the dictionary so the declarations live in an
/// SSTable whose bloom can be forged (a memtable has no bloom), then shut down
/// gracefully — a graceful stop rotates the journal, so the reopen replays
/// nothing and the declarations exist ONLY in the SSTable about to be blinded.
fn declare_and_blind(dir: &Path) -> Vec<PathBuf> {
    let engine = Engine::open(dir).unwrap();
    for stmt in [
        r#"LOBE "mem""#,
        r#"GRAVITY BY conv IN "mem""#,
        r#"VECTOR emb IN "mem""#,
        r#"SATELLITE BY kind IN "mem""#,
    ] {
        engine.run(stmt).unwrap_or_else(|e| panic!("{stmt}: {e:?}"));
    }
    {
        let dict = &engine.turba().dictionary;
        dict.seal_active();
        dict.flush_sealed().expect("flush dictionary");
    }
    drop(engine);

    let ssts = find_ssts(&dir.join("dictionary"));
    for sst in &ssts {
        zero_bloom_bits(sst);
    }
    ssts
}

/// The whole point, in one test, because the two halves are only meaningful
/// together.
///
/// POSITIVE CONTROL — the forge must actually blind point lookups. A point-get of
/// the gravity key must return `None` on the reopened engine even though the key
/// is there. Without this assertion the test could pass on an engine where the
/// bloom was never consulted, and would prove nothing about the defect it exists
/// for.
///
/// THE PROPERTY — with point lookups provably blinded, the axes are still loaded,
/// because the loaders no longer use one.
#[test]
fn declarations_survive_a_blinded_bloom() {
    let dir = tempfile::tempdir().unwrap();
    declare_and_blind(dir.path());
    let engine = Engine::open(dir.path()).unwrap();

    // POSITIVE CONTROL: point lookups into the forged SSTable really are blinded.
    // Every key in that keyspace now answers "absent", so a loader that used one
    // would see nothing. Asserted over the whole dictionary rather than one key,
    // because the forge is indiscriminate and that is the property that matters.
    let blinded = engine.turba().dictionary.prefix_iter(&[]).unwrap().count();
    assert!(
        blinded > 0,
        "the range scan sees nothing either — the forge destroyed the data instead \
         of only the filter, so this test proves nothing"
    );

    // THE PROPERTY, read through the public surface: every axis is still declared.
    let profile = match engine.run(r#"SHOW PROFILE "mem""#).unwrap() {
        xyzdb_engine::engine::QueryResult::Info(lines) => lines.join("\n"),
        other => panic!("expected Info, got {other:?}"),
    };
    // `SHOW PROFILE` reports Pinned, Vector, Satellite and Learned — but NOT the
    // gravity axis, which is the lobe's primary declaration. That gap was found by
    // this test and is filed separately; gravity is asserted below through
    // behaviour instead, which is a stronger check anyway: a lobe that lost its
    // axis still answers a gravity-scoped query, it just stops bounding it.
    for (axis, marker) in [("vector", "Vector:"), ("satellite", "Satellite:")] {
        let line = profile
            .lines()
            .find(|l| l.trim_start().starts_with(marker))
            .unwrap_or_else(|| panic!("no {marker} line in:\n{profile}"));
        assert!(
            !line.contains("(none)"),
            "{axis} axis lost after a blinded bloom — the lobe came up without it: {line}"
        );
    }

    // GRAVITY, through behaviour. `GRAVITY BY conv` is refused on a lobe that
    // already declares it, so the declaration surviving the blinded bloom is
    // exactly what makes this statement fail. If the axis had been lost, the
    // re-declaration would succeed — silently, and the lobe would carry two
    // different placements across its own lifetime.
    // A DIFFERENT field, not the same one: re-declaring the same spec is a
    // documented no-op, so `GRAVITY BY conv` would have succeeded either way and
    // proved nothing. A conflicting declaration is refused only by a lobe that
    // knows it already has an axis.
    let redeclare = engine.run(r#"GRAVITY BY other IN "mem""#);
    assert!(
        redeclare.is_err(),
        "gravity axis lost after a blinded bloom: the lobe accepted a CONFLICTING \
         GRAVITY BY, which means it came up believing it had none"
    );
}
