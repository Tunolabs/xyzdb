// SPDX-License-Identifier: BUSL-1.1
use std::sync::Arc;
use std::time::Instant;
use tokio::net::TcpStream;
use xyzdb_engine::engine::Engine;
use xyzdb_server::protocol::{self, STATUS_ERROR, STATUS_OK};

async fn start_server(engine: Arc<Engine>) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind failed");
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        loop {
            if let Ok((stream, addr)) = listener.accept().await {
                let engine = engine.clone();
                tokio::spawn(xyzdb_server::connection::handle_connection(
                    engine,
                    stream,
                    addr,
                    Arc::new(None),
                ));
            }
        }
    });

    port
}

async fn query(stream: &mut TcpStream, q: &str) -> (u8, String) {
    protocol::write_request_v1(stream, q)
        .await
        .expect("send failed");
    let (status, bytes) = protocol::read_response_raw(stream)
        .await
        .expect("recv failed");
    (status, String::from_utf8(bytes).expect("valid utf8"))
}

fn assert_ok(status: u8, payload: &str, label: &str) {
    assert_eq!(
        status, STATUS_OK,
        "{label}: expected OK, got error: {payload}"
    );
}

fn count_occurrences(payload: &str, pattern: &str) -> usize {
    payload.matches(pattern).count()
}

/// THE END-TO-END CO-LOCATION TEST over TCP.
///
/// Server ← TCP → Client
///
/// 1 Company + 1 Project + 3 Tasks, all co-located.
/// FIND | PULL returns 5 records over the wire.
#[tokio::test]
async fn test_e2e_colocation() {
    let dir = tempfile::tempdir().expect("tempdir");
    let engine = Engine::open(dir.path()).expect("engine open").into_arc();
    let port = start_server(engine).await;

    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .expect("connect failed");

    println!("\n=== E2E CO-LOCATION TEST (TCP) ===\n");

    // Setup
    let (s, p) = query(&mut stream, r#"LOBE "workspace""#).await;
    assert_ok(s, &p, "LOBE");
    println!("  {p}");

    let (s, p) = query(&mut stream, r#"ANCHOR "code" UNIQUE IN "workspace""#).await;
    assert_ok(s, &p, "ANCHOR code");
    println!("  {p}");

    let (s, p) = query(&mut stream, r#"ANCHOR "project_id" UNIQUE IN "workspace""#).await;
    assert_ok(s, &p, "ANCHOR project_id");
    println!("  {p}");

    // Insert Company
    let (s, p) = query(
        &mut stream,
        r#"PUT {_type: "Company", code: "ACME-001", name: "Acme Corp", region: "US-West"} IN "workspace""#,
    ).await;
    assert_ok(s, &p, "PUT Company");
    println!("  {p}");

    // Insert Project linked to Company
    let (s, p) = query(
        &mut stream,
        r#"PUT {_type: "Project", project_id: "PRJ-001", budget: 50000, duration: 36} IN "workspace" LINK TO "workspace" WHERE code = "ACME-001" AS "owner""#,
    ).await;
    assert_ok(s, &p, "PUT Project");
    println!("  {p}");

    // Insert 3 Tasks linked to Project
    for i in 1..=3 {
        let q = format!(
            r#"PUT {{_type: "Task", numero: {i}, hours: 8, status: "pending"}} IN "workspace" LINK TO "workspace" WHERE project_id = "PRJ-001" AS "task_of""#
        );
        let (s, p) = query(&mut stream, &q).await;
        assert_ok(s, &p, &format!("PUT Task {i}"));
    }
    println!("  3 Tasks inserted");

    // THE QUERY
    println!("\n  Executing FIND | PULL over TCP...");
    let start = Instant::now();
    let (s, p) = query(
        &mut stream,
        r#"FIND "workspace" WHERE code = "ACME-001" | PULL depth=1"#,
    )
    .await;
    let elapsed = start.elapsed();
    assert_ok(s, &p, "FIND|PULL");

    // Verify 5 records
    let record_count = count_occurrences(&p, "LID:");
    assert_eq!(
        record_count, 5,
        "Expected 5 records (1 Company + 1 Project + 3 Tasks), got {record_count}"
    );

    let companies = count_occurrences(&p, r#"_type: "Company""#);
    let projects = count_occurrences(&p, r#"_type: "Project""#);
    let tasks = count_occurrences(&p, r#"_type: "Task""#);
    assert_eq!(companies, 1, "Expected 1 Company");
    assert_eq!(projects, 1, "Expected 1 Project");
    assert_eq!(tasks, 3, "Expected 3 Tasks");

    println!("\n{p}");
    println!(
        "\n  PULL returned 5 records in {:.3}ms (TCP round-trip)",
        elapsed.as_secs_f64() * 1000.0
    );

    // Test PULL only=Task
    let (s, p) = query(
        &mut stream,
        r#"FIND "workspace" WHERE code = "ACME-001" | PULL only=Task"#,
    )
    .await;
    assert_ok(s, &p, "PULL only=Task");
    let task_count = count_occurrences(&p, "LID:");
    assert_eq!(task_count, 3, "PULL only=Task should return 3");
    println!("  PULL only=Task → {task_count} records");

    // Test SHOW LOBES
    let (s, p) = query(&mut stream, "SHOW LOBES").await;
    assert_ok(s, &p, "SHOW LOBES");
    println!("  {p}");

    // Test error handling
    let (s, _p) = query(&mut stream, r#"FIND "nonexistent" WHERE x = 1"#).await;
    assert_eq!(s, STATUS_ERROR, "Should get error for nonexistent lobe");
    println!("  Error handling works (nonexistent lobe)");

    println!("\n=== E2E TEST PASSED ===\n");
}

/// Test that ON CONFLICT UPDATE works over TCP.
#[tokio::test]
async fn test_e2e_upsert() {
    let dir = tempfile::tempdir().expect("tempdir");
    let engine = Engine::open(dir.path()).expect("engine open").into_arc();
    let port = start_server(engine).await;

    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .expect("connect");

    query(&mut stream, r#"LOBE "workspace""#).await;
    query(&mut stream, r#"ANCHOR "code" UNIQUE IN "workspace""#).await;
    query(&mut stream, r#"PUT {code: "X", name: "A"} IN "workspace""#).await;

    let (s, p) = query(
        &mut stream,
        r#"PUT {code: "X", name: "B"} IN "workspace" ON CONFLICT UPDATE"#,
    )
    .await;
    assert_ok(s, &p, "upsert");

    let (s, p) = query(&mut stream, r#"FIND "workspace" WHERE code = "X""#).await;
    assert_ok(s, &p, "find");
    assert!(p.contains(r#"name: "B""#), "Should be updated to B: {p}");
}
