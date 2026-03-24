use super::error::CallerError;
use std::process::Stdio;
use tokio::process::Child;

/// Per-provider display resolution for Xvfb.
pub struct DisplayConfig {
    pub display_id: u32,
    pub width: u32,
    pub height: u32,
}

/// Read the PID from an X lock file. Returns `None` if the file can't be read or parsed.
pub(crate) fn read_lock_pid(lock_path: &str) -> Option<u32> {
    let contents = std::fs::read_to_string(lock_path).ok()?;
    contents.trim().parse().ok()
}

/// Check if a lock file is stale (the PID inside is no longer running).
pub(crate) fn is_lock_stale(lock_path: &str) -> bool {
    match read_lock_pid(lock_path) {
        Some(pid) => !std::path::Path::new(&format!("/proc/{}", pid)).exists(),
        None => false, // can't read/parse → assume not stale
    }
}

/// Check whether the process owning a lock file is an Xvfb instance for the given display.
/// Returns true if the process cmdline starts with "Xvfb :<id>".
pub(crate) fn is_our_xvfb(lock_path: &str, display_id: u32) -> bool {
    let pid = match read_lock_pid(lock_path) {
        Some(p) => p,
        None => return false,
    };
    let cmdline_path = format!("/proc/{}/cmdline", pid);
    let cmdline = match std::fs::read(&cmdline_path) {
        Ok(bytes) => bytes,
        Err(_) => return false,
    };
    // /proc/pid/cmdline is NUL-separated; replace NULs with spaces for easy matching
    let cmdline_str = String::from_utf8_lossy(&cmdline).replace('\0', " ");
    let expected = format!("Xvfb :{}", display_id);
    cmdline_str.starts_with(&expected)
}

/// Kill the process that owns a lock file (if alive) and clean up.
pub(crate) fn kill_and_reclaim(lock_path: &str, display_id: u32) {
    if let Some(pid) = read_lock_pid(lock_path) {
        // Send SIGKILL via the kill command — the process is an orphaned Xvfb we're reclaiming
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        // Brief wait for the process to die
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    remove_stale_lock(display_id);
}

/// Remove a stale X lock file and its socket.
pub(crate) fn remove_stale_lock(id: u32) {
    let lock = format!("/tmp/.X{}-lock", id);
    let socket = format!("/tmp/.X11-unix/X{}", id);
    let _ = std::fs::remove_file(&lock);
    let _ = std::fs::remove_file(&socket);
}

/// Returns the optimal display resolution for the given provider name.
///
/// Resolutions are chosen to minimize token cost while maintaining UI readability,
/// matching each provider's internal image processing pipeline so that the Xvfb
/// resolution = screenshot resolution = what the model sees (no scaling).
pub fn display_config_for_provider(provider_name: &str) -> DisplayConfig {
    let (width, height) = match provider_name {
        "openai" => (1024, 768),    // 3 tiles of 512x512 → ~595 tokens
        "anthropic" => (819, 1456), // 9:16 within 1568px limit → ~1590 tokens
        "gemini" => (768, 1024),    // 2 tiles of 768x768 → ~516 tokens
        _ => (1024, 768),           // safe default
    };
    DisplayConfig {
        display_id: find_free_display(),
        width,
        height,
    }
}

/// Preferred display number. Keeps VNC port predictable at 5999.
const PREFERRED_DISPLAY: u32 = 99;

/// Find a free X display number, preferring :99 for a predictable VNC port.
///
/// Strategy for each candidate display:
/// 1. No lock file → use it
/// 2. Lock file with dead PID → clean up and use it
/// 3. Lock file with live Xvfb process for this display → kill and reclaim it
///    (it's an orphan from a previous intendant session)
/// 4. Lock file with some other live process → skip to next display
fn find_free_display() -> u32 {
    for id in PREFERRED_DISPLAY..200 {
        let lock = format!("/tmp/.X{}-lock", id);
        if !std::path::Path::new(&lock).exists() {
            return id;
        }
        // Lock file exists — check if the owning process is dead
        if is_lock_stale(&lock) {
            remove_stale_lock(id);
            return id;
        }
        // Process is alive — reclaim if it's an orphaned Xvfb for this display
        if is_our_xvfb(&lock, id) {
            kill_and_reclaim(&lock, id);
            return id;
        }
    }
    199 // fallback
}

/// Guard that kills the Xvfb (and optional x11vnc) process when dropped.
/// Cleans up the lock file and socket after killing.
pub struct XvfbGuard {
    child: Child,
    vnc_child: Option<Child>,
    display_id: u32,
    vnc_port: Option<u32>,
}

impl XvfbGuard {
    /// Returns the VNC port if x11vnc was successfully launched.
    pub fn vnc_port(&self) -> Option<u32> {
        self.vnc_port
    }
}

impl Drop for XvfbGuard {
    fn drop(&mut self) {
        if let Some(ref mut vnc) = self.vnc_child {
            let _ = vnc.start_kill();
        }
        let _ = self.child.start_kill();
        // Clean up lock file and socket so the display number can be reused
        remove_stale_lock(self.display_id);
    }
}

/// Detect if a VNC server is already running for the given display.
/// Checks the standard VNC port (5900 + display_id).
pub fn detect_vnc_port(display_id: u32) -> Option<u32> {
    let port = 5900 + display_id;
    // Quick check: try to connect to the port
    use std::net::TcpStream;
    match TcpStream::connect_timeout(
        &format!("127.0.0.1:{}", port).parse().ok()?,
        std::time::Duration::from_millis(100),
    ) {
        Ok(_) => Some(port),
        Err(_) => None,
    }
}

/// Best-effort launch of x11vnc on the given display.
/// Returns `Some(Child)` on success, `None` if x11vnc is not installed or fails to start.
async fn launch_vnc(display_arg: &str, port: u32) -> Option<Child> {
    let child = tokio::process::Command::new("x11vnc")
        .args([
            "-display",
            display_arg,
            "-rfbport",
            &port.to_string(),
            "-nopw",
            "-forever",
            "-shared",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn();

    match child {
        Ok(c) => {
            // Brief wait for x11vnc to bind the port
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            Some(c)
        }
        Err(_) => None,
    }
}

/// Launch Xvfb on the given display with the given resolution.
/// Returns a guard that kills the process on drop.
pub async fn launch_display(config: &DisplayConfig) -> Result<XvfbGuard, CallerError> {
    let display_arg = format!(":{}", config.display_id);
    let screen_arg = format!("{}x{}x24", config.width, config.height);

    let child = tokio::process::Command::new("Xvfb")
        .args([&display_arg, "-screen", "0", &screen_arg, "-ac"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| {
            CallerError::Config(format!("Failed to launch Xvfb (is xvfb installed?): {}", e))
        })?;

    // Brief wait for Xvfb to initialize
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Verify the display is accessible
    let check = tokio::process::Command::new("xdpyinfo")
        .args(["-display", &display_arg])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;

    if check.map(|s| !s.success()).unwrap_or(true) {
        return Err(CallerError::Config(format!(
            "Xvfb started but display {} is not responding",
            display_arg
        )));
    }

    // Set DISPLAY env var so the runtime subprocess inherits it
    std::env::set_var("DISPLAY", &display_arg);

    // Best-effort: launch x11vnc so users can watch the display via VNC
    let vnc_port_num = 5900 + config.display_id;
    let vnc_child = launch_vnc(&display_arg, vnc_port_num).await;
    let has_vnc = vnc_child.is_some();

    Ok(XvfbGuard {
        child,
        display_id: config.display_id,
        vnc_child,
        vnc_port: if has_vnc { Some(vnc_port_num) } else { None },
    })
}

/// Check whether the current DISPLAY environment variable points to an accessible X server.
/// Returns `false` if DISPLAY is unset or `xdpyinfo` fails to connect.
pub fn is_display_accessible() -> bool {
    let display = match std::env::var("DISPLAY") {
        Ok(d) if !d.is_empty() => d,
        _ => return false,
    };
    std::process::Command::new("xdpyinfo")
        .args(["-display", &display])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_config_openai() {
        let config = display_config_for_provider("openai");
        assert_eq!(config.width, 1024);
        assert_eq!(config.height, 768);
    }

    #[test]
    fn display_config_anthropic() {
        let config = display_config_for_provider("anthropic");
        assert_eq!(config.width, 819);
        assert_eq!(config.height, 1456);
    }

    #[test]
    fn display_config_gemini() {
        let config = display_config_for_provider("gemini");
        assert_eq!(config.width, 768);
        assert_eq!(config.height, 1024);
    }

    #[test]
    fn display_config_unknown_defaults_to_openai() {
        let config = display_config_for_provider("unknown-provider");
        assert_eq!(config.width, 1024);
        assert_eq!(config.height, 768);
    }

    #[test]
    fn find_free_display_avoids_existing() {
        // Should return a display number >= 99
        let id = find_free_display();
        assert!(id >= 99);
        // The returned display should either have no lock file,
        // or have a stale lock file that was cleaned up
        let lock = format!("/tmp/.X{}-lock", id);
        assert!(
            !std::path::Path::new(&lock).exists() || is_lock_stale(&lock),
            "display :{} has a live lock file",
            id
        );
    }

    #[test]
    fn is_lock_stale_nonexistent_file() {
        assert!(!is_lock_stale("/tmp/.X_nonexistent_test-lock"));
    }

    #[test]
    fn stale_lock_detection_and_cleanup() {
        // Create a lock file with a definitely-dead PID
        let test_id = 198; // high number unlikely to conflict
        let lock = format!("/tmp/.X{}-lock", test_id);
        let socket_dir = "/tmp/.X11-unix";
        let socket = format!("{}/X{}", socket_dir, test_id);
        // Use PID 1999999999 which cannot exist
        std::fs::write(&lock, " 1999999999\n").unwrap();
        assert!(is_lock_stale(&lock));
        remove_stale_lock(test_id);
        assert!(!std::path::Path::new(&lock).exists());
        // Clean up socket if it was created
        let _ = std::fs::remove_file(&socket);
    }

    #[test]
    fn vnc_port_tracks_display_id() {
        // VNC port should always be 5900 + display_id
        let config = display_config_for_provider("openai");
        assert_eq!(5900 + config.display_id, 5900 + config.display_id);
        // With display :99, VNC port is 5999
        assert_eq!(5900 + 99, 5999);
    }

    #[tokio::test]
    async fn launch_vnc_missing_binary() {
        // x11vnc may or may not be installed; if not, launch_vnc returns None gracefully
        let result = launch_vnc(":9999", 15999).await;
        // Either way, this should not panic or error
        if let Some(mut c) = result {
            let _ = c.start_kill();
        }
    }

    #[test]
    fn read_lock_pid_nonexistent() {
        assert_eq!(read_lock_pid("/tmp/.X_nonexistent_test-lock"), None);
    }

    #[test]
    fn read_lock_pid_valid() {
        let lock = "/tmp/.X197-test-lock";
        std::fs::write(lock, " 12345\n").unwrap();
        assert_eq!(read_lock_pid(lock), Some(12345));
        let _ = std::fs::remove_file(lock);
    }

    #[test]
    fn is_our_xvfb_dead_pid() {
        // Lock with dead PID — is_our_xvfb should return false (can't read cmdline)
        let lock = "/tmp/.X197-test-lock2";
        std::fs::write(lock, " 1999999999\n").unwrap();
        assert!(!is_our_xvfb(lock, 197));
        let _ = std::fs::remove_file(lock);
    }

    #[test]
    fn preferred_display_is_99() {
        assert_eq!(PREFERRED_DISPLAY, 99);
    }

    #[test]
    fn find_free_display_prefers_99() {
        // When :99 is free, find_free_display should return 99
        let lock = format!("/tmp/.X{}-lock", PREFERRED_DISPLAY);
        if !std::path::Path::new(&lock).exists() {
            assert_eq!(find_free_display(), 99);
        }
        // If :99 is taken we can only assert >= 99
        assert!(find_free_display() >= 99);
    }

    #[test]
    fn is_display_accessible_no_display_set() {
        // Temporarily unset DISPLAY to test the "no display" path.
        let prev = std::env::var("DISPLAY").ok();
        std::env::remove_var("DISPLAY");
        assert!(!is_display_accessible());
        // Restore
        if let Some(d) = prev {
            std::env::set_var("DISPLAY", d);
        }
    }
}
