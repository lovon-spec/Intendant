use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Command {
    pub function: String,
    pub command: Option<String>,
    pub nonce: u64,
    pub depending_nonce: Option<u64>,
    pub expected_status: Option<i32>,
    pub wait: Option<bool>,
    pub return_stdout: Option<bool>,
    pub return_stderr: Option<bool>,
    pub display: Option<i32>,
    pub status_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInput {
    pub wait_for_status: Option<u64>,
    pub commands: Vec<Command>,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ProcessInfo {
    pub nonce: u64,
    pub pid: i32,
    pub status: ProcessStatus,
    pub exit_code: i32,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(u8)]
pub enum ProcessStatus {
    Running = b'r',
    Completed = b'c',
    Failed = b'f',
    Waiting = b'w',
    Skipped = b's',
}

#[derive(Debug, Clone)]
pub struct StatusUpdate {
    pub nonce: u64,
    pub status: ProcessStatus,
    pub exit_code: i32,
}
