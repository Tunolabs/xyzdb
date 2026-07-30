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
        ] {
            assert!(
                desc.contains(verb),
                "served grammar must advertise `{verb}`, but it is missing"
            );
        }
    }
}
