//! Per-entity deterministic streams. Each function returns `impl Iterator`
//! seeded by a domain-separated PRNG so streams advance independently.

use crate::model::*;
use crate::rfc::{curp_for_rfc, rfc_for_ordinal};
use crate::sequences;
use chrono::{Duration, NaiveDate, TimeZone, Utc};
use rand::RngExt;
use rand_chacha::ChaCha20Rng;

const REGIONES: &[&str] = &[
    "Norte",
    "Sur",
    "Centro",
    "Occidente",
    "Sureste",
    "Bajío",
    "Pacífico",
    "Golfo",
];
const PRODUCT_NAMES: &[&str] = &[
    "Crédito Personal",
    "Crédito Empresarial",
    "Crédito Auto",
    "Crédito Hipotecario",
    "Línea de Crédito",
    "Crédito Educativo",
    "Crédito Equipamiento",
];
const ESTADOS: &[&str] = &[
    "CDMX",
    "Jalisco",
    "Nuevo León",
    "Estado de México",
    "Veracruz",
    "Puebla",
    "Guanajuato",
    "Chihuahua",
    "Michoacán",
    "Sinaloa",
];
const TAGS_POOL: &[&str] = &[
    "vip",
    "moroso",
    "nuevo",
    "recurrente",
    "premium",
    "alto-riesgo",
    "bajo-riesgo",
    "verificado",
    "pendiente-doc",
    "credito-grupo",
];
const REGIMENES: &[&str] = &[
    "Personas Físicas",
    "Régimen Simplificado",
    "Personas Morales",
    "RIF",
    "Honorarios",
];
const ACTIVIDADES: &[&str] = &[
    "Comercio",
    "Servicios",
    "Manufactura",
    "Construcción",
    "Agricultura",
    "Transporte",
    "Tecnología",
    "Educación",
];
const NOMBRES: &[&str] = &[
    "Juan", "María", "Carlos", "Ana", "Luis", "Patricia", "Jorge", "Lucía", "Roberto", "Adriana",
    "Miguel", "Sofía",
];
const APELLIDOS: &[&str] = &[
    "García",
    "Martínez",
    "Rodríguez",
    "López",
    "González",
    "Pérez",
    "Sánchez",
    "Ramírez",
    "Torres",
    "Flores",
];
const METODOS_PAGO: &[&str] = &["transfer", "cash", "card", "spei", "oxxo"];
const CANALES: &[&str] = &["sms", "email", "push", "whatsapp"];
const ACTION_TYPES: &[&str] = &["call", "visit", "email", "sms", "legal_notice"];
const RESULTADOS: &[&str] = &[
    "no_answer",
    "promise_to_pay",
    "partial_payment",
    "full_payment",
    "refused",
    "wrong_number",
];
const AUDIT_ACTIONS: &[&str] = &[
    "login",
    "credit_application_submitted",
    "credit_approved",
    "credit_denied",
    "payment_received",
    "status_changed",
    "collection_started",
];

fn epoch_2020() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap()
}

fn pick<'a, T>(rng: &mut ChaCha20Rng, slice: &'a [T]) -> &'a T {
    &slice[rng.random_range(0..slice.len())]
}

pub fn empresas_iter(mut rng: ChaCha20Rng, n: u64) -> impl Iterator<Item = Empresa> {
    (0..n).map(move |i| Empresa {
        empresa_id: sequences::empresa_id(i),
        nombre: format!("Empresa {} S.A. de C.V.", i + 1),
        region: pick(&mut rng, REGIONES).to_string(),
        activa: rng.random_bool(0.95),
    })
}

pub fn productos_iter(
    mut rng: ChaCha20Rng,
    n: u64,
    n_empresas: u64,
) -> impl Iterator<Item = Producto> {
    (0..n).map(move |i| Producto {
        producto_id: sequences::producto_id(i),
        empresa_id: sequences::empresa_id(i % n_empresas),
        nombre: pick(&mut rng, PRODUCT_NAMES).to_string(),
        tasa_interes: 0.05 + rng.random::<f64>() * 0.30,
        plazo_meses: *pick(&mut rng, &[6, 12, 18, 24, 36, 48, 60]),
    })
}

pub fn clients_iter(mut rng: ChaCha20Rng, n: u64) -> impl Iterator<Item = Client> {
    let base = epoch_2020();
    (0..n).map(move |i| {
        let rfc = rfc_for_ordinal(&mut rng, i);
        let curp = curp_for_rfc(&rfc, i);
        let bureau = 300 + rng.random_range(0..550);
        let risk = if bureau > 700 {
            "low"
        } else if bureau > 550 {
            "medium"
        } else {
            "high"
        };
        let n_tags = rng.random_range(0..4);
        let tags: Vec<String> = (0..n_tags)
            .map(|_| pick(&mut rng, TAGS_POOL).to_string())
            .collect();
        let estado = pick(&mut rng, ESTADOS).to_string();
        Client {
            rfc,
            curp,
            nombre: format!(
                "{} {} {}",
                pick(&mut rng, NOMBRES),
                pick(&mut rng, APELLIDOS),
                pick(&mut rng, APELLIDOS)
            ),
            scoring: Scoring {
                bureau,
                risk: risk.to_string(),
                limite_credito: 5_000.0 + rng.random::<f64>() * 495_000.0,
            },
            datos_ubicacion: Ubicacion {
                entidad_federativa: estado,
                municipio: format!("Municipio {}", rng.random_range(1..120)),
                codigo_postal: format!("{:05}", rng.random_range(1000..99999)),
            },
            datos_identificacion: Identificacion {
                tipo_id: pick(&mut rng, &["INE", "Pasaporte", "Cédula"]).to_string(),
                numero_id: format!("{:013}", i.wrapping_mul(0x9E3779B1)),
            },
            caracteristicas_fiscales: Fiscales {
                regimen: pick(&mut rng, REGIMENES).to_string(),
                actividad: pick(&mut rng, ACTIVIDADES).to_string(),
            },
            tags,
            fecha_alta: base + Duration::seconds(rng.random_range(0..3 * 365 * 86400)),
        }
    })
}

/// Credits stream — enumerates by client and emits a per-client count
/// drawn from a Poisson-ish distribution centred at the per-client average.
/// Output ordering: client-major (all credits of client 0, then client 1, …)
/// so xyzDB's gravity-by-rfc bulk path stays in cache for each gravity bucket.
pub fn credits_iter(
    mut rng: ChaCha20Rng,
    n_clients: u64,
    n_empresas: u64,
    n_productos: u64,
) -> impl Iterator<Item = Credit> {
    let base = epoch_2020();
    let mut credit_ord: u64 = 0;
    let mut client_ord: u64 = 0;
    let _avg_per_client = 2.0;
    let mut buf = Vec::<Credit>::new();
    std::iter::from_fn(move || {
        loop {
            if let Some(c) = buf.pop() {
                return Some(c);
            }
            if client_ord >= n_clients {
                return None;
            }
            // Fixed exactly 2 credits per client. This makes the
            // credit_ord ↔ client_ord mapping deterministic
            // (`client_ord = credit_ord / 2`), which is required so the
            // installments / payments / collections / collection_actions
            // streams can recover the parent credit's rfc by computing
            // `rfc_for_ordinal(_, credit_ord / 2)` and have it MATCH the
            // rfc that credits_iter assigned. PG enforces this FK via
            // `credits.rfc REFERENCES clientes(rfc)`; without a fixed
            // count, the bench fails Phase 1 with a FK violation.
            // Stochastic per-client variance is a v0.2.5.1 follow-up.
            let count = 2u32;
            let mut rfc_rng = derived_rng(&rng, client_ord ^ 0xC0FFEE);
            let rfc = rfc_for_ordinal(&mut rfc_rng, client_ord);
            for _ in 0..count {
                let monto = (1_000.0 + rng.random::<f64>() * 199_000.0).round_dp_ceil(2);
                let status = sample_credit_status(&mut rng);
                let dias_atraso = if status == CreditStatus::Overdue {
                    rng.random_range(1..180)
                } else {
                    0
                };
                buf.push(Credit {
                    credit_id: sequences::credit_id(credit_ord),
                    rfc: rfc.clone(),
                    empresa_id: sequences::empresa_id(rng.random_range(0..n_empresas)),
                    producto_id: sequences::producto_id(rng.random_range(0..n_productos)),
                    monto,
                    status,
                    fecha_creacion: base + Duration::seconds(rng.random_range(0..4 * 365 * 86400)),
                    fecha_vencimiento: NaiveDate::from_ymd_opt(2027, 12, 31).unwrap()
                        - Duration::days(rng.random_range(0..365)),
                    dias_atraso,
                });
                credit_ord += 1;
            }
            client_ord += 1;
            // Keep client-major order: drain in insertion order, not LIFO.
            buf.reverse();
        }
    })
}

pub fn installments_iter(
    mut rng: ChaCha20Rng,
    n_credits: u64,
) -> impl Iterator<Item = Installment> {
    let mut credit_ord: u64 = 0;
    let mut inst_ord: u64 = 0;
    let mut buf = Vec::<Installment>::new();
    std::iter::from_fn(move || {
        loop {
            if let Some(v) = buf.pop() {
                return Some(v);
            }
            if credit_ord >= n_credits {
                return None;
            }
            // Fixed 25 installments per credit. Deterministic per-credit count
            // makes `expected_counts()` formula (credits × 25) match stream
            // output exactly, so Phase 5 verify is exact. Stochastic variance
            // is a v0.2.5.1 follow-up if needed.
            let count = 25u32;
            let cid = sequences::credit_id(credit_ord);
            // RFC / empresa via the deterministic mapping `client_ord = credit_ord / 2`
            // (credits_iter emits exactly 2 credits per client, so this is exact).
            let mut sub_rng = derived_rng(&rng, credit_ord ^ 0xBADC0DE);
            let rfc = rfc_for_ordinal(&mut sub_rng, credit_ord / 2);
            let empresa_id = sequences::empresa_id(credit_ord % 80);
            for n in 1..=count {
                let status = sample_inst_status(&mut rng);
                let dias_atraso = if status == InstallmentStatus::Overdue {
                    rng.random_range(1..120)
                } else {
                    0
                };
                buf.push(Installment {
                    installment_id: sequences::installment_id(inst_ord),
                    credit_id: cid.clone(),
                    rfc: rfc.clone(),
                    empresa_id: empresa_id.clone(),
                    numero: n as i32,
                    monto_total: (200.0 + rng.random::<f64>() * 9_800.0).round_dp_ceil(2),
                    status,
                    dias_atraso,
                    fecha_vencimiento: NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()
                        + Duration::days((n as i64) * 30),
                });
                inst_ord += 1;
            }
            credit_ord += 1;
            buf.reverse();
        }
    })
}

pub fn payments_iter(mut rng: ChaCha20Rng, n_credits: u64) -> impl Iterator<Item = Payment> {
    let base = epoch_2020();
    let mut credit_ord: u64 = 0;
    let mut pay_ord: u64 = 0;
    let mut buf = Vec::<Payment>::new();
    std::iter::from_fn(move || {
        loop {
            if let Some(v) = buf.pop() {
                return Some(v);
            }
            if credit_ord >= n_credits {
                return None;
            }
            // Fixed 12 payments per credit. See note in installments_iter.
            let count = 12u32;
            let cid = sequences::credit_id(credit_ord);
            let mut sub_rng = derived_rng(&rng, credit_ord ^ 0xFEEDFACE);
            let rfc = rfc_for_ordinal(&mut sub_rng, credit_ord / 2);
            for _ in 0..count {
                // Q6 needs ~5 % of payments above $50 000.
                let monto = if rng.random_bool(0.05) {
                    50_001.0 + rng.random::<f64>() * 200_000.0
                } else {
                    100.0 + rng.random::<f64>() * 49_900.0
                };
                buf.push(Payment {
                    payment_id: sequences::payment_id(pay_ord),
                    credit_id: cid.clone(),
                    installment_id: if rng.random_bool(0.8) {
                        Some(sequences::installment_id(
                            rng.random_range(0..n_credits * 25),
                        ))
                    } else {
                        None
                    },
                    rfc: rfc.clone(),
                    monto: monto.round_dp_ceil(2),
                    // v0.3.4 Phase E Session 1 (C-4 fix Option 3): ceiling extended
                    // from 6y → 10y to keep `fecha_pago >= now - 30d` non-empty
                    // for any wall-clock run within v0.3.x lifetime. See C-8 in
                    // the cross-engine bench design notes §12.3 for the wall-clock decoupling
                    // caveat and v0.3.5 backlog Entry 14 (REFERENCE_NOW const).
                    fecha_pago: base + Duration::seconds(rng.random_range(0..10 * 365 * 86400)),
                    metodo: pick(&mut rng, METODOS_PAGO).to_string(),
                });
                pay_ord += 1;
            }
            credit_ord += 1;
            buf.reverse();
        }
    })
}

pub fn collections_iter(mut rng: ChaCha20Rng, n_credits: u64) -> impl Iterator<Item = Collection> {
    let base = epoch_2020();
    let mut credit_ord: u64 = 0;
    let mut col_ord: u64 = 0;
    let mut buf = Vec::<Collection>::new();
    std::iter::from_fn(move || {
        loop {
            if let Some(v) = buf.pop() {
                return Some(v);
            }
            if credit_ord >= n_credits {
                return None;
            }
            // Deterministic 30% of credits get exactly 1 collection
            // (3 out of every 10 credits, by `credit_ord % 10 < 3`).
            // expected_counts() says collections = credits × 0.3 → matches.
            let count = if credit_ord % 10 < 3 { 1u32 } else { 0u32 };
            let cid = sequences::credit_id(credit_ord);
            let mut sub_rng = derived_rng(&rng, credit_ord);
            let rfc = rfc_for_ordinal(&mut sub_rng, credit_ord / 2);
            for _ in 0..count {
                buf.push(Collection {
                    collection_id: sequences::collection_id(col_ord),
                    credit_id: cid.clone(),
                    rfc: rfc.clone(),
                    monto_pendiente: (500.0 + rng.random::<f64>() * 99_500.0).round_dp_ceil(2),
                    status: sample_collection_status(&mut rng),
                    fecha_inicio: base + Duration::seconds(rng.random_range(0..5 * 365 * 86400)),
                });
                col_ord += 1;
            }
            credit_ord += 1;
            buf.reverse();
        }
    })
}

pub fn collection_actions_iter(
    mut rng: ChaCha20Rng,
    n_collections: u64,
) -> impl Iterator<Item = CollectionAction> {
    let base = epoch_2020();
    let mut col_ord: u64 = 0;
    let mut act_ord: u64 = 0;
    let mut buf = Vec::<CollectionAction>::new();
    std::iter::from_fn(move || {
        loop {
            if let Some(v) = buf.pop() {
                return Some(v);
            }
            if col_ord >= n_collections {
                return None;
            }
            // Fixed 3 actions per collection. expected_counts() says
            // collection_actions = collections × 3 → matches.
            let count = 3u32;
            let coll_id = sequences::collection_id(col_ord);
            let credit_id = sequences::credit_id(col_ord); // approximation
            let mut sub_rng = derived_rng(&rng, col_ord);
            let rfc = rfc_for_ordinal(&mut sub_rng, col_ord);
            for _ in 0..count {
                buf.push(CollectionAction {
                    action_id: sequences::collection_action_id(act_ord),
                    collection_id: coll_id.clone(),
                    credit_id: credit_id.clone(),
                    rfc: rfc.clone(),
                    tipo: pick(&mut rng, ACTION_TYPES).to_string(),
                    fecha: base + Duration::seconds(rng.random_range(0..5 * 365 * 86400)),
                    resultado: pick(&mut rng, RESULTADOS).to_string(),
                });
                act_ord += 1;
            }
            col_ord += 1;
            buf.reverse();
        }
    })
}

pub fn applications_iter(
    mut rng: ChaCha20Rng,
    n: u64,
    n_clients: u64,
    n_empresas: u64,
    n_productos: u64,
) -> impl Iterator<Item = CreditApplication> {
    let base = epoch_2020();
    (0..n).map(move |i| {
        let mut sub_rng = derived_rng(&rng, i);
        let rfc = rfc_for_ordinal(&mut sub_rng, i % n_clients);
        CreditApplication {
            application_id: sequences::application_id(i),
            rfc,
            empresa_id: sequences::empresa_id(rng.random_range(0..n_empresas)),
            producto_id: sequences::producto_id(rng.random_range(0..n_productos)),
            monto_solicitado: (1_000.0 + rng.random::<f64>() * 499_000.0).round_dp_ceil(2),
            status: sample_application_status(&mut rng),
            fecha_solicitud: base + Duration::seconds(rng.random_range(0..4 * 365 * 86400)),
        }
    })
}

pub fn audit_iter(
    mut rng: ChaCha20Rng,
    n: u64,
    n_clients: u64,
) -> impl Iterator<Item = AuditLogEntry> {
    let base = epoch_2020();
    (0..n).map(move |i| {
        let mut sub_rng = derived_rng(&rng, i);
        let rfc = rfc_for_ordinal(&mut sub_rng, i % n_clients);
        AuditLogEntry {
            audit_id: i + 1,
            rfc,
            credit_id: if rng.random_bool(0.6) {
                Some(sequences::credit_id(rng.random_range(0..n_clients * 2)))
            } else {
                None
            },
            action_type: pick(&mut rng, AUDIT_ACTIONS).to_string(),
            details: serde_json::json!({
                "ip": format!("{}.{}.{}.{}", rng.random_range(1..255), rng.random_range(0..255), rng.random_range(0..255), rng.random_range(0..255)),
                "agent": "bench-harness/0.2.5",
            }),
            fecha: base + Duration::seconds(rng.random_range(0..5 * 365 * 86400)),
        }
    })
}

pub fn notifications_iter(
    mut rng: ChaCha20Rng,
    n: u64,
    n_clients: u64,
) -> impl Iterator<Item = Notification> {
    let base = epoch_2020();
    (0..n).map(move |i| {
        let mut sub_rng = derived_rng(&rng, i);
        let rfc = rfc_for_ordinal(&mut sub_rng, i % n_clients);
        Notification {
            notification_id: i + 1,
            rfc,
            canal: pick(&mut rng, CANALES).to_string(),
            contenido: format!(
                "Notificación {} - recordatorio",
                pick(
                    &mut rng,
                    &["pago", "vencimiento", "promoción", "renovación"]
                )
            ),
            fecha: base + Duration::seconds(rng.random_range(0..5 * 365 * 86400)),
        }
    })
}

pub fn bi_iter(mut rng: ChaCha20Rng, n: u64, n_empresas: u64) -> impl Iterator<Item = BiSnapshot> {
    (0..n).map(move |i| {
        let day = NaiveDate::from_ymd_opt(2024, 1, 1).unwrap() + Duration::days((i % 365) as i64);
        BiSnapshot {
            snapshot_id: i + 1,
            empresa_id: sequences::empresa_id(i % n_empresas),
            fecha: day,
            metricas: serde_json::json!({
                "creditos_activos": rng.random_range(100..50_000),
                "monto_cartera": rng.random::<f64>() * 100_000_000.0,
                "tasa_morosidad": rng.random::<f64>() * 0.3,
            }),
        }
    })
}

// ── helpers ──────────────────────────────────────────────────────────

fn sample_count(rng: &mut ChaCha20Rng, mean: f64, min: u32, max: u32) -> u32 {
    // Truncated discretised normal-ish: mean±sigma where sigma ~= mean/2.
    let v = (mean + (rng.random::<f64>() - 0.5) * mean).max(min as f64);
    (v.round() as u32).clamp(min, max)
}

fn sample_credit_status(rng: &mut ChaCha20Rng) -> CreditStatus {
    let r: f64 = rng.random();
    if r < 0.55 {
        CreditStatus::Active
    } else if r < 0.70 {
        CreditStatus::Overdue
    } else if r < 0.92 {
        CreditStatus::Paid
    } else if r < 0.98 {
        CreditStatus::Cancelled
    } else {
        CreditStatus::Defaulted
    }
}

fn sample_inst_status(rng: &mut ChaCha20Rng) -> InstallmentStatus {
    let r: f64 = rng.random();
    if r < 0.60 {
        InstallmentStatus::Paid
    } else if r < 0.78 {
        InstallmentStatus::Pending
    } else if r < 0.93 {
        InstallmentStatus::Overdue
    } else {
        InstallmentStatus::Partial
    }
}

fn sample_collection_status(rng: &mut ChaCha20Rng) -> CollectionStatus {
    let r: f64 = rng.random();
    if r < 0.4 {
        CollectionStatus::Active
    } else if r < 0.85 {
        CollectionStatus::Resolved
    } else {
        CollectionStatus::WrittenOff
    }
}

fn sample_application_status(rng: &mut ChaCha20Rng) -> ApplicationStatus {
    let r: f64 = rng.random();
    if r < 0.55 {
        ApplicationStatus::Approved
    } else if r < 0.80 {
        ApplicationStatus::Denied
    } else if r < 0.95 {
        ApplicationStatus::Submitted
    } else {
        ApplicationStatus::Withdrawn
    }
}

/// Derive a sub-stream RNG from a salt alone (parent is ignored).
/// This guarantees that the same salt produces the same sub-stream
/// regardless of how much the parent has advanced — what callers want
/// when they need a per-iteration sub-stream that is stable across
/// dataset variations.
fn derived_rng(_parent: &ChaCha20Rng, salt: u64) -> ChaCha20Rng {
    use rand::SeedableRng;
    let mut bytes = [0u8; 32];
    // Splitmix-style mixing of `salt` into 32 deterministic bytes.
    let mut x = salt.wrapping_add(0x9E3779B97F4A7C15);
    for chunk in bytes.chunks_exact_mut(8) {
        x = (x ^ (x >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94D049BB133111EB);
        x ^= x >> 31;
        chunk.copy_from_slice(&x.to_le_bytes());
    }
    ChaCha20Rng::from_seed(bytes)
}

trait RoundDp {
    fn round_dp_ceil(self, dp: u32) -> f64;
}
impl RoundDp for f64 {
    fn round_dp_ceil(self, dp: u32) -> f64 {
        let m = 10f64.powi(dp as i32);
        (self * m).round() / m
    }
}
