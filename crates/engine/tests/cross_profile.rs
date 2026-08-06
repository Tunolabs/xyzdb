//! 5c — cross-profile portability.
//!
//! The storage profile (Hdd/Ssd) is an explicit, settable override
//! (`open_with_config(.., Some(profile))`; server `--storage-profile`; no
//! auto-detect). It tunes write-time block sizing + runtime knobs, NOT the
//! on-disk format: every SST is self-describing (per-block header with sizes +
//! checksums, plus an index of block offsets), so a database written under one
//! profile must read back identically under the other. This is required for a
//! profile migration (write HDD, later serve from SSD, and vice-versa).
//!
//! Teeth: the test reads the actual records back across the profile switch and
//! checks field values + SCRUB. A profile-dependent parse (e.g. assuming the
//! open-time block size) would corrupt fields or trip a block checksum.

// SPDX-License-Identifier: BUSL-1.1
use xyzdb_core::record::Record;
use xyzdb_engine::engine::{Engine, QueryResult};
use xyzdb_engine::keyspaces::StorageProfile;
use xyzdb_engine::throttle::ThrottleConfig;

fn open(dir: &std::path::Path, profile: StorageProfile) -> Engine {
    Engine::open_with_config(dir, ThrottleConfig::default(), None, Some(profile))
        .unwrap_or_else(|e| panic!("open {profile:?}: {e:?}"))
}

fn run(engine: &Engine, s: &str) -> QueryResult {
    engine.run(s).unwrap_or_else(|e| panic!("{s:?}: {e:?}"))
}

fn records(r: QueryResult) -> Vec<Record> {
    match r {
        QueryResult::Records(v) => v,
        QueryResult::PaginatedRecords { records, .. } => records,
        other => panic!("expected Records, got {other:?}"),
    }
}

fn put_batch(engine: &Engine, lo: usize, hi: usize) {
    let items: Vec<String> = (lo..hi)
        .map(|i| format!(r#"{{id: {i}, data: "value-{i}"}}"#))
        .collect();
    run(
        engine,
        &format!(r#"PUT BATCH IN "items" [{}]"#, items.join(", ")),
    );
}

#[test]
fn data_survives_cross_profile_open_both_directions() {
    const N: usize = 1000;

    for (write_p, read_p) in [
        (StorageProfile::Hdd, StorageProfile::Ssd),
        (StorageProfile::Ssd, StorageProfile::Hdd),
    ] {
        let dir = tempfile::tempdir().unwrap();

        // Write + flush to SSTs sized by `write_p`, then clean shutdown.
        {
            let engine = open(dir.path(), write_p);
            run(&engine, r#"LOBE "items""#);
            put_batch(&engine, 0, N / 2);
            put_batch(&engine, N / 2, N);
            run(&engine, "COMPACT");
        }

        // Reopen under the OTHER profile and read everything back.
        let engine = open(dir.path(), read_p);
        let recs = records(run(&engine, r#"SCAN "items" LIMIT 5000"#));
        assert_eq!(
            recs.len(),
            N,
            "all {N} records written under {write_p:?} must read under {read_p:?}"
        );

        // Teeth: field values must be byte-identical across the profile switch.
        for probe in [0usize, 1, N / 2, N - 1] {
            let r = recs
                .iter()
                .find(|r| r.fields.get("id").and_then(|v| v.as_int()) == Some(probe as i64))
                .unwrap_or_else(|| panic!("id={probe} missing after {write_p:?}->{read_p:?}"));
            assert_eq!(
                r.fields.get("data").and_then(|v| v.as_text()),
                Some(format!("value-{probe}").as_str()),
                "field corrupted for id={probe} after {write_p:?}->{read_p:?}"
            );
        }

        // And the on-disk blocks verify clean under the read profile.
        let scrub = match run(&engine, "SCRUB") {
            QueryResult::Ok { message, .. } => message,
            other => panic!("scrub: {other:?}"),
        };
        assert!(
            scrub.contains("clean"),
            "cross-profile SCRUB must be clean ({write_p:?}->{read_p:?}), got: {scrub}"
        );
    }
}
