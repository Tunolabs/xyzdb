use super::*;

impl Engine {
    /// The lobe's sub-gravity (satellite) spec, if declared. The write path will
    /// consult this to decide the `sat` axis of a record's spatial key, mirroring
    /// how [`Self::get_gravity_spec`] feeds placement and [`Self::get_vector_spec`]
    /// feeds the searchable field.
    ///
    /// Declaration-only phase: this accessor exists and returns the persisted
    /// spec, but no write path reads it yet — placement stays at `sat = 0`.
    // Wired but unread until the placement/read phase (Tanda 2) consults it in
    // the PUT/SET/NEAREST/SCAN paths, mirroring get_vector_spec's call sites.
    #[allow(dead_code)]
    pub(crate) fn get_satellite_spec(
        &self,
        lobe: &str,
    ) -> Option<crate::satellite_spec::SatelliteSpec> {
        self.satellite_specs.read().get(lobe).cloned()
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
