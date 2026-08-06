//! Query policy for the MCP `query` tool (S1b).
//!
//! The `query` tool accepts the full xyTalk grammar — including destructive
//! verbs (`DELETE`, `DROP GHOST`, `… | DELETE`). When the caller is an
//! automated agent, one mistaken statement can wipe the data. The operator
//! can restrict the accepted verbs at the MCP layer with `--query-policy`,
//! enforced BEFORE the statement reaches the engine (covers both `--embed` and
//! `--connect`). Classification is by parsed AST, not substring matching, so a
//! field or value literally named "delete" never trips the guard.

// SPDX-License-Identifier: BUSL-1.1
use clap::ValueEnum;
use xytalk_parser::ast::{PipelineStep, Statement};

/// How much the `query` tool is allowed to mutate. Default `Full` keeps the
/// historical behaviour (no restriction); the restricted modes protect the
/// data from accidental destruction by an automated caller.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryPolicy {
    /// All verbs (default; back-compatible).
    Full,
    /// Reads + additive/update writes (PUT/SET/LINK/…). Blocks `DELETE` and
    /// `DROP` — the recommended posture for an agent-driven database.
    NoDestructive,
    /// Reads only. Blocks every mutation.
    ReadOnly,
}

impl QueryPolicy {
    /// Stable identifier for error messages / logs.
    pub fn as_str(self) -> &'static str {
        match self {
            QueryPolicy::Full => "full",
            QueryPolicy::NoDestructive => "no-destructive",
            QueryPolicy::ReadOnly => "read-only",
        }
    }

    /// Whether a statement of `class` may run under this policy.
    pub fn allows(self, class: StatementClass) -> bool {
        match self {
            QueryPolicy::Full => true,
            QueryPolicy::NoDestructive => class != StatementClass::Destructive,
            QueryPolicy::ReadOnly => class == StatementClass::Read,
        }
    }
}

/// What a statement does to the data, in increasing order of impact.
/// `Read` < `Write` < `Destructive` (the `Ord` derive relies on this order).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum StatementClass {
    /// Never mutates persistent data (SCAN/FIND/SHOW/SCRUB/…).
    Read,
    /// Inserts, updates, or maintains data/structures (PUT/SET/LINK/ANALYZE/…).
    Write,
    /// Removes data or structures (DELETE / DROP GHOST / pipeline DELETE).
    Destructive,
}

/// Classify a parsed statement. Exhaustive over `Statement` so a new verb
/// forces a deliberate decision at compile time rather than slipping through.
pub fn classify(stmt: &Statement) -> StatementClass {
    use StatementClass::*;
    match stmt {
        // Reads — never mutate persistent data. SCRUB only verifies checksums.
        Statement::Find(_)
        | Statement::Pull(_)
        | Statement::Scan(_)
        | Statement::Show(_)
        | Statement::ScanGhost(_)
        | Statement::Fetch(_)
        | Statement::Scrub => Read,

        // Destructive — remove data or a derived structure.
        Statement::Delete(_) | Statement::Purge(_) | Statement::DropGhost(_) => Destructive,

        // Additive / update / config / maintenance writes.
        Statement::Put(_)
        | Statement::PutBatch(_)
        | Statement::Set(_)
        | Statement::Link(_)
        | Statement::Anchor(_)
        | Statement::Gravity(_)
        | Statement::Vector(_)
        | Statement::Satellite(_)
        | Statement::Lobe(_)
        | Statement::AutoAnchorApply(_)
        | Statement::CreateGhost(_)
        | Statement::RefreshGhost(_)
        | Statement::Analyze(_)
        | Statement::Compact
        | Statement::BulkMode(_)
        | Statement::Migrate(_)
        | Statement::InCache(_)
        | Statement::OutCache(_)
        | Statement::Pin(_)
        | Statement::Unpin(_) => Write,

        // A pipeline is as impactful as its most-impactful step.
        Statement::Pipeline(steps) => classify_pipeline(steps),
    }
}

fn classify_pipeline(steps: &[PipelineStep]) -> StatementClass {
    use StatementClass::*;
    let mut cls = Read;
    for step in steps {
        let step_cls = match step {
            PipelineStep::Delete(_) => Destructive,
            PipelineStep::Set(_) => Write,
            PipelineStep::Find(_)
            | PipelineStep::Pull(_)
            | PipelineStep::Scan(_)
            | PipelineStep::ScanGhost(_)
            | PipelineStep::Aggregate(_)
            | PipelineStep::GroupBy(_)
            | PipelineStep::Nearest(_)
            | PipelineStep::Follow(_)
            | PipelineStep::Top(_)
            | PipelineStep::Shape(_) => Read,
        };
        cls = cls.max(step_cls);
    }
    cls
}

#[cfg(test)]
mod tests {
    use super::*;

    fn class_of(q: &str) -> StatementClass {
        classify(&xytalk_parser::parse(q).unwrap_or_else(|e| panic!("parse {q:?}: {e:?}")))
    }

    #[test]
    fn reads_writes_destructives_classified() {
        assert_eq!(class_of(r#"SCAN "m" WHERE x = 1"#), StatementClass::Read);
        assert_eq!(class_of(r#"FIND "m" WHERE id = "a""#), StatementClass::Read);
        assert_eq!(class_of(r#"PUT {a: 1} IN "m""#), StatementClass::Write);
        assert_eq!(
            class_of(r#"SET "m" a = 2 WHERE id = "a""#),
            StatementClass::Write
        );
        assert_eq!(
            class_of(r#"DELETE "m" WHERE x = 1"#),
            StatementClass::Destructive
        );
        assert_eq!(class_of(r#"DROP GHOST "g""#), StatementClass::Destructive);
    }

    #[test]
    fn pipeline_takes_most_destructive_step() {
        assert_eq!(
            class_of(r#"SCAN "m" WHERE x = 1 | DELETE"#),
            StatementClass::Destructive
        );
        assert_eq!(
            class_of(r#"SCAN "m" WHERE x = 1 | NEAREST(emb, [1.0, 0.0], 3, cosine)"#),
            StatementClass::Read
        );
    }

    #[test]
    fn policy_gates() {
        // Full allows everything.
        for c in [
            StatementClass::Read,
            StatementClass::Write,
            StatementClass::Destructive,
        ] {
            assert!(QueryPolicy::Full.allows(c));
        }
        // NoDestructive blocks only destructive.
        assert!(QueryPolicy::NoDestructive.allows(StatementClass::Read));
        assert!(QueryPolicy::NoDestructive.allows(StatementClass::Write));
        assert!(!QueryPolicy::NoDestructive.allows(StatementClass::Destructive));
        // ReadOnly blocks every mutation.
        assert!(QueryPolicy::ReadOnly.allows(StatementClass::Read));
        assert!(!QueryPolicy::ReadOnly.allows(StatementClass::Write));
        assert!(!QueryPolicy::ReadOnly.allows(StatementClass::Destructive));
    }
}
