use crate::error::AgentError;
use crate::models::{AgentInput, Command as AgentCommand, ProcessInfo, ProcessStatus};
use std::os::unix::fs::MetadataExt;

use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::{Read as _, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, LazyLock, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use chrono::Local;
use tokio::process::Command;

use portable_pty::{native_pty_system, CommandBuilder as PtyCommandBuilder, PtySize};

static ANSI_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\x1b\[[0-9;]*[a-zA-Z]|\x1b\].*?\x07|\r").unwrap());

static NONCE_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\$NONCE\[(\d+)\]").unwrap());

struct PtySession {
    writer: Box<dyn std::io::Write + Send>,
    reader: Box<dyn std::io::Read + Send>,
    // Keep master alive to prevent EOF
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

const HUMAN_TIMEOUT_MS: u64 = 5 * 60 * 1000; // 5 minutes
const HUMAN_POLL_MS: u64 = 500;
const LOG_TAIL_BYTES: u64 = 10 * 1024; // 10KB

#[derive(Clone)]
pub struct Agent {
    process_state: Arc<RwLock<HashMap<u64, ProcessInfo>>>,
    log_dir: PathBuf,
    pty_sessions: Arc<tokio::sync::Mutex<HashMap<String, PtySession>>>,
    available_displays: Vec<i32>,
    session_xauthority: Option<PathBuf>,
}

impl Agent {
    /// Create an agent with custom paths, used for testing.
    #[cfg(test)]
    pub fn new_with_paths(log_dir: PathBuf) -> Result<Self, AgentError> {
        let process_state = Arc::new(RwLock::new(HashMap::new()));

        fs::create_dir_all(&log_dir)?;

        Ok(Self {
            process_state,
            log_dir,
            pty_sessions: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            available_displays: vec![],
            session_xauthority: None,
        })
    }

    pub fn new() -> Result<Self, AgentError> {
        let process_state = Arc::new(RwLock::new(HashMap::new()));

        // Resolve log directory (reuse existing session or create new)
        let log_dir = Self::resolve_log_dir()?;

        // Discover X displays and merge xauth cookies
        let available_displays = Self::discover_displays();
        let session_xauthority = Self::setup_merged_xauthority(&available_displays, &log_dir);

        Ok(Self {
            process_state,
            log_dir,
            pty_sessions: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            available_displays,
            session_xauthority,
        })
    }

    fn resolve_log_dir() -> Result<PathBuf, AgentError> {
        // Prefer INTENDANT_LOG_DIR env var set by the caller binary
        if let Ok(dir_str) = std::env::var("INTENDANT_LOG_DIR") {
            let path = PathBuf::from(dir_str);
            if path.is_dir() {
                return Ok(path);
            }
            // Dir specified but doesn't exist yet — create it
            fs::create_dir_all(&path)?;
            return Ok(path);
        }
        // Fallback: create a fresh temp directory
        let timestamp = Local::now().format("%Y%m%d_%H%M%S").to_string();
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        let log_dir = PathBuf::from(format!("{}/.intendant/logs/{}", home, timestamp));
        fs::create_dir_all(&log_dir)?;
        Ok(log_dir)
    }

    /// Scan `/tmp/.X*-lock` for active X display numbers.
    fn discover_displays() -> Vec<i32> {
        let mut displays = Vec::new();
        if let Ok(entries) = fs::read_dir("/tmp") {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if let Some(rest) = name.strip_prefix(".X") {
                    if let Some(num_str) = rest.strip_suffix("-lock") {
                        if let Ok(n) = num_str.parse::<i32>() {
                            displays.push(n);
                        }
                    }
                }
            }
        }
        displays.sort();
        displays
    }

    /// Merge xauth cookies from all discovered displays into a session-scoped file.
    fn setup_merged_xauthority(displays: &[i32], log_dir: &Path) -> Option<PathBuf> {
        if displays.is_empty() {
            return None;
        }
        let merged_path = log_dir.join("session.Xauthority");
        let mut any_merged = false;

        // Candidate source paths for xauth cookies
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
        let user_xauth = PathBuf::from(&home).join(".Xauthority");

        for &disp in displays {
            let display_str = format!(":{}", disp);
            // Try user's own Xauthority
            if user_xauth.exists() {
                if let Ok(status) = std::process::Command::new("xauth")
                    .arg("-f")
                    .arg(&user_xauth)
                    .arg("nlist")
                    .arg(&display_str)
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::null())
                    .output()
                {
                    if !status.stdout.is_empty() {
                        if let Ok(merge_status) = std::process::Command::new("xauth")
                            .arg("-f")
                            .arg(&merged_path)
                            .arg("nmerge")
                            .arg("-")
                            .stdin(std::process::Stdio::piped())
                            .stderr(std::process::Stdio::null())
                            .stdout(std::process::Stdio::null())
                            .spawn()
                            .and_then(|mut child| {
                                use std::io::Write;
                                if let Some(ref mut stdin) = child.stdin {
                                    let _ = stdin.write_all(&status.stdout);
                                }
                                child.wait()
                            })
                        {
                            if merge_status.success() {
                                any_merged = true;
                                continue;
                            }
                        }
                    }
                }
            }
            // Try lightdm root cookie
            let lightdm_path = format!("/var/run/lightdm/root/:{}", disp);
            if Path::new(&lightdm_path).exists() {
                if let Ok(status) = std::process::Command::new("sudo")
                    .args(["-n", "xauth", "-f", &lightdm_path, "nlist", &display_str])
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::null())
                    .output()
                {
                    if !status.stdout.is_empty() {
                        if let Ok(merge_status) = std::process::Command::new("xauth")
                            .arg("-f")
                            .arg(&merged_path)
                            .arg("nmerge")
                            .arg("-")
                            .stdin(std::process::Stdio::piped())
                            .stderr(std::process::Stdio::null())
                            .stdout(std::process::Stdio::null())
                            .spawn()
                            .and_then(|mut child| {
                                use std::io::Write;
                                if let Some(ref mut stdin) = child.stdin {
                                    let _ = stdin.write_all(&status.stdout);
                                }
                                child.wait()
                            })
                        {
                            if merge_status.success() {
                                any_merged = true;
                            }
                        }
                    }
                }
            }
        }

        if any_merged {
            Some(merged_path)
        } else {
            None
        }
    }

    /// Return the first discovered display number >0, falling back to 1.
    fn default_display(&self) -> i32 {
        self.available_displays
            .iter()
            .copied()
            .find(|&d| d > 0)
            .unwrap_or(1)
    }

    /// Read the tail of a log file (up to max_bytes from the end).
    fn read_log_tail(path: &Path, max_bytes: u64) -> String {
        if !path.exists() {
            return String::new();
        }
        let mut file = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(_) => return String::new(),
        };
        let total_size = file.metadata().map(|m| m.len()).unwrap_or(0);
        let offset = total_size.saturating_sub(max_bytes);
        let _ = file.seek(SeekFrom::Start(offset));
        let read_len = total_size.saturating_sub(offset) as usize;
        let mut buf = vec![0u8; read_len];
        let bytes_read = file.read(&mut buf).unwrap_or(0);
        buf.truncate(bytes_read);
        String::from_utf8_lossy(&buf).to_string()
    }

    async fn exec_as_agent(&self, cmd: &AgentCommand) -> Result<String, AgentError> {
        let command = cmd.command.as_ref().ok_or_else(|| {
            AgentError::Process("Command string is required for execAsAgent".to_string())
        })?;

        // Wait for port if requested (port 0 means no wait)
        if let Some(port) = cmd.wait_for_port.filter(|&p| p > 0) {
            if !self.wait_for_port(port).await? {
                return Ok(serde_json::json!({
                    "nonce": cmd.nonce,
                    "exit_code": -2,
                    "error": format!("Timed out waiting for port {}", port),
                    "stdout_tail": "",
                    "stderr_tail": ""
                })
                .to_string());
            }
        }

        // Replace $NONCE references
        let command = self.replace_nonce_refs(command)?;

        // Setup output files for this command
        let stdout_path = self.log_dir.join(format!("{}_stdout.log", cmd.nonce));
        let stderr_path = self.log_dir.join(format!("{}_stderr.log", cmd.nonce));

        let stdout_file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&stdout_path)?;
        let stderr_file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&stderr_path)?;

        // Execute command
        let display_id = cmd.display.unwrap_or_else(|| {
            std::env::var("DISPLAY")
                .ok()
                .and_then(|d| d.trim_start_matches(':').parse().ok())
                .unwrap_or_else(|| self.default_display())
        });
        let mut cmd_builder = Command::new("bash");
        cmd_builder
            .arg("-c")
            .arg(&command)
            .env("DISPLAY", format!(":{}", display_id))
            .env_remove("OPENAI_API_KEY")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("GEMINI_API_KEY")
            .env_remove("GEMINI")
            .env_remove("OPENAI")
            .env_remove("ANTHROPIC")
            .stdout(Stdio::from(stdout_file))
            .stderr(Stdio::from(stderr_file));
        if let Some(ref xauth) = self.session_xauthority {
            cmd_builder.env("XAUTHORITY", xauth);
        }
        let mut child = cmd_builder.spawn()?;

        // Update process info in shared memory
        let pid = child.id().unwrap_or(0) as i32;
        self.update_process_info(cmd.nonce, pid, ProcessStatus::Running, 0)?;

        // Block until exit with timeout
        let timeout_ms = cmd.timeout_ms.unwrap_or(120_000);
        let result = tokio::time::timeout(Duration::from_millis(timeout_ms), child.wait()).await;

        let (exit_code, status) = match result {
            Ok(Ok(exit_status)) => {
                let code = exit_status.code().unwrap_or(-1);
                let s = if code == 0 {
                    ProcessStatus::Completed
                } else {
                    ProcessStatus::Failed
                };
                (code, s)
            }
            Ok(Err(e)) => {
                eprintln!("Failed to wait for process: {}", e);
                (-1, ProcessStatus::Failed)
            }
            Err(_) => {
                // Timeout — kill the process
                let _ = child.kill().await;
                (-3, ProcessStatus::Failed)
            }
        };

        self.update_process_info(cmd.nonce, pid, status, exit_code)?;

        // Read stdout/stderr tails
        let stdout_tail = Self::read_log_tail(&stdout_path, LOG_TAIL_BYTES);
        let stderr_tail = Self::read_log_tail(&stderr_path, LOG_TAIL_BYTES);

        Ok(serde_json::json!({
            "nonce": cmd.nonce,
            "pid": pid,
            "exit_code": exit_code,
            "stdout_tail": stdout_tail,
            "stderr_tail": stderr_tail
        })
        .to_string())
    }

    async fn capture_screen(&self, cmd: &AgentCommand) -> Result<String, AgentError> {
        let display = cmd.display.unwrap_or_else(|| {
            std::env::var("DISPLAY")
                .ok()
                .and_then(|d| d.trim_start_matches(':').parse().ok())
                .unwrap_or_else(|| self.default_display())
        });
        let screenshot_path = self.log_dir.join(format!("screenshot_{}.png", cmd.nonce));

        // Use import command from ImageMagick
        let mut cmd_builder = Command::new("import");
        cmd_builder.args([
            "-window",
            "root",
            "-display",
            &format!(":{}", display),
            &screenshot_path.to_string_lossy(),
        ]);
        if let Some(ref xauth) = self.session_xauthority {
            cmd_builder.env("XAUTHORITY", xauth);
        }
        let status = cmd_builder.status().await?;
        let exit_code = status.code().unwrap_or(-1);
        let process_status = if status.success() {
            ProcessStatus::Completed
        } else {
            ProcessStatus::Failed
        };

        self.update_process_info(cmd.nonce, 0, process_status, exit_code)?;

        Ok(serde_json::json!({
            "nonce": cmd.nonce,
            "exit_code": exit_code,
            "screenshot_path": screenshot_path.to_string_lossy(),
            "success": status.success()
        })
        .to_string())
    }

    fn validate_path(path_str: &str) -> Result<PathBuf, AgentError> {
        let raw = PathBuf::from(path_str);
        if raw
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(AgentError::Process(format!(
                "path traversal blocked: {}",
                path_str
            )));
        }
        let path = if raw.exists() {
            fs::canonicalize(&raw)?
        } else {
            let parent = raw.parent().unwrap_or_else(|| Path::new("."));
            let canon_parent = fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
            match raw.file_name() {
                Some(name) => canon_parent.join(name),
                None => canon_parent,
            }
        };

        // Block sensitive filesystem roots and user secret directories.
        if path == Path::new("/etc/shadow")
            || path == Path::new("/etc/gshadow")
            || path.starts_with("/proc")
            || path.starts_with("/sys")
            || path.starts_with("/dev")
            || path.components().any(|c| c.as_os_str() == ".ssh")
            || path.components().any(|c| c.as_os_str() == ".gnupg")
        {
            return Err(AgentError::Process(format!(
                "access to sensitive path blocked: {}",
                path.display()
            )));
        }

        Ok(raw)
    }

    fn inspect_path(&self, cmd: &AgentCommand) -> Result<String, AgentError> {
        let path_str = cmd
            .path
            .as_ref()
            .ok_or_else(|| AgentError::Process("path is required for inspectPath".to_string()))?;
        Self::validate_path(path_str)?;
        let path = std::path::Path::new(path_str);

        if !path.exists() {
            return Ok(serde_json::json!({
                "exists": false,
                "path": path_str
            })
            .to_string());
        }

        let symlink_meta = fs::symlink_metadata(path)?;
        let file_type = if symlink_meta.file_type().is_symlink() {
            "symlink"
        } else if symlink_meta.is_dir() {
            "directory"
        } else if symlink_meta.is_file() {
            "file"
        } else {
            "other"
        };

        let meta = symlink_meta;
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);

        Ok(serde_json::json!({
            "exists": true,
            "path": path_str,
            "type": file_type,
            "size": meta.len(),
            "permissions": format!("{:o}", meta.mode() & 0o7777),
            "modified": modified,
            "uid": meta.uid(),
            "gid": meta.gid()
        })
        .to_string())
    }

    async fn exec_pty(&self, cmd: &AgentCommand) -> Result<String, AgentError> {
        let command = cmd
            .command
            .as_ref()
            .ok_or_else(|| AgentError::Process("command is required for execPty".to_string()))?;
        let shell_id = cmd.shell_id.as_deref().unwrap_or("default").to_string();

        let mut sessions = self.pty_sessions.lock().await;

        // Lazily create PTY session
        if !sessions.contains_key(&shell_id) {
            let pty_system = native_pty_system();
            let pair = pty_system
                .openpty(PtySize {
                    rows: 24,
                    cols: 80,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(|e| AgentError::Process(format!("Failed to open PTY: {}", e)))?;

            let mut pty_cmd = PtyCommandBuilder::new("bash");
            pty_cmd.args(["--norc", "--noprofile"]);
            pair.slave
                .spawn_command(pty_cmd)
                .map_err(|e| AgentError::Process(format!("Failed to spawn shell: {}", e)))?;

            let reader = pair
                .master
                .try_clone_reader()
                .map_err(|e| AgentError::Process(format!("Failed to clone reader: {}", e)))?;
            let writer = pair
                .master
                .take_writer()
                .map_err(|e| AgentError::Process(format!("Failed to take writer: {}", e)))?;

            sessions.insert(
                shell_id.clone(),
                PtySession {
                    writer,
                    reader,
                    _master: pair.master,
                },
            );
        }

        let session = sessions
            .get_mut(&shell_id)
            .ok_or_else(|| AgentError::Process("PTY session not found".to_string()))?;

        // Generate unique start and end markers
        let start_marker = format!("__PTY_START_{}__", cmd.nonce);
        let marker = format!("__PTY_END_{}__", cmd.nonce);

        // Write: echo start-marker, then command, then echo end-marker
        let pty_input = format!("echo '{}'\n{}\necho '{}'\n", start_marker, command, marker);
        session
            .writer
            .write_all(pty_input.as_bytes())
            .map_err(|e| AgentError::Process(format!("Failed to write to PTY: {}", e)))?;
        session
            .writer
            .flush()
            .map_err(|e| AgentError::Process(format!("Failed to flush PTY: {}", e)))?;

        // Read from PTY until end marker appears, with timeout
        let mut output = String::new();
        let timeout_duration = Duration::from_secs(30);
        let start = std::time::Instant::now();
        let mut buf = [0u8; 4096];

        loop {
            if start.elapsed() >= timeout_duration {
                break;
            }

            match session.reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    output.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if output.contains(&marker) {
                        break;
                    }
                }
                Err(_) => break,
            }
        }

        // Clean output: strip ANSI escapes and carriage returns
        let cleaned = ANSI_RE.replace_all(&output, "").to_string();

        // Extract content between start_marker and end_marker
        let content = if let Some(start_pos) = cleaned.find(&start_marker) {
            let after_start = &cleaned[start_pos + start_marker.len()..];
            if let Some(end_pos) = after_start.find(&marker) {
                after_start[..end_pos].to_string()
            } else {
                after_start.to_string()
            }
        } else {
            cleaned
        };

        // Split into lines and clean
        let mut lines: Vec<&str> = content.lines().collect();

        // Remove empty lines (we'll keep meaningful content)
        lines.retain(|line| !line.trim().is_empty());

        // Remove the first line if it's the echoed command
        if !lines.is_empty() {
            let first = lines[0].trim();
            if first == command.trim() || first.ends_with(command.trim()) {
                lines.remove(0);
            }
        }

        // Remove leading empty lines
        while lines.first().is_some_and(|l| l.trim().is_empty()) {
            lines.remove(0);
        }

        // Remove trailing empty lines and bash prompt lines
        while lines.last().is_some_and(|l| {
            let t = l.trim();
            t.is_empty() || t.starts_with("bash-") || t.starts_with("$ ")
        }) {
            lines.pop();
        }

        // Remove lines that are echo commands for the markers
        lines.retain(|line| {
            let trimmed = line.trim();
            !trimmed.contains(&format!("echo '{}'", start_marker))
                && !trimmed.contains(&format!("echo '{}'", marker))
                && !trimmed.contains(&start_marker)
                && !trimmed.contains(&marker)
        });

        let final_output = lines.join("\n");

        Ok(serde_json::json!({
            "success": true,
            "shell_id": shell_id,
            "output": final_output
        })
        .to_string())
    }

    async fn ask_human_with_paths(
        &self,
        cmd: &AgentCommand,
        question_path: &Path,
        response_path: &Path,
        timeout_ms: u64,
        poll_ms: u64,
    ) -> Result<String, AgentError> {
        let question = cmd
            .question
            .as_ref()
            .ok_or_else(|| AgentError::Process("question is required for askHuman".to_string()))?;

        // Write question to file
        fs::write(question_path, question)?;
        // Also write to stderr so caller/user sees it
        eprintln!("[askHuman] {}", question);

        // Poll for response
        let start = std::time::Instant::now();
        let timeout = Duration::from_millis(timeout_ms);
        let poll_interval = Duration::from_millis(poll_ms);

        loop {
            if response_path.exists() {
                let response = fs::read_to_string(response_path)?;
                // Cleanup
                let _ = fs::remove_file(question_path);
                let _ = fs::remove_file(response_path);
                return Ok(serde_json::json!({
                    "success": true,
                    "question": question,
                    "response": response
                })
                .to_string());
            }

            if start.elapsed() >= timeout {
                // Cleanup
                let _ = fs::remove_file(question_path);
                let _ = fs::remove_file(response_path);
                return Ok(serde_json::json!({
                    "success": false,
                    "error": "Timed out waiting for human response"
                })
                .to_string());
            }

            tokio::time::sleep(poll_interval).await;
        }
    }

    async fn ask_human(&self, cmd: &AgentCommand) -> Result<String, AgentError> {
        let timeout_ms = cmd.timeout_ms.unwrap_or(HUMAN_TIMEOUT_MS);
        self.ask_human_with_paths(
            cmd,
            &self.log_dir.join("human_question"),
            &self.log_dir.join("human_response"),
            timeout_ms,
            HUMAN_POLL_MS,
        )
        .await
    }

    async fn wait_for_port_with_retries(
        &self,
        port: u16,
        max_retries: u32,
        interval_ms: u64,
    ) -> Result<bool, AgentError> {
        for _ in 0..max_retries {
            match tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port)).await {
                Ok(_) => return Ok(true),
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(interval_ms)).await;
                }
            }
        }
        Ok(false)
    }

    async fn wait_for_port(&self, port: u16) -> Result<bool, AgentError> {
        self.wait_for_port_with_retries(port, 60, 500).await
    }

    async fn browse(&self, cmd: &AgentCommand) -> Result<String, AgentError> {
        let url = cmd
            .url
            .as_ref()
            .ok_or_else(|| AgentError::Process("url is required for browse".to_string()))?;

        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err(AgentError::Process(
                "url must start with http:// or https://".to_string(),
            ));
        }

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::limited(5))
            .user_agent("Agent/1.0")
            .build()
            .map_err(|e| AgentError::Process(format!("Failed to create HTTP client: {}", e)))?;

        let response = client
            .get(url)
            .send()
            .await
            .map_err(|e| AgentError::Process(format!("HTTP request failed: {}", e)))?;

        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        let body = response
            .text()
            .await
            .map_err(|e| AgentError::Process(format!("Failed to read response body: {}", e)))?;

        let content = if content_type.contains("text/html") {
            html2text::from_read(body.as_bytes(), 120)
        } else {
            body
        };

        let max_size = 50 * 1024;
        let truncated = content.len() > max_size;
        let content = if truncated {
            truncate_utf8_by_bytes(&content, max_size).to_string()
        } else {
            content
        };

        Ok(serde_json::json!({
            "success": true,
            "url": url,
            "status": status,
            "content": content,
            "truncated": truncated
        })
        .to_string())
    }

    fn edit_file(&self, cmd: &AgentCommand) -> Result<String, AgentError> {
        let file_path = cmd
            .file_path
            .as_ref()
            .ok_or_else(|| AgentError::Process("file_path is required for editFile".to_string()))?;
        Self::validate_path(file_path)?;
        let operation = cmd
            .operation
            .as_ref()
            .ok_or_else(|| AgentError::Process("operation is required for editFile".to_string()))?;

        let path = Path::new(file_path);

        match operation.as_str() {
            "write" => {
                let content = cmd.content.as_ref().ok_or_else(|| {
                    AgentError::Process("content is required for write operation".to_string())
                })?;
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(path, content)?;
                Ok(serde_json::json!({
                    "success": true,
                    "operation": "write",
                    "file_path": file_path,
                    "bytes_written": content.len()
                })
                .to_string())
            }
            "append" => {
                let content = cmd.content.as_ref().ok_or_else(|| {
                    AgentError::Process("content is required for append operation".to_string())
                })?;
                use std::io::Write;
                let mut file = OpenOptions::new().create(true).append(true).open(path)?;
                file.write_all(content.as_bytes())?;
                Ok(serde_json::json!({
                    "success": true,
                    "operation": "append",
                    "file_path": file_path,
                    "bytes_written": content.len()
                })
                .to_string())
            }
            "replace" => {
                let match_content = cmd.match_content.as_ref().ok_or_else(|| {
                    AgentError::Process(
                        "match_content is required for replace operation".to_string(),
                    )
                })?;
                let content = cmd.content.as_ref().ok_or_else(|| {
                    AgentError::Process("content is required for replace operation".to_string())
                })?;
                let original = fs::read_to_string(path)?;
                let count = original.matches(match_content.as_str()).count();
                if count == 0 {
                    return Ok(serde_json::json!({
                        "success": false,
                        "operation": "replace",
                        "file_path": file_path,
                        "error": "match_content not found in file"
                    })
                    .to_string());
                }
                let replaced = original.replace(match_content.as_str(), content);
                fs::write(path, &replaced)?;
                Ok(serde_json::json!({
                    "success": true,
                    "operation": "replace",
                    "file_path": file_path,
                    "replacements": count
                })
                .to_string())
            }
            "insert_at" => {
                let content = cmd.content.as_ref().ok_or_else(|| {
                    AgentError::Process("content is required for insert_at operation".to_string())
                })?;
                let line_number = cmd.line_number.ok_or_else(|| {
                    AgentError::Process(
                        "line_number is required for insert_at operation".to_string(),
                    )
                })?;
                let original = if path.exists() {
                    fs::read_to_string(path)?
                } else {
                    String::new()
                };
                let mut lines: Vec<&str> = original.lines().collect();
                let insert_at = line_number.min(lines.len());
                lines.insert(insert_at, content);
                let result = lines.join("\n");
                // Preserve trailing newline if original had one
                let result = if original.ends_with('\n') || original.is_empty() {
                    format!("{}\n", result)
                } else {
                    result
                };
                fs::write(path, &result)?;
                Ok(serde_json::json!({
                    "success": true,
                    "operation": "insert_at",
                    "file_path": file_path,
                    "line_number": insert_at
                })
                .to_string())
            }
            "replace_lines" => {
                let content = cmd.content.as_ref().ok_or_else(|| {
                    AgentError::Process(
                        "content is required for replace_lines operation".to_string(),
                    )
                })?;
                let line_number = cmd.line_number.ok_or_else(|| {
                    AgentError::Process(
                        "line_number is required for replace_lines operation".to_string(),
                    )
                })?;
                let end_line = cmd.end_line.ok_or_else(|| {
                    AgentError::Process(
                        "end_line is required for replace_lines operation".to_string(),
                    )
                })?;
                if end_line < line_number {
                    return Err(AgentError::Process(
                        "end_line must be >= line_number".to_string(),
                    ));
                }
                let original = fs::read_to_string(path)?;
                let mut lines: Vec<&str> = original.lines().collect();
                let start = line_number.min(lines.len());
                let end = end_line.min(lines.len());
                lines.splice(start..end, std::iter::once(content.as_str()));
                let result = lines.join("\n");
                let result = if original.ends_with('\n') {
                    format!("{}\n", result)
                } else {
                    result
                };
                fs::write(path, &result)?;
                Ok(serde_json::json!({
                    "success": true,
                    "operation": "replace_lines",
                    "file_path": file_path,
                    "line_number": start,
                    "end_line": end
                })
                .to_string())
            }
            _ => Err(AgentError::Process(format!(
                "Unknown editFile operation: {}",
                operation
            ))),
        }
    }

    fn store_memory(&self, cmd: &AgentCommand) -> Result<String, AgentError> {
        let key = cmd
            .memory_key
            .as_ref()
            .ok_or_else(|| AgentError::Process("storeMemory requires memory_key".to_string()))?;
        let summary = cmd.memory_summary.as_ref().ok_or_else(|| {
            AgentError::Process("storeMemory requires memory_summary".to_string())
        })?;
        let memory_file = cmd
            .memory_file
            .as_ref()
            .ok_or_else(|| AgentError::Process("storeMemory requires memory_file".to_string()))?;

        let path = PathBuf::from(memory_file);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let tags: Vec<String> = cmd
            .memory_tags
            .as_deref()
            .map(|t| {
                t.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let channel = cmd
            .memory_channel
            .as_deref()
            .unwrap_or("default")
            .to_string();
        let source = cmd.memory_source.as_deref().unwrap_or("agent").to_string();

        // Determine if we should use new format: if tags/channel/source are provided, use new format
        let has_knowledge_fields = cmd.memory_tags.is_some()
            || cmd.memory_channel.is_some()
            || cmd.memory_source.is_some();

        // Try to read existing file, auto-detect format
        let mut data: serde_json::Value = if path.exists() {
            let content = fs::read_to_string(&path)?;
            serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({"entries": {}}))
        } else if has_knowledge_fields {
            // New file with knowledge fields — use new format
            serde_json::json!({"entries": [], "subscriptions": {}, "cursors": {}})
        } else {
            serde_json::json!({"entries": {}})
        };

        // Detect format: old format has entries as object, new format has entries as array
        let is_new_format = data.get("entries").is_some_and(|e| e.is_array());

        if is_new_format {
            // New KnowledgeStore format (Vec)
            let entries = data["entries"].as_array_mut().ok_or_else(|| {
                AgentError::Process("Corrupted memory file: 'entries' is not an array".to_string())
            })?;
            let existing_idx = entries.iter().position(|e| {
                e.get("key").and_then(|k| k.as_str()) == Some(key.as_str())
                    && e.get("source").and_then(|s| s.as_str()) == Some(source.as_str())
            });

            let already_exists = existing_idx.is_some();
            if let Some(idx) = existing_idx {
                let created_at = entries[idx]
                    .get("created_at")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(now);
                entries[idx] = serde_json::json!({
                    "id": key,
                    "key": key,
                    "summary": summary,
                    "tags": tags,
                    "source": source,
                    "channel": channel,
                    "created_at": created_at,
                    "updated_at": now
                });
            } else {
                entries.push(serde_json::json!({
                    "id": key,
                    "key": key,
                    "summary": summary,
                    "tags": tags,
                    "source": source,
                    "channel": channel,
                    "created_at": now,
                    "updated_at": now
                }));
            }

            fs::write(&path, serde_json::to_string_pretty(&data).unwrap())?;

            Ok(serde_json::json!({
                "success": true,
                "key": key,
                "action": if already_exists { "updated" } else { "created" }
            })
            .to_string())
        } else {
            // Old format (HashMap) — maintain backward compatibility
            let already_exists = data
                .get("entries")
                .and_then(|e| e.get(key.as_str()))
                .is_some();

            let created_at = data
                .get("entries")
                .and_then(|e| e.get(key.as_str()))
                .and_then(|e| e.get("created_at"))
                .and_then(|v| v.as_u64())
                .unwrap_or(now);

            data["entries"][key.as_str()] = serde_json::json!({
                "summary": summary,
                "created_at": created_at,
                "updated_at": now
            });

            fs::write(&path, serde_json::to_string_pretty(&data).unwrap())?;

            Ok(serde_json::json!({
                "success": true,
                "key": key,
                "action": if already_exists { "updated" } else { "created" }
            })
            .to_string())
        }
    }

    fn recall_memory(&self, cmd: &AgentCommand) -> Result<String, AgentError> {
        let query = cmd
            .memory_query
            .as_ref()
            .ok_or_else(|| AgentError::Process("recallMemory requires memory_query".to_string()))?;
        let memory_file = cmd
            .memory_file
            .as_ref()
            .ok_or_else(|| AgentError::Process("recallMemory requires memory_file".to_string()))?;

        let path = PathBuf::from(memory_file);
        if !path.exists() {
            return Ok(serde_json::json!({
                "success": true,
                "results": []
            })
            .to_string());
        }

        let content = fs::read_to_string(&path)?;
        let data: serde_json::Value =
            serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({"entries": {}}));

        let query_lower = query.to_lowercase();
        let keywords: Vec<&str> = query_lower.split_whitespace().collect();

        // Parse optional filter parameters
        let filter_tags: Option<Vec<String>> = cmd.memory_tags.as_deref().map(|t| {
            t.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        });
        let filter_channel = cmd.memory_channel.as_deref();
        let filter_source = cmd.memory_source.as_deref();
        let filter_since = cmd.memory_since;

        let mut results: Vec<serde_json::Value> = Vec::new();

        // Detect format: new (array) or old (object)
        let is_new_format = data.get("entries").is_some_and(|e| e.is_array());

        if is_new_format {
            // New KnowledgeStore format
            if let Some(entries) = data.get("entries").and_then(|e| e.as_array()) {
                for entry in entries {
                    let key = entry.get("key").and_then(|k| k.as_str()).unwrap_or("");
                    let summary = entry.get("summary").and_then(|s| s.as_str()).unwrap_or("");
                    let updated_at = entry
                        .get("updated_at")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);

                    // Apply filters
                    if let Some(ref tags) = filter_tags {
                        let entry_tags: Vec<String> = entry
                            .get("tags")
                            .and_then(|t| t.as_array())
                            .map(|a| {
                                a.iter()
                                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                    .collect()
                            })
                            .unwrap_or_default();
                        if !tags.iter().any(|t| entry_tags.contains(t)) {
                            continue;
                        }
                    }
                    if let Some(channel) = filter_channel {
                        if entry.get("channel").and_then(|c| c.as_str()) != Some(channel) {
                            continue;
                        }
                    }
                    if let Some(source) = filter_source {
                        if entry.get("source").and_then(|s| s.as_str()) != Some(source) {
                            continue;
                        }
                    }
                    if let Some(since) = filter_since {
                        if updated_at < since {
                            continue;
                        }
                    }

                    let key_lower = key.to_lowercase();
                    let summary_lower = summary.to_lowercase();

                    let score: usize = keywords
                        .iter()
                        .filter(|kw| key_lower.contains(*kw) || summary_lower.contains(*kw))
                        .count();

                    // Include entry if: keywords match, OR no keywords were given (filter-only query)
                    if score > 0 || keywords.is_empty() {
                        results.push(serde_json::json!({
                            "key": key,
                            "summary": summary,
                            "score": score,
                            "updated_at": updated_at,
                            "tags": entry.get("tags").cloned().unwrap_or(serde_json::json!([])),
                            "channel": entry.get("channel").and_then(|c| c.as_str()).unwrap_or("default"),
                            "source": entry.get("source").and_then(|s| s.as_str()).unwrap_or("")
                        }));
                    }
                }
            }
        } else {
            // Old format (HashMap)
            if let Some(entries) = data.get("entries").and_then(|e| e.as_object()) {
                for (key, value) in entries {
                    let key_lower = key.to_lowercase();
                    let summary_lower = value
                        .get("summary")
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_lowercase();

                    let score: usize = keywords
                        .iter()
                        .filter(|kw| key_lower.contains(*kw) || summary_lower.contains(*kw))
                        .count();

                    if score > 0 {
                        results.push(serde_json::json!({
                            "key": key,
                            "summary": value.get("summary").and_then(|s| s.as_str()).unwrap_or(""),
                            "score": score,
                            "updated_at": value.get("updated_at").and_then(|v| v.as_u64()).unwrap_or(0)
                        }));
                    }
                }
            }
        }

        results.sort_by(|a, b| {
            let sa = a["score"].as_u64().unwrap_or(0);
            let sb = b["score"].as_u64().unwrap_or(0);
            sb.cmp(&sa)
        });

        Ok(serde_json::json!({
            "success": true,
            "results": results
        })
        .to_string())
    }

    fn result_json(nonce: u64, data: &str) -> String {
        serde_json::json!({
            "type": "result",
            "nonce": nonce,
            "data": data
        })
        .to_string()
    }

    pub async fn process_input(&self, input: AgentInput) -> Result<Vec<String>, AgentError> {
        let mut results = Vec::new();

        // Process commands sequentially — each blocks until completion
        for cmd in input.commands {
            match cmd.function.as_str() {
                "execAsAgent" => {
                    let result = self.exec_as_agent(&cmd).await?;
                    results.push(Self::result_json(cmd.nonce, &result));
                }
                "captureScreen" => {
                    let result = self.capture_screen(&cmd).await?;
                    results.push(Self::result_json(cmd.nonce, &result));
                }
                "inspectPath" => match self.inspect_path(&cmd) {
                    Ok(result) => {
                        results.push(Self::result_json(cmd.nonce, &result));
                    }
                    Err(e) => {
                        results.push(Self::result_json(cmd.nonce, &format!("Error: {}", e)));
                    }
                },
                "editFile" => match self.edit_file(&cmd) {
                    Ok(result) => {
                        results.push(Self::result_json(cmd.nonce, &result));
                    }
                    Err(e) => {
                        results.push(Self::result_json(cmd.nonce, &format!("Error: {}", e)));
                    }
                },
                "writeFile" => {
                    let mut edit_cmd = cmd.clone();
                    edit_cmd.function = "editFile".to_string();
                    if edit_cmd.operation.is_none() {
                        edit_cmd.operation = Some("write".to_string());
                    }
                    match self.edit_file(&edit_cmd) {
                        Ok(result) => {
                            results.push(Self::result_json(cmd.nonce, &result));
                        }
                        Err(e) => {
                            results.push(Self::result_json(cmd.nonce, &format!("Error: {}", e)));
                        }
                    }
                }
                "browse" => match self.browse(&cmd).await {
                    Ok(result) => {
                        results.push(Self::result_json(cmd.nonce, &result));
                    }
                    Err(e) => {
                        results.push(Self::result_json(cmd.nonce, &format!("Error: {}", e)));
                    }
                },
                "askHuman" => match self.ask_human(&cmd).await {
                    Ok(result) => {
                        results.push(Self::result_json(cmd.nonce, &result));
                    }
                    Err(e) => {
                        results.push(Self::result_json(cmd.nonce, &format!("Error: {}", e)));
                    }
                },
                "execPty" => match self.exec_pty(&cmd).await {
                    Ok(result) => {
                        results.push(Self::result_json(cmd.nonce, &result));
                    }
                    Err(e) => {
                        results.push(Self::result_json(cmd.nonce, &format!("Error: {}", e)));
                    }
                },
                "storeMemory" => match self.store_memory(&cmd) {
                    Ok(result) => {
                        results.push(Self::result_json(cmd.nonce, &result));
                    }
                    Err(e) => {
                        results.push(Self::result_json(cmd.nonce, &format!("Error: {}", e)));
                    }
                },
                "recallMemory" => match self.recall_memory(&cmd) {
                    Ok(result) => {
                        results.push(Self::result_json(cmd.nonce, &result));
                    }
                    Err(e) => {
                        results.push(Self::result_json(cmd.nonce, &format!("Error: {}", e)));
                    }
                },
                _ => {
                    return Err(AgentError::Process(format!(
                        "Unknown function: {}",
                        cmd.function
                    )))
                }
            }
        }

        Ok(results)
    }

    // Helper methods
    fn update_process_info(
        &self,
        nonce: u64,
        pid: i32,
        status: ProcessStatus,
        exit_code: i32,
    ) -> Result<(), AgentError> {
        let info = ProcessInfo {
            nonce,
            pid,
            status,
            exit_code,
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        };
        self.process_state.write().unwrap().insert(nonce, info);
        Ok(())
    }

    fn get_process_info(&self, nonce: u64) -> Result<ProcessInfo, AgentError> {
        if let Some(info) = self.process_state.read().unwrap().get(&nonce) {
            return Ok(*info);
        }
        Err(AgentError::InvalidNonce(nonce))
    }

    fn replace_nonce_refs(&self, command: &str) -> Result<String, AgentError> {
        let mut result = command.to_string();

        for cap in NONCE_RE.captures_iter(command) {
            let nonce: u64 = cap[1].parse().map_err(|_| {
                AgentError::Process(format!("Invalid nonce reference: {}", &cap[1]))
            })?;

            let info = self.get_process_info(nonce)?;
            result = result.replace(&cap[0], &info.pid.to_string());
        }

        Ok(result)
    }
}

fn truncate_utf8_by_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Create a test agent with temp directory
    fn create_test_agent() -> (Agent, TempDir) {
        let log_dir = TempDir::new().unwrap();
        let agent = Agent::new_with_paths(log_dir.path().to_path_buf()).unwrap();
        (agent, log_dir)
    }

    #[tokio::test]
    async fn update_and_get_process_info() {
        let (agent, _log) = create_test_agent();
        agent
            .update_process_info(1, 1234, ProcessStatus::Running, 0)
            .unwrap();
        let info = agent.get_process_info(1).unwrap();
        assert_eq!(info.nonce, 1);
        assert_eq!(info.pid, 1234);
        assert_eq!(info.status, ProcessStatus::Running);
        assert_eq!(info.exit_code, 0);
    }

    #[tokio::test]
    async fn get_process_info_invalid_nonce() {
        let (agent, _log) = create_test_agent();
        let result = agent.get_process_info(999);
        assert!(result.is_err());
        match result.unwrap_err() {
            AgentError::InvalidNonce(n) => assert_eq!(n, 999),
            other => panic!("expected InvalidNonce, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn replace_nonce_refs_single() {
        let (agent, _log) = create_test_agent();
        agent
            .update_process_info(1, 4567, ProcessStatus::Running, 0)
            .unwrap();
        let result = agent.replace_nonce_refs("kill $NONCE[1]").unwrap();
        assert_eq!(result, "kill 4567");
    }

    #[tokio::test]
    async fn replace_nonce_refs_multiple() {
        let (agent, _log) = create_test_agent();
        agent
            .update_process_info(1, 100, ProcessStatus::Running, 0)
            .unwrap();
        agent
            .update_process_info(2, 200, ProcessStatus::Running, 0)
            .unwrap();
        let result = agent
            .replace_nonce_refs("echo $NONCE[1] and $NONCE[2]")
            .unwrap();
        assert_eq!(result, "echo 100 and 200");
    }

    #[tokio::test]
    async fn replace_nonce_refs_no_refs() {
        let (agent, _log) = create_test_agent();
        let result = agent.replace_nonce_refs("echo hello").unwrap();
        assert_eq!(result, "echo hello");
    }

    #[tokio::test]
    async fn replace_nonce_refs_invalid_nonce() {
        let (agent, _log) = create_test_agent();
        let result = agent.replace_nonce_refs("kill $NONCE[999]");
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn inspect_path_existing_file() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let file_path = tmp.path().join("test.txt");
        fs::write(&file_path, "hello").unwrap();

        let cmd = AgentCommand {
            function: "inspectPath".to_string(),
            nonce: 1,
            path: Some(file_path.to_string_lossy().to_string()),
            ..Default::default()
        };
        let result = agent.inspect_path(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["exists"], true);
        assert_eq!(parsed["type"], "file");
        assert_eq!(parsed["size"], 5);
    }

    #[tokio::test]
    async fn inspect_path_nonexistent() {
        let (agent, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "inspectPath".to_string(),
            nonce: 1,
            path: Some("/nonexistent/path/xyz".to_string()),
            ..Default::default()
        };
        let result = agent.inspect_path(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["exists"], false);
    }

    #[tokio::test]
    async fn inspect_path_directory() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let cmd = AgentCommand {
            function: "inspectPath".to_string(),
            nonce: 1,
            path: Some(tmp.path().to_string_lossy().to_string()),
            ..Default::default()
        };
        let result = agent.inspect_path(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["exists"], true);
        assert_eq!(parsed["type"], "directory");
    }

    #[tokio::test]
    async fn inspect_path_missing_path_field() {
        let (agent, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "inspectPath".to_string(),
            nonce: 1,
            ..Default::default()
        };
        let result = agent.inspect_path(&cmd);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn exec_as_agent_creates_log_files_and_returns_output() {
        let (agent, log_dir) = create_test_agent();
        let cmd = AgentCommand {
            function: "execAsAgent".to_string(),
            command: Some("echo test_output".to_string()),
            nonce: 10,
            display: Some(1),
            ..Default::default()
        };
        let result = agent.exec_as_agent(&cmd).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

        // Should return exit code and stdout
        assert_eq!(parsed["exit_code"], 0);
        assert_eq!(parsed["nonce"], 10);
        assert!(parsed["stdout_tail"]
            .as_str()
            .unwrap()
            .contains("test_output"));

        // Log files should exist
        let stdout_path = log_dir.path().join("10_stdout.log");
        let stderr_path = log_dir.path().join("10_stderr.log");
        assert!(stdout_path.exists(), "stdout log should be created");
        assert!(stderr_path.exists(), "stderr log should be created");
    }

    #[tokio::test]
    async fn exec_as_agent_missing_command() {
        let (agent, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "execAsAgent".to_string(),
            nonce: 1,
            ..Default::default()
        };
        let result = agent.exec_as_agent(&cmd).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn exec_as_agent_failed_command() {
        let (agent, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "execAsAgent".to_string(),
            command: Some("exit 42".to_string()),
            nonce: 1,
            display: Some(1),
            ..Default::default()
        };
        let result = agent.exec_as_agent(&cmd).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["exit_code"], 42);
    }

    #[tokio::test]
    async fn exec_as_agent_stderr_captured() {
        let (agent, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "execAsAgent".to_string(),
            command: Some("echo err_msg >&2".to_string()),
            nonce: 1,
            display: Some(1),
            ..Default::default()
        };
        let result = agent.exec_as_agent(&cmd).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(parsed["stderr_tail"].as_str().unwrap().contains("err_msg"));
    }

    #[tokio::test]
    async fn process_input_exec_returns_result() {
        let (agent, _log) = create_test_agent();
        let input = AgentInput {
            commands: vec![AgentCommand {
                function: "execAsAgent".to_string(),
                command: Some("echo hello".to_string()),
                nonce: 1,
                display: Some(1),
                ..Default::default()
            }],
        };
        let results = agent.process_input(input).await.unwrap();
        assert_eq!(results.len(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&results[0]).unwrap();
        assert_eq!(parsed["type"], "result");
        assert_eq!(parsed["nonce"], 1);
        // Data contains the exec result with exit_code
        let data: serde_json::Value =
            serde_json::from_str(parsed["data"].as_str().unwrap()).unwrap();
        assert_eq!(data["exit_code"], 0);
    }

    #[tokio::test]
    async fn process_input_unknown_function() {
        let (agent, _log) = create_test_agent();
        let input = AgentInput {
            commands: vec![AgentCommand {
                function: "unknownFunc".to_string(),
                nonce: 1,
                ..Default::default()
            }],
        };
        let result = agent.process_input(input).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn process_input_inspect_path() {
        let (agent, _log) = create_test_agent();
        let input = AgentInput {
            commands: vec![AgentCommand {
                function: "inspectPath".to_string(),
                nonce: 1,
                path: Some("/tmp".to_string()),
                ..Default::default()
            }],
        };
        let results = agent.process_input(input).await.unwrap();
        assert_eq!(results.len(), 1);
        let wrapper: serde_json::Value = serde_json::from_str(&results[0]).unwrap();
        assert_eq!(wrapper["type"], "result");
        assert_eq!(wrapper["nonce"], 1);
        let parsed: serde_json::Value =
            serde_json::from_str(wrapper["data"].as_str().unwrap()).unwrap();
        assert_eq!(parsed["exists"], true);
        assert_eq!(parsed["type"], "directory");
    }

    #[tokio::test]
    async fn read_log_tail_missing_file() {
        let tail = Agent::read_log_tail(Path::new("/nonexistent/file"), 1024);
        assert_eq!(tail, "");
    }

    #[tokio::test]
    async fn read_log_tail_small_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("small.log");
        fs::write(&path, "hello world").unwrap();
        let tail = Agent::read_log_tail(&path, 1024);
        assert_eq!(tail, "hello world");
    }

    #[tokio::test]
    async fn read_log_tail_large_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("large.log");
        let content = "x".repeat(20_000);
        fs::write(&path, &content).unwrap();
        let tail = Agent::read_log_tail(&path, 10_000);
        assert_eq!(tail.len(), 10_000);
    }

    // --- editFile tests ---

    #[tokio::test]
    async fn edit_file_write_creates_file() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let fp = tmp.path().join("new.txt");
        let cmd = AgentCommand {
            function: "editFile".to_string(),
            nonce: 1,
            file_path: Some(fp.to_string_lossy().to_string()),
            operation: Some("write".to_string()),
            content: Some("hello world".to_string()),
            ..Default::default()
        };
        let result = agent.edit_file(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["success"], true);
        assert_eq!(parsed["bytes_written"], 11);
        assert_eq!(fs::read_to_string(&fp).unwrap(), "hello world");
    }

    #[tokio::test]
    async fn edit_file_write_creates_parent_dirs() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let fp = tmp.path().join("a/b/c/file.txt");
        let cmd = AgentCommand {
            function: "editFile".to_string(),
            nonce: 1,
            file_path: Some(fp.to_string_lossy().to_string()),
            operation: Some("write".to_string()),
            content: Some("deep".to_string()),
            ..Default::default()
        };
        let result = agent.edit_file(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["success"], true);
        assert_eq!(fs::read_to_string(&fp).unwrap(), "deep");
    }

    #[tokio::test]
    async fn edit_file_write_overwrites() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let fp = tmp.path().join("existing.txt");
        fs::write(&fp, "old content").unwrap();
        let cmd = AgentCommand {
            function: "editFile".to_string(),
            nonce: 1,
            file_path: Some(fp.to_string_lossy().to_string()),
            operation: Some("write".to_string()),
            content: Some("new content".to_string()),
            ..Default::default()
        };
        agent.edit_file(&cmd).unwrap();
        assert_eq!(fs::read_to_string(&fp).unwrap(), "new content");
    }

    #[tokio::test]
    async fn edit_file_append() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let fp = tmp.path().join("append.txt");
        fs::write(&fp, "hello").unwrap();
        let cmd = AgentCommand {
            function: "editFile".to_string(),
            nonce: 1,
            file_path: Some(fp.to_string_lossy().to_string()),
            operation: Some("append".to_string()),
            content: Some(" world".to_string()),
            ..Default::default()
        };
        agent.edit_file(&cmd).unwrap();
        assert_eq!(fs::read_to_string(&fp).unwrap(), "hello world");
    }

    #[tokio::test]
    async fn edit_file_replace_found() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let fp = tmp.path().join("replace.txt");
        fs::write(&fp, "hello world hello").unwrap();
        let cmd = AgentCommand {
            function: "editFile".to_string(),
            nonce: 1,
            file_path: Some(fp.to_string_lossy().to_string()),
            operation: Some("replace".to_string()),
            match_content: Some("hello".to_string()),
            content: Some("goodbye".to_string()),
            ..Default::default()
        };
        let result = agent.edit_file(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["success"], true);
        assert_eq!(parsed["replacements"], 2);
        assert_eq!(fs::read_to_string(&fp).unwrap(), "goodbye world goodbye");
    }

    #[tokio::test]
    async fn edit_file_replace_not_found() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let fp = tmp.path().join("replace_nf.txt");
        fs::write(&fp, "hello world").unwrap();
        let cmd = AgentCommand {
            function: "editFile".to_string(),
            nonce: 1,
            file_path: Some(fp.to_string_lossy().to_string()),
            operation: Some("replace".to_string()),
            match_content: Some("xyz".to_string()),
            content: Some("abc".to_string()),
            ..Default::default()
        };
        let result = agent.edit_file(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["success"], false);
    }

    #[tokio::test]
    async fn edit_file_insert_at_beginning() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let fp = tmp.path().join("insert.txt");
        fs::write(&fp, "line1\nline2\n").unwrap();
        let cmd = AgentCommand {
            function: "editFile".to_string(),
            nonce: 1,
            file_path: Some(fp.to_string_lossy().to_string()),
            operation: Some("insert_at".to_string()),
            content: Some("line0".to_string()),
            line_number: Some(0),
            ..Default::default()
        };
        agent.edit_file(&cmd).unwrap();
        assert_eq!(fs::read_to_string(&fp).unwrap(), "line0\nline1\nline2\n");
    }

    #[tokio::test]
    async fn edit_file_insert_at_end() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let fp = tmp.path().join("insert_end.txt");
        fs::write(&fp, "line1\nline2\n").unwrap();
        let cmd = AgentCommand {
            function: "editFile".to_string(),
            nonce: 1,
            file_path: Some(fp.to_string_lossy().to_string()),
            operation: Some("insert_at".to_string()),
            content: Some("line3".to_string()),
            line_number: Some(999),
            ..Default::default()
        };
        agent.edit_file(&cmd).unwrap();
        let content = fs::read_to_string(&fp).unwrap();
        assert!(content.contains("line3"));
    }

    #[tokio::test]
    async fn edit_file_replace_lines() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let fp = tmp.path().join("replace_lines.txt");
        fs::write(&fp, "a\nb\nc\nd\n").unwrap();
        let cmd = AgentCommand {
            function: "editFile".to_string(),
            nonce: 1,
            file_path: Some(fp.to_string_lossy().to_string()),
            operation: Some("replace_lines".to_string()),
            content: Some("X".to_string()),
            line_number: Some(1),
            end_line: Some(3),
            ..Default::default()
        };
        agent.edit_file(&cmd).unwrap();
        let content = fs::read_to_string(&fp).unwrap();
        assert!(content.contains("X"));
    }

    #[tokio::test]
    async fn edit_file_replace_lines_end_before_start() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let fp = tmp.path().join("bad_range.txt");
        fs::write(&fp, "a\nb\nc\n").unwrap();
        let cmd = AgentCommand {
            function: "editFile".to_string(),
            nonce: 1,
            file_path: Some(fp.to_string_lossy().to_string()),
            operation: Some("replace_lines".to_string()),
            content: Some("X".to_string()),
            line_number: Some(2),
            end_line: Some(1),
            ..Default::default()
        };
        let result = agent.edit_file(&cmd);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn edit_file_missing_fields() {
        let (agent, _log) = create_test_agent();
        // Missing file_path
        let cmd = AgentCommand {
            function: "editFile".to_string(),
            nonce: 1,
            operation: Some("write".to_string()),
            content: Some("test".to_string()),
            ..Default::default()
        };
        assert!(agent.edit_file(&cmd).is_err());

        // Missing operation
        let cmd = AgentCommand {
            function: "editFile".to_string(),
            nonce: 1,
            file_path: Some("/tmp/test".to_string()),
            content: Some("test".to_string()),
            ..Default::default()
        };
        assert!(agent.edit_file(&cmd).is_err());
    }

    #[tokio::test]
    async fn edit_file_unknown_operation() {
        let (agent, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "editFile".to_string(),
            nonce: 1,
            file_path: Some("/tmp/test".to_string()),
            operation: Some("delete".to_string()),
            ..Default::default()
        };
        assert!(agent.edit_file(&cmd).is_err());
    }

    #[tokio::test]
    async fn edit_file_process_input_integration() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let fp = tmp.path().join("integration.txt");
        let input = AgentInput {
            commands: vec![AgentCommand {
                function: "editFile".to_string(),
                nonce: 1,
                file_path: Some(fp.to_string_lossy().to_string()),
                operation: Some("write".to_string()),
                content: Some("integrated".to_string()),
                ..Default::default()
            }],
        };
        let results = agent.process_input(input).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(fs::read_to_string(&fp).unwrap(), "integrated");
    }

    // --- browse tests ---

    #[tokio::test]
    async fn browse_missing_url() {
        let (agent, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "browse".to_string(),
            nonce: 1,
            ..Default::default()
        };
        assert!(agent.browse(&cmd).await.is_err());
    }

    #[tokio::test]
    async fn browse_invalid_scheme() {
        let (agent, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "browse".to_string(),
            nonce: 1,
            url: Some("ftp://example.com".to_string()),
            ..Default::default()
        };
        assert!(agent.browse(&cmd).await.is_err());
    }

    // --- wait_for_port tests ---

    #[tokio::test]
    async fn wait_for_port_already_open() {
        let (agent, _log) = create_test_agent();
        let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
            Ok(l) => l,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(e) => panic!("unexpected bind error: {}", e),
        };
        let port = listener.local_addr().unwrap().port();
        let result = agent
            .wait_for_port_with_retries(port, 1, 100)
            .await
            .unwrap();
        assert!(result, "should succeed when port is already open");
    }

    #[tokio::test]
    async fn wait_for_port_timeout() {
        let (agent, _log) = create_test_agent();
        let result = agent
            .wait_for_port_with_retries(59999, 2, 50)
            .await
            .unwrap();
        assert!(!result, "should fail when port is never opened");
    }

    // --- askHuman tests ---

    #[tokio::test]
    async fn ask_human_missing_question() {
        let (agent, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "askHuman".to_string(),
            nonce: 1,
            ..Default::default()
        };
        let tmp = TempDir::new().unwrap();
        let q = tmp.path().join("q");
        let r = tmp.path().join("r");
        let result = agent.ask_human_with_paths(&cmd, &q, &r, 1000, 100).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn ask_human_response_already_available() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let q = tmp.path().join("q");
        let r = tmp.path().join("r");
        fs::write(&r, "yes").unwrap();
        let cmd = AgentCommand {
            function: "askHuman".to_string(),
            nonce: 1,
            question: Some("proceed?".to_string()),
            ..Default::default()
        };
        let result = agent
            .ask_human_with_paths(&cmd, &q, &r, 1000, 100)
            .await
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["success"], true);
        assert_eq!(parsed["response"], "yes");
    }

    #[tokio::test]
    async fn ask_human_timeout() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let q = tmp.path().join("q");
        let r = tmp.path().join("r");
        let cmd = AgentCommand {
            function: "askHuman".to_string(),
            nonce: 1,
            question: Some("hello?".to_string()),
            ..Default::default()
        };
        let result = agent
            .ask_human_with_paths(&cmd, &q, &r, 200, 50)
            .await
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["success"], false);
    }

    // --- execPty tests ---

    #[tokio::test]
    async fn exec_pty_simple_echo() {
        let (agent, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "execPty".to_string(),
            nonce: 1,
            command: Some("echo pty_test".to_string()),
            ..Default::default()
        };
        let result = match agent.exec_pty(&cmd).await {
            Ok(r) => r,
            Err(AgentError::Process(msg)) if msg.contains("Permission denied") => return,
            Err(e) => panic!("unexpected exec_pty error: {}", e),
        };
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["success"], true);
        assert!(parsed["output"].as_str().unwrap().contains("pty_test"));
    }

    #[tokio::test]
    async fn exec_pty_missing_command() {
        let (agent, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "execPty".to_string(),
            nonce: 1,
            ..Default::default()
        };
        assert!(agent.exec_pty(&cmd).await.is_err());
    }

    // --- storeMemory / recallMemory tests ---

    #[tokio::test]
    async fn store_memory_create() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let mf = tmp.path().join("mem.json");
        let cmd = AgentCommand {
            function: "storeMemory".to_string(),
            nonce: 1,
            memory_key: Some("test_key".to_string()),
            memory_summary: Some("test value".to_string()),
            memory_file: Some(mf.to_string_lossy().to_string()),
            ..Default::default()
        };
        let result = agent.store_memory(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["success"], true);
        assert_eq!(parsed["action"], "created");
    }

    #[tokio::test]
    async fn store_memory_update() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let mf = tmp.path().join("mem.json");
        let cmd = AgentCommand {
            function: "storeMemory".to_string(),
            nonce: 1,
            memory_key: Some("test_key".to_string()),
            memory_summary: Some("v1".to_string()),
            memory_file: Some(mf.to_string_lossy().to_string()),
            ..Default::default()
        };
        agent.store_memory(&cmd).unwrap();
        let cmd2 = AgentCommand {
            memory_summary: Some("v2".to_string()),
            ..cmd
        };
        let result = agent.store_memory(&cmd2).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["action"], "updated");
    }

    #[tokio::test]
    async fn store_memory_missing_key() {
        let (agent, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "storeMemory".to_string(),
            nonce: 1,
            memory_summary: Some("value".to_string()),
            memory_file: Some("/tmp/mem.json".to_string()),
            ..Default::default()
        };
        assert!(agent.store_memory(&cmd).is_err());
    }

    #[tokio::test]
    async fn recall_memory_empty() {
        let (agent, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "recallMemory".to_string(),
            nonce: 1,
            memory_query: Some("anything".to_string()),
            memory_file: Some("/nonexistent/mem.json".to_string()),
            ..Default::default()
        };
        let result = agent.recall_memory(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["success"], true);
        assert!(parsed["results"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn recall_memory_finds_matches() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let mf = tmp.path().join("mem.json");
        // Store some memories
        agent
            .store_memory(&AgentCommand {
                function: "storeMemory".to_string(),
                nonce: 1,
                memory_key: Some("db_host".to_string()),
                memory_summary: Some("localhost:5432".to_string()),
                memory_file: Some(mf.to_string_lossy().to_string()),
                ..Default::default()
            })
            .unwrap();
        agent
            .store_memory(&AgentCommand {
                function: "storeMemory".to_string(),
                nonce: 2,
                memory_key: Some("api_key".to_string()),
                memory_summary: Some("secret123".to_string()),
                memory_file: Some(mf.to_string_lossy().to_string()),
                ..Default::default()
            })
            .unwrap();

        let result = agent
            .recall_memory(&AgentCommand {
                function: "recallMemory".to_string(),
                nonce: 3,
                memory_query: Some("db host".to_string()),
                memory_file: Some(mf.to_string_lossy().to_string()),
                ..Default::default()
            })
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["success"], true);
        let results = parsed["results"].as_array().unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0]["key"], "db_host");
    }

    #[tokio::test]
    async fn recall_memory_missing_query() {
        let (agent, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "recallMemory".to_string(),
            nonce: 1,
            memory_file: Some("/tmp/mem.json".to_string()),
            ..Default::default()
        };
        assert!(agent.recall_memory(&cmd).is_err());
    }

    #[tokio::test]
    async fn store_memory_with_tags_and_channel() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let mf = tmp.path().join("knowledge.json");
        let cmd = AgentCommand {
            function: "storeMemory".to_string(),
            nonce: 1,
            memory_key: Some("finding1".to_string()),
            memory_summary: Some("important discovery".to_string()),
            memory_file: Some(mf.to_string_lossy().to_string()),
            memory_tags: Some("research,important".to_string()),
            memory_channel: Some("project_x".to_string()),
            memory_source: Some("agent_1".to_string()),
            ..Default::default()
        };
        let result = agent.store_memory(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["success"], true);
        assert_eq!(parsed["action"], "created");
    }

    #[tokio::test]
    async fn recall_memory_with_tag_filter() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let mf = tmp.path().join("knowledge.json");
        // Store with tags
        agent
            .store_memory(&AgentCommand {
                function: "storeMemory".to_string(),
                nonce: 1,
                memory_key: Some("tagged_entry".to_string()),
                memory_summary: Some("has tags".to_string()),
                memory_file: Some(mf.to_string_lossy().to_string()),
                memory_tags: Some("alpha,beta".to_string()),
                memory_channel: Some("test".to_string()),
                ..Default::default()
            })
            .unwrap();
        // Recall with matching tag
        let result = agent
            .recall_memory(&AgentCommand {
                function: "recallMemory".to_string(),
                nonce: 2,
                memory_query: Some("tagged".to_string()),
                memory_file: Some(mf.to_string_lossy().to_string()),
                memory_tags: Some("alpha".to_string()),
                ..Default::default()
            })
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(!parsed["results"].as_array().unwrap().is_empty());

        // Recall with non-matching tag
        let result = agent
            .recall_memory(&AgentCommand {
                function: "recallMemory".to_string(),
                nonce: 3,
                memory_query: Some("tagged".to_string()),
                memory_file: Some(mf.to_string_lossy().to_string()),
                memory_tags: Some("gamma".to_string()),
                ..Default::default()
            })
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(parsed["results"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn recall_memory_with_channel_filter() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let mf = tmp.path().join("knowledge.json");
        agent
            .store_memory(&AgentCommand {
                function: "storeMemory".to_string(),
                nonce: 1,
                memory_key: Some("chan_entry".to_string()),
                memory_summary: Some("in channel".to_string()),
                memory_file: Some(mf.to_string_lossy().to_string()),
                memory_channel: Some("chan_a".to_string()),
                ..Default::default()
            })
            .unwrap();
        // Match channel
        let result = agent
            .recall_memory(&AgentCommand {
                function: "recallMemory".to_string(),
                nonce: 2,
                memory_query: Some("chan".to_string()),
                memory_file: Some(mf.to_string_lossy().to_string()),
                memory_channel: Some("chan_a".to_string()),
                ..Default::default()
            })
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(!parsed["results"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn store_memory_backward_compat_old_format() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let mf = tmp.path().join("old_mem.json");
        // No tags/channel/source => should use old format
        let cmd = AgentCommand {
            function: "storeMemory".to_string(),
            nonce: 1,
            memory_key: Some("old_key".to_string()),
            memory_summary: Some("old value".to_string()),
            memory_file: Some(mf.to_string_lossy().to_string()),
            ..Default::default()
        };
        agent.store_memory(&cmd).unwrap();
        let content = fs::read_to_string(&mf).unwrap();
        let data: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(data["entries"].is_object(), "should be old format (object)");
    }

    #[tokio::test]
    async fn store_memory_process_input_integration() {
        let (agent, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let mf = tmp.path().join("mem.json");
        let input = AgentInput {
            commands: vec![AgentCommand {
                function: "storeMemory".to_string(),
                nonce: 1,
                memory_key: Some("test".to_string()),
                memory_summary: Some("value".to_string()),
                memory_file: Some(mf.to_string_lossy().to_string()),
                ..Default::default()
            }],
        };
        let results = agent.process_input(input).await.unwrap();
        assert_eq!(results.len(), 1);
    }

    #[tokio::test]
    async fn recall_memory_process_input_integration() {
        let (agent, _log) = create_test_agent();
        let input = AgentInput {
            commands: vec![AgentCommand {
                function: "recallMemory".to_string(),
                nonce: 1,
                memory_query: Some("test".to_string()),
                memory_file: Some("/nonexistent/mem.json".to_string()),
                ..Default::default()
            }],
        };
        let results = agent.process_input(input).await.unwrap();
        assert_eq!(results.len(), 1);
    }

    #[tokio::test]
    async fn discover_displays_no_lock_files() {
        // This test just verifies the function doesn't panic
        let displays = Agent::discover_displays();
        // Can't assert specific values since it depends on environment
        assert!(displays.len() < 100); // sanity check
    }

    #[tokio::test]
    async fn default_display_empty_returns_1() {
        let (agent, _log) = create_test_agent();
        assert_eq!(agent.default_display(), 1);
    }

    #[tokio::test]
    async fn setup_merged_xauthority_empty_displays() {
        let tmp = TempDir::new().unwrap();
        let result = Agent::setup_merged_xauthority(&[], tmp.path());
        assert!(result.is_none());
    }

    #[test]
    fn validate_path_traversal_blocked() {
        assert!(Agent::validate_path("/tmp/../etc/passwd").is_err());
        assert!(Agent::validate_path("/home/user/..").is_err());
        assert!(Agent::validate_path("..").is_err());
    }

    #[test]
    fn validate_path_sensitive_blocked() {
        assert!(Agent::validate_path("/etc/shadow").is_err());
        assert!(Agent::validate_path("/proc/1/cmdline").is_err());
        assert!(Agent::validate_path("/sys/class/net").is_err());
        assert!(Agent::validate_path("/dev/sda").is_err());
        assert!(Agent::validate_path("/home/user/.ssh/id_rsa").is_err());
        assert!(Agent::validate_path("/home/user/.gnupg/secring.gpg").is_err());
    }

    #[test]
    fn validate_path_normal_accepted() {
        assert!(Agent::validate_path("/tmp/test.txt").is_ok());
        assert!(Agent::validate_path("/home/user/project/src/main.rs").is_ok());
        assert!(Agent::validate_path("relative/path.txt").is_ok());
    }
}
