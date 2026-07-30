/// Unified error type for xyzDB.
#[derive(Debug, thiserror::Error)]
pub enum XyzError {
    #[error("Parse error: {0}")]
    Parse(String),

    #[error("Lobe '{0}' not found")]
    LobeNotFound(String),

    #[error("Duplicate anchor '{anchor}' = '{value}' in lobe '{lobe}'. Existing: {existing_lid}")]
    DuplicateAnchor {
        anchor: String,
        value: String,
        lobe: String,
        existing_lid: String,
    },

    #[error("Record not found: {0}")]
    RecordNotFound(String),

    #[error("Storage error: {0}")]
    Storage(String),

    #[error("Type error: expected {expected}, got {got}")]
    TypeError { expected: String, got: String },

    #[error("Invalid query: {0}")]
    InvalidQuery(String),

    /// A scan routed to a ghost whose entry in the manager had been dropped
    /// (LRU eviction, TTL expiry, manual `DROP GHOST`) between the router's
    /// selection and the read path's lookup. Distinct variant so
    /// `ops/scan.rs` can catch it specifically and transparently fall back
    /// to the Primary keyspace. Treating this as `InvalidQuery` would be
    /// either silently wrong (user sees a failure for a ghost they never
    /// requested) or overly broad (hides legitimate query errors).
    #[error("Ghost '{0}' not found")]
    GhostNotFound(String),

    /// `GhostLobeManager::create` / `create_batch` was asked to build a ghost
    /// whose hashed name already exists in the registry. Distinct variant so
    /// the auto-promotion path in `Engine::maybe_create_ephemeral_ghost` can
    /// pattern-match the duplicate-loser case and account for it in
    /// `ghost_dedup_lost_count` without parsing the error message string —
    /// the v0.3.2-ghost-singleflight cycle relies on this for
    /// pre-fix vs post-fix counter delta validation.
    #[error("Ghost '{0}' already exists")]
    GhostExists(String),

    /// A `NEAREST` bucket scan ran past its time budget (`--nearest-budget-ms`)
    /// and was aborted. This is the explicit, actionable failure that replaces a
    /// silent multi-second hang once `NEAREST` is decoupled from the SCAN cap: the
    /// caller learns the bucket is too large for the budget and can raise the
    /// budget or narrow the gravity predicate, rather than seeing degraded recall
    /// with no signal.
    #[error(
        "NEAREST exceeded its {budget_ms}ms budget after scanning {scanned} candidates; \
         raise --nearest-budget-ms or narrow the gravity bucket"
    )]
    NearestBudgetExceeded { scanned: usize, budget_ms: u64 },

    #[error("Internal error: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, XyzError>;
