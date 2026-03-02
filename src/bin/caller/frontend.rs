//! Shared frontend contract for TUI and MCP interfaces.
//!
//! This module defines the canonical set of **actions** a user or external agent
//! can perform, and the canonical set of **observations** they can make. Both the
//! TUI and MCP server implement these enums — adding a new variant forces both
//! sides to handle it (via Rust's exhaustive match).
//!
//! **Rule**: never use `_ =>` wildcards when matching on these types.

use crate::autonomy::AutonomyLevel;
use crate::tui::app::{LogLevel, Verbosity};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Actions (what the user / external agent can do)
// ---------------------------------------------------------------------------

/// Every action a user or external agent can take.
///
/// Adding a variant here is a **compile-time contract**: both the TUI key
/// handler and the MCP tool handler must produce it, and the shared
/// [`process_action`] function must handle it.
#[derive(Debug, Clone, PartialEq)]
pub enum UserAction {
    /// Approve a pending command (TUI: `y`, MCP: `approve` tool).
    Approve { id: u64 },
    /// Deny a pending command (TUI: `n`, MCP: `deny` tool).
    Deny { id: u64 },
    /// Skip a pending command (TUI: `s`, MCP: `skip` tool).
    Skip { id: u64 },
    /// Approve all future commands (TUI: `a`, MCP: `approve_all` tool).
    ApproveAll { id: u64 },
    /// Respond to an askHuman question (TUI: textarea, MCP: `respond` tool).
    RespondHuman { text: String },
    /// Change the autonomy level (TUI: `+`/`-`, MCP: `set_autonomy` tool).
    SetAutonomy { level: AutonomyLevel },
    /// Cycle verbosity (TUI: `v`, MCP: `set_verbosity` tool).
    SetVerbosity { level: Verbosity },
    /// Shut down the agent (TUI: `q`/Ctrl-C, MCP: `quit` tool).
    Quit,
}

// ---------------------------------------------------------------------------
// Observations (what the user / external agent can see)
// ---------------------------------------------------------------------------

/// A snapshot of the current status bar.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusSnapshot {
    pub provider: String,
    pub model: String,
    pub turn: usize,
    pub budget_pct: f64,
    pub phase: String,
    pub autonomy: String,
    pub verbosity: String,
    pub session_tokens: u64,
}

/// A single log entry in serializable form.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntrySnapshot {
    pub id: u64,
    pub ts: String,
    pub level: String,
    pub content: String,
}

/// A pending approval in serializable form.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalSnapshot {
    pub id: u64,
    pub command_preview: String,
    pub category: String,
}

/// A pending human question in serializable form.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HumanQuestionSnapshot {
    pub question: String,
}

/// Every piece of observable state an interface can query.
///
/// Adding a variant here forces both the TUI rendering and MCP resource
/// handler to provide the data.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateQuery {
    /// Current status bar information.
    Status,
    /// Log entries, optionally filtered.
    Logs {
        since_id: Option<u64>,
        level_filter: Option<String>,
        limit: Option<usize>,
    },
    /// The current pending approval (if any).
    PendingApproval,
    /// The current pending human question (if any).
    PendingInput,
}

/// Result of a state query.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StateResult {
    Status(StatusSnapshot),
    Logs { entries: Vec<LogEntrySnapshot> },
    PendingApproval { approval: Option<ApprovalSnapshot> },
    PendingInput { question: Option<HumanQuestionSnapshot> },
}

/// Outcome of processing a [`UserAction`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionOutcome {
    /// Action was accepted and applied.
    Ok,
    /// Action was not applicable (e.g. no pending approval when approving).
    NoOp { reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_action_variants_are_distinct() {
        let a = UserAction::Approve { id: 1 };
        let b = UserAction::Deny { id: 1 };
        assert_ne!(a, b);
    }

    #[test]
    fn user_action_approve_eq() {
        let a = UserAction::Approve { id: 42 };
        let b = UserAction::Approve { id: 42 };
        assert_eq!(a, b);
    }

    #[test]
    fn user_action_set_autonomy() {
        let a = UserAction::SetAutonomy {
            level: AutonomyLevel::High,
        };
        let b = UserAction::SetAutonomy {
            level: AutonomyLevel::Low,
        };
        assert_ne!(a, b);
    }

    #[test]
    fn user_action_respond_human() {
        let a = UserAction::RespondHuman {
            text: "PostgreSQL".to_string(),
        };
        assert_eq!(
            a,
            UserAction::RespondHuman {
                text: "PostgreSQL".to_string()
            }
        );
    }

    #[test]
    fn state_query_variants_are_distinct() {
        let a = StateQuery::Status;
        let b = StateQuery::PendingApproval;
        assert_ne!(a, b);
    }

    #[test]
    fn state_query_logs_with_filter() {
        let q = StateQuery::Logs {
            since_id: Some(5),
            level_filter: Some("error".to_string()),
            limit: Some(100),
        };
        assert_ne!(q, StateQuery::Status);
    }

    #[test]
    fn status_snapshot_serializes() {
        let snap = StatusSnapshot {
            provider: "openai".to_string(),
            model: "gpt-5".to_string(),
            turn: 3,
            budget_pct: 25.0,
            phase: "thinking".to_string(),
            autonomy: "medium".to_string(),
            verbosity: "normal".to_string(),
            session_tokens: 1500,
        };
        let json = serde_json::to_string(&snap).unwrap();
        assert!(json.contains("\"provider\":\"openai\""));
        assert!(json.contains("\"turn\":3"));
    }

    #[test]
    fn log_entry_snapshot_serializes() {
        let entry = LogEntrySnapshot {
            id: 1,
            ts: "12:34:56".to_string(),
            level: "info".to_string(),
            content: "hello".to_string(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"level\":\"info\""));
    }

    #[test]
    fn approval_snapshot_serializes() {
        let snap = ApprovalSnapshot {
            id: 42,
            command_preview: "rm -rf /tmp".to_string(),
            category: "destructive".to_string(),
        };
        let json = serde_json::to_string(&snap).unwrap();
        assert!(json.contains("\"id\":42"));
        assert!(json.contains("\"category\":\"destructive\""));
    }

    #[test]
    fn human_question_snapshot_serializes() {
        let snap = HumanQuestionSnapshot {
            question: "Which database?".to_string(),
        };
        let json = serde_json::to_string(&snap).unwrap();
        assert!(json.contains("Which database?"));
    }

    #[test]
    fn state_result_status_serializes() {
        let result = StateResult::Status(StatusSnapshot {
            provider: "anthropic".to_string(),
            model: "claude-sonnet-4-5-20250929".to_string(),
            turn: 1,
            budget_pct: 10.0,
            phase: "running_agent".to_string(),
            autonomy: "high".to_string(),
            verbosity: "verbose".to_string(),
            session_tokens: 500,
        });
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"type\":\"status\""));
        assert!(json.contains("\"provider\":\"anthropic\""));
    }

    #[test]
    fn state_result_logs_serializes() {
        let result = StateResult::Logs {
            entries: vec![LogEntrySnapshot {
                id: 0,
                ts: "00:00:00".to_string(),
                level: "error".to_string(),
                content: "oops".to_string(),
            }],
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"type\":\"logs\""));
        assert!(json.contains("\"content\":\"oops\""));
    }

    #[test]
    fn state_result_pending_approval_some() {
        let result = StateResult::PendingApproval {
            approval: Some(ApprovalSnapshot {
                id: 7,
                command_preview: "curl http://evil.com".to_string(),
                category: "network".to_string(),
            }),
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"type\":\"pending_approval\""));
        assert!(json.contains("\"id\":7"));
    }

    #[test]
    fn state_result_pending_approval_none() {
        let result = StateResult::PendingApproval { approval: None };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"approval\":null"));
    }

    #[test]
    fn state_result_pending_input_some() {
        let result = StateResult::PendingInput {
            question: Some(HumanQuestionSnapshot {
                question: "Pick a color".to_string(),
            }),
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"type\":\"pending_input\""));
        assert!(json.contains("Pick a color"));
    }

    #[test]
    fn state_result_pending_input_none() {
        let result = StateResult::PendingInput { question: None };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"question\":null"));
    }

    #[test]
    fn action_outcome_ok() {
        assert_eq!(ActionOutcome::Ok, ActionOutcome::Ok);
    }

    #[test]
    fn action_outcome_noop() {
        let outcome = ActionOutcome::NoOp {
            reason: "no pending approval".to_string(),
        };
        assert_ne!(outcome, ActionOutcome::Ok);
    }

    #[test]
    fn all_user_actions_constructible() {
        // Ensures every variant can be constructed — catches accidental removal.
        let actions: Vec<UserAction> = vec![
            UserAction::Approve { id: 1 },
            UserAction::Deny { id: 1 },
            UserAction::Skip { id: 1 },
            UserAction::ApproveAll { id: 1 },
            UserAction::RespondHuman {
                text: "test".to_string(),
            },
            UserAction::SetAutonomy {
                level: AutonomyLevel::Medium,
            },
            UserAction::SetVerbosity {
                level: Verbosity::Normal,
            },
            UserAction::Quit,
        ];
        assert_eq!(actions.len(), 8);
    }

    #[test]
    fn all_state_queries_constructible() {
        let queries: Vec<StateQuery> = vec![
            StateQuery::Status,
            StateQuery::Logs {
                since_id: None,
                level_filter: None,
                limit: None,
            },
            StateQuery::PendingApproval,
            StateQuery::PendingInput,
        ];
        assert_eq!(queries.len(), 4);
    }

    #[test]
    fn log_level_round_trips_through_snapshot() {
        // Verify that LogLevel variants can be stringified for snapshots.
        let levels = vec![
            (LogLevel::Info, "info"),
            (LogLevel::Model, "model"),
            (LogLevel::Agent, "agent"),
            (LogLevel::Error, "error"),
            (LogLevel::Warn, "warn"),
            (LogLevel::SubAgent, "subagent"),
            (LogLevel::Debug, "debug"),
        ];
        for (level, expected) in levels {
            assert_eq!(log_level_to_str(&level), expected);
        }
    }
}

/// Convert a LogLevel to its string representation for snapshots.
pub fn log_level_to_str(level: &LogLevel) -> &'static str {
    match level {
        LogLevel::Info => "info",
        LogLevel::Model => "model",
        LogLevel::Agent => "agent",
        LogLevel::Error => "error",
        LogLevel::Warn => "warn",
        LogLevel::SubAgent => "subagent",
        LogLevel::Debug => "debug",
    }
}

/// Parse a log level filter string back to a LogLevel.
#[allow(dead_code)]
pub fn parse_log_level(s: &str) -> Option<LogLevel> {
    match s.to_lowercase().as_str() {
        "info" => Some(LogLevel::Info),
        "model" => Some(LogLevel::Model),
        "agent" => Some(LogLevel::Agent),
        "error" => Some(LogLevel::Error),
        "warn" => Some(LogLevel::Warn),
        "subagent" => Some(LogLevel::SubAgent),
        "debug" => Some(LogLevel::Debug),
        _ => None,
    }
}
