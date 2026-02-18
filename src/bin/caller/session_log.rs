use chrono::Local;
use serde::Serialize;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

const SESSION_FILE_PATH: &str = "/dev/shm/intendant_session";

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

/// Comprehensive structured session logger.
///
/// Writes to a directory containing:
/// - `session.jsonl`    — one JSON object per line, every event with metadata
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
    dir: PathBuf,
    current_turn: usize,
}

impl SessionLog {
    /// Open (or create) a session log directory.
    /// The `path` argument is the directory (not a file).
    pub fn open(dir: PathBuf) -> std::io::Result<Self> {
        fs::create_dir_all(&dir)?;
        fs::create_dir_all(dir.join("turns"))?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("session.jsonl"))?;
        let mut log = Self {
            writer: BufWriter::new(file),
            dir,
            current_turn: 0,
        };
        log.emit(LogEvent {
            ts: Self::ts(),
            turn: None,
            event: "session_start".to_string(),
            level: Some("info".to_string()),
            message: Some(format!("Session started at {}", Local::now().format("%Y-%m-%d %H:%M:%S"))),
            data: None,
            file: None,
            file2: None,
        });
        Ok(log)
    }

    /// Resolve the session log directory.
    /// If `override_path` is set (via --log-file), use that as the directory.
    /// Otherwise, use the shared session directory.
    pub fn resolve_path(override_path: Option<&str>) -> PathBuf {
        if let Some(path) = override_path {
            return PathBuf::from(path);
        }

        // Reuse existing session directory if present
        if let Ok(existing) = fs::read_to_string(SESSION_FILE_PATH) {
            let dir = PathBuf::from(existing.trim());
            if dir.is_dir() {
                return dir;
            }
        }

        // Create a new session directory
        let timestamp = Local::now().format("%Y%m%d_%H%M%S").to_string();
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        let dir = PathBuf::from(format!("{}/.intendant/logs/{}", home, timestamp));
        let _ = fs::create_dir_all(&dir);
        let _ = fs::write(SESSION_FILE_PATH, dir.to_string_lossy().as_bytes());
        dir
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn ts() -> String {
        Local::now().format("%H:%M:%S%.3f").to_string()
    }

    fn emit(&mut self, event: LogEvent) {
        if let Ok(json) = serde_json::to_string(&event) {
            let _ = writeln!(self.writer, "{}", json);
            let _ = self.writer.flush();
        }
    }

    /// Write content to a turn-specific file and return its relative path.
    fn write_turn_file(&self, suffix: &str, content: &str) -> Option<String> {
        let relative = format!("turns/turn_{:03}_{}", self.current_turn, suffix);
        let path = self.dir.join(&relative);
        if fs::write(&path, content).is_ok() {
            Some(relative)
        } else {
            None
        }
    }

    // ---- Public logging methods ----

    pub fn info(&mut self, msg: &str) {
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: if self.current_turn > 0 { Some(self.current_turn) } else { None },
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
            turn: if self.current_turn > 0 { Some(self.current_turn) } else { None },
            event: "warn".to_string(),
            level: Some("warn".to_string()),
            message: Some(msg.to_string()),
            data: None,
            file: None,
            file2: None,
        });
    }

    pub fn error(&mut self, msg: &str) {
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: if self.current_turn > 0 { Some(self.current_turn) } else { None },
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
            turn: if self.current_turn > 0 { Some(self.current_turn) } else { None },
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

    /// Log the full model response. Content is written to a per-turn file.
    pub fn model_response(
        &mut self,
        content: &str,
        prompt_tokens: u64,
        completion_tokens: u64,
        total_tokens: u64,
    ) {
        let file = self.write_turn_file("model.txt", content);
        let preview: String = content.chars().take(200).collect();
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: Some(self.current_turn),
            event: "model_response".to_string(),
            level: Some("info".to_string()),
            message: Some(preview),
            data: Some(serde_json::json!({
                "tokens": {
                    "prompt": prompt_tokens,
                    "completion": completion_tokens,
                    "total": total_tokens,
                },
                "content_length": content.len(),
            })),
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
            .filter_map(|cmd| cmd.get("function").and_then(|f| f.as_str()).map(String::from))
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
    pub fn agent_output(&mut self, stdout: &str, stderr: &str) {
        let file = if !stdout.is_empty() {
            self.write_turn_file("stdout.txt", stdout)
        } else {
            None
        };
        let file2 = if !stderr.is_empty() {
            self.write_turn_file("stderr.txt", stderr)
        } else {
            None
        };

        let preview: String = stdout.chars().take(200).collect();
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: Some(self.current_turn),
            event: "agent_output".to_string(),
            level: if stderr.is_empty() { Some("info".to_string()) } else { Some("warn".to_string()) },
            message: if stdout.is_empty() && stderr.is_empty() {
                Some("(no output)".to_string())
            } else {
                Some(preview)
            },
            data: Some(serde_json::json!({
                "stdout_length": stdout.len(),
                "stderr_length": stderr.len(),
            })),
            file,
            file2,
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
            .filter_map(|cmd| cmd.get("function").and_then(|f| f.as_str()).map(String::from))
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
                if done { "done signal".to_string() } else { "no commands".to_string() }
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
    pub fn write_summary(&mut self, task: &str, outcome: &str, total_turns: usize) {
        let summary = serde_json::json!({
            "task": task,
            "outcome": outcome,
            "total_turns": total_turns,
            "session_dir": self.dir.to_string_lossy(),
            "ended_at": Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        });
        let path = self.dir.join("summary.json");
        if let Ok(pretty) = serde_json::to_string_pretty(&summary) {
            let _ = fs::write(path, pretty);
        }
        self.emit(LogEvent {
            ts: Self::ts(),
            turn: None,
            event: "session_end".to_string(),
            level: Some("info".to_string()),
            message: Some(format!("Session ended: {} ({} turns)", outcome, total_turns)),
            data: None,
            file: Some("summary.json".to_string()),
            file2: None,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_creates_directory_structure() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let _log = SessionLog::open(log_dir.clone()).unwrap();
        assert!(log_dir.join("session.jsonl").exists());
        assert!(log_dir.join("turns").is_dir());
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
        log.model_response("Hello, I will help you.\nHere is my plan.", 100, 50, 150);
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
        log.agent_output("stdout content", "stderr content");
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
        log.agent_output("stdout only", "");
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
    fn resolve_path_with_override() {
        let path = SessionLog::resolve_path(Some("/tmp/custom_logs"));
        assert_eq!(path, PathBuf::from("/tmp/custom_logs"));
    }

    #[test]
    fn multiple_turns_create_separate_files() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = SessionLog::open(log_dir.clone()).unwrap();

        log.turn_start(1, 0.0, 200_000);
        log.model_response("Response 1", 100, 50, 150);
        log.agent_input(r#"{"commands":[{"function":"execAsAgent","nonce":1}]}"#);
        log.agent_output("out1", "");

        log.turn_start(2, 5.0, 190_000);
        log.model_response("Response 2", 200, 100, 300);
        log.agent_input(r#"{"commands":[{"function":"writeFile","nonce":2}]}"#);
        log.agent_output("out2", "err2");

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
}
