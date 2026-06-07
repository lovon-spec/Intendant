//! `intendant lan` subcommand: port of `scripts/setup-lan.sh` and
//! `scripts/setup-lan-guest-macos.sh` to Rust.
//!
//! Sets up an mTLS nginx reverse proxy in front of `intendant --web` so
//! LAN clients (phones, tablets, other boxes) can reach the dashboard
//! over HTTPS authenticated by a client certificate.
//!
//! Shared across platforms: cert generation (pure-Rust rcgen +
//! p12-keystore), nginx config rendering, client cert distribution, import
//! instructions. Platform differences (apt vs brew, systemd vs launchd, cert
//! dir location) are isolated behind the `LanBackend` trait.

use std::fmt;

pub mod backend;
// `certs` is pure-Rust (rcgen + p12-keystore) and compiles on every
// platform, so it stays ungated — `read_server_cert_fingerprint` backs the
// `pin-self-cert` transport, which doesn't need the nginx flow.
//
// The nginx + distribution machinery (`cert_server`, `instructions`,
// `nginx_config`, `wizard`) and the apt/brew/systemd setup flow are still
// deferred on Windows (Tier-0): they depend on nginx + apt/brew +
// systemd/launchd, none of which apply. `backend` and `state` stay available
// everywhere because `resolve_host_label` / `routable_local_addrs` (called by
// the web dashboard, not just `lan setup`) need them.
//
// On Windows only `certs::read_server_cert_fingerprint` is reachable; the
// cert/p12 *generation* functions are exercised solely by the still-deferred
// `lan setup` nginx flow, so they compile-but-are-unused there. Silence those
// dead-code warnings on Windows; every item is live on macOS/Linux.
#[cfg(not(target_os = "windows"))]
pub mod cert_server;
#[cfg_attr(target_os = "windows", allow(dead_code))]
pub mod certs;
#[cfg(not(target_os = "windows"))]
pub mod instructions;
#[cfg(not(target_os = "windows"))]
pub mod nginx_config;
pub mod state;
#[cfg(not(target_os = "windows"))]
pub mod wizard;

/// Resolve the multi-host `HostId` for this machine.
///
/// Checks the platform cert dir's `host_label` file first (written by
/// `intendant lan setup --name …`), then falls back to the system
/// hostname. Returns `"local"` only if both fail, so the dashboard
/// always has *some* label to display.
///
/// Callable from `intendant --web` without running any `lan` action,
/// because the backend's `cert_dir()` is a pure path accessor with no
/// privileged I/O.
pub fn resolve_host_label() -> String {
    let be = backend::select_backend();
    let cert_dir = be.cert_dir();
    if let Ok(label) = state::read_host_label(&cert_dir) {
        if !label.is_empty() {
            return label;
        }
    }
    // Fallback: system hostname (gethostname, not HOSTNAME env).
    if let Ok(h) = hostname() {
        if !h.is_empty() {
            return h;
        }
    }
    "local".to_string()
}

/// Read the system hostname via the POSIX `gethostname` call.
fn hostname() -> Result<String, std::io::Error> {
    let output = std::process::Command::new("hostname").output()?;
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(s)
}

/// Enumerate the local machine's routable IP addresses (one entry per
/// interface address that's globally usable). Used by:
///
/// - The federation advertise side ([`crate::web_gateway::resolve_advertise_urls`])
///   to auto-populate the Agent Card with one URL per interface — the
///   ICE host-candidate-gathering pattern, applied to peer discovery.
/// - The WebRTC display path
///   ([`crate::display::webrtc::WebRtcPeer::new`]) to bind one UDP socket
///   per interface and emit a matching host candidate. WebRTC needs
///   loopback so a browser running on the same machine can pair
///   against it; federation doesn't (advertising loopback to remote
///   peers is useless), hence the `include_loopback` parameter.
///
/// Excludes IPv6 link-local (fe80::/10), IPv4 loopback when
/// `!include_loopback`, and unspecified addresses (0.0.0.0 / ::) which
/// aren't real bind targets.
///
/// Implementation walks `getifaddrs(3)` directly via libc — same crate
/// the codebase already depends on for other unix interop.
pub fn routable_local_addrs(include_loopback: bool) -> Vec<std::net::IpAddr> {
    use std::net::{IpAddr, Ipv4Addr};

    let mut out: Vec<IpAddr> = Vec::new();
    if include_loopback {
        out.push(IpAddr::V4(Ipv4Addr::LOCALHOST));
    }

    #[cfg(unix)]
    {
        use std::ffi::CStr;
        unsafe {
            let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
            if libc::getifaddrs(&mut ifap) == 0 && !ifap.is_null() {
                let mut cur = ifap;
                while !cur.is_null() {
                    let ifa = &*cur;
                    if !ifa.ifa_addr.is_null() {
                        let family = (*ifa.ifa_addr).sa_family as i32;
                        let _name = if ifa.ifa_name.is_null() {
                            String::new()
                        } else {
                            CStr::from_ptr(ifa.ifa_name).to_string_lossy().into_owned()
                        };
                        if family == libc::AF_INET {
                            let sin = ifa.ifa_addr as *const libc::sockaddr_in;
                            let octets = (*sin).sin_addr.s_addr.to_ne_bytes();
                            let ip = Ipv4Addr::new(octets[0], octets[1], octets[2], octets[3]);
                            if !ip.is_loopback() && !ip.is_unspecified() {
                                out.push(IpAddr::V4(ip));
                            }
                        } else if family == libc::AF_INET6 {
                            let sin6 = ifa.ifa_addr as *const libc::sockaddr_in6;
                            let segs = (*sin6).sin6_addr.s6_addr;
                            let ip = std::net::Ipv6Addr::from(segs);
                            if !ip.is_loopback() && !ip.is_unspecified() && !is_link_local_v6(&ip) {
                                out.push(IpAddr::V6(ip));
                            }
                        }
                    }
                    cur = (*cur).ifa_next;
                }
                libc::freeifaddrs(ifap);
            }
        }
    }

    // Windows has no `getifaddrs(3)`; the OS API is `GetAdaptersAddresses`.
    // Rather than hand-roll that FFI walk we use the `if-addrs` crate, which
    // wraps it and yields the same per-interface address list. The filtering
    // mirrors the unix arm: drop loopback (unless requested), link-local
    // (IPv6 fe80::/10 and IPv4 169.254/16 — neither is a useful advertised
    // endpoint), and unspecified addresses. Enumeration order is preserved so
    // the caller's later stable sort keeps a multi-NIC host's primary NIC
    // first, matching the unix behaviour.
    #[cfg(windows)]
    {
        if let Ok(ifaces) = if_addrs::get_if_addrs() {
            for iface in ifaces {
                if iface.is_link_local() {
                    continue;
                }
                let ip = iface.ip();
                if ip.is_unspecified() {
                    continue;
                }
                if ip.is_loopback() {
                    // Loopback is added once up-front (as 127.0.0.1) when
                    // requested; skip the per-interface loopback entries so
                    // we don't emit duplicates or ::1 alongside it.
                    continue;
                }
                out.push(ip);
            }
        }
    }

    out
}

/// `true` for IPv6 link-local addresses (fe80::/10). Link-local is
/// scoped to one link and isn't useful as an advertised endpoint.
#[cfg(unix)]
fn is_link_local_v6(ip: &std::net::Ipv6Addr) -> bool {
    let segs = ip.segments();
    (segs[0] & 0xffc0) == 0xfe80
}

#[cfg(target_os = "linux")]
pub mod backend_linux;

#[cfg(target_os = "macos")]
pub mod backend_macos;

/// Errors from the lan subcommand — string-based on purpose: this is a
/// user-facing setup tool and most errors are meant to be printed and
/// exited on, not matched programmatically.
#[derive(Debug)]
pub struct LanError(pub String);

impl fmt::Display for LanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for LanError {}

impl From<std::io::Error> for LanError {
    fn from(e: std::io::Error) -> Self {
        LanError(format!("io: {e}"))
    }
}

// `certs` (rcgen-based, pure-Rust) uses `?` on `rcgen::Error`; surface it as
// a LanError. Available on all platforms, like the `certs` module itself.
impl From<rcgen::Error> for LanError {
    fn from(e: rcgen::Error) -> Self {
        LanError(format!("rcgen: {e}"))
    }
}

pub type LanResult<T> = Result<T, LanError>;

/// Parsed `intendant lan <action> [flags]` invocation.
// The `intendant lan` subcommand (arg parsing + setup/recert/remove/list/
// serve-certs actions) drives the OpenSSL cert machinery, so the whole
// command surface is gated off Windows. Only the lookup helpers above
// (`resolve_host_label`, `routable_local_addrs`) remain on Windows.
#[cfg(not(target_os = "windows"))]
#[derive(Debug)]
pub struct LanArgs {
    pub action: LanAction,
    pub https_port: u16,
    pub cert_port: u16,
    pub lan_ip: Option<String>,
    pub name: Option<String>,
    pub backend_addr: String,
    pub force: bool,
    /// Skip the interactive cert distribution server at the end of setup.
    /// Used by host orchestrators (e.g. the Windows batch script) that
    /// manage the distribution flow themselves and need setup to return
    /// as soon as the certs are written and nginx is reloaded.
    pub no_serve_certs: bool,
}

#[cfg(not(target_os = "windows"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanAction {
    Setup,
    Recert,
    Remove,
    List,
    ServeCerts,
    Help,
}

#[cfg(not(target_os = "windows"))]
impl Default for LanArgs {
    fn default() -> Self {
        Self {
            action: LanAction::Help,
            https_port: 8443,
            cert_port: 9999,
            lan_ip: None,
            name: None,
            backend_addr: "127.0.0.1:8765".to_string(),
            force: false,
            no_serve_certs: false,
        }
    }
}

/// Top-level entry invoked from `main()` when argv[1] == "lan".
#[cfg(not(target_os = "windows"))]
pub async fn run(argv: Vec<String>) -> LanResult<()> {
    let args = parse_args(&argv)?;
    match args.action {
        LanAction::Help => {
            print_help();
            Ok(())
        }
        LanAction::Setup => cmd_setup(args).await,
        LanAction::Recert => cmd_recert(args).await,
        LanAction::Remove => cmd_remove(args).await,
        LanAction::List => cmd_list(args),
        LanAction::ServeCerts => cmd_serve_certs(args).await,
    }
}

#[cfg(not(target_os = "windows"))]
fn parse_args(argv: &[String]) -> LanResult<LanArgs> {
    let mut args = LanArgs::default();

    let mut iter = argv.iter();
    let Some(first) = iter.next() else {
        return Ok(args);
    };

    args.action = match first.as_str() {
        "setup" => LanAction::Setup,
        "recert" => LanAction::Recert,
        "remove" => LanAction::Remove,
        "list" => LanAction::List,
        "serve-certs" => LanAction::ServeCerts,
        "help" | "-h" | "--help" => return Ok(args),
        other => {
            return Err(LanError(format!(
                "unknown lan subcommand '{other}' (expected setup/recert/remove/list/serve-certs)"
            )));
        }
    };

    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--port" => {
                let v = iter
                    .next()
                    .ok_or_else(|| LanError("missing value for --port".into()))?;
                args.https_port = v
                    .parse()
                    .map_err(|_| LanError(format!("invalid --port value '{v}'")))?;
            }
            "--cert-port" => {
                let v = iter
                    .next()
                    .ok_or_else(|| LanError("missing value for --cert-port".into()))?;
                args.cert_port = v
                    .parse()
                    .map_err(|_| LanError(format!("invalid --cert-port value '{v}'")))?;
            }
            "--lan-ip" => {
                let v = iter
                    .next()
                    .ok_or_else(|| LanError("missing value for --lan-ip".into()))?;
                args.lan_ip = Some(v.clone());
            }
            "--name" => {
                let v = iter
                    .next()
                    .ok_or_else(|| LanError("missing value for --name".into()))?;
                args.name = Some(v.clone());
            }
            "--backend" => {
                let v = iter
                    .next()
                    .ok_or_else(|| LanError("missing value for --backend".into()))?;
                args.backend_addr = v.clone();
            }
            "--force" => {
                args.force = true;
            }
            "--no-serve-certs" => {
                args.no_serve_certs = true;
            }
            "-h" | "--help" => {
                args.action = LanAction::Help;
                return Ok(args);
            }
            other => {
                return Err(LanError(format!("unknown flag '{other}'")));
            }
        }
    }

    Ok(args)
}

#[cfg(not(target_os = "windows"))]
fn print_help() {
    println!("Intendant LAN access setup");
    println!();
    println!("USAGE:");
    println!("    intendant lan <action> [flags]");
    println!();
    println!("ACTIONS:");
    println!("    setup         Install mTLS nginx reverse proxy and generate certs");
    println!("    recert        Regenerate server cert after LAN IP change");
    println!("    remove        Tear down nginx config and remove certs");
    println!("    list          Show current setup state");
    println!("    serve-certs   Run strict HTTPS client cert enrollment");
    println!();
    println!("FLAGS:");
    println!("    --port <N>         HTTPS port exposed to clients (default 8443)");
    println!("    --cert-port <N>    Port for the HTTPS enrollment server (default 9999)");
    println!("    --lan-ip <IP>      Override detected LAN IP");
    println!("    --name <LABEL>     Host label shown in cert CN and multi-host dashboard");
    println!("    --backend <ADDR>   Upstream intendant address (default 127.0.0.1:8765)");
    println!("    --force            Skip idempotency checks (regenerate even if current)");
    println!("    --no-serve-certs   Skip the enrollment server at the end of setup");
    println!();
    println!("SECURITY TIERS:");
    println!("    Trusted LAN    — mTLS (this command)");
    println!("    Public WiFi    — WireGuard + mTLS");
    println!("    NAT traversal  — Tailscale");
}

#[cfg(not(target_os = "windows"))]
async fn cmd_setup(args: LanArgs) -> LanResult<()> {
    let be = backend::select_backend();
    be.require_privileges()?;

    let lan_ip = match args.lan_ip.clone() {
        Some(ip) => {
            println!(":: LAN IP: {ip} (override)");
            ip
        }
        None => be.detect_lan_ip()?,
    };

    let cert_dir = be.cert_dir();
    std::fs::create_dir_all(&cert_dir)?;
    be.own_cert_dir(&cert_dir)?;

    let label = args.name.clone().unwrap_or_else(|| lan_ip.clone());

    let state = certs::ensure_certs(&cert_dir, &lan_ip, &label, args.force)?;
    state::write_host_label(&cert_dir, &label)?;

    be.install_nginx()?;
    let nginx_conf = nginx_config::render(&cert_dir, &args.backend_addr, args.https_port);
    be.write_nginx_site(&nginx_conf)?;
    be.reload_nginx()?;

    println!();
    println!("============================================================");
    println!("  Setup complete!");
    println!("============================================================");
    println!();
    println!("  Phone connects to: https://{lan_ip}:{}", args.https_port);
    println!();

    if args.no_serve_certs {
        // Host orchestrators can run strict enrollment separately when
        // they have an interactive operator channel for fingerprint
        // verification.
        println!("  Skipping enrollment server (--no-serve-certs).");
        println!("  Run `intendant lan serve-certs` later to distribute the client cert.");
        println!();
        return Ok(());
    }

    // Start strict client enrollment (blocks until Ctrl+C).
    println!(
        "  Starting HTTPS enrollment server on port {}...",
        args.cert_port
    );
    println!("  Press Ctrl+C when every client has imported the cert.");
    println!();
    cert_server::serve(&state, args.cert_port, &lan_ip, args.https_port).await?;

    Ok(())
}

#[cfg(not(target_os = "windows"))]
async fn cmd_recert(args: LanArgs) -> LanResult<()> {
    let be = backend::select_backend();
    be.require_privileges()?;

    let lan_ip = match args.lan_ip.clone() {
        Some(ip) => {
            println!(":: LAN IP: {ip} (override)");
            ip
        }
        None => be.detect_lan_ip()?,
    };

    let cert_dir = be.cert_dir();
    if !cert_dir.join("ca.key").exists() {
        return Err(LanError(format!(
            "no CA found in {} — run `intendant lan setup` first",
            cert_dir.display()
        )));
    }

    certs::recert(&cert_dir, &lan_ip, args.force)?;
    be.reload_nginx()?;

    println!(":: done — nginx restarted with new cert");
    println!(":: no changes needed on your phone (same CA)");
    println!("!! if you get certificate errors, clear your browser's cache/history");

    Ok(())
}

#[cfg(not(target_os = "windows"))]
async fn cmd_remove(_args: LanArgs) -> LanResult<()> {
    let be = backend::select_backend();
    be.require_privileges()?;
    be.remove_nginx_site()?;
    let cert_dir = be.cert_dir();
    if cert_dir.exists() {
        std::fs::remove_dir_all(&cert_dir)?;
        println!(":: removed cert dir {}", cert_dir.display());
    }
    println!(":: done");
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn cmd_list(_args: LanArgs) -> LanResult<()> {
    let be = backend::select_backend();
    let cert_dir = be.cert_dir();
    if !cert_dir.join("ca.crt").exists() {
        println!(":: no setup found (cert dir: {})", cert_dir.display());
        return Ok(());
    }
    let label = state::read_host_label(&cert_dir).unwrap_or_else(|_| "<unknown>".to_string());
    println!("  Cert dir:   {}", cert_dir.display());
    println!("  Host label: {label}");
    if let Ok(ip) = certs::current_cert_ip(&cert_dir) {
        println!("  Cert IP:    {ip}");
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
async fn cmd_serve_certs(args: LanArgs) -> LanResult<()> {
    let be = backend::select_backend();
    let cert_dir = be.cert_dir();
    if !cert_dir.join("client.p12").exists() {
        return Err(LanError(format!(
            "no client.p12 found in {} — run `intendant lan setup` first",
            cert_dir.display()
        )));
    }
    let state = certs::CertState {
        cert_dir: cert_dir.clone(),
        p12_password: state::read_p12_password(&cert_dir)?,
        label: state::read_host_label(&cert_dir).unwrap_or_default(),
    };
    let lan_ip = match args.lan_ip.clone() {
        Some(ip) => ip,
        None => be.detect_lan_ip()?,
    };
    cert_server::serve(&state, args.cert_port, &lan_ip, args.https_port).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    #[test]
    fn include_loopback_prepends_localhost() {
        let addrs = routable_local_addrs(true);
        assert_eq!(
            addrs.first(),
            Some(&IpAddr::V4(Ipv4Addr::LOCALHOST)),
            "include_loopback should put 127.0.0.1 first"
        );
    }

    #[test]
    fn no_loopback_excludes_loopback_addrs() {
        // With loopback disabled, no returned address may be a loopback
        // address on any platform.
        let addrs = routable_local_addrs(false);
        assert!(
            addrs.iter().all(|ip| !ip.is_loopback()),
            "no-loopback result must not contain loopback addresses: {addrs:?}"
        );
    }

    #[test]
    fn returned_addrs_are_never_unspecified() {
        for include_loopback in [false, true] {
            let addrs = routable_local_addrs(include_loopback);
            assert!(
                addrs.iter().all(|ip| !ip.is_unspecified()),
                "0.0.0.0 / :: are not real bind targets: {addrs:?}"
            );
        }
    }

    // Windows-specific: the GetAdaptersAddresses-backed enumeration must
    // surface the machine's real routable interface(s), not just loopback.
    // Runs on the CI/build VM, which has a routable NIC.
    #[cfg(windows)]
    #[test]
    fn windows_enumerates_at_least_one_routable_addr() {
        let addrs = routable_local_addrs(false);
        assert!(
            !addrs.is_empty(),
            "expected at least one non-loopback routable interface address"
        );
        assert!(
            addrs
                .iter()
                .all(|ip| !ip.is_loopback() && !ip.is_unspecified()),
            "every address must be routable (non-loopback, non-unspecified): {addrs:?}"
        );
    }
}
