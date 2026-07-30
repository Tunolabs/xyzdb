//! Phase 0 schema setup: tables, partitioning, indexes, mat views.
//! Follows the native cross-engine bench design.

use anyhow::Result;
use native_generator::bench::{SchemaMetrics, SchemaMode};
use std::time::Instant;
use tracing::info;

pub fn setup(driver: &super::PostgresDriver, mode: SchemaMode) -> Result<SchemaMetrics> {
    let start = Instant::now();
    info!(target: "postgres", "Phase 0: schema setup mode={:?}", mode);

    // Drop / cascade existing schema (tolerated to keep the bench
    // re-runnable from clean state).
    let drops = [
        "DROP FUNCTION IF EXISTS get_customer_360(TEXT);",
        "DROP MATERIALIZED VIEW IF EXISTS monthly_close_mat CASCADE;",
        "DROP MATERIALIZED VIEW IF EXISTS overdue_by_empresa_mat CASCADE;",
        "DROP MATERIALIZED VIEW IF EXISTS top_active_balance CASCADE;",
        "DROP MATERIALIZED VIEW IF EXISTS credits_by_rfc_mat CASCADE;",
        "DROP MATERIALIZED VIEW IF EXISTS credits_by_rfc CASCADE;",
        "DROP TABLE IF EXISTS bi_snapshots CASCADE;",
        "DROP TABLE IF EXISTS notifications CASCADE;",
        "DROP TABLE IF EXISTS audit_log CASCADE;",
        "DROP TABLE IF EXISTS credit_applications CASCADE;",
        "DROP TABLE IF EXISTS collection_actions CASCADE;",
        "DROP TABLE IF EXISTS collections CASCADE;",
        "DROP TABLE IF EXISTS payments CASCADE;",
        "DROP TABLE IF EXISTS installments CASCADE;",
        "DROP TABLE IF EXISTS credits CASCADE;",
        "DROP TABLE IF EXISTS clientes CASCADE;",
        "DROP TABLE IF EXISTS productos CASCADE;",
        "DROP TABLE IF EXISTS empresas CASCADE;",
    ];
    for d in drops {
        let _ = driver.execute_simple(d);
    }

    let mut setup_statements = 0usize;

    // Catalog
    driver.execute_simple(
        r#"CREATE TABLE empresas (
            empresa_id  TEXT PRIMARY KEY,
            nombre      TEXT NOT NULL,
            region      TEXT,
            activa      BOOLEAN DEFAULT true
        )"#,
    )?;
    setup_statements += 1;

    driver.execute_simple(
        r#"CREATE TABLE productos (
            producto_id   TEXT PRIMARY KEY,
            empresa_id    TEXT REFERENCES empresas(empresa_id),
            nombre        TEXT NOT NULL,
            tasa_interes  NUMERIC(6,4),
            plazo_meses   INT
        )"#,
    )?;
    setup_statements += 1;

    // clientes — single demographic row per RFC, NOT partitioned.
    driver.execute_simple(
        r#"CREATE TABLE clientes (
            rfc                       TEXT PRIMARY KEY,
            curp                      TEXT UNIQUE NOT NULL,
            nombre                    TEXT NOT NULL,
            scoring                   JSONB,
            datos_ubicacion           JSONB,
            datos_identificacion      JSONB,
            caracteristicas_fiscales  JSONB,
            tags                      TEXT[],
            fecha_alta                TIMESTAMPTZ DEFAULT NOW()
        )"#,
    )?;
    setup_statements += 1;

    // Partitioned tables. Note: with PARTITION BY, the partition key
    // must appear in any UNIQUE / PK constraint. We use compound PK
    // (id, empresa_id).
    driver.execute_simple(
        r#"CREATE TABLE credits (
            credit_id           TEXT NOT NULL,
            rfc                 TEXT NOT NULL REFERENCES clientes(rfc),
            empresa_id          TEXT NOT NULL REFERENCES empresas(empresa_id),
            producto_id         TEXT REFERENCES productos(producto_id),
            monto               NUMERIC(14,2) NOT NULL,
            status              TEXT NOT NULL CHECK (status IN ('active','overdue','paid','cancelled','defaulted')),
            fecha_creacion      TIMESTAMPTZ NOT NULL,
            fecha_vencimiento   DATE,
            dias_atraso         INT DEFAULT 0,
            PRIMARY KEY (credit_id, empresa_id)
        ) PARTITION BY LIST (empresa_id)"#,
    )?;
    setup_statements += 1;

    // Default partition catches any empresa_id not explicitly handled
    // (and saves us creating 80 partitions individually for the bench).
    driver.execute_simple("CREATE TABLE credits_default PARTITION OF credits DEFAULT")?;
    setup_statements += 1;

    driver.execute_simple(
        r#"CREATE TABLE installments (
            installment_id      TEXT NOT NULL,
            credit_id           TEXT NOT NULL,
            empresa_id          TEXT NOT NULL,
            numero              INT NOT NULL,
            monto_total         NUMERIC(14,2) NOT NULL,
            status              TEXT NOT NULL CHECK (status IN ('pending','paid','overdue','partial')),
            dias_atraso         INT DEFAULT 0,
            fecha_vencimiento   DATE NOT NULL,
            PRIMARY KEY (installment_id, empresa_id)
        ) PARTITION BY LIST (empresa_id)"#,
    )?;
    setup_statements += 1;
    driver.execute_simple("CREATE TABLE installments_default PARTITION OF installments DEFAULT")?;
    setup_statements += 1;

    driver.execute_simple(
        r#"CREATE TABLE payments (
            payment_id          TEXT PRIMARY KEY,
            credit_id           TEXT NOT NULL,
            installment_id      TEXT,
            rfc                 TEXT NOT NULL,
            monto               NUMERIC(14,2) NOT NULL,
            fecha_pago          TIMESTAMPTZ NOT NULL,
            metodo              TEXT
        )"#,
    )?;
    setup_statements += 1;

    driver.execute_simple(
        r#"CREATE TABLE collections (
            collection_id       TEXT PRIMARY KEY,
            credit_id           TEXT NOT NULL,
            monto_pendiente     NUMERIC(14,2) NOT NULL,
            status              TEXT NOT NULL,
            fecha_inicio        TIMESTAMPTZ
        )"#,
    )?;
    setup_statements += 1;

    driver.execute_simple(
        r#"CREATE TABLE collection_actions (
            action_id           TEXT PRIMARY KEY,
            collection_id       TEXT NOT NULL REFERENCES collections(collection_id),
            tipo                TEXT NOT NULL,
            fecha               TIMESTAMPTZ NOT NULL,
            resultado           TEXT
        )"#,
    )?;
    setup_statements += 1;

    driver.execute_simple(
        r#"CREATE TABLE credit_applications (
            application_id      TEXT PRIMARY KEY,
            rfc                 TEXT NOT NULL,
            empresa_id          TEXT,
            producto_id         TEXT,
            monto_solicitado    NUMERIC(14,2) NOT NULL,
            status              TEXT NOT NULL,
            fecha_solicitud     TIMESTAMPTZ NOT NULL
        )"#,
    )?;
    setup_statements += 1;

    driver.execute_simple(
        r#"CREATE TABLE audit_log (
            audit_id            BIGSERIAL PRIMARY KEY,
            rfc                 TEXT,
            credit_id           TEXT,
            action_type         TEXT NOT NULL,
            details             JSONB,
            fecha               TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )"#,
    )?;
    setup_statements += 1;

    driver.execute_simple(
        r#"CREATE TABLE notifications (
            notification_id     BIGSERIAL PRIMARY KEY,
            rfc                 TEXT NOT NULL,
            canal               TEXT NOT NULL,
            contenido           TEXT,
            fecha               TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )"#,
    )?;
    setup_statements += 1;

    driver.execute_simple(
        r#"CREATE TABLE bi_snapshots (
            snapshot_id         BIGSERIAL PRIMARY KEY,
            empresa_id          TEXT,
            fecha               DATE NOT NULL,
            metricas            JSONB NOT NULL
        )"#,
    )?;
    setup_statements += 1;

    // Indexes (same set regardless of mode — required for queries).
    let indexes = [
        "CREATE INDEX idx_credits_rfc          ON credits(rfc)",
        "CREATE INDEX idx_credits_empresa      ON credits(empresa_id)",
        "CREATE INDEX idx_credits_status_monto ON credits(status, monto DESC)",
        "CREATE INDEX idx_credits_active_monto ON credits(monto DESC) WHERE status IN ('active','overdue')",
        "CREATE INDEX idx_installments_credit  ON installments(credit_id)",
        "CREATE INDEX idx_installments_status_dias ON installments(status, dias_atraso DESC)",
        "CREATE INDEX idx_installments_overdue ON installments(empresa_id, monto_total) WHERE status = 'overdue'",
        "CREATE INDEX idx_payments_credit      ON payments(credit_id)",
        "CREATE INDEX idx_payments_rfc         ON payments(rfc)",
        "CREATE INDEX idx_payments_recent_high ON payments(fecha_pago DESC) INCLUDE (rfc, monto, credit_id) WHERE monto > 50000",
        "CREATE INDEX idx_collections_credit   ON collections(credit_id)",
        "CREATE INDEX idx_credit_applications_rfc ON credit_applications(rfc, fecha_solicitud DESC)",
        "CREATE INDEX idx_audit_log_rfc        ON audit_log(rfc, fecha DESC)",
        "CREATE INDEX idx_notifications_rfc    ON notifications(rfc, fecha DESC)",
        "CREATE INDEX idx_clientes_scoring     ON clientes USING gin(scoring jsonb_path_ops)",
        "CREATE INDEX idx_clientes_tags        ON clientes USING gin(tags)",
        "CREATE INDEX idx_clientes_ubicacion   ON clientes USING gin(datos_ubicacion jsonb_path_ops)",
    ];
    for idx in indexes {
        driver.execute_simple(idx)?;
        setup_statements += 1;
    }

    // Materialised views (Q2 + Q4 enhancement). Skipped in AutoOnly
    // mode — though PG has no telemetry-driven mat-view promotion, so
    // AutoOnly here is essentially "stripped of pre-aggregation"
    // baseline that mirrors xyzDB's no-ghost variant.
    if mode == SchemaMode::Full {
        // Q2 mat-view — renamed to `credits_by_rfc_mat` per design §8.B2;
        // type casts ::float8 / ::int8 align column types with Q2 driver
        // expectations (Phase 2 fairness fix — audit §3 + §7 finding 6).
        driver.execute_simple(
            r#"CREATE MATERIALIZED VIEW credits_by_rfc_mat AS
               SELECT rfc,
                      SUM(monto)::float8 AS sum_monto,
                      COUNT(*)::int8     AS n_creditos
               FROM credits
               WHERE status IN ('active','overdue')
               GROUP BY rfc
               WITH NO DATA"#,
        )?;
        setup_statements += 1;
        driver.execute_simple("CREATE UNIQUE INDEX ON credits_by_rfc_mat(rfc)")?;
        setup_statements += 1;
        // Q4 top-N by exposure reuses this mat-view: its sum_monto IS the
        // active+overdue exposure per rfc (identical aggregate). A sum_monto
        // DESC index turns the top-N read into an index scan (EXPLAIN before:
        // Seq Scan + HashAggregate + top-N sort 10.5ms @scale 0.01; after:
        // Index Scan 0.13ms — top-5 rfc/sum verified identical). No separate
        // `top_active_balance` mat-view: it would duplicate this data +
        // REFRESH cost for zero gain.
        driver.execute_simple("CREATE INDEX ON credits_by_rfc_mat(sum_monto DESC)")?;
        setup_statements += 1;

        // Q5 mat-view — NEW v0.3.3 per design §8.B5 (audit fairness fix:
        // pre-v0.3.3 Q5 ran runtime aggregation against `installments`
        // ignoring the absence of any pre-aggregation; v0.3.3 adds the
        // mat-view symmetric to xyzdb's overdue_by_empresa ghost and
        // Mongo's overdue_by_empresa_agg pre-agg collection).
        driver.execute_simple(
            r#"CREATE MATERIALIZED VIEW overdue_by_empresa_mat AS
               SELECT empresa_id,
                      SUM(monto_total)::float8 AS sum_monto,
                      COUNT(*)::int8           AS n
               FROM installments
               WHERE status = 'overdue'
               GROUP BY empresa_id
               WITH NO DATA"#,
        )?;
        setup_statements += 1;
        driver.execute_simple("CREATE UNIQUE INDEX ON overdue_by_empresa_mat(empresa_id)")?;
        setup_statements += 1;

        // Q4 top-N by exposure: served by credits_by_rfc_mat +
        // `sum_monto DESC` index above (best weapon, restored). No separate
        // `top_active_balance` mat-view — the Q2 mat-view already holds the
        // exact active+overdue per-rfc aggregate Q4 needs.

        // Q8 mat-view — NEW v0.3.3 per design §8.B8 (composite 4-CTE
        // monthly close per empresa: active credits + overdue installments
        // + recent payments 30d + collection actions 30d). UNIQUE INDEX on
        // empresa_id makes REFRESH ... CONCURRENTLY legal (Phase 2.c
        // 3rd REFRESH thread, methodology gate §8.B5).
        driver.execute_simple(
            r#"CREATE MATERIALIZED VIEW monthly_close_mat AS
               WITH active_credits_per_emp AS (
                   SELECT empresa_id, COUNT(*)::int8 AS n_active
                   FROM credits
                   WHERE status IN ('active','overdue')
                   GROUP BY empresa_id
               ),
               overdue_install_per_emp AS (
                   SELECT empresa_id,
                          SUM(monto_total)::float8 AS overdue_sum,
                          COUNT(*)::int8           AS overdue_n
                   FROM installments
                   WHERE status = 'overdue'
                   GROUP BY empresa_id
               ),
               recent_pay_per_emp AS (
                   SELECT c.empresa_id,
                          SUM(p.monto)::float8 AS recent_pay_sum,
                          COUNT(*)::int8       AS recent_pay_n
                   FROM payments p
                   JOIN credits c ON c.credit_id = p.credit_id
                   WHERE p.fecha_pago >= NOW() - INTERVAL '30 days'
                   GROUP BY c.empresa_id
               ),
               col_actions_per_emp AS (
                   SELECT c.empresa_id,
                          COUNT(*)::int8 AS col_actions_n
                   FROM collection_actions ca
                   JOIN collections col ON col.collection_id = ca.collection_id
                   JOIN credits c       ON c.credit_id = col.credit_id
                   WHERE ca.fecha >= NOW() - INTERVAL '30 days'
                   GROUP BY c.empresa_id
               )
               SELECT e.empresa_id,
                      COALESCE(ac.n_active,        0) AS n_active,
                      COALESCE(oi.overdue_sum,     0) AS overdue_sum,
                      COALESCE(oi.overdue_n,       0) AS overdue_n,
                      COALESCE(rp.recent_pay_sum,  0) AS recent_pay_sum,
                      COALESCE(rp.recent_pay_n,    0) AS recent_pay_n,
                      COALESCE(ca.col_actions_n,   0) AS col_actions_n
               -- Domain = every empresa with a credit portfolio (not the full
               -- empresas catalog): matches xyzDB's GROUP BY over the creditos
               -- lobe and Mongo's group over all credits, so the three return
               -- the same empresa set. An empresa with only closed credits
               -- still appears (COALESCE 0).
               FROM (SELECT DISTINCT empresa_id FROM credits) e
               LEFT JOIN active_credits_per_emp  ac ON ac.empresa_id = e.empresa_id
               LEFT JOIN overdue_install_per_emp oi ON oi.empresa_id = e.empresa_id
               LEFT JOIN recent_pay_per_emp      rp ON rp.empresa_id = e.empresa_id
               LEFT JOIN col_actions_per_emp     ca ON ca.empresa_id = e.empresa_id
               WITH NO DATA"#,
        )?;
        setup_statements += 1;
        driver.execute_simple("CREATE UNIQUE INDEX ON monthly_close_mat(empresa_id)")?;
        setup_statements += 1;
        driver.execute_simple("CREATE INDEX ON monthly_close_mat(overdue_sum DESC)")?;
        setup_statements += 1;

        // Q9 FUNCTION — NEW v0.3.3 per design §8.B9. Single-roundtrip
        // composite JSONB return (cliente + 5 credits + 30 payments +
        // 10 collections + 50 audit + 20 notifications). Phase 2.d
        // Decision 3: LANGUAGE sql STABLE; NULL-tolerant on missing
        // rfc; no grants needed (bench user owns schema).
        driver.execute_simple(
            r#"CREATE OR REPLACE FUNCTION get_customer_360(p_rfc TEXT)
               RETURNS JSONB
               LANGUAGE sql
               STABLE
               AS $$
                   SELECT jsonb_build_object(
                       'cliente',       (SELECT row_to_json(c.*) FROM clientes c WHERE c.rfc = p_rfc),
                       -- Canonical 360: ALL of the customer's credits (the full
                       -- portfolio universe), not a sample. Activity feeds below
                       -- keep their recent-N caps.
                       'credits',       (SELECT jsonb_agg(row_to_json(x.*))
                                         FROM (SELECT * FROM credits
                                               WHERE rfc = p_rfc
                                               ORDER BY fecha_creacion DESC) x),
                       'payments',      (SELECT jsonb_agg(row_to_json(x.*))
                                         FROM (SELECT * FROM payments
                                               WHERE rfc = p_rfc
                                               ORDER BY fecha_pago DESC LIMIT 30) x),
                       'collections',   (SELECT jsonb_agg(row_to_json(x.*))
                                         FROM (SELECT col.* FROM collections col
                                               JOIN credits c ON c.credit_id = col.credit_id
                                               WHERE c.rfc = p_rfc
                                               ORDER BY col.fecha_inicio DESC LIMIT 10) x),
                       'audit',         (SELECT jsonb_agg(row_to_json(x.*))
                                         FROM (SELECT * FROM audit_log
                                               WHERE rfc = p_rfc
                                               ORDER BY fecha DESC LIMIT 50) x),
                       'notifications', (SELECT jsonb_agg(row_to_json(x.*))
                                         FROM (SELECT * FROM notifications
                                               WHERE rfc = p_rfc
                                               ORDER BY fecha DESC LIMIT 20) x)
                   )
               $$"#,
        )?;
        setup_statements += 1;
    }

    Ok(SchemaMetrics {
        mode,
        setup_statements,
        setup_duration_ms: start.elapsed().as_millis() as u64,
    })
}
