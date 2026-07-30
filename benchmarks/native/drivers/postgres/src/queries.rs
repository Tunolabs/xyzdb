//! Q1..Q6 native SQL forms for PostgreSQL.

use anyhow::Result;
use native_generator::bench::{BusinessQuery, QueryExecution, QueryParams};
use std::time::Instant;
use tokio_postgres::Client;

/// Synchronous wrapper used by the orchestrator's per-query path.
pub fn run_one(
    driver: &super::PostgresDriver,
    q: BusinessQuery,
    params: &QueryParams,
) -> Result<QueryExecution> {
    driver.ensure_client()?;
    let mut g = driver.client.lock().unwrap();
    let client = g.as_mut().unwrap();

    // Q7's real credit_id for the rfc (lookup + clone outside the timed region).
    let credit_for_rfc = driver.q7_credit_for(&params.rfc);
    let t0 = Instant::now();
    let n = driver.rt().block_on(async {
        exec_async(
            client,
            q,
            &params.rfc,
            params.limit,
            credit_for_rfc.as_deref(),
        )
        .await
    })?;
    Ok(QueryExecution {
        latency_ms: t0.elapsed().as_secs_f64() * 1000.0,
        records_returned: n,
    })
}

/// Async query executor — used both by the synchronous path above and by
/// the concurrent reader threads (each on its own runtime).
pub async fn exec_async(
    client: &mut Client,
    q: BusinessQuery,
    rfc: &str,
    limit: usize,
    credit_for_rfc: Option<&str>,
) -> Result<u64> {
    Ok(match q {
        BusinessQuery::Q1Point => {
            let rows = client
                .query(
                    "SELECT rfc, curp, nombre FROM clientes WHERE rfc = $1",
                    &[&rfc],
                )
                .await?;
            rows.len() as u64
        }
        BusinessQuery::Q2Aggregate => {
            // Read pre-aggregated mat-view (per §8.B2).
            let rows = client
                .query(
                    "SELECT sum_monto, n_creditos FROM credits_by_rfc_mat WHERE rfc = $1",
                    &[&rfc],
                )
                .await?;
            rows.get(0)
                .map(|r| r.get::<_, i64>("n_creditos") as u64)
                .unwrap_or(0)
        }
        BusinessQuery::Q3FullHistory => {
            // 5-source UNION ALL with ORDER BY fecha — the customer's financial
            // portfolio history (design v2 Q3): credits + installments +
            // payments + collections + collection_actions. audit_log and
            // notifications are NOT portfolio (operational metadata) — they
            // belong to Q9's customer-360, not here.
            let rows = client
                .query(
                    r#"
                    WITH client_credits AS (
                      SELECT credit_id FROM credits WHERE rfc = $1
                    )
                    SELECT 'Credit'::text AS kind, c.fecha_creacion AS fecha
                    FROM credits c WHERE c.rfc = $1
                    UNION ALL
                    SELECT 'Installment'::text, i.fecha_vencimiento::timestamptz
                    FROM installments i JOIN client_credits cc ON cc.credit_id = i.credit_id
                    UNION ALL
                    SELECT 'Payment'::text, p.fecha_pago
                    FROM payments p JOIN client_credits cc ON cc.credit_id = p.credit_id
                    UNION ALL
                    SELECT 'Collection'::text, col.fecha_inicio
                    FROM collections col JOIN client_credits cc ON cc.credit_id = col.credit_id
                    UNION ALL
                    SELECT 'CollectionAction'::text, ca.fecha
                    FROM collection_actions ca
                    JOIN collections col ON col.collection_id = ca.collection_id
                    JOIN client_credits cc ON cc.credit_id = col.credit_id
                    ORDER BY fecha ASC
                    LIMIT 5000
                    "#,
                    &[&rfc],
                )
                .await?;
            rows.len() as u64
        }
        BusinessQuery::Q4TopExposure => {
            let limit_i = limit as i64;
            // Best weapon: top-N read off credits_by_rfc_mat via its
            // sum_monto DESC index (EXPLAIN: Index Scan, 0.13ms).
            let rows = client
                .query(
                    "SELECT rfc, sum_monto, n_creditos FROM credits_by_rfc_mat \
                     ORDER BY sum_monto DESC LIMIT $1",
                    &[&limit_i],
                )
                .await?;
            rows.len() as u64
        }
        BusinessQuery::Q5OverdueByEmpresa => {
            let rows = client
                .query(
                    "SELECT empresa_id, sum_monto, n FROM overdue_by_empresa_mat ORDER BY sum_monto DESC",
                    &[],
                )
                .await?;
            rows.len() as u64
        }
        BusinessQuery::Q6RecentPayments => {
            let limit_i = limit as i64;
            let rows = client
                .query(
                    "SELECT rfc, monto, credit_id, fecha_pago FROM payments
                     WHERE monto > 50000 AND fecha_pago >= NOW() - INTERVAL '30 days'
                     ORDER BY fecha_pago DESC
                     LIMIT $1",
                    &[&limit_i],
                )
                .await?;
            rows.len() as u64
        }
        BusinessQuery::Q7BatchIngest => {
            // Q7 — Batch insert 100 payments per design §7.8 + §8.B7.
            // Multi-row VALUES is the most-efficient PG batch-insert
            // primitive without `COPY` (audit Section 7 finding 10).
            //
            // The payment borrows a real credit_id of the rfc ($2) so PG's Q3/Q8
            // join (`payments.credit_id = credits.credit_id`) resolves it into
            // the credit's empresa; an old fecha_pago (`NOW() - 400 days`) keeps
            // it OUT of Q8's 30-day cobrado window in every engine (same date
            // filter). Q7 measures ingest latency, not analytical freshness.
            // Fallback credit_id (never fires for a sampled client) is dropped by
            // the inner join, matching the pre-fix behaviour for that edge.
            let credit_id = credit_for_rfc.unwrap_or("CR_Q7_BENCH");
            let now_ms = chrono::Utc::now().timestamp_millis();
            let mut sql = String::with_capacity(8 * 1024);
            sql.push_str(
                "INSERT INTO payments (payment_id, credit_id, rfc, monto, fecha_pago, metodo) VALUES ",
            );
            for i in 0..100 {
                if i > 0 {
                    sql.push(',');
                }
                sql.push_str(&format!(
                    "('Q7_PAY_{}_{}', $2, $1, 100.0, NOW() - INTERVAL '400 days', 'bench')",
                    now_ms, i
                ));
            }
            sql.push_str(" ON CONFLICT DO NOTHING");
            let rows_affected = client.execute(sql.as_str(), &[&rfc, &credit_id]).await?;
            rows_affected
        }
        BusinessQuery::Q8MonthlyClose => {
            // Read pre-aggregated mat-view (4-CTE composite refreshed by
            // Phase 3 cadence per §8.B8).
            let rows = client
                .query(
                    "SELECT * FROM monthly_close_mat ORDER BY overdue_sum DESC",
                    &[],
                )
                .await?;
            rows.len() as u64
        }
        BusinessQuery::Q9CustomerContext => {
            // Single-roundtrip via PL/pgSQL FUNCTION (§8.B9).
            let rows = client.query("SELECT get_customer_360($1)", &[&rfc]).await?;
            rows.len() as u64
        }
    })
}
