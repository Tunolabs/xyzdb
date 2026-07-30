//! `/health` and `/ready` endpoint E2E tests. v0.4 cp 2.2.3.
//!
//! Both endpoints MUST be reachable WITHOUT auth even when the server
//! is configured with `--auth-token`. Load balancer health probes
//! should not need to know the bearer token.

use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use xyzdb_engine::engine::Engine;
use xyzdb_server::protocol::{self, STATUS_OK};

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
    let path = dir.keep();
    Engine::open(&path).expect("engine open").into_arc()
}

/// `/health` accessible without auth even when server requires auth.
#[tokio::test]
async fn test_health_bypasses_auth() {
    let engine = engine_arc().await;
    let port = start_server(engine, Arc::new(Some("secret".to_string()))).await;

    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .expect("connect");

    // No auth frame — go straight to V1 with /health.
    protocol::write_request_v1(&mut stream, "/health")
        .await
        .expect("send /health");

    let (status, body) = protocol::read_response_raw(&mut stream)
        .await
        .expect("read response");
    assert_eq!(status, STATUS_OK);
    let body_str = String::from_utf8_lossy(&body);
    assert!(
        body_str.contains("\"alive\": true"),
        "expected alive:true; got: {body_str}"
    );
}

/// `/ready` accessible without auth, returns ready:true on a fresh
/// engine (durability=durable default — group commit thread alive).
#[tokio::test]
async fn test_ready_bypasses_auth() {
    let engine = engine_arc().await;
    let port = start_server(engine, Arc::new(Some("secret".to_string()))).await;

    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .expect("connect");

    protocol::write_request_v1(&mut stream, "/ready")
        .await
        .expect("send /ready");

    let (status, body) = protocol::read_response_raw(&mut stream)
        .await
        .expect("read response");
    let body_str = String::from_utf8_lossy(&body);
    // Status should be OK; body should report ready:true.
    assert_eq!(status, STATUS_OK, "got body: {body_str}");
    assert!(
        body_str.contains("\"ready\": true"),
        "expected ready:true; got: {body_str}"
    );
}

/// Non-allowlisted query without auth → rejected.
#[tokio::test]
async fn test_non_health_query_still_requires_auth() {
    let engine = engine_arc().await;
    let port = start_server(engine, Arc::new(Some("secret".to_string()))).await;

    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .expect("connect");

    protocol::write_request_v1(&mut stream, "SHOW LOBES")
        .await
        .expect("send SHOW LOBES");

    let (status, body) = protocol::read_response_raw(&mut stream)
        .await
        .expect("read response");
    assert_ne!(status, STATUS_OK);
    let body_str = String::from_utf8_lossy(&body);
    assert!(
        body_str.contains("requires auth"),
        "expected auth-required error; got: {body_str}"
    );
}

/// /health on a server WITHOUT auth also works (no special-case needed
/// — same endpoint, same response).
#[tokio::test]
async fn test_health_works_without_auth_configured() {
    let engine = engine_arc().await;
    let port = start_server(engine, Arc::new(None)).await;

    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .expect("connect");

    protocol::write_request_v1(&mut stream, "/health")
        .await
        .expect("send /health");

    let (status, body) = protocol::read_response_raw(&mut stream)
        .await
        .expect("read response");
    assert_eq!(status, STATUS_OK);
    let body_str = String::from_utf8_lossy(&body);
    assert!(body_str.contains("\"alive\": true"));
}

/// Authenticated client can also query /health and /ready (they remain
/// available inside an authenticated session, not just at handshake).
#[tokio::test]
async fn test_health_inside_authenticated_session() {
    let engine = engine_arc().await;
    let port = start_server(engine, Arc::new(Some("secret".to_string()))).await;

    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .expect("connect");

    // Authenticate first.
    protocol::write_auth_frame(&mut stream, "secret")
        .await
        .expect("send auth");
    // Send /health on an authenticated connection.
    protocol::write_request_v1(&mut stream, "/health")
        .await
        .expect("send /health");

    let (status, body) = protocol::read_response_raw(&mut stream)
        .await
        .expect("read response");
    assert_eq!(status, STATUS_OK);
    assert!(String::from_utf8_lossy(&body).contains("\"alive\": true"));
}
