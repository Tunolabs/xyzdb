use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use xytalk_parser::ast::FilterExpr;
use xyzdb_core::record::FilterOp;
use xyzdb_core::value::Value;

/// Where to read from.
#[derive(Debug, Clone)]
pub enum ScanSource {
    Primary,
    /// Ghost with sort_key references — supports ORDER BY + LIMIT via prefix_iter
    Ghost(String),
    /// Pre-computed aggregates/groups — zero scan, microsecond response
    GhostPreComputed(String),
}

/// Metadata for a ghost registered in the router.
///
/// `filter_fields` carries the operator alongside field+value so non-`Eq`
/// filters (Gt, Lt, Contains, …) can route to their matching ghost — prior
/// to v0.2 the router matched field+value only and silently treated every
/// filter as `Eq`, so ghosts built for Q6/Q7/Q9 never fired.
///
/// `filter_desc` is an optional canonicalized string representation of the
/// original filter expression. Used for matching OR / complex expressions
/// that don't flatten into an AND-of-comparisons tuple. Set by callers that
/// create a ghost from a specific query (auto-ghost path in Phase 2+); left
/// `None` for ghosts registered at boot from persisted flat-AND filters.
struct GhostRoutingMeta {
    /// Flat-AND coverage tuples (empty for a non-flat ghost). Drives the
    /// AND-subset slow path.
    filter_fields: Vec<(String, FilterOp, Value)>,
    /// The ghost's full membership expression. An OR/NOT ghost routes ONLY by
    /// structural equality against the query's expression (its `filter_fields`
    /// are empty and must not vacuously match). Replaces the old `filter_desc`
    /// string fast path for OR/NOT.
    filter: FilterExpr,
    /// Auto-ghost PATTERN identity (`format!("{:?}", query_filter_expr)`), set
    /// by the auto-ghost pool and read on eviction to clear the telemetry flag.
    /// NOT used for routing — that is `filter` (structural) + `filter_fields`.
    filter_desc: Option<String>,
    order_by_field: String,
    sort_inverted: bool,
    has_aggregates: bool,
    group_fields: Vec<String>,
    /// The ghost's aggregate metric signature (sorted `op␁field␁label␁filter`
    /// per metric; see [`crate::aggregate_state::aggregate_signature`]). Empty
    /// for a covering ghost. A query routes to this ghost's PreComputed state
    /// only when its own signature is a subset — the metric-match guard.
    aggregate_sig: Vec<String>,
    state_ready: bool,
    /// True when the ghost embeds a field PROJECTION (EMBED). Such a ghost
    /// returns only the projected fields and `read_topn` skips the point-read,
    /// so it CANNOT losslessly serve a full-record SCAN (missing fields) nor a
    /// query whose predicate / downstream step needs a non-embedded field (e.g.
    /// NEAREST's vector — the add.39 mis-route). Excluded from record routing;
    /// reachable only via an explicit `SCAN GHOST` where the caller opted in.
    has_projection: bool,
}

/// Lightweight ghost router: decides whether a SCAN should read from
/// the primary keyspace or a Ghost Lobe.
pub struct GhostRouter {
    ghosts: HashMap<String, GhostRoutingMeta>,
    total_writes: AtomicU64,
}

impl Default for GhostRouter {
    fn default() -> Self {
        Self::new()
    }
}

impl GhostRouter {
    pub fn new() -> Self {
        Self {
            ghosts: HashMap::new(),
            total_writes: AtomicU64::new(0),
        }
    }

    /// Register a ghost for routing consideration.
    ///
    /// `filter_fields` carries each filter's operator — pass the actual op
    /// (Eq, Gt, Contains, …) from the source ghost, not `Eq` for everything.
    /// Callers building this tuple from persisted `Filter` structs should go
    /// through `ops::convert_filter_op` to translate parser-side FilterOp
    /// into the core FilterOp the router compares against.
    pub fn register_ghost(
        &mut self,
        name: &str,
        filter_fields: Vec<(String, FilterOp, Value)>,
        order_by_field: String,
        sort_inverted: bool,
        has_aggregates: bool,
        group_fields: Vec<String>,
    ) {
        self.ghosts.insert(
            name.to_string(),
            GhostRoutingMeta {
                filter_fields,
                // Structural filter defaults to "match nothing extra"; the engine
                // sets the real expression via `set_filter` right after register
                // (same post-register pattern as `set_filter_desc`). An `And([])`
                // is flat, so a ghost that never gets `set_filter` keeps the
                // pre-2.3 tuple-coverage behavior.
                filter: FilterExpr::And(Vec::new()),
                filter_desc: None,
                order_by_field,
                sort_inverted,
                has_aggregates,
                group_fields,
                state_ready: true,
                has_projection: false,
                // Set by the engine right after register (post-register pattern);
                // empty until then, so an aggregate query COVERS it only if it too
                // is metric-less — i.e. it never vacuously serves a real query.
                aggregate_sig: Vec::new(),
            },
        );
    }

    /// Set the ghost's full membership expression for structural routing (the
    /// only route for an OR/NOT ghost). Called by the engine right after
    /// `register_ghost`, mirroring `set_filter_desc` / `set_has_projection`.
    pub fn set_filter(&mut self, name: &str, filter: FilterExpr) {
        if let Some(meta) = self.ghosts.get_mut(name) {
            meta.filter = filter;
        }
    }

    /// Mark a ghost as carrying a field projection (EMBED). Set by the engine
    /// after `register_ghost` when the ghost's `projection` is non-empty, so
    /// the router can exclude it from full-record routing (see field docs).
    pub fn set_has_projection(&mut self, name: &str, has_projection: bool) {
        if let Some(meta) = self.ghosts.get_mut(name) {
            meta.has_projection = has_projection;
        }
    }

    /// Attach the ghost's aggregate metric signature (see
    /// [`crate::aggregate_state::aggregate_signature`]). Set by the engine after
    /// `register_ghost` for an aggregate ghost, so the router can verify a query
    /// requests only metrics this ghost precomputes before routing to it.
    pub fn set_aggregate_sig(&mut self, name: &str, sig: Vec<String>) {
        if let Some(meta) = self.ghosts.get_mut(name) {
            meta.aggregate_sig = sig;
        }
    }

    /// Whether the ghost `name` COVERS `query_sig`: every metric the query
    /// requests is precomputed by the ghost (same op, field, label, and
    /// per-metric filter). A superset ghost (e.g. an auto-ghost that precomputes
    /// count + sum over several fields) still serves a subset query; a ghost that
    /// lacks a requested metric — or precomputes it under a different filter —
    /// does NOT, so the caller falls back to a correct primary scan rather than
    /// returning another metric's value. An empty `query_sig` is covered
    /// vacuously (no aggregate metrics requested).
    pub fn aggregate_sig_covers(&self, name: &str, query_sig: &[String]) -> bool {
        match self.ghosts.get(name) {
            Some(meta) => query_sig.iter().all(|q| meta.aggregate_sig.contains(q)),
            None => false,
        }
    }

    /// Attach a filter_desc to an already-registered ghost. Used by the
    /// auto-ghost path when the source query carries an OR/NOT expression
    /// that cannot be represented as flat AND filters.
    pub fn set_filter_desc(&mut self, name: &str, desc: String) {
        if let Some(meta) = self.ghosts.get_mut(name) {
            meta.filter_desc = Some(desc);
        }
    }

    /// Read the `filter_desc` of a registered ghost. Used by the reaper
    /// and the LRU eviction path to recover the pattern key in
    /// `ScanTelemetryRegistry` when a ghost is about to disappear, so the
    /// pattern's `ghost_created` flag can be cleared and the same filter
    /// can re-trigger a fresh auto-ghost if it stays hot.
    pub fn get_filter_desc(&self, name: &str) -> Option<&str> {
        self.ghosts.get(name).and_then(|m| m.filter_desc.as_deref())
    }

    /// Move a ghost registration from `old_name` to `new_name`, preserving
    /// `filter_fields`, `filter_desc`, and all other routing metadata.
    /// Used by promotion — the Ephemeral's router entry becomes the
    /// Promoted's router entry without losing the OR/complex `filter_desc`
    /// that the original registration put in place. Returns `true` if the
    /// rename happened,
    /// `false` if `old_name` wasn't registered.
    pub fn rename_ghost(&mut self, old_name: &str, new_name: &str) -> bool {
        if let Some(meta) = self.ghosts.remove(old_name) {
            self.ghosts.insert(new_name.to_string(), meta);
            true
        } else {
            false
        }
    }

    /// Unregister a ghost (on DROP).
    pub fn unregister_ghost(&mut self, name: &str) {
        self.ghosts.remove(name);
    }

    /// Mark a ghost as ready or not (Building/Paused).
    #[cfg(test)]
    pub fn set_ghost_ready(&mut self, name: &str, ready: bool) {
        if let Some(meta) = self.ghosts.get_mut(name) {
            meta.state_ready = ready;
        }
    }

    /// Record N writes (atomic, lock-free).
    pub fn record_writes(&self, n: u64) {
        self.total_writes.fetch_add(n, Ordering::Relaxed);
    }

    /// Get the current total_writes counter value.
    pub fn total_writes(&self) -> u64 {
        self.total_writes.load(Ordering::Relaxed)
    }

    /// Set total_writes (used at boot to restore persisted value).
    pub fn set_total_writes(&self, val: u64) {
        self.total_writes.store(val, Ordering::Relaxed);
    }

    /// Plan a scan: pick the best source.
    ///
    /// Priority: Anchor-Eq override > PreComputed > Ordered > Filter-only > Primary.
    ///
    /// A ghost is "relevant" when either (a) its `filter_desc` equals the
    /// query's `filter_desc` — used for OR / complex expressions — or (b)
    /// every entry in its `filter_fields` tuple (field, op, value) also
    /// appears in the query filters.
    ///
    /// **Anchor-Eq override (v0.4 — caveat C-16, backlog Entry 22)**: when
    /// the query has at least one `Eq` predicate whose field is registered
    /// as an anchor on the source lobe, the router returns `Primary`
    /// unconditionally. The primary path resolves anchor → gravity → scan
    /// (`docs/xytalk-spec.md` §2.5); anchor lookup is O(1) on the dictionary
    /// keyspace and gives a small post-anchor record set, which is at least
    /// as fast as any ghost route given v0.4 ghosts lack zone_maps + sparse
    /// index + bloom (Entry 19, deferred to v0.5 sub-cycle A). This override
    /// addresses Q2's 380× regression observed at scale 1.0 SSD where
    /// `WHERE key=X` was unconditionally routed to an `items_by_key` ghost
    /// that walked 3 M entries.
    ///
    /// `anchored_fields` is the set of anchor field names for the source
    /// lobe (from `AnchorRegistry::get_anchors`); pass `None` to skip the
    /// override entirely (used by tests that pre-date this gate). The check
    /// uses `is_anchor()` semantics: a field marked declared-anchor is
    /// treated as populated. Operational deployments populate anchors
    /// normally (declarative `ANCHOR ... UNIQUE` followed by writes that
    /// fill the dictionary, or `AUTOANCHOR APPLY` for retroactive
    /// population). The pathological case of declared-but-not-populated
    /// falls through to scan via the primary path's resolution order (no
    /// worse than current behaviour).
    // Plan inputs; bundling into a struct is a design change, deferred (not a lint fix).
    #[allow(clippy::too_many_arguments)]
    pub fn plan_scan(
        &self,
        filters: &[(String, FilterOp, Value)],
        _primary_has_data: bool,
        order_by: Option<(&str, bool)>,
        has_aggregates: bool,
        group_by: &[String],
        has_limit: bool,
        query_filter: Option<&FilterExpr>,
        anchored_fields: Option<&BTreeSet<String>>,
    ) -> ScanSource {
        // Anchor-Eq override (C-16 / Entry 22). Detected before relevance
        // collection so that the trace below reflects what the router
        // actually decided, not a phantom ghost match.
        let anchor_eq_present = anchored_fields
            .map(|set| {
                !set.is_empty()
                    && filters
                        .iter()
                        .any(|(f, op, _)| *op == FilterOp::Eq && set.contains(f))
            })
            .unwrap_or(false);
        if anchor_eq_present {
            tracing::info!(
                "TRACE[5] plan_scan: anchor-Eq override (C-16) → Primary; filters={}, anchors={:?}",
                filters.len(),
                anchored_fields
            );
            return ScanSource::Primary;
        }

        let relevant: Vec<&str> = self
            .ghosts
            .iter()
            .filter(|(name, meta)| {
                if !meta.state_ready {
                    return false;
                }

                // Structural exact match: the query's expression IS the ghost's.
                // This is the only route for an OR/NOT ghost (and covers an
                // exact-same flat query too). Replaces the old filter_desc
                // string-equality fast path with a typed comparison.
                if let Some(qf) = query_filter
                    && &meta.filter == qf
                {
                    return true;
                }

                // A non-flat (OR/NOT) ghost has empty `filter_fields`; the
                // AND-subset coverage below would then match it against ANY
                // query (vacuous `all()`). Such a ghost may route ONLY by the
                // structural equality checked above.
                if meta.filter.as_flat_and().is_none() {
                    return false;
                }

                // Slow path: every ghost filter (field, op, value) must be
                // present in the query's flat filters.
                let filters_match = meta.filter_fields.iter().all(|(gf, gop, gv)| {
                    filters
                        .iter()
                        .any(|(sf, sop, sv)| sf == gf && sop == gop && sv == gv)
                });

                if !filters_match {
                    for (gf, gop, gv) in &meta.filter_fields {
                        let found = filters
                            .iter()
                            .any(|(sf, sop, sv)| sf == gf && sop == gop && sv == gv);
                        if !found {
                            tracing::debug!(
                                "TRACE[5] ghost '{}' filter ({}, {:?}, {:?}) NOT in query filters",
                                name,
                                gf,
                                gop,
                                gv
                            );
                        }
                    }
                }

                filters_match
            })
            .map(|(name, _)| name.as_str())
            .collect();

        tracing::info!(
            "TRACE[5] plan_scan: has_aggregates={}, group_by={:?}, query_filters={}, relevant={:?}",
            has_aggregates,
            group_by,
            filters.len(),
            relevant
        );

        // Pass 1a: PreComputed (aggregates + matching groups).
        //
        // Finding 11 (v0.2.4): every query predicate must be "covered"
        // by the ghost, meaning one of:
        //   (a) the predicate exactly matches a ghost `filter_fields`
        //       entry (ghost-constant predicate, e.g. `_type = "Credit"`);
        //   (b) the predicate is on a field in the ghost's
        //       `group_fields` AND its operator is `FilterOp::Eq`
        //       (Eq-on-group-key — `read_precomputed` applies these).
        // Any other predicate disqualifies the ghost from PreComputed
        // routing: returning the whole ghost would silently drop the
        // predicate from the result set. Such queries fall through to
        // pass 1b / 1c or to `ScanSource::Primary`.
        if has_aggregates {
            let mut best: Option<(&str, usize)> = None;
            for name in &relevant {
                let meta = &self.ghosts[*name];
                let groups_match = if group_by.is_empty() {
                    meta.group_fields.is_empty()
                } else {
                    meta.group_fields == group_by
                };
                tracing::info!(
                    "TRACE[5] pass 1a checking '{}': has_agg={}, groups_match={}, group_fields={:?}",
                    name,
                    meta.has_aggregates,
                    groups_match,
                    meta.group_fields
                );
                if !meta.has_aggregates {
                    continue;
                }
                if !groups_match {
                    continue;
                }

                // Finding 11: every query predicate must be covered by
                // the ghost's filter_fields (ghost-constant) or be an
                // Eq predicate on a group_fields entry.
                let all_predicates_covered = filters.iter().all(|(qf, qop, qv)| {
                    let is_ghost_constant = meta
                        .filter_fields
                        .iter()
                        .any(|(gf, gop, gv)| gf == qf && gop == qop && gv == qv);
                    if is_ghost_constant {
                        return true;
                    }

                    *qop == FilterOp::Eq && meta.group_fields.iter().any(|gf| gf == qf)
                });

                if !all_predicates_covered {
                    tracing::info!(
                        "TRACE[5] pass 1a '{}' disqualified: query has predicates outside ghost filter_fields ∪ Eq-on-group_fields (Finding 11 guard)",
                        name
                    );
                    continue;
                }

                let specificity = meta.filter_fields.len();
                if best.is_none() || specificity > best.unwrap().1 {
                    best = Some((name, specificity));
                }
            }
            if let Some((name, _)) = best {
                tracing::info!("TRACE[5] pass 1a → GhostPreComputed('{}')", name);
                return ScanSource::GhostPreComputed(name.to_string());
            }
            tracing::info!(
                "TRACE[5] pass 1a → no match (has_aggregates=true but no ghost qualified)"
            );
        }

        // Passes 1b/1c return individual records (or feed a runtime
        // aggregate over a covering index), so they must NEVER route to a
        // row-collapsing ghost: a GROUP BY / AGGREGATE ghost stores one
        // summary row per group, not the underlying records. Routing a
        // record scan there silently drops every record past the first per
        // group — the 1g bug, where `SCAN WHERE g = X AND t = "k"` matched a
        // `GROUP BY g AGGREGATE …` ghost and returned 1 of N matching rows.
        // Such ghosts are valid only via Pass 1a, where the query's own
        // GROUP BY matches the ghost's grouping.
        // A ghost may serve a full-record scan only if it stores whole records:
        // not a GROUP BY/AGGREGATE ghost (one summary row per group — the 1g bug)
        // and not a PROJECTION ghost (only embedded fields — would drop fields and
        // skip predicates/NEAREST on non-embedded fields, the add.39 mis-route).
        let serves_records = |meta: &GhostRoutingMeta| {
            !meta.has_aggregates && meta.group_fields.is_empty() && !meta.has_projection
        };

        // Pass 1b: Ordered scan (ORDER BY + LIMIT)
        if let Some((field, descending)) = order_by {
            for name in &relevant {
                let meta = &self.ghosts[*name];
                if has_limit
                    && meta.order_by_field == field
                    && meta.sort_inverted == descending
                    && serves_records(meta)
                {
                    return ScanSource::Ghost(name.to_string());
                }
            }
        }

        // Pass 1c: Filter-only (no ORDER BY)
        if order_by.is_none()
            && let Some(name) = relevant
                .iter()
                .find(|name| serves_records(&self.ghosts[**name]))
        {
            return ScanSource::Ghost(name.to_string());
        }

        ScanSource::Primary
    }

    /// Check if the router has any ghosts registered.
    pub fn has_ghosts(&self) -> bool {
        !self.ghosts.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int_val(n: i64) -> Value {
        Value::Int(n)
    }

    #[test]
    fn eq_ghost_routes_on_matching_query() {
        let mut router = GhostRouter::new();
        router.register_ghost(
            "g_eq",
            vec![(
                "status".to_string(),
                FilterOp::Eq,
                Value::Text("active".into()),
            )],
            String::new(),
            false,
            false,
            vec![],
        );

        let src = router.plan_scan(
            &[(
                "status".to_string(),
                FilterOp::Eq,
                Value::Text("active".into()),
            )],
            true,
            None,
            false,
            &[],
            false,
            None,
            None,
        );
        assert!(matches!(src, ScanSource::Ghost(ref name) if name == "g_eq"));
    }

    /// The Q6/Q7/Q9 regression: ghosts built for non-Eq filters must route.
    /// Pre-v0.2 the router compared field+value only and hardcoded op=Eq,
    /// so `x > 5` ghosts never fired for `x > 5` queries.
    #[test]
    fn gt_ghost_routes_on_matching_gt_query() {
        let mut router = GhostRouter::new();
        router.register_ghost(
            "g_gt",
            vec![("score".to_string(), FilterOp::Gt, int_val(650))],
            String::new(),
            false,
            false,
            vec![],
        );

        let src = router.plan_scan(
            &[("score".to_string(), FilterOp::Gt, int_val(650))],
            true,
            None,
            false,
            &[],
            false,
            None,
            None,
        );
        assert!(matches!(src, ScanSource::Ghost(ref name) if name == "g_gt"));
    }

    #[test]
    fn contains_ghost_routes_on_matching_contains_query() {
        let mut router = GhostRouter::new();
        router.register_ghost(
            "g_contains",
            vec![(
                "tags".to_string(),
                FilterOp::Contains,
                Value::Text("urgent".into()),
            )],
            String::new(),
            false,
            false,
            vec![],
        );

        let src = router.plan_scan(
            &[(
                "tags".to_string(),
                FilterOp::Contains,
                Value::Text("urgent".into()),
            )],
            true,
            None,
            false,
            &[],
            false,
            None,
            None,
        );
        assert!(matches!(src, ScanSource::Ghost(ref name) if name == "g_contains"));
    }

    /// Operator mismatch must NOT route. An Eq ghost cannot serve a Gt query
    /// even when field and value match — the ghost may have pruned rows the
    /// range query needs.
    #[test]
    fn eq_ghost_does_not_route_gt_query() {
        let mut router = GhostRouter::new();
        router.register_ghost(
            "g_eq",
            vec![("score".to_string(), FilterOp::Eq, int_val(650))],
            String::new(),
            false,
            false,
            vec![],
        );

        let src = router.plan_scan(
            &[("score".to_string(), FilterOp::Gt, int_val(650))],
            true,
            None,
            false,
            &[],
            false,
            None,
            None,
        );
        assert!(matches!(src, ScanSource::Primary));
    }

    /// OR ghost: `filter_fields` is empty (an OR doesn't flatten to AND
    /// tuples), so it routes ONLY by structural equality against the query's
    /// FilterExpr — the replacement for the old `filter_desc` string fast path.
    #[test]
    fn or_ghost_routes_by_structural_match() {
        use xytalk_parser::ast::{Filter, FilterOp as AstOp, Literal};
        let or_expr = || {
            FilterExpr::Or(vec![
                FilterExpr::Condition(Filter {
                    field: "a".into(),
                    op: AstOp::Eq,
                    value: Literal::Int(1),
                }),
                FilterExpr::Condition(Filter {
                    field: "b".into(),
                    op: AstOp::Eq,
                    value: Literal::Int(2),
                }),
            ])
        };
        let mut router = GhostRouter::new();
        router.register_ghost("g_or", vec![], String::new(), false, false, vec![]);
        router.set_filter("g_or", or_expr());

        // Same OR expression → structural match → routes. A different query
        // would fall through to the (empty) tuple set, which the router guards
        // against for non-flat ghosts, so only the exact OR routes here.
        let q = or_expr();
        let src = router.plan_scan(&[], true, None, false, &[], false, Some(&q), None);
        assert!(matches!(src, ScanSource::Ghost(ref name) if name == "g_or"));
    }

    /// Ghost without filter_desc + query with filter_desc: filter_desc fast
    /// path silently misses, fall-through to tuple matching. Empty ghost
    /// filters trivially match any query, so this still routes.
    #[test]
    fn filter_desc_mismatch_falls_through_to_tuple() {
        let mut router = GhostRouter::new();
        router.register_ghost(
            "g_bare",
            vec![], // empty filter_fields = matches everything
            String::new(),
            false,
            false,
            vec![],
        );

        let src = router.plan_scan(
            &[("x".to_string(), FilterOp::Eq, int_val(1))],
            true,
            None,
            false,
            &[],
            false,
            None,
            None,
        );
        assert!(matches!(src, ScanSource::Ghost(_)));
    }

    /// Auto-ghost regression: an auto-ghost created from a filter-only scan
    /// pattern registers with `has_aggregates=false`. When a later query
    /// asks for aggregates (`SCAN ... | count(), sum(m)`), the router MUST
    /// NOT route to `GhostPreComputed` — reading that ghost's zero-spec
    /// AggregateState returns `{count: N}` with the Sum missing, which
    /// would silently produce wrong results. The ghost CAN route as
    /// `Ghost` (filter-only covering index) and the aggregate is then
    /// computed by the scan loop over the ghost entries.
    #[test]
    fn filter_only_ghost_never_routes_to_precomputed() {
        let mut router = GhostRouter::new();
        router.register_ghost(
            "g_no_agg",
            vec![(
                "status".to_string(),
                FilterOp::Eq,
                Value::Text("active".into()),
            )],
            String::new(),
            false,
            false, // has_aggregates = false
            vec![],
        );

        let src = router.plan_scan(
            &[(
                "status".to_string(),
                FilterOp::Eq,
                Value::Text("active".into()),
            )],
            true,
            None,
            true, // query asks for aggregates
            &[],
            false,
            None,
            None,
        );

        // Must NOT be PreComputed — that would give sum = 0 wrongly.
        assert!(!matches!(src, ScanSource::GhostPreComputed(_)));
    }

    #[test]
    fn not_ready_ghost_does_not_route() {
        let mut router = GhostRouter::new();
        router.register_ghost(
            "g",
            vec![("x".to_string(), FilterOp::Eq, int_val(1))],
            String::new(),
            false,
            false,
            vec![],
        );
        router.set_ghost_ready("g", false);

        let src = router.plan_scan(
            &[("x".to_string(), FilterOp::Eq, int_val(1))],
            true,
            None,
            false,
            &[],
            false,
            None,
            None,
        );
        assert!(matches!(src, ScanSource::Primary));
    }

    // ─── v0.4 cost model (caveat C-16, backlog Entry 22) ────────────────

    /// (a) Anchor populated + ghost matching → Primary preferred.
    ///
    /// Models the C-16 case Q2 (`SCAN creditos WHERE _type="Credit" AND
    /// rfc=X | GROUP BY rfc | AGGREGATE`). With `rfc` declared as anchor on
    /// `creditos`, the router must override the matching `credits_by_rfc`
    /// ghost and pick Primary so the anchor → gravity → scan resolution
    /// runs (sub-ms) instead of walking the unaccelerated ghost (~345 ms
    /// observed at scale 1.0 SSD).
    #[test]
    fn anchor_eq_overrides_ghost_match_to_primary() {
        let mut router = GhostRouter::new();
        // Ghost matches both filter and group_by; would normally route to
        // GhostPreComputed (`_type` ghost-constant + `rfc` Eq on group key).
        router.register_ghost(
            "credits_by_rfc",
            vec![(
                "_type".to_string(),
                FilterOp::Eq,
                Value::Text("Credit".into()),
            )],
            String::new(),
            false,
            true, // has_aggregates
            vec!["rfc".to_string()],
        );

        let mut anchored = BTreeSet::new();
        anchored.insert("rfc".to_string());

        let src = router.plan_scan(
            &[
                (
                    "_type".to_string(),
                    FilterOp::Eq,
                    Value::Text("Credit".into()),
                ),
                (
                    "rfc".to_string(),
                    FilterOp::Eq,
                    Value::Text("ACME-001".into()),
                ),
            ],
            true,
            None,
            true,
            &["rfc".to_string()],
            false,
            None,
            Some(&anchored),
        );

        // C-16 fix: even though the ghost technically qualifies for
        // PreComputed, the anchor on `rfc` makes Primary at least as fast.
        assert!(matches!(src, ScanSource::Primary));
    }

    /// (b) PreComputed vs Primary (no anchor on filter columns) →
    /// PreComputed wins. Confirms the override does NOT trigger when the
    /// anchor set has no overlap with the query's Eq predicates, preserving
    /// the Finding 11 contract for queries the cost model doesn't apply to.
    #[test]
    fn no_anchor_on_filter_keeps_precomputed() {
        let mut router = GhostRouter::new();
        router.register_ghost(
            "credits_agg",
            vec![(
                "_type".to_string(),
                FilterOp::Eq,
                Value::Text("Credit".into()),
            )],
            String::new(),
            false,
            true, // has_aggregates
            vec!["rfc".to_string()],
        );

        // Anchor exists on the lobe but on a different field (not in the query).
        let mut anchored = BTreeSet::new();
        anchored.insert("rfc".to_string());

        let src = router.plan_scan(
            // Query has Eq on `_type` (not anchored) — no anchor-Eq match.
            &[(
                "_type".to_string(),
                FilterOp::Eq,
                Value::Text("Credit".into()),
            )],
            true,
            None,
            true, // has_aggregates
            &["rfc".to_string()],
            false,
            None,
            Some(&anchored),
        );

        assert!(matches!(src, ScanSource::GhostPreComputed(ref name) if name == "credits_agg"));
    }

    /// (c) Anchor declared but does not cover the filter (anchor on field
    /// that is not part of the query's filter set) → Ghost(name) remains
    /// eligible. Verifies the override is bounded: the mere presence of an
    /// anchor on the lobe must not disqualify ghost routes for queries
    /// that do not invoke the anchor.
    #[test]
    fn anchor_outside_filter_keeps_ghost_eligible() {
        let mut router = GhostRouter::new();
        router.register_ghost(
            "g_status",
            vec![(
                "status".to_string(),
                FilterOp::Eq,
                Value::Text("active".into()),
            )],
            String::new(),
            false,
            false,
            vec![],
        );

        // Anchor on `rfc`, but query doesn't filter on `rfc`.
        let mut anchored = BTreeSet::new();
        anchored.insert("rfc".to_string());

        let src = router.plan_scan(
            &[(
                "status".to_string(),
                FilterOp::Eq,
                Value::Text("active".into()),
            )],
            true,
            None,
            false,
            &[],
            false,
            None,
            Some(&anchored),
        );

        // Anchor doesn't apply to the filter set → ghost wins normally.
        assert!(matches!(src, ScanSource::Ghost(ref name) if name == "g_status"));
    }

    /// GLOBAL aggregate route: a ghost with aggregates but NO grouping
    /// (`group_fields` empty) must serve an aggregate query that also has no
    /// GROUP BY. Guards pass 1a's `group_by.is_empty() → group_fields.is_empty()`
    /// branch — the "not grouped" case the Grouping redesign makes explicit.
    #[test]
    fn global_aggregate_ghost_routes_to_precomputed() {
        let mut router = GhostRouter::new();
        router.register_ghost(
            "g_global",
            vec![(
                "status".to_string(),
                FilterOp::Eq,
                Value::Text("active".into()),
            )],
            String::new(),
            false,
            true,   // has_aggregates
            vec![], // no grouping → global aggregate
        );

        let src = router.plan_scan(
            &[(
                "status".to_string(),
                FilterOp::Eq,
                Value::Text("active".into()),
            )],
            true,
            None,
            true, // has_aggregates
            &[],  // query has no GROUP BY
            false,
            None,
            None,
        );

        assert!(matches!(src, ScanSource::GhostPreComputed(ref name) if name == "g_global"));
    }
}
