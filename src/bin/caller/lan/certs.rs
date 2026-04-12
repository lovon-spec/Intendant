//! Certificate generation for `intendant lan`.
//!
//! Produces:
//! - A self-signed CA (10-year validity), reused across re-runs.
//! - A server cert with `subjectAltName = IP:<lan_ip>` (825-day validity —
//!   iOS requires ≤825 days since 2020).
//! - A client cert (10-year validity).
//! - A password-protected PKCS#12 bundle containing the client key, cert,
//!   and CA chain, packaged with **legacy** algorithms so older iOS
//!   versions can import it (see `build_legacy_p12`).
//!
//! Everything is idempotent — if certs already exist, the CA and client
//! cert are preserved and only the server cert is regenerated when the
//! LAN IP has changed.

use std::path::{Path, PathBuf};

use openssl::asn1::Asn1Time;
use openssl::bn::{BigNum, MsbOption};
use openssl::hash::MessageDigest;
use openssl::nid::Nid;
use openssl::pkcs12::Pkcs12;
use openssl::pkey::{PKey, Private};
use openssl::rsa::Rsa;
use openssl::stack::Stack;
use openssl::x509::extension::{
    BasicConstraints, ExtendedKeyUsage, KeyUsage, SubjectAlternativeName,
};
use openssl::x509::{X509Builder, X509NameBuilder, X509};

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
    load_legacy_provider_if_needed();

    let ca_exists = cert_dir.join(CA_KEY).exists() && cert_dir.join(CA_CRT).exists();
    let client_exists =
        cert_dir.join(CLIENT_KEY).exists() && cert_dir.join(CLIENT_P12).exists();

    if ca_exists && client_exists && !force {
        println!(":: certs already exist in {} (use --force to regenerate)", cert_dir.display());

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

    let (server_cert, server_key) = generate_server_cert(&ca_cert, &ca_key, lan_ip)?;
    write_pem_cert(&cert_dir.join(SERVER_CRT), &server_cert)?;
    write_pem_private_key(&cert_dir.join(SERVER_KEY), &server_key)?;

    let (client_cert, client_key) = generate_client_cert(&ca_cert, &ca_key, label)?;
    write_pem_cert(&cert_dir.join(CLIENT_CRT), &client_cert)?;
    write_pem_private_key(&cert_dir.join(CLIENT_KEY), &client_key)?;

    let password = random_password(12);
    let p12_bytes =
        build_legacy_p12(&client_key, &client_cert, &[ca_cert.clone()], label, &password)?;
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
    load_legacy_provider_if_needed();

    if !force && !cert_needs_regen_for_ip(cert_dir, lan_ip)? {
        println!(":: server cert already matches {lan_ip} — nothing to do (use --force to regenerate)");
        return Ok(());
    }

    let old_ip = current_cert_ip(cert_dir).unwrap_or_else(|_| "unknown".to_string());
    println!(":: IP changed: {old_ip} → {lan_ip}");
    regenerate_server_cert(cert_dir, lan_ip)?;
    Ok(())
}

fn regenerate_server_cert(cert_dir: &Path, lan_ip: &str) -> LanResult<()> {
    let ca_cert = read_pem_cert(&cert_dir.join(CA_CRT))?;
    let ca_key = read_pem_private_key(&cert_dir.join(CA_KEY))?;
    let (server_cert, server_key) = generate_server_cert(&ca_cert, &ca_key, lan_ip)?;
    write_pem_cert(&cert_dir.join(SERVER_CRT), &server_cert)?;
    write_pem_private_key(&cert_dir.join(SERVER_KEY), &server_key)?;
    println!(":: server cert issued for {lan_ip} (valid 825 days)");
    Ok(())
}

/// Extract the current server cert's SAN IP (for drift detection).
pub fn current_cert_ip(cert_dir: &Path) -> LanResult<String> {
    let cert = read_pem_cert(&cert_dir.join(SERVER_CRT))?;
    // openssl doesn't expose a typed SAN reader easily — parse the
    // subject CN instead, which we set to the IP. If someone ever
    // changes generate_server_cert to use a hostname, this needs to
    // parse the SAN extension directly.
    let subject = cert.subject_name();
    for entry in subject.entries_by_nid(Nid::COMMONNAME) {
        if let Ok(cn) = entry.data().as_utf8() {
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

fn generate_ca(label: &str) -> LanResult<(X509, PKey<Private>)> {
    let rsa = Rsa::generate(2048)?;
    let pkey = PKey::from_rsa(rsa)?;

    let mut name = X509NameBuilder::new()?;
    name.append_entry_by_nid(Nid::COMMONNAME, &format!("Intendant CA ({label})"))?;
    let name = name.build();

    let serial = random_serial()?;
    let not_before = Asn1Time::days_from_now(0)?;
    let not_after = Asn1Time::days_from_now(3650)?;

    let mut builder = X509Builder::new()?;
    builder.set_version(2)?; // v3
    builder.set_serial_number(&serial)?;
    builder.set_subject_name(&name)?;
    builder.set_issuer_name(&name)?;
    builder.set_pubkey(&pkey)?;
    builder.set_not_before(&not_before)?;
    builder.set_not_after(&not_after)?;

    let bc = BasicConstraints::new().critical().ca().build()?;
    builder.append_extension(bc)?;
    let ku = KeyUsage::new()
        .critical()
        .key_cert_sign()
        .crl_sign()
        .build()?;
    builder.append_extension(ku)?;

    builder.sign(&pkey, MessageDigest::sha256())?;
    Ok((builder.build(), pkey))
}

fn generate_server_cert(
    ca_cert: &X509,
    ca_key: &PKey<Private>,
    lan_ip: &str,
) -> LanResult<(X509, PKey<Private>)> {
    let rsa = Rsa::generate(2048)?;
    let pkey = PKey::from_rsa(rsa)?;

    let mut name = X509NameBuilder::new()?;
    name.append_entry_by_nid(Nid::COMMONNAME, lan_ip)?;
    let name = name.build();

    let serial = random_serial()?;
    let not_before = Asn1Time::days_from_now(0)?;
    // iOS requires server cert validity ≤825 days.
    let not_after = Asn1Time::days_from_now(825)?;

    let mut builder = X509Builder::new()?;
    builder.set_version(2)?;
    builder.set_serial_number(&serial)?;
    builder.set_subject_name(&name)?;
    builder.set_issuer_name(ca_cert.subject_name())?;
    builder.set_pubkey(&pkey)?;
    builder.set_not_before(&not_before)?;
    builder.set_not_after(&not_after)?;

    builder.append_extension(BasicConstraints::new().critical().build()?)?;
    builder.append_extension(
        KeyUsage::new()
            .critical()
            .digital_signature()
            .key_encipherment()
            .build()?,
    )?;
    builder.append_extension(ExtendedKeyUsage::new().server_auth().build()?)?;

    let san = SubjectAlternativeName::new()
        .ip(lan_ip)
        .build(&builder.x509v3_context(Some(ca_cert), None))?;
    builder.append_extension(san)?;

    builder.sign(ca_key, MessageDigest::sha256())?;
    Ok((builder.build(), pkey))
}

fn generate_client_cert(
    ca_cert: &X509,
    ca_key: &PKey<Private>,
    label: &str,
) -> LanResult<(X509, PKey<Private>)> {
    let rsa = Rsa::generate(2048)?;
    let pkey = PKey::from_rsa(rsa)?;

    let mut name = X509NameBuilder::new()?;
    name.append_entry_by_nid(Nid::COMMONNAME, &format!("Intendant Client ({label})"))?;
    let name = name.build();

    let serial = random_serial()?;
    let not_before = Asn1Time::days_from_now(0)?;
    let not_after = Asn1Time::days_from_now(3650)?;

    let mut builder = X509Builder::new()?;
    builder.set_version(2)?;
    builder.set_serial_number(&serial)?;
    builder.set_subject_name(&name)?;
    builder.set_issuer_name(ca_cert.subject_name())?;
    builder.set_pubkey(&pkey)?;
    builder.set_not_before(&not_before)?;
    builder.set_not_after(&not_after)?;

    builder.append_extension(BasicConstraints::new().critical().build()?)?;
    builder.append_extension(
        KeyUsage::new()
            .critical()
            .digital_signature()
            .key_encipherment()
            .build()?,
    )?;
    builder.append_extension(ExtendedKeyUsage::new().client_auth().build()?)?;

    builder.sign(ca_key, MessageDigest::sha256())?;
    Ok((builder.build(), pkey))
}

/// Build a PKCS#12 bundle using **legacy** encryption algorithms for
/// broad iOS/macOS compatibility. The OpenSSL 3 defaults (AES-256-CBC +
/// SHA-256) are rejected by older Apple import paths; Apple only broadened
/// PKCS#12 algorithm support in iOS 18 / macOS 15. We need to keep old
/// devices working, so we force:
///
/// - key bag = `PBE_WITHSHA1AND3_KEY_TRIPLEDES_CBC`
/// - cert bag = `PBE_WITHSHA1AND40BITRC2_CBC`
/// - MAC digest = SHA-1, mac_iter = 1
///
/// RC2-40 is only available in OpenSSL 3's legacy provider, which we
/// load once at module init (see `load_legacy_provider_if_needed`).
fn build_legacy_p12(
    key: &PKey<Private>,
    cert: &X509,
    chain: &[X509],
    friendly_name: &str,
    password: &str,
) -> LanResult<Vec<u8>> {
    let mut builder = Pkcs12::builder();
    builder.name(friendly_name);
    builder.pkey(key);
    builder.cert(cert);
    if !chain.is_empty() {
        let mut stack = Stack::<X509>::new()?;
        for c in chain {
            stack.push(c.clone())?;
        }
        builder.ca(stack);
    }

    // Legacy algorithms per iOS compatibility research.
    builder.key_algorithm(Nid::PBE_WITHSHA1AND3_KEY_TRIPLEDES_CBC);
    builder.cert_algorithm(Nid::PBE_WITHSHA1AND40BITRC2_CBC);
    builder.key_iter(2048);
    builder.mac_iter(1);
    builder.mac_md(MessageDigest::sha1());

    let p12 = builder.build2(password)?;
    let der = p12.to_der()?;
    Ok(der)
}

/// OpenSSL 3 ships AES-based defaults and moves legacy ciphers (including
/// RC2-40 which we need for cert-bag encryption) to the `legacy` provider,
/// which must be explicitly loaded. Safe to call more than once. The
/// `ossl3` cfg is set by `build.rs` based on `DEP_OPENSSL_VERSION_NUMBER`
/// exported by openssl-sys. On OpenSSL 1.x the provider API doesn't exist
/// and all algorithms are available unconditionally.
#[cfg(ossl3)]
static LEGACY_PROVIDER: std::sync::OnceLock<
    Result<(openssl::provider::Provider, openssl::provider::Provider), String>,
> = std::sync::OnceLock::new();

#[cfg(ossl3)]
fn load_legacy_provider_if_needed() {
    // Providers loaded via `try_load(.., retain_fallbacks=true)` must be
    // kept alive — dropping them unloads the provider from the default
    // library context. We stash them in a `OnceLock` so they persist for
    // the lifetime of the process.
    LEGACY_PROVIDER.get_or_init(|| {
        let legacy = openssl::provider::Provider::try_load(None, "legacy", true)
            .map_err(|e| format!("legacy provider: {e}"))?;
        // Loading a non-default provider disables the implicit default
        // provider; reload it explicitly so modern algorithms still work.
        let default = openssl::provider::Provider::try_load(None, "default", true)
            .map_err(|e| format!("default provider: {e}"))?;
        Ok((legacy, default))
    });
}

#[cfg(not(ossl3))]
fn load_legacy_provider_if_needed() {}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn random_serial() -> LanResult<openssl::asn1::Asn1Integer> {
    let mut bn = BigNum::new()?;
    bn.rand(159, MsbOption::MAYBE_ZERO, false)?;
    Ok(bn.to_asn1_integer()?)
}

fn random_password(len: usize) -> String {
    use openssl::rand::rand_bytes;
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut raw = vec![0u8; len];
    rand_bytes(&mut raw).expect("openssl rand");
    raw.iter()
        .map(|b| ALPHABET[(*b as usize) % ALPHABET.len()] as char)
        .collect()
}

fn write_pem_cert(path: &Path, cert: &X509) -> LanResult<()> {
    std::fs::write(path, cert.to_pem()?)?;
    Ok(())
}

fn write_pem_private_key(path: &Path, key: &PKey<Private>) -> LanResult<()> {
    let pem = key.private_key_to_pem_pkcs8()?;
    std::fs::write(path, pem)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

fn read_pem_cert(path: &Path) -> LanResult<X509> {
    let bytes = std::fs::read(path)?;
    Ok(X509::from_pem(&bytes)?)
}

fn read_pem_private_key(path: &Path) -> LanResult<PKey<Private>> {
    let bytes = std::fs::read(path)?;
    Ok(PKey::private_key_from_pem(&bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use openssl::pkcs12::Pkcs12;
    use tempfile::TempDir;

    #[test]
    fn ensure_certs_produces_full_chain() {
        let tmp = TempDir::new().unwrap();
        let state = ensure_certs(tmp.path(), "192.168.1.100", "test-host", false).unwrap();
        for name in ["ca.crt", "ca.key", "server.crt", "server.key",
                     "client.crt", "client.key", "client.p12"] {
            let p = tmp.path().join(name);
            assert!(p.exists(), "missing: {}", p.display());
        }
        assert!(!state.p12_password.is_empty());
    }

    #[test]
    fn server_cert_cn_matches_lan_ip() {
        let tmp = TempDir::new().unwrap();
        ensure_certs(tmp.path(), "10.0.0.42", "label", false).unwrap();
        let ip = current_cert_ip(tmp.path()).unwrap();
        assert_eq!(ip, "10.0.0.42");
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

    #[test]
    fn p12_is_parseable_with_password() {
        let tmp = TempDir::new().unwrap();
        let state = ensure_certs(tmp.path(), "10.0.0.1", "test", false).unwrap();
        let bytes = std::fs::read(tmp.path().join("client.p12")).unwrap();

        let p12 = Pkcs12::from_der(&bytes).expect("p12 parse");
        let parsed = p12.parse2(&state.p12_password).expect("p12 parse2");
        assert!(parsed.cert.is_some(), "client cert missing from p12");
        assert!(parsed.pkey.is_some(), "client key missing from p12");
        let chain = parsed.ca.expect("ca chain missing from p12");
        assert_eq!(chain.len(), 1, "expected exactly 1 CA in chain");
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
    /// with `openssl pkcs12 -info`. Gated behind an env var so it only
    /// runs when we explicitly want a sample.
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

