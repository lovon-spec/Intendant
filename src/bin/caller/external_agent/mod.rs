use crate::conversation::ImageData;
use crate::error::CallerError;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::sync::mpsc;

pub mod claude_code;
pub mod codex;
pub mod gemini;

/// One image attachment passed alongside a user message.
///
/// Some backends (Codex) prefer file paths to keep base64 out of the JSON-RPC
/// stream; others (Gemini ACP) embed base64 inline in `ContentBlock::Image`.
/// We pass both so each backend can pick the form it supports best.
#[derive(Debug, Clone)]
pub struct AgentImageAttachment {
    /// Path on disk where the image is stored (used by Codex `LocalImage`).
    pub local_path: Option<PathBuf>,
    /// Base64-encoded image data (used by Gemini ACP `Image` content block).
    pub base64: String,
    /// MIME type, e.g. `image/jpeg`.
    pub mime_type: String,
}

impl AgentImageAttachment {
    /// Build from a `conversation::ImageData` (base64 only — no on-disk path).
    pub fn from_image_data(img: &ImageData) -> Self {
        Self {
            local_path: None,
            base64: img.data.clone(),
            mime_type: img.media_type.clone(),
        }
    }

    /// Build from on-disk frame data, capturing both path and base64.
    pub fn from_frame_path(path: PathBuf, base64: String, mime_type: String) -> Self {
        Self {
            local_path: Some(path),
            base64,
            mime_type,
        }
    }
}

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
        // Accept the canonical short forms (what the dashboard and new
        // TOML writes use) plus the Display forms ("Claude Code",
        // "Gemini CLI" — with spaces) so existing intendant.toml files
        // that were written by earlier code still parse. Case-insensitive.
        match s.to_lowercase().as_str() {
            "codex" => Some(Self::Codex),
            "claude-code" | "claude_code" | "claudecode" | "cc" | "claude code" => {
                Some(Self::ClaudeCode)
            }
            "gemini" | "gemini-cli" | "gemini_cli" | "gemini cli" => Some(Self::GeminiCli),
            _ => None,
        }
    }

    /// Canonical short-form identifier used in wire formats and the
    /// `[agent] default_backend` TOML field. Matches the `<option value>`
    /// attributes in the dashboard's external-agent dropdown, so a
    /// round-trip through /api/settings preserves identity.
    pub fn as_short_str(&self) -> &'static str {
        match self {
            AgentBackend::Codex => "codex",
            AgentBackend::ClaudeCode => "claude-code",
            AgentBackend::GeminiCli => "gemini",
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
    /// The agent's chain-of-thought / reasoning trace.
    ///
    /// Codex emits this via `item/completed` with `type: "reasoning"`. The
    /// text is surfaced at `"detail"` verbosity (visible in Verbose + Debug,
    /// hidden in Normal) via `AppEvent::ModelResponse` with `reasoning` set.
    Reasoning { text: String },
    /// The agent's execution plan (task decomposition with status).
    ///
    /// Each entry is `(content, priority, status)` as plain strings so that
    /// the external-agent module doesn't leak ACP schema types.
    PlanUpdate {
        entries: Vec<(String, String, String)>,
    },
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

/// Re-export of the shared approval decision type. The canonical
/// definition lives in [`crate::approval`] because `peer::event`
/// needs the same vocabulary and a duplicate would drift.
pub use crate::approval::ApprovalDecision;

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

    /// Send a user message with attached images. Default implementation
    /// falls back to text-only `send_message`, ignoring attachments — backends
    /// that support multimodal input should override this.
    async fn send_message_with_images(
        &mut self,
        thread: &AgentThread,
        message: &str,
        images: &[AgentImageAttachment],
    ) -> Result<(), CallerError> {
        let _ = images;
        self.send_message(thread, message).await
    }

    /// Respond to an approval request from the agent.
    async fn resolve_approval(
        &mut self,
        request_id: &str,
        decision: ApprovalDecision,
    ) -> Result<(), CallerError>;

    /// Request interruption of the current turn. Default implementation is a no-op
    /// for backends that don't support mid-turn interruption.
    ///
    /// Backends that implement this should:
    /// - Send their protocol-specific cancel/interrupt message
    /// - Clean up any pending approval state
    /// - Let the reader task emit a final TurnCompleted or Terminated event
    ///
    /// This is a best-effort — if the backend can't cleanly interrupt, it may
    /// return an error or the caller may need to escalate to `shutdown()`.
    async fn interrupt_turn(&mut self) -> Result<(), CallerError> {
        Err(CallerError::ExternalAgent(
            "interruption not supported by this backend".into(),
        ))
    }

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
    fn from_str_loose_accepts_display_forms() {
        // The Display impl produces "Codex" / "Claude Code" / "Gemini CLI".
        // `from_str_loose` must accept those (lowercased) so TOML files
        // written in the Display form by earlier code don't break startup.
        assert_eq!(
            AgentBackend::from_str_loose("Gemini CLI"),
            Some(AgentBackend::GeminiCli)
        );
        assert_eq!(
            AgentBackend::from_str_loose("Claude Code"),
            Some(AgentBackend::ClaudeCode)
        );
        assert_eq!(
            AgentBackend::from_str_loose("gemini cli"),
            Some(AgentBackend::GeminiCli)
        );
    }

    #[test]
    fn as_short_str_matches_dashboard_option_values() {
        // These MUST match the <option value> attributes in the Settings
        // dropdown or the TOML round-trip breaks.
        assert_eq!(AgentBackend::Codex.as_short_str(), "codex");
        assert_eq!(AgentBackend::ClaudeCode.as_short_str(), "claude-code");
        assert_eq!(AgentBackend::GeminiCli.as_short_str(), "gemini");
        // And from_str_loose must round-trip every as_short_str output.
        for v in [AgentBackend::Codex, AgentBackend::ClaudeCode, AgentBackend::GeminiCli] {
            assert_eq!(AgentBackend::from_str_loose(v.as_short_str()), Some(v));
        }
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
