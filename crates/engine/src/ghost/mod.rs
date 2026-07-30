use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use turba_engine::tree::Tree;
use xytalk_parser::ast::{self, Filter};
use xyzdb_core::error::{Result, XyzError};
use xyzdb_core::record::{FilterOp, Record};
use xyzdb_core::value::Value;

mod build;
mod lifecycle;
pub(crate) mod metric_order;
mod notify;
mod persist;
mod read;

pub(crate) use metric_order::MetricOrder;

#[cfg(test)]
mod tests;

// ─── Ghost post-write hook types ──────────────────────────────────────────

/// Type of write operation, used by the post-write hook to maintain ghosts.
pub enum WriteType {
    Insert,
    /// A SET/update. Carries the record's PRE-update spatial key: a SET can
    /// re-gravitate the record (its spatial key moves), and the ghost entry was
    /// keyed by the OLD key. Without it the maintenance would operate on the new
    /// key and leave the old entry dangling — silent covering loss. The new key
    /// is the `spatial_key_bytes` argument of `notify_write`.
    Update {
        old_record: Record,
        old_spatial_key: Vec<u8>,
    },
    Delete,
}

/// Tracks the average overhead of ghost post-write hooks using EMA.
pub struct OverheadTracker {
    avg_hook_ns: AtomicU64,
    samples: AtomicU64,
}

impl OverheadTracker {
    pub fn new() -> Self {
        Self {
            avg_hook_ns: AtomicU64::new(0),
            samples: AtomicU64::new(0),
        }
    }

    /// Update the EMA with a new elapsed time sample (alpha = 0.001).
    pub fn update(&self, elapsed_ns: u64) {
        let prev = self.avg_hook_ns.load(Ordering::Relaxed);
        let count = self.samples.fetch_add(1, Ordering::Relaxed);
        if count == 0 {
            self.avg_hook_ns.store(elapsed_ns, Ordering::Relaxed);
        } else {
            // EMA: new_avg = 0.999 * old + 0.001 * sample
            // Use integer math: new = (999 * old + 1 * sample) / 1000
            let new_avg = (999 * prev + elapsed_ns) / 1000;
            self.avg_hook_ns.store(new_avg, Ordering::Relaxed);
        }
    }
}

// ─── Persistence format ─────────────────────────────────────────────────────

/// Reserved dictionary key prefix for ghost metadata: `[GHOST_META][ghost_id:2]`.
/// Shares its prefix with the legacy pin keyspace; meta values carry format
/// byte `0x03` (pins `0x01`), which disambiguates them. See [`crate::reserved_keys`].
const META_PREFIX: [u8; 2] = crate::reserved_keys::GHOST_META;

/// Reserved dictionary key prefix for total_writes counter:
/// `[GHOST_WRITES][lobe_id:2]`. See [`crate::reserved_keys`].
const WRITES_PREFIX: [u8; 2] = crate::reserved_keys::GHOST_WRITES;

// ─── Lightweight ghosts (0.7.6) — on-disk group rollups ────────────────────
//
// An aggregate ghost keeps one `AggregateState` per group. Grouping by a
// high-cardinality field (one group per rfc → millions) made the in-RAM
// `group_summaries` map gigabytes at scale-1 (~2.3 GB measured), the
// dominant share of engine RSS. Past `group_spill_limit()` groups the map
// is spilled to the DICTIONARY keyspace as one rollup entry per group and
// cleared; from then on the ghost is "lightweight": its in-RAM map stays
// empty and reads/incremental writes go through the rollup namespace.
//
// The discriminator is implicit — `group_fields` declared + empty in-RAM
// map → consult disk. An empty map means either "spilled" or "no groups
// yet"; both read the same way (an empty rollup range yields no groups),
// so no meta-format bump is needed and pre-0.7.6 persisted metas load
// unchanged.
//
// Rollups live in the dictionary keyspace, NOT the ghost keyspace: entry
// readers iterate `prefix(ghost_id)` over the ghost keyspace, and rollup
// entries inside that range would pollute every scan (fatal for DESC
// ghosts, where rollup keys would sort before all entries).
//
// Exactly ONE canonical entry per group, always. Build-time spills do a
// get-merge-put against it (`AggregateState::merge` is exact; misses are
// bloom-absorbed). The first cut appended seq-suffixed PARTIALS instead
// and merged them on read via prefix_iter — at scale 1 that turned every
// pinned lookup into a ~20 ms multi-level merge and every write RMW into
// the same (Q2 23.8 ms, Q7 17x). The canonical entry keeps pinned reads
// and RMWs on the bloom-backed exact-get path.

/// Reserved dictionary key prefix for lightweight-ghost group rollups:
/// `[ROLLUP][ghost_id:2][klen:u16][group_key]`.
/// The `klen` length prefix keeps the per-ghost wildcard scan parseable
/// (group keys are '|'-joined fragments of arbitrary length). See
/// [`crate::reserved_keys`].
const ROLLUP_PREFIX: [u8; 2] = crate::reserved_keys::ROLLUP;

/// Groups an aggregate ghost may hold in RAM before its summaries spill
/// to the rollup namespace. 64k groups ≈ a few MB — comfortably resident;
/// the next order of magnitude is not. `XYZ_GHOST_SUMMARIES_MAX_GROUPS`
/// overrides for tests (forcing tiny limits to exercise the spill path
/// without millions of records).
fn group_spill_limit() -> usize {
    std::env::var("XYZ_GHOST_SUMMARIES_MAX_GROUPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(65_536)
}

/// Canonical rollup key for one (ghost, group) pair. See [`ROLLUP_PREFIX`].
fn rollup_key(ghost_id: u16, group_key: &str) -> Vec<u8> {
    let gk = group_key.as_bytes();
    let mut key = Vec::with_capacity(6 + gk.len());
    key.extend_from_slice(&ROLLUP_PREFIX);
    key.extend_from_slice(&ghost_id.to_be_bytes());
    key.extend_from_slice(&(gk.len().min(u16::MAX as usize) as u16).to_be_bytes());
    key.extend_from_slice(&gk[..gk.len().min(u16::MAX as usize)]);
    key
}

/// Smallest byte string strictly greater than every string having `prefix` as a
/// prefix — i.e. the exclusive upper bound of the prefix range. `None` when
/// `prefix` is all `0xFF` (no successor exists; the range runs to the keyspace
/// tail). Used by the ghost range-seek to bound `[start, end)`.
fn byte_successor(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut s = prefix.to_vec();
    while let Some(&last) = s.last() {
        if last < 0xFF {
            *s.last_mut().unwrap() = last + 1;
            return Some(s);
        }
        s.pop();
    }
    None
}

/// Prefix covering every rollup of one ghost.
fn rollup_ghost_prefix(ghost_id: u16) -> Vec<u8> {
    let mut p = Vec::with_capacity(4);
    p.extend_from_slice(&ROLLUP_PREFIX);
    p.extend_from_slice(&ghost_id.to_be_bytes());
    p
}

/// Group key embedded in a rollup key, or `None` for a malformed key.
fn rollup_key_group(key: &[u8]) -> Option<&str> {
    if key.len() < 6 {
        return None;
    }
    let klen = u16::from_be_bytes([key[4], key[5]]) as usize;
    key.get(6..6 + klen)
        .and_then(|b| std::str::from_utf8(b).ok())
}

// Rollup values are now `RollupDelta` (hilo B): written as blind delta-appends
// and folded by `RollupMergeOperator`. Encoding/decoding live in
// `aggregate_state` (`RollupDelta::encode` / `decode_rollup_delta`).

/// Build-time spill state for one ghost's group summaries. The build scan
/// folds groups into the in-RAM map as before; past `group_spill_limit()`
/// groups the map is merged into the canonical rollup entries and
/// cleared, keeping build RAM O(limit) instead of O(groups).
struct RollupSpiller {
    ghost_id: u16,
    spill_count: u32,
    limit: usize,
}

impl RollupSpiller {
    fn new(ghost_id: u16) -> Self {
        Self {
            ghost_id,
            spill_count: 0,
            limit: group_spill_limit(),
        }
    }

    /// True once at least one spill happened — the ghost is lightweight.
    fn spilled(&self) -> bool {
        self.spill_count > 0
    }

    fn spill(
        &mut self,
        map: &mut std::collections::BTreeMap<String, crate::aggregate_state::AggregateState>,
        dictionary: &Tree,
    ) -> Result<()> {
        self.spill_count += 1;
        for (gk, st) in map.iter() {
            // Blind append (hilo B): the rollup merge operator folds same-group
            // deltas at compaction and read, so a group that re-appears across
            // spill rounds (records scattered over the scan) just appends
            // another delta — no get-merge-put. This kills the O(groups) RMW
            // that made the build O(groups·disk) and forced P0-2's revert.
            let key = rollup_key(self.ghost_id, gk);
            let delta = crate::aggregate_state::RollupDelta::from_aggregate_state(st);
            dictionary
                .insert(&key, &delta.encode())
                .map_err(|e| XyzError::Storage(format!("rollup spill append: {e}")))?;
        }
        map.clear();
        // Bare Tree::insert never triggers a flush (only the WriteBatch
        // path does) — without this seal, a multi-million-group build
        // would re-grow in the dictionary memtable the very RAM the spill
        // exists to bound.
        if dictionary.active_memtable_size() >= dictionary.max_memtable_size() {
            dictionary.seal_active();
            dictionary.notify_bg();
        }
        Ok(())
    }

    /// Spill when the map outgrows the in-RAM budget.
    fn maybe_spill(
        &mut self,
        map: &mut std::collections::BTreeMap<String, crate::aggregate_state::AggregateState>,
        dictionary: &Tree,
    ) -> Result<()> {
        if map.len() > self.limit {
            self.spill(map, dictionary)?;
        }
        Ok(())
    }

    /// End of build: a ghost that spilled must be UNIFORMLY lightweight
    /// (every group on disk, in-RAM map empty) — a split brain where some
    /// groups live in RAM and some on disk would make the empty-map
    /// discriminator lie. Ghosts that never spilled keep their map.
    fn finalize(
        &mut self,
        map: &mut std::collections::BTreeMap<String, crate::aggregate_state::AggregateState>,
        dictionary: &Tree,
    ) -> Result<()> {
        if self.spilled() && !map.is_empty() {
            self.spill(map, dictionary)?;
        }
        Ok(())
    }
}

/// Byte that marks the on-disk encoding version of a persisted ghost meta
/// record: `[MAGIC:2][GHOST_META_FORMAT:1][postcard payload]`.
///
/// **Bump this constant every commit that adds, removes, or reorders a field
/// in `PersistedGhostMeta`.** Postcard is a sequential format with no support
/// for `#[serde(default)]` on trailing fields — when it hits EOF expecting
/// another field, it returns `DeserializeUnexpectedEnd`, not a default value.
/// The format byte is the actual escape hatch: `load_all` skips records with
/// a byte it doesn't recognize and emits a warning instructing the operator
/// (dev, in v0.2 pre-release) to recreate the ghosts via `CREATE GHOST`.
///
/// Stable v0.1 was 0x02. Every Phase 1 schema change in v0.2-dev adds one.
/// At Phase 1 close, pick a clean number for the v0.2.0 release and freeze.
///
/// 0x04 (P0-2): the ghost ENTRY key gained a spatial-key uniqueness suffix and
/// Text sort values are now prefix-free encoded — both change the on-disk
/// `ghosts`-keyspace layout, not `PersistedGhostMeta`. The format byte is still
/// the right lever: bumping it makes `load_all` drop pre-0x04 ghosts (queries
/// fall back to a correct primary scan) so stale-format entries are never
/// served; recreate the ghosts via `CREATE GHOST` / `REFRESH`, or recreate the
/// data dir, to restore ghost-class latency.
/// 0x05 (ghost redesign, sub-step 1): `PersistedGhostMeta` gained an explicit
/// `spilled` marker so a grouped ghost's residency (in-RAM vs spilled-to-disk)
/// survives a reload instead of being inferred from an empty summaries map.
/// No back-compat: pre-0x05 ghosts are skipped by `load_all` and recreated via
/// `CREATE GHOST` / `REFRESH`.
/// 0x06 (ghost redesign, grouping key): group keys are now length-prefixed and
/// type-tagged (killing the `'|'`-join and `"null"`/`Int` collisions), which
/// changes both the persisted `group_summaries` keys and the on-disk rollup
/// keys of spilled ghosts. A pre-0x06 meta's keys would be misread by the new
/// decoder, so `load_all` drops pre-0x06 ghosts; recreate via `CREATE GHOST` /
/// `REFRESH`.
// 0x08: aggregate metric labels use the single canonical scheme (`sum(field)`,
// shared with the runtime path), replacing the ghost-only `field:Sum`. 0x07
// added per-metric label + filter; both older formats hit the gate in
// `decode_persisted_ghost_meta` and are rebuilt from source.
// 0x09: `PersistedGhostMeta` gained `metric_order` (the `ORDER BY <metric>`
// declaration) + `order_emitted_at` (freshness of the metric-ordered rollup).
// Pre-0x09 metas hit the format gate and are rebuilt via `CREATE GHOST` /
// `REFRESH`.
const GHOST_META_FORMAT: u8 = 0x09;

#[derive(Serialize, Deserialize)]
struct PersistedFilter {
    field: String,
    op: PersistedFilterOp,
    value: PersistedLiteral,
}

/// On-disk form of a ghost's membership `FilterExpr` tree. Mirrors the parser
/// AST (`Condition | And | Or | Not`) with persisted leaves, so a ghost can
/// carry OR/NOT/In, not just flat-AND. Introduced under a clean-start format —
/// no pre-existing on-disk ghost used a non-flat filter.
#[derive(Serialize, Deserialize)]
enum PersistedFilterExpr {
    Condition(PersistedFilter),
    And(Vec<PersistedFilterExpr>),
    Or(Vec<PersistedFilterExpr>),
    Not(Box<PersistedFilterExpr>),
}

/// On-disk form of an aggregate [`Metric`](crate::aggregate_state::Metric). The
/// walker-ready `filter_core` is not stored — it is rebuilt from `filter` at
/// load, mirroring the ghost header filter's cache.
#[derive(Serialize, Deserialize)]
struct PersistedMetric {
    field: String,
    op: crate::aggregate_state::AggOp,
    label: String,
    filter: Option<PersistedFilterExpr>,
}

#[derive(Serialize, Deserialize)]
enum PersistedFilterOp {
    Eq,
    Neq,
    Gt,
    Gte,
    Lt,
    Lte,
    IsNull,    // V4: ordinal 6 — must stay after Lte for backward compat
    IsNotNull, // V4: ordinal 7
    Contains,  // V4: ordinal 8
    In,        // ordinal 9 — appended last to keep prior ordinals stable
}

#[derive(Serialize, Deserialize)]
enum PersistedLiteral {
    Int(i64),
    Float(f64),
    Text(String),
    Bool(bool),
    Timestamp(String),
    Lid(String),
    Null,                                 // V4: ordinal 6
    List(Vec<PersistedLiteral>),          // V4: ordinal 7
    Map(Vec<(String, PersistedLiteral)>), // V4: ordinal 8
}

// ─── Conversion: AST ↔ Persisted ────────────────────────────────────────────

fn literal_ast_to_persisted(lit: &ast::Literal) -> PersistedLiteral {
    match lit {
        ast::Literal::Int(v) => PersistedLiteral::Int(*v),
        ast::Literal::Float(v) => PersistedLiteral::Float(*v),
        ast::Literal::Text(v) => PersistedLiteral::Text(v.clone()),
        ast::Literal::Bool(v) => PersistedLiteral::Bool(*v),
        ast::Literal::Timestamp(v) => PersistedLiteral::Timestamp(v.clone()),
        ast::Literal::Lid(v) => PersistedLiteral::Lid(v.clone()),
        ast::Literal::Null => PersistedLiteral::Null,
        ast::Literal::List(items) => {
            PersistedLiteral::List(items.iter().map(literal_ast_to_persisted).collect())
        }
        ast::Literal::Map(pairs) => PersistedLiteral::Map(
            pairs
                .iter()
                .map(|(k, v)| (k.clone(), literal_ast_to_persisted(v)))
                .collect(),
        ),
        // S1: params are bound before a ghost definition is persisted.
        ast::Literal::Param(name) => {
            unreachable!(
                "unbound parameter ${name} reached ghost persistence (bind_params skipped?)"
            )
        }
    }
}

fn persisted_to_literal_ast(p: &PersistedLiteral) -> ast::Literal {
    match p {
        PersistedLiteral::Int(v) => ast::Literal::Int(*v),
        PersistedLiteral::Float(v) => ast::Literal::Float(*v),
        PersistedLiteral::Text(v) => ast::Literal::Text(v.clone()),
        PersistedLiteral::Bool(v) => ast::Literal::Bool(*v),
        PersistedLiteral::Timestamp(v) => ast::Literal::Timestamp(v.clone()),
        PersistedLiteral::Lid(v) => ast::Literal::Lid(v.clone()),
        PersistedLiteral::Null => ast::Literal::Null,
        PersistedLiteral::List(items) => {
            ast::Literal::List(items.iter().map(persisted_to_literal_ast).collect())
        }
        PersistedLiteral::Map(pairs) => ast::Literal::Map(
            pairs
                .iter()
                .map(|(k, v)| (k.clone(), persisted_to_literal_ast(v)))
                .collect(),
        ),
    }
}

fn filter_to_persisted(f: &Filter) -> PersistedFilter {
    PersistedFilter {
        field: f.field.clone(),
        op: match f.op {
            ast::FilterOp::Eq => PersistedFilterOp::Eq,
            ast::FilterOp::Neq => PersistedFilterOp::Neq,
            ast::FilterOp::Gt => PersistedFilterOp::Gt,
            ast::FilterOp::Gte => PersistedFilterOp::Gte,
            ast::FilterOp::Lt => PersistedFilterOp::Lt,
            ast::FilterOp::Lte => PersistedFilterOp::Lte,
            ast::FilterOp::IsNull => PersistedFilterOp::IsNull,
            ast::FilterOp::IsNotNull => PersistedFilterOp::IsNotNull,
            ast::FilterOp::Contains => PersistedFilterOp::Contains,
            ast::FilterOp::In => PersistedFilterOp::In,
        },
        value: literal_ast_to_persisted(&f.value),
    }
}

fn persisted_to_filter(p: &PersistedFilter) -> Filter {
    Filter {
        field: p.field.clone(),
        op: match p.op {
            PersistedFilterOp::Eq => ast::FilterOp::Eq,
            PersistedFilterOp::Neq => ast::FilterOp::Neq,
            PersistedFilterOp::Gt => ast::FilterOp::Gt,
            PersistedFilterOp::Gte => ast::FilterOp::Gte,
            PersistedFilterOp::Lt => ast::FilterOp::Lt,
            PersistedFilterOp::Lte => ast::FilterOp::Lte,
            PersistedFilterOp::IsNull => ast::FilterOp::IsNull,
            PersistedFilterOp::IsNotNull => ast::FilterOp::IsNotNull,
            PersistedFilterOp::Contains => ast::FilterOp::Contains,
            PersistedFilterOp::In => ast::FilterOp::In,
        },
        value: persisted_to_literal_ast(&p.value),
    }
}

/// Convert a parser `FilterExpr` tree to its on-disk form (leaves via
/// [`filter_to_persisted`]).
fn filter_expr_to_persisted(e: &ast::FilterExpr) -> PersistedFilterExpr {
    match e {
        ast::FilterExpr::Condition(f) => PersistedFilterExpr::Condition(filter_to_persisted(f)),
        ast::FilterExpr::And(v) => {
            PersistedFilterExpr::And(v.iter().map(filter_expr_to_persisted).collect())
        }
        ast::FilterExpr::Or(v) => {
            PersistedFilterExpr::Or(v.iter().map(filter_expr_to_persisted).collect())
        }
        ast::FilterExpr::Not(inner) => {
            PersistedFilterExpr::Not(Box::new(filter_expr_to_persisted(inner)))
        }
    }
}

/// Rebuild a parser `FilterExpr` tree from its on-disk form.
fn persisted_to_filter_expr(p: &PersistedFilterExpr) -> ast::FilterExpr {
    match p {
        PersistedFilterExpr::Condition(f) => ast::FilterExpr::Condition(persisted_to_filter(f)),
        PersistedFilterExpr::And(v) => {
            ast::FilterExpr::And(v.iter().map(persisted_to_filter_expr).collect())
        }
        PersistedFilterExpr::Or(v) => {
            ast::FilterExpr::Or(v.iter().map(persisted_to_filter_expr).collect())
        }
        PersistedFilterExpr::Not(inner) => {
            ast::FilterExpr::Not(Box::new(persisted_to_filter_expr(inner)))
        }
    }
}

/// Flatten an in-RAM [`Metric`](crate::aggregate_state::Metric) to its on-disk
/// form. The walker-ready `filter_core` is dropped (rebuilt at load).
fn metric_to_persisted(m: &crate::aggregate_state::Metric) -> PersistedMetric {
    PersistedMetric {
        field: m.field.clone(),
        op: m.op.clone(),
        label: m.label.clone(),
        filter: m.filter.as_ref().map(filter_expr_to_persisted),
    }
}

/// Rebuild an in-RAM `Metric` from its on-disk form, recompiling the
/// walker-ready `filter_core` from the persisted `filter`.
fn persisted_to_metric(p: &PersistedMetric) -> crate::aggregate_state::Metric {
    crate::aggregate_state::Metric::new(
        p.field.clone(),
        p.op.clone(),
        p.label.clone(),
        p.filter.as_ref().map(persisted_to_filter_expr),
    )
}

/// Count of leaf conditions in a filter tree — a display stat for SHOW GHOSTS.
fn filter_expr_condition_count(e: &ast::FilterExpr) -> usize {
    match e {
        ast::FilterExpr::Condition(_) => 1,
        ast::FilterExpr::And(v) | ast::FilterExpr::Or(v) => {
            v.iter().map(filter_expr_condition_count).sum()
        }
        ast::FilterExpr::Not(inner) => filter_expr_condition_count(inner),
    }
}

fn meta_dictionary_key(ghost_id: u16) -> Vec<u8> {
    let mut key = Vec::with_capacity(4);
    key.extend_from_slice(&META_PREFIX);
    key.extend_from_slice(&ghost_id.to_be_bytes());
    key
}

/// Unix-epoch microseconds as i64. Matches `GhostMeta.last_accessed` and
/// `created_at`. Used by the create paths, the bump path, and the
/// load_all reset.
pub(crate) fn now_micros() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as i64
}

/// Microseconds in one UTC day. Used by the TTL reaper to compute the
/// "day bucket" (`now_micros() / MICROS_PER_DAY`) — a monotonic integer
/// that increments exactly once at each UTC midnight. Comparing the
/// current bucket against the last-seen bucket tells the reaper whether
/// to rotate daily access bitmaps, without pulling in `chrono` or doing
/// timezone arithmetic.
pub(crate) const MICROS_PER_DAY: i64 = 86_400_000_000;

/// One ghost identified as expired by the TTL reaper. The `lobe_id` is
/// carried alongside the name so the caller (`Engine::reap_cycle`) can
/// unregister the ghost from the right router without another lock
/// round-trip on the ghost manager. Using a named struct instead of
/// `(String, u16)` keeps call sites self-documenting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpiredGhost {
    pub name: String,
    pub lobe_id: u16,
}

// ─── Runtime metadata ───────────────────────────────────────────────────────

/// Lifecycle class of a ghost.
///
/// - `Permanent`: created via `CREATE GHOST`. No TTL, no automatic eviction.
/// - `Ephemeral`: created automatically from scan telemetry. 24h TTL from
///   last access, aggregate-only, capped at 20 per lobe (LRU eviction;
///   bumped 10 → 20 in v0.2.1 Finding 5).
/// - `Promoted`: an Ephemeral that was accessed for 7 consecutive days.
///   Rebuilt with EMBED + ORDER BY, 30d TTL, capped at 5 per lobe.
///
/// `Default = Permanent` matches legacy ghost behavior — any path that
/// builds a `GhostMeta` from scratch defaults to permanent unless
/// explicitly reclassified (the auto-ghost path sets Ephemeral after
/// `create()` returns).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum GhostType {
    #[default]
    Permanent,
    Ephemeral,
    Promoted,
}

/// The non-`Permanent` half of [`GhostType`]: an auto-created ghost is either
/// still `Ephemeral` or has been `Promoted`. Keeps `GhostLifecycle::Auto` from
/// representing a nonsensical "auto Permanent".
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoClass {
    Ephemeral,
    Promoted,
}

/// Access telemetry driving auto-ghost promotion + LRU. Present only on `Auto`
/// ghosts (a `Declared` ghost has no telemetry to carry).
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct AccessTelemetry {
    /// One bit per recent day of access, rotated right daily. The seven low
    /// bits all set = accessed 7 consecutive days → promotion candidate.
    pub daily_access_bitmap: u32,
    pub access_count_total: u64,
}

/// The lifecycle axis of the two-axis taxonomy (§2): how a ghost is born and how
/// it dies. Collapses the old loose `is_auto` + `ghost_type` + `ttl_seconds` +
/// `daily_access_bitmap` + `access_count_total`, so impossible states
/// (Permanent-but-auto, telemetry on a Declared ghost, TTL on a Permanent) are
/// unrepresentable. The reaper / promotion / LRU / telemetry machinery is
/// unchanged — this only re-encapsulates the fields it reads.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub enum GhostLifecycle {
    /// `CREATE GHOST`: permanent, no TTL, no telemetry.
    #[default]
    Declared,
    /// Auto-created from scan telemetry (`Ephemeral`, may be `Promoted`).
    Auto {
        class: AutoClass,
        ttl_seconds: u64,
        telemetry: AccessTelemetry,
    },
}

/// Default TTL for a freshly auto-created (`Ephemeral`) ghost: 24h from last
/// access. The auto-ghost pool confirms this via `reclassify_lifecycle` right
/// after create; kept here so the create-time `Auto` state is never bogus.
pub(crate) const EPHEMERAL_TTL_SECONDS: u64 = 86_400;

/// Build the create-time lifecycle from the old `is_auto` flag: an auto-ghost
/// starts `Auto{Ephemeral}` (the pool reclassifies with the real TTL right
/// after), a `CREATE GHOST` starts `Declared`.
pub(crate) fn lifecycle_for(is_auto: bool) -> GhostLifecycle {
    if is_auto {
        GhostLifecycle::Auto {
            class: AutoClass::Ephemeral,
            ttl_seconds: EPHEMERAL_TTL_SECONDS,
            telemetry: AccessTelemetry::default(),
        }
    } else {
        GhostLifecycle::Declared
    }
}

/// Residency of a grouped-aggregate ghost's per-group summaries.
///
/// Explicit state that replaces the old overloaded `group_summaries.is_empty()`
/// discriminator, which read THREE distinct states as one: "no groups yet",
/// "spilled to disk", and (when combined with `group_fields`) "not grouped".
/// Now `Residency` is only present on grouped ghosts, and it distinguishes the
/// two that used to alias:
///
/// - `InRam(map)` — summaries held in RAM. The map may be empty (no matched
///   groups yet); that is NOT the same as spilled.
/// - `Spilled` — the group count crossed `group_spill_limit()`, so the
///   summaries were merged into the dictionary rollup keyspace and the in-RAM
///   map dropped. Reads and incremental maintenance go through the rollups.
///
/// A ghost that spilled is UNIFORMLY `Spilled` (never a split brain where some
/// groups live in RAM and some on disk) — see `RollupSpiller::finalize`.
#[derive(Clone, Debug)]
pub enum Residency {
    InRam(std::collections::BTreeMap<String, crate::aggregate_state::AggregateState>),
    Spilled,
}

/// Whether an aggregate ghost is grouped, made an explicit state instead of
/// inferred from `group_fields.is_empty()`.
///
/// The old flat `group_fields: Vec<String>` overloaded emptiness to mean "not
/// grouped" — the same `is_empty()` a reader had to disambiguate from a grouped
/// ghost that legitimately declared no fields (unrepresentable, but the type
/// allowed it). `Grouping` makes the two cases distinct at the type level: a
/// `Grouped` variant is non-empty by construction.
///
/// - `Global` — one summary over every matching record (no GROUP BY).
/// - `Grouped(fields)` — one summary per distinct tuple of `fields`. Invariant:
///   `fields` is non-empty (built via [`Grouping::from_fields`]).
#[derive(Clone, Debug)]
pub enum Grouping {
    Global,
    Grouped(Vec<String>),
}

impl Grouping {
    /// Build from the flat field list used on disk and by the parser: an empty
    /// list is `Global`, a non-empty list is `Grouped`.
    pub fn from_fields(fields: Vec<String>) -> Self {
        if fields.is_empty() {
            Grouping::Global
        } else {
            Grouping::Grouped(fields)
        }
    }

    /// The grouping fields as a slice (`&[]` when global).
    pub fn fields(&self) -> &[String] {
        match self {
            Grouping::Global => &[],
            Grouping::Grouped(v) => v,
        }
    }

    /// True when this is a grouped (GROUP BY) aggregate.
    pub fn is_grouped(&self) -> bool {
        matches!(self, Grouping::Grouped(_))
    }
}

/// The aggregate overlay of a ghost — present only when the ghost computes
/// aggregates. Layered ON TOP of the universal entry index (`GhostMeta` keeps
/// its entries whether or not this is `Some`), so it holds only the state that
/// is meaningless for a pure covering ghost: the specs, the global roll-up, and
/// the grouping + its residency. Bundling them here makes an inconsistent state
/// (e.g. grouping set on a non-aggregate ghost) unrepresentable.
#[derive(Clone, Debug)]
pub struct AggregateContent {
    /// The per-metric specs this ghost precomputes (op + field + label + optional
    /// per-metric filter). Identity is per-metric, so same-op metrics with
    /// different filters stay distinct.
    pub aggregate_specs: Vec<crate::aggregate_state::Metric>,
    /// Whole-ghost aggregate (the `GROUP BY`-less roll-up), always maintained.
    pub global_aggregates: crate::aggregate_state::AggregateState,
    /// `Global` for a whole-ghost aggregate, `Grouped(fields)` for a GROUP BY.
    pub grouping: Grouping,
    /// Per-group state: an in-RAM map or a marker that it spilled to the
    /// dictionary rollup keyspace. Only consulted for grouped aggregates.
    pub residency: Residency,
}

/// Outcome of decoding a raw ghost-metadata byte string from the dictionary
/// keyspace. Exposes the three states `load_all` needs to distinguish so
/// the warn log can be precise about what went wrong.
// One variant is larger; boxing it is a design change, deferred (not a lint fix).
#[allow(clippy::large_enum_variant)]
enum DecodedMeta {
    Ok(PersistedGhostMeta),
    /// Magic / format byte didn't match `GHOST_META_FORMAT`. Almost always
    /// means the record was written by a different schema version of this
    /// binary during v0.2 dev iteration. Safe to skip.
    UnknownFormat {
        found: u8,
    },
    /// Magic and format matched, but postcard rejected the payload. True
    /// corruption (disk flip, truncation).
    Corrupt(postcard::Error),
}

/// Decode a single ghost-meta record. Extracted out of `load_all` so the
/// format-byte escape hatch is directly unit-testable without spinning up
/// an Engine + Tree.
fn decode_persisted_ghost_meta(val: &[u8]) -> DecodedMeta {
    if val.len() < 3 || val[0..2] != xyzdb_core::record::XYZDB_MAGIC {
        // No magic → treat as unknown. Use 0x00 when there's nothing to report.
        return DecodedMeta::UnknownFormat {
            found: if val.len() >= 3 { val[2] } else { 0 },
        };
    }
    if val[2] != GHOST_META_FORMAT {
        return DecodedMeta::UnknownFormat { found: val[2] };
    }
    match postcard::from_bytes::<PersistedGhostMeta>(&val[3..]) {
        Ok(p) => DecodedMeta::Ok(p),
        Err(e) => DecodedMeta::Corrupt(e),
    }
}

/// Serializable ghost metadata (no AST types — uses plain strings/enums).
#[derive(Serialize, Deserialize)]
struct PersistedGhostMeta {
    // Identity
    name: String,
    ghost_id: u16,
    version: u8, // always 2
    lobe_id: u16,
    source_lobe: String,
    is_auto: bool,

    // Filters and order
    filter: PersistedFilterExpr,
    order_by_field: String,
    sort_inverted: bool, // true for DESC

    // `ORDER BY <metric>` metric-ordered rollup: the declaration + the freshness
    // of the last emit. `MetricOrder` is plain (String + bool), no AST types.
    metric_order: Option<MetricOrder>,
    order_emitted_at: Option<i64>,

    // State: 0=Building, 1=Ready, 2=Paused
    state: u8,

    // Index stats
    index_count: u64,

    // Pre-computed aggregates
    aggregate_specs: Vec<PersistedMetric>,
    global_aggregates: crate::aggregate_state::AggregateState,

    // Group summaries: group-by fields, the in-RAM per-group summaries (empty
    // when spilled), and an explicit `spilled` marker so residency survives a
    // reload — a spilled ghost persists an empty map + `spilled = true`, which is
    // distinct from a grouped ghost that simply has no groups yet (empty + false).
    group_fields: Vec<String>,
    group_summaries: std::collections::BTreeMap<String, crate::aggregate_state::AggregateState>,
    spilled: bool,

    // Projection
    projection: Vec<String>,

    // Tracking
    created_at: i64,
    last_accessed: i64,
    incremental_updates: u64,

    // ── Lifecycle (flat on-disk form of GhostLifecycle) ──────────────────
    //
    // NOTE: no `#[serde(default)]` — postcard doesn't honor it for trailing
    // fields. Schema evolution is handled by bumping `GHOST_META_FORMAT` at
    // the top of this file; `load_all` skips records with an unrecognized
    // format byte and logs a recreate-with-CREATE-GHOST hint.
    ghost_type: GhostType,
    ttl_seconds: Option<u64>,
    /// Rolling 32-bit bitmap of access days (bit 0 = today, bit 1 = yesterday).
    /// The TTL reaper rotates this once per day. The promotion check uses it
    /// to detect "7 consecutive days of access" for promotion.
    daily_access_bitmap: u32,
    /// Cumulative access count since the ghost was created. Not used for
    /// eviction decisions (last_accessed drives LRU) but emitted in metrics.
    access_count_total: u64,
}

/// Runtime ghost metadata (uses AST Filter type).
pub struct GhostMeta {
    // Identity
    pub name: String,
    pub ghost_id: u16,
    pub version: u8, // always 2
    pub lobe_id: u16,
    pub source_lobe: String,

    // Membership predicate (full expression: And/Or/Not/Condition + In).
    pub filter: ast::FilterExpr,
    pub order_by_field: String,
    pub sort_inverted: bool,

    /// `ORDER BY <metric>` declaration (grouped-aggregate ghosts only): the
    /// canonical aggregate label + direction the groups are also kept ordered by,
    /// so `TOP n BY <metric>` reads O(N) from the metric-ordered rollup. `None`
    /// for a plain ghost. See [`metric_order`].
    pub metric_order: Option<MetricOrder>,
    /// Unix micros of the last successful metric-order emission, or `None` when no
    /// order is declared OR the last emit failed / collided (stale — reads fall
    /// back to O(M)). Surfaced by `SHOW GHOSTS` as the order's age.
    pub order_emitted_at: Option<i64>,

    // State: 0=Building, 1=Ready, 2=Paused
    pub state: u8,

    // Index stats
    pub index_count: u64,

    // Aggregate overlay: `None` for a pure covering ghost, `Some` when the
    // ghost also computes aggregates. The entry index above is UNIVERSAL — an
    // aggregate ghost keeps its entries too (readable via `SCAN GHOST`); the
    // aggregate state is layered on top. Making it an `Option` means a covering
    // ghost cannot carry grouping/residency it has no use for (irrepresentable),
    // replacing the `aggregate_specs.is_empty()` discriminator.
    pub aggregate: Option<AggregateContent>,

    // Projection: fields embedded in ghost entries for zero-point-read reads
    pub projection: Vec<String>,

    // Tracking
    pub created_at: i64,
    pub last_accessed: i64,
    pub incremental_updates: u64,

    // ── Lifecycle (§2 axis 2) — Declared vs Auto{class, ttl, telemetry} ──
    pub lifecycle: GhostLifecycle,

    // ── Derived cache (NOT persisted) ────────────────────────────────────
    /// Lazily-built core-typed filter tree of [`Self::filters`], memoised across
    /// writes and evaluated by the single walker `matches_core_expr`. `filters`
    /// is immutable once the ghost is created, so after `notify_write` fills this
    /// it never goes stale. Rebuilding it on every write — deep-cloning each
    /// `Text`/`List`/`Map` literal through `literal_to_value`, once per ghost —
    /// was a hot-path cost (audit P2-2); the write path reads this cache, never
    /// reconverts. `GhostMeta` is persisted via `PersistedGhostMeta`, not
    /// directly, so this runtime-only field does not touch the on-disk format.
    pub(crate) core_filters_cache: Option<crate::ops::CoreFilterExpr>,

    /// Runtime health flag (NOT persisted): set when an incremental maintenance
    /// op on this ghost fails (entry insert/remove, or a rollup append), so the
    /// ghost may be missing entries/aggregates until a `REFRESH GHOST`. Surfaced
    /// by `SHOW GHOSTS` so the failure is visible, not just a `tracing::warn!`.
    /// Resets on reload (a restart re-reads the persisted state); REFRESH clears
    /// it by rebuilding from source.
    pub(crate) maintenance_degraded: bool,
}

impl GhostMeta {
    /// Whether the ghost's aggregates may be stale: `dirty` on the global
    /// roll-up or on any in-RAM group (a Min/Max subtract under delete sets it —
    /// see option D). Spilled groups can't be checked without disk I/O, so this
    /// reports the always-maintained global + in-RAM groups. `false` for a
    /// covering ghost. Surfaced by `SHOW GHOSTS`.
    pub fn aggregates_dirty(&self) -> bool {
        match &self.aggregate {
            None => false,
            Some(agg) => {
                agg.global_aggregates.dirty
                    || matches!(&agg.residency, Residency::InRam(m) if m.values().any(|s| s.dirty))
            }
        }
    }

    /// The GROUP BY fields, or `&[]` when the ghost is not a grouped aggregate
    /// (covering ghosts and global aggregates). Bridges callers that key/label/
    /// route by field name to the [`Grouping`] enum inside the overlay.
    pub fn group_fields(&self) -> &[String] {
        match &self.aggregate {
            Some(agg) => agg.grouping.fields(),
            None => &[],
        }
    }

    /// True when the ghost computes aggregates. This is an OVERLAY on the
    /// universal entry index — an aggregate ghost still keeps its per-record/
    /// per-group entries (readable via `SCAN GHOST`); the aggregates are
    /// additional. Replaces the `aggregate_specs.is_empty()` discriminator.
    pub fn is_aggregate(&self) -> bool {
        self.aggregate.is_some()
    }

    /// True only for a GROUP BY aggregate (`Some(Grouped)`). Covering ghosts and
    /// global aggregates are both false.
    pub fn is_grouped(&self) -> bool {
        matches!(&self.aggregate, Some(a) if a.grouping.is_grouped())
    }

    /// True when the ghost embeds a field projection (EMBED) in its entries.
    /// A modifier on the entry index, orthogonal to [`Self::is_aggregate`].
    pub fn has_projection(&self) -> bool {
        !self.projection.is_empty()
    }

    /// Classification derived from the lifecycle: `Declared` → `Permanent`,
    /// `Auto{Ephemeral}` → `Ephemeral`, `Auto{Promoted}` → `Promoted`.
    pub fn ghost_type(&self) -> GhostType {
        match &self.lifecycle {
            GhostLifecycle::Declared => GhostType::Permanent,
            GhostLifecycle::Auto {
                class: AutoClass::Ephemeral,
                ..
            } => GhostType::Ephemeral,
            GhostLifecycle::Auto {
                class: AutoClass::Promoted,
                ..
            } => GhostType::Promoted,
        }
    }

    /// TTL in seconds, or `None` for a `Declared` (permanent) ghost.
    pub fn ttl_seconds(&self) -> Option<u64> {
        match &self.lifecycle {
            GhostLifecycle::Declared => None,
            GhostLifecycle::Auto { ttl_seconds, .. } => Some(*ttl_seconds),
        }
    }

    /// True for an auto-created (`Auto`) ghost.
    pub fn is_auto(&self) -> bool {
        matches!(self.lifecycle, GhostLifecycle::Auto { .. })
    }

    /// Access telemetry, present only on `Auto` ghosts.
    pub fn telemetry(&self) -> Option<&AccessTelemetry> {
        match &self.lifecycle {
            GhostLifecycle::Auto { telemetry, .. } => Some(telemetry),
            GhostLifecycle::Declared => None,
        }
    }

    /// Mutable access telemetry, present only on `Auto` ghosts (the daily-bitmap
    /// rotate and access-count bump no-op on `Declared`).
    pub fn telemetry_mut(&mut self) -> Option<&mut AccessTelemetry> {
        match &mut self.lifecycle {
            GhostLifecycle::Auto { telemetry, .. } => Some(telemetry),
            GhostLifecycle::Declared => None,
        }
    }

    /// Set the lifecycle from a `(GhostType, ttl)` pair (the old flat form),
    /// preserving any accrued telemetry. Single source for `reclassify_lifecycle`
    /// and boot-time test setup.
    pub(crate) fn set_lifecycle(&mut self, ghost_type: GhostType, ttl_seconds: Option<u64>) {
        let telemetry = self.telemetry().cloned().unwrap_or_default();
        self.lifecycle = match ghost_type {
            GhostType::Permanent => GhostLifecycle::Declared,
            GhostType::Ephemeral => GhostLifecycle::Auto {
                class: AutoClass::Ephemeral,
                ttl_seconds: ttl_seconds.unwrap_or(EPHEMERAL_TTL_SECONDS),
                telemetry,
            },
            GhostType::Promoted => GhostLifecycle::Auto {
                class: AutoClass::Promoted,
                ttl_seconds: ttl_seconds.unwrap_or(EPHEMERAL_TTL_SECONDS),
                telemetry,
            },
        };
    }

    /// Memoise the core-typed filter tree from [`Self::filter`] on first use;
    /// subsequent calls are a no-op. `filter` is immutable per ghost, so the
    /// tree never goes stale. The write path calls this then evaluates
    /// membership against the cache — it never reconverts AST→core per write
    /// (audit P2-2). The single walker `matches_core_expr` handles the whole
    /// tree (And/Or/Not/In), so an OR/NOT ghost's membership is exact.
    pub(crate) fn ensure_core_filters(&mut self) {
        if self.core_filters_cache.is_none() {
            self.core_filters_cache = Some(crate::ops::to_core_expr(&self.filter));
        }
    }
}

/// The uniqueness suffix appended to a ghost entry's sort key, at the
/// granularity the ghost's output requires (audit P0-2).
///
/// - **Covering** ghosts (no `GROUP BY`) key one entry per record, so the
///   suffix is the record's spatial key: records sharing an ORDER BY value no
///   longer collapse to one key and overwrite each other.
/// - **Grouped** ghosts key one entry per group, so the suffix is the group
///   key: groups that happen to share an ORDER BY value (e.g. `ORDER BY
///   empresa_id GROUP BY empresa_id, rfc`) stay distinct, while records within
///   a group still fold to a single entry — their aggregate lives in the
///   group summaries / rollups, not the entry.
///
/// Returns `Borrowed` for the covering case so the hot insert path allocates
/// nothing extra; only grouped ghosts pay the group-key allocation.
fn ghost_entry_tiebreak<'a>(
    group_fields: &[String],
    fields: &std::collections::BTreeMap<String, Value>,
    spatial_key_bytes: &'a [u8],
) -> std::borrow::Cow<'a, [u8]> {
    if group_fields.is_empty() {
        std::borrow::Cow::Borrowed(spatial_key_bytes)
    } else {
        std::borrow::Cow::Owned(
            crate::aggregate_state::extract_group_key(fields, group_fields).into_bytes(),
        )
    }
}

/// Encode ghost entry value: spatial_key + optional projected fields.
/// When projection is empty, value = spatial_key (its current size — v0.5.x
/// shipped 18 bytes; v0.6.0-pre is 22 bytes per `SPATIAL_KEY_SIZE`).
/// When projection is set, value = spatial_key + postcard([Option<Value>; N]).
fn encode_ghost_value(spatial_key: &[u8], record: &Record, projection: &[String]) -> Vec<u8> {
    if projection.is_empty() {
        return spatial_key.to_vec();
    }
    let mut buf = Vec::with_capacity(spatial_key.len() + 64);
    buf.extend_from_slice(spatial_key);
    // Encode projected fields as: [count:u8] [field_name_len:u8][field_name][value_bytes]...
    // Using postcard for each individual Value to avoid Option wrapper issues.
    let mut projected: Vec<(String, Value)> = Vec::new();
    for f in projection {
        if let Some(v) = record.fields.get(f) {
            projected.push((f.clone(), v.clone()));
        }
    }
    buf.push(projected.len() as u8);
    for (name, value) in &projected {
        let name_bytes = name.as_bytes();
        buf.push(name_bytes.len() as u8);
        buf.extend_from_slice(name_bytes);
        if let Ok(vb) = postcard::to_allocvec(value) {
            buf.extend_from_slice(&(vb.len() as u16).to_be_bytes());
            buf.extend_from_slice(&vb);
        } else {
            buf.extend_from_slice(&0u16.to_be_bytes());
        }
    }
    buf
}

/// Decode projected fields from ghost entry value.
/// Returns a Record with only the projected fields (no spatial lookup needed).
fn decode_ghost_projection(
    entry_value: &[u8],
    spatial_key_len: usize,
    _projection: &[String],
    source_lobe: &str,
) -> Option<Record> {
    if entry_value.len() <= spatial_key_len {
        return None;
    }
    let payload = &entry_value[spatial_key_len..];
    if payload.is_empty() {
        return None;
    }

    let count = payload[0] as usize;
    let mut pos = 1;
    let mut fields = std::collections::BTreeMap::new();

    for _ in 0..count {
        if pos >= payload.len() {
            break;
        }
        let name_len = payload[pos] as usize;
        pos += 1;
        if pos + name_len > payload.len() {
            break;
        }
        let name = std::str::from_utf8(&payload[pos..pos + name_len])
            .ok()?
            .to_string();
        pos += name_len;
        if pos + 2 > payload.len() {
            break;
        }
        let val_len = u16::from_be_bytes([payload[pos], payload[pos + 1]]) as usize;
        pos += 2;
        if val_len > 0
            && pos + val_len <= payload.len()
            && let Ok(value) = postcard::from_bytes::<Value>(&payload[pos..pos + val_len])
        {
            fields.insert(name, value);
        }
        pos += val_len;
    }

    if fields.is_empty() {
        return None;
    }

    Some(Record {
        lid: xyzdb_core::lid::LID::from_raw(0),
        lobe_name: source_lobe.to_string(),
        fields,
        created_at: 0,
        updated_at: 0,
    })
}

/// Extract the sort value from a record for ghost ordering.
fn get_sort_value<'a>(record: &'a Record, order_by: &str) -> Option<&'a Value> {
    if order_by.is_empty() {
        return None;
    }
    record.fields.get(order_by)
}

/// One lobe's ghosts, behind its own lock. `Arc` so a caller can clone the
/// handle out from under the outer `shards` lock and then lock just this shard —
/// a write to lobe A never touches lobe B's lock (TANDA B: guaranteed per-lobe
/// decoupling of the hot write path, not probabilistic).
pub(crate) type GhostShard = Arc<RwLock<BTreeMap<String, GhostMeta>>>;

/// Manages all Ghost Lobes in a single shared keyspace (turba-engine Tree).
/// Each ghost is identified by a `ghost_id: u16` prefix on its keys.
///
/// Ghosts are sharded by `lobe_id`: `shards` maps a lobe to its own locked
/// submap, and `ghost_index` maps a ghost name to its lobe so a by-name op finds
/// its shard without scanning. **Lock-order invariant: always `ghost_index` →
/// shard, and `shards` (outer) → shard; never the reverse.** Every by-name
/// accessor takes the index read lock, resolves the lobe, then locks the shard;
/// no path holds a shard lock while taking the index or outer lock.
pub struct GhostLobeManager {
    /// `lobe_id` → that lobe's ghosts (name → meta), each behind its own lock.
    shards: RwLock<BTreeMap<u16, GhostShard>>,
    /// `ghost name` → `lobe_id`, so a by-name op routes to the right shard.
    ghost_index: RwLock<BTreeMap<String, u16>>,
    next_id: AtomicU16,
    keyspace: Option<Arc<Tree>>,
    /// Dictionary keyspace handle — lightweight ghosts store their group
    /// rollups here (see the `ROLLUP_PREFIX` block for why not the ghost
    /// keyspace). `None` only in unwired unit-test contexts; lightweight
    /// paths degrade to empty results rather than panic.
    dictionary: Option<Arc<Tree>>,
    /// Mirrors the engine's BULKMODE state. While on, notify_write skips
    /// ALL aggregate maintenance (in-RAM map and on-disk rollup RMW
    /// alike): a per-record disk RMW at bulk rates is fatal (~tens of
    /// rec/s observed at scale 1), and the bulk contract already requires
    /// REFRESH after the load, which rebuilds every aggregate from
    /// scratch. Covering-index entry inserts continue regardless.
    bulk_mode: std::sync::atomic::AtomicBool,
    pub overhead_tracker: OverheadTracker,
}

impl Default for GhostLobeManager {
    fn default() -> Self {
        Self::new()
    }
}

impl GhostLobeManager {
    pub fn new() -> Self {
        Self {
            shards: RwLock::new(BTreeMap::new()),
            ghost_index: RwLock::new(BTreeMap::new()),
            next_id: AtomicU16::new(1),
            keyspace: None,
            dictionary: None,
            bulk_mode: std::sync::atomic::AtomicBool::new(false),
            overhead_tracker: OverheadTracker::new(),
        }
    }

    /// Set the dictionary keyspace handle (called once at engine init).
    /// Lightweight-ghost rollup reads/writes require it.
    pub fn set_dictionary_arc(&mut self, dict: Arc<Tree>) {
        self.dictionary = Some(dict);
    }

    /// Mirror the engine's BULKMODE state (see the `bulk_mode` field).
    pub fn set_bulk_mode(&self, on: bool) {
        self.bulk_mode.store(on, Ordering::Relaxed);
    }

    /// Alias used from engine.rs at init.
    pub fn set_keyspace_arc(&mut self, ks: Arc<Tree>) {
        self.keyspace = Some(ks);
    }

    pub(crate) fn ks(&self) -> Result<&Arc<Tree>> {
        self.keyspace
            .as_ref()
            .ok_or_else(|| XyzError::Internal("ghost keyspace not initialized".into()))
    }

    /// Allocate a new ghost_id.
    fn alloc_id(&self) -> u16 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    // ─── Sharded ghost store (TANDA B) ───────────────────────────────────
    // Lock order everywhere: ghost_index → shards(outer) → shard-inner. No path
    // holds a shard lock while taking the index or outer lock, so there is no
    // deadlock cycle. By-name ops route through the index; whole-store scans
    // snapshot the shard handles then lock each in turn (per-shard sequential,
    // non-atomic across lobes — every such consumer is best-effort/per-ghost).

    /// Clone the shard handle for `lobe_id`, if the lobe has one.
    fn lobe_shard(&self, lobe_id: u16) -> Option<GhostShard> {
        self.shards.read().get(&lobe_id).cloned()
    }

    /// Clone the shard for `lobe_id`, creating an empty one if absent.
    fn lobe_shard_or_create(&self, lobe_id: u16) -> GhostShard {
        if let Some(s) = self.shards.read().get(&lobe_id).cloned() {
            return s;
        }
        self.shards
            .write()
            .entry(lobe_id)
            .or_insert_with(|| Arc::new(RwLock::new(BTreeMap::new())))
            .clone()
    }

    /// Resolve a ghost name to its shard via the index (lock order: index → shard).
    fn shard_for_name(&self, name: &str) -> Option<GhostShard> {
        let lobe_id = *self.ghost_index.read().get(name)?;
        self.lobe_shard(lobe_id)
    }

    /// Snapshot every shard handle (outer read only). Callers lock each shard in
    /// turn: a whole-store scan is per-shard sequential, NOT a cross-lobe atomic
    /// view. Safe because every whole-store consumer (TTL reaper, LRU,
    /// promotable, SHOW GHOSTS, counts) is best-effort or per-ghost and never
    /// relies on seeing all lobes at one instant.
    fn all_shards(&self) -> Vec<GhostShard> {
        self.shards.read().values().cloned().collect()
    }

    /// True when no ghost exists in any lobe.
    pub fn is_empty(&self) -> bool {
        self.all_shards().iter().all(|s| s.read().is_empty())
    }

    /// Total ghost count across all lobes (per-shard sequential, non-atomic).
    pub fn ghost_count(&self) -> usize {
        self.all_shards().iter().map(|s| s.read().len()).sum()
    }

    /// Whether a ghost named `name` is registered.
    pub fn contains_ghost(&self, name: &str) -> bool {
        self.ghost_index.read().contains_key(name)
    }

    /// Names of all registered ghosts (from the index; order unspecified).
    #[cfg(test)]
    pub fn ghost_names(&self) -> Vec<String> {
        self.ghost_index.read().keys().cloned().collect()
    }

    /// Run `f` against the ghost named `name`, locking only its shard for read.
    pub fn with_ghost<R>(&self, name: &str, f: impl FnOnce(&GhostMeta) -> R) -> Option<R> {
        let shard = self.shard_for_name(name)?;
        let guard = shard.read();
        guard.get(name).map(f)
    }

    /// Run `f` against the ghost named `name` mutably, locking only its shard.
    pub fn with_ghost_mut<R>(&self, name: &str, f: impl FnOnce(&mut GhostMeta) -> R) -> Option<R> {
        let shard = self.shard_for_name(name)?;
        let mut guard = shard.write();
        guard.get_mut(name).map(f)
    }

    /// Insert (or replace) a ghost. Holds the index write lock across the shard
    /// insert (index → shard order) so a concurrent by-name reader sees the
    /// ghost either fully present or fully absent, never index-without-shard.
    pub(crate) fn insert_ghost(&self, meta: GhostMeta) {
        let name = meta.name.clone();
        let lobe_id = meta.lobe_id;
        let mut index = self.ghost_index.write();
        let shard = self.lobe_shard_or_create(lobe_id);
        shard.write().insert(name.clone(), meta);
        index.insert(name, lobe_id);
    }

    /// Remove a ghost by name, returning its meta. Holds the index write lock
    /// across the shard removal (atomic w.r.t. by-name readers).
    pub(crate) fn remove_ghost_entry(&self, name: &str) -> Option<GhostMeta> {
        let mut index = self.ghost_index.write();
        let lobe_id = *index.get(name)?;
        let removed = self
            .lobe_shard(lobe_id)
            .and_then(|s| s.write().remove(name));
        index.remove(name);
        removed
    }

    /// Iterate every ghost (read), per-shard sequential (non-atomic snapshot).
    fn for_each_ghost(&self, mut f: impl FnMut(&str, &GhostMeta)) {
        for shard in self.all_shards() {
            for (name, meta) in shard.read().iter() {
                f(name, meta);
            }
        }
    }

    /// Iterate every ghost (write), per-shard sequential (non-atomic snapshot).
    fn for_each_ghost_mut(&self, mut f: impl FnMut(&mut GhostMeta)) {
        for shard in self.all_shards() {
            for meta in shard.write().values_mut() {
                f(meta);
            }
        }
    }

    /// Estimated resident bytes of in-RAM aggregate state across all
    /// ghosts (global aggregates + per-group summaries). At scale,
    /// high-cardinality GROUP BY ghosts make `group_summaries` the
    /// dominant un-modelled VmRSS term (~2.3 GB measured on the scale-1
    /// bench), which is why ram_budget carries it as its own component.
    pub fn aggregate_state_bytes(&self) -> u64 {
        // Amortised BTreeMap per-entry overhead (node slots + parents).
        const MAP_ENTRY_OVERHEAD: usize = 32;
        let mut total: u64 = 0;
        self.for_each_ghost(|_, m| {
            // Covering ghosts (no aggregate overlay) hold no aggregate state.
            let Some(agg) = &m.aggregate else {
                return;
            };
            // Spilled ghosts hold no in-RAM group map (rollups live on disk),
            // so only InRam summaries count toward resident bytes.
            let groups: usize = match &agg.residency {
                Residency::InRam(map) => map
                    .iter()
                    .map(|(k, st)| {
                        MAP_ENTRY_OVERHEAD
                            + std::mem::size_of::<String>()
                            + k.len()
                            + st.estimated_bytes()
                    })
                    .sum(),
                Residency::Spilled => 0,
            };
            total += (agg.global_aggregates.estimated_bytes() + groups) as u64;
        });
        total
    }

    /// List all Ghost Lobes.
    pub fn list(&self) -> Vec<GhostInfo> {
        let mut out = Vec::new();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as i64;
        self.for_each_ghost(|_, g| {
            out.push(GhostInfo {
                name: g.name.clone(),
                source_lobe: g.source_lobe.clone(),
                order_by: g.order_by_field.clone(),
                record_count: g.index_count,
                filter_count: filter_expr_condition_count(&g.filter),
                aggregates_stale: g.aggregates_dirty(),
                maintenance_degraded: g.maintenance_degraded,
                metric_order: g
                    .metric_order
                    .as_ref()
                    .map(|mo| (mo.label.clone(), mo.descending)),
                // Age only when the order is declared AND emitted; a declared but
                // un-emitted (stale) order reports `None`.
                order_age_secs: g.order_emitted_at.map(|ts| (now - ts) / 1_000_000),
            });
        });
        out
    }

    /// Get the membership filter used to create a ghost (for REFRESH).
    pub fn get_filter(&self, name: &str) -> Result<ast::FilterExpr> {
        self.with_ghost(name, |meta| meta.filter.clone())
            .ok_or_else(|| XyzError::GhostNotFound(name.to_string()))
    }

    /// Get the ghost_id for a named ghost.
    #[cfg(test)]
    pub fn ghost_id(&self, name: &str) -> Option<u16> {
        self.with_ghost(name, |m| m.ghost_id)
    }

    /// Flush the shared ghost keyspace to disk.
    pub fn flush(&self) -> Result<()> {
        if let Some(ks) = &self.keyspace {
            ks.seal_active();
            ks.flush_sealed()
                .map_err(|e| XyzError::Storage(e.to_string()))?;
        }
        Ok(())
    }
}

/// Specification for a single ghost in a batch create.
pub struct GhostSpec {
    pub name: String,
    pub filter: ast::FilterExpr,
    pub order_by_field: String,
    pub sort_inverted: bool,
    pub is_auto: bool,
    pub aggregate_specs: Vec<crate::aggregate_state::Metric>,
    pub group_fields: Vec<String>,
    /// Fields to embed in ghost entries. Empty = reference-only (18 bytes).
    pub projection: Vec<String>,
    /// `ORDER BY <metric>` declaration, if any (see [`MetricOrder`]).
    pub metric_order: Option<MetricOrder>,
}

/// Result type for pre-computed aggregates.
pub enum GhostAggregates {
    /// Global aggregates (no grouping).
    Global(crate::aggregate_state::AggregateState),
    /// Per-group aggregates keyed by group key string.
    Grouped(std::collections::BTreeMap<String, crate::aggregate_state::AggregateState>),
}

pub struct GhostInfo {
    pub name: String,
    pub source_lobe: String,
    pub order_by: String,
    pub record_count: u64,
    pub filter_count: usize,
    /// Aggregates may be stale (Min/Max dirtied by a delete — option D).
    pub aggregates_stale: bool,
    /// An incremental maintenance op failed; REFRESH to rebuild.
    pub maintenance_degraded: bool,
    /// `ORDER BY <metric>` declaration: (canonical label, descending). `None` for
    /// a ghost without a metric-ordered rollup.
    pub metric_order: Option<(String, bool)>,
    /// Age in seconds of the metric-ordered rollup since its last emit, or `None`
    /// when there is no order OR it is stale (declared but not currently emitted —
    /// TOP falls back to O(M)).
    pub order_age_secs: Option<i64>,
}

// ─── CompactionObserver for fused COMPACT+GHOST ───────────────────────────

// NOTE: CompactionObserver for fused COMPACT+GHOST was removed after benchmarking
// showed it was slower than sequential (Mutex overhead on 405M entries + interleaved I/O).
// The observer trait remains in turba-engine for future use with a lock-free design.
