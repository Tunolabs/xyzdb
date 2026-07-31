use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

/// Test-only knobs for the satellite bounded-scan gates. No-ops in production
/// (nothing sets them); re-exported so the satellite test suite can drive them.
///
/// - `SAT_FORCE_PARENT_SCAN` makes [`Engine::detect_satellite_eq`] return `None`,
///   so a query that WOULD take the bounded per-satellite range instead falls
///   back to the parent-bucket scan. The route-equivalence gate runs the same
///   query both ways and asserts identical row *sequences* (a pure optimisation).
/// - `SAT_SKIP_ANTICOLLISION_RESIDUAL` makes the satellite-bounded read paths
///   SKIP the residual filter that drops `hash16` collisions. The collision gate
///   flips it on as a negative control: with the residual gone, an intruder from
///   a colliding value leaks and the test must fail.
pub static SAT_FORCE_PARENT_SCAN: AtomicBool = AtomicBool::new(false);
pub static SAT_SKIP_ANTICOLLISION_RESIDUAL: AtomicBool = AtomicBool::new(false);

impl Engine {
    /// Whether the anti-collision residual is currently active on the satellite
    /// bounded paths (true in production; false only when a test disables it via
    /// `SAT_SKIP_ANTICOLLISION_RESIDUAL`).
    pub(crate) fn satellite_residual_active(&self) -> bool {
        !SAT_SKIP_ANTICOLLISION_RESIDUAL.load(Ordering::Relaxed)
    }

    /// The lobe's sub-gravity (satellite) spec, if declared. Consulted by the
    /// placement helper ([`Self::satellite_sat_for`]) and the bounded-scan
    /// detector ([`Self::detect_satellite_eq`]), mirroring how
    /// [`Self::get_gravity_spec`] / [`Self::get_vector_spec`] feed their paths.
    pub(crate) fn get_satellite_spec(
        &self,
        lobe: &str,
    ) -> Option<crate::satellite_spec::SatelliteSpec> {
        self.satellite_specs.read().get(lobe).cloned()
    }

    /// Detect a satellite-bounded scan: `Some(sat)` when `lobe` declares a
    /// satellite axis AND the query's flattened filters carry an `Eq` on that
    /// field. The read path then scans one satellite sub-range instead of the
    /// whole gravity bucket, keeping the field predicate as an anti-collision
    /// residual. Mirrors [`crate::ops::scan::detect_gravity_eq`] /
    /// `GravitySpec::pinned_gravity_hash`.
    ///
    /// Returns `None` (⇒ parent-bucket scan) when: no satellite axis, the field
    /// is absent from the filters, the op is not `Eq`, the field appears twice,
    /// or the `SAT_FORCE_PARENT_SCAN` test knob is set. Routes the literal value
    /// through the SAME [`Self::satellite_sat_for`] canonicalisation the write
    /// path used, so detection and placement cannot diverge.
    pub(crate) fn detect_satellite_eq(
        &self,
        lobe: &str,
        core_filters: &[(
            String,
            xyzdb_core::record::FilterOp,
            xyzdb_core::value::Value,
        )],
    ) -> Option<u16> {
        use xyzdb_core::record::FilterOp;
        if SAT_FORCE_PARENT_SCAN.load(Ordering::Relaxed) {
            return None;
        }
        let spec = self.get_satellite_spec(lobe)?;
        let mut found: Option<&xyzdb_core::value::Value> = None;
        for (f, op, v) in core_filters {
            if f == &spec.field {
                if !matches!(op, FilterOp::Eq) {
                    return None; // a range/≠ on the satellite field → no bounded scan
                }
                if found.is_some() {
                    return None; // two Eq on the same field → bail conservatively
                }
                found = Some(v);
            }
        }
        let v = found?; // field not equality-constrained → parent scan
        Some(xyzdb_core::key::hash_to_16bits(
            &crate::ops::put::value_to_anchor_string(v),
        ))
    }

    /// The satellite (`sat`) axis for a record of `lobe`, given its fields.
    ///
    /// Returns `None` when the lobe has no declared satellite axis — the caller
    /// then uses [`SpatialKey::new`] (sat 0), so a non-satellite lobe's write
    /// path is byte-for-byte unchanged. Returns `Some(sat)` when declared:
    /// `hash_to_16bits(value_to_anchor_string(field))`, or `Some(0)` when the
    /// field is ABSENT (the default satellite — the shared "dumpster"; a bounded
    /// query still resolves it exactly because the read path applies the field
    /// predicate as a residual).
    ///
    /// Both the write path (placement) and the read path (bounded-scan
    /// detection) MUST route the field value through this one function so they
    /// canonicalise identically — otherwise placement and detection diverge and
    /// the bounded scan silently finds nothing (the gravity keel footgun).
    pub(crate) fn satellite_sat_for(
        &self,
        lobe: &str,
        fields: &std::collections::BTreeMap<String, xyzdb_core::value::Value>,
    ) -> Option<u16> {
        let spec = self.get_satellite_spec(lobe)?;
        let sat = fields
            .get(&spec.field)
            .map(|v| xyzdb_core::key::hash_to_16bits(&crate::ops::put::value_to_anchor_string(v)))
            .unwrap_or(0);
        Some(sat)
    }

    /// Execute `SATELLITE BY <field> IN "lobe"`: declare the lobe's sub-gravity
    /// axis. Must be declared on an EMPTY lobe (§6: no orphan window — records
    /// already placed at `sat = 0` would be invisible to a future bounded query).
    /// Mirrors [`Self::execute_vector`].
    pub(super) fn execute_satellite(
        &self,
        stmt: xytalk_parser::ast::SatelliteStmt,
    ) -> Result<QueryResult> {
        if stmt.field.is_empty() {
            return Err(XyzError::InvalidQuery(
                "SATELLITE BY requires a non-empty field name".to_string(),
            ));
        }
        self.register_satellite_spec(
            &stmt.lobe,
            crate::satellite_spec::SatelliteSpec::new(stmt.field.clone()),
        )?;
        Ok(QueryResult::Ok {
            lid: None,
            message: format!(
                "Satellite axis '{}' declared for '{}'",
                stmt.field, stmt.lobe
            ),
        })
    }

    /// Declare a lobe's satellite spec explicitly. A matching declaration is a
    /// no-op; a different spec on a lobe that already has one errors (one axis
    /// per lobe, §7.1). Declaration on a NON-EMPTY lobe is refused (§6). Persists
    /// before the in-memory insert (D1). Mirrors [`Self::register_vector_spec`].
    fn register_satellite_spec(
        &self,
        lobe: &str,
        spec: crate::satellite_spec::SatelliteSpec,
    ) -> Result<()> {
        let lobe_id = self
            .lobe_registry
            .read()
            .get(lobe)
            .map(|l| l.id)
            .ok_or_else(|| XyzError::LobeNotFound(lobe.to_string()))?;
        let mut s = self.satellite_specs.write();
        if let Some(current) = s.get(lobe) {
            if *current == spec {
                return Ok(());
            }
            return Err(XyzError::InvalidQuery(format!(
                "lobe '{lobe}' already has a satellite axis ({current:?}); one axis per lobe — \
                 changing it would re-bucket existing data (re-packing, a later phase)"
            )));
        }
        // §6: reject on a non-empty lobe. Declaring the axis over live records
        // would leave them at `sat = 0`, where a future bounded (per-satellite)
        // query could not see them — an orphan window. The honest, simplest v1
        // is to require a clean lobe; declared re-packing of existing data is a
        // later path with its own justification. Emptiness = no live record in
        // the spatial keyspace under this lobe's prefix (`prefix_iter` is
        // MVCC-filtered, so tombstones do not count as present).
        let non_empty = self
            .turba
            .spatial
            .prefix_iter(&lobe_id.to_be_bytes())
            .map_err(|e| XyzError::Storage(format!("satellite empty-check: {e}")))?
            .next()
            .is_some();
        if non_empty {
            return Err(XyzError::InvalidQuery(format!(
                "lobe '{lobe}' is not empty; SATELLITE BY must be declared before the first write \
                 — declaring it over existing records would leave them unreachable by a bounded \
                 (per-satellite) query (re-packing existing data is a later phase)"
            )));
        }
        Self::persist_satellite(&self.turba.dictionary, lobe_id, &spec)?;
        s.insert(lobe.to_string(), spec);
        tracing::debug!(lobe, "satellite axis declared");
        Ok(())
    }

    fn persist_satellite(
        dictionary: &turba_engine::tree::Tree,
        lobe_id: u16,
        spec: &crate::satellite_spec::SatelliteSpec,
    ) -> Result<()> {
        let mut key = Vec::with_capacity(4);
        key.extend_from_slice(&crate::reserved_keys::SATELLITE);
        key.extend_from_slice(&lobe_id.to_be_bytes());
        let bytes = spec
            .encode()
            .map_err(|e| XyzError::Storage(format!("satellite serialize: {e}")))?;
        dictionary
            .insert(&key, &bytes)
            .map_err(|e| XyzError::Storage(format!("satellite persist: {e}")))?;

        // D1: callers receive Ok only after the spec is durable. Without
        // seal+flush the insert lives in the active memtable and can vanish on
        // crash. Same pattern as persist_vector / persist_gravity.
        dictionary.seal_active();
        dictionary
            .flush_sealed()
            .map_err(|e| XyzError::Storage(format!("satellite flush: {e}")))?;
        Ok(())
    }

    /// Load every lobe's satellite spec at boot. Mirrors
    /// [`Self::load_vector_fields`] (no migration flag — the slot has one format).
    pub(super) fn load_satellite_fields(
        dictionary: &turba_engine::tree::Tree,
        lobes: &LobeRegistry,
    ) -> HashMap<String, crate::satellite_spec::SatelliteSpec> {
        let mut result = HashMap::new();
        for (name, config) in lobes.all() {
            let mut key = Vec::with_capacity(4);
            key.extend_from_slice(&crate::reserved_keys::SATELLITE);
            key.extend_from_slice(&config.id.to_be_bytes());
            if let Ok(Some(val)) = dictionary.get(&key)
                && let Some(spec) = crate::satellite_spec::SatelliteSpec::decode(&val)
            {
                result.insert(name.to_string(), spec);
            }
        }
        result
    }
}
