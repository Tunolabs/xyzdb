//! Logical record types emitted by the generator. Drivers materialise
//! these into engine-native shapes.

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

/// Status enums match across engines so the generator can emit one logical
/// state and the driver projects it to its native representation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CreditStatus {
    Active,
    Overdue,
    Paid,
    Cancelled,
    Defaulted,
}

impl CreditStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Overdue => "overdue",
            Self::Paid => "paid",
            Self::Cancelled => "cancelled",
            Self::Defaulted => "defaulted",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstallmentStatus {
    Pending,
    Paid,
    Overdue,
    Partial,
}

impl InstallmentStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Paid => "paid",
            Self::Overdue => "overdue",
            Self::Partial => "partial",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CollectionStatus {
    Active,
    Resolved,
    WrittenOff,
}

impl CollectionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Resolved => "resolved",
            Self::WrittenOff => "written_off",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApplicationStatus {
    Submitted,
    Approved,
    Denied,
    Withdrawn,
}

impl ApplicationStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Submitted => "submitted",
            Self::Approved => "approved",
            Self::Denied => "denied",
            Self::Withdrawn => "withdrawn",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Empresa {
    pub empresa_id: String,
    pub nombre: String,
    pub region: String,
    pub activa: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Producto {
    pub producto_id: String,
    pub empresa_id: String,
    pub nombre: String,
    pub tasa_interes: f64,
    pub plazo_meses: i32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Client {
    pub rfc: String,
    pub curp: String,
    pub nombre: String,
    pub scoring: Scoring,
    pub datos_ubicacion: Ubicacion,
    pub datos_identificacion: Identificacion,
    pub caracteristicas_fiscales: Fiscales,
    pub tags: Vec<String>,
    pub fecha_alta: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Scoring {
    pub bureau: i32,
    pub risk: String,
    pub limite_credito: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Ubicacion {
    pub entidad_federativa: String,
    pub municipio: String,
    pub codigo_postal: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Identificacion {
    pub tipo_id: String,
    pub numero_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Fiscales {
    pub regimen: String,
    pub actividad: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Credit {
    pub credit_id: String,
    pub rfc: String,
    pub empresa_id: String,
    pub producto_id: String,
    pub monto: f64,
    pub status: CreditStatus,
    pub fecha_creacion: DateTime<Utc>,
    pub fecha_vencimiento: NaiveDate,
    pub dias_atraso: i32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Installment {
    pub installment_id: String,
    pub credit_id: String,
    /// Denormalised onto the installment so engines that don't JOIN
    /// (xyzDB, Mongo embedded) can filter directly.
    pub rfc: String,
    pub empresa_id: String,
    pub numero: i32,
    pub monto_total: f64,
    pub status: InstallmentStatus,
    pub dias_atraso: i32,
    pub fecha_vencimiento: NaiveDate,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Payment {
    pub payment_id: String,
    pub credit_id: String,
    pub installment_id: Option<String>,
    pub rfc: String,
    pub monto: f64,
    pub fecha_pago: DateTime<Utc>,
    pub metodo: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Collection {
    pub collection_id: String,
    pub credit_id: String,
    pub rfc: String,
    pub monto_pendiente: f64,
    pub status: CollectionStatus,
    pub fecha_inicio: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CollectionAction {
    pub action_id: String,
    pub collection_id: String,
    pub credit_id: String,
    pub rfc: String,
    pub tipo: String,
    pub fecha: DateTime<Utc>,
    pub resultado: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CreditApplication {
    pub application_id: String,
    pub rfc: String,
    pub empresa_id: String,
    pub producto_id: String,
    pub monto_solicitado: f64,
    pub status: ApplicationStatus,
    pub fecha_solicitud: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuditLogEntry {
    pub audit_id: u64,
    pub rfc: String,
    pub credit_id: Option<String>,
    pub action_type: String,
    pub details: serde_json::Value,
    pub fecha: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Notification {
    pub notification_id: u64,
    pub rfc: String,
    pub canal: String,
    pub contenido: String,
    pub fecha: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BiSnapshot {
    pub snapshot_id: u64,
    pub empresa_id: String,
    pub fecha: NaiveDate,
    pub metricas: serde_json::Value,
}
