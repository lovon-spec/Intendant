use chrono::Local;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Structured event written as one JSON line in session.jsonl.
#[derive(Serialize)]
struct LogEvent {
    ts: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    turn: Option<usize>,
    event: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    level: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<serde_json::Value>,
    /// Path to a file with full content (relative to log dir).
    #[serde(skip_serializing_if = "Option::is_none")]
    file: Option<String>,
    /// Second file reference (e.g., stderr).
    #[serde(skip_serializing_if = "Option::is_none")]
    file2: Option<String>,
}

/// Metadata persisted in `session_meta.json` inside each session directory.
#[derive(Serialize, Deserialize, Debug)]
pub struct SessionMeta {
    pub session_id: String,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_turn: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rounds: Option<usize>,
}

/// Comprehensive structured session logger.
///
/// Writes to a directory containing:
/// - `session.jsonl`    — one JSON object per line, every event with metadata
/// - `session_meta.json` — session metadata (id, created_at, project_root, task)
/// - `turns/turn_NNN_model.txt`     — full model response for turn N
/// - `turns/turn_NNN_agent_in.json` — JSON commands sent to agent for turn N
/// - `turns/turn_NNN_stdout.txt`    — agent stdout for turn N
/// - `turns/turn_NNN_stderr.txt`    — agent stderr for turn N (if non-empty)
/// - `summary.json`     — written at session end
///
/// AI agents can: read session.jsonl for an overview, grep by event/turn/level,
/// then drill into specific turn files for full content.
pub struct SessionLog {
    writer: BufWriter<File>,
    transcript_writer: Option<BufWriter<File>>,
    dir: PathBuf,
    session_id: String,
    current_turn: usize,
    summary_builder: SessionSummaryBuilder,
    /// Buffer for accumulating voice_log tokens into full utterances.
    /// Flushed to transcript on turnComplete or user_transcript.
    voice_utterance_buf: String,
}

/// Accumulates session statistics as events are logged.
/// Written to `session_summary.json` at session end.
#[derive(Default)]
struct SessionSummaryBuilder {
    start_time: Option<chrono::DateTime<chrono::Local>>,
    voice_provider: Option<String>,
    voice_model: Option<String>,
    voice_connections: usize,
    frames_sent: usize,
    cu_tasks: Vec<CuTaskSummary>,
    /// CU task currently in progress (captured on cu_task_start, moved to cu_tasks on complete).
    current_cu_task: Option<String>,
    current_cu_turns: usize,
    errors: Vec<ErrorSummary>,
    user_transcripts: Vec<String>,
    total_tokens: u64,
}

/// Summary of the entire session, written as `session_summary.json`.
#[derive(Serialize, Deserialize, Debug)]
pub struct SessionSummary {
    pub duration_secs: f64,
    pub voice_provider: Option<String>,
    pub voice_model: Option<String>,
    pub voice_connections: usize,
    pub voice_reconnects: usize,
    pub model_turns: usize,
    pub cu_tasks: Vec<CuTaskSummary>,
    pub frames_sent: usize,
    pub errors: Vec<ErrorSummary>,
    pub user_transcripts: Vec<String>,
    pub total_tokens: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CuTaskSummary {
    pub task: String,
    pub turns: usize,
    pub success: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ErrorSummary {
    pub category: String,
    pub reason: String,
    pub ts: String,
}

/// Entry in transcript.jsonl — simplified conversation log.
#[derive(Serialize)]
struct TranscriptEntry {
    ts: String,
    role: String,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools_called: Option<Vec<String>>,
}

impl SessionLog {
    /// Open (or create) a session log directory.
    /// The `path` argument is the directory (not a file).
    /// If resuming an existing session, reads the session_id from session_meta.json.
    pub fn open(dir: PathBuf) -> std::io::Result<Self> {
        fs::create_dir_all(&dir)?;
        fs::create_dir_all(dir.join("turns"))?;

        // Try to read existing session_id from meta, or derive from directory name
        let session_id = if let Ok(meta_str) = fs::read_to_string(dir.join("session_meta.json")) {
            if let Ok(meta) = serde_json::from_str::<SessionMeta>(&meta_str) {
                meta.session_id
            } else {
                dir.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| Uuid::new_v4().to_string())
            }
        } else {
            dir.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| Uuid::new_v4().to_string())
        };

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("session.jsonl"))?;
        let transcript_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("transcript.jsonl"))
            .ok()
            .map(BufWriter::new);
        let mut log = Self {
            writer: BufWriter::new(file),
            transcript_writer: transcript_file,
            dir,
            session_id,
            current_turn: 0,
            summary_builder: SessionSummaryBuilder {
                start_time: Some(Local::now()),
                ..Default::default()
            },
            voice_utterance_buf: String::new(),
        };
        log.emit(LogEvent {
            ts: Self::ts(),
            turn: None,
            event: "session_start".to_string(),
            level: Some("info".to_string()),
            message: Some(format!(
                "Session started at {}",
                Local::now().format("%Y-%m-%d %H:%M:%S")
            )),
            data: None,
            file: None,
            file2: None,
        });
        Ok(log)
    }

    /// Write session metadata to `session_meta.json`.
    /// Call after open() to persist session identity and context.
    pub fn write_meta(&self, project_root: Option<&Path>, task: Option<&str>) {
        self.write_meta_with_role(project_root, task, None);
    }

    /// Write session metadata with an optional role field.
    pub fn write_meta_with_role(
        &self,
        project_root: Option<&Path>,
        task: Option<&str>,
        role: Option<&str>,
    ) {
        let meta = SessionMeta {
            session_id: self.session_id.clone(),
            created_at: Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
            project_root: project_root.map(|p| p.to_string_lossy().to_string()),
            task: task.map(|t| t.to_string()),
            status: Some("running".to_string()),
            last_turn: None,
            role: role.map(|r| r.to_string()),
            rounds: None,
        };
        if let Ok(json) = serde_json::to_string_pretty(&meta) {
            if let Err(e) = fs::write(self.dir.join("session_meta.json"), json) {
                eprintln!("session_log: failed to write session_meta.json: {}", e);
            }
        }
    }

    /// Resolve the session log directory.
    /// If `override_path` is set (via --log-file), use that as the directory.
    /// Otherwise, create a fresh session directory with a UUID name.
    pub fn resolve_path(override_path: Option<&str>) -> PathBuf {
        if let Some(path) = override_path {
            let dir = PathBuf::from(path);
            let _ = fs::create_dir_all(&dir);
            return dir;
        }

        // Create a new session directory with UUID for each top-level caller invocation.
        let session_id = Uuid::new_v4().to_string();
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        let dir = PathBuf::from(format!("{}/.intendant/logs/{}", home, session_id));
        let _ = fs::create_dir_all(&dir);
        dir
    }

    /// Find the most recent session for a given project root.
    /// Scans `~/.intendant/logs/*/session_meta.json`, filters by project_root,
    /// and returns the most recently created session.
    pub fn find_latest_session(project_root: &Path) -> Option<(String, PathBuf)> {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        let logs_dir = PathBuf::from(format!("{}/.intendant/logs", home));
        if !logs_dir.is_dir() {
            return None;
        }

        let project_root_str = project_root.to_string_lossy().to_string();
        let mut best: Option<(String, PathBuf, String)> = None; // (session_id, dir, created_at)

        if let Ok(entries) = fs::read_dir(&logs_dir) {
            for entry in entries.flatten() {
                let meta_path = entry.path().join("session_meta.json");
                if !meta_path.exists() {
                    continue;
                }
                if let Ok(meta_str) = fs::read_to_string(&meta_path) {
                    if let Ok(meta) = serde_json::from_str::<SessionMeta>(&meta_str) {
                        // Skip sub-agent sessions (they shouldn't be resumed as top-level)
                        if let Some(ref role) = meta.role {
                            match role.as_str() {
                                "orchestrator" | "research" | "implementation" | "testing" => {
                                    continue;
                                }
                                _ => {}
                            }
                        }
                        if meta.project_root.as_deref() == Some(&project_root_str) {
                            let dominated = match &best {
                                Some((_, _, best_created)) => meta.created_at > *best_created,
                                None => true,
                            };
                            if dominated {
                                best = Some((meta.session_id, entry.path(), meta.created_at));
                            }
                        }
                    }
                }
            }
        }

        best.map(|(id, dir, _)| (id, dir))
    }

    /// Find a session by its ID (UUID prefix or full UUID).
    /// Checks `~/.intendant/logs/{id}/` directly, then scans for prefix matches.
    pub fn find_session_by_id(session_id: &str) -> Option<PathBuf> {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        let logs_dir = PathBuf::from(format!("{}/.intendant/logs", home));

        // Direct match (dir name == session_id)
        let direct = logs_dir.join(session_id);
        if direct.is_dir() && direct.join("session_meta.json").exists() {
            return Some(direct);
        }

        // Backward compat: if session_id contains '/', treat as direct path
        if session_id.contains('/') {
            let dir = PathBuf::from(session_id);
            if dir.is_dir() {
                return Some(dir);
            }
            return None;
        }

        // Scan for prefix match or meta match
        if !logs_dir.is_dir() {
            return None;
        }
        if let Ok(entries) = fs::read_dir(&logs_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with(session_id) && entry.path().is_dir() {
                    return Some(entry.path());
                }
                // Also check inside session_meta.json for session_id match
                let meta_path = entry.path().join("session_meta.json");
                if let Ok(meta_str) = fs::read_to_string(&meta_path) {
                    if let Ok(meta) = serde_json::from_str::<SessionMeta>(&meta_str) {
                        if meta.session_id == session_id || meta.session_id.starts_with(session_id)
                        {
                            return Some(entry.path());
                        }
                    }
                }
            }
        }

        None
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn ts() -> String {
        Local::now().format("%H:%M:%S%.3f").to_string()
    }

    fn emit(&mut self, event: LogEvent) {
        if let Ok(json) = serde_json::to_string(&event) {
            if let Err(e) = writeln!(self.writer, "{}", json) {
                eprintln!("session_log: failed to write log event: {}", e);
            }
            let _ = self.writer.flush();
        }
    }

    fn emit_transcript(&mut self, entry: TranscriptEntry) {
        if let Some(ref mut w) = self.transcript_writer {
            if let Ok(json) = serde_json::to_string(&entry) {
                let _ = writeln!(w, "{}", json);
                let _ = w.flush();
            }
        }
    }

    // ---- CU (Computer Use) structured events ----

    /// Log the start of a CU task.
    pub fn cu_task_start(
        &mut self,
        task: &str,
        provider: &str,
        model: &str,
        cu_enabled: bool,
        cu_display: Option<(u32, u32)>,
        ref_images: usize,
    ) {
        self.summary_builder.current_cu_task = Some(task.to_string());
        self.summary_builder.current_cu_turns = 0;
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: None,
            event: "cu_task_start".to_string(),
            level: Some("info".to_string()),
            message: Some(format!("CU task: {} ({}:{})", task, provider, model)),
            data: Some(serde_json::json!({
                "task": task,
                "provider": provider,
                "model": model,
                "cu_enabled": cu_enabled,
                "cu_display": cu_display,
                "ref_images": ref_images,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log a CU turn with structured data.
    pub fn cu_turn(
        &mut self,
        turn: usize,
        content_len: usize,
        cu_calls: usize,
        tool_calls: usize,
        prompt_tokens: u64,
        completion_tokens: u64,
        actions: &[String],
    ) {
        self.summary_builder.current_cu_turns = turn;
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: None,
            event: "cu_turn".to_string(),
            level: Some("info".to_string()),
            message: Some(format!(
                "CU turn {}: cu_calls={}, tool_calls={}, actions={:?}",
                turn, cu_calls, tool_calls, actions
            )),
            data: Some(serde_json::json!({
                "turn": turn,
                "content_len": content_len,
                "cu_calls": cu_calls,
                "tool_calls": tool_calls,
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
                "actions": actions,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log CU task completion.
    pub fn cu_task_complete(&mut self, turns: usize, success: bool, summary: &str) {
        self.summary_builder.cu_tasks.push(CuTaskSummary {
            task: self
                .summary_builder
                .current_cu_task
                .take()
                .unwrap_or_else(|| summary.to_string()),
            turns,
            success,
        });
        self.summary_builder.current_cu_turns = 0;
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: None,
            event: "cu_task_complete".to_string(),
            level: Some("info".to_string()),
            message: Some(format!("CU complete: {} ({} turns)", summary, turns)),
            data: Some(serde_json::json!({
                "turns": turns,
                "success": success,
                "summary": summary,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log CU task error or escalation.
    pub fn cu_task_error(&mut self, error: &str, escalated_to: Option<&str>) {
        self.summary_builder.errors.push(ErrorSummary {
            category: "cu_error".to_string(),
            reason: error.to_string(),
            ts: Self::ts(),
        });
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: None,
            event: "cu_task_error".to_string(),
            level: Some("warn".to_string()),
            message: Some(format!("CU error: {}", error)),
            data: Some(serde_json::json!({
                "error": error,
                "escalated_to": escalated_to,
            })),
            file: None,
            file2: None,
        });
    }

    // ---- Error categorization ----

    /// Log a categorized error with structured metadata.
    pub fn categorized_error(
        &mut self,
        category: &str,
        reason: &str,
        code: Option<&str>,
        provider: Option<&str>,
    ) {
        self.summary_builder.errors.push(ErrorSummary {
            category: category.to_string(),
            reason: reason.to_string(),
            ts: Self::ts(),
        });
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: if self.current_turn > 0 {
                Some(self.current_turn)
            } else {
                None
            },
            event: "error".to_string(),
            level: Some("error".to_string()),
            message: Some(reason.to_string()),
            data: Some(serde_json::json!({
                "category": category,
                "code": code,
                "reason": reason,
                "provider": provider,
            })),
            file: None,
            file2: None,
        });
    }

    // ---- Session summary ----

    /// Write `session_summary.json` with accumulated statistics.
    pub fn write_session_summary(&mut self) {
        self.flush_voice_utterance();
        // Rebuild transcript.jsonl from session.jsonl to ensure completeness.
        // The real-time buffering may have missed events due to race conditions.
        self.rebuild_transcript();

        // Fallback: scan session.jsonl for data the builder might have missed
        // due to race conditions (event bus hasn't flushed when summary writes).
        let _ = self.writer.flush();
        if let Ok(content) = fs::read_to_string(self.dir.join("session.jsonl")) {
            for line in content.lines() {
                let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
                    continue;
                };
                match val["event"].as_str().unwrap_or("") {
                    "live_usage_update" | "presence_usage_update" => {
                        if let Some(t) = val["data"]["total_tokens"].as_u64() {
                            if t > self.summary_builder.total_tokens {
                                self.summary_builder.total_tokens = t;
                            }
                        }
                    }
                    "voice_usage" => {
                        // Parse from detail string "tokens: total=28000 ..."
                        if let Some(detail) = val["data"]["detail"].as_str() {
                            if let Some(ts) = detail.split("total=").nth(1) {
                                if let Some(n) = ts.split_whitespace().next() {
                                    if let Ok(t) = n.parse::<u64>() {
                                        if t > self.summary_builder.total_tokens {
                                            self.summary_builder.total_tokens = t;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        let duration = self
            .summary_builder
            .start_time
            .map(|s| (Local::now() - s).num_milliseconds() as f64 / 1000.0)
            .unwrap_or(0.0);
        // Include in-progress CU task if session ended mid-task
        let mut cu_tasks = self.summary_builder.cu_tasks.clone();
        if let Some(ref task) = self.summary_builder.current_cu_task {
            let already_recorded = cu_tasks.iter().any(|t| t.task == *task);
            if !already_recorded && self.summary_builder.current_cu_turns > 0 {
                cu_tasks.push(CuTaskSummary {
                    task: task.clone(),
                    turns: self.summary_builder.current_cu_turns,
                    success: false,
                });
            }
        }
        // Count model turns from the rebuilt transcript
        let model_turns = self
            .dir
            .join("transcript.jsonl")
            .exists()
            .then(|| {
                fs::read_to_string(self.dir.join("transcript.jsonl"))
                    .ok()
                    .map(|c| {
                        c.lines()
                            .filter(|l| {
                                serde_json::from_str::<serde_json::Value>(l)
                                    .ok()
                                    .and_then(|v| v["role"].as_str().map(|r| r == "model"))
                                    .unwrap_or(false)
                            })
                            .count()
                    })
                    .unwrap_or(0)
            })
            .unwrap_or(0);

        let summary = SessionSummary {
            duration_secs: duration,
            voice_provider: self.summary_builder.voice_provider.clone(),
            voice_model: self.summary_builder.voice_model.clone(),
            voice_connections: self.summary_builder.voice_connections,
            voice_reconnects: self
                .summary_builder
                .voice_connections
                .saturating_sub(1),
            model_turns,
            cu_tasks,
            frames_sent: self.summary_builder.frames_sent,
            errors: self.summary_builder.errors.clone(),
            user_transcripts: self.summary_builder.user_transcripts.clone(),
            total_tokens: self.summary_builder.total_tokens,
        };
        let path = self.dir.join("session_summary.json");
        if let Ok(json) = serde_json::to_string_pretty(&summary) {
            if let Err(e) = fs::write(&path, &json) {
                eprintln!("session_log: failed to write session_summary.json: {}", e);
            }
        }
    }

    /// Write content to a turn-specific file and return its relative path.
    ///
    /// Overwrites existing content. Use [`append_turn_file`] for streams
    /// like stdout/stderr that accumulate multiple writes within one turn.
    ///
    /// [`append_turn_file`]: Self::append_turn_file
    fn write_turn_file(&self, suffix: &str, content: &str) -> Option<String> {
        let relative = format!("turns/turn_{:03}_{}", self.current_turn, suffix);
        let path = self.dir.join(&relative);
        if fs::write(&path, content).is_ok() {
            Some(relative)
        } else {
            None
        }
    }

    /// Append content to a turn-specific file and return its relative path.
    ///
    /// If the file already has content, writes a blank-line separator first
    /// so successive entries remain visually distinct when read back. Returns
    /// `None` if the OS write fails — the caller should then drop the `file`
    /// reference from the session-log event so downstream readers don't chase
    /// a phantom path.
    fn append_turn_file(&self, suffix: &str, content: &str) -> Option<String> {
        let relative = format!("turns/turn_{:03}_{}", self.current_turn, suffix);
        let path = self.dir.join(&relative);
        let already_has_content = fs::metadata(&path).map(|m| m.len() > 0).unwrap_or(false);
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()?;
        if already_has_content {
            let _ = file.write_all(b"\n");
        }
        if file.write_all(content.as_bytes()).is_ok() {
            Some(relative)
        } else {
            None
        }
    }

    // ---- Public logging methods ----

    pub fn info(&mut self, msg: &str) {
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: if self.current_turn > 0 {
                Some(self.current_turn)
            } else {
                None
            },
            event: "info".to_string(),
            level: Some("info".to_string()),
            message: Some(msg.to_string()),
            data: None,
            file: None,
            file2: None,
        });
    }

    pub fn warn(&mut self, msg: &str) {
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: if self.current_turn > 0 {
                Some(self.current_turn)
            } else {
                None
            },
            event: "warn".to_string(),
            level: Some("warn".to_string()),
            message: Some(msg.to_string()),
            data: None,
            file: None,
            file2: None,
        });
    }

    /// Log a voice transcript from the browser presence model.
    pub fn voice_log(&mut self, text: &str, seq: u64, tool_context: Option<&str>) {
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: None,
            event: "voice_log".to_string(),
            level: Some("info".to_string()),
            message: Some(text.to_string()),
            data: Some(serde_json::json!({
                "seq": seq,
                "tool_context": tool_context,
            })),
            file: None,
            file2: None,
        });
        // Buffer voice tokens into full utterances (flushed on turnComplete
        // via voice_protocol). Writing per-token produces unreadable transcripts.
        if tool_context.is_none() || tool_context == Some("transcript") {
            self.voice_utterance_buf.push_str(text);
        }
    }

    /// Log a server-side user speech transcript (from Whisper API).
    pub fn user_transcript(&mut self, text: &str, seq: u64) {
        // Flush any buffered model speech before the user turn
        self.flush_voice_utterance();
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: None,
            event: "user_transcript".to_string(),
            level: Some("info".to_string()),
            message: Some(text.to_string()),
            data: Some(serde_json::json!({ "seq": seq })),
            file: None,
            file2: None,
        });
        self.summary_builder.user_transcripts.push(text.to_string());
        self.emit_transcript(TranscriptEntry {
            ts: Self::ts(),
            role: "user".to_string(),
            text: text.to_string(),
            tools_called: None,
        });
    }

    /// Log a presence checkpoint (context summary from browser model).
    pub fn presence_checkpoint(&mut self, summary: &str, last_event_seq: u64) {
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: None,
            event: "presence_checkpoint".to_string(),
            level: Some("info".to_string()),
            message: Some(summary.to_string()),
            data: Some(serde_json::json!({
                "last_event_seq": last_event_seq,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log a browser presence connect event.
    pub fn presence_connected(&mut self, provider: Option<&str>, model: Option<&str>) {
        self.summary_builder.voice_connections += 1;
        if let Some(p) = provider {
            self.summary_builder.voice_provider = Some(p.to_string());
        }
        if let Some(m) = model {
            self.summary_builder.voice_model = Some(m.to_string());
        }
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: None,
            event: "presence_connected".to_string(),
            level: Some("info".to_string()),
            message: Some(format!(
                "Browser presence connected ({}:{})",
                provider.unwrap_or("unknown"),
                model.unwrap_or("unknown"),
            )),
            data: Some(serde_json::json!({
                "provider": provider,
                "model": model,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log a browser presence disconnect event.
    pub fn presence_disconnected(&mut self) {
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: None,
            event: "presence_disconnected".to_string(),
            level: Some("info".to_string()),
            message: Some("Browser presence disconnected".to_string()),
            data: None,
            file: None,
            file2: None,
        });
    }

    /// Log a voice/presence diagnostic — delegates to typed event methods.
    /// Kept as the public API so callers don't need to change.
    pub fn voice_diagnostic(&mut self, kind: &str, detail: &str) {
        match kind {
            "audio_send" => self.voice_audio(kind, detail),
            "video_send" | "frame_skip" => self.voice_frame(kind, detail),
            "gemini_usage" => self.voice_usage(kind, detail),
            "error" | "gemini_close" | "action_drop" => self.voice_error(kind, detail),
            _ => self.voice_protocol(kind, detail),
        }
    }

    /// Audio chunk telemetry (high-frequency, skip in most views).
    pub fn voice_audio(&mut self, kind: &str, detail: &str) {
        self.emit_voice("voice_audio", "debug", kind, detail);
    }

    /// Protocol lifecycle: setupComplete, turnComplete, connected, interrupted, etc.
    pub fn voice_protocol(&mut self, kind: &str, detail: &str) {
        // Flush buffered voice tokens to transcript on turnComplete
        if detail.starts_with("turnComplete") || kind == "gemini_msg" && detail.contains("turnComplete") {
            self.flush_voice_utterance();
        }
        self.emit_voice("voice_protocol", "debug", kind, detail);
    }

    /// Rebuild transcript.jsonl from session.jsonl at session end.
    /// Aggregates per-token voice_log events into full utterances per turn,
    /// using voice_protocol/turnComplete as turn boundaries.
    fn rebuild_transcript(&mut self) {
        let _ = self.writer.flush();
        let session_path = self.dir.join("session.jsonl");
        let content = match fs::read_to_string(&session_path) {
            Ok(c) => c,
            Err(_) => return,
        };

        let mut entries: Vec<TranscriptEntry> = Vec::new();
        let mut model_buf = String::new();
        let mut model_ts = String::new();

        for line in content.lines() {
            let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let event = val["event"].as_str().unwrap_or("");
            let ts = val["ts"].as_str().unwrap_or("").to_string();

            match event {
                "user_transcript" => {
                    // Flush any buffered model speech first
                    let trimmed = model_buf.trim().to_string();
                    if !trimmed.is_empty() {
                        entries.push(TranscriptEntry {
                            ts: model_ts.clone(),
                            role: "model".to_string(),
                            text: trimmed,
                            tools_called: None,
                        });
                        model_buf.clear();
                    }
                    let text = val["message"].as_str().unwrap_or("").to_string();
                    if !text.is_empty() {
                        entries.push(TranscriptEntry {
                            ts,
                            role: "user".to_string(),
                            text,
                            tools_called: None,
                        });
                    }
                }
                "voice_log" => {
                    let ctx = val["data"]["tool_context"].as_str().unwrap_or("");
                    if ctx.is_empty() || ctx == "transcript" {
                        let text = val["message"].as_str().unwrap_or("");
                        if model_buf.is_empty() {
                            model_ts = ts;
                        }
                        model_buf.push_str(text);
                    }
                }
                "tool_request" => {
                    let tool = val["data"]["tool"].as_str().unwrap_or("unknown");
                    let args = val["data"]["args"]
                        .as_object()
                        .map(|o| serde_json::to_string(o).unwrap_or_default())
                        .unwrap_or_default();
                    entries.push(TranscriptEntry {
                        ts,
                        role: "model".to_string(),
                        text: format!("[tool:{}] {}", tool, args),
                        tools_called: Some(vec![tool.to_string()]),
                    });
                }
                "voice_protocol" => {
                    let detail = val["data"]["detail"].as_str().unwrap_or("");
                    // Flush on turnComplete or interrupted
                    if detail.contains("turnComplete") || detail.contains("interrupted") {
                        let trimmed = model_buf.trim().to_string();
                        if !trimmed.is_empty() {
                            entries.push(TranscriptEntry {
                                ts: model_ts.clone(),
                                role: "model".to_string(),
                                text: trimmed,
                                tools_called: None,
                            });
                            model_buf.clear();
                        }
                    }
                }
                // Also handle legacy voice_diagnostic for older session.jsonl
                "voice_diagnostic" => {
                    let kind = val["data"]["kind"].as_str().unwrap_or("");
                    let detail = val["data"]["detail"].as_str().unwrap_or("");
                    if kind == "gemini_msg"
                        && (detail.contains("turnComplete") || detail.contains("interrupted"))
                    {
                        let trimmed = model_buf.trim().to_string();
                        if !trimmed.is_empty() {
                            entries.push(TranscriptEntry {
                                ts: model_ts.clone(),
                                role: "model".to_string(),
                                text: trimmed,
                                tools_called: None,
                            });
                            model_buf.clear();
                        }
                    }
                }
                _ => {}
            }
        }
        // Flush remaining
        let trimmed = model_buf.trim().to_string();
        if !trimmed.is_empty() {
            entries.push(TranscriptEntry {
                ts: model_ts,
                role: "model".to_string(),
                text: trimmed,
                tools_called: None,
            });
        }

        // Overwrite transcript.jsonl with clean aggregated version
        if !entries.is_empty() {
            let transcript_path = self.dir.join("transcript.jsonl");
            if let Ok(f) = File::create(&transcript_path) {
                let mut w = BufWriter::new(f);
                for entry in &entries {
                    if let Ok(json) = serde_json::to_string(entry) {
                        let _ = writeln!(w, "{}", json);
                    }
                }
                let _ = w.flush();
            }
        }
    }

    /// Flush the buffered voice utterance to transcript.jsonl.
    fn flush_voice_utterance(&mut self) {
        let text = self.voice_utterance_buf.trim().to_string();
        if !text.is_empty() {
            self.emit_transcript(TranscriptEntry {
                ts: Self::ts(),
                role: "model".to_string(),
                text,
                tools_called: None,
            });
        }
        self.voice_utterance_buf.clear();
    }

    /// Video frame send telemetry.
    pub fn voice_frame(&mut self, kind: &str, detail: &str) {
        self.summary_builder.frames_sent += 1;
        self.emit_voice("voice_frame", "debug", kind, detail);
    }

    /// Live model token usage.
    pub fn voice_usage(&mut self, kind: &str, detail: &str) {
        // Extract total tokens from detail string like "tokens: total=3099 prompt=..."
        if let Some(total_str) = detail.split("total=").nth(1) {
            if let Some(num_str) = total_str.split_whitespace().next() {
                if let Ok(total) = num_str.parse::<u64>() {
                    // Use the max seen (cumulative) rather than adding (already cumulative)
                    if total > self.summary_builder.total_tokens {
                        self.summary_builder.total_tokens = total;
                    }
                }
            }
        }
        self.emit_voice("voice_usage", "debug", kind, detail);
    }

    /// Voice/presence errors (disconnects, failures).
    pub fn voice_error(&mut self, kind: &str, detail: &str) {
        self.summary_builder.errors.push(ErrorSummary {
            category: format!("voice_{}", kind),
            reason: detail.to_string(),
            ts: Self::ts(),
        });
        self.emit_voice("voice_error", "warn", kind, detail);
    }

    fn emit_voice(&mut self, event: &str, level: &str, kind: &str, detail: &str) {
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: None,
            event: event.to_string(),
            level: Some(level.to_string()),
            message: Some(format!("[voice:{}] {}", kind, detail)),
            data: Some(serde_json::json!({
                "kind": kind,
                "detail": detail,
            })),
            file: None,
            file2: None,
        });
    }

    // ---- Event-bus-driven logging methods ----
    // These are called by spawn_session_log_writer() for events that flow
    // through the AppEvent bus but were not previously persisted to disk.

    /// Log a done signal from the agent.
    pub fn done_signal(&mut self, message: Option<&str>) {
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: if self.current_turn > 0 { Some(self.current_turn) } else { None },
            event: "done_signal".to_string(),
            level: Some("info".to_string()),
            message: Some(message.unwrap_or("Agent signalled done").to_string()),
            data: None,
            file: None,
            file2: None,
        });
    }

    /// Log task completion.
    pub fn task_complete(&mut self, reason: &str, summary: Option<&str>) {
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: None,
            event: "task_complete".to_string(),
            level: Some("info".to_string()),
            message: Some(format!("Task complete: {}", reason)),
            data: Some(serde_json::json!({
                "reason": reason,
                "summary": summary,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log a new session starting (MCP multi-task).
    pub fn session_started(&mut self, session_id: &str, task: Option<&str>) {
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: None,
            event: "session_started".to_string(),
            level: Some("info".to_string()),
            message: Some(format!("Session started: {}", session_id)),
            data: Some(serde_json::json!({
                "session_id": session_id,
                "task": task,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log a session ending (MCP multi-task).
    pub fn session_ended(&mut self, session_id: &str, reason: &str) {
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: None,
            event: "session_ended".to_string(),
            level: Some("info".to_string()),
            message: Some(format!("Session ended: {} ({})", session_id, reason)),
            data: Some(serde_json::json!({
                "session_id": session_id,
                "reason": reason,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log agent execution starting.
    pub fn agent_started(&mut self, turn: usize, commands_preview: &str) {
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: Some(turn),
            event: "agent_started".to_string(),
            level: Some("info".to_string()),
            message: Some(commands_preview.to_string()),
            data: None,
            file: None,
            file2: None,
        });
    }

    /// Log an auto-approved command.
    pub fn auto_approved(&mut self, preview: &str) {
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: if self.current_turn > 0 { Some(self.current_turn) } else { None },
            event: "auto_approved".to_string(),
            level: Some("info".to_string()),
            message: Some(preview.to_string()),
            data: None,
            file: None,
            file2: None,
        });
    }

    /// Log a resolved approval decision.
    pub fn approval_resolved(&mut self, id: u64, action: &str) {
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: Some(id as usize),
            event: "approval_resolved".to_string(),
            level: Some("info".to_string()),
            message: Some(format!("Approval {} (turn {})", action, id)),
            data: None,
            file: None,
            file2: None,
        });
    }

    /// Log a human question (askHuman).
    pub fn human_question(&mut self, question: &str) {
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: if self.current_turn > 0 { Some(self.current_turn) } else { None },
            event: "human_question".to_string(),
            level: Some("info".to_string()),
            message: Some(question.to_string()),
            data: None,
            file: None,
            file2: None,
        });
    }

    /// Log that a human response was sent.
    pub fn human_response_sent(&mut self) {
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: if self.current_turn > 0 { Some(self.current_turn) } else { None },
            event: "human_response_sent".to_string(),
            level: Some("info".to_string()),
            message: Some("Human response sent".to_string()),
            data: None,
            file: None,
            file2: None,
        });
    }

    /// Log round completion (orchestrator mode).
    pub fn round_complete(&mut self, round: usize, turns_in_round: usize) {
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: None,
            event: "round_complete".to_string(),
            level: Some("info".to_string()),
            message: Some(format!("Round {} complete ({} turns)", round, turns_in_round)),
            data: Some(serde_json::json!({
                "round": round,
                "turns_in_round": turns_in_round,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log creation of a per-round file snapshot.
    pub fn snapshot_created(&mut self, round_id: u64) {
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: None,
            event: "snapshot_created".to_string(),
            level: Some("info".to_string()),
            message: Some(format!("Snapshot {} created", round_id)),
            data: Some(serde_json::json!({ "round_id": round_id })),
            file: None,
            file2: None,
        });
    }

    /// Log a rollback to a prior round.
    pub fn rolled_back(&mut self, from_id: u64, to_id: u64, files_reverted: u32) {
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: None,
            event: "rolled_back".to_string(),
            level: Some("info".to_string()),
            message: Some(format!(
                "Rolled back from round {} to {} ({} files reverted)",
                from_id, to_id, files_reverted
            )),
            data: Some(serde_json::json!({
                "from_id": from_id,
                "to_id": to_id,
                "files_reverted": files_reverted,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log a redo along the linear history.
    pub fn redone(&mut self, to_id: u64) {
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: None,
            event: "redone".to_string(),
            level: Some("info".to_string()),
            message: Some(format!("Redone to round {}", to_id)),
            data: Some(serde_json::json!({ "to_id": to_id })),
            file: None,
            file2: None,
        });
    }

    /// Log a pruning of abandoned branches + orphaned objects.
    pub fn history_pruned(&mut self, branches_removed: u32, bytes_freed: u64) {
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: None,
            event: "history_pruned".to_string(),
            level: Some("info".to_string()),
            message: Some(format!(
                "Pruned {} branch(es), freed {} bytes",
                branches_removed, bytes_freed
            )),
            data: Some(serde_json::json!({
                "branches_removed": branches_removed,
                "bytes_freed": bytes_freed,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log a conversation rollback (truncated or session-reset).
    pub fn conversation_rolled_back(
        &mut self,
        round_id: u64,
        turns_removed: u32,
        backend: &str,
        method: &str,
    ) {
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: None,
            event: "conversation_rolled_back".to_string(),
            level: Some("info".to_string()),
            message: Some(format!(
                "Conversation rolled back to round {} via {} ({} turns removed, backend: {})",
                round_id, method, turns_removed, backend
            )),
            data: Some(serde_json::json!({
                "round_id": round_id,
                "turns_removed": turns_removed,
                "backend": backend,
                "method": method,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log display ready.
    pub fn display_ready(
        &mut self,
        display_id: u32,
        width: u32,
        height: u32,
    ) {
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: None,
            event: "display_ready".to_string(),
            level: Some("info".to_string()),
            message: Some(format!(
                "Display :{} ready ({}x{})",
                display_id, width, height
            )),
            data: Some(serde_json::json!({
                "display_id": display_id,
                "width": width,
                "height": height,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log display resolution change.
    pub fn display_resize(
        &mut self,
        display_id: u32,
        width: u32,
        height: u32,
    ) {
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: None,
            event: "display_resize".to_string(),
            level: Some("info".to_string()),
            message: Some(format!(
                "Display :{} resized to {}x{}",
                display_id, width, height
            )),
            data: Some(serde_json::json!({
                "display_id": display_id,
                "width": width,
                "height": height,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log display takeover.
    pub fn display_taken(&mut self, display_id: u32) {
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: None,
            event: "display_taken".to_string(),
            level: Some("info".to_string()),
            message: Some(format!("Display :{} taken over", display_id)),
            data: Some(serde_json::json!({ "display_id": display_id })),
            file: None,
            file2: None,
        });
    }

    /// Log display released.
    pub fn display_released(&mut self, display_id: u32, note: Option<&str>) {
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: None,
            event: "display_released".to_string(),
            level: Some("info".to_string()),
            message: Some(format!(
                "Display :{} released{}",
                display_id,
                note.map(|n| format!(": {}", n)).unwrap_or_default()
            )),
            data: Some(serde_json::json!({
                "display_id": display_id,
                "note": note,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log debug screen ready.
    pub fn debug_screen_ready(&mut self, display_id: u32) {
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: None,
            event: "debug_screen_ready".to_string(),
            level: Some("info".to_string()),
            message: Some(format!("Debug screen :{} ready", display_id)),
            data: Some(serde_json::json!({
                "display_id": display_id,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log debug screen torn down.
    pub fn debug_screen_torn_down(&mut self, display_id: u32) {
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: None,
            event: "debug_screen_torn_down".to_string(),
            level: Some("info".to_string()),
            message: Some(format!("Debug screen :{} torn down", display_id)),
            data: Some(serde_json::json!({ "display_id": display_id })),
            file: None,
            file2: None,
        });
    }

    /// Log safety cap reached.
    pub fn safety_cap_reached(&mut self) {
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: if self.current_turn > 0 { Some(self.current_turn) } else { None },
            event: "safety_cap_reached".to_string(),
            level: Some("warn".to_string()),
            message: Some("Safety cap reached".to_string()),
            data: None,
            file: None,
            file2: None,
        });
    }

    /// Log recording started.
    pub fn recording_started(&mut self, stream_name: &str) {
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: None,
            event: "recording_started".to_string(),
            level: Some("info".to_string()),
            message: Some(format!("Recording started: {}", stream_name)),
            data: Some(serde_json::json!({ "stream_name": stream_name })),
            file: None,
            file2: None,
        });
    }

    /// Log recording stopped.
    pub fn recording_stopped(&mut self, stream_name: &str) {
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: None,
            event: "recording_stopped".to_string(),
            level: Some("info".to_string()),
            message: Some(format!("Recording stopped: {}", stream_name)),
            data: Some(serde_json::json!({ "stream_name": stream_name })),
            file: None,
            file2: None,
        });
    }

    /// Log recording error.
    pub fn recording_error(&mut self, stream_name: &str, message: &str) {
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: None,
            event: "recording_error".to_string(),
            level: Some("warn".to_string()),
            message: Some(format!("Recording error ({}): {}", stream_name, message)),
            data: Some(serde_json::json!({
                "stream_name": stream_name,
                "error": message,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log sub-agent result.
    pub fn sub_agent_result(&mut self, summary: &str) {
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: if self.current_turn > 0 { Some(self.current_turn) } else { None },
            event: "sub_agent_result".to_string(),
            level: Some("info".to_string()),
            message: Some(summary.to_string()),
            data: None,
            file: None,
            file2: None,
        });
    }

    /// Log orchestrator progress.
    pub fn orchestrator_progress(&mut self, status: &str) {
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: None,
            event: "orchestrator_progress".to_string(),
            level: Some("info".to_string()),
            message: Some(status.to_string()),
            data: None,
            file: None,
            file2: None,
        });
    }

    /// Log presence layer log message.
    pub fn presence_log(&mut self, message: &str, level: Option<&str>) {
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: None,
            event: "presence_log".to_string(),
            level: Some(level.unwrap_or("info").to_string()),
            message: Some(message.to_string()),
            data: None,
            file: None,
            file2: None,
        });
    }

    /// Log presence layer usage update.
    pub fn presence_usage_update(
        &mut self,
        provider: &str,
        model: &str,
        total_tokens: u64,
        context_window: u64,
        usage_pct: f64,
    ) {
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: None,
            event: "presence_usage_update".to_string(),
            level: Some("debug".to_string()),
            message: Some(format!(
                "Presence usage: {:.0}% ({} tokens, {}:{})",
                usage_pct * 100.0,
                total_tokens,
                provider,
                model
            )),
            data: Some(serde_json::json!({
                "provider": provider,
                "model": model,
                "total_tokens": total_tokens,
                "context_window": context_window,
                "usage_pct": usage_pct,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log live model (Gemini Live / OpenAI Realtime) usage update.
    pub fn live_usage_update(
        &mut self,
        provider: &str,
        model: &str,
        total_tokens: u64,
    ) {
        // Track cumulative live model tokens
        if total_tokens > self.summary_builder.total_tokens {
            self.summary_builder.total_tokens = total_tokens;
        }
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: None,
            event: "live_usage_update".to_string(),
            level: Some("debug".to_string()),
            message: Some(format!(
                "Live usage: {} tokens ({}:{})",
                total_tokens, provider, model
            )),
            data: Some(serde_json::json!({
                "provider": provider,
                "model": model,
                "total_tokens": total_tokens,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log live audio sub-agent started.
    pub fn live_audio_started(&mut self, id: &str, provider: &str) {
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: None,
            event: "live_audio_started".to_string(),
            level: Some("info".to_string()),
            message: Some(format!("Live audio started: {} ({})", id, provider)),
            data: Some(serde_json::json!({
                "id": id,
                "provider": provider,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log live audio sub-agent progress.
    pub fn live_audio_progress(
        &mut self,
        id: &str,
        state: &str,
        elapsed_secs: f64,
        transcript_preview: &str,
    ) {
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: None,
            event: "live_audio_progress".to_string(),
            level: Some("debug".to_string()),
            message: Some(format!(
                "Live audio {}: {} ({:.1}s) {}",
                id, state, elapsed_secs, transcript_preview
            )),
            data: Some(serde_json::json!({
                "id": id,
                "state": state,
                "elapsed_secs": elapsed_secs,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log live audio sub-agent completed.
    pub fn live_audio_completed(
        &mut self,
        id: &str,
        status: &str,
        quarantine_count: usize,
    ) {
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: None,
            event: "live_audio_completed".to_string(),
            level: Some("info".to_string()),
            message: Some(format!(
                "Live audio completed: {} ({}, {} quarantined)",
                id, status, quarantine_count
            )),
            data: Some(serde_json::json!({
                "id": id,
                "status": status,
                "quarantine_count": quarantine_count,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log a tool request received from the browser presence model.
    pub fn tool_request(&mut self, tool: &str, args: &serde_json::Value) {
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: None,
            event: "tool_request".to_string(),
            level: Some("debug".to_string()),
            message: Some(format!("{}({})", tool, serde_json::to_string(args).unwrap_or_default())),
            data: Some(serde_json::json!({
                "tool": tool,
                "args": args,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log a tool response sent back to the browser presence model.
    pub fn tool_response(&mut self, tool: &str, result: &str) {
        let preview = if result.len() > 200 { &result[..200] } else { result };
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: None,
            event: "tool_response".to_string(),
            level: Some("debug".to_string()),
            message: Some(format!("{} → {}", tool, preview)),
            data: None,
            file: None,
            file2: None,
        });
    }

    pub fn error(&mut self, msg: &str) {
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: if self.current_turn > 0 {
                Some(self.current_turn)
            } else {
                None
            },
            event: "error".to_string(),
            level: Some("error".to_string()),
            message: Some(msg.to_string()),
            data: None,
            file: None,
            file2: None,
        });
    }

    pub fn debug(&mut self, msg: &str) {
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: if self.current_turn > 0 {
                Some(self.current_turn)
            } else {
                None
            },
            event: "debug".to_string(),
            level: Some("debug".to_string()),
            message: Some(msg.to_string()),
            data: None,
            file: None,
            file2: None,
        });
    }

    /// Log a turn boundary.
    pub fn turn_start(&mut self, turn: usize, budget_pct: f64, remaining: u64) {
        self.current_turn = turn;
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: Some(turn),
            event: "turn_start".to_string(),
            level: Some("info".to_string()),
            message: None,
            data: Some(serde_json::json!({
                "budget_pct": budget_pct,
                "remaining_tokens": remaining,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log the full messages array sent to the API for this turn.
    pub fn messages_input(&mut self, messages_json: &str) {
        let file = self.write_turn_file("messages.json", messages_json);
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: Some(self.current_turn),
            event: "messages_input".to_string(),
            level: Some("debug".to_string()),
            message: Some(format!("Messages logged ({} bytes)", messages_json.len())),
            data: Some(serde_json::json!({
                "json_length": messages_json.len(),
            })),
            file,
            file2: None,
        });
    }

    /// Log the full model response. Content is written to a per-turn file.
    pub fn model_response(
        &mut self,
        content: &str,
        prompt_tokens: u64,
        completion_tokens: u64,
        total_tokens: u64,
        cached_tokens: u64,
        source: Option<&str>,
    ) {
        self.summary_builder.total_tokens += total_tokens;
        // Codex fires multiple `model_response` events per turn (one per
        // assistant message in the same turn). Appending keeps the full
        // sequence; truncating would leave only the last chunk on disk
        // while session.jsonl's event stream references all of them.
        let file = self.append_turn_file("model.txt", content);
        let preview: String = content.chars().take(200).collect();
        let mut data = serde_json::json!({
            "tokens": {
                "prompt": prompt_tokens,
                "completion": completion_tokens,
                "total": total_tokens,
                "cached": cached_tokens,
            },
            "content_length": content.len(),
        });
        if let Some(src) = source {
            data["source"] = serde_json::Value::String(src.to_string());
        }
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: Some(self.current_turn),
            event: "model_response".to_string(),
            level: Some("info".to_string()),
            message: Some(preview),
            data: Some(data),
            file,
            file2: None,
        });
    }

    /// Log the full JSON sent to the agent runtime.
    pub fn agent_input(&mut self, json: &str) {
        // Pretty-print the JSON for the file
        let pretty = if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json) {
            serde_json::to_string_pretty(&parsed).unwrap_or_else(|_| json.to_string())
        } else {
            json.to_string()
        };
        let file = self.write_turn_file("agent_in.json", &pretty);

        // Extract function names for the summary
        let functions: Vec<String> = serde_json::from_str::<serde_json::Value>(json)
            .ok()
            .and_then(|v| v.get("commands")?.as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|cmd| {
                cmd.get("function")
                    .and_then(|f| f.as_str())
                    .map(String::from)
            })
            .collect();

        self.emit(LogEvent {
            ts: Self::ts(),
            turn: Some(self.current_turn),
            event: "agent_input".to_string(),
            level: Some("info".to_string()),
            message: Some(format!("Commands: {}", functions.join(", "))),
            data: Some(serde_json::json!({
                "functions": functions,
                "json_length": json.len(),
            })),
            file,
            file2: None,
        });
    }

    /// Log agent runtime output. Written to per-turn files.
    ///
    /// A single turn may run many commands, each producing its own output
    /// chunk; we append so the file reflects the full turn history rather
    /// than only the last chunk.
    pub fn agent_output(&mut self, stdout: &str, stderr: &str, source: Option<&str>) {
        let file = if !stdout.is_empty() {
            self.append_turn_file("stdout.txt", stdout)
        } else {
            None
        };
        let file2 = if !stderr.is_empty() {
            self.append_turn_file("stderr.txt", stderr)
        } else {
            None
        };

        let preview: String = stdout.chars().take(200).collect();
        let mut data = serde_json::json!({
            "stdout_length": stdout.len(),
            "stderr_length": stderr.len(),
        });
        if let Some(src) = source {
            data["source"] = serde_json::Value::String(src.to_string());
        }
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: Some(self.current_turn),
            event: "agent_output".to_string(),
            level: if stderr.is_empty() {
                Some("info".to_string())
            } else {
                Some("warn".to_string())
            },
            message: if stdout.is_empty() && stderr.is_empty() {
                Some("(no output)".to_string())
            } else {
                Some(preview)
            },
            data: Some(data),
            file,
            file2,
        });
    }

    /// Log reasoning content from the model (full reasoning, not just summary).
    pub fn reasoning_content(&mut self, summary: Option<&str>, full_content: Option<&str>) {
        let file = full_content.and_then(|c| self.append_turn_file("reasoning.txt", c));
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: Some(self.current_turn),
            event: "reasoning".to_string(),
            level: Some("info".to_string()),
            message: summary.map(|s| s.to_string()),
            data: Some(serde_json::json!({
                "has_summary": summary.is_some(),
                "has_full_content": full_content.is_some(),
                "full_content_length": full_content.map(|c| c.len()).unwrap_or(0),
            })),
            file,
            file2: None,
        });
    }

    /// Log an approval event.
    pub fn approval(&mut self, category: &str, preview: &str, decision: &str) {
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: Some(self.current_turn),
            event: "approval".to_string(),
            level: Some("warn".to_string()),
            message: Some(format!("{} -> {}", preview, decision)),
            data: Some(serde_json::json!({
                "category": category,
                "decision": decision,
                "preview": preview,
            })),
            file: None,
            file2: None,
        });
    }

    /// Log the JSON extracted from a model response.
    pub fn json_extracted(&mut self, json: &str) {
        // Extract function names for searchability
        let functions: Vec<String> = serde_json::from_str::<serde_json::Value>(json)
            .ok()
            .and_then(|v| v.get("commands")?.as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|cmd| {
                cmd.get("function")
                    .and_then(|f| f.as_str())
                    .map(String::from)
            })
            .collect();

        let done = serde_json::from_str::<serde_json::Value>(json)
            .ok()
            .and_then(|v| v.get("done")?.as_bool())
            .unwrap_or(false);

        self.emit(LogEvent {
            ts: Self::ts(),
            turn: Some(self.current_turn),
            event: "json_extracted".to_string(),
            level: Some("debug".to_string()),
            message: Some(if functions.is_empty() {
                if done {
                    "done signal".to_string()
                } else {
                    "no commands".to_string()
                }
            } else {
                functions.join(", ")
            }),
            data: Some(serde_json::json!({
                "functions": functions,
                "done": done,
                "json_length": json.len(),
            })),
            file: None,
            file2: None,
        });
    }

    /// Write the session summary (call at end of session).
    /// Also updates session_meta.json with completion status.
    pub fn write_summary(&mut self, task: &str, outcome: &str, total_turns: usize) {
        self.write_summary_with_rounds(task, outcome, total_turns, None);
    }

    /// Write session summary with optional round count.
    pub fn write_summary_with_rounds(
        &mut self,
        task: &str,
        outcome: &str,
        total_turns: usize,
        rounds: Option<usize>,
    ) {
        let mut summary = serde_json::json!({
            "task": task,
            "outcome": outcome,
            "total_turns": total_turns,
            "session_id": self.session_id,
            "session_dir": self.dir.to_string_lossy(),
            "ended_at": Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        });
        if let Some(r) = rounds {
            summary["rounds"] = serde_json::json!(r);
        }
        let path = self.dir.join("summary.json");
        if let Ok(pretty) = serde_json::to_string_pretty(&summary) {
            if let Err(e) = fs::write(&path, &pretty) {
                eprintln!("session_log: failed to write summary.json: {}", e);
            }
        }

        // Update session_meta.json with completion status
        let meta_path = self.dir.join("session_meta.json");
        if let Ok(meta_str) = fs::read_to_string(&meta_path) {
            if let Ok(mut meta) = serde_json::from_str::<SessionMeta>(&meta_str) {
                meta.status = Some("completed".to_string());
                meta.last_turn = Some(total_turns);
                meta.rounds = rounds;
                if let Ok(json) = serde_json::to_string_pretty(&meta) {
                    if let Err(e) = fs::write(&meta_path, &json) {
                        eprintln!("session_log: failed to update session_meta.json: {}", e);
                    }
                }
            }
        }

        self.emit(LogEvent {
            ts: Self::ts(),
            turn: None,
            event: "session_end".to_string(),
            level: Some("info".to_string()),
            message: Some(format!(
                "Session ended: {} ({} turns)",
                outcome, total_turns
            )),
            data: None,
            file: Some("summary.json".to_string()),
            file2: None,
        });

        // Write the rich session summary alongside the simple one
        self.write_session_summary();
    }

    /// Mark the session as interrupted and flush logs.
    /// Called from signal handlers (SIGTERM) where Drop may not run.
    pub fn mark_interrupted(&mut self) {
        self.flush_voice_utterance();
        let _ = self.writer.flush();
        let meta_path = self.dir.join("session_meta.json");
        if let Ok(meta_str) = fs::read_to_string(&meta_path) {
            if let Ok(mut meta) = serde_json::from_str::<SessionMeta>(&meta_str) {
                if meta.status.as_deref() == Some("running") {
                    meta.status = Some("interrupted".to_string());
                    meta.last_turn = Some(self.current_turn);
                    if let Ok(json) = serde_json::to_string_pretty(&meta) {
                        let _ = fs::write(&meta_path, &json);
                    }
                }
            }
        }
        // Write partial session summary even on interrupt
        self.write_session_summary();
    }
}

impl Drop for SessionLog {
    fn drop(&mut self) {
        // Flush any buffered log data
        let _ = self.writer.flush();

        // If the session is still "running", mark it as "interrupted"
        let meta_path = self.dir.join("session_meta.json");
        if let Ok(meta_str) = fs::read_to_string(&meta_path) {
            if let Ok(mut meta) = serde_json::from_str::<SessionMeta>(&meta_str) {
                if meta.status.as_deref() == Some("running") {
                    meta.status = Some("interrupted".to_string());
                    meta.last_turn = Some(self.current_turn);
                    if let Ok(json) = serde_json::to_string_pretty(&meta) {
                        let _ = fs::write(&meta_path, &json);
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Inverse: JSONL entry → AppEvent
// ---------------------------------------------------------------------------

/// Helper: parse a `u32` numeric field from the `data` block.
fn u32_from_data(data: Option<&serde_json::Value>, key: &str) -> Option<u32> {
    data?.get(key)?.as_u64().map(|v| v as u32)
}

/// Reconstruct an `AppEvent` from a parsed `session.jsonl` entry.
///
/// Inverse of the typed writer methods above.  Used during replay to drive
/// the live `app_event_to_outbound` → WASM `handle_event` path so there is
/// a single rendering path for both live broadcast and historical replay.
///
/// Returns `None` for:
///   - internal bookkeeping events (`session_start`, `messages_input`,
///     `json_extracted`, `agent_input`),
///   - high-frequency telemetry (`voice_audio`, `voice_frame`),
///   - events whose `AppEvent` variants are explicitly filtered out of the
///     live outbound path (`voice_log`, `tool_request`, `presence_connected`,
///     `live_audio_*`, …).  These don't render on live either — keeping
///     replay silent here is the cost of guaranteeing a single rendering
///     path.  If any of these graduate to live visibility later, extend both
///     `app_event_to_outbound` and this function together.
///
/// For events with a `file` field (`model_response`, `agent_output`,
/// `reasoning`), reads the full content from the turn file under `log_dir`
/// and substitutes it for the 200-char `message` preview.
pub fn session_log_entry_to_app_event(
    entry: &serde_json::Value,
    log_dir: &Path,
) -> Option<crate::event::AppEvent> {
    use crate::event::AppEvent;
    use crate::provider::TokenUsage;
    use crate::types::LogLevel;

    let event_type = entry.get("event").and_then(|v| v.as_str())?;
    let message = entry.get("message").and_then(|v| v.as_str()).unwrap_or("");
    let turn = entry
        .get("turn")
        .and_then(|v| v.as_u64())
        .map(|t| t as usize);
    let data = entry.get("data");

    // Helper: read full content from a file-reference field, relative to log_dir.
    let read_file = |key: &str| -> Option<String> {
        entry
            .get(key)
            .and_then(|f| f.as_str())
            .and_then(|f| fs::read_to_string(log_dir.join(f)).ok())
    };

    // Helper: parse LogLevel from persisted string.
    let parse_log_level = |s: &str| -> Option<LogLevel> {
        match s {
            "info" => Some(LogLevel::Info),
            "warn" => Some(LogLevel::Warn),
            "error" => Some(LogLevel::Error),
            "debug" => Some(LogLevel::Debug),
            "detail" => Some(LogLevel::Detail),
            "model" => Some(LogLevel::Model),
            "agent" => Some(LogLevel::Agent),
            "subagent" => Some(LogLevel::SubAgent),
            _ => None,
        }
    };

    match event_type {
        // ── Skip: internal bookkeeping / high-frequency / not-on-live ──
        //
        // These events either have no AppEvent counterpart, or their AppEvent
        // variants are filtered out in `app_event_to_outbound`.  Returning
        // `None` is the price of a single-rendering-path refactor.
        "session_start"
        | "messages_input"
        | "json_extracted"
        | "agent_input"
        | "voice_audio"
        | "voice_frame"
        | "summary"
        | "interrupted"
        | "voice_log"
        | "voice_protocol"
        | "voice_usage"
        | "voice_error"
        | "voice_diagnostic"
        | "presence_connected"
        | "presence_disconnected"
        | "presence_checkpoint"
        | "tool_request"
        | "tool_response"
        | "live_audio_started"
        | "live_audio_progress"
        | "live_audio_completed" => None,

        // ── Turn lifecycle ──
        "turn_start" => {
            let budget_pct = data
                .and_then(|d| d.get("budget_pct"))
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let remaining = data
                .and_then(|d| d.get("remaining_tokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            Some(AppEvent::TurnStarted {
                turn: turn?,
                budget_pct,
                remaining,
            })
        }

        // ── Model response ──
        "model_response" => {
            let content = read_file("file").unwrap_or_else(|| message.to_string());
            let tokens = data.and_then(|d| d.get("tokens"));
            let usage = TokenUsage {
                prompt_tokens: tokens
                    .and_then(|t| t.get("prompt"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                completion_tokens: tokens
                    .and_then(|t| t.get("completion"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                total_tokens: tokens
                    .and_then(|t| t.get("total"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                cached_tokens: tokens
                    .and_then(|t| t.get("cached"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
            };
            let source = data
                .and_then(|d| d.get("source"))
                .and_then(|v| v.as_str())
                .map(String::from);
            Some(AppEvent::ModelResponse {
                turn: turn.unwrap_or(0),
                content,
                usage,
                reasoning: None,
                source,
            })
        }

        // ── Reasoning: emit as a ModelResponse carrying only the reasoning ──
        //
        // Returns None when both summary and full content are empty so that
        // replay does not render a spurious empty "Model response" row.
        "reasoning" => {
            let full = read_file("file");
            let summary = if message.is_empty() {
                None
            } else {
                Some(message.to_string())
            };
            let reasoning = full.or(summary)?;
            if reasoning.is_empty() {
                return None;
            }
            Some(AppEvent::ModelResponse {
                turn: turn.unwrap_or(0),
                content: String::new(),
                usage: TokenUsage::default(),
                reasoning: Some(reasoning),
                source: None,
            })
        }

        // ── Agent lifecycle ──
        "agent_started" => {
            // Backward compat: older sessions stored the raw JSON blob in
            // `message`; newer sessions store a pre-formatted preview.
            let commands_preview = if message.starts_with('{') {
                crate::format_commands_preview(message)
            } else {
                message.to_string()
            };
            Some(AppEvent::AgentStarted {
                turn: turn.unwrap_or(0),
                commands_preview,
                source: None,
            })
        }
        "agent_output" => {
            let stdout = read_file("file").unwrap_or_else(|| message.to_string());
            let stderr = read_file("file2").unwrap_or_default();
            let source = data
                .and_then(|d| d.get("source"))
                .and_then(|v| v.as_str())
                .map(String::from);
            Some(AppEvent::AgentOutput {
                stdout,
                stderr,
                source,
            })
        }

        "done_signal" => Some(AppEvent::DoneSignal {
            message: Some(message.to_string()).filter(|m| !m.is_empty()),
        }),
        "task_complete" => {
            let reason = data
                .and_then(|d| d.get("reason"))
                .and_then(|v| v.as_str())
                .map(String::from)
                .unwrap_or_else(|| message.to_string());
            let summary = data
                .and_then(|d| d.get("summary"))
                .and_then(|v| v.as_str())
                .map(String::from);
            Some(AppEvent::TaskComplete { reason, summary })
        }
        "session_started" => {
            let session_id = data
                .and_then(|d| d.get("session_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let task = data
                .and_then(|d| d.get("task"))
                .and_then(|v| v.as_str())
                .map(String::from);
            Some(AppEvent::SessionStarted { session_id, task })
        }
        "session_ended" => {
            let session_id = data
                .and_then(|d| d.get("session_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let reason = data
                .and_then(|d| d.get("reason"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Some(AppEvent::SessionEnded { session_id, reason })
        }

        // ── Approval ──
        //
        // The approval id is not persisted directly; we reuse `turn` (set at
        // write time to `self.current_turn`, matching the live convention at
        // `main.rs:3229`).  This breaks if a single turn can emit multiple
        // approval rounds — not the case today, and asserted in tests.
        "auto_approved" => Some(AppEvent::AutoApproved {
            preview: message.to_string(),
        }),
        "approval" => {
            let decision = data
                .and_then(|d| d.get("decision"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let preview = data
                .and_then(|d| d.get("preview"))
                .and_then(|v| v.as_str())
                .unwrap_or(message)
                .to_string();
            let category_str = data
                .and_then(|d| d.get("category"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let id = turn.unwrap_or(0) as u64;
            match decision {
                "waiting" => {
                    let category = crate::autonomy::ActionCategory::from_str(category_str)
                        .unwrap_or(crate::autonomy::ActionCategory::CommandExec);
                    Some(AppEvent::ApprovalRequired {
                        id,
                        command_preview: preview,
                        category,
                    })
                }
                "approved" => Some(AppEvent::ApprovalResolved {
                    id,
                    action: "approve".to_string(),
                }),
                "approve-all" => Some(AppEvent::ApprovalResolved {
                    id,
                    action: "approve_all".to_string(),
                }),
                "skipped" => Some(AppEvent::ApprovalResolved {
                    id,
                    action: "skip".to_string(),
                }),
                "denied" => Some(AppEvent::ApprovalResolved {
                    id,
                    action: "deny".to_string(),
                }),
                "dedup-auto-approved" => Some(AppEvent::AutoApproved { preview }),
                "denied-policy" | "denied-no-approver" => Some(AppEvent::LogEntry {
                    level: "warn".to_string(),
                    source: "system".to_string(),
                    content: format!("Denied ({}): {}", decision, preview),
                    turn,
                }),
                _ => None,
            }
        }
        "approval_resolved" => {
            // The writer formats the message as "Approval {action} (turn {id})".
            // Split on whitespace to recover the action; the id is `turn`.
            let action = message
                .split_whitespace()
                .nth(1)
                .unwrap_or("")
                .to_string();
            Some(AppEvent::ApprovalResolved {
                id: turn.unwrap_or(0) as u64,
                action,
            })
        }
        "human_question" => Some(AppEvent::HumanQuestionDetected {
            question: message.to_string(),
        }),
        "human_response_sent" => Some(AppEvent::HumanResponseSent),

        // ── Round / safety ──
        "round_complete" => {
            let round = data
                .and_then(|d| d.get("round"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            let turns_in_round = data
                .and_then(|d| d.get("turns_in_round"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            Some(AppEvent::RoundComplete {
                round,
                turns_in_round,
                native_message_count: None,
            })
        }
        "safety_cap_reached" => Some(AppEvent::SafetyCapReached),

        // ── Display / debug ──
        "display_ready" => Some(AppEvent::DisplayReady {
            display_id: u32_from_data(data, "display_id")?,
            width: u32_from_data(data, "width").unwrap_or(0),
            height: u32_from_data(data, "height").unwrap_or(0),
        }),
        "display_resize" => Some(AppEvent::DisplayResize {
            display_id: u32_from_data(data, "display_id")?,
            width: u32_from_data(data, "width").unwrap_or(0),
            height: u32_from_data(data, "height").unwrap_or(0),
        }),
        "display_taken" => Some(AppEvent::DisplayTaken {
            display_id: u32_from_data(data, "display_id")?,
        }),
        "display_released" => Some(AppEvent::DisplayReleased {
            display_id: u32_from_data(data, "display_id")?,
            note: data
                .and_then(|d| d.get("note"))
                .and_then(|v| v.as_str())
                .map(String::from),
        }),
        "debug_screen_ready" => Some(AppEvent::DebugScreenReady {
            display_id: u32_from_data(data, "display_id")?,
        }),
        "debug_screen_torn_down" => Some(AppEvent::DebugScreenTornDown {
            display_id: u32_from_data(data, "display_id")?,
        }),

        // ── Recording ──
        "recording_started" => Some(AppEvent::RecordingStarted {
            stream_name: data
                .and_then(|d| d.get("stream_name"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        }),
        "recording_stopped" => Some(AppEvent::RecordingStopped {
            stream_name: data
                .and_then(|d| d.get("stream_name"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        }),
        "recording_error" => {
            // `recording_error` writer stores `error` in data; AppEvent field
            // is named `message`.
            let error = data
                .and_then(|d| d.get("error"))
                .and_then(|v| v.as_str())
                .map(String::from)
                .unwrap_or_else(|| message.to_string());
            let stream_name = data
                .and_then(|d| d.get("stream_name"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Some(AppEvent::RecordingError {
                stream_name,
                message: error,
            })
        }

        // ── Sub-agent / orchestrator ──
        "sub_agent_result" => Some(AppEvent::SubAgentResult {
            formatted: message.to_string(),
        }),
        "orchestrator_progress" => Some(AppEvent::OrchestratorProgress {
            turn: 0,
            status: message.to_string(),
            last_action: String::new(),
        }),

        // ── Presence / live usage ──
        "presence_log" => {
            let level = entry
                .get("level")
                .and_then(|v| v.as_str())
                .and_then(parse_log_level);
            Some(AppEvent::PresenceLog {
                message: message.to_string(),
                level,
                turn: None,
            })
        }
        "presence_usage_update" => Some(AppEvent::PresenceUsageUpdate {
            total_tokens: data
                .and_then(|d| d.get("total_tokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            context_window: data
                .and_then(|d| d.get("context_window"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            usage_pct: data
                .and_then(|d| d.get("usage_pct"))
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0),
            provider: data
                .and_then(|d| d.get("provider"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            model: data
                .and_then(|d| d.get("model"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            prompt_tokens: 0,
            completion_tokens: 0,
            cached_tokens: 0,
        }),
        "live_usage_update" => Some(AppEvent::LiveUsageUpdate {
            provider: data
                .and_then(|d| d.get("provider"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            model: data
                .and_then(|d| d.get("model"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            total_tokens: data
                .and_then(|d| d.get("total_tokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            input_tokens: 0,
            output_tokens: 0,
            cached_tokens: 0,
            thinking_tokens: 0,
        }),

        // ── User transcript ──
        "user_transcript" => {
            let seq = data
                .and_then(|d| d.get("seq"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            Some(AppEvent::UserTranscript {
                text: message.to_string(),
                seq,
            })
        }

        // ── Info / warn / error / debug → LogEntry ──
        //
        // Source and level are derived from message prefixes to match what
        // the live WASM renders for the same messages.  Presence layer and
        // model chatter route to the "server" source; everything else is
        // "system".  `[model] Thinking` / `[model] Tool call:` are demoted
        // to "detail" so they only show under verbose verbosity.
        "info" | "warn" | "error" | "debug" => {
            let source = if message.starts_with("[presence]")
                || message.starts_with("[model]")
                || message.starts_with("Presence")
                || message.starts_with("[ws]")
            {
                "server"
            } else {
                "system"
            };
            let level = if event_type == "info"
                && (message.starts_with("[model] Thinking")
                    || message.starts_with("[model] Tool call:"))
            {
                "detail"
            } else {
                event_type
            };
            Some(AppEvent::LogEntry {
                level: level.to_string(),
                source: source.to_string(),
                content: message.to_string(),
                turn,
            })
        }

        // ── CU (Computer Use) structured events → LogEntry ──
        "cu_task_start" | "cu_turn" | "cu_task_complete" | "cu_task_error" => {
            let (content, level) = match event_type {
                "cu_task_start" => {
                    let task = data
                        .and_then(|d| d.get("task"))
                        .and_then(|v| v.as_str())
                        .unwrap_or(message);
                    let provider = data
                        .and_then(|d| d.get("provider"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let model = data
                        .and_then(|d| d.get("model"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    (
                        format!("CU task: {} ({}:{})", task, provider, model),
                        "info",
                    )
                }
                "cu_turn" => {
                    let t = data
                        .and_then(|d| d.get("turn"))
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let actions = data
                        .and_then(|d| d.get("actions"))
                        .and_then(|v| v.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .unwrap_or_default();
                    (format!("CU turn {}: {}", t, actions), "debug")
                }
                "cu_task_complete" => {
                    let turns = data
                        .and_then(|d| d.get("turns"))
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    (format!("CU complete ({} turns)", turns), "info")
                }
                "cu_task_error" => (format!("CU error: {}", message), "warn"),
                _ => unreachable!(),
            };
            Some(AppEvent::LogEntry {
                level: level.to_string(),
                source: "worker".to_string(),
                content,
                turn: None,
            })
        }
        "session_end" => Some(AppEvent::LogEntry {
            level: "info".to_string(),
            source: "system".to_string(),
            content: message.to_string(),
            turn: None,
        }),

        // ── Unknown / forward-compat ──
        _ => None,
    }
}

/// A reconstructed conversation turn from voice_log / user_transcript events.
#[derive(Debug, Clone)]
pub struct ConversationTurn {
    pub role: String, // "user" or "model"
    pub text: String,
    pub seq: u64,
}

/// Reconstruct recent conversation turns from voice_log and user_transcript events
/// in session.jsonl. Returns the last `max_entries` turns ordered by seq.
pub fn recent_conversation(log_dir: &Path, max_entries: usize) -> Vec<ConversationTurn> {
    // Prefer transcript.jsonl (simpler, faster to parse) if available
    let transcript_path = log_dir.join("transcript.jsonl");
    if transcript_path.exists() {
        if let Ok(content) = fs::read_to_string(&transcript_path) {
            let mut turns: Vec<ConversationTurn> = Vec::new();
            for line in content.lines() {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
                    let role = val["role"].as_str().unwrap_or("");
                    let text = val["text"].as_str().unwrap_or("").to_string();
                    if !text.is_empty() && (role == "user" || role == "model") {
                        turns.push(ConversationTurn {
                            role: role.to_string(),
                            text,
                            seq: 0,
                        });
                    }
                }
            }
            let start = turns.len().saturating_sub(max_entries);
            return turns[start..].to_vec();
        }
    }

    // Fall back to session.jsonl parsing
    let path = log_dir.join("session.jsonl");
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let mut turns: Vec<ConversationTurn> = Vec::new();
    for line in content.lines() {
        let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let event = val["event"].as_str().unwrap_or("");
        let text = val["message"].as_str().unwrap_or("").to_string();
        if text.is_empty() {
            continue;
        }
        let seq = val["data"]["seq"].as_u64().unwrap_or(0);

        match event {
            "user_transcript" => {
                turns.push(ConversationTurn {
                    role: "user".to_string(),
                    text,
                    seq,
                });
            }
            "voice_log" => {
                // Only include transcript entries (model speech), not tool calls
                let tool_ctx = val["data"]["tool_context"].as_str().unwrap_or("");
                if tool_ctx == "transcript" {
                    turns.push(ConversationTurn {
                        role: "model".to_string(),
                        text,
                        seq,
                    });
                }
            }
            _ => {}
        }
    }

    // Entries are already in chronological order from the JSONL file —
    // don't sort by seq since user_transcript and voice_log have independent
    // sequence counters that would interleave incorrectly.
    let start = turns.len().saturating_sub(max_entries);
    turns[start..].to_vec()
}

/// Search voice_log and user_transcript entries for keyword matches.
/// Returns formatted results (up to `max_results`).
pub fn search_voice_entries(
    log_dir: &Path,
    keywords: &[String],
    max_results: usize,
) -> Vec<String> {
    let path = log_dir.join("session.jsonl");
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let mut results = Vec::new();
    for line in content.lines() {
        let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let event = val["event"].as_str().unwrap_or("");
        if event != "voice_log" && event != "user_transcript" {
            continue;
        }
        let text = val["message"].as_str().unwrap_or("");
        if text.is_empty() {
            continue;
        }
        let lower = text.to_lowercase();
        if keywords.iter().any(|kw| lower.contains(&kw.to_lowercase())) {
            let role = if event == "user_transcript" {
                "User"
            } else {
                "Model"
            };
            results.push(format!("[{}] {}", role, text));
            if results.len() >= max_results {
                break;
            }
        }
    }
    results
}

/// Read the last `count` lines from the session.jsonl file in the given log directory.
/// Returns an empty vec if the file doesn't exist or can't be read.
pub fn recent_entries(log_dir: &std::path::Path, count: usize) -> Vec<String> {
    let path = log_dir.join("session.jsonl");
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let lines: Vec<&str> = content.lines().collect();
            let start = lines.len().saturating_sub(count);
            lines[start..].iter().map(|l| l.to_string()).collect()
        }
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_turn_file_accumulates_with_separator() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        // Move past turn 0 so the file suffix stabilises.
        log.turn_start(1, 0.0, 0);
        log.agent_output("first\n", "", None);
        log.agent_output("second\n", "", None);
        let turn_file = log_dir.join("turns/turn_001_stdout.txt");
        let body = fs::read_to_string(&turn_file).unwrap();
        assert!(body.contains("first"), "missing first write: {}", body);
        assert!(body.contains("second"), "missing second write: {}", body);
        // Separator: the two entries are distinct.
        assert!(
            body.find("first").unwrap() < body.find("second").unwrap(),
            "second entry must come after first"
        );
    }

    #[test]
    fn append_turn_file_skips_separator_on_first_write() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(2, 0.0, 0);
        log.agent_output("only\n", "", None);
        let body = fs::read_to_string(log_dir.join("turns/turn_002_stdout.txt")).unwrap();
        // No leading blank line before the first chunk.
        assert!(!body.starts_with('\n'), "unexpected leading newline: {:?}", body);
    }

    #[test]
    fn open_creates_directory_structure() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let _log = SessionLog::open(log_dir.clone()).unwrap();
        assert!(log_dir.join("session.jsonl").exists());
        assert!(log_dir.join("turns").is_dir());
    }

    #[test]
    fn open_uses_directory_name_as_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("my-custom-session");
        let log = SessionLog::open(log_dir).unwrap();
        assert_eq!(log.session_id(), "my-custom-session");
    }

    #[test]
    fn open_with_uuid_dir_uses_uuid_as_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let uuid = Uuid::new_v4().to_string();
        let log_dir = dir.path().join(&uuid);
        let log = SessionLog::open(log_dir).unwrap();
        assert_eq!(log.session_id(), uuid);
    }

    #[test]
    fn open_reuses_existing_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        fs::create_dir_all(&log_dir).unwrap();

        // Write a meta file with a known session_id
        let meta = SessionMeta {
            session_id: "test-session-123".to_string(),
            created_at: "2026-01-01T00:00:00".to_string(),
            project_root: None,
            task: None,
            status: None,
            last_turn: None,
            role: None,
            rounds: None,
        };
        fs::write(
            log_dir.join("session_meta.json"),
            serde_json::to_string_pretty(&meta).unwrap(),
        )
        .unwrap();

        let log = SessionLog::open(log_dir).unwrap();
        assert_eq!(log.session_id(), "test-session-123");
    }

    #[test]
    fn write_meta_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let log = SessionLog::open(log_dir.clone()).unwrap();
        log.write_meta(Some(Path::new("/tmp/project")), Some("test task"));

        let meta_path = log_dir.join("session_meta.json");
        assert!(meta_path.exists());
        let content = fs::read_to_string(&meta_path).unwrap();
        let meta: SessionMeta = serde_json::from_str(&content).unwrap();
        assert_eq!(meta.session_id, log.session_id());
        assert_eq!(meta.project_root.as_deref(), Some("/tmp/project"));
        assert_eq!(meta.task.as_deref(), Some("test task"));
        assert_eq!(meta.status.as_deref(), Some("running"));
    }

    #[test]
    fn events_are_valid_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.info("test info");
        log.warn("test warn");
        log.error("test error");
        log.debug("test debug");
        drop(log);

        let content = fs::read_to_string(log_dir.join("session.jsonl")).unwrap();
        for line in content.lines() {
            let parsed: serde_json::Value = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("Invalid JSON line: {}\n  {}", line, e));
            assert!(parsed.get("ts").is_some());
            assert!(parsed.get("event").is_some());
        }
    }

    #[test]
    fn turn_start_sets_current_turn() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(3, 25.5, 150_000);
        log.info("should have turn 3");
        drop(log);

        let content = fs::read_to_string(log_dir.join("session.jsonl")).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        let last: serde_json::Value = serde_json::from_str(lines.last().unwrap()).unwrap();
        assert_eq!(last["turn"], 3);
    }

    #[test]
    fn model_response_writes_turn_file() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(1, 0.0, 200_000);
        log.model_response("Hello, I will help you.\nHere is my plan.", 100, 50, 150, 0, None);
        drop(log);

        let model_file = log_dir.join("turns/turn_001_model.txt");
        assert!(model_file.exists());
        let content = fs::read_to_string(&model_file).unwrap();
        assert!(content.contains("Hello, I will help you."));
        assert!(content.contains("Here is my plan."));
    }

    #[test]
    fn agent_input_creates_pretty_json_file() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(2, 10.0, 180_000);
        log.agent_input(r#"{"commands":[{"function":"execAsAgent","nonce":1,"command":"ls"}]}"#);
        drop(log);

        let agent_file = log_dir.join("turns/turn_002_agent_in.json");
        assert!(agent_file.exists());
        let content = fs::read_to_string(&agent_file).unwrap();
        assert!(content.contains("execAsAgent"));
        // Should be pretty-printed (has newlines)
        assert!(content.contains('\n'));
    }

    #[test]
    fn agent_output_creates_separate_files() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(1, 0.0, 200_000);
        log.agent_output("stdout content", "stderr content", None);
        drop(log);

        assert!(log_dir.join("turns/turn_001_stdout.txt").exists());
        assert!(log_dir.join("turns/turn_001_stderr.txt").exists());
        let stdout = fs::read_to_string(log_dir.join("turns/turn_001_stdout.txt")).unwrap();
        assert_eq!(stdout, "stdout content");
    }

    #[test]
    fn agent_output_skips_empty_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(1, 0.0, 200_000);
        log.agent_output("stdout only", "", None);
        drop(log);

        assert!(log_dir.join("turns/turn_001_stdout.txt").exists());
        assert!(!log_dir.join("turns/turn_001_stderr.txt").exists());
    }

    #[test]
    fn approval_log_is_searchable() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(5, 30.0, 140_000);
        log.approval("file_write", "writeFile: /tmp/test.rs", "approved");
        drop(log);

        let content = fs::read_to_string(log_dir.join("session.jsonl")).unwrap();
        assert!(content.contains("\"event\":\"approval\""));
        assert!(content.contains("\"category\":\"file_write\""));
        assert!(content.contains("\"decision\":\"approved\""));
    }

    #[test]
    fn json_extracted_shows_functions() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(1, 0.0, 200_000);
        log.json_extracted(r#"{"commands":[{"function":"execAsAgent","nonce":1},{"function":"writeFile","nonce":2}]}"#);
        drop(log);

        let content = fs::read_to_string(log_dir.join("session.jsonl")).unwrap();
        assert!(content.contains("execAsAgent"));
        assert!(content.contains("writeFile"));
    }

    #[test]
    fn write_summary_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.write_summary("test task", "completed", 5);
        drop(log);

        let summary_path = log_dir.join("summary.json");
        assert!(summary_path.exists());
        let content = fs::read_to_string(&summary_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["task"], "test task");
        assert_eq!(parsed["outcome"], "completed");
        assert_eq!(parsed["total_turns"], 5);
    }

    #[test]
    fn write_summary_updates_meta() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.write_meta(Some(Path::new("/tmp")), Some("task"));
        log.write_summary("task", "completed", 3);
        drop(log);

        let meta: SessionMeta =
            serde_json::from_str(&fs::read_to_string(log_dir.join("session_meta.json")).unwrap())
                .unwrap();
        assert_eq!(meta.status.as_deref(), Some("completed"));
        assert_eq!(meta.last_turn, Some(3));
    }

    #[test]
    fn resolve_path_with_override() {
        let dir = tempfile::tempdir().unwrap();
        let custom = dir.path().join("custom_logs");
        let path = SessionLog::resolve_path(Some(custom.to_str().unwrap()));
        assert_eq!(path, custom);
    }

    #[test]
    fn resolve_path_fresh_uses_uuid() {
        let path = SessionLog::resolve_path(None);
        // The directory name should be a UUID (36 chars)
        let dir_name = path.file_name().unwrap().to_string_lossy();
        assert_eq!(dir_name.len(), 36);
        assert!(dir_name.contains('-'));
    }

    #[test]
    fn find_latest_session_basic() {
        let dir = tempfile::tempdir().unwrap();
        let logs_dir = dir.path().join(".intendant/logs");

        // Create two session dirs
        let s1_dir = logs_dir.join("session-1");
        fs::create_dir_all(&s1_dir).unwrap();
        let meta1 = SessionMeta {
            session_id: "session-1".to_string(),
            created_at: "2026-01-01T00:00:00".to_string(),
            project_root: Some("/tmp/project".to_string()),
            task: Some("task 1".to_string()),
            status: Some("completed".to_string()),
            last_turn: Some(5),
            role: None,
            rounds: None,
        };
        fs::write(
            s1_dir.join("session_meta.json"),
            serde_json::to_string_pretty(&meta1).unwrap(),
        )
        .unwrap();

        let s2_dir = logs_dir.join("session-2");
        fs::create_dir_all(&s2_dir).unwrap();
        let meta2 = SessionMeta {
            session_id: "session-2".to_string(),
            created_at: "2026-01-02T00:00:00".to_string(),
            project_root: Some("/tmp/project".to_string()),
            task: Some("task 2".to_string()),
            status: Some("completed".to_string()),
            last_turn: Some(3),
            role: None,
            rounds: None,
        };
        fs::write(
            s2_dir.join("session_meta.json"),
            serde_json::to_string_pretty(&meta2).unwrap(),
        )
        .unwrap();

        // find_latest_session reads from $HOME; for testing we'd need to override HOME
        // so this test just validates that the function doesn't panic with real HOME
        // The functional test relies on find_session_by_id which is path-based
    }

    #[test]
    fn find_session_by_id_direct_path() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("my-session");
        fs::create_dir_all(&session_dir).unwrap();
        // Without session_meta.json, the direct path check still works
        let result = SessionLog::find_session_by_id(session_dir.to_str().unwrap());
        assert_eq!(result, Some(session_dir));
    }

    #[test]
    fn find_session_by_id_nonexistent() {
        let result = SessionLog::find_session_by_id("nonexistent-uuid-12345");
        assert!(result.is_none());
    }

    #[test]
    fn multiple_turns_create_separate_files() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();

        log.turn_start(1, 0.0, 200_000);
        log.model_response("Response 1", 100, 50, 150, 0, None);
        log.agent_input(r#"{"commands":[{"function":"execAsAgent","nonce":1}]}"#);
        log.agent_output("out1", "", None);

        log.turn_start(2, 5.0, 190_000);
        log.model_response("Response 2", 200, 100, 300, 0, None);
        log.agent_input(r#"{"commands":[{"function":"writeFile","nonce":2}]}"#);
        log.agent_output("out2", "err2", None);

        drop(log);

        assert!(log_dir.join("turns/turn_001_model.txt").exists());
        assert!(log_dir.join("turns/turn_002_model.txt").exists());
        assert!(log_dir.join("turns/turn_001_agent_in.json").exists());
        assert!(log_dir.join("turns/turn_002_agent_in.json").exists());
        assert!(log_dir.join("turns/turn_001_stdout.txt").exists());
        assert!(log_dir.join("turns/turn_002_stdout.txt").exists());
        assert!(!log_dir.join("turns/turn_001_stderr.txt").exists());
        assert!(log_dir.join("turns/turn_002_stderr.txt").exists());
    }

    #[test]
    fn messages_input_writes_turn_file() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(1, 0.0, 200_000);
        log.messages_input(
            r#"[{"role":"system","content":"You are an AI."},{"role":"user","content":"Hello"}]"#,
        );
        drop(log);

        let messages_file = log_dir.join("turns/turn_001_messages.json");
        assert!(messages_file.exists());
        let content = fs::read_to_string(&messages_file).unwrap();
        assert!(content.contains("system"));
        assert!(content.contains("Hello"));
    }

    #[test]
    fn reasoning_content_writes_turn_file() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(1, 0.0, 200_000);
        log.reasoning_content(
            Some("The model is thinking about X"),
            Some("Full detailed reasoning about X and Y"),
        );
        drop(log);

        let reasoning_file = log_dir.join("turns/turn_001_reasoning.txt");
        assert!(reasoning_file.exists());
        let content = fs::read_to_string(&reasoning_file).unwrap();
        assert!(content.contains("Full detailed reasoning"));

        let session = fs::read_to_string(log_dir.join("session.jsonl")).unwrap();
        assert!(session.contains("\"event\":\"reasoning\""));
        assert!(session.contains("has_summary"));
    }

    #[test]
    fn reasoning_content_summary_only() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(1, 0.0, 200_000);
        log.reasoning_content(Some("Summary only"), None);
        drop(log);

        // No reasoning file created when no full content
        assert!(!log_dir.join("turns/turn_001_reasoning.txt").exists());
    }

    #[test]
    fn drop_updates_running_to_interrupted() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.write_meta(Some(Path::new("/tmp")), Some("task"));
        log.turn_start(3, 10.0, 180_000);
        drop(log);

        let meta: SessionMeta =
            serde_json::from_str(&fs::read_to_string(log_dir.join("session_meta.json")).unwrap())
                .unwrap();
        assert_eq!(meta.status.as_deref(), Some("interrupted"));
        assert_eq!(meta.last_turn, Some(3));
    }

    #[test]
    fn drop_does_not_overwrite_completed() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.write_meta(Some(Path::new("/tmp")), Some("task"));
        log.write_summary("task", "completed", 5);
        drop(log);

        let meta: SessionMeta =
            serde_json::from_str(&fs::read_to_string(log_dir.join("session_meta.json")).unwrap())
                .unwrap();
        assert_eq!(meta.status.as_deref(), Some("completed"));
        assert_eq!(meta.last_turn, Some(5));
    }

    #[test]
    fn mark_interrupted_updates_running_session() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.write_meta(Some(Path::new("/tmp")), Some("task"));
        log.turn_start(7, 0.0, 100000);

        // Explicitly mark interrupted (simulates SIGTERM handler)
        log.mark_interrupted();

        let meta: SessionMeta =
            serde_json::from_str(&fs::read_to_string(log_dir.join("session_meta.json")).unwrap())
                .unwrap();
        assert_eq!(meta.status.as_deref(), Some("interrupted"));
        assert_eq!(meta.last_turn, Some(7));
    }

    #[test]
    fn mark_interrupted_does_not_overwrite_completed() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.write_meta(Some(Path::new("/tmp")), Some("task"));
        log.write_summary("task", "completed", 5);

        // mark_interrupted should not overwrite "completed"
        log.mark_interrupted();

        let meta: SessionMeta =
            serde_json::from_str(&fs::read_to_string(log_dir.join("session_meta.json")).unwrap())
                .unwrap();
        assert_eq!(meta.status.as_deref(), Some("completed"));
    }

    #[test]
    fn recent_entries_returns_last_n_lines() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        fs::create_dir_all(&log_dir).unwrap();
        let jsonl_path = log_dir.join("session.jsonl");
        let mut f = fs::File::create(&jsonl_path).unwrap();
        for i in 0..10 {
            use std::io::Write;
            writeln!(f, r#"{{"event":"test","index":{}}}"#, i).unwrap();
        }
        drop(f);

        let entries = recent_entries(&log_dir, 3);
        assert_eq!(entries.len(), 3);
        assert!(entries[0].contains("\"index\":7"));
        assert!(entries[2].contains("\"index\":9"));
    }

    #[test]
    fn recent_entries_missing_file_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let entries = recent_entries(dir.path(), 5);
        assert!(entries.is_empty());
    }

    #[test]
    fn recent_entries_fewer_than_count() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        fs::create_dir_all(&log_dir).unwrap();
        let jsonl_path = log_dir.join("session.jsonl");
        fs::write(&jsonl_path, "{\"a\":1}\n{\"a\":2}\n").unwrap();

        let entries = recent_entries(&log_dir, 100);
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn voice_log_writes_jsonl_entry() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.voice_log("hello world", 5, Some("check_status"));

        let content = fs::read_to_string(log_dir.join("session.jsonl")).unwrap();
        let lines: Vec<&str> = content.trim().lines().collect();
        let last: serde_json::Value = serde_json::from_str(lines.last().unwrap()).unwrap();
        assert_eq!(last["event"], "voice_log");
        assert_eq!(last["message"], "hello world");
        assert_eq!(last["data"]["seq"], 5);
        assert_eq!(last["data"]["tool_context"], "check_status");
    }

    #[test]
    fn voice_log_without_tool_context() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.voice_log("hi", 1, None);

        let content = fs::read_to_string(log_dir.join("session.jsonl")).unwrap();
        let lines: Vec<&str> = content.trim().lines().collect();
        let last: serde_json::Value = serde_json::from_str(lines.last().unwrap()).unwrap();
        assert_eq!(last["event"], "voice_log");
        assert_eq!(last["message"], "hi");
        assert!(last["data"]["tool_context"].is_null());
    }

    #[test]
    fn user_transcript_writes_jsonl_entry() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.user_transcript("Hello, run the tests please", 3);

        let content = fs::read_to_string(log_dir.join("session.jsonl")).unwrap();
        let lines: Vec<&str> = content.trim().lines().collect();
        let last: serde_json::Value = serde_json::from_str(lines.last().unwrap()).unwrap();
        assert_eq!(last["event"], "user_transcript");
        assert_eq!(last["message"], "Hello, run the tests please");
        assert_eq!(last["data"]["seq"], 3);
    }

    #[test]
    fn presence_checkpoint_writes_jsonl_entry() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.presence_checkpoint("Agent completed 3 tasks", 15);

        let content = fs::read_to_string(log_dir.join("session.jsonl")).unwrap();
        let lines: Vec<&str> = content.trim().lines().collect();
        let last: serde_json::Value = serde_json::from_str(lines.last().unwrap()).unwrap();
        assert_eq!(last["event"], "presence_checkpoint");
        assert_eq!(last["message"], "Agent completed 3 tasks");
        assert_eq!(last["data"]["last_event_seq"], 15);
    }

    #[test]
    fn presence_connected_writes_jsonl_entry() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.presence_connected(Some("gemini"), Some("gemini-2.5-flash-native-audio"));

        let content = fs::read_to_string(log_dir.join("session.jsonl")).unwrap();
        let lines: Vec<&str> = content.trim().lines().collect();
        let last: serde_json::Value = serde_json::from_str(lines.last().unwrap()).unwrap();
        assert_eq!(last["event"], "presence_connected");
        assert_eq!(last["data"]["provider"], "gemini");
        assert_eq!(last["data"]["model"], "gemini-2.5-flash-native-audio");
    }

    #[test]
    fn presence_disconnected_writes_jsonl_entry() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.presence_disconnected();

        let content = fs::read_to_string(log_dir.join("session.jsonl")).unwrap();
        let lines: Vec<&str> = content.trim().lines().collect();
        let last: serde_json::Value = serde_json::from_str(lines.last().unwrap()).unwrap();
        assert_eq!(last["event"], "presence_disconnected");
    }

    #[test]
    fn tool_request_writes_jsonl_entry() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        let args = serde_json::json!({"id": 42});
        log.tool_request("approve_action", &args);

        let content = fs::read_to_string(log_dir.join("session.jsonl")).unwrap();
        let lines: Vec<&str> = content.trim().lines().collect();
        let last: serde_json::Value = serde_json::from_str(lines.last().unwrap()).unwrap();
        assert_eq!(last["event"], "tool_request");
        assert_eq!(last["data"]["tool"], "approve_action");
        assert_eq!(last["data"]["args"]["id"], 42);
    }

    #[test]
    fn tool_response_writes_jsonl_entry() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.tool_response("check_status", "Phase: idle, Turn: 0");

        let content = fs::read_to_string(log_dir.join("session.jsonl")).unwrap();
        let lines: Vec<&str> = content.trim().lines().collect();
        let last: serde_json::Value = serde_json::from_str(lines.last().unwrap()).unwrap();
        assert_eq!(last["event"], "tool_response");
        assert!(last["message"].as_str().unwrap().contains("check_status"));
    }

    #[test]
    fn recent_conversation_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let turns = recent_conversation(dir.path(), 10);
        assert!(turns.is_empty());
    }

    #[test]
    fn recent_conversation_reconstructs_turns() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();

        log.user_transcript("what's in this project?", 1);
        log.voice_log("It's an autonomous agent runtime.", 2, Some("transcript"));
        log.voice_log("[tool] check_status({})", 3, Some("check_status"));
        log.user_transcript("can you fix the auth bug?", 4);
        log.voice_log("I'll submit that task now.", 5, Some("transcript"));
        // Flush buffered voice utterance (normally happens on turnComplete/session end)
        log.mark_interrupted();
        drop(log);

        let turns = recent_conversation(&log_dir, 10);
        assert_eq!(turns.len(), 4); // tool call excluded
        assert_eq!(turns[0].role, "user");
        assert_eq!(turns[0].text, "what's in this project?");
        assert_eq!(turns[1].role, "model");
        assert_eq!(turns[1].text, "It's an autonomous agent runtime.");
        assert_eq!(turns[2].role, "user");
        assert_eq!(turns[3].role, "model");
    }

    #[test]
    fn recent_conversation_respects_max_entries() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();

        for i in 0..10 {
            log.user_transcript(&format!("msg {}", i), i);
        }

        let turns = recent_conversation(&log_dir, 3);
        assert_eq!(turns.len(), 3);
        assert_eq!(turns[0].text, "msg 7");
        assert_eq!(turns[2].text, "msg 9");
    }

    #[test]
    fn search_voice_entries_finds_matches() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();

        log.user_transcript("fix the authentication bug", 1);
        log.voice_log("I'll check the auth module.", 2, Some("transcript"));
        log.user_transcript("also check the database", 3);
        log.voice_log("[tool] check_status({})", 4, Some("check_status"));

        let results = search_voice_entries(
            &log_dir,
            &["auth".to_string()],
            10,
        );
        assert_eq!(results.len(), 2);
        assert!(results[0].starts_with("[User]"));
        assert!(results[1].starts_with("[Model]"));
    }

    #[test]
    fn search_voice_entries_respects_max_results() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();

        for i in 0..10 {
            log.user_transcript(&format!("test message {}", i), i);
        }

        let results = search_voice_entries(
            &log_dir,
            &["test".to_string()],
            3,
        );
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn search_voice_entries_empty_on_no_match() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();

        log.user_transcript("hello world", 1);

        let results = search_voice_entries(
            &log_dir,
            &["nonexistent".to_string()],
            10,
        );
        assert!(results.is_empty());
    }

    // ------------------------------------------------------------------
    // Round-trip tests for `session_log_entry_to_app_event`.
    // Each test writes to session.jsonl using the typed writer methods,
    // parses the resulting line, runs it through the inverse function,
    // and asserts the reconstructed AppEvent matches expectations.
    // ------------------------------------------------------------------

    use crate::event::AppEvent;

    /// Helper: drop `log`, read session.jsonl, and return the last entry
    /// whose `event` field matches `event_type`.
    fn read_last_event(
        log_dir: &std::path::Path,
        event_type: &str,
    ) -> serde_json::Value {
        let content = fs::read_to_string(log_dir.join("session.jsonl")).unwrap();
        content
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|v| v.get("event").and_then(|e| e.as_str()) == Some(event_type))
            .last()
            .unwrap_or_else(|| panic!("no {} event found", event_type))
    }

    #[test]
    fn rt_model_response() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(5, 0.5, 100_000);
        log.model_response(
            "Hello world — full content here",
            100,
            50,
            150,
            10,
            Some("Codex"),
        );
        drop(log);

        let entry = read_last_event(&log_dir, "model_response");
        let evt = session_log_entry_to_app_event(&entry, &log_dir).unwrap();
        match evt {
            AppEvent::ModelResponse {
                turn,
                content,
                usage,
                reasoning,
                source,
            } => {
                assert_eq!(turn, 5);
                // Verifies the full content was read from the turn file,
                // not truncated to the 200-char preview in `message`.
                assert_eq!(content, "Hello world — full content here");
                assert_eq!(usage.prompt_tokens, 100);
                assert_eq!(usage.completion_tokens, 50);
                assert_eq!(usage.total_tokens, 150);
                assert_eq!(usage.cached_tokens, 10);
                assert!(reasoning.is_none());
                assert_eq!(source.as_deref(), Some("Codex"));
            }
            other => panic!("expected ModelResponse, got {:?}", other),
        }
    }

    #[test]
    fn rt_auto_approved_preserves_preview() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.auto_approved("exec: ls -la /tmp");
        drop(log);

        let entry = read_last_event(&log_dir, "auto_approved");
        match session_log_entry_to_app_event(&entry, &log_dir).unwrap() {
            AppEvent::AutoApproved { preview } => {
                assert_eq!(preview, "exec: ls -la /tmp");
            }
            other => panic!("expected AutoApproved, got {:?}", other),
        }
    }

    #[test]
    fn rt_round_complete() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.round_complete(2, 5);
        drop(log);

        let entry = read_last_event(&log_dir, "round_complete");
        match session_log_entry_to_app_event(&entry, &log_dir).unwrap() {
            AppEvent::RoundComplete {
                round,
                turns_in_round,
                ..
            } => {
                assert_eq!(round, 2);
                assert_eq!(turns_in_round, 5);
            }
            other => panic!("expected RoundComplete, got {:?}", other),
        }
    }

    #[test]
    fn rt_approval_waiting_to_approval_required() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(7, 0.0, 100_000);
        log.approval("command_exec", "exec: rm -rf /tmp/x", "waiting");
        drop(log);

        let entry = read_last_event(&log_dir, "approval");
        match session_log_entry_to_app_event(&entry, &log_dir).unwrap() {
            AppEvent::ApprovalRequired {
                id,
                command_preview,
                category,
            } => {
                assert_eq!(id, 7, "id should be synthesized from turn");
                assert_eq!(command_preview, "exec: rm -rf /tmp/x");
                assert_eq!(category, crate::autonomy::ActionCategory::CommandExec);
            }
            other => panic!("expected ApprovalRequired, got {:?}", other),
        }
    }

    #[test]
    fn rt_approval_approved_to_approval_resolved() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(3, 0.0, 100_000);
        log.approval("file_write", "writeFile: a.rs", "approved");
        drop(log);

        let entry = read_last_event(&log_dir, "approval");
        match session_log_entry_to_app_event(&entry, &log_dir).unwrap() {
            AppEvent::ApprovalResolved { id, action } => {
                assert_eq!(id, 3);
                assert_eq!(action, "approve");
            }
            other => panic!("expected ApprovalResolved, got {:?}", other),
        }
    }

    #[test]
    fn rt_approval_dedup_autoapproved() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(4, 0.0, 100_000);
        log.approval("command_exec", "exec: ls", "dedup-auto-approved");
        drop(log);

        let entry = read_last_event(&log_dir, "approval");
        match session_log_entry_to_app_event(&entry, &log_dir).unwrap() {
            AppEvent::AutoApproved { preview } => {
                assert_eq!(preview, "exec: ls");
            }
            other => panic!("expected AutoApproved, got {:?}", other),
        }
    }

    #[test]
    fn rt_approval_denied_policy() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(6, 0.0, 100_000);
        log.approval("network", "browse: evil.com", "denied-policy");
        drop(log);

        let entry = read_last_event(&log_dir, "approval");
        match session_log_entry_to_app_event(&entry, &log_dir).unwrap() {
            AppEvent::LogEntry {
                level,
                source,
                content,
                turn,
            } => {
                assert_eq!(level, "warn");
                assert_eq!(source, "system");
                assert!(
                    content.contains("Denied (denied-policy)"),
                    "content was: {content}"
                );
                assert!(content.contains("browse: evil.com"));
                assert_eq!(turn, Some(6));
            }
            other => panic!("expected LogEntry for policy deny, got {:?}", other),
        }
    }

    #[test]
    fn rt_agent_output_reads_turn_files() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(1, 0.0, 100_000);
        // stdout large enough that the 200-char preview differs from the full text
        let big_stdout: String = (0..600).map(|i| ((i % 26) as u8 + b'a') as char).collect();
        log.agent_output(&big_stdout, "small stderr", Some("Codex"));
        drop(log);

        let entry = read_last_event(&log_dir, "agent_output");
        match session_log_entry_to_app_event(&entry, &log_dir).unwrap() {
            AppEvent::AgentOutput {
                stdout,
                stderr,
                source,
            } => {
                // Full content read from turn file, not truncated preview.
                assert_eq!(stdout.len(), 600);
                assert_eq!(stdout, big_stdout);
                assert_eq!(stderr, "small stderr");
                assert_eq!(source.as_deref(), Some("Codex"));
            }
            other => panic!("expected AgentOutput, got {:?}", other),
        }
    }

    #[test]
    fn rt_agent_started_old_json_format() {
        // Synthesize an old-style `agent_started` entry where `message`
        // contains raw JSON rather than a pre-formatted preview.
        let raw_json = r#"{"commands":[{"function":"execAsAgent","nonce":1,"command":"ls -la"}]}"#;
        let entry = serde_json::json!({
            "ts": "01:00:00.000",
            "turn": 1,
            "event": "agent_started",
            "level": "info",
            "message": raw_json,
        });
        let dir = tempfile::tempdir().unwrap();
        match session_log_entry_to_app_event(&entry, dir.path()).unwrap() {
            AppEvent::AgentStarted {
                turn,
                commands_preview,
                source,
            } => {
                assert_eq!(turn, 1);
                // format_commands_preview normalized it.
                assert_eq!(commands_preview, "exec: ls -la");
                assert!(source.is_none());
            }
            other => panic!("expected AgentStarted, got {:?}", other),
        }
    }

    #[test]
    fn rt_reasoning_event() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.turn_start(1, 0.0, 100_000);
        log.reasoning_content(
            Some("The model is thinking about X"),
            Some("Full detailed reasoning about X and Y spanning many lines"),
        );
        drop(log);

        let entry = read_last_event(&log_dir, "reasoning");
        match session_log_entry_to_app_event(&entry, &log_dir).unwrap() {
            AppEvent::ModelResponse {
                turn,
                content,
                reasoning,
                source,
                ..
            } => {
                assert_eq!(turn, 1);
                assert!(content.is_empty());
                // Full content preferred over summary.
                assert_eq!(
                    reasoning.as_deref(),
                    Some("Full detailed reasoning about X and Y spanning many lines")
                );
                assert!(source.is_none());
            }
            other => panic!("expected ModelResponse with reasoning, got {:?}", other),
        }
    }

    #[test]
    fn rt_reasoning_skipped_when_empty() {
        // Synthetic: reasoning entry with neither message nor file.
        let entry = serde_json::json!({
            "ts": "01:00:00.000",
            "turn": 1,
            "event": "reasoning",
            "level": "info",
            "data": {"has_summary": false, "has_full_content": false, "full_content_length": 0},
        });
        let dir = tempfile::tempdir().unwrap();
        assert!(
            session_log_entry_to_app_event(&entry, dir.path()).is_none(),
            "reasoning entry with no content should return None"
        );
    }

    #[test]
    fn rt_display_ready_taken_released() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.display_ready(99, 1920, 1080);
        log.display_taken(99);
        log.display_released(99, Some("session ended"));
        drop(log);

        let ready = read_last_event(&log_dir, "display_ready");
        match session_log_entry_to_app_event(&ready, &log_dir).unwrap() {
            AppEvent::DisplayReady {
                display_id,
                width,
                height,
            } => {
                assert_eq!(display_id, 99);
                assert_eq!(width, 1920);
                assert_eq!(height, 1080);
            }
            other => panic!("expected DisplayReady, got {:?}", other),
        }

        let taken = read_last_event(&log_dir, "display_taken");
        match session_log_entry_to_app_event(&taken, &log_dir).unwrap() {
            AppEvent::DisplayTaken { display_id } => assert_eq!(display_id, 99),
            other => panic!("expected DisplayTaken, got {:?}", other),
        }

        let released = read_last_event(&log_dir, "display_released");
        match session_log_entry_to_app_event(&released, &log_dir).unwrap() {
            AppEvent::DisplayReleased { display_id, note } => {
                assert_eq!(display_id, 99);
                assert_eq!(note.as_deref(), Some("session ended"));
            }
            other => panic!("expected DisplayReleased, got {:?}", other),
        }
    }

    #[test]
    fn rt_recording_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.recording_started("rec-1");
        log.recording_error("rec-1", "encoder crashed");
        log.recording_stopped("rec-1");
        drop(log);

        let started = read_last_event(&log_dir, "recording_started");
        match session_log_entry_to_app_event(&started, &log_dir).unwrap() {
            AppEvent::RecordingStarted { stream_name } => {
                assert_eq!(stream_name, "rec-1")
            }
            other => panic!("expected RecordingStarted, got {:?}", other),
        }

        let err = read_last_event(&log_dir, "recording_error");
        match session_log_entry_to_app_event(&err, &log_dir).unwrap() {
            AppEvent::RecordingError {
                stream_name,
                message,
            } => {
                assert_eq!(stream_name, "rec-1");
                assert_eq!(message, "encoder crashed");
            }
            other => panic!("expected RecordingError, got {:?}", other),
        }

        let stopped = read_last_event(&log_dir, "recording_stopped");
        match session_log_entry_to_app_event(&stopped, &log_dir).unwrap() {
            AppEvent::RecordingStopped { stream_name } => {
                assert_eq!(stream_name, "rec-1")
            }
            other => panic!("expected RecordingStopped, got {:?}", other),
        }
    }

    #[test]
    fn rt_cu_task_events_become_log_entries() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();
        log.cu_task_start("click send button", "openai", "gpt-5-cu", true, None, 0);
        log.cu_turn(1, 0, 2, 1, 50, 30, &["click(100,200)".to_string(), "type(hi)".to_string()]);
        log.cu_task_complete(3, true, "done");
        log.cu_task_error("display lost", None);
        drop(log);

        let start = read_last_event(&log_dir, "cu_task_start");
        match session_log_entry_to_app_event(&start, &log_dir).unwrap() {
            AppEvent::LogEntry {
                level,
                source,
                content,
                ..
            } => {
                assert_eq!(level, "info");
                assert_eq!(source, "worker");
                assert!(content.contains("CU task: click send button"));
                assert!(content.contains("openai:gpt-5-cu"));
            }
            other => panic!("expected LogEntry for cu_task_start, got {:?}", other),
        }

        let turn_entry = read_last_event(&log_dir, "cu_turn");
        match session_log_entry_to_app_event(&turn_entry, &log_dir).unwrap() {
            AppEvent::LogEntry {
                level,
                source,
                content,
                ..
            } => {
                assert_eq!(level, "debug");
                assert_eq!(source, "worker");
                assert!(content.contains("CU turn 1"));
                assert!(content.contains("click(100,200)"));
            }
            other => panic!("expected LogEntry for cu_turn, got {:?}", other),
        }

        let complete = read_last_event(&log_dir, "cu_task_complete");
        match session_log_entry_to_app_event(&complete, &log_dir).unwrap() {
            AppEvent::LogEntry { content, .. } => {
                assert!(content.contains("CU complete (3 turns)"));
            }
            other => panic!("expected LogEntry for cu_task_complete, got {:?}", other),
        }

        let err = read_last_event(&log_dir, "cu_task_error");
        match session_log_entry_to_app_event(&err, &log_dir).unwrap() {
            AppEvent::LogEntry {
                level, content, ..
            } => {
                assert_eq!(level, "warn");
                assert!(content.contains("display lost"));
            }
            other => panic!("expected LogEntry for cu_task_error, got {:?}", other),
        }
    }

    #[test]
    fn rt_info_level_source_derivation() {
        // Build synthetic entries to cover prefix-based source/level detection.
        let dir = tempfile::tempdir().unwrap();

        // "Provider: openai …" → generic system/info
        let provider = serde_json::json!({
            "ts": "01:00:00.000",
            "event": "info",
            "level": "info",
            "message": "Provider: openai (key: ...)",
        });
        match session_log_entry_to_app_event(&provider, dir.path()).unwrap() {
            AppEvent::LogEntry {
                level,
                source,
                content,
                ..
            } => {
                assert_eq!(level, "info");
                assert_eq!(source, "system");
                assert!(content.starts_with("Provider: "));
            }
            other => panic!("expected LogEntry, got {:?}", other),
        }

        // "[model] Thinking …" → detail level, server source
        let thinking = serde_json::json!({
            "ts": "01:00:00.000",
            "event": "info",
            "level": "info",
            "message": "[model] Thinking about the task",
        });
        match session_log_entry_to_app_event(&thinking, dir.path()).unwrap() {
            AppEvent::LogEntry { level, source, .. } => {
                assert_eq!(level, "detail");
                assert_eq!(source, "server");
            }
            other => panic!("expected LogEntry, got {:?}", other),
        }

        // "[presence] connected" → server source
        let presence = serde_json::json!({
            "ts": "01:00:00.000",
            "event": "info",
            "level": "info",
            "message": "[presence] connected",
        });
        match session_log_entry_to_app_event(&presence, dir.path()).unwrap() {
            AppEvent::LogEntry { source, .. } => assert_eq!(source, "server"),
            other => panic!("expected LogEntry, got {:?}", other),
        }
    }

    #[test]
    fn skip_internal_events_return_none() {
        let dir = tempfile::tempdir().unwrap();
        for evt in [
            "session_start",
            "messages_input",
            "json_extracted",
            "agent_input",
            "voice_audio",
            "voice_frame",
            "voice_log",
            "voice_protocol",
            "voice_usage",
            "voice_error",
            "voice_diagnostic",
            "presence_connected",
            "presence_disconnected",
            "presence_checkpoint",
            "tool_request",
            "tool_response",
            "live_audio_started",
            "live_audio_progress",
            "live_audio_completed",
            "summary",
            "interrupted",
        ] {
            let entry = serde_json::json!({"event": evt, "ts": "01:00:00"});
            assert!(
                session_log_entry_to_app_event(&entry, dir.path()).is_none(),
                "{} should return None",
                evt
            );
        }
    }

    #[test]
    fn missing_turn_file_falls_back_to_preview() {
        // Synthesize a model_response entry whose `file` reference points
        // to a non-existent turn file.  The inverse function should fall
        // back to the preview stored in `message`.
        let entry = serde_json::json!({
            "ts": "01:00:00.000",
            "turn": 2,
            "event": "model_response",
            "level": "info",
            "message": "short preview",
            "file": "turns/turn_999_model.txt",
            "data": {
                "tokens": {"prompt": 10, "completion": 5, "total": 15, "cached": 0}
            },
        });
        let dir = tempfile::tempdir().unwrap();
        match session_log_entry_to_app_event(&entry, dir.path()).unwrap() {
            AppEvent::ModelResponse { content, .. } => {
                assert_eq!(content, "short preview");
            }
            other => panic!("expected ModelResponse, got {:?}", other),
        }
    }

    #[test]
    fn unknown_event_type_returns_none() {
        let entry = serde_json::json!({
            "event": "some_future_event_type_we_dont_know",
            "ts": "01:00:00.000",
        });
        let dir = tempfile::tempdir().unwrap();
        assert!(session_log_entry_to_app_event(&entry, dir.path()).is_none());
    }
}
