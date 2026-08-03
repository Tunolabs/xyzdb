use crate::json_response;
use crate::protocol::{self, FORMAT_BINARY, STATUS_ERROR, STATUS_OK};
use crate::response;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::net::TcpStream;
use xytalk_parser::ast::Statement;
use xyzdb_core::value::Value;
use xyzdb_engine::engine::Engine;
use xyzdb_engine::ops::put::BulkRecord;

const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Handle a single plain-TCP client connection.
///
/// Supports the full wire surface including V4 chunked streaming, which
/// requires raw-fd access for the sync-writer path inside
/// `handle_streaming_scan`. TLS-wrapped connections go through
/// [`handle_tls_connection`] which shares the V1/V2/V3 sync paths via the
/// generic `process_request_sync` helper but rejects chunked streaming.
pub async fn handle_connection(
    engine: Arc<Engine>,
    mut stream: TcpStream,
    addr: std::net::SocketAddr,
    expected_token: Arc<Option<String>>,
) {
    let _ = stream.set_nodelay(true);
    tracing::info!("Client connected: {addr}");

    // Peek the first byte once — every dispatch path below needs it.
    let first = match tokio::time::timeout(IDLE_TIMEOUT, stream.read_u8()).await {
        Ok(Ok(b)) => b,
        Ok(Err(_)) => return,
        Err(_) => {
            tracing::debug!("Client {addr} idle timeout on handshake");
            return;
        }
    };

    // HTTP detection. Browsers/curl hitting / want HTML back, and the
    // operator page fetches /stats over HTTP. Only common HTTP methods are
    // recognised — they don't collide with the 0x01/0x02/0x03/0x04
    // protocol-version bytes nor with AUTH_MAGIC (0x41).
    if crate::http::is_http_method_first_byte(first) {
        crate::http::handle_http_request(&engine, &mut stream, addr, &expected_token, first).await;
        return;
    }

    // v0.4 cp 2.2.2 + 2.2.3: bearer-token auth gate with /health-/ready
    // bypass. Returns Continue(version) for normal flow,
    // BypassedHandled for unauth probe (drop connection), or Reject.
    let version = match auth_handshake(&mut stream, addr, &engine, &expected_token, first).await {
        AuthOutcome::Continue(b) => b,
        AuthOutcome::BypassedHandled | AuthOutcome::Reject => return,
    };

    if version == protocol::PROTOCOL_V3 {
        if let Err(e) = handle_v3_bulk_load(&engine, &mut stream, addr).await {
            tracing::error!("V3 bulk load error from {addr}: {e}");
        }
        return;
    }

    let first_req = match read_first_v1v2_request(&mut stream, version, addr).await {
        Some(r) => r,
        None => return,
    };

    // Plain TCP path supports streaming; `process_request` covers it.
    if !process_request(&engine, &mut stream, addr, &first_req).await {
        return;
    }

    loop {
        let request = match protocol::read_request(&mut stream).await {
            Ok(Some(r)) => r,
            Ok(None) => {
                tracing::info!("Client disconnected: {addr}");
                return;
            }
            Err(e) => {
                tracing::error!("Read error from {addr}: {e}");
                return;
            }
        };

        if !process_request(&engine, &mut stream, addr, &request).await {
            return;
        }
    }
}

/// Handle a TLS-wrapped client connection. v0.4 item 3.
///
/// Shares the V1/V2 sync request path with `handle_connection` via
/// `process_request_sync`. V4 chunked streaming is rejected with an error
/// frame because the plain-TCP streaming path uses `as_raw_fd` + sync
/// writes that bypass TLS record framing. Streaming SCAN over TLS is v0.5.
pub async fn handle_tls_connection<S>(
    engine: Arc<Engine>,
    mut stream: S,
    addr: std::net::SocketAddr,
    expected_token: Arc<Option<String>>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    tracing::info!("TLS client connected: {addr}");

    // Peek the first byte once — every dispatch path below needs it.
    let first = match tokio::time::timeout(IDLE_TIMEOUT, stream.read_u8()).await {
        Ok(Ok(b)) => b,
        Ok(Err(_)) => return,
        Err(_) => {
            tracing::debug!("TLS client {addr} idle timeout on handshake");
            return;
        }
    };

    // v0.4 cp 5.1.1: HTTP detection over TLS (HTTPS). Same byte-set as
    // the plain-TCP path; the dispatcher below is generic over the stream.
    if crate::http::is_http_method_first_byte(first) {
        crate::http::handle_http_request(&engine, &mut stream, addr, &expected_token, first).await;
        return;
    }

    let version = match auth_handshake(&mut stream, addr, &engine, &expected_token, first).await {
        AuthOutcome::Continue(b) => b,
        AuthOutcome::BypassedHandled | AuthOutcome::Reject => return,
    };

    if version == protocol::PROTOCOL_V3 {
        if let Err(e) = handle_v3_bulk_load(&engine, &mut stream, addr).await {
            tracing::error!("TLS V3 bulk load error from {addr}: {e}");
        }
        return;
    }

    let first_req = match read_first_v1v2_request(&mut stream, version, addr).await {
        Some(r) => r,
        None => return,
    };

    if !process_request_sync(&engine, &mut stream, addr, &first_req).await {
        return;
    }

    loop {
        let request = match protocol::read_request(&mut stream).await {
            Ok(Some(r)) => r,
            Ok(None) => {
                tracing::info!("TLS client disconnected: {addr}");
                return;
            }
            Err(e) => {
                tracing::error!("TLS read error from {addr}: {e}");
                return;
            }
        };

        if !process_request_sync(&engine, &mut stream, addr, &request).await {
            return;
        }
    }
}

/// Outcome of `auth_handshake`. Distinguishes "auth ok, dispatch continues"
/// from "auth missing but client requested an unauthenticated endpoint".
enum AuthOutcome {
    /// Auth either passed or was not required; the contained byte is the
    /// protocol-version byte that the dispatcher should consume next.
    Continue(u8),
    /// Auth was required and missing, but the client sent a V1/V2 frame
    /// containing a whitelisted unauthenticated query (e.g. `/health`).
    /// `auth_handshake` already wrote the response; the caller should
    /// drop the connection.
    BypassedHandled,
    /// Auth was required and missing or mismatched. `auth_handshake`
    /// already wrote an error frame; the caller should drop the connection.
    Reject,
}

/// v0.4 cp 2.2.2 + 2.2.3: read the optional auth frame (marker
/// `AUTH_MAGIC` = `0x41`), validate against `expected_token` if
/// configured, return the next byte as the protocol version.
///
/// **Auth-bypass for liveness probes (cp 2.2.3)**: when the server
/// requires auth and the client sends a V1 or V2 frame WITHOUT the auth
/// preamble, the handshake reads the frame and checks whether the query
/// is a liveness probe (`is_liveness_probe`: `/health`, `/ready`). If
/// yes, it is dispatched inline (single-shot) and the connection closes —
/// load balancers and Kubernetes probes reach `/health` / `/ready`
/// without the bearer token. Anything else, including `STATS` and
/// `/metrics`, is rejected with the auth error: authentication applies to
/// everything except liveness.
async fn auth_handshake<S>(
    stream: &mut S,
    addr: std::net::SocketAddr,
    engine: &Arc<Engine>,
    expected_token: &Arc<Option<String>>,
    first: u8,
) -> AuthOutcome
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    if first == protocol::AUTH_MAGIC {
        // Read the token body and validate.
        let presented = match protocol::read_auth_frame_body(stream).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("Malformed auth frame from {addr}: {e}");
                let _ = protocol::write_response_bytes(
                    stream,
                    STATUS_ERROR,
                    b"ERROR: malformed auth frame",
                )
                .await;
                return AuthOutcome::Reject;
            }
        };
        if let Some(expected) = expected_token.as_ref() {
            // Constant-time compare to avoid timing side-channel.
            if !constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
                tracing::warn!("Auth rejected from {addr}: token mismatch");
                let _ = protocol::write_response_bytes(
                    stream,
                    STATUS_ERROR,
                    b"ERROR: auth token mismatch",
                )
                .await;
                return AuthOutcome::Reject;
            }
            tracing::debug!("Auth accepted from {addr}");
        } else {
            // No auth configured — silently ignore the frame for client
            // ergonomics (XYZDB_TOKEN set against a dev server).
            tracing::trace!(
                "Auth frame received from {addr} but server has no --auth-token; ignoring"
            );
        }
        // Read the actual protocol version byte after the auth frame.
        match stream.read_u8().await {
            Ok(b) => AuthOutcome::Continue(b),
            Err(_) => AuthOutcome::Reject,
        }
    } else if expected_token.is_some() {
        // Auth required and client did not send the auth frame. Check
        // whether the client is making a probe request on an unauth-
        // enticated endpoint (load balancer health/ready check).
        if first == protocol::PROTOCOL_V1 || first == protocol::PROTOCOL_V2 {
            // Read the V1/V2 frame inline using the same logic as
            // read_first_v1v2_request, then check the allowlist.
            let probe_req = match read_first_v1v2_request(stream, first, addr).await {
                Some(r) => r,
                None => return AuthOutcome::Reject,
            };
            if is_liveness_probe(probe_req.query.trim()) {
                handle_unauth_probe(engine, stream, addr, &probe_req).await;
                return AuthOutcome::BypassedHandled;
            }
            // Authenticated request without auth → reject.
            tracing::warn!(
                "Auth required but client {addr} sent V{} query without auth frame: query is not on unauthenticated allowlist",
                first
            );
            let _ = protocol::write_response_bytes(
                stream,
                STATUS_ERROR,
                b"ERROR: server requires auth (Authorization: Bearer <token>); send AUTH_MAGIC frame first",
            )
            .await;
            AuthOutcome::Reject
        } else {
            tracing::warn!(
                "Auth required but client {addr} sent unsupported first byte 0x{:02x}",
                first
            );
            let _ = protocol::write_response_bytes(
                stream,
                STATUS_ERROR,
                b"ERROR: server requires auth (Authorization: Bearer <token>); send AUTH_MAGIC frame first",
            )
            .await;
            AuthOutcome::Reject
        }
    } else {
        // No auth configured, no auth frame — first byte is the protocol version.
        AuthOutcome::Continue(first)
    }
}

/// The liveness-probe allowlist — the only queries that bypass
/// authentication when a token is configured: `/health`, `/ready`,
/// `HEALTH`, `READY` (case-insensitive). They expose no data, and load
/// balancers / Kubernetes probes must reach them without the token.
/// Everything else follows the token.
fn is_liveness_probe(query: &str) -> bool {
    let upper = query.to_uppercase();
    matches!(upper.as_str(), "/HEALTH" | "HEALTH" | "/READY" | "READY")
}

/// True if the query is a built-in probe served by [`handle_unauth_probe`]
/// rather than by the xyTalk parser: the liveness probes plus `STATS`,
/// `SHOW STATS`, `/metrics`, `METRICS`. This only *routes* the request — it
/// grants no auth bypass. `STATS` / `SHOW STATS` / `/metrics` return the engine
/// stats snapshot and so require authentication when a token is configured;
/// they reach this path only on an already-authenticated connection (or when no
/// token is set). See [`is_liveness_probe`] for the auth bypass.
fn is_probe_query(query: &str) -> bool {
    let upper = query.to_uppercase();
    matches!(
        upper.as_str(),
        "/HEALTH" | "HEALTH" | "/READY" | "READY" | "STATS" | "SHOW STATS" | "/METRICS" | "METRICS"
    )
}

/// Dispatch a single unauthenticated probe request inline. Writes the
/// response frame and returns. Caller drops the connection afterwards
/// (probes are single-shot).
async fn handle_unauth_probe<S>(
    engine: &Arc<Engine>,
    stream: &mut S,
    addr: std::net::SocketAddr,
    request: &protocol::Request,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let upper = request.query.trim().to_uppercase();
    let (status, payload): (u8, Vec<u8>) = match upper.as_str() {
        "STATS" | "SHOW STATS" => {
            let snapshot = engine.stats_snapshot();
            match serde_json::to_vec(&snapshot) {
                Ok(b) => (STATUS_OK, b),
                Err(e) => (
                    STATUS_ERROR,
                    format!("stats serialization error: {e}").into_bytes(),
                ),
            }
        }
        "/HEALTH" | "HEALTH" => (STATUS_OK, br#"{"alive": true}"#.to_vec()),
        "/READY" | "READY" => readiness_response(engine),
        "/METRICS" | "METRICS" => {
            // v0.4 cp 2.2.4: Prometheus exposition format.
            let snapshot = engine.stats_snapshot();
            let body = crate::metrics::serialize_stats_to_prometheus(&snapshot);
            (STATUS_OK, body.into_bytes())
        }
        _ => (STATUS_ERROR, b"ERROR: not on allowlist".to_vec()),
    };
    if let Err(e) = protocol::write_response_bytes(stream, status, &payload).await {
        tracing::warn!("Probe write error to {addr}: {e}");
    }
}

/// Return `(STATUS_OK, {"ready": true, ...})` if the engine is ready to
/// serve queries, or `(STATUS_ERROR, {"ready": false, "reason": "..."})`
/// otherwise. v0.4 cp 2.2.3.
///
/// Heuristics:
/// - `synced_epoch` heartbeat freshness < 5 s (group-commit thread alive).
/// - `pending_epoch == synced_epoch` is NOT required — a small backlog
///   is normal under write load. Just liveness of the sync thread.
/// - BULKMODE-active is currently not exposed via stats; the readiness
///   check ignores it for now and relies on heartbeat as the dominant
///   signal. Tracked as cycle plan §8 finding for a future refinement.
fn readiness_response(engine: &Arc<Engine>) -> (u8, Vec<u8>) {
    let snapshot = engine.stats_snapshot();
    let last_sync_ms = snapshot.sync_thread.last_successful_sync_ts_ms;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    // last_sync_ms == 0 happens in two cases: (a) durability is Batched
    // or Async (group-commit disabled — normal); (b) Durable mode but no
    // successful sync yet. Distinguishing requires durability mode in
    // /stats, not exposed today; for v0.4 we accept (a) as ready.
    if last_sync_ms != 0 && now_ms.saturating_sub(last_sync_ms) > 5_000 {
        let body = format!(
            r#"{{"ready": false, "reason": "sync_thread heartbeat stale", "last_sync_ts_ms": {last_sync_ms}, "now_ms": {now_ms}}}"#
        );
        return (STATUS_ERROR, body.into_bytes());
    }
    (STATUS_OK, br#"{"ready": true}"#.to_vec())
}

/// Constant-time byte slice compare. Returns true iff `a == b`. Avoids
/// the timing side-channel of `==` short-circuiting on first mismatch,
/// which leaks the length of the shared prefix.
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

/// Read the first V1/V2 request (after the version byte was consumed
/// during dispatch). Returns None on protocol error / disconnect — caller
/// should drop the connection on None.
async fn read_first_v1v2_request<S>(
    stream: &mut S,
    version: u8,
    addr: std::net::SocketAddr,
) -> Option<protocol::Request>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let (format, has_params) = if version == protocol::PROTOCOL_V2 {
        (stream.read_u8().await.ok()?, false)
    } else if version == protocol::PROTOCOL_V4 {
        (stream.read_u8().await.ok()?, true) // S1: query + bound params
    } else if version == protocol::PROTOCOL_V1 {
        (protocol::FORMAT_TEXT, false)
    } else {
        tracing::error!("Unsupported protocol version {version} from {addr}");
        return None;
    };
    let length = stream.read_u32().await.ok()?;
    let mut buf = vec![0u8; length as usize];
    stream.read_exact(&mut buf).await.ok()?;
    let query = String::from_utf8(buf).ok()?;
    let params_json = if has_params {
        let plen = stream.read_u32().await.ok()?;
        if plen > protocol::MAX_FRAME_SIZE {
            tracing::error!("Params frame too large ({plen}) from {addr}");
            return None;
        }
        let mut pbuf = vec![0u8; plen as usize];
        stream.read_exact(&mut pbuf).await.ok()?;
        Some(pbuf)
    } else {
        None
    };
    Some(protocol::Request {
        query,
        format,
        params_json,
    })
}

/// Process a single V1/V2 request on a plain `TcpStream`. Supports V4
/// chunked streaming. Returns false if the connection should close.
async fn process_request(
    engine: &Arc<Engine>,
    stream: &mut TcpStream,
    addr: std::net::SocketAddr,
    request: &protocol::Request,
) -> bool {
    // V4: streaming path is plain-TCP-only (sync writes via dup_fd). TLS
    // path goes through `process_request_sync` and rejects chunked formats.
    if protocol::is_chunked_format(request.format)
        && request.params_json.is_none() // params must bind via the sync path
        && let Ok(stmt) = xytalk_parser::parse(request.query.trim())
        && is_streamable(&stmt)
    {
        let op_start = Instant::now();
        let stream_result = handle_streaming_scan(engine, stream, &stmt, request.format).await;
        engine.throttle.record_read(op_start.elapsed());
        if let Err(e) = stream_result {
            tracing::error!("Streaming error to {addr}: {e}");
            return false;
        }
        return true;
    }
    process_request_sync(engine, stream, addr, request).await
}

/// Process a single V1/V2 request on any async stream, NO streaming. Used
/// for both plain-TCP non-streaming requests and the TLS path.
async fn process_request_sync<S>(
    engine: &Arc<Engine>,
    stream: &mut S,
    addr: std::net::SocketAddr,
    request: &protocol::Request,
) -> bool
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let query_trimmed = request.query.trim();
    if query_trimmed.is_empty() {
        if let Err(e) = protocol::write_response_bytes(stream, STATUS_OK, b"").await {
            tracing::error!("Write error to {addr}: {e}");
            return false;
        }
        return true;
    }

    // Built-in probes (/health, /ready, STATS, SHOW STATS, /metrics)
    // short-circuit before the parser/engine path: their response body is
    // structured JSON (or Prometheus text), not a `QueryResult`. Reaching here
    // means the connection is already authenticated, or the server has no
    // token. /health and /ready are additionally auth-bypassed at handshake
    // time; STATS / SHOW STATS / /metrics are not — they require authentication
    // when a token is configured.
    if is_probe_query(query_trimmed) {
        let probe_req = protocol::Request {
            query: request.query.clone(),
            format: request.format,
            params_json: None,
        };
        handle_unauth_probe(engine, stream, addr, &probe_req).await;
        return true;
    }

    let is_write = is_write_query(query_trimmed);

    if is_write {
        let delay = engine.throttle.write_delay();
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
    }

    // v0.4 cp 3.2.2: SNAPSHOT CREATE <name> short-circuit. Privileged
    // (NOT on unauth allowlist) — a request reaching this point has
    // already passed the auth gate. Wire form: `SNAPSHOT CREATE <name>`
    // (case-insensitive verb; name is the trailing token).
    if let Some(name) = parse_snapshot_create(query_trimmed) {
        let result = engine.create_snapshot(&name);
        let (status, payload): (u8, Vec<u8>) = match result {
            Ok(meta) => match serde_json::to_vec(&meta) {
                Ok(b) => (STATUS_OK, b),
                Err(e) => (
                    STATUS_ERROR,
                    format!("snapshot meta serialize error: {e}").into_bytes(),
                ),
            },
            Err(e) => (STATUS_ERROR, format!("ERROR: {e}").into_bytes()),
        };
        if let Err(e) = protocol::write_response_bytes(stream, status, &payload).await {
            tracing::error!("Write error to {addr}: {e}");
            return false;
        }
        return true;
    }

    // Chunked streaming over a non-TcpStream (i.e. TLS): reject.
    if protocol::is_chunked_format(request.format) {
        let err_msg = "ERROR: chunked streaming format unsupported on a TLS connection; \
             use a non-chunked request format over TLS";
        if let Err(e) =
            protocol::write_response_bytes(stream, STATUS_ERROR, err_msg.as_bytes()).await
        {
            tracing::error!("Write error to {addr}: {e}");
            return false;
        }
        return true;
    }

    let op_start = Instant::now();
    let result = match &request.params_json {
        Some(bytes) => match params_from_json(bytes) {
            Ok(p) => engine.run_with_params(query_trimmed, &p),
            Err(e) => Err(e),
        },
        None => engine.run(query_trimmed),
    };
    let op_latency = op_start.elapsed();

    if is_write {
        engine.throttle.record_write(op_latency);
    } else {
        engine.throttle.record_read(op_latency);
    }

    // Evaluate throttle based on LSM compaction pressure (every ~100 ops)
    if op_start.elapsed().subsec_micros().is_multiple_of(100) {
        let (l0, sealed) = engine.lsm_pressure();
        engine.throttle.evaluate_lsm(l0, sealed);
    }

    let is_pull = is_pull_query(query_trimmed);

    const FORMAT_JSON: u8 = 0x02;
    let (status, payload) = match result {
        Ok(ref qr) => {
            if request.format == FORMAT_BINARY {
                match bincode::serialize(qr) {
                    Ok(bytes) => (STATUS_OK, bytes),
                    Err(e) => (
                        STATUS_ERROR,
                        format!("Serialization error: {e}").into_bytes(),
                    ),
                }
            } else if request.format == FORMAT_JSON {
                let root_lid = extract_root_lid(qr);
                (
                    STATUS_OK,
                    json_response::serialize_json(qr, op_latency, is_pull, root_lid.as_ref()),
                )
            } else {
                (STATUS_OK, response::format_result(qr).into_bytes())
            }
        }
        Err(e) => {
            let err_msg = format!("{e}");
            if request.format == FORMAT_JSON {
                (STATUS_ERROR, json_response::serialize_json_error(&err_msg))
            } else {
                (STATUS_ERROR, format!("ERROR: {err_msg}").into_bytes())
            }
        }
    };

    if let Err(e) = protocol::write_response_bytes(stream, status, &payload).await {
        tracing::error!("Write error to {addr}: {e}");
        return false;
    }
    true
}

// ─── V5: Protocol V3 — Binary Bulk Load ──────────────────────────────────────

/// Handle a V3 bulk load connection. Reads header + batch frames, writes
/// records. Generic over the stream so the same impl serves plain TCP and
/// TLS — all I/O goes through the async stream traits, no `as_raw_fd` use.
async fn handle_v3_bulk_load<S>(
    engine: &Arc<Engine>,
    stream: &mut S,
    addr: std::net::SocketAddr,
) -> Result<(), Box<dyn std::error::Error>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let header = protocol::read_v3_header(stream).await?;
    tracing::info!(
        "V3 bulk load from {addr}: lobe='{}' flags=0x{:02x}",
        header.lobe_name,
        header.flags
    );

    let mut total_records = 0u64;

    loop {
        let frame = match protocol::read_v3_batch_frame(stream).await? {
            Some(f) => f,
            None => break, // End of stream
        };

        let records = parse_v3_batch_payload(&frame.payload, frame.record_count)?;
        let engine_ref = engine.clone();
        let lobe = header.lobe_name.clone();

        // Execute bulk insert in blocking task (Turba writes are sync)
        let result = tokio::task::spawn_blocking(move || {
            xyzdb_engine::ops::put::execute_bulk_insert(&engine_ref, &lobe, records)
        })
        .await??;

        total_records += result.count as u64;

        protocol::write_v3_batch_response(
            stream,
            STATUS_OK,
            result.count,
            result.first_lid.raw(),
            result.last_lid.raw(),
        )
        .await?;
    }

    tracing::info!(
        "V3 bulk load complete from {addr}: {} records in '{}'",
        total_records,
        header.lobe_name
    );
    Ok(())
}

/// Parse a V3 batch payload into BulkRecords.
/// Record format: [gravity_count:u8]
///   [gravity entries: [name_len:u16 BE][name:bytes][value_len:u16 BE][value:postcard(Value)]]...
///   [fields_len:u32 BE][fields:postcard(BTreeMap<String,Value>)]
fn parse_v3_batch_payload(
    payload: &[u8],
    expected_count: u32,
) -> Result<Vec<BulkRecord>, Box<dyn std::error::Error>> {
    // Cap the pre-allocation by the payload length. `expected_count` is an
    // untrusted u32 from the frame header; without a bound a value near u32::MAX
    // would size the Vec to billions of entries and OOM the process before the
    // per-record truncation checks below ever run. A record consumes at least
    // one byte, so the count can never legitimately exceed `payload.len()`; the
    // Vec still grows on its own if the honest count is higher than this hint.
    let cap = (expected_count as usize).min(payload.len());
    let mut records = Vec::with_capacity(cap);
    let mut pos = 0;

    for _ in 0..expected_count {
        if pos >= payload.len() {
            return Err("Truncated batch payload".into());
        }

        // Gravity fields
        let gravity_count = payload[pos] as usize;
        pos += 1;
        let mut gravity_fields = Vec::with_capacity(gravity_count);
        for _ in 0..gravity_count {
            if pos + 2 > payload.len() {
                return Err("Truncated gravity name len".into());
            }
            let name_len = u16::from_be_bytes([payload[pos], payload[pos + 1]]) as usize;
            pos += 2;
            if pos + name_len > payload.len() {
                return Err("Truncated gravity name".into());
            }
            let name = std::str::from_utf8(&payload[pos..pos + name_len])?.to_string();
            pos += name_len;

            if pos + 2 > payload.len() {
                return Err("Truncated gravity value len".into());
            }
            let val_len = u16::from_be_bytes([payload[pos], payload[pos + 1]]) as usize;
            pos += 2;
            if pos + val_len > payload.len() {
                return Err("Truncated gravity value".into());
            }
            let value: Value = postcard::from_bytes(&payload[pos..pos + val_len])?;
            pos += val_len;

            gravity_fields.push((name, value));
        }

        // Fields
        if pos + 4 > payload.len() {
            return Err("Truncated fields len".into());
        }
        let fields_len = u32::from_be_bytes([
            payload[pos],
            payload[pos + 1],
            payload[pos + 2],
            payload[pos + 3],
        ]) as usize;
        pos += 4;
        if pos + fields_len > payload.len() {
            return Err("Truncated fields".into());
        }
        let fields: BTreeMap<String, Value> =
            postcard::from_bytes(&payload[pos..pos + fields_len])?;
        pos += fields_len;

        records.push(BulkRecord {
            fields,
            gravity_fields,
        });
    }

    Ok(records)
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// S1: decode the V4 params JSON object into engine `Value`s for `$param`
/// binding. Errors (not panics) on malformed JSON or a non-object body.
fn params_from_json(
    bytes: &[u8],
) -> std::result::Result<
    std::collections::HashMap<String, xyzdb_core::value::Value>,
    xyzdb_core::error::XyzError,
> {
    let v: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| {
        xyzdb_core::error::XyzError::InvalidQuery(format!("invalid params JSON: {e}"))
    })?;
    let obj = v.as_object().ok_or_else(|| {
        xyzdb_core::error::XyzError::InvalidQuery("params must be a JSON object".into())
    })?;
    Ok(obj
        .iter()
        .map(|(k, v)| (k.clone(), json_to_value(v)))
        .collect())
}

/// Map a `serde_json::Value` to an engine `Value` (integers stay `Int`, other
/// numbers become `Float`; arrays/objects recurse).
fn json_to_value(v: &serde_json::Value) -> xyzdb_core::value::Value {
    use xyzdb_core::value::Value;
    match v {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => n
            .as_i64()
            .map(Value::Int)
            .unwrap_or_else(|| Value::Float(n.as_f64().unwrap_or(0.0))),
        serde_json::Value::String(s) => Value::Text(s.clone()),
        serde_json::Value::Array(a) => Value::List(a.iter().map(json_to_value).collect()),
        serde_json::Value::Object(o) => Value::Map(
            o.iter()
                .map(|(k, v)| (k.clone(), json_to_value(v)))
                .collect(),
        ),
    }
}

fn is_write_query(query: &str) -> bool {
    let upper = query.trim().to_uppercase();
    upper.starts_with("PUT")
        || upper.starts_with("SET")
        || upper.starts_with("DELETE")
        || upper.starts_with("AUTOANCHOR APPLY")
        || (upper.contains('|') && (upper.contains("| SET") || upper.contains("| DELETE")))
}

fn is_pull_query(query: &str) -> bool {
    let upper = query.trim().to_uppercase();
    upper.starts_with("PULL") || upper.contains("| PULL")
}

/// Parse a `SNAPSHOT CREATE <name>` query (case-insensitive verb, name
/// case-preserved). Returns the snapshot name on success, None if the
/// query is not a snapshot-create. v0.4 cp 3.2.2.
///
/// Accepts the name optionally quoted with `"..."` for ergonomics; the
/// inner string is used verbatim. Whitespace around the name is
/// trimmed. Names that are empty or contain path separators are
/// rejected (returns None — falls through to the parser, which will
/// also reject them — and the operator gets a parse error).
fn parse_snapshot_create(query: &str) -> Option<String> {
    let trimmed = query.trim();
    let upper = trimmed.to_uppercase();
    let prefix = "SNAPSHOT CREATE ";
    if !upper.starts_with(prefix) {
        return None;
    }
    let raw_name = trimmed[prefix.len()..].trim();
    let name = if let Some(stripped) = raw_name.strip_prefix('"').and_then(|s| s.strip_suffix('"'))
    {
        stripped
    } else {
        raw_name
    };
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
        return None;
    }
    Some(name.to_string())
}

fn extract_root_lid(qr: &xyzdb_core::result::QueryResult) -> Option<xyzdb_core::lid::LID> {
    match qr {
        xyzdb_core::result::QueryResult::Records(records) => records.first().map(|r| r.lid),
        _ => None,
    }
}

fn is_streamable(stmt: &Statement) -> bool {
    matches!(stmt, Statement::Scan(s) if s.order_by.is_none())
}

/// Handle a streaming SCAN: write chunked header, stream records, write end marker.
async fn handle_streaming_scan(
    engine: &Arc<Engine>,
    stream: &mut TcpStream,
    stmt: &Statement,
    format: u8,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::fd::{AsRawFd, FromRawFd};

    let scan_stmt = match stmt {
        Statement::Scan(s) => s,
        _ => return Err("Not a SCAN statement".into()),
    };

    let serialize_fn: fn(&xyzdb_core::record::Record) -> Vec<u8> =
        if format == protocol::FORMAT_BINARY_CHUNKED {
            |r| bincode::serialize(r).unwrap_or_default()
        } else {
            |r| {
                let json = json_response::record_to_json(r);
                serde_json::to_vec(&json).unwrap_or_default()
            }
        };

    protocol::write_chunked_header(stream).await?;

    let fd = stream.as_raw_fd();
    // SAFETY: `fd` is a valid, open descriptor borrowed from the live `stream`;
    // `libc::dup` on a valid fd is always sound and returns a new fd or -1, which
    // is checked immediately below.
    let dup_fd = unsafe { libc::dup(fd) };
    if dup_fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: `dup_fd` is a fresh, valid descriptor (checked >= 0 above), a
    // duplicate of the socket fd independent of `stream`'s own fd. `from_raw_fd`
    // takes exclusive ownership, so the new `TcpStream` solely owns `dup_fd` and
    // closes it on drop.
    let std_stream = unsafe { std::net::TcpStream::from_raw_fd(dup_fd) };
    let mut writer = std::io::BufWriter::new(std_stream);

    let engine_ref = engine.clone();
    let scan_stmt_clone = scan_stmt.clone();
    let count = tokio::task::spawn_blocking(move || {
        let result = xyzdb_engine::ops::scan::execute_scan_streaming(
            &engine_ref,
            &scan_stmt_clone,
            &mut writer,
            serialize_fn,
        );
        let _ = protocol::write_end_marker_sync(&mut writer);
        result
    })
    .await??;

    tracing::debug!("Streamed {count} records");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A dishonest `record_count` near `u32::MAX` must not drive the
    /// pre-allocation: the cap bounds the capacity hint to the payload length,
    /// so parsing fails cleanly on truncation instead of OOM-aborting the
    /// process. Without the cap this test would be killed before asserting.
    #[test]
    fn huge_record_count_does_not_overallocate() {
        let msg = parse_v3_batch_payload(&[], u32::MAX)
            .err()
            .unwrap()
            .to_string();
        assert!(msg.contains("Truncated"));

        // gravity_count=5 but no entries follow → bounded parse, clean error.
        let msg = parse_v3_batch_payload(&[5u8], u32::MAX)
            .err()
            .unwrap()
            .to_string();
        assert!(msg.contains("Truncated"));
    }

    /// An honest batch (no gravity fields, empty field map) still parses with
    /// the cap in place — the Vec grows as needed when the count is truthful.
    #[test]
    fn valid_single_record_parses() {
        let fields_bytes = postcard::to_allocvec(&BTreeMap::<String, Value>::new()).unwrap();
        let mut payload = vec![0u8]; // gravity_count = 0
        payload.extend_from_slice(&(fields_bytes.len() as u32).to_be_bytes());
        payload.extend_from_slice(&fields_bytes);

        let records = parse_v3_batch_payload(&payload, 1).unwrap();
        assert_eq!(records.len(), 1);
        assert!(records[0].fields.is_empty());
        assert!(records[0].gravity_fields.is_empty());
    }

    #[test]
    fn only_liveness_probes_bypass_auth() {
        for q in ["/health", "HEALTH", "/ready", "READY", "/READY"] {
            assert!(is_liveness_probe(q), "{q} should bypass auth");
        }
        // Stats + metrics expose data — they must follow the token, not bypass.
        for q in ["STATS", "SHOW STATS", "/metrics", "METRICS"] {
            assert!(!is_liveness_probe(q), "{q} must not bypass auth");
        }
    }

    #[test]
    fn probe_routing_still_covers_stats_and_metrics() {
        // Routing (post-auth) must still reach the probe handler for all of
        // these, so an authenticated client can read STATS / /metrics.
        for q in [
            "/health",
            "/ready",
            "STATS",
            "SHOW STATS",
            "/metrics",
            "METRICS",
        ] {
            assert!(is_probe_query(q), "{q} should route to the probe handler");
        }
        assert!(!is_probe_query("FIND \"m\" WHERE x = 1"));
    }
}
