use crate::types::{AgentStateSnapshot, TaskEnvelope};
use serde_json::Value;

/// Actions produced by tool call dispatch.
/// The platform layer (native or WASM) interprets and executes these.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum PresenceAction {
    /// Tool produced a text result directly (no I/O needed).
    TextResult(String),
    /// Submit a task to the agent loop.
    SubmitTask(TaskEnvelope),
    /// Approve a pending action.
    Approve { id: u64 },
    /// Deny a pending action.
    Deny { id: u64 },
    /// Skip a pending action.
    Skip { id: u64 },
    /// Respond to an askHuman question.
    Respond { text: String },
    /// Change the autonomy level.
    SetAutonomy { level: String },
    /// Needs platform I/O — delegate to the platform's PresenceIO impl.
    NeedsIO {
        tool_name: String,
        args: Value,
    },
}

/// Human-readable confirmation text for an action that was dispatched.
pub fn action_confirmation(action: &PresenceAction) -> String {
    match action {
        PresenceAction::SubmitTask(envelope) => format!("Task submitted: {}", envelope.task),
        PresenceAction::Approve { id } => format!("Approved action {}", id),
        PresenceAction::Deny { id } => format!("Denied action {}", id),
        PresenceAction::Skip { id } => format!("Skipped action {}", id),
        PresenceAction::Respond { text } => format!("Sent response: {}", text),
        PresenceAction::SetAutonomy { level } => format!("Autonomy set to {}", level),
        PresenceAction::TextResult(_) | PresenceAction::NeedsIO { .. } => {
            "Action dispatched".to_string()
        }
    }
}

/// Dispatch a presence tool call. Pure-logic tools return immediately;
/// I/O-dependent tools return `NeedsIO` for the platform to handle.
pub fn dispatch_tool_call(
    name: &str,
    args: &Value,
    state: &AgentStateSnapshot,
) -> PresenceAction {
    match name {
        "submit_task" => handle_submit_task(args),
        "check_status" => PresenceAction::TextResult(handle_check_status(state)),
        "query_detail" => PresenceAction::NeedsIO {
            tool_name: "query_detail".to_string(),
            args: args.clone(),
        },
        "recall_memory" => PresenceAction::NeedsIO {
            tool_name: "recall_memory".to_string(),
            args: args.clone(),
        },
        "approve_action" => handle_approve(args),
        "deny_action" => handle_deny(args),
        "skip_action" => handle_skip(args),
        "respond_to_question" => handle_respond(args),
        "set_autonomy" => handle_set_autonomy(args),
        _ => PresenceAction::TextResult(format!("Unknown tool: {}", name)),
    }
}

fn handle_submit_task(args: &Value) -> PresenceAction {
    let task = args["task"].as_str().unwrap_or("").to_string();
    if task.is_empty() {
        return PresenceAction::TextResult("Error: task is required".to_string());
    }
    let force_direct = args["force_direct"].as_bool().unwrap_or(false);
    let context_hints = args["context_hints"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    PresenceAction::SubmitTask(TaskEnvelope {
        task,
        force_direct,
        context_hints,
    })
}

fn handle_check_status(state: &AgentStateSnapshot) -> String {
    let mut parts = Vec::new();
    parts.push(format!("Phase: {}", state.phase));
    parts.push(format!("Turn: {}", state.turn));
    parts.push(format!("Budget: {:.0}%", state.budget_pct * 100.0));
    if !state.last_command_preview.is_empty() {
        parts.push(format!("Last command: {}", state.last_command_preview));
    }
    if !state.last_output_summary.is_empty() {
        parts.push(format!("Last output: {}", state.last_output_summary));
    }
    if !state.active_workers.is_empty() {
        parts.push(format!("Workers: {}", state.active_workers.join(", ")));
    }
    if let Some(ref pa) = state.pending_approval {
        parts.push(format!(
            "PENDING APPROVAL: {} (id: {}, category: {})",
            pa.command_preview, pa.id, pa.category
        ));
    }
    if state.last_task_result.is_some() {
        parts.push("Task result: available (use query_detail scope 'task_result' for details)".to_string());
    }
    parts.join("\n")
}

fn handle_approve(args: &Value) -> PresenceAction {
    let id = args["id"].as_u64().unwrap_or(0);
    PresenceAction::Approve { id }
}

fn handle_deny(args: &Value) -> PresenceAction {
    let id = args["id"].as_u64().unwrap_or(0);
    PresenceAction::Deny { id }
}

fn handle_skip(args: &Value) -> PresenceAction {
    let id = args["id"].as_u64().unwrap_or(0);
    PresenceAction::Skip { id }
}

fn handle_respond(args: &Value) -> PresenceAction {
    let text = args["text"].as_str().unwrap_or("").to_string();
    if text.is_empty() {
        return PresenceAction::TextResult("Error: text is required".to_string());
    }
    PresenceAction::Respond { text }
}

fn handle_set_autonomy(args: &Value) -> PresenceAction {
    let level = args["level"].as_str().unwrap_or("medium").to_string();
    PresenceAction::SetAutonomy { level }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn dispatch_check_status() {
        let state = AgentStateSnapshot {
            phase: "thinking".to_string(),
            turn: 3,
            budget_pct: 0.15,
            ..Default::default()
        };
        let action = dispatch_tool_call("check_status", &json!({}), &state);
        match action {
            PresenceAction::TextResult(text) => {
                assert!(text.contains("Phase: thinking"));
                assert!(text.contains("Turn: 3"));
                assert!(text.contains("Budget: 15%"));
            }
            _ => panic!("expected TextResult"),
        }
    }

    #[test]
    fn dispatch_check_status_with_pending_approval() {
        use crate::types::PendingApprovalSnapshot;
        let state = AgentStateSnapshot {
            phase: "waiting_approval".to_string(),
            turn: 1,
            budget_pct: 0.0,
            pending_approval: Some(PendingApprovalSnapshot {
                id: 1,
                command_preview: "exec: ls -la /tmp".to_string(),
                category: "CommandExec".to_string(),
            }),
            ..Default::default()
        };
        let action = dispatch_tool_call("check_status", &json!({}), &state);
        match action {
            PresenceAction::TextResult(text) => {
                assert!(text.contains("PENDING APPROVAL"));
                assert!(text.contains("exec: ls -la /tmp"));
                assert!(text.contains("id: 1"));
                assert!(text.contains("CommandExec"));
            }
            _ => panic!("expected TextResult"),
        }
    }

    #[test]
    fn dispatch_submit_task() {
        let state = AgentStateSnapshot::default();
        let action = dispatch_tool_call(
            "submit_task",
            &json!({"task": "fix the bug", "force_direct": true}),
            &state,
        );
        match action {
            PresenceAction::SubmitTask(envelope) => {
                assert_eq!(envelope.task, "fix the bug");
                assert!(envelope.force_direct);
            }
            _ => panic!("expected SubmitTask"),
        }
    }

    #[test]
    fn dispatch_submit_task_empty() {
        let state = AgentStateSnapshot::default();
        let action = dispatch_tool_call("submit_task", &json!({"task": ""}), &state);
        match action {
            PresenceAction::TextResult(text) => {
                assert!(text.contains("Error"));
            }
            _ => panic!("expected TextResult error"),
        }
    }

    #[test]
    fn dispatch_approve() {
        let state = AgentStateSnapshot::default();
        let action = dispatch_tool_call("approve_action", &json!({"id": 42}), &state);
        match action {
            PresenceAction::Approve { id } => assert_eq!(id, 42),
            _ => panic!("expected Approve"),
        }
    }

    #[test]
    fn dispatch_deny() {
        let state = AgentStateSnapshot::default();
        let action = dispatch_tool_call("deny_action", &json!({"id": 7}), &state);
        match action {
            PresenceAction::Deny { id } => assert_eq!(id, 7),
            _ => panic!("expected Deny"),
        }
    }

    #[test]
    fn dispatch_respond() {
        let state = AgentStateSnapshot::default();
        let action = dispatch_tool_call(
            "respond_to_question",
            &json!({"text": "yes, do it"}),
            &state,
        );
        match action {
            PresenceAction::Respond { text } => assert_eq!(text, "yes, do it"),
            _ => panic!("expected Respond"),
        }
    }

    #[test]
    fn dispatch_set_autonomy() {
        let state = AgentStateSnapshot::default();
        let action = dispatch_tool_call("set_autonomy", &json!({"level": "full"}), &state);
        match action {
            PresenceAction::SetAutonomy { level } => assert_eq!(level, "full"),
            _ => panic!("expected SetAutonomy"),
        }
    }

    #[test]
    fn dispatch_query_detail_needs_io() {
        let state = AgentStateSnapshot::default();
        let action = dispatch_tool_call(
            "query_detail",
            &json!({"scope": "diff"}),
            &state,
        );
        match action {
            PresenceAction::NeedsIO { tool_name, .. } => {
                assert_eq!(tool_name, "query_detail");
            }
            _ => panic!("expected NeedsIO"),
        }
    }

    #[test]
    fn dispatch_recall_memory_needs_io() {
        let state = AgentStateSnapshot::default();
        let action = dispatch_tool_call(
            "recall_memory",
            &json!({"keywords": ["test"]}),
            &state,
        );
        match action {
            PresenceAction::NeedsIO { tool_name, .. } => {
                assert_eq!(tool_name, "recall_memory");
            }
            _ => panic!("expected NeedsIO"),
        }
    }

    #[test]
    fn dispatch_unknown_tool() {
        let state = AgentStateSnapshot::default();
        let action = dispatch_tool_call("nonexistent", &json!({}), &state);
        match action {
            PresenceAction::TextResult(text) => {
                assert!(text.contains("Unknown tool"));
            }
            _ => panic!("expected TextResult"),
        }
    }

    #[test]
    fn action_confirmation_all_variants() {
        assert_eq!(
            action_confirmation(&PresenceAction::SubmitTask(TaskEnvelope {
                task: "fix bug".to_string(),
                force_direct: false,
                context_hints: vec![],
            })),
            "Task submitted: fix bug"
        );
        assert_eq!(action_confirmation(&PresenceAction::Approve { id: 42 }), "Approved action 42");
        assert_eq!(action_confirmation(&PresenceAction::Deny { id: 7 }), "Denied action 7");
        assert_eq!(action_confirmation(&PresenceAction::Skip { id: 3 }), "Skipped action 3");
        assert_eq!(
            action_confirmation(&PresenceAction::Respond { text: "yes".to_string() }),
            "Sent response: yes"
        );
        assert_eq!(
            action_confirmation(&PresenceAction::SetAutonomy { level: "full".to_string() }),
            "Autonomy set to full"
        );
        assert_eq!(
            action_confirmation(&PresenceAction::TextResult("ok".to_string())),
            "Action dispatched"
        );
        assert_eq!(
            action_confirmation(&PresenceAction::NeedsIO {
                tool_name: "q".to_string(),
                args: json!({}),
            }),
            "Action dispatched"
        );
    }

    #[test]
    fn presence_action_serde_roundtrip() {
        let actions = vec![
            PresenceAction::Approve { id: 1 },
            PresenceAction::TextResult("hello".to_string()),
            PresenceAction::NeedsIO { tool_name: "q".to_string(), args: json!({"x": 1}) },
        ];
        for action in actions {
            let json = serde_json::to_string(&action).unwrap();
            let back: PresenceAction = serde_json::from_str(&json).unwrap();
            assert_eq!(serde_json::to_string(&back).unwrap(), json);
        }
    }
}
