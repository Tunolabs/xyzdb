//! Deterministic fintech dataset generator for the native cross-engine bench.
//!
//! Same seed → byte-identical logical records. Each driver materialises the
//! emitted records into its native shape (xyzDB heterogeneous lobes,
//! PostgreSQL normalised tables, MongoDB embedded documents).
//!
//! Volume proportions calibrated against realistic Mexican fintech operator
//! shape.

pub mod bench;
pub mod erratica;
pub mod golden;
pub mod model;
pub mod personas;
pub mod rfc;
pub mod schedule;
pub mod sequences;

mod streams;

pub use golden::{
    AggregateCount, AggregateCountSum, GoldenDiff, GoldenFile, GoldenVerifyQueries,
    GoldenVerifyResults, V4LobeTypeCounts, V6ConfigCounts, compare_count, compare_count_sum,
};
pub use model::*;
use rand::SeedableRng;
use rand_chacha::ChaCha20Rng;

/// Default seed used for reproducible runs.
pub const DEFAULT_SEED: u64 = 42;

/// Per-stream domain-separation salts. Each entity stream uses
/// `seed + salt` so re-seeding a single stream does not perturb others.
mod salt {
    pub const EMPRESAS: u64 = 0x01;
    pub const PRODUCTOS: u64 = 0x02;
    pub const CLIENTS: u64 = 0x03;
    pub const CREDITS: u64 = 0x04;
    pub const INSTALLMENTS: u64 = 0x05;
    pub const PAYMENTS: u64 = 0x06;
    pub const COLLECTIONS: u64 = 0x07;
    pub const COLLECTION_ACTIONS: u64 = 0x08;
    pub const APPLICATIONS: u64 = 0x09;
    pub const AUDIT_LOG: u64 = 0x0A;
    pub const NOTIFICATIONS: u64 = 0x0B;
    pub const BI_SNAPSHOTS: u64 = 0x0C;
}

/// Top-level dataset. Cheap to construct — exposes per-entity iterators
/// that drivers consume in the order they need (xyzDB by lobe, PG by FK
/// dependency order, etc.).
#[derive(Clone, Copy, Debug)]
pub struct Dataset {
    pub seed: u64,
    pub scale: f64,
}

impl Dataset {
    pub fn new(seed: u64, scale: f64) -> Self {
        assert!(scale > 0.0, "scale must be positive");
        Self { seed, scale }
    }

    /// Default reference dataset: seed 42, scale 0.1.
    pub fn reference() -> Self {
        Self::new(DEFAULT_SEED, 0.1)
    }

    /// Per-entity expected counts. Used by Phase 5 verify and by capacity
    /// planning. Numbers reflect the proportions in the design doc §6.2.
    pub fn expected_counts(&self) -> ExpectedCounts {
        // Catalog (independent of scale)
        let empresas = 80;
        let productos = 350;

        // Client-derived entities (scale-linear)
        let clients = (1_500_000.0 * self.scale).round() as u64;

        // Per-client averages (matched to legacy harness proportions
        // verified against real Mexican fintech operator data shape).
        let credits = (clients as f64 * 2.0).round() as u64;
        let installments = (credits as f64 * 25.0).round() as u64;
        let payments = (credits as f64 * 12.0).round() as u64;
        let collections = (credits as f64 * 0.3).round() as u64;
        let collection_actions = (collections as f64 * 3.0).round() as u64;
        let applications = (clients as f64 * 2.5).round() as u64;
        let audit_log = (clients as f64 * 10.0).round() as u64;
        let notifications = (clients as f64 * 8.0).round() as u64;
        let bi_snapshots = (84_000.0 * self.scale).round() as u64;

        ExpectedCounts {
            empresas,
            productos,
            clients,
            credits,
            installments,
            payments,
            collections,
            collection_actions,
            applications,
            audit_log,
            notifications,
            bi_snapshots,
        }
    }

    fn rng_for(&self, salt: u64) -> ChaCha20Rng {
        // Domain-separated PRNG: seed XOR salt then push through ChaCha20.
        // Each stream advances independently — adding/removing entities
        // from one stream does not perturb others.
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(&self.seed.to_le_bytes());
        bytes[8..16].copy_from_slice(&salt.to_le_bytes());
        ChaCha20Rng::from_seed(bytes)
    }

    pub fn empresas(&self) -> impl Iterator<Item = Empresa> {
        let counts = self.expected_counts();
        streams::empresas_iter(self.rng_for(salt::EMPRESAS), counts.empresas)
    }

    pub fn productos(&self) -> impl Iterator<Item = Producto> {
        let counts = self.expected_counts();
        streams::productos_iter(
            self.rng_for(salt::PRODUCTOS),
            counts.productos,
            counts.empresas,
        )
    }

    pub fn clients(&self) -> impl Iterator<Item = Client> {
        let counts = self.expected_counts();
        streams::clients_iter(self.rng_for(salt::CLIENTS), counts.clients)
    }

    pub fn credits(&self) -> impl Iterator<Item = Credit> {
        let counts = self.expected_counts();
        streams::credits_iter(
            self.rng_for(salt::CREDITS),
            counts.clients,
            counts.empresas,
            counts.productos,
        )
    }

    pub fn installments(&self) -> impl Iterator<Item = Installment> {
        let counts = self.expected_counts();
        streams::installments_iter(self.rng_for(salt::INSTALLMENTS), counts.credits)
    }

    pub fn payments(&self) -> impl Iterator<Item = Payment> {
        let counts = self.expected_counts();
        streams::payments_iter(self.rng_for(salt::PAYMENTS), counts.credits)
    }

    pub fn collections(&self) -> impl Iterator<Item = Collection> {
        let counts = self.expected_counts();
        streams::collections_iter(self.rng_for(salt::COLLECTIONS), counts.credits)
    }

    pub fn collection_actions(&self) -> impl Iterator<Item = CollectionAction> {
        let counts = self.expected_counts();
        streams::collection_actions_iter(self.rng_for(salt::COLLECTION_ACTIONS), counts.collections)
    }

    pub fn credit_applications(&self) -> impl Iterator<Item = CreditApplication> {
        let counts = self.expected_counts();
        streams::applications_iter(
            self.rng_for(salt::APPLICATIONS),
            counts.applications,
            counts.clients,
            counts.empresas,
            counts.productos,
        )
    }

    pub fn audit_log(&self) -> impl Iterator<Item = AuditLogEntry> {
        let counts = self.expected_counts();
        streams::audit_iter(
            self.rng_for(salt::AUDIT_LOG),
            counts.audit_log,
            counts.clients,
        )
    }

    pub fn notifications(&self) -> impl Iterator<Item = Notification> {
        let counts = self.expected_counts();
        streams::notifications_iter(
            self.rng_for(salt::NOTIFICATIONS),
            counts.notifications,
            counts.clients,
        )
    }

    pub fn bi_snapshots(&self) -> impl Iterator<Item = BiSnapshot> {
        let counts = self.expected_counts();
        streams::bi_iter(
            self.rng_for(salt::BI_SNAPSHOTS),
            counts.bi_snapshots,
            counts.empresas,
        )
    }

    /// v0.3.4 Phase E Session 1 — verify-golden helper.
    ///
    /// Truth source for the golden file: generator iterators in-memory,
    /// NOT any engine's `bulk_load` result. This isolates dataset semantics
    /// from ingestion semantics; ingestion bugs surface as `golden_diffs`,
    /// not as silent truth. See the cross-engine bench design notes §12.3
    /// "Verify-golden methodology" + caveat C-8 (wall-clock decoupling).
    ///
    /// Computes aggregates V1-V6 by walking the dataset iterators once.
    /// Cost is `O(records)` per scale — acceptable since the golden file
    /// is generated once per (seed, scale) pair and cached.
    pub fn compute_golden_aggregates(
        &self,
        reference_now: chrono::DateTime<chrono::Utc>,
    ) -> GoldenFile {
        // V1: credits — count + sum(monto).
        let mut v1_n: u64 = 0;
        let mut v1_sum: f64 = 0.0;
        for c in self.credits() {
            v1_n += 1;
            v1_sum += c.monto;
        }

        // V2: installments WHERE status = "overdue" — count + sum(monto_total).
        let mut v2_n: u64 = 0;
        let mut v2_sum: f64 = 0.0;
        for i in self.installments() {
            if matches!(i.status, InstallmentStatus::Overdue) {
                v2_n += 1;
                v2_sum += i.monto_total;
            }
        }

        // V3: payments — count + sum(monto).
        let mut v3_n: u64 = 0;
        let mut v3_sum: f64 = 0.0;
        for p in self.payments() {
            v3_n += 1;
            v3_sum += p.monto;
        }

        // V4: counts by (lobe, _type). Derived from expected_counts() so
        // matches what bulk_load is supposed to ingest exactly.
        let counts = self.expected_counts();
        let v4 = V4LobeTypeCounts::from_counts(&counts);

        // V5: distinct rfc on clients — by construction each client has a
        // unique rfc (sequences.rs::rfc_for_ordinal is injective on the
        // client ordinal 0..counts.clients), so distinct count == clients.
        let v5_n = counts.clients;

        // V6: configuracion catalogue counts (canary).
        let v6 = V6ConfigCounts {
            empresas: counts.empresas,
            productos: counts.productos,
            total: counts.empresas + counts.productos,
        };

        GoldenFile {
            version: "v0.3.4".to_string(),
            seed: self.seed,
            scale: self.scale,
            reference_now: reference_now.to_rfc3339(),
            // 1e-6 relative is ~3 orders looser than the f64 sum noise
            // expected from cross-engine order-of-operations divergence
            // (PG column-major vs Mongo aggregation pipeline vs xyzDB
            // gravity-co-located scan), and ~6 orders stricter than any
            // difference that would indicate a real bug.
            tolerance_f64_relative: 1e-6,
            // Caveats explicitly excluded from this golden file. Empty for the
            // current three-engine set (xyzDB/PG/Mongo): the former C-3 (Surreal
            // Q8 3-step) exclusion was retired with the SurrealDB driver. A new
            // entry is added only if a real cross-engine exclusion reappears.
            caveats_active: vec![],
            verify_queries: GoldenVerifyQueries {
                v1_credits_total: AggregateCountSum {
                    n: v1_n,
                    sum: v1_sum,
                },
                v2_installments_overdue: AggregateCountSum {
                    n: v2_n,
                    sum: v2_sum,
                },
                v3_payments_total: AggregateCountSum {
                    n: v3_n,
                    sum: v3_sum,
                },
                v4_lobe_type_counts: v4,
                v5_clients_distinct_rfc: AggregateCount { n: v5_n },
                v6_config_counts: v6,
            },
        }
    }
}

/// Per-entity expected counts. Phase 5 verify checks exact equality.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct ExpectedCounts {
    pub empresas: u64,
    pub productos: u64,
    pub clients: u64,
    pub credits: u64,
    pub installments: u64,
    pub payments: u64,
    pub collections: u64,
    pub collection_actions: u64,
    pub applications: u64,
    pub audit_log: u64,
    pub notifications: u64,
    pub bi_snapshots: u64,
}

impl ExpectedCounts {
    pub fn total(&self) -> u64 {
        self.empresas
            + self.productos
            + self.clients
            + self.credits
            + self.installments
            + self.payments
            + self.collections
            + self.collection_actions
            + self.applications
            + self.audit_log
            + self.notifications
            + self.bi_snapshots
    }
}
