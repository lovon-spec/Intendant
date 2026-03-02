use super::error::CallerError;
use std::process::Stdio;
use tokio::process::Child;

/// Per-provider display resolution for Xvfb.
pub struct DisplayConfig {
    pub display_id: u32,
    pub width: u32,
    pub height: u32,
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
fn find_free_display() -> u32 {
    for id in 99..200 {
        let lock = format!("/tmp/.X{}-lock", id);
        if !std::path::Path::new(&lock).exists() {
            return id;
        }
    }
    199 // fallback
}

/// Guard that kills the Xvfb process when dropped.
pub struct XvfbGuard {
    child: Child,
}

impl Drop for XvfbGuard {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
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

    Ok(XvfbGuard { child })
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
        // The returned display should not have a lock file
        let lock = format!("/tmp/.X{}-lock", id);
        assert!(!std::path::Path::new(&lock).exists());
    }
}
