//! Platform-agnostic virtual audio device management.
//!
//! Creates a bidirectional audio bridge between an app and a live audio model:
//! - Model audio output → virtual mic → app reads as mic input
//! - App audio output → virtual speaker → captured and sent to model
//!
//! On Linux, uses PulseAudio null sinks. On macOS, uses BlackHole virtual
//! audio driver with SwitchAudioSource for device management.

use crate::error::CallerError;
use std::process::Stdio;

/// Platform-agnostic handle to a virtual audio bridge.
///
/// The bridge creates two virtual audio devices and is cleaned up on drop.
/// Use `model_output_device()` and `app_capture_device()` to get the device
/// names for piping audio to/from the live model.
pub struct AudioBridge {
    inner: PlatformBridge,
    /// Previous default source, saved for restore on drop.
    prev_default_source: Option<String>,
    /// Previous default sink, saved for restore on drop.
    prev_default_sink: Option<String>,
}

impl AudioBridge {
    /// Device name where model audio output should be played.
    /// Apps reading mic input will hear audio written here.
    pub fn model_output_device(&self) -> &str {
        self.inner.model_output_device()
    }

    /// Device name to capture app audio from (feed to model).
    pub fn app_capture_device(&self) -> &str {
        self.inner.app_capture_device()
    }

    /// Build the command to capture audio from the app (for piping to the model).
    /// Returns (program, args) for spawning a subprocess whose stdout emits raw PCM16.
    pub fn capture_command(&self, sample_rate: u32) -> (&'static str, Vec<String>) {
        self.inner.capture_command(sample_rate)
    }

    /// Build the command to play audio to the app (model output → app mic).
    /// Returns (program, args) for spawning a subprocess whose stdin accepts raw PCM16.
    pub fn playback_command(&self, sample_rate: u32) -> (&'static str, Vec<String>) {
        self.inner.playback_command(sample_rate)
    }
}

impl Drop for AudioBridge {
    fn drop(&mut self) {
        // Restore previous defaults before destroying devices
        if let Some(ref source) = self.prev_default_source {
            self.inner.set_default_source(source);
        }
        if let Some(ref sink) = self.prev_default_sink {
            self.inner.set_default_sink(sink);
        }
        // PlatformBridge::drop handles device cleanup
    }
}

/// Check if virtual audio routing is available on this platform.
pub async fn is_available() -> bool {
    PlatformBridge::is_available().await
}

/// Create a virtual audio bridge for a live audio session.
///
/// The bridge is cleaned up on drop. Call `set_as_default()` to make the
/// virtual devices the system default (global routing).
pub async fn create_bridge(session_id: &str) -> Result<AudioBridge, CallerError> {
    let inner = PlatformBridge::create(session_id).await?;
    Ok(AudioBridge {
        inner,
        prev_default_source: None,
        prev_default_sink: None,
    })
}

/// Set the bridge's virtual devices as system defaults so all apps use them.
/// Saves the current defaults for restoration on drop.
pub async fn set_as_default(bridge: &mut AudioBridge) -> Result<(), CallerError> {
    let (prev_source, prev_sink) = bridge.inner.get_defaults().await?;
    bridge.prev_default_source = Some(prev_source);
    bridge.prev_default_sink = Some(prev_sink);
    bridge.inner.set_as_default().await
}

/// Route a specific app's audio through the bridge (per-app, not global).
pub async fn route_app(bridge: &AudioBridge, app_name: &str) -> Result<(), CallerError> {
    bridge.inner.route_app(app_name).await
}

// ─── PulseAudio backend (Linux) ─────────────────────────────────────────────

#[cfg(target_os = "linux")]
struct PlatformBridge {
    mic_module_id: Option<u32>,
    speaker_module_id: Option<u32>,
    mic_sink_name: String,
    speaker_sink_name: String,
    speaker_monitor_name: String,
    mic_monitor_name: String,
}

#[cfg(target_os = "linux")]
impl PlatformBridge {
    async fn is_available() -> bool {
        tokio::process::Command::new("pactl")
            .arg("info")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false)
    }

    async fn create(session_id: &str) -> Result<Self, CallerError> {
        let mic_sink_name = format!("intendant_mic_{}", session_id);
        let speaker_sink_name = format!("intendant_speaker_{}", session_id);

        let mic_module_id =
            load_null_sink(&mic_sink_name, "Intendant Virtual Mic").await?;

        let speaker_module_id =
            match load_null_sink(&speaker_sink_name, "Intendant Virtual Speaker").await {
                Ok(id) => id,
                Err(e) => {
                    let _ = std::process::Command::new("pactl")
                        .args(["unload-module", &mic_module_id.to_string()])
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status();
                    return Err(e);
                }
            };

        Ok(Self {
            mic_module_id: Some(mic_module_id),
            speaker_module_id: Some(speaker_module_id),
            mic_monitor_name: format!("{}.monitor", mic_sink_name),
            speaker_monitor_name: format!("{}.monitor", speaker_sink_name),
            mic_sink_name,
            speaker_sink_name,
        })
    }

    fn model_output_device(&self) -> &str {
        &self.mic_sink_name
    }

    fn app_capture_device(&self) -> &str {
        &self.speaker_monitor_name
    }

    fn capture_command(&self, sample_rate: u32) -> (&'static str, Vec<String>) {
        (
            "parec",
            vec![
                format!("--device={}", self.speaker_monitor_name),
                "--format=s16le".into(),
                format!("--rate={}", sample_rate),
                "--channels=1".into(),
                "--raw".into(),
            ],
        )
    }

    fn playback_command(&self, sample_rate: u32) -> (&'static str, Vec<String>) {
        (
            "pacat",
            vec![
                "--playback".into(),
                format!("--device={}", self.mic_sink_name),
                "--format=s16le".into(),
                format!("--rate={}", sample_rate),
                "--channels=1".into(),
                "--raw".into(),
            ],
        )
    }

    async fn get_defaults(&self) -> Result<(String, String), CallerError> {
        let source = pactl_get_default("default-source").await?;
        let sink = pactl_get_default("default-sink").await?;
        Ok((source, sink))
    }

    async fn set_as_default(&self) -> Result<(), CallerError> {
        // Set the mic monitor as default source (apps record from here)
        pactl_set_default("set-default-source", &self.mic_monitor_name).await?;
        // Set the speaker sink as default sink (apps play audio here)
        pactl_set_default("set-default-sink", &self.speaker_sink_name).await?;
        Ok(())
    }

    fn set_default_source(&self, name: &str) {
        let _ = std::process::Command::new("pactl")
            .args(["set-default-source", name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    fn set_default_sink(&self, name: &str) {
        let _ = std::process::Command::new("pactl")
            .args(["set-default-sink", name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    async fn route_app(&self, app_name: &str) -> Result<(), CallerError> {
        let sink_inputs = find_stream_indices("sink-input", app_name).await?;
        for idx in &sink_inputs {
            move_stream("sink-input", *idx, &self.speaker_sink_name).await?;
        }

        let source_outputs = find_stream_indices("source-output", app_name).await?;
        for idx in &source_outputs {
            move_stream("source-output", *idx, &self.mic_monitor_name).await?;
        }

        if sink_inputs.is_empty() && source_outputs.is_empty() {
            return Err(CallerError::Agent(format!(
                "no audio streams found for app '{}'",
                app_name
            )));
        }

        Ok(())
    }
}

#[cfg(target_os = "linux")]
impl Drop for PlatformBridge {
    fn drop(&mut self) {
        if let Some(id) = self.mic_module_id {
            let _ = std::process::Command::new("pactl")
                .args(["unload-module", &id.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        if let Some(id) = self.speaker_module_id {
            let _ = std::process::Command::new("pactl")
                .args(["unload-module", &id.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

// ─── PulseAudio helpers (Linux) ─────────────────────────────────────────────

#[cfg(target_os = "linux")]
async fn load_null_sink(sink_name: &str, description: &str) -> Result<u32, CallerError> {
    let output = tokio::process::Command::new("pactl")
        .args([
            "load-module",
            "module-null-sink",
            &format!("sink_name={}", sink_name),
            &format!(
                "sink_properties=device.description=\"{}\"",
                description
            ),
            "rate=24000",
            "channels=1",
            "format=s16le",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| CallerError::Agent(format!("failed to run pactl: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CallerError::Agent(format!(
            "pactl load-module failed for {}: {}",
            sink_name, stderr
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .trim()
        .parse::<u32>()
        .map_err(|e| {
            CallerError::Agent(format!(
                "failed to parse module ID: {} (output: {:?})",
                e, stdout
            ))
        })
}

#[cfg(target_os = "linux")]
async fn pactl_get_default(property: &str) -> Result<String, CallerError> {
    let output = tokio::process::Command::new("pactl")
        .args(["get-" .to_string() + property])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
        .map_err(|e| CallerError::Agent(format!("pactl get-{} failed: {}", property, e)))?;

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(target_os = "linux")]
async fn pactl_set_default(command: &str, name: &str) -> Result<(), CallerError> {
    let output = tokio::process::Command::new("pactl")
        .args([command, name])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| CallerError::Agent(format!("pactl {} failed: {}", command, e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CallerError::Agent(format!(
            "pactl {} {} failed: {}",
            command, name, stderr
        )));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
async fn find_stream_indices(
    stream_type: &str,
    app_name: &str,
) -> Result<Vec<u32>, CallerError> {
    let list_cmd = format!("list {}s", stream_type);
    let output = tokio::process::Command::new("pactl")
        .args(list_cmd.split_whitespace())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| CallerError::Agent(format!("pactl list failed: {}", e)))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_stream_indices(&stdout, app_name)
}

#[cfg(target_os = "linux")]
fn parse_stream_indices(pactl_output: &str, app_name: &str) -> Result<Vec<u32>, CallerError> {
    let mut indices = Vec::new();
    let mut current_index: Option<u32> = None;

    for line in pactl_output.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed
            .strip_prefix("Sink Input #")
            .or_else(|| trimmed.strip_prefix("Source Output #"))
        {
            current_index = rest.parse().ok();
        }
        if trimmed.contains("application.name") {
            if let Some(idx) = current_index {
                let name_lower = trimmed.to_lowercase();
                if name_lower.contains(&app_name.to_lowercase()) {
                    indices.push(idx);
                }
            }
        }
    }

    Ok(indices)
}

#[cfg(target_os = "linux")]
async fn move_stream(
    stream_type: &str,
    index: u32,
    target: &str,
) -> Result<(), CallerError> {
    let move_cmd = format!("move-{}", stream_type);
    let output = tokio::process::Command::new("pactl")
        .args([&move_cmd, &index.to_string(), target])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| CallerError::Agent(format!("pactl move failed: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CallerError::Agent(format!(
            "pactl {} {} {} failed: {}",
            move_cmd, index, target, stderr
        )));
    }
    Ok(())
}

// ─── macOS backend (BlackHole + SwitchAudioSource) ──────────────────────────
//
// Requires:
// - BlackHole 2ch AND BlackHole 16ch: brew install --cask blackhole-2ch blackhole-16ch
//   (reboot required after install)
// - SwitchAudioSource: brew install switchaudio-osx
// - sox (with CoreAudio support): brew install sox
//
// BlackHole 2ch = virtual mic (model output → app mic input)
// BlackHole 16ch = virtual speaker (app audio output → model capture)

#[cfg(target_os = "macos")]
struct PlatformBridge {
    /// BlackHole device used as virtual mic (model plays here, apps read as mic).
    mic_device_name: String,
    /// BlackHole device used as virtual speaker (apps play here, model captures).
    speaker_device_name: String,
}

#[cfg(target_os = "macos")]
impl PlatformBridge {
    async fn is_available() -> bool {
        has_switchaudio().await && find_blackhole_devices().await.is_some()
    }

    async fn create(_session_id: &str) -> Result<Self, CallerError> {
        if !has_switchaudio().await {
            return Err(CallerError::Agent(
                "SwitchAudioSource is required for audio routing on macOS. \
                 Install with: brew install switchaudio-osx"
                    .into(),
            ));
        }
        let (mic, speaker) = find_blackhole_devices().await.ok_or_else(|| {
            CallerError::Agent(
                "BlackHole virtual audio driver not found. Two instances are required \
                 for bidirectional audio: BlackHole 2ch (virtual mic) and \
                 BlackHole 16ch (app capture). Install with: \
                 brew install --cask blackhole-2ch blackhole-16ch \
                 (reboot required after install)"
                    .into(),
            )
        })?;
        Ok(Self {
            mic_device_name: mic,
            speaker_device_name: speaker,
        })
    }

    fn model_output_device(&self) -> &str {
        &self.mic_device_name
    }

    fn app_capture_device(&self) -> &str {
        &self.speaker_device_name
    }

    fn capture_command(&self, sample_rate: u32) -> (&'static str, Vec<String>) {
        (
            "sox",
            vec![
                "-t".into(), "coreaudio".into(),
                self.speaker_device_name.clone(),
                "-t".into(), "raw".into(),
                "-r".into(), sample_rate.to_string(),
                "-e".into(), "signed-integer".into(),
                "-b".into(), "16".into(),
                "-c".into(), "1".into(),
                "-".into(),
            ],
        )
    }

    fn playback_command(&self, sample_rate: u32) -> (&'static str, Vec<String>) {
        (
            "sox",
            vec![
                "-t".into(), "raw".into(),
                "-r".into(), sample_rate.to_string(),
                "-e".into(), "signed-integer".into(),
                "-b".into(), "16".into(),
                "-c".into(), "1".into(),
                "-".into(),
                "-t".into(), "coreaudio".into(),
                self.mic_device_name.clone(),
            ],
        )
    }

    async fn get_defaults(&self) -> Result<(String, String), CallerError> {
        let source = switchaudio_get_current("input").await?;
        let sink = switchaudio_get_current("output").await?;
        Ok((source, sink))
    }

    async fn set_as_default(&self) -> Result<(), CallerError> {
        // Default input = mic device (apps record model output from here)
        switchaudio_set(&self.mic_device_name, "input").await?;
        // Default output = speaker device (apps play audio here for model to capture)
        switchaudio_set(&self.speaker_device_name, "output").await?;
        Ok(())
    }

    fn set_default_source(&self, name: &str) {
        let _ = std::process::Command::new("SwitchAudioSource")
            .args(["-s", name, "-t", "input"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    fn set_default_sink(&self, name: &str) {
        let _ = std::process::Command::new("SwitchAudioSource")
            .args(["-s", name, "-t", "output"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    async fn route_app(&self, _app_name: &str) -> Result<(), CallerError> {
        Err(CallerError::Agent(
            "Per-app audio routing is not supported on macOS. \
             Use set_as_default to route all audio through the bridge."
                .into(),
        ))
    }
}

#[cfg(target_os = "macos")]
impl Drop for PlatformBridge {
    fn drop(&mut self) {
        // BlackHole devices are system-level (kernel extension), no cleanup needed.
        // Default device restoration is handled by AudioBridge::drop.
    }
}

// ─── macOS helpers ──────────────────────────────────────────────────────────

/// Check if SwitchAudioSource CLI is installed.
#[cfg(target_os = "macos")]
async fn has_switchaudio() -> bool {
    tokio::process::Command::new("SwitchAudioSource")
        .args(["-a", "-t", "output"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Find BlackHole 2ch and 16ch devices. Returns (mic_device, speaker_device).
#[cfg(target_os = "macos")]
async fn find_blackhole_devices() -> Option<(String, String)> {
    // List all output devices — BlackHole appears as both input and output
    let output = tokio::process::Command::new("SwitchAudioSource")
        .args(["-a", "-t", "output"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
        .ok()?;

    let devices = String::from_utf8_lossy(&output.stdout);
    let has_2ch = devices.lines().any(|l| l.trim() == "BlackHole 2ch");
    let has_16ch = devices.lines().any(|l| l.trim() == "BlackHole 16ch");

    if has_2ch && has_16ch {
        // 2ch = virtual mic (model → app), 16ch = virtual speaker (app → model)
        Some(("BlackHole 2ch".into(), "BlackHole 16ch".into()))
    } else {
        None
    }
}

/// Get the current default device for a given type ("input" or "output").
#[cfg(target_os = "macos")]
async fn switchaudio_get_current(device_type: &str) -> Result<String, CallerError> {
    let output = tokio::process::Command::new("SwitchAudioSource")
        .args(["-c", "-t", device_type])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| CallerError::Agent(format!("SwitchAudioSource failed: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CallerError::Agent(format!(
            "SwitchAudioSource -c -t {} failed: {}",
            device_type, stderr
        )));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Set the default device for a given type ("input" or "output").
#[cfg(target_os = "macos")]
async fn switchaudio_set(device_name: &str, device_type: &str) -> Result<(), CallerError> {
    let output = tokio::process::Command::new("SwitchAudioSource")
        .args(["-s", device_name, "-t", device_type])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| CallerError::Agent(format!("SwitchAudioSource failed: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CallerError::Agent(format!(
            "SwitchAudioSource -s '{}' -t {} failed: {}",
            device_name, device_type, stderr
        )));
    }
    Ok(())
}

// ─── Fallback for other platforms ───────────────────────────────────────────

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
struct PlatformBridge;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
impl PlatformBridge {
    async fn is_available() -> bool { false }
    async fn create(_session_id: &str) -> Result<Self, CallerError> {
        Err(CallerError::Agent("virtual audio routing is not supported on this platform".into()))
    }
    fn model_output_device(&self) -> &str { "" }
    fn app_capture_device(&self) -> &str { "" }
    fn capture_command(&self, _sample_rate: u32) -> (&'static str, Vec<String>) { ("false", vec![]) }
    fn playback_command(&self, _sample_rate: u32) -> (&'static str, Vec<String>) { ("false", vec![]) }
    async fn get_defaults(&self) -> Result<(String, String), CallerError> {
        Err(CallerError::Agent("not supported".into()))
    }
    async fn set_as_default(&self) -> Result<(), CallerError> {
        Err(CallerError::Agent("not supported".into()))
    }
    fn set_default_source(&self, _name: &str) {}
    fn set_default_sink(&self, _name: &str) {}
    async fn route_app(&self, _app_name: &str) -> Result<(), CallerError> {
        Err(CallerError::Agent("not supported".into()))
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
impl Drop for PlatformBridge {
    fn drop(&mut self) {}
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_sink_inputs() {
        let output = r#"
Sink Input #42
	Driver: protocol-native.c
	Owner Module: 9
	Client: 15
	Sink: 0
	Properties:
		media.name = "Playback"
		application.name = "WhatsApp"
		native-protocol.peer = "UNIX socket client"

Sink Input #43
	Driver: protocol-native.c
	Owner Module: 9
	Client: 16
	Sink: 0
	Properties:
		media.name = "Playback"
		application.name = "Firefox"
"#;

        let indices = parse_stream_indices(output, "WhatsApp").unwrap();
        assert_eq!(indices, vec![42]);

        let indices = parse_stream_indices(output, "firefox").unwrap();
        assert_eq!(indices, vec![43]);

        let indices = parse_stream_indices(output, "Chrome").unwrap();
        assert!(indices.is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_source_outputs() {
        let output = r#"
Source Output #7
	Driver: protocol-native.c
	Owner Module: 9
	Client: 15
	Source: 0
	Properties:
		media.name = "Record"
		application.name = "WhatsApp"
"#;

        let indices = parse_stream_indices(output, "WhatsApp").unwrap();
        assert_eq!(indices, vec![7]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_empty_output() {
        let indices = parse_stream_indices("", "anything").unwrap();
        assert!(indices.is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn capture_command_uses_parec() {
        let bridge = PlatformBridge {
            mic_module_id: None,
            speaker_module_id: None,
            mic_sink_name: "intendant_mic_test".into(),
            speaker_sink_name: "intendant_speaker_test".into(),
            mic_monitor_name: "intendant_mic_test.monitor".into(),
            speaker_monitor_name: "intendant_speaker_test.monitor".into(),
        };
        let (cmd, args) = bridge.capture_command(24000);
        assert_eq!(cmd, "parec");
        assert!(args.iter().any(|a| a.contains("intendant_speaker_test.monitor")));
        assert!(args.iter().any(|a| a.contains("24000")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn playback_command_uses_pacat() {
        let bridge = PlatformBridge {
            mic_module_id: None,
            speaker_module_id: None,
            mic_sink_name: "intendant_mic_test".into(),
            speaker_sink_name: "intendant_speaker_test".into(),
            mic_monitor_name: "intendant_mic_test.monitor".into(),
            speaker_monitor_name: "intendant_speaker_test.monitor".into(),
        };
        let (cmd, args) = bridge.playback_command(24000);
        assert_eq!(cmd, "pacat");
        assert!(args.iter().any(|a| a.contains("intendant_mic_test")));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn capture_command_uses_sox_coreaudio() {
        let bridge = PlatformBridge {
            mic_device_name: "BlackHole 2ch".into(),
            speaker_device_name: "BlackHole 16ch".into(),
        };
        let (cmd, args) = bridge.capture_command(24000);
        assert_eq!(cmd, "sox");
        assert!(args.contains(&"coreaudio".into()));
        assert!(args.contains(&"BlackHole 16ch".into()));
        assert!(args.contains(&"24000".to_string()));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn playback_command_uses_sox_coreaudio() {
        let bridge = PlatformBridge {
            mic_device_name: "BlackHole 2ch".into(),
            speaker_device_name: "BlackHole 16ch".into(),
        };
        let (cmd, args) = bridge.playback_command(24000);
        assert_eq!(cmd, "sox");
        assert!(args.contains(&"coreaudio".into()));
        assert!(args.contains(&"BlackHole 2ch".into()));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn model_output_is_mic_device() {
        let bridge = PlatformBridge {
            mic_device_name: "BlackHole 2ch".into(),
            speaker_device_name: "BlackHole 16ch".into(),
        };
        assert_eq!(bridge.model_output_device(), "BlackHole 2ch");
        assert_eq!(bridge.app_capture_device(), "BlackHole 16ch");
    }
}
