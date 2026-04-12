//! `intendant lan` subcommand: port of `scripts/setup-lan.sh` and
//! `scripts/setup-lan-guest-macos.sh` to Rust.
//!
//! Sets up an mTLS nginx reverse proxy in front of `intendant --web` so
//! LAN clients (phones, tablets, other boxes) can reach the dashboard
//! over HTTPS authenticated by a client certificate.
//!
//! Shared across platforms: cert generation (openssl), nginx config
//! rendering, client cert distribution, import instructions. Platform
//! differences (apt vs brew, systemd vs launchd, cert dir location) are
//! isolated behind the `LanBackend` trait.

use std::fmt;

pub mod backend;
pub mod cert_server;
pub mod certs;
pub mod instructions;
pub mod nginx_config;
pub mod state;
pub mod wizard;

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

impl From<openssl::error::ErrorStack> for LanError {
    fn from(e: openssl::error::ErrorStack) -> Self {
        LanError(format!("openssl: {e}"))
    }
}

pub type LanResult<T> = Result<T, LanError>;

/// Parsed `intendant lan <action> [flags]` invocation.
#[derive(Debug)]
pub struct LanArgs {
    pub action: LanAction,
    pub https_port: u16,
    pub cert_port: u16,
    pub lan_ip: Option<String>,
    pub name: Option<String>,
    pub backend_addr: String,
    pub force: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanAction {
    Setup,
    Recert,
    Remove,
    List,
    ServeCerts,
    Help,
}

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
        }
    }
}

/// Top-level entry invoked from `main()` when argv[1] == "lan".
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
                let v = iter.next().ok_or_else(|| LanError("missing value for --port".into()))?;
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
                let v = iter.next().ok_or_else(|| LanError("missing value for --name".into()))?;
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
    println!("    serve-certs   Run the temporary client cert distribution server");
    println!();
    println!("FLAGS:");
    println!("    --port <N>         HTTPS port exposed to clients (default 8443)");
    println!("    --cert-port <N>    Port for the client cert distribution server (default 9999)");
    println!("    --lan-ip <IP>      Override detected LAN IP");
    println!("    --name <LABEL>     Host label shown in cert CN and multi-host dashboard");
    println!("    --backend <ADDR>   Upstream intendant address (default 127.0.0.1:8765)");
    println!("    --force            Skip idempotency checks (regenerate even if current)");
    println!();
    println!("SECURITY TIERS:");
    println!("    Trusted LAN    — mTLS (this command)");
    println!("    Public WiFi    — WireGuard + mTLS");
    println!("    NAT traversal  — Tailscale");
}

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

    // Start the cert distribution server (blocks until Ctrl+C).
    println!("  Starting client cert distribution server on port {}...", args.cert_port);
    println!("  Press Ctrl+C when every client has imported the cert.");
    println!();
    cert_server::serve(
        &state,
        args.cert_port,
        &lan_ip,
        args.https_port,
    )
    .await?;

    Ok(())
}

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
