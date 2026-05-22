//! Debug screen management for e2e testing.
//!
//! On Linux: Xvfb + passive Firefox.
//! On macOS: opens a native browser window (no virtual display needed).
//!
//! Provides a one-click observer display that records what happens in the
//! web dashboard. Daemon-scoped recordings persist at `~/.intendant/recordings/`.

use crate::event::{AppEvent, ControlMsg, EventBus};
use crate::project::RecordingConfig;
use crate::recording;
use crate::vision;
use std::path::PathBuf;
use tokio::process::Child;

/// Preferred debug display range (50-59). Intendant reserves 99+ for agent displays.
const DEBUG_DISPLAY_MIN: u32 = 50;
const DEBUG_DISPLAY_MAX: u32 = 59;

/// RAII guard for the debug screen.
/// On Linux: Xvfb + Firefox. On macOS: just a browser window.
/// Kills the browser on drop; XvfbGuard (if present) handles Xvfb.
pub struct DebugScreen {
    pub xvfb_guard: Option<vision::XvfbGuard>,
    pub firefox: Child,
    pub display_id: u32,
}

impl Drop for DebugScreen {
    fn drop(&mut self) {
        let _ = self.firefox.start_kill();
    }
}

/// Find a free display in the 50-59 range for debug use.
/// On non-Linux platforms the X lock file helpers are stubs, so this
/// returns the first display in range immediately.
pub fn find_free_debug_display() -> u32 {
    for id in DEBUG_DISPLAY_MIN..=DEBUG_DISPLAY_MAX {
        let lock = format!("/tmp/.X{}-lock", id);
        if !std::path::Path::new(&lock).exists() {
            return id;
        }
        if vision::is_lock_stale(&lock) {
            vision::remove_stale_lock(id);
            return id;
        }
        if vision::is_our_xvfb(&lock, id) {
            vision::kill_and_reclaim(&lock, id);
            return id;
        }
    }
    DEBUG_DISPLAY_MAX // fallback
}

/// Returns `~/.intendant/recordings/` for daemon-scoped recordings.
pub fn daemon_recordings_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".intendant").join("recordings")
}

/// Set up a debug screen.
/// Linux: Xvfb + passive Firefox.
/// macOS: opens a native browser window via `open`.
#[cfg(target_os = "macos")]
pub async fn setup_debug_screen(web_port: u16) -> Result<DebugScreen, String> {
    let url = format!("http://localhost:{}/app?passive=1", web_port);

    // On macOS, `open` launches the default browser — no virtual display needed.
    let browser = tokio::process::Command::new("open")
        .args(["-na", "Safari", "--args", &url])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("Failed to open browser: {}", e))?;

    // display_id 0 = main screen (used by avfoundation for recording)
    Ok(DebugScreen {
        xvfb_guard: None,
        firefox: browser,
        display_id: 0,
    })
}

/// Set up a debug screen: Xvfb + passive Firefox.
#[cfg(target_os = "linux")]
pub async fn setup_debug_screen(web_port: u16) -> Result<DebugScreen, String> {
    let display_id = find_free_debug_display();
    let config = vision::DisplayConfig {
        target: super::computer_use::DisplayTarget::Virtual { id: display_id },
        width: 1280,
        height: 720,
    };

    let xvfb_guard = vision::launch_display(&config)
        .await
        .map_err(|e| format!("Failed to launch debug Xvfb: {}", e))?;

    // Create/reuse debug Firefox profile
    let profile_dir = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".intendant")
        .join("debug-profile");
    if !profile_dir.exists() {
        let _ = std::fs::create_dir_all(&profile_dir);
        // Write user.js with debugger prefs and passive defaults
        let user_js = profile_dir.join("user.js");
        let prefs = r#"user_pref("devtools.debugger.remote-enabled", true);
user_pref("devtools.chrome.enabled", true);
user_pref("devtools.debugger.prompt-connection", false);
user_pref("devtools.debugger.force-local", false);
user_pref("browser.shell.checkDefaultBrowser", false);
user_pref("browser.startup.homepage_override.mstone", "ignore");
user_pref("datareporting.policy.dataSubmissionEnabled", false);
"#;
        let _ = std::fs::write(&user_js, prefs);
    }

    // Launch Firefox in passive mode on the debug display
    let display_arg = format!(":{}", display_id);
    let url = format!("http://localhost:{}/app?passive=1", web_port);
    let firefox = tokio::process::Command::new("firefox")
        .args([
            "-profile",
            profile_dir.to_str().unwrap_or("/tmp"),
            "--no-remote",
            "--new-window",
            &url,
        ])
        .env("DISPLAY", &display_arg)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("Failed to launch debug Firefox: {}", e))?;

    Ok(DebugScreen {
        xvfb_guard: Some(xvfb_guard),
        firefox,
        display_id,
    })
}

/// Set up a debug screen — not available on non-Linux/macOS platforms.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub async fn setup_debug_screen(_web_port: u16) -> Result<DebugScreen, String> {
    Err("Debug screen is not supported on this platform".into())
}

/// Start a daemon-scoped recording of a debug display.
pub async fn start_debug_recording(
    display_id: u32,
    config: &RecordingConfig,
) -> Result<recording::RecordingGuard, String> {
    let dir = daemon_recordings_dir();
    let _ = std::fs::create_dir_all(&dir);
    recording::start_display_recording(display_id, 1280, 720, config, &dir).await
}

/// Spawn a background task that handles debug screen control messages.
pub fn spawn_debug_screen_handler(
    mut event_rx: tokio::sync::broadcast::Receiver<AppEvent>,
    recording_config: RecordingConfig,
    web_port: u16,
    bus: EventBus,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut screen: Option<DebugScreen> = None;
        let mut rec_guard: Option<recording::RecordingGuard> = None;

        loop {
            match event_rx.recv().await {
                Ok(AppEvent::ControlCommand(ControlMsg::SetupDebugScreen)) => {
                    if screen.is_some() {
                        eprintln!("[debug] Screen already active");
                        continue;
                    }
                    match setup_debug_screen(web_port).await {
                        Ok(s) => {
                            let display_id = s.display_id;
                            eprintln!("[debug] Screen ready on :{}", display_id);
                            bus.send(AppEvent::DebugScreenReady { display_id });
                            screen = Some(s);
                        }
                        Err(e) => {
                            eprintln!("[debug] Setup failed: {}", e);
                        }
                    }
                }
                Ok(AppEvent::ControlCommand(ControlMsg::TeardownDebugScreen)) => {
                    if let Some(s) = screen.take() {
                        let display_id = s.display_id;
                        rec_guard.take(); // stop recording first
                        drop(s); // kills Firefox + Xvfb
                        bus.send(AppEvent::DebugScreenTornDown { display_id });
                        eprintln!("[debug] Screen torn down");
                    }
                }
                Ok(AppEvent::ControlCommand(ControlMsg::StartDebugRecording)) => {
                    if let Some(ref s) = screen {
                        if rec_guard.is_some() {
                            eprintln!("[debug] Already recording");
                            continue;
                        }
                        match start_debug_recording(s.display_id, &recording_config).await {
                            Ok(guard) => {
                                let stream = guard.stream_name().to_string();
                                rec_guard = Some(guard);
                                bus.send(AppEvent::RecordingStarted {
                                    stream_name: stream,
                                });
                                eprintln!("[debug] Recording started");
                            }
                            Err(e) => {
                                eprintln!("[debug] Recording failed: {}", e);
                            }
                        }
                    } else {
                        eprintln!("[debug] No screen active — set up first");
                    }
                }
                Ok(AppEvent::ControlCommand(ControlMsg::StopDebugRecording)) => {
                    if let Some(guard) = rec_guard.take() {
                        let stream = guard.stream_name().to_string();
                        drop(guard);
                        bus.send(AppEvent::RecordingStopped {
                            stream_name: stream,
                        });
                        eprintln!("[debug] Recording stopped");
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    // Daemon shutting down — clean up
                    rec_guard.take();
                    screen.take();
                    break;
                }
                _ => continue,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_free_debug_display_in_range() {
        let id = find_free_debug_display();
        assert!(id >= DEBUG_DISPLAY_MIN && id <= DEBUG_DISPLAY_MAX);
    }

    #[test]
    fn daemon_recordings_dir_exists() {
        let dir = daemon_recordings_dir();
        // Build the expected tail with the platform separator so the
        // assertion holds on Windows (where `join` yields '\\') as well as
        // POSIX. `daemon_recordings_dir` itself uses `PathBuf::join`, so it
        // is already platform-correct; only the literal here was POSIX-only.
        let tail: PathBuf = [".intendant", "recordings"].iter().collect();
        assert!(dir.ends_with(&tail), "unexpected recordings dir: {dir:?}");
    }
}
