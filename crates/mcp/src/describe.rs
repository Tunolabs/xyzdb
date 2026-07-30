//! describe_lobe tool — Pillar 4 of v0.2.6.
//!
//! Composes three SHOW queries into a single response with per-field
//! independent fallibility (PartialResult per field). The agent gets
//! whatever did succeed plus explicit error markers for whatever did
//! not, rather than total failure or silent omission.
//!
//! Wire shape (untagged enum) — design doc §6.4:
//!
//! ```json
//! {
//!   "name": "items",
//!   "anchors": [{ "name": "id", "unique": true }],
//!   "ghosts":  { "error": "SHOW GHOSTS failed: ..." },
//!   "profile": { "pinned_fields": [...], "learned_patterns": [...], "active_ghosts_count": 0 }
//! }
//! ```
//!
//! With `#[serde(untagged)]`, Ok(T) serialises as T directly (array for
//! Vec, object for ProfileInfo) and Err { error } serialises as
//! `{"error": "..."}`. Distinguishable by presence of the `error` key.
//! No T currently includes an `error` field; if a future T does, this
//! module switches to a custom Serialize impl (~5 LOC).

use rmcp::schemars::{self, JsonSchema};
use serde::Serialize;

/// Per-field result of describe_lobe sub-calls. See module docs.
#[derive(Debug, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum PartialResult<T> {
    Ok(T),
    Err { error: String },
}

/// Top-level describe_lobe response.
#[derive(Debug, Serialize, JsonSchema)]
pub struct LobeDescription {
    /// Lobe name, mirrored from the request.
    pub name: String,
    /// Anchors declared on the lobe, or per-field error.
    pub anchors: PartialResult<Vec<AnchorInfo>>,
    /// Ghosts whose source_lobe matches the request (Permanent /
    /// Ephemeral / Promoted), or per-field error.
    pub ghosts: PartialResult<Vec<GhostInfo>>,
    /// Profile (pinned fields, learned patterns, active ghost count),
    /// or per-field error.
    pub profile: PartialResult<ProfileInfo>,
    /// Searchable vector field: `null` if the lobe declares none, else
    /// `{ "field": ..., "dim": ... }` where `dim` is `null` until the first
    /// embedding fixes it. Hoisted from the parsed profile so an agent reads
    /// vector capability at the top level, not buried in `profile`.
    pub vector: Option<VectorField>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct AnchorInfo {
    pub name: String,
    /// Always true for v0.2.6 — anchors are UNIQUE-only constraints.
    pub unique: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct GhostInfo {
    pub name: String,
    pub source_lobe: String,
    /// `ORDER BY` field if declared at CREATE GHOST, else None.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_by: Option<String>,
    pub record_count: u64,
    pub filter_count: u32,
}

/// The lobe's searchable vector field. `field` is the embedding column a
/// `NEAREST` query targets; `dim` is the vector length — `None` until the
/// first embedding is written (the dimension is learned then enforced), and
/// on a legacy spec loaded without it. A `null` `vector` on [`LobeDescription`]
/// means the lobe has no searchable field at all; do not conflate the two.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct VectorField {
    pub field: String,
    pub dim: Option<u32>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ProfileInfo {
    pub pinned_fields: Vec<String>,
    /// Raw learned-pattern lines from scan telemetry, one per line.
    /// Spike-grade: kept as opaque strings; structured parse is Pillar
    /// 6+ scope if telemetry shows agents needing it.
    pub learned_patterns: Vec<String>,
    /// Number of ghosts whose source_lobe matches the queried lobe.
    /// Cross-checked against the dedicated SHOW GHOSTS call but
    /// reported here from SHOW PROFILE for consistency with the
    /// engine's profile shape.
    pub active_ghosts_count: u32,
    /// Searchable vector field parsed from the SHOW PROFILE `Vector:` line.
    /// Carried here as the parse target, then hoisted to the top-level
    /// `vector` of [`LobeDescription`]; not serialised inside `profile`.
    #[serde(skip)]
    pub vector: Option<VectorField>,
}

// ─── Parsers ────────────────────────────────────────────────────────────────

/// Parse SHOW ANCHORS IN "lobe" output:
///
/// ```text
/// Anchors in 'lobe':
///   - id (UNIQUE)
///   - email (UNIQUE)
/// ```
pub fn parse_show_anchors(lines: &[String]) -> Vec<AnchorInfo> {
    lines
        .iter()
        .filter_map(|line| {
            let trimmed = line.trim_start();
            let after_dash = trimmed.strip_prefix("- ")?;
            // Engine emits "(UNIQUE)" today; in the future a different
            // constraint shape would land here.
            let paren_pos = after_dash.find(" (")?;
            let name = after_dash[..paren_pos].to_string();
            Some(AnchorInfo { name, unique: true })
        })
        .collect()
}

/// Parse SHOW GHOSTS output filtered to a single source lobe.
///
/// ```text
/// Ghost Lobes:
///   ghost_a — from 'lobe1' order by 'due_date' (123 records, 2 filters)
///   ghost_b — from 'lobe2' order by '' (456 records, 1 filters)
/// ```
///
/// Ghosts whose `from '<src>'` does not match `lobe` are filtered out.
pub fn parse_show_ghosts_filtered(lines: &[String], lobe: &str) -> Vec<GhostInfo> {
    lines
        .iter()
        .filter_map(|line| parse_ghost_line(line, lobe))
        .collect()
}

fn parse_ghost_line(line: &str, lobe: &str) -> Option<GhostInfo> {
    let trimmed = line.trim_start();
    // Skip header "Ghost Lobes:" or "No Ghost Lobes." which lack the
    // canonical " — from '" separator.
    let (name_part, rest) = trimmed.split_once(" — from '")?;
    let name = name_part.to_string();
    let (source, rest) = rest.split_once("' order by '")?;
    if source != lobe {
        return None;
    }
    let (order, rest) = rest.split_once("' (")?;
    let order_by = if order.is_empty() {
        None
    } else {
        Some(order.to_string())
    };
    // rest: "{N} records, {M} filters)"
    let inside = rest.strip_suffix(')')?;
    let (records_part, filters_part) = inside.split_once(", ")?;
    let record_count: u64 = records_part.split_whitespace().next()?.parse().ok()?;
    let filter_count: u32 = filters_part.split_whitespace().next()?.parse().ok()?;
    Some(GhostInfo {
        name,
        source_lobe: source.to_string(),
        order_by,
        record_count,
        filter_count,
    })
}

/// Parse SHOW PROFILE "lobe" output:
///
/// ```text
/// Profile for 'lobe':
///   Pinned: field1, field2          OR  Pinned: (none)
///   Learned: (no scan patterns yet) OR  Learned patterns:
///                                         <indented pattern 1>
///                                         <indented pattern 2>
///   Ghosts: (none)                  OR  Ghosts: 3 active
///                                         <indented ghost detail 1>
///                                         <indented ghost detail 2>
/// ```
pub fn parse_show_profile(lines: &[String]) -> ProfileInfo {
    let mut pinned_fields: Vec<String> = Vec::new();
    let mut learned_patterns: Vec<String> = Vec::new();
    let mut active_ghosts_count: u32 = 0;
    // Absent when talking to an older server that predates the Vector line —
    // treated as "no vector reported", never a parse error.
    let mut vector: Option<VectorField> = None;

    #[derive(PartialEq)]
    enum Section {
        None,
        LearnedItems,
        GhostItems,
    }
    let mut section = Section::None;

    for line in lines {
        let trimmed = line.trim_start();

        if let Some(rest) = trimmed.strip_prefix("Pinned: ") {
            section = Section::None;
            if rest != "(none)" {
                pinned_fields = rest.split(", ").map(str::to_string).collect();
            }
            continue;
        }

        if trimmed == "Learned: (no scan patterns yet)" {
            section = Section::None;
            continue;
        }

        if trimmed == "Learned patterns:" {
            section = Section::LearnedItems;
            continue;
        }

        if trimmed == "Ghosts: (none)" {
            section = Section::None;
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("Ghosts: ") {
            // "{N} active"
            section = Section::GhostItems;
            if let Some(n_str) = rest.split_whitespace().next() {
                active_ghosts_count = n_str.parse().unwrap_or(0);
            }
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("Vector: ") {
            section = Section::None;
            if rest != "(none)" {
                // "{field} dim {N}"  or  "{field} dim unknown"
                if let Some((field, dimpart)) = rest.rsplit_once(" dim ") {
                    vector = Some(VectorField {
                        field: field.to_string(),
                        dim: dimpart.parse::<u32>().ok(),
                    });
                }
            }
            continue;
        }

        // Indented continuation lines belong to the current section.
        match section {
            Section::LearnedItems => {
                if !trimmed.is_empty() {
                    learned_patterns.push(trimmed.to_string());
                }
            }
            Section::GhostItems => {
                // We capture ghost details via SHOW GHOSTS separately.
                // Profile-level ghost lines are redundant; ignore.
            }
            Section::None => {}
        }
    }

    ProfileInfo {
        pinned_fields,
        learned_patterns,
        active_ghosts_count,
        vector,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── PartialResult untagged serialisation ─────────────────────────────

    #[test]
    fn partial_result_ok_serialises_inner_directly() {
        let r: PartialResult<Vec<AnchorInfo>> = PartialResult::Ok(vec![AnchorInfo {
            name: "rfc".into(),
            unique: true,
        }]);
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(json, r#"[{"name":"rfc","unique":true}]"#);
    }

    #[test]
    fn partial_result_err_serialises_with_error_key() {
        let r: PartialResult<Vec<AnchorInfo>> = PartialResult::Err {
            error: "SHOW ANCHORS failed: lobe lock".into(),
        };
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(json, r#"{"error":"SHOW ANCHORS failed: lobe lock"}"#);
    }

    #[test]
    fn partial_result_ok_for_object_serialises_as_object() {
        // Sanity check: when T is itself an object (ProfileInfo), Ok
        // variant emits the object directly; Err emits `{"error": ...}`.
        // Distinguishable by presence of `error` key.
        let r: PartialResult<ProfileInfo> = PartialResult::Ok(ProfileInfo {
            pinned_fields: vec!["a".into()],
            learned_patterns: vec![],
            active_ghosts_count: 0,
            vector: None,
        });
        let v: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert!(v.get("pinned_fields").is_some());
        assert!(v.get("error").is_none());
    }

    // ─── parse_show_anchors ──────────────────────────────────────────────

    #[test]
    fn parse_anchors_basic() {
        let lines = vec![
            "Anchors in 'clientes':".to_string(),
            "  - rfc (UNIQUE)".to_string(),
            "  - email (UNIQUE)".to_string(),
        ];
        let parsed = parse_show_anchors(&lines);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].name, "rfc");
        assert!(parsed[0].unique);
        assert_eq!(parsed[1].name, "email");
    }

    #[test]
    fn parse_anchors_empty_lobe() {
        let lines = vec!["Anchors in 'fintech':".to_string()];
        assert!(parse_show_anchors(&lines).is_empty());
    }

    // ─── parse_show_ghosts_filtered ──────────────────────────────────────

    #[test]
    fn parse_ghosts_filters_by_source_lobe() {
        let lines = vec![
            "Ghost Lobes:".to_string(),
            "  ov_by_date — from 'creditos' order by 'due_date' (1500 records, 2 filters)"
                .to_string(),
            "  client_top — from 'clientes' order by 'monto' (200 records, 0 filters)".to_string(),
            "  cred_by_rfc — from 'creditos' order by '' (3000 records, 1 filters)".to_string(),
        ];
        let parsed = parse_show_ghosts_filtered(&lines, "creditos");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].name, "ov_by_date");
        assert_eq!(parsed[0].source_lobe, "creditos");
        assert_eq!(parsed[0].order_by.as_deref(), Some("due_date"));
        assert_eq!(parsed[0].record_count, 1500);
        assert_eq!(parsed[0].filter_count, 2);
        assert_eq!(parsed[1].name, "cred_by_rfc");
        assert_eq!(parsed[1].order_by, None); // empty order_by → None
    }

    #[test]
    fn parse_ghosts_no_ghosts_returns_empty() {
        let lines = vec!["No Ghost Lobes.".to_string()];
        assert!(parse_show_ghosts_filtered(&lines, "any").is_empty());
    }

    #[test]
    fn parse_ghosts_filters_out_other_lobes() {
        let lines = vec![
            "Ghost Lobes:".to_string(),
            "  not_mine — from 'other_lobe' order by 'x' (10 records, 0 filters)".to_string(),
        ];
        assert!(parse_show_ghosts_filtered(&lines, "creditos").is_empty());
    }

    // ─── parse_show_profile ──────────────────────────────────────────────

    #[test]
    fn parse_profile_pinned_fields() {
        let lines = vec![
            "Profile for 'creditos':".to_string(),
            "  Pinned: monto, rfc, status".to_string(),
            "  Learned: (no scan patterns yet)".to_string(),
            "  Ghosts: (none)".to_string(),
        ];
        let p = parse_show_profile(&lines);
        assert_eq!(p.pinned_fields, vec!["monto", "rfc", "status"]);
        assert!(p.learned_patterns.is_empty());
        assert_eq!(p.active_ghosts_count, 0);
    }

    #[test]
    fn parse_profile_vector_declared_with_dim() {
        let lines = vec![
            "Profile for 'mem':".to_string(),
            "  Pinned: (none)".to_string(),
            "  Vector: embedding dim 768".to_string(),
            "  Ghosts: (none)".to_string(),
        ];
        let v = parse_show_profile(&lines).vector.expect("vector present");
        assert_eq!(v.field, "embedding");
        assert_eq!(v.dim, Some(768));
    }

    #[test]
    fn parse_profile_vector_dim_unknown() {
        let lines = vec![
            "Profile for 'mem':".to_string(),
            "  Vector: embedding dim unknown".to_string(),
        ];
        let v = parse_show_profile(&lines).vector.expect("vector present");
        assert_eq!(v.field, "embedding");
        assert_eq!(v.dim, None);
    }

    #[test]
    fn parse_profile_vector_none() {
        let lines = vec![
            "Profile for 'mem':".to_string(),
            "  Vector: (none)".to_string(),
        ];
        assert!(parse_show_profile(&lines).vector.is_none());
    }

    #[test]
    fn parse_profile_vector_absent_line_tolerated() {
        // Older server that predates the Vector line: absent, not a panic.
        let lines = vec![
            "Profile for 'mem':".to_string(),
            "  Pinned: (none)".to_string(),
            "  Ghosts: (none)".to_string(),
        ];
        assert!(parse_show_profile(&lines).vector.is_none());
    }

    #[test]
    fn parse_profile_no_pins() {
        let lines = vec![
            "Profile for 'creditos':".to_string(),
            "  Pinned: (none)".to_string(),
            "  Learned: (no scan patterns yet)".to_string(),
            "  Ghosts: (none)".to_string(),
        ];
        let p = parse_show_profile(&lines);
        assert!(p.pinned_fields.is_empty());
    }

    #[test]
    fn parse_profile_learned_patterns_captured() {
        let lines = vec![
            "Profile for 'creditos':".to_string(),
            "  Pinned: (none)".to_string(),
            "  Learned patterns:".to_string(),
            "    pattern_1 — 5 hits, avg 23 ms".to_string(),
            "    pattern_2 — 7 hits, avg 41 ms".to_string(),
            "  Ghosts: (none)".to_string(),
        ];
        let p = parse_show_profile(&lines);
        assert_eq!(p.learned_patterns.len(), 2);
        assert!(p.learned_patterns[0].contains("pattern_1"));
        assert!(p.learned_patterns[1].contains("pattern_2"));
    }

    #[test]
    fn parse_profile_ghosts_count_extracted() {
        let lines = vec![
            "Profile for 'creditos':".to_string(),
            "  Pinned: (none)".to_string(),
            "  Learned: (no scan patterns yet)".to_string(),
            "  Ghosts: 3 active".to_string(),
            "    g1 — 100 records, 0 filters".to_string(),
            "    g2 — 200 records, 1 filters".to_string(),
            "    g3 — 50 records, 0 filters".to_string(),
        ];
        let p = parse_show_profile(&lines);
        assert_eq!(p.active_ghosts_count, 3);
        // Ghost detail lines are intentionally not surfaced via profile;
        // the dedicated SHOW GHOSTS call provides structured GhostInfo.
    }

    #[test]
    fn parse_profile_full_combined() {
        let lines = vec![
            "Profile for 'creditos':".to_string(),
            "  Pinned: monto, rfc".to_string(),
            "  Learned patterns:".to_string(),
            "    p1".to_string(),
            "  Ghosts: 1 active".to_string(),
            "    g1 — ...".to_string(),
        ];
        let p = parse_show_profile(&lines);
        assert_eq!(p.pinned_fields, vec!["monto", "rfc"]);
        assert_eq!(p.learned_patterns, vec!["p1"]);
        assert_eq!(p.active_ghosts_count, 1);
    }
}
