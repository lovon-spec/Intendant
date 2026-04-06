//! Clipboard monitor for bidirectional clipboard sync between remote display
//! and browser.
//!
//! Polls the system clipboard every 500ms and emits changes.  Text-only.
//!
//! Platform support:
//! - **macOS**: `pbpaste` / `pbcopy`
//! - **Linux**: `xclip -o -selection clipboard` / `xclip -i -selection clipboard`

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

/// Monitors the system clipboard for changes and provides methods to read/write
/// clipboard text.
pub struct ClipboardMonitor {
    last_content: Arc<Mutex<String>>,
    shutdown: Arc<AtomicBool>,
}

impl ClipboardMonitor {
    pub fn new() -> Self {
        Self {
            last_content: Arc::new(Mutex::new(String::new())),
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Start watching the clipboard for changes.
    ///
    /// Returns a receiver that emits the new clipboard text whenever it changes.
    /// The polling loop runs every 500ms until `stop()` is called.
    pub fn start_watching(&self) -> mpsc::Receiver<String> {
        let (tx, rx) = mpsc::channel::<String>(4);
        let last_content = Arc::clone(&self.last_content);
        let shutdown = Arc::clone(&self.shutdown);

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
            loop {
                interval.tick().await;
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }
                let current = match read_clipboard().await {
                    Some(text) => text,
                    None => continue,
                };
                let mut last = last_content.lock().await;
                if !current.is_empty() && current != *last {
                    *last = current.clone();
                    if tx.send(current).await.is_err() {
                        break; // receiver dropped
                    }
                }
            }
        });

        rx
    }

    /// Inject text into the system clipboard.
    ///
    /// Also updates the internal `last_content` so the next poll does not
    /// re-emit this text as a "change".
    pub async fn set_text(&self, text: &str) -> Result<(), String> {
        write_clipboard(text).await?;
        *self.last_content.lock().await = text.to_string();
        Ok(())
    }

    /// Stop the polling loop.
    pub fn stop(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

impl Drop for ClipboardMonitor {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------------
// Platform: macOS
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
async fn read_clipboard() -> Option<String> {
    let output = tokio::process::Command::new("pbpaste")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        None
    }
}

#[cfg(target_os = "macos")]
async fn write_clipboard(text: &str) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;
    let mut child = tokio::process::Command::new("pbcopy")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn pbcopy: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(text.as_bytes())
            .await
            .map_err(|e| format!("write to pbcopy: {e}"))?;
    }
    let status = child.wait().await.map_err(|e| format!("wait pbcopy: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("pbcopy exited with non-zero status".to_string())
    }
}

// ---------------------------------------------------------------------------
// Platform: Linux
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
async fn read_clipboard() -> Option<String> {
    let output = tokio::process::Command::new("xclip")
        .args(["-o", "-selection", "clipboard"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        None
    }
}

#[cfg(target_os = "linux")]
async fn write_clipboard(text: &str) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;
    let mut child = tokio::process::Command::new("xclip")
        .args(["-i", "-selection", "clipboard"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn xclip: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(text.as_bytes())
            .await
            .map_err(|e| format!("write to xclip: {e}"))?;
    }
    let status = child.wait().await.map_err(|e| format!("wait xclip: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("xclip exited with non-zero status".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_monitor_creates() {
        let monitor = ClipboardMonitor::new();
        assert!(!monitor.shutdown.load(Ordering::Relaxed));
    }

    #[test]
    fn clipboard_monitor_stop_sets_flag() {
        let monitor = ClipboardMonitor::new();
        monitor.stop();
        assert!(monitor.shutdown.load(Ordering::Relaxed));
    }
}
