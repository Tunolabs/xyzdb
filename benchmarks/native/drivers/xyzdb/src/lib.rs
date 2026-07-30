//! Native xyzDB driver for the native cross-engine bench.
//!
//! Schema and queries follow the native cross-engine bench design
//! (Q1..Q7).
//!
//! Uses the V1 text protocol (`xytalk-spec.md` §5) for all paths in this
//! version. V3 binary bulk is an optimisation path tracked as v0.2.5.1
//! follow-up — V1 `PUT BATCH` is correct and adequate for Scale 0.1
//! validation runs.

use anyhow::{Context, Result, bail};
use native_generator::bench::*;
use native_generator::{
    Dataset, ExpectedCounts, GoldenDiff, GoldenFile, GoldenVerifyResults, compare_count,
    compare_count_sum,
};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

mod queries;
mod schema;

const PROTO_V1: u8 = 1;
const STATUS_OK: u8 = 0x00;
const MAX_RESPONSE: usize = 512 * 1024 * 1024;

/// Bulk-load batch size. V1 PUT BATCH frames are bounded by the server's
/// 16 MiB frame cap (crates/server protocol); a 5 000-record batch keeps
/// frames well under it and GC-friendly.
const BULK_BATCH: usize = 5_000;

pub struct XyzdbDriver {
    host: String,
    port: u16,
    /// One persistent connection used for serial schema + queries. The
    /// concurrent runner spawns its own per-thread connections.
    conn: Mutex<Option<TcpStream>>,
    /// 30-day cutoff (epoch millis) baked into the Q6/Q8 ghosts at schema
    /// setup. Q8 reads the composite ghost via the router GROUP BY | AGGREGATE
    /// idiom, whose per-metric predicate must carry the *same* literal cutoff
    /// as the ghost — otherwise the aggregate signatures differ, the router
    /// declines the ghost, and the query silently falls back to a runtime
    /// scan. Set by `schema::setup`, read by `run_q8_monthly_close`.
    cutoff_ms: std::sync::atomic::AtomicI64,
    /// `rfc -> (credit_id, empresa_id)` of one real credit per client, built at
    /// load. Q7's synthetic payments borrow a real (credit, empresa) so they
    /// attach to a real credit (parity with PG's join in Q3) and a real empresa
    /// group in Q8 — never a phantom null-empresa. See `build_q7_put_batch`.
    rfc_first_credit: std::sync::OnceLock<HashMap<String, (String, String)>>,
}

impl XyzdbDriver {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
            conn: Mutex::new(None),
            cutoff_ms: std::sync::atomic::AtomicI64::new(0),
            rfc_first_credit: std::sync::OnceLock::new(),
        }
    }

    /// One real `(credit_id, empresa_id)` for `rfc`, if the load-time map is
    /// populated. Q7 uses it so its synthetic payments attach to a real credit
    /// and a real empresa group. `None` before load or for an unknown rfc.
    pub(crate) fn q7_credit_for(&self, rfc: &str) -> Option<(String, String)> {
        self.rfc_first_credit
            .get()
            .and_then(|m| m.get(rfc).cloned())
    }

    pub(crate) fn set_cutoff_ms(&self, cutoff_ms: i64) {
        self.cutoff_ms
            .store(cutoff_ms, std::sync::atomic::Ordering::Relaxed);
    }

    pub(crate) fn cutoff_ms(&self) -> i64 {
        self.cutoff_ms.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn connect(&self) -> Result<TcpStream> {
        let addr = format!("{}:{}", self.host, self.port);
        let stream = TcpStream::connect(&addr).with_context(|| format!("xyzDB connect {addr}"))?;
        stream.set_nodelay(true)?;
        Ok(stream)
    }

    fn ensure_conn(&self) -> Result<()> {
        let mut g = self.conn.lock().unwrap();
        if g.is_none() {
            *g = Some(self.connect()?);
        }
        Ok(())
    }

    fn execute(&self, query: &str) -> Result<String> {
        self.ensure_conn()?;
        let mut g = self.conn.lock().unwrap();
        let conn = g.as_mut().unwrap();
        execute_on(conn, query)
    }

    /// Convenience: run statements that should always return OK.
    fn execute_ok(&self, query: &str) -> Result<()> {
        let resp = self.execute(query)?;
        if !resp.starts_with("OK") && !resp.is_empty() && !resp.contains("indexed") {
            warn!(target: "xyzdb", "unexpected response: {}",
                  &resp[..resp.len().min(160)]);
        }
        Ok(())
    }
}

fn execute_on(conn: &mut TcpStream, query: &str) -> Result<String> {
    let payload = query.as_bytes();
    conn.write_all(&[PROTO_V1])?;
    conn.write_all(&(payload.len() as u32).to_be_bytes())?;
    conn.write_all(payload)?;
    conn.flush()?;

    let mut status_buf = [0u8; 1];
    conn.read_exact(&mut status_buf)?;
    let status = status_buf[0];

    let mut len_buf = [0u8; 4];
    conn.read_exact(&mut len_buf)?;
    let length = u32::from_be_bytes(len_buf) as usize;
    if length > MAX_RESPONSE {
        bail!("xyzDB response too large: {length} bytes");
    }

    let mut buf = vec![0u8; length];
    if length > 0 {
        conn.read_exact(&mut buf)?;
    }
    let text = String::from_utf8_lossy(&buf).to_string();
    if status != STATUS_OK {
        bail!("xyzDB error: {text}");
    }
    Ok(text)
}

impl NativeDriver for XyzdbDriver {
    fn kind(&self) -> EngineKind {
        EngineKind::Xyzdb
    }

    fn setup_schema(&self, mode: SchemaMode) -> Result<SchemaMetrics> {
        schema::setup(self, mode)
    }

    fn bulk_load(&self, dataset: &Dataset) -> Result<LoadMetrics> {
        info!(target: "xyzdb", "Phase 1: bulk-loading scale={}", dataset.scale);
        let start = Instant::now();
        let mut total: u64 = 0;

        // Order: configuracion (catalog) → clientes → creditos (heterogeneous,
        // all _types interleaved by gravity bucket) → operaciones → bi.
        // The order respects gravity locality so V1 PUT BATCH frames stay
        // within one gravity bucket as long as possible.

        // configuracion
        total += self.bulk_emit_empresas(dataset)?;
        total += self.bulk_emit_productos(dataset)?;

        // clientes
        total += self.bulk_emit_clients(dataset)?;

        // creditos (heterogeneous lobe — Credit + Installment + Payment + Collection + CollectionAction)
        total += self.bulk_emit_credits(dataset)?;
        total += self.bulk_emit_installments(dataset)?;
        // Denormalise empresa_id onto Payment / Collection / CollectionAction.
        // The generator model carries only credit_id / rfc on these entities
        // (model.rs:174-203), so empresa_id is recovered from the parent
        // credit. Q8's per-empresa metrics (cobrado_*, acciones_n) group
        // payments and collection actions by empresa_id; without this they
        // land in the empty group. PostgreSQL recovers empresa_id via JOIN to
        // credits at query time; xyzDB and Mongo (mongo/lib.rs:826) denormalise
        // at load since neither joins.
        let credit_to_empresa = build_credit_empresa_map(dataset);
        // rfc -> one real (credit_id, empresa_id), for Q7's synthetic payments.
        let _ = self
            .rfc_first_credit
            .set(build_rfc_first_credit_map(dataset));
        total += self.bulk_emit_payments(dataset, &credit_to_empresa)?;
        total += self.bulk_emit_collections(dataset, &credit_to_empresa)?;
        total += self.bulk_emit_collection_actions(dataset, &credit_to_empresa)?;

        // operaciones (heterogeneous lobe — CreditApplication + AuditLog + Notification)
        total += self.bulk_emit_applications(dataset)?;
        total += self.bulk_emit_audit_log(dataset)?;
        total += self.bulk_emit_notifications(dataset)?;

        // bi
        total += self.bulk_emit_bi(dataset)?;

        let dur = start.elapsed();
        let rate = total as f64 / dur.as_secs_f64();
        info!(target: "xyzdb", "Phase 1 done: {} records in {:?} ({:.0} rec/s)",
              total, dur, rate);

        Ok(LoadMetrics {
            records_loaded: total,
            duration_ms: dur.as_millis() as u64,
            records_per_sec: rate,
        })
    }

    fn post_load(&self) -> Result<()> {
        info!(target: "xyzdb", "Phase 0.5: BULKMODE OFF + REFRESH GHOST + COMPACT + AUTOANCHOR APPLY rfc");
        self.execute_ok("BULKMODE OFF")?;
        // COMPACT runs AFTER the refresh chain (below), not here: each
        // REFRESH drops + rebuilds a ghost, leaving per-key tombstones and
        // shadowed duplicate versions in the ghost keyspace plus millions
        // of rollup entries in the dictionary. Compacting before the
        // refreshes measured queries against that garbage (Q4 0.9→115 ms
        // in the first 0.7.6 validation); one compact after the chain
        // cleans spatial, ghost and dictionary keyspaces in a single pass.
        // GROUP BY / AGGREGATE ghosts are NOT maintained by the incremental
        // notify_write hooks (those only add covering-index entries), so an
        // aggregate ghost created on the empty lobe in Phase 0 stays empty after
        // a BULKMODE load — it must be rebuilt from the loaded data. An earlier
        // attempt (64c0b5f) OOMed the engine on the ~75M-row source lobe and was
        // rolled back (c7fad92): the ghost build materialised every matching row
        // into one Vec before flushing. Fixed engine-side to stream
        // (O(buffer+groups) memory, ghost.rs::create), so REFRESH now completes.
        // This is what populates Q5OverdueByEmpresa.
        self.execute_ok(r#"REFRESH GHOST "overdue_by_empresa""#)?;
        // The additional pure GROUP BY/AGGREGATE ghosts share the same
        // incremental-fold gap. Refreshing them populates them so the router
        // serves the precomputed path instead of a primary scan.
        // credits_by_rfc groups by rfc (one group per client → millions);
        // since 0.7.6 high-cardinality ghosts spill their rollups to disk
        // ("lightweight", design/v0.7.6-lightweight-ghosts.md), so the
        // refresh that was dropped in 0.7.5 for costing ~1 GB of RAM is safe
        // again — Q2 gets the ghost point-read at block-cache cost. The Q8
        // ghosts group by empresa_id (thousands) and stay in RAM.
        // Pre-0.7.6 these were populated as a side effect of notify_write
        // during the bulk load; the engine now defers aggregate maintenance
        // under BULKMODE (the per-record rollup RMW collapsed load
        // throughput), so every aggregate ghost must be rebuilt here.
        // credits_by_rfc serves both Q2 (single-group read) and Q4
        // (all-group read + client top-N); monthly_close_by_emp is the Q8
        // composite.
        self.execute_ok(r#"REFRESH GHOST "credits_by_rfc""#)?; // Q2 + Q4
        self.execute_ok(r#"REFRESH GHOST "monthly_close_by_emp""#)?; // Q8
        self.execute_ok("COMPACT")?;
        // Only `rfc` in `clientes` is a legitimate UNIQUE anchor (see
        // schema.rs comment). Idempotent re-apply (Finding 12 contract).
        self.execute_ok(r#"AUTOANCHOR APPLY "rfc" IN "clientes""#)?;
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
        info!(target: "xyzdb", "Phase 5: integrity verify");
        let count = |lobe: &str| -> Result<u64> {
            let resp = self.execute(&format!(r#"SCAN "{}" | AGGREGATE count()"#, lobe))?;
            // Response format: "count: N" (V1 text). Parse naively.
            for line in resp.lines() {
                if let Some(rest) = line.strip_prefix("count: ") {
                    if let Ok(v) = rest.trim().parse::<u64>() {
                        return Ok(v);
                    }
                }
                if let Ok(v) = line.trim().parse::<u64>() {
                    return Ok(v);
                }
            }
            bail!("could not parse count from: {}", resp);
        };

        let mut diffs = Vec::new();
        let creditos_total = expected.credits
            + expected.installments
            + expected.payments
            + expected.collections
            + expected.collection_actions;
        let operaciones_total = expected.applications + expected.audit_log + expected.notifications;
        let configuracion_total = expected.empresas + expected.productos;

        let pairs = [
            ("clientes", expected.clients),
            ("creditos", creditos_total),
            ("operaciones", operaciones_total),
            ("configuracion", configuracion_total),
            ("bi", expected.bi_snapshots),
        ];
        for (lobe, exp) in pairs {
            let observed = count(lobe).unwrap_or(0);
            if observed != exp {
                diffs.push(EntityDiff {
                    entity: lobe.to_string(),
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
        info!(target: "xyzdb", "Phase 1.5: verify_golden vs seed={} scale={}",
              golden.seed, golden.scale);
        let mut diffs: Vec<GoldenDiff> = Vec::new();
        let tol = golden.tolerance_f64_relative;

        // V1 — credits count + sum(monto). _type filter on creditos lobe.
        let (n, s) = self.aggregate_count_sum(
            r#"SCAN "creditos" WHERE _type = "Credit" | AGGREGATE count(), sum(monto)"#,
            "monto",
        )?;
        compare_count_sum(
            "V1_credits_total",
            &golden.verify_queries.v1_credits_total,
            n,
            s,
            tol,
            &mut diffs,
        );

        // V2 — installments overdue: count + sum(monto_total). _type +
        // status filter both inside the SCAN WHERE.
        let (n, s) = self.aggregate_count_sum(
            r#"SCAN "creditos" WHERE _type = "Installment" AND status = "overdue" | AGGREGATE count(), sum(monto_total)"#,
            "monto_total",
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
        let (n, s) = self.aggregate_count_sum(
            r#"SCAN "creditos" WHERE _type = "Payment" | AGGREGATE count(), sum(monto)"#,
            "monto",
        )?;
        compare_count_sum(
            "V3_payments_total",
            &golden.verify_queries.v3_payments_total,
            n,
            s,
            tol,
            &mut diffs,
        );

        // V4 — lobe × _type counts. Iterate the golden's BTreeMaps so we
        // emit one query per declared (lobe, type) entry; this also keeps
        // the diff key naming aligned across drivers.
        self.verify_v4_lobe_counts(
            "clientes",
            &golden.verify_queries.v4_lobe_type_counts.clientes,
            &mut diffs,
        )?;
        self.verify_v4_lobe_counts(
            "creditos",
            &golden.verify_queries.v4_lobe_type_counts.creditos,
            &mut diffs,
        )?;
        self.verify_v4_lobe_counts(
            "operaciones",
            &golden.verify_queries.v4_lobe_type_counts.operaciones,
            &mut diffs,
        )?;
        self.verify_v4_lobe_counts(
            "configuracion",
            &golden.verify_queries.v4_lobe_type_counts.configuracion,
            &mut diffs,
        )?;
        self.verify_v4_lobe_counts(
            "bi",
            &golden.verify_queries.v4_lobe_type_counts.bi,
            &mut diffs,
        )?;

        // V5 — clients distinct rfc. xyzDB anchored `rfc` in `clientes`
        // (post_load AUTOANCHOR APPLY) so a plain count() over the lobe
        // is the cardinality (one record per unique rfc by anchor
        // contract). Future drift (multiple records per rfc) would
        // surface here as a count mismatch, not as a silent pass.
        let n = self.aggregate_count(r#"SCAN "clientes" | AGGREGATE count()"#)?;
        compare_count(
            "V5_clients_distinct_rfc",
            &golden.verify_queries.v5_clients_distinct_rfc,
            n,
            &mut diffs,
        );

        // V6 — configuracion catalogue. Counts per _type + total.
        let n_emp = self.aggregate_count(
            r#"SCAN "configuracion" WHERE _type = "Empresa" | AGGREGATE count()"#,
        )?;
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
        let n_prod = self.aggregate_count(
            r#"SCAN "configuracion" WHERE _type = "Producto" | AGGREGATE count()"#,
        )?;
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
        let n_total = self.aggregate_count(r#"SCAN "configuracion" | AGGREGATE count()"#)?;
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

        info!(target: "xyzdb", "Phase 1.5 done: {} diffs", diffs.len());
        Ok(GoldenVerifyResults {
            overall_match: diffs.is_empty(),
            diffs,
        })
    }

    /// Phase 5b — append-invariant content gate. See `content_gate.rs`-style
    /// rationale inline: we re-derive the loaded anchored entities from the
    /// seed (ground truth) and fold a per-record content hash, then read the
    /// same entities back from the engine (clientes by its `rfc` anchor;
    /// creditos `Credit` rows via the gravity-bounded `WHERE rfc` fast path,
    /// filtered to `_type = "Credit"`) and fold the observed hash. A mismatch
    /// means a loaded immutable row's content changed under the concurrent
    /// workload — the signal the post-concurrent cardinality verify cannot
    /// give because it drifts with Phase 3 appends.
    ///
    /// Append-invariant by construction: the expectation is keyed by the
    /// seed-regenerated keys, and Phase 3 appends only `Payment` rows with
    /// brand-new keys — never looked up, and excluded by the `_type` filter.
    fn verify_content_gate(&self, dataset: &Dataset) -> Result<ContentGateResults> {
        info!(target: "xyzdb",
              "Phase 5b: content gate (clientes by anchor + creditos Credit rows)");

        // ── Stride sample: at scale 1.0 the anchored set is ~tens of millions
        //    of entities, and reading every one back is a per-client wire round
        //    trip — ~13 h. The gate's job is to catch SYSTEMATIC content drift
        //    of loaded immutable rows under the concurrent workload; a
        //    deterministic stride sample (~CONTENT_GATE_MAX_CLIENTS clients and
        //    their Credit rows) detects that at a fraction of the cost. stride=1
        //    (full coverage) when the client count is at or below the cap, so
        //    small scales are unchanged. Expected + observed fold the SAME
        //    sampled set, so the hashes stay comparable. ──
        const CONTENT_GATE_MAX_CLIENTS: usize = 20_000;
        let total_clients = dataset.clients().count();
        let stride = (total_clients / CONTENT_GATE_MAX_CLIENTS).max(1);
        let sampled: Vec<_> = dataset.clients().step_by(stride).collect();
        let sampled_rfcs: std::collections::HashSet<String> =
            sampled.iter().map(|c| c.rfc.clone()).collect();

        // ── Expected folds over the sampled set: re-derived from the seed,
        //    order-independent (wrapping_add) so engine read-back order never
        //    matters. ──
        let (mut exp_clientes, mut exp_creditos): (u64, u64) = (0, 0);
        let (mut n_clientes, mut n_creditos): (u64, u64) = (0, 0);
        for c in &sampled {
            let fields = [
                ("curp", norm_value(FieldKind::Str, &c.curp)),
                ("nombre", norm_value(FieldKind::Str, &c.nombre)),
                (
                    "scoring_bureau",
                    norm_value(FieldKind::Int, &c.scoring.bureau.to_string()),
                ),
                ("scoring_risk", norm_value(FieldKind::Str, &c.scoring.risk)),
                (
                    "limite_credito",
                    norm_value(FieldKind::F64x2, &c.scoring.limite_credito.to_string()),
                ),
                (
                    "entidad",
                    norm_value(FieldKind::Str, &c.datos_ubicacion.entidad_federativa),
                ),
                (
                    "municipio",
                    norm_value(FieldKind::Str, &c.datos_ubicacion.municipio),
                ),
                (
                    "regimen",
                    norm_value(FieldKind::Str, &c.caracteristicas_fiscales.regimen),
                ),
                (
                    "actividad",
                    norm_value(FieldKind::Str, &c.caracteristicas_fiscales.actividad),
                ),
            ];
            exp_clientes =
                exp_clientes.wrapping_add(content_record_hash("Client", &c.rfc, &fields));
            n_clientes += 1;
        }
        for c in dataset.credits() {
            if !sampled_rfcs.contains(c.rfc.as_str()) {
                continue;
            }
            let fields = [
                ("empresa_id", norm_value(FieldKind::Str, &c.empresa_id)),
                ("producto_id", norm_value(FieldKind::Str, &c.producto_id)),
                ("monto", norm_value(FieldKind::F64x2, &c.monto.to_string())),
                ("status", norm_value(FieldKind::Str, c.status.as_str())),
                (
                    "dias_atraso",
                    norm_value(FieldKind::Int, &c.dias_atraso.to_string()),
                ),
            ];
            exp_creditos =
                exp_creditos.wrapping_add(content_record_hash("Credit", &c.credit_id, &fields));
            n_creditos += 1;
        }

        // ── Observed folds: read each anchored entity back per rfc. One
        //    anchor lookup + one gravity-bounded lookup per client. ──
        let (mut obs_clientes, mut obs_creditos): (u64, u64) = (0, 0);
        for c in &sampled {
            let rfc = escape(&c.rfc);
            let resp = self.execute(&format!(r#"FIND "clientes" WHERE rfc = "{rfc}""#))?;
            for rec in parse_box_records(&resp) {
                if rec.get("_type").map(String::as_str) == Some("Client") {
                    if let Some(fields) = canonical_fields(&rec, CLIENTE_FIELDS) {
                        obs_clientes = obs_clientes
                            .wrapping_add(content_record_hash("Client", &c.rfc, &fields));
                    }
                }
            }
            // creditos has no unique anchor: rfc is the gravity/placement
            // key. Use the gravity-pruned SCAN (the bench's own creditos
            // access shape) and filter `_type = "Credit"` CLIENT-SIDE.
            //
            // Deliberately NOT a server-side `AND _type = "Credit"`: on the
            // gravity fast path the compound predicate under-returns
            // (`WHERE rfc=X AND _type=Credit` yields 1 of 2 stored Credit
            // rows, while `WHERE rfc=X` then client-filter yields both —
            // verified at scale 0.0005, an engine query-layer finding logged
            // separately). Client-side filtering keeps the gate sound and
            // append-invariant (Phase 3 appends are Payments, dropped here).
            // The LIMIT is the engine's hard SCAN maximum (10000), far above
            // any per-rfc cluster; truncation would surface as a mismatch,
            // never a silent pass.
            let resp = self.execute(&format!(
                r#"SCAN "creditos" WHERE rfc = "{rfc}" LIMIT 10000"#
            ))?;
            for rec in parse_box_records(&resp) {
                if rec.get("_type").map(String::as_str) != Some("Credit") {
                    continue;
                }
                let Some(cid) = rec.get("credit_id").cloned() else {
                    continue;
                };
                if let Some(fields) = canonical_fields(&rec, CREDIT_FIELDS) {
                    obs_creditos =
                        obs_creditos.wrapping_add(content_record_hash("Credit", &cid, &fields));
                }
            }
        }

        let clientes_match = exp_clientes == obs_clientes;
        let creditos_match = exp_creditos == obs_creditos;
        if !clientes_match {
            warn!(target: "xyzdb",
                  "content gate clientes MISMATCH: expected={exp_clientes:016x} observed={obs_clientes:016x}");
        }
        if !creditos_match {
            warn!(target: "xyzdb",
                  "content gate creditos MISMATCH: expected={exp_creditos:016x} observed={obs_creditos:016x}");
        }
        info!(target: "xyzdb", "Phase 5b done: clientes_match={clientes_match} creditos_match={creditos_match}");

        Ok(ContentGateResults {
            overall_match: clientes_match && creditos_match,
            ran: true,
            lobes: vec![
                ContentGateLobe {
                    lobe: "clientes".to_string(),
                    matched: clientes_match,
                    records_hashed: n_clientes,
                    expected_hash: format!("{exp_clientes:016x}"),
                    observed_hash: format!("{obs_clientes:016x}"),
                },
                ContentGateLobe {
                    lobe: "creditos".to_string(),
                    matched: creditos_match,
                    records_hashed: n_creditos,
                    expected_hash: format!("{exp_creditos:016x}"),
                    observed_hash: format!("{obs_creditos:016x}"),
                },
            ],
            scope: format!(
                "stride sample (1/{stride}: {n_clientes} clientes of {total_clients} + their \
                 Credit rows) of loaded anchored entities: clientes (rfc anchor) + creditos \
                 Credit rows (credit_id). Detects SYSTEMATIC content drift, not a per-record \
                 audit (stride=1 below the cap = full coverage). Append-invariant: Phase 3 \
                 appends only Payment rows with new keys, excluded by the seed-regenerated key \
                 set and the _type=Credit filter. KNOWN GAP: non-anchored child rows \
                 (Installment/Payment/Collection) are not content-hashed — see the follow-up \
                 sequential-scan gate."
            ),
        })
    }
}

// ── verify_golden helpers ────────────────────────────────────────────

impl XyzdbDriver {
    /// Issue a single-group `AGGREGATE count(), sum(<field>)` and parse
    /// V1 text response. Wire format (server response.rs `QueryResult::
    /// Aggregation`): `key: value\n` per agg key. The `sum(<field>)` key
    /// comes from the Primary AggAccumulator path; the alternate
    /// `<field>:Sum` form (PreComputed path) is also accepted because
    /// integration test 3344-3346 documents both naming conventions.
    fn aggregate_count_sum(&self, query: &str, sum_field: &str) -> Result<(u64, f64)> {
        let resp = self.execute(query)?;
        let mut n: Option<u64> = None;
        let mut s: Option<f64> = None;
        let sum_alt = format!("{sum_field}:Sum");
        let sum_primary = format!("sum({sum_field})");
        for line in resp.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("count: ") {
                n = rest.trim().parse::<u64>().ok();
                continue;
            }
            if let Some(rest) = line.strip_prefix(&format!("{sum_primary}: ")) {
                s = rest.trim().parse::<f64>().ok();
                continue;
            }
            if let Some(rest) = line.strip_prefix(&format!("{sum_alt}: ")) {
                s = rest.trim().parse::<f64>().ok();
                continue;
            }
        }
        match (n, s) {
            (Some(n), Some(s)) => Ok((n, s)),
            _ => bail!(
                "could not parse count + sum from response (query={query}): {}",
                &resp[..resp.len().min(200)]
            ),
        }
    }

    /// Issue a single-key `AGGREGATE count()` and parse `count: N`.
    fn aggregate_count(&self, query: &str) -> Result<u64> {
        let resp = self.execute(query)?;
        for line in resp.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("count: ") {
                if let Ok(v) = rest.trim().parse::<u64>() {
                    return Ok(v);
                }
            }
            if let Ok(v) = line.parse::<u64>() {
                return Ok(v);
            }
        }
        bail!(
            "could not parse count from response (query={query}): {}",
            &resp[..resp.len().min(200)]
        );
    }

    /// Per-lobe V4 counts: one COUNT per (lobe, _type) pair declared in
    /// the golden's `BTreeMap`. The `_total` key short-circuits to a
    /// type-less SCAN over the whole lobe.
    fn verify_v4_lobe_counts(
        &self,
        lobe: &str,
        expected: &std::collections::BTreeMap<String, u64>,
        diffs: &mut Vec<GoldenDiff>,
    ) -> Result<()> {
        for (typ, &exp) in expected.iter() {
            let q = if typ == "_total" {
                format!(r#"SCAN "{lobe}" | AGGREGATE count()"#)
            } else {
                format!(r#"SCAN "{lobe}" WHERE _type = "{typ}" | AGGREGATE count()"#)
            };
            let observed = self.aggregate_count(&q).unwrap_or(0);
            if observed != exp {
                let exp_f = exp as f64;
                diffs.push(GoldenDiff {
                    query: format!("V4_lobe_type:{lobe}:{typ}"),
                    field: "n".to_string(),
                    expected: exp_f,
                    observed: observed as f64,
                    relative_delta: (observed as f64 - exp_f).abs() / exp_f.max(1.0),
                });
            }
        }
        Ok(())
    }
}

// ── bulk emit helpers ────────────────────────────────────────────────

impl XyzdbDriver {
    fn batch_put(&self, lobe: &str, records: Vec<String>) -> Result<u64> {
        if records.is_empty() {
            return Ok(0);
        }
        let body = records.join(",\n  ");
        // Note: cannot use raw string r#"..."# here because we want actual
        // newlines + the "\n" literal in a raw string is two chars the
        // parser rejects.
        let stmt = format!("PUT BATCH IN \"{}\" [\n  {}\n]", lobe, body);
        self.execute(&stmt)?;
        Ok(records.len() as u64)
    }

    fn flush_batch(&self, lobe: &str, buf: &mut Vec<String>) -> Result<u64> {
        if buf.is_empty() {
            return Ok(0);
        }
        let n = buf.len() as u64;
        let recs = std::mem::take(buf);
        self.batch_put(lobe, recs)?;
        Ok(n)
    }

    fn bulk_emit_empresas(&self, ds: &Dataset) -> Result<u64> {
        let mut buf = Vec::with_capacity(BULK_BATCH);
        let mut total = 0;
        for e in ds.empresas() {
            buf.push(format!(
                r#"{{*empresa_id: "{}", _type: "Empresa", nombre: "{}", region: "{}", activa: {}}}"#,
                e.empresa_id,
                escape(&e.nombre),
                escape(&e.region),
                e.activa
            ));
            if buf.len() >= BULK_BATCH {
                total += self.flush_batch("configuracion", &mut buf)?;
            }
        }
        total += self.flush_batch("configuracion", &mut buf)?;
        debug!(target: "xyzdb", "configuracion: emitted {} empresas", total);
        Ok(total)
    }

    fn bulk_emit_productos(&self, ds: &Dataset) -> Result<u64> {
        let mut buf = Vec::with_capacity(BULK_BATCH);
        let mut total = 0;
        for p in ds.productos() {
            buf.push(format!(
                r#"{{*empresa_id: "{}", _type: "Producto", producto_id: "{}", nombre: "{}", tasa_interes: {:.4}, plazo_meses: {}}}"#,
                p.empresa_id,
                p.producto_id,
                escape(&p.nombre),
                p.tasa_interes,
                p.plazo_meses
            ));
            if buf.len() >= BULK_BATCH {
                total += self.flush_batch("configuracion", &mut buf)?;
            }
        }
        total += self.flush_batch("configuracion", &mut buf)?;
        debug!(target: "xyzdb", "configuracion: emitted {} productos", total);
        Ok(total)
    }

    fn bulk_emit_clients(&self, ds: &Dataset) -> Result<u64> {
        let mut buf = Vec::with_capacity(BULK_BATCH);
        let mut total = 0;
        for c in ds.clients() {
            buf.push(format!(
                r#"{{*rfc: "{}", _type: "Client", curp: "{}", nombre: "{}", scoring_bureau: {}, scoring_risk: "{}", limite_credito: {:.2}, entidad: "{}", municipio: "{}", regimen: "{}", actividad: "{}"}}"#,
                c.rfc,
                c.curp,
                escape(&c.nombre),
                c.scoring.bureau,
                c.scoring.risk,
                c.scoring.limite_credito,
                escape(&c.datos_ubicacion.entidad_federativa),
                escape(&c.datos_ubicacion.municipio),
                escape(&c.caracteristicas_fiscales.regimen),
                escape(&c.caracteristicas_fiscales.actividad),
            ));
            if buf.len() >= BULK_BATCH {
                total += self.flush_batch("clientes", &mut buf)?;
            }
        }
        total += self.flush_batch("clientes", &mut buf)?;
        info!(target: "xyzdb", "clientes: emitted {} clients", total);
        Ok(total)
    }

    fn bulk_emit_credits(&self, ds: &Dataset) -> Result<u64> {
        let mut buf = Vec::with_capacity(BULK_BATCH);
        let mut total = 0;
        for c in ds.credits() {
            buf.push(format!(
                r#"{{*rfc: "{}", _type: "Credit", credit_id: "{}", empresa_id: "{}", producto_id: "{}", monto: {:.2}, status: "{}", dias_atraso: {}, fecha_creacion_ms: {}, fecha_vencimiento_ms: {}}}"#,
                c.rfc,
                c.credit_id,
                c.empresa_id,
                c.producto_id,
                c.monto,
                c.status.as_str(),
                c.dias_atraso,
                c.fecha_creacion.timestamp_millis(),
                naive_date_to_ms(c.fecha_vencimiento),
            ));
            if buf.len() >= BULK_BATCH {
                total += self.flush_batch("creditos", &mut buf)?;
            }
        }
        total += self.flush_batch("creditos", &mut buf)?;
        info!(target: "xyzdb", "creditos: emitted {} Credit records", total);
        Ok(total)
    }

    fn bulk_emit_installments(&self, ds: &Dataset) -> Result<u64> {
        let mut buf = Vec::with_capacity(BULK_BATCH);
        let mut total = 0;
        for i in ds.installments() {
            buf.push(format!(
                r#"{{*rfc: "{}", _type: "Installment", installment_id: "{}", credit_id: "{}", empresa_id: "{}", numero: {}, monto_total: {:.2}, status: "{}", dias_atraso: {}, fecha_vencimiento_ms: {}}}"#,
                i.rfc,
                i.installment_id,
                i.credit_id,
                i.empresa_id,
                i.numero,
                i.monto_total,
                i.status.as_str(),
                i.dias_atraso,
                naive_date_to_ms(i.fecha_vencimiento),
            ));
            if buf.len() >= BULK_BATCH {
                total += self.flush_batch("creditos", &mut buf)?;
            }
        }
        total += self.flush_batch("creditos", &mut buf)?;
        info!(target: "xyzdb", "creditos: emitted {} Installment records", total);
        Ok(total)
    }

    fn bulk_emit_payments(
        &self,
        ds: &Dataset,
        credit_to_empresa: &HashMap<String, String>,
    ) -> Result<u64> {
        let mut buf = Vec::with_capacity(BULK_BATCH);
        let mut total = 0;
        for p in ds.payments() {
            let empresa_id = credit_to_empresa
                .get(&p.credit_id)
                .map(String::as_str)
                .unwrap_or("");
            buf.push(format!(
                r#"{{*rfc: "{}", _type: "Payment", payment_id: "{}", credit_id: "{}", empresa_id: "{}", monto: {:.2}, fecha_pago_ms: {}, metodo: "{}"}}"#,
                p.rfc,
                p.payment_id,
                p.credit_id,
                empresa_id,
                p.monto,
                p.fecha_pago.timestamp_millis(),
                p.metodo,
            ));
            if buf.len() >= BULK_BATCH {
                total += self.flush_batch("creditos", &mut buf)?;
            }
        }
        total += self.flush_batch("creditos", &mut buf)?;
        info!(target: "xyzdb", "creditos: emitted {} Payment records", total);
        Ok(total)
    }

    fn bulk_emit_collections(
        &self,
        ds: &Dataset,
        credit_to_empresa: &HashMap<String, String>,
    ) -> Result<u64> {
        let mut buf = Vec::with_capacity(BULK_BATCH);
        let mut total = 0;
        for c in ds.collections() {
            let empresa_id = credit_to_empresa
                .get(&c.credit_id)
                .map(String::as_str)
                .unwrap_or("");
            buf.push(format!(
                r#"{{*rfc: "{}", _type: "Collection", collection_id: "{}", credit_id: "{}", empresa_id: "{}", monto_pendiente: {:.2}, status: "{}", fecha_inicio_ms: {}}}"#,
                c.rfc,
                c.collection_id,
                c.credit_id,
                empresa_id,
                c.monto_pendiente,
                c.status.as_str(),
                c.fecha_inicio.timestamp_millis(),
            ));
            if buf.len() >= BULK_BATCH {
                total += self.flush_batch("creditos", &mut buf)?;
            }
        }
        total += self.flush_batch("creditos", &mut buf)?;
        info!(target: "xyzdb", "creditos: emitted {} Collection records", total);
        Ok(total)
    }

    fn bulk_emit_collection_actions(
        &self,
        ds: &Dataset,
        credit_to_empresa: &HashMap<String, String>,
    ) -> Result<u64> {
        let mut buf = Vec::with_capacity(BULK_BATCH);
        let mut total = 0;
        for a in ds.collection_actions() {
            let empresa_id = credit_to_empresa
                .get(&a.credit_id)
                .map(String::as_str)
                .unwrap_or("");
            buf.push(format!(
                r#"{{*rfc: "{}", _type: "CollectionAction", action_id: "{}", collection_id: "{}", credit_id: "{}", empresa_id: "{}", tipo: "{}", fecha_ms: {}, resultado: "{}"}}"#,
                a.rfc,
                a.action_id,
                a.collection_id,
                a.credit_id,
                empresa_id,
                a.tipo,
                a.fecha.timestamp_millis(),
                a.resultado,
            ));
            if buf.len() >= BULK_BATCH {
                total += self.flush_batch("creditos", &mut buf)?;
            }
        }
        total += self.flush_batch("creditos", &mut buf)?;
        info!(target: "xyzdb", "creditos: emitted {} CollectionAction records", total);
        Ok(total)
    }

    fn bulk_emit_applications(&self, ds: &Dataset) -> Result<u64> {
        let mut buf = Vec::with_capacity(BULK_BATCH);
        let mut total = 0;
        for a in ds.credit_applications() {
            buf.push(format!(
                r#"{{*rfc: "{}", _type: "CreditApplication", application_id: "{}", empresa_id: "{}", producto_id: "{}", monto_solicitado: {:.2}, status: "{}"}}"#,
                a.rfc,
                a.application_id,
                a.empresa_id,
                a.producto_id,
                a.monto_solicitado,
                a.status.as_str(),
            ));
            if buf.len() >= BULK_BATCH {
                total += self.flush_batch("operaciones", &mut buf)?;
            }
        }
        total += self.flush_batch("operaciones", &mut buf)?;
        info!(target: "xyzdb", "operaciones: emitted {} CreditApplication records", total);
        Ok(total)
    }

    fn bulk_emit_audit_log(&self, ds: &Dataset) -> Result<u64> {
        let mut buf = Vec::with_capacity(BULK_BATCH);
        let mut total = 0;
        for a in ds.audit_log() {
            buf.push(format!(
                r#"{{*rfc: "{}", _type: "AuditLog", audit_id: {}, action_type: "{}", fecha_ms: {}}}"#,
                a.rfc,
                a.audit_id,
                a.action_type,
                a.fecha.timestamp_millis(),
            ));
            if buf.len() >= BULK_BATCH {
                total += self.flush_batch("operaciones", &mut buf)?;
            }
        }
        total += self.flush_batch("operaciones", &mut buf)?;
        info!(target: "xyzdb", "operaciones: emitted {} AuditLog records", total);
        Ok(total)
    }

    fn bulk_emit_notifications(&self, ds: &Dataset) -> Result<u64> {
        let mut buf = Vec::with_capacity(BULK_BATCH);
        let mut total = 0;
        for n in ds.notifications() {
            buf.push(format!(
                r#"{{*rfc: "{}", _type: "Notification", notification_id: {}, canal: "{}", fecha_ms: {}}}"#,
                n.rfc,
                n.notification_id,
                n.canal,
                n.fecha.timestamp_millis(),
            ));
            if buf.len() >= BULK_BATCH {
                total += self.flush_batch("operaciones", &mut buf)?;
            }
        }
        total += self.flush_batch("operaciones", &mut buf)?;
        info!(target: "xyzdb", "operaciones: emitted {} Notification records", total);
        Ok(total)
    }

    fn bulk_emit_bi(&self, ds: &Dataset) -> Result<u64> {
        let mut buf = Vec::with_capacity(BULK_BATCH);
        let mut total = 0;
        for b in ds.bi_snapshots() {
            buf.push(format!(
                r#"{{*empresa_id: "{}", _type: "BiSnapshot", snapshot_id: {}, fecha: "{}"}}"#,
                b.empresa_id,
                b.snapshot_id,
                b.fecha.format("%Y-%m-%d"),
            ));
            if buf.len() >= BULK_BATCH {
                total += self.flush_batch("bi", &mut buf)?;
            }
        }
        total += self.flush_batch("bi", &mut buf)?;
        info!(target: "xyzdb", "bi: emitted {} BiSnapshot records", total);
        Ok(total)
    }
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Build the `credit_id → empresa_id` map used to denormalise empresa_id onto
/// Payment / Collection / CollectionAction at load time (the generator model
/// omits it on these entities — model.rs:174-203). The Dataset is a
/// deterministic seeded stream, so a fresh pass over `credits()` reproduces the
/// exact credit set the child entities reference.
fn build_credit_empresa_map(ds: &Dataset) -> HashMap<String, String> {
    let mut map = HashMap::with_capacity(300_000);
    for c in ds.credits() {
        map.insert(c.credit_id, c.empresa_id);
    }
    map
}

/// `rfc -> (credit_id, empresa_id)` for one real credit per client (the first
/// seen). Q7's synthetic payments borrow this pair so they attach to a real
/// credit (PG's Q3 join resolves them) and a real empresa (Q8's per-empresa
/// group), never a phantom null-empresa. Symmetric across engines.
fn build_rfc_first_credit_map(ds: &Dataset) -> HashMap<String, (String, String)> {
    let mut map = HashMap::with_capacity(200_000);
    for c in ds.credits() {
        map.entry(c.rfc).or_insert((c.credit_id, c.empresa_id));
    }
    map
}

/// Epoch-millis for a `NaiveDate`, taken at 00:00:00 UTC. Keeps date fields in
/// the same `_ms` representation the engine already uses for `fecha_pago_ms` so
/// WHERE predicates compare integers, not formatted strings.
fn naive_date_to_ms(d: chrono::NaiveDate) -> i64 {
    d.and_hms_opt(0, 0, 0)
        .map(|dt| dt.and_utc().timestamp_millis())
        .unwrap_or(0)
}

// ── content gate (Phase 5b) helpers ───────────────────────────────────

/// How a content-hashed field is canonicalised so the seed-regenerated
/// expectation and the engine read-back agree regardless of wire
/// formatting. The hazard this closes: the loader writes `monto` as the
/// literal `{:.2}` (e.g. `"1234.50"`), the parser stores it as `f64`
/// `1234.5`, and the V1 text dump renders it back via `f64` Display as
/// `"1234.5"` — so a naive string compare would spuriously fail. Parsing
/// both sides to the same shape removes the mismatch.
#[derive(Clone, Copy)]
enum FieldKind {
    Str,
    Int,
    F64x2,
}

/// Canonicalise one field value. Unparseable numerics fall back to the
/// trimmed raw string (a corruption that breaks the type still diverges
/// from the expectation, so the gate still fires).
fn norm_value(kind: FieldKind, raw: &str) -> String {
    let t = raw.trim();
    match kind {
        FieldKind::Str => t.to_string(),
        FieldKind::Int => t
            .parse::<i64>()
            .map(|v| v.to_string())
            .unwrap_or_else(|_| t.to_string()),
        FieldKind::F64x2 => t
            .parse::<f64>()
            .map(|v| format!("{v:.2}"))
            .unwrap_or_else(|_| t.to_string()),
    }
}

/// The content-hashed fields per anchored entity, with the type used to
/// canonicalise each. Domain-specific by design: this lives in the bench
/// driver, which models a specific workload (the engine itself is
/// domain-agnostic). `_type`, the anchor/key, and engine internals (LID)
/// are deliberately excluded — the key is folded separately.
const CLIENTE_FIELDS: &[(&str, FieldKind)] = &[
    ("curp", FieldKind::Str),
    ("nombre", FieldKind::Str),
    ("scoring_bureau", FieldKind::Int),
    ("scoring_risk", FieldKind::Str),
    ("limite_credito", FieldKind::F64x2),
    ("entidad", FieldKind::Str),
    ("municipio", FieldKind::Str),
    ("regimen", FieldKind::Str),
    ("actividad", FieldKind::Str),
];
const CREDIT_FIELDS: &[(&str, FieldKind)] = &[
    ("empresa_id", FieldKind::Str),
    ("producto_id", FieldKind::Str),
    ("monto", FieldKind::F64x2),
    ("status", FieldKind::Str),
    ("dias_atraso", FieldKind::Int),
];

/// Deterministic per-record content hash. The key folds in first so two
/// records that differ only by key never collide; fields are sorted by
/// name inside so caller ordering is irrelevant. `DefaultHasher::new()`
/// uses fixed SipHash keys, so this is stable within and across runs —
/// expected and observed folds are computed in the same binary regardless.
fn content_record_hash(kind_tag: &str, key: &str, fields: &[(&str, String)]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut sorted: Vec<&(&str, String)> = fields.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    let mut h = std::collections::hash_map::DefaultHasher::new();
    kind_tag.hash(&mut h);
    key.hash(&mut h);
    for (name, val) in sorted {
        name.hash(&mut h);
        val.hash(&mut h);
    }
    h.finish()
}

/// Pull the canonical (name, value) list for the given field spec out of a
/// parsed record. Returns `None` if any required field is absent — a
/// missing field then drops the record from the observed fold, diverging
/// from the expectation (so the gate fires rather than silently passing).
fn canonical_fields(
    rec: &std::collections::BTreeMap<String, String>,
    spec: &[(&'static str, FieldKind)],
) -> Option<Vec<(&'static str, String)>> {
    let mut out = Vec::with_capacity(spec.len());
    for (name, kind) in spec {
        let raw = rec.get(*name)?;
        out.push((*name, norm_value(*kind, raw)));
    }
    Some(out)
}

/// Strip exactly one pair of surrounding double quotes, the way
/// `Value`'s `Display` wraps `Text` (`Value::Text(v) => "\"{v}\""`).
/// Numerics/bools/timestamps render unquoted, so they pass through
/// untouched. Internal quotes are preserved (Display does not re-escape
/// them), so a single outer-pair strip is exactly the inverse.
fn unquote(v: &str) -> &str {
    if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        &v[1..v.len() - 1]
    } else {
        v
    }
}

/// Parse the V1 text record dump (`format_record` box drawing) into a list
/// of field maps. Records are delimited by `┌`/`└` border lines; field
/// lines are `│ name: value<pad>│`. The `LID`/`Lobe` header lines are
/// skipped; `_type` is kept (the gate filters on it). String values arrive
/// quoted (see `unquote`) and are unwrapped here.
fn parse_box_records(resp: &str) -> Vec<std::collections::BTreeMap<String, String>> {
    let mut out = Vec::new();
    let mut cur: Option<std::collections::BTreeMap<String, String>> = None;
    for line in resp.lines() {
        let t = line.trim();
        if t.starts_with('┌') {
            if let Some(m) = cur.take() {
                out.push(m);
            }
            cur = Some(std::collections::BTreeMap::new());
            continue;
        }
        if t.starts_with('└') {
            if let Some(m) = cur.take() {
                out.push(m);
            }
            continue;
        }
        if t.starts_with('│') {
            let inner = t.trim_start_matches('│').trim_end_matches('│').trim();
            if inner.starts_with("LID:") || inner.starts_with("Lobe:") {
                continue;
            }
            if let Some((k, v)) = inner.split_once(": ") {
                if let Some(m) = cur.as_mut() {
                    m.insert(k.trim().to_string(), unquote(v.trim_end()).to_string());
                }
            }
            continue;
        }
    }
    if let Some(m) = cur.take() {
        out.push(m);
    }
    out
}

// ── Concurrent runner ─────────────────────────────────────────────────

fn run_concurrent_workload(
    driver: &XyzdbDriver,
    profile: &ConcurrentProfile,
    rfc_pool: &[String],
) -> Result<ConcurrentResults> {
    let total_threads = profile.readers + profile.writers;
    info!(target: "xyzdb",
          "Phase 3: concurrent {} mixed-mode threads (errática-driven), duration={:?}, hot_ratio={}, lambda_idle/busy={}/{}",
          total_threads, profile.duration,
          profile.erratica.hot_ratio,
          profile.erratica.lambda_idle, profile.erratica.lambda_busy);

    if rfc_pool.is_empty() {
        bail!("rfc_pool is empty; need at least one RFC to drive errática threads");
    }

    let stop_at = Instant::now() + profile.duration;
    let reads = Arc::new(AtomicU64::new(0));
    let writes = Arc::new(AtomicU64::new(0));

    // Per-query latency samples (ms). Each thread pushes onto the shared
    // vector behind a Mutex; final stats computed at the end.
    type Samples = Vec<(BusinessQuery, f64, u64)>;
    let samples: Arc<Mutex<Samples>> = Arc::new(Mutex::new(Vec::new()));

    let host = driver.host.clone();
    let port = driver.port;
    let pool = Arc::new(rfc_pool.to_vec());

    let mut handles = Vec::new();

    // Phase 3 v0.3.3: mixed-mode threads driven by `ErraticaPicker`
    // (design §6). Each thread instantiates its own MMPP sampler +
    // session lifecycle + working-set drift + query mixer. State-
    // dependent R/W mix per §6.4: Idle 95R/5W, Busy 70R/30W. RFC pick
    // hot/cold biased per §6.3 (95 % hot, 5 % cold). The legacy v0.2.5
    // separate reader/writer thread split is collapsed: each thread
    // issues both reads and writes per the picker's state-dependent
    // mix. Thread count = readers + writers from `ErraticaConfig` (default 9).
    // Q8's composite read must carry the same cutoff literal baked into the
    // ghost at schema setup (see dispatch_thread_local / run_q8_monthly_close).
    let cutoff_ms = driver.cutoff_ms();
    // rfc→(credit_id, empresa_id) for the pool's clients, so Phase-3 Q7 writes
    // borrow a real credit/empresa (same invariant as the cold path: no
    // null-empresa group, symmetric with PG/Mongo). Filtered to the pool to stay
    // small; shared read-only across threads.
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
    // v0.5 — shared run_start so all threads anchor schedule.resolve()
    // to the same wall-clock origin. Per-thread persona + schedule
    // come from the profile (Option<...>, None → legacy uniform).
    let run_start = Instant::now();
    let total_duration = profile.duration;
    let has_personas = profile.persona_assignment.is_some();
    for tid in 0..total_threads {
        let persona_for_tid = profile
            .persona_assignment
            .as_ref()
            .and_then(|pa| pa.persona_for(tid));
        // Idle padding slot when personas configured but this tid was
        // not assigned a persona: skip spawning entirely so the slot
        // contributes no event traffic (per spec §6.2 idle thread =
        // silent).
        if has_personas && persona_for_tid.is_none() {
            continue;
        }
        let host = host.clone();
        let pool = (*pool).clone();
        let samples = samples.clone();
        let reads = reads.clone();
        let writes = writes.clone();
        let cfg = profile.erratica.clone();
        let schedule = profile.schedule.clone();
        let rfc_map = rfc_map.clone();
        let h = std::thread::spawn(move || -> Result<()> {
            let stream = TcpStream::connect(format!("{host}:{port}"))?;
            stream.set_nodelay(true)?;
            let mut conn = stream;
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
                let (sleep_dur, query_pair): (
                    std::time::Duration,
                    Option<(BusinessQuery, String)>,
                ) = match event {
                    native_generator::erratica::ErraticaEvent::Sleep(d) => (d, None),
                    native_generator::erratica::ErraticaEvent::SleepThenQuery {
                        sleep,
                        query,
                        rfc,
                    } => (sleep, Some((query, rfc))),
                    native_generator::erratica::ErraticaEvent::Query { query, rfc } => {
                        (std::time::Duration::ZERO, Some((query, rfc)))
                    }
                };
                if sleep_dur > std::time::Duration::ZERO {
                    let remaining = stop_at.saturating_duration_since(now);
                    let cap = sleep_dur.min(remaining);
                    if cap > std::time::Duration::ZERO {
                        std::thread::sleep(cap);
                    }
                    if Instant::now() >= stop_at {
                        break;
                    }
                }
                if let Some((query, rfc)) = query_pair {
                    let t0 = Instant::now();
                    match dispatch_thread_local(&mut conn, query, &rfc, cutoff_ms, &rfc_map) {
                        Ok((records, was_read)) => {
                            let lat_ms = t0.elapsed().as_secs_f64() * 1000.0;
                            samples.lock().unwrap().push((query, lat_ms, records));
                            if was_read {
                                reads.fetch_add(1, Ordering::Relaxed);
                            } else {
                                writes.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        Err(_) => {
                            if let Ok(c2) = TcpStream::connect(format!("{host}:{port}")) {
                                conn = c2;
                                let _ = conn.set_nodelay(true);
                            }
                        }
                    }
                }
            }
            Ok(())
        });
        handles.push(h);
    }

    for h in handles {
        let _ = h.join();
    }

    let dur_secs = profile.duration.as_secs_f64();
    let reads_total = reads.load(Ordering::Relaxed);
    let writes_total = writes.load(Ordering::Relaxed);

    // Compute per-query stats
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
        throughput_cv: 0.0, // populated by post-processing if 30 s windows are captured
        refresh_count: 0,
        refresh_total_ms: 0,
    })
}

/// Phase 3 v0.3.3 thread-local dispatch (errática-driven mixed-mode threads).
/// Returns `(records_returned, was_read)`. Multi-statement Q9 dispatches
/// sequentially and sums records (same 3-call gravity path as the cold-phase
/// `queries::run_q9_customer_context`).
fn dispatch_thread_local(
    conn: &mut TcpStream,
    query: BusinessQuery,
    rfc: &str,
    cutoff_ms: i64,
    rfc_map: &HashMap<String, (String, String)>,
) -> Result<(u64, bool)> {
    use BusinessQuery::*;
    match query {
        Q1Point | Q2Aggregate | Q3FullHistory | Q4TopExposure | Q5OverdueByEmpresa
        | Q6RecentPayments => {
            let xytalk = queries::query_text(query, rfc, 50);
            let resp = execute_on(conn, &xytalk)?;
            Ok((parse_record_count(&resp), true))
        }
        Q7BatchIngest => {
            // Borrow a real (credit_id, empresa_id) of the rfc (same invariant
            // as the cold path) so Q7 writes never contaminate Q8.
            let xytalk = queries::build_q7_put_batch(rfc, rfc_map.get(rfc).cloned());
            let _resp = execute_on(conn, &xytalk)?;
            // PUT BATCH ack has no "N record(s)" line — report the known batch
            // size (same as the cold path), matching PG/Mongo insert counts.
            Ok((queries::Q7_BATCH_SIZE as u64, false))
        }
        Q8MonthlyClose => {
            // One composite read via the router GROUP BY | AGGREGATE idiom,
            // routed to the `monthly_close_by_emp` composite ghost.
            let stmt = queries::monthly_close_query(cutoff_ms);
            let total = parse_record_count(&execute_on(conn, &stmt)?);
            Ok((total, true))
        }
        Q9CustomerContext => {
            // Customer 360 — 3-call sequence, identical to the cold-path
            // `queries::run_q9_customer_context`: FIND client + one SCAN per
            // lobe. Each SCAN walks the whole rfc gravity bucket, so one scan
            // per lobe returns every co-located _type; split client-side into
            // PG's six sections (cliente + credits ALL + payments 30 +
            // collections 10 + audit 50 + notifications 20). Gravity
            // co-location makes the two lobe scans hit warm pages
            // intrinsically — no driver-side warm primitive. (The old path
            // here re-scanned creditos twice, omitted collections, and used a
            // non-existent HOT_CACHE keyword; unified with the cold path.)
            let find = format!(r#"FIND "clientes" WHERE rfc = "{rfc}""#);
            let scan_creditos = format!(r#"SCAN "creditos" WHERE rfc = "{rfc}" LIMIT 5000"#);
            let scan_operaciones = format!(r#"SCAN "operaciones" WHERE rfc = "{rfc}" LIMIT 5000"#);
            let n_type = |recs: &[std::collections::BTreeMap<String, String>], t: &str| {
                recs.iter()
                    .filter(|r| r.get("_type").map(String::as_str) == Some(t))
                    .count()
            };
            let mut total: u64 = parse_record_count(&execute_on(conn, &find)?);
            let creditos = parse_box_records(&execute_on(conn, &scan_creditos)?);
            total += (n_type(&creditos, "Credit") // ALL credits (canonical 360)
                + n_type(&creditos, "Payment").min(30)
                + n_type(&creditos, "Collection").min(10)) as u64;
            let operaciones = parse_box_records(&execute_on(conn, &scan_operaciones)?);
            total += (n_type(&operaciones, "AuditLog").min(50)
                + n_type(&operaciones, "Notification").min(20)) as u64;
            Ok((total, true))
        }
    }
}

fn parse_record_count(resp: &str) -> u64 {
    // V1 text responses end with a "N record(s)" or "N row(s)" or
    // "N group(s)" line in many Q-shapes; otherwise count newlines.
    for line in resp.lines() {
        if let Some(n) = line
            .split_whitespace()
            .next()
            .and_then(|s| s.parse::<u64>().ok())
        {
            if line.contains("record") || line.contains("row") || line.contains("group") {
                return n;
            }
        }
    }
    resp.lines().filter(|l| !l.is_empty()).count() as u64
}

#[allow(dead_code)]
fn unused_duration_kept_for_signature(_d: Duration) {}

#[cfg(test)]
mod content_gate_tests {
    use super::*;

    /// Mirror the server's `format_record` box drawing so the parser is
    /// tested against the exact wire shape it must handle in production.
    fn box_record(lid: u64, lobe: &str, fields: &[(&str, &str)]) -> String {
        let border = "─".repeat(50);
        let mut s = format!("┌{border}┐\n");
        s.push_str(&format!("│ LID: {:<43}│\n", lid));
        s.push_str(&format!("│ Lobe: {:<42}│\n", lobe));
        for (k, v) in fields {
            let line = format!("{k}: {v}");
            s.push_str(&format!("│ {line:<48}│\n"));
        }
        s.push_str(&format!("└{border}┘"));
        s
    }

    #[test]
    fn parses_fields_unquotes_text_keeps_type_skips_headers() {
        // Mirror the real wire: `Value::Text` renders quoted ("Credit"),
        // numerics render bare (1234.5). The parser must unwrap text.
        let resp = format!(
            "{}\n{}",
            box_record(
                1,
                "creditos",
                &[
                    ("_type", "\"Credit\""),
                    ("rfc", "\"ABC\""),
                    ("credit_id", "\"CR1\""),
                    ("monto", "1234.5"),
                    ("status", "\"active\""),
                    ("dias_atraso", "0"),
                ]
            ),
            box_record(
                2,
                "creditos",
                &[
                    ("_type", "\"Payment\""),
                    ("rfc", "\"ABC\""),
                    ("payment_id", "\"PAY1\""),
                ]
            ),
        );
        let recs = parse_box_records(&resp);
        assert_eq!(recs.len(), 2, "two records parsed");
        // text unquoted:
        assert_eq!(recs[0].get("_type").map(String::as_str), Some("Credit"));
        assert_eq!(recs[0].get("credit_id").map(String::as_str), Some("CR1"));
        // numeric left bare:
        assert_eq!(recs[0].get("monto").map(String::as_str), Some("1234.5"));
        // LID / Lobe headers are not fields:
        assert!(!recs[0].contains_key("LID"));
        assert!(!recs[0].contains_key("Lobe"));
        assert_eq!(recs[1].get("_type").map(String::as_str), Some("Payment"));
    }

    #[test]
    fn unquote_strips_one_outer_pair_only() {
        assert_eq!(unquote("\"Client\""), "Client");
        assert_eq!(unquote("1234.5"), "1234.5"); // bare numeric untouched
        assert_eq!(unquote("\"\""), ""); // empty text
        assert_eq!(unquote("\"a\"b\""), "a\"b"); // internal quote preserved
    }

    #[test]
    fn float_and_int_normalisation_matches_across_wire_shapes() {
        // The loader writes `{:.2}` but the engine renders f64 back via
        // Display, so "1234.50" and "1234.5" must canonicalise equal.
        assert_eq!(norm_value(FieldKind::F64x2, "1234.5"), "1234.50");
        assert_eq!(norm_value(FieldKind::F64x2, "1234.50"), "1234.50");
        assert_eq!(norm_value(FieldKind::F64x2, "0"), "0.00");
        assert_eq!(norm_value(FieldKind::Int, " 5 "), "5");
        assert_eq!(norm_value(FieldKind::Int, "-7"), "-7");
        assert_eq!(norm_value(FieldKind::Str, "  hi "), "hi");
    }

    #[test]
    fn hash_is_order_independent_but_content_sensitive() {
        let a = [
            ("monto".to_string(), "1.00".to_string()),
            ("status".to_string(), "active".to_string()),
        ];
        let a: Vec<(&str, String)> = a.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
        let mut b = a.clone();
        b.reverse();
        assert_eq!(
            content_record_hash("Credit", "CR1", &a),
            content_record_hash("Credit", "CR1", &b),
            "field order must not change the hash"
        );
        // Different value → different hash.
        let c = [
            ("monto", "2.00".to_string()),
            ("status", "active".to_string()),
        ];
        assert_ne!(
            content_record_hash("Credit", "CR1", &a),
            content_record_hash("Credit", "CR1", &c),
        );
        // Different key → different hash (no cross-record collision).
        assert_ne!(
            content_record_hash("Credit", "CR1", &a),
            content_record_hash("Credit", "CR2", &a),
        );
    }

    #[test]
    fn appended_payment_does_not_affect_credit_fold() {
        // Append-invariance at the parse/filter layer: a Phase-3 Payment row
        // sharing the rfc cluster must be excluded from the Credit fold.
        let credit = box_record(
            1,
            "creditos",
            &[
                ("_type", "\"Credit\""),
                ("rfc", "\"ABC\""),
                ("credit_id", "\"CR1\""),
                ("empresa_id", "\"E1\""),
                ("producto_id", "\"P1\""),
                ("monto", "1234.5"),
                ("status", "\"active\""),
                ("dias_atraso", "0"),
            ],
        );
        let payment = box_record(
            2,
            "creditos",
            &[
                ("_type", "\"Payment\""),
                ("rfc", "\"ABC\""),
                ("payment_id", "\"PAY_Q7\""),
                ("credit_id", "\"CR1\""),
                ("monto", "50.0"),
            ],
        );

        let fold = |resp: &str| -> u64 {
            let mut acc = 0u64;
            for rec in parse_box_records(resp) {
                if rec.get("_type").map(String::as_str) != Some("Credit") {
                    continue;
                }
                let cid = rec.get("credit_id").cloned().unwrap();
                let fields = canonical_fields(&rec, CREDIT_FIELDS).unwrap();
                acc = acc.wrapping_add(content_record_hash("Credit", &cid, &fields));
            }
            acc
        };

        let only_credit = fold(&credit);
        let credit_plus_append = fold(&format!("{credit}\n{payment}"));
        assert_eq!(
            only_credit, credit_plus_append,
            "appended Payment must not change the Credit content fold"
        );
        assert_ne!(only_credit, 0, "the Credit row must actually be hashed");
    }

    #[test]
    fn missing_required_field_drops_record_from_fold() {
        // A record missing a hashed field yields None → excluded from the
        // observed fold → diverges from expectation (gate fires, no silent pass).
        let truncated = box_record(
            1,
            "creditos",
            &[
                ("_type", "\"Credit\""),
                ("credit_id", "\"CR1\""),
                ("monto", "1.0"),
                // missing empresa_id / producto_id / status / dias_atraso
            ],
        );
        let rec = &parse_box_records(&truncated)[0];
        assert!(canonical_fields(rec, CREDIT_FIELDS).is_none());
    }
}
