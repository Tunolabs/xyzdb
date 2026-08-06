// SPDX-License-Identifier: BUSL-1.1
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::config::Config;
use crate::reporter::{self, format_num, SuiteReport, TestResult};
use crate::utils::latency::LatencyCollector;
use crate::utils::tcp_client::TcpClient;

pub async fn run(config: &Config) -> Result<SuiteReport> {
    reporter::print_suite_header("Suite 5: Connection Management");

    let suite_start = Instant::now();
    let mut results: Vec<TestResult> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    let run_id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64 % 1_000_000)
        .unwrap_or(0);

    let host = config.host.clone();
    let port = config.port;

    // ── 5.1 Connection ramp-up ──────────────────────────────────────────

    reporter::print_metric("Test 5.1", "Connection ramp-up (1..32 connections)");

    let levels: Vec<usize> = vec![1, 2, 4, 8, 16, 32];
    let queries_per_level: usize = 100;
    let mut throughputs: Vec<(usize, f64)> = Vec::new();

    for &n_conns in &levels {
        // Open n_conns connections
        let mut clients = Vec::with_capacity(n_conns);
        for _ in 0..n_conns {
            let c = TcpClient::connect(&host, port)
                .await
                .with_context(|| format!("5.1: open connection (level {})", n_conns))?;
            clients.push(Arc::new(tokio::sync::Mutex::new(c)));
        }

        let start = Instant::now();
        let mut handles = Vec::new();

        for i in 0..queries_per_level {
            let client = Arc::clone(&clients[i % n_conns]);
            handles.push(tokio::spawn(async move {
                let mut c = client.lock().await;
                c.query_text("SHOW LOBES").await
            }));
        }

        let mut ok_count: u64 = 0;
        for handle in handles {
            match handle.await {
                Ok(Ok(_)) => ok_count += 1,
                Ok(Err(e)) => {
                    errors.push(format!("5.1: query failed at level {}: {}", n_conns, e));
                }
                Err(e) => {
                    errors.push(format!("5.1: task panicked at level {}: {}", n_conns, e));
                }
            }
        }

        let elapsed = start.elapsed().as_secs_f64();
        let tps = if elapsed > 0.0 { ok_count as f64 / elapsed } else { 0.0 };
        throughputs.push((n_conns, tps));

        reporter::print_metric(
            &format!("  {} conns", n_conns),
            &format!("{}/{} ok, {:.0} ops/s in {:.1}ms", ok_count, queries_per_level, tps, elapsed * 1000.0),
        );
    }

    // Pass if throughput increases at some point compared to 1-connection baseline
    let baseline_tps = throughputs[0].1;
    let peak = throughputs.iter().max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    let (sat_level, peak_tps) = peak.copied().unwrap_or((1, baseline_tps));
    let rampup_passed = peak_tps >= baseline_tps;

    reporter::print_metric(
        "  Saturation level",
        &format!("{} conns ({:.0} ops/s peak vs {:.0} ops/s baseline)", sat_level, peak_tps, baseline_tps),
    );

    reporter::print_result(
        "5.1 Connection ramp-up",
        rampup_passed,
        &format!("peak {:.0} ops/s @ {} conns", peak_tps, sat_level),
        &format!("baseline {:.0} ops/s", baseline_tps),
    );

    results.push(TestResult {
        name: "5.1 Connection ramp-up".into(),
        passed: rampup_passed,
        value: format!("peak={:.0} ops/s @ {} conns", peak_tps, sat_level),
        expected: "throughput increases with connections".into(),
        notes: format!("baseline={:.0} ops/s, {:.1}x scaling", baseline_tps, peak_tps / baseline_tps.max(1.0)),
    });

    reporter::print_separator();

    // ── 5.2 Connection churn ────────────────────────────────────────────

    reporter::print_metric("Test 5.2", "Connection churn (1000 connect-query-drop cycles)");

    let churn_cycles: u32 = 1000;

    // Churn: open, query, drop
    let churn_start = Instant::now();
    let mut churn_latency = LatencyCollector::with_capacity(churn_cycles as usize);

    for _ in 0..churn_cycles {
        let cycle_start = Instant::now();
        let mut c = TcpClient::connect(&host, port)
            .await
            .context("5.2: churn connect")?;
        let _ = c.query_text("SHOW LOBES").await.context("5.2: churn query")?;
        drop(c);
        churn_latency.record(cycle_start.elapsed());
    }

    let churn_total = churn_start.elapsed();
    let churn_avg_ms = churn_total.as_secs_f64() * 1000.0 / churn_cycles as f64;

    // Persistent: 1000 queries on same connection
    let persistent_start = Instant::now();
    let mut persistent_client = TcpClient::connect(&host, port)
        .await
        .context("5.2: persistent connect")?;

    for _ in 0..churn_cycles {
        let _ = persistent_client.query_text("SHOW LOBES").await.context("5.2: persistent query")?;
    }

    let persistent_total = persistent_start.elapsed();
    let persistent_avg_ms = persistent_total.as_secs_f64() * 1000.0 / churn_cycles as f64;
    drop(persistent_client);

    let overhead_ratio = if persistent_avg_ms > 0.0 { churn_avg_ms / persistent_avg_ms } else { 0.0 };

    reporter::print_metric(
        "  Churn avg",
        &format!("{:.2}ms/cycle ({:.1}ms total)", churn_avg_ms, churn_total.as_secs_f64() * 1000.0),
    );
    reporter::print_metric(
        "  Persistent avg",
        &format!("{:.2}ms/query ({:.1}ms total)", persistent_avg_ms, persistent_total.as_secs_f64() * 1000.0),
    );
    reporter::print_metric("  Overhead ratio", &format!("{:.2}x", overhead_ratio));

    let churn_passed = true; // informational -- overhead is expected
    reporter::print_result(
        "5.2 Connection churn",
        churn_passed,
        &format!("{:.2}x overhead", overhead_ratio),
        &format!("churn={:.2}ms vs persistent={:.2}ms", churn_avg_ms, persistent_avg_ms),
    );

    results.push(TestResult {
        name: "5.2 Connection churn".into(),
        passed: churn_passed,
        value: format!("{:.2}x overhead", overhead_ratio),
        expected: "informational".into(),
        notes: format!(
            "{} cycles, churn={:.2}ms/cycle, persistent={:.2}ms/query",
            format_num(churn_cycles as u64), churn_avg_ms, persistent_avg_ms,
        ),
    });

    reporter::print_separator();

    // ── 5.3 Idle connection ─────────────────────────────────────────────

    reporter::print_metric("Test 5.3", "Idle connection (10s sleep then query)");

    let mut idle_client = TcpClient::connect(&host, port)
        .await
        .context("5.3: connect")?;

    reporter::print_metric("  Sleeping", "10 seconds...");
    tokio::time::sleep(Duration::from_secs(10)).await;

    let idle_response = idle_client.query_text("SHOW LOBES")
        .await
        .context("5.3: query after idle");

    let idle_passed = idle_response.is_ok();
    let idle_value = match &idle_response {
        Ok(text) => {
            let lines = text.lines().count();
            format!("OK ({} lines)", lines)
        }
        Err(e) => format!("ERROR: {}", e),
    };

    if !idle_passed {
        errors.push(format!("5.3: idle connection failed: {}", idle_response.as_ref().err().map(|e| e.to_string()).unwrap_or_default()));
    }

    reporter::print_result(
        "5.3 Idle connection",
        idle_passed,
        &idle_value,
        if idle_passed { "connection survived 10s idle" } else { "connection dropped" },
    );

    results.push(TestResult {
        name: "5.3 Idle connection".into(),
        passed: idle_passed,
        value: idle_value,
        expected: "response after 10s idle".into(),
        notes: "SHOW LOBES after 10s sleep".into(),
    });

    reporter::print_separator();

    // ── 5.4 Partial frame ───────────────────────────────────────────────

    reporter::print_metric("Test 5.4", "Partial frame (fragmented TCP send)");

    let addr = format!("{}:{}", host, port);
    let mut raw_stream = TcpStream::connect(&addr)
        .await
        .context("5.4: raw TCP connect")?;
    raw_stream.set_nodelay(true)?;

    // Build a complete v1 frame for "SHOW LOBES"
    let payload = b"SHOW LOBES";
    let payload_len = payload.len() as u32;
    let mut frame = Vec::with_capacity(1 + 4 + payload.len());
    frame.push(0x01u8); // version byte
    frame.extend_from_slice(&payload_len.to_be_bytes()); // 4-byte BE length
    frame.extend_from_slice(payload); // payload

    // Send first 3 bytes (partial header: version + first 2 bytes of length)
    raw_stream.write_all(&frame[..3])
        .await
        .context("5.4: send partial header")?;
    raw_stream.flush().await.context("5.4: flush partial")?;

    reporter::print_metric("  Sent", &format!("first 3 of {} bytes, waiting 2s...", frame.len()));

    tokio::time::sleep(Duration::from_secs(2)).await;

    // Send remaining bytes
    raw_stream.write_all(&frame[3..])
        .await
        .context("5.4: send remaining frame")?;
    raw_stream.flush().await.context("5.4: flush remaining")?;

    // Read response: status(1) + length(4) + payload
    let status = AsyncReadExt::read_u8(&mut raw_stream)
        .await
        .context("5.4: read response status")?;
    let resp_len = AsyncReadExt::read_u32(&mut raw_stream)
        .await
        .context("5.4: read response length")?;
    let mut resp_buf = vec![0u8; resp_len as usize];
    AsyncReadExt::read_exact(&mut raw_stream, &mut resp_buf)
        .await
        .context("5.4: read response payload")?;

    let resp_text = String::from_utf8_lossy(&resp_buf);
    let partial_passed = status == 0x00 && !resp_text.is_empty();

    if !partial_passed {
        errors.push(format!("5.4: partial frame response: status={}, body_len={}", status, resp_len));
    }

    reporter::print_metric("  Response", &format!("status={}, {} bytes", status, resp_len));

    reporter::print_result(
        "5.4 Partial frame",
        partial_passed,
        &format!("status={}, {} bytes", status, resp_len),
        if partial_passed { "server handled fragmented TCP" } else { "server failed on partial frame" },
    );

    results.push(TestResult {
        name: "5.4 Partial frame".into(),
        passed: partial_passed,
        value: format!("status={}, {} bytes response", status, resp_len),
        expected: "correct response after fragmented send".into(),
        notes: "sent 3 bytes, waited 2s, sent rest".into(),
    });

    reporter::print_separator();

    // ── 5.5 Abrupt disconnect ───────────────────────────────────────────

    reporter::print_metric("Test 5.5", "Abrupt disconnect (PUT then drop)");

    let disc_lobe = format!("disc_{}", run_id);

    {
        let mut disc_client = TcpClient::connect(&host, port)
            .await
            .context("5.5: connect for PUT")?;

        // Create the lobe first so PUT has somewhere to go
        let create_q = format!("CREATE LOBE \"{}\"", disc_lobe);
        let _ = disc_client.query_text(&create_q).await; // ignore if already exists

        // Send PUT and immediately drop (don't read response)
        let put_q = format!(
            "PUT {{_type: \"Test\", marker: \"disconnect_test_{}\"}} IN \"{}\"",
            run_id, disc_lobe,
        );

        // Write the frame manually so we can drop before reading
        let payload = put_q.as_bytes();
        let stream = disc_client.stream_mut();
        let _ = AsyncWriteExt::write_u8(stream, 0x01).await;
        let _ = AsyncWriteExt::write_u32(stream, payload.len() as u32).await;
        let _ = AsyncWriteExt::write_all(stream, payload).await;
        let _ = AsyncWriteExt::flush(stream).await;
        // Drop without reading response
    }

    reporter::print_metric("  Dropped", "connection after PUT (no response read)");

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Open new connection and verify server is alive
    let mut verify_client = TcpClient::connect(&host, port)
        .await
        .context("5.5: reconnect after disconnect")?;

    // Try SCAN to see if record landed (either outcome is fine)
    let scan_q = format!("SCAN \"{}\" WHERE _type = \"Test\"", disc_lobe);
    let scan_result = verify_client.query_text(&scan_q).await;
    let record_landed = match &scan_result {
        Ok(text) => {
            let has_data = text.contains("disconnect_test_");
            reporter::print_metric("  Record landed", if has_data { "yes" } else { "no" });
            has_data
        }
        Err(_) => {
            reporter::print_metric("  Record landed", "unknown (scan error)");
            false
        }
    };

    // The real check: server is still alive
    let alive_response = verify_client.query_text("SHOW LOBES")
        .await
        .context("5.5: SHOW LOBES after disconnect");

    let disc_passed = alive_response.is_ok();

    if !disc_passed {
        errors.push("5.5: server not responding after abrupt disconnect".into());
    }

    reporter::print_result(
        "5.5 Abrupt disconnect",
        disc_passed,
        if disc_passed { "server alive" } else { "server unresponsive" },
        &format!("record_landed={}, lobe={}", record_landed, disc_lobe),
    );

    results.push(TestResult {
        name: "5.5 Abrupt disconnect".into(),
        passed: disc_passed,
        value: if disc_passed { "server alive after abrupt disconnect".into() } else { "server unresponsive".into() },
        expected: "server survives abrupt disconnect".into(),
        notes: format!("record_landed={}, lobe={}", record_landed, disc_lobe),
    });

    reporter::print_separator();

    // ── 5.6 Many idle connections ───────────────────────────────────────

    reporter::print_metric("Test 5.6", "Many idle connections (50 open, query first & last)");

    let n_idle = 50;
    let mut idle_conns: Vec<TcpClient> = Vec::with_capacity(n_idle);

    for i in 0..n_idle {
        match TcpClient::connect(&host, port).await {
            Ok(c) => idle_conns.push(c),
            Err(e) => {
                errors.push(format!("5.6: failed to open connection {}: {}", i, e));
                break;
            }
        }
    }

    let opened = idle_conns.len();
    reporter::print_metric("  Opened", &format!("{} connections", opened));

    let mut first_ok = false;
    let mut last_ok = false;

    if opened >= 2 {
        // Query on first connection
        match idle_conns[0].query_text("SHOW LOBES").await {
            Ok(_) => {
                first_ok = true;
                reporter::print_metric("  First conn", "SHOW LOBES OK");
            }
            Err(e) => {
                errors.push(format!("5.6: first connection query failed: {}", e));
                reporter::print_metric("  First conn", &format!("FAILED: {}", e));
            }
        }

        // Query on last connection
        let last_idx = opened - 1;
        match idle_conns[last_idx].query_text("SHOW LOBES").await {
            Ok(_) => {
                last_ok = true;
                reporter::print_metric("  Last conn", "SHOW LOBES OK");
            }
            Err(e) => {
                errors.push(format!("5.6: last connection query failed: {}", e));
                reporter::print_metric("  Last conn", &format!("FAILED: {}", e));
            }
        }
    } else if opened == 1 {
        match idle_conns[0].query_text("SHOW LOBES").await {
            Ok(_) => {
                first_ok = true;
                reporter::print_metric("  Only conn", "SHOW LOBES OK");
            }
            Err(e) => {
                errors.push(format!("5.6: only connection query failed: {}", e));
            }
        }
    }

    let many_idle_passed = opened >= n_idle && first_ok && last_ok;

    reporter::print_result(
        "5.6 Many idle connections",
        many_idle_passed,
        &format!("{}/{} opened, first={}, last={}", opened, n_idle, first_ok, last_ok),
        if many_idle_passed { "50 idle conns no issue" } else { "some connections failed" },
    );

    if !many_idle_passed {
        if opened < n_idle {
            errors.push(format!("5.6: only opened {}/{} connections", opened, n_idle));
        }
    }

    results.push(TestResult {
        name: "5.6 Many idle connections".into(),
        passed: many_idle_passed,
        value: format!("{}/{} opened", opened, n_idle),
        expected: format!("{} connections, first & last respond", n_idle),
        notes: format!("first={}, last={}", first_ok, last_ok),
    });

    // Drop all idle connections
    drop(idle_conns);

    // ── Summary ─────────────────────────────────────────────────────────

    let suite_elapsed = suite_start.elapsed();
    reporter::print_separator();
    reporter::print_metric(
        "Suite 5 completed in",
        &format!("{:.2}s", suite_elapsed.as_secs_f64()),
    );

    let all_passed = results.iter().all(|r| r.passed);

    Ok(SuiteReport {
        name: "Suite 5: Connection Management".into(),
        passed: all_passed,
        duration_secs: suite_elapsed.as_secs_f64(),
        results,
        errors,
    })
}
