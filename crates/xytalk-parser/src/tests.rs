use crate::ast::*;
use crate::parse;

#[test]
fn test_put_simple() {
    let stmt = parse(r#"PUT {code: "ACME-001", name: "Acme Corp"} IN "workspace""#).unwrap();
    match stmt {
        Statement::Put(p) => {
            assert_eq!(p.lobe, "workspace");
            assert_eq!(p.fields.len(), 2);
            assert_eq!(p.fields[0].name, "code");
            assert_eq!(p.fields[0].value, Literal::Text("ACME-001".into()));
            assert!(p.link.is_none());
            assert!(p.on_conflict.is_none());
        }
        _ => panic!("Expected Put, got {stmt:?}"),
    }
}

#[test]
fn test_put_numeric_fields() {
    let stmt = parse(r#"PUT {budget: 50000, rate: 0.18, active: true} IN "workspace""#).unwrap();
    match stmt {
        Statement::Put(p) => {
            assert_eq!(p.fields[0].name, "budget");
            assert_eq!(p.fields[0].value, Literal::Int(50000));
            assert_eq!(p.fields[1].name, "rate");
            assert_eq!(p.fields[1].value, Literal::Float(0.18));
            assert_eq!(p.fields[2].name, "active");
            assert_eq!(p.fields[2].value, Literal::Bool(true));
        }
        _ => panic!("Expected Put"),
    }
}

#[test]
fn test_put_on_conflict() {
    let stmt = parse(r#"PUT {code: "X"} IN "workspace" ON CONFLICT UPDATE"#).unwrap();
    match stmt {
        Statement::Put(p) => {
            assert_eq!(p.on_conflict, Some(OnConflict::Update));
        }
        _ => panic!("Expected Put"),
    }
}

#[test]
fn test_find_by_field() {
    let stmt = parse(r#"FIND "workspace" WHERE code = "ACME-001""#).unwrap();
    match stmt {
        Statement::Find(f) => {
            assert_eq!(f.target, FindTarget::Lobe("workspace".into()));
            assert_eq!(f.filters.len(), 1);
            assert_eq!(f.filters[0].field, "code");
            assert_eq!(f.filters[0].op, FilterOp::Eq);
        }
        _ => panic!("Expected Find"),
    }
}

#[test]
fn test_find_by_lid() {
    let stmt = parse(r#"FIND LID("0000:001A:0005E2F3A1B0:00000001:0000")"#).unwrap();
    match stmt {
        Statement::Find(f) => {
            assert!(matches!(f.target, FindTarget::ByLid(_)));
            assert!(f.filters.is_empty());
        }
        _ => panic!("Expected Find"),
    }
}

#[test]
fn test_find_multi_filter() {
    let stmt = parse(r#"FIND "workspace" WHERE budget > 10000 AND status = "active""#).unwrap();
    match stmt {
        Statement::Find(f) => {
            assert_eq!(f.filters.len(), 2);
            assert_eq!(f.filters[0].op, FilterOp::Gt);
            assert_eq!(f.filters[1].op, FilterOp::Eq);
        }
        _ => panic!("Expected Find"),
    }
}

#[test]
fn test_find_unquoted_lobe() {
    let stmt = parse(r#"FIND Company WHERE code = "ACME-001""#).unwrap();
    match stmt {
        Statement::Find(f) => {
            assert_eq!(f.target, FindTarget::Lobe("Company".into()));
        }
        _ => panic!("Expected Find"),
    }
}

#[test]
fn test_pull_pipeline() {
    let stmt = parse(r#"FIND "workspace" WHERE code = "X" | PULL depth=3"#).unwrap();
    match stmt {
        Statement::Pipeline(steps) => {
            assert_eq!(steps.len(), 2);
            assert!(matches!(steps[0], PipelineStep::Find(_)));
            match &steps[1] {
                PipelineStep::Pull(p) => {
                    assert_eq!(p.depth, 3);
                    assert!(p.target.is_none());
                }
                _ => panic!("Expected Pull step"),
            }
        }
        _ => panic!("Expected Pipeline"),
    }
}

#[test]
fn test_pull_with_only() {
    let stmt = parse(r#"FIND "workspace" WHERE code = "X" | PULL depth=2 only=Task"#).unwrap();
    match stmt {
        Statement::Pipeline(steps) => match &steps[1] {
            PipelineStep::Pull(p) => {
                assert_eq!(p.depth, 2);
                assert_eq!(p.only, Some("Task".into()));
            }
            _ => panic!("Expected Pull step"),
        },
        _ => panic!("Expected Pipeline"),
    }
}

#[test]
fn test_scan_with_filter() {
    let stmt = parse(r#"SCAN "workspace" WHERE status = "pending""#).unwrap();
    match stmt {
        Statement::Scan(s) => {
            assert_eq!(s.lobe, "workspace");
            assert!(s.filter_expr.is_some());
        }
        _ => panic!("Expected Scan"),
    }
}

#[test]
fn test_set_pipeline() {
    let stmt = parse(r#"FIND "workspace" WHERE code = "X" | SET status = "inactive""#).unwrap();
    match stmt {
        Statement::Pipeline(steps) => {
            assert_eq!(steps.len(), 2);
            match &steps[1] {
                PipelineStep::Set(s) => {
                    assert_eq!(s.assignments[0].0, "status");
                    assert_eq!(s.assignments[0].1, Literal::Text("inactive".into()));
                }
                _ => panic!("Expected Set step"),
            }
        }
        _ => panic!("Expected Pipeline"),
    }
}

#[test]
fn test_delete_pipeline_step() {
    let stmt = parse(r#"FIND "workspace" WHERE code = "X" | DELETE"#).unwrap();
    match stmt {
        Statement::Pipeline(steps) => match &steps[1] {
            PipelineStep::Delete(_) => {}
            _ => panic!("Expected Delete step"),
        },
        _ => panic!("Expected Pipeline"),
    }
}

#[test]
fn test_anchor() {
    let stmt = parse(r#"ANCHOR "code" UNIQUE IN "workspace""#).unwrap();
    match stmt {
        Statement::Anchor(a) => {
            assert_eq!(a.field, "code");
            assert_eq!(a.lobe, "workspace");
        }
        _ => panic!("Expected Anchor"),
    }
}

#[test]
fn test_lobe() {
    let stmt = parse(r#"LOBE "catalog" HINT="products and inventory""#).unwrap();
    match stmt {
        Statement::Lobe(l) => {
            assert_eq!(l.name, "catalog");
            assert_eq!(l.hint, Some("products and inventory".into()));
        }
        _ => panic!("Expected Lobe"),
    }
}

#[test]
fn test_lobe_no_hint() {
    let stmt = parse(r#"LOBE "workspace""#).unwrap();
    match stmt {
        Statement::Lobe(l) => {
            assert_eq!(l.name, "workspace");
            assert!(l.hint.is_none());
        }
        _ => panic!("Expected Lobe"),
    }
}

#[test]
fn test_show_lobes() {
    let stmt = parse("SHOW LOBES").unwrap();
    assert_eq!(stmt, Statement::Show(ShowStmt::Lobes));
}

#[test]
fn test_show_anchors() {
    let stmt = parse(r#"SHOW ANCHORS IN "workspace""#).unwrap();
    match stmt {
        Statement::Show(ShowStmt::Anchors(lobe)) => assert_eq!(lobe, "workspace"),
        _ => panic!("Expected Show Anchors"),
    }
}

#[test]
fn test_case_insensitive() {
    let stmt = parse(r#"put {x: 1} in "t""#).unwrap();
    assert!(matches!(stmt, Statement::Put(_)));

    let stmt2 = parse(r#"Put {x: 1} In "t""#).unwrap();
    assert!(matches!(stmt2, Statement::Put(_)));
}

#[test]
fn test_comments() {
    let stmt = parse(r#"PUT {x: 1} IN "t" -- this is a comment"#).unwrap();
    assert!(matches!(stmt, Statement::Put(_)));
}

#[test]
fn test_error_missing_in() {
    let result = parse(r#"PUT {x: 1}"#);
    assert!(result.is_err());
}

#[test]
fn test_error_empty() {
    let result = parse("");
    assert!(result.is_err());
}

#[test]
fn test_timestamp_literal() {
    let stmt = parse(r#"PUT {due_date: @"2026-03-25"} IN "t""#).unwrap();
    match stmt {
        Statement::Put(p) => {
            assert_eq!(p.fields[0].value, Literal::Timestamp("2026-03-25".into()));
        }
        _ => panic!("Expected Put"),
    }
}

#[test]
fn test_negative_number() {
    let stmt = parse(r#"PUT {balance: -5000} IN "t""#).unwrap();
    match stmt {
        Statement::Put(p) => {
            assert_eq!(p.fields[0].value, Literal::Int(-5000));
        }
        _ => panic!("Expected Put"),
    }
}

#[test]
fn test_scan_unquoted() {
    let stmt = parse(r#"SCAN workspace WHERE status = "pending""#).unwrap();
    match stmt {
        Statement::Scan(s) => assert_eq!(s.lobe, "workspace"),
        _ => panic!("Expected Scan"),
    }
}

#[test]
fn test_pull_from() {
    let stmt = parse(r#"PULL FROM "workspace" depth=2"#).unwrap();
    match stmt {
        Statement::Pull(p) => {
            assert_eq!(p.depth, 2);
            assert!(p.target.is_some());
        }
        _ => panic!("Expected Pull"),
    }
}

#[test]
fn test_utf8_accented_chars() {
    let stmt = parse(r#"PUT {name: "José María", country: "México"} IN "test""#).unwrap();
    match stmt {
        Statement::Put(p) => {
            assert_eq!(p.fields[0].value, Literal::Text("José María".into()));
            assert_eq!(p.fields[1].value, Literal::Text("México".into()));
        }
        _ => panic!("Expected Put"),
    }
}

#[test]
fn test_utf8_in_filter() {
    let stmt = parse(r#"FIND "test" WHERE name = "José""#).unwrap();
    match stmt {
        Statement::Find(f) => {
            assert_eq!(f.filters[0].value, Literal::Text("José".into()));
        }
        _ => panic!("Expected Find"),
    }
}

#[test]
fn test_put_batch_simple() {
    let stmt = parse(r#"PUT BATCH IN "workspace" [{x: 1}, {x: 2}, {x: 3}]"#).unwrap();
    match stmt {
        Statement::PutBatch(b) => {
            assert_eq!(b.lobe, "workspace");
            assert_eq!(b.records.len(), 3);
            assert!(b.link.is_none());
        }
        _ => panic!("Expected PutBatch"),
    }
}

#[test]
fn test_put_batch_with_link() {
    let stmt = parse(
        r#"PUT BATCH IN "workspace" [{numero: 1, hours: 8}, {numero: 2, hours: 4}] LINK TO "workspace" WHERE project_id = "PRJ-001" AS "task_of""#,
    ).unwrap();
    match stmt {
        Statement::PutBatch(b) => {
            assert_eq!(b.records.len(), 2);
            assert!(b.link.is_some());
            assert_eq!(b.link.as_ref().unwrap().relation_name, "task_of");
        }
        _ => panic!("Expected PutBatch"),
    }
}

#[test]
fn test_put_batch_with_types() {
    let stmt = parse(
        r#"PUT BATCH IN "workspace" [{_type: "Task", n: 1, hours: 8, status: "pending"}, {_type: "Task", n: 2, hours: 4, status: "pending"}]"#,
    ).unwrap();
    match stmt {
        Statement::PutBatch(b) => {
            assert_eq!(b.records.len(), 2);
            assert_eq!(b.records[0][0].name, "_type");
            assert_eq!(b.records[0][0].value, Literal::Text("Task".into()));
        }
        _ => panic!("Expected PutBatch"),
    }
}

#[test]
fn test_scan_with_limit() {
    let stmt = parse(r#"SCAN "workspace" WHERE status = "pending" LIMIT 100"#).unwrap();
    match stmt {
        Statement::Scan(s) => {
            assert_eq!(s.lobe, "workspace");
            assert!(s.filter_expr.is_some());
            assert_eq!(s.limit, Some(100));
        }
        _ => panic!("Expected Scan"),
    }
}

#[test]
fn test_scan_without_limit() {
    let stmt = parse(r#"SCAN "workspace" WHERE status = "pending""#).unwrap();
    match stmt {
        Statement::Scan(s) => {
            assert_eq!(s.limit, None);
        }
        _ => panic!("Expected Scan"),
    }
}

#[test]
fn test_scan_limit_no_where() {
    let stmt = parse(r#"SCAN "workspace" LIMIT 50"#).unwrap();
    match stmt {
        Statement::Scan(s) => {
            assert_eq!(s.lobe, "workspace");
            assert!(s.filter_expr.is_none());
            assert_eq!(s.limit, Some(50));
        }
        _ => panic!("Expected Scan"),
    }
}

#[test]
fn test_scan_ghost_with_limit() {
    let stmt = parse(r#"SCAN GHOST "overdue" WHERE due_date < "2026-01-01" LIMIT 500"#).unwrap();
    match stmt {
        Statement::ScanGhost(s) => {
            assert_eq!(s.name, "overdue");
            let f = s.filter_expr.as_ref().unwrap().as_flat_and().unwrap();
            assert_eq!(f.len(), 1);
            assert_eq!(s.limit, Some(500));
        }
        _ => panic!("Expected ScanGhost"),
    }
}

#[test]
fn test_scan_order_by_desc_limit() {
    let stmt =
        parse(r#"SCAN "workspace" WHERE _type = "Credit" ORDER BY balance DESC LIMIT 10"#).unwrap();
    match stmt {
        Statement::Scan(s) => {
            assert_eq!(s.lobe, "workspace");
            assert!(s.filter_expr.is_some());
            let ob = s.order_by.unwrap();
            assert_eq!(ob.field, "balance");
            assert!(ob.descending);
            assert_eq!(s.limit, Some(10));
        }
        _ => panic!("Expected Scan"),
    }
}

#[test]
fn test_scan_order_by_asc_default() {
    let stmt = parse(r#"SCAN "workspace" ORDER BY name LIMIT 100"#).unwrap();
    match stmt {
        Statement::Scan(s) => {
            let ob = s.order_by.unwrap();
            assert_eq!(ob.field, "name");
            assert!(!ob.descending, "Default should be ASC");
            assert_eq!(s.limit, Some(100));
        }
        _ => panic!("Expected Scan"),
    }
}

#[test]
fn test_scan_ghost_pipeline_parse() {
    let stmt = parse(r#"SCAN GHOST "overdue" | AGGREGATE count()"#).unwrap();
    match stmt {
        Statement::Pipeline(steps) => {
            assert_eq!(steps.len(), 2);
            assert!(matches!(steps[0], PipelineStep::ScanGhost(_)));
            assert!(matches!(steps[1], PipelineStep::Aggregate(_)));
        }
        _ => panic!("Expected Pipeline"),
    }
}

#[test]
fn test_put_gravity_field() {
    let stmt = parse(r#"PUT {*code: "ABC", name: "Test"} IN "items""#).unwrap();
    match stmt {
        Statement::Put(p) => {
            assert_eq!(p.fields.len(), 2);
            assert_eq!(p.fields[0].name, "code");
            assert!(p.fields[0].gravity, "code should be marked as gravity");
            assert_eq!(p.fields[1].name, "name");
            assert!(!p.fields[1].gravity, "name should NOT be gravity");
        }
        _ => panic!("Expected Put"),
    }
}

#[test]
fn test_put_batch_gravity_field() {
    let stmt =
        parse(r#"PUT BATCH IN "items" [{*ref: "R1", val: 1}, {*ref: "R2", val: 2}]"#).unwrap();
    match stmt {
        Statement::PutBatch(b) => {
            assert_eq!(b.records.len(), 2);
            assert!(b.records[0][0].gravity, "ref should be gravity in batch");
            assert!(!b.records[0][1].gravity, "val should NOT be gravity");
        }
        _ => panic!("Expected PutBatch"),
    }
}

#[test]
fn test_migrate_all() {
    let stmt = parse("MIGRATE").unwrap();
    assert_eq!(stmt, Statement::Migrate(None));
}

#[test]
fn test_migrate_lobe() {
    let stmt = parse(r#"MIGRATE "clientes""#).unwrap();
    assert_eq!(stmt, Statement::Migrate(Some("clientes".to_string())));
}

// ─── CURSOR (v0.2.5.1) ────────────────────────────────────────────────────────

#[test]
fn test_scan_cursor_default_none() {
    // Existing SCAN syntax: cursor field defaults to None when CURSOR is absent.
    let stmt = parse(r#"SCAN "creditos" WHERE rfc = "X" LIMIT 100"#).unwrap();
    match stmt {
        Statement::Scan(s) => {
            assert_eq!(s.lobe, "creditos");
            assert_eq!(s.limit, Some(100));
            assert_eq!(s.cursor, None);
        }
        _ => panic!("Expected Scan"),
    }
}

#[test]
fn test_scan_with_cursor() {
    // CURSOR without LIMIT — the engine will apply MAX_LIMIT_DEFAULT.
    let stmt = parse(r#"SCAN "creditos" CURSOR "abc123""#).unwrap();
    match stmt {
        Statement::Scan(s) => {
            assert_eq!(s.lobe, "creditos");
            assert_eq!(s.cursor, Some("abc123".to_string()));
            assert_eq!(s.limit, None);
        }
        _ => panic!("Expected Scan"),
    }
}

#[test]
fn test_scan_limit_then_cursor() {
    // Documented order: WHERE → ORDER BY → LIMIT → CURSOR.
    let stmt =
        parse(r#"SCAN "creditos" WHERE rfc = "X" LIMIT 1000 CURSOR "opaque-token-42""#).unwrap();
    match stmt {
        Statement::Scan(s) => {
            assert!(s.filter_expr.is_some());
            assert_eq!(s.limit, Some(1000));
            assert_eq!(s.cursor, Some("opaque-token-42".to_string()));
        }
        _ => panic!("Expected Scan"),
    }
}

// ─── WHERE in standalone SET / DELETE / LINK (v0.2.5.1) ───────────────────────

#[test]
fn test_set_standalone_with_where() {
    // Pre-v0.2.5.1 the standalone SET grammar did not accept WHERE at all
    // (FAIL-pre). Now it parses and the filter is captured on `filters`.
    let stmt = parse(r#"SET "creditos" status = "paid" WHERE rfc = "X" "#).unwrap();
    match stmt {
        Statement::Set(s) => {
            assert_eq!(s.assignments.len(), 1);
            let f = s.filter_expr.as_ref().unwrap().as_flat_and().unwrap();
            assert_eq!(f.len(), 1);
            assert_eq!(f[0].field, "rfc");
            assert!(matches!(s.target, Some(FindTarget::Lobe(ref n)) if n == "creditos"));
        }
        other => panic!("Expected Set, got: {other:?}"),
    }
}

#[test]
fn test_delete_standalone_with_where() {
    // Standalone DELETE with a WHERE filter.
    let stmt = parse(r#"DELETE "creditos" WHERE status = "cancelled""#).unwrap();
    match stmt {
        Statement::Delete(d) => {
            let f = d.filter_expr.as_ref().unwrap().as_flat_and().unwrap();
            assert_eq!(f.len(), 1);
            assert_eq!(f[0].field, "status");
        }
        other => panic!("Expected Delete, got: {other:?}"),
    }
}

#[test]
fn test_delete_without_where_is_rejected() {
    // P7: a WHERE-less DELETE used to empty the whole lobe silently — now it
    // errors and teaches the explicit total-delete verb, PURGE.
    let e = parse(r#"DELETE "creditos""#).unwrap_err();
    let msg = format!("{e:?}");
    assert!(msg.contains("WHERE"), "error must mention WHERE: {msg}");
    assert!(msg.contains("PURGE"), "error must teach PURGE: {msg}");
    // With a WHERE it still parses.
    assert!(parse(r#"DELETE "creditos" WHERE status = "cancelled""#).is_ok());
}

#[test]
fn test_purge_parses() {
    // P7: PURGE "lobe" is the explicit total-delete verb.
    match parse(r#"PURGE "creditos""#).unwrap() {
        Statement::Purge(p) => assert_eq!(p.lobe, "creditos"),
        other => panic!("Expected Purge, got: {other:?}"),
    }
}

#[test]
fn test_link_standalone_with_where_on_both_sides() {
    // WHERE permitted on both source and target — this is the form the
    // public xytalk-spec.md showed but the parser previously refused.
    let stmt =
        parse(r#"LINK "clientes" WHERE rfc = "X" TO "creditos" WHERE credit_id = "C1" AS "owner""#)
            .unwrap();
    match stmt {
        Statement::Link(l) => {
            assert_eq!(l.relation_name, "owner");
            let src = l
                .source_filter_expr
                .as_ref()
                .unwrap()
                .as_flat_and()
                .unwrap();
            assert_eq!(src.len(), 1);
            assert_eq!(src[0].field, "rfc");
            let tgt = l
                .target_filter_expr
                .as_ref()
                .unwrap()
                .as_flat_and()
                .unwrap();
            assert_eq!(tgt.len(), 1);
            assert_eq!(tgt[0].field, "credit_id");
        }
        other => panic!("Expected Link, got: {other:?}"),
    }
}

// ─── INCACHE / OUTCACHE (v0.2.5.1 nom rewrite) ────────────────────────────────

#[test]
fn test_incache_quoted_lobe() {
    let stmt = parse(r#"INCACHE "creditos""#).unwrap();
    match stmt {
        Statement::InCache(s) => {
            assert_eq!(s.lobe, "creditos");
            assert!(s.filter_expr.is_none());
        }
        other => panic!("Expected InCache, got: {other:?}"),
    }
}

#[test]
fn test_incache_unquoted_lobe() {
    // Bare-identifier form must work just like PUT/SCAN/FIND accept
    // unquoted lobes — consistency across the language surface.
    let stmt = parse("INCACHE creditos").unwrap();
    match stmt {
        Statement::InCache(s) => {
            assert_eq!(s.lobe, "creditos");
            assert!(s.filter_expr.is_none());
        }
        other => panic!("Expected InCache, got: {other:?}"),
    }
}

#[test]
fn test_incache_with_where_v4_expr() {
    // INCACHE uses the V4 boolean expression grammar (AND/OR/NOT),
    // matching SCAN — verified here with an OR expression which the
    // V3 AND-only grammar would reject.
    let stmt =
        parse(r#"INCACHE "creditos" WHERE status = "active" OR status = "overdue""#).unwrap();
    match stmt {
        Statement::InCache(s) => {
            assert_eq!(s.lobe, "creditos");
            assert!(s.filter_expr.is_some());
        }
        other => panic!("Expected InCache, got: {other:?}"),
    }
}

#[test]
fn test_outcache_quoted_lobe() {
    let stmt = parse(r#"OUTCACHE "creditos""#).unwrap();
    assert_eq!(stmt, Statement::OutCache("creditos".to_string()));
}

#[test]
fn test_outcache_unquoted_lobe() {
    let stmt = parse("OUTCACHE creditos").unwrap();
    assert_eq!(stmt, Statement::OutCache("creditos".to_string()));
}

/// FAIL-pre / PASS-post: the pre-v0.2.5.1 hand-rolled parser accepted
/// the bare `OUTCACHE` keyword and produced `OutCache("")` silently —
/// the engine then errored later with `LobeNotFound("")`. The nom
/// rewrite rejects at parse time with a clear message.
#[test]
fn test_outcache_bare_keyword_rejected() {
    let r = parse("OUTCACHE");
    assert!(
        r.is_err(),
        "Bare `OUTCACHE` keyword must be rejected; got: {r:?}"
    );
}

/// Same for INCACHE — bare keyword must be rejected by the nom path.
#[test]
fn test_incache_bare_keyword_rejected() {
    let r = parse("INCACHE");
    assert!(
        r.is_err(),
        "Bare `INCACHE` keyword must be rejected; got: {r:?}"
    );
}

// ─── v0.2.5.2: cursor + limit on FIND ────────────────────────────────────────

#[test]
fn parser_find_with_cursor_and_limit() {
    // Grammar accepts the v0.2.5.2 extension:
    //   FIND "lobe" WHERE field = X LIMIT n CURSOR "<token>"
    // FindStmt gains `limit: Option<u64>` and `cursor: Option<String>`.
    let q = r#"FIND "creditos" WHERE rfc = "ACME-001" LIMIT 100 CURSOR "AQEAAQ_DUMMY""#;
    let stmt = parse(q).unwrap();
    let find = match stmt {
        Statement::Find(f) => f,
        other => panic!("expected Find, got {other:?}"),
    };
    assert_eq!(find.limit, Some(100), "LIMIT should parse to Some(100)");
    assert_eq!(
        find.cursor.as_deref(),
        Some("AQEAAQ_DUMMY"),
        "CURSOR token should parse verbatim"
    );
    // Sanity: WHERE filter still parsed.
    assert_eq!(find.filters.len(), 1);
    assert_eq!(find.filters[0].field, "rfc");
}

#[test]
fn parser_find_without_cursor_keeps_existing_shape() {
    // Backward compat: a plain FIND parses with cursor = None and
    // limit = None. v0.2.5.1 callers see no shape change.
    let q = r#"FIND "clientes" WHERE rfc = "X""#;
    let stmt = parse(q).unwrap();
    let find = match stmt {
        Statement::Find(f) => f,
        other => panic!("expected Find, got {other:?}"),
    };
    assert_eq!(find.limit, None);
    assert_eq!(find.cursor, None);
}

// ─── VECTOR (searchable embedding field) ──────────────────────────────────

#[test]
fn parse_vector_field() {
    assert_eq!(
        parse(r#"VECTOR embedding IN "mem""#).unwrap(),
        Statement::Vector(VectorStmt {
            field: "embedding".into(),
            lobe: "mem".into(),
        })
    );
}

#[test]
fn parse_vector_requires_in_and_lobe() {
    assert!(parse("VECTOR embedding").is_err());
    assert!(parse(r#"VECTOR IN "mem""#).is_err());
}

// ─── SATELLITE BY (sub-gravity axis) ──────────────────────────────────────

#[test]
fn parse_satellite_field() {
    assert_eq!(
        parse(r#"SATELLITE BY kind IN "events""#).unwrap(),
        Statement::Satellite(SatelliteStmt {
            field: "kind".into(),
            lobe: "events".into(),
        })
    );
}

#[test]
fn parse_satellite_requires_by_in_and_lobe() {
    assert!(parse("SATELLITE kind").is_err());
    assert!(parse(r#"SATELLITE BY IN "events""#).is_err());
    assert!(parse(r#"SATELLITE kind IN "events""#).is_err());
    assert!(parse("SATELLITE BY kind").is_err());
}

// ─── GRAVITY BY (v0.8 keel) ───────────────────────────────────────────────

#[test]
fn parse_gravity_raw_field() {
    assert_eq!(
        parse(r#"GRAVITY BY rfc IN "creditos""#).unwrap(),
        Statement::Gravity(GravityStmt {
            lobe: "creditos".into(),
            spec: GravitySpecAst::Raw("rfc".into()),
        })
    );
}

#[test]
fn parse_gravity_normalized_lower_and_trim() {
    assert_eq!(
        parse(r#"GRAVITY BY lower(empresa) IN "x""#).unwrap(),
        Statement::Gravity(GravityStmt {
            lobe: "x".into(),
            spec: GravitySpecAst::Normalized("empresa".into(), GravityTransform::Lower),
        })
    );
    assert_eq!(
        parse(r#"GRAVITY BY trim(code) IN "x""#).unwrap(),
        Statement::Gravity(GravityStmt {
            lobe: "x".into(),
            spec: GravitySpecAst::Normalized("code".into(), GravityTransform::Trim),
        })
    );
}

#[test]
fn parse_gravity_composite_tuple() {
    assert_eq!(
        parse(r#"GRAVITY BY (tenant, doc) IN "x""#).unwrap(),
        Statement::Gravity(GravityStmt {
            lobe: "x".into(),
            spec: GravitySpecAst::Composite(vec!["tenant".into(), "doc".into()]),
        })
    );
}

#[test]
fn parse_gravity_field_named_like_transform_is_raw() {
    // A field literally named `lower` (no parens) backtracks to Raw, not a transform.
    assert_eq!(
        parse(r#"GRAVITY BY lower IN "x""#).unwrap(),
        Statement::Gravity(GravityStmt {
            lobe: "x".into(),
            spec: GravitySpecAst::Raw("lower".into()),
        })
    );
}

#[test]
fn test_scientific_notation_floats() {
    // Embedding components serialize as scientific notation; the parser must
    // accept `e`/`E` exponents (with optional sign), not just plain decimals.
    let stmt = parse(r#"PUT {a: 5.7e-05, b: 1E9, c: -2.3e+4} IN "m""#).unwrap();
    if let Statement::Put(p) = stmt {
        assert_eq!(p.fields[0].value, Literal::Float(5.7e-05));
        assert_eq!(p.fields[1].value, Literal::Float(1e9));
        assert_eq!(p.fields[2].value, Literal::Float(-2.3e4));
    } else {
        panic!("expected Put");
    }
}

#[test]
fn test_vector_literal_with_scientific_notation() {
    // The exact shape that failed in the LongMemEval ingest: a float list with a
    // tiny scientific-notation component inline.
    let stmt = parse(r#"PUT {vec: [0.1, 5.698e-05, -3.0e-2]} IN "m""#).unwrap();
    if let Statement::Put(p) = stmt {
        assert_eq!(
            p.fields[0].value,
            Literal::List(vec![
                Literal::Float(0.1),
                Literal::Float(5.698e-05),
                Literal::Float(-3.0e-2),
            ])
        );
    } else {
        panic!("expected Put");
    }
}

#[test]
fn test_plain_int_and_float_still_parse() {
    // Regression: the exponent suffix is optional — plain ints stay Int, plain
    // decimals stay Float.
    let stmt = parse(r#"PUT {i: 42, f: 0.18} IN "m""#).unwrap();
    if let Statement::Put(p) = stmt {
        assert_eq!(p.fields[0].value, Literal::Int(42));
        assert_eq!(p.fields[1].value, Literal::Float(0.18));
    } else {
        panic!("expected Put");
    }
}

#[test]
fn test_s1_param_parses_in_where_and_put() {
    // WHERE value as $param
    let s = parse(r#"FIND "m" WHERE name = $n"#).unwrap();
    if let Statement::Find(f) = s {
        assert_eq!(f.filters[0].value, Literal::Param("n".into()));
    } else {
        panic!("expected Find");
    }
    // PUT field value as $param + positional $1
    let s = parse(r#"PUT {a: $v, b: $1} IN "m""#).unwrap();
    if let Statement::Put(p) = s {
        assert_eq!(p.fields[0].value, Literal::Param("v".into()));
        assert_eq!(p.fields[1].value, Literal::Param("1".into()));
    } else {
        panic!("expected Put");
    }
}

#[test]
fn test_parse_in_filter() {
    // `field IN (v1, v2, …)` parses to a single Filter with FilterOp::In and
    // the candidate set as a Literal::List (parenthesised, distinct from a
    // `[...]` list literal).
    let stmt = parse(r#"FIND "m" WHERE status IN ("active", "pending", "overdue")"#).unwrap();
    match stmt {
        Statement::Find(f) => {
            assert_eq!(f.filters.len(), 1);
            assert_eq!(f.filters[0].field, "status");
            assert_eq!(f.filters[0].op, FilterOp::In);
            assert_eq!(
                f.filters[0].value,
                Literal::List(vec![
                    Literal::Text("active".into()),
                    Literal::Text("pending".into()),
                    Literal::Text("overdue".into()),
                ])
            );
        }
        other => panic!("expected Find, got {other:?}"),
    }
}

#[test]
fn test_parse_in_filter_single_and_ints() {
    // Single-element and integer candidate sets both parse.
    let stmt = parse(r#"FIND "m" WHERE n IN (1, 2, 3)"#).unwrap();
    match stmt {
        Statement::Find(f) => {
            assert_eq!(f.filters[0].op, FilterOp::In);
            assert_eq!(
                f.filters[0].value,
                Literal::List(vec![Literal::Int(1), Literal::Int(2), Literal::Int(3)])
            );
        }
        other => panic!("expected Find, got {other:?}"),
    }
}

// ─── xyTalk v1 (0.9.5) — wave 1: coherence ───────────────────────────────────

/// Extract the first AGGREGATE step's funcs from a pipeline query.
#[cfg(test)]
fn first_aggregate(q: &str) -> Vec<Aggregate> {
    match parse(q).unwrap_or_else(|e| panic!("parse {q}: {e:?}")) {
        Statement::Pipeline(steps) => steps
            .into_iter()
            .find_map(|s| match s {
                PipelineStep::Aggregate(a) => Some(a),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no AGGREGATE step in {q}")),
        other => panic!("expected Pipeline for {q}, got {other:?}"),
    }
}

#[test]
fn test_count_star_alias_of_count() {
    // P4: count(*) / count( * ) are aliases of count() — all parse to Count.
    for q in [
        r#"SCAN "x" | AGGREGATE count()"#,
        r#"SCAN "x" | AGGREGATE count(*)"#,
        r#"SCAN "x" | AGGREGATE count( * )"#,
    ] {
        assert_eq!(first_aggregate(q)[0].func, AggregateFunc::Count, "{q}");
    }
}

#[test]
fn test_in_list_bracket_alias_of_paren() {
    // P3: IN [a,b] (canonical) == IN (a,b) (paren alias) — identical Filter.
    fn in_filter(q: &str) -> Filter {
        match parse(q).unwrap_or_else(|e| panic!("parse {q}: {e:?}")) {
            Statement::Find(f) => f.filters[0].clone(),
            other => panic!("expected Find for {q}, got {other:?}"),
        }
    }
    let brk = in_filter(r#"FIND "m" WHERE status IN ["active", "overdue"]"#);
    let par = in_filter(r#"FIND "m" WHERE status IN ("active", "overdue")"#);
    assert_eq!(brk.op, FilterOp::In);
    assert_eq!(brk, par, "IN [...] must equal IN (...)");
    assert_eq!(
        brk.value,
        Literal::List(vec![
            Literal::Text("active".into()),
            Literal::Text("overdue".into())
        ])
    );
}

#[test]
fn test_order_by_requires_limit() {
    // P5: ORDER BY without LIMIT is a parse error (unbounded-sort footgun);
    // with LIMIT it parses; plain SCAN without ORDER BY needs no LIMIT.
    assert!(
        parse(r#"SCAN "x" ORDER BY fecha DESC"#).is_err(),
        "ORDER BY without LIMIT must error"
    );
    assert!(parse(r#"SCAN "x" ORDER BY fecha DESC LIMIT 10"#).is_ok());
    assert!(parse(r#"SCAN "x" WHERE a = 1"#).is_ok());
}

#[test]
fn test_find_rejects_or_teaches_scan() {
    // P1: FIND is AND-only; OR/NOT must error (was silently dropped), pointing
    // to SCAN. AND still works; plain equality still works.
    let e = parse(r#"FIND "m" WHERE a = 1 OR b = 2"#).unwrap_err();
    assert!(
        format!("{e:?}").contains("SCAN"),
        "OR error should teach SCAN: {e:?}"
    );
    assert!(parse(r#"FIND "m" WHERE a = 1 AND b = 2"#).is_ok());
    assert!(parse(r#"FIND "m" WHERE a = 1"#).is_ok());
    assert!(parse(r#"FIND "m" WHERE a = 1 NOT b = 2"#).is_err());
}

/// Helper: pull the single `TOP` step (the `TAKE`/`TOP` node) out of a
/// `SCAN | GROUP BY | AGGREGATE | TAKE …` pipeline.
fn take_step(q: &str) -> TopStmt {
    match parse(q).unwrap() {
        Statement::Pipeline(steps) => match steps.into_iter().last() {
            Some(PipelineStep::Top(t)) => t,
            other => panic!("expected a TAKE/TOP step, got {other:?}"),
        },
        other => panic!("expected a pipeline, got {other:?}"),
    }
}

#[test]
fn test_take_is_alias_of_top() {
    // P2: `TAKE` is canonical, `TOP` is a live alias — same AST node, so every
    // downstream path (and the benchmark harness's `TOP … BY sum(monto)`) is
    // untouched. Default direction is DESC; ASC carries through the alias.
    let base = r#"SCAN "l" | GROUP BY grp | AGGREGATE sum(monto)"#;
    let take = take_step(&format!("{base} | TAKE 5 BY sum(monto)"));
    let top = take_step(&format!("{base} | TOP 5 BY sum(monto)"));
    assert_eq!(take, top, "TAKE must parse to the identical node as TOP");
    assert_eq!(take.n, 5);
    assert!(take.descending, "default direction is DESC");
    assert!(take.by.is_some(), "BY present → Some");

    let asc = take_step(&format!("{base} | TAKE 5 BY sum(monto) ASC"));
    assert!(!asc.descending, "ASC must carry through the alias");
}

#[test]
fn test_take_without_by_is_truncate() {
    // `TAKE n` with no BY = truncate (pipeline LIMIT): the node has no metric.
    let t = take_step(r#"SCAN "l" | GROUP BY grp | AGGREGATE sum(monto) | TAKE 3"#);
    assert_eq!(t.n, 3);
    assert!(t.by.is_none(), "no BY → None (truncate)");

    // And it parses on a plain scan pipeline too.
    let t2 = take_step(r#"SCAN "l" | TAKE 7"#);
    assert_eq!(t2.n, 7);
    assert!(t2.by.is_none());
}

/// Helper: pull the single `NEAREST` step out of a `SCAN | NEAREST …` pipeline.
fn nearest_step(q: &str) -> NearestStmt {
    match parse(q).unwrap() {
        Statement::Pipeline(steps) => match steps.into_iter().last() {
            Some(PipelineStep::Nearest(n)) => n,
            other => panic!("expected a NEAREST step, got {other:?}"),
        },
        other => panic!("expected a pipeline, got {other:?}"),
    }
}

#[test]
fn test_nearest_phrase_equals_function_alias() {
    // P6: the phrase form is canonical, the function form is a live alias — same
    // AST node, so the engine can't tell them apart.
    let phrase = nearest_step(r#"SCAN "m" WHERE c="c1" | NEAREST 3 BY emb TO $q USING cosine"#);
    let func = nearest_step(r#"SCAN "m" WHERE c="c1" | NEAREST(emb, $q, 3, cosine)"#);
    assert_eq!(
        phrase, func,
        "phrase and function forms must parse to the same node"
    );
    assert_eq!(phrase.field, "emb");
    assert_eq!(phrase.k, 3);
    assert_eq!(phrase.metric, "cosine");
    assert_eq!(phrase.query, NearestQuery::Param("q".into()));
}

#[test]
fn test_nearest_using_defaults_to_cosine() {
    // `USING` omitted → cosine (the common case carries no parameter).
    let n = nearest_step(r#"SCAN "m" WHERE c="c1" | NEAREST 5 BY emb TO [1.0, 0.0]"#);
    assert_eq!(n.metric, "cosine");
    assert_eq!(n.k, 5);
}

#[test]
fn test_orbit_is_removed() {
    // P6: ORBIT is gone — it was an unused synonym. It must not parse.
    assert!(
        parse(r#"SCAN "m" WHERE c="c1" | ORBIT(emb, $q, 3, cosine)"#).is_err(),
        "ORBIT must no longer parse"
    );
}

/// Helper: parse a CREATE GHOST and return its statement.
fn create_ghost(q: &str) -> CreateGhostStmt {
    match parse(q).unwrap() {
        Statement::CreateGhost(g) => g,
        other => panic!("expected CreateGhost, got {other:?}"),
    }
}

#[test]
fn test_create_ghost_pipeline_equals_clause_form() {
    // P14: a ghost is a saved query. The canonical pipeline form and the classic
    // clause form must produce the identical statement — metric-order case.
    let pipeline = create_ghost(
        r#"CREATE GHOST "g" FROM "creditos" WHERE status = "active" | GROUP BY rfc | AGGREGATE sum(monto) | TAKE BY sum(monto) DESC"#,
    );
    let clause = create_ghost(
        r#"CREATE GHOST "g" FROM "creditos" WHERE status = "active" ORDER BY sum(monto) DESC GROUP BY rfc AGGREGATE sum(monto)"#,
    );
    assert_eq!(
        pipeline, clause,
        "pipeline form must equal the clause alias"
    );
    assert!(
        pipeline.order_metric.is_some(),
        "TAKE BY metric → order_metric"
    );
    assert_eq!(pipeline.group_by, vec!["rfc".to_string()]);
}

#[test]
fn test_create_ghost_pipeline_covering_and_embed() {
    // Covering (field order, no grouping) + a projection via `| EMBED`.
    let pipeline =
        create_ghost(r#"CREATE GHOST "g" FROM "l" | TAKE BY fecha | EMBED fecha, monto"#);
    let clause = create_ghost(r#"CREATE GHOST "g" FROM "l" ORDER BY fecha EMBED fecha, monto"#);
    assert_eq!(pipeline, clause);
    assert_eq!(pipeline.order_by, "fecha");
    assert!(pipeline.order_metric.is_none(), "field order → no metric");
    assert_eq!(
        pipeline.embed,
        vec!["fecha".to_string(), "monto".to_string()]
    );
}

#[test]
fn test_create_ghost_pipeline_requires_take_by() {
    // `| TAKE BY` is the order declaration and is required in the pipeline form
    // (the peer of the mandatory ORDER BY). A GROUP BY without it must not parse.
    assert!(
        parse(r#"CREATE GHOST "g" FROM "l" | GROUP BY rfc | AGGREGATE sum(monto)"#).is_err(),
        "pipeline form without | TAKE BY must error"
    );
}

#[test]
fn test_shape_step_parses_field_list() {
    // P9: `| SHAPE {a, b}` is a projection pipeline step; braces hold bare
    // field names (mirroring PUT {…}).
    match parse(r#"SCAN "l" | SHAPE {name, score}"#).unwrap() {
        Statement::Pipeline(steps) => match steps.into_iter().last() {
            Some(PipelineStep::Shape(s)) => {
                assert_eq!(s.fields, vec!["name".to_string(), "score".to_string()]);
            }
            other => panic!("expected a SHAPE step, got {other:?}"),
        },
        other => panic!("expected a pipeline, got {other:?}"),
    }
}

#[test]
fn test_fetch_parses_lobes_where_and_as() {
    // P8: FETCH lists lobes, a shared WHERE, and optional AS section names.
    match parse(r#"FETCH "clientes", "creditos" WHERE rfc = "X" AS {cliente, creditos}"#).unwrap() {
        Statement::Fetch(f) => {
            assert_eq!(
                f.lobes,
                vec!["clientes".to_string(), "creditos".to_string()]
            );
            assert!(f.filter_expr.is_some());
            assert_eq!(
                f.names,
                Some(vec!["cliente".to_string(), "creditos".to_string()])
            );
        }
        other => panic!("expected Fetch, got {other:?}"),
    }
    // AS is optional.
    match parse(r#"FETCH "a", "b" WHERE k = "x""#).unwrap() {
        Statement::Fetch(f) => assert!(f.names.is_none()),
        other => panic!("expected Fetch, got {other:?}"),
    }
}

#[test]
fn test_fetch_requires_where_and_matching_as() {
    // WHERE required; AS must have one name per lobe.
    assert!(
        parse(r#"FETCH "a", "b""#).is_err(),
        "WHERE-less FETCH must error"
    );
    assert!(
        parse(r#"FETCH "a", "b" WHERE k = "x" AS {only}"#).is_err(),
        "AS count mismatch must error"
    );
}
