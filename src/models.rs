use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Command {
    pub function: String,
    pub command: Option<String>,
    pub nonce: u64,
    pub return_stdout: Option<bool>,
    pub return_stderr: Option<bool>,
    pub display: Option<i32>,
    pub path: Option<String>,
    pub timeout_ms: Option<u64>,
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
    pub commands: Vec<Command>,
}

#[allow(dead_code)]
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_status_repr_values() {
        assert_eq!(ProcessStatus::Running as u8, b'r');
        assert_eq!(ProcessStatus::Completed as u8, b'c');
        assert_eq!(ProcessStatus::Failed as u8, b'f');
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
    }

    #[test]
    fn command_deserialize_full() {
        let json = r#"{
            "function": "execAsAgent",
            "command": "echo hello",
            "nonce": 1,
            "display": 1,
            "return_stdout": true,
            "return_stderr": false
        }"#;
        let cmd: Command = serde_json::from_str(json).unwrap();
        assert_eq!(cmd.function, "execAsAgent");
        assert_eq!(cmd.command.as_deref(), Some("echo hello"));
        assert_eq!(cmd.nonce, 1);
        assert_eq!(cmd.display, Some(1));
    }

    #[test]
    fn agent_input_deserialize() {
        let json = r#"{
            "commands": [
                {"function": "execAsAgent", "command": "ls", "nonce": 1},
                {"function": "inspectPath", "nonce": 2, "path": "/tmp"}
            ]
        }"#;
        let input: AgentInput = serde_json::from_str(json).unwrap();
        assert_eq!(input.commands.len(), 2);
    }

    #[test]
    fn agent_input_empty() {
        let json = r#"{"commands": []}"#;
        let input: AgentInput = serde_json::from_str(json).unwrap();
        assert!(input.commands.is_empty());
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
