//! Continuous video recording for display and camera streams.
//!
//! Uses ffmpeg to record Xvfb displays (x11grab) and browser camera frames
//! (image2pipe) into segmented MP4 files stored in the session directory.
//! Follows the same RAII guard pattern as `vision::XvfbGuard`.

use crate::event::{AppEvent, EventBus};
use crate::project::RecordingConfig;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;
use tokio::process::Child;

/// RAII guard for a single ffmpeg recording process.
/// Kills the ffmpeg process on drop.
pub struct RecordingGuard {
    child: Child,
    /// Stdin handle for piping frames (None for x11grab mode).
    stdin: Option<tokio::process::ChildStdin>,
    stream_name: String,
    segments_dir: PathBuf,
    started_at: chrono::DateTime<chrono::Utc>,
}

impl RecordingGuard {
    pub fn stream_name(&self) -> &str {
        &self.stream_name
    }

    pub fn segments_dir(&self) -> &Path {
        &self.segments_dir
    }

    pub fn started_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.started_at
    }

    /// Feed a JPEG frame into the recording pipeline (frame-fed mode only).
    pub async fn feed_frame(&mut self, jpeg_data: &[u8]) -> Result<(), std::io::Error> {
        if let Some(ref mut stdin) = self.stdin {
            stdin.write_all(jpeg_data).await?;
        }
        Ok(())
    }
}

impl Drop for RecordingGuard {
    fn drop(&mut self) {
        // Drop stdin first so ffmpeg sees EOF and can finalize
        self.stdin.take();
        let _ = self.child.start_kill();
    }
}

/// Check if ffmpeg is available on the system.
pub fn is_ffmpeg_available() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Start recording an X11 display via ffmpeg x11grab.
pub async fn start_display_recording(
    display_id: u32,
    width: u32,
    height: u32,
    config: &RecordingConfig,
    session_dir: &Path,
) -> Result<RecordingGuard, String> {
    let stream_name = format!("display_{}", display_id);
    let segments_dir = session_dir.join("recordings").join(&stream_name);
    std::fs::create_dir_all(&segments_dir)
        .map_err(|e| format!("Failed to create recordings dir: {}", e))?;

    let display_arg = format!(":{}", display_id);
    let size_arg = format!("{}x{}", width, height);
    let fps_arg = config.framerate.to_string();
    let crf_arg = config.crf().to_string();
    let seg_time_arg = config.segment_duration_secs.to_string();
    let output_pattern = segments_dir.join("seg_%05d.mp4");
    let segment_list = segments_dir.join("segments.csv");

    // Write manifest
    let manifest = serde_json::json!({
        "stream_name": stream_name,
        "started_at": chrono::Utc::now().to_rfc3339(),
        "framerate": config.framerate,
        "resolution": format!("{}x{}", width, height),
        "codec": "h264",
        "source": "x11grab",
    });
    let manifest_path = segments_dir.join("manifest.json");
    std::fs::write(&manifest_path, serde_json::to_string_pretty(&manifest).unwrap_or_default())
        .map_err(|e| format!("Failed to write manifest: {}", e))?;

    // Force keyframes at segment boundaries so segments split reliably.
    let keyframe_expr = format!("expr:gte(t,n_forced*{})", config.segment_duration_secs);
    let child = tokio::process::Command::new("ffmpeg")
        .args([
            "-f", "x11grab",
            "-framerate", &fps_arg,
            "-video_size", &size_arg,
            "-i", &display_arg,
            "-c:v", "libx264",
            "-preset", "ultrafast",
            "-crf", &crf_arg,
            "-pix_fmt", "yuv420p",
            "-force_key_frames", &keyframe_expr,
            "-f", "segment",
            "-segment_time", &seg_time_arg,
            "-segment_format", "mp4",
            "-segment_list", segment_list.to_str().unwrap_or("segments.csv"),
            "-segment_list_type", "csv",
            "-reset_timestamps", "1",
        ])
        .arg(output_pattern.to_str().unwrap_or("seg_%05d.mp4"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("Failed to spawn ffmpeg for display recording: {}", e))?;

    Ok(RecordingGuard {
        child,
        stdin: None,
        stream_name,
        segments_dir,
        started_at: chrono::Utc::now(),
    })
}

/// Start recording a frame-fed stream (camera frames piped via stdin as JPEG).
pub async fn start_frame_recording(
    stream_name: &str,
    config: &RecordingConfig,
    session_dir: &Path,
) -> Result<RecordingGuard, String> {
    let segments_dir = session_dir.join("recordings").join(stream_name);
    std::fs::create_dir_all(&segments_dir)
        .map_err(|e| format!("Failed to create recordings dir: {}", e))?;

    let fps_arg = config.framerate.to_string();
    let crf_arg = config.crf().to_string();
    let seg_time_arg = config.segment_duration_secs.to_string();
    let output_pattern = segments_dir.join("seg_%05d.mp4");
    let segment_list = segments_dir.join("segments.csv");

    // Write manifest
    let manifest = serde_json::json!({
        "stream_name": stream_name,
        "started_at": chrono::Utc::now().to_rfc3339(),
        "framerate": config.framerate,
        "codec": "h264",
        "source": "image2pipe",
    });
    let manifest_path = segments_dir.join("manifest.json");
    std::fs::write(&manifest_path, serde_json::to_string_pretty(&manifest).unwrap_or_default())
        .map_err(|e| format!("Failed to write manifest: {}", e))?;

    let keyframe_expr = format!("expr:gte(t,n_forced*{})", config.segment_duration_secs);
    let mut child = tokio::process::Command::new("ffmpeg")
        .args([
            "-f", "image2pipe",
            "-framerate", &fps_arg,
            "-use_wallclock_as_timestamps", "1",
            "-i", "pipe:0",
            "-c:v", "libx264",
            "-preset", "ultrafast",
            "-crf", &crf_arg,
            "-pix_fmt", "yuv420p",
            "-force_key_frames", &keyframe_expr,
            "-f", "segment",
            "-segment_time", &seg_time_arg,
            "-segment_format", "mp4",
            "-segment_list", segment_list.to_str().unwrap_or("segments.csv"),
            "-segment_list_type", "csv",
            "-reset_timestamps", "1",
        ])
        .arg(output_pattern.to_str().unwrap_or("seg_%05d.mp4"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("Failed to spawn ffmpeg for frame recording: {}", e))?;

    // Take stdin out of the child before moving it into the guard
    let stdin = child.stdin.take();

    Ok(RecordingGuard {
        child,
        stdin,
        stream_name: stream_name.to_string(),
        segments_dir,
        started_at: chrono::Utc::now(),
    })
}

/// Information about a recorded segment.
#[derive(Debug, Clone)]
pub struct SegmentInfo {
    pub filename: String,
    pub start_secs: f64,
    pub end_secs: f64,
    pub path: PathBuf,
}

/// Result of seeking to a timestamp within a recording.
#[derive(Debug, Clone)]
pub struct SeekResult {
    pub segment_path: PathBuf,
    pub offset_secs: f64,
}

/// Registry tracking active recordings and providing segment queries.
pub struct RecordingRegistry {
    recordings: HashMap<String, RecordingGuard>,
    /// Streams started via --record-display (external, persist across tasks).
    external_streams: std::collections::HashSet<String>,
    session_dir: PathBuf,
    config: RecordingConfig,
}

impl RecordingRegistry {
    pub fn new(session_dir: &Path, config: RecordingConfig) -> Self {
        Self {
            recordings: HashMap::new(),
            external_streams: std::collections::HashSet::new(),
            session_dir: session_dir.to_path_buf(),
            config,
        }
    }

    /// Whether recording is enabled in config.
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    /// Start recording a display stream.
    pub async fn start_display(
        &mut self,
        display_id: u32,
        width: u32,
        height: u32,
    ) -> Result<String, String> {
        let stream_name = format!("display_{}", display_id);
        if self.recordings.contains_key(&stream_name) {
            return Err(format!("Already recording stream: {}", stream_name));
        }
        let guard =
            start_display_recording(display_id, width, height, &self.config, &self.session_dir)
                .await?;
        self.recordings.insert(stream_name.clone(), guard);
        Ok(stream_name)
    }

    /// Start recording an external display (--record-display).
    /// External streams persist across task completions.
    pub async fn start_external_display(
        &mut self,
        display_id: u32,
        width: u32,
        height: u32,
    ) -> Result<String, String> {
        let stream_name = self.start_display(display_id, width, height).await?;
        self.external_streams.insert(stream_name.clone());
        Ok(stream_name)
    }

    /// Start recording a frame-fed stream (e.g. camera).
    pub async fn start_stream(&mut self, stream_name: &str) -> Result<(), String> {
        if self.recordings.contains_key(stream_name) {
            return Err(format!("Already recording stream: {}", stream_name));
        }
        let guard =
            start_frame_recording(stream_name, &self.config, &self.session_dir).await?;
        self.recordings.insert(stream_name.to_string(), guard);
        Ok(())
    }

    /// Feed a JPEG frame to an active frame-fed recording.
    pub async fn feed_frame(
        &mut self,
        stream_name: &str,
        jpeg_data: &[u8],
    ) -> Result<(), std::io::Error> {
        if let Some(guard) = self.recordings.get_mut(stream_name) {
            guard.feed_frame(jpeg_data).await?;
        }
        Ok(())
    }

    /// Check if a stream is currently being recorded.
    pub fn is_recording(&self, stream_name: &str) -> bool {
        self.recordings.contains_key(stream_name)
    }

    /// Stop recording a specific stream.
    pub fn stop(&mut self, stream_name: &str) {
        self.recordings.remove(stream_name);
    }

    /// Stop all recordings.
    pub fn stop_all(&mut self) {
        self.recordings.clear();
    }

    /// Stop only agent-managed recordings, keeping external (--record-display) streams alive.
    /// Returns the names of streams that were stopped.
    pub fn stop_agent_streams(&mut self) -> Vec<String> {
        let to_stop: Vec<String> = self
            .recordings
            .keys()
            .filter(|name| !self.external_streams.contains(*name))
            .cloned()
            .collect();
        for name in &to_stop {
            self.recordings.remove(name);
        }
        to_stop
    }

    /// List active recording stream names.
    pub fn active_streams(&self) -> Vec<String> {
        let mut names: Vec<String> = self.recordings.keys().cloned().collect();
        names.sort();
        names
    }

    /// Parse the segments.csv for a stream and return segment info.
    pub fn segments(&self, stream_name: &str) -> Vec<SegmentInfo> {
        let segments_dir = self.session_dir.join("recordings").join(stream_name);
        let csv_path = segments_dir.join("segments.csv");
        parse_segment_csv(&csv_path, &segments_dir)
    }

    /// Seek to a specific time offset (seconds from recording start) within a stream.
    pub fn seek(&self, stream_name: &str, offset_secs: f64) -> Option<SeekResult> {
        let segments = self.segments(stream_name);
        for seg in &segments {
            if offset_secs >= seg.start_secs && offset_secs < seg.end_secs {
                return Some(SeekResult {
                    segment_path: seg.path.clone(),
                    offset_secs: offset_secs - seg.start_secs,
                });
            }
        }
        // If past the end, return the last segment at its end
        segments.last().map(|seg| SeekResult {
            segment_path: seg.path.clone(),
            offset_secs: seg.end_secs - seg.start_secs,
        })
    }

    /// Get the session directory path (for serving segment files).
    pub fn session_dir(&self) -> &Path {
        &self.session_dir
    }

    /// Read the manifest.json for a stream, if it exists.
    pub fn manifest(&self, stream_name: &str) -> Option<serde_json::Value> {
        let manifest_path = self
            .session_dir
            .join("recordings")
            .join(stream_name)
            .join("manifest.json");
        let content = std::fs::read_to_string(manifest_path).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// Get all recorded streams (including stopped ones that have segments on disk).
    pub fn all_streams(&self) -> Vec<String> {
        let recordings_dir = self.session_dir.join("recordings");
        let mut streams = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&recordings_dir) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    if let Some(name) = entry.file_name().to_str() {
                        streams.push(name.to_string());
                    }
                }
            }
        }
        streams.sort();
        streams
    }
}

/// Parse ffmpeg's segment list CSV (filename,start_time,end_time).
fn parse_segment_csv(csv_path: &Path, segments_dir: &Path) -> Vec<SegmentInfo> {
    let content = match std::fs::read_to_string(csv_path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut segments = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split(',').collect();
        if parts.len() >= 3 {
            let filename = parts[0].trim().to_string();
            let start_secs: f64 = parts[1].trim().parse().unwrap_or(0.0);
            let end_secs: f64 = parts[2].trim().parse().unwrap_or(0.0);
            let path = segments_dir.join(&filename);
            segments.push(SegmentInfo {
                filename,
                start_secs,
                end_secs,
                path,
            });
        }
    }
    segments
}

/// Spawn a background task that listens for DisplayReady events and starts
/// display recording automatically.
pub fn spawn_recording_listener(
    mut event_rx: tokio::sync::broadcast::Receiver<AppEvent>,
    registry: std::sync::Arc<tokio::sync::RwLock<RecordingRegistry>>,
    bus: EventBus,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match event_rx.recv().await {
                Ok(AppEvent::DisplayReady {
                    display_id,
                    width,
                    height,
                    ..
                }) => {
                    let mut reg = registry.write().await;
                    if !reg.is_enabled() {
                        continue;
                    }
                    if !is_ffmpeg_available() {
                        bus.send(AppEvent::RecordingError {
                            stream_name: format!("display_{}", display_id),
                            message: "ffmpeg not installed — display recording disabled".to_string(),
                        });
                        continue;
                    }
                    match reg.start_display(display_id, width, height).await {
                        Ok(stream_name) => {
                            bus.send(AppEvent::RecordingStarted { stream_name });
                        }
                        Err(e) => {
                            bus.send(AppEvent::RecordingError {
                                stream_name: format!("display_{}", display_id),
                                message: e,
                            });
                        }
                    }
                }
                Ok(AppEvent::TaskComplete { .. }) => {
                    // Stop agent-managed recordings, keep external (--record-display) alive
                    let mut reg = registry.write().await;
                    let stopped = reg.stop_agent_streams();
                    for stream in &stopped {
                        bus.send(AppEvent::RecordingStopped {
                            stream_name: stream.clone(),
                        });
                    }
                    // Don't break — keep listening for new tasks (--continue)
                }
                Err(_) => {
                    // Channel closed — stop everything including external
                    let mut reg = registry.write().await;
                    let streams = reg.active_streams();
                    for stream in &streams {
                        bus.send(AppEvent::RecordingStopped {
                            stream_name: stream.clone(),
                        });
                    }
                    reg.stop_all();
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
    fn recording_config_crf_values() {
        let mut config = RecordingConfig::default();
        assert_eq!(config.crf(), 28); // medium default
        config.quality = "low".to_string();
        assert_eq!(config.crf(), 35);
        config.quality = "high".to_string();
        assert_eq!(config.crf(), 20);
        config.quality = "unknown".to_string();
        assert_eq!(config.crf(), 28); // fallback to medium
    }

    #[test]
    fn recording_config_defaults() {
        let config = RecordingConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.framerate, 30);
        assert_eq!(config.segment_duration_secs, 60);
        assert_eq!(config.quality, "medium");
        assert!(config.max_retention_hours.is_none());
    }

    #[test]
    fn recording_config_from_toml() {
        let toml_str = r#"
enabled = true
framerate = 15
segment_duration_secs = 120
quality = "high"
max_retention_hours = 48
"#;
        let config: RecordingConfig = toml::from_str(toml_str).unwrap();
        assert!(config.enabled);
        assert_eq!(config.framerate, 15);
        assert_eq!(config.segment_duration_secs, 120);
        assert_eq!(config.quality, "high");
        assert_eq!(config.max_retention_hours, Some(48));
    }

    #[test]
    fn parse_segment_csv_basic() {
        let tmp = tempfile::tempdir().unwrap();
        let csv = tmp.path().join("segments.csv");
        std::fs::write(&csv, "seg_00000.mp4,0.000000,60.000000\nseg_00001.mp4,60.000000,120.000000\n").unwrap();

        let segments = parse_segment_csv(&csv, tmp.path());
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].filename, "seg_00000.mp4");
        assert!((segments[0].start_secs - 0.0).abs() < 0.001);
        assert!((segments[0].end_secs - 60.0).abs() < 0.001);
        assert_eq!(segments[1].filename, "seg_00001.mp4");
        assert!((segments[1].start_secs - 60.0).abs() < 0.001);
    }

    #[test]
    fn parse_segment_csv_missing_file() {
        let segments = parse_segment_csv(Path::new("/nonexistent/segments.csv"), Path::new("/tmp"));
        assert!(segments.is_empty());
    }

    #[test]
    fn registry_new_and_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = RecordingRegistry::new(tmp.path(), RecordingConfig::default());
        assert!(!reg.is_enabled());
        assert!(reg.active_streams().is_empty());
        assert!(reg.all_streams().is_empty());
    }

    #[test]
    fn registry_seek_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = RecordingRegistry::new(tmp.path(), RecordingConfig::default());
        assert!(reg.seek("display_99", 10.0).is_none());
    }

    #[test]
    fn registry_seek_with_segments() {
        let tmp = tempfile::tempdir().unwrap();
        let stream_dir = tmp.path().join("recordings").join("display_99");
        std::fs::create_dir_all(&stream_dir).unwrap();
        // Write segment files so they exist
        std::fs::write(stream_dir.join("seg_00000.mp4"), b"fake").unwrap();
        std::fs::write(stream_dir.join("seg_00001.mp4"), b"fake").unwrap();
        // Write segment CSV
        std::fs::write(
            stream_dir.join("segments.csv"),
            "seg_00000.mp4,0.000000,60.000000\nseg_00001.mp4,60.000000,120.000000\n",
        ).unwrap();

        let reg = RecordingRegistry::new(tmp.path(), RecordingConfig::default());

        // Seek within first segment
        let result = reg.seek("display_99", 30.0).unwrap();
        assert!(result.segment_path.ends_with("seg_00000.mp4"));
        assert!((result.offset_secs - 30.0).abs() < 0.001);

        // Seek within second segment
        let result = reg.seek("display_99", 90.0).unwrap();
        assert!(result.segment_path.ends_with("seg_00001.mp4"));
        assert!((result.offset_secs - 30.0).abs() < 0.001);
    }

    #[test]
    fn registry_all_streams_reads_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let rec_dir = tmp.path().join("recordings");
        std::fs::create_dir_all(rec_dir.join("display_99")).unwrap();
        std::fs::create_dir_all(rec_dir.join("cam0")).unwrap();

        let reg = RecordingRegistry::new(tmp.path(), RecordingConfig::default());
        let streams = reg.all_streams();
        assert_eq!(streams, vec!["cam0", "display_99"]);
    }

    #[test]
    fn is_recording_returns_false_when_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = RecordingRegistry::new(tmp.path(), RecordingConfig::default());
        assert!(!reg.is_recording("display_99"));
    }
}
