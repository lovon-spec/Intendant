//! Shared type definitions used across all frontends (TUI, MCP, control socket, web gateway).
//!
//! These types were extracted from `tui/app.rs` and `control.rs` so that non-TUI
//! modules no longer need to reach into `tui::` for shared vocabulary.

use serde::Serialize;

// ---------------------------------------------------------------------------
// Agent loop phases
// ---------------------------------------------------------------------------

/// Current phase of the agent loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    Thinking,
    RunningAgent,
    Orchestrating,
    WaitingApproval,
    WaitingHuman,
    WaitingFollowUp,
    Idle,
    Done,
}

// ---------------------------------------------------------------------------
// Log levels and verbosity
// ---------------------------------------------------------------------------

/// Log entry severity / source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Model,
    Agent,
    Error,
    Warn,
    SubAgent,
    /// Operational detail — visible at Verbose and Debug, hidden at Normal.
    /// Use for token counts, auto-approved commands, presence lifecycle, etc.
    Detail,
    Debug,
}

/// Log verbosity profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verbosity {
    Quiet,
    Normal,
    Verbose,
    Debug,
}

impl Verbosity {
    pub fn next(self) -> Self {
        match self {
            Self::Quiet => Self::Normal,
            Self::Normal => Self::Verbose,
            Self::Verbose => Self::Debug,
            Self::Debug => Self::Quiet,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Quiet => "Quiet",
            Self::Normal => "Normal",
            Self::Verbose => "Verbose",
            Self::Debug => "Debug",
        }
    }

    pub fn includes(self, level: &LogLevel) -> bool {
        match self {
            Self::Quiet => matches!(level, LogLevel::Warn | LogLevel::Error),
            Self::Normal => matches!(
                level,
                LogLevel::Info
                    | LogLevel::Model
                    | LogLevel::Agent
                    | LogLevel::Warn
                    | LogLevel::Error
                    | LogLevel::SubAgent
            ),
            Self::Verbose => !matches!(level, LogLevel::Debug),
            Self::Debug => true,
        }
    }

    /// Short indicator shown in log panel for each verbosity level.
    pub fn hint(self) -> &'static str {
        match self {
            Self::Quiet => "Warn+Error only",
            Self::Normal => "Key events",
            Self::Verbose => "+detail, agent output",
            Self::Debug => "+raw model/JSON",
        }
    }
}

// ---------------------------------------------------------------------------
// Outbound events (control socket / web gateway / MCP)
// ---------------------------------------------------------------------------

/// Events sent to connected control socket clients, web gateway, and MCP.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
#[allow(dead_code)]
pub enum OutboundEvent {
    TurnStarted {
        turn: usize,
        budget_pct: f64,
    },
    AgentOutput {
        stdout: String,
        stderr: String,
    },
    ApprovalRequired {
        id: u64,
        command: String,
    },
    AskHuman {
        question: String,
    },
    TaskComplete {
        reason: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
    },
    SessionStarted {
        session_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        task: Option<String>,
    },
    SessionEnded {
        session_id: String,
        reason: String,
    },
    RoundComplete {
        round: usize,
        turns_in_round: usize,
    },
    DebugScreenReady {
        display_id: u32,
        vnc_port: u32,
    },
    DebugScreenTornDown {
        display_id: u32,
    },
    DisplayReady {
        display_id: u32,
        vnc_port: Option<u32>,
        width: u32,
        height: u32,
    },
    DisplayTaken {
        display_id: u32,
    },
    DisplayReleased {
        display_id: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    UserDisplayGranted,
    UserDisplayRevoked {
        #[serde(skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    RecordingStarted {
        stream_name: String,
    },
    RecordingStopped {
        stream_name: String,
    },
    RecordingDeleted {
        stream_name: String,
    },
    RecordingError {
        stream_name: String,
        message: String,
    },
    Status {
        turn: usize,
        phase: String,
        autonomy: String,
        session_id: String,
        task: String,
    },
    Usage {
        main: crate::frontend::ModelUsageSnapshot,
        #[serde(skip_serializing_if = "Option::is_none")]
        presence: Option<crate::frontend::ModelUsageSnapshot>,
    },
    UsageUpdate {
        main: crate::frontend::ModelUsageSnapshot,
        #[serde(skip_serializing_if = "Option::is_none")]
        presence: Option<crate::frontend::ModelUsageSnapshot>,
    },
    CommandResult {
        action: String,
        ok: bool,
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        data: Option<serde_json::Value>,
    },
    UserTranscript {
        text: String,
        seq: u64,
    },
    ModelSummary {
        turn: usize,
        summary: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning_summary: Option<String>,
    },
    // --- New variants for broadcast decoupling ---
    ModelResponseDelta {
        text: String,
    },
    AgentStarted {
        turn: usize,
        commands_preview: String,
    },
    DoneSignal {
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    AutoApproved {
        preview: String,
    },
    ApprovalResolved {
        id: u64,
        action: String,
    },
    ContextManagement {
        turn: usize,
    },
    BudgetWarning {
        pct: f64,
        remaining: u64,
    },
    BudgetExhausted {
        remaining: u64,
    },
    LoopError {
        message: String,
    },
    SubAgentResult {
        summary: String,
    },
    OrchestratorProgress {
        status: String,
    },
    ModelResponse {
        turn: usize,
        summary: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning_summary: Option<String>,
    },
    HumanResponseSent,
    SafetyCapReached,
    PresenceLog {
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        level: Option<String>,
    },
    PresenceUsageUpdate {
        total_tokens: u64,
        context_window: u64,
        usage_pct: f64,
        provider: String,
        model: String,
        #[serde(default)]
        prompt_tokens: u64,
        #[serde(default)]
        completion_tokens: u64,
        #[serde(default)]
        cached_tokens: u64,
    },
    LiveUsageUpdate {
        provider: String,
        model: String,
        input_tokens: u64,
        output_tokens: u64,
        cached_tokens: u64,
        total_tokens: u64,
        thinking_tokens: u64,
    },
    /// App-originated log entry broadcast to external consumers.
    LogEntry {
        level: String,
        source: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        turn: Option<usize>,
    },
}

// ---------------------------------------------------------------------------
// Utility functions
// ---------------------------------------------------------------------------

/// Truncate a string to a maximum byte length, respecting character boundaries.
pub fn truncate_str(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        &s[..end]
    }
}

/// Format a human-readable summary of a model's JSON response.
/// Extracts command functions and their key parameters (command strings, paths, etc.)
/// instead of showing raw JSON.
pub fn format_model_summary(content: &str) -> String {
    let parsed: serde_json::Value = match serde_json::from_str(content) {
        Ok(v) => v,
        Err(_) => {
            // Not valid JSON — return the full text for multi-line rendering.
            return content.to_string();
        }
    };

    let commands = match parsed.get("commands").and_then(|c| c.as_array()) {
        Some(cmds) if !cmds.is_empty() => cmds,
        _ => {
            if parsed
                .get("done")
                .and_then(|d| d.as_bool())
                .unwrap_or(false)
            {
                return "done signal".to_string();
            }
            return "no commands".to_string();
        }
    };

    let summaries: Vec<String> = commands
        .iter()
        .map(|cmd| {
            let func = cmd.get("function").and_then(|f| f.as_str()).unwrap_or("?");
            match func {
                "execAsAgent" => {
                    let command = cmd.get("command").and_then(|c| c.as_str()).unwrap_or("?");
                    let truncated = truncate_str(command, 120);
                    format!("exec: {}", truncated)
                }
                "editFile" => {
                    let path = cmd.get("file_path").and_then(|p| p.as_str()).unwrap_or("?");
                    let op = cmd.get("operation").and_then(|o| o.as_str()).unwrap_or("?");
                    format!("edit: {} ({})", path, op)
                }
                "inspectPath" => {
                    let path = cmd.get("path").and_then(|p| p.as_str()).unwrap_or("?");
                    format!("inspect: {}", path)
                }
                "browse" => {
                    let url = cmd.get("url").and_then(|u| u.as_str()).unwrap_or("?");
                    format!("browse: {}", truncate_str(url, 80))
                }
                "askHuman" => {
                    let q = cmd.get("question").and_then(|q| q.as_str()).unwrap_or("?");
                    format!("ask: {}", truncate_str(q, 100))
                }
                "storeMemory" => {
                    let key = cmd
                        .get("memory_key")
                        .and_then(|k| k.as_str())
                        .unwrap_or("?");
                    format!("store: {}", key)
                }
                "recallMemory" => {
                    let q = cmd
                        .get("memory_query")
                        .and_then(|q| q.as_str())
                        .unwrap_or("?");
                    format!("recall: {}", q)
                }
                "execPty" => {
                    let command = cmd.get("command").and_then(|c| c.as_str()).unwrap_or("?");
                    format!("pty: {}", truncate_str(command, 120))
                }
                _ => func.to_string(),
            }
        })
        .collect();

    summaries.join(" | ")
}
