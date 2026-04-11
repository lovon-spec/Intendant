use crate::error::CallerError;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::sync::mpsc;

pub mod codex;
pub mod gemini;

/// Identifies which external agent backend is in use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentBackend {
    Codex,
    ClaudeCode,
    GeminiCli,
}

impl AgentBackend {
    pub fn from_str_loose(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "codex" => Some(Self::Codex),
            "claude-code" | "claude_code" | "claudecode" | "cc" => Some(Self::ClaudeCode),
            "gemini" | "gemini-cli" | "gemini_cli" => Some(Self::GeminiCli),
            _ => None,
        }
    }
}

impl std::fmt::Display for AgentBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentBackend::Codex => write!(f, "Codex"),
            AgentBackend::ClaudeCode => write!(f, "Claude Code"),
            AgentBackend::GeminiCli => write!(f, "Gemini CLI"),
        }
    }
}

/// Events emitted by an external agent, normalized to Intendant concepts.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// Incremental text from the agent's message.
    MessageDelta { text: String },
    /// Complete agent message.
    Message { text: String },
    /// A tool/command execution has started.
    ToolStarted {
        item_id: String,
        tool_name: String,
        preview: String,
    },
    /// Incremental output from a running tool.
    ToolOutputDelta { item_id: String, text: String },
    /// A tool execution completed.
    ToolCompleted {
        item_id: String,
        status: ToolCompletionStatus,
    },
    /// The agent requests approval to execute a command.
    ApprovalRequest {
        request_id: String,
        command: String,
        category: ApprovalCategory,
    },
    /// The agent requests approval for a file change.
    FileApprovalRequest {
        request_id: String,
        path: String,
        diff: String,
    },
    /// The agent's turn is complete.
    TurnCompleted { message: Option<String> },
    /// A diff of files changed so far.
    DiffUpdated {
        files_changed: Vec<String>,
        unified_diff: String,
    },
    /// The agent process terminated.
    Terminated {
        reason: String,
        exit_code: Option<i32>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolCompletionStatus {
    Success,
    Failed { message: String },
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalCategory {
    CommandExecution,
    FileChange,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    Accept,
    AcceptForSession,
    Decline,
    Cancel,
}

/// Configuration passed to an external agent on initialization.
pub struct AgentConfig {
    pub model: Option<String>,
    pub working_dir: PathBuf,
    pub approval_policy: String,
    pub sandbox: bool,
    /// Web gateway port for MCP-over-HTTP config generation.
    pub web_port: Option<u16>,
}

/// Handle to a conversation thread within an external agent.
pub struct AgentThread {
    pub thread_id: String,
}

/// Trait for opaque external agent backends.
///
/// Intendant supervises the agent, bridges approval requests to its
/// TUI/web/MCP frontends, and translates [`AgentEvent`]s for display.
#[async_trait]
pub trait ExternalAgent: Send + Sync {
    /// Human-readable name of this backend.
    fn name(&self) -> &str;

    /// Start the agent process and return a receiver for events.
    async fn initialize(
        &mut self,
        config: AgentConfig,
    ) -> Result<mpsc::UnboundedReceiver<AgentEvent>, CallerError>;

    /// Create a new conversation thread.
    async fn start_thread(&mut self) -> Result<AgentThread, CallerError>;

    /// Send a user message into an existing thread (starts a turn).
    async fn send_message(
        &mut self,
        thread: &AgentThread,
        message: &str,
    ) -> Result<(), CallerError>;

    /// Respond to an approval request from the agent.
    async fn resolve_approval(
        &mut self,
        request_id: &str,
        decision: ApprovalDecision,
    ) -> Result<(), CallerError>;

    /// Shut down the agent process.
    async fn shutdown(&mut self) -> Result<(), CallerError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_str_loose_codex() {
        assert_eq!(
            AgentBackend::from_str_loose("codex"),
            Some(AgentBackend::Codex)
        );
    }

    #[test]
    fn from_str_loose_claude_code_variants() {
        assert_eq!(
            AgentBackend::from_str_loose("claude-code"),
            Some(AgentBackend::ClaudeCode)
        );
        assert_eq!(
            AgentBackend::from_str_loose("claude_code"),
            Some(AgentBackend::ClaudeCode)
        );
        assert_eq!(
            AgentBackend::from_str_loose("claudecode"),
            Some(AgentBackend::ClaudeCode)
        );
        assert_eq!(
            AgentBackend::from_str_loose("cc"),
            Some(AgentBackend::ClaudeCode)
        );
    }

    #[test]
    fn from_str_loose_case_insensitive() {
        assert_eq!(
            AgentBackend::from_str_loose("CODEX"),
            Some(AgentBackend::Codex)
        );
        assert_eq!(
            AgentBackend::from_str_loose("Claude-Code"),
            Some(AgentBackend::ClaudeCode)
        );
        assert_eq!(
            AgentBackend::from_str_loose("CC"),
            Some(AgentBackend::ClaudeCode)
        );
    }

    #[test]
    fn from_str_loose_gemini_variants() {
        assert_eq!(
            AgentBackend::from_str_loose("gemini"),
            Some(AgentBackend::GeminiCli)
        );
        assert_eq!(
            AgentBackend::from_str_loose("gemini-cli"),
            Some(AgentBackend::GeminiCli)
        );
        assert_eq!(
            AgentBackend::from_str_loose("gemini_cli"),
            Some(AgentBackend::GeminiCli)
        );
        assert_eq!(
            AgentBackend::from_str_loose("GEMINI"),
            Some(AgentBackend::GeminiCli)
        );
    }

    #[test]
    fn from_str_loose_invalid() {
        assert_eq!(AgentBackend::from_str_loose(""), None);
        assert_eq!(AgentBackend::from_str_loose("gpt"), None);
        assert_eq!(AgentBackend::from_str_loose("claude"), None);
    }

    #[test]
    fn display_impl() {
        assert_eq!(format!("{}", AgentBackend::Codex), "Codex");
        assert_eq!(format!("{}", AgentBackend::ClaudeCode), "Claude Code");
        assert_eq!(format!("{}", AgentBackend::GeminiCli), "Gemini CLI");
    }

    #[test]
    fn serde_roundtrip() {
        let json = serde_json::to_string(&AgentBackend::Codex).unwrap();
        assert_eq!(json, r#""codex""#);

        let parsed: AgentBackend = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, AgentBackend::Codex);

        let json = serde_json::to_string(&AgentBackend::ClaudeCode).unwrap();
        assert_eq!(json, r#""claude_code""#);

        let parsed: AgentBackend = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, AgentBackend::ClaudeCode);

        let json = serde_json::to_string(&AgentBackend::GeminiCli).unwrap();
        assert_eq!(json, r#""gemini_cli""#);

        let parsed: AgentBackend = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, AgentBackend::GeminiCli);
    }
}
