//! Privacy-preserving fingerprinting helpers — Pillar 6 of v0.2.6.
//!
//! Implements the design-doc §8.2 redaction rules: statement text and
//! cursor tokens never appear in INFO-level telemetry events. The
//! `query` tool emits `query_hash` (xxh3-64, first 8 hex chars) and
//! `query_kind` (first verb) as a fingerprint that is sufficient for
//! "what queries are hot" analysis without leaking literal values that
//! may carry PII.
//!
//! Only when the operator passes `--log-statements` does the full
//! statement (and cursor token) appear in TRACE-level events.

// SPDX-License-Identifier: BUSL-1.1
use twox_hash::XxHash3_64;

/// xxh3-64 fingerprint of a statement, returned as the first 8 hex
/// characters. Stable across runs (no random seed). Suitable for
/// "top hot queries" aggregation; not a cryptographic hash.
pub fn query_hash(statement: &str) -> String {
    let hash = XxHash3_64::oneshot(statement.as_bytes());
    format!("{:08x}", (hash >> 32) as u32)
}

/// First whitespace-separated token of a statement, uppercased — the
/// xyTalk verb (`PUT`, `FIND`, `SCAN`, `SET`, `DELETE`, `LINK`, `SHOW`,
/// `LOBE`, `ANCHOR`, `CREATE`, `AGGREGATE`, …). Empty input or
/// whitespace-only input returns `"UNKNOWN"`.
pub fn query_kind(statement: &str) -> String {
    statement
        .split_whitespace()
        .next()
        .map(|s| s.to_ascii_uppercase())
        .unwrap_or_else(|| "UNKNOWN".to_string())
}

/// Loopback host classifier used by the `--log-statements` cross-actor
/// guard (design doc §8.2). Accepts the literal forms an operator would
/// type:
/// - `127.0.0.1` and any `127.x.y.z`,
/// - `::1` and the bracketed `[::1]`,
/// - `localhost` (case-insensitive).
///
/// Anything else is treated as non-loopback. The guard refuses
/// `--log-statements` together with `--connect <non-loopback>` to keep
/// statement text emitted by *other actors* sharing the upstream
/// `xyzdb-server` from landing in this MCP process's stderr.
pub fn is_loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    if host == "::1" || host == "[::1]" {
        return true;
    }
    if let Some(rest) = host.strip_prefix("127.") {
        // 127.x.y.z — three dotted octets must follow.
        let parts: Vec<&str> = rest.split('.').collect();
        if parts.len() == 3 && parts.iter().all(|p| p.parse::<u8>().is_ok()) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── query_hash ───────────────────────────────────────────────────────

    #[test]
    fn query_hash_is_8_lowercase_hex_chars() {
        let h = query_hash(r#"FIND "creditos" WHERE rfc = "AAAA111""#);
        assert_eq!(h.len(), 8);
        assert!(
            h.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
        );
    }

    #[test]
    fn query_hash_is_stable_across_calls() {
        let stmt = r#"SCAN "creditos" LIMIT 100"#;
        assert_eq!(query_hash(stmt), query_hash(stmt));
    }

    #[test]
    fn query_hash_differs_for_different_statements() {
        let a = query_hash(r#"FIND "a" WHERE x = 1"#);
        let b = query_hash(r#"FIND "b" WHERE x = 1"#);
        assert_ne!(a, b);
    }

    #[test]
    fn query_hash_handles_empty_string() {
        let h = query_hash("");
        assert_eq!(h.len(), 8);
    }

    // ─── query_kind ───────────────────────────────────────────────────────

    #[test]
    fn query_kind_uppercases_first_verb() {
        assert_eq!(query_kind("put {x:1} IN \"a\""), "PUT");
        assert_eq!(query_kind("scan \"a\""), "SCAN");
        assert_eq!(query_kind("Find \"a\" where x = 1"), "FIND");
    }

    #[test]
    fn query_kind_handles_leading_whitespace() {
        assert_eq!(query_kind("   SHOW LOBES"), "SHOW");
    }

    #[test]
    fn query_kind_returns_unknown_on_empty() {
        assert_eq!(query_kind(""), "UNKNOWN");
        assert_eq!(query_kind("   \t\n  "), "UNKNOWN");
    }

    #[test]
    fn query_kind_handles_compound_keywords() {
        // "CREATE GHOST ..." — only the first token is reported. The
        // analytics consumer aggregates by verb, not by full prefix.
        assert_eq!(query_kind("CREATE GHOST \"g\" FROM \"l\""), "CREATE");
    }

    // ─── is_loopback_host ─────────────────────────────────────────────────

    #[test]
    fn loopback_accepts_canonical_ipv4() {
        assert!(is_loopback_host("127.0.0.1"));
    }

    #[test]
    fn loopback_accepts_full_127_range() {
        assert!(is_loopback_host("127.0.0.2"));
        assert!(is_loopback_host("127.255.255.254"));
    }

    #[test]
    fn loopback_rejects_partial_127_match() {
        // 127. followed by something other than 3 octets.
        assert!(!is_loopback_host("127.0.0"));
        assert!(!is_loopback_host("127.0.0.1.2"));
        assert!(!is_loopback_host("127.0.0.x"));
    }

    #[test]
    fn loopback_accepts_ipv6_loopback() {
        assert!(is_loopback_host("::1"));
        assert!(is_loopback_host("[::1]"));
    }

    #[test]
    fn loopback_accepts_localhost_case_insensitive() {
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("LOCALHOST"));
        assert!(is_loopback_host("LocalHost"));
    }

    #[test]
    fn loopback_rejects_lan_ips() {
        assert!(!is_loopback_host("10.0.0.1"));
        assert!(!is_loopback_host("192.168.1.1"));
        assert!(!is_loopback_host("172.16.0.1"));
        assert!(!is_loopback_host("8.8.8.8"));
    }

    #[test]
    fn loopback_rejects_dns_names() {
        assert!(!is_loopback_host("xyzdb-server.internal"));
        assert!(!is_loopback_host("example.com"));
        assert!(!is_loopback_host(""));
    }
}
