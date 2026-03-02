use crate::error::CallerError;
use crate::provider::TokenUsage;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SubAgentRole {
    Research,
    Implementation,
    Testing,
    Orchestrator,
    Custom(String),
}

impl SubAgentRole {
    pub fn as_str(&self) -> &str {
        match self {
            SubAgentRole::Research => "research",
            SubAgentRole::Implementation => "implementation",
            SubAgentRole::Testing => "testing",
            SubAgentRole::Orchestrator => "orchestrator",
            SubAgentRole::Custom(s) => s,
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "research" => SubAgentRole::Research,
            "implementation" => SubAgentRole::Implementation,
            "testing" => SubAgentRole::Testing,
            "orchestrator" => SubAgentRole::Orchestrator,
            other => SubAgentRole::Custom(other.to_string()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgentSpec {
    pub id: String,
    pub task: String,
    pub role: SubAgentRole,
    pub working_dir: PathBuf,
    pub result_file: PathBuf,
    pub progress_file: PathBuf,
    pub system_prompt: Option<String>,
    pub inherit_memory: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SubAgentStatus {
    Completed,
    Failed(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgentResult {
    pub id: String,
    pub status: SubAgentStatus,
    pub summary: String,
    pub findings: Vec<String>,
    pub artifacts: Vec<PathBuf>,
    pub usage: TokenUsage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgentProgress {
    pub id: String,
    pub turn: usize,
    pub status: String,
    pub last_action: String,
    pub question: Option<String>,
}

pub fn build_spawn_command(spec: &SubAgentSpec, caller_path: &Path) -> String {
    let caller = caller_path.to_string_lossy();
    let mut env_parts = vec![
        format!("INTENDANT_ROLE={}", shell_escape(spec.role.as_str())),
        format!("INTENDANT_ID={}", shell_escape(&spec.id)),
        format!(
            "INTENDANT_RESULT_FILE={}",
            shell_escape(&spec.result_file.to_string_lossy())
        ),
        format!(
            "INTENDANT_PROGRESS_FILE={}",
            shell_escape(&spec.progress_file.to_string_lossy())
        ),
    ];

    if spec.inherit_memory {
        env_parts.push("INTENDANT_INHERIT_MEMORY=1".to_string());
    }

    if let Some(ref prompt) = spec.system_prompt {
        env_parts.push(format!("INTENDANT_SYSTEM_PROMPT={}", shell_escape(prompt)));
    }

    let env_str = env_parts.join(" ");
    let wd = spec.working_dir.to_string_lossy();

    format!(
        "cd {} && {} {} {}",
        shell_escape(&wd),
        env_str,
        shell_escape(&caller),
        shell_escape(&spec.task)
    )
}

pub fn read_result(path: &Path) -> Result<SubAgentResult, CallerError> {
    let content = std::fs::read_to_string(path).map_err(|e| {
        CallerError::SubAgent(format!("Failed to read result file {:?}: {}", path, e))
    })?;
    serde_json::from_str(&content).map_err(|e| {
        CallerError::SubAgent(format!(
            "Failed to parse result JSON from {:?}: {}",
            path, e
        ))
    })
}

pub fn read_progress(path: &Path) -> Result<SubAgentProgress, CallerError> {
    let content = std::fs::read_to_string(path).map_err(|e| {
        CallerError::SubAgent(format!("Failed to read progress file {:?}: {}", path, e))
    })?;
    serde_json::from_str(&content).map_err(|e| {
        CallerError::SubAgent(format!(
            "Failed to parse progress JSON from {:?}: {}",
            path, e
        ))
    })
}

pub fn write_result(path: &Path, result: &SubAgentResult) -> Result<(), CallerError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(result)
        .map_err(|e| CallerError::SubAgent(format!("Failed to serialize result: {}", e)))?;
    std::fs::write(path, content)?;
    Ok(())
}

pub fn write_progress(path: &Path, progress: &SubAgentProgress) -> Result<(), CallerError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string(progress)
        .map_err(|e| CallerError::SubAgent(format!("Failed to serialize progress: {}", e)))?;
    std::fs::write(path, content)?;
    Ok(())
}

pub fn format_result_message(result: &SubAgentResult) -> String {
    let status_str = match &result.status {
        SubAgentStatus::Completed => "completed".to_string(),
        SubAgentStatus::Failed(reason) => format!("failed: {}", reason),
    };

    let mut msg = format!(
        "[Sub-Agent Result: {}]\nStatus: {}\nSummary: {}",
        result.id, status_str, result.summary
    );

    if !result.findings.is_empty() {
        msg.push_str("\nFindings:");
        for finding in &result.findings {
            msg.push_str(&format!("\n  - {}", finding));
        }
    }

    if !result.artifacts.is_empty() {
        msg.push_str("\nArtifacts:");
        for artifact in &result.artifacts {
            msg.push_str(&format!("\n  - {}", artifact.display()));
        }
    }

    msg.push_str(&format!(
        "\nTokens used: prompt={} completion={} total={}",
        result.usage.prompt_tokens, result.usage.completion_tokens, result.usage.total_tokens
    ));

    msg
}

pub fn detect_sub_agent_mode() -> Option<(String, SubAgentRole)> {
    let role = std::env::var("INTENDANT_ROLE").ok()?;
    let id = std::env::var("INTENDANT_ID").unwrap_or_else(|_| "unnamed".to_string());
    Some((id, SubAgentRole::from_str(&role)))
}

pub fn scan_completed_results(sub_agent_dir: &Path) -> Vec<SubAgentResult> {
    let mut results = Vec::new();

    if let Ok(entries) = std::fs::read_dir(sub_agent_dir) {
        for entry in entries.flatten() {
            let agent_dir = entry.path();
            let result_file = agent_dir.join("result.json");
            let reported_marker = agent_dir.join(".reported");
            if reported_marker.exists() {
                continue;
            }
            if result_file.exists() {
                if let Ok(result) = read_result(&result_file) {
                    results.push(result);
                    let _ = std::fs::write(reported_marker, b"reported");
                }
            }
        }
    }

    results
}

fn shell_escape(s: &str) -> String {
    if s.contains(' ') || s.contains('\'') || s.contains('"') || s.contains('$') || s.contains('\\')
    {
        format!("'{}'", s.replace('\'', "'\\''"))
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_spec() -> SubAgentSpec {
        SubAgentSpec {
            id: "research-1".to_string(),
            task: "Investigate the database schema".to_string(),
            role: SubAgentRole::Research,
            working_dir: PathBuf::from("/tmp/project"),
            result_file: PathBuf::from("/tmp/project/.intendant/subagents/research-1/result.json"),
            progress_file: PathBuf::from(
                "/tmp/project/.intendant/subagents/research-1/progress.json",
            ),
            system_prompt: None,
            inherit_memory: true,
        }
    }

    #[test]
    fn sub_agent_role_roundtrip() {
        assert_eq!(SubAgentRole::from_str("research"), SubAgentRole::Research);
        assert_eq!(
            SubAgentRole::from_str("implementation"),
            SubAgentRole::Implementation
        );
        assert_eq!(SubAgentRole::from_str("testing"), SubAgentRole::Testing);
        assert_eq!(
            SubAgentRole::from_str("orchestrator"),
            SubAgentRole::Orchestrator
        );
        assert_eq!(
            SubAgentRole::from_str("custom_role"),
            SubAgentRole::Custom("custom_role".to_string())
        );

        assert_eq!(SubAgentRole::Research.as_str(), "research");
        assert_eq!(SubAgentRole::Implementation.as_str(), "implementation");
        assert_eq!(SubAgentRole::Orchestrator.as_str(), "orchestrator");
    }

    #[test]
    fn build_spawn_command_format() {
        let spec = make_spec();
        let cmd = build_spawn_command(&spec, Path::new("/usr/local/bin/intendant"));

        assert!(cmd.contains("INTENDANT_ROLE=research"));
        assert!(cmd.contains("INTENDANT_ID=research-1"));
        assert!(cmd.contains("INTENDANT_RESULT_FILE="));
        assert!(cmd.contains("INTENDANT_PROGRESS_FILE="));
        assert!(cmd.contains("INTENDANT_INHERIT_MEMORY=1"));
        assert!(cmd.contains("/usr/local/bin/intendant"));
        assert!(cmd.contains("Investigate the database schema"));
    }

    #[test]
    fn build_spawn_command_no_inherit() {
        let mut spec = make_spec();
        spec.inherit_memory = false;
        let cmd = build_spawn_command(&spec, Path::new("/usr/local/bin/intendant"));
        assert!(!cmd.contains("INTENDANT_INHERIT_MEMORY"));
    }

    #[test]
    fn build_spawn_command_with_system_prompt() {
        let mut spec = make_spec();
        spec.system_prompt = Some("Custom prompt".to_string());
        let cmd = build_spawn_command(&spec, Path::new("/usr/local/bin/intendant"));
        assert!(cmd.contains("INTENDANT_SYSTEM_PROMPT='Custom prompt'"));
    }

    #[test]
    fn read_result_valid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("result.json");
        let result = SubAgentResult {
            id: "test-1".to_string(),
            status: SubAgentStatus::Completed,
            summary: "Found 3 tables".to_string(),
            findings: vec!["users table".to_string(), "orders table".to_string()],
            artifacts: vec![PathBuf::from("/tmp/schema.sql")],
            usage: TokenUsage {
                prompt_tokens: 1000,
                completion_tokens: 500,
                total_tokens: 1500,
            },
        };
        std::fs::write(&path, serde_json::to_string(&result).unwrap()).unwrap();

        let parsed = read_result(&path).unwrap();
        assert_eq!(parsed.id, "test-1");
        assert_eq!(parsed.status, SubAgentStatus::Completed);
        assert_eq!(parsed.findings.len(), 2);
    }

    #[test]
    fn read_result_invalid_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("result.json");
        std::fs::write(&path, "not valid json").unwrap();

        assert!(read_result(&path).is_err());
    }

    #[test]
    fn read_result_missing_file() {
        let path = PathBuf::from("/nonexistent/result.json");
        assert!(read_result(&path).is_err());
    }

    #[test]
    fn read_progress_valid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("progress.json");
        let progress = SubAgentProgress {
            id: "test-1".to_string(),
            turn: 5,
            status: "running".to_string(),
            last_action: "Reading file".to_string(),
            question: None,
        };
        std::fs::write(&path, serde_json::to_string(&progress).unwrap()).unwrap();

        let parsed = read_progress(&path).unwrap();
        assert_eq!(parsed.id, "test-1");
        assert_eq!(parsed.turn, 5);
        assert!(parsed.question.is_none());
    }

    #[test]
    fn read_progress_with_question() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("progress.json");
        let progress = SubAgentProgress {
            id: "test-1".to_string(),
            turn: 3,
            status: "blocked".to_string(),
            last_action: "Needs clarification".to_string(),
            question: Some("Which database should I use?".to_string()),
        };
        std::fs::write(&path, serde_json::to_string(&progress).unwrap()).unwrap();

        let parsed = read_progress(&path).unwrap();
        assert_eq!(
            parsed.question.as_deref(),
            Some("Which database should I use?")
        );
    }

    #[test]
    fn write_result_creates_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("subdir/nested/result.json");
        let result = SubAgentResult {
            id: "test-1".to_string(),
            status: SubAgentStatus::Completed,
            summary: "Done".to_string(),
            findings: vec![],
            artifacts: vec![],
            usage: TokenUsage::default(),
        };
        write_result(&path, &result).unwrap();
        assert!(path.exists());
        let parsed = read_result(&path).unwrap();
        assert_eq!(parsed.id, "test-1");
    }

    #[test]
    fn write_and_read_progress() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("progress.json");
        let progress = SubAgentProgress {
            id: "agent-2".to_string(),
            turn: 7,
            status: "running".to_string(),
            last_action: "Compiling".to_string(),
            question: None,
        };
        write_progress(&path, &progress).unwrap();
        let parsed = read_progress(&path).unwrap();
        assert_eq!(parsed.id, "agent-2");
        assert_eq!(parsed.turn, 7);
    }

    #[test]
    fn format_result_message_completed() {
        let result = SubAgentResult {
            id: "research-1".to_string(),
            status: SubAgentStatus::Completed,
            summary: "Found the database schema".to_string(),
            findings: vec![
                "3 tables found".to_string(),
                "No migrations pending".to_string(),
            ],
            artifacts: vec![PathBuf::from("/tmp/schema.sql")],
            usage: TokenUsage {
                prompt_tokens: 1000,
                completion_tokens: 500,
                total_tokens: 1500,
            },
        };
        let msg = format_result_message(&result);
        assert!(msg.contains("[Sub-Agent Result: research-1]"));
        assert!(msg.contains("Status: completed"));
        assert!(msg.contains("Found the database schema"));
        assert!(msg.contains("3 tables found"));
        assert!(msg.contains("schema.sql"));
        assert!(msg.contains("1500"));
    }

    #[test]
    fn format_result_message_failed() {
        let result = SubAgentResult {
            id: "impl-1".to_string(),
            status: SubAgentStatus::Failed("Compilation error".to_string()),
            summary: "Could not compile the module".to_string(),
            findings: vec![],
            artifacts: vec![],
            usage: TokenUsage::default(),
        };
        let msg = format_result_message(&result);
        assert!(msg.contains("failed: Compilation error"));
        assert!(!msg.contains("Findings:"));
        assert!(!msg.contains("Artifacts:"));
    }

    #[test]
    fn format_result_message_no_findings() {
        let result = SubAgentResult {
            id: "test-1".to_string(),
            status: SubAgentStatus::Completed,
            summary: "All tests passed".to_string(),
            findings: vec![],
            artifacts: vec![],
            usage: TokenUsage::default(),
        };
        let msg = format_result_message(&result);
        assert!(!msg.contains("Findings:"));
    }

    #[test]
    fn scan_completed_results_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let results = scan_completed_results(dir.path());
        assert!(results.is_empty());
    }

    #[test]
    fn scan_completed_results_finds_results() {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("agent-1");
        std::fs::create_dir_all(&agent_dir).unwrap();

        let result = SubAgentResult {
            id: "agent-1".to_string(),
            status: SubAgentStatus::Completed,
            summary: "Done".to_string(),
            findings: vec![],
            artifacts: vec![],
            usage: TokenUsage::default(),
        };
        std::fs::write(
            agent_dir.join("result.json"),
            serde_json::to_string(&result).unwrap(),
        )
        .unwrap();

        let results = scan_completed_results(dir.path());
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "agent-1");

        let results_again = scan_completed_results(dir.path());
        assert!(results_again.is_empty());
    }

    #[test]
    fn scan_completed_results_nonexistent_dir() {
        let results = scan_completed_results(Path::new("/nonexistent/dir"));
        assert!(results.is_empty());
    }

    #[test]
    fn detect_sub_agent_mode_not_set() {
        // This test is sensitive to env; in normal test runs INTENDANT_ROLE is not set
        std::env::remove_var("INTENDANT_ROLE");
        assert!(detect_sub_agent_mode().is_none());
    }

    #[test]
    fn sub_agent_spec_serialization() {
        let spec = make_spec();
        let json = serde_json::to_string(&spec).unwrap();
        let parsed: SubAgentSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, "research-1");
        assert_eq!(parsed.role, SubAgentRole::Research);
        assert!(parsed.inherit_memory);
    }

    #[test]
    fn sub_agent_result_serialization() {
        let result = SubAgentResult {
            id: "test-1".to_string(),
            status: SubAgentStatus::Failed("timeout".to_string()),
            summary: "Timed out".to_string(),
            findings: vec!["partial result".to_string()],
            artifacts: vec![],
            usage: TokenUsage {
                prompt_tokens: 100,
                completion_tokens: 50,
                total_tokens: 150,
            },
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: SubAgentResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.status, SubAgentStatus::Failed("timeout".to_string()));
    }

    #[test]
    fn shell_escape_simple() {
        assert_eq!(shell_escape("hello"), "hello");
        assert_eq!(shell_escape("/usr/bin/caller"), "/usr/bin/caller");
    }

    #[test]
    fn shell_escape_with_spaces() {
        assert_eq!(shell_escape("hello world"), "'hello world'");
    }

    #[test]
    fn shell_escape_with_quotes() {
        assert_eq!(shell_escape("it's"), "'it'\\''s'");
    }
}
