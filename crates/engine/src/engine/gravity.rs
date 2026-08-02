use super::*;
use std::sync::atomic::Ordering::Relaxed;

/// #11 minimum gravity-declared PUTs before the omit-ratio warn can fire, so a
/// single early omit in a warming lobe never warns at a meaningless 100% ratio.
const KEEL_WARN_MIN_SAMPLE: u64 = 100;

impl Engine {
    /// Get the single gravity field name for a lobe, if its spec is `Raw`.
    /// Transitional accessor for call sites not yet migrated to the spec;
    /// returns `None` for multi-field / transformed specs (those route through
    /// [`Self::get_gravity_spec`]).
    pub fn get_gravity_field(&self, lobe: &str) -> Option<String> {
        match self.gravity_specs.read().get(lobe) {
            Some(GravitySpec::Raw(f)) => Some(f.clone()),
            _ => None,
        }
    }

    /// The lobe's gravity spec, if registered. The write path resolves the
    /// placement hash through this so it matches the SCAN fast path.
    pub(crate) fn get_gravity_spec(&self, lobe: &str) -> Option<GravitySpec> {
        self.gravity_specs.read().get(lobe).cloned()
    }

    /// #11 — record whether a PUT to a gravity-declared `lobe` carried the
    /// declared keel (`present`) or omitted it (`!present`, case C). Additive:
    /// placement already resolved via [`GravitySpec::compute_hash`]; this only
    /// counts the outcome and warns once when the omit ratio first crosses
    /// [`Self::keel_omit_warn_ratio`]. Call ONLY when a spec is declared, so the
    /// denominator is gravity-declared PUTs (not diluted by non-gravity lobes).
    pub(crate) fn observe_keel(&self, lobe: &str, spec: &GravitySpec, present: bool) {
        // Fast path: entry exists → bump under a read lock (fields are atomics).
        {
            let map = self.keel_health.read();
            if let Some(c) = map.get(lobe) {
                self.bump_keel(c, lobe, spec, present);
                return;
            }
        }
        // Slow path: first observation for this lobe.
        let mut map = self.keel_health.write();
        let c = map.entry(lobe.to_string()).or_default();
        self.bump_keel(c, lobe, spec, present);
    }

    /// Increment the counter and warn once if the omit ratio crosses the
    /// threshold. Reads atomics only, so a shared `&` serves both lock paths.
    fn bump_keel(&self, c: &KeelHealthCounters, lobe: &str, spec: &GravitySpec, present: bool) {
        if present {
            c.present.fetch_add(1, Relaxed);
        } else {
            c.absent.fetch_add(1, Relaxed);
        }
        let absent = c.absent.load(Relaxed);
        let present_n = c.present.load(Relaxed);
        let total = present_n + absent;
        if total < KEEL_WARN_MIN_SAMPLE || c.warned.load(Relaxed) {
            return;
        }
        let ratio = absent as f64 / total as f64;
        // swap latches the warn to exactly once, even under concurrent PUTs.
        if ratio >= self.keel_omit_warn_ratio && !c.warned.swap(true, Relaxed) {
            tracing::warn!(
                lobe,
                field = %spec.fields().join(","),
                keel_present = present_n,
                keel_absent = absent,
                omit_ratio = ratio,
                threshold = self.keel_omit_warn_ratio,
                "gravity keel omitted above threshold: scoped queries will silently \
                 under-recall these records (not co-located). The writer is dropping \
                 the declared gravity field."
            );
        }
    }

    /// #11 — snapshot the per-lobe keel-omit health for [`crate::stats`].
    pub(crate) fn keel_health_entries(&self) -> Vec<crate::stats::KeelHealthEntry> {
        let map = self.keel_health.read();
        let mut out: Vec<crate::stats::KeelHealthEntry> = map
            .iter()
            .map(|(lobe, c)| {
                let keel_present = c.present.load(Relaxed);
                let keel_absent = c.absent.load(Relaxed);
                let total = keel_present + keel_absent;
                crate::stats::KeelHealthEntry {
                    lobe: lobe.clone(),
                    keel_present,
                    keel_absent,
                    omit_ratio: if total == 0 {
                        0.0
                    } else {
                        keel_absent as f64 / total as f64
                    },
                }
            })
            .collect();
        out.sort_by(|a, b| a.lobe.cmp(&b.lobe)); // deterministic stats order
        out
    }

    /// Register the gravity field for a lobe (idempotent first-observed).
    /// Called by the PUT path on every record carrying a `*field` marker.
    /// First call with a given (lobe, field) records it durably; subsequent
    /// calls with the same field are a no-op; calls with a different field
    /// log a warn and keep the original (lenient consistency, avoids
    /// breaking workloads where gravity convention drifts).
    pub(crate) fn register_gravity_field(&self, lobe: &str, field: &str) -> Result<()> {
        // Fast path: read-only check first to avoid taking the write lock
        // on every record (PUT batch hot path).
        {
            let existing = self.gravity_specs.read();
            if let Some(current) = existing.get(lobe) {
                // A bare `*field` PUT registers `Raw(field)`; agreeing with the
                // existing spec is a no-op. A different registered field (or a
                // declared non-Raw spec) keeps the registered one, lenient.
                if *current == GravitySpec::Raw(field.to_string()) {
                    return Ok(());
                }
                tracing::warn!(
                    lobe,
                    registered = ?current,
                    incoming = %field,
                    "gravity field mismatch on PUT; keeping registered"
                );
                return Ok(());
            }
        }
        // Take write lock and re-check (another thread may have raced us).
        let mut g = self.gravity_specs.write();
        if g.get(lobe).is_some() {
            return Ok(());
        }
        // Persist before inserting in-memory so a crash before persist
        // doesn't leave the registry inconsistent across restart. Same
        // D1 discipline as `persist_pinned`.
        let lobes = self.lobe_registry.read();
        let lobe_id = lobes
            .get(lobe)
            .map(|l| l.id)
            .ok_or_else(|| XyzError::LobeNotFound(lobe.to_string()))?;
        drop(lobes);
        let spec = GravitySpec::Raw(field.to_string());
        Self::persist_gravity(&self.turba.dictionary, lobe_id, &spec)?;
        g.insert(lobe.to_string(), spec);
        tracing::debug!(lobe, field, "gravity field registered");
        Ok(())
    }

    /// Execute `GRAVITY BY <expr> IN "lobe"`: declare the lobe's gravity spec.
    /// Must be declared before the lobe has a gravity spec (before the first
    /// `*` PUT or a prior `GRAVITY BY`). Changing the spec of a lobe that
    /// already has gravity data re-buckets it — that is re-gravitation (a later
    /// phase), not this statement, so it errors here.
    pub(super) fn execute_gravity(
        &self,
        stmt: xytalk_parser::ast::GravityStmt,
    ) -> Result<QueryResult> {
        let spec = Self::convert_gravity_spec(stmt.spec);
        self.register_gravity_spec(&stmt.lobe, spec)?;
        Ok(QueryResult::Ok {
            lid: None,
            message: format!("Gravity spec declared for '{}'", stmt.lobe),
        })
    }

    /// Map the parser's surface spec (`GravitySpecAst`) to the engine
    /// [`GravitySpec`]. Same role as `convert_filter_op` for filters.
    fn convert_gravity_spec(ast: xytalk_parser::ast::GravitySpecAst) -> GravitySpec {
        use crate::gravity_spec::Transform;
        use xytalk_parser::ast::{GravitySpecAst, GravityTransform};
        match ast {
            GravitySpecAst::Raw(f) => GravitySpec::Raw(f),
            GravitySpecAst::Normalized(f, t) => GravitySpec::Normalized(
                f,
                match t {
                    GravityTransform::Lower => Transform::Lower,
                    GravityTransform::Trim => Transform::Trim,
                },
            ),
            GravitySpecAst::Composite(fs) => GravitySpec::Composite(fs),
        }
    }

    /// Declare a lobe's gravity spec explicitly. A matching declaration is a
    /// no-op; a different spec on a lobe that already has one errors (declare
    /// before the first write). Persists before the in-memory insert (D1).
    fn register_gravity_spec(&self, lobe: &str, spec: GravitySpec) -> Result<()> {
        let lobe_id = self
            .lobe_registry
            .read()
            .get(lobe)
            .map(|l| l.id)
            .ok_or_else(|| XyzError::LobeNotFound(lobe.to_string()))?;
        let mut g = self.gravity_specs.write();
        if let Some(current) = g.get(lobe) {
            if *current == spec {
                return Ok(());
            }
            return Err(XyzError::InvalidQuery(format!(
                "lobe '{lobe}' already has a gravity spec ({current:?}); GRAVITY BY must be \
                 declared before the first write — changing it would re-bucket existing data \
                 (re-gravitation, a later phase)"
            )));
        }
        Self::persist_gravity(&self.turba.dictionary, lobe_id, &spec)?;
        g.insert(lobe.to_string(), spec);
        tracing::debug!(lobe, "gravity spec declared");
        Ok(())
    }

    pub(super) fn persist_gravity(
        dictionary: &turba_engine::tree::Tree,
        lobe_id: u16,
        spec: &GravitySpec,
    ) -> Result<()> {
        let mut key = Vec::with_capacity(4);
        key.extend_from_slice(&GRAVITY_PREFIX);
        key.extend_from_slice(&lobe_id.to_be_bytes());
        // GravitySpec::encode writes `[MAGIC][0x02][postcard]`; pre-0.8 wrote
        // `[MAGIC][0x01][postcard(String)]`, which decode() still reads as Raw.
        let bytes = spec
            .encode()
            .map_err(|e| XyzError::Storage(format!("gravity serialize: {e}")))?;
        dictionary
            .insert(&key, &bytes)
            .map_err(|e| XyzError::Storage(format!("gravity persist: {e}")))?;

        // D1: callers (PUT path) receive Ok only after gravity metadata is
        // durable. Without seal+flush the insert lives in active memtable
        // and can vanish on crash. Same pattern as persist_pinned.
        dictionary.seal_active();
        dictionary
            .flush_sealed()
            .map_err(|e| XyzError::Storage(format!("gravity flush: {e}")))?;
        Ok(())
    }

    /// Load every lobe's gravity spec. Returns the specs plus a flag that is
    /// `true` when any slot is pre-D1 (name+value, format 0x01/0x02) — i.e. the
    /// database holds `*`-placed records that still need a `migrate` rehash.
    pub(super) fn load_gravity_fields(
        dictionary: &turba_engine::tree::Tree,
        lobes: &LobeRegistry,
    ) -> (HashMap<String, GravitySpec>, bool) {
        // PREFIX SCAN, not one point-get per lobe. A point lookup is bloom-gated,
        // and a post-recovery SSTable can carry a bloom that disagrees with its data
        // (see KNOWN-ISSUES.md — root not diagnosed). Here the miss branch is
        // indistinguishable from "no gravity declared", so a false negative would
        // bring the lobe up WITHOUT its axis: no placement, and new writes landing
        // outside the bucket the declaration promised.
        //
        // A range scan does not consult the bloom at all — the filter answers point
        // questions — so this removes the exposure instead of learning to distrust
        // it. It also replaces N lookups with one pass. `prefix_iter` applies the
        // same MVCC snapshot as `get` and excludes tombstones (verified in
        // `tree/mod.rs`), so a retired declaration stays retired.
        let by_id: HashMap<u16, &str> = lobes.all().map(|(name, c)| (c.id, name)).collect();
        let mut result = HashMap::new();
        let mut needs_migration = false;
        let Ok(entries) = dictionary.prefix_iter(&GRAVITY_PREFIX) else {
            return (result, needs_migration);
        };
        for entry in entries {
            let Some(id_bytes) = entry.key.get(GRAVITY_PREFIX.len()..) else {
                continue;
            };
            let Ok(id_arr) = <[u8; 2]>::try_from(id_bytes) else {
                continue; // not a lobe-id slot under this prefix
            };
            let Some(name) = by_id.get(&u16::from_be_bytes(id_arr)) else {
                continue; // declaration for a lobe that no longer exists
            };
            // decode handles all three slot formats (0x03 value-only, 0x02 Fase-0,
            // 0x01 bare field name → Raw); slot_is_pre_d1 flags the older two.
            if let Some(spec) = GravitySpec::decode(&entry.value) {
                if GravitySpec::slot_is_pre_d1(&entry.value) {
                    needs_migration = true;
                }
                result.insert((*name).to_string(), spec);
            }
        }
        (result, needs_migration)
    }
}
