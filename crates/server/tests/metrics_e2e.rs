//! `/metrics` Prometheus exposition format E2E test. v0.4 cp 2.2.4.
//!
//! Spins up a server, fetches `/metrics`, and feeds the body to the
//! `prometheus-parse` crate (the cycle plan rule: validate against an
//! official parser rather than hand-rolling format checks). Failures
//! mean the emitter produced something a real scraper would reject.

use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use xyzdb_engine::engine::Engine;
use xyzdb_server::protocol::{self, STATUS_ERROR, STATUS_OK};

async fn start_server(engine: Arc<Engine>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
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

async fn engine_arc() -> Arc<Engine> {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.keep();
    Engine::open(&path).expect("engine open").into_arc()
}

#[tokio::test]
async fn test_metrics_parses_with_prometheus_parser() {
    let engine = engine_arc().await;
    let port = start_server(engine).await;

    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .expect("connect");

    protocol::write_request_v1(&mut stream, "/metrics")
        .await
        .expect("send /metrics");

    let (status, body) = protocol::read_response_raw(&mut stream)
        .await
        .expect("read response");
    assert_eq!(status, STATUS_OK);
    let text = String::from_utf8(body).expect("metrics body utf8");

    // Validate via prometheus-parse: every line either parses or is a
    // valid # HELP / # TYPE / blank line.
    let lines: Vec<_> = text
        .lines()
        .map(|l| Ok::<_, std::io::Error>(l.to_string()))
        .collect();
    let parsed = prometheus_parse::Scrape::parse(lines.into_iter())
        .expect("prometheus-parse should accept emitter output");

    // Sanity: at least one sample with the xyzdb_ prefix.
    let total_samples = parsed.samples.len();
    assert!(total_samples > 0, "expected ≥ 1 sample; got: {}", text);
    for sample in &parsed.samples {
        assert!(
            sample.metric.starts_with("xyzdb_"),
            "metric missing xyzdb_ prefix: {} (full body:\n{})",
            sample.metric,
            text
        );
    }

    // Sanity: required core metrics are present.
    let names: std::collections::HashSet<_> =
        parsed.samples.iter().map(|s| s.metric.as_str()).collect();
    let required = [
        "xyzdb_keyspace_mem_active_bytes",
        "xyzdb_keyspace_compact_ok_total",
        "xyzdb_keyspace_compact_err_total",
        "xyzdb_block_cache_capacity_bytes",
        "xyzdb_block_cache_hits_total",
        "xyzdb_block_cache_misses_total",
        "xyzdb_ghost_count_total",
        "xyzdb_sync_thread_last_successful_ts_ms",
    ];
    for r in &required {
        assert!(
            names.contains(r),
            "required metric missing: {} (saw: {:?})",
            r,
            names
        );
    }
}

/// `/metrics` follows the token. It returns the engine stats snapshot, so
/// when `--auth-token` is configured an unauthenticated request is rejected
/// and only an authenticated one succeeds. (The `/health` and `/ready`
/// liveness probes stay on the unauthenticated allowlist — covered by
/// `health_ready_e2e.rs`.)
#[tokio::test]
async fn test_metrics_endpoint_requires_auth() {
    let engine = engine_arc().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        loop {
            if let Ok((stream, addr)) = listener.accept().await {
                let engine = engine.clone();
                tokio::spawn(xyzdb_server::connection::handle_connection(
                    engine,
                    stream,
                    addr,
                    Arc::new(Some("scraper-secret".to_string())),
                ));
            }
        }
    });

    // (1) /metrics WITHOUT the auth frame is rejected: it exposes engine
    // stats, so it follows the token like STATS.
    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .expect("connect");
    protocol::write_request_v1(&mut stream, "/metrics")
        .await
        .expect("send");
    let (status, _body) = protocol::read_response_raw(&mut stream)
        .await
        .expect("read response");
    assert_eq!(
        status, STATUS_ERROR,
        "/metrics without a token must be rejected when --auth-token is set"
    );

    // (2) /metrics WITH the correct auth frame succeeds and emits Prometheus text.
    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .expect("connect");
    protocol::write_auth_frame(&mut stream, "scraper-secret")
        .await
        .expect("send auth");
    protocol::write_request_v1(&mut stream, "/metrics")
        .await
        .expect("send");
    let (status, body) = protocol::read_response_raw(&mut stream)
        .await
        .expect("read response");
    assert_eq!(
        status,
        STATUS_OK,
        "authenticated /metrics should succeed; got error: {:?}",
        String::from_utf8_lossy(&body)
    );
    let text = String::from_utf8(body).expect("utf8");
    assert!(
        text.starts_with("# HELP "),
        "metrics body should start with a HELP comment; got: {}",
        text.lines().next().unwrap_or("")
    );
}
