//! Native MongoDB 8 driver for the native cross-engine bench.
//!
//! Schema, indexes, collections and queries follow the native
//! cross-engine bench design.
//!
//! Embedding strategy A: the bulk-load path performs a streaming merge
//! of the credits / installments / collections / collection_actions
//! generator streams (all four emit grouped by `credit_ord`), assembling
//! one embedded credit doc at a time without ever holding more than the
//! current credit's fanout in memory. Per-credit footprint is bounded
//! by the dataset's deterministic counts (~25 installments × 0.3
//! collections × 3 actions per collection) — well within the 16 MB
//! BSON limit.
//!
//! Bulk-load primitive: `insert_many` with `ordered=false`,
//! write_concern `{ w: 1, j: true }` (matches PG `synchronous_commit=on`
//! and xyzDB `--durability durable`). Refresh thread for Phase 3 runs
//! `$merge`-style aggregation pipelines on the cadences configured by
//! the orchestrator (Phase 2.b v0.3.3: 30 s for `overdue_by_empresa_agg`
//! Q5 NEW per design §8.C5; 60 s for `credits_by_rfc` Q2 carried v0.2.5)
//! and accumulates wall-clock + counter as the Mongo maintenance tax.
//! Pipeline definitions live alongside this file so the concurrent
//! runner reuses them.

use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDate, NaiveTime, TimeZone, Utc};
use futures_util::TryStreamExt;
use mongodb::bson::{self, Bson, Document, doc};
use mongodb::options::{ClientOptions, WriteConcern};
use mongodb::{Client as MongoClient, Collection as MongoColl, Database};
use native_generator::bench::*;
use native_generator::model::{
    AuditLogEntry, BiSnapshot, Client, Collection, CollectionAction, Credit, CreditApplication,
    Empresa, Installment, Notification, Payment, Producto,
};
use native_generator::{
    Dataset, ExpectedCounts, GoldenDiff, GoldenFile, GoldenVerifyResults, compare_count,
    compare_count_sum,
};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::runtime::Runtime;
use tracing::{info, warn};

mod queries;
mod schema;

/// `insert_many` batch size. The 16 MB BSON limit and the per-doc size
/// (~30 KB for an embedded credit, ~500 B for a payment) bound this.
/// 1000 docs × 30 KB ≈ 30 MB → split into 500-doc batches for credits.
const BATCH_CREDITS: usize = 500;
const BATCH_FLAT: usize = 5_000;

pub struct MongoDriver {
    uri: String,
    db_name: String,
    rt: Runtime,
    client: Mutex<Option<MongoClient>>,
    /// `rfc -> (credit_id, empresa_id)` of one real credit per client, built at
    /// load. Q7's synthetic payments borrow the real empresa_id (so Q8's
    /// per-empresa `$group` never sees a null-empresa) and credit_id (parity
    /// with the real payment docs), with an old fechaPago so the recent-pay
    /// `$match` excludes them — symmetric with xyzDB/PG.
    rfc_first_credit: OnceLock<HashMap<String, (String, String)>>,
}

impl MongoDriver {
    /// Build a MongoDB driver. The connection is established lazily on
    /// first phase call so that constructing the driver does not block
    /// on container readiness.
    pub fn new(uri: impl Into<String>, db_name: impl Into<String>) -> Result<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(4)
            .build()
            .context("build tokio runtime for Mongo driver")?;
        Ok(Self {
            uri: uri.into(),
            db_name: db_name.into(),
            rt,
            client: Mutex::new(None),
            rfc_first_credit: OnceLock::new(),
        })
    }

    /// One real `(credit_id, empresa_id)` for `rfc`, if the load-time map is
    /// populated. Q7 uses it so its synthetic payments carry a real empresa (and
    /// credit). `None` before load or for an unknown rfc.
    pub(crate) fn q7_credit_for(&self, rfc: &str) -> Option<(String, String)> {
        self.rfc_first_credit
            .get()
            .and_then(|m| m.get(rfc).cloned())
    }

    fn ensure_client(&self) -> Result<()> {
        let mut g = self.client.lock().unwrap();
        if g.is_none() {
            let uri = self.uri.clone();
            let client = self.rt.block_on(async move {
                let mut opts = ClientOptions::parse(&uri)
                    .await
                    .context("parse Mongo URI")?;
                // Match PG `synchronous_commit=on` + xyzDB `--durability durable`.
                opts.write_concern = Some(
                    WriteConcern::builder()
                        .w(mongodb::options::Acknowledgment::Nodes(1))
                        .journal(true)
                        .build(),
                );
                // HDD bulk-load tolerance. WiredTiger checkpoints on
                // physical HDDs can pause mongod for tens of seconds
                // while flushing pages. The driver's default monitor
                // (10s heartbeat / 30s serverSelection) tears the
                // connection during such pauses, killing the bulk
                // insert mid-flight (observed twice on Bench A Scale
                // 1.0 HDD: insert_many → "server monitor timeout").
                // Widen the windows so the driver tolerates HDD-class
                // tail latency. Values are conservative; SSD runs are
                // unaffected because heartbeats complete fast there.
                opts.heartbeat_freq = Some(std::time::Duration::from_secs(30));
                opts.server_selection_timeout = Some(std::time::Duration::from_secs(600));
                opts.connect_timeout = Some(std::time::Duration::from_secs(60));
                MongoClient::with_options(opts).context("Mongo client")
            })?;
            *g = Some(client);
        }
        Ok(())
    }

    pub(crate) fn db(&self) -> Result<Database> {
        self.ensure_client()?;
        let g = self.client.lock().unwrap();
        let client = g.as_ref().expect("ensured");
        Ok(client.database(&self.db_name))
    }

    pub(crate) fn rt(&self) -> &Runtime {
        &self.rt
    }

    pub(crate) fn uri(&self) -> &str {
        &self.uri
    }

    pub(crate) fn db_name(&self) -> &str {
        &self.db_name
    }
}

impl NativeDriver for MongoDriver {
    fn kind(&self) -> EngineKind {
        EngineKind::Mongo
    }

    fn setup_schema(&self, mode: SchemaMode) -> Result<SchemaMetrics> {
        let db = self.db()?;
        self.rt
            .block_on(async move { schema::setup(&db, mode).await })
    }

    fn bulk_load(&self, dataset: &Dataset) -> Result<LoadMetrics> {
        info!(target: "mongo", "Phase 1: bulk-loading scale={}", dataset.scale);
        let start = Instant::now();
        let db = self.db()?;
        let mut total: u64 = 0;

        total += self.rt.block_on(load_empresas(&db, dataset))?;
        total += self.rt.block_on(load_productos(&db, dataset))?;
        total += self.rt.block_on(load_clients(&db, dataset))?;
        // C-11 Path A: load_credits_embedded returns the credit_id ↦
        // empresa_id mapping it builds in-flight; load_payments uses it
        // to denormalise empresa_id into each payment doc, removing the
        // need for `$lookup` in monthly_close_agg sub-pipeline 3.
        let (n_credits, credit_to_empresa) =
            self.rt.block_on(load_credits_embedded(&db, dataset))?;
        total += n_credits;
        // rfc -> one real (credit_id, empresa_id), for Q7's synthetic payments.
        let _ = self
            .rfc_first_credit
            .set(build_rfc_first_credit_map(dataset));
        total += self
            .rt
            .block_on(load_payments(&db, dataset, &credit_to_empresa))?;
        total += self.rt.block_on(load_applications(&db, dataset))?;
        total += self.rt.block_on(load_audit(&db, dataset))?;
        total += self.rt.block_on(load_notifications(&db, dataset))?;
        total += self.rt.block_on(load_bi(&db, dataset))?;

        let dur = start.elapsed();
        let rate = total as f64 / dur.as_secs_f64();
        info!(target: "mongo", "Phase 1 done: {} records in {:?} ({:.0} rec/s)",
              total, dur, rate);
        Ok(LoadMetrics {
            records_loaded: total,
            duration_ms: dur.as_millis() as u64,
            records_per_sec: rate,
        })
    }

    fn post_load(&self) -> Result<()> {
        info!(target: "mongo", "Phase 0.5: initial $merge materialisation");
        let db = self.db()?;
        self.rt.block_on(async move {
            // Initial materialisation of the two pre-aggregation
            // collections. Cadenced refreshes during Phase 3 reuse
            // the same pipelines.
            run_merge_credits_by_rfc(&db).await?;
            run_merge_top_active_balance(&db).await?;
            run_merge_overdue_by_empresa(&db).await?;
            run_merge_monthly_close(&db).await?;
            anyhow::Ok(())
        })
    }

    fn run_query(&self, query: BusinessQuery, params: &QueryParams) -> Result<QueryExecution> {
        queries::run_one(self, query, params)
    }

    fn run_concurrent(
        &self,
        profile: &ConcurrentProfile,
        rfc_pool: &[String],
    ) -> Result<ConcurrentResults> {
        run_concurrent_workload(self, profile, rfc_pool)
    }

    fn verify(&self, expected: &ExpectedCounts) -> Result<VerifyResults> {
        info!(target: "mongo", "Phase 5: integrity verify");
        let db = self.db()?;
        let pairs: [(&str, u64); 9] = [
            ("empresas", expected.empresas),
            ("productos", expected.productos),
            ("clients", expected.clients),
            ("credits", expected.credits),
            ("payments", expected.payments),
            ("credit_applications", expected.applications),
            ("audit_log", expected.audit_log),
            ("notifications", expected.notifications),
            ("bi_snapshots", expected.bi_snapshots),
        ];
        let mut diffs = Vec::new();
        let observed = self.rt.block_on(async {
            let mut out = Vec::with_capacity(pairs.len());
            for (name, _exp) in &pairs {
                let coll: MongoColl<Document> = db.collection(*name);
                let n = coll
                    .count_documents(doc! {})
                    .await
                    .with_context(|| format!("count {name}"))?;
                out.push((*name, n));
            }
            anyhow::Ok(out)
        })?;
        for (i, (name, n)) in observed.iter().enumerate() {
            let exp = pairs[i].1;
            if *n != exp {
                diffs.push(EntityDiff {
                    entity: (*name).to_string(),
                    expected: exp,
                    observed: *n,
                });
            }
        }
        // installments + collections + collection_actions are embedded
        // inside credit docs for Mongo; their cardinality is implicit in
        // the credit count and not separately verifiable without an
        // expensive $unwind. Bench A treats credits-count exactness as
        // sufficient for Mongo (consistent with design §4.6 — embedded
        // fanout is bounded and deterministic by generator contract).
        Ok(VerifyResults {
            exact: diffs.is_empty(),
            diffs,
        })
    }

    fn verify_golden(&self, golden: &GoldenFile) -> Result<GoldenVerifyResults> {
        info!(target: "mongo", "Phase 1.5: verify_golden vs seed={} scale={}",
              golden.seed, golden.scale);
        let db = self.db()?;
        let mut diffs: Vec<GoldenDiff> = Vec::new();
        let tol = golden.tolerance_f64_relative;

        let outcomes = self.rt.block_on(async {
            // V1 — credits collection: count + sum(monto).
            let v1 = mongo_count_sum(
                &db,
                "credits",
                vec![doc! { "$group": { "_id": null, "n": { "$sum": 1 }, "s": { "$sum": "$monto" } } }],
            )
            .await?;
            // V2 — installments overdue: unwind + match + group.
            let v2 = mongo_count_sum(
                &db,
                "credits",
                vec![
                    doc! { "$unwind": "$installments" },
                    doc! { "$match": { "installments.status": "overdue" } },
                    doc! { "$group": { "_id": null, "n": { "$sum": 1 }, "s": { "$sum": "$installments.monto_total" } } },
                ],
            )
            .await?;
            // V3 — payments collection: count + sum(monto).
            let v3 = mongo_count_sum(
                &db,
                "payments",
                vec![doc! { "$group": { "_id": null, "n": { "$sum": 1 }, "s": { "$sum": "$monto" } } }],
            )
            .await?;

            // V5 — clients distinct rfc. `clients._id` is the rfc itself
            // (bulk_load contract — see schema.rs / mongo bulk_load).
            // distinct over `_id` returns the cardinality directly without
            // a $group pipeline.
            let coll: MongoColl<Document> = db.collection("clients");
            let v5 = coll
                .count_documents(doc! {})
                .await
                .context("count clients")?;

            // V6 — empresas + productos.
            let v6_emp = db
                .collection::<Document>("empresas")
                .count_documents(doc! {})
                .await
                .context("count empresas")?;
            let v6_prod = db
                .collection::<Document>("productos")
                .count_documents(doc! {})
                .await
                .context("count productos")?;

            // V4 — embedded-aware counts. Pipelines below use $sum {$size}
            // to avoid materialising every embedded element.
            let v4_inst_total = mongo_sum_size(&db, "credits", "$installments").await?;
            let v4_coll_total = mongo_sum_size(&db, "credits", "$collections").await?;
            // collection_actions are nested under collections[].actions —
            // double $unwind required to count.
            let v4_actions_total = mongo_count_double_unwind(&db).await?;

            anyhow::Ok((v1, v2, v3, v5, v6_emp, v6_prod, v4_inst_total, v4_coll_total, v4_actions_total))
        })?;
        let (v1, v2, v3, v5, v6_emp, v6_prod, v4_inst_total, v4_coll_total, v4_actions_total) =
            outcomes;

        compare_count_sum(
            "V1_credits_total",
            &golden.verify_queries.v1_credits_total,
            v1.0,
            v1.1,
            tol,
            &mut diffs,
        );
        compare_count_sum(
            "V2_installments_overdue",
            &golden.verify_queries.v2_installments_overdue,
            v2.0,
            v2.1,
            tol,
            &mut diffs,
        );
        compare_count_sum(
            "V3_payments_total",
            &golden.verify_queries.v3_payments_total,
            v3.0,
            v3.1,
            tol,
            &mut diffs,
        );
        compare_count(
            "V5_clients_distinct_rfc",
            &golden.verify_queries.v5_clients_distinct_rfc,
            v5,
            &mut diffs,
        );

        // V4 — Mongo's lobe×type mapping over embedded shape.
        let v4 = &golden.verify_queries.v4_lobe_type_counts;
        // clientes._total
        for (typ, &exp) in v4.clientes.iter() {
            push_v4_diff_if_mismatch("clientes", typ, exp, v5, &mut diffs);
        }
        // creditos.{Credit,Installment,Payment,Collection,CollectionAction}
        for (typ, &exp) in v4.creditos.iter() {
            let observed = match typ.as_str() {
                "Credit" => v1.0,
                "Installment" => v4_inst_total,
                "Payment" => v3.0,
                "Collection" => v4_coll_total,
                "CollectionAction" => v4_actions_total,
                _ => continue,
            };
            push_v4_diff_if_mismatch("creditos", typ, exp, observed, &mut diffs);
        }
        // operaciones.{CreditApplication,AuditLog,Notification} — flat collections.
        let (v4_apps, v4_audit, v4_notif) = self.rt.block_on(async {
            let a = db
                .collection::<Document>("credit_applications")
                .count_documents(doc! {})
                .await?;
            let b = db
                .collection::<Document>("audit_log")
                .count_documents(doc! {})
                .await?;
            let c = db
                .collection::<Document>("notifications")
                .count_documents(doc! {})
                .await?;
            anyhow::Ok((a, b, c))
        })?;
        for (typ, &exp) in v4.operaciones.iter() {
            let observed = match typ.as_str() {
                "CreditApplication" => v4_apps,
                "AuditLog" => v4_audit,
                "Notification" => v4_notif,
                _ => continue,
            };
            push_v4_diff_if_mismatch("operaciones", typ, exp, observed, &mut diffs);
        }
        // configuracion.{Empresa,Producto}
        for (typ, &exp) in v4.configuracion.iter() {
            let observed = match typ.as_str() {
                "Empresa" => v6_emp,
                "Producto" => v6_prod,
                _ => continue,
            };
            push_v4_diff_if_mismatch("configuracion", typ, exp, observed, &mut diffs);
        }
        // bi._total
        let v4_bi = self.rt.block_on(async {
            db.collection::<Document>("bi_snapshots")
                .count_documents(doc! {})
                .await
        })?;
        for (typ, &exp) in v4.bi.iter() {
            push_v4_diff_if_mismatch("bi", typ, exp, v4_bi, &mut diffs);
        }

        // V6 totals
        if v6_emp != golden.verify_queries.v6_config_counts.empresas {
            let exp = golden.verify_queries.v6_config_counts.empresas as f64;
            diffs.push(GoldenDiff {
                query: "V6_config:empresas".to_string(),
                field: "n".to_string(),
                expected: exp,
                observed: v6_emp as f64,
                relative_delta: (v6_emp as f64 - exp).abs() / exp.max(1.0),
            });
        }
        if v6_prod != golden.verify_queries.v6_config_counts.productos {
            let exp = golden.verify_queries.v6_config_counts.productos as f64;
            diffs.push(GoldenDiff {
                query: "V6_config:productos".to_string(),
                field: "n".to_string(),
                expected: exp,
                observed: v6_prod as f64,
                relative_delta: (v6_prod as f64 - exp).abs() / exp.max(1.0),
            });
        }
        let v6_total = v6_emp + v6_prod;
        if v6_total != golden.verify_queries.v6_config_counts.total {
            let exp = golden.verify_queries.v6_config_counts.total as f64;
            diffs.push(GoldenDiff {
                query: "V6_config:_total".to_string(),
                field: "n".to_string(),
                expected: exp,
                observed: v6_total as f64,
                relative_delta: (v6_total as f64 - exp).abs() / exp.max(1.0),
            });
        }

        info!(target: "mongo", "Phase 1.5 done: {} diffs", diffs.len());
        Ok(GoldenVerifyResults {
            overall_match: diffs.is_empty(),
            diffs,
        })
    }
}

// ── verify_golden helpers ────────────────────────────────────────────

/// Run an aggregation pipeline ending in `$group {_id:null, n, s}` and
/// extract the (n, s) tuple. Empty result set returns (0, 0.0) — that
/// surfaces as a count mismatch downstream rather than a hard error.
async fn mongo_count_sum(db: &Database, coll: &str, pipeline: Vec<Document>) -> Result<(u64, f64)> {
    let coll: MongoColl<Document> = db.collection(coll);
    let mut cur = coll.aggregate(pipeline).await.with_context(|| {
        format!(
            "aggregate count+sum on {}",
            std::any::type_name::<Document>()
        )
    })?;
    if let Some(doc) = cur.try_next().await? {
        let n = doc
            .get_i64("n")
            .or_else(|_| doc.get_i32("n").map(|v| v as i64))
            .unwrap_or(0) as u64;
        let s = doc
            .get_f64("s")
            .or_else(|_| doc.get_i64("s").map(|v| v as f64))
            .or_else(|_| doc.get_i32("s").map(|v| v as f64))
            .unwrap_or(0.0);
        return Ok((n, s));
    }
    Ok((0, 0.0))
}

/// Sum the size of an embedded array across all docs of a collection.
/// Used for Mongo's installments + collections counts (single-level
/// embedding under credits).
async fn mongo_sum_size(db: &Database, coll: &str, array_path: &str) -> Result<u64> {
    let coll: MongoColl<Document> = db.collection(coll);
    let pipeline = vec![doc! {
        "$group": {
            "_id": null,
            "n": { "$sum": { "$size": { "$ifNull": [array_path, []] } } },
        }
    }];
    let mut cur = coll
        .aggregate(pipeline)
        .await
        .context("aggregate sum-size")?;
    if let Some(doc) = cur.try_next().await? {
        let n = doc
            .get_i64("n")
            .or_else(|_| doc.get_i32("n").map(|v| v as i64))
            .unwrap_or(0) as u64;
        return Ok(n);
    }
    Ok(0)
}

/// Count `collections[].actions[]` across all credits. Two-level embed
/// requires a $unwind chain since $size doesn't traverse nested arrays.
async fn mongo_count_double_unwind(db: &Database) -> Result<u64> {
    let coll: MongoColl<Document> = db.collection("credits");
    let pipeline = vec![
        doc! { "$unwind": { "path": "$collections", "preserveNullAndEmptyArrays": false } },
        doc! { "$unwind": { "path": "$collections.actions", "preserveNullAndEmptyArrays": false } },
        doc! { "$count": "n" },
    ];
    let mut cur = coll
        .aggregate(pipeline)
        .await
        .context("aggregate double-unwind count")?;
    if let Some(doc) = cur.try_next().await? {
        let n = doc
            .get_i64("n")
            .or_else(|_| doc.get_i32("n").map(|v| v as i64))
            .unwrap_or(0) as u64;
        return Ok(n);
    }
    Ok(0)
}

fn push_v4_diff_if_mismatch(
    lobe: &str,
    typ: &str,
    expected: u64,
    observed: u64,
    diffs: &mut Vec<GoldenDiff>,
) {
    if observed != expected {
        let exp = expected as f64;
        diffs.push(GoldenDiff {
            query: format!("V4_lobe_type:{lobe}:{typ}"),
            field: "n".to_string(),
            expected: exp,
            observed: observed as f64,
            relative_delta: (observed as f64 - exp).abs() / exp.max(1.0),
        });
    }
}

// ── pre-aggregation pipelines (shared between post_load and Phase 3 refresh) ─

// Q5 NEW v0.3.3 — `$merge`-maintained `overdue_by_empresa_agg` per
// design §8.C5. Audit fairness fix: pre-v0.3.3 Mongo Q5 ran runtime
// `$unwind` aggregation (1-10s anchor at Scale 1.0); v0.3.3 adds the
// pre-agg collection symmetric to PG `overdue_by_empresa_mat` and
// xyzdb `overdue_by_empresa` ghost.
pub(crate) async fn run_merge_overdue_by_empresa(db: &Database) -> Result<()> {
    let coll: MongoColl<Document> = db.collection("credits");
    let pipeline = vec![
        doc! { "$match": { "installments.status": "overdue" } },
        doc! { "$unwind": "$installments" },
        doc! { "$match": { "installments.status": "overdue" } },
        doc! { "$group": {
            "_id": "$empresa_id",
            "sum_monto": { "$sum": "$installments.monto_total" },
            "n": { "$sum": 1 },
        } },
        doc! { "$merge": {
            "into": "overdue_by_empresa_agg",
            "on": "_id",
            "whenMatched": "replace",
            "whenNotMatched": "insert",
        } },
    ];
    let mut cursor = coll
        .aggregate(pipeline)
        .await
        .context("aggregate overdue_by_empresa_agg")?;
    while cursor.try_next().await?.is_some() {}
    Ok(())
}

pub(crate) async fn run_merge_credits_by_rfc(db: &Database) -> Result<()> {
    let coll: MongoColl<Document> = db.collection("credits");
    // Q2 = current exposure of one client = the count of that client's
    // active/overdue credits (`n_active`), matching xyzDB's `count() WHERE
    // status IN [active, overdue]` and PG's credits_by_rfc_mat `COUNT(*) WHERE
    // status IN (active, overdue)`. Q4's ranking uses the separate
    // top_active_balance pre-agg, so this collection needs only the count —
    // the old `sum_monto`/`n_credits` fields were unread and `n_credits`
    // (all-status count) was the wrong number for Q2.
    let pipeline = vec![
        doc! { "$group": {
            "_id": "$rfc",
            "n_active": { "$sum": {
                "$cond": [{ "$in": ["$status", ["active", "overdue"]] }, 1, 0]
            } },
        } },
        doc! { "$merge": {
            "into": "credits_by_rfc",
            "on": "_id",
            "whenMatched": "replace",
            "whenNotMatched": "insert",
        } },
    ];
    let mut cursor = coll
        .aggregate(pipeline)
        .await
        .context("aggregate credits_by_rfc")?;
    while cursor.try_next().await?.is_some() {}
    Ok(())
}

/// Q4 top-N by exposure — `$merge`-maintained `top_active_balance` (best
/// weapon, restored). Groups active+overdue credits by rfc into a small
/// per-rfc collection so the query reads a `{sum_monto:-1}` index top-N
/// instead of FETCHing every matching fat embedded credit doc (the runtime
/// path's scale-0.1 cache-thrash outlier). Distinct from `credits_by_rfc`,
/// whose `sum_monto` is over ALL credits (unfiltered) — not the exposure.
pub(crate) async fn run_merge_top_active_balance(db: &Database) -> Result<()> {
    let coll: MongoColl<Document> = db.collection("credits");
    let pipeline = vec![
        doc! { "$match": { "status": { "$in": ["active", "overdue"] } } },
        doc! { "$group": {
            "_id": "$rfc",
            "sum_monto": { "$sum": "$monto" },
            "n": { "$sum": 1 },
        } },
        doc! { "$merge": {
            "into": "top_active_balance",
            "on": "_id",
            "whenMatched": "replace",
            "whenNotMatched": "insert",
        } },
    ];
    let mut cursor = coll
        .aggregate(pipeline)
        .await
        .context("aggregate top_active_balance")?;
    while cursor.try_next().await?.is_some() {}
    Ok(())
}

// Q8 NEW v0.3.3 — `$merge`-maintained `monthly_close_agg` per design
// §8.C8. Composite per-empresa: 4 partial aggregations (active credits +
// overdue installments + recent payments 30d + collection actions 30d)
// each `$merge`d into `monthly_close_agg` with `whenMatched=merge` to
// layer the metrics. Sequential design (vs $facet+single-merge) matches
// audit fairness intent: the maintenance tax is the sum of 4 sub-pipes.
pub(crate) async fn run_merge_monthly_close(db: &Database) -> Result<()> {
    let coll: MongoColl<Document> = db.collection("credits");
    let cutoff_ms = (chrono::Utc::now() - chrono::Duration::days(30)).timestamp_millis();
    let cutoff_bson = mongodb::bson::DateTime::from_millis(cutoff_ms);

    // Sub-pipeline 1 — group over ALL credits per empresa; n_active counts only
    // active/overdue via $cond. Grouping over every credit (not just
    // active/overdue) sets the result domain to "every empresa with a credit
    // portfolio", matching xyzDB's GROUP BY over the creditos lobe and PG's base
    // over DISTINCT empresa_id. An empresa with only closed credits still gets a
    // row (n_active = 0), so the three engines return the same empresa set.
    let p1 = vec![
        doc! { "$group": {
            "_id": "$empresa_id",
            "n_active": { "$sum": { "$cond": [
                { "$in": ["$status", ["active", "overdue"]] }, 1, 0,
            ] } },
        } },
        doc! { "$merge": {
            "into": "monthly_close_agg",
            "on": "_id",
            "whenMatched": "merge",
            "whenNotMatched": "insert",
        } },
    ];
    let mut c = coll.aggregate(p1).await.context("monthly_close: active")?;
    while c.try_next().await?.is_some() {}

    // Sub-pipeline 2 — overdue installments per empresa
    let p2 = vec![
        doc! { "$match": { "installments.status": "overdue" } },
        doc! { "$unwind": "$installments" },
        doc! { "$match": { "installments.status": "overdue" } },
        doc! { "$group": {
            "_id": "$empresa_id",
            "overdue_sum": { "$sum": "$installments.monto_total" },
            "overdue_n": { "$sum": 1 },
        } },
        doc! { "$merge": {
            "into": "monthly_close_agg",
            "on": "_id",
            "whenMatched": "merge",
            "whenNotMatched": "insert",
        } },
    ];
    let mut c = coll.aggregate(p2).await.context("monthly_close: overdue")?;
    while c.try_next().await?.is_some() {}

    // Sub-pipeline 3 — recent payments 30d per empresa.
    // C-11 Path A (resolved Session 4): empresa_id is denormalised in
    // payment_doc at bulk_load (see payment_doc + load_payments), so
    // this pipeline groups by it directly. Pre-fix the pipeline did
    // `$lookup credits` + `$unwind: $cr` to recover empresa_id, which
    // multiplied the 3.6 M payment dataset against the 300 k credits
    // collection and entered an eviction storm on Scale 0.1 (working set
    // > WiredTiger cache). Post-fix: single-collection $group over 80
    // distinct empresa_id values; sub-second on Scale 0.1 SSD.
    let payments: MongoColl<Document> = db.collection("payments");
    let p3 = vec![
        doc! { "$match": { "fechaPago": { "$gte": cutoff_bson } } },
        doc! { "$group": {
            "_id": "$empresa_id",
            "recent_pay_sum": { "$sum": "$monto" },
            "recent_pay_n": { "$sum": 1 },
        } },
        doc! { "$merge": {
            "into": "monthly_close_agg",
            "on": "_id",
            "whenMatched": "merge",
            "whenNotMatched": "insert",
        } },
    ];
    let mut c = payments
        .aggregate(p3)
        .await
        .context("monthly_close: recent_pay")?;
    while c.try_next().await?.is_some() {}

    // Sub-pipeline 4 — collection actions 30d per empresa. Actions are embedded
    // two levels deep (`credits.collections[].actions[]`, see collection_subdoc
    // / action_subdoc), so counting them needs a DOUBLE unwind; the credit's
    // top-level empresa_id survives both. A single `$unwind
    // "$collection_actions"` over a nonexistent top-level field yielded zero
    // docs, silently dropping the acciones concept that xyzDB and PG both
    // compute — this pipeline mirrors `mongo_count_double_unwind` (golden V4).
    let p4 = vec![
        doc! { "$unwind": { "path": "$collections", "preserveNullAndEmptyArrays": false } },
        doc! { "$unwind": { "path": "$collections.actions", "preserveNullAndEmptyArrays": false } },
        doc! { "$match": { "collections.actions.fecha": { "$gte": cutoff_bson } } },
        doc! { "$group": {
            "_id": "$empresa_id",
            "col_actions_n": { "$sum": 1 },
        } },
        doc! { "$merge": {
            "into": "monthly_close_agg",
            "on": "_id",
            "whenMatched": "merge",
            "whenNotMatched": "insert",
        } },
    ];
    let mut c = coll
        .aggregate(p4)
        .await
        .context("monthly_close: col_actions")?;
    while c.try_next().await?.is_some() {}

    Ok(())
}

// ── flat collections (clients / empresas / productos / payments / etc.) ─

/// `rfc -> (credit_id, empresa_id)` for one real credit per client (the first
/// seen). Q7's synthetic payments borrow this pair so they carry a real empresa
/// and credit, symmetric with xyzDB/PG.
fn build_rfc_first_credit_map(ds: &Dataset) -> HashMap<String, (String, String)> {
    let mut map = HashMap::with_capacity(200_000);
    for c in ds.credits() {
        map.entry(c.rfc).or_insert((c.credit_id, c.empresa_id));
    }
    map
}

async fn load_empresas(db: &Database, ds: &Dataset) -> Result<u64> {
    let coll: MongoColl<Document> = db.collection("empresas");
    flush_in_batches(&coll, ds.empresas().map(empresa_doc), BATCH_FLAT).await
}

async fn load_productos(db: &Database, ds: &Dataset) -> Result<u64> {
    let coll: MongoColl<Document> = db.collection("productos");
    flush_in_batches(&coll, ds.productos().map(producto_doc), BATCH_FLAT).await
}

async fn load_clients(db: &Database, ds: &Dataset) -> Result<u64> {
    let coll: MongoColl<Document> = db.collection("clients");
    flush_in_batches(&coll, ds.clients().map(client_doc), BATCH_FLAT).await
}

async fn load_payments(
    db: &Database,
    ds: &Dataset,
    credit_to_empresa: &HashMap<String, String>,
) -> Result<u64> {
    let coll: MongoColl<Document> = db.collection("payments");
    flush_in_batches(
        &coll,
        ds.payments().map(|p| {
            // Generator contract guarantees every payment.credit_id exists
            // in the credits stream; if the mapping miss happens it is a
            // generator/driver desync, not a runtime concern. Empty
            // string degrades the V3-by-empresa aggregation gracefully
            // for any orphan payment instead of panicking.
            let eid = credit_to_empresa
                .get(&p.credit_id)
                .map(String::as_str)
                .unwrap_or("");
            payment_doc(p, eid)
        }),
        BATCH_FLAT,
    )
    .await
}

async fn load_applications(db: &Database, ds: &Dataset) -> Result<u64> {
    let coll: MongoColl<Document> = db.collection("credit_applications");
    flush_in_batches(
        &coll,
        ds.credit_applications().map(application_doc),
        BATCH_FLAT,
    )
    .await
}

async fn load_audit(db: &Database, ds: &Dataset) -> Result<u64> {
    let coll: MongoColl<Document> = db.collection("audit_log");
    flush_in_batches(&coll, ds.audit_log().map(audit_doc), BATCH_FLAT).await
}

async fn load_notifications(db: &Database, ds: &Dataset) -> Result<u64> {
    let coll: MongoColl<Document> = db.collection("notifications");
    flush_in_batches(&coll, ds.notifications().map(notification_doc), BATCH_FLAT).await
}

async fn load_bi(db: &Database, ds: &Dataset) -> Result<u64> {
    let coll: MongoColl<Document> = db.collection("bi_snapshots");
    flush_in_batches(&coll, ds.bi_snapshots().map(bi_doc), BATCH_FLAT).await
}

/// Streaming merge: emits one embedded credit doc per credit. Relies on
/// the generator streams emitting installments / collections /
/// collection_actions in `credit_ord`-monotonic order, which is the
/// documented contract (see `streams.rs`).
async fn load_credits_embedded(
    db: &Database,
    ds: &Dataset,
) -> Result<(u64, HashMap<String, String>)> {
    let coll: MongoColl<Document> = db.collection("credits");
    let mut credits = ds.credits();
    let mut installments = ds.installments().peekable();
    let mut collections = ds.collections().peekable();
    let mut actions = ds.collection_actions().peekable();

    let mut batch: Vec<Document> = Vec::with_capacity(BATCH_CREDITS);
    let mut total: u64 = 0;
    // C-11 Path A: build the credit_id ↦ empresa_id mapping in-flight so
    // load_payments can denormalise empresa_id without a re-scan of
    // credits. ~30 MB at scale 0.1 (300 k entries × ~100 B); ~300 MB at
    // scale 1.0 — still well within bench host memory budget.
    let mut credit_to_empresa: HashMap<String, String> = HashMap::with_capacity(300_000);

    while let Some(credit) = credits.next() {
        credit_to_empresa.insert(credit.credit_id.clone(), credit.empresa_id.clone());
        // Drain installments while next.credit_id matches.
        let mut inst_subdocs: Vec<Document> = Vec::with_capacity(32);
        while installments
            .peek()
            .map(|i| i.credit_id == credit.credit_id)
            .unwrap_or(false)
        {
            let inst = installments.next().unwrap();
            inst_subdocs.push(installment_subdoc(inst));
        }

        // Drain collections (and their actions) while next.credit_id matches.
        let mut coll_subdocs: Vec<Document> = Vec::new();
        while collections
            .peek()
            .map(|c| c.credit_id == credit.credit_id)
            .unwrap_or(false)
        {
            let collection = collections.next().unwrap();
            let mut action_subdocs: Vec<Document> = Vec::with_capacity(4);
            while actions
                .peek()
                .map(|a| a.collection_id == collection.collection_id)
                .unwrap_or(false)
            {
                let act = actions.next().unwrap();
                action_subdocs.push(action_subdoc(act));
            }
            coll_subdocs.push(collection_subdoc(collection, action_subdocs));
        }

        batch.push(credit_doc(credit, inst_subdocs, coll_subdocs));
        if batch.len() >= BATCH_CREDITS {
            total += flush_one(&coll, std::mem::take(&mut batch)).await?;
        }
    }
    if !batch.is_empty() {
        total += flush_one(&coll, batch).await?;
    }
    // Drain any remaining sub-stream entries (should be zero — emit warn
    // if not, indicates a generator-stream / driver-merge desync).
    let mut leftover_inst = 0u64;
    while installments.next().is_some() {
        leftover_inst += 1;
    }
    let mut leftover_coll = 0u64;
    while collections.next().is_some() {
        leftover_coll += 1;
    }
    let mut leftover_act = 0u64;
    while actions.next().is_some() {
        leftover_act += 1;
    }
    if leftover_inst + leftover_coll + leftover_act > 0 {
        warn!(target: "mongo",
              "credit-embedded merge left unmerged: inst={leftover_inst} coll={leftover_coll} act={leftover_act}");
    }
    Ok((total, credit_to_empresa))
}

async fn flush_one(coll: &MongoColl<Document>, docs: Vec<Document>) -> Result<u64> {
    if docs.is_empty() {
        return Ok(0);
    }
    let n = docs.len() as u64;
    coll.insert_many(docs)
        .ordered(false)
        .await
        .context("insert_many")?;
    Ok(n)
}

async fn flush_in_batches<I: Iterator<Item = Document>>(
    coll: &MongoColl<Document>,
    iter: I,
    batch: usize,
) -> Result<u64> {
    let mut total: u64 = 0;
    let mut buf: Vec<Document> = Vec::with_capacity(batch);
    for d in iter {
        buf.push(d);
        if buf.len() >= batch {
            total += flush_one(coll, std::mem::take(&mut buf)).await?;
        }
    }
    if !buf.is_empty() {
        total += flush_one(coll, buf).await?;
    }
    Ok(total)
}

// ── doc builders (BSON conversions) ──────────────────────────────────────────

fn dt(t: DateTime<Utc>) -> bson::DateTime {
    bson::DateTime::from_millis(t.timestamp_millis())
}

fn ndate(d: NaiveDate) -> bson::DateTime {
    let dt = Utc.from_utc_datetime(&d.and_time(NaiveTime::MIN));
    bson::DateTime::from_millis(dt.timestamp_millis())
}

fn empresa_doc(e: Empresa) -> Document {
    doc! {
        "_id": e.empresa_id,
        "nombre": e.nombre,
        "region": e.region,
        "activa": e.activa,
    }
}

fn producto_doc(p: Producto) -> Document {
    doc! {
        "_id": p.producto_id,
        "empresa_id": p.empresa_id,
        "nombre": p.nombre,
        "tasa_interes": p.tasa_interes,
        "plazo_meses": p.plazo_meses,
    }
}

fn client_doc(c: Client) -> Document {
    doc! {
        "_id": c.rfc,
        "curp": c.curp,
        "nombre": c.nombre,
        "scoring": {
            "bureau": c.scoring.bureau,
            "risk": c.scoring.risk,
            "limiteCredito": c.scoring.limite_credito,
        },
        "datosUbicacion": {
            "entidadFederativa": c.datos_ubicacion.entidad_federativa,
            "municipio": c.datos_ubicacion.municipio,
            "codigoPostal": c.datos_ubicacion.codigo_postal,
        },
        "datosIdentificacion": {
            "tipoId": c.datos_identificacion.tipo_id,
            "numeroId": c.datos_identificacion.numero_id,
        },
        "caracteristicasFiscales": {
            "regimen": c.caracteristicas_fiscales.regimen,
            "actividad": c.caracteristicas_fiscales.actividad,
        },
        "tags": c.tags,
        "fechaAlta": dt(c.fecha_alta),
    }
}

fn credit_doc(c: Credit, installments: Vec<Document>, collections: Vec<Document>) -> Document {
    let inst_arr = installments
        .into_iter()
        .map(Bson::Document)
        .collect::<Vec<_>>();
    let coll_arr = collections
        .into_iter()
        .map(Bson::Document)
        .collect::<Vec<_>>();
    doc! {
        "_id": c.credit_id,
        "rfc": c.rfc,
        "empresa_id": c.empresa_id,
        "producto_id": c.producto_id,
        "monto": c.monto,
        "status": c.status.as_str(),
        "fechaCreacion": dt(c.fecha_creacion),
        "fechaVencimiento": ndate(c.fecha_vencimiento),
        "diasAtraso": c.dias_atraso,
        "installments": inst_arr,
        "collections": coll_arr,
    }
}

fn installment_subdoc(i: Installment) -> Document {
    doc! {
        "installment_id": i.installment_id,
        "rfc": i.rfc,
        "empresa_id": i.empresa_id,
        "numero": i.numero,
        "monto_total": i.monto_total,
        "status": i.status.as_str(),
        "diasAtraso": i.dias_atraso,
        "fechaVencimiento": ndate(i.fecha_vencimiento),
    }
}

fn collection_subdoc(c: Collection, actions: Vec<Document>) -> Document {
    let arr = actions.into_iter().map(Bson::Document).collect::<Vec<_>>();
    doc! {
        "collection_id": c.collection_id,
        "rfc": c.rfc,
        "monto_pendiente": c.monto_pendiente,
        "status": c.status.as_str(),
        "fechaInicio": dt(c.fecha_inicio),
        "actions": arr,
    }
}

fn action_subdoc(a: CollectionAction) -> Document {
    doc! {
        "action_id": a.action_id,
        "rfc": a.rfc,
        "tipo": a.tipo,
        "fecha": dt(a.fecha),
        "resultado": a.resultado,
    }
}

fn payment_doc(p: Payment, empresa_id: &str) -> Document {
    // C-11 Path A (resolved Session 4): empresa_id denormalised from
    // credits at bulk_load time so monthly_close_agg sub-pipeline 3 can
    // group by empresa_id directly without `$lookup` + `$unwind` over
    // 3.6 M payment docs (caveat C-11 eviction storm).
    let mut d = doc! {
        "_id": p.payment_id,
        "credit_id": p.credit_id,
        "rfc": p.rfc,
        "empresa_id": empresa_id,
        "monto": p.monto,
        "fechaPago": dt(p.fecha_pago),
        "metodo": p.metodo,
    };
    if let Some(iid) = p.installment_id {
        d.insert("installment_id", iid);
    }
    d
}

fn application_doc(a: CreditApplication) -> Document {
    doc! {
        "_id": a.application_id,
        "rfc": a.rfc,
        "empresa_id": a.empresa_id,
        "producto_id": a.producto_id,
        "monto_solicitado": a.monto_solicitado,
        "status": a.status.as_str(),
        "fechaSolicitud": dt(a.fecha_solicitud),
    }
}

fn audit_doc(a: AuditLogEntry) -> Document {
    let mut d = doc! {
        "_id": a.audit_id as i64,
        "rfc": a.rfc,
        "actionType": a.action_type,
        "details": bson::to_bson(&a.details).unwrap_or(Bson::Null),
        "fecha": dt(a.fecha),
    };
    if let Some(cid) = a.credit_id {
        d.insert("credit_id", cid);
    }
    d
}

fn notification_doc(n: Notification) -> Document {
    doc! {
        "_id": n.notification_id as i64,
        "rfc": n.rfc,
        "canal": n.canal,
        "contenido": n.contenido,
        "fecha": dt(n.fecha),
    }
}

fn bi_doc(b: BiSnapshot) -> Document {
    doc! {
        "_id": b.snapshot_id as i64,
        "empresa_id": b.empresa_id,
        "fecha": ndate(b.fecha),
        "metricas": bson::to_bson(&b.metricas).unwrap_or(Bson::Null),
    }
}

// ── Phase 3 concurrent runner (reader pool + writer + refresh threads) ──

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

fn run_concurrent_workload(
    driver: &MongoDriver,
    profile: &ConcurrentProfile,
    rfc_pool: &[String],
) -> Result<ConcurrentResults> {
    info!(target: "mongo",
          "Phase 3: concurrent {} readers + {} writers, refresh cadences={:?}",
          profile.readers, profile.writers, profile.refresh_cadences_secs);

    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_at = Instant::now() + profile.duration;
    let reads = Arc::new(AtomicU64::new(0));
    let writes = Arc::new(AtomicU64::new(0));
    let refresh_count = Arc::new(AtomicU64::new(0));
    let refresh_total_ms = Arc::new(AtomicU64::new(0));

    type Samples = Vec<(BusinessQuery, f64, u64)>;
    let samples: Arc<Mutex<Samples>> = Arc::new(Mutex::new(Vec::new()));

    let pool = Arc::new(rfc_pool.to_vec());
    let uri = driver.uri().to_string();
    let db_name = driver.db_name().to_string();
    // rfc→(credit_id, empresa_id) for the pool's clients, so Phase-3 Q7 writes
    // borrow a real empresa/credit (same invariant as the cold path). Filtered
    // to the pool to stay small; shared read-only across threads.
    let rfc_map: Arc<HashMap<String, (String, String)>> = {
        let full = driver.rfc_first_credit.get();
        let m: HashMap<String, (String, String)> = rfc_pool
            .iter()
            .filter_map(|r| {
                full.and_then(|f| f.get(r.as_str()))
                    .map(|ce| (r.clone(), ce.clone()))
            })
            .collect();
        Arc::new(m)
    };

    let mut handles: Vec<std::thread::JoinHandle<()>> = Vec::new();

    // Refresh threads (one per cadence). Phase 2.b cadence dispatch:
    // 30 → overdue_by_empresa_agg (Q5 NEW v0.3.3, replaces dropped
    // top_active_balance per design §7.5 deliberate equivalence + §8.C5
    // audit fix); 60 → credits_by_rfc (Q2 carried v0.2.5). Mongo
    // maintenance tax accumulates wall-clock + count, parallel to PG's
    // REFRESH thread (per §11.1 *$merge-confounded* methodology gate).
    //
    // Phase 2 verification refinement (Architecture β): dispatch by
    // thread INDEX, not by cadence VALUE. Bucket-keyed dispatch (legacy
    // v0.2.5 + Phase 2.b/c) cannot distinguish two collections sharing
    // the same cadence (design SPEC §8.C5 commits {30, 30, 60} post
    // Decision 1 Path β for Q9). v0.3.4 Path α refactor (explicit
    // (target, cadence) tuple list per §13.4 Entry 8) eliminates the
    // orchestrator-config / lib.rs-dispatch coupling.
    let cadence_iter: &[u64] = &profile.refresh_cadences_secs;
    for (idx, &cadence) in cadence_iter.iter().enumerate() {
        let uri = uri.clone();
        let db_name = db_name.clone();
        let stop_flag = stop_flag.clone();
        let refresh_count = refresh_count.clone();
        let refresh_total_ms = refresh_total_ms.clone();
        let h = std::thread::spawn(move || {
            let rt = match tokio::runtime::Runtime::new() {
                Ok(r) => r,
                Err(_) => return,
            };
            let db = match rt.block_on(async {
                let mut opts = ClientOptions::parse(&uri).await?;
                opts.write_concern = Some(
                    WriteConcern::builder()
                        .w(mongodb::options::Acknowledgment::Nodes(1))
                        .journal(true)
                        .build(),
                );
                opts.heartbeat_freq = Some(Duration::from_secs(30));
                opts.server_selection_timeout = Some(Duration::from_secs(600));
                opts.connect_timeout = Some(Duration::from_secs(60));
                let client = MongoClient::with_options(opts)?;
                anyhow::Ok(client.database(&db_name))
            }) {
                Ok(d) => d,
                Err(_) => return,
            };
            while !stop_flag.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(cadence));
                if stop_flag.load(Ordering::Relaxed) {
                    break;
                }
                let t0 = Instant::now();
                let ok = rt.block_on(async {
                    // Phase 2 verification refinement enumerate-by-index
                    // dispatch. Index → target mapping per design SPEC
                    // §8.C5 cadence list {30, 30, 60} (post Decision 1
                    // Path β for Q9 Q9 — customer_360 NOT $merge-maintained):
                    //   idx 0 (30 s) → overdue_by_empresa_agg (Q5 §8.C5)
                    //   idx 1 (30 s) → credits_by_rfc (Q2) + top_active_balance
                    //                  (Q4) — both rfc-keyed credit aggregates,
                    //                  refreshed together to keep the 3-thread
                    //                  budget (no 4th cadence).
                    //   idx 2 (60 s) → monthly_close_agg (Q8 composite §8.C8)
                    // Methodology gate §8.C5: 3 background `$merge` threads
                    // vs 2 cores T6; Phase 4 acceptance test for
                    // `*$merge-confounded*` if >20% CPU or >30% IOPS.
                    match idx {
                        0 => run_merge_overdue_by_empresa(&db).await,
                        1 => {
                            run_merge_credits_by_rfc(&db).await?;
                            run_merge_top_active_balance(&db).await
                        }
                        2 => run_merge_monthly_close(&db).await,
                        _ => Ok(()),
                    }
                });
                if ok.is_ok() {
                    refresh_count.fetch_add(1, Ordering::Relaxed);
                    refresh_total_ms.fetch_add(t0.elapsed().as_millis() as u64, Ordering::Relaxed);
                }
            }
        });
        handles.push(h);
    }

    // Phase 3 v0.3.3: mixed-mode threads driven by `ErraticaPicker`
    // (design §6). Each thread instantiates own MMPP + session + drift
    // + mixer. State-dependent R/W mix per §6.4: Idle 95R/5W, Busy 70R/30W.
    // Writes (Q7) dispatch via `queries::exec_async` same as reads;
    // separate writer thread split (v0.2.5 + Phase 2.b) collapsed to
    // mixed-mode. Thread count = readers + writers from `ErraticaConfig`.
    let total_threads = profile.readers + profile.writers;
    // v0.5 multi-persona shared anchors.
    let run_start = Instant::now();
    let total_duration = profile.duration;
    let has_personas = profile.persona_assignment.is_some();
    for tid in 0..total_threads {
        let persona_for_tid = profile
            .persona_assignment
            .as_ref()
            .and_then(|pa| pa.persona_for(tid));
        if has_personas && persona_for_tid.is_none() {
            continue;
        }
        let uri = uri.clone();
        let db_name = db_name.clone();
        let pool = (*pool).clone();
        let samples = samples.clone();
        let reads = reads.clone();
        let writes = writes.clone();
        let cfg = profile.erratica.clone();
        let schedule = profile.schedule.clone();
        let rfc_map = rfc_map.clone();
        let h = std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("rt");
            let db = match rt.block_on(async {
                let mut opts = ClientOptions::parse(&uri).await?;
                opts.write_concern = Some(
                    WriteConcern::builder()
                        .w(mongodb::options::Acknowledgment::Nodes(1))
                        .journal(true)
                        .build(),
                );
                opts.heartbeat_freq = Some(Duration::from_secs(30));
                opts.server_selection_timeout = Some(Duration::from_secs(600));
                opts.connect_timeout = Some(Duration::from_secs(60));
                let client = MongoClient::with_options(opts)?;
                anyhow::Ok(client.database(&db_name))
            }) {
                Ok(d) => d,
                Err(_) => return,
            };
            let mut picker = if let Some(p_id) = persona_for_tid {
                let p_cfg = native_generator::personas::PersonaConfig::for_persona(p_id);
                native_generator::erratica::ErraticaPicker::with_persona(
                    tid as u64,
                    pool,
                    cfg,
                    Instant::now(),
                    p_cfg,
                    schedule,
                    run_start,
                    total_duration,
                )
            } else {
                native_generator::erratica::ErraticaPicker::new(
                    tid as u64,
                    pool,
                    cfg,
                    Instant::now(),
                )
            };
            while Instant::now() < stop_at {
                let now = Instant::now();
                let event = picker.next_event(now);
                let (sleep_dur, query_pair): (Duration, Option<(BusinessQuery, String)>) =
                    match event {
                        native_generator::erratica::ErraticaEvent::Sleep(d) => (d, None),
                        native_generator::erratica::ErraticaEvent::SleepThenQuery {
                            sleep,
                            query,
                            rfc,
                        } => (sleep, Some((query, rfc))),
                        native_generator::erratica::ErraticaEvent::Query { query, rfc } => {
                            (Duration::ZERO, Some((query, rfc)))
                        }
                    };
                if sleep_dur > Duration::ZERO {
                    let cap = sleep_dur.min(stop_at.saturating_duration_since(now));
                    if cap > Duration::ZERO {
                        std::thread::sleep(cap);
                    }
                    if Instant::now() >= stop_at {
                        break;
                    }
                }
                if let Some((query, rfc)) = query_pair {
                    let is_write = matches!(query, BusinessQuery::Q7BatchIngest);
                    let t0 = Instant::now();
                    let n = rt.block_on(async {
                        let ce = rfc_map.get(&rfc).map(|(c, e)| (c.as_str(), e.as_str()));
                        queries::exec_async(&db, query, &rfc, 50, ce).await
                    });
                    let lat_ms = t0.elapsed().as_secs_f64() * 1000.0;
                    if let Ok(records) = n {
                        samples.lock().unwrap().push((query, lat_ms, records));
                        if is_write {
                            writes.fetch_add(1, Ordering::Relaxed);
                        } else {
                            reads.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }
        });
        handles.push(h);
    }

    while Instant::now() < stop_at {
        std::thread::sleep(Duration::from_millis(200));
    }
    stop_flag.store(true, Ordering::Relaxed);

    for h in handles {
        let _ = h.join();
    }

    let dur_secs = profile.duration.as_secs_f64();
    let reads_total = reads.load(Ordering::Relaxed);
    let writes_total = writes.load(Ordering::Relaxed);

    let mut by_query: std::collections::HashMap<BusinessQuery, (Vec<f64>, Vec<u64>)> =
        std::collections::HashMap::new();
    for (q, lat, rec) in samples.lock().unwrap().drain(..) {
        let e = by_query.entry(q).or_default();
        e.0.push(lat);
        e.1.push(rec);
    }
    let mut per_query: Vec<QueryStats> = by_query
        .into_iter()
        .map(|(q, (lat, rec))| QueryStats::from_samples(q, &lat, &rec))
        .collect();
    per_query.sort_by(|a, b| a.query.cmp(&b.query));

    Ok(ConcurrentResults {
        reads_total,
        writes_total,
        reads_per_sec: reads_total as f64 / dur_secs,
        writes_per_sec: writes_total as f64 / dur_secs,
        per_query,
        throughput_cv: 0.0,
        refresh_count: refresh_count.load(Ordering::Relaxed),
        refresh_total_ms: refresh_total_ms.load(Ordering::Relaxed),
    })
}
