use super::*;

impl Engine {
    /// The lobe's searchable-vector spec, if declared. The PUT path consults
    /// this to decide which embedding field to hoist to the V3 record prefix,
    /// mirroring how [`Self::get_gravity_spec`] feeds placement.
    pub(crate) fn get_vector_spec(&self, lobe: &str) -> Option<crate::vector_spec::VectorSpec> {
        self.vector_fields.read().get(lobe).cloned()
    }

    /// Execute `VECTOR <field> IN "lobe"`: declare the lobe's searchable vector
    /// field. Must be declared before the lobe has a vector spec (before the
    /// first write that would hoist an embedding). Mirrors [`Self::execute_gravity`].
    pub(super) fn execute_vector(
        &self,
        stmt: xytalk_parser::ast::VectorStmt,
    ) -> Result<QueryResult> {
        if stmt.field.is_empty() {
            return Err(XyzError::InvalidQuery(
                "VECTOR requires a non-empty field name".to_string(),
            ));
        }
        self.register_vector_spec(
            &stmt.lobe,
            crate::vector_spec::VectorSpec::new(stmt.field.clone()),
        )?;
        Ok(QueryResult::Ok {
            lid: None,
            message: format!("Vector field '{}' declared for '{}'", stmt.field, stmt.lobe),
        })
    }

    /// Declare a lobe's searchable vector spec explicitly. A matching
    /// declaration is a no-op; a different spec on a lobe that already has one
    /// errors (declare before the first write). Persists before the in-memory
    /// insert (D1). Mirrors [`Self::register_gravity_spec`].
    fn register_vector_spec(&self, lobe: &str, spec: crate::vector_spec::VectorSpec) -> Result<()> {
        let lobe_id = self
            .lobe_registry
            .read()
            .get(lobe)
            .map(|l| l.id)
            .ok_or_else(|| XyzError::LobeNotFound(lobe.to_string()))?;
        let mut v = self.vector_fields.write();
        if let Some(current) = v.get(lobe) {
            if *current == spec {
                return Ok(());
            }
            return Err(XyzError::InvalidQuery(format!(
                "lobe '{lobe}' already has a vector field ({current:?}); VECTOR must be \
                 declared before the first write — changing the hoisted field would leave \
                 existing V3 records with a stale prefix (a later re-write phase)"
            )));
        }
        Self::persist_vector(&self.turba.dictionary, lobe_id, &spec)?;
        v.insert(lobe.to_string(), spec);
        tracing::debug!(lobe, "vector field declared");
        Ok(())
    }

    fn persist_vector(
        dictionary: &turba_engine::tree::Tree,
        lobe_id: u16,
        spec: &crate::vector_spec::VectorSpec,
    ) -> Result<()> {
        let mut key = Vec::with_capacity(4);
        key.extend_from_slice(&crate::reserved_keys::VECTOR_FIELD);
        key.extend_from_slice(&lobe_id.to_be_bytes());
        let bytes = spec
            .encode()
            .map_err(|e| XyzError::Storage(format!("vector serialize: {e}")))?;
        dictionary
            .insert(&key, &bytes)
            .map_err(|e| XyzError::Storage(format!("vector persist: {e}")))?;

        // D1: callers receive Ok only after the spec is durable. Without
        // seal+flush the insert lives in the active memtable and can vanish on
        // crash. Same pattern as persist_gravity.
        dictionary.seal_active();
        dictionary
            .flush_sealed()
            .map_err(|e| XyzError::Storage(format!("vector flush: {e}")))?;
        Ok(())
    }

    /// Validate a hoisted vector's dimension against the lobe's [`VectorSpec`],
    /// LEARNING (and persisting) the dimension from the first embedding written to
    /// the field and enforcing it thereafter. A mismatch is a hard error, not a
    /// silent skip: at query time `as_vector` drops a mismatched-dimension vector
    /// from every top-k with no signal (a wrong-model embedding never surfaces),
    /// so the write is refused at ingest instead. No-op when the lobe has no
    /// vector spec. Flexibility is preserved — each field learns whatever
    /// dimension its first vector has; only mixing dimensions within one field is
    /// closed (cosine across mismatched dimensions is meaningless).
    ///
    /// # Errors
    /// [`XyzError::InvalidQuery`] when `dim` differs from the field's learned
    /// dimension.
    pub(crate) fn ensure_vector_dim(&self, lobe: &str, field: &str, dim: usize) -> Result<()> {
        let mismatch = |expected: u32| {
            XyzError::InvalidQuery(format!(
                "vector field '{field}' in lobe '{lobe}' has dimension {dim}, but the field is \
                 {expected}-dimensional — a mismatched embedding (wrong model?) would be silently \
                 unsearchable, so the write is rejected; use the same model as the rest of the field"
            ))
        };
        // Fast path: dimension already learned — a read-lock and a compare.
        {
            let specs = self.vector_fields.read();
            match specs.get(lobe).map(|s| s.dim) {
                Some(Some(d)) if d as usize == dim => return Ok(()),
                Some(Some(d)) => return Err(mismatch(d)),
                Some(None) => {} // spec present, dim not yet learned — fall through
                None => return Ok(()), // no vector spec on this lobe — nothing to enforce
            }
        }
        // Learn path (first embedding): write-lock, re-check under it (a concurrent
        // PUT may have won the race), then set + persist the dimension durably.
        let lobe_id = self
            .lobe_registry
            .read()
            .get(lobe)
            .map(|l| l.id)
            .ok_or_else(|| XyzError::LobeNotFound(lobe.to_string()))?;
        let mut specs = self.vector_fields.write();
        let Some(spec) = specs.get_mut(lobe) else {
            return Ok(());
        };
        match spec.dim {
            Some(d) if d as usize == dim => Ok(()),
            Some(d) => Err(mismatch(d)),
            None => {
                spec.dim = Some(dim as u32);
                let updated = spec.clone();
                Self::persist_vector(&self.turba.dictionary, lobe_id, &updated)
            }
        }
    }

    /// Load every lobe's searchable vector spec. Mirrors
    /// [`Self::load_gravity_fields`] (no migration flag — the slot has one format).
    pub(super) fn load_vector_fields(
        dictionary: &turba_engine::tree::Tree,
        lobes: &LobeRegistry,
    ) -> HashMap<String, crate::vector_spec::VectorSpec> {
        let mut result = HashMap::new();
        // PREFIX SCAN, not one point-get per lobe: a point lookup is bloom-gated,
        // and a miss here is indistinguishable from "not declared", so a bloom
        // false negative after an unclean restart would bring the lobe up
        // without its axis. A range scan never consults the bloom, and applies
        // the same MVCC snapshot with tombstones excluded. See KNOWN-ISSUES.md.
        let by_id: std::collections::HashMap<u16, &str> =
            lobes.all().map(|(name, c)| (c.id, name)).collect();
        let Ok(entries) = dictionary.prefix_iter(&crate::reserved_keys::VECTOR_FIELD) else {
            return result;
        };
        for entry in entries {
            let Some(id_bytes) = entry.key.get(crate::reserved_keys::VECTOR_FIELD.len()..) else {
                continue;
            };
            let Ok(id_arr) = <[u8; 2]>::try_from(id_bytes) else {
                continue;
            };
            let Some(name) = by_id.get(&u16::from_be_bytes(id_arr)) else {
                continue;
            };
            if let Some(spec) = crate::vector_spec::VectorSpec::decode(&entry.value) {
                result.insert((*name).to_string(), spec);
            }
        }
        result
    }
}
