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
    RoundComplete {
        round: usize,
        turns_in_round: usize,
    },
    DisplayReady {
        display_id: u32,
        vnc_port: Option<u32>,
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
}
