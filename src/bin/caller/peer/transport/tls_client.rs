//! Client-side TLS helpers for peer federation.
//!
//! The dashboard gateway verifies inbound mTLS using the access CA. When this
//! daemon connects to another Intendant, it can act as an mTLS client by
//! presenting the installed access `client.crt` / `client.key` identity. These
//! helpers keep the initial Agent Card fetch and the later WebSocket attach on
//! the same TLS policy.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::peer::transport::pinning::{Fingerprint, PinnedFingerprintVerifier};
use crate::peer::PeerError;

/// PEM client identity on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientIdentityPaths {
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
}

/// True for URLs whose transport performs a TLS handshake.
pub fn url_uses_tls(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("wss://")
}

/// Return the installed access client identity if both PEM files exist.
///
/// Missing files are not an error here: TLS-only public peers do not need a
/// client cert, and non-TLS test/loopback peers should not be forced to run
/// `intendant access setup`. If a peer actually requires mTLS and these files
/// are absent, the handshake fails closed with the peer's TLS alert.
pub fn installed_access_client_identity_paths() -> Option<ClientIdentityPaths> {
    let cert_dir = crate::access::backend::select_backend().cert_dir();
    let cert_path = cert_dir.join("client.crt");
    let key_path = cert_dir.join("client.key");
    if cert_path.exists() && key_path.exists() {
        Some(ClientIdentityPaths {
            cert_path,
            key_path,
        })
    } else {
        None
    }
}

/// Build a reqwest client for peer Agent Card discovery.
///
/// If `pinned_fingerprints` is non-empty, server certificate verification is
/// by exact SHA-256 fingerprint. If `client_identity` is present, the same
/// rustls config also presents the daemon's client certificate during the TLS
/// handshake.
pub fn reqwest_client(
    timeout: Duration,
    pinned_fingerprints: &[Fingerprint],
    client_identity: Option<&ClientIdentityPaths>,
) -> Result<reqwest::Client, PeerError> {
    let mut builder = reqwest::Client::builder().timeout(timeout);
    if let Some(config) = rustls_client_config(pinned_fingerprints, client_identity)? {
        builder = builder.use_preconfigured_tls(config);
    }
    builder
        .build()
        .map_err(|e| PeerError::CardFetch(format!("build http client: {e}")))
}

/// Build a rustls client config for peer WebSocket/HTTP clients.
///
/// Returns `None` when neither pinning nor client-auth is required, allowing
/// callers to use their library's default TLS connector.
pub fn rustls_client_config(
    pinned_fingerprints: &[Fingerprint],
    client_identity: Option<&ClientIdentityPaths>,
) -> Result<Option<rustls::ClientConfig>, PeerError> {
    let identity = match client_identity {
        Some(paths) => Some(load_client_identity(paths)?),
        None => None,
    };

    if !pinned_fingerprints.is_empty() {
        let verifier = PinnedFingerprintVerifier::new(pinned_fingerprints.to_vec());
        let config = super::pinning::pinned_client_config_with_client_auth(verifier, identity)
            .map_err(|e| PeerError::Auth(format!("peer TLS client identity setup failed: {e}")))?;
        return Ok(Some(config));
    }

    let Some((cert_chain, key)) = identity else {
        return Ok(None);
    };

    let roots = crate::web_tls::load_native_root_store()
        .map_err(|e| PeerError::Auth(format!("load native TLS roots: {e}")))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(rustls::DEFAULT_VERSIONS)
        .map_err(|e| PeerError::Auth(format!("peer TLS protocol setup failed: {e}")))?
        .with_root_certificates(roots)
        .with_client_auth_cert(cert_chain, key)
        .map_err(|e| PeerError::Auth(format!("peer TLS client identity setup failed: {e}")))?;
    Ok(Some(config))
}

fn load_client_identity(
    paths: &ClientIdentityPaths,
) -> Result<crate::web_tls::RustlsIdentity, PeerError> {
    crate::web_tls::load_pem_cert_and_key(&paths.cert_path, &paths.key_path).map_err(|e| {
        PeerError::Auth(format!(
            "load peer mTLS client identity {} / {}: {e}",
            paths.cert_path.display(),
            paths.key_path.display()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_uses_tls_detects_http_and_ws_schemes() {
        assert!(url_uses_tls("https://host/.well-known/agent-card.json"));
        assert!(url_uses_tls("wss://host/ws"));
        assert!(!url_uses_tls("http://host/.well-known/agent-card.json"));
        assert!(!url_uses_tls("ws://host/ws"));
    }

    #[test]
    fn rustls_client_config_none_without_pin_or_identity() {
        let config = rustls_client_config(&[], None).unwrap();
        assert!(config.is_none());
    }

    #[test]
    fn rustls_client_config_builds_with_pinned_server_and_client_identity() {
        let dir = tempfile::tempdir().unwrap();
        let names = crate::access::certs::ServerNames::new(
            "127.0.0.1".parse().unwrap(),
            Vec::<std::net::IpAddr>::new(),
            Vec::<String>::new(),
        )
        .unwrap();
        crate::access::certs::ensure_certs(dir.path(), &names, "peer-client-test", false).unwrap();

        let identity = ClientIdentityPaths {
            cert_path: dir.path().join("client.crt"),
            key_path: dir.path().join("client.key"),
        };
        let config = rustls_client_config(&[[0u8; 32]], Some(&identity));
        assert!(config.is_ok(), "config should build: {:?}", config.err());
        assert!(config.unwrap().is_some());
    }
}
