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

/// One non-image file attached to a user message.
///
/// None of the three backends (Codex, Gemini, Claude Code) expose a native
/// "document" content block as of now, so we stage the file at a stable
/// path inside (or near) the workspace and lean on the agent's existing
/// file-read tools. The accompanying user message gets a short prelude
/// pointing at the path — see `format_file_attachments_prelude`.
#[derive(Debug, Clone)]
pub struct AgentFileAttachment {
    /// Path on disk where the file lives. Should be inside (or reachable
    /// from) the agent's workspace so its file-read tool can open it.
    pub local_path: PathBuf,
    /// Original filename for display in the message prelude.
    pub name: String,
    /// MIME type for reporting / potential native block use later.
    pub mime_type: String,
    /// Size in bytes (helpful for the prelude line and for the model to
    /// decide whether to read the full file or stream).
    pub size: u64,
}

/// One attachment of arbitrary kind. The dashboard produces these via the
/// Attach modal and the agent loop's `resolve_attachments` maps a mixed
/// list of `frame:<id>` / `upload:<id>` ids into this shape before
/// handing off to the backend's `send_message_with_attachments`.
#[derive(Debug, Clone)]
pub enum AgentAttachment {
    Image(AgentImageAttachment),
    File(AgentFileAttachment),
}

impl AgentAttachment {
    /// Images flow through each backend's native image path; files need
    /// the "stage + point" workaround. Exposed as a method so call sites
    /// reading a heterogeneous `&[AgentAttachment]` can split into two
    /// buckets cleanly.
    pub fn is_image(&self) -> bool {
        matches!(self, AgentAttachment::Image(_))
    }
}

/// Build the short prelude that precedes a user's message when the task
/// carries one or more non-image file attachments. Tells the model what
/// files are available and where to find them, without pretending the
/// backend has a real "document" content block.
///
/// Empty string when there are no file attachments — callers can
/// concatenate unconditionally.
pub fn format_file_attachments_prelude(files: &[&AgentFileAttachment]) -> String {
    if files.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "The user attached the following file(s). Read them with your file \
         tools when relevant; paths are absolute.\n\n",
    );
    for f in files {
        // Humanised size: "123 B" / "1.2 KB" / "4.3 MB". Nothing fancy —
        // just avoids showing raw byte counts for multi-MB PDFs.
        let size = human_bytes(f.size);
        out.push_str(&format!(
            "- `{}` ({}, {}) — path: {}\n",
            f.name,
            f.mime_type,
            size,
            f.local_path.display(),
        ));
    }
    out.push('\n');
    out
}

fn human_bytes(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * 1024 * 1024;
    if n >= GB {
        format!("{:.1} GB", n as f64 / GB as f64)
    } else if n >= MB {
        format!("{:.1} MB", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{:.1} KB", n as f64 / KB as f64)
    } else {
        format!("{} B", n)
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

    pub fn thread_id_is_canonical(&self, thread_id: &str) -> bool {
        let thread_id = thread_id.trim();
        if thread_id.is_empty() {
            return false;
        }
        match self {
            AgentBackend::Codex | AgentBackend::GeminiCli => true,
            // Claude Code does not expose a real session id during start_thread
            // today. Keep the Intendant log id as canonical until that backend
            // reports a usable native id.
            AgentBackend::ClaudeCode => thread_id != "claude-code-session",
        }
    }

    pub fn supports_user_message_rewind(&self) -> bool {
        matches!(self, AgentBackend::Codex)
    }
}

pub fn source_session_id_is_canonical(source: &str, session_id: &str) -> bool {
    AgentBackend::from_str_loose(source)
        .map(|backend| backend.thread_id_is_canonical(session_id))
        .unwrap_or(false)
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
    /// Echo of a user message observed by the external runtime. This is used
    /// internally to confirm that an accepted steer reached the conversation;
    /// it is not rendered as agent output.
    UserMessage { text: String },
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
    /// Token usage update reported by the external agent runtime.
    Usage { usage: AgentUsageSnapshot },
    /// Informational backend event that should be written to the activity log.
    Log { level: String, message: String },
    /// An external runtime spawned or interacted with native sub-agents.
    SubAgentToolCall {
        item_id: String,
        tool: String,
        status: String,
        sender_thread_id: String,
        receiver_thread_ids: Vec<String>,
        prompt: Option<String>,
        model: Option<String>,
        reasoning_effort: Option<String>,
        agents: Vec<SubAgentState>,
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
pub struct SubAgentState {
    pub thread_id: String,
    pub status: String,
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentUsageSnapshot {
    pub provider: String,
    pub model: String,
    pub tokens_used: u64,
    pub context_window: u64,
    pub usage_pct: f64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_tokens: u64,
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
    /// Directory where a backend can write exact model request payload traces.
    /// Backends that cannot capture provider-bound request bodies ignore it.
    pub request_trace_dir: Option<PathBuf>,
    pub approval_policy: String,
    /// Sandbox mode for Codex: `"read-only"`, `"workspace-write"`, or
    /// `"danger-full-access"`. Ignored by backends that don't model a
    /// sandbox (pass `String::new()` for those).
    pub sandbox: String,
    /// Codex reasoning-effort override (`low|medium|high|...`). Codex-only;
    /// other backends ignore.
    pub reasoning_effort: Option<String>,
    /// Enable Codex's `web_search` Responses tool. Codex-only.
    pub web_search: bool,
    /// Allow outbound network in Codex's `workspace-write` sandbox.
    /// Codex-only; ignored by other sandbox modes and other backends.
    pub network_access: bool,
    /// Extra writable roots for Codex's sandbox. Codex-only; other backends
    /// ignore.
    pub writable_roots: Vec<String>,
    /// Web gateway port for MCP-over-HTTP config generation.
    pub web_port: Option<u16>,
    /// Persisted backend-native session/thread id to resume instead of
    /// starting a fresh external conversation.
    pub resume_session: Option<String>,
}

/// Handle to a conversation thread within an external agent.
pub struct AgentThread {
    pub thread_id: String,
}

/// Exact model request payload exposed by an external agent backend.
#[derive(Debug, Clone)]
pub struct AgentContextSnapshot {
    pub source: String,
    pub label: String,
    pub format: String,
    pub token_count: Option<u64>,
    pub context_window: Option<u64>,
    pub item_count: Option<usize>,
    pub raw: serde_json::Value,
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

    /// Return the latest exact model request payload captured at the provider
    /// boundary. Backends without such a payload return `None`; callers should
    /// not synthesize transcript-shaped replacements.
    async fn context_snapshot(&mut self) -> Result<Option<AgentContextSnapshot>, CallerError> {
        Ok(None)
    }

    /// Send a user message with a heterogeneous list of attachments
    /// (images + files). Default implementation routes images through
    /// `send_message_with_images` and prepends a prelude describing any
    /// file attachments at stable paths. Backends that grow a native
    /// "document" content block later can override this to pass files
    /// through the wire protocol instead of staging + pointing.
    async fn send_message_with_attachments(
        &mut self,
        thread: &AgentThread,
        message: &str,
        attachments: &[AgentAttachment],
    ) -> Result<(), CallerError> {
        let images: Vec<AgentImageAttachment> = attachments
            .iter()
            .filter_map(|a| match a {
                AgentAttachment::Image(img) => Some(img.clone()),
                AgentAttachment::File(_) => None,
            })
            .collect();
        let files: Vec<&AgentFileAttachment> = attachments
            .iter()
            .filter_map(|a| match a {
                AgentAttachment::File(f) => Some(f),
                AgentAttachment::Image(_) => None,
            })
            .collect();
        let prelude = format_file_attachments_prelude(&files);
        // Prelude comes BEFORE the user's message so the model reads the
        // attachment list first, then the actual instruction.
        let augmented = if prelude.is_empty() {
            message.to_string()
        } else {
            format!("{}{}", prelude, message)
        };
        self.send_message_with_images(thread, &augmented, &images)
            .await
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

    /// Inject user text into the currently running turn without interrupting
    /// it. Backends that support native mid-turn steering (Codex via
    /// `turn/steer`) override this; the default returns a typed error so the
    /// caller can fall back to queuing the text onto `context_injection` and
    /// delivering it at the start of the next turn.
    ///
    /// The error message is load-bearing: `drain_external_agent_events`
    /// distinguishes "native steer failed" from "native steer unsupported"
    /// only via the error's short string form. We intentionally don't model
    /// the distinction in the type system because every backend eventually
    /// gains native support, at which point the fallback path is vestigial.
    async fn steer_turn(&mut self, text: &str) -> Result<(), CallerError> {
        let _ = text;
        Err(CallerError::ExternalAgent(
            "mid-turn steering not supported by this backend".into(),
        ))
    }

    /// Dispatch a backend-specific thread action (Codex: compact, fork, side,
    /// rollback, review, memory-reset; other backends currently reject).
    /// Returns a short human-readable status message on success.
    async fn thread_action(
        &mut self,
        op: &str,
        params: &serde_json::Value,
    ) -> Result<String, CallerError> {
        let _ = params;
        Err(CallerError::ExternalAgent(format!(
            "thread action /{} not supported by this backend",
            op
        )))
    }

    fn supports_user_message_rewind(&self) -> bool {
        false
    }

    /// Ask the backend to drop the last `turns_to_drop` conversational
    /// turns from the active thread. Backends that implement this
    /// (Codex, via `thread/rollback`) override it; backends that don't
    /// (Claude Code, Gemini) return the default error and the caller
    /// falls back to a session reset — shut down, re-initialize, start
    /// a new thread.
    ///
    /// The error message is load-bearing: the caller distinguishes
    /// "rollback not supported" from "rollback failed" purely by type
    /// (typed error → fall back; Ok → success).
    async fn rollback_turns(&mut self, turns_to_drop: u32) -> Result<(), CallerError> {
        let _ = turns_to_drop;
        Err(CallerError::ExternalAgent(
            "conversation rollback not supported by this backend".into(),
        ))
    }

    /// Ask the backend to drop the last `turns_to_drop` conversational
    /// turns from a specific thread. This is used for Codex side
    /// conversations, where the side child must be rewound without
    /// touching the parent thread.
    async fn rollback_thread_turns(
        &mut self,
        thread_id: &str,
        turns_to_drop: u32,
    ) -> Result<(), CallerError> {
        let _ = (thread_id, turns_to_drop);
        Err(CallerError::ExternalAgent(
            "targeted conversation rollback not supported by this backend".into(),
        ))
    }

    /// Restore the backend adapter's notion of the active thread after a
    /// targeted child-thread turn. This is local adapter state: it does not
    /// send a provider request.
    async fn activate_thread(&mut self, thread_id: &str) -> Result<(), CallerError> {
        let _ = thread_id;
        Ok(())
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
        for v in [
            AgentBackend::Codex,
            AgentBackend::ClaudeCode,
            AgentBackend::GeminiCli,
        ] {
            assert_eq!(AgentBackend::from_str_loose(v.as_short_str()), Some(v));
        }
    }

    #[test]
    fn canonical_thread_ids_match_backend_capabilities() {
        assert!(AgentBackend::Codex.thread_id_is_canonical("019e37cf-34ad-7b08-8a1e-7ad5086eb39f"));
        assert!(AgentBackend::GeminiCli.thread_id_is_canonical("session-2026-05-21T12-00"));
        assert!(!AgentBackend::ClaudeCode.thread_id_is_canonical("claude-code-session"));
        assert!(AgentBackend::ClaudeCode.thread_id_is_canonical("real-claude-session"));
        assert!(!source_session_id_is_canonical("unknown", "abc"));
        assert!(source_session_id_is_canonical("codex", "019abc"));
    }

    #[test]
    fn user_message_rewind_capability_is_explicit() {
        assert!(AgentBackend::Codex.supports_user_message_rewind());
        assert!(!AgentBackend::ClaudeCode.supports_user_message_rewind());
        assert!(!AgentBackend::GeminiCli.supports_user_message_rewind());
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
