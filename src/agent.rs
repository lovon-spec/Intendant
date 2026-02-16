use crate::error::AgentError;
use crate::models::{AgentInput, Command as AgentCommand, ProcessInfo, ProcessStatus, StatusUpdate};
use std::os::unix::fs::OpenOptionsExt;

use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    mem::size_of,
    path::PathBuf,
    process::Stdio,
    sync::{Arc, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use chrono::Local;
use memmap2::{MmapMut, MmapOptions};
use tokio::sync::mpsc;
use tokio::process::Command;

const MAX_PROCESSES: usize = 1024;
const SHARED_MEM_SIZE: usize = size_of::<ProcessInfo>() * MAX_PROCESSES;
const SHARED_MEM_PATH: &str = "/dev/shm/agent_processes";

#[derive(Clone)]
pub struct Agent {
    pub shared_mem: Arc<RwLock<MmapMut>>,
    pub process_map: Arc<RwLock<HashMap<u64, usize>>>,
    log_dir: PathBuf,
    status_tx: mpsc::Sender<StatusUpdate>,
}

impl Agent {
    pub fn new() -> Result<Self, AgentError> {
        // Create shared memory file
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .open(SHARED_MEM_PATH)?;
        file.set_len(SHARED_MEM_SIZE as u64)?;

        // Map shared memory
        let mmap = unsafe { MmapOptions::new().map_mut(&file)? };
        let shared_mem = Arc::new(RwLock::new(mmap));

        // Create log directory
        let timestamp = Local::now().format("%Y%m%d_%H%M%S").to_string();
        let log_dir = PathBuf::from(format!("/var/log/agent/{}", timestamp));
        fs::create_dir_all(&log_dir)?;

        // Setup status channel
        let (status_tx, mut status_rx) = mpsc::channel(1024);
        let status_tx_clone: mpsc::Sender<StatusUpdate> = status_tx.clone();

        // Start status monitor thread
        let shared_mem_clone = shared_mem.clone();
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
            }
        });

        Ok(Self {
            shared_mem,
            process_map: Arc::new(RwLock::new(HashMap::new())),
            log_dir,
            status_tx: status_tx_clone,
        })
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

        let info = ProcessInfo {
            nonce,
            pid: 0, // We don't need to update PID here
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
    
            if !self.check_dependency(dep_nonce, expected_status, wait).await? {
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
    
        // Replace $NONCE references
        let command = self.replace_nonce_refs(command)?;
    
        // Setup output files with append mode to prevent truncation
        let stdout_path = self.log_dir.join(format!("{}_stdout.log", cmd.nonce));
        let stderr_path = self.log_dir.join(format!("{}_stderr.log", cmd.nonce));
    
        let stdout_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&stdout_path)?;
        let stderr_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&stderr_path)?;
    
        // Execute command
        let mut child = Command::new("bash")
            .arg("-c")
            .arg(&command)
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

            if !self.check_dependency(dep_nonce, expected_status, wait).await? {
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

    fn fetch_status(&self, cmd: &AgentCommand) -> Result<String, AgentError> {
        let status_type = cmd.status_type.as_ref().ok_or_else(|| {
            AgentError::Process("status_type is required for fetchStatus".to_string())
        })?;

        match status_type.as_str() {
            "status" => {
                let info = self.get_process_info(cmd.nonce)?;
                Ok((info.status as u8 as char).to_string())
            }
            "stdout" => {
                let path = self.log_dir.join(format!("{}_stdout.log", cmd.nonce));
                Ok(fs::read_to_string(path)?)
            }
            "stderr" => {
                let path = self.log_dir.join(format!("{}_stderr.log", cmd.nonce));
                Ok(fs::read_to_string(path)?)
            }
            "exit_code" => {
                let info = self.get_process_info(cmd.nonce)?;
                Ok(info.exit_code.to_string())
            }
            _ => Err(AgentError::Process(format!(
                "Invalid status_type: {}",
                status_type
            ))),
        }
    }

    pub async fn process_input(&self, input: AgentInput) -> Result<Vec<String>, AgentError> {
        let mut results = Vec::new();
        
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
                    // Immediately return initial running status
                    results.push(format!("{}r0", cmd.nonce));
                }
                "captureScreen" => {
                    let agent = self.clone();
                    let cmd_clone = cmd.clone();
                    tokio::spawn(async move {
                        if let Err(e) = agent.capture_screen(&cmd_clone).await {
                            eprintln!("Error capturing screen {}: {}", cmd_clone.nonce, e);
                        }
                    });
                    results.push(format!("{}r0", cmd.nonce));
                }
                "fetchStatus" => {
                    // fetchStatus is synchronous and immediate
                    match self.fetch_status(&cmd) {
                        Ok(status) => results.push(status),
                        Err(e) => results.push(format!("Error: {}", e)),
                    }
                }
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
        let offset = *map
            .get(&nonce)
            .ok_or_else(|| AgentError::InvalidNonce(nonce))?;

        let mmap = self.shared_mem.read().unwrap();
        let info_slice = &mmap[offset..offset + size_of::<ProcessInfo>()];
        
        let info = unsafe {
            std::ptr::read(info_slice.as_ptr() as *const ProcessInfo)
        };
        
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
                        ProcessStatus::Failed | ProcessStatus::Skipped => {
                            return Ok(false)
                        }
                        _ if !wait => {
                            return Ok(false)
                        }
                        _ => {tokio::time::sleep(Duration::from_millis(100)).await;
                            retries -= 1;
                            continue;
                        }
                    }
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

