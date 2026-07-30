//! Q1..Q9 native xyTalk forms.
//!
//! Q7 batch ingest + Q8 monthly close are NEW v0.3.3 (design §7.8 + §7.9).
//! Q8 is ONE composite read via the router `SCAN ... | GROUP BY empresa_id |
//! AGGREGATE ...` idiom, routed to the `monthly_close_by_emp` composite ghost:
//! six per-metric conditional aggregates in a single PreComputed read (see
//! schema.rs and `monthly_close_query`). No client-side per-empresa stitch.
//!
//! Q9 customer 360 is NEW v0.3.3 (§7.10). Q9 issues a 3-call sequence (FIND
//! client + 1 SCAN creditos + 1 SCAN operaciones for the same rfc; each SCAN
//! walks the whole rfc gravity bucket, so one scan per lobe returns every
//! co-located _type). Gravity co-location warms the lobe pages intrinsically
//! once the first SCAN runs — no driver-side cache-warming wrapper, and no
//! `INCACHE` prefix (the bench issues no INCACHE statement anywhere; `INCACHE`
//! is an xyTalk primitive, §2.10 spec, but this harness does not use it).
//! (Q10 transactional cascade was removed — deferred on xyzDB/Mongo, PG-only,
//! an asymmetric cell that never belonged in the cross-engine comparison.)

use anyhow::Result;
use native_generator::bench::{BusinessQuery, QueryExecution, QueryParams};
use std::time::Instant;

/// Build the xyTalk text for a single-statement query. Q8 returns ""
/// because it dispatches to a custom multi-statement path in `run_one`.
pub fn query_text(q: BusinessQuery, rfc: &str, limit: usize) -> String {
    match q {
        BusinessQuery::Q1Point => format!(r#"FIND "clientes" WHERE rfc = "{rfc}""#),
        BusinessQuery::Q2Aggregate => {
            // Current exposure of one client = sum + count of credits with
            // status IN [active, overdue] for the rfc. The canonical
            // SCAN ... | GROUP BY rfc | AGGREGATE pattern routes via the
            // router to the `credits_by_rfc` ghost (PreComputed, rfc as an
            // Eq-on-group-key predicate → one group). The status filter
            // matches the ghost's filter_fields; without it the query would
            // not route.
            format!(
                r#"SCAN "creditos" WHERE _type = "Credit" AND status IN ["active", "overdue"] AND rfc = "{rfc}" | GROUP BY rfc | AGGREGATE sum(monto), count()"#
            )
        }
        BusinessQuery::Q3FullHistory => {
            format!(r#"SCAN "creditos" WHERE rfc = "{rfc}" LIMIT 5000"#)
        }
        BusinessQuery::Q4TopExposure => {
            // Top-N clients by current exposure, resolved SERVER-SIDE with the
            // TAKE pipeline step (no more ORDER-BY-in-client). Same aggregate as
            // Q2, read across all rfc groups, then TAKE n BY sum(monto). Routes
            // to the shared `credits_by_rfc` ghost; the engine partial-selects
            // the N groups and returns only those.
            format!(
                r#"SCAN "creditos" WHERE _type = "Credit" AND status IN ["active", "overdue"] | GROUP BY rfc | AGGREGATE sum(monto), count() | TAKE {limit} BY sum(monto)"#
            )
        }
        BusinessQuery::Q5OverdueByEmpresa => {
            // Overdue installments per empresa, read via the router idiom so
            // it hits the `overdue_by_empresa` ghost's PreComputed grouped
            // aggregates (SCAN GHOST returns representative records, not the
            // per-group sums).
            r#"SCAN "creditos" WHERE _type = "Installment" AND status = "overdue" | GROUP BY empresa_id | AGGREGATE sum(monto_total), count()"#.to_string()
        }
        BusinessQuery::Q6RecentPayments => {
            format!(r#"SCAN GHOST "payments_high_recent_30d" LIMIT {limit}"#)
        }
        // Q7/Q8/Q9 build with driver context (the rfc→credit map / cutoff) in
        // `run_one`; they are not built here.
        BusinessQuery::Q7BatchIngest => String::new(),
        BusinessQuery::Q8MonthlyClose => String::new(),
        // Q9 dispatches to a multi-statement custom path in `run_one`.
        BusinessQuery::Q9CustomerContext => String::new(),
    }
}

/// Rows per Q7 batch insert. Shared by the query builder and the
/// records_returned report so the two never drift: a `PUT BATCH` ack has no
/// `"N record(s)"` line, so `parse_record_count` would fall back to counting
/// ack lines (~3) and undercount the insert — the driver reports this count
/// directly instead, matching PG's `rows_affected` and Mongo's `inserted_ids`.
pub(crate) const Q7_BATCH_SIZE: usize = 100;

/// Q7 — PUT BATCH [`Q7_BATCH_SIZE`] synthetic payment records keyed on the
/// sampled rfc (gravity co-location with the client's existing creditos block).
/// Per design §7.8 + §8.A7: `PUT BATCH IN "creditos" [...]` is the
/// xyTalk atomic-batch primitive.
///
/// `credit_empresa` is one real `(credit_id, empresa_id)` of `rfc` (from the
/// load-time map). Two invariants keep Q7 from contaminating Q8 (and keep the
/// three engines symmetric):
/// - **real credit_id + empresa_id** → the payment attaches to a real credit
///   (PG's Q3 join resolves it) and a real empresa (Q8's per-empresa group),
///   never a phantom null-empresa.
/// - **old `fecha_pago_ms`** (well before the 30-day cutoff) → the payment is
///   OUTSIDE Q8's "cobrado" window in every engine (identical date filter), so
///   the three exclude the same Q7 payments. Q7 measures ingest latency, not
///   analytical freshness.
pub(crate) fn build_q7_put_batch(rfc: &str, credit_empresa: Option<(String, String)>) -> String {
    let now_ms = chrono::Utc::now().timestamp_millis();
    // ~400 days back: comfortably before any 30-day cutoff, still inside the
    // 2020-2030 data window.
    let old_fecha_ms = now_ms - 400 * 86_400_000;
    // Fallback (never fires for a sampled client — every rfc has credits) keeps
    // a valid batch; the old date still excludes it from Q8's cobrado window.
    let (credit_id, empresa_id) =
        credit_empresa.unwrap_or_else(|| (format!("{rfc}-C0"), "EMP_UNKNOWN".to_string()));
    let mut buf = String::with_capacity(8 * 1024);
    buf.push_str(r#"PUT BATCH IN "creditos" ["#);
    for i in 0..Q7_BATCH_SIZE {
        if i > 0 {
            buf.push(',');
        }
        // Synthetic payment record. `*rfc` marks gravity-key per xytalk-spec.
        // Unique payment_id avoids anchor collision under repeated Q7 runs;
        // credit_id/empresa_id are the borrowed real pair (see doc above).
        let payment_id = format!("Q7_PAY_{}_{}", now_ms, i);
        buf.push_str(&format!(
            r#"{{*rfc: "{rfc}", _type: "Payment", payment_id: "{payment_id}", credit_id: "{credit_id}", empresa_id: "{empresa_id}", monto: 100, fecha_pago_ms: {old_fecha_ms}}}"#
        ));
    }
    buf.push(']');
    buf
}

pub fn run_one(
    driver: &super::XyzdbDriver,
    q: BusinessQuery,
    params: &QueryParams,
) -> Result<QueryExecution> {
    match q {
        BusinessQuery::Q7BatchIngest => {
            let xytalk = build_q7_put_batch(&params.rfc, driver.q7_credit_for(&params.rfc));
            let t0 = Instant::now();
            let _resp = driver.execute(&xytalk)?;
            let lat_ms = t0.elapsed().as_secs_f64() * 1000.0;
            // PUT BATCH ack has no "N record(s)" line, so parse_record_count
            // would count ack lines (~3) and undercount. Report the known batch
            // size, matching PG's rows_affected (100) and Mongo's
            // inserted_ids.len() (100).
            return Ok(QueryExecution {
                latency_ms: lat_ms,
                records_returned: Q7_BATCH_SIZE as u64,
            });
        }
        BusinessQuery::Q8MonthlyClose => return run_q8_monthly_close(driver),
        BusinessQuery::Q9CustomerContext => {
            return run_q9_customer_context(driver, &params.rfc);
        }
        _ => {}
    }
    let xytalk = query_text(q, &params.rfc, params.limit);
    let t0 = Instant::now();
    let resp = driver.execute(&xytalk)?;
    let lat_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let records = super::parse_record_count(&resp);
    Ok(QueryExecution {
        latency_ms: lat_ms,
        records_returned: records,
    })
}

/// Q8 — Monthly close per empresa: one composite read with six per-metric
/// conditional aggregates. The router idiom (`SCAN ... | GROUP BY ... |
/// AGGREGATE ...`) routes to the `monthly_close_by_emp` composite ghost
/// (PreComputed). The per-metric cutoff literal must equal the one baked
/// into the ghost at schema setup (`driver.cutoff_ms`); otherwise the
/// signatures differ and the router declines the ghost, silently falling
/// back to Primary.
fn run_q8_monthly_close(driver: &super::XyzdbDriver) -> Result<QueryExecution> {
    let stmt = monthly_close_query(driver.cutoff_ms());
    let t0 = Instant::now();
    let resp = driver.execute(&stmt)?;
    let lat_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let records = super::parse_record_count(&resp);
    Ok(QueryExecution {
        latency_ms: lat_ms,
        records_returned: records,
    })
}

/// The Q8 composite read. Kept in one place so the six per-metric predicates
/// stay byte-identical to the `monthly_close_by_emp` ghost's AGGREGATE clause
/// (schema.rs) — the aggregate signature must match for the router to serve the
/// ghost.
pub(crate) fn monthly_close_query(cutoff_ms: i64) -> String {
    format!(
        r#"SCAN "creditos" | GROUP BY empresa_id | AGGREGATE count() AS n_vigentes WHERE _type = "Credit" AND status IN ["active", "overdue"], sum(monto_total) AS vencido_sum WHERE _type = "Installment" AND status = "overdue", count() AS vencido_n WHERE _type = "Installment" AND status = "overdue", sum(monto) AS cobrado_sum WHERE _type = "Payment" AND fecha_pago_ms >= {cutoff_ms}, count() AS cobrado_n WHERE _type = "Payment" AND fecha_pago_ms >= {cutoff_ms}, count() AS acciones_n WHERE _type = "CollectionAction" AND fecha_ms >= {cutoff_ms}"#
    )
}

/// Q9 — Customer 360 context pull. 3-call sequence (FIND client + 1 SCAN
/// creditos + 1 SCAN operaciones): each SCAN walks the whole rfc gravity
/// bucket, so one scan per lobe returns every co-located _type, split
/// client-side into the six sections PG's get_customer_360 returns
/// (cliente + credits + payments + collections + audit + notifications).
/// Gravity co-location guarantees the 2 lobe SCANs after the FIND hit the
/// same physical pages, so the in-call cache effect is intrinsic to the
/// engine's storage layout — no driver-side warming wrapper needed, plus the
/// cumulative gravity-cache warming behaviour across repeated SCANs on the
/// same rfc.
///
/// xyTalk has a warm-up primitive, `INCACHE "lobe" [WHERE …]` (xytalk-spec
/// §2.10), distinct from the engine's internal `HotCache` struct and the
/// server's `--hot-cache-size N` flag. This bench does NOT issue `INCACHE`
/// anywhere: Q9's warm-page effect comes for free from gravity co-location
/// (the first per-rfc SCAN pulls the whole bucket; the following scans hit warm
/// pages), so no driver-side warming wrapper and no per-Q9 prefix are used.
fn run_q9_customer_context(driver: &super::XyzdbDriver, rfc: &str) -> Result<QueryExecution> {
    // 3 calls, not 5: a `SCAN "lobe" WHERE rfc` walks the rfc's whole gravity
    // bucket and returns every co-located entity (scan_primary_gravity_indexed,
    // engine ops/scan.rs), so a single scan per lobe already holds every _type —
    // the old per-_type scans re-scanned a bucket already fetched. Fetch each
    // bucket once and split by _type client-side (see the section counts below),
    // fewer roundtrips. The six sections match PG's get_customer_360 exactly
    // (cliente + credits + payments + collections + audit + notifications).
    // Still 3 roundtrips vs PG's 1-roundtrip function — honest.
    let find_client = format!(r#"FIND "clientes" WHERE rfc = "{rfc}""#);
    let scan_creditos = format!(r#"SCAN "creditos" WHERE rfc = "{rfc}" LIMIT 5000"#);
    let scan_operaciones = format!(r#"SCAN "operaciones" WHERE rfc = "{rfc}" LIMIT 5000"#);

    let t0 = Instant::now();
    // 1. client identity
    let mut total_records: u64 = super::parse_record_count(&driver.execute(&find_client)?);
    // Canonical Q9: the 360 covers ALL of the customer's credits (not a
    // sample), plus recent activity feeds. Count each section by _type from the
    // rfc gravity bucket — every entity carries rfc, so the ONE creditos scan
    // already holds credits + payments + collections co-located; split
    // client-side, no extra roundtrip, no engine change:
    //   credits      — ALL (the full portfolio universe)
    //   payments     — recent 30 · collections — recent 10  (activity feeds)
    //   audit        — recent 50 · notifications — recent 20 (from operaciones)
    // Same universe as PG's get_customer_360 (all credits of the rfc) with the
    // same activity caps — PG join-plano / xyzDB bucket-plano, same question.
    let n_type = |recs: &[std::collections::BTreeMap<String, String>], t: &str| -> usize {
        recs.iter()
            .filter(|r| r.get("_type").map(String::as_str) == Some(t))
            .count()
    };
    // 2. one creditos bucket scan → credits (all) + payments (30) + collections (10)
    let creditos = super::parse_box_records(&driver.execute(&scan_creditos)?);
    total_records += (n_type(&creditos, "Credit")
        + n_type(&creditos, "Payment").min(30)
        + n_type(&creditos, "Collection").min(10)) as u64;
    // 3. one operaciones bucket scan → audit (50) + notifications (20)
    let operaciones = super::parse_box_records(&driver.execute(&scan_operaciones)?);
    total_records += (n_type(&operaciones, "AuditLog").min(50)
        + n_type(&operaciones, "Notification").min(20)) as u64;

    let lat_ms = t0.elapsed().as_secs_f64() * 1000.0;
    Ok(QueryExecution {
        latency_ms: lat_ms,
        records_returned: total_records,
    })
}
