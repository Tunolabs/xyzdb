//! Resource URIs for the MCP resources spec — Pillar 5 of v0.2.6.
//!
//! Three URIs surface the same data as the corresponding tools, via the
//! resource-URI navigation pattern that some MCP clients (via their
//! resource tree) prefer over imperative tool calls. The underlying
//! engine path is identical; resources are duplicates of tool responses
//! by design (see `docs/mcp-integration.md`).
//!
//! Surface:
//! - `xyzdb://lobes` (concrete) — list of `LobeSummary`, identical to
//!   `list_lobes` tool response.
//! - `xyzdb://stats` (concrete) — full `StatsResponse` snapshot,
//!   identical to `stats` tool response.
//! - `xyzdb://lobes/{name}` (template) — full `LobeDescription`,
//!   identical to `describe_lobe(lobe=name)` tool response.
//!
//! `xyzdb://lobes/{name}` is exposed via `resources/templates/list` (per
//! MCP spec); `xyzdb://lobes` and `xyzdb://stats` are exposed via
//! `resources/list`. All three URIs are accepted by `resources/read`.

// SPDX-License-Identifier: BUSL-1.1
/// Concrete URI: list of lobes.
pub const URI_LOBES: &str = "xyzdb://lobes";

/// Concrete URI: stats snapshot.
pub const URI_STATS: &str = "xyzdb://stats";

/// Template URI for single-lobe description. The `{name}` placeholder is
/// substituted by the lobe name on `resources/read`.
pub const URI_LOBE_TEMPLATE: &str = "xyzdb://lobes/{name}";

const LOBE_PREFIX: &str = "xyzdb://lobes/";

/// Match `xyzdb://lobes/<name>` and return `<name>`. Returns `None` for:
/// - The bare `xyzdb://lobes` URI (no trailing slash → not a per-lobe URI).
/// - A URI with extra path segments (e.g. `xyzdb://lobes/foo/bar`).
/// - A URI with an empty name (e.g. `xyzdb://lobes/`).
///
/// The caller is still responsible for ensuring the name corresponds to
/// an existing lobe; this function is a syntactic check only.
pub fn parse_lobe_uri(uri: &str) -> Option<&str> {
    let name = uri.strip_prefix(LOBE_PREFIX)?;
    if name.is_empty() || name.contains('/') {
        return None;
    }
    Some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_lobe_uri_matches_simple_name() {
        assert_eq!(parse_lobe_uri("xyzdb://lobes/creditos"), Some("creditos"));
    }

    #[test]
    fn parse_lobe_uri_rejects_bare_lobes() {
        // `xyzdb://lobes` is the list URI, not a per-lobe URI.
        assert_eq!(parse_lobe_uri("xyzdb://lobes"), None);
    }

    #[test]
    fn parse_lobe_uri_rejects_trailing_slash_only() {
        assert_eq!(parse_lobe_uri("xyzdb://lobes/"), None);
    }

    #[test]
    fn parse_lobe_uri_rejects_extra_path_segments() {
        assert_eq!(parse_lobe_uri("xyzdb://lobes/creditos/extra"), None);
    }

    #[test]
    fn parse_lobe_uri_rejects_unrelated_scheme() {
        assert_eq!(parse_lobe_uri("xyzdb://stats"), None);
        assert_eq!(parse_lobe_uri("file:///lobes/creditos"), None);
        assert_eq!(parse_lobe_uri(""), None);
    }

    #[test]
    fn parse_lobe_uri_preserves_case_and_underscores() {
        // Lobe names are case-sensitive at the engine layer; the URI
        // parser preserves whatever it received verbatim.
        assert_eq!(parse_lobe_uri("xyzdb://lobes/CamelCase"), Some("CamelCase"));
        assert_eq!(
            parse_lobe_uri("xyzdb://lobes/with_underscore_123"),
            Some("with_underscore_123")
        );
    }
}
