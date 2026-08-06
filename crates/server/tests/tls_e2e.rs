//! End-to-end TLS 1.3 handshake + V1 query test.
//!
//! v0.4 item 3 (cycle plan §3 Bloque 2 checkpoint 2.2.1). Spins up a
//! server with a fresh self-signed cert/key, connects with a rustls-tokio
//! client that bypasses verification (permissive verifier — test only),
//! sends a V1 STATS query and asserts a JSON response.
//!
//! **Runtime dependency**: `openssl` CLI in PATH. Available on macOS and
//! standard CI runners; gated behind a `which` probe so the test skips
//! cleanly when openssl isn't present rather than failing opaquely.

// SPDX-License-Identifier: BUSL-1.1
use std::process::Command;
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};
use tokio_rustls::rustls::{ClientConfig, ServerConfig};
use tokio_rustls::{TlsAcceptor, TlsConnector};
use xyzdb_engine::engine::Engine;
use xyzdb_server::protocol::{self, STATUS_OK};

/// Permissive cert verifier for tests against a self-signed server cert.
/// Accepts everything; never use outside tests.
#[derive(Debug)]
struct AcceptAnyServer;

impl tokio_rustls::rustls::client::danger::ServerCertVerifier for AcceptAnyServer {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: tokio_rustls::rustls::pki_types::UnixTime,
    ) -> Result<tokio_rustls::rustls::client::danger::ServerCertVerified, tokio_rustls::rustls::Error>
    {
        Ok(tokio_rustls::rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &tokio_rustls::rustls::DigitallySignedStruct,
    ) -> Result<
        tokio_rustls::rustls::client::danger::HandshakeSignatureValid,
        tokio_rustls::rustls::Error,
    > {
        Ok(tokio_rustls::rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &tokio_rustls::rustls::DigitallySignedStruct,
    ) -> Result<
        tokio_rustls::rustls::client::danger::HandshakeSignatureValid,
        tokio_rustls::rustls::Error,
    > {
        Ok(tokio_rustls::rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<tokio_rustls::rustls::SignatureScheme> {
        use tokio_rustls::rustls::SignatureScheme as S;
        vec![
            S::RSA_PSS_SHA256,
            S::RSA_PSS_SHA384,
            S::RSA_PSS_SHA512,
            S::ECDSA_NISTP256_SHA256,
            S::ECDSA_NISTP384_SHA384,
            S::ED25519,
        ]
    }
}

/// Generate a fresh self-signed cert + key via openssl. Returns
/// `(cert_pem_path, key_pem_path)` in the temp dir, or panics if openssl
/// is missing or fails.
fn gen_self_signed(tempdir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let cert = tempdir.join("cert.pem");
    let key = tempdir.join("key.pem");
    let status = Command::new("openssl")
        .args([
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-keyout",
            key.to_str().unwrap(),
            "-out",
            cert.to_str().unwrap(),
            "-days",
            "1",
            "-subj",
            "/CN=localhost",
            "-addext",
            "subjectAltName=DNS:localhost,IP:127.0.0.1",
        ])
        .status()
        .expect("openssl in PATH (skip test if missing on this host)");
    assert!(status.success(), "openssl req failed");
    (cert, key)
}

fn build_server_tls_config(cert: &std::path::Path, key: &std::path::Path) -> ServerConfig {
    use std::fs::File;
    use std::io::BufReader;

    let cert_file = File::open(cert).expect("open cert");
    let mut cert_reader = BufReader::new(cert_file);
    let certs: Vec<_> = rustls_pemfile::certs(&mut cert_reader)
        .collect::<Result<_, _>>()
        .expect("parse certs");

    let key_file = File::open(key).expect("open key");
    let mut key_reader = BufReader::new(key_file);
    let key = rustls_pemfile::private_key(&mut key_reader)
        .expect("parse key")
        .expect("private key present");

    ServerConfig::builder_with_protocol_versions(&[&tokio_rustls::rustls::version::TLS13])
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .expect("server config")
}

fn build_client_tls_config() -> ClientConfig {
    ClientConfig::builder_with_protocol_versions(&[&tokio_rustls::rustls::version::TLS13])
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyServer))
        .with_no_client_auth()
}

#[tokio::test]
async fn test_tls_handshake_and_v1_query() {
    // Skip cleanly if openssl is missing on this host.
    if Command::new("openssl").arg("version").output().is_err() {
        eprintln!("openssl not in PATH — skipping TLS E2E test");
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let (cert, key) = gen_self_signed(dir.path());

    // Engine + server with TLS acceptor.
    let engine_dir = tempfile::tempdir().expect("engine tempdir");
    let engine = Engine::open(engine_dir.path())
        .expect("engine open")
        .into_arc();
    let acceptor = TlsAcceptor::from(Arc::new(build_server_tls_config(&cert, &key)));

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        loop {
            if let Ok((tcp_stream, peer_addr)) = listener.accept().await {
                let acceptor = acceptor.clone();
                let engine = engine.clone();
                tokio::spawn(async move {
                    match acceptor.accept(tcp_stream).await {
                        Ok(tls_stream) => {
                            xyzdb_server::connection::handle_tls_connection(
                                engine,
                                tls_stream,
                                peer_addr,
                                Arc::new(None),
                            )
                            .await;
                        }
                        Err(e) => eprintln!("server-side TLS handshake failed: {e}"),
                    }
                });
            }
        }
    });

    // Client side: rustls connector with permissive verifier.
    let connector = TlsConnector::from(Arc::new(build_client_tls_config()));
    let tcp = TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .expect("tcp connect");
    let server_name = ServerName::try_from("localhost").expect("server name");
    let mut tls_stream = connector
        .connect(server_name, tcp)
        .await
        .expect("client TLS handshake");

    // Send a V1 STATS query.
    protocol::write_request_v1(&mut tls_stream, "STATS")
        .await
        .expect("send STATS");

    // Read response: status u8 + length u32 BE + payload.
    let status = tls_stream.read_u8().await.expect("read status");
    let length = tls_stream.read_u32().await.expect("read length");
    let mut payload = vec![0u8; length as usize];
    tls_stream
        .read_exact(&mut payload)
        .await
        .expect("read payload");

    assert_eq!(status, STATUS_OK, "STATS over TLS should succeed");
    let body = std::str::from_utf8(&payload).expect("utf8");
    assert!(
        body.contains("\"keyspaces\""),
        "STATS JSON should contain 'keyspaces' key; got: {body}"
    );

    // v0.4 item 3 limitation: chunked streaming is rejected over TLS.
    // The "chunked" formats are 0x03 / 0x04. Test that V2 with chunked
    // returns ERROR rather than producing garbage.
    protocol::write_request_v2(&mut tls_stream, protocol::FORMAT_JSON_CHUNKED, "SCAN \"x\"")
        .await
        .expect("send V2 chunked");
    let status = tls_stream.read_u8().await.expect("read status");
    let length = tls_stream.read_u32().await.expect("read length");
    let mut payload = vec![0u8; length as usize];
    tls_stream
        .read_exact(&mut payload)
        .await
        .expect("read payload");
    assert_ne!(
        status, STATUS_OK,
        "chunked streaming over TLS must NOT succeed in v0.4"
    );
    let body = std::str::from_utf8(&payload).expect("utf8");
    assert!(
        body.contains("chunked streaming"),
        "rejection message should mention chunked streaming; got: {body}"
    );
}
