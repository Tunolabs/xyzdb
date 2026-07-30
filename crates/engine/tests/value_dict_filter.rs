//! 1f — filtering by a low-cardinality field stays correct across ANALYZE.
//!
//! ANALYZE registers a value dictionary for low-cardinality TEXT fields
//! (`analyze.rs` → `DictRegistry::register`). That dictionary's encode
//! (`encode_record_fields`) and decode (`decode_record_fields`) paths are
//! currently UNWIRED — no write encodes a value to a code, no read decodes
//! one, and `deserialize_record` applies no value-decode. So field values
//! are always stored as `Text` and the records-path filter compares
//! `Text == Text` correctly, with or without a registered dictionary.
//!
//! This test locks that invariant and acts as a forward guard: the day a
//! value is encoded to a code on write (waking the dictionary), the read
//! path MUST decode it before filtering. If encode is ever wired without a
//! matching decode, post-ANALYZE rows would be stored as codes, the
//! `kind = "a"` predicate (a `Text` literal) would stop matching them, and
//! this assertion would fail — surfacing the gap instead of silently
//! dropping rows. Domain-neutral vocab: the engine is agnostic.

use xyzdb_engine::engine::{Engine, QueryResult};

fn exec(engine: &Engine, s: &str) -> QueryResult {
    let stmt = xytalk_parser::parse(s).unwrap_or_else(|e| panic!("parse {s:?}: {e:?}"));
    engine
        .execute(stmt)
        .unwrap_or_else(|e| panic!("exec {s:?}: {e:?}"))
}

fn count(qr: QueryResult) -> usize {
    match qr {
        QueryResult::Records(r) => r.len(),
        QueryResult::PaginatedRecords { records, .. } => records.len(),
        other => panic!("unexpected scan result: {other:?}"),
    }
}

/// Build one `PUT BATCH` for rows `[lo, hi)`, `kind` alternating a/b when
/// `kind_a_only` is false, else all `"a"`.
fn put_rows(engine: &Engine, lo: usize, hi: usize, kind_a_only: bool) {
    let mut body = String::from(r#"PUT BATCH IN "items" ["#);
    for i in lo..hi {
        if i > lo {
            body.push(',');
        }
        let kind = if kind_a_only || i % 2 == 0 { "a" } else { "b" };
        body.push_str(&format!(r#"{{kind: "{kind}", id: "R{i}", n: {i}}}"#));
    }
    body.push(']');
    exec(engine, &body);
}

#[test]
fn filter_by_low_cardinality_field_correct_across_analyze() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "items""#);

    // 1000 rows pre-ANALYZE: 500 kind="a", 500 kind="b". Meets ANALYZE's
    // thresholds (>=1000 total, cardinality 2 < 1000, TEXT, count >= 100).
    put_rows(&engine, 0, 1000, false);

    // Register the value dictionary for `kind`, and assert it actually
    // registered — otherwise this test would be a trivial pass that guards
    // nothing.
    let analyze = exec(&engine, r#"ANALYZE "items""#);
    let report = match analyze {
        QueryResult::Info(lines) => lines.join("\n"),
        other => format!("{other:?}"),
    };
    assert!(
        report.contains("Dictionary encoding created") && report.contains("kind"),
        "ANALYZE must register a value dictionary for `kind` (else the guard \
         is vacuous); report was:\n{report}"
    );

    // 24 more kind="a" rows AFTER ANALYZE — the mixed-encoding scenario.
    put_rows(&engine, 1000, 1024, true);

    // Records-path filter (no ghost, no gravity): must return every kind="a"
    // row, pre- and post-ANALYZE. 500 + 24 = 524.
    let n = count(exec(
        &engine,
        r#"SCAN "items" WHERE kind = "a" LIMIT 10000"#,
    ));
    assert_eq!(
        n, 524,
        "filtering by a dictionary-registered field must return all matching \
         rows across ANALYZE (got {n}); a code stored without a matching \
         decode would drop the post-ANALYZE rows"
    );
}
