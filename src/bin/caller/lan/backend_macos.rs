//! macOS implementation of `LanBackend`.
//!
//! - Homebrew for nginx install
//! - `brew services` for service reload
//! - `$HOME/.intendant/lan-certs` for cert dir (user-writeable — no
//!   sudo required for this backend at all, unlike Linux)
//! - `$(brew --prefix)/etc/nginx/servers/intendant-lan.conf` for the
//!   site config
//!
//! Mirrors `scripts/setup-lan-guest-macos.sh`.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::backend::LanBackend;
use super::{LanError, LanResult};

pub struct MacOsBackend;

const NGINX_SITE_NAME: &str = "intendant-lan";

impl MacOsBackend {
    fn brew_prefix(&self) -> LanResult<PathBuf> {
        let out = Command::new("brew")
            .arg("--prefix")
            .output()
            .map_err(|e| LanError(format!(
                "brew --prefix: {e} (install Homebrew from https://brew.sh)"
            )))?;
        if !out.status.success() {
            return Err(LanError("brew --prefix failed".into()));
        }
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if s.is_empty() {
            // Fall back to the default prefix for Apple Silicon.
            return Ok(PathBuf::from("/opt/homebrew"));
        }
        Ok(PathBuf::from(s))
    }

    fn home_dir(&self) -> LanResult<PathBuf> {
        dirs::home_dir().ok_or_else(|| LanError("could not resolve HOME".into()))
    }
}

impl LanBackend for MacOsBackend {
    fn cert_dir(&self) -> PathBuf {
        self.home_dir()
            .map(|h| h.join(".intendant").join("lan-certs"))
            .unwrap_or_else(|_| PathBuf::from("/tmp/intendant-lan-certs"))
    }

    fn nginx_site_path(&self) -> PathBuf {
        self.brew_prefix()
            .map(|p| p.join("etc").join("nginx").join("servers").join(format!("{NGINX_SITE_NAME}.conf")))
            .unwrap_or_else(|_| {
                PathBuf::from(format!("/opt/homebrew/etc/nginx/servers/{NGINX_SITE_NAME}.conf"))
            })
    }

    fn require_privileges(&self) -> LanResult<()> {
        // macOS backend runs as the user — Homebrew is user-level and
        // the cert dir lives in $HOME. Refuse to run as root so we
        // don't leave root-owned files in the user's home.
        if unsafe { libc::geteuid() } == 0 {
            return Err(LanError(
                "do not run as root on macOS — this backend uses Homebrew and \
                 writes to $HOME/.intendant/lan-certs. Run as your normal user."
                    .into(),
            ));
        }
        // Also verify brew is actually available up front.
        Command::new("brew")
            .arg("--version")
            .output()
            .map_err(|_| {
                LanError(
                    "Homebrew is required — install from https://brew.sh and re-run"
                        .into(),
                )
            })?;
        Ok(())
    }

    fn detect_lan_ip(&self) -> LanResult<String> {
        // Get the default interface from `route -n get default`, then
        // its IP via `ipconfig getifaddr <iface>`.
        let route = Command::new("route")
            .args(["-n", "get", "default"])
            .output()
            .map_err(|e| LanError(format!("route -n get default: {e}")))?;
        if !route.status.success() {
            return Err(LanError("route -n get default failed".into()));
        }
        let stdout = String::from_utf8_lossy(&route.stdout);
        let iface = stdout
            .lines()
            .find_map(|l| l.trim().strip_prefix("interface:"))
            .map(|s| s.trim().to_string())
            .ok_or_else(|| LanError("could not parse default interface".into()))?;

        let ip_out = Command::new("ipconfig")
            .args(["getifaddr", &iface])
            .output()
            .map_err(|e| LanError(format!("ipconfig getifaddr {iface}: {e}")))?;
        let ip = String::from_utf8_lossy(&ip_out.stdout).trim().to_string();
        if ip.is_empty() {
            return Err(LanError(format!("no IP detected for interface {iface}")));
        }
        println!(":: LAN IP: {ip} (iface {iface})");
        Ok(ip)
    }

    fn own_cert_dir(&self, _path: &Path) -> LanResult<()> {
        // Lives in $HOME — already owned by the user.
        Ok(())
    }

    fn install_nginx(&self) -> LanResult<()> {
        if Command::new("which")
            .arg("nginx")
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            println!(":: nginx already installed");
            return Ok(());
        }
        println!(":: installing nginx via Homebrew...");
        let out = Command::new("brew")
            .args(["install", "nginx"])
            .status()
            .map_err(|e| LanError(format!("brew install nginx: {e}")))?;
        if !out.success() {
            return Err(LanError("brew install nginx failed".into()));
        }
        Ok(())
    }

    fn write_nginx_site(&self, contents: &str) -> LanResult<()> {
        println!(":: configuring nginx...");
        let site_path = self.nginx_site_path();
        if let Some(parent) = site_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&site_path, contents)?;

        let check = Command::new("nginx")
            .arg("-t")
            .status()
            .map_err(|e| LanError(format!("nginx -t: {e}")))?;
        if !check.success() {
            return Err(LanError("nginx config test failed".into()));
        }
        Ok(())
    }

    fn reload_nginx(&self) -> LanResult<()> {
        let out = Command::new("brew")
            .args(["services", "restart", "nginx"])
            .status()
            .map_err(|e| LanError(format!("brew services restart nginx: {e}")))?;
        if !out.success() {
            return Err(LanError("brew services restart nginx failed".into()));
        }
        println!(":: nginx restarted (brew services)");
        Ok(())
    }

    fn remove_nginx_site(&self) -> LanResult<()> {
        let _ = std::fs::remove_file(self.nginx_site_path());
        let _ = Command::new("brew")
            .args(["services", "restart", "nginx"])
            .status();
        Ok(())
    }
}
