//! Periodic frame capture for the user's session display.
//!
//! When the user grants display access, a background task captures screenshots
//! at a fixed interval and registers them in the frame registry as the
//! `display_user_session` stream. This feeds the CU-first pipeline and live
//! model with up-to-date views of the user's actual screen.
//!
//! Cross-platform: `import` (X11), `screencapture` (macOS).

use crate::computer_use::DisplayTarget;
use crate::event::{AppEvent, EventBus};
use crate::frames::FrameRegistry;
use presence_core::FrameMeta;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Default capture interval in milliseconds.
const CAPTURE_INTERVAL_MS: u64 = 2000;

/// Stream name used in the frame registry for user session frames.
const STREAM_NAME: &str = "display_user_session";

/// Spawn a background task that captures the user's display at regular intervals.
///
/// The task listens for `UserDisplayGranted` / `UserDisplayRevoked` events on
/// the bus and starts/stops capture accordingly. It runs for the lifetime of
/// the session — cancel via the returned `JoinHandle`.
pub fn spawn_user_display_capture(
    bus: EventBus,
    frame_registry: Arc<RwLock<FrameRegistry>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut event_rx = bus.subscribe();
        let mut capturing = false;
        let mut capture_handle: Option<tokio::task::JoinHandle<()>> = None;
        let (stop_tx, _) = tokio::sync::watch::channel(false);

        loop {
            match event_rx.recv().await {
                Ok(AppEvent::UserDisplayGranted) => {
                    if capturing {
                        continue;
                    }
                    capturing = true;

                    // Detect resolution and emit DisplayReady
                    let (width, height) = query_user_display_resolution();
                    bus.send(AppEvent::DisplayReady {
                        display_id: 0,
                        vnc_port: None,
                        width,
                        height,
                    });

                    // Spawn capture loop
                    let registry = frame_registry.clone();
                    let mut stop_rx = stop_tx.subscribe();
                    capture_handle = Some(tokio::spawn(async move {
                        let mut counter = 0u64;
                        loop {
                            tokio::select! {
                                _ = tokio::time::sleep(
                                    std::time::Duration::from_millis(CAPTURE_INTERVAL_MS)
                                ) => {}
                                _ = stop_rx.changed() => break,
                            }

                            match capture_screenshot().await {
                                Ok(jpeg_data) => {
                                    counter += 1;
                                    let frame_id =
                                        format!("{}-f{:05}", STREAM_NAME, counter);
                                    // Extract dimensions from JPEG header if possible
                                    let (w, h) = jpeg_dimensions(&jpeg_data)
                                        .unwrap_or((0, 0));
                                    let meta = FrameMeta {
                                        frame_id,
                                        stream: STREAM_NAME.to_string(),
                                        timestamp: chrono::Utc::now().to_rfc3339(),
                                        sent_to_live: false,
                                        live_resolution: None,
                                        hq_resolution: if w > 0 {
                                            Some(format!("{}x{}", w, h))
                                        } else {
                                            None
                                        },
                                    };
                                    let mut reg = registry.write().await;
                                    let _ = reg.register(meta, &jpeg_data);
                                }
                                Err(e) => {
                                    eprintln!(
                                        "[display_capture] screenshot failed: {}",
                                        e
                                    );
                                }
                            }
                        }
                    }));
                }
                Ok(AppEvent::UserDisplayRevoked { .. }) => {
                    if !capturing {
                        continue;
                    }
                    capturing = false;
                    // Signal stop and wait for capture loop to exit
                    let _ = stop_tx.send(true);
                    if let Some(handle) = capture_handle.take() {
                        let _ = handle.await;
                    }
                    // Reset stop channel for next grant cycle
                    let _ = stop_tx.send(false);
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                _ => {}
            }
        }
    })
}

/// Capture a screenshot of the user's session display as JPEG bytes.
///
/// macOS: `screencapture -x -t jpg` outputs JPEG directly.
/// Linux: `import -window root -display :0` with `.jpg` extension outputs JPEG.
async fn capture_screenshot() -> Result<Vec<u8>, String> {
    let tmp = std::env::temp_dir().join("intendant_user_display.jpg");

    #[cfg(target_os = "macos")]
    {
        let output = tokio::process::Command::new("screencapture")
            .args(["-x", "-t", "jpg", &tmp.to_string_lossy()])
            .output()
            .await
            .map_err(|e| format!("screencapture: {}", e))?;
        if !output.status.success() {
            return Err(format!(
                "screencapture failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        let display = DisplayTarget::UserSession.display_env_string();
        let mut cmd = tokio::process::Command::new("import");
        cmd.args([
            "-window",
            "root",
            "-display",
            &display,
            &tmp.to_string_lossy(),
        ]);
        if let Ok(xauth) = std::env::var("XAUTHORITY") {
            cmd.env("XAUTHORITY", xauth);
        }
        let output = cmd
            .output()
            .await
            .map_err(|e| format!("import: {}", e))?;
        if !output.status.success() {
            return Err(format!(
                "import failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
    }

    let bytes = tokio::fs::read(&tmp)
        .await
        .map_err(|e| format!("read screenshot: {}", e))?;
    let _ = tokio::fs::remove_file(&tmp).await;
    Ok(bytes)
}

/// Extract width and height from a JPEG file's SOF0 marker.
/// Returns None if the header can't be parsed.
fn jpeg_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    // Scan for SOF0 marker (0xFF 0xC0) or SOF2 (0xFF 0xC2)
    let mut i = 0;
    while i + 1 < data.len() {
        if data[i] == 0xFF && (data[i + 1] == 0xC0 || data[i + 1] == 0xC2) {
            // SOF marker found: skip marker (2) + length (2) + precision (1)
            // Then height (2 bytes BE) and width (2 bytes BE)
            if i + 9 <= data.len() {
                let height = u16::from_be_bytes([data[i + 5], data[i + 6]]) as u32;
                let width = u16::from_be_bytes([data[i + 7], data[i + 8]]) as u32;
                return Some((width, height));
            }
        }
        i += 1;
    }
    None
}

/// Query user display resolution. Platform-specific.
fn query_user_display_resolution() -> (u32, u32) {
    #[cfg(target_os = "macos")]
    {
        if let Ok(out) = std::process::Command::new("system_profiler")
            .arg("SPDisplaysDataType")
            .output()
        {
            let text = String::from_utf8_lossy(&out.stdout);
            for line in text.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("Resolution:") {
                    let parts: Vec<&str> = trimmed.split_whitespace().collect();
                    if parts.len() >= 4 {
                        if let (Ok(w), Ok(h)) = (parts[1].parse(), parts[3].parse()) {
                            return (w, h);
                        }
                    }
                }
            }
        }
        (1920, 1080)
    }

    #[cfg(not(target_os = "macos"))]
    {
        let display = DisplayTarget::UserSession.display_env_string();
        let output = std::process::Command::new("xdpyinfo")
            .arg("-display")
            .arg(&display)
            .output();
        if let Ok(out) = output {
            let text = String::from_utf8_lossy(&out.stdout);
            for line in text.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("dimensions:") {
                    if let Some(dims) = trimmed.split_whitespace().nth(1) {
                        let parts: Vec<&str> = dims.split('x').collect();
                        if parts.len() == 2 {
                            if let (Ok(w), Ok(h)) = (parts[0].parse(), parts[1].parse()) {
                                return (w, h);
                            }
                        }
                    }
                }
            }
        }
        (1920, 1080)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jpeg_dimensions_valid() {
        // Minimal JPEG with SOF0 marker
        let mut data = vec![
            0xFF, 0xD8, // SOI
            0xFF, 0xC0, // SOF0
            0x00, 0x11, // length
            0x08, // precision
            0x03, 0x00, // height = 768
            0x04, 0x00, // width = 1024
        ];
        data.extend_from_slice(&[0u8; 20]); // padding
        assert_eq!(jpeg_dimensions(&data), Some((1024, 768)));
    }

    #[test]
    fn jpeg_dimensions_too_short() {
        assert_eq!(jpeg_dimensions(&[0xFF, 0xC0]), None);
    }

    #[test]
    fn stream_name_matches_display_prefix() {
        // auto_attach_display_frames looks for streams starting with "display_"
        assert!(STREAM_NAME.starts_with("display_"));
    }
}
