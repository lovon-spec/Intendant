use super::error::CallerError;
use std::process::Stdio;
use tokio::process::Child;

/// Per-provider display resolution for Xvfb.
pub struct DisplayConfig {
    pub display_id: u32,
    pub width: u32,
    pub height: u32,
}

/// Check if a lock file is stale (the PID inside is no longer running).
fn is_lock_stale(lock_path: &str) -> bool {
    let contents = match std::fs::read_to_string(lock_path) {
        Ok(c) => c,
        Err(_) => return false, // can't read → assume not stale
    };
    let pid_str = contents.trim();
    let pid: u32 = match pid_str.parse() {
        Ok(p) => p,
        Err(_) => return false,
    };
    // Check if the process is alive via /proc (Linux-only)
    !std::path::Path::new(&format!("/proc/{}", pid)).exists()
}

/// Remove a stale X lock file and its socket.
fn remove_stale_lock(id: u32) {
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

/// Find a free X display number by checking for lock files.
/// Cleans up stale lock files (dead PIDs) encountered along the way.
fn find_free_display() -> u32 {
    for id in 99..200 {
        let lock = format!("/tmp/.X{}-lock", id);
        if !std::path::Path::new(&lock).exists() {
            return id;
        }
        // Lock file exists — check if the owning process is dead
        if is_lock_stale(&lock) {
            remove_stale_lock(id);
            return id;
        }
    }
    199 // fallback
}

/// Guard that kills the Xvfb process when dropped.
/// Cleans up the lock file and socket after killing.
pub struct XvfbGuard {
    child: Child,
    display_id: u32,
}

impl Drop for XvfbGuard {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        // Clean up lock file and socket so the display number can be reused
        remove_stale_lock(self.display_id);
    }
}

/// Launch Xvfb on the given display with the given resolution.
/// Returns a guard that kills the process on drop.
pub async fn launch_display(config: &DisplayConfig) -> Result<XvfbGuard, CallerError> {
    let display_arg = format!(":{}", config.display_id);
    let screen_arg = format!("{}x{}x24", config.width, config.height);

    let child = tokio::process::Command::new("Xvfb")
        .args([&display_arg, "-screen", "0", &screen_arg])
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

    Ok(XvfbGuard {
        child,
        display_id: config.display_id,
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
