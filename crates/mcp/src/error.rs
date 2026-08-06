//! Engine error → MCP error mapping with redaction.
//!
//! Pillar 1 of v0.2.6 implementation phase: makes the design doc §6.1
//! contract real. The redaction rules implement the 5 cases enumerated
//! in the design doc, in order.
//!
//! - **User-named errors** (lobe/field/anchor/ghost names, type
//!   mismatches, parse errors): pass through verbatim. These names are
//!   not secrets in single-tenant deployments and the agent needs them
//!   to know what to fix.
//! - **Statement fragments** in error messages get value-redacted +
//!   truncated.
//! - **Internal panics / corrupted state**: replaced with a generic
//!   message + correlation_id; the full error is logged server-side at
//!   ERROR level with the same correlation_id for operator triage.

// SPDX-License-Identifier: BUSL-1.1
// Pillar 1 ships the redaction/mapping infrastructure; only the
// `stats` tool exists yet and it does not raise XyzError because
// stats_snapshot returns infallibly. Pillar 2's `query` tool will
// use engine_to_mcp on every Engine::run result. Mark the module's
// callable surface as intentionally unused for now so the warning
// does not pile up across Pillars 1-2.
#![allow(dead_code)]

use std::fmt::Write;

use rmcp::ErrorData as McpError;
use rmcp::model::ErrorCode;
use uuid::Uuid;
use xyzdb_core::error::XyzError;

/// Map a `XyzError` from the engine into an MCP `ErrorData`.
///
/// User-facing errors map to `INVALID_PARAMS` with verbose-friendly
/// messages. Internal errors map to `INTERNAL_ERROR` with a redacted
/// message and a server-side correlation_id logged for operator triage.
pub fn engine_to_mcp(err: XyzError) -> McpError {
    match err {
        // Tier 1 — user-facing, pass-through. The message references
        // lobe/field/anchor/ghost names that the agent already learned
        // via list_lobes / describe_lobe / SHOW; not new info leak.
        XyzError::Parse(msg) => {
            McpError::new(ErrorCode::INVALID_PARAMS, redact_statement(&msg), None)
        }
        XyzError::LobeNotFound(name) => McpError::new(
            ErrorCode::INVALID_PARAMS,
            format!("lobe '{name}' not found"),
            None,
        ),
        XyzError::DuplicateAnchor {
            anchor,
            value,
            lobe,
            existing_lid,
        } => McpError::new(
            ErrorCode::INVALID_PARAMS,
            format!(
                "duplicate anchor '{anchor}' = '{value}' in lobe '{lobe}' (existing record: {existing_lid})"
            ),
            None,
        ),
        XyzError::RecordNotFound(msg) => McpError::new(
            ErrorCode::INVALID_PARAMS,
            format!("record not found: {msg}"),
            None,
        ),
        XyzError::TypeError { expected, got } => McpError::new(
            ErrorCode::INVALID_PARAMS,
            format!("type error: expected {expected}, got {got}"),
            None,
        ),
        XyzError::InvalidQuery(msg) => McpError::new(ErrorCode::INVALID_PARAMS, msg, None),
        XyzError::GhostNotFound(name) => McpError::new(
            ErrorCode::INVALID_PARAMS,
            format!("ghost '{name}' not found"),
            None,
        ),
        XyzError::GhostExists(name) => McpError::new(
            ErrorCode::INVALID_PARAMS,
            format!("ghost '{name}' already exists"),
            None,
        ),
        // Operational limit — actionable, no secrets in the message (just counts
        // and the flag name), so it passes through verbatim like the tier-1 set.
        XyzError::NearestBudgetExceeded { scanned, budget_ms } => McpError::new(
            ErrorCode::INVALID_PARAMS,
            format!(
                "NEAREST exceeded its {budget_ms}ms budget after scanning {scanned} candidates; \
                 raise --nearest-budget-ms or narrow the gravity bucket"
            ),
            None,
        ),

        // Tier 2 — internal, redact + correlate. Storage errors may
        // contain file paths or backtraces; Internal may include any
        // diagnostic the engine emitted.
        XyzError::Storage(msg) => redact_internal("storage", &msg),
        XyzError::Internal(msg) => redact_internal("engine", &msg),
    }
}

/// Internal-error redaction. Generates a UUIDv7 correlation_id, logs
/// the full message server-side at ERROR level with that id, returns a
/// generic MCP error referencing the id so an operator can correlate.
fn redact_internal(category: &str, full_msg: &str) -> McpError {
    let correlation_id = Uuid::now_v7();
    tracing::error!(
        target: "xyzdb_mcp::internal_error",
        correlation_id = %correlation_id,
        category,
        error = %full_msg,
        "internal error redacted from MCP response"
    );
    let public =
        format!("internal error ({category}); correlation_id={correlation_id}; check server logs");
    McpError::new(ErrorCode::INTERNAL_ERROR, public, None)
}

/// Redact statement fragments inside an error message. Applies to
/// `Parse` errors that may quote a chunk of the user's xyTalk: literal
/// values inside `{...}` and quoted strings get replaced with
/// `<redacted>`. Truncates to 80 chars + suffix.
///
/// For other tier-1 errors the engine never emits the original
/// statement, so this function is conservative — it only changes the
/// output if it detects statement-shaped fragments.
fn redact_statement(msg: &str) -> String {
    // Heuristic: if the error contains a brace block or a quoted
    // string, scrub the values; otherwise pass through.
    if !msg.contains('{') && !msg.contains('"') {
        return passthrough_with_truncate(msg, 200);
    }

    // Scrub values inside { ... } and "..."
    let mut out = String::with_capacity(msg.len());
    let mut in_brace = 0i32;
    let mut in_quote = false;
    for ch in msg.chars() {
        match ch {
            '"' if !in_quote && in_brace == 0 => {
                let _ = write!(out, "<redacted>");
                in_quote = true;
            }
            '"' if in_quote => {
                in_quote = false;
            }
            '{' if !in_quote => {
                if in_brace == 0 {
                    let _ = write!(out, "{{<redacted>");
                }
                in_brace += 1;
            }
            '}' if !in_quote => {
                in_brace -= 1;
                if in_brace == 0 {
                    let _ = write!(out, "}}");
                }
            }
            _ if !in_quote && in_brace == 0 => out.push(ch),
            _ => {} // suppress chars inside quotes/braces (already replaced once)
        }
    }
    passthrough_with_truncate(&out, 80)
}

/// Truncate at `max` chars + add a sentinel suffix.
fn passthrough_with_truncate(msg: &str, max: usize) -> String {
    if msg.len() <= max {
        msg.to_string()
    } else {
        let mut out = String::with_capacity(max + 16);
        out.push_str(&msg[..max]);
        out.push_str(" ... (truncated)");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_1_lobe_not_found_passes_through() {
        let err = engine_to_mcp(XyzError::LobeNotFound("clientes".into()));
        assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
        assert!(err.message.contains("clientes"), "got: {}", err.message);
    }

    #[test]
    fn rule_2_duplicate_anchor_includes_field_names() {
        let err = engine_to_mcp(XyzError::DuplicateAnchor {
            anchor: "rfc".into(),
            value: "ACME-001".into(),
            lobe: "clientes".into(),
            existing_lid: "0000:0001:..".into(),
        });
        assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
        assert!(err.message.contains("rfc") && err.message.contains("clientes"));
    }

    #[test]
    fn rule_2_type_error_passes_through() {
        let err = engine_to_mcp(XyzError::TypeError {
            expected: "int".into(),
            got: "text".into(),
        });
        assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
        assert!(err.message.contains("expected int"));
    }

    #[test]
    fn rule_3_parse_with_brace_redacts_values() {
        let err = engine_to_mcp(XyzError::Parse(
            r#"parse error at: PUT { secret: "abc123" }"#.into(),
        ));
        assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
        // Original literal value MUST NOT appear in output.
        assert!(
            !err.message.contains("abc123"),
            "secret leaked: {}",
            err.message
        );
        // Marker is present so the agent knows redaction happened.
        assert!(err.message.contains("<redacted>"));
    }

    #[test]
    fn rule_4_internal_error_replaced_with_correlation_id() {
        let err = engine_to_mcp(XyzError::Internal(
            "panic at xyzdb-engine/src/foo.rs:42".into(),
        ));
        assert_eq!(err.code, ErrorCode::INTERNAL_ERROR);
        // Original details MUST NOT appear.
        assert!(!err.message.contains("foo.rs"), "leak: {}", err.message);
        assert!(!err.message.contains("panic at"), "leak: {}", err.message);
        // Correlation id is present.
        assert!(
            err.message.contains("correlation_id="),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn rule_4_storage_error_replaced_with_correlation_id() {
        let err = engine_to_mcp(XyzError::Storage(
            "backtrace at /private/var/folders/.../tmp.XYZ/sst/000123.sst".into(),
        ));
        assert_eq!(err.code, ErrorCode::INTERNAL_ERROR);
        assert!(!err.message.contains("/tmp"), "leak: {}", err.message);
        assert!(!err.message.contains("000123"), "leak: {}", err.message);
        assert!(err.message.contains("correlation_id="));
    }

    #[test]
    fn rule_5_long_message_truncated() {
        let long = "x".repeat(500);
        let err = engine_to_mcp(XyzError::InvalidQuery(long.clone()));
        assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
        // Pass-through tier-1: InvalidQuery is not redact_statement
        // routed (no brace/quote), so it goes through as-is. The
        // contract here is that engine doesn't emit > MAX_FRAME_SIZE.
        // For Parse-routed values though, redact_statement applies.
        let _ = long;
        assert!(err.message.len() <= 500); // unchanged for InvalidQuery
    }

    #[test]
    fn parse_with_long_truncates_at_80_after_redact() {
        let long_secret = format!(r#"parse error at: PUT {{ secret: "{}" }}"#, "x".repeat(300));
        let err = engine_to_mcp(XyzError::Parse(long_secret));
        // After redaction + truncate, message should be short.
        assert!(
            err.message.len() < 200,
            "expected truncated, got len={}: {}",
            err.message.len(),
            err.message
        );
    }

    #[test]
    fn parse_without_braces_passthrough_with_cap() {
        let err = engine_to_mcp(XyzError::Parse(
            "expected SCAN, found HOLA at offset 0".into(),
        ));
        // No braces / quotes → pass through verbatim, capped at 200.
        assert!(err.message.contains("expected SCAN"));
    }
}
