//! Anti-drift gate for the xyTalk grammar the `query` tool advertises to agent
//! callers (xyTalk v1 P15).
//!
//! The served grammar is the only way an agent discovers what the language
//! accepts. When the language moves and the text does not, every agent keeps
//! trying the old surface (an `ORBIT` that no longer parses) and never learns
//! the new one (`TAKE`, `PURGE`). rmcp's `#[tool]` requires the description to
//! be a string literal, so the text lives inline on `XyzdbServer::query`
//! (`main.rs`); this module holds the tests that keep it honest.
//!
//! The tests read the *actual served description* back through the generated
//! `tool_router()` (not a copy of it) and check it against the real parser:
//! every advertised example must parse, a removed keyword must be absent from
//! the text AND rejected by the parser, and every current verb must appear.
//! Change the parser without updating the description and `cargo test` fails.

#[cfg(test)]
mod tests {
    /// The served description of the `query` tool — the exact text an MCP client
    /// receives, read back through the generated router (single source of truth).
    fn served_query_description() -> String {
        let router = crate::XyzdbServer::tool_router();
        let tool = router
            .get("query")
            .expect("the `query` tool must be registered");
        tool.description
            .as_deref()
            .expect("the `query` tool must carry a description")
            .to_string()
    }

    /// One canonical statement per verb / form the description advertises. Every
    /// entry must parse through the real parser, so the grammar can never teach a
    /// form the engine would reject.
    const CANONICAL_EXAMPLES: &[&str] = &[
        r#"PUT {a: 1, *g: "x"} IN "l""#,
        r#"PUT BATCH IN "l" [{a: 1}, {a: 2}]"#,
        r#"FIND "l" WHERE a = 1 AND b = 2"#,
        r#"SCAN "l" WHERE a = 1 OR b = 2"#,
        r#"SCAN "l" WHERE a IN [1, 2]"#,
        r#"SCAN "l" WHERE a IN (1, 2)"#,
        r#"SCAN "l" WHERE status = "x" ORDER BY due LIMIT 10"#,
        r#"SET "l" a = 2 WHERE b = 1 OR c = 3"#,
        r#"DELETE "l" WHERE a = 1"#,
        r#"PURGE "l""#,
        r#"LINK "s" WHERE a = 1 TO "d" WHERE b = 2 AS "rel""#,
        r#"FETCH "a", "b" WHERE rfc = "x""#,
        r#"FETCH "a", "b" WHERE rfc = "x" AS {sa, sb}"#,
        r#"SCAN "l" | AGGREGATE count(*)"#,
        r#"SCAN "l" | GROUP BY g | AGGREGATE sum(x) | TAKE 5 BY sum(x) DESC"#,
        r#"SCAN "l" | GROUP BY g | AGGREGATE sum(x) | TOP 5 BY sum(x)"#,
        r#"SCAN "l" | TAKE 3"#,
        r#"SCAN "l" | SHAPE {k, grp}"#,
        r#"VECTOR emb IN "l""#,
        r#"SCAN "l" WHERE g = "x" | NEAREST 5 BY emb TO $q USING cosine"#,
        r#"SCAN "l" WHERE g = "x" | NEAREST 5 BY emb TO $q"#,
        r#"SCAN "l" WHERE g = "x" | NEAREST(emb, $q, 5, cosine)"#,
        r#"CREATE GHOST "gname" FROM "l" | GROUP BY g | AGGREGATE sum(x) | TAKE BY sum(x) DESC"#,
        r#"CREATE GHOST "gname2" FROM "l" ORDER BY x GROUP BY g AGGREGATE sum(x)"#,
        r#"GRAVITY BY rfc IN "l""#,
        r#"SHOW LOBES"#,
        r#"SHOW GHOSTS"#,
    ];

    /// Every advertised form parses through the real parser: the grammar can
    /// never teach a statement the engine would reject.
    #[test]
    fn every_canonical_example_parses() {
        for stmt in CANONICAL_EXAMPLES {
            xytalk_parser::parse(stmt).unwrap_or_else(|e| {
                panic!("advertised grammar example does not parse: {stmt}\n  err: {e:?}")
            });
        }
    }

    /// A removed keyword is gone from the served text AND rejected by the parser
    /// — the two must agree, so the doc can't keep teaching a dead verb.
    #[test]
    fn removed_keywords_absent_and_rejected() {
        assert!(
            !served_query_description().contains("ORBIT"),
            "ORBIT was removed from the language; it must not appear in the served grammar"
        );
        assert!(
            xytalk_parser::parse(r#"SCAN "l" WHERE g = "x" | ORBIT(emb, $q, 5, cosine)"#).is_err(),
            "ORBIT must be rejected by the parser (kept in sync with the grammar)"
        );
    }

    /// Every current verb / form the language gained is actually advertised, so
    /// an agent can discover it from the served description.
    #[test]
    fn current_verbs_are_advertised() {
        let desc = served_query_description();
        for verb in [
            "TAKE",
            "PURGE",
            "NEAREST k BY",
            "count(*)",
            "CREATE GHOST",
            "IN [",
            "SHAPE {",
            "FETCH",
            "SATELLITE BY",
        ] {
            assert!(
                desc.contains(verb),
                "served grammar must advertise `{verb}`, but it is missing"
            );
        }
    }

    // ─── the other direction: everything the parser accepts is accounted for ───
    //
    // The tests above prove that what the description advertises parses. They
    // cannot catch the opposite: a statement the parser accepts that the
    // description never mentions. That is the failure that actually happened —
    // 1.1 added `SATELLITE BY` and the served grammar did not gain a word about
    // it, so an agent could read the axis back from `describe_lobe` and had no
    // way to learn how to declare one. Nothing above fired, because everything
    // listed still parsed.
    //
    // So the match below is exhaustive over `Statement` on purpose: adding a
    // variant to the parser does not compile here until someone decides whether
    // an agent should be told about it. `NotAdvertised` is a decision with a
    // reason attached, never an omission — same rule as the debt register.

    /// What the served grammar owes a given statement.
    enum Owed {
        /// The description must contain this exact substring.
        Phrase(&'static str),
        /// Deliberately absent from the agent-facing grammar, for this reason.
        NotAdvertised(&'static str),
    }

    /// Exhaustive by design — see the comment above.
    fn owed_for(stmt: &xytalk_parser::ast::Statement) -> Owed {
        use xytalk_parser::ast::Statement as S;
        match stmt {
            S::Put(_) => Owed::Phrase("PUT {field"),
            S::PutBatch(_) => Owed::Phrase("PUT BATCH IN"),
            S::Find(_) => Owed::Phrase("FIND \"lobe\" WHERE"),
            S::Pull(_) => Owed::Phrase("PULL"),
            S::Scan(_) => Owed::Phrase("SCAN \"lobe\" WHERE"),
            S::Set(_) => Owed::Phrase("SET \"lobe\""),
            S::Delete(_) => Owed::Phrase("DELETE \"lobe\" WHERE"),
            S::Purge(_) => Owed::Phrase("PURGE \"lobe\""),
            S::Fetch(_) => Owed::Phrase("FETCH"),
            S::Link(_) => Owed::Phrase("LINK \"src\""),
            S::Anchor(_) => Owed::Phrase("ANCHOR \"field\" UNIQUE IN"),
            S::Gravity(_) => Owed::Phrase("GRAVITY BY"),
            S::Vector(_) => Owed::Phrase("VECTOR field IN"),
            S::Satellite(_) => Owed::Phrase("SATELLITE BY field IN"),
            S::Lobe(_) => Owed::Phrase("LOBE \"name\""),
            S::Show(_) => Owed::Phrase("SHOW LOBES"),
            S::AutoAnchorApply(_) => Owed::Phrase("AUTOANCHOR APPLY"),
            S::CreateGhost(_) => Owed::Phrase("CREATE GHOST"),
            S::ScanGhost(_) => Owed::Phrase("SCAN GHOST"),
            S::RefreshGhost(_) => Owed::Phrase("REFRESH GHOST"),
            S::DropGhost(_) => Owed::Phrase("DROP GHOST"),
            S::Pipeline(_) => Owed::Phrase(" | "),
            S::Analyze(_) => Owed::Phrase("ANALYZE"),
            S::Compact => Owed::Phrase("COMPACT"),
            S::InCache(_) => Owed::Phrase("INCACHE"),
            S::OutCache(_) => Owed::Phrase("OUTCACHE"),
            S::Pin(_) => Owed::Phrase("PIN field IN"),
            S::Unpin(_) => Owed::Phrase("UNPIN"),
            // Operator housekeeping an agent should not reach for. They are
            // reachable through `query` if explicitly asked, but the grammar
            // does not teach them: BULKMODE trades durability for load speed,
            // MIGRATE rewrites on-disk records, SCRUB is a full-disk read.
            // The operational line already tells the agent this class exists
            // and to leave it alone.
            S::BulkMode(_) => Owed::NotAdvertised("relaxes durability; operator-only"),
            S::Migrate(_) => Owed::NotAdvertised("rewrites on-disk records; operator-only"),
            S::Scrub => Owed::NotAdvertised("full-disk integrity read; operator-only"),
        }
    }

    /// One parseable sample per `Statement` variant. Checked for completeness
    /// below, so this list cannot quietly fall behind the enum.
    const ONE_PER_VARIANT: &[&str] = &[
        r#"PUT {a: 1} IN "l""#,
        r#"PUT BATCH IN "l" [{a: 1}]"#,
        r#"FIND "l" WHERE a = 1"#,
        r#"PULL FROM "l" depth=2"#,
        r#"SCAN "l" WHERE a = 1"#,
        r#"SET "l" a = 2 WHERE b = 1"#,
        r#"DELETE "l" WHERE a = 1"#,
        r#"PURGE "l""#,
        r#"FETCH "a", "b" WHERE rfc = "x""#,
        r#"LINK "s" WHERE a = 1 TO "d" WHERE b = 2 AS "rel""#,
        r#"ANCHOR "code" UNIQUE IN "l""#,
        r#"GRAVITY BY rfc IN "l""#,
        r#"VECTOR emb IN "l""#,
        r#"SATELLITE BY kind IN "l""#,
        r#"LOBE "l""#,
        r#"SHOW LOBES"#,
        r#"AUTOANCHOR APPLY "code" IN "l""#,
        r#"CREATE GHOST "g" FROM "l" | GROUP BY g | AGGREGATE sum(x) | TAKE BY sum(x) DESC"#,
        r#"SCAN GHOST "g" LIMIT 10"#,
        r#"REFRESH GHOST "g""#,
        r#"DROP GHOST "g""#,
        r#"SCAN "l" | SHAPE {a}"#,
        r#"ANALYZE "l""#,
        r#"COMPACT"#,
        r#"SCRUB"#,
        r#"BULKMODE ON"#,
        r#"MIGRATE "l""#,
        r#"INCACHE "l""#,
        r#"OUTCACHE "l""#,
        r#"PIN a IN "l""#,
        r#"UNPIN a IN "l""#,
    ];

    /// Number of `Statement` variants. Lives next to `owed_for`: when the parser
    /// gains a variant, `owed_for` stops compiling, and whoever fixes it lands
    /// here and in `ONE_PER_VARIANT`.
    const STATEMENT_VARIANTS: usize = 31;

    /// Every statement the parser accepts either appears in the served grammar
    /// or is explicitly marked as not advertised, with a reason.
    #[test]
    fn every_statement_is_advertised_or_explicitly_not() {
        let desc = served_query_description();
        let mut seen = std::collections::HashSet::new();

        for sample in ONE_PER_VARIANT {
            let stmt = xytalk_parser::parse(sample)
                .unwrap_or_else(|e| panic!("sample does not parse: {sample}\n  err: {e:?}"));
            seen.insert(std::mem::discriminant(&stmt));
            match owed_for(&stmt) {
                Owed::Phrase(p) => assert!(
                    desc.contains(p),
                    "the parser accepts `{sample}` but the served grammar never says \
                     `{p}` — an agent cannot discover it"
                ),
                // A reason is the whole point of the variant: "nobody wrote one"
                // and "we decided not to advertise this" must not look alike.
                Owed::NotAdvertised(reason) => assert!(
                    !reason.trim().is_empty(),
                    "`{sample}` is marked as not advertised with no reason given"
                ),
            }
        }

        assert_eq!(
            seen.len(),
            STATEMENT_VARIANTS,
            "ONE_PER_VARIANT must cover every Statement variant exactly once; \
             {} distinct variants reached from {} samples",
            seen.len(),
            ONE_PER_VARIANT.len()
        );
    }

    /// Negative control for the test above: a phrase the description does not
    /// contain must be reported as missing. Without this, a gate whose
    /// `desc.contains` always passed would look identical to a healthy one.
    #[test]
    fn advertising_check_can_fail() {
        let desc = served_query_description();
        assert!(
            !desc.contains("SUBLIMATE BY"),
            "control phrase leaked into the description; pick another"
        );
        // The same assertion shape the real test uses, inverted: proof that a
        // missing phrase is detectable rather than silently absent.
        let missing = !desc.contains("SUBLIMATE BY");
        assert!(missing, "the contains-check cannot distinguish absence");
    }
}
