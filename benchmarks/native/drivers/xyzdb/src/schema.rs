//! Phase 0 schema setup: lobes, anchors, PIN sets, ghosts (per
//! the native cross-engine bench design).

use anyhow::Result;
use native_generator::bench::{SchemaMetrics, SchemaMode};
use std::time::Instant;
use tracing::info;

pub fn setup(driver: &super::XyzdbDriver, mode: SchemaMode) -> Result<SchemaMetrics> {
    let start = Instant::now();
    let mut setup_statements = 0usize;

    info!(target: "xyzdb", "Phase 0: schema setup mode={:?}", mode);

    // Lobes
    let lobes = [
        (r#"LOBE "clientes" HINT="Client demographics, scoring""#, 1),
        (
            r#"LOBE "creditos" HINT="Credit + Installment + Payment + Collection + CollectionAction""#,
            1,
        ),
        (
            r#"LOBE "operaciones" HINT="CreditApplication + AuditLog + Notification""#,
            1,
        ),
        (
            r#"LOBE "configuracion" HINT="Empresa + Producto catalog""#,
            1,
        ),
        (r#"LOBE "bi" HINT="BI snapshots""#, 1),
    ];
    for (stmt, n) in lobes {
        driver.execute_ok(stmt)?;
        setup_statements += n;
    }

    // Anchors. UNIQUE enforces per-lobe uniqueness, so anchors only
    // apply to fields that are genuinely unique within their lobe.
    // - `rfc` in `clientes` ✓ (one Client per RFC).
    // - `credit_id` in `creditos` ✗ (Installments/Payments/... share
    //   credit_id with their parent Credit via gravity co-location;
    //   declaring it UNIQUE would reject the heterogeneous siblings).
    // - `empresa_id` in `configuracion` ✗ (Productos share empresa_id
    //   with their owning Empresa).
    // Q1 (FIND by rfc) is the only query that benefits from an anchor;
    // Q2/Q4/Q5/Q6 use ghosts, not anchor lookups.
    let anchors = [r#"ANCHOR "rfc" UNIQUE IN "clientes""#];
    for stmt in anchors {
        driver.execute_ok(stmt)?;
        setup_statements += 1;
    }

    // PIN sets. Path A (PIN rfc on creditos) was measured at Scale 0.01
    // and confirmed inert — bloom on rfc has near-zero effective prune
    // because rfc has high in-block cardinality. Reverted; Finding 13
    // (gravity-as-index in SCAN equality path) is the engine-level fix
    // that addresses the same cost.
    let pins = [
        r#"PIN status, monto, monto_total, fecha_vencimiento, dias_atraso, fecha_pago_ms IN "creditos""#,
        r#"PIN rfc, scoring_risk IN "clientes""#,
        r#"PIN action_type, fecha_ms IN "operaciones""#,
    ];
    for stmt in pins {
        driver.execute_ok(stmt)?;
        setup_statements += 1;
    }

    // BULKMODE on for Phase 1 — a load-mode toggle, NOT a schema declaration,
    // so it is not counted in setup_statements (parity: pg/mongo load-time
    // tuning is not counted either).
    driver.execute_ok("BULKMODE ON")?;

    // 30-day cutoff (epoch millis), baked once and shared: the Q6/Q8
    // ghosts embed it as a literal predicate, and the Q8 read must carry
    // the identical literal so the router recognises the composite
    // ghost. The parser has no `now() - N` arithmetic, so an absolute cutoff
    // is the honest equivalent; it is regenerated each Phase 0.
    let cutoff_ms: i64 = (chrono::Utc::now() - chrono::Duration::days(30)).timestamp_millis();
    driver.set_cutoff_ms(cutoff_ms);

    // Business-question-anchored ghosts (skipped in AutoOnly mode).
    // The parser lifts IN-lists (parse_where_expr) and per-metric conditional
    // aggregates with AS aliases (parse_aggregate_item), so the ideal
    // semantics materialise directly — no single-status workaround, no
    // client-side stitch.
    if mode == SchemaMode::Full {
        // Q2 + Q4 share one ghost: "current exposure" = credits with status IN
        // (active, overdue), grouped by rfc. Q2 reads a single group (rfc as an
        // Eq-on-group-key predicate); Q4 reads the top-N SERVER-SIDE via
        // `TAKE n BY sum(monto)` off the metric-ordered ghost (declared with
        // `| TAKE BY sum(monto) DESC` below), so the engine returns only the N
        // groups — no client-side rank, no full-group read.
        //
        // Q6 uses the shared literal `cutoff_ms` (no now()-N in the parser).
        //
        // Q8 "monthly close" is ONE composite ghost with six per-metric
        // conditional aggregates (no header WHERE — each metric self-filters).
        // Read back via the router `SCAN ... | GROUP BY ... | AGGREGATE ...`
        // idiom: `SCAN GHOST` returns representative index records, not grouped
        // aggregates. Payments/actions group by the empresa_id denormalised at
        // load. The read must repeat the same cutoff_ms literal (driver.cutoff_ms).
        let q6_ghost = format!(
            r#"CREATE GHOST "payments_high_recent_30d" FROM "creditos" WHERE _type = "Payment" AND monto > 50000 AND fecha_pago_ms >= {cutoff_ms} | TAKE BY fecha_pago_ms DESC | EMBED rfc, monto, credit_id, fecha_pago_ms"#
        );
        let q8_ghost = format!(
            r#"CREATE GHOST "monthly_close_by_emp" FROM "creditos" | GROUP BY empresa_id | AGGREGATE count() AS n_vigentes WHERE _type = "Credit" AND status IN ["active", "overdue"], sum(monto_total) AS vencido_sum WHERE _type = "Installment" AND status = "overdue", count() AS vencido_n WHERE _type = "Installment" AND status = "overdue", sum(monto) AS cobrado_sum WHERE _type = "Payment" AND fecha_pago_ms >= {cutoff_ms}, count() AS cobrado_n WHERE _type = "Payment" AND fecha_pago_ms >= {cutoff_ms}, count() AS acciones_n WHERE _type = "CollectionAction" AND fecha_ms >= {cutoff_ms} | TAKE BY empresa_id"#
        );
        let ghosts: [&str; 4] = [
            // Q2 + Q4 — current exposure per rfc (status IN active, overdue).
            // `| TAKE BY sum(monto) DESC` declares a metric-ordered rollup so
            // Q4's `TAKE n BY sum(monto)` reads O(N) instead of scanning all M rfcs.
            r#"CREATE GHOST "credits_by_rfc" FROM "creditos" WHERE _type = "Credit" AND status IN ["active", "overdue"] | GROUP BY rfc | AGGREGATE sum(monto), count() | TAKE BY sum(monto) DESC"#,
            // Q5 — overdue installments per empresa
            r#"CREATE GHOST "overdue_by_empresa" FROM "creditos" WHERE _type = "Installment" AND status = "overdue" | GROUP BY empresa_id | AGGREGATE sum(monto_total), count() | TAKE BY empresa_id"#,
            // Q6 — recent high-value payments with 30-day literal cutoff
            &q6_ghost,
            // Q8 — composite monthly close (read via router GROUP BY | AGGREGATE)
            &q8_ghost,
        ];
        for stmt in ghosts {
            driver.execute_ok(stmt)?;
            setup_statements += 1;
        }
    }

    Ok(SchemaMetrics {
        mode,
        setup_statements,
        setup_duration_ms: start.elapsed().as_millis() as u64,
    })
}
