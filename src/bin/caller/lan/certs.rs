//! Certificate generation for `intendant lan`.
//!
//! Produces:
//! - A self-signed CA (10-year validity), reused across re-runs.
//! - A server cert with `subjectAltName = IP:<lan_ip>` (825-day validity —
//!   iOS requires ≤825 days since 2020).
//! - A client cert (10-year validity).
//! - A password-protected PKCS#12 bundle containing the client key, cert,
//!   and CA chain, packaged with **modern** algorithms (PBES2 /
//!   PBKDF2-HMAC-SHA256 / AES-256-CBC + SHA-256 MAC) that iOS 18 /
//!   macOS 15 import (see `build_modern_p12`).
//!
//! Everything is idempotent — if certs already exist, the CA and client
//! cert are preserved and only the server cert is regenerated when the
//! LAN IP has changed.
//!
//! Pure-Rust: RSA keys are generated with RustCrypto `rsa`, certs are
//! signed with `rcgen` via the `ring` backend (no OpenSSL/aws-lc-sys C
//! toolchain), and the PKCS#12 bundle is built with `p12-keystore`
//! (RustCrypto PBES2/AES).

use std::net::IpAddr;
use std::path::{Path, PathBuf};

use p12_keystore::{Certificate as P12Certificate, KeyStore, KeyStoreEntry, PrivateKeyChain};
use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose, SanType, PKCS_RSA_SHA256,
};
use rsa::pkcs8::{EncodePrivateKey, LineEnding};
use time::{Duration, OffsetDateTime};

use super::{state, LanError, LanResult};

/// Filenames within `cert_dir`.
const CA_KEY: &str = "ca.key";
const CA_CRT: &str = "ca.crt";
const SERVER_KEY: &str = "server.key";
const SERVER_CRT: &str = "server.crt";
const CLIENT_KEY: &str = "client.key";
const CLIENT_CRT: &str = "client.crt";
const CLIENT_P12: &str = "client.p12";

/// Data returned from `ensure_certs` and used downstream by the cert
/// distribution server.
pub struct CertState {
    pub cert_dir: PathBuf,
    pub p12_password: String,
    #[allow(dead_code)]
    pub label: String,
}

/// Generate everything that's missing, idempotently. If the CA and
/// client cert already exist, they're reused; if the server cert's IP
/// doesn't match `lan_ip`, it's regenerated.
pub fn ensure_certs(
    cert_dir: &Path,
    lan_ip: &str,
    label: &str,
    force: bool,
) -> LanResult<CertState> {
    let ca_exists = cert_dir.join(CA_KEY).exists() && cert_dir.join(CA_CRT).exists();
    let client_exists = cert_dir.join(CLIENT_KEY).exists() && cert_dir.join(CLIENT_P12).exists();

    if ca_exists && client_exists && !force {
        println!(
            ":: certs already exist in {} (use --force to regenerate)",
            cert_dir.display()
        );

        if cert_needs_regen_for_ip(cert_dir, lan_ip)? {
            println!("!! IP changed — regenerating server cert");
            regenerate_server_cert(cert_dir, lan_ip)?;
        }

        let password = state::read_p12_password(cert_dir)?;
        return Ok(CertState {
            cert_dir: cert_dir.to_path_buf(),
            p12_password: password,
            label: label.to_string(),
        });
    }

    println!(":: generating certificates...");

    let (ca_cert, ca_key) = generate_ca(label)?;
    write_pem_cert(&cert_dir.join(CA_CRT), &ca_cert)?;
    write_pem_private_key(&cert_dir.join(CA_KEY), &ca_key)?;

    // The CA acts as issuer for both the server and client leaf certs.
    // Derive the issuer from the freshly generated CA cert PEM so it
    // carries the real subject DN + key-usage extensions (same path
    // `recert` uses when reading the CA back from disk).
    let ca_issuer = issuer_from_pem(&ca_cert.pem(), ca_key)?;

    let (server_cert, server_key) = generate_server_cert(&ca_issuer, lan_ip)?;
    write_pem_cert(&cert_dir.join(SERVER_CRT), &server_cert)?;
    write_pem_private_key(&cert_dir.join(SERVER_KEY), &server_key)?;

    let (client_cert, client_key) = generate_client_cert(&ca_issuer, label)?;
    write_pem_cert(&cert_dir.join(CLIENT_CRT), &client_cert)?;
    write_pem_private_key(&cert_dir.join(CLIENT_KEY), &client_key)?;

    let password = random_password(12);
    let p12_bytes = build_modern_p12(&client_key, &client_cert, &[ca_cert], label, &password)?;
    std::fs::write(cert_dir.join(CLIENT_P12), &p12_bytes)?;
    state::write_p12_password(cert_dir, &password)?;

    println!(":: certificates generated in {}", cert_dir.display());
    println!(":: server cert issued for {lan_ip} (valid 825 days)");

    Ok(CertState {
        cert_dir: cert_dir.to_path_buf(),
        p12_password: password,
        label: label.to_string(),
    })
}

/// Regenerate just the server cert, e.g. after a LAN IP change. The CA,
/// client cert, and .p12 are preserved — clients that already imported
/// the CA don't need to do anything.
pub fn recert(cert_dir: &Path, lan_ip: &str, force: bool) -> LanResult<()> {
    if !force && !cert_needs_regen_for_ip(cert_dir, lan_ip)? {
        println!(
            ":: server cert already matches {lan_ip} — nothing to do (use --force to regenerate)"
        );
        return Ok(());
    }

    let old_ip = current_cert_ip(cert_dir).unwrap_or_else(|_| "unknown".to_string());
    println!(":: IP changed: {old_ip} → {lan_ip}");
    regenerate_server_cert(cert_dir, lan_ip)?;
    Ok(())
}

fn regenerate_server_cert(cert_dir: &Path, lan_ip: &str) -> LanResult<()> {
    let ca_pem = std::fs::read_to_string(cert_dir.join(CA_CRT))?;
    let ca_key_pem = std::fs::read_to_string(cert_dir.join(CA_KEY))?;
    let ca_key = KeyPair::from_pem(&ca_key_pem)?;
    let ca_issuer = issuer_from_pem(&ca_pem, ca_key)?;

    let (server_cert, server_key) = generate_server_cert(&ca_issuer, lan_ip)?;
    write_pem_cert(&cert_dir.join(SERVER_CRT), &server_cert)?;
    write_pem_private_key(&cert_dir.join(SERVER_KEY), &server_key)?;
    println!(":: server cert issued for {lan_ip} (valid 825 days)");
    Ok(())
}

/// SHA-256 fingerprint of this daemon's local server cert, in
/// the lowercase-hex format that
/// [`crate::peer::transport::pinning::parse_fingerprint`] consumes.
///
/// Used by `[server.auth] advertised_transport = "pin-self-cert"`
/// to auto-fill the local Agent Card's
/// `auth.transport = PinnedMutualTls` field — operators don't have
/// to compute the fingerprint by hand. Returns `None` when no
/// `server.crt` is present in `cert_dir` (e.g. `intendant lan
/// setup` hasn't been run yet); the caller treats `None` as a
/// configuration error since `pin-self-cert` without a cert is
/// nonsensical.
///
/// Reads the PEM cert, converts to DER, hashes via SHA-256.
/// Same byte-for-byte hash a connecting peer's
/// `PinnedFingerprintVerifier` will compute on the wire, so the
/// pin matches.
pub fn read_server_cert_fingerprint(cert_dir: &Path) -> Option<String> {
    use sha2::{Digest, Sha256};

    let der = read_cert_der(&cert_dir.join(SERVER_CRT)).ok()?;
    let mut hasher = Sha256::new();
    hasher.update(&der);
    let fp: [u8; 32] = hasher.finalize().into();
    let mut s = String::with_capacity(64);
    for byte in fp {
        s.push_str(&format!("{byte:02x}"));
    }
    Some(s)
}

/// Extract the current server cert's subject CN (for drift detection).
/// `generate_server_cert` sets both `CN` and `subjectAltName = IP:<ip>`
/// to the LAN IP, so reading the CN recovers it.
pub fn current_cert_ip(cert_dir: &Path) -> LanResult<String> {
    use x509_parser::prelude::*;

    let der = read_cert_der(&cert_dir.join(SERVER_CRT))?;
    let (_, cert) =
        X509Certificate::from_der(&der).map_err(|e| LanError(format!("parse server cert: {e}")))?;
    for attr in cert.subject().iter_common_name() {
        if let Ok(cn) = attr.as_str() {
            return Ok(cn.to_string());
        }
    }
    Err(LanError("no CN in server cert".into()))
}

fn cert_needs_regen_for_ip(cert_dir: &Path, lan_ip: &str) -> LanResult<bool> {
    if !cert_dir.join(SERVER_CRT).exists() {
        return Ok(true);
    }
    let current = current_cert_ip(cert_dir)?;
    Ok(current != lan_ip)
}

// ── Cert primitives ─────────────────────────────────────────────────────────

/// Build the `CertificateParams` for the CA. Factored out so the same
/// shape is used whether we're self-signing or rederiving an issuer.
fn ca_params_for(label: &str) -> LanResult<CertificateParams> {
    let mut params =
        CertificateParams::new(vec![]).map_err(|e| LanError(format!("ca params: {e}")))?;
    params
        .distinguished_name
        .push(DnType::CommonName, format!("Intendant CA ({label})"));
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let now = OffsetDateTime::now_utc();
    params.not_before = now - Duration::hours(1);
    params.not_after = now + Duration::days(3650);
    Ok(params)
}

fn generate_ca(label: &str) -> LanResult<(Certificate, KeyPair)> {
    let params = ca_params_for(label)?;
    let key = generate_rsa_key_pair()?;
    let cert = params.self_signed(&key)?;
    Ok((cert, key))
}

fn generate_rsa_key_pair() -> LanResult<KeyPair> {
    let mut rng = rand::rngs::OsRng;
    let rsa_key = rsa::RsaPrivateKey::new(&mut rng, 2048)
        .map_err(|e| LanError(format!("generate RSA key: {e}")))?;
    let pkcs8_pem = rsa_key
        .to_pkcs8_pem(LineEnding::LF)
        .map_err(|e| LanError(format!("encode RSA key: {e}")))?;
    KeyPair::from_pem_and_sign_algo(pkcs8_pem.as_str(), &PKCS_RSA_SHA256)
        .map_err(|e| LanError(format!("load RSA key: {e}")))
}

/// Reconstruct a signing [`Issuer`] from a CA cert in PEM form plus its
/// key pair. Used both right after generation and on the `recert` path
/// where the CA is read back from disk. The issuer captures the CA's
/// subject DN and key-usage extensions from the parsed cert.
fn issuer_from_pem(ca_pem: &str, ca_key: KeyPair) -> LanResult<Issuer<'static, KeyPair>> {
    Issuer::from_ca_cert_pem(ca_pem, ca_key).map_err(|e| LanError(format!("load CA issuer: {e}")))
}

fn generate_server_cert(
    ca_issuer: &Issuer<'_, KeyPair>,
    lan_ip: &str,
) -> LanResult<(Certificate, KeyPair)> {
    let ip: IpAddr = lan_ip
        .parse()
        .map_err(|_| LanError(format!("invalid LAN IP: {lan_ip}")))?;

    let mut params =
        CertificateParams::new(vec![]).map_err(|e| LanError(format!("server params: {e}")))?;
    params.distinguished_name.push(DnType::CommonName, lan_ip);
    params.subject_alt_names = vec![SanType::IpAddress(ip)];
    params.is_ca = IsCa::NoCa;
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let now = OffsetDateTime::now_utc();
    params.not_before = now - Duration::hours(1);
    // iOS requires server cert validity ≤825 days.
    params.not_after = now + Duration::days(825);

    let key = generate_rsa_key_pair()?;
    let cert = params.signed_by(&key, ca_issuer)?;
    Ok((cert, key))
}

fn generate_client_cert(
    ca_issuer: &Issuer<'_, KeyPair>,
    label: &str,
) -> LanResult<(Certificate, KeyPair)> {
    let mut params =
        CertificateParams::new(vec![]).map_err(|e| LanError(format!("client params: {e}")))?;
    params
        .distinguished_name
        .push(DnType::CommonName, format!("Intendant Client ({label})"));
    params.is_ca = IsCa::NoCa;
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let now = OffsetDateTime::now_utc();
    params.not_before = now - Duration::hours(1);
    params.not_after = now + Duration::days(3650);

    let key = generate_rsa_key_pair()?;
    let cert = params.signed_by(&key, ca_issuer)?;
    Ok((cert, key))
}

/// Build a PKCS#12 bundle using **modern** encryption: PBES2 with
/// PBKDF2-HMAC-SHA256 and AES-256-CBC, plus a SHA-256 MAC. This is the
/// algorithm set Apple's modern import path (`SecPKCS12Import`, used by
/// Keychain Access on macOS 15+ and the iOS 18 profile installer)
/// accepts. `p12-keystore`'s writer defaults to exactly this
/// (encryption = `PbeWithHmacSha256AndAes256`, MAC = `HmacSha256`,
/// 10 000 iterations), so we build with the defaults.
///
/// (Older Apple releases that predate iOS 18 / macOS 15 only accept the
/// legacy RC2-40/3DES + SHA-1 packaging; supporting those is a
/// deliberately dropped requirement — it was the sole reason OpenSSL was
/// ever in the tree.)
fn build_modern_p12(
    key: &KeyPair,
    cert: &Certificate,
    chain: &[Certificate],
    friendly_name: &str,
    password: &str,
) -> LanResult<Vec<u8>> {
    // Leaf (client) cert first, CA(s) after — the order p12-keystore and
    // Apple both expect (entity first, root last).
    let mut certs = Vec::with_capacity(1 + chain.len());
    certs.push(
        P12Certificate::from_der(cert.der())
            .map_err(|e| LanError(format!("p12 leaf cert: {e}")))?,
    );
    for c in chain {
        certs.push(
            P12Certificate::from_der(c.der()).map_err(|e| LanError(format!("p12 ca cert: {e}")))?,
        );
    }

    // Private key as PKCS#8 DER bytes.
    let key_der = key.serialize_der();
    let entry = KeyStoreEntry::PrivateKeyChain(PrivateKeyChain::new(
        &key_der,
        friendly_name.as_bytes(),
        certs,
    ));

    let mut store = KeyStore::new();
    store.add_entry(friendly_name, entry);

    // Writer defaults: PbeWithHmacSha256AndAes256 + HmacSha256 MAC.
    store
        .writer(password)
        .write()
        .map_err(|e| LanError(format!("p12 write: {e}")))
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn random_password(len: usize) -> String {
    use rand::Rng;
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char)
        .collect()
}

fn write_pem_cert(path: &Path, cert: &Certificate) -> LanResult<()> {
    std::fs::write(path, cert.pem())?;
    Ok(())
}

fn write_pem_private_key(path: &Path, key: &KeyPair) -> LanResult<()> {
    std::fs::write(path, key.serialize_pem())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

/// Read a PEM-encoded certificate file and return its DER bytes.
pub(crate) fn read_cert_der(path: &Path) -> LanResult<Vec<u8>> {
    let bytes = std::fs::read(path)?;
    let pem = pem::parse(&bytes).map_err(|e| LanError(format!("parse cert PEM: {e}")))?;
    Ok(pem.into_contents())
}

#[cfg(test)]
mod tests {
    use super::*;
    use p12_keystore::KeyStore;
    use tempfile::TempDir;

    #[test]
    fn ensure_certs_produces_full_chain() {
        let tmp = TempDir::new().unwrap();
        let state = ensure_certs(tmp.path(), "192.168.1.100", "test-host", false).unwrap();
        for name in [
            "ca.crt",
            "ca.key",
            "server.crt",
            "server.key",
            "client.crt",
            "client.key",
            "client.p12",
        ] {
            let p = tmp.path().join(name);
            assert!(p.exists(), "missing: {}", p.display());
        }
        assert!(!state.p12_password.is_empty());
    }

    #[test]
    fn ensure_certs_produces_rsa_certificate_payloads() {
        use x509_parser::oid_registry::OID_PKCS1_RSAENCRYPTION;
        use x509_parser::prelude::*;

        let tmp = TempDir::new().unwrap();
        ensure_certs(tmp.path(), "192.168.1.100", "rsa-test", false).unwrap();

        for name in [CA_CRT, SERVER_CRT, CLIENT_CRT] {
            let der = read_cert_der(&tmp.path().join(name)).unwrap();
            let (_, cert) = X509Certificate::from_der(&der).unwrap();
            assert_eq!(
                cert.public_key().algorithm.algorithm,
                OID_PKCS1_RSAENCRYPTION,
                "{name} must use an RSA public key for broad Apple profile compatibility"
            );
        }
    }

    #[test]
    fn server_cert_cn_matches_lan_ip() {
        let tmp = TempDir::new().unwrap();
        ensure_certs(tmp.path(), "10.0.0.42", "label", false).unwrap();
        let ip = current_cert_ip(tmp.path()).unwrap();
        assert_eq!(ip, "10.0.0.42");
    }

    /// `read_server_cert_fingerprint` returns `None` when no cert
    /// is present. The caller (`build_local_advertised_auth`) treats
    /// this as a configuration error for `pin-self-cert` since the
    /// pin would be empty.
    #[test]
    fn read_server_cert_fingerprint_returns_none_when_no_cert() {
        let tmp = TempDir::new().unwrap();
        assert!(read_server_cert_fingerprint(tmp.path()).is_none());
    }

    /// `read_server_cert_fingerprint` returns a 64-char lowercase
    /// hex string for an existing cert, matching the format
    /// `parse_fingerprint` consumes. Same SHA-256 a connecting
    /// peer's `PinnedFingerprintVerifier` will compute on the wire,
    /// so the pin matches.
    #[test]
    fn read_server_cert_fingerprint_matches_pinning_format() {
        let tmp = TempDir::new().unwrap();
        ensure_certs(tmp.path(), "10.0.0.99", "fp-test", false).unwrap();

        let fp = read_server_cert_fingerprint(tmp.path()).expect("cert exists");
        assert_eq!(fp.len(), 64, "lowercase hex, no separators");
        assert!(
            fp.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "all chars must be lowercase hex, got: {fp}"
        );
        // Round-trips through the pinning parser — same byte sequence
        // a connecting peer's verifier consumes.
        let parsed = crate::peer::transport::pinning::parse_fingerprint(&fp).unwrap();
        let reformatted = crate::peer::transport::pinning::format_fingerprint(&parsed);
        assert_eq!(fp, reformatted);
    }

    /// `read_server_cert_fingerprint` is deterministic: same cert →
    /// same fingerprint. Recerting (which writes a new cert) changes
    /// the fingerprint.
    #[test]
    fn read_server_cert_fingerprint_changes_on_recert() {
        let tmp = TempDir::new().unwrap();
        ensure_certs(tmp.path(), "10.0.0.1", "label", false).unwrap();
        let before = read_server_cert_fingerprint(tmp.path()).unwrap();

        recert(tmp.path(), "10.0.0.2", false).unwrap();
        let after = read_server_cert_fingerprint(tmp.path()).unwrap();

        assert_ne!(
            before, after,
            "fingerprint must change when the cert is regenerated"
        );
    }

    #[test]
    fn recert_regenerates_on_ip_change() {
        let tmp = TempDir::new().unwrap();
        ensure_certs(tmp.path(), "10.0.0.1", "label", false).unwrap();
        let before = std::fs::read(tmp.path().join("server.crt")).unwrap();

        recert(tmp.path(), "10.0.0.2", false).unwrap();
        let after = std::fs::read(tmp.path().join("server.crt")).unwrap();
        assert_ne!(before, after, "server cert did not change on recert");
        assert_eq!(current_cert_ip(tmp.path()).unwrap(), "10.0.0.2");

        // CA should be unchanged.
        let ca1 = std::fs::read(tmp.path().join("ca.crt")).unwrap();
        let ca2 = std::fs::read(tmp.path().join("ca.crt")).unwrap();
        assert_eq!(ca1, ca2);
    }

    /// **Real Apple importer acceptance.** Round-tripping through the
    /// `p12-keystore` crate (see `p12_is_parseable_with_password`) only
    /// proves *we* can read what *we* wrote — it doesn't prove Apple's
    /// importer accepts our modern PBES2/AES + SHA-256 packaging. This test
    /// drives the actual OS importer, `SecPKCS12Import` (Security.framework,
    /// the same call Keychain Access on macOS 15+ and the iOS 18 profile
    /// installer make), against the generated `client.p12`.
    ///
    /// It imports into a throwaway keychain created in a `TempDir` (never the
    /// user's login keychain) so it leaves no trace and triggers no
    /// interactive password prompt. A successful import that yields the
    /// client identity plus its cert chain is the proof that a genuine Apple
    /// importer — not just the `p12-keystore` reader — accepts the bundle.
    ///
    /// macOS-only: `SecPKCS12Import` and `SecKeychain` are Security.framework
    /// APIs, so the test is gated to `target_os = "macos"`.
    #[cfg(target_os = "macos")]
    #[test]
    fn p12_imports_via_real_macos_keychain() {
        use security_framework::import_export::Pkcs12ImportOptions;
        use security_framework::os::macos::keychain::CreateOptions;

        let tmp = TempDir::new().unwrap();
        let state = ensure_certs(tmp.path(), "10.0.0.1", "keychain-import", false).unwrap();
        let p12_bytes = std::fs::read(tmp.path().join(CLIENT_P12)).unwrap();

        // A disposable, file-backed keychain in the temp dir. Its own
        // password is unrelated to the .p12 password; creating it leaves the
        // login keychain untouched and avoids any UI prompt. `.keychain(...)`
        // is an inherent macOS-only method on `Pkcs12ImportOptions`.
        let keychain = CreateOptions::new()
            .password("intendant-test-keychain")
            .create(tmp.path().join("import-test.keychain"))
            .expect("create temporary keychain");

        // Drive Apple's SecPKCS12Import. If our modern PBES2/AES + SHA-256
        // packaging were not accepted by the real importer, this errors.
        let identities = Pkcs12ImportOptions::new()
            .passphrase(&state.p12_password)
            .keychain(keychain)
            .import(&p12_bytes)
            .expect("SecPKCS12Import must accept the generated client.p12");

        assert_eq!(
            identities.len(),
            1,
            "expected exactly one imported identity from client.p12"
        );
        let imported = &identities[0];
        assert!(
            imported.identity.is_some(),
            "imported item must carry a SecIdentity (private key + leaf cert)"
        );
        // Leaf (client) cert + CA in the validated chain.
        let chain_len = imported
            .cert_chain
            .as_ref()
            .map(|c| c.len())
            .unwrap_or_default();
        assert!(
            chain_len >= 1,
            "imported identity must carry at least the leaf cert in its chain, got {chain_len}"
        );
    }

    /// The generated PKCS#12 parses back with its password, yields the
    /// client identity, and carries the CA in the chain. Uses
    /// `p12-keystore`'s own reader (which understands the modern
    /// PBES2/AES + SHA-256 MAC packaging we write).
    #[test]
    fn p12_is_parseable_with_password() {
        let tmp = TempDir::new().unwrap();
        let state = ensure_certs(tmp.path(), "10.0.0.1", "test", false).unwrap();
        let bytes = std::fs::read(tmp.path().join("client.p12")).unwrap();

        let store = KeyStore::from_pkcs12(&bytes, &state.p12_password).expect("p12 parse");
        let (_alias, chain) = store
            .private_key_chain()
            .expect("private key chain missing from p12");
        assert!(!chain.key().is_empty(), "client key missing from p12");
        // Leaf (client) + CA.
        assert_eq!(chain.chain().len(), 2, "expected client + CA in the chain");
    }

    #[test]
    fn idempotent_reuses_existing_certs() {
        let tmp = TempDir::new().unwrap();
        ensure_certs(tmp.path(), "10.0.0.1", "label", false).unwrap();
        let ca_before = std::fs::read(tmp.path().join("ca.crt")).unwrap();
        let client_before = std::fs::read(tmp.path().join("client.p12")).unwrap();

        ensure_certs(tmp.path(), "10.0.0.1", "label", false).unwrap();
        let ca_after = std::fs::read(tmp.path().join("ca.crt")).unwrap();
        let client_after = std::fs::read(tmp.path().join("client.p12")).unwrap();

        assert_eq!(ca_before, ca_after);
        assert_eq!(client_before, client_after);
    }

    /// Drops a freshly-generated p12 into /tmp so it can be inspected
    /// with `openssl pkcs12 -info` or imported via `SecPKCS12Import`.
    /// Gated behind an env var so it only runs when we explicitly want
    /// a sample.
    #[test]
    fn dump_sample_p12() {
        if std::env::var("LAN_DUMP_SAMPLE_P12").is_err() {
            return;
        }
        let tmp = TempDir::new().unwrap();
        let state = ensure_certs(tmp.path(), "10.0.0.1", "sample", false).unwrap();
        std::fs::copy(tmp.path().join("client.p12"), "/tmp/sample.p12").unwrap();
        std::fs::write("/tmp/sample-p12-password", &state.p12_password).unwrap();
        eprintln!("wrote /tmp/sample.p12 (password in /tmp/sample-p12-password)");
    }

    #[test]
    fn force_regenerates_everything() {
        let tmp = TempDir::new().unwrap();
        ensure_certs(tmp.path(), "10.0.0.1", "label", false).unwrap();
        let ca_before = std::fs::read(tmp.path().join("ca.crt")).unwrap();

        ensure_certs(tmp.path(), "10.0.0.1", "label", true).unwrap();
        let ca_after = std::fs::read(tmp.path().join("ca.crt")).unwrap();
        assert_ne!(ca_before, ca_after, "force did not regenerate CA");
    }
}
