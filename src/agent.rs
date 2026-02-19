use crate::error::AgentError;
use crate::models::{
    AgentInput, Command as AgentCommand, ProcessInfo, ProcessStatus, StatusUpdate,
};
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;

use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::{Read as _, Seek, SeekFrom},
    mem::size_of,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use chrono::Local;
use memmap2::{MmapMut, MmapOptions};
use tokio::process::Command;
use tokio::sync::mpsc;

use portable_pty::{native_pty_system, CommandBuilder as PtyCommandBuilder, PtySize};

struct PtySession {
    writer: Box<dyn std::io::Write + Send>,
    reader: Box<dyn std::io::Read + Send>,
    // Keep master alive to prevent EOF
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

const HUMAN_TIMEOUT_MS: u64 = 5 * 60 * 1000; // 5 minutes
const HUMAN_POLL_MS: u64 = 500;

const MAX_PROCESSES: usize = 1024;
const SHARED_MEM_SIZE: usize = size_of::<ProcessInfo>() * MAX_PROCESSES;
#[derive(Clone)]
pub struct Agent {
    pub shared_mem: Arc<RwLock<MmapMut>>,
    pub process_map: Arc<RwLock<HashMap<u64, usize>>>,
    scope_to_nonce: Arc<RwLock<HashMap<String, u64>>>,
    nonce_to_scope: Arc<RwLock<HashMap<u64, String>>>,
    log_dir: PathBuf,
    status_tx: mpsc::Sender<StatusUpdate>,
    pty_sessions: Arc<tokio::sync::Mutex<HashMap<String, PtySession>>>,
    source_generation: u64,
}

impl Agent {
    fn current_session_id() -> Option<String> {
        fs::read_to_string(session_file_path())
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    }

    /// Create an agent with custom paths, used for testing.
    #[cfg(test)]
    pub fn new_with_paths(shared_mem_path: &str, log_dir: PathBuf) -> Result<Self, AgentError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .open(shared_mem_path)?;
        file.set_len(SHARED_MEM_SIZE as u64)?;

        let mmap = unsafe { MmapOptions::new().map_mut(&file)? };
        let process_map = Self::rebuild_process_map(&mmap);
        let shared_mem = Arc::new(RwLock::new(mmap));
        let process_map = Arc::new(RwLock::new(process_map));

        fs::create_dir_all(&log_dir)?;

        let (status_tx, mut status_rx) = mpsc::channel::<StatusUpdate>(1024);

        let shared_mem_clone = shared_mem.clone();
        let process_map_clone = process_map.clone();
        tokio::spawn(async move {
            while let Some(update) = status_rx.recv().await {
                if let Err(e) = Self::update_process_status(
                    shared_mem_clone.clone(),
                    update.nonce,
                    update.status,
                    update.exit_code,
                ) {
                    eprintln!("Failed to update process status: {}", e);
                }
                let info_size = size_of::<ProcessInfo>();
                let offset = (update.nonce as usize % MAX_PROCESSES) * info_size;
                process_map_clone
                    .write()
                    .unwrap()
                    .insert(update.nonce, offset);
            }
        });

        Ok(Self {
            shared_mem,
            process_map,
            scope_to_nonce: Arc::new(RwLock::new(HashMap::new())),
            nonce_to_scope: Arc::new(RwLock::new(HashMap::new())),
            log_dir,
            status_tx,
            pty_sessions: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            source_generation: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        })
    }

    pub fn new() -> Result<Self, AgentError> {
        // Open/create shared memory file — truncate(false) preserves existing content across restarts
        #[allow(clippy::suspicious_open_options)]
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .open(shared_mem_path())?;
        file.set_len(SHARED_MEM_SIZE as u64)?;

        let resume_session = std::env::var("INTENDANT_RESUME_SESSION")
            .ok()
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        // Map shared memory. Preserve state across turns in the same caller session,
        // but reset when a new top-level session starts (unless explicitly resuming).
        let mut mmap = unsafe { MmapOptions::new().map_mut(&file)? };
        let session_id = Self::current_session_id();
        let marker_id = fs::read_to_string(runtime_session_marker_path())
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        let preserve_state = if resume_session {
            true
        } else if let Some(ref sid) = session_id {
            marker_id.as_deref() == Some(sid.as_str())
        } else {
            false
        };

        let process_map = if preserve_state {
            Self::rebuild_process_map(&mmap)
        } else {
            Self::reset_shared_mem(&mut mmap);
            HashMap::new()
        };
        if let Some(sid) = session_id {
            let _ = fs::write(runtime_session_marker_path(), sid);
        }
        let shared_mem = Arc::new(RwLock::new(mmap));
        let process_map = Arc::new(RwLock::new(process_map));

        // Resolve log directory (reuse existing session or create new)
        let log_dir = Self::resolve_log_dir()?;

        // Setup status channel
        let (status_tx, mut status_rx) = mpsc::channel::<StatusUpdate>(1024);

        // Start status monitor thread
        let shared_mem_clone = shared_mem.clone();
        let process_map_clone = process_map.clone();
        tokio::spawn(async move {
            while let Some(update) = status_rx.recv().await {
                if let Err(e) = Self::update_process_status(
                    shared_mem_clone.clone(),
                    update.nonce,
                    update.status,
                    update.exit_code,
                ) {
                    eprintln!("Failed to update process status: {}", e);
                }
                // Update process_map so StatusMonitor can see this nonce
                let info_size = size_of::<ProcessInfo>();
                let offset = (update.nonce as usize % MAX_PROCESSES) * info_size;
                process_map_clone
                    .write()
                    .unwrap()
                    .insert(update.nonce, offset);
            }
        });

        Ok(Self {
            shared_mem,
            process_map,
            scope_to_nonce: Arc::new(RwLock::new(HashMap::new())),
            nonce_to_scope: Arc::new(RwLock::new(HashMap::new())),
            log_dir,
            status_tx,
            pty_sessions: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            source_generation: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        })
    }

    fn resolve_log_dir() -> Result<PathBuf, AgentError> {
        if let Ok(existing) = fs::read_to_string(session_file_path()) {
            let path = PathBuf::from(existing.trim());
            if path.is_dir() {
                return Ok(path);
            }
        }
        let timestamp = Local::now().format("%Y%m%d_%H%M%S").to_string();
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        let log_dir = PathBuf::from(format!("{}/.intendant/logs/{}", home, timestamp));
        fs::create_dir_all(&log_dir)?;
        fs::write(session_file_path(), log_dir.to_string_lossy().as_bytes())?;
        Ok(log_dir)
    }

    fn rebuild_process_map(mmap: &MmapMut) -> HashMap<u64, usize> {
        let mut map = HashMap::new();
        let info_size = size_of::<ProcessInfo>();
        for i in 0..MAX_PROCESSES {
            let offset = i * info_size;
            let info = unsafe {
                std::ptr::read(mmap[offset..offset + info_size].as_ptr() as *const ProcessInfo)
            };
            if info.nonce != 0 {
                map.insert(info.nonce, offset);
            }
        }
        map
    }

    fn reset_shared_mem(mmap: &mut MmapMut) {
        mmap.fill(0);
    }

    fn scope_key(run_id: &str, agent_id: &str, attempt_id: &str, command_id: &str) -> String {
        format!("{}|{}|{}|{}", run_id, agent_id, attempt_id, command_id)
    }

    fn scope_parts(
        &self,
        cmd: &AgentCommand,
        default_command_id: String,
    ) -> (String, String, String, String) {
        let run_id = cmd
            .run_id
            .clone()
            .or_else(|| std::env::var("INTENDANT_RUN_ID").ok())
            .or_else(Self::current_session_id)
            .unwrap_or_else(|| "default".to_string());
        let agent_id = cmd
            .agent_id
            .clone()
            .or_else(|| std::env::var("INTENDANT_AGENT_ID").ok())
            .unwrap_or_else(|| "runtime".to_string());
        let attempt_id = cmd
            .attempt_id
            .clone()
            .or_else(|| std::env::var("INTENDANT_ATTEMPT_ID").ok())
            .unwrap_or_else(|| "0".to_string());
        let command_id = cmd.command_id.clone().unwrap_or(default_command_id);
        (run_id, agent_id, attempt_id, command_id)
    }

    fn register_command_scope(&self, cmd: &AgentCommand) {
        let (run_id, agent_id, attempt_id, command_id) =
            self.scope_parts(cmd, format!("nonce:{}", cmd.nonce));
        let key = Self::scope_key(&run_id, &agent_id, &attempt_id, &command_id);
        self.scope_to_nonce
            .write()
            .unwrap()
            .insert(key.clone(), cmd.nonce);
        self.nonce_to_scope.write().unwrap().insert(cmd.nonce, key);
    }

    fn lookup_scope_by_nonce(&self, nonce: u64) -> Option<(String, String, String, String)> {
        let key = self.nonce_to_scope.read().unwrap().get(&nonce).cloned()?;
        let mut parts = key.split('|');
        let run_id = parts.next()?.to_string();
        let agent_id = parts.next()?.to_string();
        let attempt_id = parts.next()?.to_string();
        let command_id = parts.next()?.to_string();
        Some((run_id, agent_id, attempt_id, command_id))
    }

    fn update_process_status(
        shared_mem: Arc<RwLock<MmapMut>>,
        nonce: u64,
        status: ProcessStatus,
        exit_code: i32,
    ) -> Result<(), AgentError> {
        let mut mmap = shared_mem.write().unwrap();
        let info_size = size_of::<ProcessInfo>();
        let offset = (nonce as usize % MAX_PROCESSES) * info_size;

        // Read existing entry to preserve PID
        let existing = unsafe {
            std::ptr::read(mmap[offset..offset + info_size].as_ptr() as *const ProcessInfo)
        };

        let info = ProcessInfo {
            nonce,
            pid: existing.pid,
            status,
            exit_code,
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        };

        let bytes = unsafe {
            std::slice::from_raw_parts(
                &info as *const ProcessInfo as *const u8,
                size_of::<ProcessInfo>(),
            )
        };
        mmap[offset..offset + info_size].copy_from_slice(bytes);
        Ok(())
    }

    async fn exec_as_agent(&self, cmd: &AgentCommand) -> Result<(), AgentError> {
        let command = cmd.command.as_ref().ok_or_else(|| {
            AgentError::Process("Command string is required for execAsAgent".to_string())
        })?;

        // Handle dependencies if any
        if let Some(dep_nonce) = cmd.depending_nonce {
            let wait = cmd.wait.unwrap_or(false);
            let expected_status = cmd.expected_status.unwrap_or(0);

            if !self
                .check_dependency(dep_nonce, expected_status, wait)
                .await?
            {
                self.status_tx
                    .send(StatusUpdate {
                        nonce: cmd.nonce,
                        status: ProcessStatus::Skipped,
                        exit_code: 0,
                    })
                    .await
                    .map_err(|e| AgentError::Process(e.to_string()))?;
                return Ok(());
            }
        }

        // Wait for port if requested
        if let Some(port) = cmd.wait_for_port {
            if !self.wait_for_port(port).await? {
                self.status_tx
                    .send(StatusUpdate {
                        nonce: cmd.nonce,
                        status: ProcessStatus::Failed,
                        exit_code: -2,
                    })
                    .await
                    .map_err(|e| AgentError::Process(e.to_string()))?;
                return Ok(());
            }
        }

        // Replace $NONCE references
        let command = self.replace_nonce_refs(command)?;

        // Setup output files for this command attempt.
        // Truncate old content to avoid stale log mixing between different runs/attempts.
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
        let display_id = cmd.display.unwrap_or(1);
        let mut child = Command::new("bash")
            .arg("-c")
            .arg(&command)
            .env("DISPLAY", format!(":{}", display_id))
            .stdout(Stdio::from(stdout_file))
            .stderr(Stdio::from(stderr_file))
            .spawn()?;

        // Update process info in shared memory
        let pid = child.id().unwrap_or(0) as i32;
        self.update_process_info(cmd.nonce, pid, ProcessStatus::Running, 0)?;

        // Monitor process in background
        let nonce = cmd.nonce;
        let status_tx = self.status_tx.clone();
        tokio::spawn(async move {
            match child.wait().await {
                Ok(status) => {
                    let exit_code = status.code().unwrap_or(-1);
                    let process_status = if exit_code == 0 {
                        ProcessStatus::Completed
                    } else {
                        ProcessStatus::Failed
                    };
                    if let Err(e) = status_tx
                        .send(StatusUpdate {
                            nonce,
                            status: process_status,
                            exit_code,
                        })
                        .await
                    {
                        eprintln!("Failed to send status update: {}", e);
                    }
                }
                Err(e) => {
                    eprintln!("Failed to wait for process: {}", e);
                    if let Err(e) = status_tx
                        .send(StatusUpdate {
                            nonce,
                            status: ProcessStatus::Failed,
                            exit_code: -1,
                        })
                        .await
                    {
                        eprintln!("Failed to send status update: {}", e);
                    }
                }
            }
        });

        Ok(())
    }

    async fn capture_screen(&self, cmd: &AgentCommand) -> Result<(), AgentError> {
        let display = cmd.display.unwrap_or(1);
        let screenshot_path = self.log_dir.join(format!("screenshot_{}.png", cmd.nonce));

        // Handle dependencies similarly to exec_as_agent
        if let Some(dep_nonce) = cmd.depending_nonce {
            let wait = cmd.wait.unwrap_or(false);
            let expected_status = cmd.expected_status.unwrap_or(0);

            if !self
                .check_dependency(dep_nonce, expected_status, wait)
                .await?
            {
                self.status_tx
                    .send(StatusUpdate {
                        nonce: cmd.nonce,
                        status: ProcessStatus::Skipped,
                        exit_code: 0,
                    })
                    .await
                    .map_err(|e| AgentError::Process(e.to_string()))?;
                return Ok(());
            }
        }

        // Use import command from ImageMagick
        let status = Command::new("import")
            .args([
                "-window",
                "root",
                "-display",
                &format!(":{}", display),
                &screenshot_path.to_string_lossy(),
            ])
            .status()
            .await?;
        let exit_code = status.code().unwrap_or(-1);
        let process_status = if status.success() {
            ProcessStatus::Completed
        } else {
            ProcessStatus::Failed
        };

        self.status_tx
            .send(StatusUpdate {
                nonce: cmd.nonce,
                status: process_status,
                exit_code,
            })
            .await
            .map_err(|e| AgentError::Process(e.to_string()))?;

        Ok(())
    }

    fn read_log_partial(
        &self,
        path: &Path,
        offset: Option<u64>,
        limit: Option<u64>,
    ) -> Result<String, AgentError> {
        if !path.exists() {
            return Ok(serde_json::json!({
                "content": "",
                "total_size": 0,
                "offset": 0,
                "bytes_read": 0
            })
            .to_string());
        }

        let mut file = std::fs::File::open(path)?;
        let total_size = file.metadata()?.len();

        let (read_offset, read_limit) = match (offset, limit) {
            (Some(o), Some(l)) => (o, l),
            (Some(o), None) => (o, total_size.saturating_sub(o)),
            (None, Some(l)) => (total_size.saturating_sub(l), l),
            (None, None) => {
                // Default: tail last 10KB
                let default_tail = 10 * 1024u64;
                (
                    total_size.saturating_sub(default_tail),
                    default_tail.min(total_size),
                )
            }
        };

        let actual_offset = read_offset.min(total_size);
        let actual_limit = read_limit.min(total_size.saturating_sub(actual_offset));

        file.seek(SeekFrom::Start(actual_offset))?;
        let mut buf = vec![0u8; actual_limit as usize];
        let bytes_read = file.read(&mut buf)?;
        buf.truncate(bytes_read);

        let content = String::from_utf8_lossy(&buf).to_string();

        Ok(serde_json::json!({
            "content": content,
            "total_size": total_size,
            "offset": actual_offset,
            "bytes_read": bytes_read
        })
        .to_string())
    }

    fn fetch_status(&self, cmd: &AgentCommand) -> Result<String, AgentError> {
        let status_type = cmd.status_type.as_ref().ok_or_else(|| {
            AgentError::Process("status_type is required for fetchStatus".to_string())
        })?;

        // Resolve target command using scoped identity when provided, otherwise fallback to nonce.
        let (req_run_id, req_agent_id, req_attempt_id, req_command_id) = self.scope_parts(
            cmd,
            format!("nonce:{}", cmd.depending_nonce.unwrap_or(cmd.nonce)),
        );
        let req_scope_key =
            Self::scope_key(&req_run_id, &req_agent_id, &req_attempt_id, &req_command_id);

        let target_nonce = if let Some(dep_nonce) = cmd.depending_nonce {
            dep_nonce
        } else if cmd.command_id.is_some() {
            match self
                .scope_to_nonce
                .read()
                .unwrap()
                .get(&req_scope_key)
                .copied()
            {
                Some(n) => n,
                None => {
                    return Ok(serde_json::json!({
                        "ok": false,
                        "status_type": status_type,
                        "run_id": req_run_id,
                        "agent_id": req_agent_id,
                        "attempt_id": req_attempt_id,
                        "command_id": req_command_id,
                        "error": "COMMAND_NOT_FOUND",
                        "complete": false,
                        "events": [],
                        "next_cursor": cmd.cursor.or(cmd.offset).unwrap_or(0),
                        "source_generation": self.source_generation
                    })
                    .to_string());
                }
            }
        } else {
            cmd.nonce
        };

        let (run_id, agent_id, attempt_id, command_id) =
            self.lookup_scope_by_nonce(target_nonce).unwrap_or((
                req_run_id.clone(),
                req_agent_id.clone(),
                req_attempt_id.clone(),
                format!("nonce:{}", target_nonce),
            ));
        let stream_id = cmd.stream_id.clone().unwrap_or_else(|| {
            format!(
                "{}:{}:{}:{}:{}",
                run_id, agent_id, attempt_id, command_id, status_type
            )
        });
        let cursor = cmd.cursor.or(cmd.offset).unwrap_or(0);
        let info = self.get_process_info(target_nonce).ok();
        let is_terminal = |s: ProcessStatus| {
            matches!(
                s,
                ProcessStatus::Completed | ProcessStatus::Failed | ProcessStatus::Skipped
            )
        };
        let mut consistency_flags: Vec<String> = Vec::new();
        if run_id != req_run_id || agent_id != req_agent_id || attempt_id != req_attempt_id {
            consistency_flags.push("scope_mismatch".to_string());
        }
        if cmd.command_id.is_some() && command_id != req_command_id {
            consistency_flags.push("command_mismatch".to_string());
        }

        match status_type.as_str() {
            "status" => Ok(match info {
                Some(i) => serde_json::json!({
                    "ok": true,
                    "status_type": "status",
                    "run_id": run_id,
                    "agent_id": agent_id,
                    "attempt_id": attempt_id,
                    "command_id": command_id,
                    "target_nonce": target_nonce,
                    "value": (i.status as u8 as char).to_string(),
                    "exit_code": i.exit_code,
                    "complete": is_terminal(i.status),
                    "events": [{
                        "event_seq": cursor + 1,
                        "ts": i.timestamp,
                        "type": "status",
                        "payload": {"value": (i.status as u8 as char).to_string(), "exit_code": i.exit_code}
                    }],
                    "next_cursor": cursor + 1,
                    "source_generation": self.source_generation,
                    "consistency_flags": consistency_flags.clone()
                })
                .to_string(),
                None => serde_json::json!({
                    "ok": false,
                    "status_type": "status",
                    "run_id": run_id,
                    "agent_id": agent_id,
                    "attempt_id": attempt_id,
                    "command_id": command_id,
                    "target_nonce": target_nonce,
                    "error": format!("nonce_not_found:{}", target_nonce),
                    "error_code": "COMMAND_NOT_FOUND",
                    "complete": false,
                    "events": [],
                    "next_cursor": cursor,
                    "source_generation": self.source_generation,
                    "consistency_flags": consistency_flags.clone()
                })
                .to_string(),
            }),
            "stdout" | "stderr" => {
                let path = self.log_dir.join(format!("{}_{}.log", target_nonce, status_type));
                if info.is_none() && !path.exists() {
                    return Ok(serde_json::json!({
                        "ok": false,
                        "status_type": status_type,
                        "run_id": run_id,
                        "agent_id": agent_id,
                        "attempt_id": attempt_id,
                        "command_id": command_id,
                        "stream_id": stream_id,
                        "target_nonce": target_nonce,
                        "error": format!("nonce_not_found:{}", target_nonce),
                        "error_code": "COMMAND_NOT_FOUND",
                        "complete": false,
                        "next_offset": cursor,
                        "next_cursor": cursor,
                        "stream_closed": false,
                        "events": [],
                        "source_generation": self.source_generation,
                        "consistency_flags": consistency_flags.clone(),
                        "data": {
                            "content": "",
                            "total_size": 0,
                            "offset": 0,
                            "bytes_read": 0
                        }
                    })
                    .to_string());
                }

                let read_offset = cmd.cursor.or(cmd.offset);
                let chunk = self.read_log_partial(&path, read_offset, cmd.limit)?;
                let data: serde_json::Value = serde_json::from_str(&chunk).unwrap_or_else(|_| {
                    serde_json::json!({
                        "content": "",
                        "total_size": 0,
                        "offset": 0,
                        "bytes_read": 0
                    })
                });
                let next_offset =
                    data["offset"].as_u64().unwrap_or(0) + data["bytes_read"].as_u64().unwrap_or(0);
                let complete = info.map(|i| is_terminal(i.status)).unwrap_or(false);
                let event_seq = next_offset.max(cursor);
                let event_ts = info.map(|i| i.timestamp).unwrap_or_else(|| {
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs()
                });
                let mut payload = serde_json::json!({
                    "ok": true,
                    "status_type": status_type,
                    "run_id": run_id,
                    "agent_id": agent_id,
                    "attempt_id": attempt_id,
                    "command_id": command_id,
                    "stream_id": stream_id,
                    "target_nonce": target_nonce,
                    "complete": complete,
                    "next_offset": next_offset,
                    "next_cursor": event_seq,
                    "stream_closed": complete,
                    "events": [{
                        "event_seq": event_seq,
                        "ts": event_ts,
                        "type": status_type,
                        "payload": data
                    }],
                    "source_generation": self.source_generation,
                    "consistency_flags": consistency_flags.clone(),
                    "data": data
                });
                if info.is_none() {
                    payload["warning"] =
                        serde_json::Value::String(format!("nonce_not_found:{}", target_nonce));
                }
                Ok(payload.to_string())
            }
            "exit_code" => Ok(match info {
                Some(i) => serde_json::json!({
                    "ok": true,
                    "status_type": "exit_code",
                    "run_id": run_id,
                    "agent_id": agent_id,
                    "attempt_id": attempt_id,
                    "command_id": command_id,
                    "target_nonce": target_nonce,
                    "value": i.exit_code,
                    "complete": is_terminal(i.status),
                    "events": [{
                        "event_seq": cursor + 1,
                        "ts": i.timestamp,
                        "type": "exit_code",
                        "payload": {"value": i.exit_code}
                    }],
                    "next_cursor": cursor + 1,
                    "source_generation": self.source_generation,
                    "consistency_flags": consistency_flags.clone()
                })
                .to_string(),
                None => serde_json::json!({
                    "ok": false,
                    "status_type": "exit_code",
                    "run_id": run_id,
                    "agent_id": agent_id,
                    "attempt_id": attempt_id,
                    "command_id": command_id,
                    "target_nonce": target_nonce,
                    "error": format!("nonce_not_found:{}", target_nonce),
                    "error_code": "COMMAND_NOT_FOUND",
                    "complete": false,
                    "events": [],
                    "next_cursor": cursor,
                    "source_generation": self.source_generation,
                    "consistency_flags": consistency_flags
                })
                .to_string(),
            }),
            _ => Err(AgentError::Process(format!(
                "Invalid status_type: {}",
                status_type
            ))),
        }
    }

    fn inspect_path(&self, cmd: &AgentCommand) -> Result<String, AgentError> {
        let path_str = cmd
            .path
            .as_ref()
            .ok_or_else(|| AgentError::Process("path is required for inspectPath".to_string()))?;
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

        let meta = fs::metadata(path)?;
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
        let ansi_re = regex::Regex::new(r"\x1b\[[0-9;]*[a-zA-Z]|\x1b\].*?\x07|\r").unwrap();
        let cleaned = ansi_re.replace_all(&output, "").to_string();

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
        self.ask_human_with_paths(
            cmd,
            &human_question_path(),
            &human_response_path(),
            HUMAN_TIMEOUT_MS,
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
            let entries = data["entries"].as_array_mut().unwrap();
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

    pub async fn process_input(&self, input: AgentInput) -> Result<Vec<String>, AgentError> {
        let mut results = Vec::new();

        // Pre-register all command nonces to avoid dependency race conditions.
        // Synchronous commands (editFile, inspectPath, etc.) must also be registered
        // so that downstream execAsAgent commands with depending_nonce can find them.
        for cmd in &input.commands {
            self.update_process_info(cmd.nonce, 0, ProcessStatus::Waiting, 0)?;
            self.register_command_scope(cmd);
        }

        // Start all commands asynchronously without waiting for completion
        for cmd in input.commands {
            match cmd.function.as_str() {
                "execAsAgent" => {
                    let agent = self.clone();
                    let cmd_clone = cmd.clone();
                    tokio::spawn(async move {
                        if let Err(e) = agent.exec_as_agent(&cmd_clone).await {
                            eprintln!("Error executing command {}: {}", cmd_clone.nonce, e);
                        }
                    });
                    if cmd.depending_nonce.is_some() {
                        results.push(format!("{}w0", cmd.nonce));
                    } else {
                        results.push(format!("{}r0", cmd.nonce));
                    }
                }
                "captureScreen" => {
                    let agent = self.clone();
                    let cmd_clone = cmd.clone();
                    tokio::spawn(async move {
                        if let Err(e) = agent.capture_screen(&cmd_clone).await {
                            eprintln!("Error capturing screen {}: {}", cmd_clone.nonce, e);
                        }
                    });
                    if cmd.depending_nonce.is_some() {
                        results.push(format!("{}w0", cmd.nonce));
                    } else {
                        results.push(format!("{}r0", cmd.nonce));
                    }
                }
                "fetchStatus" => {
                    // Wait for dependency if specified
                    if let Some(dep_nonce) = cmd.depending_nonce {
                        let wait = cmd.wait.unwrap_or(false);
                        let expected_status = cmd.expected_status.unwrap_or(0);
                        if wait {
                            self.check_dependency(dep_nonce, expected_status, true)
                                .await?;
                        }
                    }
                    match self.fetch_status(&cmd) {
                        Ok(status) => {
                            results.push(status);
                            self.update_process_info(cmd.nonce, 0, ProcessStatus::Completed, 0)?;
                        }
                        Err(e) => {
                            results.push(format!("Error: {}", e));
                            self.update_process_info(cmd.nonce, 0, ProcessStatus::Failed, 1)?;
                        }
                    }
                }
                "inspectPath" => match self.inspect_path(&cmd) {
                    Ok(result) => {
                        results.push(result);
                        self.update_process_info(cmd.nonce, 0, ProcessStatus::Completed, 0)?;
                    }
                    Err(e) => {
                        results.push(format!("Error: {}", e));
                        self.update_process_info(cmd.nonce, 0, ProcessStatus::Failed, 1)?;
                    }
                },
                "editFile" => match self.edit_file(&cmd) {
                    Ok(result) => {
                        results.push(result);
                        self.update_process_info(cmd.nonce, 0, ProcessStatus::Completed, 0)?;
                    }
                    Err(e) => {
                        results.push(format!("Error: {}", e));
                        self.update_process_info(cmd.nonce, 0, ProcessStatus::Failed, 1)?;
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
                            results.push(result);
                            self.update_process_info(cmd.nonce, 0, ProcessStatus::Completed, 0)?;
                        }
                        Err(e) => {
                            results.push(format!("Error: {}", e));
                            self.update_process_info(cmd.nonce, 0, ProcessStatus::Failed, 1)?;
                        }
                    }
                }
                "browse" => match self.browse(&cmd).await {
                    Ok(result) => {
                        results.push(result);
                        self.update_process_info(cmd.nonce, 0, ProcessStatus::Completed, 0)?;
                    }
                    Err(e) => {
                        results.push(format!("Error: {}", e));
                        self.update_process_info(cmd.nonce, 0, ProcessStatus::Failed, 1)?;
                    }
                },
                "askHuman" => match self.ask_human(&cmd).await {
                    Ok(result) => {
                        results.push(result);
                        self.update_process_info(cmd.nonce, 0, ProcessStatus::Completed, 0)?;
                    }
                    Err(e) => {
                        results.push(format!("Error: {}", e));
                        self.update_process_info(cmd.nonce, 0, ProcessStatus::Failed, 1)?;
                    }
                },
                "execPty" => match self.exec_pty(&cmd).await {
                    Ok(result) => {
                        results.push(result);
                        self.update_process_info(cmd.nonce, 0, ProcessStatus::Completed, 0)?;
                    }
                    Err(e) => {
                        results.push(format!("Error: {}", e));
                        self.update_process_info(cmd.nonce, 0, ProcessStatus::Failed, 1)?;
                    }
                },
                "storeMemory" => match self.store_memory(&cmd) {
                    Ok(result) => {
                        results.push(result);
                        self.update_process_info(cmd.nonce, 0, ProcessStatus::Completed, 0)?;
                    }
                    Err(e) => {
                        results.push(format!("Error: {}", e));
                        self.update_process_info(cmd.nonce, 0, ProcessStatus::Failed, 1)?;
                    }
                },
                "recallMemory" => match self.recall_memory(&cmd) {
                    Ok(result) => {
                        results.push(result);
                        self.update_process_info(cmd.nonce, 0, ProcessStatus::Completed, 0)?;
                    }
                    Err(e) => {
                        results.push(format!("Error: {}", e));
                        self.update_process_info(cmd.nonce, 0, ProcessStatus::Failed, 1)?;
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

        // Wait for the specified duration if requested
        if let Some(wait_time) = input.wait_for_status {
            tokio::time::sleep(Duration::from_millis(wait_time)).await;
        }

        // Return current results without waiting for completion
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
        let mut mmap = self.shared_mem.write().unwrap();
        let info_size = size_of::<ProcessInfo>();
        let offset = (nonce as usize % MAX_PROCESSES) * info_size;

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

        let bytes = unsafe {
            std::slice::from_raw_parts(
                &info as *const ProcessInfo as *const u8,
                size_of::<ProcessInfo>(),
            )
        };
        mmap[offset..offset + info_size].copy_from_slice(bytes);

        // Update process map
        let mut map = self.process_map.write().unwrap();
        map.insert(nonce, offset);

        Ok(())
    }

    fn get_process_info(&self, nonce: u64) -> Result<ProcessInfo, AgentError> {
        let map = self.process_map.read().unwrap();
        let offset = *map.get(&nonce).ok_or(AgentError::InvalidNonce(nonce))?;

        let mmap = self.shared_mem.read().unwrap();
        let info_slice = &mmap[offset..offset + size_of::<ProcessInfo>()];

        let info = unsafe { std::ptr::read(info_slice.as_ptr() as *const ProcessInfo) };

        Ok(info)
    }

    async fn check_dependency(
        &self,
        depending_nonce: u64,
        expected_status: i32,
        wait: bool,
    ) -> Result<bool, AgentError> {
        let mut retries = if wait { 100 } else { 1 };

        while retries > 0 {
            match self.get_process_info(depending_nonce) {
                Ok(info) => {
                    match info.status {
                        ProcessStatus::Completed if info.exit_code == expected_status => {
                            return Ok(true)
                        }
                        ProcessStatus::Completed
                        | ProcessStatus::Failed
                        | ProcessStatus::Skipped => {
                            // Terminal states - no point retrying
                            return Ok(false);
                        }
                        _ if !wait => return Ok(false),
                        _ => {
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            retries -= 1;
                            continue;
                        }
                    }
                }
                Err(AgentError::InvalidNonce(_)) => {
                    if !wait {
                        return Ok(false);
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    retries -= 1;
                }
                Err(_) if !wait => return Ok(false),
                Err(e) => {
                    eprintln!("Error checking dependency: {}", e);
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    retries -= 1;
                }
            }
        }

        Ok(false)
    }

    fn replace_nonce_refs(&self, command: &str) -> Result<String, AgentError> {
        let re = regex::Regex::new(r"\$NONCE\[(\d+)\]").unwrap();
        let mut result = command.to_string();

        for cap in re.captures_iter(command) {
            let nonce: u64 = cap[1].parse().map_err(|_| {
                AgentError::Process(format!("Invalid nonce reference: {}", &cap[1]))
            })?;

            let info = self.get_process_info(nonce)?;
            result = result.replace(&cap[0], &info.pid.to_string());
        }

        Ok(result)
    }
}

fn shared_dir() -> PathBuf {
    std::env::var("INTENDANT_SHARED_DIR")
        .map(PathBuf::from)
        .ok()
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| {
            let shm = PathBuf::from("/dev/shm");
            if shm.exists() {
                Some(shm)
            } else {
                None
            }
        })
        .unwrap_or_else(std::env::temp_dir)
}

fn shared_path(name: &str) -> PathBuf {
    shared_dir().join(name)
}

fn shared_mem_path() -> PathBuf {
    shared_path("intendant_processes")
}

fn session_file_path() -> PathBuf {
    shared_path("intendant_session")
}

fn runtime_session_marker_path() -> PathBuf {
    shared_path("intendant_runtime_session")
}

fn human_question_path() -> PathBuf {
    shared_path("intendant_human_question")
}

fn human_response_path() -> PathBuf {
    shared_path("intendant_human_response")
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

    /// Create a test agent with temp directories
    fn create_test_agent() -> (Agent, TempDir, TempDir) {
        let shm_dir = TempDir::new().unwrap();
        let log_dir = TempDir::new().unwrap();
        let shm_path = shm_dir.path().join("test_processes");
        let agent = Agent::new_with_paths(shm_path.to_str().unwrap(), log_dir.path().to_path_buf())
            .unwrap();
        (agent, shm_dir, log_dir)
    }

    #[tokio::test]
    async fn update_and_get_process_info() {
        let (agent, _shm, _log) = create_test_agent();
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
        let (agent, _shm, _log) = create_test_agent();
        let result = agent.get_process_info(999);
        assert!(result.is_err());
        match result.unwrap_err() {
            AgentError::InvalidNonce(n) => assert_eq!(n, 999),
            other => panic!("expected InvalidNonce, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn replace_nonce_refs_single() {
        let (agent, _shm, _log) = create_test_agent();
        agent
            .update_process_info(1, 4567, ProcessStatus::Running, 0)
            .unwrap();
        let result = agent.replace_nonce_refs("kill $NONCE[1]").unwrap();
        assert_eq!(result, "kill 4567");
    }

    #[tokio::test]
    async fn replace_nonce_refs_multiple() {
        let (agent, _shm, _log) = create_test_agent();
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
        let (agent, _shm, _log) = create_test_agent();
        let result = agent.replace_nonce_refs("echo hello").unwrap();
        assert_eq!(result, "echo hello");
    }

    #[tokio::test]
    async fn replace_nonce_refs_invalid_nonce() {
        let (agent, _shm, _log) = create_test_agent();
        let result = agent.replace_nonce_refs("kill $NONCE[999]");
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn inspect_path_existing_file() {
        let (agent, _shm, _log) = create_test_agent();
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
        let (agent, _shm, _log) = create_test_agent();
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
        let (agent, _shm, _log) = create_test_agent();
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
        let (agent, _shm, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "inspectPath".to_string(),
            nonce: 1,
            ..Default::default()
        };
        let result = agent.inspect_path(&cmd);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn fetch_status_returns_status_char() {
        let (agent, _shm, _log) = create_test_agent();
        agent
            .update_process_info(5, 1000, ProcessStatus::Running, 0)
            .unwrap();
        let cmd = AgentCommand {
            function: "fetchStatus".to_string(),
            nonce: 5,
            status_type: Some("status".to_string()),
            ..Default::default()
        };
        let result = agent.fetch_status(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["ok"], true);
        assert_eq!(parsed["value"], "r");
        assert_eq!(parsed["target_nonce"], 5);
    }

    #[tokio::test]
    async fn fetch_status_returns_exit_code() {
        let (agent, _shm, _log) = create_test_agent();
        agent
            .update_process_info(5, 1000, ProcessStatus::Failed, 127)
            .unwrap();
        let cmd = AgentCommand {
            function: "fetchStatus".to_string(),
            nonce: 5,
            status_type: Some("exit_code".to_string()),
            ..Default::default()
        };
        let result = agent.fetch_status(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["ok"], true);
        assert_eq!(parsed["value"], 127);
        assert_eq!(parsed["target_nonce"], 5);
    }

    #[tokio::test]
    async fn fetch_status_stdout_reads_log() {
        let (agent, _shm, log_dir) = create_test_agent();
        agent
            .update_process_info(3, 100, ProcessStatus::Completed, 0)
            .unwrap();
        // Create the log file manually
        fs::write(log_dir.path().join("3_stdout.log"), "hello world").unwrap();
        let cmd = AgentCommand {
            function: "fetchStatus".to_string(),
            nonce: 3,
            status_type: Some("stdout".to_string()),
            ..Default::default()
        };
        let result = agent.fetch_status(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["ok"], true);
        assert_eq!(parsed["data"]["content"], "hello world");
        assert_eq!(parsed["data"]["total_size"], 11);
    }

    #[tokio::test]
    async fn fetch_status_uses_depending_nonce_as_target() {
        let (agent, _shm, log_dir) = create_test_agent();
        agent
            .update_process_info(100, 5000, ProcessStatus::Completed, 0)
            .unwrap();
        // Log file is named after the exec command's nonce (100)
        fs::write(log_dir.path().join("100_stdout.log"), "target output").unwrap();
        // fetchStatus has its own nonce (101) but references target via depending_nonce
        let cmd = AgentCommand {
            function: "fetchStatus".to_string(),
            nonce: 101,
            depending_nonce: Some(100),
            status_type: Some("stdout".to_string()),
            ..Default::default()
        };
        let result = agent.fetch_status(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["ok"], true);
        assert_eq!(parsed["target_nonce"], 100);
        assert_eq!(parsed["data"]["content"], "target output");
        assert_eq!(parsed["data"]["total_size"], 13);
    }

    #[tokio::test]
    async fn fetch_status_depending_nonce_status() {
        let (agent, _shm, _log) = create_test_agent();
        agent
            .update_process_info(100, 5000, ProcessStatus::Completed, 42)
            .unwrap();
        let cmd = AgentCommand {
            function: "fetchStatus".to_string(),
            nonce: 101,
            depending_nonce: Some(100),
            status_type: Some("exit_code".to_string()),
            ..Default::default()
        };
        let result = agent.fetch_status(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["ok"], true);
        assert_eq!(parsed["value"], 42);
        assert_eq!(parsed["target_nonce"], 100);
    }

    #[tokio::test]
    async fn fetch_status_missing_status_type() {
        let (agent, _shm, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "fetchStatus".to_string(),
            nonce: 1,
            ..Default::default()
        };
        let result = agent.fetch_status(&cmd);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn fetch_status_invalid_status_type() {
        let (agent, _shm, _log) = create_test_agent();
        agent
            .update_process_info(1, 100, ProcessStatus::Running, 0)
            .unwrap();
        let cmd = AgentCommand {
            function: "fetchStatus".to_string(),
            nonce: 1,
            status_type: Some("invalid".to_string()),
            ..Default::default()
        };
        let result = agent.fetch_status(&cmd);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn fetch_status_unknown_nonce_returns_structured_error() {
        let (agent, _shm, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "fetchStatus".to_string(),
            nonce: 4242,
            status_type: Some("status".to_string()),
            ..Default::default()
        };
        let result = agent.fetch_status(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["ok"], false);
        assert_eq!(parsed["target_nonce"], 4242);
        assert!(parsed["error"]
            .as_str()
            .unwrap()
            .contains("nonce_not_found"));
    }

    #[tokio::test]
    async fn fetch_status_stdout_missing_log_file() {
        let (agent, _shm, _log) = create_test_agent();
        agent
            .update_process_info(99, 100, ProcessStatus::Waiting, 0)
            .unwrap();
        // Don't create the log file - it doesn't exist for Waiting processes
        let cmd = AgentCommand {
            function: "fetchStatus".to_string(),
            nonce: 99,
            status_type: Some("stdout".to_string()),
            ..Default::default()
        };
        let result = agent.fetch_status(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["ok"], true);
        assert_eq!(parsed["data"]["content"], "");
        assert_eq!(parsed["data"]["total_size"], 0);
        assert_eq!(parsed["data"]["bytes_read"], 0);
    }

    #[tokio::test]
    async fn check_dependency_completed_matching_exit() {
        let (agent, _shm, _log) = create_test_agent();
        agent
            .update_process_info(1, 100, ProcessStatus::Completed, 0)
            .unwrap();
        let result = agent.check_dependency(1, 0, false).await.unwrap();
        assert!(result, "should pass when exit code matches");
    }

    #[tokio::test]
    async fn check_dependency_completed_wrong_exit_no_wait() {
        let (agent, _shm, _log) = create_test_agent();
        agent
            .update_process_info(1, 100, ProcessStatus::Completed, 1)
            .unwrap();
        let result = agent.check_dependency(1, 0, false).await.unwrap();
        assert!(!result, "should fail when exit code doesn't match");
    }

    #[tokio::test]
    async fn check_dependency_completed_wrong_exit_with_wait() {
        let (agent, _shm, _log) = create_test_agent();
        agent
            .update_process_info(1, 100, ProcessStatus::Completed, 1)
            .unwrap();
        let start = std::time::Instant::now();
        let result = agent.check_dependency(1, 0, true).await.unwrap();
        let elapsed = start.elapsed();
        assert!(!result, "should fail when exit code doesn't match");
        // Completed is a terminal state, should return immediately even with wait=true
        assert!(
            elapsed.as_secs() < 2,
            "check_dependency took {:?} - should be instant for completed process with wrong exit code",
            elapsed
        );
    }

    #[tokio::test]
    async fn check_dependency_failed() {
        let (agent, _shm, _log) = create_test_agent();
        agent
            .update_process_info(1, 100, ProcessStatus::Failed, 1)
            .unwrap();
        let result = agent.check_dependency(1, 0, true).await.unwrap();
        assert!(!result, "should fail on Failed status");
    }

    #[tokio::test]
    async fn check_dependency_skipped() {
        let (agent, _shm, _log) = create_test_agent();
        agent
            .update_process_info(1, 100, ProcessStatus::Skipped, 0)
            .unwrap();
        let result = agent.check_dependency(1, 0, true).await.unwrap();
        assert!(!result, "should fail on Skipped status");
    }

    #[tokio::test]
    async fn check_dependency_invalid_nonce_no_wait() {
        let (agent, _shm, _log) = create_test_agent();
        let result = agent.check_dependency(999, 0, false).await.unwrap();
        assert!(!result, "should fail for invalid nonce without wait");
    }

    #[tokio::test]
    async fn process_input_exec_returns_running() {
        let (agent, _shm, _log) = create_test_agent();
        let input = AgentInput {
            commands: vec![AgentCommand {
                function: "execAsAgent".to_string(),
                command: Some("echo hello".to_string()),
                nonce: 1,
                display: Some(1),
                ..Default::default()
            }],
            wait_for_status: None,
        };
        let results = agent.process_input(input).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], "1r0");
    }

    #[tokio::test]
    async fn process_input_exec_with_dependency_returns_waiting() {
        let (agent, _shm, _log) = create_test_agent();
        let input = AgentInput {
            commands: vec![AgentCommand {
                function: "execAsAgent".to_string(),
                command: Some("echo hello".to_string()),
                nonce: 2,
                depending_nonce: Some(1),
                expected_status: Some(0),
                wait: Some(true),
                display: Some(1),
                ..Default::default()
            }],
            wait_for_status: None,
        };
        let results = agent.process_input(input).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], "2w0");
    }

    #[tokio::test]
    async fn process_input_unknown_function() {
        let (agent, _shm, _log) = create_test_agent();
        let input = AgentInput {
            commands: vec![AgentCommand {
                function: "unknownFunc".to_string(),
                nonce: 1,
                ..Default::default()
            }],
            wait_for_status: None,
        };
        let result = agent.process_input(input).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn process_input_inspect_path() {
        let (agent, _shm, _log) = create_test_agent();
        let input = AgentInput {
            commands: vec![AgentCommand {
                function: "inspectPath".to_string(),
                nonce: 1,
                path: Some("/tmp".to_string()),
                ..Default::default()
            }],
            wait_for_status: None,
        };
        let results = agent.process_input(input).await.unwrap();
        assert_eq!(results.len(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&results[0]).unwrap();
        assert_eq!(parsed["exists"], true);
        assert_eq!(parsed["type"], "directory");
    }

    #[tokio::test]
    async fn process_input_with_wait_for_status() {
        let (agent, _shm, _log) = create_test_agent();
        let input = AgentInput {
            commands: vec![AgentCommand {
                function: "execAsAgent".to_string(),
                command: Some("echo fast".to_string()),
                nonce: 1,
                display: Some(1),
                ..Default::default()
            }],
            wait_for_status: Some(200),
        };
        let start = std::time::Instant::now();
        let results = agent.process_input(input).await.unwrap();
        let elapsed = start.elapsed();
        assert_eq!(results.len(), 1);
        // Should have waited at least 200ms
        assert!(
            elapsed.as_millis() >= 150,
            "should have waited ~200ms, took {:?}",
            elapsed
        );
    }

    #[tokio::test]
    async fn exec_as_agent_creates_log_files() {
        let (agent, _shm, log_dir) = create_test_agent();
        let cmd = AgentCommand {
            function: "execAsAgent".to_string(),
            command: Some("echo test_output".to_string()),
            nonce: 10,
            display: Some(1),
            ..Default::default()
        };
        agent.exec_as_agent(&cmd).await.unwrap();
        // Give the command time to complete
        tokio::time::sleep(Duration::from_millis(500)).await;

        let stdout_path = log_dir.path().join("10_stdout.log");
        let stderr_path = log_dir.path().join("10_stderr.log");
        assert!(stdout_path.exists(), "stdout log should be created");
        assert!(stderr_path.exists(), "stderr log should be created");
        let stdout_content = fs::read_to_string(stdout_path).unwrap();
        assert_eq!(stdout_content.trim(), "test_output");
    }

    #[tokio::test]
    async fn exec_as_agent_missing_command() {
        let (agent, _shm, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "execAsAgent".to_string(),
            nonce: 1,
            ..Default::default()
        };
        let result = agent.exec_as_agent(&cmd).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn update_process_status_preserves_pid() {
        let (agent, _shm, _log) = create_test_agent();
        // Set a process with a real PID
        agent
            .update_process_info(1, 9999, ProcessStatus::Running, 0)
            .unwrap();

        // Simulate status update (what happens when process completes)
        Agent::update_process_status(agent.shared_mem.clone(), 1, ProcessStatus::Completed, 0)
            .unwrap();

        // Read the process info back - PID should still be 9999
        let info = agent.get_process_info(1).unwrap();
        assert_eq!(info.status, ProcessStatus::Completed);
        assert_eq!(
            info.pid, 9999,
            "PID should be preserved after status update"
        );
    }

    #[tokio::test]
    async fn rebuild_process_map_finds_existing_entries() {
        let (agent, _shm, _log) = create_test_agent();
        agent
            .update_process_info(10, 100, ProcessStatus::Running, 0)
            .unwrap();
        agent
            .update_process_info(20, 200, ProcessStatus::Completed, 0)
            .unwrap();

        let mmap = agent.shared_mem.read().unwrap();
        let map = Agent::rebuild_process_map(&mmap);
        assert!(map.contains_key(&10));
        assert!(map.contains_key(&20));
        assert!(!map.contains_key(&30));
    }

    // --- Log Tail tests ---

    #[tokio::test]
    async fn fetch_status_default_tail() {
        let (agent, _shm, log_dir) = create_test_agent();
        // Create a small file (< 10KB)
        let content = "a".repeat(500);
        fs::write(log_dir.path().join("1_stdout.log"), &content).unwrap();
        agent
            .update_process_info(1, 100, ProcessStatus::Completed, 0)
            .unwrap();
        let cmd = AgentCommand {
            function: "fetchStatus".to_string(),
            nonce: 1,
            status_type: Some("stdout".to_string()),
            ..Default::default()
        };
        let result = agent.fetch_status(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["data"]["content"].as_str().unwrap().len(), 500);
        assert_eq!(parsed["data"]["total_size"], 500);
    }

    #[tokio::test]
    async fn fetch_status_default_tail_large_file() {
        let (agent, _shm, log_dir) = create_test_agent();
        // Create a file larger than 10KB
        let content = "x".repeat(20_000);
        fs::write(log_dir.path().join("1_stdout.log"), &content).unwrap();
        agent
            .update_process_info(1, 100, ProcessStatus::Completed, 0)
            .unwrap();
        let cmd = AgentCommand {
            function: "fetchStatus".to_string(),
            nonce: 1,
            status_type: Some("stdout".to_string()),
            ..Default::default()
        };
        let result = agent.fetch_status(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        // Default tail is 10KB = 10240 bytes
        assert_eq!(parsed["data"]["bytes_read"], 10240);
        assert_eq!(parsed["data"]["total_size"], 20000);
        assert_eq!(parsed["data"]["offset"], 20000 - 10240);
    }

    #[tokio::test]
    async fn fetch_status_offset_and_limit() {
        let (agent, _shm, log_dir) = create_test_agent();
        fs::write(log_dir.path().join("1_stdout.log"), "0123456789").unwrap();
        agent
            .update_process_info(1, 100, ProcessStatus::Completed, 0)
            .unwrap();
        let cmd = AgentCommand {
            function: "fetchStatus".to_string(),
            nonce: 1,
            status_type: Some("stdout".to_string()),
            offset: Some(3),
            limit: Some(4),
            ..Default::default()
        };
        let result = agent.fetch_status(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["data"]["content"], "3456");
        assert_eq!(parsed["data"]["offset"], 3);
        assert_eq!(parsed["data"]["bytes_read"], 4);
        assert_eq!(parsed["data"]["total_size"], 10);
    }

    #[tokio::test]
    async fn fetch_status_offset_only() {
        let (agent, _shm, log_dir) = create_test_agent();
        fs::write(log_dir.path().join("1_stdout.log"), "0123456789").unwrap();
        agent
            .update_process_info(1, 100, ProcessStatus::Completed, 0)
            .unwrap();
        let cmd = AgentCommand {
            function: "fetchStatus".to_string(),
            nonce: 1,
            status_type: Some("stdout".to_string()),
            offset: Some(7),
            ..Default::default()
        };
        let result = agent.fetch_status(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["data"]["content"], "789");
        assert_eq!(parsed["data"]["offset"], 7);
        assert_eq!(parsed["data"]["bytes_read"], 3);
    }

    #[tokio::test]
    async fn fetch_status_limit_only() {
        let (agent, _shm, log_dir) = create_test_agent();
        fs::write(log_dir.path().join("1_stdout.log"), "0123456789").unwrap();
        agent
            .update_process_info(1, 100, ProcessStatus::Completed, 0)
            .unwrap();
        let cmd = AgentCommand {
            function: "fetchStatus".to_string(),
            nonce: 1,
            status_type: Some("stdout".to_string()),
            limit: Some(3),
            ..Default::default()
        };
        let result = agent.fetch_status(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["data"]["content"], "789");
        assert_eq!(parsed["data"]["offset"], 7);
        assert_eq!(parsed["data"]["bytes_read"], 3);
    }

    #[tokio::test]
    async fn fetch_status_offset_beyond_file() {
        let (agent, _shm, log_dir) = create_test_agent();
        fs::write(log_dir.path().join("1_stdout.log"), "short").unwrap();
        agent
            .update_process_info(1, 100, ProcessStatus::Completed, 0)
            .unwrap();
        let cmd = AgentCommand {
            function: "fetchStatus".to_string(),
            nonce: 1,
            status_type: Some("stdout".to_string()),
            offset: Some(1000),
            ..Default::default()
        };
        let result = agent.fetch_status(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["data"]["content"], "");
        assert_eq!(parsed["data"]["bytes_read"], 0);
        assert_eq!(parsed["data"]["total_size"], 5);
    }

    #[tokio::test]
    async fn fetch_status_stderr_partial() {
        let (agent, _shm, log_dir) = create_test_agent();
        fs::write(log_dir.path().join("1_stderr.log"), "error output here").unwrap();
        agent
            .update_process_info(1, 100, ProcessStatus::Completed, 0)
            .unwrap();
        let cmd = AgentCommand {
            function: "fetchStatus".to_string(),
            nonce: 1,
            status_type: Some("stderr".to_string()),
            offset: Some(6),
            limit: Some(6),
            ..Default::default()
        };
        let result = agent.fetch_status(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["data"]["content"], "output");
    }

    // --- editFile tests ---

    #[tokio::test]
    async fn edit_file_write_creates_file() {
        let (agent, _shm, _log) = create_test_agent();
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
        let (agent, _shm, _log) = create_test_agent();
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
        let (agent, _shm, _log) = create_test_agent();
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
        let (agent, _shm, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let fp = tmp.path().join("append.txt");
        fs::write(&fp, "first").unwrap();
        let cmd = AgentCommand {
            function: "editFile".to_string(),
            nonce: 1,
            file_path: Some(fp.to_string_lossy().to_string()),
            operation: Some("append".to_string()),
            content: Some("_second".to_string()),
            ..Default::default()
        };
        let result = agent.edit_file(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["success"], true);
        assert_eq!(fs::read_to_string(&fp).unwrap(), "first_second");
    }

    #[tokio::test]
    async fn edit_file_replace_found() {
        let (agent, _shm, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let fp = tmp.path().join("replace.txt");
        fs::write(&fp, "hello world hello").unwrap();
        let cmd = AgentCommand {
            function: "editFile".to_string(),
            nonce: 1,
            file_path: Some(fp.to_string_lossy().to_string()),
            operation: Some("replace".to_string()),
            match_content: Some("hello".to_string()),
            content: Some("hi".to_string()),
            ..Default::default()
        };
        let result = agent.edit_file(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["success"], true);
        assert_eq!(parsed["replacements"], 2);
        assert_eq!(fs::read_to_string(&fp).unwrap(), "hi world hi");
    }

    #[tokio::test]
    async fn edit_file_replace_not_found() {
        let (agent, _shm, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let fp = tmp.path().join("replace.txt");
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
        let (agent, _shm, _log) = create_test_agent();
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
        let result = agent.edit_file(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["success"], true);
        let content = fs::read_to_string(&fp).unwrap();
        assert!(content.starts_with("line0\n"));
    }

    #[tokio::test]
    async fn edit_file_insert_at_end() {
        let (agent, _shm, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let fp = tmp.path().join("insert.txt");
        fs::write(&fp, "line1\nline2").unwrap();
        let cmd = AgentCommand {
            function: "editFile".to_string(),
            nonce: 1,
            file_path: Some(fp.to_string_lossy().to_string()),
            operation: Some("insert_at".to_string()),
            content: Some("line3".to_string()),
            line_number: Some(100),
            ..Default::default()
        };
        agent.edit_file(&cmd).unwrap();
        let content = fs::read_to_string(&fp).unwrap();
        assert!(content.contains("line2\nline3"));
    }

    #[tokio::test]
    async fn edit_file_replace_lines() {
        let (agent, _shm, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let fp = tmp.path().join("rlines.txt");
        fs::write(&fp, "a\nb\nc\nd\ne").unwrap();
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
        let result = agent.edit_file(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["success"], true);
        let content = fs::read_to_string(&fp).unwrap();
        assert_eq!(content, "a\nX\nd\ne");
    }

    #[tokio::test]
    async fn edit_file_replace_lines_end_before_start() {
        let (agent, _shm, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let fp = tmp.path().join("rlines.txt");
        fs::write(&fp, "a\nb\nc").unwrap();
        let cmd = AgentCommand {
            function: "editFile".to_string(),
            nonce: 1,
            file_path: Some(fp.to_string_lossy().to_string()),
            operation: Some("replace_lines".to_string()),
            content: Some("X".to_string()),
            line_number: Some(3),
            end_line: Some(1),
            ..Default::default()
        };
        let result = agent.edit_file(&cmd);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn edit_file_missing_fields() {
        let (agent, _shm, _log) = create_test_agent();
        // Missing file_path
        let cmd = AgentCommand {
            function: "editFile".to_string(),
            nonce: 1,
            operation: Some("write".to_string()),
            ..Default::default()
        };
        assert!(agent.edit_file(&cmd).is_err());

        // Missing operation
        let cmd = AgentCommand {
            function: "editFile".to_string(),
            nonce: 1,
            file_path: Some("/tmp/test".to_string()),
            ..Default::default()
        };
        assert!(agent.edit_file(&cmd).is_err());
    }

    #[tokio::test]
    async fn edit_file_unknown_operation() {
        let (agent, _shm, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "editFile".to_string(),
            nonce: 1,
            file_path: Some("/tmp/test".to_string()),
            operation: Some("unknown_op".to_string()),
            ..Default::default()
        };
        assert!(agent.edit_file(&cmd).is_err());
    }

    #[tokio::test]
    async fn edit_file_process_input_integration() {
        let (agent, _shm, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let fp = tmp.path().join("integration.txt");
        let input = AgentInput {
            commands: vec![AgentCommand {
                function: "editFile".to_string(),
                nonce: 1,
                file_path: Some(fp.to_string_lossy().to_string()),
                operation: Some("write".to_string()),
                content: Some("via process_input".to_string()),
                ..Default::default()
            }],
            wait_for_status: None,
        };
        let results = agent.process_input(input).await.unwrap();
        assert_eq!(results.len(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&results[0]).unwrap();
        assert_eq!(parsed["success"], true);
        assert_eq!(fs::read_to_string(&fp).unwrap(), "via process_input");
    }

    // --- browse tests ---

    #[tokio::test]
    async fn browse_missing_url() {
        let (agent, _shm, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "browse".to_string(),
            nonce: 1,
            ..Default::default()
        };
        let result = agent.browse(&cmd).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn browse_invalid_scheme() {
        let (agent, _shm, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "browse".to_string(),
            nonce: 1,
            url: Some("ftp://example.com".to_string()),
            ..Default::default()
        };
        let result = agent.browse(&cmd).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn browse_html_conversion() {
        // Start a simple HTTP server that returns HTML
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await;
            let html = "<html><body><h1>Hello</h1><p>World</p></body></html>";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{}",
                html.len(),
                html
            );
            let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes()).await;
        });
        let (agent, _shm, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "browse".to_string(),
            nonce: 1,
            url: Some(format!("http://{}", addr)),
            ..Default::default()
        };
        let result = agent.browse(&cmd).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["success"], true);
        assert_eq!(parsed["status"], 200);
        let content = parsed["content"].as_str().unwrap();
        assert!(
            content.contains("Hello"),
            "should contain converted text, got: {}",
            content
        );
        assert!(
            content.contains("World"),
            "should contain converted text, got: {}",
            content
        );
    }

    #[tokio::test]
    async fn browse_plain_text_passthrough() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await;
            let body = "plain text content";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes()).await;
        });
        let (agent, _shm, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "browse".to_string(),
            nonce: 1,
            url: Some(format!("http://{}", addr)),
            ..Default::default()
        };
        let result = agent.browse(&cmd).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["content"], "plain text content");
    }

    #[tokio::test]
    async fn browse_http_error_status() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await;
            let body = "Not Found";
            let response = format!(
                "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes()).await;
        });
        let (agent, _shm, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "browse".to_string(),
            nonce: 1,
            url: Some(format!("http://{}", addr)),
            ..Default::default()
        };
        let result = agent.browse(&cmd).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["status"], 404);
    }

    #[tokio::test]
    async fn browse_large_content_truncation() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await;
            let body = "x".repeat(60_000);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes()).await;
        });
        let (agent, _shm, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "browse".to_string(),
            nonce: 1,
            url: Some(format!("http://{}", addr)),
            ..Default::default()
        };
        let result = agent.browse(&cmd).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["truncated"], true);
        assert_eq!(parsed["content"].as_str().unwrap().len(), 50 * 1024);
    }

    #[tokio::test]
    async fn browse_process_input_integration() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await;
            let body = "ok";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes()).await;
        });
        let (agent, _shm, _log) = create_test_agent();
        let input = AgentInput {
            commands: vec![AgentCommand {
                function: "browse".to_string(),
                nonce: 1,
                url: Some(format!("http://{}", addr)),
                ..Default::default()
            }],
            wait_for_status: None,
        };
        let results = agent.process_input(input).await.unwrap();
        assert_eq!(results.len(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&results[0]).unwrap();
        assert_eq!(parsed["success"], true);
    }

    // --- wait_for_port tests ---

    #[tokio::test]
    async fn wait_for_port_already_open() {
        let (agent, _shm, _log) = create_test_agent();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let result = agent
            .wait_for_port_with_retries(port, 3, 100)
            .await
            .unwrap();
        assert!(result, "should succeed when port is already open");
    }

    #[tokio::test]
    async fn wait_for_port_opens_during_wait() {
        let (agent, _shm, _log) = create_test_agent();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // Drop listener to close port, then reopen after a delay
        drop(listener);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let _listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{}", port))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        let result = agent
            .wait_for_port_with_retries(port, 20, 100)
            .await
            .unwrap();
        assert!(result, "should succeed when port opens during wait");
    }

    #[tokio::test]
    async fn wait_for_port_timeout() {
        let (agent, _shm, _log) = create_test_agent();
        // Use a port that is very likely not open
        let result = agent
            .wait_for_port_with_retries(19999, 3, 50)
            .await
            .unwrap();
        assert!(!result, "should fail when port never opens");
    }

    #[tokio::test]
    async fn exec_as_agent_wait_for_port_success() {
        let (agent, _shm, log_dir) = create_test_agent();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let cmd = AgentCommand {
            function: "execAsAgent".to_string(),
            command: Some("echo port_ready".to_string()),
            nonce: 1,
            wait_for_port: Some(port),
            display: Some(1),
            ..Default::default()
        };
        agent.exec_as_agent(&cmd).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        // The command should have run since the port was open
        let stdout_path = log_dir.path().join("1_stdout.log");
        assert!(stdout_path.exists(), "stdout log should exist");
        let content = fs::read_to_string(stdout_path).unwrap();
        assert_eq!(content.trim(), "port_ready");
    }

    // --- askHuman tests ---

    #[tokio::test]
    async fn ask_human_missing_question() {
        let (agent, _shm, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "askHuman".to_string(),
            nonce: 1,
            ..Default::default()
        };
        let tmp = TempDir::new().unwrap();
        let q_path = tmp.path().join("question");
        let r_path = tmp.path().join("response");
        let result = agent
            .ask_human_with_paths(&cmd, &q_path, &r_path, 1000, 100)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn ask_human_response_already_available() {
        let (agent, _shm, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let q_path = tmp.path().join("question");
        let r_path = tmp.path().join("response");
        // Pre-create response file
        fs::write(&r_path, "the answer").unwrap();
        let cmd = AgentCommand {
            function: "askHuman".to_string(),
            nonce: 1,
            question: Some("what is the answer?".to_string()),
            ..Default::default()
        };
        let result = agent
            .ask_human_with_paths(&cmd, &q_path, &r_path, 5000, 100)
            .await
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["success"], true);
        assert_eq!(parsed["question"], "what is the answer?");
        assert_eq!(parsed["response"], "the answer");
        // Files should be cleaned up
        assert!(!q_path.exists());
        assert!(!r_path.exists());
    }

    #[tokio::test]
    async fn ask_human_response_after_delay() {
        let (agent, _shm, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let q_path = tmp.path().join("question");
        let r_path = tmp.path().join("response");
        let r_path_clone = r_path.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            fs::write(&r_path_clone, "delayed answer").unwrap();
        });
        let cmd = AgentCommand {
            function: "askHuman".to_string(),
            nonce: 1,
            question: Some("waiting for answer".to_string()),
            ..Default::default()
        };
        let result = agent
            .ask_human_with_paths(&cmd, &q_path, &r_path, 5000, 100)
            .await
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["success"], true);
        assert_eq!(parsed["response"], "delayed answer");
    }

    #[tokio::test]
    async fn ask_human_timeout() {
        let (agent, _shm, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let q_path = tmp.path().join("question");
        let r_path = tmp.path().join("response");
        let cmd = AgentCommand {
            function: "askHuman".to_string(),
            nonce: 1,
            question: Some("will time out".to_string()),
            ..Default::default()
        };
        let result = agent
            .ask_human_with_paths(&cmd, &q_path, &r_path, 300, 100)
            .await
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["success"], false);
        assert!(parsed["error"].as_str().unwrap().contains("Timed out"));
    }

    #[tokio::test]
    async fn ask_human_question_file_content() {
        let (agent, _shm, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let q_path = tmp.path().join("question");
        let r_path = tmp.path().join("response");
        // Pre-create response so it returns immediately
        fs::write(&r_path, "ok").unwrap();
        let cmd = AgentCommand {
            function: "askHuman".to_string(),
            nonce: 1,
            question: Some("my question here".to_string()),
            ..Default::default()
        };
        agent
            .ask_human_with_paths(&cmd, &q_path, &r_path, 5000, 100)
            .await
            .unwrap();
        // Question file should have been cleaned up, but we can verify the write happened
        // by checking the response worked (question was written first, then response read)
    }

    #[tokio::test]
    async fn ask_human_file_cleanup_on_timeout() {
        let (agent, _shm, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let q_path = tmp.path().join("question");
        let r_path = tmp.path().join("response");
        let cmd = AgentCommand {
            function: "askHuman".to_string(),
            nonce: 1,
            question: Some("cleanup test".to_string()),
            ..Default::default()
        };
        agent
            .ask_human_with_paths(&cmd, &q_path, &r_path, 200, 50)
            .await
            .unwrap();
        assert!(
            !q_path.exists(),
            "question file should be cleaned up after timeout"
        );
        assert!(
            !r_path.exists(),
            "response file should be cleaned up after timeout"
        );
    }

    // --- execPty tests ---

    #[tokio::test]
    async fn exec_pty_simple_echo() {
        let (agent, _shm, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "execPty".to_string(),
            command: Some("echo hello_pty".to_string()),
            nonce: 1,
            ..Default::default()
        };
        let result = agent.exec_pty(&cmd).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["success"], true);
        assert_eq!(parsed["shell_id"], "default");
        let output = parsed["output"].as_str().unwrap();
        assert!(
            output.contains("hello_pty"),
            "output should contain echo result, got: {}",
            output
        );
    }

    #[tokio::test]
    async fn exec_pty_state_persistence() {
        let (agent, _shm, _log) = create_test_agent();
        // Set an env var in the first command
        let cmd1 = AgentCommand {
            function: "execPty".to_string(),
            command: Some("export MY_TEST_VAR=persistent_value".to_string()),
            nonce: 1,
            shell_id: Some("persist".to_string()),
            ..Default::default()
        };
        agent.exec_pty(&cmd1).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Read it back in the second command (same shell)
        let cmd2 = AgentCommand {
            function: "execPty".to_string(),
            command: Some("echo $MY_TEST_VAR".to_string()),
            nonce: 2,
            shell_id: Some("persist".to_string()),
            ..Default::default()
        };
        let result = agent.exec_pty(&cmd2).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["success"], true);
        let output = parsed["output"].as_str().unwrap();
        assert!(
            output.contains("persistent_value"),
            "env var should persist, got: {}",
            output
        );
    }

    #[tokio::test]
    async fn exec_pty_cd_persistence() {
        let (agent, _shm, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let tmp_path = tmp.path().to_string_lossy().to_string();

        let cmd1 = AgentCommand {
            function: "execPty".to_string(),
            command: Some(format!("cd {}", tmp_path)),
            nonce: 1,
            shell_id: Some("cd_test".to_string()),
            ..Default::default()
        };
        agent.exec_pty(&cmd1).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let cmd2 = AgentCommand {
            function: "execPty".to_string(),
            command: Some("pwd".to_string()),
            nonce: 2,
            shell_id: Some("cd_test".to_string()),
            ..Default::default()
        };
        let result = agent.exec_pty(&cmd2).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let output = parsed["output"].as_str().unwrap();
        assert!(
            output.contains(&tmp_path),
            "cd should persist, got: {}",
            output
        );
    }

    #[tokio::test]
    async fn exec_pty_multiple_sessions() {
        let (agent, _shm, _log) = create_test_agent();
        let cmd1 = AgentCommand {
            function: "execPty".to_string(),
            command: Some("export S=session_a".to_string()),
            nonce: 1,
            shell_id: Some("a".to_string()),
            ..Default::default()
        };
        agent.exec_pty(&cmd1).await.unwrap();

        let cmd2 = AgentCommand {
            function: "execPty".to_string(),
            command: Some("export S=session_b".to_string()),
            nonce: 2,
            shell_id: Some("b".to_string()),
            ..Default::default()
        };
        agent.exec_pty(&cmd2).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Read from session a
        let cmd3 = AgentCommand {
            function: "execPty".to_string(),
            command: Some("echo $S".to_string()),
            nonce: 3,
            shell_id: Some("a".to_string()),
            ..Default::default()
        };
        let result = agent.exec_pty(&cmd3).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let output = parsed["output"].as_str().unwrap();
        assert!(
            output.contains("session_a"),
            "session a should have its own state, got: {}",
            output
        );
    }

    #[tokio::test]
    async fn exec_pty_default_shell_id() {
        let (agent, _shm, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "execPty".to_string(),
            command: Some("echo default_test".to_string()),
            nonce: 1,
            ..Default::default()
        };
        let result = agent.exec_pty(&cmd).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["shell_id"], "default");
    }

    #[tokio::test]
    async fn exec_pty_missing_command() {
        let (agent, _shm, _log) = create_test_agent();
        let cmd = AgentCommand {
            function: "execPty".to_string(),
            nonce: 1,
            ..Default::default()
        };
        let result = agent.exec_pty(&cmd).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn exec_pty_process_input_integration() {
        let (agent, _shm, _log) = create_test_agent();
        let input = AgentInput {
            commands: vec![AgentCommand {
                function: "execPty".to_string(),
                command: Some("echo integration_test".to_string()),
                nonce: 1,
                ..Default::default()
            }],
            wait_for_status: None,
        };
        let results = agent.process_input(input).await.unwrap();
        assert_eq!(results.len(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&results[0]).unwrap();
        assert_eq!(parsed["success"], true);
        let output = parsed["output"].as_str().unwrap();
        assert!(output.contains("integration_test"), "got: {}", output);
    }

    // storeMemory tests

    #[tokio::test]
    async fn store_memory_create() {
        let dir = tempfile::tempdir().unwrap();
        let shm = dir.path().join("shm");
        let log = dir.path().join("log");
        let agent = Agent::new_with_paths(shm.to_str().unwrap(), log.clone()).unwrap();

        let memory_file = dir.path().join("agent").join("memory.json");
        let cmd = AgentCommand {
            function: "storeMemory".to_string(),
            nonce: 1,
            memory_key: Some("db-config".to_string()),
            memory_summary: Some("Database uses PostgreSQL on port 5432".to_string()),
            memory_file: Some(memory_file.to_string_lossy().to_string()),
            ..Default::default()
        };

        let result = agent.store_memory(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["success"], true);
        assert_eq!(parsed["key"], "db-config");
        assert_eq!(parsed["action"], "created");

        assert!(memory_file.exists());
        let content: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&memory_file).unwrap()).unwrap();
        assert_eq!(
            content["entries"]["db-config"]["summary"],
            "Database uses PostgreSQL on port 5432"
        );
    }

    #[tokio::test]
    async fn store_memory_update() {
        let dir = tempfile::tempdir().unwrap();
        let shm = dir.path().join("shm");
        let log = dir.path().join("log");
        let agent = Agent::new_with_paths(shm.to_str().unwrap(), log.clone()).unwrap();

        let memory_file = dir.path().join("memory.json");

        // Create first
        let cmd = AgentCommand {
            function: "storeMemory".to_string(),
            nonce: 1,
            memory_key: Some("key1".to_string()),
            memory_summary: Some("version 1".to_string()),
            memory_file: Some(memory_file.to_string_lossy().to_string()),
            ..Default::default()
        };
        agent.store_memory(&cmd).unwrap();

        // Update
        let cmd2 = AgentCommand {
            function: "storeMemory".to_string(),
            nonce: 2,
            memory_key: Some("key1".to_string()),
            memory_summary: Some("version 2".to_string()),
            memory_file: Some(memory_file.to_string_lossy().to_string()),
            ..Default::default()
        };
        let result = agent.store_memory(&cmd2).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["action"], "updated");

        let content: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&memory_file).unwrap()).unwrap();
        assert_eq!(content["entries"]["key1"]["summary"], "version 2");
    }

    #[tokio::test]
    async fn store_memory_missing_key() {
        let dir = tempfile::tempdir().unwrap();
        let shm = dir.path().join("shm");
        let log = dir.path().join("log");
        let agent = Agent::new_with_paths(shm.to_str().unwrap(), log.clone()).unwrap();

        let cmd = AgentCommand {
            function: "storeMemory".to_string(),
            nonce: 1,
            memory_summary: Some("summary".to_string()),
            memory_file: Some("/tmp/test.json".to_string()),
            ..Default::default()
        };
        assert!(agent.store_memory(&cmd).is_err());
    }

    // recallMemory tests

    #[tokio::test]
    async fn recall_memory_empty() {
        let dir = tempfile::tempdir().unwrap();
        let shm = dir.path().join("shm");
        let log = dir.path().join("log");
        let agent = Agent::new_with_paths(shm.to_str().unwrap(), log.clone()).unwrap();

        let memory_file = dir.path().join("nonexistent.json");
        let cmd = AgentCommand {
            function: "recallMemory".to_string(),
            nonce: 1,
            memory_query: Some("anything".to_string()),
            memory_file: Some(memory_file.to_string_lossy().to_string()),
            ..Default::default()
        };

        let result = agent.recall_memory(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["success"], true);
        assert!(parsed["results"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn recall_memory_finds_matches() {
        let dir = tempfile::tempdir().unwrap();
        let shm = dir.path().join("shm");
        let log = dir.path().join("log");
        let agent = Agent::new_with_paths(shm.to_str().unwrap(), log.clone()).unwrap();

        let memory_file = dir.path().join("memory.json");

        // Store some memories
        for (key, summary) in &[
            ("db-config", "PostgreSQL on port 5432"),
            ("api-auth", "Uses JWT tokens for authentication"),
            ("db-migration", "Run migrations with cargo sqlx"),
        ] {
            let cmd = AgentCommand {
                function: "storeMemory".to_string(),
                nonce: 1,
                memory_key: Some(key.to_string()),
                memory_summary: Some(summary.to_string()),
                memory_file: Some(memory_file.to_string_lossy().to_string()),
                ..Default::default()
            };
            agent.store_memory(&cmd).unwrap();
        }

        // Recall with "db" query
        let cmd = AgentCommand {
            function: "recallMemory".to_string(),
            nonce: 1,
            memory_query: Some("db".to_string()),
            memory_file: Some(memory_file.to_string_lossy().to_string()),
            ..Default::default()
        };

        let result = agent.recall_memory(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["success"], true);
        let results = parsed["results"].as_array().unwrap();
        assert_eq!(results.len(), 2);
    }

    #[tokio::test]
    async fn recall_memory_missing_query() {
        let dir = tempfile::tempdir().unwrap();
        let shm = dir.path().join("shm");
        let log = dir.path().join("log");
        let agent = Agent::new_with_paths(shm.to_str().unwrap(), log.clone()).unwrap();

        let cmd = AgentCommand {
            function: "recallMemory".to_string(),
            nonce: 1,
            memory_file: Some("/tmp/test.json".to_string()),
            ..Default::default()
        };
        assert!(agent.recall_memory(&cmd).is_err());
    }

    #[tokio::test]
    async fn store_memory_process_input_integration() {
        let dir = tempfile::tempdir().unwrap();
        let shm = dir.path().join("shm");
        let log = dir.path().join("log");
        let agent = Agent::new_with_paths(shm.to_str().unwrap(), log.clone()).unwrap();

        let memory_file = dir.path().join("mem.json");
        let input = AgentInput {
            commands: vec![AgentCommand {
                function: "storeMemory".to_string(),
                nonce: 1,
                memory_key: Some("test".to_string()),
                memory_summary: Some("summary".to_string()),
                memory_file: Some(memory_file.to_string_lossy().to_string()),
                ..Default::default()
            }],
            wait_for_status: None,
        };
        let results = agent.process_input(input).await.unwrap();
        assert_eq!(results.len(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&results[0]).unwrap();
        assert_eq!(parsed["success"], true);
    }

    #[tokio::test]
    async fn recall_memory_process_input_integration() {
        let dir = tempfile::tempdir().unwrap();
        let shm = dir.path().join("shm");
        let log = dir.path().join("log");
        let agent = Agent::new_with_paths(shm.to_str().unwrap(), log.clone()).unwrap();

        let memory_file = dir.path().join("mem2.json");
        let input = AgentInput {
            commands: vec![AgentCommand {
                function: "recallMemory".to_string(),
                nonce: 1,
                memory_query: Some("test".to_string()),
                memory_file: Some(memory_file.to_string_lossy().to_string()),
                ..Default::default()
            }],
            wait_for_status: None,
        };
        let results = agent.process_input(input).await.unwrap();
        assert_eq!(results.len(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&results[0]).unwrap();
        assert_eq!(parsed["success"], true);
    }

    // --- Knowledge system field tests ---

    #[tokio::test]
    async fn store_memory_with_tags_and_channel() {
        let dir = tempfile::tempdir().unwrap();
        let shm = dir.path().join("shm");
        let log = dir.path().join("log");
        let agent = Agent::new_with_paths(shm.to_str().unwrap(), log.clone()).unwrap();

        let memory_file = dir.path().join("knowledge.json");

        // First, create a new-format file
        std::fs::write(
            &memory_file,
            r#"{"entries":[],"subscriptions":{},"cursors":{}}"#,
        )
        .unwrap();

        let cmd = AgentCommand {
            function: "storeMemory".to_string(),
            nonce: 1,
            memory_key: Some("db-schema".to_string()),
            memory_summary: Some("PostgreSQL with 5 tables".to_string()),
            memory_file: Some(memory_file.to_string_lossy().to_string()),
            memory_tags: Some("database, schema".to_string()),
            memory_channel: Some("findings".to_string()),
            memory_source: Some("research-1".to_string()),
            ..Default::default()
        };
        let result = agent.store_memory(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["success"], true);
        assert_eq!(parsed["action"], "created");

        // Verify the file contains tags and channel
        let content = std::fs::read_to_string(&memory_file).unwrap();
        let data: serde_json::Value = serde_json::from_str(&content).unwrap();
        let entry = &data["entries"][0];
        assert_eq!(entry["key"], "db-schema");
        assert_eq!(entry["channel"], "findings");
        assert_eq!(entry["source"], "research-1");
        let tags = entry["tags"].as_array().unwrap();
        assert!(tags.contains(&serde_json::json!("database")));
        assert!(tags.contains(&serde_json::json!("schema")));
    }

    #[tokio::test]
    async fn recall_memory_with_tag_filter() {
        let dir = tempfile::tempdir().unwrap();
        let shm = dir.path().join("shm");
        let log = dir.path().join("log");
        let agent = Agent::new_with_paths(shm.to_str().unwrap(), log.clone()).unwrap();

        let memory_file = dir.path().join("knowledge.json");

        // Create knowledge store with tagged entries
        let content = r#"{
            "entries": [
                {"id":"1","key":"db-schema","summary":"PostgreSQL tables","tags":["database"],"source":"agent","channel":"findings","created_at":1000,"updated_at":1000},
                {"id":"2","key":"api-routes","summary":"REST endpoints","tags":["api"],"source":"agent","channel":"findings","created_at":1000,"updated_at":1000}
            ],
            "subscriptions": {},
            "cursors": {}
        }"#;
        std::fs::write(&memory_file, content).unwrap();

        // Recall with tag filter
        let cmd = AgentCommand {
            function: "recallMemory".to_string(),
            nonce: 1,
            memory_query: Some("schema endpoints".to_string()),
            memory_file: Some(memory_file.to_string_lossy().to_string()),
            memory_tags: Some("database".to_string()),
            ..Default::default()
        };
        let result = agent.recall_memory(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["success"], true);
        let results = parsed["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["key"], "db-schema");
    }

    #[tokio::test]
    async fn recall_memory_with_channel_filter() {
        let dir = tempfile::tempdir().unwrap();
        let shm = dir.path().join("shm");
        let log = dir.path().join("log");
        let agent = Agent::new_with_paths(shm.to_str().unwrap(), log.clone()).unwrap();

        let memory_file = dir.path().join("knowledge.json");

        let content = r#"{
            "entries": [
                {"id":"1","key":"finding-1","summary":"Found something","tags":[],"source":"agent","channel":"findings","created_at":1000,"updated_at":1000},
                {"id":"2","key":"decision-1","summary":"Decided something","tags":[],"source":"agent","channel":"decisions","created_at":1000,"updated_at":1000}
            ],
            "subscriptions": {},
            "cursors": {}
        }"#;
        std::fs::write(&memory_file, content).unwrap();

        let cmd = AgentCommand {
            function: "recallMemory".to_string(),
            nonce: 1,
            memory_query: Some("something".to_string()),
            memory_file: Some(memory_file.to_string_lossy().to_string()),
            memory_channel: Some("findings".to_string()),
            ..Default::default()
        };
        let result = agent.recall_memory(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let results = parsed["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["key"], "finding-1");
    }

    #[tokio::test]
    async fn store_memory_backward_compat_old_format() {
        let dir = tempfile::tempdir().unwrap();
        let shm = dir.path().join("shm");
        let log = dir.path().join("log");
        let agent = Agent::new_with_paths(shm.to_str().unwrap(), log.clone()).unwrap();

        let memory_file = dir.path().join("old_mem.json");
        // Pre-populate with old format
        std::fs::write(
            &memory_file,
            r#"{"entries":{"existing":{"summary":"old data","created_at":100,"updated_at":200}}}"#,
        )
        .unwrap();

        // Store in old format (no tags/channel/source)
        let cmd = AgentCommand {
            function: "storeMemory".to_string(),
            nonce: 1,
            memory_key: Some("new-key".to_string()),
            memory_summary: Some("new data".to_string()),
            memory_file: Some(memory_file.to_string_lossy().to_string()),
            ..Default::default()
        };
        let result = agent.store_memory(&cmd).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["success"], true);
        assert_eq!(parsed["action"], "created");

        // Verify old format is maintained
        let content = std::fs::read_to_string(&memory_file).unwrap();
        let data: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(data["entries"].is_object());
        assert!(data["entries"]["existing"].is_object());
        assert!(data["entries"]["new-key"].is_object());
    }

    // --- Synchronous command shared memory registration tests ---

    #[tokio::test]
    async fn sync_command_registers_completed_in_shared_memory() {
        let (agent, _shm, _log) = create_test_agent();
        let input = AgentInput {
            commands: vec![AgentCommand {
                function: "inspectPath".to_string(),
                nonce: 42,
                path: Some("/tmp".to_string()),
                ..Default::default()
            }],
            wait_for_status: None,
        };
        agent.process_input(input).await.unwrap();

        // The synchronous command should now be registered as Completed
        let info = agent.get_process_info(42).unwrap();
        assert_eq!(info.status, ProcessStatus::Completed);
        assert_eq!(info.exit_code, 0);
    }

    #[tokio::test]
    async fn sync_command_registers_failed_in_shared_memory() {
        let (agent, _shm, _log) = create_test_agent();
        // editFile without required fields will fail
        let input = AgentInput {
            commands: vec![AgentCommand {
                function: "editFile".to_string(),
                nonce: 43,
                // Missing file_path and operation -> error
                ..Default::default()
            }],
            wait_for_status: None,
        };
        let results = agent.process_input(input).await.unwrap();
        assert!(results[0].starts_with("Error:"));

        // The synchronous command should now be registered as Failed
        let info = agent.get_process_info(43).unwrap();
        assert_eq!(info.status, ProcessStatus::Failed);
        assert_eq!(info.exit_code, 1);
    }

    #[tokio::test]
    async fn exec_depends_on_sync_command_runs() {
        let (agent, _shm, _log) = create_test_agent();
        let tmp = TempDir::new().unwrap();
        let fp = tmp.path().join("dep_test.txt");

        let input = AgentInput {
            commands: vec![
                // Synchronous command: editFile (nonce 10)
                AgentCommand {
                    function: "editFile".to_string(),
                    nonce: 10,
                    file_path: Some(fp.to_string_lossy().to_string()),
                    operation: Some("write".to_string()),
                    content: Some("created by editFile".to_string()),
                    ..Default::default()
                },
                // Async command depending on editFile (nonce 10)
                AgentCommand {
                    function: "execAsAgent".to_string(),
                    command: Some("echo chained".to_string()),
                    nonce: 11,
                    depending_nonce: Some(10),
                    expected_status: Some(0),
                    wait: Some(true),
                    display: Some(1),
                    ..Default::default()
                },
            ],
            wait_for_status: None,
        };

        let results = agent.process_input(input).await.unwrap();

        // editFile should have succeeded
        let parsed: serde_json::Value = serde_json::from_str(&results[0]).unwrap();
        assert_eq!(parsed["success"], true);

        // The editFile nonce should be Completed in shared memory
        let info = agent.get_process_info(10).unwrap();
        assert_eq!(info.status, ProcessStatus::Completed);

        // The execAsAgent should report waiting (it's async, spawned with dependency)
        // but crucially it was NOT skipped — the dependency was found
        assert_eq!(results[1], "11w0");

        // Give the spawned command time to complete and verify it ran
        tokio::time::sleep(Duration::from_millis(500)).await;
        let exec_info = agent.get_process_info(11).unwrap();
        assert_eq!(exec_info.status, ProcessStatus::Completed);
    }

    #[tokio::test]
    async fn exec_depends_on_failed_sync_command_is_skipped() {
        let (agent, _shm, _log) = create_test_agent();
        let input = AgentInput {
            commands: vec![
                // Synchronous command that will fail (missing file_path)
                AgentCommand {
                    function: "editFile".to_string(),
                    nonce: 20,
                    // Missing required fields -> error
                    ..Default::default()
                },
                // Async command depending on the failed editFile
                AgentCommand {
                    function: "execAsAgent".to_string(),
                    command: Some("echo should_not_run".to_string()),
                    nonce: 21,
                    depending_nonce: Some(20),
                    expected_status: Some(0),
                    wait: Some(true),
                    display: Some(1),
                    ..Default::default()
                },
            ],
            wait_for_status: None,
        };

        let results = agent.process_input(input).await.unwrap();

        // editFile failed
        assert!(results[0].starts_with("Error:"));

        // The failed editFile should be Failed in shared memory
        let info = agent.get_process_info(20).unwrap();
        assert_eq!(info.status, ProcessStatus::Failed);

        // The execAsAgent reports waiting (spawned async)
        assert_eq!(results[1], "21w0");

        // Give the spawned command time to process dependency check
        tokio::time::sleep(Duration::from_millis(500)).await;

        // The exec should be skipped since its dependency failed (exit_code 1 != expected 0)
        let exec_info = agent.get_process_info(21).unwrap();
        assert_eq!(exec_info.status, ProcessStatus::Skipped);
    }
}
