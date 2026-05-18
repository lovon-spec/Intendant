//! Server-cert pinning for the `PinnedMutualTls` transport-auth
//! variant. Custom rustls [`ServerCertVerifier`] that accepts a
//! presented server cert iff its SHA-256 fingerprint matches one of
//! the operator-supplied pinned values.
//!
//! Why pinning on top of (or instead of) plain mTLS: defense in
//! depth. mTLS alone trusts every cert signed by a trusted CA. If
//! the CA is compromised, or a wildcard cert leaks, or someone
//! issues a cert for the same name through a parallel CA, the
//! attacker can pose as the peer. Pinning the *exact* expected
//! cert (or rotation set) closes that gap — only the specific cert
//! whose fingerprint the operator has copied into their config /
//! card is accepted, regardless of CA trust.
//!
//! ## How fingerprints flow
//!
//! - The pinned peer's operator computes the SHA-256 of their
//!   server cert's DER bytes and advertises it in their Agent Card
//!   under `auth.transport = PinnedMutualTls { server_cert_fingerprints }`.
//! - Connecting daemons read the card, build a [`PinnedFingerprintVerifier`]
//!   from the listed fingerprints, and use it as the rustls
//!   `ServerCertVerifier` for both the WebSocket connect and the
//!   agent-card HTTP fetch.
//! - On every TLS handshake, rustls hands `verify_server_cert` the
//!   end-entity cert. We hash and compare. Match → accept; mismatch
//!   → fail with a typed error containing both the presented and
//!   pinned fingerprints so the operator can diagnose copy-paste
//!   errors quickly.
//!
//! ## Accepted fingerprint formats
//!
//! Lowercase hex, with optional `:` separators (the OpenSSL output
//! format `aa:bb:cc:...`). Both `aabbcc...` and `aa:bb:cc:...` parse
//! to the same 32 bytes. Uppercase is also accepted.
//!
//! ## TLS signature verification
//!
//! Pinning replaces only the *cert path* verification; the TLS
//! signature on the handshake is still verified normally using the
//! crypto provider's signature verification algorithms. This way an
//! attacker who steals the cert *bytes* but not the private key
//! still can't impersonate the peer — they'd fail the handshake's
//! signature step.

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error as RustlsError, SignatureScheme};
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// SHA-256 fingerprint of a DER-encoded cert, as 32 raw bytes.
pub type Fingerprint = [u8; 32];

/// Compute the SHA-256 fingerprint of a DER-encoded cert.
pub fn fingerprint_of_der(der: &[u8]) -> Fingerprint {
    let mut hasher = Sha256::new();
    hasher.update(der);
    hasher.finalize().into()
}

/// Format a fingerprint as lowercase hex (no separators). The
/// canonical wire form for `server_cert_fingerprints` entries.
pub fn format_fingerprint(fp: &Fingerprint) -> String {
    let mut s = String::with_capacity(64);
    for byte in fp {
        s.push_str(&format!("{byte:02x}"));
    }
    s
}

/// Parse a fingerprint string into its 32-byte form. Accepts:
///
/// - Plain lowercase or uppercase hex: `"aabbcc...11"` (64 chars).
/// - Hex with `:` separators (OpenSSL output): `"aa:bb:cc:...:11"`.
/// - Mixed case + separators: handled by lowercasing and stripping.
///
/// Returns `Err` for any other format (wrong length after stripping,
/// non-hex characters, etc.) so the operator sees a clear failure
/// rather than the verifier silently rejecting every cert.
pub fn parse_fingerprint(s: &str) -> Result<Fingerprint, String> {
    let cleaned: String = s.chars().filter(|c| *c != ':').collect();
    if cleaned.len() != 64 {
        return Err(format!(
            "fingerprint must be 64 hex chars (32 bytes); got {} chars after stripping `:`",
            cleaned.len()
        ));
    }
    let mut out = [0u8; 32];
    for (i, byte_out) in out.iter_mut().enumerate() {
        let pair = &cleaned[i * 2..i * 2 + 2];
        *byte_out = u8::from_str_radix(pair, 16)
            .map_err(|_| format!("non-hex characters in fingerprint at offset {}", i * 2))?;
    }
    Ok(out)
}

/// Custom rustls verifier that accepts certs whose SHA-256
/// fingerprint matches one of the pinned values.
#[derive(Debug)]
pub struct PinnedFingerprintVerifier {
    pinned: Vec<Fingerprint>,
    /// Crypto provider used for the TLS signature checks (which we
    /// still want — only cert-path verification is replaced by
    /// pinning). Cloned from the process-installed default if
    /// available; otherwise constructed from the rustls crate's
    /// default ring provider.
    provider: Arc<CryptoProvider>,
}

impl PinnedFingerprintVerifier {
    /// Build from a pre-parsed list of fingerprints. Empty list is
    /// allowed at construction time — the verifier rejects every
    /// connection until at least one fingerprint is added. Use
    /// [`from_strings`] to parse operator-supplied hex inputs first.
    pub fn new(pinned: Vec<Fingerprint>) -> Self {
        Self {
            pinned,
            provider: default_crypto_provider(),
        }
    }

    /// Build from a list of operator-supplied fingerprint strings.
    /// Each is parsed via [`parse_fingerprint`]; the first parse
    /// failure aborts and returns an error mentioning the offending
    /// entry so the operator can fix their config.
    pub fn from_strings<I, S>(strings: I) -> Result<Self, String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut pinned = Vec::new();
        for s in strings {
            let s = s.as_ref();
            let fp = parse_fingerprint(s).map_err(|e| format!("invalid fingerprint {s:?}: {e}"))?;
            pinned.push(fp);
        }
        Ok(Self::new(pinned))
    }

    /// Number of pinned fingerprints. Useful for logging /
    /// invariant checks.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.pinned.len()
    }
}

/// Resolve a `CryptoProvider` for signature verification.
///
/// Prefers the process-installed default if one exists (typically
/// installed by reqwest's rustls-tls feature or tokio-tungstenite's
/// rustls-tls-native-roots feature when they initialize). Falls back
/// to a fresh ring provider so the verifier still works in test
/// contexts where no provider has been installed yet.
fn default_crypto_provider() -> Arc<CryptoProvider> {
    if let Some(p) = CryptoProvider::get_default() {
        return p.clone();
    }
    Arc::new(rustls::crypto::ring::default_provider())
}

impl ServerCertVerifier for PinnedFingerprintVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, RustlsError> {
        let fp = fingerprint_of_der(end_entity.as_ref());
        if self.pinned.contains(&fp) {
            return Ok(ServerCertVerified::assertion());
        }
        // Mismatch: include the presented fingerprint AND the pinned
        // ones in the error so the operator can immediately tell
        // whether they (a) typo'd a pinned entry, (b) put the wrong
        // peer's fingerprint in, or (c) the peer rotated its cert
        // and the config needs updating.
        let presented = format_fingerprint(&fp);
        let pinned: Vec<String> = self.pinned.iter().map(format_fingerprint).collect();
        Err(RustlsError::General(format!(
            "server cert fingerprint {presented} doesn't match any pinned: [{}]",
            pinned.join(", ")
        )))
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        // Still verify the handshake signature: an attacker who has
        // the cert bytes but not the private key would fail here,
        // closing the "stolen cert without key" attack vector that
        // pure cert-path verification would also have closed.
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Build a rustls `ClientConfig` that delegates server-cert
/// verification to a [`PinnedFingerprintVerifier`]. Used by both
/// the WebSocket connect path (via `tokio_tungstenite::Connector`)
/// and the agent-card HTTP fetch path (via reqwest's
/// `use_preconfigured_tls`) when the peer's `auth.transport` is
/// `PinnedMutualTls`.
pub fn pinned_client_config(verifier: PinnedFingerprintVerifier) -> rustls::ClientConfig {
    let provider = verifier.provider.clone();
    rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(rustls::DEFAULT_VERSIONS)
        .expect("default rustls protocol versions are valid")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_of_der_is_sha256() {
        let bytes = b"hello world";
        let fp = fingerprint_of_der(bytes);
        // Known SHA-256 of "hello world".
        let expected = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";
        assert_eq!(format_fingerprint(&fp), expected);
    }

    #[test]
    fn format_fingerprint_is_lowercase_hex_no_separators() {
        let mut fp = [0u8; 32];
        fp[0] = 0xAB;
        fp[31] = 0xCD;
        let s = format_fingerprint(&fp);
        assert_eq!(s.len(), 64);
        assert!(s.starts_with("ab"));
        assert!(s.ends_with("cd"));
        assert!(!s.contains(':'));
    }

    #[test]
    fn parse_fingerprint_accepts_plain_hex() {
        let fp =
            parse_fingerprint("aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899")
                .unwrap();
        assert_eq!(fp[0], 0xaa);
        assert_eq!(fp[1], 0xbb);
        assert_eq!(fp[31], 0x99);
    }

    #[test]
    fn parse_fingerprint_accepts_colon_separators() {
        let with = parse_fingerprint(
            "aa:bb:cc:dd:ee:ff:00:11:22:33:44:55:66:77:88:99:aa:bb:cc:dd:ee:ff:00:11:22:33:44:55:66:77:88:99",
        )
        .unwrap();
        let without =
            parse_fingerprint("aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899")
                .unwrap();
        assert_eq!(with, without);
    }

    #[test]
    fn parse_fingerprint_accepts_uppercase() {
        // u8::from_str_radix(..., 16) is case-insensitive, so this works
        // for both cases without an explicit `to_lowercase()` call.
        let upper =
            parse_fingerprint("AABBCCDDEEFF00112233445566778899AABBCCDDEEFF00112233445566778899")
                .unwrap();
        let lower =
            parse_fingerprint("aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899")
                .unwrap();
        assert_eq!(upper, lower);
    }

    #[test]
    fn parse_fingerprint_rejects_wrong_length() {
        let err = parse_fingerprint("aabb").unwrap_err();
        assert!(err.contains("64 hex chars"));
    }

    #[test]
    fn parse_fingerprint_rejects_non_hex() {
        let err =
            parse_fingerprint("zz0000000000000000000000000000000000000000000000000000000000000000")
                .unwrap_err();
        // Could fail on length (65 chars) or non-hex; either is
        // acceptable diagnostic. Test the length path explicitly:
        assert!(err.contains("64 hex chars") || err.contains("non-hex"));
    }

    #[test]
    fn parse_fingerprint_proper_non_hex_path() {
        // Exactly 64 chars but with non-hex characters — exercises
        // the per-byte parse error rather than the length check.
        let mut s = "ab".repeat(31);
        s.push_str("zz");
        let err = parse_fingerprint(&s).unwrap_err();
        assert!(err.contains("non-hex"));
    }

    #[test]
    fn from_strings_parses_multiple() {
        let v = PinnedFingerprintVerifier::from_strings([
            "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899",
            "11:22:33:44:55:66:77:88:99:aa:bb:cc:dd:ee:ff:00:11:22:33:44:55:66:77:88:99:aa:bb:cc:dd:ee:ff:00",
        ])
        .unwrap();
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn from_strings_reports_offending_entry() {
        let err = PinnedFingerprintVerifier::from_strings(["valid-but-wrong-length"]).unwrap_err();
        assert!(err.contains("valid-but-wrong-length"));
    }

    /// Fingerprints are compared exactly: matching cert is accepted.
    /// Construct a pinned verifier whose pinned set contains the
    /// fingerprint of a synthetic byte sequence, then call
    /// `verify_server_cert` directly on that same sequence wrapped
    /// in a `CertificateDer`. Should pass.
    #[test]
    fn verify_server_cert_accepts_matching_fingerprint() {
        let cert_bytes = b"synthetic-cert-bytes-for-test";
        let fp = fingerprint_of_der(cert_bytes);
        let v = PinnedFingerprintVerifier::new(vec![fp]);
        let cert = CertificateDer::from(cert_bytes.to_vec());
        let server_name = ServerName::try_from("test.example").unwrap();
        let result = v.verify_server_cert(&cert, &[], &server_name, &[], UnixTime::now());
        assert!(result.is_ok(), "expected accept, got: {result:?}");
    }

    /// Fingerprints don't match: rejected with a clear error
    /// containing the presented fingerprint so the operator can
    /// diagnose.
    #[test]
    fn verify_server_cert_rejects_mismatched_fingerprint() {
        let pinned_fp = fingerprint_of_der(b"some-other-bytes");
        let v = PinnedFingerprintVerifier::new(vec![pinned_fp]);
        let cert = CertificateDer::from(b"different-bytes".to_vec());
        let server_name = ServerName::try_from("test.example").unwrap();
        let result = v.verify_server_cert(&cert, &[], &server_name, &[], UnixTime::now());
        let err = result.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("doesn't match"), "got: {msg}");
        // Presented fingerprint is in the error.
        let presented = format_fingerprint(&fingerprint_of_der(b"different-bytes"));
        assert!(
            msg.contains(&presented),
            "presented fingerprint in error: {msg}"
        );
    }

    /// Empty pinned list rejects every cert (operator gave us no
    /// fingerprints to match against).
    #[test]
    fn verify_server_cert_rejects_when_no_fingerprints_pinned() {
        let v = PinnedFingerprintVerifier::new(vec![]);
        let cert = CertificateDer::from(b"any-bytes".to_vec());
        let server_name = ServerName::try_from("test.example").unwrap();
        let result = v.verify_server_cert(&cert, &[], &server_name, &[], UnixTime::now());
        assert!(result.is_err());
    }

    /// `pinned_client_config` produces a usable `rustls::ClientConfig`.
    /// Smoke test only — full TLS handshake testing requires a test
    /// gateway with a known cert, deferred to integration tests.
    #[test]
    fn pinned_client_config_builds_without_panic() {
        let v = PinnedFingerprintVerifier::new(vec![[0u8; 32]]);
        let _config = pinned_client_config(v);
    }
}
