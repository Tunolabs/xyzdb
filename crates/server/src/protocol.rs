// SPDX-License-Identifier: BUSL-1.1
/// Wire protocol for xyzDB. See `PROTOCOL.md` for the full specification;
/// this module is the implementation.
///
/// Optional auth preamble (when `--auth-token` is set):
///   [AUTH_MAGIC=0x41][token_len: u16 BE][token: UTF-8]
///
/// Request framing, selected by the version byte:
///   V1: [0x01][len: u32 BE][query: UTF-8 xyTalk]                (format forced to TEXT)
///   V2: [0x02][format: u8][len: u32 BE][query: UTF-8 xyTalk]
///   V3: [0x03] ...                                              (binary bulk load, PROTOCOL.md §7)
///   V4: [0x04][format: u8][query_len: u32 BE][query][params_len: u32 BE][params: JSON]
///
/// Response: [status: u8][len: u32 BE][payload: bytes]; status 0x00=OK, 0x01=Error.
///
/// Format byte: 0x00=TEXT, 0x01=BINARY (bincode `QueryResult`), 0x02=JSON,
/// 0x03=JSON_CHUNKED, 0x04=BINARY_CHUNKED. First-party clients use JSON; the
/// BINARY format is defined but not exercised by any first-party client.
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const PROTOCOL_V1: u8 = 1;
pub const PROTOCOL_V2: u8 = 2;
pub const PROTOCOL_V3: u8 = 3; // binary bulk load (own connection mode; PROTOCOL.md §7)
/// Query with bound parameters. Same framing as V2
/// (`[format:u8][query_len:u32 BE][query]`) followed by
/// `[params_len:u32 BE][params: JSON object bytes]`. The params object maps
/// `$name` placeholders to values, substituted before execution so untrusted
/// text never enters the statement as syntax. Backward compatible: a client
/// with no parameters keeps sending V2.
pub const PROTOCOL_V4: u8 = 4;
/// Bearer-token preamble marker. When the server is configured with
/// `--auth-token`, the first byte on every connection (after the TLS
/// handshake, if any) must be `AUTH_MAGIC`, followed by
/// `[token_len: u16 BE][token: UTF-8 bytes]`. The server validates the token
/// against the configured value; a mismatch closes the connection with an
/// error frame. After successful auth the connection reads the protocol
/// version byte normally and proceeds with V1/V2/V3/V4.
///
/// Servers WITHOUT `--auth-token` configured still accept and silently
/// consume an auth frame if a client sends one — preserves operational
/// ergonomics for clients that always set `XYZDB_TOKEN` even against
/// dev servers without auth.
///
/// Chosen value: `'A'` (0x41) — outside the existing protocol-version
/// space (0x01/0x02/0x03) and printable for log readability.
pub const AUTH_MAGIC: u8 = 0x41;
/// Maximum token length accepted at the wire. 4 KiB is generous for
/// tokens (typical 32-256 bytes) without being abusable as DoS amplifier.
pub const MAX_AUTH_TOKEN_LEN: u16 = 4096;
pub const FORMAT_TEXT: u8 = 0x00;
pub const FORMAT_BINARY: u8 = 0x01;
pub const FORMAT_JSON: u8 = 0x02;
pub const STATUS_OK: u8 = 0x00;
pub const STATUS_ERROR: u8 = 0x01;
pub const STATUS_CHUNKED: u8 = 0x02;
pub const FORMAT_JSON_CHUNKED: u8 = 0x03;
pub const FORMAT_BINARY_CHUNKED: u8 = 0x04;
pub const MAX_FRAME_SIZE: u32 = 16 * 1024 * 1024;

// V3 bulk load constants
pub const V3_FLAG_SORTED: u8 = 0x01;
pub const V3_FLAG_LZ4: u8 = 0x02;
pub const V3_FRAME_DATA: u8 = 0x01;
pub const V3_FRAME_END: u8 = 0x00;

/// Parsed request with format info (V1/V2/V4 protocols).
pub struct Request {
    pub query: String,
    pub format: u8,
    /// S1: raw JSON bytes of the bound-params object (`{"name": value}`), or
    /// `None` for V1/V2. The connection handler converts and binds them.
    pub params_json: Option<Vec<u8>>,
}

/// V3 bulk load connection header.
pub struct V3Header {
    pub flags: u8,
    pub lobe_name: String,
}

/// V3 batch frame from client.
pub struct V3BatchFrame {
    pub record_count: u32,
    pub payload: Vec<u8>,
}

/// Read a request frame. Supports both V1 and V2 protocols.
pub async fn read_request<R: AsyncReadExt + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<Request>> {
    let version = match reader.read_u8().await {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    };

    let (format, has_params) = match version {
        PROTOCOL_V1 => (FORMAT_TEXT, false),
        PROTOCOL_V2 => (reader.read_u8().await?, false),
        PROTOCOL_V4 => (reader.read_u8().await?, true),
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Unsupported protocol version: {version}"),
            ));
        }
    };

    let length = reader.read_u32().await?;
    if length > MAX_FRAME_SIZE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Frame too large: {length} bytes"),
        ));
    }

    let mut buf = vec![0u8; length as usize];
    reader.read_exact(&mut buf).await?;

    let query = String::from_utf8(buf).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Invalid UTF-8: {e}"),
        )
    })?;

    // S1 (V4): bound-params JSON object follows the query frame.
    let params_json = if has_params {
        let plen = reader.read_u32().await?;
        if plen > MAX_FRAME_SIZE {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Params frame too large: {plen} bytes"),
            ));
        }
        let mut pbuf = vec![0u8; plen as usize];
        reader.read_exact(&mut pbuf).await?;
        Some(pbuf)
    } else {
        None
    };

    Ok(Some(Request {
        query,
        format,
        params_json,
    }))
}

/// Write a response frame (raw bytes).
pub async fn write_response_bytes<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    status: u8,
    payload: &[u8],
) -> std::io::Result<()> {
    writer.write_u8(status).await?;
    writer.write_u32(payload.len() as u32).await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
    Ok(())
}

/// Write a V1 request (TEXT format, used by CLI).
pub async fn write_request_v1<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    query: &str,
) -> std::io::Result<()> {
    let payload = query.as_bytes();
    writer.write_u8(PROTOCOL_V1).await?;
    writer.write_u32(payload.len() as u32).await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
    Ok(())
}

/// Write a V2 request (with format byte, used by bench/drivers).
pub async fn write_request_v2<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    format: u8,
    query: &str,
) -> std::io::Result<()> {
    let payload = query.as_bytes();
    writer.write_u8(PROTOCOL_V2).await?;
    writer.write_u8(format).await?;
    writer.write_u32(payload.len() as u32).await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
    Ok(())
}

/// Read a response frame. Returns (status, raw bytes).
pub async fn read_response_raw<R: AsyncReadExt + Unpin>(
    reader: &mut R,
) -> std::io::Result<(u8, Vec<u8>)> {
    let status = reader.read_u8().await?;
    let length = reader.read_u32().await?;
    if length > MAX_FRAME_SIZE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Response too large: {length} bytes"),
        ));
    }
    let mut buf = vec![0u8; length as usize];
    reader.read_exact(&mut buf).await?;
    Ok((status, buf))
}

// ─── V4: Chunked streaming protocol ─────────────────────────────────────────

/// Write the chunked response header: [status=0x02][reserved: u32=0]
pub async fn write_chunked_header<W: AsyncWriteExt + Unpin>(writer: &mut W) -> std::io::Result<()> {
    writer.write_u8(STATUS_CHUNKED).await?;
    writer.write_u32(0).await?; // reserved / total_chunks unknown
    writer.flush().await?;
    Ok(())
}

/// Write a single chunk: [length: u32 BE][payload: bytes]
pub fn write_chunk_sync<W: std::io::Write>(writer: &mut W, payload: &[u8]) -> std::io::Result<()> {
    let len = payload.len() as u32;
    writer.write_all(&len.to_be_bytes())?;
    writer.write_all(payload)?;
    Ok(())
}

/// Write end-of-stream marker: [length: u32 BE = 0]
pub fn write_end_marker_sync<W: std::io::Write>(writer: &mut W) -> std::io::Result<()> {
    writer.write_all(&0u32.to_be_bytes())?;
    writer.flush()?;
    Ok(())
}

/// Returns true if the format byte requests chunked streaming.
pub fn is_chunked_format(format: u8) -> bool {
    format == FORMAT_JSON_CHUNKED || format == FORMAT_BINARY_CHUNKED
}

// ─── Protocol V3 — Binary Bulk Load ─────────────────────────────────────────

/// Read V3 connection header (after version byte already consumed).
/// Format: [flags:u8][lobe_name_len:u16 BE][lobe_name:bytes]
pub async fn read_v3_header<R: AsyncReadExt + Unpin>(reader: &mut R) -> std::io::Result<V3Header> {
    let flags = reader.read_u8().await?;
    let name_len = reader.read_u16().await?;
    let mut name_buf = vec![0u8; name_len as usize];
    reader.read_exact(&mut name_buf).await?;
    let lobe_name = String::from_utf8(name_buf).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Invalid lobe name: {e}"),
        )
    })?;
    Ok(V3Header { flags, lobe_name })
}

/// Read one V3 batch frame. Returns None on end-of-stream (frame_type=0x00).
/// Format: [frame_type:u8][record_count:u32 BE][payload_len:u32 BE][payload:bytes]
pub async fn read_v3_batch_frame<R: AsyncReadExt + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<V3BatchFrame>> {
    let frame_type = reader.read_u8().await?;
    if frame_type == V3_FRAME_END {
        return Ok(None);
    }
    let record_count = reader.read_u32().await?;
    let payload_len = reader.read_u32().await?;
    if payload_len > MAX_FRAME_SIZE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("V3 batch too large: {payload_len} bytes"),
        ));
    }
    let mut payload = vec![0u8; payload_len as usize];
    reader.read_exact(&mut payload).await?;
    Ok(Some(V3BatchFrame {
        record_count,
        payload,
    }))
}

/// Write V3 batch response: [status:u8][count:u32 BE][first_lid:u128 BE][last_lid:u128 BE]
pub async fn write_v3_batch_response<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    status: u8,
    count: u32,
    first_lid: u128,
    last_lid: u128,
) -> std::io::Result<()> {
    writer.write_u8(status).await?;
    writer.write_u32(count).await?;
    writer.write_u128(first_lid).await?;
    writer.write_u128(last_lid).await?;
    writer.flush().await?;
    Ok(())
}

// ─── v0.4 cp 2.2.2: bearer-token auth preamble ──────────────────────────────

/// Write a bearer-token preamble: `[AUTH_MAGIC][len: u16 BE][token]`. Used
/// by clients (CLI, Python SDK) before sending the protocol version byte
/// when they have `XYZDB_TOKEN` set.
pub async fn write_auth_frame<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    token: &str,
) -> std::io::Result<()> {
    let bytes = token.as_bytes();
    if bytes.len() > MAX_AUTH_TOKEN_LEN as usize {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "token exceeds MAX_AUTH_TOKEN_LEN",
        ));
    }
    writer.write_u8(AUTH_MAGIC).await?;
    writer.write_u16(bytes.len() as u16).await?;
    writer.write_all(bytes).await?;
    writer.flush().await?;
    Ok(())
}

/// Read the body of an auth frame *after* the caller has already consumed
/// the `AUTH_MAGIC` byte. Returns the presented token as a `String`.
pub async fn read_auth_frame_body<R: AsyncReadExt + Unpin>(
    reader: &mut R,
) -> std::io::Result<String> {
    let len = reader.read_u16().await?;
    if len > MAX_AUTH_TOKEN_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("auth token too large: {len} bytes"),
        ));
    }
    let mut buf = vec![0u8; len as usize];
    reader.read_exact(&mut buf).await?;
    String::from_utf8(buf).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid token utf8: {e}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(version: u8, query: &[u8], params: Option<&[u8]>) -> Vec<u8> {
        let mut f = vec![version, FORMAT_JSON];
        f.extend_from_slice(&(query.len() as u32).to_be_bytes());
        f.extend_from_slice(query);
        if let Some(p) = params {
            f.extend_from_slice(&(p.len() as u32).to_be_bytes());
            f.extend_from_slice(p);
        }
        f
    }

    #[tokio::test]
    async fn v4_request_carries_query_and_params() {
        let query = br#"SCAN "m" WHERE name = $n"#;
        let params = br#"{"n":"alice"}"#;
        let buf = frame(PROTOCOL_V4, query, Some(params));
        let mut reader = &buf[..];
        let req = read_request(&mut reader).await.unwrap().unwrap();
        assert_eq!(req.query.as_bytes(), query);
        assert_eq!(req.format, FORMAT_JSON);
        assert_eq!(req.params_json.as_deref(), Some(&params[..]));
    }

    #[tokio::test]
    async fn v2_request_has_no_params() {
        let query = br#"FIND "m""#;
        let buf = frame(PROTOCOL_V2, query, None);
        let mut reader = &buf[..];
        let req = read_request(&mut reader).await.unwrap().unwrap();
        assert_eq!(req.query.as_bytes(), query);
        assert!(req.params_json.is_none());
    }
}
