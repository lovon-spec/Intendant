//! Linux implementation of `LanBackend`.
//!
//! - apt for nginx install
//! - systemctl for service reload
//! - `/etc/intendant-lan` for cert dir (root-owned)
//! - `/etc/nginx/sites-available/intendant-lan` for the site config
//!
//! Requires root. Mirrors the behavior of the old `scripts/setup-lan.sh`.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::backend::LanBackend;
use super::{LanError, LanResult};

pub struct LinuxBackend;

const CERT_DIR: &str = "/etc/intendant-lan";
const NGINX_SITE_NAME: &str = "intendant-lan";

impl LanBackend for LinuxBackend {
    fn cert_dir(&self) -> PathBuf {
        PathBuf::from(CERT_DIR)
    }

    fn nginx_site_path(&self) -> PathBuf {
        PathBuf::from(format!("/etc/nginx/sites-available/{NGINX_SITE_NAME}"))
    }

    fn require_privileges(&self) -> LanResult<()> {
        if unsafe { libc::geteuid() } != 0 {
            return Err(LanError(
                "this command must be run as root (try: sudo intendant lan <action>)".into(),
            ));
        }
        Ok(())
    }

    fn detect_lan_ip(&self) -> LanResult<String> {
        // `hostname -I` prints all IPs, space-separated. Take the first.
        let output = Command::new("hostname")
            .arg("-I")
            .output()
            .map_err(|e| LanError(format!("hostname -I: {e}")))?;
        if !output.status.success() {
            return Err(LanError("hostname -I failed".into()));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let ip = stdout.split_whitespace().next().unwrap_or("").to_string();
        if ip.is_empty() {
            return Err(LanError("could not detect LAN IP".into()));
        }
        println!(":: LAN IP: {ip}");
        Ok(ip)
    }

    fn own_cert_dir(&self, _path: &Path) -> LanResult<()> {
        // cert dir lives under /etc — already root-owned. Nothing to do.
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

        println!(":: installing nginx...");
        let update = Command::new("apt-get")
            .args(["update", "-qq"])
            .status()
            .map_err(|e| LanError(format!("apt-get update: {e}")))?;
        if !update.success() {
            return Err(LanError("apt-get update failed".into()));
        }
        let install = Command::new("apt-get")
            .args(["install", "-y", "-qq", "nginx"])
            .env("DEBIAN_FRONTEND", "noninteractive")
            .status()
            .map_err(|e| LanError(format!("apt-get install nginx: {e}")))?;
        if !install.success() {
            return Err(LanError("apt-get install nginx failed".into()));
        }
        Ok(())
    }

    fn write_nginx_site(&self, contents: &str) -> LanResult<()> {
        println!(":: configuring nginx...");
        let site_path = self.nginx_site_path();
        std::fs::write(&site_path, contents)?;

        let enabled = PathBuf::from(format!("/etc/nginx/sites-enabled/{NGINX_SITE_NAME}"));
        if enabled.exists() {
            // Replace the symlink in case the site path ever moves.
            std::fs::remove_file(&enabled)?;
        }
        std::os::unix::fs::symlink(&site_path, &enabled)?;

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
        let out = Command::new("systemctl")
            .args(["restart", "nginx"])
            .status()
            .map_err(|e| LanError(format!("systemctl restart nginx: {e}")))?;
        if !out.success() {
            return Err(LanError("systemctl restart nginx failed".into()));
        }
        println!(":: nginx restarted");
        Ok(())
    }

    fn remove_nginx_site(&self) -> LanResult<()> {
        let enabled = PathBuf::from(format!("/etc/nginx/sites-enabled/{NGINX_SITE_NAME}"));
        let available = self.nginx_site_path();
        let _ = std::fs::remove_file(&enabled);
        let _ = std::fs::remove_file(&available);
        if Command::new("systemctl")
            .args(["is-active", "nginx"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            let _ = Command::new("systemctl").args(["restart", "nginx"]).status();
        }
        Ok(())
    }
}
