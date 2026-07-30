//! Native PostgreSQL 18 driver for the native cross-engine bench.
//!
//! Schema, partitioning, indexes, mat views and queries follow the
//! native cross-engine bench design.
//!
//! Uses tokio-postgres + COPY for bulk load. The refresh thread for
//! Phase 3 runs `REFRESH MATERIALIZED VIEW CONCURRENTLY` on the configured
//! cadences and accumulates wall-clock + counter for the maintenance tax.

use anyhow::{Context, Result, anyhow};
use native_generator::bench::*;
use native_generator::{
    Dataset, ExpectedCounts, GoldenDiff, GoldenFile, GoldenVerifyResults, compare_count,
    compare_count_sum,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::runtime::Runtime;
use tokio_postgres::{Client, NoTls};
use tracing::{info, warn};

mod queries;
mod schema;

pub struct PostgresDriver {
    conn_str: String,
    rt: Runtime,
    client: Mutex<Option<Client>>,
    /// `rfc -> credit_id` of one real credit per client, built at load. Q7's
    /// synthetic payments borrow a real credit_id so PG's Q3/Q8 join
    /// (`payments.credit_id = credits.credit_id`) resolves them into the credit's
    /// empresa — never dropped by the inner join. PG recovers empresa via the
    /// join, so it needs the credit_id, not a denormalised empresa_id.
    rfc_first_credit: OnceLock<HashMap<String, String>>,
}

impl PostgresDriver {
    pub fn new(conn_str: impl Into<String>) -> Result<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(4)
            .build()
            .context("build tokio runtime")?;
        Ok(Self {
            conn_str: conn_str.into(),
            rt,
            client: Mutex::new(None),
            rfc_first_credit: OnceLock::new(),
        })
    }

    /// One real `credit_id` for `rfc`, if the load-time map is populated. Q7 uses
    /// it so its synthetic payments join back to a real credit. `None` before
    /// load or for an unknown rfc.
    pub(crate) fn q7_credit_for(&self, rfc: &str) -> Option<String> {
        self.rfc_first_credit
            .get()
            .and_then(|m| m.get(rfc).cloned())
    }

    /// Get an owned client by connecting fresh. Used by background tasks.
    fn fresh_client_blocking(&self) -> Result<Client> {
        let conn_str = self.conn_str.clone();
        self.rt.block_on(async move {
            let (client, conn) = tokio_postgres::connect(&conn_str, NoTls)
                .await
                .context("PG connect")?;
            tokio::spawn(async move {
                if let Err(e) = conn.await {
                    warn!(target: "postgres", "connection error: {e}");
                }
            });
            Ok::<Client, anyhow::Error>(client)
        })
    }

    fn ensure_client(&self) -> Result<()> {
        let mut g = self.client.lock().unwrap();
        if g.is_none() {
            *g = Some(self.fresh_client_blocking()?);
        }
        Ok(())
    }

    pub(crate) fn execute_simple(&self, sql: &str) -> Result<u64> {
        self.ensure_client()?;
        let mut g = self.client.lock().unwrap();
        let client = g.as_mut().unwrap();
        let conn_str = self.conn_str.clone();
        let result = self.rt.block_on(async { client.execute(sql, &[]).await });
        match result {
            Ok(n) => Ok(n),
            Err(e) => {
                warn!(target: "postgres", "exec failed, reconnecting: {e}");
                drop(g);
                let new_client = self.rt.block_on(async {
                    let (client, conn) = tokio_postgres::connect(&conn_str, NoTls).await?;
                    tokio::spawn(async move {
                        let _ = conn.await;
                    });
                    Ok::<Client, anyhow::Error>(client)
                })?;
                *self.client.lock().unwrap() = Some(new_client);
                Err(anyhow!(e))
            }
        }
    }

    pub(crate) fn rt(&self) -> &Runtime {
        &self.rt
    }
}

impl NativeDriver for PostgresDriver {
    fn kind(&self) -> EngineKind {
        EngineKind::Postgres
    }

    fn setup_schema(&self, mode: SchemaMode) -> Result<SchemaMetrics> {
        schema::setup(self, mode)
    }

    fn bulk_load(&self, dataset: &Dataset) -> Result<LoadMetrics> {
        info!(target: "postgres", "Phase 1: bulk-loading scale={}", dataset.scale);
        let start = Instant::now();
        let mut total: u64 = 0;

        // Topological FK order:
        total += self.copy_empresas(dataset)?;
        total += self.copy_productos(dataset)?;
        total += self.copy_clientes(dataset)?;
        total += self.copy_credits(dataset)?;
        // rfc -> one real credit_id, for Q7's synthetic payments (separate pass:
        // copy_credits moves the credits iterator into its COPY closure).
        let _ = self
            .rfc_first_credit
            .set(build_rfc_first_credit_map(dataset));
        total += self.copy_installments(dataset)?;
        total += self.copy_payments(dataset)?;
        total += self.copy_collections(dataset)?;
        total += self.copy_collection_actions(dataset)?;
        total += self.copy_credit_applications(dataset)?;
        total += self.copy_audit_log(dataset)?;
        total += self.copy_notifications(dataset)?;
        total += self.copy_bi_snapshots(dataset)?;

        let dur = start.elapsed();
        let rate = total as f64 / dur.as_secs_f64();
        info!(target: "postgres", "Phase 1 done: {} records in {:?} ({:.0} rec/s)",
              total, dur, rate);
        Ok(LoadMetrics {
            records_loaded: total,
            duration_ms: dur.as_millis() as u64,
            records_per_sec: rate,
        })
    }

    fn post_load(&self) -> Result<()> {
        info!(target: "postgres", "Phase 0.5: ANALYZE + initial REFRESH");
        self.execute_simple("ANALYZE;")?;
        // Initial materialised-view materialisation. CONCURRENTLY requires
        // a unique index — set up in schema::setup.
        let _ = self.execute_simple("REFRESH MATERIALIZED VIEW credits_by_rfc_mat;");
        let _ = self.execute_simple("REFRESH MATERIALIZED VIEW overdue_by_empresa_mat;");
        let _ = self.execute_simple("REFRESH MATERIALIZED VIEW monthly_close_mat;");
        Ok(())
    }

    fn run_query(&self, q: BusinessQuery, params: &QueryParams) -> Result<QueryExecution> {
        queries::run_one(self, q, params)
    }

    fn run_concurrent(
        &self,
        profile: &ConcurrentProfile,
        rfc_pool: &[String],
    ) -> Result<ConcurrentResults> {
        run_concurrent_workload(self, profile, rfc_pool)
    }

    fn verify(&self, expected: &ExpectedCounts) -> Result<VerifyResults> {
        info!(target: "postgres", "Phase 5: integrity verify");
        self.ensure_client()?;
        let mut diffs = Vec::new();
        let pairs = [
            ("empresas", expected.empresas),
            ("productos", expected.productos),
            ("clientes", expected.clients),
            ("credits", expected.credits),
            ("installments", expected.installments),
            ("payments", expected.payments),
            ("collections", expected.collections),
            ("collection_actions", expected.collection_actions),
            ("credit_applications", expected.applications),
            ("audit_log", expected.audit_log),
            ("notifications", expected.notifications),
            ("bi_snapshots", expected.bi_snapshots),
        ];
        for (table, exp) in pairs {
            let sql = format!("SELECT COUNT(*) FROM {}", table);
            let n: i64 = self.rt.block_on(async {
                let mut g = self.client.lock().unwrap();
                let client = g.as_mut().unwrap();
                let row = client.query_one(&sql, &[]).await?;
                Ok::<i64, anyhow::Error>(row.get::<_, i64>(0))
            })?;
            let observed = n as u64;
            if observed != exp {
                diffs.push(EntityDiff {
                    entity: table.to_string(),
                    expected: exp,
                    observed,
                });
            }
        }
        Ok(VerifyResults {
            exact: diffs.is_empty(),
            diffs,
        })
    }

    fn verify_golden(&self, golden: &GoldenFile) -> Result<GoldenVerifyResults> {
        info!(target: "postgres", "Phase 1.5: verify_golden vs seed={} scale={}",
              golden.seed, golden.scale);
        self.ensure_client()?;
        let mut diffs: Vec<GoldenDiff> = Vec::new();
        let tol = golden.tolerance_f64_relative;

        // V1 — credits count + sum(monto) over normalised `credits` table.
        let (n, s) = self.pg_count_sum_numeric(
            "SELECT count(*)::bigint, COALESCE(sum(monto), 0)::float8 FROM credits",
        )?;
        compare_count_sum(
            "V1_credits_total",
            &golden.verify_queries.v1_credits_total,
            n,
            s,
            tol,
            &mut diffs,
        );

        // V2 — installments overdue: count + sum(monto_total) WHERE status='overdue'.
        let (n, s) = self.pg_count_sum_numeric(
            "SELECT count(*)::bigint, COALESCE(sum(monto_total), 0)::float8 FROM installments WHERE status = 'overdue'"
        )?;
        compare_count_sum(
            "V2_installments_overdue",
            &golden.verify_queries.v2_installments_overdue,
            n,
            s,
            tol,
            &mut diffs,
        );

        // V3 — payments count + sum(monto).
        let (n, s) = self.pg_count_sum_numeric(
            "SELECT count(*)::bigint, COALESCE(sum(monto), 0)::float8 FROM payments",
        )?;
        compare_count_sum(
            "V3_payments_total",
            &golden.verify_queries.v3_payments_total,
            n,
            s,
            tol,
            &mut diffs,
        );

        // V4 — counts per (lobe, type). PG's normalised model maps each
        // (lobe × type) entry to one table; verify_v4_pg below routes the
        // lobe-table mapping declared in `verify` (line 188 onward).
        self.verify_v4_pg(&golden.verify_queries.v4_lobe_type_counts, &mut diffs)?;

        // V5 — clients distinct rfc.
        let n = self.pg_count_one("SELECT count(DISTINCT rfc)::bigint FROM clientes")?;
        compare_count(
            "V5_clients_distinct_rfc",
            &golden.verify_queries.v5_clients_distinct_rfc,
            n,
            &mut diffs,
        );

        // V6 — configuracion catalogue: empresas + productos + total.
        let n_emp = self.pg_count_one("SELECT count(*)::bigint FROM empresas")?;
        if n_emp != golden.verify_queries.v6_config_counts.empresas {
            let exp = golden.verify_queries.v6_config_counts.empresas as f64;
            diffs.push(GoldenDiff {
                query: "V6_config:empresas".to_string(),
                field: "n".to_string(),
                expected: exp,
                observed: n_emp as f64,
                relative_delta: (n_emp as f64 - exp).abs() / exp.max(1.0),
            });
        }
        let n_prod = self.pg_count_one("SELECT count(*)::bigint FROM productos")?;
        if n_prod != golden.verify_queries.v6_config_counts.productos {
            let exp = golden.verify_queries.v6_config_counts.productos as f64;
            diffs.push(GoldenDiff {
                query: "V6_config:productos".to_string(),
                field: "n".to_string(),
                expected: exp,
                observed: n_prod as f64,
                relative_delta: (n_prod as f64 - exp).abs() / exp.max(1.0),
            });
        }
        let n_total = n_emp + n_prod;
        if n_total != golden.verify_queries.v6_config_counts.total {
            let exp = golden.verify_queries.v6_config_counts.total as f64;
            diffs.push(GoldenDiff {
                query: "V6_config:_total".to_string(),
                field: "n".to_string(),
                expected: exp,
                observed: n_total as f64,
                relative_delta: (n_total as f64 - exp).abs() / exp.max(1.0),
            });
        }

        info!(target: "postgres", "Phase 1.5 done: {} diffs", diffs.len());
        Ok(GoldenVerifyResults {
            overall_match: diffs.is_empty(),
            diffs,
        })
    }
}

// ── verify_golden helpers ────────────────────────────────────────────

impl PostgresDriver {
    fn pg_count_sum_numeric(&self, sql: &str) -> Result<(u64, f64)> {
        let row = self.rt.block_on(async {
            let mut g = self.client.lock().unwrap();
            let client = g.as_mut().unwrap();
            let row = client.query_one(sql, &[]).await?;
            let n: i64 = row.get(0);
            let s: f64 = row.get(1);
            Ok::<(i64, f64), anyhow::Error>((n, s))
        })?;
        Ok((row.0 as u64, row.1))
    }

    fn pg_count_one(&self, sql: &str) -> Result<u64> {
        let n = self.rt.block_on(async {
            let mut g = self.client.lock().unwrap();
            let client = g.as_mut().unwrap();
            let row = client.query_one(sql, &[]).await?;
            Ok::<i64, anyhow::Error>(row.get::<_, i64>(0))
        })?;
        Ok(n as u64)
    }

    /// V4 lobe×type mapping for PG's normalised schema. xyzDB lobe names
    /// project onto PG tables 1:1 except `creditos` (heterogeneous lobe)
    /// → 5 tables and `operaciones` → 3 tables. `_total` keys aggregate
    /// across the lobe's tables.
    fn verify_v4_pg(
        &self,
        v4: &native_generator::V4LobeTypeCounts,
        diffs: &mut Vec<GoldenDiff>,
    ) -> Result<()> {
        // clientes (single table; only `_total` key in golden).
        for (typ, &exp) in v4.clientes.iter() {
            let observed = self
                .pg_count_one("SELECT count(*)::bigint FROM clientes")
                .unwrap_or(0);
            self.push_v4_diff_if_mismatch("clientes", typ, exp, observed, diffs);
        }
        // creditos lobe → 5 PG tables.
        for (typ, &exp) in v4.creditos.iter() {
            let table = match typ.as_str() {
                "Credit" => "credits",
                "Installment" => "installments",
                "Payment" => "payments",
                "Collection" => "collections",
                "CollectionAction" => "collection_actions",
                _ => continue,
            };
            let observed = self
                .pg_count_one(&format!("SELECT count(*)::bigint FROM {table}"))
                .unwrap_or(0);
            self.push_v4_diff_if_mismatch("creditos", typ, exp, observed, diffs);
        }
        // operaciones lobe → 3 PG tables.
        for (typ, &exp) in v4.operaciones.iter() {
            let table = match typ.as_str() {
                "CreditApplication" => "credit_applications",
                "AuditLog" => "audit_log",
                "Notification" => "notifications",
                _ => continue,
            };
            let observed = self
                .pg_count_one(&format!("SELECT count(*)::bigint FROM {table}"))
                .unwrap_or(0);
            self.push_v4_diff_if_mismatch("operaciones", typ, exp, observed, diffs);
        }
        // configuracion lobe → 2 PG tables.
        for (typ, &exp) in v4.configuracion.iter() {
            let table = match typ.as_str() {
                "Empresa" => "empresas",
                "Producto" => "productos",
                _ => continue,
            };
            let observed = self
                .pg_count_one(&format!("SELECT count(*)::bigint FROM {table}"))
                .unwrap_or(0);
            self.push_v4_diff_if_mismatch("configuracion", typ, exp, observed, diffs);
        }
        // bi (single table; only `_total` key).
        for (typ, &exp) in v4.bi.iter() {
            let observed = self
                .pg_count_one("SELECT count(*)::bigint FROM bi_snapshots")
                .unwrap_or(0);
            self.push_v4_diff_if_mismatch("bi", typ, exp, observed, diffs);
        }
        Ok(())
    }

    fn push_v4_diff_if_mismatch(
        &self,
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
}

// ── COPY-based bulk load helpers ──────────────────────────────────────

impl PostgresDriver {
    fn copy<F>(&self, table: &str, columns: &[&str], mut emit: F) -> Result<u64>
    where
        F: FnMut() -> Option<String>,
    {
        use bytes::Bytes;
        use futures_util::SinkExt;
        self.ensure_client()?;
        let cols = columns.join(", ");
        let copy_sql = format!("COPY {} ({}) FROM STDIN WITH (FORMAT text)", table, cols);
        let total = self.rt.block_on(async {
            let mut g = self.client.lock().unwrap();
            let client = g.as_mut().unwrap();
            let sink = client.copy_in(&copy_sql).await?;
            tokio::pin!(sink);
            let mut count: u64 = 0;
            let mut buf = String::with_capacity(1_000_000);
            while let Some(line) = emit() {
                buf.push_str(&line);
                buf.push('\n');
                count += 1;
                if buf.len() > 900_000 {
                    sink.send(Bytes::from(buf.as_bytes().to_vec())).await?;
                    buf.clear();
                }
            }
            if !buf.is_empty() {
                sink.send(Bytes::from(buf.as_bytes().to_vec())).await?;
            }
            sink.finish().await?;
            Ok::<u64, anyhow::Error>(count)
        })?;
        Ok(total)
    }

    fn copy_empresas(&self, ds: &Dataset) -> Result<u64> {
        let mut iter = ds.empresas();
        self.copy(
            "empresas",
            &["empresa_id", "nombre", "region", "activa"],
            move || {
                iter.next().map(|e| {
                    format!(
                        "{}\t{}\t{}\t{}",
                        e.empresa_id,
                        pg_escape(&e.nombre),
                        pg_escape(&e.region),
                        if e.activa { "t" } else { "f" }
                    )
                })
            },
        )
    }

    fn copy_productos(&self, ds: &Dataset) -> Result<u64> {
        let mut iter = ds.productos();
        self.copy(
            "productos",
            &[
                "producto_id",
                "empresa_id",
                "nombre",
                "tasa_interes",
                "plazo_meses",
            ],
            move || {
                iter.next().map(|p| {
                    format!(
                        "{}\t{}\t{}\t{:.4}\t{}",
                        p.producto_id,
                        p.empresa_id,
                        pg_escape(&p.nombre),
                        p.tasa_interes,
                        p.plazo_meses
                    )
                })
            },
        )
    }

    fn copy_clientes(&self, ds: &Dataset) -> Result<u64> {
        let mut iter = ds.clients();
        self.copy(
            "clientes",
            &[
                "rfc",
                "curp",
                "nombre",
                "scoring",
                "datos_ubicacion",
                "datos_identificacion",
                "caracteristicas_fiscales",
                "tags",
                "fecha_alta",
            ],
            move || {
                iter.next().map(|c| {
                    let scoring = serde_json::to_string(&c.scoring).unwrap();
                    let ubicacion = serde_json::to_string(&c.datos_ubicacion).unwrap();
                    let ident = serde_json::to_string(&c.datos_identificacion).unwrap();
                    let fiscales = serde_json::to_string(&c.caracteristicas_fiscales).unwrap();
                    let tags = format!(
                        "{{{}}}",
                        c.tags
                            .iter()
                            .map(|t| format!("\"{}\"", t.replace('"', "\\\"")))
                            .collect::<Vec<_>>()
                            .join(",")
                    );
                    format!(
                        "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                        c.rfc,
                        c.curp,
                        pg_escape(&c.nombre),
                        pg_escape(&scoring),
                        pg_escape(&ubicacion),
                        pg_escape(&ident),
                        pg_escape(&fiscales),
                        pg_escape(&tags),
                        c.fecha_alta.to_rfc3339()
                    )
                })
            },
        )
    }

    fn copy_credits(&self, ds: &Dataset) -> Result<u64> {
        let mut iter = ds.credits();
        self.copy(
            "credits",
            &[
                "credit_id",
                "rfc",
                "empresa_id",
                "producto_id",
                "monto",
                "status",
                "fecha_creacion",
                "fecha_vencimiento",
                "dias_atraso",
            ],
            move || {
                iter.next().map(|c| {
                    format!(
                        "{}\t{}\t{}\t{}\t{:.2}\t{}\t{}\t{}\t{}",
                        c.credit_id,
                        c.rfc,
                        c.empresa_id,
                        c.producto_id,
                        c.monto,
                        c.status.as_str(),
                        c.fecha_creacion.to_rfc3339(),
                        c.fecha_vencimiento.format("%Y-%m-%d"),
                        c.dias_atraso
                    )
                })
            },
        )
    }

    fn copy_installments(&self, ds: &Dataset) -> Result<u64> {
        let mut iter = ds.installments();
        self.copy(
            "installments",
            &[
                "installment_id",
                "credit_id",
                "empresa_id",
                "numero",
                "monto_total",
                "status",
                "dias_atraso",
                "fecha_vencimiento",
            ],
            move || {
                iter.next().map(|i| {
                    format!(
                        "{}\t{}\t{}\t{}\t{:.2}\t{}\t{}\t{}",
                        i.installment_id,
                        i.credit_id,
                        i.empresa_id,
                        i.numero,
                        i.monto_total,
                        i.status.as_str(),
                        i.dias_atraso,
                        i.fecha_vencimiento.format("%Y-%m-%d"),
                    )
                })
            },
        )
    }

    fn copy_payments(&self, ds: &Dataset) -> Result<u64> {
        let mut iter = ds.payments();
        self.copy(
            "payments",
            &[
                "payment_id",
                "credit_id",
                "installment_id",
                "rfc",
                "monto",
                "fecha_pago",
                "metodo",
            ],
            move || {
                iter.next().map(|p| {
                    let inst = p
                        .installment_id
                        .as_ref()
                        .map(|s| s.as_str())
                        .unwrap_or("\\N");
                    format!(
                        "{}\t{}\t{}\t{}\t{:.2}\t{}\t{}",
                        p.payment_id,
                        p.credit_id,
                        inst,
                        p.rfc,
                        p.monto,
                        p.fecha_pago.to_rfc3339(),
                        p.metodo
                    )
                })
            },
        )
    }

    fn copy_collections(&self, ds: &Dataset) -> Result<u64> {
        let mut iter = ds.collections();
        self.copy(
            "collections",
            &[
                "collection_id",
                "credit_id",
                "monto_pendiente",
                "status",
                "fecha_inicio",
            ],
            move || {
                iter.next().map(|c| {
                    format!(
                        "{}\t{}\t{:.2}\t{}\t{}",
                        c.collection_id,
                        c.credit_id,
                        c.monto_pendiente,
                        c.status.as_str(),
                        c.fecha_inicio.to_rfc3339()
                    )
                })
            },
        )
    }

    fn copy_collection_actions(&self, ds: &Dataset) -> Result<u64> {
        let mut iter = ds.collection_actions();
        self.copy(
            "collection_actions",
            &["action_id", "collection_id", "tipo", "fecha", "resultado"],
            move || {
                iter.next().map(|a| {
                    format!(
                        "{}\t{}\t{}\t{}\t{}",
                        a.action_id,
                        a.collection_id,
                        a.tipo,
                        a.fecha.to_rfc3339(),
                        a.resultado
                    )
                })
            },
        )
    }

    fn copy_credit_applications(&self, ds: &Dataset) -> Result<u64> {
        let mut iter = ds.credit_applications();
        self.copy(
            "credit_applications",
            &[
                "application_id",
                "rfc",
                "empresa_id",
                "producto_id",
                "monto_solicitado",
                "status",
                "fecha_solicitud",
            ],
            move || {
                iter.next().map(|a| {
                    format!(
                        "{}\t{}\t{}\t{}\t{:.2}\t{}\t{}",
                        a.application_id,
                        a.rfc,
                        a.empresa_id,
                        a.producto_id,
                        a.monto_solicitado,
                        a.status.as_str(),
                        a.fecha_solicitud.to_rfc3339()
                    )
                })
            },
        )
    }

    fn copy_audit_log(&self, ds: &Dataset) -> Result<u64> {
        let mut iter = ds.audit_log();
        self.copy(
            "audit_log",
            &["rfc", "credit_id", "action_type", "details", "fecha"],
            move || {
                iter.next().map(|a| {
                    let cid = a.credit_id.as_ref().map(|s| s.as_str()).unwrap_or("\\N");
                    let det = serde_json::to_string(&a.details).unwrap();
                    format!(
                        "{}\t{}\t{}\t{}\t{}",
                        a.rfc,
                        cid,
                        a.action_type,
                        pg_escape(&det),
                        a.fecha.to_rfc3339()
                    )
                })
            },
        )
    }

    fn copy_notifications(&self, ds: &Dataset) -> Result<u64> {
        let mut iter = ds.notifications();
        self.copy(
            "notifications",
            &["rfc", "canal", "contenido", "fecha"],
            move || {
                iter.next().map(|n| {
                    format!(
                        "{}\t{}\t{}\t{}",
                        n.rfc,
                        n.canal,
                        pg_escape(&n.contenido),
                        n.fecha.to_rfc3339()
                    )
                })
            },
        )
    }

    fn copy_bi_snapshots(&self, ds: &Dataset) -> Result<u64> {
        let mut iter = ds.bi_snapshots();
        self.copy(
            "bi_snapshots",
            &["empresa_id", "fecha", "metricas"],
            move || {
                iter.next().map(|b| {
                    let m = serde_json::to_string(&b.metricas).unwrap();
                    format!(
                        "{}\t{}\t{}",
                        b.empresa_id,
                        b.fecha.format("%Y-%m-%d"),
                        pg_escape(&m)
                    )
                })
            },
        )
    }
}

fn pg_escape(s: &str) -> String {
    // PostgreSQL COPY text format escapes: \\ \t \n \r
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    out
}

/// `rfc -> credit_id` for one real credit per client (the first seen). Q7's
/// synthetic payments borrow this credit_id so PG's inner join resolves them.
fn build_rfc_first_credit_map(ds: &Dataset) -> HashMap<String, String> {
    let mut map = HashMap::with_capacity(200_000);
    for c in ds.credits() {
        map.entry(c.rfc).or_insert(c.credit_id);
    }
    map
}

// ── Concurrent runner with refresh thread ─────────────────────────────

fn run_concurrent_workload(
    driver: &PostgresDriver,
    profile: &ConcurrentProfile,
    rfc_pool: &[String],
) -> Result<ConcurrentResults> {
    info!(target: "postgres",
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
    let conn_str = driver.conn_str.clone();
    // rfc→credit_id for the pool's clients, so Phase-3 Q7 writes borrow a real
    // credit_id (same invariant as the cold path). Filtered to the pool to stay
    // small; shared read-only across threads.
    let rfc_map: Arc<HashMap<String, String>> = {
        let full = driver.rfc_first_credit.get();
        let m: HashMap<String, String> = rfc_pool
            .iter()
            .filter_map(|r| {
                full.and_then(|f| f.get(r.as_str()))
                    .map(|c| (r.clone(), c.clone()))
            })
            .collect();
        Arc::new(m)
    };
    let mut handles: Vec<std::thread::JoinHandle<()>> = Vec::new();

    // Refresh threads (one per cadence). Each runs its own connection.
    // Phase 2 verification refinement (Architecture β): dispatch by
    // thread INDEX, not by cadence VALUE. Bucket-keyed dispatch (legacy
    // v0.2.5 + Phase 2.b/c) cannot distinguish two mat-views sharing the
    // same cadence (design SPEC §8.B5 commits {30, 30, 60}). Each
    // orchestrator-passed cadence position maps to a fixed target;
    // rotation drift requires both orchestrator config AND lib.rs
    // dispatch to update together. v0.3.4 Path α refactor (explicit
    // (target, cadence) tuple list per §13.4 Entry 8) eliminates the
    // coupling.
    let cadence_iter: &[u64] = &profile.refresh_cadences_secs;
    for (idx, &cadence) in cadence_iter.iter().enumerate() {
        let conn_str = conn_str.clone();
        let stop_flag = stop_flag.clone();
        let refresh_count = refresh_count.clone();
        let refresh_total_ms = refresh_total_ms.clone();
        let h = std::thread::spawn(move || {
            let rt = match tokio::runtime::Runtime::new() {
                Ok(r) => r,
                Err(_) => return,
            };
            let client = match rt.block_on(async {
                let (c, conn) = tokio_postgres::connect(&conn_str, NoTls).await?;
                tokio::spawn(async move {
                    let _ = conn.await;
                });
                Ok::<Client, anyhow::Error>(c)
            }) {
                Ok(c) => c,
                Err(_) => return,
            };
            // Phase 2 verification refinement enumerate-by-index dispatch.
            // Index → target mapping per design SPEC §8.B5 cadence list
            // {30, 30, 60} (orchestrator config order):
            //   idx 0 (30 s) → overdue_by_empresa_mat (Q5 §8.B5)
            //   idx 1 (30 s) → credits_by_rfc_mat (Q2 §8.B2)
            //   idx 2 (60 s) → monthly_close_mat (Q8 §8.B8)
            // Methodology gate §8.B5: 3 background REFRESH threads vs 2
            // cores T6; Phase 4 acceptance test for `*REFRESH-confounded*`
            // marker if >20% CPU or >30% IOPS.
            let target = match idx {
                0 => "REFRESH MATERIALIZED VIEW CONCURRENTLY overdue_by_empresa_mat",
                1 => "REFRESH MATERIALIZED VIEW CONCURRENTLY credits_by_rfc_mat",
                2 => "REFRESH MATERIALIZED VIEW CONCURRENTLY monthly_close_mat",
                _ => return,
            };
            while !stop_flag.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(cadence));
                if stop_flag.load(Ordering::Relaxed) {
                    break;
                }
                let t0 = Instant::now();
                let ok = rt.block_on(async { client.execute(target, &[]).await.is_ok() });
                if ok {
                    refresh_count.fetch_add(1, Ordering::Relaxed);
                    refresh_total_ms.fetch_add(t0.elapsed().as_millis() as u64, Ordering::Relaxed);
                }
            }
        });
        handles.push(h);
    }

    // Phase 3 v0.3.3: mixed-mode threads driven by `ErraticaPicker`
    // (design §6). Each thread instantiates own MMPP + session + drift +
    // mixer. State-dependent R/W mix per §6.4: Idle 95R/5W, Busy 70R/30W.
    // Writes (Q7) dispatch via `queries::exec_async` same as reads;
    // separate writer thread split (v0.2.5 + Phase 2.b) collapsed to
    // mixed-mode. Thread count = readers + writers from `ErraticaConfig`.
    let total_threads = profile.readers + profile.writers;
    // v0.5 multi-persona shared anchors (same pattern as xyzdb driver).
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
        let conn_str = conn_str.clone();
        let pool = (*pool).clone();
        let samples = samples.clone();
        let reads = reads.clone();
        let writes = writes.clone();
        let cfg = profile.erratica.clone();
        let schedule = profile.schedule.clone();
        let rfc_map = rfc_map.clone();
        let h = std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("rt");
            let mut client = match rt.block_on(async {
                let (c, conn) = tokio_postgres::connect(&conn_str, NoTls).await?;
                tokio::spawn(async move {
                    let _ = conn.await;
                });
                Ok::<Client, anyhow::Error>(c)
            }) {
                Ok(c) => c,
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
                        queries::exec_async(
                            &mut client,
                            query,
                            &rfc,
                            50,
                            rfc_map.get(&rfc).map(|s| s.as_str()),
                        )
                        .await
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

    // Wait until duration elapses
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
