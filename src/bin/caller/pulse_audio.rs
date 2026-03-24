use crate::error::CallerError;
use std::process::Stdio;
use tokio::process::Command;

/// A bidirectional PulseAudio bridge between an app and a live audio model.
///
/// Creates two null sinks:
/// - `intendant_mic_<id>`: model audio output -> app reads from its `.monitor` as mic input
/// - `intendant_speaker_<id>`: app audio output -> captured from its `.monitor` -> sent to model
///
/// Cleaned up on drop via `pactl unload-module`.
pub struct PulseAudioBridge {
    mic_module_id: Option<u32>,
    speaker_module_id: Option<u32>,
    pub mic_sink_name: String,
    pub speaker_sink_name: String,
    pub speaker_monitor_name: String,
    pub mic_monitor_name: String,
}

impl PulseAudioBridge {
    /// The sink name where model audio should be played (app reads from its .monitor).
    pub fn model_output_sink(&self) -> &str {
        &self.mic_sink_name
    }

    /// The monitor source name to capture app audio from (feed to model).
    pub fn app_audio_monitor(&self) -> &str {
        &self.speaker_monitor_name
    }
}

impl Drop for PulseAudioBridge {
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

/// Check if PulseAudio is available and running.
pub async fn is_pulse_available() -> bool {
    Command::new("pactl")
        .arg("info")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Create a PulseAudio bridge for a live audio session.
///
/// This creates two null sinks with unique names based on the session ID.
/// The bridge must be kept alive for the duration of the session; dropping it
/// unloads the PulseAudio modules.
pub async fn create_bridge(session_id: &str) -> Result<PulseAudioBridge, CallerError> {
    let mic_sink_name = format!("intendant_mic_{}", session_id);
    let speaker_sink_name = format!("intendant_speaker_{}", session_id);

    // Create the mic sink (model output -> app mic input)
    let mic_module_id = load_null_sink(&mic_sink_name, "Intendant Virtual Mic").await?;

    // Create the speaker sink (app audio output -> model input)
    let speaker_module_id = match load_null_sink(&speaker_sink_name, "Intendant Virtual Speaker").await {
        Ok(id) => id,
        Err(e) => {
            // Clean up the mic sink if speaker creation fails
            let _ = std::process::Command::new("pactl")
                .args(["unload-module", &mic_module_id.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            return Err(e);
        }
    };

    Ok(PulseAudioBridge {
        mic_module_id: Some(mic_module_id),
        speaker_module_id: Some(speaker_module_id),
        mic_monitor_name: format!("{}.monitor", mic_sink_name),
        speaker_monitor_name: format!("{}.monitor", speaker_sink_name),
        mic_sink_name,
        speaker_sink_name,
    })
}

/// Load a null-sink PulseAudio module and return its module ID.
async fn load_null_sink(sink_name: &str, description: &str) -> Result<u32, CallerError> {
    let output = Command::new("pactl")
        .args([
            "load-module",
            "module-null-sink",
            &format!("sink_name={}", sink_name),
            &format!("sink_properties=device.description=\"{}\"", description),
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
        .map_err(|e| CallerError::Agent(format!("failed to parse module ID: {} (output: {:?})", e, stdout)))
}

/// Route an application's audio through the bridge.
///
/// Moves the app's sink-input (audio output) to the speaker sink, and the app's
/// source-output (mic input) to the mic sink's monitor. The app is identified
/// by its PulseAudio application name.
pub async fn route_app_to_bridge(
    bridge: &PulseAudioBridge,
    app_name: &str,
) -> Result<(), CallerError> {
    // Find sink-inputs belonging to the app
    let sink_inputs = find_stream_indices("sink-input", app_name).await?;
    for idx in &sink_inputs {
        move_stream("sink-input", *idx, &bridge.speaker_sink_name).await?;
    }

    // Find source-outputs belonging to the app
    let source_outputs = find_stream_indices("source-output", app_name).await?;
    for idx in &source_outputs {
        move_stream("source-output", *idx, &bridge.mic_monitor_name).await?;
    }

    if sink_inputs.is_empty() && source_outputs.is_empty() {
        return Err(CallerError::Agent(format!(
            "no PulseAudio streams found for app '{}'",
            app_name
        )));
    }

    Ok(())
}

/// Find PulseAudio stream indices matching an application name.
async fn find_stream_indices(
    stream_type: &str,
    app_name: &str,
) -> Result<Vec<u32>, CallerError> {
    let list_cmd = format!("list {}s", stream_type);
    let output = Command::new("pactl")
        .args(list_cmd.split_whitespace())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| CallerError::Agent(format!("pactl list failed: {}", e)))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_stream_indices(&stdout, app_name)
}

/// Parse `pactl list sink-inputs` / `pactl list source-outputs` output
/// to find stream indices matching an application name.
fn parse_stream_indices(pactl_output: &str, app_name: &str) -> Result<Vec<u32>, CallerError> {
    let mut indices = Vec::new();
    let mut current_index: Option<u32> = None;

    for line in pactl_output.lines() {
        let trimmed = line.trim();

        // Lines like "Sink Input #42" or "Source Output #7"
        if let Some(rest) = trimmed.strip_prefix("Sink Input #")
            .or_else(|| trimmed.strip_prefix("Source Output #"))
        {
            current_index = rest.parse().ok();
        }

        // Lines like "application.name = \"Firefox\""
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

/// Move a PulseAudio stream to a different sink/source.
async fn move_stream(
    stream_type: &str,
    index: u32,
    target: &str,
) -> Result<(), CallerError> {
    let move_cmd = format!("move-{}", stream_type);
    let output = Command::new("pactl")
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn parse_empty_output() {
        let indices = parse_stream_indices("", "anything").unwrap();
        assert!(indices.is_empty());
    }

    #[test]
    fn bridge_names_are_correct() {
        // Can't create a real bridge without PulseAudio, but verify the naming
        let bridge = PulseAudioBridge {
            mic_module_id: None,
            speaker_module_id: None,
            mic_sink_name: "intendant_mic_test1".into(),
            speaker_sink_name: "intendant_speaker_test1".into(),
            mic_monitor_name: "intendant_mic_test1.monitor".into(),
            speaker_monitor_name: "intendant_speaker_test1.monitor".into(),
        };

        assert_eq!(bridge.model_output_sink(), "intendant_mic_test1");
        assert_eq!(bridge.app_audio_monitor(), "intendant_speaker_test1.monitor");
    }

    #[test]
    fn drop_with_no_modules_is_safe() {
        // Drop a bridge with no module IDs — should not panic
        let _bridge = PulseAudioBridge {
            mic_module_id: None,
            speaker_module_id: None,
            mic_sink_name: String::new(),
            speaker_sink_name: String::new(),
            mic_monitor_name: String::new(),
            speaker_monitor_name: String::new(),
        };
    }
}
