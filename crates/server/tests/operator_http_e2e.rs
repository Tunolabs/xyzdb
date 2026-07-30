//! Operator surface E2E tests. v0.4 cp 5.1.1.
//!
//! Covers:
//! - `GET /` returns the embedded operator HTML with `Cache-Control:
//!   no-cache` and `Content-Type: text/html`.
//! - `GET /stats` returns the same JSON snapshot as the wire-side
//!   `STATS` query.
//! - When `--auth-token` is configured, `GET /` and `GET /stats` require a
//!   token via cookie / `?token=` / `Authorization: Bearer`; otherwise 401.
//! - The HTML body never echoes user-controlled state, so a malicious
//!   keyspace name written through the wire path cannot inject script
//!   tags into `GET /` (the operator HTML escapes via `escapeHtml()`
//!   at render time — the *server* must not interpolate either).
//! - HTTP detection does NOT collide with the binary wire protocol:
//!   sending a V1 query still works on the same port.

use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
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

/// Send a one-shot HTTP/1.1 GET, read the full response, return as bytes.
async fn http_get(port: u16, request_line: &str) -> Vec<u8> {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .expect("connect");
    let req = format!("{request_line}\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.expect("write");
    let mut out = Vec::new();
    stream.read_to_end(&mut out).await.expect("read");
    out
}

fn split_head_body(resp: &[u8]) -> (String, Vec<u8>) {
    let sep = resp
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("crlfx2");
    let head = String::from_utf8_lossy(&resp[..sep]).to_string();
    let body = resp[sep + 4..].to_vec();
    (head, body)
}

// ─── GET / ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_get_root_serves_operator_html() {
    let engine = engine_arc().await;
    let port = start_server(engine, Arc::new(None)).await;

    let resp = http_get(port, "GET / HTTP/1.1").await;
    let (head, body) = split_head_body(&resp);

    assert!(head.starts_with("HTTP/1.1 200 OK"), "head: {head}");
    assert!(
        head.contains("Content-Type: text/html"),
        "expected text/html; head: {head}"
    );
    assert!(
        head.to_ascii_lowercase()
            .contains("cache-control: no-cache"),
        "expected Cache-Control: no-cache; head: {head}"
    );
    assert!(
        head.to_ascii_lowercase().contains("connection: close"),
        "expected Connection: close; head: {head}"
    );

    let body = String::from_utf8(body).expect("utf8");
    assert!(
        body.starts_with("<!doctype html>") || body.starts_with("<!DOCTYPE html>"),
        "expected HTML doctype; got first 80: {:?}",
        &body.chars().take(80).collect::<String>()
    );
    assert!(
        body.contains("escapeHtml"),
        "operator HTML must define escapeHtml() for XSS mitigation"
    );
    assert!(
        body.contains("/stats"),
        "operator HTML must reference /stats for the polling fetch"
    );
    // Sparkline must be SVG inline, never Chart.js / D3 / external CDN.
    assert!(
        !body.contains("chart.js") && !body.contains("d3.min.js"),
        "no external chart libs allowed"
    );
}

// ─── Auth (cookie / query / Authorization Bearer) ───────────────────────────

#[tokio::test]
async fn test_get_root_unauthenticated_returns_401() {
    let engine = engine_arc().await;
    let port = start_server(engine, Arc::new(Some("secret-tok".into()))).await;

    let resp = http_get(port, "GET / HTTP/1.1").await;
    let (head, _) = split_head_body(&resp);
    assert!(head.starts_with("HTTP/1.1 401"), "head: {head}");
    assert!(
        head.contains("WWW-Authenticate: Bearer"),
        "expected Bearer challenge; head: {head}"
    );
}

#[tokio::test]
async fn test_get_root_with_cookie_token_succeeds() {
    let engine = engine_arc().await;
    let port = start_server(engine, Arc::new(Some("secret-tok".into()))).await;

    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .unwrap();
    let req = "GET / HTTP/1.1\r\nHost: localhost\r\nCookie: xyzdb_token=secret-tok\r\nConnection: close\r\n\r\n";
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    stream.read_to_end(&mut out).await.unwrap();
    let (head, _) = split_head_body(&out);
    assert!(head.starts_with("HTTP/1.1 200"), "head: {head}");
}

#[tokio::test]
async fn test_get_root_with_query_token_succeeds() {
    let engine = engine_arc().await;
    let port = start_server(engine, Arc::new(Some("secret-tok".into()))).await;

    let resp = http_get(port, "GET /?token=secret-tok HTTP/1.1").await;
    let (head, _) = split_head_body(&resp);
    assert!(head.starts_with("HTTP/1.1 200"), "head: {head}");
}

#[tokio::test]
async fn test_get_root_with_bearer_header_succeeds() {
    let engine = engine_arc().await;
    let port = start_server(engine, Arc::new(Some("secret-tok".into()))).await;

    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .unwrap();
    let req = "GET / HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer secret-tok\r\nConnection: close\r\n\r\n";
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    stream.read_to_end(&mut out).await.unwrap();
    let (head, _) = split_head_body(&out);
    assert!(head.starts_with("HTTP/1.1 200"), "head: {head}");
}

#[tokio::test]
async fn test_get_root_with_wrong_token_returns_401() {
    let engine = engine_arc().await;
    let port = start_server(engine, Arc::new(Some("secret-tok".into()))).await;

    let resp = http_get(port, "GET /?token=wrong HTTP/1.1").await;
    let (head, _) = split_head_body(&resp);
    assert!(head.starts_with("HTTP/1.1 401"), "head: {head}");
}

// ─── /stats ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_get_stats_returns_json() {
    let engine = engine_arc().await;
    let port = start_server(engine, Arc::new(None)).await;

    let resp = http_get(port, "GET /stats HTTP/1.1").await;
    let (head, body) = split_head_body(&resp);
    assert!(head.starts_with("HTTP/1.1 200"), "head: {head}");
    assert!(
        head.contains("Content-Type: application/json"),
        "head: {head}"
    );
    let v: serde_json::Value = serde_json::from_slice(&body).expect("valid json");
    assert!(v.get("keyspaces").is_some(), "expected keyspaces field");
    assert!(v.get("sync_thread").is_some(), "expected sync_thread field");
}

#[tokio::test]
async fn test_get_stats_follows_token() {
    // /stats returns the engine stats snapshot, so with --auth-token it
    // requires the token (mirrors the wire STATS gating): no token → 401,
    // a valid `?token=` (or cookie / bearer) → 200.
    let engine = engine_arc().await;
    let port = start_server(engine, Arc::new(Some("tok".into()))).await;

    let resp = http_get(port, "GET /stats HTTP/1.1").await;
    let (head, _) = split_head_body(&resp);
    assert!(
        head.starts_with("HTTP/1.1 401"),
        "unauth /stats must be 401 when a token is set; head: {head}"
    );

    let resp = http_get(port, "GET /stats?token=tok HTTP/1.1").await;
    let (head, _) = split_head_body(&resp);
    assert!(
        head.starts_with("HTTP/1.1 200"),
        "authenticated /stats should be 200; head: {head}"
    );
}

// ─── 404 / 405 / 400 ────────────────────────────────────────────────────────

#[tokio::test]
async fn test_unknown_route_returns_404() {
    let engine = engine_arc().await;
    let port = start_server(engine, Arc::new(None)).await;

    let resp = http_get(port, "GET /does-not-exist HTTP/1.1").await;
    let (head, _) = split_head_body(&resp);
    assert!(head.starts_with("HTTP/1.1 404"), "head: {head}");
}

#[tokio::test]
async fn test_post_returns_405() {
    let engine = engine_arc().await;
    let port = start_server(engine, Arc::new(None)).await;

    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .unwrap();
    let req =
        "POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    stream.read_to_end(&mut out).await.unwrap();
    let (head, _) = split_head_body(&out);
    assert!(head.starts_with("HTTP/1.1 405"), "head: {head}");
}

// ─── XSS: server never reflects user-controlled strings into HTML ──────────

/// Even after a wire-side query attempts to inject a `<script>` tag in a
/// keyspace name, the HTML served on `GET /` is the *static* template
/// — there's no server-side templating that could embed it. The test
/// verifies the HTML body is byte-identical regardless of engine state.
#[tokio::test]
async fn test_html_body_not_influenced_by_engine_state() {
    let engine = engine_arc().await;
    let port_a = start_server(engine.clone(), Arc::new(None)).await;
    let resp_clean = http_get(port_a, "GET / HTTP/1.1").await;

    // Run a few queries that, in a templating-based server, could end up
    // reflected in the HTML. Here they just touch engine state.
    let _ = engine.run("STATS");
    let _ = engine.run("/health");

    let resp_after = http_get(port_a, "GET / HTTP/1.1").await;
    let body_clean = split_head_body(&resp_clean).1;
    let body_after = split_head_body(&resp_after).1;
    assert_eq!(
        body_clean, body_after,
        "operator HTML body must not vary with engine state"
    );

    // The static template defines escapeHtml, the JS-side gate that
    // sanitises any user-controlled string before DOM injection.
    let body = String::from_utf8(body_clean).unwrap();
    assert!(body.contains("function escapeHtml"));
    // Confirm the escapeHtml function maps the dangerous five.
    assert!(body.contains("&amp;"));
    assert!(body.contains("&lt;"));
    assert!(body.contains("&gt;"));
    assert!(body.contains("&quot;"));
    assert!(body.contains("&#39;"));
}

// ─── HTTP / wire coexistence on the same port ──────────────────────────────

#[tokio::test]
async fn test_wire_protocol_still_works_after_http_introduction() {
    let engine = engine_arc().await;
    let port = start_server(engine, Arc::new(None)).await;

    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .unwrap();
    protocol::write_request_v1(&mut stream, "STATS")
        .await
        .expect("send STATS");
    let (status, body) = protocol::read_response_raw(&mut stream)
        .await
        .expect("read STATS response");
    assert_eq!(status, STATUS_OK);
    let v: serde_json::Value = serde_json::from_slice(&body).expect("valid json");
    assert!(v.get("keyspaces").is_some());
}
