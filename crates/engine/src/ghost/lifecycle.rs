use super::*;

impl GhostLobeManager {
    /// Identify (but do not drop) ghosts whose TTL has elapsed. Caller is
    /// expected to invoke `drop_ghost` for each and orchestrate any
    /// downstream cleanup (router unregistration, telemetry).
    ///
    /// `ttl_seconds == None` means Permanent: always skipped. Never write
    /// `unwrap_or(u64::MAX)` here — `u64::MAX as i64` wraps to -1 and
    /// every permanent ghost would trip the expiration check on the first
    /// tick. Early-bail on `None` is the only safe pattern.
    ///
    /// Clock-skew tolerance: if the system clock jumps backward (NTP
    /// correction, virtualization), `last_accessed` may exceed `now`. The
    /// `saturating_sub().max(0)` combo treats that as "zero time
    /// elapsed," so the ghost stays alive through the anomaly instead of
    /// being expired-by-accident.
    pub fn identify_expired_ghosts(&self) -> Vec<ExpiredGhost> {
        let now = now_micros();
        // Per-shard sequential scan (non-atomic across lobes); TTL is a per-ghost
        // decision, so no cross-lobe consistency is needed.
        let mut expired = Vec::new();
        self.for_each_ghost(|name, meta| {
            let Some(ttl_s) = meta.ttl_seconds() else {
                return;
            };
            let elapsed = now.saturating_sub(meta.last_accessed).max(0);
            let ttl_micros = (ttl_s as i64).saturating_mul(1_000_000);
            if elapsed > ttl_micros {
                expired.push(ExpiredGhost {
                    name: name.to_string(),
                    lobe_id: meta.lobe_id,
                });
            }
        });
        expired
    }

    /// Drop every ghost whose TTL has elapsed. Returns the ghosts that
    /// were successfully removed so the caller can cascade the cleanup
    /// to routers. Ghosts that fail to drop (disk I/O error, missing
    /// keyspace) are logged and skipped; they'll be reconsidered on the
    /// next reaper tick.
    pub fn drop_expired_ghosts(&self, dictionary: &Tree) -> Vec<ExpiredGhost> {
        let candidates = self.identify_expired_ghosts();
        let mut dropped = Vec::with_capacity(candidates.len());
        for eg in candidates {
            match self.drop_ghost(&eg.name, dictionary) {
                Ok(_) => {
                    tracing::info!(
                        "TTL reaper: expired ghost '{}' (lobe={}) dropped",
                        eg.name,
                        eg.lobe_id
                    );
                    dropped.push(eg);
                }
                Err(e) => {
                    tracing::warn!(
                        "TTL reaper: failed to drop expired ghost '{}': {e}",
                        eg.name
                    );
                }
            }
        }
        dropped
    }

    /// Count ghosts of a given lifecycle type that were built from
    /// `source_lobe`. Used by LRU-limit enforcement to decide whether an
    /// eviction is needed before a new ghost of the same type is created.
    pub fn count_by_type(&self, source_lobe: &str, ghost_type: GhostType) -> usize {
        // Ghosts of one `source_lobe` all share a lobe_id → a single shard, so
        // this counts within one shard (the scan skips the others cheaply).
        let mut n = 0;
        self.for_each_ghost(|_, g| {
            if g.source_lobe == source_lobe && g.ghost_type() == ghost_type {
                n += 1;
            }
        });
        n
    }

    /// Return the least-recently-accessed ghost of the given type in the
    /// given lobe, or `None` if no ghost matches. Caller uses this to pick
    /// the victim when enforcing per-type-per-lobe limits.
    ///
    /// Tie-breaking: if multiple ghosts share the same `last_accessed`
    /// (which happens right after boot — the boot-reset sets all timestamps to
    /// `now` in `load_all`), the result depends on `BTreeMap` iteration
    /// order, i.e. lexicographic by ghost name. Deterministic but not
    /// intuitive. In practice ties resolve within seconds as `bump_access`
    /// updates timestamps on subsequent reads, so LRU right after boot is
    /// essentially random — acceptable because the TTL reaper handles the
    /// bulk of cleanup and LRU only fires when a lobe hits its type limit
    /// (20 Ephemeral, 5 Promoted; Ephemeral cap bumped 10 → 20 in v0.2.1
    /// Finding 5).
    pub fn identify_lru(&self, source_lobe: &str, ghost_type: GhostType) -> Option<String> {
        // Same-source_lobe ghosts live in one shard, iterated by name — the
        // strict `<` keeps the FIRST minimum, matching the old `min_by_key` +
        // BTreeMap-name tie-break.
        let mut best: Option<(i64, String)> = None;
        self.for_each_ghost(|name, g| {
            if g.source_lobe == source_lobe && g.ghost_type() == ghost_type {
                let la = g.last_accessed;
                if best.as_ref().is_none_or(|(bla, _)| la < *bla) {
                    best = Some((la, name.to_string()));
                }
            }
        });
        best.map(|(_, name)| name)
    }

    /// If `count_by_type(source_lobe, ghost_type) >= max`, drop the LRU
    /// ghost of that (lobe, type) and return the dropped ghost so the
    /// caller can cascade to router unregistration. If the count is
    /// already below `max`, return `None` without touching anything.
    ///
    /// Strict pre-limit check (count >= max, not count > max): called
    /// BEFORE creating a new ghost, so that creating brings the count to
    /// exactly `max`, never above. Post-create eviction would leave a
    /// brief window of `max + 1` ghosts, and `notify_write` on every
    /// incoming write would pay the cost of that extra ghost during
    /// the window — small but real.
    pub fn evict_lru_at_limit(
        &self,
        source_lobe: &str,
        ghost_type: GhostType,
        max: usize,
        dictionary: &Tree,
    ) -> Option<ExpiredGhost> {
        if self.count_by_type(source_lobe, ghost_type) < max {
            return None;
        }
        let name = self.identify_lru(source_lobe, ghost_type)?;
        let lobe_id = self.with_ghost(&name, |m| m.lobe_id)?;
        match self.drop_ghost(&name, dictionary) {
            Ok(_) => Some(ExpiredGhost { name, lobe_id }),
            Err(e) => {
                tracing::warn!("LRU eviction: failed to drop '{name}': {e}");
                None
            }
        }
    }

    /// Rotate every ghost's `daily_access_bitmap` one bit to the right if
    /// the day bucket advanced since the last rotation. Bit 0 (today)
    /// shifts into bit 1 (yesterday); bit 6 into bit 7 (8 days ago, drops
    /// off on the next rotation). The promotion check reads bits 0-6
    /// to decide "7 consecutive days of access."
    ///
    /// `current_day_bucket` is injected (computed by the caller as
    /// `now_micros() / MICROS_PER_DAY`) so tests can advance the "day"
    /// without waiting 24h — they just pass successive values.
    /// `last_rotation` is mutated to the current bucket when a rotation
    /// fires, so the caller can store it across tick iterations.
    pub fn rotate_bitmaps_if_needed(&self, current_day_bucket: i64, last_rotation: &mut i64) {
        if current_day_bucket == *last_rotation {
            return;
        }
        // Per-shard sequential (non-atomic); each bitmap shift is independent.
        self.for_each_ghost_mut(|meta| {
            // Declared ghosts have no telemetry to rotate.
            if let Some(t) = meta.telemetry_mut() {
                t.daily_access_bitmap >>= 1;
            }
        });
        *last_rotation = current_day_bucket;
    }

    /// Identify Ephemeral ghosts eligible for promotion to Promoted.
    ///
    /// Criterion: `ghost_type == Ephemeral` AND `daily_access_bitmap & 0x7F
    /// == 0x7F`. The mask covers bits 0-6 — "accessed today and each of
    /// the six preceding days," i.e. 7 consecutive days of activity. Bit 7
    /// and higher (older history) are ignored.
    ///
    /// Returns just names; caller (Engine::reap_cycle) looks up the rest
    /// of the meta to orchestrate the promotion + router swap.
    pub fn identify_promotable(&self) -> Vec<String> {
        const SEVEN_DAY_MASK: u32 = 0x7F;
        let mut names = Vec::new();
        self.for_each_ghost(|name, g| {
            if matches!(
                &g.lifecycle,
                GhostLifecycle::Auto { class: AutoClass::Ephemeral, telemetry, .. }
                    if (telemetry.daily_access_bitmap & SEVEN_DAY_MASK) == SEVEN_DAY_MASK
            ) {
                names.push(name.to_string());
            }
        });
        names
    }

    /// Promote an Ephemeral ghost to Promoted in place. The ghost_id and
    /// every index entry in the ghost keyspace stay put — we only rewrite
    /// the runtime map key (`auto_...` → `promoted_...`), the `ghost_type`,
    /// the `ttl_seconds` (24h → 30d), the `name` field on the meta, and
    /// the persisted metadata (keyed by ghost_id, so the dictionary key is
    /// stable). No spatial re-scan, no keyspace rebuild, no window where
    /// the covering index disappears.
    ///
    /// Returns the new name. Errors if the ghost is not found or is not
    /// Ephemeral (Permanent and already-Promoted ghosts are silently
    /// left alone to keep the reaper idempotent).
    pub fn promote_ghost(&self, old_name: &str, dictionary: &Tree) -> Result<String> {
        const PROMOTED_TTL_SECONDS: u64 = 30 * 24 * 60 * 60;

        let new_name = {
            // Rename is the one place a ghost's index KEY changes (its lobe_id
            // does NOT — promotion stays in the same lobe). Hold the index write
            // lock across the whole shard rename (index → shard order) so a
            // concurrent by-name reader sees the ghost under EITHER the old or
            // the new name, never neither/both — atomic w.r.t. index→shard
            // readers (parity with the old single-global-lock rename).
            let mut index = self.ghost_index.write();
            let lobe_id = *index.get(old_name).ok_or_else(|| {
                XyzError::InvalidQuery(format!("promote_ghost: '{old_name}' not found"))
            })?;
            let shard = self.lobe_shard(lobe_id).ok_or_else(|| {
                XyzError::InvalidQuery(format!("promote_ghost: '{old_name}' not found"))
            })?;
            let mut ghosts = shard.write();
            let mut meta = ghosts.remove(old_name).ok_or_else(|| {
                XyzError::InvalidQuery(format!("promote_ghost: '{old_name}' not found"))
            })?;

            if meta.ghost_type() != GhostType::Ephemeral {
                // Put it back and bail — promotion is defined only for
                // Ephemerals; touching anything else is a caller bug. Index
                // untouched (old_name still maps to this lobe).
                ghosts.insert(old_name.to_string(), meta);
                return Err(XyzError::InvalidQuery(format!(
                    "promote_ghost: '{old_name}' is not Ephemeral"
                )));
            }

            let new_name = if let Some(stripped) = old_name.strip_prefix("auto_") {
                format!("promoted_{stripped}")
            } else {
                // Auto-ghosts use the "auto_" prefix. A non-auto Ephemeral
                // is an unexpected state (someone manually reclassified a
                // Permanent to Ephemeral?) — fall back to a suffix so the
                // new name is at least distinct.
                format!("{old_name}_promoted")
            };

            meta.name = new_name.clone();
            // In-place class bump, preserving the accrued telemetry. `meta` is
            // Ephemeral here (checked above) so it is `Auto`.
            if let GhostLifecycle::Auto {
                class, ttl_seconds, ..
            } = &mut meta.lifecycle
            {
                *class = AutoClass::Promoted;
                *ttl_seconds = PROMOTED_TTL_SECONDS;
            }
            // Re-persist while holding the write lock: ghost_id is stable,
            // so the dictionary key stays the same — this overwrites the
            // old Ephemeral meta in place.
            self.persist_meta(&meta, dictionary)?;
            ghosts.insert(new_name.clone(), meta);
            // Re-key the index in the same critical section (same lobe_id).
            index.remove(old_name);
            index.insert(new_name.clone(), lobe_id);
            new_name
        };

        tracing::info!(
            "ghost promoted: '{old_name}' → '{new_name}' (Ephemeral → Promoted, TTL 30d)"
        );
        Ok(new_name)
    }

    /// Bump in-memory access tracking for a ghost routed to by a scan.
    /// Updates `last_accessed`, `access_count_total`, and sets bit 0 of
    /// `daily_access_bitmap` ("accessed today").
    ///
    /// Deliberately does NOT re-persist. At 152 reads/s (v0.1 concurrent
    /// headline), re-persisting on every access would mean ~150 extra
    /// dictionary-keyspace writes per second — all of them latency-path
    /// under group commit, none of them carrying durable-interesting data.
    /// Ephemeral ghosts have 24h TTLs and promotion windows of days; the
    /// correctness cost of losing this state on restart is "the ghost
    /// lives an extra 24h from the reboot" (Ephemeral) or "the 7-day
    /// consecutive-access window restarts" (Promoted). Both acceptable.
    ///
    /// The TTL reaper reads `last_accessed` from in-memory state
    /// only; it never consults the persisted value. `load_all` resets
    /// these three fields at boot for consistency.
    ///
    /// Silent no-op if the ghost was just evicted — the reaper won the
    /// race, which is the correct outcome.
    pub fn bump_access(&self, name: &str) {
        self.with_ghost_mut(name, |meta| {
            meta.last_accessed = now_micros();
            // Only Auto ghosts carry telemetry; Declared ghosts no-op.
            if let Some(t) = meta.telemetry_mut() {
                t.access_count_total = t.access_count_total.saturating_add(1);
                t.daily_access_bitmap |= 1;
            }
        });
    }

    /// Mutate a ghost's lifecycle classification and re-persist the metadata
    /// atomically. Used by the auto-ghost path immediately after
    /// `create()` returns (reclassifies Permanent → Ephemeral + 24h TTL)
    /// and, in later steps, by promotion (Ephemeral → Promoted + 30d TTL).
    ///
    /// Silently returns Ok if the ghost was removed between creation and
    /// this call — that's the LRU / TTL reaper stepping on us, and it's
    /// the correct outcome (don't resurrect a just-evicted ghost).
    pub fn reclassify_lifecycle(
        &self,
        name: &str,
        ghost_type: GhostType,
        ttl_seconds: Option<u64>,
        dictionary: &Tree,
    ) -> Result<()> {
        // Silently Ok if the ghost was evicted between create and this call.
        // Holds only its shard's write lock across the set + persist (parity with
        // the old single-lock behavior; persist is dictionary I/O, not the store).
        self.with_ghost_mut(name, |meta| {
            // Preserve accrued telemetry across a reclassify (Permanent→Ephemeral
            // right after create carries none yet; a re-reclassify keeps it).
            meta.set_lifecycle(ghost_type, ttl_seconds);
            self.persist_meta(meta, dictionary)
        })
        .unwrap_or(Ok(()))
    }

    /// Remove a ghost's on-disk footprint: every index entry in the
    /// ghost keyspace under the `ghost_id` prefix, plus the metadata
    /// record in the dictionary keyspace.
    ///
    /// Does NOT touch the in-memory ghosts map — `drop_ghost` (the
    /// runtime drop path) pops from the map first and then calls this;
    /// `load_all` (the boot purge path) calls this directly on ghosts
    /// that never made it into the map. Keeping the in-memory and
    /// on-disk responsibilities split avoids the map-lookup-then-error
    /// pattern that would otherwise force `load_all` to insert-then-drop.
    ///
    /// Returns errors strictly. Callers that want to tolerate purge
    /// failures (load_all falls through to the insert and lets the
    /// reaper retry within ~60s) should match on the result; callers
    /// that need a hard guarantee (the explicit `DROP GHOST` path)
    /// propagate with `?`.
    pub(super) fn purge_ghost_data(&self, ghost_id: u16, dictionary: &Tree) -> Result<()> {
        let ks = self.ks()?;
        let prefix = ghost_id.to_be_bytes();
        let keys: Vec<Vec<u8>> = ks
            .prefix(&prefix)
            .map_err(|e| XyzError::Storage(e.to_string()))?
            .into_iter()
            .map(|entry| entry.key)
            .collect();
        for key in &keys {
            let _ = ks.remove(key);
        }
        // Lightweight rollups live in the dictionary keyspace under their
        // own namespace — purge those too or DROP leaks them.
        let rollup_keys: Vec<Vec<u8>> = dictionary
            .prefix(&rollup_ghost_prefix(ghost_id))
            .map_err(|e| XyzError::Storage(e.to_string()))?
            .into_iter()
            .map(|entry| entry.key)
            .collect();
        for key in &rollup_keys {
            let _ = dictionary.remove(key);
        }
        // Metric-ordered rollup (ORDER BY <metric>) lives in its own namespace
        // too — purge it or DROP/REFRESH would leak (or serve) a stale order.
        let order_keys: Vec<Vec<u8>> = dictionary
            .prefix(&metric_order::metric_order_ghost_prefix(ghost_id))
            .map_err(|e| XyzError::Storage(e.to_string()))?
            .into_iter()
            .map(|entry| entry.key)
            .collect();
        for key in &order_keys {
            let _ = dictionary.remove(key);
        }
        Self::delete_meta(ghost_id, dictionary)
    }

    /// Drop a Ghost Lobe: pop from the in-memory map, then purge the
    /// on-disk footprint (keyspace entries + dictionary meta) via the
    /// shared `purge_ghost_data` helper. `load_all` uses the same helper
    /// for its boot-time expiry path — one implementation of "remove a
    /// ghost from disk," two entry points.
    pub fn drop_ghost(&self, name: &str, dictionary: &Tree) -> Result<String> {
        let ghost_id = self.remove_ghost_entry(name).map(|m| m.ghost_id);

        match ghost_id {
            Some(id) => {
                self.purge_ghost_data(id, dictionary)?;
                Ok(format!("Ghost '{}' dropped", name))
            }
            None => Err(XyzError::GhostNotFound(name.to_string())),
        }
    }
}
