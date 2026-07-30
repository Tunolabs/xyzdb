//! `--connect` mode — TCP client to an external `xyzdb-server`.
//!
//! Spike day 2 (Día 5 of v0.2.6 cycle): single-shot V2 protocol client
//! per tool call. Connection pooling, retry, and timeout policies are
//! Día 6+ implementation work; the spike validates that the wire path
//! works end-to-end against a real `xyzdb-server` instance.
//!
//! Wire format (mirrors xyzdb-server/src/protocol.rs):
//!
//! ```text
//! Auth (opt):  [0x41][token_len:u16 BE][utf8 token]     (when XYZDB_TOKEN set)
//! Request V2:  [version=2:u8][format:u8][len:u32 BE][utf8 query]
//! Response:    [status:u8][len:u32 BE][payload:bytes]
//! ```
//!
//! Format `FORMAT_JSON = 0x02` makes the server emit a JSON-shaped response
//! which we deserialize directly into our types. When `XYZDB_TOKEN` is set the
//! bearer-token preamble is sent first, so `--connect` works against a server
//! started with `--auth-token`.

use std::net::IpAddr;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

// Wire protocol constants — copied from xyzdb-server::protocol to avoid
// pulling the whole server crate as a dependency. Values must match.
pub const PROTOCOL_V2: u8 = 2;
pub const FORMAT_JSON: u8 = 0x02;
pub const STATUS_OK: u8 = 0x00;
pub const STATUS_ERROR: u8 = 0x01;
pub const MAX_FRAME_SIZE: u32 = 16 * 1024 * 1024;
/// Bearer-token preamble marker; must match `xyzdb-server::protocol::AUTH_MAGIC`.
pub const AUTH_MAGIC: u8 = 0x41;
pub const MAX_AUTH_TOKEN_LEN: u16 = 4096;

/// Trust class of the target host. Used to decide whether to emit a
/// boot-time warning; never blocks the connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostClass {
    Loopback,
    Private,
    LinkLocal,
    Public,
    /// Could not parse host as an IP — treat as public for warning
    /// purposes (DNS names that resolve at connect time may go anywhere).
    DnsName,
}

/// Parse a `host:port` argument into components.
pub fn parse_addr(s: &str) -> Result<(String, u16)> {
    let (host, port_str) = s
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("--connect requires host:port (got '{s}')"))?;
    let port: u16 = port_str
        .parse()
        .with_context(|| format!("invalid port '{port_str}' in --connect"))?;
    if host.is_empty() {
        return Err(anyhow!("--connect requires non-empty host (got '{s}')"));
    }
    Ok((host.to_string(), port))
}

/// Classify a host string into a trust class. IPv4 + IPv6 supported.
/// DNS names return `DnsName` (caller treats as public for warning).
pub fn classify_host(host: &str) -> HostClass {
    let Ok(ip) = IpAddr::from_str(host) else {
        return HostClass::DnsName;
    };
    match ip {
        IpAddr::V4(v4) => {
            if v4.is_loopback() {
                HostClass::Loopback
            } else if v4.is_private() {
                HostClass::Private
            } else if v4.is_link_local() {
                HostClass::LinkLocal
            } else {
                HostClass::Public
            }
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() {
                HostClass::Loopback
            } else if is_ipv6_ula(&v6) {
                HostClass::Private
            } else if is_ipv6_link_local(&v6) {
                HostClass::LinkLocal
            } else {
                HostClass::Public
            }
        }
    }
}

/// IPv6 Unique Local Address: `fc00::/7`. RFC 4193.
fn is_ipv6_ula(addr: &std::net::Ipv6Addr) -> bool {
    let segments = addr.segments();
    (segments[0] & 0xfe00) == 0xfc00
}

/// IPv6 link-local: `fe80::/10`. RFC 4291.
fn is_ipv6_link_local(addr: &std::net::Ipv6Addr) -> bool {
    let segments = addr.segments();
    (segments[0] & 0xffc0) == 0xfe80
}

/// Emit a warning at startup if the target host is public or a DNS
/// name. Informational only — never blocks the connection.
pub fn warn_host_class(host: &str, class: HostClass) {
    match class {
        HostClass::Loopback | HostClass::Private => {} // trusted, no warning
        HostClass::LinkLocal => {
            tracing::warn!(
                host = %host,
                "xyzdb-mcp connecting to a link-local host. Typically physical-adjacency \
                 deployments only; not a recommended production topology."
            );
        }
        HostClass::Public | HostClass::DnsName => {
            tracing::warn!(
                host = %host,
                "xyzdb-mcp connecting to a non-private host. Ensure xyzdb-server has \
                 appropriate network ACLs and authentication. Public-facing xyzdb-server \
                 without auth is a security risk. See docs/mcp-integration.md threat model. \
                 v0.2.7+ HTTP transport with TLS+auth is the canonical path for cross-network \
                 deployments."
            );
        }
    }
}

/// Single-shot V2 query: open a fresh TCP connection, send the query
/// with `FORMAT_JSON`, read the response, close the connection. The
/// returned bytes are the JSON payload (validated as STATUS_OK; STATUS_ERROR
/// surfaces as `Err`).
///
/// Spike-grade: no connection pooling, no retry. Timeouts default to
/// 30 s wall-clock for the whole exchange; configurable in Día 6+.
pub async fn query_json(host: &str, port: u16, query: &str) -> Result<Vec<u8>> {
    const SPIKE_TIMEOUT: Duration = Duration::from_secs(30);

    let exchange = async {
        let mut stream = TcpStream::connect((host, port))
            .await
            .with_context(|| format!("failed to connect to xyzdb-server at {host}:{port}"))?;

        // Bearer-token preamble: when XYZDB_TOKEN is set, authenticate before
        // the version byte (`[AUTH_MAGIC][len:u16 BE][token]`). A server started
        // with --auth-token requires it for every non-liveness query — without
        // this, `--connect` tools (STATS, SHOW LOBES, any FIND/SCAN) fail against
        // a secured server.
        if let Ok(token) = std::env::var("XYZDB_TOKEN")
            && !token.is_empty()
        {
            let tb = token.as_bytes();
            if tb.len() > MAX_AUTH_TOKEN_LEN as usize {
                return Err(anyhow!("XYZDB_TOKEN exceeds {MAX_AUTH_TOKEN_LEN} bytes"));
            }
            stream.write_u8(AUTH_MAGIC).await?;
            stream.write_u16(tb.len() as u16).await?;
            stream.write_all(tb).await?;
        }

        // Send V2 request.
        let payload = query.as_bytes();
        stream.write_u8(PROTOCOL_V2).await?;
        stream.write_u8(FORMAT_JSON).await?;
        stream.write_u32(payload.len() as u32).await?;
        stream.write_all(payload).await?;
        stream.flush().await?;

        // Read response.
        let status = stream.read_u8().await?;
        let length = stream.read_u32().await?;
        if length > MAX_FRAME_SIZE {
            return Err(anyhow!("response too large: {length} bytes"));
        }
        let mut buf = vec![0u8; length as usize];
        stream.read_exact(&mut buf).await?;

        match status {
            STATUS_OK => Ok(buf),
            STATUS_ERROR => {
                let msg = String::from_utf8_lossy(&buf);
                Err(anyhow!("xyzdb-server error: {msg}"))
            }
            other => Err(anyhow!("unexpected response status byte: 0x{other:02x}")),
        }
    };

    tokio::time::timeout(SPIKE_TIMEOUT, exchange)
        .await
        .map_err(|_| {
            anyhow!(
                "xyzdb-server query timeout after {}s",
                SPIKE_TIMEOUT.as_secs()
            )
        })?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_addr_ok() {
        assert_eq!(
            parse_addr("127.0.0.1:2505").unwrap(),
            ("127.0.0.1".to_string(), 2505)
        );
        assert_eq!(
            parse_addr("xyzdb.internal:5432").unwrap(),
            ("xyzdb.internal".to_string(), 5432)
        );
    }

    #[test]
    fn parse_addr_ipv6() {
        // IPv6 literal needs brackets when port is appended in URL form,
        // but our format is plain rsplit on ':'. Document quirk: bare
        // IPv6 not supported in v0.2.6; users wrap in brackets like
        // [::1]:2505 — handled by the rsplit (port = 2505, host = "[::1]").
        let (host, port) = parse_addr("[::1]:2505").unwrap();
        assert_eq!(host, "[::1]");
        assert_eq!(port, 2505);
    }

    #[test]
    fn parse_addr_rejects_missing_port() {
        assert!(parse_addr("127.0.0.1").is_err());
        assert!(parse_addr("127.0.0.1:").is_err());
        assert!(parse_addr(":2505").is_err());
    }

    #[test]
    fn classify_loopback() {
        assert_eq!(classify_host("127.0.0.1"), HostClass::Loopback);
        assert_eq!(classify_host("127.0.0.42"), HostClass::Loopback);
        assert_eq!(classify_host("::1"), HostClass::Loopback);
    }

    #[test]
    fn classify_private_ipv4() {
        assert_eq!(classify_host("10.0.0.1"), HostClass::Private);
        assert_eq!(classify_host("172.16.5.10"), HostClass::Private);
        assert_eq!(classify_host("192.168.1.100"), HostClass::Private);
    }

    #[test]
    fn classify_private_ipv6_ula() {
        assert_eq!(classify_host("fc00::1"), HostClass::Private);
        assert_eq!(classify_host("fd12:3456:789a::1"), HostClass::Private);
    }

    #[test]
    fn classify_link_local() {
        assert_eq!(classify_host("169.254.1.1"), HostClass::LinkLocal);
        assert_eq!(classify_host("fe80::1"), HostClass::LinkLocal);
    }

    #[test]
    fn classify_public() {
        assert_eq!(classify_host("8.8.8.8"), HostClass::Public);
        assert_eq!(classify_host("2001:4860:4860::8888"), HostClass::Public);
    }

    #[test]
    fn classify_dns_name() {
        assert_eq!(classify_host("xyzdb.example.com"), HostClass::DnsName);
        assert_eq!(classify_host("localhost"), HostClass::DnsName);
    }
}
