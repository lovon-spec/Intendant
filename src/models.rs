use serde::{Deserialize, Deserializer, Serialize};

/// Deserialize an `Option<u64>` where `0` is treated as `None`.
/// Nonce 0 is never valid, so LLMs sending `"depending_nonce": 0`
/// mean "no dependency."
fn deserialize_zero_as_none<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    let opt = Option::<u64>::deserialize(deserializer)?;
    Ok(opt.filter(|&v| v != 0))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Command {
    pub function: String,
    pub command: Option<String>,
    pub nonce: u64,
    #[serde(default, deserialize_with = "deserialize_zero_as_none")]
    pub depending_nonce: Option<u64>,
    pub expected_status: Option<i32>,
    pub wait: Option<bool>,
    pub return_stdout: Option<bool>,
    pub return_stderr: Option<bool>,
    pub display: Option<i32>,
    pub status_type: Option<String>,
    pub path: Option<String>,
    // Log tail fields
    pub offset: Option<u64>,
    pub limit: Option<u64>,
    pub cursor: Option<u64>,
    pub timeout_ms: Option<u64>,
    // Execution identity fields (preferred over nonce-only fetch semantics)
    pub run_id: Option<String>,
    pub agent_id: Option<String>,
    pub attempt_id: Option<String>,
    pub command_id: Option<String>,
    pub stream_id: Option<String>,
    // editFile fields
    pub file_path: Option<String>,
    pub operation: Option<String>,
    pub content: Option<String>,
    pub match_content: Option<String>,
    pub line_number: Option<usize>,
    pub end_line: Option<usize>,
    // browse field
    pub url: Option<String>,
    // wait_for_port field
    pub wait_for_port: Option<u16>,
    // askHuman field
    pub question: Option<String>,
    // execPty field
    pub shell_id: Option<String>,
    // storeMemory / recallMemory fields
    pub memory_key: Option<String>,
    pub memory_summary: Option<String>,
    pub memory_query: Option<String>,
    pub memory_file: Option<String>,
    // Knowledge system fields
    pub memory_tags: Option<String>,
    pub memory_channel: Option<String>,
    pub memory_source: Option<String>,
    pub memory_since: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentInput {
    pub wait_for_status: Option<u64>,
    pub commands: Vec<Command>,
    #[serde(default)]
    pub state_socket: Option<String>,
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_status_repr_values() {
        assert_eq!(ProcessStatus::Running as u8, b'r');
        assert_eq!(ProcessStatus::Completed as u8, b'c');
        assert_eq!(ProcessStatus::Failed as u8, b'f');
        assert_eq!(ProcessStatus::Waiting as u8, b'w');
        assert_eq!(ProcessStatus::Skipped as u8, b's');
    }

    #[test]
    fn process_status_char_conversion() {
        let status = ProcessStatus::Running;
        let ch = status as u8 as char;
        assert_eq!(ch, 'r');

        let status = ProcessStatus::Completed;
        let ch = status as u8 as char;
        assert_eq!(ch, 'c');
    }

    #[test]
    fn command_deserialize_minimal() {
        let json = r#"{
            "function": "execAsAgent",
            "nonce": 42
        }"#;
        let cmd: Command = serde_json::from_str(json).unwrap();
        assert_eq!(cmd.function, "execAsAgent");
        assert_eq!(cmd.nonce, 42);
        assert!(cmd.command.is_none());
        assert!(cmd.depending_nonce.is_none());
        assert!(cmd.wait.is_none());
    }

    #[test]
    fn command_deserialize_full() {
        let json = r#"{
            "function": "execAsAgent",
            "command": "echo hello",
            "nonce": 1,
            "depending_nonce": 0,
            "expected_status": 0,
            "wait": true,
            "display": 1,
            "return_stdout": true,
            "return_stderr": false
        }"#;
        let cmd: Command = serde_json::from_str(json).unwrap();
        assert_eq!(cmd.function, "execAsAgent");
        assert_eq!(cmd.command.as_deref(), Some("echo hello"));
        assert_eq!(cmd.nonce, 1);
        assert_eq!(cmd.depending_nonce, None); // 0 is normalized to None
        assert_eq!(cmd.expected_status, Some(0));
        assert_eq!(cmd.wait, Some(true));
        assert_eq!(cmd.display, Some(1));
    }

    #[test]
    fn agent_input_deserialize() {
        let json = r#"{
            "commands": [
                {"function": "execAsAgent", "command": "ls", "nonce": 1},
                {"function": "fetchStatus", "nonce": 1, "status_type": "stdout"}
            ],
            "wait_for_status": 500
        }"#;
        let input: AgentInput = serde_json::from_str(json).unwrap();
        assert_eq!(input.commands.len(), 2);
        assert_eq!(input.wait_for_status, Some(500));
    }

    #[test]
    fn agent_input_no_wait() {
        let json = r#"{"commands": []}"#;
        let input: AgentInput = serde_json::from_str(json).unwrap();
        assert!(input.commands.is_empty());
        assert!(input.wait_for_status.is_none());
        assert!(input.state_socket.is_none());
    }

    #[test]
    fn agent_input_with_state_socket() {
        let json = r#"{"commands": [], "state_socket": "/tmp/test.sock"}"#;
        let input: AgentInput = serde_json::from_str(json).unwrap();
        assert_eq!(input.state_socket.as_deref(), Some("/tmp/test.sock"));
    }

    #[test]
    fn process_info_size_nonzero() {
        let size = std::mem::size_of::<ProcessInfo>();
        assert!(size > 0);
        assert_eq!(size, std::mem::size_of::<ProcessInfo>());
    }

    #[test]
    fn process_info_clone_copy() {
        let info = ProcessInfo {
            nonce: 1,
            pid: 1234,
            status: ProcessStatus::Running,
            exit_code: 0,
            timestamp: 1000,
        };
        let copy = info;
        assert_eq!(copy.nonce, 1);
        assert_eq!(copy.pid, 1234);
        assert_eq!(copy.status, ProcessStatus::Running);
    }

    #[test]
    fn depending_nonce_zero_becomes_none() {
        let json = r#"{"function": "execAsAgent", "nonce": 1, "depending_nonce": 0}"#;
        let cmd: Command = serde_json::from_str(json).unwrap();
        assert_eq!(cmd.depending_nonce, None);
    }

    #[test]
    fn depending_nonce_nonzero_preserved() {
        let json = r#"{"function": "execAsAgent", "nonce": 2, "depending_nonce": 5}"#;
        let cmd: Command = serde_json::from_str(json).unwrap();
        assert_eq!(cmd.depending_nonce, Some(5));
    }

    #[test]
    fn depending_nonce_absent_is_none() {
        let json = r#"{"function": "execAsAgent", "nonce": 3}"#;
        let cmd: Command = serde_json::from_str(json).unwrap();
        assert_eq!(cmd.depending_nonce, None);
    }

    #[test]
    fn command_serialize_roundtrip() {
        let cmd = Command {
            function: "inspectPath".to_string(),
            nonce: 5,
            path: Some("/tmp/test".to_string()),
            ..Default::default()
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let deserialized: Command = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.function, "inspectPath");
        assert_eq!(deserialized.path.as_deref(), Some("/tmp/test"));
        assert_eq!(deserialized.nonce, 5);
    }
}
