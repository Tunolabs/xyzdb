//! Minimal HTTP/1.x server multiplexed onto the wire-protocol port. v0.4
//! cp 5.1.1.
//!
//! # Why
//!
//! xyzDB's wire protocol is a custom binary frame on TCP/2505. Operators
//! still want a *browser-accessible* surface for live status (`GET /`),
//! the same JSON `/stats` exposed over the wire, the `/metrics`
//! Prometheus exposition, and `/health` + `/ready` probes. Adding a
//! second HTTP listener is heavier (extra port, second TLS config); the
//! protocol-version byte gives us cheap multiplexing because no V1/V2/V3
//! version byte (0x01/0x02/0x03) and no `AUTH_MAGIC` (0x41) collides
//! with the ASCII letters that begin every HTTP request line. The
//! [`is_http_method_first_byte`] gate is consulted in
//! [`crate::connection::handle_connection`] (and the TLS counterpart);
//! when it matches, the dispatcher hands the stream to
//! [`handle_http_request`] and returns.
//!
//! # Scope
//!
//! - HTTP/1.0 and HTTP/1.1 GET only. POST/PUT/DELETE return 405.
//! - Single request per connection (`Connection: close`). No keep-alive,
//!   no chunked transfer-encoding, no compression.
//! - Header parsing tolerates `\r\n` and `\n` line terminators.
//! - Auth: when the server has `--auth-token` configured, `GET /`, `/stats`,
//!   and `/metrics` demand a bearer token (via the `xyzdb_token` cookie or the
//!   `?token=` query-parameter debug fallback) — they expose engine data, so a
//!   Prometheus scraper must present the token. Only the `/health` and `/ready`
//!   liveness probes stay unauthenticated, so load balancers + Kubernetes
//!   probes work without a credential.
//!
//! # Security notes
//!
//! - The operator HTML embedded via `include_str!` is the *only* place
//!   user-controlled strings enter a DOM. The template's `escapeHtml()`
//!   JS function is the XSS gate; the server does not interpolate
//!   anything into the HTML body itself.
//! - The query-string `?token=` form is a debug ergonomic; tokens leak
//!   into web-server access logs and the browser history. Production
//!   operators should set the `xyzdb_token` cookie.
//! - We never reflect request headers or body into responses.
//! - Maximum request size is bounded by [`MAX_REQUEST_BYTES`] to prevent
//!   slowloris-style buffer exhaustion.

use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use xyzdb_engine::engine::Engine;

/// Hard cap on the size (in bytes) of the HTTP request line + headers.
/// Anything larger and we close the connection with a 431. The operator
/// surface only sends GETs with a couple of headers; the bound is
/// generous on purpose.
const MAX_REQUEST_BYTES: usize = 8 * 1024;

/// Maximum time we'll wait for the full request line + headers to
/// arrive. Browsers send promptly; long stalls are bots or slowloris.
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Embedded operator HTML (cp 5.1.1). The template is fully self-
/// contained — no CDN imports, no external JS — so the binary is the
/// single artefact an operator needs to deploy.
const OPERATOR_HTML: &str = include_str!("../templates/operator.html");

/// Cookie name carrying the bearer token. Matches what the operator HTML
/// would set if a future `/login` flow is added (none in v0.4 — operators
/// set the cookie manually via DevTools).
const AUTH_COOKIE: &str = "xyzdb_token";

/// True if the byte is the first byte of a recognised HTTP request line.
///
/// Matches `G` (GET), `H` (HEAD), `P` (POST/PUT/PATCH), `O` (OPTIONS),
/// `D` (DELETE), `T` (TRACE), `C` (CONNECT). None of these collide with
/// the V1/V2/V3 protocol bytes (`0x01`/`0x02`/`0x03`) nor with
/// `AUTH_MAGIC` (`0x41` = `A`). Method strings starting with any other
/// letter are treated as wire protocol (and rejected by the wire path).
///
/// # Examples
///
/// ```
/// # use xyzdb_server::http::is_http_method_first_byte;
/// assert!(is_http_method_first_byte(b'G'));
/// assert!(is_http_method_first_byte(b'P'));
/// assert!(!is_http_method_first_byte(0x01)); // PROTOCOL_V1
/// assert!(!is_http_method_first_byte(0x41)); // AUTH_MAGIC ('A')
/// ```
#[inline]
pub fn is_http_method_first_byte(b: u8) -> bool {
    matches!(b, b'G' | b'H' | b'P' | b'O' | b'D' | b'T' | b'C')
}

/// Handle a single HTTP request on the given async stream. Generic over
/// the stream so the same code serves plain TCP and TLS.
///
/// `first_byte` is the first byte already consumed by the dispatcher in
/// [`crate::connection`] for protocol detection — we re-prepend it
/// before parsing so the request line is intact.
///
/// The function reads the request, dispatches to one of the route
/// handlers, writes the response, and returns. The connection is
/// closed by the caller on return (we set `Connection: close` so HTTP
/// clients don't expect another request frame).
///
/// # Errors
///
/// Errors are logged and converted to HTTP error responses where
/// possible; the function itself returns `()` so the dispatcher can
/// drop the connection cleanly.
pub async fn handle_http_request<S>(
    engine: &Arc<Engine>,
    stream: &mut S,
    addr: std::net::SocketAddr,
    expected_token: &Arc<Option<String>>,
    first_byte: u8,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let raw = match read_request_head(stream, first_byte).await {
        Ok(b) => b,
        Err(ReadErr::Timeout) => {
            tracing::debug!("HTTP read timeout from {addr}");
            return;
        }
        Err(ReadErr::TooLarge) => {
            let _ = write_simple(
                stream,
                431,
                "Request Header Fields Too Large",
                "text/plain",
                b"431 request too large",
            )
            .await;
            return;
        }
        Err(ReadErr::Io(e)) => {
            tracing::debug!("HTTP read error from {addr}: {e}");
            return;
        }
    };

    let req = match parse_request(&raw) {
        Some(r) => r,
        None => {
            let _ =
                write_simple(stream, 400, "Bad Request", "text/plain", b"400 bad request").await;
            return;
        }
    };

    tracing::debug!("HTTP {} {} from {addr}", req.method, req.path);

    if req.method != "GET" && req.method != "HEAD" {
        let _ = write_simple(
            stream,
            405,
            "Method Not Allowed",
            "text/plain",
            b"405 method not allowed",
        )
        .await;
        return;
    }

    // Routes: split path from optional ?query.
    let (route, query) = match req.path.find('?') {
        Some(i) => (&req.path[..i], &req.path[i + 1..]),
        None => (req.path.as_str(), ""),
    };

    match route {
        "/" => serve_operator_html(stream, expected_token, &req, query).await,
        // `/stats` returns the engine stats snapshot, so it follows the token
        // like `GET /` — the operator page fetches it with the same
        // `xyzdb_token` cookie, an unauthenticated caller gets 401. `/health`,
        // `/ready` and `/metrics` are served on the wire path (V1 query).
        "/stats" => {
            if http_token_ok(stream, expected_token, &req, query).await {
                serve_stats(stream, engine).await;
            }
        }
        _ => {
            let _ = write_simple(stream, 404, "Not Found", "text/plain", b"404 not found").await;
        }
    }
}

// ─── Request reading + parsing ──────────────────────────────────────────────

enum ReadErr {
    Timeout,
    TooLarge,
    Io(std::io::Error),
}

/// Read until `\r\n\r\n` (or `\n\n`) or [`MAX_REQUEST_BYTES`].
async fn read_request_head<S>(stream: &mut S, first_byte: u8) -> Result<Vec<u8>, ReadErr>
where
    S: AsyncRead + Unpin,
{
    let mut buf = Vec::with_capacity(512);
    buf.push(first_byte);

    let read_loop = async {
        let mut chunk = [0u8; 512];
        loop {
            let n = stream.read(&mut chunk).await.map_err(ReadErr::Io)?;
            if n == 0 {
                return Err(ReadErr::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "client closed before request complete",
                )));
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.len() > MAX_REQUEST_BYTES {
                return Err(ReadErr::TooLarge);
            }
            if find_double_crlf(&buf).is_some() {
                return Ok(buf);
            }
        }
    };

    match tokio::time::timeout(READ_TIMEOUT, read_loop).await {
        Ok(r) => r,
        Err(_) => Err(ReadErr::Timeout),
    }
}

/// Locate end-of-headers marker in the buffer.
fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
        return Some(pos + 4);
    }
    if let Some(pos) = buf.windows(2).position(|w| w == b"\n\n") {
        return Some(pos + 2);
    }
    None
}

struct HttpRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
}

impl HttpRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

fn parse_request(raw: &[u8]) -> Option<HttpRequest> {
    let text = std::str::from_utf8(raw).ok()?;
    let mut lines = text.split('\n');

    let request_line = lines.next()?.trim_end_matches('\r');
    let mut parts = request_line.splitn(3, ' ');
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let _version = parts.next()?;

    if method.is_empty() || path.is_empty() {
        return None;
    }

    let mut headers = Vec::new();
    for line in lines {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            break;
        }
        if let Some(idx) = line.find(':') {
            let k = line[..idx].trim().to_string();
            let v = line[idx + 1..].trim().to_string();
            headers.push((k, v));
        }
    }

    Some(HttpRequest {
        method,
        path,
        headers,
    })
}

// ─── Auth helpers ───────────────────────────────────────────────────────────

/// Extract the bearer token from the request, in priority order:
/// 1. `Authorization: Bearer <token>`
/// 2. `Cookie: xyzdb_token=<token>`
/// 3. `?token=<token>` query parameter (debug fallback)
fn extract_token(req: &HttpRequest, query: &str) -> Option<String> {
    if let Some(auth) = req.header("Authorization")
        && let Some(t) = auth.strip_prefix("Bearer ")
    {
        return Some(t.trim().to_string());
    }
    if let Some(cookie_hdr) = req.header("Cookie") {
        for kv in cookie_hdr.split(';') {
            let kv = kv.trim();
            if let Some(t) = kv.strip_prefix(&format!("{AUTH_COOKIE}=")) {
                return Some(t.to_string());
            }
        }
    }
    for kv in query.split('&') {
        if let Some(t) = kv.strip_prefix("token=") {
            return Some(url_decode(t));
        }
    }
    None
}

/// Minimal `application/x-www-form-urlencoded` decoder. Tokens are
/// alphanumeric in our default config but operators may roll their own,
/// so we honour `%XX` and `+`.
fn url_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = &s[i + 1..i + 3];
                if let Ok(b) = u8::from_str_radix(hex, 16) {
                    out.push(b as char);
                    i += 3;
                } else {
                    out.push(bytes[i] as char);
                    i += 1;
                }
            }
            b => {
                out.push(b as char);
                i += 1;
            }
        }
    }
    out
}

/// Constant-time byte slice compare (mirrors `connection::constant_time_eq`).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ─── Route handlers ─────────────────────────────────────────────────────────

/// HTTP auth gate. Returns `true` if the request may proceed — no `--auth-token`
/// configured (open server), or a matching token was presented (Authorization
/// header, `xyzdb_token` cookie, or `?token=`). Otherwise writes a `401` and
/// returns `false`. Used by the routes that expose data (`GET /` and `/stats`);
/// `/health` and `/ready` never pass through it.
async fn http_token_ok<S>(
    stream: &mut S,
    expected_token: &Arc<Option<String>>,
    req: &HttpRequest,
    query: &str,
) -> bool
where
    S: AsyncWrite + Unpin,
{
    let Some(expected) = expected_token.as_ref() else {
        return true; // no token configured — open server
    };
    let ok = matches!(
        extract_token(req, query),
        Some(t) if constant_time_eq(t.as_bytes(), expected.as_bytes())
    );
    if !ok {
        let resp = build_response(
            401,
            "Unauthorized",
            "text/plain; charset=utf-8",
            b"401 unauthorized\n",
            "WWW-Authenticate: Bearer realm=\"xyzdb operator\"\r\n",
        );
        let _ = write_all(stream, &resp).await;
    }
    ok
}

async fn serve_operator_html<S>(
    stream: &mut S,
    expected_token: &Arc<Option<String>>,
    req: &HttpRequest,
    query: &str,
) where
    S: AsyncWrite + Unpin,
{
    if !http_token_ok(stream, expected_token, req, query).await {
        return;
    }

    let resp = build_response(
        200,
        "OK",
        "text/html; charset=utf-8",
        OPERATOR_HTML.as_bytes(),
        "Cache-Control: no-cache, no-store, must-revalidate\r\n\
         X-Content-Type-Options: nosniff\r\n\
         Referrer-Policy: no-referrer\r\n",
    );
    let _ = write_all(stream, &resp).await;
}

async fn serve_stats<S>(stream: &mut S, engine: &Arc<Engine>)
where
    S: AsyncWrite + Unpin,
{
    let snapshot = engine.stats_snapshot();
    let resp = match serde_json::to_vec(&snapshot) {
        Ok(b) => build_response(200, "OK", "application/json; charset=utf-8", &b, ""),
        Err(_) => build_response(
            500,
            "Internal Server Error",
            "text/plain",
            b"stats serialization error",
            "",
        ),
    };
    let _ = write_all(stream, &resp).await;
}

// ─── Response writing ───────────────────────────────────────────────────────
//
// Responses are built into a single `Vec<u8>` first and then handed to a
// tiny generic write helper. Keeping the response-building logic
// non-generic prevents the HTTP route handlers from being monomorphised
// twice (once for plain `TcpStream`, once for the TLS stream wrapper) —
// roughly halving the compiled HTTP footprint.

async fn write_all<S>(stream: &mut S, bytes: &[u8]) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    stream.write_all(bytes).await?;
    stream.flush().await?;
    Ok(())
}

fn build_response(
    status: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
    extra_headers: &str,
) -> Vec<u8> {
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         Server: xyzdb\r\n\
         {extra}\
         \r\n",
        len = body.len(),
        extra = extra_headers,
    );
    let mut out = Vec::with_capacity(head.len() + body.len());
    out.extend_from_slice(head.as_bytes());
    out.extend_from_slice(body);
    out
}

async fn write_simple<S>(
    stream: &mut S,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let resp = build_response(status, reason, content_type, body, "");
    write_all(stream, &resp).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_method_first_bytes_do_not_collide_with_protocol_bytes() {
        // None of 0x01/0x02/0x03 (PROTOCOL_V1/V2/V3) nor 0x41 (AUTH_MAGIC)
        // is detected as HTTP.
        assert!(!is_http_method_first_byte(0x01));
        assert!(!is_http_method_first_byte(0x02));
        assert!(!is_http_method_first_byte(0x03));
        assert!(!is_http_method_first_byte(0x41));
        // Common HTTP methods start with these letters:
        for b in b"GHPOD".iter() {
            assert!(is_http_method_first_byte(*b), "method byte {b:#x}");
        }
    }

    #[test]
    fn parse_request_extracts_method_path_and_headers() {
        let raw = b"GET /stats?token=abc HTTP/1.1\r\nHost: localhost:2505\r\nCookie: xyzdb_token=tok123\r\n\r\n";
        let req = parse_request(raw).expect("parse");
        assert_eq!(req.method, "GET");
        assert_eq!(req.path, "/stats?token=abc");
        assert_eq!(req.header("Host"), Some("localhost:2505"));
        assert_eq!(req.header("cookie"), Some("xyzdb_token=tok123"));
    }

    #[test]
    fn parse_request_rejects_garbage() {
        assert!(parse_request(b"NOTHTTP\r\n\r\n").is_none());
        assert!(parse_request(b"\xff\xfe\xfd").is_none());
    }

    #[test]
    fn extract_token_prefers_authorization_then_cookie_then_query() {
        let req = HttpRequest {
            method: "GET".into(),
            path: "/".into(),
            headers: vec![
                ("Authorization".into(), "Bearer xyz-from-header".into()),
                ("Cookie".into(), "xyzdb_token=cookie-tok; other=v".into()),
            ],
        };
        assert_eq!(
            extract_token(&req, "token=query-tok").as_deref(),
            Some("xyz-from-header")
        );

        let req = HttpRequest {
            method: "GET".into(),
            path: "/".into(),
            headers: vec![("Cookie".into(), "a=b; xyzdb_token=cookie-tok".into())],
        };
        assert_eq!(
            extract_token(&req, "token=query-tok").as_deref(),
            Some("cookie-tok")
        );

        let req = HttpRequest {
            method: "GET".into(),
            path: "/".into(),
            headers: vec![],
        };
        assert_eq!(
            extract_token(&req, "token=query-tok").as_deref(),
            Some("query-tok")
        );
        assert!(extract_token(&req, "").is_none());
    }

    #[test]
    fn url_decode_handles_percent_and_plus() {
        assert_eq!(url_decode("a+b%20c"), "a b c");
        assert_eq!(url_decode("plain"), "plain");
        assert_eq!(url_decode("%41"), "A");
    }

    #[test]
    fn find_double_crlf_handles_both_terminators() {
        assert_eq!(find_double_crlf(b"GET / HTTP/1.1\r\n\r\n"), Some(18));
        assert_eq!(find_double_crlf(b"GET / HTTP/1.1\n\n"), Some(16));
        assert!(find_double_crlf(b"GET / HTTP/1.1\r\n").is_none());
    }
}
