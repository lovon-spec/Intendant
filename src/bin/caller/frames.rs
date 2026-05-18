use presence_core::FrameMeta;
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Server-side frame registry.
///
/// Stores frame metadata in memory and HQ frame data on disk under the session
/// directory (`<session_dir>/frames/`). The browser assigns frame IDs client-side
/// using a deterministic `{stream}-f{counter}` scheme; the server validates
/// uniqueness and persists HQ data.
pub struct FrameRegistry {
    /// Frame metadata indexed by frame_id.
    frames: HashMap<String, FrameMeta>,
    /// Ordered list of frame IDs for temporal queries.
    frame_order: Vec<String>,
    /// Per-stream counters tracking the latest frame.
    stream_heads: HashMap<String, String>,
    /// Directory for HQ frame storage.
    frames_dir: PathBuf,
}

impl FrameRegistry {
    /// Create a new frame registry, storing HQ frames under `session_dir/frames/`.
    pub fn new(session_dir: &Path) -> Self {
        let frames_dir = session_dir.join("frames");
        // Best-effort directory creation; callers should ensure session_dir exists.
        let _ = fs::create_dir_all(&frames_dir);
        Self {
            frames: HashMap::new(),
            frame_order: Vec::new(),
            stream_heads: HashMap::new(),
            frames_dir,
        }
    }

    /// Register a frame and store its HQ image data to disk.
    ///
    /// `hq_data` is the raw image bytes (e.g. JPEG).
    /// Returns the path where the HQ image was stored, or an error.
    pub fn register(&mut self, meta: FrameMeta, hq_data: &[u8]) -> Result<PathBuf, std::io::Error> {
        let hq_path = self.frames_dir.join(format!("{}.jpg", &meta.frame_id));
        fs::write(&hq_path, hq_data)?;

        // Append metadata to frames.jsonl manifest
        if let Ok(json_line) = serde_json::to_string(&meta) {
            let manifest = self.frames_dir.join("frames.jsonl");
            if let Ok(mut f) = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&manifest)
            {
                let _ = writeln!(f, "{}", json_line);
            }
        }

        let frame_id = meta.frame_id.clone();
        let stream = meta.stream.clone();

        self.stream_heads.insert(stream, frame_id.clone());
        self.frames.insert(frame_id.clone(), meta);
        self.frame_order.push(frame_id);

        Ok(hq_path)
    }

    /// Get metadata for a specific frame.
    pub fn get(&self, frame_id: &str) -> Option<&FrameMeta> {
        self.frames.get(frame_id)
    }

    /// Get the most recent frame ID across all streams, or for a specific stream.
    pub fn latest(&self, stream: Option<&str>) -> Option<&str> {
        match stream {
            Some(s) => self.stream_heads.get(s).map(|s| s.as_str()),
            None => self.frame_order.last().map(|s| s.as_str()),
        }
    }

    /// Read the HQ image data for a frame from disk.
    pub fn read_hq(&self, frame_id: &str) -> Result<Vec<u8>, std::io::Error> {
        let path = self.frames_dir.join(format!("{}.jpg", frame_id));
        fs::read(&path)
    }

    /// Compute the on-disk HQ path for a frame ID. Does not check for existence.
    /// Used by external-agent attachment resolvers that need to pass a file path
    /// (e.g. Codex `LocalImage`) instead of base64 over JSON-RPC.
    pub fn path_for(&self, frame_id: &str) -> PathBuf {
        self.frames_dir.join(format!("{}.jpg", frame_id))
    }

    /// Query frames by stream and/or recent count.
    /// Returns metadata for matching frames, most recent last.
    pub fn query(&self, stream: Option<&str>, count: usize) -> Vec<&FrameMeta> {
        let iter = self.frame_order.iter().rev();
        let mut results: Vec<&FrameMeta> = iter
            .filter_map(|fid| self.frames.get(fid))
            .filter(|meta| stream.map_or(true, |s| meta.stream == s))
            .take(count)
            .collect();
        results.reverse(); // chronological order
        results
    }

    /// Get all active stream names.
    pub fn active_streams(&self) -> Vec<String> {
        let mut streams: Vec<String> = self.stream_heads.keys().cloned().collect();
        streams.sort();
        streams
    }

    /// Total number of registered frames.
    pub fn total_frames(&self) -> u64 {
        self.frames.len() as u64
    }

    /// Build a `VideoState` snapshot for the presence layer.
    pub fn video_state(&self) -> presence_core::VideoState {
        presence_core::VideoState {
            active_streams: self.active_streams(),
            current_frame_id: self.frame_order.last().cloned(),
            total_frames: self.total_frames(),
        }
    }

    /// Format frame metadata for injection into tool responses.
    pub fn format_frame_list(frames: &[&FrameMeta]) -> String {
        if frames.is_empty() {
            return "No frames found.".to_string();
        }
        frames
            .iter()
            .map(|f| {
                let base = format!(
                    "{} | stream={} | ts={} | live={}",
                    f.frame_id, f.stream, f.timestamp, f.sent_to_live
                );
                match &f.note {
                    Some(n) if !n.is_empty() => format!("{} | note={}", base, n),
                    _ => base,
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_meta(frame_id: &str, stream: &str) -> FrameMeta {
        FrameMeta {
            frame_id: frame_id.to_string(),
            stream: stream.to_string(),
            timestamp: "2026-03-21T10:00:00Z".to_string(),
            sent_to_live: true,
            live_resolution: Some("768x768".to_string()),
            hq_resolution: Some("1920x1080".to_string()),
            note: None,
        }
    }

    #[test]
    fn register_and_retrieve() {
        let tmp = tempfile::tempdir().unwrap();
        let mut reg = FrameRegistry::new(tmp.path());

        let meta = test_meta("cam0-f00001", "cam0");
        let hq_data = b"fake jpeg data";
        let path = reg.register(meta, hq_data).unwrap();

        assert!(path.exists());
        assert_eq!(reg.total_frames(), 1);

        let retrieved = reg.get("cam0-f00001").unwrap();
        assert_eq!(retrieved.stream, "cam0");
        assert!(retrieved.sent_to_live);

        let data = reg.read_hq("cam0-f00001").unwrap();
        assert_eq!(data, hq_data);
    }

    #[test]
    fn latest_frame() {
        let tmp = tempfile::tempdir().unwrap();
        let mut reg = FrameRegistry::new(tmp.path());

        reg.register(test_meta("cam0-f00001", "cam0"), b"a")
            .unwrap();
        reg.register(test_meta("cam0-f00002", "cam0"), b"b")
            .unwrap();
        reg.register(test_meta("d99-f00001", "display:99"), b"c")
            .unwrap();

        assert_eq!(reg.latest(None), Some("d99-f00001"));
        assert_eq!(reg.latest(Some("cam0")), Some("cam0-f00002"));
        assert_eq!(reg.latest(Some("display:99")), Some("d99-f00001"));
        assert_eq!(reg.latest(Some("nonexistent")), None);
    }

    #[test]
    fn query_by_stream() {
        let tmp = tempfile::tempdir().unwrap();
        let mut reg = FrameRegistry::new(tmp.path());

        reg.register(test_meta("cam0-f00001", "cam0"), b"a")
            .unwrap();
        reg.register(test_meta("d99-f00001", "display:99"), b"b")
            .unwrap();
        reg.register(test_meta("cam0-f00002", "cam0"), b"c")
            .unwrap();

        let cam_frames = reg.query(Some("cam0"), 10);
        assert_eq!(cam_frames.len(), 2);
        assert_eq!(cam_frames[0].frame_id, "cam0-f00001");
        assert_eq!(cam_frames[1].frame_id, "cam0-f00002");

        let all = reg.query(None, 2);
        assert_eq!(all.len(), 2);
        // Most recent 2 in chronological order
        assert_eq!(all[0].frame_id, "d99-f00001");
        assert_eq!(all[1].frame_id, "cam0-f00002");
    }

    #[test]
    fn video_state_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let mut reg = FrameRegistry::new(tmp.path());

        let vs = reg.video_state();
        assert!(vs.active_streams.is_empty());
        assert!(vs.current_frame_id.is_none());
        assert_eq!(vs.total_frames, 0);

        reg.register(test_meta("cam0-f00001", "cam0"), b"a")
            .unwrap();
        reg.register(test_meta("cam0-f00002", "cam0"), b"b")
            .unwrap();

        let vs = reg.video_state();
        assert_eq!(vs.active_streams, vec!["cam0"]);
        assert_eq!(vs.current_frame_id.as_deref(), Some("cam0-f00002"));
        assert_eq!(vs.total_frames, 2);
    }

    #[test]
    fn format_frame_list_output() {
        let tmp = tempfile::tempdir().unwrap();
        let mut reg = FrameRegistry::new(tmp.path());

        reg.register(test_meta("cam0-f00001", "cam0"), b"a")
            .unwrap();
        reg.register(test_meta("cam0-f00002", "cam0"), b"b")
            .unwrap();

        let frames = reg.query(None, 10);
        let text = FrameRegistry::format_frame_list(&frames);
        assert!(text.contains("cam0-f00001"));
        assert!(text.contains("cam0-f00002"));
        assert!(text.contains("stream=cam0"));
    }

    #[test]
    fn format_frame_list_empty() {
        let text = FrameRegistry::format_frame_list(&[]);
        assert_eq!(text, "No frames found.");
    }
}
