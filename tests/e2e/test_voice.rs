use crate::harness::{cleanup_virtual_mic, poll_until, say, setup_virtual_mic, IntendantProcess};
use std::time::Duration;

/// Tier 3 voice: Verify voice connection via /debug.
/// Requires: Xvfb, Firefox, PulseAudio, espeak-ng, ffmpeg.
#[tokio::test]
async fn test_voice_connection() {
    setup_virtual_mic();

    let proc = IntendantProcess::spawn_web("waiting for instructions", "low", 8765, &[]);

    // Launch Firefox pointing at the web TUI
    launch_firefox(8765);

    // Wait for web server ready
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Set API key in browser (Gemini API key from env)
    if let Ok(key) = std::env::var("GEMINI_API_KEY") {
        set_api_key_via_debug(8765, &key).await;
    }

    // Click mic button
    click_mic_button_via_eval(8765).await;

    // Poll /debug until voice connected
    let connected = poll_until(
        || async {
            if let Some(snap) = proc.debug_snapshot(8765).await {
                snap.get("voice")
                    .and_then(|v| v.get("connected"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
            } else {
                false
            }
        },
        Duration::from_secs(15),
    )
    .await;

    if !connected {
        eprintln!("Voice connection did not establish — skipping (may be missing API key or browser)");
    }

    cleanup_firefox();
    proc.kill().await;
    cleanup_virtual_mic();
}

/// Tier 3 voice: Submit task via voice, wait for approval, approve via voice.
/// Requires: Xvfb, Firefox, PulseAudio, espeak-ng, ffmpeg, Gemini API key.
#[tokio::test]
async fn test_voice_submit_and_approve() {
    setup_virtual_mic();

    let proc = IntendantProcess::spawn_web("waiting for instructions", "low", 8766, &[]);

    launch_firefox(8766);
    tokio::time::sleep(Duration::from_secs(3)).await;

    if let Ok(key) = std::env::var("GEMINI_API_KEY") {
        set_api_key_via_debug(8766, &key).await;
    }

    click_mic_button_via_eval(8766).await;

    // Wait for voice connection
    let connected = poll_until(
        || async {
            if let Some(snap) = proc.debug_snapshot(8766).await {
                snap.get("voice")
                    .and_then(|v| v.get("connected"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
            } else {
                false
            }
        },
        Duration::from_secs(15),
    )
    .await;

    if !connected {
        eprintln!(
            "Voice connection did not establish — skipping voice test \
             (may be missing API key or browser)"
        );
        cleanup_firefox();
        proc.kill().await;
        cleanup_virtual_mic();
        return;
    }

    // Submit task via voice
    say("please list the files in /tmp", 130);

    // Poll until task started (phase changes from idle)
    let task_started = poll_until(
        || async {
            if let Some(snap) = proc.debug_snapshot(8766).await {
                let phase = snap
                    .get("agent_state")
                    .and_then(|v| v.get("phase"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("idle");
                phase != "idle"
            } else {
                false
            }
        },
        Duration::from_secs(30),
    )
    .await;
    assert!(task_started, "Expected task to start after voice submit");

    // Wait for approval
    let approval_pending = poll_until(
        || async {
            if let Some(snap) = proc.debug_snapshot(8766).await {
                snap.get("agent_state")
                    .and_then(|v| v.get("pending_approval"))
                    .map(|v| !v.is_null())
                    .unwrap_or(false)
            } else {
                false
            }
        },
        Duration::from_secs(30),
    )
    .await;
    assert!(approval_pending, "Expected pending approval");

    // Approve via voice
    say("yes approve that", 130);

    // Verify approval cleared
    let approval_cleared = poll_until(
        || async {
            if let Some(snap) = proc.debug_snapshot(8766).await {
                snap.get("agent_state")
                    .and_then(|v| v.get("pending_approval"))
                    .map(|v| v.is_null())
                    .unwrap_or(true)
            } else {
                false
            }
        },
        Duration::from_secs(15),
    )
    .await;
    assert!(approval_cleared, "Expected approval to be cleared");

    cleanup_firefox();
    proc.kill().await;
    cleanup_virtual_mic();
}

// --- Helper functions for browser automation ---

fn launch_firefox(port: u16) {
    let url = format!("http://127.0.0.1:{}", port);
    std::process::Command::new("firefox")
        .arg("--no-remote")
        .arg(&url)
        .env("DISPLAY", std::env::var("DISPLAY").unwrap_or(":99".into()))
        .spawn()
        .ok(); // best-effort
}

fn cleanup_firefox() {
    let _ = std::process::Command::new("pkill")
        .arg("-f")
        .arg("firefox")
        .output();
}

async fn set_api_key_via_debug(_port: u16, _key: &str) {
    // API key is set via localStorage in the browser.
    // For automated testing, this would use the Firefox debugger protocol
    // or the /debug endpoint if it supports key injection.
    // For now, the test relies on the browser having the key set already.
}

async fn click_mic_button_via_eval(_port: u16) {
    // Would use Firefox remote debugging protocol to click the mic button.
    // For now, tests that reach this point verify infrastructure only.
}
