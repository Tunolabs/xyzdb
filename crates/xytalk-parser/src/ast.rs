// AST types for xyTalk. Produced by the parser, consumed by the engine.

/// Top-level result of parsing one xyTalk statement.
#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    Put(PutStmt),
    PutBatch(PutBatchStmt),
    Find(FindStmt),
    Pull(PullStmt),
    Scan(ScanStmt),
    Set(SetStmt),
    Delete(DeleteStmt),
    /// `PURGE "lobe"` — empty a whole lobe. The explicit, hard-to-typo spelling
    /// for total deletion; `DELETE` now requires a WHERE, so emptying a lobe can
    /// never happen by omission.
    Purge(PurgeStmt),
    /// `FETCH "a","b","c" WHERE … [AS {n1,n2,n3}]` — read N co-located lobes in
    /// one call, returned as one record with a named section per lobe.
    Fetch(FetchStmt),
    Link(LinkStmt),
    Anchor(AnchorStmt),
    Gravity(GravityStmt),
    /// `VECTOR <field> IN "<lobe>"` — declare the lobe's searchable embedding
    /// field (hoisted to the V3 record prefix for exact NEAREST). A foundational
    /// axis sibling to gravity, not an index.
    Vector(VectorStmt),
    /// `SATELLITE BY <field> IN "<lobe>"` — declare the lobe's sub-gravity axis:
    /// the single field whose value sub-buckets a large gravity bucket, so a
    /// bounded query scans one satellite instead of the whole parent. A
    /// foundational axis sibling to gravity/vector; one per lobe; declared on an
    /// empty lobe. See `docs/xytalk-spec.md` §2.21.
    Satellite(SatelliteStmt),
    Lobe(LobeStmt),
    Show(ShowStmt),
    AutoAnchorApply(AutoAnchorApplyStmt),
    CreateGhost(CreateGhostStmt),
    ScanGhost(ScanGhostStmt),
    RefreshGhost(String),
    DropGhost(String),
    Analyze(String),
    Compact,
    /// SCRUB — verify on-disk integrity (SST block checksums + MANIFEST) of
    /// every keyspace and report corruption. Read-only; alert, never repair.
    Scrub,
    /// BULKMODE ON / BULKMODE OFF — disable/enable auto-compaction for bulk loads.
    BulkMode(bool),
    /// V5: MIGRATE or MIGRATE "lobe" — rewrite records to latest on-disk format
    Migrate(Option<String>),
    /// V5: INCACHE "lobe" [WHERE ...] — load records into RecordCache
    InCache(InCacheStmt),
    /// V5: OUTCACHE "lobe" — evict lobe from RecordCache
    OutCache(String),
    /// V3: PIN campo1, campo2 IN "lobe"
    Pin(PinStmt),
    /// V3: UNPIN campo1 IN "lobe"
    Unpin(UnpinStmt),
    Pipeline(Vec<PipelineStep>),
}

/// `PURGE "lobe"` — remove every record in `lobe` (ghosts and indexes are
/// maintained, exactly as a WHERE-matching DELETE would).
#[derive(Debug, Clone, PartialEq)]
pub struct PurgeStmt {
    pub lobe: String,
}

/// `FETCH "a","b","c" WHERE <expr> [AS {n1,n2,n3}]` — resolve the shared `WHERE`
/// against each named lobe and return one record whose fields are a section per
/// lobe (a list of the matching records). `names`, when present, renames the
/// sections positionally and must have one entry per lobe; otherwise each
/// section is named by its lobe. One call, N co-located reads packaged
/// server-side — no composition logic beyond the packaging.
#[derive(Debug, Clone, PartialEq)]
pub struct FetchStmt {
    pub lobes: Vec<String>,
    pub filter_expr: Option<FilterExpr>,
    pub names: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PinStmt {
    pub fields: Vec<String>,
    pub lobe: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UnpinStmt {
    pub fields: Vec<String>,
    pub lobe: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InCacheStmt {
    pub lobe: String,
    pub filter_expr: Option<FilterExpr>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateGhostStmt {
    pub name: String,
    pub source_lobe: String,
    /// Membership predicate. `And([])` when the CREATE had no WHERE (covers the
    /// whole lobe). A full `FilterExpr` so ghosts can carry OR/NOT/In, not just
    /// flat-AND.
    pub filter: FilterExpr,
    pub order_by: String,
    pub sort_descending: bool,
    /// `ORDER BY <metric>` where the target is a declared aggregate (`sum(monto)`)
    /// rather than a record field: keep the groups ordered by that metric so
    /// `TOP n BY <metric>` reads O(N). `None` = the classic field order (`order_by`).
    pub order_metric: Option<TopBy>,
    pub group_by: Vec<String>,
    pub aggregates: Vec<Aggregate>,
    /// Fields to embed in ghost entries (avoids point reads on read_topn).
    pub embed: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScanGhostStmt {
    pub name: String,
    /// Optional WHERE on the ghost read. Full OR/NOT/IN tree (xyTalk v1 P1):
    /// AND-pure pushes into `read_topn` (early-out at the limit); OR/NOT reads
    /// the ordered entries then walker-filters and truncates.
    pub filter_expr: Option<FilterExpr>,
    pub limit: Option<u64>,
}

/// A field in a PUT statement. The `gravity` flag indicates co-location by value.
#[derive(Debug, Clone, PartialEq)]
pub struct PutField {
    pub name: String,
    pub value: Literal,
    /// If true, gravity_hash is derived from this field's value (co-location).
    pub gravity: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PutStmt {
    pub fields: Vec<PutField>,
    pub lobe: String,
    pub link: Option<LinkClause>,
    pub on_conflict: Option<OnConflict>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PutBatchStmt {
    pub records: Vec<Vec<PutField>>,
    pub lobe: String,
    pub link: Option<LinkClause>,
    pub on_conflict: Option<OnConflict>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FindStmt {
    pub target: FindTarget,
    pub filters: Vec<Filter>, // FIND stays Vec<Filter> (AND-only — routes to anchor/gravity)
    /// Page size when paginating with `CURSOR`. Without `cursor` the
    /// engine ignores `limit` (FIND is a fast-lookup verb; full-page
    /// truncation is a SCAN concern).
    pub limit: Option<u64>,
    /// Opaque resume token (v0.2.5.2). Accepted only when the predicate
    /// matches the gravity-bounded fast path (Finding 13). Anchor lookup
    /// and no-fast-path predicates reject the cursor explicitly. See
    /// `xyzdb-engine/src/ops/find.rs::execute_find_paginated`.
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FindTarget {
    /// FIND "workspace" WHERE ... or FIND Company WHERE ...
    Lobe(String),
    /// FIND LID("...")
    ByLid(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct PullStmt {
    pub target: Option<FindTarget>,
    pub depth: u32,
    pub only: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScanStmt {
    pub lobe: String,
    pub filter_expr: Option<FilterExpr>, // V4: OR/NOT/AND tree (None = no WHERE)
    pub order_by: Option<OrderBy>,
    pub limit: Option<u64>,
    /// v0.2.5.1: opaque pagination token returned by a previous SCAN.
    /// Decoded by the engine; the parser only carries the raw string.
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderBy {
    pub field: String,
    pub descending: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SetStmt {
    pub target: Option<FindTarget>,
    pub assignments: Vec<(String, Literal)>,
    /// Optional WHERE on the standalone form (`SET "lobe" f = v WHERE …`).
    /// `None` in pipeline form. Full OR/NOT/IN tree (xyTalk v1 P1): AND-pure
    /// resolves via the anchor/gravity fast path, OR/NOT falls to scan+walker.
    pub filter_expr: Option<FilterExpr>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeleteStmt {
    pub target: Option<FindTarget>,
    /// The standalone form's WHERE (`DELETE "lobe" WHERE …`), required by P7 —
    /// the dispatch rejects a WHERE-less standalone DELETE and points to PURGE.
    /// `None` in pipeline form (the upstream records are the selection). Full
    /// OR/NOT/IN tree (xyTalk v1 P1).
    pub filter_expr: Option<FilterExpr>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LinkStmt {
    pub source: FindTarget,
    pub target: FindTarget,
    pub relation_name: String,
    /// Optional WHERE on the source side (`LINK "src" WHERE … TO "tgt" AS "rel"`).
    /// Full OR/NOT/IN tree (xyTalk v1 P1).
    pub source_filter_expr: Option<FilterExpr>,
    /// Optional WHERE on the target side (`LINK "src" TO "tgt" WHERE … AS "rel"`).
    pub target_filter_expr: Option<FilterExpr>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AnchorStmt {
    pub field: String,
    pub lobe: String,
}

/// `VECTOR <field> IN "<lobe>"` — names the lobe's searchable embedding field.
#[derive(Debug, Clone, PartialEq)]
pub struct VectorStmt {
    pub field: String,
    pub lobe: String,
}

/// `SATELLITE BY <field> IN "<lobe>"` — names the lobe's sub-gravity axis: the
/// single field whose value splits a gravity bucket into ordered sub-buckets.
/// One field, `BY` keyword like gravity, single value like vector.
#[derive(Debug, Clone, PartialEq)]
pub struct SatelliteStmt {
    pub field: String,
    pub lobe: String,
}

/// `GRAVITY BY <expr> IN "lobe"` — declare how a lobe derives its gravity hash
/// (the v0.8 keel). Declared before the first write. `<expr>` is a field name
/// (Raw), `lower(field)` / `trim(field)` (Normalized), or `(a, b, ...)`
/// (Composite). A bare `*field` in PUT remains sugar for `Raw(field)`; this is
/// the explicit, richer form that also resolves the two-`*` footgun.
#[derive(Debug, Clone, PartialEq)]
pub struct GravityStmt {
    pub lobe: String,
    pub spec: GravitySpecAst,
}

/// Surface form of a gravity spec. The engine maps this to its own
/// `GravitySpec` (the parser crate does not depend on the engine).
#[derive(Debug, Clone, PartialEq)]
pub enum GravitySpecAst {
    Raw(String),
    Normalized(String, GravityTransform),
    Composite(Vec<String>),
}

/// Identity-safe value folds for `Normalized` gravity.
#[derive(Debug, Clone, PartialEq)]
pub enum GravityTransform {
    Lower,
    Trim,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LobeStmt {
    pub name: String,
    pub hint: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ShowStmt {
    Lobes,
    Anchors(String),
    Throttle,
    Ghosts,
    ScanStats,
    /// V3: SHOW PROFILE "lobe"
    Profile(String),
    /// V5: SHOW CACHE
    Cache,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AutoAnchorApplyStmt {
    pub field: String,
    pub lobe: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Filter {
    pub field: String,
    pub op: FilterOp,
    pub value: Literal,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FilterOp {
    Eq,
    Neq,
    Gt,
    Gte,
    Lt,
    Lte,
    IsNull,    // V4
    IsNotNull, // V4
    Contains,  // V4: List CONTAINS element
    In,        // scalar field ∈ a list: `x IN (a, b, c)`
}

#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Int(i64),
    Float(f64),
    Text(String),
    Bool(bool),
    Timestamp(String),
    Lid(String),
    Null,                        // V4
    List(Vec<Literal>),          // V4
    Map(Vec<(String, Literal)>), // V4: ordered pairs for determinism
    /// A bound-parameter placeholder, e.g. `$name` or `$1`. Transient: it is
    /// substituted with its bound value by `bind_params` before execution.
    /// Any `Param` that reaches the executor is an unbound-parameter bug and
    /// must be rejected, never silently treated as a value (anti-injection).
    Param(String), // S1
}

/// V4: Boolean filter expression tree with AND/OR/NOT and precedence.
/// NOT > AND > OR. Parentheses override precedence.
#[derive(Debug, Clone, PartialEq)]
pub enum FilterExpr {
    Condition(Filter),
    And(Vec<FilterExpr>),
    Or(Vec<FilterExpr>),
    Not(Box<FilterExpr>),
}

impl FilterExpr {
    /// Convert a Vec<Filter> (AND-only, V3 compat) to FilterExpr.
    pub fn from_filters(filters: Vec<Filter>) -> Self {
        if filters.len() == 1 {
            FilterExpr::Condition(filters.into_iter().next().unwrap())
        } else {
            FilterExpr::And(filters.into_iter().map(FilterExpr::Condition).collect())
        }
    }

    /// Extract flat AND conditions (for ghost routing backward compat).
    /// Returns None if the expression contains OR or NOT.
    pub fn as_flat_and(&self) -> Option<Vec<&Filter>> {
        match self {
            FilterExpr::Condition(f) => Some(vec![f]),
            FilterExpr::And(exprs) => {
                let mut result = Vec::new();
                for e in exprs {
                    result.extend(e.as_flat_and()?);
                }
                Some(result)
            }
            FilterExpr::Or(_) | FilterExpr::Not(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LinkClause {
    pub target: FindTarget,
    pub filters: Vec<Filter>,
    pub relation_name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum OnConflict {
    Update,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PipelineStep {
    Find(FindStmt),
    Pull(PullStmt),
    Set(SetStmt),
    Delete(DeleteStmt),
    Scan(ScanStmt),
    ScanGhost(ScanGhostStmt),
    Aggregate(Vec<Aggregate>),
    GroupBy(Vec<String>), // V4: GROUP BY field1, field2
    Nearest(NearestStmt), // V8: semantic top-k over the (gravity-bounded) scan
    Follow(FollowStmt),   // V8: cross-entity expansion (follow a reference to another lobe)
    Top(TopStmt),         // top-N over the grouped aggregate result, by a metric
    Shape(ShapeStmt),     // v1: project each record to a chosen set of fields
}

/// `| SHAPE {field1, field2, …}` — project each record down to the named fields
/// (the read-side mirror of `PUT {…}`: braces put a shape in, braces take a
/// shape out). Names not present on a record are simply absent from the result;
/// it is a projection, not a filter.
#[derive(Debug, Clone, PartialEq)]
pub struct ShapeStmt {
    pub fields: Vec<String>,
}

/// Which aggregate column a `TAKE` step orders by: a metric named by its function
/// (`sum(monto)` → the canonical label the engine resolves) or by its `AS` alias.
#[derive(Debug, Clone, PartialEq)]
pub enum TopBy {
    Metric(AggregateFunc),
    Alias(String),
}

/// `… | GROUP BY … | AGGREGATE … | TAKE <n> [BY <metric> [DESC|ASC]]` — the
/// canonical top-N step (`TOP` is a live alias). With `BY <metric>`, keep the
/// `n` groups with the highest (DESC, default) or lowest (ASC) value of one of
/// the declared aggregate metrics, server-side, so the client never sorts N-of-M
/// groups. Total order is (metric, then group key) so ties at the N/N+1 cut are
/// deterministic and the result equals sort-all-then-truncate.
///
/// Without `BY` (`| TAKE n`), it truncates the stream to the first `n` items in
/// their existing order — the pipeline form of `LIMIT`, valid on grouped rows or
/// on a plain record stream. `descending` is unused in that case.
#[derive(Debug, Clone, PartialEq)]
pub struct TopStmt {
    pub n: u64,
    pub by: Option<TopBy>,
    pub descending: bool,
}

/// `FOLLOW <field> TO "<lobe>" ON <target_field>` — cross-entity (cross-bucket)
/// expansion: for each current record, resolve its `field` value as
/// `target_field` in `lobe` and fetch those records. This is the relational
/// bridge `PULL` cannot cross (PULL stays inside one gravity bucket); it turns
/// "chat → its cited document (a different entity)" into one pipeline step.
#[derive(Debug, Clone, PartialEq)]
pub struct FollowStmt {
    pub field: String,
    pub lobe: String,
    pub target_field: String,
}

/// The query side of `NEAREST` — what the records' `field` is compared against.
///
/// The engine never embeds text: the caller embeds the query (with the same
/// model the corpus used) and supplies the vector, either inline or — better —
/// as a bound parameter passed out-of-band so a 768-float literal never lands
/// in the query string.
#[derive(Debug, Clone, PartialEq)]
pub enum NearestQuery {
    /// Inline list literal, e.g. `[0.1, -0.4, …]`.
    Vector(Literal),
    /// A bound parameter `$name`; the vector travels via the protocol and is
    /// substituted before execution.
    Param(String),
    /// `REF "id"` — "more like this": use, as the query, the embedding of the
    /// scanned record whose field value uniquely equals `id` (and exclude that
    /// record from the results).
    Ref(String),
}

/// `NEAREST(field, query, k, metric)` — keep the `k` records whose `field`
/// embedding is most similar to the `query` vector under `metric` (the raw
/// name `cosine`/`dot`/`l2`, resolved by the engine).
#[derive(Debug, Clone, PartialEq)]
pub struct NearestStmt {
    pub field: String,
    pub query: NearestQuery,
    pub k: u64,
    pub metric: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AggregateFunc {
    Count,
    Sum(String),
    Avg(String),
    Min(String),
    Max(String),
}

/// One metric in an `AGGREGATE` clause: an aggregate function, an optional
/// per-metric `WHERE` gate, and an optional `AS` alias for the result column.
///
/// `filter`/`alias` are `None` for a plain `func()`, so a single-metric ghost or
/// pipeline is byte-for-byte the pre-existing behavior. A per-metric `WHERE`
/// composes with the query/ghost header predicate as `header AND metric`; the
/// alias names the result column so several metrics of the same op (e.g. two
/// `count()` with different filters) stay distinguishable.
#[derive(Debug, Clone, PartialEq)]
pub struct Aggregate {
    pub func: AggregateFunc,
    pub filter: Option<FilterExpr>,
    pub alias: Option<String>,
}
