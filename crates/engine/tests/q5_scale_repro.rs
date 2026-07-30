//! Reproduces the Q5-empty-at-scale bug at miniature scale.
//!
//! `Q5OverdueByEmpresa` (`SCAN GHOST "overdue_by_empresa"`) returns 80 rows at
//! Scale 0.002 but EMPTY at Scale 1.0. The differentiator is the ghost build's
//! multi-chunk flush: at small scale every entry fits in the 8 MB ghost-keyspace
//! buffer (one flush), at large scale the buffer flushes many times. This test
//! shrinks the ghost buffer via `TURBA_GHOST_MEMTABLE_BYTES` so a few hundred
//! matching records force the multi-chunk path — no 150K records needed — and
//! mirrors the bench's exact lifecycle (CREATE GHOST → BULKMODE load → COMPACT →
//! REFRESH GHOST → SCAN GHOST).

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

#[test]
fn scan_ghost_not_empty_after_multichunk_refresh() {
    // Shrink the ghost-keyspace memtable BEFORE opening the engine so the build
    // crosses `buf_limit` many times on a tiny dataset. Set before any engine
    // thread starts; this is the only test in this binary (own process), so no
    // concurrent env access.
    // SAFETY: single-threaded test, set before the engine (and its background
    // threads) are created; no other thread reads or writes the environment.
    // Shrink EVERY keyspace so the spatial tree forms deep LSM levels (L0..Ln)
    // after COMPACT — replicating the at-scale structure the build scans over —
    // and the ghost buffer flushes in many chunks.
    unsafe {
        std::env::set_var("TURBA_TEST_MEMTABLE_BYTES", "16384");
        std::env::set_var("TURBA_GHOST_MEMTABLE_BYTES", "8192");
    }

    const EMPRESAS: usize = 200;
    const INSTALLMENTS_PER_EMPRESA: usize = 20; // 4000 matching records total

    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();

    // Phase 0 — schema mirroring the bench: a HETEROGENEOUS creditos lobe with
    // several ghosts SHARING the ghost keyspace. Crucially, an EMBED
    // (covering-index) ghost is maintained incrementally during the load, so by
    // the time `overdue_by_empresa` is REFRESHed the ghost keyspace already
    // holds sibling entries — and the build's final `major_compact()` compacts
    // the WHOLE keyspace, siblings included. This is the dimension the earlier
    // repros lacked.
    exec(&engine, r#"LOBE "creditos""#);
    exec(
        &engine,
        r#"CREATE GHOST "overdue_by_empresa" FROM "creditos" WHERE _type = "Installment" AND status = "overdue" ORDER BY empresa_id GROUP BY empresa_id AGGREGATE sum(monto_total), count()"#,
    );
    // Aggregate sibling (needs REFRESH, like Q2 credits_by_rfc).
    exec(
        &engine,
        r#"CREATE GHOST "credits_by_rfc" FROM "creditos" WHERE _type = "Credit" ORDER BY rfc GROUP BY rfc AGGREGATE sum(monto), count()"#,
    );
    // EMBED sibling (covering index, maintained incrementally during load).
    exec(
        &engine,
        r#"CREATE GHOST "payments_high_recent" FROM "creditos" WHERE _type = "Payment" AND monto > 50000 ORDER BY fecha_pago_ms DESC EMBED rfc, monto, credit_id, fecha_pago_ms"#,
    );

    // Phase 1 — bulk load: gravity-co-located heterogeneous records, like the
    // bench (Credit + Installments + Payment share `*credit_id`).
    exec(&engine, "BULKMODE ON");
    for emp in 0..EMPRESAS {
        for c in 0..INSTALLMENTS_PER_EMPRESA {
            let cid = emp * INSTALLMENTS_PER_EMPRESA + c;
            // Parent Credit (active) — feeds credits_by_rfc.
            exec(
                &engine,
                &format!(
                    r#"PUT {{*credit_id: "C{cid:06}", _type: "Credit", status: "active", empresa_id: "E{emp:04}", rfc: "RFC{emp:04}", monto: 5000.0}} IN "creditos""#
                ),
            );
            // Overdue Installment — feeds overdue_by_empresa (Q5).
            exec(
                &engine,
                &format!(
                    r#"PUT {{*credit_id: "C{cid:06}", _type: "Installment", status: "overdue", empresa_id: "E{emp:04}", monto_total: {amt}}} IN "creditos""#,
                    amt = 100.0 + (c as f64)
                ),
            );
            // High-value Payment — feeds the EMBED ghost incrementally.
            exec(
                &engine,
                &format!(
                    r#"PUT {{*credit_id: "C{cid:06}", _type: "Payment", monto: 60000.0, rfc: "RFC{emp:04}", fecha_pago_ms: {ts}}} IN "creditos""#,
                    ts = 1_700_000_000_000i64 + cid as i64
                ),
            );
        }
    }

    // Phase 0.5 — post-load: the bench's exact sequence.
    exec(&engine, "BULKMODE OFF");
    exec(&engine, "COMPACT");
    exec(&engine, r#"REFRESH GHOST "overdue_by_empresa""#);

    // Q5: explicit ghost scan. One row per empresa (order_by == group_by ==
    // empresa_id), so EMPRESAS rows. Empty = the bug reproduced.
    let n = count(exec(&engine, r#"SCAN GHOST "overdue_by_empresa""#));
    eprintln!("SCAN GHOST returned {n} rows (expected {EMPRESAS})");
    assert_eq!(
        n, EMPRESAS,
        "SCAN GHOST on the aggregate ghost must return one row per empresa after a \
         multi-chunk REFRESH; got {n} (0/short = the Q5-at-scale bug)"
    );
}
