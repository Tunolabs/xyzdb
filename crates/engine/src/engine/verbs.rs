use super::*;

impl Engine {
    // ── ANCHOR ────────────────────────────────────────────────────────────

    pub(super) fn execute_anchor(
        &self,
        stmt: xytalk_parser::ast::AnchorStmt,
    ) -> Result<QueryResult> {
        {
            let lobes = self.lobe_registry.read();
            if lobes.get(&stmt.lobe).is_none() {
                return Err(XyzError::LobeNotFound(stmt.lobe.clone()));
            }
        }

        let mut anchors = self.anchor_registry.write();
        anchors.register(&stmt.lobe, &stmt.field)?;
        self.persist_anchor_registry(&anchors)?;

        Ok(QueryResult::Ok {
            lid: None,
            message: format!("Anchor '{}' registered in '{}'", stmt.field, stmt.lobe),
        })
    }

    // ── LOBE ──────────────────────────────────────────────────────────────

    pub(super) fn execute_lobe(&self, stmt: xytalk_parser::ast::LobeStmt) -> Result<QueryResult> {
        let mut lobes = self.lobe_registry.write();
        let id = lobes.create(&stmt.name, stmt.hint)?;
        self.persist_lobe_registry(&lobes)?;

        Ok(QueryResult::Ok {
            lid: None,
            message: format!("Lobe '{}' created (id={})", stmt.name, id),
        })
    }

    // ── SHOW ──────────────────────────────────────────────────────────────

    pub(super) fn execute_show(&self, stmt: xytalk_parser::ast::ShowStmt) -> Result<QueryResult> {
        match stmt {
            xytalk_parser::ast::ShowStmt::Lobes => {
                let lobes = self.lobe_registry.read();
                let lines: Vec<String> = lobes
                    .list()
                    .iter()
                    .map(|l| {
                        let hint = l.hint.as_deref().unwrap_or("");
                        let anchors = self.anchor_registry.read();
                        let anchor_count = anchors.get_anchors(&l.name).len();
                        format!(
                            "  {}. {} ({} anchors){}",
                            l.id,
                            l.name,
                            anchor_count,
                            if hint.is_empty() {
                                String::new()
                            } else {
                                format!(" — {hint}")
                            }
                        )
                    })
                    .collect();
                let mut result = vec!["Lobes:".into()];
                result.extend(lines);
                Ok(QueryResult::Info(result))
            }
            xytalk_parser::ast::ShowStmt::Anchors(lobe) => {
                let anchors = self.anchor_registry.read();
                let set = anchors.get_anchors(&lobe);
                let mut lines = vec![format!("Anchors in '{lobe}':")];
                for a in set {
                    lines.push(format!("  - {a} (UNIQUE)"));
                }
                Ok(QueryResult::Info(lines))
            }
            xytalk_parser::ast::ShowStmt::Throttle => {
                let status = self.throttle.status();
                Ok(QueryResult::Info(vec![format!("{status}")]))
            }
            xytalk_parser::ast::ShowStmt::Ghosts => {
                let ghosts = self.ghost_manager.list();
                if ghosts.is_empty() {
                    return Ok(QueryResult::Info(vec!["No Ghost Lobes.".into()]));
                }
                let mut lines = vec!["Ghost Lobes:".into()];
                for g in &ghosts {
                    // Health markers: aggregates dirtied by a delete (Min/Max,
                    // option D) or an incremental maintenance failure — either
                    // makes the ghost inexact until REFRESH, so it is shown, not
                    // just logged.
                    let mut health = String::new();
                    if g.aggregates_stale {
                        health.push_str(" [aggregates stale — REFRESH to reconcile]");
                    }
                    if g.maintenance_degraded {
                        health.push_str(" [maintenance degraded — REFRESH to rebuild]");
                    }
                    // Metric-ordered rollup (ORDER BY <metric>): show the order and
                    // its freshness (age since last emit), or STALE when declared
                    // but not currently emitted (TOP falls back to O(M)).
                    let order_note = match (&g.metric_order, g.order_age_secs) {
                        (Some((label, desc)), Some(age)) => format!(
                            " [metric-order {label} {} — emitted {age}s ago]",
                            if *desc { "DESC" } else { "ASC" }
                        ),
                        (Some((label, desc)), None) => format!(
                            " [metric-order {label} {} — STALE, not emitted]",
                            if *desc { "DESC" } else { "ASC" }
                        ),
                        (None, _) => String::new(),
                    };
                    lines.push(format!(
                        "  {} — from '{}' order by '{}' ({} records, {} filters){}{}",
                        g.name,
                        g.source_lobe,
                        g.order_by,
                        g.record_count,
                        g.filter_count,
                        order_note,
                        health
                    ));
                }
                Ok(QueryResult::Info(lines))
            }
            xytalk_parser::ast::ShowStmt::ScanStats => {
                let telemetry = self.scan_telemetry.read();
                Ok(QueryResult::Info(telemetry.format_stats()))
            }
            xytalk_parser::ast::ShowStmt::Profile(lobe) => self.execute_show_profile(&lobe),
            xytalk_parser::ast::ShowStmt::Cache => match &self.record_cache {
                Some(cache) => {
                    let stats = cache.stats();
                    let lobes = self.lobe_registry.read();
                    let mut lines = vec![format!(
                        "RecordCache: {:.1}MB / {:.1}MB",
                        stats.used_bytes as f64 / 1_048_576.0,
                        stats.budget_bytes as f64 / 1_048_576.0,
                    )];
                    for info in &stats.lobes {
                        let name = lobes
                            .get_by_id(info.lobe_id)
                            .map(|c| c.name.as_str())
                            .unwrap_or("?");
                        lines.push(format!(
                            "  {}: {} records (~{:.1}MB)",
                            name,
                            info.record_count,
                            info.estimated_bytes as f64 / 1_048_576.0,
                        ));
                    }
                    Ok(QueryResult::Info(lines))
                }
                None => Ok(QueryResult::Info(vec![
                    "RecordCache: disabled (--record-cache-size=0)".into(),
                ])),
            },
        }
    }

    // ── INCACHE / OUTCACHE ────────────────────────────────────────────────

    pub(super) fn execute_incache(
        &self,
        stmt: xytalk_parser::ast::InCacheStmt,
    ) -> Result<QueryResult> {
        let cache = self.record_cache.as_ref().ok_or_else(|| {
            XyzError::InvalidQuery(
                "RecordCache not enabled. Start server with --record-cache-size N".into(),
            )
        })?;

        let lobes = self.lobe_registry.read();
        let lobe_config = lobes
            .get(&stmt.lobe)
            .ok_or_else(|| XyzError::LobeNotFound(stmt.lobe.clone()))?;
        let lobe_id = lobe_config.id;
        drop(lobes);

        // Scan records from spatial (with optional WHERE filter)
        let prefix = lobe_id.to_be_bytes();
        let lobe_name = self.lobe_name_for_id(lobe_id);
        let fr_guard = self.field_registry.read();
        let fd = fr_guard.get_dict(lobe_id);
        let mut records = Vec::new();

        for entry in self
            .spatial_tree()
            .prefix_iter(&prefix)
            .map_err(|e| XyzError::Storage(e.to_string()))?
        {
            let val = &entry.value;
            if let Ok(record) = xyzdb_core::record::deserialize_record(val, &lobe_name, fd)
                && crate::ops::record_matches_opt_expr(&record, &stmt.filter_expr)
            {
                records.push(record);
            }
        }
        drop(fr_guard);

        let count = cache.load_records(lobe_id, records)?;
        Ok(QueryResult::Ok {
            lid: None,
            message: format!("INCACHE: {} records loaded for '{}'", count, stmt.lobe),
        })
    }

    pub(super) fn execute_outcache(&self, lobe_name: &str) -> Result<QueryResult> {
        let cache = self.record_cache.as_ref().ok_or_else(|| {
            XyzError::InvalidQuery(
                "RecordCache not enabled. Start server with --record-cache-size N".into(),
            )
        })?;

        let lobes = self.lobe_registry.read();
        let lobe_config = lobes
            .get(lobe_name)
            .ok_or_else(|| XyzError::LobeNotFound(lobe_name.into()))?;
        let lobe_id = lobe_config.id;
        drop(lobes);

        cache.evict_lobe(lobe_id);
        Ok(QueryResult::Ok {
            lid: None,
            message: format!("OUTCACHE: '{}' evicted", lobe_name),
        })
    }

    // ── PIN / UNPIN / SHOW PROFILE ──────────────────────────────────────

    pub(super) fn execute_pin(&self, stmt: xytalk_parser::ast::PinStmt) -> Result<QueryResult> {
        {
            let lobes = self.lobe_registry.read();
            if lobes.get(&stmt.lobe).is_none() {
                return Err(XyzError::LobeNotFound(stmt.lobe.clone()));
            }
        }

        let mut pins = self.pinned_fields.write();
        let entry = pins.entry(stmt.lobe.clone()).or_default();
        let mut added = Vec::new();
        for field in &stmt.fields {
            if !entry.contains(field) {
                entry.push(field.clone());
                added.push(field.clone());
            }
        }
        let lobe_id = self
            .lobe_registry
            .read()
            .get(&stmt.lobe)
            .map(|l| l.id)
            .unwrap_or(0);
        Self::persist_pinned(&self.turba.dictionary, lobe_id, entry)?;

        Ok(QueryResult::Ok {
            lid: None,
            message: format!(
                "Pinned {} field(s) in '{}': {}",
                added.len(),
                stmt.lobe,
                added.join(", ")
            ),
        })
    }

    pub(super) fn execute_unpin(&self, stmt: xytalk_parser::ast::UnpinStmt) -> Result<QueryResult> {
        {
            let lobes = self.lobe_registry.read();
            if lobes.get(&stmt.lobe).is_none() {
                return Err(XyzError::LobeNotFound(stmt.lobe.clone()));
            }
        }

        let mut pins = self.pinned_fields.write();
        let entry = pins.entry(stmt.lobe.clone()).or_default();
        let before = entry.len();
        entry.retain(|f| !stmt.fields.contains(f));
        let removed = before - entry.len();
        let lobe_id = self
            .lobe_registry
            .read()
            .get(&stmt.lobe)
            .map(|l| l.id)
            .unwrap_or(0);
        Self::persist_pinned(&self.turba.dictionary, lobe_id, entry)?;

        Ok(QueryResult::Ok {
            lid: None,
            message: format!("Unpinned {} field(s) from '{}'", removed, stmt.lobe),
        })
    }

    fn execute_show_profile(&self, lobe: &str) -> Result<QueryResult> {
        let lobes = self.lobe_registry.read();
        if lobes.get(lobe).is_none() {
            return Err(XyzError::LobeNotFound(lobe.to_string()));
        }
        drop(lobes);

        let mut lines = vec![format!("Profile for '{lobe}':")];

        // Pinned fields
        let pins = self.pinned_fields.read();
        let pinned = pins.get(lobe);
        if let Some(fields) = pinned {
            if fields.is_empty() {
                lines.push("  Pinned: (none)".into());
            } else {
                lines.push(format!("  Pinned: {}", fields.join(", ")));
            }
        } else {
            lines.push("  Pinned: (none)".into());
        }
        drop(pins);

        // Searchable vector field (if declared). Additive line; the three
        // states are distinguishable — declared+dim, declared+unknown, none.
        match self.get_vector_spec(lobe) {
            Some(spec) => match spec.dim {
                Some(d) => lines.push(format!("  Vector: {} dim {}", spec.field, d)),
                None => lines.push(format!("  Vector: {} dim unknown", spec.field)),
            },
            None => lines.push("  Vector: (none)".into()),
        }

        // Sub-gravity axis (if declared). Additive line, same three-state shape as
        // Vector above. This is CONTRACT for a caller, not decoration: knowing the
        // axis exists changes which query to emit, because an equality on it is
        // bounded (`kind = X` reads one sub-range) while a range on it is not
        // (`kind < X` sweeps the parent). A caller that cannot see the axis cannot
        // choose the cheap shape, so hiding it hides a capability rather than a
        // detail.
        match self.get_satellite_spec(lobe) {
            Some(spec) => lines.push(format!("  Satellite: {}", spec.field)),
            None => lines.push("  Satellite: (none)".into()),
        }

        // Learned fields from telemetry
        let telemetry = self.scan_telemetry.read();
        let stats = telemetry.format_stats();
        let learned: Vec<String> = stats
            .iter()
            .filter(|s| s.contains(lobe) || s.contains("times"))
            .cloned()
            .collect();
        if learned.is_empty() {
            lines.push("  Learned: (no scan patterns yet)".into());
        } else {
            lines.push("  Learned patterns:".into());
            for l in &learned {
                lines.push(format!("    {l}"));
            }
        }

        // Active ghosts for this lobe
        let ghosts = self.ghost_manager.list();
        let lobe_ghosts: Vec<_> = ghosts.iter().filter(|g| g.source_lobe == lobe).collect();
        if lobe_ghosts.is_empty() {
            lines.push("  Ghosts: (none)".into());
        } else {
            lines.push(format!("  Ghosts: {} active", lobe_ghosts.len()));
            for g in &lobe_ghosts {
                lines.push(format!(
                    "    {} — {} records, {} filters",
                    g.name, g.record_count, g.filter_count
                ));
            }
        }

        Ok(QueryResult::Info(lines))
    }

    /// Get pinned fields for a lobe (used in ghost projection calculation).
    pub fn get_pinned_fields(&self, lobe: &str) -> Vec<String> {
        self.pinned_fields
            .read()
            .get(lobe)
            .cloned()
            .unwrap_or_default()
    }

    fn persist_pinned(
        dictionary: &turba_engine::tree::Tree,
        lobe_id: u16,
        fields: &[String],
    ) -> Result<()> {
        let mut key = Vec::with_capacity(4);
        key.extend_from_slice(&PIN_PREFIX);
        key.extend_from_slice(&lobe_id.to_be_bytes());
        let payload = postcard::to_allocvec(fields)
            .map_err(|e| XyzError::Storage(format!("pin serialize: {e}")))?;
        let mut bytes = Vec::with_capacity(3 + payload.len());
        bytes.extend_from_slice(&xyzdb_core::record::XYZDB_MAGIC);
        bytes.push(0x01);
        bytes.extend_from_slice(&payload);
        dictionary
            .insert(&key, &bytes)
            .map_err(|e| XyzError::Storage(format!("pin persist: {e}")))?;

        // Invariant D1: callers (PIN / UNPIN commands) receive Ok only
        // after the pin metadata is durable. Without the seal+flush,
        // the insert lives in the active memtable and can vanish on
        // crash before the next natural flush.
        dictionary.seal_active();
        dictionary
            .flush_sealed()
            .map_err(|e| XyzError::Storage(format!("pin flush: {e}")))?;
        Ok(())
    }

    pub(super) fn load_pinned_fields(
        dictionary: &turba_engine::tree::Tree,
        lobes: &LobeRegistry,
    ) -> HashMap<String, Vec<String>> {
        let mut result = HashMap::new();
        for (name, config) in lobes.all() {
            let mut key = Vec::with_capacity(4);
            key.extend_from_slice(&PIN_PREFIX);
            key.extend_from_slice(&config.id.to_be_bytes());
            if let Ok(Some(val)) = dictionary.get(&key) {
                let fields_opt: Option<Vec<String>> = if val.len() >= 3
                    && val[0..2] == xyzdb_core::record::XYZDB_MAGIC
                    && val[2] == 0x01
                {
                    postcard::from_bytes(&val[3..])
                        .ok()
                        .or_else(|| bincode::deserialize(&val).ok())
                } else {
                    bincode::deserialize(&val).ok()
                };
                if let Some(fields) = fields_opt
                    && !fields.is_empty()
                {
                    result.insert(name.to_string(), fields);
                }
                continue;
            }

            // Legacy fallback: pins written pre-0.7.6 live under the key
            // that ghost metadata also uses. Accept ONLY pin-shaped values
            // (magic + 0x01) — a ghost meta (0x03) under the same key is
            // not a pin — and migrate to the new prefix so the legacy slot
            // stops being load-bearing. The legacy entry is left in place:
            // deleting it here could erase a ghost meta racing this boot,
            // and a stale pin-shaped value under the old key is inert.
            let mut legacy_key = Vec::with_capacity(4);
            legacy_key.extend_from_slice(&PIN_PREFIX_LEGACY);
            legacy_key.extend_from_slice(&config.id.to_be_bytes());
            if let Ok(Some(val)) = dictionary.get(&legacy_key)
                && val.len() >= 3
                && val[0..2] == xyzdb_core::record::XYZDB_MAGIC
                && val[2] == 0x01
                && let Some(fields) = postcard::from_bytes::<Vec<String>>(&val[3..]).ok()
                && !fields.is_empty()
            {
                if let Err(e) = Self::persist_pinned(dictionary, config.id, &fields) {
                    tracing::warn!("pin migration for lobe '{name}' failed: {e}");
                }
                result.insert(name.to_string(), fields);
            }
        }
        result
    }

    // ── AUTOANCHOR APPLY ──────────────────────────────────────────────────

    pub(super) fn execute_autoanchor_apply(
        &self,
        stmt: xytalk_parser::ast::AutoAnchorApplyStmt,
    ) -> Result<QueryResult> {
        // Verify lobe exists
        let lobes = self.lobe_registry.read();
        if lobes.get(&stmt.lobe).is_none() {
            return Err(XyzError::LobeNotFound(stmt.lobe.clone()));
        }
        drop(lobes);

        // Register the anchor — but only if not already declared.
        // Finding 12 (v0.2.4): `AUTOANCHOR APPLY` is the *populate*
        // operation; its registration step is idempotent by intent
        // (the operator may run it after a bulk load against a
        // schema where `ANCHOR ... UNIQUE IN` already declared the
        // anchor). Unconditional `register` would error here on the
        // duplicate, blocking the populate work that is the whole
        // point of the statement. The declarative `ANCHOR ... UNIQUE
        // IN` path (handled elsewhere) retains its strict semantics
        // — duplicate declarations of the same field still error
        // there.
        let mut anchors = self.anchor_registry.write();
        if !anchors.is_anchor(&stmt.lobe, &stmt.field) {
            anchors.register(&stmt.lobe, &stmt.field)?;
            self.persist_anchor_registry(&anchors)?;
        }
        drop(anchors);

        // Build dictionary entries for all existing records with this field
        let lobes = self.lobe_registry.read();
        let lobe_id = lobes.get(&stmt.lobe).map(|l| l.id).unwrap_or(0);
        drop(lobes);

        let prefix = lobe_id.to_be_bytes();
        let mut indexed = 0u64;
        let mut duplicates = Vec::new();

        for entry in self
            .spatial_tree()
            .prefix_iter(&prefix)
            .map_err(|e| XyzError::Storage(e.to_string()))?
        {
            let val = &entry.value;
            let lobe_name = self.lobe_name_for_id(lobe_id);
            let fr_guard = self.field_registry.read();
            let fd = fr_guard.get_dict(lobe_id);
            if let Ok(record) = xyzdb_core::record::deserialize_record(val, &lobe_name, fd)
                && let Some(field_val) = record.fields.get(&stmt.field)
            {
                let val_str = match field_val {
                    xyzdb_core::value::Value::Text(s) => s.clone(),
                    other => format!("{other}"),
                };
                let dk = crate::anchor::dictionary_key(lobe_id, &stmt.field, &val_str);

                if self.turba.dictionary.get(&dk).ok().flatten().is_some() {
                    duplicates.push(val_str);
                } else {
                    self.turba
                        .dictionary
                        .insert(&dk, &record.lid.to_bytes())
                        .map_err(|e| {
                            XyzError::Storage(format!(
                                "autoanchor apply: dictionary insert failed: {e}"
                            ))
                        })?;
                    indexed += 1;
                }
            }
        }

        // Invariant D1: seal + flush the dictionary before reporting
        // `indexed` to the caller. Without this, the count promises
        // writes that live only in the active memtable until the next
        // natural flush — a crash in that window would lose entries we
        // already acknowledged.
        self.turba.dictionary.seal_active();
        self.turba.dictionary.flush_sealed().map_err(|e| {
            XyzError::Storage(format!("autoanchor apply: flush after insert failed: {e}"))
        })?;

        if !duplicates.is_empty() {
            Ok(QueryResult::Ok {
                lid: None,
                message: format!(
                    "Anchor '{}' applied in '{}': {} indexed, {} duplicates found (first: {:?})",
                    stmt.field,
                    stmt.lobe,
                    indexed,
                    duplicates.len(),
                    duplicates.first()
                ),
            })
        } else {
            Ok(QueryResult::Ok {
                lid: None,
                message: format!(
                    "Anchor '{}' applied in '{}': {} records indexed",
                    stmt.field, stmt.lobe, indexed
                ),
            })
        }
    }
}
