//! Q1..Q6 native Mongo forms. Intent follows design doc §4.3.
//!
//! Q3 is the customer's financial portfolio history (design v2): credits
//! (with installments / collections / collection_actions embedded) plus
//! payments — two streams. audit_log and notifications are NOT portfolio
//! (operational metadata) and belong to Q9's customer-360, so Q3 does not
//! fetch them. Sum-of-count is bench-equivalent — we do not stitch the
//! streams together since the latency we measure is the time-to-result,
//! not the post-processing.

use anyhow::Result;
use chrono::{Duration, Utc};
use mongodb::Database;
use mongodb::bson::{Document, doc};
use native_generator::bench::{BusinessQuery, QueryExecution, QueryParams};
use std::time::Instant;

/// Synchronous wrapper used by the orchestrator's per-query path.
pub fn run_one(
    driver: &super::MongoDriver,
    q: BusinessQuery,
    params: &QueryParams,
) -> Result<QueryExecution> {
    let db = driver.db()?;
    // Q7's real (credit_id, empresa_id) for the rfc (lookup outside the timed
    // region).
    let credit_empresa = driver.q7_credit_for(&params.rfc);
    let t0 = Instant::now();
    let n = driver.rt().block_on(async {
        let ce = credit_empresa
            .as_ref()
            .map(|(c, e)| (c.as_str(), e.as_str()));
        exec_async(&db, q, &params.rfc, params.limit, ce).await
    })?;
    Ok(QueryExecution {
        latency_ms: t0.elapsed().as_secs_f64() * 1000.0,
        records_returned: n,
    })
}

/// Async query executor — used both by the synchronous path above and by
/// the concurrent reader threads (each on its own runtime).
pub async fn exec_async(
    db: &Database,
    q: BusinessQuery,
    rfc: &str,
    limit: usize,
    credit_empresa: Option<(&str, &str)>,
) -> Result<u64> {
    use futures_util::TryStreamExt;
    Ok(match q {
        BusinessQuery::Q1Point => {
            let coll: mongodb::Collection<Document> = db.collection("clients");
            let r = coll.find_one(doc! { "_id": rfc }).await?;
            r.is_some() as u64
        }
        BusinessQuery::Q2Aggregate => {
            // Current exposure = count of the client's active/overdue credits
            // (`n_active`), matching xyzDB's filtered count() and PG's
            // n_creditos. Reading `n_credits` (all statuses) was the wrong
            // number for Q2.
            let coll: mongodb::Collection<Document> = db.collection("credits_by_rfc");
            let r = coll.find_one(doc! { "_id": rfc }).await?;
            r.as_ref()
                .and_then(|d| d.get_i32("n_active").ok())
                .map(|n| n as u64)
                .unwrap_or(0)
        }
        BusinessQuery::Q3FullHistory => {
            // 2 round trips — the customer's financial portfolio history
            // (design v2 Q3). credits returns the credit doc with
            // installments / collections / collection_actions embedded (one
            // fetch ≡ PG's JOINs across those four); payments is a separate
            // stream. audit_log and notifications are NOT portfolio
            // (operational metadata) — they belong to Q9's customer-360.
            let mut total: u64 = 0;
            let credits: mongodb::Collection<Document> = db.collection("credits");
            let mut cur = credits
                .find(doc! { "rfc": rfc })
                .sort(doc! { "fechaCreacion": 1 })
                .limit(5_000)
                .await?;
            while cur.try_next().await?.is_some() {
                total += 1;
            }

            let payments: mongodb::Collection<Document> = db.collection("payments");
            let mut cur = payments
                .find(doc! { "rfc": rfc })
                .sort(doc! { "fechaPago": 1 })
                .limit(5_000)
                .await?;
            while cur.try_next().await?.is_some() {
                total += 1;
            }

            // Bench reports min(total, 5_000) to match PG's `LIMIT 5000`
            // on the UNION ALL.
            total.min(5_000)
        }
        BusinessQuery::Q4TopExposure => {
            let n = limit as i64;
            // Best weapon: read the `top_active_balance` pre-agg by its
            // {sum_monto:-1} index (explain: docsExamined == limit).
            let coll: mongodb::Collection<Document> = db.collection("top_active_balance");
            let mut cur = coll
                .find(doc! {})
                .sort(doc! { "sum_monto": -1 })
                .limit(n)
                .await?;
            let mut total: u64 = 0;
            while cur.try_next().await?.is_some() {
                total += 1;
            }
            total
        }
        BusinessQuery::Q5OverdueByEmpresa => {
            let coll: mongodb::Collection<Document> = db.collection("overdue_by_empresa_agg");
            let mut cur = coll.find(doc! {}).sort(doc! { "sum_monto": -1 }).await?;
            let mut total: u64 = 0;
            while cur.try_next().await?.is_some() {
                total += 1;
            }
            total
        }
        BusinessQuery::Q6RecentPayments => {
            // Covering compound index { fechaPago: -1, monto: 1 }.
            let coll: mongodb::Collection<Document> = db.collection("payments");
            let cutoff = Utc::now() - Duration::days(30);
            let cutoff_bson = mongodb::bson::DateTime::from_millis(cutoff.timestamp_millis());
            let n = limit as i64;
            let mut cur = coll
                .find(doc! {
                    "monto": { "$gt": 50_000.0 },
                    "fechaPago": { "$gte": cutoff_bson },
                })
                .sort(doc! { "fechaPago": -1 })
                .limit(n)
                .await?;
            let mut total: u64 = 0;
            while cur.try_next().await?.is_some() {
                total += 1;
            }
            total
        }
        BusinessQuery::Q7BatchIngest => {
            // Q7 — Batch insert 100 payments per design §7.8 + §8.C7.
            // `insertMany` with `ordered:false` allows partial completion
            // on duplicate-key collisions; optimises write pipeline.
            //
            // The payment borrows a real (credit_id, empresa_id) of the rfc so
            // the doc is shaped like the real payment docs (empresa_id
            // denormalised, see payment_doc), and an old fechaPago (400 days)
            // keeps it OUT of Q8's recent-pay `$match` (fechaPago >= cutoff) in
            // every engine. Q7 measures ingest latency, not analytical freshness.
            // Fallback (never fires for a sampled client) keeps a valid batch.
            let (credit_id, empresa_id) = credit_empresa.unwrap_or(("CR_Q7_BENCH", "EMP_UNKNOWN"));
            let coll: mongodb::Collection<Document> = db.collection("payments");
            let now = mongodb::bson::DateTime::now();
            let now_ms = now.timestamp_millis();
            let old_fecha = mongodb::bson::DateTime::from_millis(now_ms - 400 * 86_400_000);
            let mut docs: Vec<Document> = Vec::with_capacity(100);
            for i in 0..100 {
                docs.push(doc! {
                    "_id": format!("Q7_PAY_{}_{}", now_ms, i),
                    "credit_id": credit_id,
                    "rfc": rfc,
                    "empresa_id": empresa_id,
                    "monto": 100.0,
                    "fechaPago": old_fecha,
                    "metodo": "bench",
                });
            }
            let opts = mongodb::options::InsertManyOptions::builder()
                .ordered(false)
                .build();
            match coll.insert_many(docs).with_options(opts).await {
                Ok(r) => r.inserted_ids.len() as u64,
                Err(_) => 0,
            }
        }
        BusinessQuery::Q8MonthlyClose => {
            let coll: mongodb::Collection<Document> = db.collection("monthly_close_agg");
            let mut cur = coll.find(doc! {}).sort(doc! { "overdue_sum": -1 }).await?;
            let mut total: u64 = 0;
            while cur.try_next().await?.is_some() {
                total += 1;
            }
            total
        }
        BusinessQuery::Q9CustomerContext => {
            // Q9 — Phase 2.d Decision 1 path β: `$lookup` runtime
            // aggregation, NO pre-agg `customer_360` collection.
            // Single-pipeline server-side stitch — symmetric with PG
            // `get_customer_360` FUNCTION single-roundtrip; avoids 4th
            // `$merge` cadence thread (methodology gate §8.C5 budget
            // preserved at 3 threads). Engine-idiom cross-reference:
            // xyzDB Q9 issues a 3-call sequence (FIND + 1 SCAN per lobe,
            // each walking the whole rfc gravity bucket) relying on gravity
            // co-location for in-call cache reuse (no driver-side warm
            // wrapper); Mongo's single-pipeline `$lookup` is the
            // structurally distinct idiom on the same business question.
            // Each engine's Q9 path matches its idiomatic single-
            // roundtrip primitive — defensible-fair §2.
            let clients: mongodb::Collection<Document> = db.collection("clients");
            let pipeline = vec![
                doc! { "$match": { "_id": rfc } },
                doc! { "$lookup": {
                    "from": "credits",
                    "localField": "_id",
                    "foreignField": "rfc",
                    "as": "credits",
                } },
                doc! { "$lookup": {
                    "from": "payments",
                    "localField": "_id",
                    "foreignField": "rfc",
                    "as": "payments",
                } },
                doc! { "$lookup": {
                    "from": "audit_log",
                    "localField": "_id",
                    "foreignField": "rfc",
                    "as": "audit",
                } },
                doc! { "$lookup": {
                    "from": "notifications",
                    "localField": "_id",
                    "foreignField": "rfc",
                    "as": "notifications",
                } },
                // Canonical 360: ALL of the customer's credits (the full
                // portfolio universe), not a sample — so the collections
                // embedded in each credit (Mongo strategy A) span every credit,
                // not just a sliced subset. Activity feeds keep their recent-N
                // caps (payments 30, audit 50, notifications 20). No $unwind —
                // the embedded read is Mongo's idiom and stays untouched.
                doc! { "$project": {
                    "cliente": "$$ROOT",
                    "credits": "$credits",
                    "payments": { "$slice": ["$payments", 30] },
                    "audit": { "$slice": ["$audit", 50] },
                    "notifications": { "$slice": ["$notifications", 20] },
                } },
            ];
            let mut cur = clients.aggregate(pipeline).await?;
            let mut total: u64 = 0;
            while cur.try_next().await?.is_some() {
                total += 1;
            }
            total
        }
    })
}

// Phase 3 v0.3.3: legacy `writer_insert_payment` removed. The separate
// writer thread (v0.2.5 + Phase 2.b) is collapsed into mixed-mode
// errática threads (per design §6.4 state-dependent R/W mix). Q7 batch
// ingest (`exec_async`'s Q7BatchIngest arm) replaces the single-doc
// `payments.insertOne` path. (Q10 transactional cascade removed — deferred
// on Mongo standalone / xyzDB, PG-only, dropped from the bench.)
