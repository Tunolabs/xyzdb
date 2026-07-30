//! Telemetry setup for xyzdb-mcp.
//!
//! Pillar 6 of v0.2.6 implementation: per-tool-call structured event
//! per design doc §8.1, with default-on redaction (§8.2). Output goes
//! to stderr because stdout is reserved for MCP framing. `RUST_LOG`
//! env var overrides the default level.
//!
//! The TRACE-level statement / cursor logging gated by the
//! `--log-statements` flag is enabled here by widening the EnvFilter
//! at `init()` time. The flag is propagated from the CLI parser in
//! `main.rs`; the cross-actor guard (refusing the flag on a non-loopback
//! `--connect` target) is enforced in `main.rs` before this function
//! runs.

use tracing_subscriber::EnvFilter;

const STATEMENT_TARGET: &str = "xyzdb_mcp::statements";

/// Initialize tracing for the MCP server. Idempotent — safe to call
/// once at startup. Panics on second call (subscriber already
/// installed).
///
/// `log_statements`:
/// - `false` (default) → INFO+ on all targets; the
///   `xyzdb_mcp::statements` TRACE events used by the `query` tool
///   to emit raw statements are filtered out before they reach
///   stderr.
/// - `true` → INFO+ everywhere PLUS TRACE on the statement target;
///   raw xyTalk statements and cursor tokens land in stderr. The
///   boot warning emitted at startup makes this opt-in posture
///   explicit to the operator.
///
/// `RUST_LOG` overrides everything if set, regardless of
/// `log_statements`.
pub fn init(log_statements: bool) {
    let default_filter = if log_statements {
        format!("info,{STATEMENT_TARGET}=trace")
    } else {
        "info".to_string()
    };
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter)),
        )
        .init();
}

/// Per-tool-call event payload. Construct one per invocation, pass to
/// [`emit_tool_call`]. The `query`-only fields (`query_hash`,
/// `query_kind`, `cursor_present`, `records_returned`) carry `None`
/// for the other three tools.
pub struct ToolCallEvent<'a> {
    /// Tool name as exposed via `tools/list`.
    pub tool: &'a str,
    /// UUIDv7 generated at the start of this call. Useful for span
    /// correlation in a future v0.2.7 multi-actor deployment; in
    /// v0.2.6 single-tenant stdio it is still emitted so the schema
    /// is stable.
    pub request_id: &'a str,
    /// Wall-clock latency from tool entry to result production.
    pub latency_ms: f64,
    /// Outcome of the call.
    pub result: ToolCallResult,
    /// xxh3-64 fingerprint of the statement (first 8 hex chars).
    /// Populated only for the `query` tool.
    pub query_hash: Option<&'a str>,
    /// First xyTalk verb of the statement (uppercased).
    /// Populated only for the `query` tool.
    pub query_kind: Option<&'a str>,
    /// Whether the call carried a cursor argument or statement-embedded
    /// cursor. The token itself is never logged at this level
    /// (see `--log-statements` for the TRACE-level surface).
    pub cursor_present: Option<bool>,
    /// Records returned by the engine for this call (count only,
    /// never content). 0 for non-record-returning verbs.
    pub records_returned: Option<u64>,
}

/// Emit the per-tool-call structured event at completion.
///
/// All events carry `caller_id="stdio"` (v0.2.6 invariant — the
/// transport is stdin/stdout exclusively). v0.2.7 multi-actor will
/// replace this with a per-connection token hash.
pub fn emit_tool_call(ev: ToolCallEvent<'_>) {
    match ev.result {
        ToolCallResult::Success => {
            tracing::info!(
                target: "xyzdb_mcp::telemetry",
                caller_id = "stdio",
                request_id = ev.request_id,
                tool = ev.tool,
                latency_ms = ev.latency_ms,
                result = "success",
                query_hash = ev.query_hash,
                query_kind = ev.query_kind,
                cursor_present = ev.cursor_present,
                records_returned = ev.records_returned,
                "tool call completed"
            );
        }
        ToolCallResult::Error { code } => {
            tracing::warn!(
                target: "xyzdb_mcp::telemetry",
                caller_id = "stdio",
                request_id = ev.request_id,
                tool = ev.tool,
                latency_ms = ev.latency_ms,
                result = "error",
                error_code = code,
                query_hash = ev.query_hash,
                query_kind = ev.query_kind,
                cursor_present = ev.cursor_present,
                "tool call failed"
            );
        }
    }
}

/// Result of a tool call for telemetry purposes.
pub enum ToolCallResult {
    Success,
    Error { code: &'static str },
}

/// Boot warning emitted at startup when `--log-statements` is active.
/// One-time, multi-line, distinguishable in stderr scans. Goes to
/// `tracing::warn` so the same sink that captures structured events
/// captures the warning too.
pub fn warn_log_statements_active() {
    tracing::warn!(
        target: "xyzdb_mcp::telemetry",
        "--log-statements is ACTIVE. Full xyTalk statements and cursor tokens \
         will be recorded at TRACE level on target xyzdb_mcp::statements. \
         Do not use this flag in production deployments. PII contained in \
         statement literals will appear in stderr and any sink capturing it \
         (journald, Docker logs, MCP client diagnostics)."
    );
}

/// TRACE-level emission of a raw statement + cursor token. Goes to a
/// dedicated target (`xyzdb_mcp::statements`) so the EnvFilter can
/// gate this surface independently of the rest of telemetry. When
/// `--log-statements` was NOT passed, the EnvFilter set up in
/// [`init`] discards these events before they reach stderr.
///
/// Callers should pass the cursor token through `cursor`. When the
/// caller has no cursor, pass `None`; `cursor_present` is logged
/// either way for cross-correlation with the INFO-level event.
pub fn trace_statement(request_id: &str, statement: &str, cursor: Option<&str>) {
    tracing::trace!(
        target: STATEMENT_TARGET,
        request_id,
        statement,
        cursor,
        "statement (log-statements active)"
    );
}
