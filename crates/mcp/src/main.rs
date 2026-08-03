//! xyzdb-mcp — MCP server for xyzDB.
//!
//! Serves **5 tools** — `stats`, `query`, `snapshot`, `list_lobes`,
//! `describe_lobe` — and **3 resources** (`xyzdb://lobes`, `xyzdb://stats`,
//! and the `xyzdb://lobes/{name}` template). Async lifecycle with error
//! mapping and telemetry; `query` carries the xyTalk grammar reference and
//! is gated by `--query-policy`, the other tools wrap `SHOW`/snapshot calls.
//!
//! Two connection modes:
//! - `--embed <PATH>`: the process owns the data dir (LSM lock holder).
//! - `--connect <HOST:PORT>`: the process is a TCP client of an
//!   external `xyzdb-server` already serving the data dir.
//!
//! See `docs/mcp-integration.md` for tools, resources, and modes.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use clap::{ArgGroup, Parser};
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters,
    model::{
        AnnotateAble, CallToolResult, Content, Implementation, ListResourceTemplatesResult,
        ListResourcesResult, PaginatedRequestParams, ProtocolVersion, RawResource,
        RawResourceTemplate, ReadResourceRequestParams, ReadResourceResult, ResourceContents,
        ServerCapabilities, ServerInfo,
    },
    schemars,
    service::RequestContext,
    tool, tool_handler, tool_router,
    transport::stdio,
};
use xyzdb_engine::engine::Engine;

mod connect;
mod describe;
mod error;
mod grammar;
mod policy;
mod redact;
mod resources;
mod serialize;
mod telemetry;

use crate::error::engine_to_mcp;
use crate::telemetry::{ToolCallEvent, ToolCallResult, emit_tool_call};

/// CLI args. The two modes are mutually exclusive; clap's `ArgGroup`
/// with `multiple = false` enforces it.
#[derive(Parser, Debug)]
#[command(name = "xyzdb-mcp", version, about = "MCP server for xyzDB")]
#[command(group(ArgGroup::new("source").required(true).multiple(false).args(["embed", "connect"])))]
struct Cli {
    /// Path to the xyzDB data directory. The MCP server opens the
    /// data dir directly (LSM lock holder); not compatible with
    /// running an external `xyzdb-server` against the same dir.
    #[arg(long, value_name = "PATH")]
    embed: Option<PathBuf>,

    /// Address of an external `xyzdb-server` to forward queries to
    /// over the V2 TCP protocol. Format: `host:port`.
    /// Use this mode when `xyzdb-server` already holds the data dir.
    #[arg(long, value_name = "HOST:PORT")]
    connect: Option<String>,

    /// Skip the connectivity probe at startup (`--connect` mode only).
    /// By default, Pillar 1 issues a single STATS query at startup
    /// to surface reachability issues early. Use this flag in CI or
    /// when the upstream is intentionally not yet available.
    #[arg(long, default_value_t = false)]
    no_probe: bool,

    /// Log full xyTalk statements and cursor tokens at TRACE level on
    /// target `xyzdb_mcp::statements`. **Off by default**; the default
    /// posture is privacy-clean (statements appear only as xxh3-64
    /// fingerprints + first verb). Development-only flag.
    ///
    /// Cross-actor leak guard: rejected at startup when `--connect`
    /// targets a non-loopback host. Statements from other actors
    /// concurrently sharing the same upstream `xyzdb-server` would
    /// otherwise land in this MCP process's stderr. Use `--connect
    /// 127.0.0.1` (loopback) or `--embed` (single-actor by definition)
    /// for development logging.
    #[arg(long, default_value_t = false)]
    log_statements: bool,

    /// Maximum wall-clock budget for a single `query` tool invocation,
    /// in milliseconds. The engine call is wrapped in
    /// `tokio::time::timeout`; a budget exceedance surfaces to the
    /// agent as `INTERNAL_ERROR` with message `"query timed out after
    /// <N>ms"` and is labelled `TIMEOUT` in telemetry.
    ///
    /// Only the `query` tool is bounded — `stats`, `list_lobes`, and
    /// `describe_lobe` complete in < 50 ms in practice (they wrap one
    /// or three `SHOW`/snapshot calls). The timeout is a guard against
    /// a pathological xyTalk statement (large unbounded `SCAN`,
    /// runaway aggregation) tying up the dispatcher.
    #[arg(long, default_value_t = 30_000, value_name = "MS")]
    query_timeout_ms: u64,

    /// Restrict which xyTalk verbs the `query` tool accepts, to protect the
    /// data from accidental destruction by an automated caller (S1b):
    /// `full` (default — all verbs), `no-destructive` (block DELETE / DROP),
    /// `read-only` (block every mutation). Enforced at the MCP layer before
    /// the statement reaches the engine.
    #[arg(long, value_enum, default_value = "full")]
    query_policy: policy::QueryPolicy,
}

/// Where the MCP handler gets its data from.
///
/// `Embed`: in-process `Engine` direct calls. `--embed` mode.
/// `Connect`: TCP client to a remote `xyzdb-server`. `--connect` mode.
#[derive(Clone)]
enum EngineSource {
    Embed(Arc<Engine>),
    Connect { host: String, port: u16 },
}

/// MCP server handler. The `EngineSource` is cheap to clone (Arc bump
/// or two small strings + integer) so each tool-call future gets its
/// own copy without lifetime gymnastics.
#[derive(Clone)]
struct XyzdbServer {
    source: EngineSource,
    /// Whether the operator passed `--log-statements`. Read by the
    /// `query` tool to decide if it should emit a TRACE-level event
    /// with the raw statement + cursor token in addition to the
    /// default INFO-level redacted event.
    log_statements: bool,
    /// Per-call budget for the `query` tool, in milliseconds. Enforced
    /// via `tokio::time::timeout` around `query_inner`. Other tools
    /// are not subject to this budget (they run bounded work).
    query_timeout_ms: u64,
    /// Restricted-verb posture for the `query` tool (S1b). `Full` by default;
    /// `NoDestructive`/`ReadOnly` reject mutating statements before the engine.
    query_policy: policy::QueryPolicy,
}

#[tool_router]
impl XyzdbServer {
    fn new(
        source: EngineSource,
        log_statements: bool,
        query_timeout_ms: u64,
        query_policy: policy::QueryPolicy,
    ) -> Self {
        Self {
            source,
            log_statements,
            query_timeout_ms,
            query_policy,
        }
    }

    /// Stats snapshot. The two modes diverge here:
    /// - Embed: in-process `Engine::stats_snapshot()`, isolated from
    ///   the tokio reactor via `spawn_blocking`.
    /// - Connect: V2 query `STATS` with `FORMAT_JSON` to the remote
    ///   server; the JSON body is forwarded verbatim to the MCP client.
    #[tool(
        description = "Snapshot of xyzDB engine internals: keyspace stats (memtables, SSTables, compaction counters), block cache, ghosts, sync thread health, process memory, cgroup memory. Same JSON shape as the /stats endpoint on xyzdb-server's TCP port. Prefer this tool over `query` with `SHOW STATS` — the response is structured JSON, not human text."
    )]
    async fn stats(&self) -> Result<CallToolResult, McpError> {
        let request_id = uuid::Uuid::now_v7().to_string();
        let started = Instant::now();
        let result = self.stats_inner().await;
        let latency_ms = started.elapsed().as_secs_f64() * 1000.0;
        emit_tool_call(ToolCallEvent {
            tool: "stats",
            request_id: &request_id,
            latency_ms,
            result: tool_call_outcome(&result),
            query_hash: None,
            query_kind: None,
            cursor_present: None,
            records_returned: None,
        });
        result
    }

    /// Execute an arbitrary xyTalk statement against the engine.
    ///
    /// The two modes diverge here:
    /// - Embed: `Engine::run(&statement)` via `spawn_blocking`; result
    ///   serialised to JSON via `serialize::query_result_to_json`.
    /// - Connect: V2 query with `FORMAT_JSON` against the upstream
    ///   `xyzdb-server`; server JSON forwarded verbatim.
    ///
    /// Cursor handling: when the `cursor` argument is present, the
    /// statement is augmented with `CURSOR "<token>"` before dispatch.
    /// If the statement already contains a `CURSOR` clause, the
    /// argument is ignored (statement-embedded cursor wins). The xyTalk
    /// parser will reject duplicate `CURSOR` clauses naturally.
    // The description is the xyTalk grammar reference served to MCP clients. It
    // is kept honest against the parser by the tests in `grammar.rs`, which read
    // this exact text back via the generated `query_tool_attr()` accessor and
    // parse every advertised form. rmcp's `#[tool]` needs a string literal here,
    // so the text lives inline; the anti-drift guarantee is the test, not a const.
    #[tool(
        description = r#"Execute an xyTalk statement against xyzDB. xyTalk is a pipeline query language: a source verb (FIND locates, SCAN traverses) feeds transforms left-to-right through `|`. Records co-locate by gravity — a field written `*field` — so an entity and everything about it sit in one bucket, reachable in one read. WHERE filters (AND/OR/NOT/IN, parentheses). Common verbs:
- PUT {field: value, ...} IN "lobe" — insert a record. Prefix a field with * to mark it as gravity (co-location key).
- PUT BATCH IN "lobe" [{...}, {...}] — insert many records atomically.
- FIND "lobe" WHERE field = X [AND ...] — point or gravity-bounded lookup. AND-only (anchor/gravity fast path); an OR/NOT in a FIND is a parse error that points to SCAN. Anchor lookup is O(1); gravity lookup is a bounded range scan.
- FIND "lobe" WHERE gravity_field = X LIMIT n CURSOR "<token>" — paginated fast-path lookup over a gravity bucket.
- SCAN "lobe" WHERE filter [ORDER BY field] [LIMIT n] [CURSOR "<token>"] — iterate with optional sort and pagination. WHERE supports AND/OR/NOT, IN [a, b], and parentheses. ORDER BY requires LIMIT.
- SET "lobe" field = value WHERE filter — update fields in matching records. WHERE supports the full OR/NOT/IN tree.
- DELETE "lobe" WHERE filter — remove matching records. WHERE is required.
- PURGE "lobe" — empty a whole lobe (the explicit total-delete verb).
- FIND "lobe" WHERE key = X | PULL [depth=N] [only=Type] — traverse an entity's co-located subtree (a range scan over everything sharing its gravity). This is gravity, the differentiator: related records are already physically adjacent, so PULL reads the whole graph around an entity in one pass, not a JOIN. `PULL FROM "lobe"` is the standalone source form.
- SCAN "lobe" ... | FOLLOW field TO "other_lobe" ON target_field — expand across lobes by following a stored reference; the read-side counterpart to LINK (LINK writes a relationship, FOLLOW walks it).
- LINK "src" WHERE ... TO "dst" WHERE ... AS "relation" — create a relationship.
- FETCH "a", "b", "c" WHERE key = X [AS {n1, n2, n3}] — read several co-located lobes in one call. Returns one record with a named section (a list of the matching records) per lobe; section names default to the lobe names. WHERE is required (the shared co-location key).
- SCAN ... | AGGREGATE count(), sum(field), avg(field), min(field), max(field) — pipeline aggregation. GROUP BY field also supported. count(*) is an accepted alias of count().
- SCAN ... | GROUP BY field | AGGREGATE ... | TAKE n BY metric [DESC|ASC] — server-side top-N over the grouped result; DESC is the default. TAKE n with no BY truncates the stream to n (pipeline LIMIT). TOP is a deprecated alias of TAKE.
- SCAN ... | SHAPE {field1, field2} — project each returned record down to the named fields (the read-side mirror of PUT {...}). Fields absent from a record are simply omitted.
- LOBE "name" [HINT="..."] — create a lobe (a co-located bucket). A PUT to an unknown lobe also creates it, so this is optional.
- ANCHOR "field" UNIQUE IN "lobe" — declare a UNIQUE field, which gives FIND an O(1) lookup on it. Declare before bulk-loading; to populate one over already-loaded records use AUTOANCHOR APPLY "field" IN "lobe".
- VECTOR field IN "lobe" — declare a field as a searchable f32 embedding; afterwards PUT {field: [floats], ...} stores the vector for that record.
- SCAN "lobe" [WHERE ...] [LIMIT n] | NEAREST k BY field TO ($q | [q1, q2, ...] | REF "lid") [USING metric] — semantic top-k over the scanned set. metric is cosine (the default when USING is omitted), dot, or l2; results carry a similarity score. Exact within a gravity bucket (not ANN). IMPORTANT — xyzDB never computes embeddings. You always supply the query vector yourself, in one of three forms: a bound `$param`, an inline `[f1, f2, ...]` list, or `REF "id"` to reuse a stored record's vector — embedded with the same model the corpus used. Passing raw text to NEAREST does not work and is the single most common mistake. The metric is chosen per query via `USING cosine|dot|l2` (cosine is the default). The function form NEAREST(field, query, k, metric) is an accepted alias.
- CREATE GHOST "name" FROM "lobe" [WHERE ...] [| GROUP BY ...] [| AGGREGATE ...] | TAKE BY (field | metric) [DESC|ASC] — declare a materialised view (a saved query). The clause form (ORDER BY ... GROUP BY ... AGGREGATE ...) is an accepted alias.
- GRAVITY BY expr IN "lobe" — declare the on-disk co-location key (a field, a composite (a, b), or a transform lower(field) / trim(field)).
- SHOW LOBES / ANCHORS IN "lobe" / GHOSTS / PROFILE "lobe" — schema introspection.
- Operational verbs — NOT needed for correctness. The engine self-manages compaction, indexing, and caching; reach for these only when explicitly asked to tune or maintain, and last of all: COMPACT, ANALYZE "lobe", PIN field IN "lobe" / UNPIN, INCACHE / OUTCACHE, SCAN GHOST / REFRESH GHOST / DROP GHOST.

Pagination: SCAN and FIND-on-gravity may return a cursor when the result exceeds LIMIT. Default LIMIT is 1000 records; hard cap is 10000. Pass the cursor verbatim back in the next call to fetch the following page. Cursor + ORDER BY and cursor + ghost routing are not supported (use plain SCAN with LIMIT for large sorted results).

The statement parameter accepts the full xyTalk grammar including writes (PUT/SET/DELETE/PURGE/LINK). The server may also set `--query-policy`: `no-destructive` blocks DELETE/DROP, `read-only` blocks every write. A statement refused by the policy returns an error that is a permission limit, not a syntax problem — do not rewrite the query to get around it. (Separately, deployment read-only posture may also be enforced at the trust boundary via filesystem permissions or a server-side role.)

Tool selection: prefer the dedicated `list_lobes`, `describe_lobe`, and `stats` tools for schema discovery and engine introspection — they parse the `SHOW` output into structured JSON. Use `query` for actual data operations (PUT/FIND/SCAN/NEAREST/AGGREGATE/TAKE/SET/DELETE/PURGE/LINK and VECTOR/GRAVITY/CREATE GHOST declarations) and for SHOW commands not covered by a dedicated tool."#
    )]
    async fn query(&self, params: Parameters<QueryRequest>) -> Result<CallToolResult, McpError> {
        let request_id = uuid::Uuid::now_v7().to_string();
        let req = params.0;

        // S1b — query-policy guard. Under a restricted policy, parse the
        // statement and reject any class the policy forbids BEFORE it reaches
        // the engine. Classification is by AST (not substring). A statement we
        // cannot parse is refused under a restricted policy rather than
        // forwarded unverified; under `Full` the engine reports parse errors.
        if !matches!(self.query_policy, policy::QueryPolicy::Full) {
            match xytalk_parser::parse(&req.statement) {
                Ok(stmt) => {
                    let class = policy::classify(&stmt);
                    if !self.query_policy.allows(class) {
                        return Err(McpError::invalid_params(
                            format!(
                                "query-policy '{}' forbids a {:?} statement (this MCP server is \
                                 configured to protect the data; rerun the operator with a less \
                                 restrictive --query-policy if this write is intended)",
                                self.query_policy.as_str(),
                                class
                            ),
                            None,
                        ));
                    }
                }
                Err(_) => {
                    return Err(McpError::invalid_params(
                        "query rejected: statement could not be parsed for policy classification \
                         under a restricted --query-policy"
                            .to_string(),
                        None,
                    ));
                }
            }
        }

        // Default-on redaction: compute the privacy-preserving fingerprint
        // BEFORE the engine runs. The full statement appears in stderr only
        // if --log-statements is set (TRACE-level event below).
        let q_hash = redact::query_hash(&req.statement);
        let q_kind = redact::query_kind(&req.statement);
        let cursor_present =
            req.cursor.is_some() || req.statement.to_uppercase().contains("CURSOR");

        if self.log_statements {
            telemetry::trace_statement(&request_id, &req.statement, req.cursor.as_deref());
        }

        let started = Instant::now();
        let timeout = Duration::from_millis(self.query_timeout_ms);
        let (result, records_returned) =
            match tokio::time::timeout(timeout, self.query_inner(req)).await {
                Ok(pair) => pair,
                Err(_) => {
                    // Timeout. The spawned blocking task continues to
                    // completion in the runtime's blocking pool — we
                    // intentionally do not try to abort it (tokio cannot
                    // pre-empt blocking work). The agent gets a clear
                    // INVALID_PARAMS-shaped error; future calls are
                    // unaffected.
                    let err = McpError::internal_error(
                        format!("query timed out after {}ms", self.query_timeout_ms),
                        Some(serde_json::json!({ "timeout": true })),
                    );
                    (Err(err), None)
                }
            };
        let latency_ms = started.elapsed().as_secs_f64() * 1000.0;
        emit_tool_call(ToolCallEvent {
            tool: "query",
            request_id: &request_id,
            latency_ms,
            result: tool_call_outcome(&result),
            query_hash: Some(&q_hash),
            query_kind: Some(&q_kind),
            cursor_present: Some(cursor_present),
            records_returned,
        });
        result
    }

    /// Backup tool (C8) — create a hot, consistent snapshot of the embedded
    /// database. `--embed` only; `--connect` uses `xyzdb-cli admin snapshot`.
    #[tool(
        description = "Create a hot, consistent backup of the embedded xyzDB database: hard-links the live SSTs and copies the WAL into snapshots/<name>/ under the data dir — no downtime, point-in-time consistent. The name must be a single path component (no '/', '..'). Only in --embed mode; in --connect mode run `xyzdb-cli admin snapshot create` against the server. Restore is offline via `xyzdb-cli admin snapshot restore`. Use this to back up the database before risky operations or on a schedule."
    )]
    async fn snapshot(
        &self,
        params: Parameters<SnapshotRequest>,
    ) -> Result<CallToolResult, McpError> {
        let request_id = uuid::Uuid::now_v7().to_string();
        let started = Instant::now();
        let result = self.snapshot_inner(params.0).await;
        let latency_ms = started.elapsed().as_secs_f64() * 1000.0;
        emit_tool_call(ToolCallEvent {
            tool: "snapshot",
            request_id: &request_id,
            latency_ms,
            result: tool_call_outcome(&result),
            query_hash: None,
            query_kind: None,
            cursor_present: None,
            records_returned: None,
        });
        result
    }

    /// Discovery tool — list all lobes registered in the database with
    /// their name, declared anchor count, and optional hint. The agent
    /// uses this as the first call to find what data is available;
    /// `describe_lobe` follows for full schema (Pillar 4).
    #[tool(
        description = "List all lobes registered in xyzDB. Returns each lobe's name, anchor count, and optional descriptive hint. Use this as the FIRST discovery call to find what data the database holds — prefer it over `query` with `SHOW LOBES` (the response is structured JSON, not parsed human text). For full lobe schema (anchors, ghosts, profile), call `describe_lobe` next."
    )]
    async fn list_lobes(&self) -> Result<CallToolResult, McpError> {
        let request_id = uuid::Uuid::now_v7().to_string();
        let started = Instant::now();
        let result = self.list_lobes_inner().await;
        let latency_ms = started.elapsed().as_secs_f64() * 1000.0;
        emit_tool_call(ToolCallEvent {
            tool: "list_lobes",
            request_id: &request_id,
            latency_ms,
            result: tool_call_outcome(&result),
            query_hash: None,
            query_kind: None,
            cursor_present: None,
            records_returned: None,
        });
        result
    }

    /// Full lobe schema introspection — anchors, ghosts (filtered to
    /// those whose source_lobe matches), and profile (pinned fields,
    /// learned scan patterns, active ghost count). Composes three SHOW
    /// queries with **per-field independent fallibility**: if any one
    /// of the three sub-calls fails, the corresponding field is replaced
    /// by `{"error": "..."}` while the other two carry their data
    /// unchanged. The lobe-not-found case is surfaced as a top-level
    /// `INVALID_PARAMS` (no partial body), gated by a SHOW LOBES
    /// pre-flight check.
    #[tool(
        description = "Full schema for a single lobe: anchors, ghosts (those whose source_lobe matches), profile (pinned fields, learned scan patterns, active ghost count), and the searchable vector field. Returns a structured object with `anchors`, `ghosts`, `profile` (each independently fallible — on failure replaced by `{\"error\": \"...\"}`) and top-level `vector` and `satellite`: `vector` is `null` if the lobe has no searchable field, else `{\"field\": ..., \"dim\": ...}`; `satellite` is `null` unless the lobe declares a sub-gravity axis, in which case it names that field. The satellite axis changes WHICH QUERY IS CHEAP: an equality on it (`field = X`) reads one sub-range of the gravity bucket, while a range (`field < X`) sweeps the whole parent — so prefer equality on the satellite field when you can. A `null` `dim` means the dimension is not fixed yet (declared but no embedding written) — you may choose it on the first write; a set `dim` means every `NEAREST` query vector must match it (the engine never embeds; you supply the vector). If the lobe does not exist, the tool returns INVALID_PARAMS with no partial body. Prefer this over `query` with `SHOW ANCHORS` / `SHOW GHOSTS` / `SHOW PROFILE` separately — it composes the three SHOW calls and parses the result. Use after `list_lobes` to plan queries."
    )]
    async fn describe_lobe(
        &self,
        params: Parameters<DescribeLobeRequest>,
    ) -> Result<CallToolResult, McpError> {
        let request_id = uuid::Uuid::now_v7().to_string();
        let started = Instant::now();
        let result = self.describe_lobe_inner(params.0).await;
        let latency_ms = started.elapsed().as_secs_f64() * 1000.0;
        emit_tool_call(ToolCallEvent {
            tool: "describe_lobe",
            request_id: &request_id,
            latency_ms,
            result: tool_call_outcome(&result),
            query_hash: None,
            query_kind: None,
            cursor_present: None,
            records_returned: None,
        });
        result
    }
}

impl XyzdbServer {
    /// Build the stats JSON payload. Shared between the `stats` tool
    /// and the `xyzdb://stats` resource — same data, two surfaces.
    async fn stats_json(&self) -> Result<String, McpError> {
        match &self.source {
            EngineSource::Embed(engine) => {
                let engine = engine.clone();
                let snapshot = tokio::task::spawn_blocking(move || engine.stats_snapshot())
                    .await
                    .map_err(|e| {
                        McpError::internal_error(format!("stats_snapshot join failed: {e}"), None)
                    })?;
                serde_json::to_string_pretty(&snapshot).map_err(|e| {
                    McpError::internal_error(format!("stats serialize failed: {e}"), None)
                })
            }
            EngineSource::Connect { host, port } => {
                let bytes = connect::query_json(host, *port, "STATS")
                    .await
                    .map_err(|e| {
                        // Surface as INVALID_PARAMS only if the message
                        // looks like a server-side validation; otherwise
                        // INTERNAL_ERROR. The connect helper returns
                        // anyhow::Error so we keep the original wording.
                        McpError::internal_error(format!("connect-mode STATS failed: {e}"), None)
                    })?;
                String::from_utf8(bytes).map_err(|e| {
                    McpError::internal_error(
                        format!("xyzdb-server returned non-utf8 stats body: {e}"),
                        None,
                    )
                })
            }
        }
    }

    /// Inner stats logic separated from telemetry wrapper for clarity.
    async fn stats_inner(&self) -> Result<CallToolResult, McpError> {
        let json = self.stats_json().await?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Inner snapshot logic. Embed: in-process `Engine::create_snapshot` via
    /// `spawn_blocking` (the create path is hardened — H12 compaction drain +
    /// S3 name validation). Connect: not supported (no xyTalk verb for an admin
    /// snapshot); the caller is told to use `xyzdb-cli`.
    async fn snapshot_inner(&self, req: SnapshotRequest) -> Result<CallToolResult, McpError> {
        match &self.source {
            EngineSource::Embed(engine) => {
                let engine = engine.clone();
                let name = req.name;
                let join = tokio::task::spawn_blocking(move || engine.create_snapshot(&name)).await;
                match join {
                    Err(e) => Err(McpError::internal_error(
                        format!("snapshot join failed: {e}"),
                        None,
                    )),
                    // Display carries the precise reason (invalid name, exists,
                    // I/O); surfaced to the caller verbatim.
                    Ok(Err(err)) => Err(McpError::internal_error(
                        format!("snapshot failed: {err}"),
                        None,
                    )),
                    Ok(Ok(meta)) => {
                        let value = serde_json::json!({
                            "snapshot": meta.name,
                            "wal_bytes": meta.wal_bytes,
                            "keyspaces": meta
                                .keyspaces
                                .iter()
                                .map(|k| serde_json::json!({
                                    "keyspace": k.keyspace,
                                    "ssts": k.sst_filenames.len(),
                                }))
                                .collect::<Vec<_>>(),
                        });
                        match serde_json::to_string_pretty(&value) {
                            Ok(json) => Ok(CallToolResult::success(vec![Content::text(json)])),
                            Err(e) => Err(McpError::internal_error(
                                format!("snapshot serialize failed: {e}"),
                                None,
                            )),
                        }
                    }
                }
            }
            EngineSource::Connect { .. } => Err(McpError::invalid_params(
                "snapshot is only available in --embed mode; in --connect mode run \
                 `xyzdb-cli admin snapshot create <name>` against the server"
                    .to_string(),
                None,
            )),
        }
    }

    /// Inner query logic. Augments the statement with a cursor clause
    /// when one is supplied as an argument (and the statement does not
    /// already contain its own); dispatches to embed or connect paths.
    ///
    /// Returns `(result, records_returned)`. `records_returned` is
    /// `Some(n)` for the embed path (the engine's `QueryResult` is
    /// inspected before serialisation) and `None` for the connect
    /// path (the upstream JSON is forwarded verbatim; counting
    /// records would require parsing it back, which is wasted work
    /// for telemetry-only data — the upstream `xyzdb-server` already
    /// has its own per-call telemetry).
    async fn query_inner(
        &self,
        req: QueryRequest,
    ) -> (Result<CallToolResult, McpError>, Option<u64>) {
        let stmt = augment_with_cursor(&req.statement, req.cursor.as_deref());

        // Bound params are an --embed-only feature for now (Phase 1). Reject
        // them on --connect rather than silently dropping them, which would be
        // a false sense of injection safety.
        if matches!(self.source, EngineSource::Connect { .. })
            && req.params.as_ref().is_some_and(|p| !p.is_empty())
        {
            return (
                Err(McpError::invalid_params(
                    "bound params require --embed; --connect does not support them yet".to_string(),
                    None,
                )),
                None,
            );
        }

        match &self.source {
            EngineSource::Embed(engine) => {
                let engine = engine.clone();
                let stmt_clone = stmt.clone();
                let bound = req
                    .params
                    .as_ref()
                    .map(json_params_to_values)
                    .unwrap_or_default();
                let started = std::time::Instant::now();
                let join = tokio::task::spawn_blocking(move || {
                    engine.run_with_params(&stmt_clone, &bound)
                })
                .await;
                let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;

                match join {
                    Err(e) => (
                        Err(McpError::internal_error(
                            format!("engine join failed: {e}"),
                            None,
                        )),
                        None,
                    ),
                    Ok(engine_result) => match engine_result {
                        Err(err) => (Err(engine_to_mcp(err)), None),
                        Ok(qr) => {
                            let count = count_records(&qr);
                            let value = serialize::query_result_to_json(&qr, elapsed_ms);
                            match serde_json::to_string_pretty(&value) {
                                Ok(json) => (
                                    Ok(CallToolResult::success(vec![Content::text(json)])),
                                    Some(count),
                                ),
                                Err(e) => (
                                    Err(McpError::internal_error(
                                        format!("query serialize failed: {e}"),
                                        None,
                                    )),
                                    None,
                                ),
                            }
                        }
                    },
                }
            }
            EngineSource::Connect { host, port } => {
                let result = async {
                    let bytes = connect::query_json(host, *port, &stmt).await.map_err(|e| {
                        McpError::internal_error(format!("connect-mode query failed: {e}"), None)
                    })?;
                    let json = String::from_utf8(bytes).map_err(|e| {
                        McpError::internal_error(
                            format!("xyzdb-server returned non-utf8 body: {e}"),
                            None,
                        )
                    })?;
                    Ok(CallToolResult::success(vec![Content::text(json)]))
                }
                .await;
                (result, None)
            }
        }
    }

    /// Inner list_lobes logic. Dispatches `SHOW LOBES` to the engine
    /// (embed) or to the upstream server (connect), parses the
    /// human-formatted Info lines into `LobeSummary` structs, returns
    /// the structured response as JSON.
    async fn list_lobes_inner(&self) -> Result<CallToolResult, McpError> {
        let json = self.list_lobes_json().await?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Build the list_lobes JSON payload. Shared between the `list_lobes`
    /// tool and the `xyzdb://lobes` resource.
    async fn list_lobes_json(&self) -> Result<String, McpError> {
        let lines: Vec<String> = match &self.source {
            EngineSource::Embed(engine) => {
                let engine = engine.clone();
                let result = tokio::task::spawn_blocking(move || engine.run("SHOW LOBES"))
                    .await
                    .map_err(|e| {
                        McpError::internal_error(format!("list_lobes join failed: {e}"), None)
                    })?;
                let qr = result.map_err(engine_to_mcp)?;
                match qr {
                    xyzdb_core::result::QueryResult::Info(lines) => lines,
                    other => {
                        return Err(McpError::internal_error(
                            format!("SHOW LOBES returned unexpected variant: {:?}", other),
                            None,
                        ));
                    }
                }
            }
            EngineSource::Connect { host, port } => {
                let bytes = connect::query_json(host, *port, "SHOW LOBES")
                    .await
                    .map_err(|e| {
                        McpError::internal_error(
                            format!("connect-mode SHOW LOBES failed: {e}"),
                            None,
                        )
                    })?;
                let json: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| {
                    McpError::internal_error(
                        format!("server returned non-JSON for SHOW LOBES: {e}"),
                        None,
                    )
                })?;
                json.get("info")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| {
                        McpError::internal_error(
                            "SHOW LOBES JSON missing 'info' array".to_string(),
                            None,
                        )
                    })?
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            }
        };

        let lobes = parse_show_lobes(&lines);
        let response = LobesListResponse {
            count: lobes.len(),
            lobes,
        };
        serde_json::to_string_pretty(&response).map_err(|e| {
            McpError::internal_error(format!("list_lobes serialize failed: {e}"), None)
        })
    }

    /// Inner describe_lobe logic. Steps:
    /// 1. SHOW LOBES pre-flight to confirm the lobe exists; on miss,
    ///    return `INVALID_PARAMS` so the agent gets a clear top-level
    ///    error rather than a partial body with three error fields.
    /// 2. Fire SHOW ANCHORS / SHOW GHOSTS / SHOW PROFILE sequentially.
    ///    Each is wrapped in `PartialResult`: success → parsed payload,
    ///    failure → `{"error": "..."}`. The three sub-calls are NOT
    ///    parallelised — embed mode shares one `Engine` and `spawn_blocking`
    ///    threads, and the savings from parallel SHOW calls are not
    ///    worth the engine-side contention or the connect-mode TCP
    ///    fan-out.
    async fn describe_lobe_inner(
        &self,
        req: DescribeLobeRequest,
    ) -> Result<CallToolResult, McpError> {
        let json = self.describe_lobe_json(req).await?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Build the describe_lobe JSON payload. Shared between the
    /// `describe_lobe` tool and the `xyzdb://lobes/{name}` resource.
    async fn describe_lobe_json(&self, req: DescribeLobeRequest) -> Result<String, McpError> {
        let lobe = req.lobe.trim();
        if lobe.is_empty() {
            return Err(McpError::invalid_params(
                "describe_lobe: `lobe` argument must be non-empty".to_string(),
                None,
            ));
        }

        if !self.lobe_exists(lobe).await? {
            return Err(McpError::invalid_params(
                format!("describe_lobe: lobe '{lobe}' not found"),
                None,
            ));
        }

        let anchors = match self
            .fetch_show_lines(&format!(r#"SHOW ANCHORS IN "{lobe}""#))
            .await
        {
            Ok(lines) => describe::PartialResult::Ok(describe::parse_show_anchors(&lines)),
            Err(e) => describe::PartialResult::Err {
                error: format!("SHOW ANCHORS failed: {}", e.message),
            },
        };

        let ghosts = match self.fetch_show_lines("SHOW GHOSTS").await {
            Ok(lines) => {
                describe::PartialResult::Ok(describe::parse_show_ghosts_filtered(&lines, lobe))
            }
            Err(e) => describe::PartialResult::Err {
                error: format!("SHOW GHOSTS failed: {}", e.message),
            },
        };

        let profile = match self
            .fetch_show_lines(&format!(r#"SHOW PROFILE "{lobe}""#))
            .await
        {
            Ok(lines) => describe::PartialResult::Ok(describe::parse_show_profile(&lines)),
            Err(e) => describe::PartialResult::Err {
                error: format!("SHOW PROFILE failed: {}", e.message),
            },
        };

        // Hoist the parsed vector field to the top level so an agent reads
        // vector capability without digging into `profile`. If SHOW PROFILE
        // failed there is nothing to report → null.
        let vector = match &profile {
            describe::PartialResult::Ok(p) => p.vector.clone(),
            describe::PartialResult::Err { .. } => None,
        };
        // Same hoist as `vector`: the axis is contract for the caller, so it must
        // be readable at the top level rather than buried in `profile`.
        let satellite = match &profile {
            describe::PartialResult::Ok(p) => p.satellite.clone(),
            describe::PartialResult::Err { .. } => None,
        };
        // The PRIMARY axis, hoisted for the same reason as the other two and with
        // more force: it decides whether a query is bounded at all. Without it a
        // caller could see the satellite — which only means something relative to
        // the gravity bucket it subdivides — and not the bucket.
        let gravity = match &profile {
            describe::PartialResult::Ok(p) => p.gravity.clone(),
            describe::PartialResult::Err { .. } => None,
        };
        let response = describe::LobeDescription {
            name: lobe.to_string(),
            anchors,
            ghosts,
            profile,
            vector,
            satellite,
            gravity,
        };
        serde_json::to_string_pretty(&response).map_err(|e| {
            McpError::internal_error(format!("describe_lobe serialize failed: {e}"), None)
        })
    }

    /// SHOW LOBES + parse + membership check. Reuses `parse_show_lobes`
    /// from Pillar 3. Returns true iff the lobe is registered.
    async fn lobe_exists(&self, lobe: &str) -> Result<bool, McpError> {
        let lines = self.fetch_show_lines("SHOW LOBES").await?;
        Ok(parse_show_lobes(&lines).iter().any(|l| l.name == lobe))
    }

    /// Run a SHOW-class statement and return its `Info` lines. Both
    /// modes converge on `Vec<String>`:
    /// - Embed: dispatch via `Engine::run` under `spawn_blocking`,
    ///   destructure the `QueryResult::Info(lines)` variant.
    /// - Connect: V2 query with `FORMAT_JSON`; the upstream's JSON has
    ///   shape `{"info": ["line1", ...]}`, so we extract that array.
    async fn fetch_show_lines(&self, stmt: &str) -> Result<Vec<String>, McpError> {
        match &self.source {
            EngineSource::Embed(engine) => {
                let engine = engine.clone();
                let stmt_owned = stmt.to_string();
                let result = tokio::task::spawn_blocking(move || engine.run(&stmt_owned))
                    .await
                    .map_err(|e| {
                        McpError::internal_error(format!("show join failed: {e}"), None)
                    })?;
                let qr = result.map_err(engine_to_mcp)?;
                match qr {
                    xyzdb_core::result::QueryResult::Info(lines) => Ok(lines),
                    other => Err(McpError::internal_error(
                        format!("'{stmt}' returned unexpected variant: {:?}", other),
                        None,
                    )),
                }
            }
            EngineSource::Connect { host, port } => {
                let bytes = connect::query_json(host, *port, stmt).await.map_err(|e| {
                    McpError::internal_error(format!("connect-mode '{stmt}' failed: {e}"), None)
                })?;
                let json: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| {
                    McpError::internal_error(
                        format!("server returned non-JSON for '{stmt}': {e}"),
                        None,
                    )
                })?;
                let lines = json
                    .get("info")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| {
                        McpError::internal_error(
                            format!("'{stmt}' JSON missing 'info' array"),
                            None,
                        )
                    })?
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect();
                Ok(lines)
            }
        }
    }

    /// Resource dispatch by URI string. Reuses the `*_json` helpers so
    /// resource and tool surfaces stay in lock-step. URIs not matching
    /// any of the three patterns return `INVALID_PARAMS`.
    async fn read_resource_inner(&self, uri: &str) -> Result<ReadResourceResult, McpError> {
        let json = if uri == resources::URI_LOBES {
            self.list_lobes_json().await?
        } else if uri == resources::URI_STATS {
            self.stats_json().await?
        } else if let Some(name) = resources::parse_lobe_uri(uri) {
            self.describe_lobe_json(DescribeLobeRequest {
                lobe: name.to_string(),
            })
            .await?
        } else {
            return Err(McpError::invalid_params(
                format!("unknown resource URI: {uri}"),
                None,
            ));
        };

        Ok(ReadResourceResult::new(vec![
            ResourceContents::text(json, uri).with_mime_type("application/json"),
        ]))
    }
}

/// Output shape for the `list_lobes` tool.
#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
struct LobesListResponse {
    /// Total number of lobes registered.
    count: usize,
    /// One entry per lobe with name + declared anchor count + optional hint.
    lobes: Vec<LobeSummary>,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
struct LobeSummary {
    /// Lobe name as declared via LOBE / created on first PUT.
    name: String,
    /// Number of UNIQUE anchors declared on this lobe.
    anchor_count: u32,
    /// Optional descriptive hint provided to LOBE "name" HINT="...".
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<String>,
}

/// Parse the human-formatted output of `SHOW LOBES`. The engine emits
/// lines like:
///
/// ```text
/// Lobes:
///   0. owners (2 anchors) — owner records
///   1. events (0 anchors)
/// ```
///
/// Skips the header line and any line that does not match the
/// `id. name (n anchors)[ — hint]` shape.
fn parse_show_lobes(lines: &[String]) -> Vec<LobeSummary> {
    lines
        .iter()
        .filter_map(|line| parse_lobe_line(line))
        .collect()
}

fn parse_lobe_line(line: &str) -> Option<LobeSummary> {
    let trimmed = line.trim_start();
    // Header line "Lobes:" has no `. ` separator → returns None below.
    let dot_pos = trimmed.find(". ")?;
    let id_str = &trimmed[..dot_pos];
    // Numeric id sanity check; rejects non-data lines gracefully.
    if id_str.parse::<u32>().is_err() {
        return None;
    }
    let rest = &trimmed[dot_pos + 2..];
    let paren_open = rest.find(" (")?;
    let name = rest[..paren_open].to_string();
    let after_open = &rest[paren_open + 2..];
    let paren_close = after_open.find(')')?;
    let inside = &after_open[..paren_close];
    // "N anchors" → first whitespace-separated token.
    let anchor_count: u32 = inside.split_whitespace().next()?.parse().ok()?;
    let after_close = &after_open[paren_close + 1..];
    // Optional " — hint". The em-dash is the literal U+2014 char emitted
    // by the engine; we use split_once on " — " to get the hint.
    let hint = after_close
        .split_once(" — ")
        .map(|(_, h)| h.trim().to_string())
        .filter(|h| !h.is_empty());

    Some(LobeSummary {
        name,
        anchor_count,
        hint,
    })
}

/// Append a `CURSOR "<token>"` clause to a statement when an explicit
/// cursor argument was supplied, unless the statement already contains
/// a CURSOR clause. Statement-embedded cursor wins.
/// Convert the MCP JSON params object into engine `Value`s for `$param` binding.
fn json_params_to_values(
    params: &serde_json::Map<String, serde_json::Value>,
) -> std::collections::HashMap<String, xyzdb_core::value::Value> {
    params
        .iter()
        .map(|(k, v)| (k.clone(), json_to_value(v)))
        .collect()
}

/// Map a `serde_json::Value` to an engine `Value` (integers stay `Int`, other
/// numbers become `Float`; objects/arrays recurse). Timestamps/bytes are not
/// representable here and surface as the engine's "type not bindable" error.
fn json_to_value(v: &serde_json::Value) -> xyzdb_core::value::Value {
    use xyzdb_core::value::Value;
    match v {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => n
            .as_i64()
            .map(Value::Int)
            .unwrap_or_else(|| Value::Float(n.as_f64().unwrap_or(0.0))),
        serde_json::Value::String(s) => Value::Text(s.clone()),
        serde_json::Value::Array(a) => Value::List(a.iter().map(json_to_value).collect()),
        serde_json::Value::Object(o) => Value::Map(
            o.iter()
                .map(|(k, v)| (k.clone(), json_to_value(v)))
                .collect(),
        ),
    }
}

fn augment_with_cursor(stmt: &str, cursor: Option<&str>) -> String {
    match cursor {
        Some(token) if !stmt.to_uppercase().contains("CURSOR") => {
            format!(r#"{stmt} CURSOR "{token}""#)
        }
        _ => stmt.to_string(),
    }
}

/// Input schema for the `query` tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct QueryRequest {
    /// xyTalk statement to execute. See the tool description for the
    /// language surface and pagination rules.
    statement: String,
    /// Optional opaque pagination cursor returned by a previous query
    /// call. Round-trip the value verbatim. Ignored if the statement
    /// already contains a CURSOR clause.
    #[serde(default)]
    cursor: Option<String>,
    /// Optional bound parameters: `{"name": value}` substituted for `$name`
    /// placeholders before execution (anti-injection — untrusted text never
    /// enters the statement as syntax). Supported in `--embed` only.
    #[serde(default)]
    params: Option<serde_json::Map<String, serde_json::Value>>,
}

/// Input schema for the `describe_lobe` tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SnapshotRequest {
    /// Name for the snapshot directory under `<data dir>/snapshots/`. Must be a
    /// single path component — no `/`, `\`, `..`, `.`, or leading separator
    /// (rejected for path-traversal safety).
    name: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct DescribeLobeRequest {
    /// Lobe name as it appears in SHOW LOBES output. Case-sensitive.
    /// If the lobe does not exist, the tool returns INVALID_PARAMS.
    lobe: String,
}

/// Map a tool-call result to its telemetry outcome.
fn tool_call_outcome(result: &Result<CallToolResult, McpError>) -> ToolCallResult {
    match result {
        Ok(_) => ToolCallResult::Success,
        Err(e) => ToolCallResult::Error {
            code: error_code_label(e),
        },
    }
}

/// Telemetry record-count derivation per `QueryResult` variant. Counts
/// records, not records-content; the value lands in the
/// `records_returned` telemetry field. `Info` (SHOW) returns 0 because
/// info lines are metadata, not records. `Ok` returns 1 if there is a
/// LID, 0 otherwise.
fn count_records(qr: &xyzdb_core::result::QueryResult) -> u64 {
    use xyzdb_core::result::QueryResult::*;
    match qr {
        Ok { lid, .. } => {
            if lid.is_some() {
                1
            } else {
                0
            }
        }
        BatchOk { count, .. } => *count as u64,
        Records(v) => v.len() as u64,
        Aggregation(_) => 1,
        Info(_) => 0,
        GroupedAggregation(v) => v.len() as u64,
        PaginatedRecords { records, .. } => records.len() as u64,
    }
}

/// Best-effort static label for an MCP error code, used only for
/// telemetry. The numeric code stays on the wire; this is the human
/// label that appears in the structured log.
///
/// Timeout detection: the `query` tool tags its timeout error with
/// `data: {"timeout": true}` so we can surface `TIMEOUT` in
/// telemetry while keeping the wire-level code as the standard
/// `INTERNAL_ERROR` (no JSON-RPC TIMEOUT code exists, and inventing
/// one would break agent error-handling that switches on `code`).
fn error_code_label(err: &McpError) -> &'static str {
    use rmcp::model::ErrorCode;
    if let Some(data) = err.data.as_ref()
        && data.get("timeout").and_then(|v| v.as_bool()) == Some(true)
    {
        return "TIMEOUT";
    }
    match err.code {
        ErrorCode::INVALID_PARAMS => "INVALID_PARAMS",
        ErrorCode::INTERNAL_ERROR => "INTERNAL_ERROR",
        ErrorCode::METHOD_NOT_FOUND => "METHOD_NOT_FOUND",
        ErrorCode::INVALID_REQUEST => "INVALID_REQUEST",
        ErrorCode::PARSE_ERROR => "PARSE_ERROR",
        _ => "OTHER",
    }
}

#[tool_handler]
impl ServerHandler for XyzdbServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_server_info(Implementation::new("xyzdb-mcp", env!("CARGO_PKG_VERSION")))
        .with_protocol_version(ProtocolVersion::V_2024_11_05)
        .with_instructions(
            "xyzDB MCP server. Tools: stats, query, list_lobes, describe_lobe, snapshot. \
                 Resources: xyzdb://lobes, xyzdb://stats, and template xyzdb://lobes/{name}. \
                 Two modes: --embed and --connect. \
                 See docs/mcp-integration.md for tools, resources, and modes."
                .to_string(),
        )
    }

    /// Concrete resources: list of lobes + stats snapshot. The
    /// per-lobe descriptions live behind the `xyzdb://lobes/{name}`
    /// template (surfaced via `list_resource_templates`).
    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        let lobes = RawResource::new(resources::URI_LOBES, "lobes")
            .with_title("All lobes")
            .with_description(
                "List of every lobe registered in xyzDB. Same shape as the list_lobes tool.",
            )
            .with_mime_type("application/json")
            .no_annotation();
        let stats = RawResource::new(resources::URI_STATS, "stats")
            .with_title("Engine stats snapshot")
            .with_description(
                "Live snapshot of xyzDB internals: memtables, SSTables, compaction \
                 counters, block cache, ghosts, sync thread, process and cgroup memory. \
                 Same shape as the stats tool.",
            )
            .with_mime_type("application/json")
            .no_annotation();

        Ok(ListResourcesResult::with_all_items(vec![lobes, stats]))
    }

    /// Templated resource: per-lobe description. Surfaced via
    /// `resources/templates/list` so MCP clients can build a
    /// "lobe browser" UI without enumerating every lobe up-front
    /// in `resources/list`.
    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        let template = RawResourceTemplate::new(resources::URI_LOBE_TEMPLATE, "lobe")
            .with_title("Lobe schema")
            .with_description(
                "Full schema for one lobe (anchors + ghosts + profile). Substitute \
                 `{name}` with a lobe name from xyzdb://lobes. Same shape as the \
                 describe_lobe tool response, including per-field PartialResult on \
                 partial failure.",
            )
            .with_mime_type("application/json")
            .no_annotation();
        Ok(ListResourceTemplatesResult::with_all_items(vec![template]))
    }

    /// Dispatch by URI. The three URIs reuse the JSON helpers behind
    /// the corresponding tools, so no new engine paths are introduced.
    /// A non-existent lobe in `xyzdb://lobes/{name}` surfaces as
    /// `INVALID_PARAMS` (same semantics as `describe_lobe`).
    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        let started = Instant::now();
        let uri = request.uri.clone();
        let result = self.read_resource_inner(&uri).await;
        let latency_ms = started.elapsed().as_secs_f64() * 1000.0;
        match &result {
            Ok(_) => tracing::info!(
                target: "xyzdb_mcp::telemetry",
                uri = %uri,
                latency_ms,
                result = "success",
                "resource read completed"
            ),
            Err(e) => tracing::warn!(
                target: "xyzdb_mcp::telemetry",
                uri = %uri,
                latency_ms,
                result = "error",
                error_code = error_code_label(e),
                "resource read failed"
            ),
        }
        result
    }
}

/// Connectivity probe for `--connect` mode. Issues a single STATS
/// query at startup so reachability issues surface before the first
/// MCP client request rather than during it. Log-only on failure
/// (returns Ok) — the operator may have intentionally launched the
/// MCP server before the upstream is ready.
async fn probe_connectivity(host: &str, port: u16) {
    match connect::query_json(host, port, "STATS").await {
        Ok(_) => {
            tracing::info!(
                host = %host,
                port,
                "connectivity probe OK"
            );
        }
        Err(e) => {
            tracing::warn!(
                host = %host,
                port,
                error = %e,
                "connectivity probe failed; first tool call may also fail"
            );
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Cross-actor leak guard (design doc §8.2). Must run BEFORE
    // telemetry::init so the operator gets a clean stderr error
    // rather than a half-initialised tracing subscriber. We also
    // exit with code 2 to distinguish "config rejected" from a
    // generic anyhow failure.
    if cli.log_statements
        && let Some(addr) = cli.connect.as_deref()
    {
        let (host, _port) = connect::parse_addr(addr)?;
        if !redact::is_loopback_host(&host) {
            eprintln!(
                "error: --log-statements is not allowed with --connect to non-loopback \
                     host '{host}'. Statements from other MCP-side actors targeting the \
                     same xyzdb-server would be logged cross-actor, which is a privacy \
                     leak. Use --connect 127.0.0.1 (loopback) or --embed for development \
                     logging."
            );
            std::process::exit(2);
        }
    }

    telemetry::init(cli.log_statements);
    if cli.log_statements {
        telemetry::warn_log_statements_active();
    }

    let source = match (cli.embed, cli.connect) {
        (Some(path), None) => {
            tracing::info!(path = %path.display(), "opening xyzdb engine in --embed mode");
            let engine = Engine::open(&path)
                .with_context(|| format!("failed to open xyzdb at {}", path.display()))?
                .into_arc();
            tracing::info!("engine ready");
            EngineSource::Embed(engine)
        }
        (None, Some(addr)) => {
            let (host, port) = connect::parse_addr(&addr)?;
            let class = connect::classify_host(&host);
            connect::warn_host_class(&host, class);
            tracing::info!(host = %host, port, class = ?class, "xyzdb-mcp in --connect mode");
            if !cli.no_probe {
                probe_connectivity(&host, port).await;
            }
            EngineSource::Connect { host, port }
        }
        // ArgGroup(required=true, multiple=false) makes these
        // unreachable; defensive guard.
        (None, None) => return Err(anyhow!("must specify --embed or --connect")),
        (Some(_), Some(_)) => return Err(anyhow!("--embed and --connect are mutually exclusive")),
    };

    tracing::info!("entering MCP serve loop");

    let server = XyzdbServer::new(
        source,
        cli.log_statements,
        cli.query_timeout_ms,
        cli.query_policy,
    );

    let service = server
        .serve(stdio())
        .await
        .context("failed to start MCP serve")?;

    // Graceful shutdown: race the service against SIGINT (Ctrl-C).
    // SIGTERM coverage is Unix-specific; we add it on Pillar 1 with
    // a runtime feature-detection guard so Windows builds (theoretical
    // for v0.2.7+ desktop MCP clients on Windows) still compile.
    tokio::select! {
        res = service.waiting() => {
            res.context("MCP service exited with error")?;
        }
        _ = shutdown_signal() => {
            tracing::info!("shutdown signal received; exiting gracefully");
            // Service drops here; engine drop releases LSM lock + flushes
            // WAL via the documented Drop contract.
        }
    }

    Ok(())
}

/// Wait for the canonical shutdown signal: SIGINT (Ctrl-C) on all
/// platforms; SIGTERM on Unix in addition. Returns when either fires.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            let _ = sig.recv().await;
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_show_lobes_with_anchors_and_hint() {
        let line = "  0. clientes (2 anchors) — Customer records";
        let s = parse_lobe_line(line).expect("should parse");
        assert_eq!(s.name, "clientes");
        assert_eq!(s.anchor_count, 2);
        assert_eq!(s.hint.as_deref(), Some("Customer records"));
    }

    #[test]
    fn parse_show_lobes_no_hint() {
        let line = "  1. fintech (0 anchors)";
        let s = parse_lobe_line(line).expect("should parse");
        assert_eq!(s.name, "fintech");
        assert_eq!(s.anchor_count, 0);
        assert_eq!(s.hint, None);
    }

    #[test]
    fn parse_show_lobes_skips_header() {
        // The "Lobes:" header has no `". "` separator, so it returns None.
        assert!(parse_lobe_line("Lobes:").is_none());
    }

    #[test]
    fn parse_show_lobes_skips_garbage() {
        assert!(parse_lobe_line("not a lobe line").is_none());
        assert!(parse_lobe_line("  abc. clientes (oops)").is_none());
    }

    #[test]
    fn parse_show_lobes_full_block() {
        let lines = vec![
            "Lobes:".to_string(),
            "  0. clientes (2 anchors) — Customer records".to_string(),
            "  1. fintech (0 anchors)".to_string(),
            "  2. creditos (1 anchors) — Credit lifecycle".to_string(),
        ];
        let parsed = parse_show_lobes(&lines);
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].name, "clientes");
        assert_eq!(parsed[0].anchor_count, 2);
        assert_eq!(parsed[1].name, "fintech");
        assert_eq!(parsed[2].name, "creditos");
        assert_eq!(parsed[2].hint.as_deref(), Some("Credit lifecycle"));
    }

    #[test]
    fn augment_with_cursor_appends_when_absent() {
        let result = augment_with_cursor(r#"SCAN "x" LIMIT 100"#, Some("AQEAAQ_TOKEN"));
        assert_eq!(result, r#"SCAN "x" LIMIT 100 CURSOR "AQEAAQ_TOKEN""#);
    }

    #[test]
    fn augment_with_cursor_skips_when_already_present() {
        let stmt = r#"SCAN "x" LIMIT 100 CURSOR "ALREADY""#;
        let result = augment_with_cursor(stmt, Some("ARGUMENT_TOKEN"));
        assert_eq!(result, stmt, "statement-embedded cursor wins");
    }

    #[test]
    fn augment_with_cursor_passes_through_when_no_cursor_arg() {
        let stmt = r#"SCAN "x" LIMIT 100"#;
        let result = augment_with_cursor(stmt, None);
        assert_eq!(result, stmt);
    }

    #[test]
    fn error_code_label_detects_timeout_via_data_marker() {
        let err = McpError::internal_error(
            "query timed out after 30000ms".to_string(),
            Some(serde_json::json!({ "timeout": true })),
        );
        assert_eq!(error_code_label(&err), "TIMEOUT");
    }

    #[test]
    fn error_code_label_passes_through_internal_error_without_marker() {
        // INTERNAL_ERROR without the timeout data flag stays as
        // INTERNAL_ERROR — only the explicit marker triggers TIMEOUT.
        let err = McpError::internal_error("generic internal failure".to_string(), None);
        assert_eq!(error_code_label(&err), "INTERNAL_ERROR");
    }

    #[test]
    fn error_code_label_ignores_unrelated_data_field() {
        // Other tools may use err.data for their own context; they
        // must not be misclassified as TIMEOUT.
        let err = McpError::internal_error(
            "boundary error".to_string(),
            Some(serde_json::json!({ "correlation_id": "abc-123" })),
        );
        assert_eq!(error_code_label(&err), "INTERNAL_ERROR");
    }

    #[test]
    fn count_records_per_variant() {
        use xyzdb_core::lid::LID;
        use xyzdb_core::result::QueryResult;

        let lid = LID::new(0u16);
        assert_eq!(
            count_records(&QueryResult::Ok {
                lid: Some(lid),
                message: "ok".into()
            }),
            1
        );
        assert_eq!(
            count_records(&QueryResult::Ok {
                lid: None,
                message: "ok".into()
            }),
            0
        );
        assert_eq!(
            count_records(&QueryResult::BatchOk {
                count: 42,
                first_lid: lid,
                last_lid: lid,
            }),
            42
        );
        assert_eq!(count_records(&QueryResult::Records(vec![])), 0);
        assert_eq!(
            count_records(&QueryResult::Aggregation(Default::default())),
            1
        );
        assert_eq!(count_records(&QueryResult::Info(vec!["x".into()])), 0);
        assert_eq!(
            count_records(&QueryResult::GroupedAggregation(vec![
                Default::default();
                3
            ])),
            3
        );
        assert_eq!(
            count_records(&QueryResult::PaginatedRecords {
                records: vec![],
                cursor: None,
                has_more: false,
                budget_stop: None,
            }),
            0
        );
    }
}
