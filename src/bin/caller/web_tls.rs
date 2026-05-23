//! Native TLS for the `--web` dashboard gateway.
//!
//! Provides a pure-Rust ([`rustls`] + [`rcgen`]) path to serving the
//! dashboard over HTTPS/WSS on every platform — including Windows, where
//! the `intendant lan` / nginx / OpenSSL story is gated off. The accept
//! loop's per-connection demux (in [`crate::web_gateway`]) peeks the first
//! bytes of each connection and, when it sees a TLS ClientHello, wraps the
//! socket in the [`tokio_rustls::TlsAcceptor`] built here before handing
//! the decrypted stream to the existing HTTP/WebSocket handling.
//!
//! Crypto provider: everything rides the **`ring`** provider so it matches
//! the rest of the tree (`rustls` is pinned to `ring` in `Cargo.toml`; no
//! `aws-lc` is pulled in). `rcgen` is likewise built with its `ring`
//! feature, so cert generation and the TLS server share one C-free crypto
//! backend.

use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use rustls::ServerConfig;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_rustls::TlsAcceptor;

/// A stream with a replayed prefix in front of an inner async stream.
///
/// The gateway's demux reads the first chunk of the (decrypted, for TLS)
/// request to inspect the HTTP request line / headers, which consumes
/// those bytes from the underlying stream. The downstream HTTP/WebSocket
/// handlers, however, expect to read the request from byte zero. Wrapping
/// the stream in a `PrefixedStream` re-serves the already-read prefix
/// first, then transparently forwards to the inner stream — so the
/// handlers behave identically whether or not the demux pre-read anything.
///
/// Writes pass straight through. Used for the TLS path (prefix = decrypted
/// request head). The plain-TCP path uses an empty prefix, where it's a
/// zero-overhead pass-through (the kernel still holds the peeked bytes).
pub struct PrefixedStream<S> {
    prefix: Vec<u8>,
    pos: usize,
    inner: S,
}

impl<S> PrefixedStream<S> {
    pub fn new(prefix: Vec<u8>, inner: S) -> Self {
        Self {
            prefix,
            pos: 0,
            inner,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for PrefixedStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if this.pos < this.prefix.len() {
            let remaining = &this.prefix[this.pos..];
            let n = remaining.len().min(buf.remaining());
            buf.put_slice(&remaining[..n]);
            this.pos += n;
            // Serve only the prefix on this poll; the next read drains the
            // inner stream. This keeps the logic simple and is fine for the
            // header-read path (callers loop until they have enough bytes).
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for PrefixedStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// First byte of a TLS record is the content type; `0x16` = Handshake.
/// A ClientHello is the first handshake message on a fresh connection, so
/// the very first byte a TLS client sends is always `0x16`. This value is
/// disjoint from every other protocol the gateway demuxes:
///
/// - HTTP request methods start with ASCII uppercase letters (`G`, `P`,
///   `O`, `D`, … all `>= 0x41`).
/// - ICE-TCP is RFC 4571 framed: a 2-byte big-endian length prefix whose
///   high byte (`buf[0]`) is the length's MSB. For a STUN binding request
///   the framed payload is ~20-28 bytes, so `buf[0] == 0x00`.
///
/// So `buf[0] == 0x16` unambiguously means TLS.
pub const TLS_HANDSHAKE_CONTENT_TYPE: u8 = 0x16;

/// Returns true when a peeked byte prefix looks like the start of a TLS
/// ClientHello: a Handshake record (`0x16`) carrying a TLS major version
/// byte of `0x03` (TLS 1.0-1.3 all use legacy record version `3.x`).
///
/// We check two bytes rather than just the content type to avoid mistaking
/// a stray `0x16` (e.g. a truncated/garbage first byte) for TLS. The
/// record layout is: `[content_type=0x16][version_major=0x03][version_minor][len_hi][len_lo]...`.
pub fn looks_like_tls_client_hello(buf: &[u8]) -> bool {
    buf.len() >= 3 && buf[0] == TLS_HANDSHAKE_CONTENT_TYPE && buf[1] == 0x03
}

/// How the gateway should obtain its server certificate.
#[derive(Debug, Clone)]
pub enum TlsCertSource {
    /// Generate a self-signed cert at startup (the default when TLS is on).
    /// SAN list always includes `localhost`, `127.0.0.1`, and `::1`, plus
    /// the bind IP and any extra `hostname` supplied by the operator.
    SelfSigned {
        /// The address the listener is bound to. When it's a concrete
        /// (non-wildcard) IP it's added to the SAN list so the cert
        /// validates for direct-IP access.
        bind_ip: Option<IpAddr>,
        /// Optional extra DNS SAN (e.g. `intendant.local`, a tailnet name).
        hostname: Option<String>,
    },
    /// Use operator-supplied PEM files (cert chain + private key).
    Files {
        cert_path: std::path::PathBuf,
        key_path: std::path::PathBuf,
    },
}

/// Build a [`TlsAcceptor`] from the requested certificate source.
///
/// The returned acceptor wraps an `Arc<ServerConfig>` configured with the
/// `ring` provider, no client-cert verification (this is server-auth TLS
/// for a browser dashboard, not mTLS), and ALPN advertising `http/1.1`
/// (the gateway speaks HTTP/1.1 + WebSocket only — no HTTP/2).
pub fn build_acceptor(source: &TlsCertSource) -> Result<TlsAcceptor, String> {
    let (cert_chain, key) = match source {
        TlsCertSource::SelfSigned { bind_ip, hostname } => {
            generate_self_signed(*bind_ip, hostname.as_deref())?
        }
        TlsCertSource::Files {
            cert_path,
            key_path,
        } => load_pem_cert_and_key(cert_path, key_path)?,
    };

    let config = server_config_from(cert_chain, key)?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// Assemble a [`rustls::ServerConfig`] from a parsed cert chain + key.
///
/// Pins the `ring` crypto provider explicitly (rather than relying on the
/// process-default provider, which other subsystems may or may not have
/// installed) so this path is self-contained and order-independent.
fn server_config_from(
    cert_chain: Vec<rustls::pki_types::CertificateDer<'static>>,
    key: rustls::pki_types::PrivateKeyDer<'static>,
) -> Result<ServerConfig, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("rustls protocol version setup failed: {e}"))?
        .with_no_client_auth()
        .with_single_cert(cert_chain, key)
        .map_err(|e| format!("rustls server config (cert/key) failed: {e}"))?;
    // The dashboard speaks HTTP/1.1 and upgrades to WebSocket; advertise
    // only http/1.1 so a browser never negotiates h2 (which the gateway's
    // hand-rolled HTTP/1 handler doesn't implement).
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}

/// Mint a fresh self-signed certificate + key with `rcgen`.
///
/// SAN list: always `localhost` / `127.0.0.1` / `::1`, plus the bind IP
/// (when concrete) and an optional extra hostname. A wildcard bind
/// (`0.0.0.0` / `::`) contributes no IP SAN — clients reach such a server
/// by a concrete address, which they'll supply via `localhost`, the
/// hostname SAN, or by accepting the cert anyway (`curl -k`, browser
/// "proceed").
fn generate_self_signed(
    bind_ip: Option<IpAddr>,
    hostname: Option<&str>,
) -> Result<
    (
        Vec<rustls::pki_types::CertificateDer<'static>>,
        rustls::pki_types::PrivateKeyDer<'static>,
    ),
    String,
> {
    let mut sans: Vec<String> = vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ];
    if let Some(ip) = bind_ip {
        if !ip.is_unspecified() {
            let s = ip.to_string();
            if !sans.contains(&s) {
                sans.push(s);
            }
        }
    }
    if let Some(h) = hostname {
        let h = h.trim();
        if !h.is_empty() && !sans.contains(&h.to_string()) {
            sans.push(h.to_string());
        }
    }

    // `generate_simple_self_signed` accepts a SAN list of strings and
    // classifies each entry as an IP or DNS name internally, mints a
    // keypair (ring provider), and returns the cert + serialized key.
    let cert = rcgen::generate_simple_self_signed(sans)
        .map_err(|e| format!("rcgen self-signed cert generation failed: {e}"))?;

    let cert_der = rustls::pki_types::CertificateDer::from(cert.cert.der().to_vec());
    let key_der = rustls::pki_types::PrivateKeyDer::try_from(cert.signing_key.serialize_der())
        .map_err(|e| format!("rcgen key -> rustls PrivateKeyDer failed: {e}"))?;

    Ok((vec![cert_der], key_der))
}

/// Load a PEM cert chain and private key from disk (operator override).
///
/// Accepts PKCS#8, PKCS#1 (RSA), and SEC1 (EC) private keys — `rustls_pemfile`
/// returns whichever it finds first in the key file.
fn load_pem_cert_and_key(
    cert_path: &std::path::Path,
    key_path: &std::path::Path,
) -> Result<
    (
        Vec<rustls::pki_types::CertificateDer<'static>>,
        rustls::pki_types::PrivateKeyDer<'static>,
    ),
    String,
> {
    use std::io::BufReader;

    let cert_bytes = std::fs::read(cert_path)
        .map_err(|e| format!("reading TLS cert {}: {e}", cert_path.display()))?;
    let mut cert_reader = BufReader::new(cert_bytes.as_slice());
    let cert_chain: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&mut cert_reader)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("parsing TLS cert {}: {e}", cert_path.display()))?;
    if cert_chain.is_empty() {
        return Err(format!(
            "no certificates found in {} (expected PEM)",
            cert_path.display()
        ));
    }

    let key_bytes = std::fs::read(key_path)
        .map_err(|e| format!("reading TLS key {}: {e}", key_path.display()))?;
    let mut key_reader = BufReader::new(key_bytes.as_slice());
    let key = rustls_pemfile::private_key(&mut key_reader)
        .map_err(|e| format!("parsing TLS key {}: {e}", key_path.display()))?
        .ok_or_else(|| {
            format!(
                "no private key found in {} (expected PKCS#8/PKCS#1/SEC1 PEM)",
                key_path.display()
            )
        })?;

    Ok((cert_chain, key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tls_client_hello_detection() {
        // Real ClientHello prefix: handshake record, TLS 1.2 record version.
        assert!(looks_like_tls_client_hello(&[0x16, 0x03, 0x01, 0x02, 0x00]));
        assert!(looks_like_tls_client_hello(&[0x16, 0x03, 0x03]));
    }

    #[test]
    fn tls_detection_rejects_http() {
        // "GET ", "POST", "OPTI" — all ASCII letters, never 0x16.
        assert!(!looks_like_tls_client_hello(b"GET /"));
        assert!(!looks_like_tls_client_hello(b"POST"));
        assert!(!looks_like_tls_client_hello(b"OPTIONS *"));
    }

    #[test]
    fn tls_detection_rejects_stun_ice_tcp() {
        // RFC 4571 framed STUN binding request: 2-byte BE length prefix
        // (0x00, 0x14 = 20), then STUN type 0x0001, len, magic cookie.
        // buf[0] == 0x00 != 0x16, so this is never mistaken for TLS.
        let stun = [
            0x00, 0x14, // RFC 4571 length = 20
            0x00, 0x01, // STUN binding request
            0x00, 0x08, // attrs length
            0x21, 0x12, 0xA4, 0x42, // magic cookie at offset 6
        ];
        assert!(!looks_like_tls_client_hello(&stun));
    }

    #[test]
    fn tls_detection_needs_version_byte() {
        // Lone 0x16 without the 0x03 version byte is not enough.
        assert!(!looks_like_tls_client_hello(&[0x16]));
        assert!(!looks_like_tls_client_hello(&[0x16, 0x99, 0x01]));
    }

    #[test]
    fn self_signed_acceptor_builds() {
        // End-to-end: generate a self-signed cert for a concrete bind IP +
        // hostname and assemble a working acceptor. Exercises rcgen ->
        // rustls plumbing on the ring provider.
        let src = TlsCertSource::SelfSigned {
            bind_ip: Some("192.168.1.42".parse().unwrap()),
            hostname: Some("intendant.local".to_string()),
        };
        let acceptor = build_acceptor(&src);
        assert!(
            acceptor.is_ok(),
            "acceptor build failed: {:?}",
            acceptor.err()
        );
    }

    #[test]
    fn self_signed_acceptor_wildcard_bind() {
        // A wildcard bind contributes no IP SAN but still produces a cert
        // (localhost + ::1 + 127.0.0.1 are always present).
        let src = TlsCertSource::SelfSigned {
            bind_ip: Some("0.0.0.0".parse().unwrap()),
            hostname: None,
        };
        assert!(build_acceptor(&src).is_ok());
    }
}
