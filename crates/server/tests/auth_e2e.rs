//! Bearer-token auth E2E tests. v0.4 cp 2.2.2.
//!
//! Covers the four required scenarios from the cycle plan §3 Bloque 2 2.2.2:
//! (a) sin token rechazado, (b) token incorrecto rechazado, (c) token
//! correcto aceptado, (d) cliente sin auth contra server sin auth sigue
//! funcionando (back-compat).

use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use xyzdb_engine::engine::Engine;
use xyzdb_server::protocol::{self, STATUS_ERROR, STATUS_OK};

/// Spin up a server bound to 127.0.0.1:0 with the provided expected
/// token. Returns the bound port. Server runs as a background task for
/// the test's lifetime.
async fn start_server(engine: Arc<Engine>, expected_token: Arc<Option<String>>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        loop {
            if let Ok((stream, addr)) = listener.accept().await {
                let engine = engine.clone();
                let token = expected_token.clone();
                tokio::spawn(xyzdb_server::connection::handle_connection(
                    engine, stream, addr, token,
                ));
            }
        }
    });

    port
}

async fn engine_arc() -> Arc<Engine> {
    let dir = tempfile::tempdir().expect("tempdir");
    // Leak the tempdir so the data dir lives for the test's lifetime.
    let path = dir.keep();
    Engine::open(&path).expect("engine open").into_arc()
}

/// (c) Correct token accepted. Server requires `secret123`; client sends
/// `secret123` then a V1 STATS query — succeeds.
#[tokio::test]
async fn test_auth_correct_token_accepted() {
    let engine = engine_arc().await;
    let port = start_server(engine, Arc::new(Some("secret123".to_string()))).await;

    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .expect("connect");

    protocol::write_auth_frame(&mut stream, "secret123")
        .await
        .expect("send auth");
    protocol::write_request_v1(&mut stream, "STATS")
        .await
        .expect("send STATS");

    let (status, body) = protocol::read_response_raw(&mut stream)
        .await
        .expect("read response");
    assert_eq!(
        status,
        STATUS_OK,
        "expected OK; got error: {:?}",
        String::from_utf8_lossy(&body)
    );
    let body_str = String::from_utf8_lossy(&body);
    assert!(
        body_str.contains("\"keyspaces\""),
        "STATS JSON should contain 'keyspaces'; got: {body_str}"
    );
}

/// (b) Wrong token rejected. Server requires `secret123`; client sends
/// `wrong` — server closes with error frame.
#[tokio::test]
async fn test_auth_wrong_token_rejected() {
    let engine = engine_arc().await;
    let port = start_server(engine, Arc::new(Some("secret123".to_string()))).await;

    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .expect("connect");

    protocol::write_auth_frame(&mut stream, "wrong")
        .await
        .expect("send auth");

    // Server should write an error frame and close. We expect to read
    // exactly one response frame with STATUS_ERROR.
    let (status, body) = protocol::read_response_raw(&mut stream)
        .await
        .expect("read response");
    assert_eq!(status, STATUS_ERROR);
    let body_str = String::from_utf8_lossy(&body);
    assert!(
        body_str.contains("auth token mismatch"),
        "rejection should mention mismatch; got: {body_str}"
    );
}

/// (a) Missing token rejected. Server requires auth but client sends a
/// non-allowlisted V1 query directly — server closes with error frame
/// mentioning required auth. (Only the `/health` / `/ready` liveness probes
/// bypass auth — tested in health_ready_e2e.rs; `STATS` and `/metrics` follow
/// the token, tested in metrics_e2e.rs. This test exercises the rejection
/// path.)
#[tokio::test]
async fn test_auth_missing_token_rejected() {
    let engine = engine_arc().await;
    let port = start_server(engine, Arc::new(Some("secret123".to_string()))).await;

    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .expect("connect");

    // Client sends V1 SHOW LOBES (not on the unauth allowlist) without
    // prior AUTH frame.
    protocol::write_request_v1(&mut stream, "SHOW LOBES")
        .await
        .expect("send SHOW LOBES");

    let (status, body) = protocol::read_response_raw(&mut stream)
        .await
        .expect("read response");
    assert_eq!(status, STATUS_ERROR);
    let body_str = String::from_utf8_lossy(&body);
    assert!(
        body_str.contains("requires auth"),
        "rejection should mention auth requirement; got: {body_str}"
    );
}

/// (d) Server without auth still accepts plain clients (back-compat).
#[tokio::test]
async fn test_no_auth_back_compat() {
    let engine = engine_arc().await;
    let port = start_server(engine, Arc::new(None)).await;

    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .expect("connect");

    protocol::write_request_v1(&mut stream, "STATS")
        .await
        .expect("send STATS");

    let (status, body) = protocol::read_response_raw(&mut stream)
        .await
        .expect("read response");
    assert_eq!(status, STATUS_OK);
    let body_str = String::from_utf8_lossy(&body);
    assert!(body_str.contains("\"keyspaces\""), "got: {body_str}");
}

/// (d-extra) Server without auth still accepts a client that sends an
/// AUTH frame anyway. Preserves XYZDB_TOKEN ergonomics for clients that
/// always set the env var even when targeting dev servers.
#[tokio::test]
async fn test_no_auth_silently_consumes_auth_frame() {
    let engine = engine_arc().await;
    let port = start_server(engine, Arc::new(None)).await;

    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .expect("connect");

    protocol::write_auth_frame(&mut stream, "any-token-server-ignores")
        .await
        .expect("send auth");
    protocol::write_request_v1(&mut stream, "STATS")
        .await
        .expect("send STATS");

    let (status, _body) = protocol::read_response_raw(&mut stream)
        .await
        .expect("read response");
    assert_eq!(status, STATUS_OK);
}

/// Constant-time eq invariant: tokens of different length never collide.
/// Sanity check that the server-side comparator does not coerce length.
#[tokio::test]
async fn test_auth_length_mismatch_rejected() {
    let engine = engine_arc().await;
    let port = start_server(engine, Arc::new(Some("longer-secret-token".to_string()))).await;

    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .expect("connect");

    // Shorter token that is a prefix of the expected.
    protocol::write_auth_frame(&mut stream, "longer-secret")
        .await
        .expect("send auth");

    let (status, _body) = protocol::read_response_raw(&mut stream)
        .await
        .expect("read response");
    assert_eq!(status, STATUS_ERROR);
}
