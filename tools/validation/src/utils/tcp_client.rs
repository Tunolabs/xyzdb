use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const V1: u8 = 1;
const V2: u8 = 2;
const FORMAT_BINARY: u8 = 0x01;
const STATUS_OK: u8 = 0x00;

/// TCP client for xyzDB server.
pub struct TcpClient {
    stream: TcpStream,
}

impl TcpClient {
    pub async fn connect(host: &str, port: u16) -> Result<Self> {
        let addr = format!("{host}:{port}");
        let stream = TcpStream::connect(&addr)
            .await
            .with_context(|| format!("connect to {addr}"))?;
        stream.set_nodelay(true)?;
        Ok(Self { stream })
    }

    /// Send query, receive text response (v1 protocol).
    pub async fn query_text(&mut self, q: &str) -> Result<String> {
        let payload = q.as_bytes();
        self.stream.write_u8(V1).await?;
        self.stream.write_u32(payload.len() as u32).await?;
        self.stream.write_all(payload).await?;
        self.stream.flush().await?;

        let status = self.stream.read_u8().await?;
        let length = self.stream.read_u32().await?;
        let mut buf = vec![0u8; length as usize];
        self.stream.read_exact(&mut buf).await?;
        let text = String::from_utf8(buf)?;

        if status != STATUS_OK {
            bail!("Server error: {text}");
        }
        Ok(text)
    }

    /// Send query, receive binary QueryResult (v2 protocol).
    pub async fn query_bin(&mut self, q: &str) -> Result<xyzdb_core::result::QueryResult> {
        let payload = q.as_bytes();
        self.stream.write_u8(V2).await?;
        self.stream.write_u8(FORMAT_BINARY).await?;
        self.stream.write_u32(payload.len() as u32).await?;
        self.stream.write_all(payload).await?;
        self.stream.flush().await?;

        let status = self.stream.read_u8().await?;
        let length = self.stream.read_u32().await?;
        let mut buf = vec![0u8; length as usize];
        self.stream.read_exact(&mut buf).await?;

        if status != STATUS_OK {
            bail!("Server error: {}", String::from_utf8_lossy(&buf));
        }

        let result: xyzdb_core::result::QueryResult = bincode::deserialize(&buf)
            .context("deserialize binary response")?;
        Ok(result)
    }

    /// Send query, ignore response (fire-and-forget with ack drain).
    pub async fn exec(&mut self, q: &str) -> Result<()> {
        let _ = self.query_text(q).await?;
        Ok(())
    }

    /// Get underlying stream for low-level tests.
    pub fn stream_mut(&mut self) -> &mut TcpStream {
        &mut self.stream
    }
}
