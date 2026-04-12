//! Small persistent bits of state the `lan` subcommand reads and writes:
//! the host label (used as the future multi-host `HostId`) and the .p12
//! password (so `serve-certs` can print it back after the initial setup
//! session has ended).

use std::path::Path;

use super::{LanError, LanResult};

const HOST_LABEL_FILE: &str = "host_label";
const P12_PASSWORD_FILE: &str = "p12_password";

pub fn write_host_label(cert_dir: &Path, label: &str) -> LanResult<()> {
    let path = cert_dir.join(HOST_LABEL_FILE);
    std::fs::write(&path, label.as_bytes())?;
    set_readable_perms(&path)?;
    Ok(())
}

pub fn read_host_label(cert_dir: &Path) -> LanResult<String> {
    let path = cert_dir.join(HOST_LABEL_FILE);
    let contents = std::fs::read_to_string(&path)
        .map_err(|e| LanError(format!("read {}: {e}", path.display())))?;
    Ok(contents.trim().to_string())
}

pub fn write_p12_password(cert_dir: &Path, password: &str) -> LanResult<()> {
    let path = cert_dir.join(P12_PASSWORD_FILE);
    std::fs::write(&path, password.as_bytes())?;
    // .p12 password is mildly sensitive — 0600.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&path, perms)?;
    }
    Ok(())
}

pub fn read_p12_password(cert_dir: &Path) -> LanResult<String> {
    let path = cert_dir.join(P12_PASSWORD_FILE);
    let contents = std::fs::read_to_string(&path)
        .map_err(|e| LanError(format!("read {}: {e}", path.display())))?;
    Ok(contents.trim().to_string())
}

fn set_readable_perms(path: &Path) -> LanResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(path, perms)?;
    }
    let _ = path;
    Ok(())
}
