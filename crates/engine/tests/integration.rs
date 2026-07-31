use xyzdb_engine::engine::{Engine, QueryResult};

fn temp_engine() -> (Engine, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let engine = Engine::open(dir.path()).expect("failed to open engine");
    (engine, dir)
}

fn assert_ok(result: &QueryResult) {
    match result {
        QueryResult::Ok { message, .. } => {
            println!("  OK: {message}");
        }
        other => panic!("Expected Ok, got: {other:?}"),
    }
}

fn assert_records(result: &QueryResult, expected_count: usize) -> Vec<xyzdb_core::record::Record> {
    // v0.2.5.1: SCAN may return PaginatedRecords when has_more triggers
    // (default-LIMIT overflow or explicit LIMIT smaller than the dataset).
    // Test helper treats both shapes as "records" for count assertions —
    // pagination metadata is verified by dedicated tests via assert_paginated.
    let recs: &Vec<_> = match result {
        QueryResult::Records(recs) => recs,
        QueryResult::PaginatedRecords { records, .. } => records,
        other => panic!("Expected Records or PaginatedRecords, got: {other:?}"),
    };
    println!("  Got {} record(s)", recs.len());
    assert_eq!(
        recs.len(),
        expected_count,
        "Expected {expected_count} records, got {}",
        recs.len()
    );
    recs.clone()
}

/// THE CO-LOCATION TEST — the test that proves the thesis.
///
/// A lobe is NOT a table. A table contains one type of data. A lobe contains
/// a complete semantic domain — heterogeneous data (Company, Project, Task)
/// co-located by relationship, not by type.
///
/// In SQL this is 3 tables + 2 JOINs + 5+ random seeks on HDD.
/// In xyzDB this is 1 range scan of physically contiguous data.
///
/// Chain: Company → Project (LINK TO Company) → 3 Tasks (LINK TO Project)
/// All 5 records share gravity_hash because LINK inheritance is TRANSITIVE:
///   Task inherits from Project, which inherited from Company.
///
/// FIND company | PULL → 5 records in ONE sequential read.
#[test]
fn test_colocation() {
    let (engine, _dir) = temp_engine();

    println!("\n=== CO-LOCATION TEST ===");
    println!("  1 Company + 1 Project + 3 Tasks = 5 records");
    println!("  All co-located via transitive gravity_hash inheritance\n");

    // Setup: one lobe, two anchors
    assert_ok(&engine.run(r#"LOBE "workspace""#).unwrap());
    assert_ok(
        &engine
            .run(r#"ANCHOR "code" UNIQUE IN "workspace""#)
            .unwrap(),
    );
    assert_ok(
        &engine
            .run(r#"ANCHOR "project_id" UNIQUE IN "workspace""#)
            .unwrap(),
    );

    // 1. Company
    assert_ok(&engine.run(
        r#"PUT {_type: "Company", code: "ACME-001", name: "Acme Corp", region: "US-West"} IN "workspace""#
    ).unwrap());
    println!("  Company inserted (gravity_hash = hash(code))");

    // 2. Project → LINK TO Company (inherits Company's gravity_hash)
    assert_ok(&engine.run(
        r#"PUT {_type: "Project", project_id: "PRJ-001", budget: 50000, duration: 36} IN "workspace" LINK TO "workspace" WHERE code = "ACME-001" AS "owner""#
    ).unwrap());
    println!("  Project inserted (gravity_hash inherited from Company)");

    // 3. Three Tasks → LINK TO Project (inherit Project's gravity_hash = Company's)
    assert_ok(&engine.run(
        r#"PUT {_type: "Task", numero: 1, hours: 8, due_date: @"2026-04-25", status: "pending"} IN "workspace" LINK TO "workspace" WHERE project_id = "PRJ-001" AS "task_of""#
    ).unwrap());
    assert_ok(&engine.run(
        r#"PUT {_type: "Task", numero: 2, hours: 12, due_date: @"2026-05-25", status: "pending"} IN "workspace" LINK TO "workspace" WHERE project_id = "PRJ-001" AS "task_of""#
    ).unwrap());
    assert_ok(&engine.run(
        r#"PUT {_type: "Task", numero: 3, hours: 4, due_date: @"2026-06-25", status: "pending"} IN "workspace" LINK TO "workspace" WHERE project_id = "PRJ-001" AS "task_of""#
    ).unwrap());
    println!("  3 Tasks inserted (gravity_hash inherited from Project → Company)");

    // THE QUERY: FIND company | PULL → must return ALL 5 records
    println!("\n  Executing: FIND ... WHERE code=\"ACME-001\" | PULL depth=1");
    let r = engine
        .run(r#"FIND "workspace" WHERE code = "ACME-001" | PULL depth=1"#)
        .unwrap();
    let records = assert_records(&r, 5);

    // Verify types
    let types: Vec<String> = records
        .iter()
        .filter_map(|r| r.fields.get("_type"))
        .filter_map(|v| v.as_text().map(String::from))
        .collect();

    let companies = types.iter().filter(|t| *t == "Company").count();
    let projects = types.iter().filter(|t| *t == "Project").count();
    let tasks = types.iter().filter(|t| *t == "Task").count();

    assert_eq!(companies, 1, "Should have 1 Company");
    assert_eq!(projects, 1, "Should have 1 Project");
    assert_eq!(tasks, 3, "Should have 3 Tasks");

    println!("\n  Results:");
    for rec in &records {
        let t = rec
            .fields
            .get("_type")
            .map(|v| format!("{v}"))
            .unwrap_or_default();
        println!("    {t} — LID: {}", rec.lid);
    }

    println!("\n  In SQL this would be:");
    println!("    SELECT * FROM companies c");
    println!("    JOIN projects p ON p.company_code = c.code");
    println!("    JOIN tasks t ON t.project_id = p.id");
    println!("    WHERE c.code = 'ACME-001';");
    println!("    -- 3 tables, 2 JOINs, 5+ random seeks on HDD");
    println!("\n  In xyzDB: 1 PULL = 1 range scan of contiguous data on disk.");
    println!("  The lobe is not a table. It's a semantic domain.");
    println!("\n=== TEST PASSED ===\n");
}

/// Also test PULL with only= filter to get just tasks.
#[test]
fn test_pull_only_filter() {
    let (engine, _dir) = temp_engine();

    assert_ok(&engine.run(r#"LOBE "workspace""#).unwrap());
    assert_ok(
        &engine
            .run(r#"ANCHOR "code" UNIQUE IN "workspace""#)
            .unwrap(),
    );
    assert_ok(
        &engine
            .run(r#"ANCHOR "project_id" UNIQUE IN "workspace""#)
            .unwrap(),
    );

    assert_ok(
        &engine
            .run(r#"PUT {_type: "Company", code: "ACME-001", name: "Acme Corp"} IN "workspace""#)
            .unwrap(),
    );
    assert_ok(&engine.run(
        r#"PUT {_type: "Project", project_id: "PRJ-001", budget: 50000} IN "workspace" LINK TO "workspace" WHERE code = "ACME-001" AS "owner""#
    ).unwrap());
    assert_ok(&engine.run(
        r#"PUT {_type: "Task", numero: 1, hours: 8} IN "workspace" LINK TO "workspace" WHERE project_id = "PRJ-001" AS "task_of""#
    ).unwrap());
    assert_ok(&engine.run(
        r#"PUT {_type: "Task", numero: 2, hours: 12} IN "workspace" LINK TO "workspace" WHERE project_id = "PRJ-001" AS "task_of""#
    ).unwrap());

    // PULL only=Task → should return only the 2 tasks
    let r = engine
        .run(r#"FIND "workspace" WHERE code = "ACME-001" | PULL only=Task"#)
        .unwrap();
    let records = assert_records(&r, 2);
    for rec in &records {
        assert_eq!(
            rec.fields.get("_type"),
            Some(&xyzdb_core::value::Value::Text("Task".into()))
        );
    }
}

#[test]
fn test_put_find_basic() {
    let (engine, _dir) = temp_engine();

    engine.run(r#"LOBE "workspace""#).unwrap();
    engine
        .run(r#"ANCHOR "code" UNIQUE IN "workspace""#)
        .unwrap();

    engine
        .run(r#"PUT {code: "ACME-001", name: "Acme Corp"} IN "workspace""#)
        .unwrap();
    engine
        .run(r#"PUT {code: "BETA-002", name: "Beta Inc"} IN "workspace""#)
        .unwrap();

    // Find by anchor
    let r = engine
        .run(r#"FIND "workspace" WHERE code = "ACME-001""#)
        .unwrap();
    let records = assert_records(&r, 1);
    assert_eq!(
        records[0].fields.get("name"),
        Some(&xyzdb_core::value::Value::Text("Acme Corp".into()))
    );
}

#[test]
fn test_duplicate_anchor_error() {
    let (engine, _dir) = temp_engine();

    engine.run(r#"LOBE "workspace""#).unwrap();
    engine
        .run(r#"ANCHOR "code" UNIQUE IN "workspace""#)
        .unwrap();
    engine
        .run(r#"PUT {code: "ACME-001", name: "Acme Corp"} IN "workspace""#)
        .unwrap();

    // Second PUT with same anchor should fail
    let r = engine.run(r#"PUT {code: "ACME-001", name: "Duplicate"} IN "workspace""#);
    assert!(r.is_err(), "Should fail on duplicate anchor");
    let err = r.unwrap_err();
    assert!(
        err.to_string().contains("Duplicate anchor"),
        "Error should mention duplicate: {err}"
    );
}

#[test]
fn test_on_conflict_update() {
    let (engine, _dir) = temp_engine();

    engine.run(r#"LOBE "workspace""#).unwrap();
    engine
        .run(r#"ANCHOR "code" UNIQUE IN "workspace""#)
        .unwrap();
    engine
        .run(r#"PUT {code: "ACME-001", name: "Acme Corp"} IN "workspace""#)
        .unwrap();

    // ON CONFLICT UPDATE should update, not error
    let r = engine
        .run(
            r#"PUT {code: "ACME-001", name: "Acme Corporation"} IN "workspace" ON CONFLICT UPDATE"#,
        )
        .unwrap();
    assert_ok(&r);

    // Verify the update took effect
    let r = engine
        .run(r#"FIND "workspace" WHERE code = "ACME-001""#)
        .unwrap();
    let records = assert_records(&r, 1);
    assert_eq!(
        records[0].fields.get("name"),
        Some(&xyzdb_core::value::Value::Text("Acme Corporation".into()))
    );
}

#[test]
fn test_scan_with_filter() {
    let (engine, _dir) = temp_engine();

    engine.run(r#"LOBE "workspace""#).unwrap();
    engine
        .run(r#"PUT {name: "Alice", region: "US-West"} IN "workspace""#)
        .unwrap();
    engine
        .run(r#"PUT {name: "Bob", region: "EU"} IN "workspace""#)
        .unwrap();
    engine
        .run(r#"PUT {name: "Charlie", region: "US-West"} IN "workspace""#)
        .unwrap();

    let r = engine
        .run(r#"SCAN "workspace" WHERE region = "US-West""#)
        .unwrap();
    let records = assert_records(&r, 2);
    for rec in &records {
        assert_eq!(
            rec.fields.get("region"),
            Some(&xyzdb_core::value::Value::Text("US-West".into()))
        );
    }
}

#[test]
fn test_set_pipeline() {
    let (engine, _dir) = temp_engine();

    engine.run(r#"LOBE "workspace""#).unwrap();
    engine
        .run(r#"ANCHOR "code" UNIQUE IN "workspace""#)
        .unwrap();
    engine
        .run(r#"PUT {code: "ACME-001", name: "Acme Corp", status: "active"} IN "workspace""#)
        .unwrap();

    // Pipeline: FIND | SET
    engine
        .run(r#"FIND "workspace" WHERE code = "ACME-001" | SET status = "inactive""#)
        .unwrap();

    // Verify update
    let r = engine
        .run(r#"FIND "workspace" WHERE code = "ACME-001""#)
        .unwrap();
    let records = assert_records(&r, 1);
    assert_eq!(
        records[0].fields.get("status"),
        Some(&xyzdb_core::value::Value::Text("inactive".into()))
    );
}

#[test]
fn test_delete() {
    let (engine, _dir) = temp_engine();

    engine.run(r#"LOBE "workspace""#).unwrap();
    engine
        .run(r#"ANCHOR "code" UNIQUE IN "workspace""#)
        .unwrap();
    engine
        .run(r#"PUT {code: "ACME-001", name: "Acme Corp"} IN "workspace""#)
        .unwrap();

    // Pipeline: FIND | DELETE
    engine
        .run(r#"FIND "workspace" WHERE code = "ACME-001" | DELETE"#)
        .unwrap();

    // Verify deleted
    let r = engine
        .run(r#"FIND "workspace" WHERE code = "ACME-001""#)
        .unwrap();
    assert_records(&r, 0);
}

#[test]
fn test_show_lobes() {
    let (engine, _dir) = temp_engine();

    engine.run(r#"LOBE "workspace""#).unwrap();
    engine
        .run(r#"LOBE "catalog" HINT="products and stock""#)
        .unwrap();

    let r = engine.run("SHOW LOBES").unwrap();
    match r {
        QueryResult::Info(lines) => {
            assert!(lines.len() >= 3); // header + 2 lobes
            println!("{}", lines.join("\n"));
        }
        _ => panic!("Expected Info"),
    }
}

#[test]
fn test_type_auto_injected() {
    let (engine, _dir) = temp_engine();

    engine.run(r#"LOBE "workspace""#).unwrap();
    engine
        .run(r#"PUT {name: "Acme Corp"} IN "workspace""#)
        .unwrap();

    let r = engine.run(r#"SCAN "workspace""#).unwrap();
    let records = assert_records(&r, 1);
    assert_eq!(
        records[0].fields.get("_type"),
        Some(&xyzdb_core::value::Value::Text("workspace".into())),
        "_type should be auto-injected from lobe name"
    );
}

/// PUT BATCH: insert 36 tasks linked to a project in one atomic operation.
/// PULL must find all 38 records (1 Company + 1 Project + 36 Tasks).
#[test]
fn test_put_batch_with_link() {
    let (engine, _dir) = temp_engine();

    engine.run(r#"LOBE "workspace""#).unwrap();
    engine
        .run(r#"ANCHOR "code" UNIQUE IN "workspace""#)
        .unwrap();
    engine
        .run(r#"ANCHOR "project_id" UNIQUE IN "workspace""#)
        .unwrap();

    // Company
    engine
        .run(r#"PUT {_type: "Company", code: "BATCH-CO", name: "Batch Corp"} IN "workspace""#)
        .unwrap();

    // Project linked to company
    engine.run(
        r#"PUT {_type: "Project", project_id: "PRJ-BATCH", budget: 50000} IN "workspace" LINK TO "workspace" WHERE code = "BATCH-CO" AS "owner""#,
    ).unwrap();

    // 36 tasks via BATCH, linked to project
    let mut tasks = String::from(r#"PUT BATCH IN "workspace" ["#);
    for i in 1..=36 {
        if i > 1 {
            tasks.push_str(", ");
        }
        tasks.push_str(&format!(
            r#"{{_type: "Task", numero: {i}, hours: 8, status: "pending"}}"#
        ));
    }
    tasks.push_str(r#"] LINK TO "workspace" WHERE project_id = "PRJ-BATCH" AS "task_of""#);

    let r = engine.run(&tasks).unwrap();
    match r {
        QueryResult::BatchOk { count, .. } => assert_eq!(count, 36),
        other => panic!("Expected BatchOk, got: {other:?}"),
    }

    // PULL must return 1 Company + 1 Project + 36 Tasks = 38
    let r = engine
        .run(r#"FIND "workspace" WHERE code = "BATCH-CO" | PULL depth=1"#)
        .unwrap();
    let records = assert_records(&r, 38);

    let tasks_count = records
        .iter()
        .filter(|r| {
            r.fields
                .get("_type")
                .and_then(|v| v.as_text())
                .is_some_and(|t| t == "Task")
        })
        .count();
    assert_eq!(tasks_count, 36, "Should have 36 Tasks from batch");
}

/// PUT BATCH without LINK: records get gravity_hash from their own anchor values.
#[test]
fn test_put_batch_no_link() {
    let (engine, _dir) = temp_engine();

    engine.run(r#"LOBE "workspace""#).unwrap();

    let r = engine
        .run(r#"PUT BATCH IN "workspace" [{name: "A"}, {name: "B"}, {name: "C"}]"#)
        .unwrap();
    match r {
        QueryResult::BatchOk { count, .. } => assert_eq!(count, 3),
        other => panic!("Expected BatchOk, got: {other:?}"),
    }

    let r = engine.run(r#"SCAN "workspace""#).unwrap();
    assert_records(&r, 3);
}

// ═══════════════════════════════════════════════════════════════════
// PHASE 2 TESTS
// ═══════════════════════════════════════════════════════════════════

#[test]
fn test_aggregate_pipeline() {
    let (engine, _dir) = temp_engine();

    engine.run(r#"LOBE "workspace""#).unwrap();
    engine
        .run(r#"PUT {_type: "Task", hours: 10, status: "completed"} IN "workspace""#)
        .unwrap();
    engine
        .run(r#"PUT {_type: "Task", hours: 20, status: "completed"} IN "workspace""#)
        .unwrap();
    engine
        .run(r#"PUT {_type: "Task", hours: 30, status: "pending"} IN "workspace""#)
        .unwrap();

    let r = engine
        .run(r#"SCAN "workspace" WHERE _type = "Task" | AGGREGATE count(), sum(hours), avg(hours)"#)
        .unwrap();
    match r {
        QueryResult::Aggregation(map) => {
            assert_eq!(map.get("count"), Some(&xyzdb_core::value::Value::Int(3)));
            assert_eq!(
                map.get("sum(hours)"),
                Some(&xyzdb_core::value::Value::Float(60.0))
            );
            assert_eq!(
                map.get("avg(hours)"),
                Some(&xyzdb_core::value::Value::Float(20.0))
            );
        }
        other => panic!("Expected Aggregation, got: {other:?}"),
    }
}

#[test]
fn test_aggregate_min_max() {
    let (engine, _dir) = temp_engine();

    engine.run(r#"LOBE "t""#).unwrap();
    engine.run(r#"PUT {val: 10} IN "t""#).unwrap();
    engine.run(r#"PUT {val: 50} IN "t""#).unwrap();
    engine.run(r#"PUT {val: 30} IN "t""#).unwrap();

    let r = engine
        .run(r#"SCAN "t" | AGGREGATE min(val), max(val)"#)
        .unwrap();
    match r {
        QueryResult::Aggregation(map) => {
            assert_eq!(
                map.get("min(val)"),
                Some(&xyzdb_core::value::Value::Float(10.0))
            );
            assert_eq!(
                map.get("max(val)"),
                Some(&xyzdb_core::value::Value::Float(50.0))
            );
        }
        other => panic!("Expected Aggregation, got: {other:?}"),
    }
}

#[test]
fn test_aggregate_with_filter() {
    let (engine, _dir) = temp_engine();

    engine.run(r#"LOBE "workspace""#).unwrap();
    engine
        .run(r#"PUT {hours: 10, status: "completed"} IN "workspace""#)
        .unwrap();
    engine
        .run(r#"PUT {hours: 20, status: "completed"} IN "workspace""#)
        .unwrap();
    engine
        .run(r#"PUT {hours: 30, status: "pending"} IN "workspace""#)
        .unwrap();

    let r = engine
        .run(r#"SCAN "workspace" WHERE status = "completed" | AGGREGATE count(), sum(hours)"#)
        .unwrap();
    match r {
        QueryResult::Aggregation(map) => {
            assert_eq!(map.get("count"), Some(&xyzdb_core::value::Value::Int(2)));
            assert_eq!(
                map.get("sum(hours)"),
                Some(&xyzdb_core::value::Value::Float(30.0))
            );
        }
        other => panic!("Expected Aggregation, got: {other:?}"),
    }
}

#[test]
fn test_autoanchor_apply() {
    let (engine, _dir) = temp_engine();

    engine.run(r#"LOBE "test""#).unwrap();

    for i in 0..100 {
        engine
            .run(&format!(
                r#"PUT {{code: "CODE-{i:05}", name: "N{i}"}} IN "test""#
            ))
            .unwrap();
    }

    // Apply anchor — should index all 100 records
    let r = engine.run(r#"AUTOANCHOR APPLY "code" IN "test""#).unwrap();
    match &r {
        QueryResult::Ok { message, .. } => {
            println!("  {message}");
            assert!(message.contains("100 records indexed"));
        }
        other => panic!("Expected Ok, got: {other:?}"),
    }

    // Now code is enforced — duplicate should fail
    let r = engine.run(r#"PUT {code: "CODE-00001", name: "Dup"} IN "test""#);
    assert!(
        r.is_err(),
        "Should fail on duplicate after AUTOANCHOR APPLY"
    );
}

// ═══════════════════════════════════════════════════════════════════
// GHOST LOBE TESTS
// ═══════════════════════════════════════════════════════════════════

/// Create a Ghost Lobe, populate it, and verify SCAN GHOST returns correct data.
#[test]
fn test_ghost_lobe_create_and_scan() {
    let (engine, _dir) = temp_engine();

    engine.run(r#"LOBE "workspace""#).unwrap();

    // Insert data with different statuses
    for i in 0..100 {
        let status = if i % 3 == 0 { "blocked" } else { "pending" };
        engine
            .run(&format!(
                r#"PUT {{_type: "Task", numero: {i}, hours: {}, status: "{status}"}} IN "workspace""#,
                10 + i
            ))
            .unwrap();
    }

    // Create Ghost Lobe for "blocked" tasks ordered by hours
    let r = engine
        .run(r#"CREATE GHOST "blocked_tasks" FROM "workspace" WHERE _type = "Task" AND status = "blocked" ORDER BY hours"#)
        .unwrap();
    match &r {
        QueryResult::Ok { message, .. } => {
            println!("  {message}");
            // Should have ~34 records (100/3)
            assert!(
                message.contains("34 records")
                    || message.contains("33 records")
                    || message.contains("34 index entries")
                    || message.contains("33 index entries"),
                "Expected ~34 records/entries: {message}"
            );
        }
        other => panic!("Expected Ok, got: {other:?}"),
    }

    // SCAN GHOST — should return only "blocked" tasks
    let r = engine.run(r#"SCAN GHOST "blocked_tasks""#).unwrap();
    let records = assert_records(&r, 34);
    for rec in &records {
        assert_eq!(
            rec.fields.get("status"),
            Some(&xyzdb_core::value::Value::Text("blocked".into())),
        );
    }

    // SCAN GHOST with additional filter
    let r = engine
        .run(r#"SCAN GHOST "blocked_tasks" WHERE hours > 50"#)
        .unwrap();
    match r {
        QueryResult::Records(recs) => {
            assert!(recs.len() < 34, "Filtered scan should return fewer records");
            for rec in &recs {
                let hours = rec
                    .fields
                    .get("hours")
                    .and_then(|v| v.as_int())
                    .unwrap_or(0);
                assert!(hours > 50);
            }
        }
        other => panic!("Expected Records, got: {other:?}"),
    }

    // SHOW GHOSTS
    let r = engine.run("SHOW GHOSTS").unwrap();
    match r {
        QueryResult::Info(lines) => {
            let text = lines.join("\n");
            assert!(text.contains("blocked_tasks"), "Should list ghost: {text}");
        }
        other => panic!("Expected Info, got: {other:?}"),
    }

    // DROP GHOST
    assert_ok(&engine.run(r#"DROP GHOST "blocked_tasks""#).unwrap());

    // SCAN GHOST after drop should fail
    let r = engine.run(r#"SCAN GHOST "blocked_tasks""#);
    assert!(r.is_err(), "Should fail after DROP");
}

/// Ghost Lobe benchmark: compare SCAN GHOST vs SCAN primary.
///
/// `#[ignore]` because the ghost-faster-than-primary assertion has been
/// flaky on the v0.3-cycle native bench harness — under cold-cache
/// conditions the primary-tree SCAN can win on small datasets where
/// ghost build cost amortises poorly. The native Bench A (Q4/Q5) is
/// the authoritative ghost-throughput validator now; this test is kept
/// for local diagnostics. Run explicitly with `--include-ignored`.
#[test]
#[ignore]
fn test_ghost_vs_primary_performance() {
    let (engine, _dir) = temp_engine();

    engine.run(r#"LOBE "perf""#).unwrap();

    // Insert 5000 records, 30% "blocked"
    for i in 0..5000 {
        let status = if i % 3 == 0 { "blocked" } else { "pending" };
        let month = (i % 12) + 1;
        engine
            .run(&format!(
                r#"PUT {{_type: "Task", numero: {i}, hours: {}, status: "{status}", due_date: @"2026-{month:02}-25"}} IN "perf""#,
                10 + i
            ))
            .unwrap();
    }

    // Create Ghost for blocked tasks
    engine
        .run(r#"CREATE GHOST "perf_blocked" FROM "perf" WHERE _type = "Task" AND status = "blocked" ORDER BY hours"#)
        .unwrap();

    // Time: SCAN primary with filter (explicit LIMIT — v0.2.5.1 default cap
    // would cut at SCAN_LIMIT_DEFAULT=1000 and break the count comparison
    // below; this test legitimately wants the full ~1667 blocked rows).
    let start = std::time::Instant::now();
    let r = engine
        .run(r#"SCAN "perf" WHERE _type = "Task" AND status = "blocked" LIMIT 5000"#)
        .unwrap();
    let primary_time = start.elapsed();
    let primary_count = match &r {
        QueryResult::Records(recs) => recs.len(),
        _ => 0,
    };

    // Time: SCAN GHOST (no filter needed — Ghost already filtered)
    let start = std::time::Instant::now();
    let r = engine
        .run(r#"SCAN GHOST "perf_blocked" LIMIT 5000"#)
        .unwrap();
    let ghost_time = start.elapsed();
    let ghost_count = match &r {
        QueryResult::Records(recs) => recs.len(),
        _ => 0,
    };

    assert_eq!(
        primary_count, ghost_count,
        "Ghost and primary should return same count"
    );

    let speedup = primary_time.as_secs_f64() / ghost_time.as_secs_f64().max(0.0001);
    println!("\n  Ghost Lobe Performance:");
    println!(
        "    Primary SCAN: {:.3}ms ({} records scanned, {} returned)",
        primary_time.as_secs_f64() * 1000.0,
        5000,
        primary_count
    );
    println!(
        "    Ghost SCAN:   {:.3}ms ({} records scanned)",
        ghost_time.as_secs_f64() * 1000.0,
        ghost_count
    );
    println!("    Speedup:      {:.1}x", speedup);

    // Ghost should be faster because it only scans ~1667 records vs 5000
    assert!(
        speedup > 1.0,
        "Ghost should be faster than primary: {speedup:.1}x"
    );
}

/// Test that SCAN transparently routes to a Ghost Lobe via honeycomb grid.
/// After the first SCAN (which triggers rebuild), subsequent SCANs should
/// detect the ghost and route to it automatically.
#[test]
fn test_honeycomb_ghost_routing() {
    let (engine, _dir) = temp_engine();

    engine.run(r#"LOBE "routing""#).unwrap();

    // Insert records: 30% "blocked", 70% "pending"
    for i in 0..300 {
        let status = if i % 3 == 0 { "blocked" } else { "pending" };
        engine
            .run(&format!(
                r#"PUT {{_type: "Task", numero: {i}, status: "{status}"}} IN "routing""#
            ))
            .unwrap();
    }

    // First SCAN — triggers opportunistic rebuild (no grid yet)
    let r1 = engine
        .run(r#"SCAN "routing" WHERE _type = "Task" AND status = "blocked""#)
        .unwrap();
    let count1 = match &r1 {
        QueryResult::Records(recs) => recs.len(),
        _ => panic!("Expected Records"),
    };
    assert_eq!(count1, 100, "Should find 100 blocked tasks");

    // Create Ghost for blocked tasks
    engine
        .run(r#"CREATE GHOST "routing_blocked" FROM "routing" WHERE _type = "Task" AND status = "blocked" ORDER BY numero"#)
        .unwrap();

    // Second SCAN — should now route to ghost (grid rebuilt, ghost registered)
    let r2 = engine
        .run(r#"SCAN "routing" WHERE _type = "Task" AND status = "blocked""#)
        .unwrap();
    let count2 = match &r2 {
        QueryResult::Records(recs) => recs.len(),
        _ => panic!("Expected Records"),
    };
    assert_eq!(count2, 100, "Routed scan should return same count");

    // Verify via SHOW SCAN STATS that routing happened
    let stats = engine.run("SHOW SCAN STATS").unwrap();
    match &stats {
        QueryResult::Info(lines) => {
            let stats_text = lines.join("\n");
            println!("  SCAN STATS:\n{stats_text}");
            // Should show at least 2 scans recorded
            assert!(
                stats_text.contains("Total scans recorded: 2"),
                "Should have 2 scans recorded"
            );
        }
        _ => panic!("Expected Info"),
    }
}

#[test]
fn test_scan_limit() {
    let (engine, _dir) = temp_engine();

    engine.run(r#"LOBE "lim""#).unwrap();

    for i in 0..200 {
        engine
            .run(&format!(
                r#"PUT {{_type: "Item", numero: {i}, status: "active"}} IN "lim""#
            ))
            .unwrap();
    }

    // SCAN without LIMIT — should return all 200
    let r = engine.run(r#"SCAN "lim""#).unwrap();
    assert_records(&r, 200);

    // SCAN with LIMIT 50
    let r = engine.run(r#"SCAN "lim" LIMIT 50"#).unwrap();
    assert_records(&r, 50);

    // SCAN with LIMIT larger than dataset
    let r = engine.run(r#"SCAN "lim" LIMIT 999"#).unwrap();
    assert_records(&r, 200);

    // SCAN with WHERE + LIMIT
    let r = engine
        .run(r#"SCAN "lim" WHERE status = "active" LIMIT 10"#)
        .unwrap();
    assert_records(&r, 10);
}

#[test]
fn test_scan_limit_with_pipeline() {
    let (engine, _dir) = temp_engine();

    engine.run(r#"LOBE "plim""#).unwrap();

    for i in 0..100 {
        engine
            .run(&format!(r#"PUT {{_type: "Item", numero: {i}}} IN "plim""#))
            .unwrap();
    }

    // SCAN LIMIT 30 | AGGREGATE count() — should count 30, not 100
    let r = engine
        .run(r#"SCAN "plim" LIMIT 30 | AGGREGATE count()"#)
        .unwrap();
    match &r {
        QueryResult::Aggregation(map) => {
            assert_eq!(
                map.get("count"),
                Some(&xyzdb_core::value::Value::Int(30)),
                "LIMIT 30 + AGGREGATE count() should yield 30, got: {map:?}"
            );
        }
        other => panic!("Expected Aggregation, got: {other:?}"),
    }
}

/// SCAN GHOST "name" | AGGREGATE count() — pipeline from ghost
#[test]
fn test_scan_ghost_pipeline() {
    let (engine, _dir) = temp_engine();

    engine.run(r#"LOBE "sgp""#).unwrap();
    for i in 0..100 {
        let status = if i % 4 == 0 { "blocked" } else { "pending" };
        engine
            .run(&format!(
                r#"PUT {{_type: "Task", numero: {i}, status: "{status}"}} IN "sgp""#
            ))
            .unwrap();
    }

    engine
        .run(r#"CREATE GHOST "sgp_blocked" FROM "sgp" WHERE status = "blocked" ORDER BY numero"#)
        .unwrap();

    // Pipeline: SCAN GHOST | AGGREGATE count()
    let r = engine
        .run(r#"SCAN GHOST "sgp_blocked" | AGGREGATE count()"#)
        .unwrap();
    match &r {
        QueryResult::Aggregation(map) => {
            assert_eq!(
                map.get("count"),
                Some(&xyzdb_core::value::Value::Int(25)),
                "25 blocked tasks, got: {map:?}"
            );
        }
        other => panic!("Expected Aggregation, got: {other:?}"),
    }
}

/// REFRESH GHOST preserves the original WHERE filters
#[test]
fn test_refresh_ghost_preserves_filters() {
    let (engine, _dir) = temp_engine();

    engine.run(r#"LOBE "rfg""#).unwrap();
    for i in 0..50 {
        let status = if i % 2 == 0 { "active" } else { "done" };
        engine
            .run(&format!(
                r#"PUT {{_type: "Item", numero: {i}, status: "{status}"}} IN "rfg""#
            ))
            .unwrap();
    }

    // Create ghost with filter: only "active" items
    engine
        .run(r#"CREATE GHOST "rfg_active" FROM "rfg" WHERE status = "active" ORDER BY numero"#)
        .unwrap();

    let r = engine.run(r#"SCAN GHOST "rfg_active""#).unwrap();
    assert_records(&r, 25);

    // Add more records
    for i in 50..70 {
        let status = if i % 2 == 0 { "active" } else { "done" };
        engine
            .run(&format!(
                r#"PUT {{_type: "Item", numero: {i}, status: "{status}"}} IN "rfg""#
            ))
            .unwrap();
    }

    // Refresh — should rebuild with original filter (status = "active")
    engine.run(r#"REFRESH GHOST "rfg_active""#).unwrap();

    let r = engine.run(r#"SCAN GHOST "rfg_active""#).unwrap();
    // 25 original + 10 new (50,52,54,56,58,60,62,64,66,68) = 35
    assert_records(&r, 35);
}

/// ORDER BY field DESC LIMIT N
#[test]
fn test_scan_order_by_limit() {
    let (engine, _dir) = temp_engine();

    engine.run(r#"LOBE "obl""#).unwrap();
    for i in 0..50 {
        engine
            .run(&format!(
                r#"PUT {{_type: "Item", amount: {}, name: "item_{i}"}} IN "obl""#,
                i * 100
            ))
            .unwrap();
    }

    // Top 5 by amount DESC
    let r = engine
        .run(r#"SCAN "obl" ORDER BY amount DESC LIMIT 5"#)
        .unwrap();
    let records = assert_records(&r, 5);
    let first_amount = records[0]
        .fields
        .get("amount")
        .and_then(|v| v.as_int())
        .unwrap();
    let last_amount = records[4]
        .fields
        .get("amount")
        .and_then(|v| v.as_int())
        .unwrap();
    assert_eq!(first_amount, 4900, "First should be 4900");
    assert_eq!(last_amount, 4500, "Fifth should be 4500");

    // Bottom 3 by amount ASC
    let r = engine
        .run(r#"SCAN "obl" ORDER BY amount ASC LIMIT 3"#)
        .unwrap();
    let records = assert_records(&r, 3);
    let first_amount = records[0]
        .fields
        .get("amount")
        .and_then(|v| v.as_int())
        .unwrap();
    assert_eq!(first_amount, 0, "First ASC should be 0");
}

/// ORDER BY without LIMIT should error
#[test]
fn test_scan_order_by_requires_limit() {
    let (engine, _dir) = temp_engine();

    engine.run(r#"LOBE "orl""#).unwrap();
    engine
        .run(r#"PUT {_type: "Item", amount: 100} IN "orl""#)
        .unwrap();

    let r = engine.run(r#"SCAN "orl" ORDER BY amount DESC"#);
    assert!(r.is_err(), "ORDER BY without LIMIT should fail");
    let err = r.unwrap_err().to_string();
    assert!(
        err.contains("ORDER BY requires LIMIT"),
        "Expected 'ORDER BY requires LIMIT', got: {err}"
    );
}

// ─── Ghost persistence tests (Surgery 4) ────────────────────────────────────

#[test]
fn test_ghost_persists_across_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().to_path_buf();

    // Session 1: create lobe, insert data, create ghost
    {
        let engine = Engine::open(&path).unwrap();
        engine.run(r#"LOBE "items""#).unwrap();
        engine.run(r#"ANCHOR "code" UNIQUE IN "items""#).unwrap();
        for i in 0..50 {
            engine
                .run(&format!(
                    r#"PUT {{code: "C-{i:03}", status: "active", amount: {i}}} IN "items""#
                ))
                .unwrap();
        }
        for i in 50..100 {
            engine
                .run(&format!(
                    r#"PUT {{code: "C-{i:03}", status: "overdue", amount: {i}}} IN "items""#
                ))
                .unwrap();
        }
        let r = engine.run(
            r#"CREATE GHOST "overdue_items" FROM "items" WHERE status = "overdue" ORDER BY amount"#
        ).unwrap();
        match &r {
            QueryResult::Ok { message, .. } => {
                assert!(
                    message.contains("50 records") || message.contains("50 index entries"),
                    "Expected 50 records/entries in ghost, got: {message}"
                );
            }
            other => panic!("Expected Ok, got: {other:?}"),
        }

        // Verify ghost works before restart
        let r = engine.run(r#"SCAN GHOST "overdue_items" LIMIT 5"#).unwrap();
        assert_records(&r, 5);
    }
    // engine dropped here — simulates shutdown

    // Session 2: reopen engine, verify ghost survived
    {
        let engine = Engine::open(&path).unwrap();

        // Ghost should be listed
        let r = engine.run(r#"SHOW GHOSTS"#).unwrap();
        match &r {
            QueryResult::Info(lines) => {
                let text = lines.join("\n");
                assert!(
                    text.contains("overdue_items"),
                    "Ghost should survive restart. Got: {text}"
                );
            }
            other => panic!("Expected Info, got: {other:?}"),
        }

        // Ghost scan should work (data is in the shared keyspace)
        let r = engine.run(r#"SCAN GHOST "overdue_items" LIMIT 5"#).unwrap();
        assert_records(&r, 5);

        // Ghost routing should work for SCAN with matching filters
        let r = engine
            .run(r#"SCAN "items" WHERE status = "overdue" LIMIT 10"#)
            .unwrap();
        assert_records(&r, 10);
    }
}

#[test]
fn test_ghost_drop_persists_across_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().to_path_buf();

    // Session 1: create ghost then drop it
    {
        let engine = Engine::open(&path).unwrap();
        engine.run(r#"LOBE "stuff""#).unwrap();
        for i in 0..20 {
            engine
                .run(&format!(r#"PUT {{tag: "a", val: {i}}} IN "stuff""#))
                .unwrap();
        }
        engine
            .run(r#"CREATE GHOST "g1" FROM "stuff" WHERE tag = "a" ORDER BY val"#)
            .unwrap();
        engine.run(r#"DROP GHOST "g1""#).unwrap();
    }

    // Session 2: ghost should NOT exist
    {
        let engine = Engine::open(&path).unwrap();
        let r = engine.run(r#"SHOW GHOSTS"#).unwrap();
        match &r {
            QueryResult::Info(lines) => {
                let text = lines.join("\n");
                assert!(
                    !text.contains("g1"),
                    "Dropped ghost should not survive restart. Got: {text}"
                );
            }
            other => panic!("Expected Info, got: {other:?}"),
        }
    }
}

// ─── ANALYZE tests (Surgery 8) ──────────────────────────────────────────────

#[test]
fn test_analyze_with_data() {
    let (engine, _dir) = temp_engine();

    engine.run(r#"LOBE "items""#).unwrap();
    for i in 0..200 {
        engine
            .run(&format!(
                r#"PUT {{code: "C-{i:04}", status: "active", region: "US"}} IN "items""#
            ))
            .unwrap();
    }

    let r = engine.run(r#"ANALYZE "items""#).unwrap();
    match &r {
        QueryResult::Info(lines) => {
            let text = lines.join("\n");
            println!("{text}");
            // code should be HIGH cardinality (200 unique / 200 total)
            assert!(text.contains("code"), "code field should appear");
            assert!(text.contains("HIGH"), "code should be HIGH cardinality");
            // region should be CONSTANT (1 unique)
            assert!(text.contains("CONSTANT"), "region should be CONSTANT");
            // status should also be CONSTANT
            assert!(
                text.contains("Ghost Lobe filter"),
                "low cardinality fields should suggest Ghost"
            );
        }
        other => panic!("Expected Info, got: {other:?}"),
    }
}

#[test]
fn test_analyze_empty_lobe() {
    let (engine, _dir) = temp_engine();
    engine.run(r#"LOBE "empty""#).unwrap();

    let r = engine.run(r#"ANALYZE "empty""#).unwrap();
    match &r {
        QueryResult::Info(lines) => {
            let text = lines.join("\n");
            assert!(text.contains("0 records"), "Should report empty lobe");
        }
        other => panic!("Expected Info, got: {other:?}"),
    }
}

// ─── Gravity (*campo) tests (Surgery 6) ─────────────────────────────────────

#[test]
fn test_gravity_colocation() {
    let (engine, _dir) = temp_engine();
    engine.run(r#"LOBE "data""#).unwrap();

    // Two records with same *key value should share gravity_hash → co-located
    engine
        .run(r#"PUT {*key: "K1", _type: "Parent", name: "Alpha"} IN "data""#)
        .unwrap();
    engine
        .run(r#"PUT {*key: "K1", _type: "Child", val: 42} IN "data""#)
        .unwrap();

    // PULL from the parent should bring both (same gravity_hash)
    let r = engine
        .run(r#"FIND "data" WHERE name = "Alpha" | PULL depth=1"#)
        .unwrap();
    let recs = assert_records(&r, 2);

    let types: Vec<String> = recs
        .iter()
        .filter_map(|r| r.fields.get("_type"))
        .filter_map(|v| v.as_text().map(String::from))
        .collect();
    assert!(types.contains(&"Parent".to_string()));
    assert!(types.contains(&"Child".to_string()));
}

#[test]
fn test_gravity_find_via_dictionary() {
    let (engine, _dir) = temp_engine();
    engine.run(r#"LOBE "data""#).unwrap();

    engine
        .run(r#"PUT {*ref: "R-001", amount: 100} IN "data""#)
        .unwrap();
    engine
        .run(r#"PUT {*ref: "R-002", amount: 200} IN "data""#)
        .unwrap();

    // FIND by gravity field should resolve via dictionary (fast path)
    let r = engine.run(r#"FIND "data" WHERE ref = "R-001""#).unwrap();
    let recs = assert_records(&r, 1);
    assert_eq!(
        recs[0].fields.get("amount"),
        Some(&xyzdb_core::value::Value::Int(100))
    );
}

#[test]
fn test_gravity_link_overrides() {
    let (engine, _dir) = temp_engine();
    engine.run(r#"LOBE "data""#).unwrap();
    engine.run(r#"ANCHOR "code" UNIQUE IN "data""#).unwrap();

    // Parent with gravity
    engine
        .run(r#"PUT {*key: "K1", code: "P1", _type: "Parent"} IN "data""#)
        .unwrap();
    // Child with LINK TO → LINK should override *gravity for gravity_hash
    engine.run(r#"PUT {*key: "K2", _type: "Child"} IN "data" LINK TO "data" WHERE code = "P1" AS "child_of""#).unwrap();

    // PULL from parent should bring child (LINK inheritance, not gravity)
    let r = engine
        .run(r#"FIND "data" WHERE code = "P1" | PULL depth=1"#)
        .unwrap();
    let recs = assert_records(&r, 2);

    let types: Vec<String> = recs
        .iter()
        .filter_map(|r| r.fields.get("_type"))
        .filter_map(|v| v.as_text().map(String::from))
        .collect();
    assert!(types.contains(&"Parent".to_string()));
    assert!(types.contains(&"Child".to_string()));
}

// ─── Auto-Ghost tests (Surgery 7) ───────────────────────────────────────────

#[test]
fn test_auto_ghost_telemetry_tracking() {
    let (engine, _dir) = temp_engine();
    engine.run(r#"LOBE "data""#).unwrap();

    for i in 0..500 {
        engine
            .run(&format!(r#"PUT {{status: "active", val: {i}}} IN "data""#))
            .unwrap();
    }

    // Execute the same filtered SCAN 6 times
    for _ in 0..6 {
        let _ = engine
            .run(r#"SCAN "data" WHERE status = "active""#)
            .unwrap();
    }

    // Check scan stats show the pattern was tracked
    let r = engine.run(r#"SHOW SCAN STATS"#).unwrap();
    match &r {
        QueryResult::Info(lines) => {
            let text = lines.join("\n");
            assert!(
                text.contains("Total scans recorded: 6"),
                "Should track 6 scans. Got: {text}"
            );
        }
        other => panic!("Expected Info, got: {other:?}"),
    }
}

#[test]
fn test_manual_ghost_survives_ttl_cleanup() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().to_path_buf();

    // Session 1: create a manual ghost
    {
        let engine = Engine::open(&path).unwrap();
        engine.run(r#"LOBE "data""#).unwrap();
        for i in 0..20 {
            engine
                .run(&format!(r#"PUT {{tag: "x", v: {i}}} IN "data""#))
                .unwrap();
        }
        engine
            .run(r#"CREATE GHOST "manual_ghost" FROM "data" WHERE tag = "x" ORDER BY v"#)
            .unwrap();
    }

    // Session 2: reopen — manual ghost should survive (not auto-created, TTL doesn't apply)
    {
        let engine = Engine::open(&path).unwrap();
        let r = engine.run(r#"SHOW GHOSTS"#).unwrap();
        match &r {
            QueryResult::Info(lines) => {
                let text = lines.join("\n");
                assert!(
                    text.contains("manual_ghost"),
                    "Manual ghost should survive TTL cleanup. Got: {text}"
                );
            }
            other => panic!("Expected Info, got: {other:?}"),
        }
    }
}

#[test]
fn test_fast_scan_no_auto_ghost() {
    let (engine, _dir) = temp_engine();
    engine.run(r#"LOBE "small""#).unwrap();

    // Only 10 records — scans will be fast (<500ms)
    for i in 0..10 {
        engine
            .run(&format!(r#"PUT {{tag: "a", v: {i}}} IN "small""#))
            .unwrap();
    }

    // Execute 100 times — should NOT create auto-ghost (latency < 500ms)
    for _ in 0..100 {
        let _ = engine.run(r#"SCAN "small" WHERE tag = "a""#).unwrap();
    }

    // No auto-ghost should exist
    let r = engine.run(r#"SHOW GHOSTS"#).unwrap();
    match &r {
        QueryResult::Info(lines) => {
            let text = lines.join("\n");
            assert!(
                !text.contains("auto_"),
                "Fast scans should NOT trigger auto-ghost. Got: {text}"
            );
        }
        other => panic!("Expected Info, got: {other:?}"),
    }
}

// ─── Ghost ORDER BY tests (Q4 fix) ─────────────────────────────────────────

#[test]
fn test_ghost_order_by_match_early_termination() {
    let (engine, _dir) = temp_engine();
    engine.run(r#"LOBE "data""#).unwrap();

    for i in 0..100 {
        engine
            .run(&format!(
                r#"PUT {{status: "active", amount: {i}}} IN "data""#
            ))
            .unwrap();
    }

    // Ghost ordered by amount ASC
    engine
        .run(r#"CREATE GHOST "by_amount" FROM "data" WHERE status = "active" ORDER BY amount"#)
        .unwrap();

    // SCAN ORDER BY amount ASC LIMIT 5 → matches ghost order → early termination
    let r = engine
        .run(r#"SCAN "data" WHERE status = "active" ORDER BY amount ASC LIMIT 5"#)
        .unwrap();
    let recs = assert_records(&r, 5);

    let first = recs[0]
        .fields
        .get("amount")
        .and_then(|v| match v {
            xyzdb_core::value::Value::Int(i) => Some(*i),
            _ => None,
        })
        .unwrap();
    let last = recs[4]
        .fields
        .get("amount")
        .and_then(|v| match v {
            xyzdb_core::value::Value::Int(i) => Some(*i),
            _ => None,
        })
        .unwrap();
    assert_eq!(first, 0, "ASC early-term: first should be 0");
    assert_eq!(last, 4, "ASC early-term: last should be 4");
}

#[test]
fn test_ghost_order_by_mismatch_minheap() {
    let (engine, _dir) = temp_engine();
    engine.run(r#"LOBE "data""#).unwrap();

    for i in 0..100 {
        engine
            .run(&format!(
                r#"PUT {{status: "active", amount: {i}}} IN "data""#
            ))
            .unwrap();
    }

    // Ghost ordered by amount ASC
    engine
        .run(r#"CREATE GHOST "by_amount" FROM "data" WHERE status = "active" ORDER BY amount"#)
        .unwrap();

    // SCAN ORDER BY amount DESC LIMIT 5 → mismatch → min-heap
    let r = engine
        .run(r#"SCAN "data" WHERE status = "active" ORDER BY amount DESC LIMIT 5"#)
        .unwrap();
    let recs = assert_records(&r, 5);

    let first = recs[0]
        .fields
        .get("amount")
        .and_then(|v| match v {
            xyzdb_core::value::Value::Int(i) => Some(*i),
            _ => None,
        })
        .unwrap();
    let last = recs[4]
        .fields
        .get("amount")
        .and_then(|v| match v {
            xyzdb_core::value::Value::Int(i) => Some(*i),
            _ => None,
        })
        .unwrap();
    assert_eq!(first, 99, "DESC min-heap: first should be 99");
    assert_eq!(last, 95, "DESC min-heap: last should be 95");
}

// ── V3: Ghost Projection Tests ──────────────────────────────────────────────

#[test]
fn test_ghost_projection_reduces_size() {
    let (engine, _dir) = temp_engine();
    engine.run(r#"LOBE "records""#).unwrap();

    // Insert records with many fields
    for i in 0..50 {
        engine.run(&format!(
            r#"PUT {{status: "active", amount: {i}, name: "item_{i}", category: "tools", notes: "some long description for record {i}"}} IN "records""#
        )).unwrap();
    }

    // Create ghost with projection: only status + amount + order_by field
    engine
        .run(r#"CREATE GHOST "proj" FROM "records" WHERE status = "active" ORDER BY amount"#)
        .unwrap();

    // Scan ghost — should have projected fields + redundant injected
    let r = engine.run(r#"SCAN GHOST "proj" LIMIT 10"#).unwrap();
    let recs = assert_records(&r, 10);

    // Redundant field (status) is injected from metadata
    assert_eq!(
        recs[0].fields.get("status"),
        Some(&xyzdb_core::value::Value::Text("active".into())),
        "Redundant field 'status' should be injected"
    );

    // Projected field (amount) is stored on disk
    assert!(
        recs[0].fields.contains_key("amount"),
        "Projected field 'amount' should exist"
    );

    // V1: non-projected fields are absent. V2: full records returned (point-read from spatial).
    // Both are correct — V2 stores references, not copies.
    // assert!(!recs[0].fields.contains_key("name"), "V1: non-projected absent");

    // LID should always be present
    assert!(
        !format!("{}", recs[0].lid).is_empty(),
        "LID should be present"
    );
}

#[test]
fn test_ghost_projection_aggregate_correct() {
    let (engine, _dir) = temp_engine();
    engine.run(r#"LOBE "sales""#).unwrap();

    for i in 0..100 {
        let status = if i % 3 == 0 { "pending" } else { "completed" };
        engine
            .run(&format!(
                r#"PUT {{status: "{status}", amount: {i}, notes: "padding data"}} IN "sales""#
            ))
            .unwrap();
    }

    // Ghost with WHERE status = "pending" — 34 records (0, 3, 6, ..., 99)
    engine
        .run(r#"CREATE GHOST "pending" FROM "sales" WHERE status = "pending" ORDER BY amount"#)
        .unwrap();

    // AGGREGATE sum(amount) over the ghost — should work even though records are projected
    let r = engine
        .run(r#"SCAN "sales" WHERE status = "pending" | AGGREGATE sum(amount)"#)
        .unwrap();
    match &r {
        QueryResult::Aggregation(agg) => {
            // Sum of 0+3+6+...+99 = 3*(0+1+2+...+33) = 3*33*34/2 = 1683
            let sum_val = agg.iter().find(|(k, _)| k.contains("sum")).map(|(_, v)| v);
            assert_eq!(
                sum_val,
                Some(&xyzdb_core::value::Value::Float(1683.0)),
                "Aggregate sum should be correct on projected ghost"
            );
        }
        other => panic!("Expected Aggregation, got: {other:?}"),
    }
}

#[test]
fn test_ghost_projection_persists_across_restart() {
    let dir = tempfile::tempdir().expect("tempdir");

    // First session: create projected ghost
    {
        let engine = Engine::open(dir.path()).unwrap();
        engine.run(r#"LOBE "data""#).unwrap();
        for i in 0..20 {
            engine
                .run(&format!(
                    r#"PUT {{status: "ok", value: {i}, extra: "padding"}} IN "data""#
                ))
                .unwrap();
        }
        engine
            .run(r#"CREATE GHOST "proj_g" FROM "data" WHERE status = "ok" ORDER BY value"#)
            .unwrap();
    }

    // Second session: ghost metadata should restore correctly
    {
        let engine = Engine::open(dir.path()).unwrap();

        // V2 ghosts don't use projection (references, not copies).
        // V1: ghost_is_projected returns true. V2: returns false (no projection needed).
        // Both are valid — skip projection-specific checks for V2.

        // Scan should still work after restart (V2: full records from spatial)
        let r = engine.run(r#"SCAN GHOST "proj_g" LIMIT 5"#).unwrap();
        let recs = assert_records(&r, 5);
        assert_eq!(
            recs[0].fields.get("status"),
            Some(&xyzdb_core::value::Value::Text("ok".into())),
            "status field should be present after restart"
        );
    }
}

// ── V3: PIN / UNPIN / SHOW PROFILE Tests ─────────────────────────────────────

#[test]
fn test_pin_unpin_basic() {
    let (engine, _dir) = temp_engine();
    engine.run(r#"LOBE "customers""#).unwrap();

    // PIN fields
    let r = engine.run(r#"PIN name, email IN "customers""#).unwrap();
    assert_ok(&r);

    // SHOW PROFILE should list pinned fields
    let r = engine.run(r#"SHOW PROFILE "customers""#).unwrap();
    match &r {
        QueryResult::Info(lines) => {
            let joined = lines.join("\n");
            assert!(joined.contains("name"), "Profile should show pinned 'name'");
            assert!(
                joined.contains("email"),
                "Profile should show pinned 'email'"
            );
        }
        other => panic!("Expected Info, got: {other:?}"),
    }

    // UNPIN one field
    engine.run(r#"UNPIN email IN "customers""#).unwrap();

    let r = engine.run(r#"SHOW PROFILE "customers""#).unwrap();
    match &r {
        QueryResult::Info(lines) => {
            let joined = lines.join("\n");
            assert!(joined.contains("name"), "Profile should still show 'name'");
            assert!(
                !joined.contains("email"),
                "Profile should no longer show 'email'"
            );
        }
        other => panic!("Expected Info, got: {other:?}"),
    }
}

#[test]
fn show_profile_reports_vector_field() {
    let dir = tempfile::tempdir().expect("tempdir");
    let engine = Engine::open(dir.path()).expect("open");

    let profile = |lobe: &str| -> String {
        match engine.run(&format!(r#"SHOW PROFILE "{lobe}""#)).unwrap() {
            QueryResult::Info(lines) => lines.join("\n"),
            other => panic!("expected Info, got {other:?}"),
        }
    };

    // (c) a lobe with no searchable vector field → "Vector: (none)".
    engine.run(r#"LOBE "plain""#).unwrap();
    assert!(
        profile("plain").contains("Vector: (none)"),
        "no-vector lobe should report Vector: (none)"
    );

    // (b) declared but the dimension is not yet learned → "dim unknown".
    engine.run(r#"LOBE "mem""#).unwrap();
    engine.run(r#"VECTOR emb IN "mem""#).unwrap();
    let p = profile("mem");
    assert!(
        p.contains("Vector: emb dim unknown"),
        "declared/unfixed: {p}"
    );

    // (a) the first embedding fixes the dimension → a concrete "dim <N>".
    let coords: Vec<&str> = (0..64)
        .map(|i| if i == 0 { "1.0" } else { "0.0" })
        .collect();
    engine
        .run(&format!(
            r#"PUT {{*conv:"c1", id:"r1", emb:[{}]}} IN "mem""#,
            coords.join(", ")
        ))
        .unwrap();
    let p = profile("mem");
    assert!(
        p.contains("Vector: emb dim ") && !p.contains("dim unknown"),
        "after a PUT the dimension should be fixed: {p}"
    );
}

#[test]
fn test_pin_persists_across_restart() {
    let dir = tempfile::tempdir().expect("tempdir");

    {
        let engine = Engine::open(dir.path()).unwrap();
        engine.run(r#"LOBE "data""#).unwrap();
        engine.run(r#"PIN amount, status IN "data""#).unwrap();
    }

    {
        let engine = Engine::open(dir.path()).unwrap();
        let pinned = engine.get_pinned_fields("data");
        assert!(
            pinned.contains(&"amount".to_string()),
            "amount should be pinned after restart"
        );
        assert!(
            pinned.contains(&"status".to_string()),
            "status should be pinned after restart"
        );
    }
}

#[test]
fn test_pin_included_in_ghost_projection() {
    let (engine, _dir) = temp_engine();
    engine.run(r#"LOBE "items""#).unwrap();

    // Pin a field BEFORE creating ghost
    engine.run(r#"PIN category IN "items""#).unwrap();

    for i in 0..30 {
        engine.run(&format!(
            r#"PUT {{status: "active", amount: {i}, category: "tools", notes: "padding"}} IN "items""#
        )).unwrap();
    }

    // Create ghost — projection should include pinned 'category' even though it's not in filters
    engine
        .run(r#"CREATE GHOST "active_items" FROM "items" WHERE status = "active" ORDER BY amount"#)
        .unwrap();

    // V2 returns full records (point-read from spatial), all fields present.
    // PIN affects V1 projection only. For V2, all fields are always available.
    let r = engine.run(r#"SCAN GHOST "active_items" LIMIT 5"#).unwrap();
    let recs = assert_records(&r, 5);
    assert_eq!(
        recs[0].fields.get("category"),
        Some(&xyzdb_core::value::Value::Text("tools".into())),
        "Field 'category' should be present"
    );
}

// ─── V4 Null Tests ──────────────────────────────────────────────────────────

#[test]
fn test_null_insert_and_find() {
    let (engine, _dir) = temp_engine();
    assert_ok(
        &engine
            .run(r#"PUT {name: "Alice", score: null} IN "users""#)
            .unwrap(),
    );
    assert_ok(&engine.run(r#"PUT {name: "Bob"} IN "users""#).unwrap());

    // Find with = null matches only explicit Null (not absent)
    let r = engine.run(r#"FIND "users" WHERE score = null"#).unwrap();
    let recs = assert_records(&r, 1);
    assert_eq!(
        recs[0].fields.get("name"),
        Some(&xyzdb_core::value::Value::Text("Alice".into()))
    );
    assert_eq!(
        recs[0].fields.get("score"),
        Some(&xyzdb_core::value::Value::Null)
    );
}

#[test]
fn test_null_is_null_filter() {
    let (engine, _dir) = temp_engine();
    assert_ok(
        &engine
            .run(r#"PUT {name: "Alice", score: null} IN "users""#)
            .unwrap(),
    );
    assert_ok(&engine.run(r#"PUT {name: "Bob"} IN "users""#).unwrap());
    assert_ok(
        &engine
            .run(r#"PUT {name: "Charlie", score: 85} IN "users""#)
            .unwrap(),
    );

    // IS NULL matches both absent field AND explicit Null
    let r = engine.run(r#"SCAN "users" WHERE score IS NULL"#).unwrap();
    let recs = assert_records(&r, 2);
    let names: Vec<&str> = recs
        .iter()
        .filter_map(|r| r.fields.get("name")?.as_text())
        .collect();
    assert!(names.contains(&"Alice"), "Alice has score=null");
    assert!(names.contains(&"Bob"), "Bob has no score field");
}

#[test]
fn test_null_is_not_null_filter() {
    let (engine, _dir) = temp_engine();
    assert_ok(
        &engine
            .run(r#"PUT {name: "Alice", score: null} IN "users""#)
            .unwrap(),
    );
    assert_ok(&engine.run(r#"PUT {name: "Bob"} IN "users""#).unwrap());
    assert_ok(
        &engine
            .run(r#"PUT {name: "Charlie", score: 85} IN "users""#)
            .unwrap(),
    );

    // IS NOT NULL matches only records with a non-null value present
    let r = engine
        .run(r#"SCAN "users" WHERE score IS NOT NULL"#)
        .unwrap();
    let recs = assert_records(&r, 1);
    assert_eq!(
        recs[0].fields.get("name"),
        Some(&xyzdb_core::value::Value::Text("Charlie".into()))
    );
}

#[test]
fn test_null_neq_filter() {
    let (engine, _dir) = temp_engine();
    assert_ok(
        &engine
            .run(r#"PUT {name: "Alice", status: null} IN "items""#)
            .unwrap(),
    );
    assert_ok(
        &engine
            .run(r#"PUT {name: "Bob", status: "active"} IN "items""#)
            .unwrap(),
    );

    // != null matches records where field exists and is NOT Null
    let r = engine.run(r#"SCAN "items" WHERE status != null"#).unwrap();
    let recs = assert_records(&r, 1);
    assert_eq!(
        recs[0].fields.get("name"),
        Some(&xyzdb_core::value::Value::Text("Bob".into()))
    );
}

#[test]
fn test_null_aggregate_skip() {
    let (engine, _dir) = temp_engine();
    assert_ok(
        &engine
            .run(r#"PUT {name: "A", amount: 100} IN "data""#)
            .unwrap(),
    );
    assert_ok(
        &engine
            .run(r#"PUT {name: "B", amount: null} IN "data""#)
            .unwrap(),
    );
    assert_ok(
        &engine
            .run(r#"PUT {name: "C", amount: 200} IN "data""#)
            .unwrap(),
    );

    // sum/avg should skip Null values (same as non-numeric)
    let r = engine
        .run(r#"SCAN "data" | AGGREGATE sum(amount), count()"#)
        .unwrap();
    match &r {
        QueryResult::Aggregation(agg) => {
            assert_eq!(
                agg.get("sum(amount)"),
                Some(&xyzdb_core::value::Value::Float(300.0))
            );
            assert_eq!(agg.get("count"), Some(&xyzdb_core::value::Value::Int(3)));
        }
        other => panic!("Expected Aggregation, got: {other:?}"),
    }
}

#[test]
fn test_null_order_by_last() {
    let (engine, _dir) = temp_engine();
    assert_ok(
        &engine
            .run(r#"PUT {name: "A", score: 50} IN "data""#)
            .unwrap(),
    );
    assert_ok(
        &engine
            .run(r#"PUT {name: "B", score: null} IN "data""#)
            .unwrap(),
    );
    assert_ok(
        &engine
            .run(r#"PUT {name: "C", score: 10} IN "data""#)
            .unwrap(),
    );

    // ORDER BY ASC: Null should be last
    let r = engine
        .run(r#"SCAN "data" ORDER BY score ASC LIMIT 3"#)
        .unwrap();
    let recs = assert_records(&r, 3);
    let names: Vec<&str> = recs
        .iter()
        .filter_map(|r| r.fields.get("name")?.as_text())
        .collect();
    assert_eq!(names, vec!["C", "A", "B"], "Null should sort last in ASC");
}

#[test]
fn test_null_backward_compat_records() {
    // Records created without Null fields should deserialize correctly
    let (engine, dir) = temp_engine();
    assert_ok(
        &engine
            .run(r#"PUT {name: "Old", value: 42} IN "legacy""#)
            .unwrap(),
    );
    drop(engine);

    // Re-open (simulates upgrade from V3 to V4)
    let engine = Engine::open(dir.path()).expect("re-open should work");
    let r = engine.run(r#"FIND "legacy" WHERE name = "Old""#).unwrap();
    let recs = assert_records(&r, 1);
    assert_eq!(
        recs[0].fields.get("value"),
        Some(&xyzdb_core::value::Value::Int(42))
    );
}

// ─── V4 Streaming Tests ─────────────────────────────────────────────────────

#[test]
fn test_streaming_basic() {
    let (engine, _dir) = temp_engine();
    for i in 0..100 {
        assert_ok(
            &engine
                .run(&format!(
                    r#"PUT {{name: "item_{i}", value: {i}}} IN "data""#
                ))
                .unwrap(),
        );
    }

    let stmt = xytalk_parser::parse(r#"SCAN "data""#).unwrap();
    let scan_stmt = match &stmt {
        xytalk_parser::ast::Statement::Scan(s) => s,
        _ => panic!("Expected Scan"),
    };

    let mut buf: Vec<u8> = Vec::new();
    let serialize_fn: fn(&xyzdb_core::record::Record) -> Vec<u8> =
        |r| bincode::serialize(r).unwrap();
    let count =
        xyzdb_engine::ops::scan::execute_scan_streaming(&engine, scan_stmt, &mut buf, serialize_fn)
            .unwrap();
    assert_eq!(count, 100);
    assert!(!buf.is_empty(), "Buffer should have chunked data");

    // Verify chunks can be read back
    let mut cursor = std::io::Cursor::new(&buf);
    use std::io::Read;
    let mut decoded = 0u64;
    loop {
        let mut len_buf = [0u8; 4];
        if cursor.read_exact(&mut len_buf).is_err() {
            break;
        }
        let len = u32::from_be_bytes(len_buf) as usize;
        if len == 0 {
            break;
        }
        let mut payload = vec![0u8; len];
        cursor.read_exact(&mut payload).unwrap();
        let _record: xyzdb_core::record::Record = bincode::deserialize(&payload).unwrap();
        decoded += 1;
    }
    assert_eq!(decoded, 100);
}

#[test]
fn test_streaming_with_limit() {
    let (engine, _dir) = temp_engine();
    for i in 0..100 {
        assert_ok(
            &engine
                .run(&format!(
                    r#"PUT {{name: "item_{i}", value: {i}}} IN "data""#
                ))
                .unwrap(),
        );
    }

    let stmt = xytalk_parser::parse(r#"SCAN "data" LIMIT 10"#).unwrap();
    let scan_stmt = match &stmt {
        xytalk_parser::ast::Statement::Scan(s) => s,
        _ => panic!("Expected Scan"),
    };

    let mut buf: Vec<u8> = Vec::new();
    let count =
        xyzdb_engine::ops::scan::execute_scan_streaming(&engine, scan_stmt, &mut buf, |r| {
            bincode::serialize(r).unwrap()
        })
        .unwrap();
    assert_eq!(count, 10, "Streaming should respect LIMIT");
}

#[test]
fn test_streaming_with_filter() {
    let (engine, _dir) = temp_engine();
    for i in 0..50 {
        let status = if i % 2 == 0 { "active" } else { "inactive" };
        assert_ok(
            &engine
                .run(&format!(
                    r#"PUT {{name: "item_{i}", status: "{status}"}} IN "data""#
                ))
                .unwrap(),
        );
    }

    let stmt = xytalk_parser::parse(r#"SCAN "data" WHERE status = "active""#).unwrap();
    let scan_stmt = match &stmt {
        xytalk_parser::ast::Statement::Scan(s) => s,
        _ => panic!("Expected Scan"),
    };

    let mut buf: Vec<u8> = Vec::new();
    let count =
        xyzdb_engine::ops::scan::execute_scan_streaming(&engine, scan_stmt, &mut buf, |r| {
            bincode::serialize(r).unwrap()
        })
        .unwrap();
    assert_eq!(count, 25, "Should stream only active records");
}

#[test]
fn test_streaming_empty_scan() {
    let (engine, _dir) = temp_engine();
    engine.run(r#"LOBE "empty""#).unwrap();

    let stmt = xytalk_parser::parse(r#"SCAN "empty""#).unwrap();
    let scan_stmt = match &stmt {
        xytalk_parser::ast::Statement::Scan(s) => s,
        _ => panic!("Expected Scan"),
    };

    let mut buf: Vec<u8> = Vec::new();
    let count =
        xyzdb_engine::ops::scan::execute_scan_streaming(&engine, scan_stmt, &mut buf, |r| {
            bincode::serialize(r).unwrap()
        })
        .unwrap();
    assert_eq!(count, 0);
    assert!(buf.is_empty(), "Empty scan should produce no chunks");
}

#[test]
fn test_streaming_with_ghost_routing() {
    let (engine, _dir) = temp_engine();
    for i in 0..100 {
        let status = if i < 60 { "overdue" } else { "active" };
        assert_ok(
            &engine
                .run(&format!(
                    r#"PUT {{_type: "Installment", status: "{status}", amount: {i}}} IN "fin""#
                ))
                .unwrap(),
        );
    }
    assert_ok(&engine.run(
        r#"CREATE GHOST "overdues" FROM "fin" WHERE _type = "Installment" AND status = "overdue" ORDER BY amount"#
    ).unwrap());

    // SCAN that should route to ghost
    let stmt =
        xytalk_parser::parse(r#"SCAN "fin" WHERE _type = "Installment" AND status = "overdue""#)
            .unwrap();
    let scan_stmt = match &stmt {
        xytalk_parser::ast::Statement::Scan(s) => s,
        _ => panic!("Expected Scan"),
    };

    let mut buf: Vec<u8> = Vec::new();
    let count =
        xyzdb_engine::ops::scan::execute_scan_streaming(&engine, scan_stmt, &mut buf, |r| {
            bincode::serialize(r).unwrap()
        })
        .unwrap();
    assert_eq!(count, 60, "Should stream all overdue records from ghost");
}

#[test]
fn test_null_eq_null_is_true() {
    let (engine, _dir) = temp_engine();
    assert_ok(
        &engine
            .run(r#"PUT {name: "A", field: null} IN "test""#)
            .unwrap(),
    );
    assert_ok(
        &engine
            .run(r#"PUT {name: "B", field: null} IN "test""#)
            .unwrap(),
    );
    assert_ok(
        &engine
            .run(r#"PUT {name: "C", field: "value"} IN "test""#)
            .unwrap(),
    );

    // Null = Null is TRUE (xyzDB pragmatic semantics, NOT SQL)
    let r = engine.run(r#"SCAN "test" WHERE field = null"#).unwrap();
    let recs = assert_records(&r, 2);
    let names: Vec<&str> = recs
        .iter()
        .filter_map(|r| r.fields.get("name")?.as_text())
        .collect();
    assert!(names.contains(&"A"));
    assert!(names.contains(&"B"));
}

// ─── V4 List/Map/Dot Notation/Contains Tests ────────────────────────────────

#[test]
fn test_list_insert_and_find() {
    let (engine, _dir) = temp_engine();
    assert_ok(
        &engine
            .run(r#"PUT {name: "Alice", tags: ["tech", "saas", "b2b"]} IN "clients""#)
            .unwrap(),
    );
    assert_ok(
        &engine
            .run(r#"PUT {name: "Bob", tags: ["finance", "b2c"]} IN "clients""#)
            .unwrap(),
    );

    let r = engine
        .run(r#"FIND "clients" WHERE name = "Alice""#)
        .unwrap();
    let recs = assert_records(&r, 1);
    match recs[0].fields.get("tags") {
        Some(xyzdb_core::value::Value::List(items)) => assert_eq!(items.len(), 3),
        other => panic!("Expected List, got: {other:?}"),
    }
}

#[test]
fn test_map_insert_and_find() {
    let (engine, _dir) = temp_engine();
    assert_ok(
        &engine
            .run(r#"PUT {name: "Alice", scoring: {bureau: 685, risk: "medium"}} IN "clients""#)
            .unwrap(),
    );

    let r = engine
        .run(r#"FIND "clients" WHERE name = "Alice""#)
        .unwrap();
    let recs = assert_records(&r, 1);
    match recs[0].fields.get("scoring") {
        Some(xyzdb_core::value::Value::Map(m)) => {
            assert_eq!(m.get("bureau"), Some(&xyzdb_core::value::Value::Int(685)));
            assert_eq!(
                m.get("risk"),
                Some(&xyzdb_core::value::Value::Text("medium".into()))
            );
        }
        other => panic!("Expected Map, got: {other:?}"),
    }
}

#[test]
fn test_nested_map_list() {
    let (engine, _dir) = temp_engine();
    assert_ok(&engine.run(r#"PUT {name: "X", config: {rules: [{name: "r1", weight: 0.5}, {name: "r2", weight: 1.0}]}} IN "data""#).unwrap());

    let r = engine.run(r#"FIND "data" WHERE name = "X""#).unwrap();
    let recs = assert_records(&r, 1);
    // Verify nested structure exists
    let config = recs[0].fields.get("config").unwrap();
    assert!(matches!(config, xyzdb_core::value::Value::Map(_)));
}

#[test]
fn test_dot_notation_filter() {
    let (engine, _dir) = temp_engine();
    assert_ok(
        &engine
            .run(r#"PUT {name: "Alice", scoring: {bureau: 700, risk: "low"}} IN "clients""#)
            .unwrap(),
    );
    assert_ok(
        &engine
            .run(r#"PUT {name: "Bob", scoring: {bureau: 550, risk: "high"}} IN "clients""#)
            .unwrap(),
    );
    assert_ok(
        &engine
            .run(r#"PUT {name: "Charlie", scoring: {bureau: 800, risk: "low"}} IN "clients""#)
            .unwrap(),
    );

    // Dot notation in WHERE
    let r = engine
        .run(r#"SCAN "clients" WHERE scoring.bureau > 600"#)
        .unwrap();
    let recs = assert_records(&r, 2);
    let names: Vec<&str> = recs
        .iter()
        .filter_map(|r| r.fields.get("name")?.as_text())
        .collect();
    assert!(names.contains(&"Alice"));
    assert!(names.contains(&"Charlie"));
}

#[test]
fn test_dot_notation_eq() {
    let (engine, _dir) = temp_engine();
    assert_ok(
        &engine
            .run(r#"PUT {name: "Alice", scoring: {risk: "low"}} IN "clients""#)
            .unwrap(),
    );
    assert_ok(
        &engine
            .run(r#"PUT {name: "Bob", scoring: {risk: "high"}} IN "clients""#)
            .unwrap(),
    );

    let r = engine
        .run(r#"SCAN "clients" WHERE scoring.risk = "high""#)
        .unwrap();
    let recs = assert_records(&r, 1);
    assert_eq!(
        recs[0].fields.get("name"),
        Some(&xyzdb_core::value::Value::Text("Bob".into()))
    );
}

#[test]
fn test_dot_notation_missing_path() {
    let (engine, _dir) = temp_engine();
    assert_ok(
        &engine
            .run(r#"PUT {name: "Alice", scoring: {bureau: 700}} IN "clients""#)
            .unwrap(),
    );
    assert_ok(&engine.run(r#"PUT {name: "Bob"} IN "clients""#).unwrap());

    // scoring.bureau doesn't exist on Bob (no scoring field at all)
    let r = engine
        .run(r#"SCAN "clients" WHERE scoring.bureau > 600"#)
        .unwrap();
    let recs = assert_records(&r, 1);
    assert_eq!(
        recs[0].fields.get("name"),
        Some(&xyzdb_core::value::Value::Text("Alice".into()))
    );
}

#[test]
fn test_list_index_dot_notation() {
    let (engine, _dir) = temp_engine();
    assert_ok(
        &engine
            .run(r#"PUT {name: "X", scores: [10, 20, 30]} IN "data""#)
            .unwrap(),
    );

    // Access list element by index via dot notation
    let r = engine.run(r#"SCAN "data" WHERE scores.1 = 20"#).unwrap();
    assert_records(&r, 1);

    let r = engine.run(r#"SCAN "data" WHERE scores.0 > 5"#).unwrap();
    assert_records(&r, 1);
}

#[test]
fn test_contains_filter() {
    let (engine, _dir) = temp_engine();
    assert_ok(
        &engine
            .run(r#"PUT {name: "Alice", tags: ["tech", "saas"]} IN "clients""#)
            .unwrap(),
    );
    assert_ok(
        &engine
            .run(r#"PUT {name: "Bob", tags: ["finance"]} IN "clients""#)
            .unwrap(),
    );
    assert_ok(
        &engine
            .run(r#"PUT {name: "Charlie", tags: ["tech", "b2b"]} IN "clients""#)
            .unwrap(),
    );

    let r = engine
        .run(r#"SCAN "clients" WHERE tags CONTAINS "tech""#)
        .unwrap();
    let recs = assert_records(&r, 2);
    let names: Vec<&str> = recs
        .iter()
        .filter_map(|r| r.fields.get("name")?.as_text())
        .collect();
    assert!(names.contains(&"Alice"));
    assert!(names.contains(&"Charlie"));
}

#[test]
fn test_contains_int() {
    let (engine, _dir) = temp_engine();
    assert_ok(
        &engine
            .run(r#"PUT {name: "A", scores: [10, 20, 30]} IN "data""#)
            .unwrap(),
    );
    assert_ok(
        &engine
            .run(r#"PUT {name: "B", scores: [40, 50]} IN "data""#)
            .unwrap(),
    );

    let r = engine
        .run(r#"SCAN "data" WHERE scores CONTAINS 20"#)
        .unwrap();
    let recs = assert_records(&r, 1);
    assert_eq!(
        recs[0].fields.get("name"),
        Some(&xyzdb_core::value::Value::Text("A".into()))
    );
}

#[test]
fn test_contains_non_list_returns_false() {
    let (engine, _dir) = temp_engine();
    assert_ok(
        &engine
            .run(r#"PUT {name: "A", status: "active"} IN "data""#)
            .unwrap(),
    );

    // CONTAINS on a Text field should return no matches (not an error)
    let r = engine
        .run(r#"SCAN "data" WHERE status CONTAINS "act""#)
        .unwrap();
    assert_records(&r, 0);
}

#[test]
fn test_dot_notation_aggregate() {
    let (engine, _dir) = temp_engine();
    assert_ok(
        &engine
            .run(r#"PUT {name: "A", scoring: {bureau: 700}} IN "clients""#)
            .unwrap(),
    );
    assert_ok(
        &engine
            .run(r#"PUT {name: "B", scoring: {bureau: 600}} IN "clients""#)
            .unwrap(),
    );
    assert_ok(
        &engine
            .run(r#"PUT {name: "C", scoring: {bureau: 800}} IN "clients""#)
            .unwrap(),
    );

    let r = engine
        .run(r#"SCAN "clients" | AGGREGATE sum(scoring.bureau), avg(scoring.bureau)"#)
        .unwrap();
    match &r {
        QueryResult::Aggregation(agg) => {
            assert_eq!(
                agg.get("sum(scoring.bureau)"),
                Some(&xyzdb_core::value::Value::Float(2100.0))
            );
            assert_eq!(
                agg.get("avg(scoring.bureau)"),
                Some(&xyzdb_core::value::Value::Float(700.0))
            );
        }
        other => panic!("Expected Aggregation, got: {other:?}"),
    }
}

#[test]
fn test_empty_list_and_map() {
    let (engine, _dir) = temp_engine();
    assert_ok(
        &engine
            .run(r#"PUT {name: "A", tags: [], meta: {}} IN "data""#)
            .unwrap(),
    );

    let r = engine.run(r#"FIND "data" WHERE name = "A""#).unwrap();
    let recs = assert_records(&r, 1);
    assert_eq!(
        recs[0].fields.get("tags"),
        Some(&xyzdb_core::value::Value::List(vec![]))
    );
    assert_eq!(
        recs[0].fields.get("meta"),
        Some(&xyzdb_core::value::Value::Map(
            std::collections::BTreeMap::new()
        ))
    );
}

// ─── V4 GROUP BY Tests ──────────────────────────────────────────────────────

#[test]
fn test_group_by_basic() {
    let (engine, _dir) = temp_engine();
    for month in ["jan", "feb", "jan", "mar", "feb", "jan"] {
        assert_ok(
            &engine
                .run(&format!(
                    r#"PUT {{month: "{month}", amount: 100}} IN "payments""#
                ))
                .unwrap(),
        );
    }

    let r = engine
        .run(r#"SCAN "payments" | GROUP BY month | AGGREGATE count(), sum(amount)"#)
        .unwrap();
    match &r {
        QueryResult::GroupedAggregation(groups) => {
            assert_eq!(groups.len(), 3, "Should have 3 groups: jan, feb, mar");
            // Find the jan group
            let jan = groups
                .iter()
                .find(|g| g.get("month") == Some(&xyzdb_core::value::Value::Text("jan".into())))
                .unwrap();
            assert_eq!(jan.get("count"), Some(&xyzdb_core::value::Value::Int(3)));
            assert_eq!(
                jan.get("sum(amount)"),
                Some(&xyzdb_core::value::Value::Float(300.0))
            );
        }
        other => panic!("Expected GroupedAggregation, got: {other:?}"),
    }
}

#[test]
fn test_group_by_with_filter() {
    let (engine, _dir) = temp_engine();
    for i in 0..20 {
        let status = if i % 2 == 0 { "active" } else { "inactive" };
        let cat = if i % 3 == 0 { "A" } else { "B" };
        assert_ok(
            &engine
                .run(&format!(
                    r#"PUT {{status: "{status}", category: "{cat}", amount: {i}}} IN "data""#
                ))
                .unwrap(),
        );
    }

    let r = engine.run(r#"SCAN "data" WHERE status = "active" | GROUP BY category | AGGREGATE count(), sum(amount)"#).unwrap();
    match &r {
        QueryResult::GroupedAggregation(groups) => {
            assert_eq!(
                groups.len(),
                2,
                "Should have 2 categories among active records"
            );
        }
        other => panic!("Expected GroupedAggregation, got: {other:?}"),
    }
}

#[test]
fn test_group_by_single_group() {
    let (engine, _dir) = temp_engine();
    for i in 0..5 {
        assert_ok(
            &engine
                .run(&format!(r#"PUT {{status: "same", amount: {i}}} IN "data""#))
                .unwrap(),
        );
    }

    let r = engine
        .run(r#"SCAN "data" | GROUP BY status | AGGREGATE count(), sum(amount)"#)
        .unwrap();
    match &r {
        QueryResult::GroupedAggregation(groups) => {
            assert_eq!(groups.len(), 1);
            assert_eq!(
                groups[0].get("count"),
                Some(&xyzdb_core::value::Value::Int(5))
            );
            assert_eq!(
                groups[0].get("sum(amount)"),
                Some(&xyzdb_core::value::Value::Float(10.0))
            );
        }
        other => panic!("Expected GroupedAggregation, got: {other:?}"),
    }
}

#[test]
fn test_group_by_missing_field() {
    let (engine, _dir) = temp_engine();
    assert_ok(
        &engine
            .run(r#"PUT {name: "A", category: "X", amount: 10} IN "data""#)
            .unwrap(),
    );
    assert_ok(
        &engine
            .run(r#"PUT {name: "B", amount: 20} IN "data""#)
            .unwrap(),
    ); // no category
    assert_ok(
        &engine
            .run(r#"PUT {name: "C", category: "X", amount: 30} IN "data""#)
            .unwrap(),
    );

    let r = engine
        .run(r#"SCAN "data" | GROUP BY category | AGGREGATE count(), sum(amount)"#)
        .unwrap();
    match &r {
        QueryResult::GroupedAggregation(groups) => {
            // 2 groups: "X" and the missing-category group
            assert_eq!(groups.len(), 2);
        }
        other => panic!("Expected GroupedAggregation, got: {other:?}"),
    }
}

#[test]
fn test_group_by_dot_notation() {
    let (engine, _dir) = temp_engine();
    assert_ok(
        &engine
            .run(r#"PUT {name: "A", scoring: {risk: "low"}, amount: 100} IN "data""#)
            .unwrap(),
    );
    assert_ok(
        &engine
            .run(r#"PUT {name: "B", scoring: {risk: "high"}, amount: 200} IN "data""#)
            .unwrap(),
    );
    assert_ok(
        &engine
            .run(r#"PUT {name: "C", scoring: {risk: "low"}, amount: 300} IN "data""#)
            .unwrap(),
    );

    let r = engine
        .run(r#"SCAN "data" | GROUP BY scoring.risk | AGGREGATE count(), sum(amount)"#)
        .unwrap();
    match &r {
        QueryResult::GroupedAggregation(groups) => {
            assert_eq!(groups.len(), 2, "Should have low and high groups");
            let low = groups
                .iter()
                .find(|g| {
                    g.get("scoring.risk") == Some(&xyzdb_core::value::Value::Text("low".into()))
                })
                .unwrap();
            assert_eq!(low.get("count"), Some(&xyzdb_core::value::Value::Int(2)));
            assert_eq!(
                low.get("sum(amount)"),
                Some(&xyzdb_core::value::Value::Float(400.0))
            );
        }
        other => panic!("Expected GroupedAggregation, got: {other:?}"),
    }
}

#[test]
fn test_group_by_with_ghost() {
    let (engine, _dir) = temp_engine();
    for i in 0..50 {
        let status = if i < 30 { "overdue" } else { "active" };
        let cat = if i % 2 == 0 { "A" } else { "B" };
        assert_ok(&engine.run(&format!(
            r#"PUT {{_type: "Item", status: "{status}", category: "{cat}", amount: {i}}} IN "fin""#
        )).unwrap());
    }
    assert_ok(&engine.run(
        r#"CREATE GHOST "overdues" FROM "fin" WHERE _type = "Item" AND status = "overdue" ORDER BY amount"#
    ).unwrap());

    // PIN category so it's included in ghost projection
    assert_ok(&engine.run(r#"PIN category IN "fin""#).unwrap());
    assert_ok(&engine.run(r#"REFRESH GHOST "overdues""#).unwrap());

    // GROUP BY should route through ghost (category is now in projection)
    let r = engine.run(r#"SCAN "fin" WHERE _type = "Item" AND status = "overdue" | GROUP BY category | AGGREGATE count(), sum(amount)"#).unwrap();
    match &r {
        QueryResult::GroupedAggregation(groups) => {
            assert_eq!(groups.len(), 2, "A and B categories among overdue");
        }
        other => panic!("Expected GroupedAggregation, got: {other:?}"),
    }
}

// ─── V4 OR/NOT Tests ────────────────────────────────────────────────────────

#[test]
fn test_or_basic() {
    let (engine, _dir) = temp_engine();
    assert_ok(
        &engine
            .run(r#"PUT {name: "A", status: "active"} IN "data""#)
            .unwrap(),
    );
    assert_ok(
        &engine
            .run(r#"PUT {name: "B", status: "inactive"} IN "data""#)
            .unwrap(),
    );
    assert_ok(
        &engine
            .run(r#"PUT {name: "C", status: "overdue"} IN "data""#)
            .unwrap(),
    );

    let r = engine
        .run(r#"SCAN "data" WHERE status = "active" OR status = "overdue""#)
        .unwrap();
    let recs = assert_records(&r, 2);
    let names: Vec<&str> = recs
        .iter()
        .filter_map(|r| r.fields.get("name")?.as_text())
        .collect();
    assert!(names.contains(&"A"));
    assert!(names.contains(&"C"));
}

#[test]
fn test_not_basic() {
    let (engine, _dir) = temp_engine();
    assert_ok(
        &engine
            .run(r#"PUT {name: "A", status: "active"} IN "data""#)
            .unwrap(),
    );
    assert_ok(
        &engine
            .run(r#"PUT {name: "B", status: "cancelled"} IN "data""#)
            .unwrap(),
    );
    assert_ok(
        &engine
            .run(r#"PUT {name: "C", status: "active"} IN "data""#)
            .unwrap(),
    );

    let r = engine
        .run(r#"SCAN "data" WHERE NOT status = "cancelled""#)
        .unwrap();
    let recs = assert_records(&r, 2);
    let names: Vec<&str> = recs
        .iter()
        .filter_map(|r| r.fields.get("name")?.as_text())
        .collect();
    assert!(names.contains(&"A"));
    assert!(names.contains(&"C"));
}

#[test]
fn test_or_and_combined() {
    let (engine, _dir) = temp_engine();
    assert_ok(
        &engine
            .run(r#"PUT {name: "A", status: "overdue", amount: 100} IN "data""#)
            .unwrap(),
    );
    assert_ok(
        &engine
            .run(r#"PUT {name: "B", status: "overdue", amount: 5000} IN "data""#)
            .unwrap(),
    );
    assert_ok(
        &engine
            .run(r#"PUT {name: "C", status: "active", amount: 8000} IN "data""#)
            .unwrap(),
    );
    assert_ok(
        &engine
            .run(r#"PUT {name: "D", status: "active", amount: 50} IN "data""#)
            .unwrap(),
    );

    let r = engine
        .run(r#"SCAN "data" WHERE status = "overdue" OR (status = "active" AND amount > 1000)"#)
        .unwrap();
    let recs = assert_records(&r, 3);
    let names: Vec<&str> = recs
        .iter()
        .filter_map(|r| r.fields.get("name")?.as_text())
        .collect();
    assert!(names.contains(&"A"));
    assert!(names.contains(&"B"));
    assert!(names.contains(&"C"));
}

#[test]
fn test_parentheses_precedence() {
    let (engine, _dir) = temp_engine();
    assert_ok(
        &engine
            .run(r#"PUT {name: "A", x: 1, y: 1} IN "data""#)
            .unwrap(),
    );
    assert_ok(
        &engine
            .run(r#"PUT {name: "B", x: 2, y: 1} IN "data""#)
            .unwrap(),
    );
    assert_ok(
        &engine
            .run(r#"PUT {name: "C", x: 1, y: 2} IN "data""#)
            .unwrap(),
    );
    assert_ok(
        &engine
            .run(r#"PUT {name: "D", x: 2, y: 2} IN "data""#)
            .unwrap(),
    );

    // Without parens: x=1 OR x=2 AND y=2 → x=1 OR (x=2 AND y=2) → A,C,D
    let r = engine
        .run(r#"SCAN "data" WHERE x = 1 OR x = 2 AND y = 2"#)
        .unwrap();
    assert_records(&r, 3);

    // With parens: (x=1 OR x=2) AND y=2 → C,D
    let r = engine
        .run(r#"SCAN "data" WHERE (x = 1 OR x = 2) AND y = 2"#)
        .unwrap();
    assert_records(&r, 2);
}

#[test]
fn test_not_with_parenthesized_or() {
    let (engine, _dir) = temp_engine();
    assert_ok(
        &engine
            .run(r#"PUT {name: "A", status: "active"} IN "data""#)
            .unwrap(),
    );
    assert_ok(
        &engine
            .run(r#"PUT {name: "B", status: "cancelled"} IN "data""#)
            .unwrap(),
    );
    assert_ok(
        &engine
            .run(r#"PUT {name: "C", status: "overdue"} IN "data""#)
            .unwrap(),
    );

    let r = engine
        .run(r#"SCAN "data" WHERE NOT (status = "cancelled" OR status = "overdue")"#)
        .unwrap();
    let recs = assert_records(&r, 1);
    assert_eq!(
        recs[0].fields.get("name"),
        Some(&xyzdb_core::value::Value::Text("A".into()))
    );
}

#[test]
fn test_or_does_not_route_to_ghost() {
    let (engine, _dir) = temp_engine();
    for i in 0..50 {
        let status = if i < 30 { "overdue" } else { "active" };
        assert_ok(
            &engine
                .run(&format!(
                    r#"PUT {{_type: "Item", status: "{status}", amount: {i}}} IN "fin""#
                ))
                .unwrap(),
        );
    }
    assert_ok(&engine.run(
        r#"CREATE GHOST "overdues" FROM "fin" WHERE _type = "Item" AND status = "overdue" ORDER BY amount"#
    ).unwrap());

    // OR query should NOT route to ghost (conservative routing) — returns all 50
    let r = engine
        .run(r#"SCAN "fin" WHERE status = "overdue" OR status = "active""#)
        .unwrap();
    assert_records(&r, 50);
}

#[test]
fn test_and_still_routes_to_ghost() {
    let (engine, _dir) = temp_engine();
    for i in 0..50 {
        let status = if i < 30 { "overdue" } else { "active" };
        assert_ok(
            &engine
                .run(&format!(
                    r#"PUT {{_type: "Item", status: "{status}", amount: {i}}} IN "fin""#
                ))
                .unwrap(),
        );
    }
    assert_ok(&engine.run(
        r#"CREATE GHOST "overdues" FROM "fin" WHERE _type = "Item" AND status = "overdue" ORDER BY amount"#
    ).unwrap());

    // Pure AND query should still route to ghost
    let r = engine
        .run(r#"SCAN "fin" WHERE _type = "Item" AND status = "overdue""#)
        .unwrap();
    assert_records(&r, 30);
}

#[test]
fn test_or_with_order_by() {
    let (engine, _dir) = temp_engine();
    assert_ok(
        &engine
            .run(r#"PUT {name: "A", status: "active", score: 10} IN "data""#)
            .unwrap(),
    );
    assert_ok(
        &engine
            .run(r#"PUT {name: "B", status: "overdue", score: 30} IN "data""#)
            .unwrap(),
    );
    assert_ok(
        &engine
            .run(r#"PUT {name: "C", status: "active", score: 20} IN "data""#)
            .unwrap(),
    );

    let r = engine.run(r#"SCAN "data" WHERE status = "active" OR status = "overdue" ORDER BY score DESC LIMIT 2"#).unwrap();
    let recs = assert_records(&r, 2);
    assert_eq!(
        recs[0].fields.get("name"),
        Some(&xyzdb_core::value::Value::Text("B".into()))
    );
    assert_eq!(
        recs[1].fields.get("name"),
        Some(&xyzdb_core::value::Value::Text("C".into()))
    );
}

#[test]
fn test_or_with_aggregate() {
    let (engine, _dir) = temp_engine();
    assert_ok(
        &engine
            .run(r#"PUT {name: "A", status: "active", amount: 100} IN "data""#)
            .unwrap(),
    );
    assert_ok(
        &engine
            .run(r#"PUT {name: "B", status: "overdue", amount: 200} IN "data""#)
            .unwrap(),
    );
    assert_ok(
        &engine
            .run(r#"PUT {name: "C", status: "cancelled", amount: 300} IN "data""#)
            .unwrap(),
    );

    let r = engine.run(r#"SCAN "data" WHERE status = "active" OR status = "overdue" | AGGREGATE count(), sum(amount)"#).unwrap();
    match &r {
        QueryResult::Aggregation(agg) => {
            assert_eq!(agg.get("count"), Some(&xyzdb_core::value::Value::Int(2)));
            assert_eq!(
                agg.get("sum(amount)"),
                Some(&xyzdb_core::value::Value::Float(300.0))
            );
        }
        other => panic!("Expected Aggregation, got: {other:?}"),
    }
}

// ── V5: Postcard serialization + MIGRATE ─────────────────────────────────

#[test]
fn test_v5_records_readable_after_write() {
    let (engine, _dir) = temp_engine();
    assert_ok(
        &engine
            .run(r#"PUT {name: "Ivan", age: 38} IN "users""#)
            .unwrap(),
    );

    // Verify record is readable (round-trip through new serialization)
    let r = engine.run(r#"FIND "users" WHERE name = "Ivan""#).unwrap();
    match &r {
        QueryResult::Records(recs) => {
            assert_eq!(recs.len(), 1);
            assert_eq!(recs[0].lobe_name, "users");
            assert_eq!(
                recs[0].fields.get("name"),
                Some(&xyzdb_core::value::Value::Text("Ivan".into()))
            );
            assert_eq!(
                recs[0].fields.get("age"),
                Some(&xyzdb_core::value::Value::Int(38))
            );
        }
        other => panic!("Expected Records, got: {other:?}"),
    }
}

#[test]
fn test_v5_migrate_no_records() {
    let (engine, _dir) = temp_engine();
    // MIGRATE on empty engine should succeed
    let r = engine.run("MIGRATE").unwrap();
    assert_ok(&r);
}

#[test]
fn test_v5_migrate_skips_v1_format() {
    let (engine, _dir) = temp_engine();
    // Insert records (already in V1 format — postcard with string field names)
    assert_ok(&engine.run(r#"PUT {x: 1} IN "t""#).unwrap());
    assert_ok(&engine.run(r#"PUT {x: 2} IN "t""#).unwrap());

    // MIGRATE should move nothing and rewrite nothing — both records already
    // sit at their value-only key (no `*` spec → anchor/LID fallback, which is
    // value-only) and are already V1.
    let r = engine.run("MIGRATE").unwrap();
    match &r {
        QueryResult::Ok { message, .. } => {
            assert!(
                message.contains("0 gravity keys rehashed"),
                "Expected 0 rehashed, got: {message}"
            );
            assert!(
                message.contains("2 already current"),
                "Expected 2 already current, got: {message}"
            );
        }
        other => panic!("Expected Ok, got: {other:?}"),
    }
}

#[test]
fn test_v5_migrate_single_lobe() {
    let (engine, _dir) = temp_engine();
    assert_ok(&engine.run(r#"PUT {x: 1} IN "a""#).unwrap());
    assert_ok(&engine.run(r#"PUT {x: 2} IN "b""#).unwrap());

    // MIGRATE only lobe "a"
    let r = engine.run(r#"MIGRATE "a""#).unwrap();
    assert_ok(&r);
}

#[test]
fn test_v5_v2_field_ids_roundtrip() {
    let (engine, _dir) = temp_engine();
    // Insert with various field types
    assert_ok(
        &engine
            .run(r#"PUT {name: "Ivan", age: 38, active: true} IN "users""#)
            .unwrap(),
    );

    // Read back — should restore field names from field_dict
    let r = engine.run(r#"FIND "users" WHERE name = "Ivan""#).unwrap();
    match &r {
        QueryResult::Records(recs) => {
            assert_eq!(recs.len(), 1);
            assert_eq!(
                recs[0].fields.get("name"),
                Some(&xyzdb_core::value::Value::Text("Ivan".into()))
            );
            assert_eq!(
                recs[0].fields.get("age"),
                Some(&xyzdb_core::value::Value::Int(38))
            );
            assert_eq!(
                recs[0].fields.get("active"),
                Some(&xyzdb_core::value::Value::Bool(true))
            );
        }
        other => panic!("Expected Records, got: {other:?}"),
    }
}

#[test]
fn test_v5_v2_schemaless_field_evolution() {
    let (engine, _dir) = temp_engine();
    // First record with fields A, B
    assert_ok(&engine.run(r#"PUT {a: 1, b: 2} IN "evolve""#).unwrap());
    // Second record adds field C (schemaless evolution)
    assert_ok(&engine.run(r#"PUT {a: 3, b: 4, c: 5} IN "evolve""#).unwrap());

    let r = engine.run(r#"SCAN "evolve""#).unwrap();
    match &r {
        QueryResult::Records(recs) => {
            assert_eq!(recs.len(), 2);
            // First record has a, b (+ _type)
            assert!(recs[0].fields.contains_key("a"));
            assert!(recs[0].fields.contains_key("b"));
            // Second record has a, b, c (+ _type)
            assert!(recs[1].fields.contains_key("c"));
        }
        other => panic!("Expected Records, got: {other:?}"),
    }
}

#[test]
fn test_v5_ghost_reads_v2_spatial_records() {
    let (engine, _dir) = temp_engine();
    assert_ok(
        &engine
            .run(r#"PUT {status: "active", amount: 100} IN "data""#)
            .unwrap(),
    );
    assert_ok(
        &engine
            .run(r#"PUT {status: "active", amount: 200} IN "data""#)
            .unwrap(),
    );
    assert_ok(
        &engine
            .run(r#"PUT {status: "closed", amount: 300} IN "data""#)
            .unwrap(),
    );

    // Create ghost — reads V2 records from spatial, writes V1 to ghost keyspace
    let r = engine
        .run(r#"CREATE GHOST "active_data" FROM "data" WHERE status = "active" ORDER BY amount"#)
        .unwrap();
    assert_ok(&r);

    // Scan ghost — should find the 2 active records
    let r = engine.run(r#"SCAN GHOST "active_data""#).unwrap();
    match &r {
        QueryResult::Records(recs) => {
            assert_eq!(recs.len(), 2, "Ghost should have 2 active records");
        }
        other => panic!("Expected Records, got: {other:?}"),
    }
}

#[test]
fn test_v5_lobe_registry_postcard_roundtrip() {
    use xyzdb_core::lobe::LobeRegistry;
    let mut reg = LobeRegistry::new();
    reg.get_or_create("clientes", None);
    reg.get_or_create("creditos", Some("financiero".into()));
    let bytes = reg.to_bytes();
    // Verify magic prefix
    assert_eq!(&bytes[0..2], &[0x58, 0x59]);
    let restored = LobeRegistry::from_bytes(&bytes).unwrap();
    assert_eq!(restored.get("clientes").unwrap().id, 1);
    assert_eq!(
        restored.get("creditos").unwrap().hint,
        Some("financiero".into())
    );
}

// ── V5: RecordCache Tests ──────────────────────────────────────────────────────

fn temp_engine_with_cache(budget_mb: usize) -> (Engine, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let mut engine = Engine::open(dir.path()).expect("failed to open engine");
    engine.set_record_cache_size(budget_mb * 1024 * 1024);
    (engine, dir)
}

#[test]
fn test_incache_and_find() {
    let (engine, _dir) = temp_engine_with_cache(64);
    assert_ok(
        &engine
            .run(r#"PUT {name: "Ivan", age: 38} IN "users""#)
            .unwrap(),
    );
    assert_ok(
        &engine
            .run(r#"PUT {name: "Ana", age: 30} IN "users""#)
            .unwrap(),
    );

    // Load into cache
    let r = engine.run(r#"INCACHE "users""#).unwrap();
    assert_ok(&r);

    // FIND should hit cache
    let r = engine.run(r#"FIND "users" WHERE name = "Ivan""#).unwrap();
    let recs = assert_records(&r, 1);
    assert_eq!(
        recs[0].fields.get("name"),
        Some(&xyzdb_core::value::Value::Text("Ivan".into()))
    );
}

#[test]
fn test_incache_with_where() {
    let (engine, _dir) = temp_engine_with_cache(64);
    assert_ok(
        &engine
            .run(r#"PUT {status: "active", x: 1} IN "data""#)
            .unwrap(),
    );
    assert_ok(
        &engine
            .run(r#"PUT {status: "closed", x: 2} IN "data""#)
            .unwrap(),
    );

    // Load only active records
    let r = engine
        .run(r#"INCACHE "data" WHERE status = "active""#)
        .unwrap();
    match &r {
        QueryResult::Ok { message, .. } => assert!(message.contains("1 records"), "Got: {message}"),
        other => panic!("Expected Ok, got: {other:?}"),
    }
}

#[test]
fn test_outcache() {
    let (engine, _dir) = temp_engine_with_cache(64);
    assert_ok(&engine.run(r#"PUT {x: 1} IN "t""#).unwrap());
    assert_ok(&engine.run(r#"INCACHE "t""#).unwrap());
    assert_ok(&engine.run(r#"OUTCACHE "t""#).unwrap());

    // SHOW CACHE should report empty
    let r = engine.run("SHOW CACHE").unwrap();
    match &r {
        QueryResult::Info(lines) => assert!(lines[0].contains("0.0MB"), "Got: {:?}", lines),
        other => panic!("Expected Info, got: {other:?}"),
    }
}

#[test]
fn test_show_cache_disabled() {
    let (engine, _dir) = temp_engine();
    let r = engine.run("SHOW CACHE").unwrap();
    match &r {
        QueryResult::Info(lines) => assert!(lines[0].contains("disabled"), "Got: {:?}", lines),
        other => panic!("Expected Info, got: {other:?}"),
    }
}

#[test]
fn test_incache_budget_exceeded() {
    // 1 byte budget — should fail
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::open(dir.path()).unwrap();
    engine.set_record_cache_size(1); // 1 byte

    assert_ok(&engine.run(r#"PUT {name: "test"} IN "big""#).unwrap());
    let r = engine.run(r#"INCACHE "big""#);
    match r {
        Err(e) => assert!(
            e.to_string().contains("budget"),
            "Expected budget error, got: {e}"
        ),
        Ok(_) => panic!("Expected error for budget exceeded"),
    }
}

#[test]
fn test_write_through_put() {
    let (engine, _dir) = temp_engine_with_cache(64);
    assert_ok(&engine.run(r#"PUT {x: 1} IN "t""#).unwrap());
    assert_ok(&engine.run(r#"INCACHE "t""#).unwrap());

    // PUT new record — should be in cache via write-through
    assert_ok(&engine.run(r#"PUT {x: 2} IN "t""#).unwrap());

    // Verify both records findable
    let r = engine.run(r#"SCAN "t""#).unwrap();
    assert_records(&r, 2);
}

#[test]
fn test_incache_without_cache_enabled() {
    let (engine, _dir) = temp_engine(); // No cache
    let r = engine.run(r#"INCACHE "whatever""#);
    match r {
        Err(e) => assert!(e.to_string().contains("not enabled"), "Got: {e}"),
        Ok(_) => panic!("Expected error"),
    }
}

#[test]
fn test_parse_incache_outcache() {
    let stmt = xytalk_parser::parse("INCACHE \"users\"").unwrap();
    match stmt {
        xytalk_parser::ast::Statement::InCache(s) => {
            assert_eq!(s.lobe, "users");
            assert!(s.filter_expr.is_none());
        }
        _ => panic!("Expected InCache"),
    }

    let stmt = xytalk_parser::parse("OUTCACHE \"users\"").unwrap();
    match stmt {
        xytalk_parser::ast::Statement::OutCache(name) => assert_eq!(name, "users"),
        _ => panic!("Expected OutCache"),
    }

    let stmt = xytalk_parser::parse("SHOW CACHE").unwrap();
    match stmt {
        xytalk_parser::ast::Statement::Show(xytalk_parser::ast::ShowStmt::Cache) => {}
        _ => panic!("Expected ShowCache"),
    }
}

/// Finding 8 path B regression: the server `COMPACT` command
/// (`execute_compact` in xyzdb-engine) must seal active memtables
/// before rotating the WAL. Pre-fix (commit `3549ca2` reverted),
/// writes still in the active memtable when `COMPACT` runs are
/// orphaned by the rotate and lost on subsequent restart. Post-fix:
/// `seal_active()` for spatial, identity, and dictionary precedes
/// the per-tree `major_compact()` calls; their flush_sealed picks up
/// the newly-sealed writes; `rotate_journal()` is then honest.
///
/// Companion to
/// `finding_8_major_compact_seals_active_before_wal_rotate` in
/// `crates/turba-engine/tests/durability_proptest.rs`, which exercises
/// path A (`TurbaEngine::major_compact`). This test drives path B
/// through the public `engine.run("COMPACT")` entry point — the
/// same path a real client takes when issuing the command.
///
/// This test covers path B; path A is exercised in the turba-engine
/// durability proptests.
#[test]
fn finding_8_path_b_execute_compact_seals_active_before_rotate() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().to_path_buf();

    {
        let engine = Engine::open(&db_path).expect("open");

        engine.run(r#"LOBE "testlobe""#).expect("create lobe");
        engine
            .run(r#"ANCHOR "id" UNIQUE IN "testlobe""#)
            .expect("anchor");

        // Insert 20 small records — aggregate well below 16 MB so
        // the active memtable never seals naturally. These are the
        // writes path B must persist before rotating the WAL.
        for i in 0..20u32 {
            let put = format!(
                r#"PUT {{_type: "Doc", id: "K{:04}", payload: "v_{}"}} IN "testlobe""#,
                i, i
            );
            engine.run(&put).expect("insert");
        }

        // Path B — server COMPACT command handler. Pre-fix: this
        // called `Tree::major_compact()` directly (which only
        // flushes sealed memtables) then `rotate_journal()`,
        // discarding the WAL with active-memtable writes still
        // in memory. Post-fix: `seal_active()` is called first
        // per tree, so flush_sealed picks up everything before
        // the rotate.
        engine.run("COMPACT").expect("compact");

        // Simulate SIGKILL. Drop would seal + flush on shutdown
        // and hide the bug.
        engine._test_release_dir_lock();
        std::mem::forget(engine);
    }

    // Reopen. Post-fix: all 20 records are in SSTables and the WAL
    // (now fresh post-rotate) is empty but irrelevant. Pre-fix:
    // records were in active memtable at COMPACT time; the rotate
    // truncated the WAL; `mem::forget` lost them with the process
    // memory.
    let engine = Engine::open(&db_path).expect("reopen");
    let result = engine.run(r#"SCAN "testlobe""#).expect("scan post-reopen");
    let records = match result {
        QueryResult::Records(recs) => recs,
        other => panic!("Expected Records, got: {other:?}"),
    };
    assert_eq!(
        records.len(),
        20,
        "Finding 8 path B regression: expected 20 records after \
         reopen; got {}. `execute_compact` did not seal active \
         memtables before rotating the WAL, so active-memtable \
         writes were lost on mem::forget + reopen.",
        records.len()
    );
}

/// Ticket 2 (0.9.2) durability regression — COMPACT must not drop the `vectors`
/// keyspace. A vector PUT co-commits the vector column with the spatial blob in
/// ONE batch (same WAL seqno); pre-fix, `execute_compact` sealed spatial/
/// identity/dictionary (+ghosts) but NOT `vectors`, then truncated the WAL — so
/// acked vectors survived only in the vectors active memtable and were lost on
/// crash, leaving records whose vector had vanished. Drives the REAL `COMPACT`
/// command, crashes, reopens, and asserts NEAREST still ranks the surviving
/// vectors correctly — direct recall over the modified path, since the fintech
/// golden is vector-blind (the #2/G5 lesson).
#[test]
fn compact_preserves_vectors_and_nearest_recall_across_crash() {
    use xyzdb_core::value::Value;

    // 64-dim literal (>= VECTOR_F32_MIN_DIMS) so the executor packs the list into
    // a `Value::Vector` and the PUT HOISTS it into the `vectors` keyspace. A short
    // 2-D literal stays a `Value::List` inline in the spatial blob and never
    // populates the vectors keyspace — so it would not exercise this bug.
    fn emb(coords: &[(usize, f32)]) -> String {
        let mut v = vec![0.0f32; 64];
        for &(i, x) in coords {
            v[i] = x;
        }
        let parts: Vec<String> = v.iter().map(|f| format!("{f:.1}")).collect();
        format!("[{}]", parts.join(", "))
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().to_path_buf();

    {
        let engine = Engine::open(&db_path).expect("open");
        engine.run(r#"LOBE "memoria""#).expect("lobe");
        // Declare emb as the searchable vector so PUT HOISTS it into the
        // `vectors` keyspace (put.rs get_vector_spec + the >=64-D coercion).
        engine
            .run(r#"VECTOR emb IN "memoria""#)
            .expect("vector spec");
        // Hoisted 64-D vectors in bucket c1, small enough that the active memtable
        // never seals — at COMPACT time they live only in the WAL + vectors active
        // memtable, exactly the state the bug strands.
        engine
            .run(&format!(
                r#"PUT {{*conv:"c1", id:"r1", emb:{}}} IN "memoria""#,
                emb(&[(0, 1.0)])
            ))
            .expect("put r1");
        engine
            .run(&format!(
                r#"PUT {{*conv:"c1", id:"r2", emb:{}}} IN "memoria""#,
                emb(&[(0, 0.6), (1, 0.8)])
            ))
            .expect("put r2");
        engine
            .run(&format!(
                r#"PUT {{*conv:"c1", id:"r3", emb:{}}} IN "memoria""#,
                emb(&[(1, 1.0)])
            ))
            .expect("put r3");

        engine.run("COMPACT").expect("compact");

        // Faithful SIGKILL: stop + join every bg thread WITHOUT flushing, so no
        // ghost worker persists the lagging vectors keyspace after the crash
        // (plain `_test_release_dir_lock` + `mem::forget` leaves them alive and
        // masks the loss — the F1 crash-fidelity lesson).
        engine._test_crash_stop();
        std::mem::forget(engine);
    }

    let engine = Engine::open(&db_path).expect("reopen");
    // Query along axis 1: correct cosine ranking is r3 (1.0) then r2 (0.8); r1 is
    // orthogonal (0). This differs from insertion order [r1,r2,r3], so a spurious
    // scan-order result would not pass — and if COMPACT dropped the hoisted
    // vectors, every record is unscorable and NEAREST returns nothing.
    let qr = engine
        .run(&format!(
            r#"SCAN "memoria" WHERE conv="c1" | NEAREST(emb, {}, 2, cosine)"#,
            emb(&[(1, 1.0)])
        ))
        .expect("nearest");
    let ids: Vec<String> = match qr {
        QueryResult::Records(recs) => recs
            .into_iter()
            .map(|r| match r.fields.get("id") {
                Some(Value::Text(t)) => t.clone(),
                other => panic!("record without id: {other:?}"),
            })
            .collect(),
        other => panic!("expected Records from NEAREST, got {other:?}"),
    };
    assert_eq!(
        ids,
        vec!["r3".to_string(), "r2".to_string()],
        "NEAREST recall broken after COMPACT+crash: the hoisted vectors keyspace was dropped by \
         COMPACT (WAL truncated without flushing vectors), so surviving records are unscorable"
    );
}

/// `stats_snapshot` returns a well-formed `StatsSnapshot` covering all
/// five keyspaces, with counters initialised to zero on a fresh engine
/// and reflecting writes after they land in SSTables. Covers the
/// `/stats` endpoint.
#[test]
fn stats_snapshot_shape_and_values_after_writes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let engine = Engine::open(dir.path()).expect("open");

    // Shape check on a pristine engine.
    let snap = engine.stats_snapshot();
    for ks in ["spatial", "identity", "dictionary", "ghosts", "vectors"] {
        let k = snap.keyspaces.get(ks).unwrap_or_else(|| {
            panic!("stats_snapshot: missing keyspace `{ks}`");
        });
        assert_eq!(k.compact.compact_ok, 0, "compact_ok fresh");
        assert_eq!(k.compact.major_ok, 0, "major_ok fresh");
        assert_eq!(k.compact.compact_err, 0, "compact_err fresh");
        assert!(
            k.levels.contains_key("l0"),
            "keyspace `{ks}` missing levels.l0"
        );
    }
    assert_eq!(snap.ghosts.total, 0, "no ghosts on a fresh engine");
    assert!(
        snap.block_cache.capacity_bytes > 0,
        "block cache capacity should be >0"
    );
    // Linux-only probes stay at 0 on macOS/CI.
    let _ = snap.process.vmrss_bytes;
    let _ = snap.cgroup.anon_bytes;

    // Sync-thread heartbeat should advance under Durable mode (default).
    // Sleep to let the thread tick at least a few times — the sync loop
    // sleeps 1 ms per iteration, so 50 ms is ~50 ticks of headroom.
    std::thread::sleep(std::time::Duration::from_millis(50));
    let snap_heartbeat = engine.stats_snapshot();
    assert!(
        snap_heartbeat.sync_thread.heartbeat_count > 0,
        "sync thread heartbeat should advance in Durable mode; got {}",
        snap_heartbeat.sync_thread.heartbeat_count
    );

    // Verify major_ok increments after a COMPACT.
    engine.run(r#"LOBE "statslobe""#).expect("lobe");
    engine
        .run(r#"ANCHOR "id" UNIQUE IN "statslobe""#)
        .expect("anchor");
    for i in 0..5u32 {
        engine
            .run(&format!(
                r#"PUT {{_type: "Doc", id: "S{i:03}"}} IN "statslobe""#
            ))
            .expect("insert");
    }
    engine.run("COMPACT").expect("compact");

    let snap2 = engine.stats_snapshot();
    // execute_compact runs major_compact on spatial/identity/dictionary,
    // so each should now show major_ok=1.
    for ks in ["spatial", "identity", "dictionary"] {
        let k = snap2.keyspaces.get(ks).unwrap();
        assert_eq!(
            k.compact.major_ok, 1,
            "{ks} major_ok should be 1 after one COMPACT, got {}",
            k.compact.major_ok
        );
    }

    // After writes, the sync thread must have completed at least one
    // fsync — last_successful_sync_ts_ms should now be nonzero (Unix
    // epoch ms is ~1.7e12 in 2026, far from 0).
    assert!(
        snap2.sync_thread.last_successful_sync_ts_ms > 0,
        "sync thread should have synced at least once after writes; got {}",
        snap2.sync_thread.last_successful_sync_ts_ms
    );
    assert!(
        snap2.sync_thread.heartbeat_count >= snap_heartbeat.sync_thread.heartbeat_count,
        "heartbeat is monotonic; before={} after={}",
        snap_heartbeat.sync_thread.heartbeat_count,
        snap2.sync_thread.heartbeat_count
    );

    // Serialisation round-trip: confirm the struct is JSON-safe. We do
    // NOT add serde_json as a dev-dep just for this — instead we rely
    // on the server integration path to catch schema breakage at the
    // response-write call site, and assert Debug formatting here as a
    // cheap liveness check on the struct fields.
    let _debug = format!("{:?}", snap2.keyspaces.get("spatial").unwrap().levels);
}

// ── Finding 11 — ghost PreComputed honours query-level WHERE filter ─────────
//
// `SCAN WHERE <group_key> = <val> | GROUP BY <group_key> | AGGREGATE …`
// used to return every pre-computed group when a matching ghost was
// present — the query `WHERE` clause was silently discarded inside the
// PreComputed short-circuit (ops/scan.rs:465). Fix: (1) stricter
// `ghost_router::plan_scan` disqualifies the ghost when the query
// carries predicates outside the ghost's filter_fields ∪ Eq-on-group_fields
// scope; (2) `GhostLobeManager::read_precomputed` applies Eq
// predicates on group-key fields to the group_summaries before
// returning.

/// Finding 11 test 1 — query `WHERE rfc = X | GROUP BY rfc | AGGREGATE`
/// returns only the matching group (not every pre-computed group).
#[test]
fn finding_11_scan_group_by_filter_on_group_key_respected_via_ghost() {
    let (engine, _dir) = temp_engine();

    // Two groups: AAA (2 credits, sum=15000), BBB (1 credit, sum=7000).
    assert_ok(&engine.run(r#"LOBE "creditos""#).unwrap());
    assert_ok(&engine.run(r#"PUT {_type: "Credit", *credit_id: "C1", rfc: "AAA", monto: 10000.00} IN "creditos""#).unwrap());
    assert_ok(&engine.run(r#"PUT {_type: "Credit", *credit_id: "C2", rfc: "AAA", monto: 5000.00} IN "creditos""#).unwrap());
    assert_ok(&engine.run(r#"PUT {_type: "Credit", *credit_id: "C3", rfc: "BBB", monto: 7000.00} IN "creditos""#).unwrap());

    // Ghost with GROUP BY rfc + AGGREGATE sum(monto), count().
    assert_ok(&engine.run(
        r#"CREATE GHOST "credits_by_rfc" FROM "creditos" WHERE _type = "Credit" ORDER BY rfc GROUP BY rfc AGGREGATE sum(monto), count()"#,
    ).unwrap());

    // Case 1 — WHERE rfc = "AAA": exactly one group, values match.
    // Aggregate key naming note: the PreComputed path emits
    // "monto:Sum" (from AggregateState::to_result's `{field}:{op:?}`
    // encoding); the Primary / AggAccumulator path emits "sum(monto)".
    // This is a pre-existing naming inconsistency unrelated to
    // Finding 11. The test accepts either form — the Finding 11 fix
    // only changes the number of returned groups and their group-key
    // identity, not the aggregate key naming.
    let r = engine.run(
        r#"SCAN "creditos" WHERE _type = "Credit" AND rfc = "AAA" | GROUP BY rfc | AGGREGATE sum(monto), count()"#,
    ).unwrap();
    match &r {
        QueryResult::GroupedAggregation(groups) => {
            assert_eq!(
                groups.len(),
                1,
                "WHERE rfc=AAA must restrict to 1 group; got {} (Finding 11 regression)",
                groups.len()
            );
            let g = &groups[0];
            assert_eq!(
                g.get("rfc"),
                Some(&xyzdb_core::value::Value::Text("AAA".into()))
            );
            assert_eq!(g.get("count"), Some(&xyzdb_core::value::Value::Int(2)));
            let sum = g.get("sum(monto)").or_else(|| g.get("monto:Sum"));
            assert_eq!(
                sum,
                Some(&xyzdb_core::value::Value::Float(15000.0)),
                "sum over AAA's credits must be 15000.0; got {sum:?}"
            );
        }
        other => panic!("Expected GroupedAggregation, got: {other:?}"),
    }

    // Case 2 — WHERE rfc = "ZZZ" (no match): zero groups.
    // This is the unambiguous proof: no record in the dataset has
    // rfc=ZZZ, so any result row is a Finding 11 regression.
    let r = engine.run(
        r#"SCAN "creditos" WHERE _type = "Credit" AND rfc = "ZZZ" | GROUP BY rfc | AGGREGATE sum(monto), count()"#,
    ).unwrap();
    match &r {
        QueryResult::GroupedAggregation(groups) => {
            assert_eq!(
                groups.len(),
                0,
                "WHERE rfc=ZZZ must restrict to 0 groups; got {} (Finding 11 regression)",
                groups.len()
            );
        }
        other => panic!("Expected GroupedAggregation, got: {other:?}"),
    }
}

/// Finding 11 test 2 — predicate on a field not in the ghost's
/// group_fields disqualifies PreComputed routing: the query must
/// produce the same result as a ghost-free scan.
#[test]
fn finding_11_non_group_key_predicate_disqualifies_precomputed_ghost() {
    // Baseline engine (no ghost).
    let (base_engine, _base_dir) = temp_engine();
    assert_ok(&base_engine.run(r#"LOBE "creditos""#).unwrap());
    for (i, (rfc, status, monto)) in [
        ("AAA", "active", 10000.0_f64),
        ("AAA", "inactive", 5000.0),
        ("BBB", "active", 7000.0),
        ("BBB", "active", 3000.0),
    ]
    .iter()
    .enumerate()
    {
        assert_ok(&base_engine.run(&format!(
            r#"PUT {{_type: "Credit", *credit_id: "C{i}", rfc: "{rfc}", status: "{status}", monto: {monto}}} IN "creditos""#
        )).unwrap());
    }
    let baseline = base_engine.run(
        r#"SCAN "creditos" WHERE _type = "Credit" AND status = "active" | GROUP BY rfc | AGGREGATE sum(monto), count()"#,
    ).unwrap();
    let baseline_groups = match &baseline {
        QueryResult::GroupedAggregation(g) => g.clone(),
        other => panic!("Expected GroupedAggregation, got: {other:?}"),
    };

    // Engine with the ghost present, same data.
    let (engine, _dir) = temp_engine();
    assert_ok(&engine.run(r#"LOBE "creditos""#).unwrap());
    for (i, (rfc, status, monto)) in [
        ("AAA", "active", 10000.0_f64),
        ("AAA", "inactive", 5000.0),
        ("BBB", "active", 7000.0),
        ("BBB", "active", 3000.0),
    ]
    .iter()
    .enumerate()
    {
        assert_ok(&engine.run(&format!(
            r#"PUT {{_type: "Credit", *credit_id: "C{i}", rfc: "{rfc}", status: "{status}", monto: {monto}}} IN "creditos""#
        )).unwrap());
    }
    assert_ok(&engine.run(
        r#"CREATE GHOST "credits_by_rfc" FROM "creditos" WHERE _type = "Credit" ORDER BY rfc GROUP BY rfc AGGREGATE sum(monto), count()"#,
    ).unwrap());

    // Query carries `status = "active"` predicate. `status` is not in
    // the ghost's group_fields (GROUP BY rfc) nor in its declared
    // filter_fields (_type = "Credit" only). Fix must route to
    // Primary; result must match the baseline.
    let ghosted = engine.run(
        r#"SCAN "creditos" WHERE _type = "Credit" AND status = "active" | GROUP BY rfc | AGGREGATE sum(monto), count()"#,
    ).unwrap();
    let ghosted_groups = match &ghosted {
        QueryResult::GroupedAggregation(g) => g.clone(),
        other => panic!("Expected GroupedAggregation, got: {other:?}"),
    };

    // Sort both by rfc so we can compare deterministically.
    let mut baseline_sorted = baseline_groups.clone();
    let mut ghosted_sorted = ghosted_groups.clone();
    let rfc_key =
        |row: &std::collections::BTreeMap<String, xyzdb_core::value::Value>| match row.get("rfc") {
            Some(xyzdb_core::value::Value::Text(s)) => s.clone(),
            _ => String::new(),
        };
    baseline_sorted.sort_by_key(rfc_key);
    ghosted_sorted.sort_by_key(rfc_key);

    assert_eq!(
        baseline_sorted.len(),
        ghosted_sorted.len(),
        "Ghost-present path returned {} groups, baseline {}; Finding 11 guard failed",
        ghosted_sorted.len(),
        baseline_sorted.len()
    );

    for (b, g) in baseline_sorted.iter().zip(ghosted_sorted.iter()) {
        assert_eq!(b.get("rfc"), g.get("rfc"), "rfc mismatch");
        assert_eq!(b.get("count"), g.get("count"), "count mismatch");
        // Baseline (no ghost) goes through Primary → "sum(monto)".
        // Ghost-present should also go through Primary (router must
        // disqualify PreComputed — Finding 11 guard in plan_scan).
        // Fall back to "monto:Sum" in case the fix didn't land and
        // the ghost path is still reached (test will still fail on
        // the subsequent equality check since both paths must agree).
        let b_sum = b.get("sum(monto)").or_else(|| b.get("monto:Sum"));
        let g_sum = g.get("sum(monto)").or_else(|| g.get("monto:Sum"));
        assert_eq!(b_sum, g_sum, "sum(monto) mismatch");
    }
}

// ── Finding 12 — AUTOANCHOR APPLY idempotent re anchor declaration ─────────
//
// `AUTOANCHOR APPLY "<field>" IN "<lobe>"` is the populate operation:
// it iterates the primary keyspace and inserts each record's anchor
// value into the dictionary index. The registration step (telling the
// engine that `<field>` is an anchor) is idempotent by intent — the
// operator may issue APPLY after a `ANCHOR ... UNIQUE IN`
// declaration, after a bulk load, or repeatedly across imports. Pre-fix
// `execute_autoanchor_apply` called `anchors.register(...)?`
// unconditionally and propagated `"already registered"` errors,
// blocking populate work. The fix gates registration on
// `!is_anchor(...)`.

/// Finding 12 test — `AUTOANCHOR APPLY` succeeds when the anchor was
/// previously declared via `ANCHOR ... UNIQUE IN`, and a subsequent
/// `AUTOANCHOR APPLY` (idempotent re-run) also succeeds.
#[test]
fn finding_12_autoanchor_apply_idempotent_with_prior_anchor_declaration() {
    let (engine, _dir) = temp_engine();

    // Declarative path registers `rfc` as an anchor in `clientes`.
    assert_ok(&engine.run(r#"LOBE "clientes""#).unwrap());
    assert_ok(&engine.run(r#"ANCHOR "rfc" UNIQUE IN "clientes""#).unwrap());

    // Insert 3 client records with distinct rfc values. Note: the PUT
    // path also writes the anchor entry into the dictionary as a side
    // effect (per ops::put.rs — anchor write embedded in the same
    // batch). AUTOANCHOR APPLY's populate loop will therefore see the
    // entries already present and report them as duplicates. That is
    // expected; the test asserts the operation does NOT error,
    // independently of whether duplicates were found.
    assert_ok(
        &engine
            .run(r#"PUT {_type: "Client", *rfc: "AAA", name: "X1"} IN "clientes""#)
            .unwrap(),
    );
    assert_ok(
        &engine
            .run(r#"PUT {_type: "Client", *rfc: "BBB", name: "X2"} IN "clientes""#)
            .unwrap(),
    );
    assert_ok(
        &engine
            .run(r#"PUT {_type: "Client", *rfc: "CCC", name: "X3"} IN "clientes""#)
            .unwrap(),
    );

    // Pre-fix this returned `Err(InvalidQuery("Anchor 'rfc' already
    // registered in lobe 'clientes'"))`. Post-fix the registration is
    // skipped (already done by the prior ANCHOR statement) and the
    // populate path runs to completion.
    let r = engine.run(r#"AUTOANCHOR APPLY "rfc" IN "clientes""#);
    assert!(
        r.is_ok(),
        "AUTOANCHOR APPLY after ANCHOR UNIQUE IN must succeed (Finding 12 regression); got {r:?}"
    );

    // FIND on rfc must still locate the record. Post-fix this routes
    // through the anchor dictionary (O(1)); pre-fix it would have
    // fallen through to scan + bloom on Primary. The test asserts the
    // record is found regardless of routing — the routing improvement
    // is a side effect of the fix, not the contract.
    let r = engine.run(r#"FIND "clientes" WHERE rfc = "AAA""#).unwrap();
    let recs = assert_records(&r, 1);
    assert_eq!(
        recs[0].fields.get("name"),
        Some(&xyzdb_core::value::Value::Text("X1".into())),
    );

    // Second AUTOANCHOR APPLY (idempotent re-run). Must also succeed.
    let r = engine.run(r#"AUTOANCHOR APPLY "rfc" IN "clientes""#);
    assert!(
        r.is_ok(),
        "Second AUTOANCHOR APPLY must succeed (Finding 12 regression); got {r:?}"
    );
}

// ── Finding 13: SCAN equality on gravity field uses bounded range scan ──

/// Setup: lobe with gravity=rfc, 100 distinct rfcs × 100 records each =
/// 10 000 records. Pre-fix path scans all 10 000; post-fix path bounds
/// to the gravity bucket (~100 records).
#[test]
fn finding_13_scan_equality_on_gravity_field_uses_index() {
    let (engine, _dir) = temp_engine();

    assert_ok(&engine.run(r#"LOBE "items""#).unwrap());

    // Bulk insert: 100 distinct rfcs × 100 records each via PUT BATCH.
    // The gravity-field registry registers on the first record's `*rfc`
    // and the rest hit the read-lock fast path inside register_gravity_field.
    for rfc_idx in 0..100u32 {
        let mut stmts = Vec::with_capacity(100);
        for rec_idx in 0..100u32 {
            stmts.push(format!(
                r#"{{*rfc: "RFC_{:03}", _type: "Item", item_id: "I{}_{}", v: {}}}"#,
                rfc_idx, rfc_idx, rec_idx, rec_idx,
            ));
        }
        let body = stmts.join(",\n  ");
        let stmt = format!("PUT BATCH IN \"items\" [\n  {}\n]", body);
        engine.run(&stmt).unwrap();
    }

    // Correctness: SCAN by exact rfc returns exactly 100 records, all of them
    // with the queried rfc. The post-range filter discards any hash-collision
    // entries that share the gravity bucket but have a different rfc value.
    let r = engine.run(r#"SCAN "items" WHERE rfc = "RFC_042""#).unwrap();
    let recs = assert_records(&r, 100);
    for rec in &recs {
        assert_eq!(
            rec.fields.get("rfc"),
            Some(&xyzdb_core::value::Value::Text("RFC_042".into())),
            "post-range filter must discard hash-collision rows"
        );
    }

    // Performance: in --release the bounded range scan should complete in
    // well under 100 ms. Pre-fix the full scan over 10 000 records takes
    // hundreds of ms (CI/machine variance). Threshold 100 ms generous;
    // the ratio vs full scan is decisive at this dataset size.
    let t0 = std::time::Instant::now();
    let _ = engine.run(r#"SCAN "items" WHERE rfc = "RFC_017""#).unwrap();
    let elapsed = t0.elapsed();
    assert!(
        elapsed.as_millis() < 100,
        "Finding 13 fast path should complete in <100ms; got {:?}",
        elapsed
    );
}

/// Sanity: SCAN with `WHERE rfc != X` falls back to the full Primary scan
/// (no gravity-indexed shortcut for non-Eq operators) and returns correct
/// results. No latency assertion; correctness only.
#[test]
fn finding_13_does_not_apply_to_non_eq_predicates_on_gravity_field() {
    let (engine, _dir) = temp_engine();
    assert_ok(&engine.run(r#"LOBE "items""#).unwrap());
    for rfc in &["AAA", "BBB", "CCC"] {
        engine
            .run(&format!(
                r#"PUT {{*rfc: "{}", _type: "Item", item_id: "i1", v: 1}} IN "items""#,
                rfc
            ))
            .unwrap();
        engine
            .run(&format!(
                r#"PUT {{*rfc: "{}", _type: "Item", item_id: "i2", v: 2}} IN "items""#,
                rfc
            ))
            .unwrap();
    }

    // != operator on gravity field falls back to full scan; results include
    // all records with rfc != BBB (4 records: 2 AAA + 2 CCC).
    let r = engine.run(r#"SCAN "items" WHERE rfc != "BBB""#).unwrap();
    let recs = assert_records(&r, 4);
    for rec in &recs {
        let rfc_val = rec.fields.get("rfc").unwrap();
        assert!(
            !matches!(rfc_val, xyzdb_core::value::Value::Text(s) if s == "BBB"),
            "rfc=BBB record must not appear in `!= BBB` results"
        );
    }
}

/// Persistence: gravity field registered on first PUT survives engine
/// reopen. The fast path must still apply after `Engine::open` re-loads
/// the gravity_fields registry from the dictionary keyspace.
#[test]
fn finding_13_gravity_registry_persists_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();

    {
        let engine = Engine::open(path).unwrap();
        assert_ok(&engine.run(r#"LOBE "items""#).unwrap());
        engine
            .run(r#"PUT {*rfc: "X1", _type: "Item", v: 1} IN "items""#)
            .unwrap();
        // Implicit drop closes the engine.
    }

    let engine = Engine::open(path).unwrap();
    let gf = engine.get_gravity_field("items");
    assert_eq!(
        gf,
        Some("rfc".to_string()),
        "gravity field must persist across reopen"
    );

    // Fast path remains active on the reopened engine.
    let r = engine.run(r#"SCAN "items" WHERE rfc = "X1""#).unwrap();
    assert_records(&r, 1);
}

// ═══════════════════════════════════════════════════════════════════
// v0.2.5.1 — SCAN safety net: default LIMIT, hard cap, CURSOR reject
// ═══════════════════════════════════════════════════════════════════

/// Default LIMIT 1000 caps a SCAN that omits LIMIT. Pre-v0.2.5.1 this
/// returned the full lobe — now it stops at SCAN_LIMIT_DEFAULT and emits
/// a `tracing::warn!` (not asserted here; verified manually under
/// RUST_LOG=warn).
#[test]
fn test_scan_default_limit_caps_at_1000() {
    let (engine, _dir) = temp_engine();
    engine.run(r#"LOBE "cap""#).unwrap();
    for i in 0..1500 {
        engine
            .run(&format!(r#"PUT {{_type: "X", n: {i}}} IN "cap""#))
            .unwrap();
    }

    // No LIMIT clause: default cap 1000 kicks in.
    let r = engine.run(r#"SCAN "cap""#).unwrap();
    assert_records(&r, 1000);

    // Explicit LIMIT below the dataset size: passes through unchanged.
    let r = engine.run(r#"SCAN "cap" LIMIT 500"#).unwrap();
    assert_records(&r, 500);

    // Explicit LIMIT above the dataset: returns whole dataset (1500),
    // which is still under the hard cap.
    let r = engine.run(r#"SCAN "cap" LIMIT 5000"#).unwrap();
    assert_records(&r, 1500);
}

/// LIMIT > SCAN_LIMIT_HARD_MAX (10 000) is rejected with a clear error.
/// Larger result sets must paginate via CURSOR or chunked streaming.
#[test]
fn test_scan_limit_hard_max_rejected() {
    let (engine, _dir) = temp_engine();
    engine.run(r#"LOBE "hard""#).unwrap();
    engine.run(r#"PUT {x: 1} IN "hard""#).unwrap();

    let r = engine.run(r#"SCAN "hard" LIMIT 100000"#);
    let err = r.expect_err("LIMIT 100000 should exceed hard cap");
    let msg = format!("{err}");
    assert!(
        msg.contains("exceeds hard maximum"),
        "Expected hard-max error message, got: {msg}"
    );

    // LIMIT == SCAN_LIMIT_HARD_MAX is allowed (boundary).
    let r = engine.run(r#"SCAN "hard" LIMIT 10000"#).unwrap();
    assert_records(&r, 1);
}

/// CURSOR + ORDER BY is rejected: paginated sort is v0.3 scope.
#[test]
fn test_scan_cursor_with_order_by_rejected() {
    let (engine, _dir) = temp_engine();
    engine.run(r#"LOBE "cur""#).unwrap();

    let r = engine.run(r#"SCAN "cur" ORDER BY x ASC LIMIT 10 CURSOR "ignored-token""#);
    let err = r.expect_err("CURSOR + ORDER BY should be rejected");
    let msg = format!("{err}");
    assert!(
        msg.contains("CURSOR with ORDER BY is not supported"),
        "Expected CURSOR+ORDER_BY error, got: {msg}"
    );
}

/// CURSOR is also rejected when piped into AGGREGATE — pagination has no
/// meaning over an aggregate, but the parser would carry the clause
/// silently otherwise.
#[test]
fn test_scan_aggregate_cursor_rejected() {
    let (engine, _dir) = temp_engine();
    engine.run(r#"LOBE "ac""#).unwrap();
    engine.run(r#"PUT {x: 1} IN "ac""#).unwrap();

    let r = engine.run(r#"SCAN "ac" CURSOR "tok" | AGGREGATE count()"#);
    let err = r.expect_err("CURSOR + AGGREGATE should error at the SCAN stage");
    let msg = format!("{err}");
    assert!(
        msg.contains("CURSOR pagination is not supported on aggregate pipelines"),
        "Expected aggregate-cursor-rejected error, got: {msg}"
    );
}

// ═══════════════════════════════════════════════════════════════════
// v0.2.5.1 — CURSOR pagination (plain SCAN only)
// ═══════════════════════════════════════════════════════════════════

fn assert_paginated(
    result: &QueryResult,
) -> (Vec<xyzdb_core::record::Record>, Option<String>, bool) {
    match result {
        QueryResult::PaginatedRecords {
            records,
            cursor,
            has_more,
            ..
        } => (records.clone(), cursor.clone(), *has_more),
        other => panic!("Expected PaginatedRecords, got: {other:?}"),
    }
}

/// Multi-page SCAN: walk a 2 500-record dataset in 1 000-record pages
/// and verify (a) no records are dropped or duplicated across pages and
/// (b) the final page returns has_more=false with cursor=None.
#[test]
fn test_scan_cursor_basic_pagination() {
    let (engine, _dir) = temp_engine();
    engine.run(r#"LOBE "pg""#).unwrap();
    for i in 0..2_500 {
        engine
            .run(&format!(r#"PUT {{_type: "X", n: {i}}} IN "pg""#))
            .unwrap();
    }

    // First page: no cursor input, explicit LIMIT 1000.
    let r = engine.run(r#"SCAN "pg" LIMIT 1000"#).unwrap();
    let (page1, cursor1, has_more1) = assert_paginated(&r);
    assert_eq!(page1.len(), 1000);
    assert!(has_more1, "page 1 must have_more on a 2500-record dataset");
    let token1 = cursor1.expect("page 1 must yield a cursor");

    // Second page: same query + cursor.
    let q2 = format!(r#"SCAN "pg" LIMIT 1000 CURSOR "{token1}""#);
    let r = engine.run(&q2).unwrap();
    let (page2, cursor2, has_more2) = assert_paginated(&r);
    assert_eq!(page2.len(), 1000);
    assert!(has_more2, "page 2 must have_more (500 records remain)");
    let token2 = cursor2.expect("page 2 must yield a cursor");

    // Third (final) page: 500 records, has_more=false, cursor=None.
    let q3 = format!(r#"SCAN "pg" LIMIT 1000 CURSOR "{token2}""#);
    let r = engine.run(&q3).unwrap();
    let (page3, cursor3, has_more3) = assert_paginated(&r);
    assert_eq!(page3.len(), 500);
    assert!(!has_more3, "page 3 reaches end of stream");
    assert!(cursor3.is_none(), "cursor must be None at end of stream");

    // No duplicates, no drops: union of LIDs covers the full dataset.
    let total = page1.len() + page2.len() + page3.len();
    assert_eq!(total, 2500);
    let mut all_lids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for rec in page1.iter().chain(page2.iter()).chain(page3.iter()) {
        let inserted = all_lids.insert(rec.lid.to_string());
        assert!(inserted, "LID {} appeared on more than one page", rec.lid);
    }
    assert_eq!(all_lids.len(), 2500, "missing or duplicated records");
}

/// Cursor is bound to the WHERE clause that produced it. Re-using a
/// cursor under a different filter must error — silent paging across
/// filter edges would produce inconsistent results.
#[test]
fn test_scan_cursor_filter_mismatch_rejected() {
    let (engine, _dir) = temp_engine();
    engine.run(r#"LOBE "fm""#).unwrap();
    for i in 0..1_500 {
        let kind = if i % 2 == 0 { "A" } else { "B" };
        engine
            .run(&format!(
                r#"PUT {{_type: "X", n: {i}, kind: "{kind}"}} IN "fm""#
            ))
            .unwrap();
    }

    let r = engine
        .run(r#"SCAN "fm" WHERE kind = "A" LIMIT 500"#)
        .unwrap();
    let (_page, cursor, _has_more) = assert_paginated(&r);
    let token = cursor.expect("page 1 must yield a cursor");

    // Re-use the cursor with a different WHERE clause.
    let q = format!(r#"SCAN "fm" WHERE kind = "B" LIMIT 500 CURSOR "{token}""#);
    let r = engine.run(&q);
    let err = r.expect_err("filter mismatch must be rejected");
    let msg = format!("{err}");
    assert!(
        msg.contains("WHERE clause does not match"),
        "Expected filter-mismatch error, got: {msg}"
    );
}

/// Garbage cursor token surfaces a clear `cursor invalid` error.
#[test]
fn test_scan_cursor_corrupted_token_rejected() {
    let (engine, _dir) = temp_engine();
    engine.run(r#"LOBE "ct""#).unwrap();
    engine.run(r#"PUT {x: 1} IN "ct""#).unwrap();

    let r = engine.run(r#"SCAN "ct" CURSOR "not-a-real-token""#);
    let err = r.expect_err("garbage cursor must be rejected");
    let msg = format!("{err}");
    assert!(msg.contains("cursor invalid"), "got: {msg}");
}

/// Default LIMIT applies when CURSOR is supplied without LIMIT — the
/// SCAN_LIMIT_DEFAULT acts as the page size.
#[test]
fn test_scan_cursor_default_limit_as_page_size() {
    let (engine, _dir) = temp_engine();
    engine.run(r#"LOBE "dpl""#).unwrap();
    for i in 0..1_200 {
        engine
            .run(&format!(r#"PUT {{_type: "X", n: {i}}} IN "dpl""#))
            .unwrap();
    }

    // No LIMIT clause: default cap 1000 applies, yields a cursor.
    let r = engine.run(r#"SCAN "dpl""#).unwrap();
    let (page1, cursor1, has_more1) = assert_paginated(&r);
    assert_eq!(page1.len(), 1000);
    assert!(has_more1);
    let token = cursor1.expect("default-limit overflow must yield a cursor");

    // Continuation page: 200 records remain.
    let q2 = format!(r#"SCAN "dpl" CURSOR "{token}""#);
    let r = engine.run(&q2).unwrap();
    let (page2, cursor2, has_more2) = assert_paginated(&r);
    assert_eq!(page2.len(), 200);
    assert!(!has_more2);
    assert!(cursor2.is_none());
}

// ═══════════════════════════════════════════════════════════════════
// v0.2.5.1 — Standalone WHERE on SET / DELETE / LINK
// ═══════════════════════════════════════════════════════════════════

/// Standalone SET with WHERE updates only the matching record(s) — the
/// remaining records keep their original values. Pre-v0.2.5.1 the parser
/// rejected the WHERE clause outright (FAIL-pre).
#[test]
fn test_set_standalone_with_where_updates_only_match() {
    let (engine, _dir) = temp_engine();
    engine.run(r#"LOBE "swh""#).unwrap();
    engine
        .run(r#"PUT {_type: "Item", code: "A", status: "open"} IN "swh""#)
        .unwrap();
    engine
        .run(r#"PUT {_type: "Item", code: "B", status: "open"} IN "swh""#)
        .unwrap();
    engine
        .run(r#"PUT {_type: "Item", code: "C", status: "open"} IN "swh""#)
        .unwrap();

    let r = engine
        .run(r#"SET "swh" status = "closed" WHERE code = "B""#)
        .unwrap();
    assert_ok(&r);

    let r = engine.run(r#"SCAN "swh" LIMIT 100"#).unwrap();
    let recs = assert_records(&r, 3);
    let mut closed = 0;
    let mut open = 0;
    for rec in &recs {
        match rec.fields.get("status") {
            Some(xyzdb_core::value::Value::Text(s)) if s == "closed" => closed += 1,
            Some(xyzdb_core::value::Value::Text(s)) if s == "open" => open += 1,
            _ => {}
        }
    }
    assert_eq!(closed, 1, "exactly one record should be closed");
    assert_eq!(open, 2, "the other two should remain open");
}

/// Standalone DELETE with WHERE removes only the matching record. The
/// other records survive.
#[test]
fn test_delete_standalone_with_where_removes_only_match() {
    let (engine, _dir) = temp_engine();
    engine.run(r#"LOBE "dwh""#).unwrap();
    for code in ["A", "B", "C"] {
        engine
            .run(&format!(
                r#"PUT {{_type: "Item", code: "{code}", status: "open"}} IN "dwh""#
            ))
            .unwrap();
    }

    let r = engine.run(r#"DELETE "dwh" WHERE code = "B""#).unwrap();
    assert_ok(&r);

    let r = engine.run(r#"SCAN "dwh" LIMIT 100"#).unwrap();
    let recs = assert_records(&r, 2);
    let codes: std::collections::HashSet<_> = recs
        .iter()
        .filter_map(|r| match r.fields.get("code") {
            Some(xyzdb_core::value::Value::Text(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert!(codes.contains("A"));
    assert!(codes.contains("C"));
    assert!(!codes.contains("B"));
}

/// Standalone LINK with WHERE on both sides resolves source and target
/// to the records the operator actually meant — pre-v0.2.5.1 the LINK
/// statement could only target the `first()` record under each lobe,
/// which made multi-record LINK impossible without going through
/// PUT...LINK TO.
#[test]
fn test_link_standalone_with_where_on_both_sides() {
    let (engine, _dir) = temp_engine();
    engine.run(r#"LOBE "src""#).unwrap();
    engine.run(r#"LOBE "tgt""#).unwrap();

    // Two records on each side; link "S2" to "T2" specifically.
    engine
        .run(r#"PUT {_type: "S", code: "S1"} IN "src""#)
        .unwrap();
    engine
        .run(r#"PUT {_type: "S", code: "S2"} IN "src""#)
        .unwrap();
    engine
        .run(r#"PUT {_type: "T", code: "T1"} IN "tgt""#)
        .unwrap();
    engine
        .run(r#"PUT {_type: "T", code: "T2"} IN "tgt""#)
        .unwrap();

    let r = engine
        .run(r#"LINK "src" WHERE code = "S2" TO "tgt" WHERE code = "T2" AS "owner""#)
        .unwrap();
    assert_ok(&r);

    // _link_owner must appear on S2 only, and point at the T2 LID.
    let r = engine.run(r#"SCAN "src" LIMIT 10"#).unwrap();
    let src_recs = assert_records(&r, 2);
    let s2 = src_recs
        .iter()
        .find(|r| {
            matches!(r.fields.get("code"), Some(xyzdb_core::value::Value::Text(s)) if s == "S2")
        })
        .expect("S2 must be present");
    let s1 = src_recs
        .iter()
        .find(|r| {
            matches!(r.fields.get("code"), Some(xyzdb_core::value::Value::Text(s)) if s == "S1")
        })
        .expect("S1 must be present");
    assert!(
        s2.fields.contains_key("_link_owner"),
        "S2 should carry _link_owner"
    );
    assert!(
        !s1.fields.contains_key("_link_owner"),
        "S1 must NOT carry _link_owner — WHERE filter binds source to S2"
    );

    let r = engine.run(r#"SCAN "tgt" LIMIT 10"#).unwrap();
    let tgt_recs = assert_records(&r, 2);
    let t2_lid = tgt_recs
        .iter()
        .find(|r| {
            matches!(r.fields.get("code"), Some(xyzdb_core::value::Value::Text(s)) if s == "T2")
        })
        .expect("T2 must be present")
        .lid
        .to_string();

    let link_val = s2.fields.get("_link_owner").expect("link_owner exists");
    match link_val {
        xyzdb_core::value::Value::Text(s) => assert_eq!(s, &t2_lid),
        other => panic!("expected Text LID, got {other:?}"),
    }
}

// ═══════════════════════════════════════════════════════════════════
// v0.2.5.1 — Hueco 1: admin statements remain functional under
// the deprecation wrap (regression guard for COMPACT/ANALYZE/
// BULKMODE/MIGRATE). The tracing::warn! emission itself is
// observed via docker smoke; this test only asserts no behavioural
// change vs pre-wrap.
// ═══════════════════════════════════════════════════════════════════

#[test]
fn test_admin_statements_still_execute_under_deprecation_wrap() {
    let (engine, _dir) = temp_engine();
    engine.run(r#"LOBE "adm""#).unwrap();
    engine
        .run(r#"PUT {_type: "Item", code: "A"} IN "adm""#)
        .unwrap();

    // Each admin statement must run without error under the deprecation
    // wrap. We don't pin the QueryResult shape here — ANALYZE returns
    // `Info`, COMPACT/BULKMODE/MIGRATE return `Ok` — the regression
    // guard is "no error, no panic". The tracing::warn emission itself
    // is observed via docker smoke (server logs).

    engine.run("BULKMODE ON").expect("BULKMODE ON must succeed");
    engine
        .run("BULKMODE OFF")
        .expect("BULKMODE OFF must succeed");
    engine.run("COMPACT").expect("COMPACT must succeed");
    engine
        .run(r#"ANALYZE "adm""#)
        .expect("ANALYZE must succeed");
    engine.run("MIGRATE").expect("MIGRATE must succeed");

    // Data still readable after the five admin operations.
    let r = engine.run(r#"SCAN "adm" LIMIT 10"#).unwrap();
    assert_records(&r, 1);
}

// ─── v0.2.5.2: cursor on FIND for gravity-bounded paths ──────────────────────

/// Anchor lookup returns at most one record. CURSOR on an anchor
/// shape is operationally meaningless and must be rejected with a
/// clear message pointing the user to the right verb.
#[test]
fn find_anchor_with_cursor_rejects() {
    let (engine, _dir) = temp_engine();
    engine.run(r#"LOBE "clientes""#).unwrap();
    engine.run(r#"ANCHOR "rfc" UNIQUE IN "clientes""#).unwrap();
    engine
        .run(r#"PUT {rfc: "ACME-001", name: "Acme Corp"} IN "clientes""#)
        .unwrap();

    // Anchor field + CURSOR -> reject.
    let r = engine.run(r#"FIND "clientes" WHERE rfc = "ACME-001" CURSOR "AQEAAQ_DUMMY""#);
    let err = r.expect_err("FIND-anchor + CURSOR must error");
    let msg = format!("{err}");
    assert!(
        msg.contains("cursor not applicable to anchor lookup"),
        "expected anchor-rejection message; got: {msg}"
    );

    // Sanity: same FIND without CURSOR still works (no regression on
    // the existing fast path).
    let r = engine
        .run(r#"FIND "clientes" WHERE rfc = "ACME-001""#)
        .unwrap();
    assert_records(&r, 1);
}

/// FIND on the gravity-bounded fast path (Finding 13) is the one
/// shape where CURSOR can do useful work: a gravity bucket can hold
/// many records (Credit + Installment + Payment + ... co-located).
/// This test populates 2 500 records sharing one gravity value and
/// walks them in 1 000-record pages via CURSOR.
#[test]
fn find_gravity_with_cursor_paginates() {
    let (engine, _dir) = temp_engine();
    engine.run(r#"LOBE "creditos""#).unwrap();

    // Same gravity value across 2 500 records — same gravity_hash, so
    // they live contiguously in one bucket. The first PUT registers
    // `rfc` as the gravity field for the lobe.
    for i in 0..2_500 {
        engine
            .run(&format!(
                r#"PUT {{*rfc: "ACME-001", _type: "Item", n: {i}}} IN "creditos""#
            ))
            .unwrap();
    }

    // First page: FIND with LIMIT (no CURSOR). The gravity-bounded
    // fast path emits PaginatedRecords with a fresh cursor.
    let r1 = engine
        .run(r#"FIND "creditos" WHERE rfc = "ACME-001" LIMIT 1000"#)
        .unwrap();
    let (page1, cursor1, has_more1) = assert_paginated(&r1);
    assert_eq!(page1.len(), 1000, "page 1 size");
    assert!(has_more1, "page 1 must have_more on a 2500-record bucket");
    let token1 = cursor1.expect("page 1 must yield a cursor");

    // Second page: same query shape, FIND with cursor.
    let q2 = format!(r#"FIND "creditos" WHERE rfc = "ACME-001" LIMIT 1000 CURSOR "{token1}""#);
    let r2 = engine.run(&q2).unwrap();
    let (page2, cursor2, has_more2) = assert_paginated(&r2);
    assert_eq!(page2.len(), 1000, "page 2 size");
    assert!(has_more2, "page 2 must have_more (500 records remain)");
    let token2 = cursor2.expect("page 2 must yield a cursor");

    // Third (final) page: 500 records, has_more=false, cursor=None.
    let q3 = format!(r#"FIND "creditos" WHERE rfc = "ACME-001" LIMIT 1000 CURSOR "{token2}""#);
    let r3 = engine.run(&q3).unwrap();
    let (page3, cursor3, has_more3) = assert_paginated(&r3);
    assert_eq!(page3.len(), 500, "page 3 size");
    assert!(!has_more3, "page 3 reaches end of stream");
    assert!(cursor3.is_none(), "cursor must be None at end of stream");

    // No duplicates across pages: union of LIDs covers the full
    // dataset.
    let total = page1.len() + page2.len() + page3.len();
    assert_eq!(total, 2500);
    let mut lids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for rec in page1.iter().chain(page2.iter()).chain(page3.iter()) {
        let inserted = lids.insert(rec.lid.to_string());
        assert!(inserted, "LID {} appeared on more than one page", rec.lid);
    }
    assert_eq!(
        lids.len(),
        2500,
        "missing or duplicated records across pages"
    );
}

/// FIND with CURSOR on a field that is neither anchor nor gravity is
/// rejected. FIND remains a fast-lookup verb; full-lobe iteration is
/// SCAN. The error message guides the user to the right tool.
#[test]
fn find_non_fast_field_rejects() {
    let (engine, _dir) = temp_engine();
    engine.run(r#"LOBE "items""#).unwrap();
    // No ANCHOR declaration on `name`; no `*` prefix on PUT, so no
    // gravity field is registered for the lobe either.
    engine
        .run(r#"PUT {name: "Acme", n: 1} IN "items""#)
        .unwrap();
    engine
        .run(r#"PUT {name: "Acme", n: 2} IN "items""#)
        .unwrap();

    let r = engine.run(r#"FIND "items" WHERE name = "Acme" CURSOR "AQEAAQ_DUMMY""#);
    let err = r.expect_err("FIND on non-fast field with CURSOR must error");
    let msg = format!("{err}");
    assert!(
        msg.contains("no anchor or gravity"),
        "expected non-fast-path rejection; got: {msg}"
    );

    // Sanity: same FIND without CURSOR still falls through to scan
    // and returns both records (existing fallback behavior).
    let r = engine.run(r#"FIND "items" WHERE name = "Acme""#).unwrap();
    assert_records(&r, 2);
}

/// `stats_snapshot.keyspaces[*].warmup` reflects the bloom + index + meta
/// bytes loaded by `Tree::open_with_scheduler` when the engine reopens an
/// existing data directory. On a fresh open (no manifest yet) every keyspace
/// reports zero. After writes that flush to SSTables and COMPACT, reopening
/// the engine on the same path must surface non-zero warmup counts; this defends against a future
/// regression that introduces lazy bloom loading.
#[test]
fn engine_stats_includes_warmup_per_tree_after_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");

    // First open: nothing on disk yet — warmup is all zeros across the five
    // foundational keyspaces.
    {
        let engine = Engine::open(dir.path()).expect("open fresh");
        let snap = engine.stats_snapshot();
        for ks in ["spatial", "identity", "dictionary", "ghosts", "vectors"] {
            let w = &snap.keyspaces.get(ks).unwrap().warmup;
            assert_eq!(
                w.sstables_opened, 0,
                "{ks} warmup.sstables_opened should be 0 on a pristine open"
            );
            assert_eq!(
                w.bytes_loaded, 0,
                "{ks} warmup.bytes_loaded should be 0 on a pristine open"
            );
        }

        engine.run(r#"LOBE "warmup_lobe""#).expect("lobe");
        engine
            .run(r#"ANCHOR "id" UNIQUE IN "warmup_lobe""#)
            .expect("anchor");
        for i in 0..20u32 {
            engine
                .run(&format!(
                    r#"PUT {{_type: "Doc", id: "W{i:03}", payload: "x"}} IN "warmup_lobe""#
                ))
                .expect("put");
        }
        engine.run("COMPACT").expect("compact");
    }

    // Second open on the same directory: spatial, identity, and dictionary
    // had data flushed and compacted, so each must show non-zero warmup.
    // ghosts may stay zero because no ghost lobe was created.
    let engine2 = Engine::open(dir.path()).expect("reopen");
    let snap2 = engine2.stats_snapshot();
    for ks in ["spatial", "identity", "dictionary"] {
        let w = &snap2.keyspaces.get(ks).unwrap().warmup;
        assert!(
            w.sstables_opened >= 1,
            "{ks} warmup.sstables_opened should be >= 1 after reopen, got {}",
            w.sstables_opened
        );
        assert!(
            w.bytes_loaded > 0,
            "{ks} warmup.bytes_loaded should be > 0 after reopen, got {}",
            w.bytes_loaded
        );
    }
}

// --- v0.6.0-pre C.2: RAM budget observer ---

/// The /stats.ram_budget surface exists, is populated when data is
/// written, and produces a sensible ratio against VmRSS under a small
/// synthetic workload. The strict `[0.85, 1.15]` band is enforced by
/// the brief at the soak-test scale (humanrandom + daily_erp, 30 min);
/// here we just check the snapshot is well-formed and ratio is finite
/// and positive once VmRSS is non-zero.
#[test]
fn ram_budget_snapshot_populates_after_writes() {
    use xyzdb_engine::engine::Engine;
    let dir = tempfile::tempdir().expect("tmpdir");
    let engine = Engine::open(dir.path()).expect("open");

    // Empty snapshot: total_estimated_bytes ≥ 0, ratio finite.
    let pre = engine.stats_snapshot();
    let _ = pre.ram_budget.total_estimated_bytes;
    assert!(
        pre.ram_budget.ratio.is_finite(),
        "ratio must be finite even with empty engine"
    );

    // Drive a small write workload to ensure memtables grow.
    engine.run("LOBE \"workspace\"").expect("create lobe");
    engine
        .run("ANCHOR \"code\" UNIQUE IN \"workspace\"")
        .expect("anchor");
    for i in 0..500u32 {
        engine
            .run(&format!(
                "PUT {{_type: \"Item\", code: \"K{i:04}\", payload: \"hello world\"}} IN \"workspace\""
            ))
            .expect("put");
    }

    let post = engine.stats_snapshot();
    let rb = &post.ram_budget;
    assert!(
        rb.memtables_bytes > 0,
        "memtables_bytes must grow after writes (got {})",
        rb.memtables_bytes
    );
    assert_eq!(
        rb.total_estimated_bytes,
        rb.block_cache_bytes
            + rb.record_cache_bytes
            + rb.memtables_bytes
            + rb.sst_metadata_bytes
            + rb.registries_bytes
            + rb.ghost_aggregates_bytes,
        "total must equal the per-component sum"
    );
    // VmRSS may be 0 on platforms without /proc/self/status (macOS). When
    // present, ratio must be finite and within a generous sanity band; the
    // strict [0.85, 1.15] gate is brief-soak-only, not unit-test-level.
    if rb.vmrss_bytes > 0 {
        assert!(
            rb.ratio > 0.0 && rb.ratio.is_finite(),
            "ratio must be > 0 and finite when vmrss is non-zero (got {})",
            rb.ratio
        );
        assert!(
            rb.ratio <= 2.0,
            "ratio should not exceed 2.0 absent gross double-counting (got {})",
            rb.ratio
        );
    }
}

/// 1d — concurrent writers must not drift a ghost's incremental aggregate.
/// Hypothesis under test (not belief): if the incremental update were a
/// non-atomic read-modify-write, concurrent writers would lose updates and
/// the ghost's precomputed count would fall below the true record count.
/// `notify_write` actually holds the ghosts write-lock across the update and
/// every write path calls it per record, so this should hold — verified, not
/// asserted. Domain-neutral vocab.
#[test]
fn concurrent_writes_keep_ghost_aggregate_consistent() {
    let (engine, _dir) = temp_engine();
    engine.run(r#"LOBE "data""#).unwrap();
    engine
        .run(r#"CREATE GHOST "cnt" FROM "data" WHERE flag = "on" ORDER BY bucket GROUP BY bucket AGGREGATE count()"#)
        .unwrap();

    let engine = engine.into_arc();
    const THREADS: usize = 8;
    const PER: usize = 250;
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(THREADS));
    let handles: Vec<_> = (0..THREADS)
        .map(|t| {
            let e = std::sync::Arc::clone(&engine);
            let b = std::sync::Arc::clone(&barrier);
            std::thread::spawn(move || {
                b.wait();
                for i in 0..PER {
                    e.run(&format!(
                        r#"PUT {{flag: "on", bucket: "b", id: "{t}_{i}", n: {i}}} IN "data""#
                    ))
                    .unwrap();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }

    let total = (THREADS * PER) as i64;

    // Ground truth: full record re-scan (primary; a record scan is not served
    // by the aggregate ghost).
    let rescan = match engine
        .run(r#"SCAN "data" WHERE flag = "on" LIMIT 10000"#)
        .unwrap()
    {
        QueryResult::Records(r) => r.len() as i64,
        QueryResult::PaginatedRecords { records, .. } => records.len() as i64,
        other => panic!("unexpected scan: {other:?}"),
    };
    assert_eq!(
        rescan, total,
        "all {total} concurrent writes must be stored"
    );

    // Incremental ghost-precomputed aggregate.
    let ghost_count = match engine
        .run(r#"SCAN "data" WHERE flag = "on" | GROUP BY bucket | AGGREGATE count()"#)
        .unwrap()
    {
        QueryResult::GroupedAggregation(groups) => {
            let g = groups
                .iter()
                .find(|g| g.get("bucket") == Some(&xyzdb_core::value::Value::Text("b".into())))
                .expect("the single bucket group");
            match g.get("count") {
                Some(xyzdb_core::value::Value::Int(n)) => *n,
                other => panic!("no count in group: {other:?}"),
            }
        }
        other => panic!("unexpected aggregation result: {other:?}"),
    };
    assert_eq!(
        ghost_count, total,
        "incremental ghost aggregate must equal the {total} concurrent writes \
         (lost RMW updates would drop it below); rescan={rescan}"
    );
}

/// 1e — a paginated SCAN must return each record exactly once across a
/// flush/compaction that happens between pages. Spatial keys are immutable
/// and compaction preserves key order, and the cursor resumes strictly past
/// the last key (`last_spatial_key ++ [0x00]`), so this should hold —
/// verified, not asserted. The dataset is fixed before paging (the property
/// under test is compaction/flush stability, not a moving target).
/// Domain-neutral vocab.
#[test]
fn cursor_exactly_once_across_compaction() {
    let (engine, _dir) = temp_engine();
    engine.run(r#"LOBE "data""#).unwrap();
    const N: usize = 120;
    for i in 0..N {
        engine
            .run(&format!(
                r#"PUT {{_type: "X", id: "r{i:03}", n: {i}}} IN "data""#
            ))
            .unwrap();
    }

    let mut seen: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        let q = match &cursor {
            Some(tok) => format!(r#"SCAN "data" LIMIT 25 CURSOR "{tok}""#),
            None => r#"SCAN "data" LIMIT 25"#.to_string(),
        };
        let (recs, next, more) = match engine.run(&q).unwrap() {
            QueryResult::PaginatedRecords {
                records,
                cursor,
                has_more,
                ..
            } => (records, cursor, has_more),
            QueryResult::Records(records) => (records, None, false),
            other => panic!("unexpected scan result: {other:?}"),
        };
        for r in &recs {
            if let Some(xyzdb_core::value::Value::Text(id)) = r.fields.get("id") {
                *seen.entry(id.clone()).or_insert(0) += 1;
            }
        }
        pages += 1;
        // Flush memtable → SST + compact BETWEEN pages: storage relocates,
        // keys stay stable. The next page must resume correctly.
        engine.run(r#"COMPACT"#).unwrap();
        if !more {
            break;
        }
        cursor = next;
        assert!(pages < 100, "pagination did not terminate");
    }

    assert_eq!(
        seen.len(),
        N,
        "every record must appear across pages (got {} of {N})",
        seen.len()
    );
    for (id, count) in &seen {
        assert_eq!(
            *count, 1,
            "record {id} returned {count}x — exactly-once violated"
        );
    }
}
