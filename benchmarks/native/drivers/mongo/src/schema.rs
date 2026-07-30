//! Phase 0: schema setup for MongoDB. Creates the active collections,
//! the two pre-aggregation collections (`credits_by_rfc` and
//! `overdue_by_empresa_agg` per v0.3.3-bench-redesign §8.3) and all
//! indexes from the design doc.
//!
//! Mongo auto-creates a collection on first insert, so the only
//! must-run-now operation is index creation. We materialise empty
//! collections here so verify on a fresh database does not surface
//! "namespace not found" before bulk load.

use anyhow::{Context, Result};
use mongodb::bson::doc;
use mongodb::options::IndexOptions;
use mongodb::{Database, IndexModel};
use native_generator::bench::{SchemaMetrics, SchemaMode};
use std::time::Instant;
use tracing::info;

const ACTIVE_COLLECTIONS: &[&str] = &[
    "empresas",
    "productos",
    "clients",
    "credits",
    "payments",
    "credit_applications",
    "audit_log",
    "notifications",
    "bi_snapshots",
];

// Phase 2.b/c v0.3.3 audit fairness fixes (per design §8.3 + §7.5 + §8.C8):
//   - `top_active_balance` (Q4 pre-agg) RESTORED — best-weapon parity.
//     The runtime `$group` over active+overdue credits FETCHes every
//     matching (fat, embedded) doc — explain: docsExamined=all-matching,
//     the scale-0.1 cache-thrash outlier. The `$merge`-maintained pre-agg
//     (_id=rfc, sum_monto) with a `{sum_monto:-1}` index turns Q4 into a
//     top-N index read (explain after: docsExamined=limit).
//   - `overdue_by_empresa_agg` (Q5) NEW Phase 2.b — design §8.C5 audit
//     fix: pre-v0.3.3 Q5 ran runtime `$unwind` (1-10s at Scale 1.0);
//     `$merge`-maintained pre-agg symmetric to PG/xyzdb.
//   - `monthly_close_agg` (Q8) NEW Phase 2.c — design §8.C8 composite:
//     4-aggregation per empresa (active credits + overdue install +
//     recent pay 30d + col actions 30d) layered via `$merge whenMatched
//     =merge`. Symmetric to PG `monthly_close_mat` 4-CTE composite.
const PREAGG_COLLECTIONS: &[&str] = &[
    "credits_by_rfc",
    "overdue_by_empresa_agg",
    "monthly_close_agg",
    "top_active_balance",
];

/// Build the index set for Bench A. Returns the list of (collection,
/// IndexModel) pairs so the caller can execute and count DDL lines.
fn index_set() -> Vec<(&'static str, IndexModel)> {
    vec![
        // clients — _id is the natural primary key (= rfc), auto-indexed.
        (
            "clients",
            IndexModel::builder()
                .keys(doc! { "scoring.bureau": 1 })
                .build(),
        ),
        (
            "clients",
            IndexModel::builder().keys(doc! { "tags": 1 }).build(),
        ),
        (
            "clients",
            IndexModel::builder()
                .keys(doc! { "datosUbicacion.entidadFederativa": 1 })
                .build(),
        ),
        // credits — Q2 (per-rfc) + Q4 (status, monto) + Q5 (overdue installment by empresa)
        (
            "credits",
            IndexModel::builder().keys(doc! { "rfc": 1 }).build(),
        ),
        (
            "credits",
            IndexModel::builder().keys(doc! { "empresa_id": 1 }).build(),
        ),
        (
            "credits",
            IndexModel::builder()
                .keys(doc! { "status": 1, "monto": -1 })
                .build(),
        ),
        (
            "credits",
            IndexModel::builder()
                .keys(doc! {
                    "installments.status": 1,
                    "installments.fechaVencimiento": 1,
                    "empresa_id": 1,
                })
                .build(),
        ),
        // payments — Q6 covering compound index
        (
            "payments",
            IndexModel::builder()
                .keys(doc! { "fechaPago": -1, "monto": 1 })
                .build(),
        ),
        // payments — Q6 PARTIAL index (best weapon): only high-value
        // payments, walked by fecha DESC. The compound {fechaPago,monto}
        // above can't use the monto range after the fecha range → it
        // over-scans the index (explain before: keysExamined 2167 for 100
        // rows). Partial mirrors PG's `idx_payments_recent_high` (explain
        // after: keysExamined == nReturned).
        (
            "payments",
            IndexModel::builder()
                .keys(doc! { "fechaPago": -1 })
                .options(
                    IndexOptions::builder()
                        .name("fechaPago_hi".to_string())
                        .partial_filter_expression(doc! { "monto": { "$gt": 50000 } })
                        .build(),
                )
                .build(),
        ),
        (
            "payments",
            IndexModel::builder().keys(doc! { "credit_id": 1 }).build(),
        ),
        (
            "payments",
            IndexModel::builder().keys(doc! { "rfc": 1 }).build(),
        ),
        // applications, audit, notifications
        (
            "credit_applications",
            IndexModel::builder()
                .keys(doc! { "rfc": 1, "fechaSolicitud": -1 })
                .build(),
        ),
        (
            "audit_log",
            IndexModel::builder()
                .keys(doc! { "rfc": 1, "fecha": -1 })
                .build(),
        ),
        (
            "notifications",
            IndexModel::builder()
                .keys(doc! { "rfc": 1, "fecha": -1 })
                .build(),
        ),
        // pre-aggregation read layout. `credits_by_rfc` relies on
        // `_id` (= rfc) which Mongo auto-indexes as unique — no
        // explicit index entry needed (and explicit `unique:true` on
        // `_id` is rejected by the server with InvalidIndexOption).
        // `overdue_by_empresa_agg._id` = empresa_id (auto-indexed); add
        // a secondary index on sum_monto for Q5 sort.
        (
            "overdue_by_empresa_agg",
            IndexModel::builder().keys(doc! { "sum_monto": -1 }).build(),
        ),
        // `monthly_close_agg._id` = empresa_id (auto-indexed); add a
        // secondary index on overdue_sum DESC for Q8 sort path.
        (
            "monthly_close_agg",
            IndexModel::builder()
                .keys(doc! { "overdue_sum": -1 })
                .build(),
        ),
        // `top_active_balance._id` = rfc (auto-indexed); sum_monto DESC
        // index serves Q4's top-N read (explain: docsExamined == limit).
        (
            "top_active_balance",
            IndexModel::builder().keys(doc! { "sum_monto": -1 }).build(),
        ),
    ]
}

pub async fn setup(db: &Database, mode: SchemaMode) -> Result<SchemaMetrics> {
    let start = Instant::now();
    info!(target: "mongo", "Phase 0: schema setup mode={:?}", mode);

    // Materialise empty active collections so verify works pre-load.
    for name in ACTIVE_COLLECTIONS {
        let _ = db.create_collection(*name).await;
    }
    if matches!(mode, SchemaMode::Full) {
        for name in PREAGG_COLLECTIONS {
            let _ = db.create_collection(*name).await;
        }
    }

    let mut indexes = index_set();
    if matches!(mode, SchemaMode::AutoOnly) {
        // AutoOnly — strip pre-aggregation indexes. Queries against the
        // unmaterialised credits_by_rfc / overdue_by_empresa_agg fall back
        // to runtime aggregation on `credits` — Phase 6 validates the
        // self-tuning pillar.
        indexes.retain(|(coll, _)| {
            !matches!(
                *coll,
                "credits_by_rfc"
                    | "overdue_by_empresa_agg"
                    | "monthly_close_agg"
                    | "top_active_balance"
            )
        });
    }
    let n_index = indexes.len();
    // Authored pre-aggregations (each maintained by a `$merge` pipeline during
    // load) — counted as declarations for parity with PG mat-views / xyzDB
    // ghosts. Collections are NOT counted: Mongo auto-creates them on first
    // write (the create_collection calls above are bench-convenience for the
    // pre-load verify), so they are not an authoring burden.
    let n_preagg = if matches!(mode, SchemaMode::Full) {
        PREAGG_COLLECTIONS.len()
    } else {
        0
    };
    let setup_statements = n_index + n_preagg;

    for (coll, model) in indexes {
        db.collection::<mongodb::bson::Document>(coll)
            .create_index(model)
            .await
            .with_context(|| format!("create index on {coll}"))?;
    }

    let dur = start.elapsed();
    info!(
        target: "mongo",
        "Phase 0 done: {} indexes + {} pre-agg = {} setup statements in {:?}",
        n_index, n_preagg, setup_statements, dur
    );
    Ok(SchemaMetrics {
        mode,
        setup_statements,
        setup_duration_ms: dur.as_millis() as u64,
    })
}
