use super::*;

/// One line `SHOW PROFILE` can emit.
///
/// It exists so the set is **enumerable and therefore checkable**. `label()` has no
/// wildcard arm, so adding a line does not compile until someone names it, and the
/// test at the bottom of this file then fails until `docs/xytalk-spec.md` §2.19
/// documents it. `Gravity:` was added in 1.1.0 and §2.19 was not updated with it;
/// nothing could have caught that while the labels were bare format strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ProfileLine {
    Gravity,
    Pinned,
    Vector,
    Satellite,
    Learned,
    Ghosts,
}

impl ProfileLine {
    /// The line's prefix, indent included. Exhaustive on purpose.
    fn label(self) -> &'static str {
        match self {
            ProfileLine::Gravity => "  Gravity:",
            ProfileLine::Pinned => "  Pinned:",
            ProfileLine::Vector => "  Vector:",
            ProfileLine::Satellite => "  Satellite:",
            ProfileLine::Learned => "  Learned:",
            ProfileLine::Ghosts => "  Ghosts:",
        }
    }

    /// Every variant. Kept beside `label` so the compiler error that lands there
    /// puts whoever adds a line in front of this list too.
    const ALL: [ProfileLine; Self::COUNT] = [
        ProfileLine::Gravity,
        ProfileLine::Pinned,
        ProfileLine::Vector,
        ProfileLine::Satellite,
        ProfileLine::Learned,
        ProfileLine::Ghosts,
    ];

    /// Number of variants. A new one makes `label` fail to compile; fixing that
    /// lands here, and `ALL` is length-checked against it.
    const COUNT: usize = 6;

    /// `"  Label: (none)"` — the shape every three-state line uses when the lobe
    /// declares nothing.
    fn none(self) -> String {
        format!("{} (none)", self.label())
    }

    /// `"  Label: <value>"`.
    fn with(self, value: impl std::fmt::Display) -> String {
        format!("{} {}", self.label(), value)
    }
}

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

    /// `SHOW PROFILE "lobe"` — every declaration the lobe carries, one line each.
    ///
    /// Line labels come from [`ProfileLine`] rather than being written inline, so
    /// the set is enumerable and a test can hold `docs/xytalk-spec.md` §2.19 to it.
    fn execute_show_profile(&self, lobe: &str) -> Result<QueryResult> {
        let lobes = self.lobe_registry.read();
        if lobes.get(lobe).is_none() {
            return Err(XyzError::LobeNotFound(lobe.to_string()));
        }
        drop(lobes);

        let mut lines = vec![format!("Profile for '{lobe}':")];

        // Gravity axis (if declared). Emitted FIRST because it is the lobe's
        // primary declaration: it decides whether a query is bounded to one bucket
        // or sweeps the whole lobe. It was missing from this profile entirely —
        // `Pinned`, `Vector` and `Satellite` were reported and the axis they hang
        // off was not — so an agent reading a lobe over MCP could discover the
        // satellite and not the thing that makes the satellite mean anything.
        //
        // Same three-state shape as `Vector:` and `Satellite:`, so the parser on
        // the other side reads it the same way.
        match self.get_gravity_spec(lobe) {
            Some(spec) => lines.push(ProfileLine::Gravity.with(spec.fields().join(", "))),
            None => lines.push(ProfileLine::Gravity.none()),
        }

        // Pinned fields
        let pins = self.pinned_fields.read();
        let pinned = pins.get(lobe);
        if let Some(fields) = pinned {
            if fields.is_empty() {
                lines.push(ProfileLine::Pinned.none());
            } else {
                lines.push(ProfileLine::Pinned.with(fields.join(", ")));
            }
        } else {
            lines.push(ProfileLine::Pinned.none());
        }
        drop(pins);

        // Searchable vector field (if declared). Additive line; the three
        // states are distinguishable — declared+dim, declared+unknown, none.
        match self.get_vector_spec(lobe) {
            Some(spec) => match spec.dim {
                Some(d) => {
                    lines.push(ProfileLine::Vector.with(format!("{} dim {}", spec.field, d)))
                }
                None => lines.push(ProfileLine::Vector.with(format!("{} dim unknown", spec.field))),
            },
            None => lines.push(ProfileLine::Vector.none()),
        }

        // Sub-gravity axis (if declared). Additive line, same three-state shape as
        // Vector above. This is CONTRACT for a caller, not decoration: knowing the
        // axis exists changes which query to emit, because an equality on it is
        // bounded (`kind = X` reads one sub-range) while a range on it is not
        // (`kind < X` sweeps the parent). A caller that cannot see the axis cannot
        // choose the cheap shape, so hiding it hides a capability rather than a
        // detail.
        match self.get_satellite_spec(lobe) {
            Some(spec) => lines.push(ProfileLine::Satellite.with(spec.field)),
            None => lines.push(ProfileLine::Satellite.none()),
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
            lines.push(ProfileLine::Learned.with("(no scan patterns yet)"));
        } else {
            // Same label as the empty case: a caller parsing this block should not
            // have to know two spellings of one line.
            lines.push(ProfileLine::Learned.with(format!("{} pattern(s)", learned.len())));
            for l in &learned {
                lines.push(format!("    {l}"));
            }
        }

        // Active ghosts for this lobe
        let ghosts = self.ghost_manager.list();
        let lobe_ghosts: Vec<_> = ghosts.iter().filter(|g| g.source_lobe == lobe).collect();
        if lobe_ghosts.is_empty() {
            lines.push(ProfileLine::Ghosts.none());
        } else {
            lines.push(ProfileLine::Ghosts.with(format!("{} active", lobe_ghosts.len())));
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
        // PREFIX SCAN, not one point-get per lobe. A point lookup is bloom-gated,
        // and a post-recovery SSTable can carry a bloom that disagrees with its data
        // (KNOWN-ISSUES.md). Measured with a forged bloom: the previous loader
        // brought a lobe up reporting `Pinned: (none)` — the declaration was gone.
        //
        // The UNIQUE constraint itself survived that, but for a different reason:
        // the write path's duplicate check confirms a miss WITHOUT the bloom
        // (`ops/put.rs`). That shield was added for the false negative in the
        // duplicate check, and it happens to also cover the lost declaration on
        // that one route. **That is architectural luck, not design** — anyone
        // "simplifying" it on the belief that it only covers the duplicate-anchor
        // case would silently remove the second half.
        //
        // A range scan never consults the bloom, so the declaration stops depending
        // on either. Same MVCC snapshot as a point read, tombstones excluded.
        let by_id: HashMap<u16, &str> = lobes.all().map(|(name, c)| (c.id, name)).collect();
        let mut result = HashMap::new();

        // Value shape decides, never the key: a ghost meta (0x03) lives under the
        // legacy prefix too, so accepting by position would read one as a pin list.
        let as_pin_fields = |val: &[u8], allow_bincode: bool| -> Option<Vec<String>> {
            if val.len() >= 3 && val[0..2] == xyzdb_core::record::XYZDB_MAGIC && val[2] == 0x01 {
                postcard::from_bytes(&val[3..]).ok().or_else(|| {
                    if allow_bincode {
                        bincode::deserialize(val).ok()
                    } else {
                        None
                    }
                })
            } else if allow_bincode {
                bincode::deserialize(val).ok()
            } else {
                None
            }
        };

        let lookup = |prefix: &[u8], allow_bincode: bool| -> Vec<(u16, Vec<String>)> {
            let Ok(entries) = dictionary.prefix_iter(prefix) else {
                return Vec::new();
            };
            let plen = prefix.len();
            entries
                .filter_map(|e| {
                    let id = <[u8; 2]>::try_from(e.key.get(plen..)?).ok()?;
                    let fields = as_pin_fields(&e.value, allow_bincode)?;
                    (!fields.is_empty()).then(|| (u16::from_be_bytes(id), fields))
                })
                .collect()
        };

        for (id, fields) in lookup(&PIN_PREFIX, true) {
            if let Some(name) = by_id.get(&id) {
                result.insert((*name).to_string(), fields);
            }
        }

        // Legacy fallback: pins written pre-0.7.6 live under the key ghost metadata
        // also uses, so only pin-shaped values (magic + 0x01) count — `allow_bincode
        // = false` keeps a bare bincode blob from being read as a pin there. Migrate
        // to the new prefix so the legacy slot stops being load-bearing; the legacy
        // entry is LEFT in place, because deleting it could erase a ghost meta
        // racing this boot, and a stale pin-shaped value under the old key is inert.
        for (id, fields) in lookup(&PIN_PREFIX_LEGACY, false) {
            let Some(name) = by_id.get(&id) else { continue };
            if result.contains_key(*name) {
                continue; // the current prefix already answered for this lobe
            }
            if let Err(e) = Self::persist_pinned(dictionary, id, &fields) {
                tracing::warn!("pin migration for lobe '{name}' failed: {e}");
            }
            result.insert((*name).to_string(), fields);
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

#[cfg(test)]
mod profile_line_docs {
    //! `SHOW PROFILE`'s line set versus what §2.19 of the spec documents.
    //!
    //! The failure this closes: `Gravity:` was added to the profile in 1.1.0 and
    //! §2.19 kept describing the old four lines. Nothing could catch it — the
    //! labels were inline format strings, so there was no set to compare against.
    //! Now there is one, `label()` has no wildcard arm, and a new line fails to
    //! compile until it is named and documented.

    use super::ProfileLine;

    /// The spec, read from the repo rather than duplicated here: a copy would go
    /// stale in exactly the way this test exists to prevent.
    fn spec() -> String {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/xytalk-spec.md");
        std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("cannot read the spec at {path}: {e}"))
    }

    #[test]
    fn all_covers_every_variant() {
        let distinct: std::collections::HashSet<_> = ProfileLine::ALL.iter().collect();
        assert_eq!(
            distinct.len(),
            ProfileLine::COUNT,
            "ProfileLine::ALL must list every variant exactly once"
        );
    }

    #[test]
    fn every_emitted_line_is_documented() {
        let spec = spec();
        for line in ProfileLine::ALL {
            // The label carries its indent; the spec quotes it inside a fenced
            // block, so match on the trimmed label plus its colon.
            let needle = line.label().trim();
            assert!(
                spec.contains(needle),
                "SHOW PROFILE emits `{needle}` and docs/xytalk-spec.md never mentions \
                 it — document the line in §2.19 or stop emitting it"
            );
        }
    }

    /// Negative control. Without it, a `contains` that matched everything would
    /// look exactly like a healthy gate.
    #[test]
    fn the_check_can_fail() {
        assert!(
            !spec().contains("Sublimation:"),
            "control string leaked into the spec; pick another"
        );
    }
}
