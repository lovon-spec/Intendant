//! MCP (Model Context Protocol) server for Intendant.
//!
//! This module implements an MCP server that exposes Intendant's full state and
//! controls via the standard protocol. It is architecturally a **peer** of the
//! TUI — both consume the same [`EventBus`] events and translate user/agent
//! actions through the shared [`UserAction`](crate::frontend::UserAction) enum.
//!
//! ## Parity Contract
//!
//! The [`IntendantServer`] uses the same [`UserAction`] enum and [`StateQuery`]
//! types as the TUI. Adding a new `UserAction` variant forces both this module
//! and the TUI key handler to handle it (Rust exhaustive match, no wildcards).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        Implementation, ListResourcesResult, PaginatedRequestParams, RawResource,
        ReadResourceRequestParams, ReadResourceResult, Resource, ResourceContents,
        ResourceUpdatedNotificationParam, ServerCapabilities, ServerInfo, SubscribeRequestParams,
        UnsubscribeRequestParams,
    },
    schemars,
    service::{RequestContext, RoleServer},
    tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler, ServiceExt,
};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

use crate::autonomy::{AutonomyLevel, SharedAutonomy};
use crate::control::{self, OutboundEvent};
use crate::frontend::{
    self, ActionOutcome, ApprovalSnapshot, HumanQuestionSnapshot, LogEntrySnapshot, StateResult,
    StatusSnapshot, UserAction,
};
use crate::tui::app::{LogLevel, Phase, Verbosity};
use crate::tui::event::{AppEvent, ApprovalResponse, ControlMsg, EventBus};

// ---------------------------------------------------------------------------
// Task launcher: allows MCP to start agent loops on demand
// ---------------------------------------------------------------------------

/// A boxed async closure that spawns an agent loop for the given task.
///
/// The closure receives the task string and an `EventBus` for communicating
/// events back to the MCP server. It returns a `JoinHandle` for the spawned
/// background task.
pub type TaskLauncher = Box<
    dyn Fn(String, EventBus) -> Pin<Box<dyn Future<Output = tokio::task::JoinHandle<()>> + Send>>
        + Send
        + Sync,
>;

// ---------------------------------------------------------------------------
// Shared state that both the event listener and MCP handlers access
// ---------------------------------------------------------------------------

/// Observable state mirroring what the TUI's App struct tracks.
/// Updated by the event listener task, read by MCP tool/resource handlers.
pub struct McpAppState {
    pub provider_name: String,
    pub model_name: String,
    pub turn: usize,
    pub budget_pct: f64,
    pub phase: Phase,
    pub phase_entered_at: std::time::Instant,
    pub autonomy: SharedAutonomy,
    pub verbosity: Verbosity,
    pub session_tokens: u64,
    pub log_entries: std::collections::VecDeque<LogEntrySnapshot>,
    next_log_id: u64,
    pub pending_approval: Option<PendingApprovalState>,
    pub human_question: Option<String>,
    pub should_quit: bool,
    /// Session log directory for askHuman files.
    pub log_dir: std::path::PathBuf,
    /// Optional launcher for starting tasks via MCP. Set by main.rs.
    pub launcher: Option<Arc<TaskLauncher>>,
    /// Handle to the currently running agent loop, if any.
    pub task_handle: Option<tokio::task::JoinHandle<()>>,
    /// Pending or completed controller restart plan.
    pub controller_restart: Option<ControllerRestartState>,
}

/// Tracks a pending approval along with the oneshot sender.
pub struct PendingApprovalState {
    pub id: u64,
    pub command_preview: String,
    pub category: String,
    pub responder: Option<tokio::sync::oneshot::Sender<ApprovalResponse>>,
}

impl McpAppState {
    pub fn new(
        provider_name: String,
        model_name: String,
        autonomy: SharedAutonomy,
        log_dir: std::path::PathBuf,
    ) -> Self {
        Self {
            provider_name,
            model_name,
            turn: 0,
            budget_pct: 0.0,
            phase: Phase::Idle,
            phase_entered_at: std::time::Instant::now(),
            autonomy,
            verbosity: Verbosity::Normal,
            session_tokens: 0,
            log_entries: std::collections::VecDeque::new(),
            next_log_id: 0,
            pending_approval: None,
            human_question: None,
            should_quit: false,
            log_dir,
            launcher: None,
            task_handle: None,
            controller_restart: None,
        }
    }

    fn set_phase(&mut self, phase: Phase) {
        if self.phase != phase {
            self.phase = phase;
            self.phase_entered_at = std::time::Instant::now();
        }
    }

    fn push_log(&mut self, level: LogLevel, content: String) {
        let id = self.next_log_id;
        self.next_log_id += 1;
        let ts = chrono::Local::now().format("%H:%M:%S").to_string();
        if self.log_entries.len() >= 10_000 {
            self.log_entries.pop_front();
        }
        self.log_entries.push_back(LogEntrySnapshot {
            id,
            ts,
            level: frontend::log_level_to_str(&level).to_string(),
            content,
        });
    }

    fn status_snapshot(&self) -> StatusSnapshot {
        StatusSnapshot {
            provider: self.provider_name.clone(),
            model: self.model_name.clone(),
            turn: self.turn,
            budget_pct: self.budget_pct,
            phase: phase_to_str(&self.phase).to_string(),
            autonomy: "unknown".to_string(), // filled by caller with async read
            verbosity: verbosity_to_str(self.verbosity).to_string(),
            session_tokens: self.session_tokens,
        }
    }

    fn approval_snapshot(&self) -> Option<ApprovalSnapshot> {
        self.pending_approval.as_ref().map(|p| ApprovalSnapshot {
            id: p.id,
            command_preview: p.command_preview.clone(),
            category: p.category.clone(),
        })
    }

    fn human_question_snapshot(&self) -> Option<HumanQuestionSnapshot> {
        self.human_question.as_ref().map(|q| HumanQuestionSnapshot {
            question: q.clone(),
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RestartAfter {
    TurnEnd,
    Now,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RestartPhase {
    AwaitingTurnComplete,
    Ready,
    Restarting,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControllerRestartState {
    pub restart_id: String,
    pub controller_id: String,
    pub north_star_goal: String,
    pub reason: Option<String>,
    pub restart_after: RestartAfter,
    pub phase: RestartPhase,
    pub turn_complete_token: String,
    pub handoff_summary: Option<String>,
    pub completion_status: Option<String>,
    pub restart_command: Option<String>,
    pub auto_start_task: bool,
    pub max_attempts: u32,
    pub cooldown_sec: u64,
    pub attempts: u32,
    pub created_at: String,
    pub updated_at: String,
    pub last_attempt_at: Option<String>,
    pub last_error: Option<String>,
    pub last_result: Option<String>,
}

impl ControllerRestartState {
    fn now_string() -> String {
        chrono::Utc::now().to_rfc3339()
    }

    fn new(params: &ScheduleControllerRestartParams) -> Self {
        let now = Self::now_string();
        let restart_after = parse_restart_after(params.restart_after.as_deref())
            .expect("restart_after must be validated before creating ControllerRestartState");
        Self {
            restart_id: Uuid::new_v4().to_string(),
            controller_id: params.controller_id.clone(),
            north_star_goal: params.north_star_goal.clone(),
            reason: params.reason.clone(),
            restart_after,
            phase: match restart_after {
                RestartAfter::TurnEnd => RestartPhase::AwaitingTurnComplete,
                RestartAfter::Now => RestartPhase::Ready,
            },
            turn_complete_token: Uuid::new_v4().to_string(),
            handoff_summary: None,
            completion_status: None,
            restart_command: params.restart_command.clone(),
            auto_start_task: params.auto_start_task.unwrap_or(false),
            max_attempts: params.max_attempts.unwrap_or(1),
            cooldown_sec: params.cooldown_sec.unwrap_or(30),
            attempts: 0,
            created_at: now.clone(),
            updated_at: now,
            last_attempt_at: None,
            last_error: None,
            last_result: None,
        }
    }
}

fn parse_restart_after(raw: Option<&str>) -> Result<RestartAfter, String> {
    match raw.map(str::trim).map(str::to_lowercase).as_deref() {
        None | Some("") | Some("turn_end") => Ok(RestartAfter::TurnEnd),
        Some("now") => Ok(RestartAfter::Now),
        Some(other) => Err(format!(
            "Invalid request: restart_after must be 'turn_end' or 'now' (got '{}')",
            other
        )),
    }
}

fn normalize_string_field(value: &mut String) {
    *value = value.trim().to_string();
}

fn normalize_optional_string_field(value: &mut Option<String>) {
    if let Some(trimmed) = value.as_ref().map(|v| v.trim().to_string()) {
        if trimmed.is_empty() {
            *value = None;
        } else {
            *value = Some(trimmed);
        }
    }
}

fn normalize_schedule_controller_restart_params(params: &mut ScheduleControllerRestartParams) {
    normalize_string_field(&mut params.controller_id);
    normalize_string_field(&mut params.north_star_goal);
    normalize_optional_string_field(&mut params.reason);
    normalize_optional_string_field(&mut params.restart_after);
    if let Some(cmd) = params.restart_command.as_mut() {
        normalize_string_field(cmd);
    }
}

fn normalize_controller_turn_complete_params(params: &mut ControllerTurnCompleteParams) {
    normalize_string_field(&mut params.restart_id);
    normalize_string_field(&mut params.turn_complete_token);
    normalize_optional_string_field(&mut params.status);
    normalize_optional_string_field(&mut params.handoff_summary);
}

fn normalize_cancel_controller_restart_params(params: &mut CancelControllerRestartParams) {
    normalize_optional_string_field(&mut params.restart_id);
}

fn validate_schedule_controller_restart_params(
    params: &ScheduleControllerRestartParams,
) -> Result<(), String> {
    if params.controller_id.trim().is_empty() {
        return Err("Invalid request: controller_id must not be empty".to_string());
    }
    if params.north_star_goal.trim().is_empty() {
        return Err("Invalid request: north_star_goal must not be empty".to_string());
    }
    parse_restart_after(params.restart_after.as_deref())?;
    if matches!(params.max_attempts, Some(0)) {
        return Err("Invalid request: max_attempts must be >= 1".to_string());
    }
    if let Some(cmd) = params.restart_command.as_ref() {
        if cmd.trim().is_empty() {
            return Err("Invalid request: restart_command must not be empty".to_string());
        }
    }
    let has_restart_command = params
        .restart_command
        .as_ref()
        .map(|cmd| !cmd.trim().is_empty())
        .unwrap_or(false);
    let auto_start_task = params.auto_start_task.unwrap_or(false);
    if !has_restart_command && !auto_start_task {
        return Err(
            "Invalid request: configure at least one restart action (restart_command and/or auto_start_task=true)"
                .to_string(),
        );
    }
    Ok(())
}

fn restart_state_path(log_dir: &std::path::Path) -> std::path::PathBuf {
    log_dir.join("controller_restart.json")
}

fn persist_restart_state(log_dir: &std::path::Path, state: &Option<ControllerRestartState>) {
    let path = restart_state_path(log_dir);
    if let Some(s) = state {
        if let Ok(json) = serde_json::to_string_pretty(s) {
            let _ = std::fs::write(path, json);
        }
    } else {
        let _ = std::fs::remove_file(path);
    }
}

fn emit_control_result(
    control_tx: &Option<broadcast::Sender<String>>,
    action: &str,
    ok: bool,
    message: String,
    data: Option<serde_json::Value>,
) {
    if let Some(tx) = control_tx {
        let event = OutboundEvent::CommandResult {
            action: action.to_string(),
            ok,
            message,
            data,
        };
        control::broadcast_event(tx, &event);
    }
}

fn current_autonomy_label(level: AutonomyLevel) -> String {
    level.to_string().to_lowercase()
}

async fn emit_control_status(
    state: &SharedMcpState,
    control_tx: &Option<broadcast::Sender<String>>,
) {
    if let Some(tx) = control_tx {
        let s = state.read().await;
        let autonomy_level = s.autonomy.read().await.level;
        let event = OutboundEvent::Status {
            turn: s.turn,
            phase: phase_to_str(&s.phase).to_string(),
            autonomy: current_autonomy_label(autonomy_level),
        };
        control::broadcast_event(tx, &event);
    }
}

async fn start_task_with_state(
    state: &SharedMcpState,
    bus: &EventBus,
    task: String,
    source: &str,
) -> Result<(), String> {
    let mut s = state.write().await;

    match s.phase {
        Phase::Thinking
        | Phase::RunningAgent
        | Phase::Orchestrating
        | Phase::WaitingApproval
        | Phase::WaitingHuman => {
            return Err(format!(
                "agent is currently in '{}' phase",
                phase_to_str(&s.phase)
            ));
        }
        Phase::Idle | Phase::Done => {}
    }

    let launcher = s
        .launcher
        .as_ref()
        .cloned()
        .ok_or_else(|| "no task launcher configured".to_string())?;

    s.turn = 0;
    s.budget_pct = 0.0;
    s.session_tokens = 0;
    s.set_phase(Phase::Thinking);
    s.pending_approval = None;
    s.human_question = None;
    s.should_quit = false;
    s.push_log(
        LogLevel::Info,
        format!("Task started via {}: {}", source, task),
    );

    let bus = bus.clone();
    drop(s);

    let handle = (launcher)(task, bus).await;
    let mut s = state.write().await;
    s.task_handle = Some(handle);
    Ok(())
}

fn restart_state_public_value(state: Option<&ControllerRestartState>) -> serde_json::Value {
    let mut value = serde_json::to_value(state).unwrap_or(serde_json::Value::Null);
    if let Some(obj) = value.as_object_mut() {
        if obj.contains_key("turn_complete_token") {
            obj.insert(
                "turn_complete_token".to_string(),
                serde_json::Value::String("[redacted]".to_string()),
            );
        }
    }
    value
}

async fn spawn_detached_restart_command(cmd: &str) -> Result<u32, String> {
    use std::process::Stdio;
    use tokio::process::Command;

    // Use setsid when available to separate process group/session so parent
    // shutdown doesn't tear down the restarted controller process.
    let wrapper = r#"
if command -v setsid >/dev/null 2>&1; then
  nohup setsid bash -lc "$INTENDANT_RESTART_COMMAND" </dev/null >/dev/null 2>&1 &
else
  nohup bash -lc "$INTENDANT_RESTART_COMMAND" </dev/null >/dev/null 2>&1 &
fi
echo $!
"#;

    let output = Command::new("bash")
        .args(["-lc", wrapper])
        .env("INTENDANT_RESTART_COMMAND", cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
        .map_err(|e| format!("Failed to launch detached restart command: {}", e))?;

    if !output.status.success() {
        return Err(format!(
            "Failed to launch detached restart command (exit={})",
            output.status
        ));
    }

    let pid_text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    pid_text
        .parse::<u32>()
        .map_err(|e| format!("Failed to parse detached restart pid '{}': {}", pid_text, e))
}

async fn run_scheduled_controller_restart_with_state(
    state: &SharedMcpState,
    bus: &EventBus,
) -> Result<String, String> {
    let (restart, log_dir) = {
        let mut s = state.write().await;
        let log_dir = s.log_dir.clone();
        let Some(active) = s.controller_restart.as_mut() else {
            return Err("No scheduled controller restart".to_string());
        };

        if !matches!(active.phase, RestartPhase::Ready) {
            return Err(format!(
                "Restart is not ready (current phase: {:?})",
                active.phase
            ));
        }

        if active.attempts >= active.max_attempts {
            active.phase = RestartPhase::Failed;
            active.last_error = Some("Max restart attempts reached".to_string());
            active.updated_at = ControllerRestartState::now_string();
            let snapshot = s.controller_restart.clone();
            persist_restart_state(&log_dir, &snapshot);
            return Err("Max restart attempts reached".to_string());
        }

        if let Some(last_attempt) = &active.last_attempt_at {
            if let Ok(last) = chrono::DateTime::parse_from_rfc3339(last_attempt) {
                let elapsed = chrono::Utc::now() - last.with_timezone(&chrono::Utc);
                if elapsed.num_seconds() < active.cooldown_sec as i64 {
                    return Err(format!(
                        "Restart cooldown active ({}s remaining)",
                        active
                            .cooldown_sec
                            .saturating_sub(elapsed.num_seconds() as u64)
                    ));
                }
            }
        }

        active.phase = RestartPhase::Restarting;
        active.attempts += 1;
        active.last_attempt_at = Some(ControllerRestartState::now_string());
        active.updated_at = ControllerRestartState::now_string();
        active.last_error = None;
        active.last_result = None;
        let restart = active.clone();
        let snapshot = s.controller_restart.clone();
        persist_restart_state(&log_dir, &snapshot);
        (restart, log_dir)
    };

    let mut result_parts = Vec::new();

    if let Some(cmd) = &restart.restart_command {
        match spawn_detached_restart_command(cmd).await {
            Ok(pid) => {
                result_parts.push(format!("spawned controller command (pid {})", pid));
            }
            Err(e) => {
                let mut s = state.write().await;
                if let Some(active) = s.controller_restart.as_mut() {
                    active.phase = RestartPhase::Failed;
                    active.last_error = Some(format!("Failed to spawn restart_command: {}", e));
                    active.updated_at = ControllerRestartState::now_string();
                }
                let snapshot = s.controller_restart.clone();
                persist_restart_state(&log_dir, &snapshot);
                return Err(format!("Failed to spawn restart_command: {}", e));
            }
        }
    }

    if restart.auto_start_task {
        if let Err(e) = start_task_with_state(
            state,
            bus,
            restart.north_star_goal.clone(),
            "controller_restart",
        )
        .await
        {
            let failure = format!("Failed to start follow-up task: {}", e);
            let mut s = state.write().await;
            if let Some(active) = s.controller_restart.as_mut() {
                active.phase = RestartPhase::Failed;
                active.last_error = Some(failure.clone());
                active.updated_at = ControllerRestartState::now_string();
            }
            let snapshot = s.controller_restart.clone();
            persist_restart_state(&log_dir, &snapshot);
            return Err(failure);
        }
        result_parts.push("started autonomous follow-up task".to_string());
    }

    if restart.restart_command.is_none() && !restart.auto_start_task {
        let mut s = state.write().await;
        if let Some(active) = s.controller_restart.as_mut() {
            active.phase = RestartPhase::Failed;
            active.last_error = Some(
                "No restart action configured: set restart_command and/or auto_start_task=true"
                    .to_string(),
            );
            active.updated_at = ControllerRestartState::now_string();
        }
        let snapshot = s.controller_restart.clone();
        persist_restart_state(&log_dir, &snapshot);
        return Err("No restart action configured".to_string());
    }

    let mut s = state.write().await;
    if let Some(active) = s.controller_restart.as_mut() {
        active.phase = RestartPhase::Completed;
        active.last_result = Some(if result_parts.is_empty() {
            "ok".to_string()
        } else {
            result_parts.join("; ")
        });
        active.updated_at = ControllerRestartState::now_string();
    }
    let snapshot = s.controller_restart.clone();
    persist_restart_state(&log_dir, &snapshot);

    Ok(result_parts.join("; "))
}

fn restart_phase_value(state: &ControllerRestartState) -> serde_json::Value {
    serde_json::to_value(state.phase).unwrap_or(serde_json::Value::Null)
}

fn restart_error_response(
    status: &str,
    restart_id: &str,
    phase: Option<RestartPhase>,
    error: String,
) -> String {
    let mut output = serde_json::json!({
        "status": status,
        "restart_id": restart_id,
        "ok": false,
        "error": error,
    });
    if let Some(phase) = phase {
        output["phase"] = serde_json::to_value(phase).unwrap_or(serde_json::Value::Null);
    }
    serde_json::to_string_pretty(&output).unwrap_or_else(|_| "{}".to_string())
}

fn schedule_error_response(
    error: String,
    restart_id: Option<&str>,
    phase: Option<RestartPhase>,
) -> String {
    let mut output = serde_json::json!({
        "status": "rejected",
        "ok": false,
        "error": error,
    });
    if let Some(restart_id) = restart_id {
        output["restart_id"] = serde_json::Value::String(restart_id.to_string());
    }
    if let Some(phase) = phase {
        output["phase"] = serde_json::to_value(phase).unwrap_or(serde_json::Value::Null);
    }
    serde_json::to_string_pretty(&output).unwrap_or_else(|_| "{}".to_string())
}

async fn handle_control_command_mcp(
    state: &SharedMcpState,
    bus: &EventBus,
    control_tx: &Option<broadcast::Sender<String>>,
    msg: ControlMsg,
) -> Option<&'static str> {
    match msg {
        ControlMsg::Status => {
            emit_control_status(state, control_tx).await;
            None
        }
        ControlMsg::Approve { id } => {
            let mut s = state.write().await;
            let outcome = process_action_sync(&mut s, UserAction::Approve { id });
            emit_control_result(
                control_tx,
                "approve",
                matches!(outcome, ActionOutcome::Ok),
                format_outcome(outcome),
                None,
            );
            Some(RESOURCE_APPROVAL_URI)
        }
        ControlMsg::Deny { id } => {
            let mut s = state.write().await;
            let outcome = process_action_sync(&mut s, UserAction::Deny { id });
            emit_control_result(
                control_tx,
                "deny",
                matches!(outcome, ActionOutcome::Ok),
                format_outcome(outcome),
                None,
            );
            Some(RESOURCE_APPROVAL_URI)
        }
        ControlMsg::Input { text } => {
            let mut s = state.write().await;
            let outcome = process_action_sync(&mut s, UserAction::RespondHuman { text });
            emit_control_result(
                control_tx,
                "input",
                matches!(outcome, ActionOutcome::Ok),
                format_outcome(outcome),
                None,
            );
            Some(RESOURCE_INPUT_URI)
        }
        ControlMsg::SetAutonomy { level } => {
            let parsed = AutonomyLevel::from_str_loose(&level);
            {
                let s = state.read().await;
                let autonomy = s.autonomy.clone();
                drop(s);
                let mut a = autonomy.write().await;
                a.level = parsed;
            }
            emit_control_result(
                control_tx,
                "set_autonomy",
                true,
                format!("Autonomy set to {}", parsed),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::ScheduleControllerRestart {
            controller_id,
            north_star_goal,
            reason,
            restart_after,
            restart_command,
            auto_start_task,
            max_attempts,
            cooldown_sec,
        } => {
            let mut params = ScheduleControllerRestartParams {
                controller_id,
                north_star_goal,
                reason,
                restart_after,
                restart_command,
                auto_start_task,
                max_attempts,
                cooldown_sec,
            };
            normalize_schedule_controller_restart_params(&mut params);

            if let Err(e) = validate_schedule_controller_restart_params(&params) {
                emit_control_result(control_tx, "schedule_controller_restart", false, e, None);
                return Some(RESOURCE_RESTART_URI);
            }

            let restart = {
                let mut s = state.write().await;
                if let Some(active) = s.controller_restart.as_ref() {
                    if matches!(
                        active.phase,
                        RestartPhase::AwaitingTurnComplete
                            | RestartPhase::Ready
                            | RestartPhase::Restarting
                    ) {
                        emit_control_result(
                            control_tx,
                            "schedule_controller_restart",
                            false,
                            format!(
                                "A restart is already active (id={}, phase={:?})",
                                active.restart_id, active.phase
                            ),
                            None,
                        );
                        return Some(RESOURCE_RESTART_URI);
                    }
                }

                let restart = ControllerRestartState::new(&params);
                s.push_log(
                    LogLevel::Info,
                    format!(
                        "Controller restart scheduled for '{}' (id={})",
                        restart.controller_id, restart.restart_id
                    ),
                );
                s.controller_restart = Some(restart.clone());
                persist_restart_state(&s.log_dir, &s.controller_restart);
                restart
            };

            let mut payload = serde_json::json!({
                "status": "scheduled",
                "restart_id": restart.restart_id,
                "turn_complete_token": restart.turn_complete_token,
            });
            let mut command_ok = true;
            let mut command_message = "ok".to_string();

            if matches!(restart.restart_after, RestartAfter::Now) {
                match run_scheduled_controller_restart_with_state(state, bus).await {
                    Ok(result) => {
                        payload["execution"] = serde_json::Value::String(if result.is_empty() {
                            "ok".to_string()
                        } else {
                            result
                        });
                    }
                    Err(e) => {
                        command_ok = false;
                        command_message = "restart execution failed".to_string();
                        payload["execution_error"] = serde_json::Value::String(e);
                    }
                }
            }
            let phase = {
                let s = state.read().await;
                s.controller_restart
                    .as_ref()
                    .map(restart_phase_value)
                    .unwrap_or_else(|| {
                        serde_json::to_value(restart.phase).unwrap_or(serde_json::Value::Null)
                    })
            };
            payload["phase"] = phase;

            emit_control_result(
                control_tx,
                "schedule_controller_restart",
                command_ok,
                command_message,
                Some(payload),
            );
            Some(RESOURCE_RESTART_URI)
        }
        ControlMsg::ControllerTurnComplete {
            restart_id,
            turn_complete_token,
            status,
            handoff_summary,
        } => {
            let mut params = ControllerTurnCompleteParams {
                restart_id,
                turn_complete_token,
                status,
                handoff_summary,
            };
            normalize_controller_turn_complete_params(&mut params);
            {
                let mut s = state.write().await;
                let log_dir = s.log_dir.clone();
                let Some(active) = s.controller_restart.as_mut() else {
                    emit_control_result(
                        control_tx,
                        "controller_turn_complete",
                        false,
                        "No controller restart is scheduled".to_string(),
                        None,
                    );
                    return Some(RESOURCE_RESTART_URI);
                };
                if active.restart_id != params.restart_id {
                    emit_control_result(
                        control_tx,
                        "controller_turn_complete",
                        false,
                        "restart_id does not match the active restart".to_string(),
                        None,
                    );
                    return Some(RESOURCE_RESTART_URI);
                }
                if active.turn_complete_token != params.turn_complete_token {
                    emit_control_result(
                        control_tx,
                        "controller_turn_complete",
                        false,
                        "turn_complete_token is invalid".to_string(),
                        None,
                    );
                    return Some(RESOURCE_RESTART_URI);
                }
                if !matches!(
                    active.phase,
                    RestartPhase::AwaitingTurnComplete | RestartPhase::Ready
                ) {
                    emit_control_result(
                        control_tx,
                        "controller_turn_complete",
                        false,
                        format!(
                            "Restart is not awaiting completion (phase={:?})",
                            active.phase
                        ),
                        None,
                    );
                    return Some(RESOURCE_RESTART_URI);
                }

                active.handoff_summary = params.handoff_summary.clone();
                active.completion_status = params.status.clone();
                active.phase = RestartPhase::Ready;
                active.updated_at = ControllerRestartState::now_string();
                let snapshot = s.controller_restart.clone();
                persist_restart_state(&log_dir, &snapshot);
            }

            match run_scheduled_controller_restart_with_state(state, bus).await {
                Ok(result) => emit_control_result(
                    control_tx,
                    "controller_turn_complete",
                    true,
                    if result.is_empty() {
                        "ok".to_string()
                    } else {
                        result
                    },
                    None,
                ),
                Err(e) => {
                    emit_control_result(control_tx, "controller_turn_complete", false, e, None)
                }
            }
            Some(RESOURCE_RESTART_URI)
        }
        ControlMsg::GetRestartStatus => {
            let s = state.read().await;
            let data = serde_json::to_value(&s.controller_restart).ok();
            emit_control_result(
                control_tx,
                "get_restart_status",
                true,
                "ok".to_string(),
                data,
            );
            Some(RESOURCE_RESTART_URI)
        }
        ControlMsg::CancelControllerRestart { restart_id } => {
            let mut params = CancelControllerRestartParams { restart_id };
            normalize_cancel_controller_restart_params(&mut params);
            let mut s = state.write().await;
            let log_dir = s.log_dir.clone();
            let Some(active) = s.controller_restart.as_mut() else {
                emit_control_result(
                    control_tx,
                    "cancel_controller_restart",
                    false,
                    "No controller restart is scheduled".to_string(),
                    None,
                );
                return Some(RESOURCE_RESTART_URI);
            };

            if let Some(expected_id) = params.restart_id.as_deref() {
                if expected_id != active.restart_id {
                    emit_control_result(
                        control_tx,
                        "cancel_controller_restart",
                        false,
                        format!(
                            "restart_id '{}' does not match active '{}'",
                            expected_id, active.restart_id
                        ),
                        None,
                    );
                    return Some(RESOURCE_RESTART_URI);
                }
            }

            active.phase = RestartPhase::Cancelled;
            active.updated_at = ControllerRestartState::now_string();
            active.last_result = Some("Cancelled by operator".to_string());
            let snapshot = s.controller_restart.clone();
            persist_restart_state(&log_dir, &snapshot);
            emit_control_result(
                control_tx,
                "cancel_controller_restart",
                true,
                "ok".to_string(),
                None,
            );
            Some(RESOURCE_RESTART_URI)
        }
        ControlMsg::Quit => {
            let action = UserAction::Quit;
            let mut s = state.write().await;
            let outcome = process_action_sync(&mut s, action);
            emit_control_result(
                control_tx,
                "quit",
                matches!(outcome, ActionOutcome::Ok),
                format_outcome(outcome),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
    }
}

pub type SharedMcpState = Arc<RwLock<McpAppState>>;

fn phase_to_str(phase: &Phase) -> &'static str {
    match phase {
        Phase::Thinking => "thinking",
        Phase::RunningAgent => "running_agent",
        Phase::Orchestrating => "orchestrating",
        Phase::WaitingApproval => "waiting_approval",
        Phase::WaitingHuman => "waiting_human",
        Phase::Idle => "idle",
        Phase::Done => "done",
    }
}

fn verbosity_to_str(v: Verbosity) -> &'static str {
    match v {
        Verbosity::Quiet => "quiet",
        Verbosity::Normal => "normal",
        Verbosity::Verbose => "verbose",
        Verbosity::Debug => "debug",
    }
}

fn parse_verbosity(s: &str) -> Option<Verbosity> {
    match s.to_lowercase().as_str() {
        "quiet" => Some(Verbosity::Quiet),
        "normal" => Some(Verbosity::Normal),
        "verbose" => Some(Verbosity::Verbose),
        "debug" => Some(Verbosity::Debug),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Event listener: consumes AppEvents and updates shared state
// ---------------------------------------------------------------------------

/// Spawn a background task that consumes AppEvents and mirrors them into
/// [`McpAppState`], exactly as the TUI's `handle_event` does.
///
/// Returns a handle for cleanup.
pub fn spawn_event_listener(
    state: SharedMcpState,
    mut event_rx: tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
    peer: Arc<Mutex<Option<rmcp::Peer<RoleServer>>>>,
    bus: EventBus,
    human_question_path: Option<crate::tui::event::SharedQuestionPath>,
    control_tx: Option<broadcast::Sender<String>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
            let mut resource_changed: Option<&str> = None;
            let mut deferred_control_msg: Option<ControlMsg> = None;

            {
                let mut s = state.write().await;
                // Exhaustive match — no wildcard. Adding a new AppEvent variant
                // will cause a compile error here, enforcing parity.
                match event {
                    AppEvent::Key(_) => {} // MCP doesn't handle key events
                    AppEvent::Resize(_, _) => {}
                    AppEvent::Tick => {
                        // Detect stuck phases — warn every 30s after 120s
                        if matches!(
                            s.phase,
                            Phase::Thinking | Phase::RunningAgent | Phase::Orchestrating
                        ) {
                            let elapsed = s.phase_entered_at.elapsed().as_secs();
                            if elapsed >= 120 && elapsed % 30 == 0 {
                                let phase_name = phase_to_str(&s.phase).to_string();
                                s.push_log(
                                    LogLevel::Warn,
                                    format!(
                                        "Phase '{}' active for {}s (possible stuck state)",
                                        phase_name, elapsed
                                    ),
                                );
                                resource_changed = Some("intendant://logs");
                            }
                        }
                    }
                    AppEvent::Quit => {
                        s.should_quit = true;
                        break;
                    }

                    AppEvent::TurnStarted {
                        turn,
                        budget_pct,
                        remaining: _,
                    } => {
                        s.turn = turn;
                        s.budget_pct = budget_pct;
                        s.set_phase(Phase::Thinking);
                        s.push_log(
                            LogLevel::Info,
                            format!("Turn {} started (budget: {:.1}%)", turn, budget_pct),
                        );
                        resource_changed = Some("intendant://status");
                    }

                    AppEvent::ModelResponse {
                        turn,
                        content,
                        usage,
                        reasoning,
                    } => {
                        s.session_tokens += usage.total_tokens;
                        let preview = if content.len() > 500 {
                            format!("{}...", &content[..500])
                        } else {
                            content
                        };
                        s.push_log(LogLevel::Model, format!("[T{}] {}", turn, preview));
                        if let Some(r) = reasoning {
                            s.push_log(
                                LogLevel::Debug,
                                format!("[T{}] reasoning: {}...", turn, &r[..r.len().min(100)]),
                            );
                        }
                        resource_changed = Some("intendant://logs");
                    }

                    AppEvent::ModelResponseDelta { .. } => {
                        // Streaming deltas: MCP doesn't need to handle incremental text
                    }

                    AppEvent::JsonExtracted { preview } => {
                        s.push_log(LogLevel::Debug, format!("JSON: {}", preview));
                    }

                    AppEvent::DoneSignal { message } => {
                        s.set_phase(Phase::Done);
                        s.push_log(
                            LogLevel::Info,
                            format!("Done: {}", message.as_deref().unwrap_or("task complete")),
                        );
                        resource_changed = Some("intendant://status");
                    }

                    AppEvent::AgentStarted {
                        turn,
                        commands_preview,
                    } => {
                        s.set_phase(Phase::RunningAgent);
                        s.push_log(LogLevel::Agent, format!("[T{}] {}", turn, commands_preview));
                        resource_changed = Some("intendant://status");
                    }

                    AppEvent::AgentOutput { stdout, stderr } => {
                        if !stdout.is_empty() {
                            for line in stdout.lines() {
                                s.push_log(LogLevel::Agent, line.to_string());
                            }
                        }
                        if !stderr.is_empty() {
                            for line in stderr.lines() {
                                s.push_log(LogLevel::Warn, line.to_string());
                            }
                        }
                        resource_changed = Some("intendant://logs");
                    }

                    AppEvent::SubAgentResult { formatted } => {
                        s.push_log(LogLevel::SubAgent, formatted);
                        resource_changed = Some("intendant://logs");
                    }

                    AppEvent::OrchestratorProgress {
                        turn,
                        status,
                        last_action,
                    } => {
                        s.set_phase(Phase::Orchestrating);
                        s.push_log(
                            LogLevel::SubAgent,
                            format!("[T{}] {} — {}", turn, status, last_action),
                        );
                        resource_changed = Some("intendant://status");
                    }

                    AppEvent::ContextManagement { turn } => {
                        s.push_log(LogLevel::Debug, format!("[T{}] Context management", turn));
                    }

                    AppEvent::TaskComplete { reason } => {
                        s.set_phase(Phase::Done);
                        s.push_log(LogLevel::Info, format!("Task complete: {}", reason));
                        resource_changed = Some("intendant://status");
                    }

                    AppEvent::BudgetWarning { pct, remaining } => {
                        s.budget_pct = pct;
                        s.push_log(
                            LogLevel::Warn,
                            format!(
                                "Budget warning: {:.1}% used ({} tokens remaining)",
                                pct, remaining
                            ),
                        );
                        resource_changed = Some("intendant://status");
                    }

                    AppEvent::BudgetExhausted { remaining } => {
                        s.budget_pct = 100.0;
                        s.push_log(
                            LogLevel::Error,
                            format!("Budget exhausted ({} tokens remaining)", remaining),
                        );
                        resource_changed = Some("intendant://status");
                    }

                    AppEvent::SafetyCapReached => {
                        s.set_phase(Phase::Done);
                        s.push_log(
                            LogLevel::Error,
                            "Safety cap reached (500 turns)".to_string(),
                        );
                        resource_changed = Some("intendant://status");
                    }

                    AppEvent::LoopError(msg) => {
                        s.set_phase(Phase::Done);
                        s.push_log(LogLevel::Error, format!("Error: {}", msg));
                        resource_changed = Some("intendant://status");
                    }

                    AppEvent::HumanQuestionDetected { question } => {
                        s.set_phase(Phase::WaitingHuman);
                        s.human_question = Some(question.clone());
                        s.push_log(LogLevel::Info, format!("Human question: {}", question));
                        resource_changed = Some("intendant://pending-input");
                    }

                    AppEvent::HumanResponseSent => {
                        s.human_question = None;
                        s.set_phase(Phase::RunningAgent);
                        s.push_log(LogLevel::Info, "Human response sent".to_string());
                        resource_changed = Some("intendant://pending-input");
                    }

                    AppEvent::ApprovalRequired {
                        id,
                        command_preview,
                        category,
                        responder,
                    } => {
                        s.set_phase(Phase::WaitingApproval);
                        s.push_log(
                            LogLevel::Info,
                            format!("Approval required [{}]: {}", category, command_preview),
                        );
                        s.pending_approval = Some(PendingApprovalState {
                            id,
                            command_preview,
                            category: category.to_string(),
                            responder: Some(responder),
                        });
                        resource_changed = Some("intendant://pending-approval");
                    }

                    AppEvent::SessionDirChanged { ref path } => {
                        s.log_dir = path.clone();
                        persist_restart_state(&s.log_dir, &s.controller_restart);
                        // Update the human question monitor's watched path
                        if let Some(ref hqp) = human_question_path {
                            if let Ok(mut p) = hqp.try_write() {
                                *p = path.join("human_question");
                            }
                        }
                    }

                    AppEvent::ControlCommand(msg) => deferred_control_msg = Some(msg),
                }
            }

            if let Some(msg) = deferred_control_msg {
                if let Some(uri) = handle_control_command_mcp(&state, &bus, &control_tx, msg).await
                {
                    resource_changed = Some(uri);
                }
            }

            // Send resource update notification if something changed
            if let Some(uri) = resource_changed {
                let peer_guard = peer.lock().await;
                if let Some(ref p) = *peer_guard {
                    let _ = p
                        .notify_resource_updated(ResourceUpdatedNotificationParam {
                            uri: uri.to_string(),
                        })
                        .await;
                }
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Tool parameter types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ApproveParams {
    /// The approval ID (turn number) to approve.
    pub id: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DenyParams {
    /// The approval ID (turn number) to deny.
    pub id: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SkipParams {
    /// The approval ID (turn number) to skip.
    pub id: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ApproveAllParams {
    /// The approval ID (turn number) to approve (also sets autonomy to Full).
    pub id: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RespondParams {
    /// The text response to the askHuman question.
    pub text: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SetAutonomyParams {
    /// The autonomy level: "low", "medium", "high", or "full".
    pub level: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SetVerbosityParams {
    /// The verbosity level: "quiet", "normal", "verbose", or "debug".
    pub level: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct StartTaskParams {
    /// The task description for the AI agent to execute.
    pub task: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ScheduleControllerRestartParams {
    /// Identifier for the controlling agent/client (e.g. "codex", "claude_code").
    pub controller_id: String,
    /// Goal for the next controller session / autonomous cycle.
    pub north_star_goal: String,
    /// Optional operator-provided reason.
    #[serde(default)]
    pub reason: Option<String>,
    /// When to execute restart: "turn_end" (default) or "now".
    #[serde(default)]
    pub restart_after: Option<String>,
    /// Optional command to spawn for controller restart.
    #[serde(default)]
    pub restart_command: Option<String>,
    /// Auto-start the next intendant task with north_star_goal (default: false).
    #[serde(default)]
    pub auto_start_task: Option<bool>,
    /// Maximum restart attempts before failing (default: 1).
    #[serde(default)]
    pub max_attempts: Option<u32>,
    /// Cooldown between restart attempts in seconds (default: 30).
    #[serde(default)]
    pub cooldown_sec: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ControllerTurnCompleteParams {
    /// Restart ID returned by schedule_controller_restart.
    pub restart_id: String,
    /// Completion token returned by schedule_controller_restart.
    pub turn_complete_token: String,
    /// Optional completion status from the controller.
    #[serde(default)]
    pub status: Option<String>,
    /// Optional final handoff summary from the controller.
    #[serde(default)]
    pub handoff_summary: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CancelControllerRestartParams {
    /// Optional restart ID guard. If provided and mismatched, cancellation is rejected.
    #[serde(default)]
    pub restart_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetLogsParams {
    /// Only return log entries with IDs greater than this value (cursor-based pagination).
    #[serde(default)]
    pub since_id: Option<u64>,
    /// Filter by log level: "info", "model", "agent", "error", "warn", "subagent", "debug".
    #[serde(default)]
    pub level_filter: Option<String>,
    /// Maximum number of entries to return (default: 100).
    #[serde(default)]
    pub limit: Option<usize>,
}

// ---------------------------------------------------------------------------
// MCP Server
// ---------------------------------------------------------------------------

/// The Intendant MCP server. Exposes tools (actions) and resources (observations)
/// that mirror the TUI exactly.
#[derive(Clone)]
pub struct IntendantServer {
    state: SharedMcpState,
    bus: EventBus,
    tool_router: ToolRouter<Self>,
}

impl IntendantServer {
    pub fn new(state: SharedMcpState, bus: EventBus) -> Self {
        Self {
            state,
            bus,
            tool_router: Self::tool_router(),
        }
    }

    async fn start_task_internal(&self, task: String, source: &str) -> Result<(), String> {
        start_task_with_state(&self.state, &self.bus, task, source).await
    }

    async fn run_scheduled_controller_restart(&self) -> Result<String, String> {
        run_scheduled_controller_restart_with_state(&self.state, &self.bus).await
    }
}

/// Process a [`UserAction`] against the shared state. This is the **single**
/// handler that both TUI and MCP feed into.
///
/// Note: for actions that need async access (like writing autonomy), the caller
/// must handle the async parts. This function handles the state-mutation and
/// oneshot-sending synchronously.
fn process_action_sync(state: &mut McpAppState, action: UserAction) -> ActionOutcome {
    // Exhaustive match — no wildcard. Compile-time parity enforcement.
    match action {
        UserAction::Approve { id: _ } => {
            if let Some(mut pending) = state.pending_approval.take() {
                if let Some(responder) = pending.responder.take() {
                    let _ = responder.send(ApprovalResponse::Approve);
                }
                state.set_phase(Phase::RunningAgent);
                state.push_log(LogLevel::Info, "Approved by MCP agent".to_string());
                ActionOutcome::Ok
            } else {
                ActionOutcome::NoOp {
                    reason: "No pending approval".to_string(),
                }
            }
        }
        UserAction::Deny { id: _ } => {
            if let Some(mut pending) = state.pending_approval.take() {
                if let Some(responder) = pending.responder.take() {
                    let _ = responder.send(ApprovalResponse::Deny);
                }
                state.set_phase(Phase::Done);
                state.push_log(LogLevel::Info, "Denied by MCP agent".to_string());
                ActionOutcome::Ok
            } else {
                ActionOutcome::NoOp {
                    reason: "No pending approval".to_string(),
                }
            }
        }
        UserAction::Skip { id: _ } => {
            if let Some(mut pending) = state.pending_approval.take() {
                if let Some(responder) = pending.responder.take() {
                    let _ = responder.send(ApprovalResponse::Skip);
                }
                state.set_phase(Phase::RunningAgent);
                state.push_log(LogLevel::Info, "Skipped by MCP agent".to_string());
                ActionOutcome::Ok
            } else {
                ActionOutcome::NoOp {
                    reason: "No pending approval".to_string(),
                }
            }
        }
        UserAction::ApproveAll { id: _ } => {
            if let Some(mut pending) = state.pending_approval.take() {
                if let Some(responder) = pending.responder.take() {
                    let _ = responder.send(ApprovalResponse::ApproveAll);
                }
                state.set_phase(Phase::RunningAgent);
                state.push_log(
                    LogLevel::Info,
                    "Approved all (autonomy → Full) by MCP agent".to_string(),
                );
                ActionOutcome::Ok
            } else {
                ActionOutcome::NoOp {
                    reason: "No pending approval".to_string(),
                }
            }
        }
        UserAction::RespondHuman { text } => {
            if state.human_question.is_some() {
                // Write response to session-scoped file (same mechanism as TUI)
                let response_path = state.log_dir.join("human_response");
                if std::fs::write(&response_path, &text).is_ok() {
                    state.human_question = None;
                    state.set_phase(Phase::RunningAgent);
                    state.push_log(LogLevel::Info, format!("Human response (MCP): {}", text));
                    ActionOutcome::Ok
                } else {
                    ActionOutcome::NoOp {
                        reason: "Failed to write response file".to_string(),
                    }
                }
            } else {
                ActionOutcome::NoOp {
                    reason: "No pending human question".to_string(),
                }
            }
        }
        UserAction::SetAutonomy { level: _ } => {
            // Autonomy is set asynchronously by the caller after this returns.
            ActionOutcome::Ok
        }
        UserAction::SetVerbosity { level } => {
            state.verbosity = level;
            state.push_log(
                LogLevel::Info,
                format!("Verbosity set to {} by MCP agent", verbosity_to_str(level)),
            );
            ActionOutcome::Ok
        }
        UserAction::Quit => {
            state.should_quit = true;
            state.push_log(LogLevel::Info, "Quit requested by MCP agent".to_string());
            ActionOutcome::Ok
        }
    }
}

// ---------------------------------------------------------------------------
// Tool implementations
// ---------------------------------------------------------------------------

#[tool_router]
impl IntendantServer {
    #[tool(
        description = "Get current status: provider, model, turn, budget, phase, autonomy, verbosity, tokens."
    )]
    async fn get_status(&self) -> String {
        let s = self.state.read().await;
        let mut snap = s.status_snapshot();
        // Fill autonomy from shared state
        drop(s);
        let state = self.state.read().await;
        let autonomy_level = state.autonomy.read().await.level;
        snap.autonomy = autonomy_level.to_string().to_lowercase();
        serde_json::to_string_pretty(&snap).unwrap_or_else(|_| "{}".to_string())
    }

    #[tool(
        description = "Get log entries. Supports cursor-based pagination via since_id and filtering by level."
    )]
    async fn get_logs(&self, Parameters(params): Parameters<GetLogsParams>) -> String {
        let s = self.state.read().await;
        let limit = params.limit.unwrap_or(100);
        let entries: Vec<&LogEntrySnapshot> = s
            .log_entries
            .iter()
            .filter(|e| {
                if let Some(since) = params.since_id {
                    if e.id <= since {
                        return false;
                    }
                }
                if let Some(ref filter) = params.level_filter {
                    if e.level != *filter {
                        return false;
                    }
                }
                true
            })
            .take(limit)
            .collect();
        serde_json::to_string_pretty(&entries).unwrap_or_else(|_| "[]".to_string())
    }

    #[tool(
        description = "Get the current pending approval request, if any. Returns null if no approval is pending."
    )]
    async fn get_pending_approval(&self) -> String {
        let s = self.state.read().await;
        match s.approval_snapshot() {
            Some(snap) => {
                serde_json::to_string_pretty(&snap).unwrap_or_else(|_| "null".to_string())
            }
            None => "null".to_string(),
        }
    }

    #[tool(
        description = "Get the current pending human question, if any. Returns null if no question is pending."
    )]
    async fn get_pending_input(&self) -> String {
        let s = self.state.read().await;
        match s.human_question_snapshot() {
            Some(snap) => {
                serde_json::to_string_pretty(&snap).unwrap_or_else(|_| "null".to_string())
            }
            None => "null".to_string(),
        }
    }

    #[tool(
        description = "Approve a pending command execution. Equivalent to pressing 'y' in the TUI."
    )]
    async fn approve(&self, Parameters(params): Parameters<ApproveParams>) -> String {
        let action = UserAction::Approve { id: params.id };
        let mut s = self.state.write().await;
        let outcome = process_action_sync(&mut s, action);
        format_outcome(outcome)
    }

    #[tool(
        description = "Deny a pending command execution. Stops the agent loop. Equivalent to pressing 'n' in the TUI."
    )]
    async fn deny(&self, Parameters(params): Parameters<DenyParams>) -> String {
        let action = UserAction::Deny { id: params.id };
        let mut s = self.state.write().await;
        let outcome = process_action_sync(&mut s, action);
        format_outcome(outcome)
    }

    #[tool(
        description = "Skip a pending command execution. The agent continues with the next command. Equivalent to pressing 's' in the TUI."
    )]
    async fn skip(&self, Parameters(params): Parameters<SkipParams>) -> String {
        let action = UserAction::Skip { id: params.id };
        let mut s = self.state.write().await;
        let outcome = process_action_sync(&mut s, action);
        format_outcome(outcome)
    }

    #[tool(
        description = "Approve this and all future commands (sets autonomy to Full). Equivalent to pressing 'a' in the TUI."
    )]
    async fn approve_all(&self, Parameters(params): Parameters<ApproveAllParams>) -> String {
        let action = UserAction::ApproveAll { id: params.id };
        let mut s = self.state.write().await;
        let outcome = process_action_sync(&mut s, action);
        if outcome == ActionOutcome::Ok {
            let autonomy = s.autonomy.clone();
            drop(s);
            let mut a = autonomy.write().await;
            a.level = AutonomyLevel::Full;
        }
        format_outcome(outcome)
    }

    #[tool(
        description = "Respond to an askHuman question. Equivalent to typing a response and pressing Enter in the TUI."
    )]
    async fn respond(&self, Parameters(params): Parameters<RespondParams>) -> String {
        let action = UserAction::RespondHuman { text: params.text };
        let mut s = self.state.write().await;
        let outcome = process_action_sync(&mut s, action);
        format_outcome(outcome)
    }

    #[tool(
        description = "Set the autonomy level. Controls how much approval is required. Equivalent to +/- keys in the TUI."
    )]
    async fn set_autonomy(&self, Parameters(params): Parameters<SetAutonomyParams>) -> String {
        let level = AutonomyLevel::from_str_loose(&params.level);
        let s = self.state.read().await;
        let autonomy = s.autonomy.clone();
        drop(s);
        {
            let mut a = autonomy.write().await;
            a.level = level;
        }
        let mut s = self.state.write().await;
        let _ = process_action_sync(&mut s, UserAction::SetAutonomy { level });
        s.push_log(
            LogLevel::Info,
            format!("Autonomy set to {} by MCP agent", level),
        );
        format!("Autonomy set to {}", level)
    }

    #[tool(
        description = "Set log verbosity level. Controls which log entries are shown. Equivalent to pressing 'v' in the TUI."
    )]
    async fn set_verbosity(&self, Parameters(params): Parameters<SetVerbosityParams>) -> String {
        match parse_verbosity(&params.level) {
            Some(level) => {
                let action = UserAction::SetVerbosity { level };
                let mut s = self.state.write().await;
                let outcome = process_action_sync(&mut s, action);
                format_outcome(outcome)
            }
            None => format!(
                "Invalid verbosity level '{}'. Use: quiet, normal, verbose, debug",
                params.level
            ),
        }
    }

    #[tool(
        description = "Shut down the Intendant agent. Equivalent to pressing 'q' or Ctrl-C in the TUI."
    )]
    async fn quit(&self) -> String {
        let action = UserAction::Quit;
        let mut s = self.state.write().await;
        let outcome = process_action_sync(&mut s, action);
        format_outcome(outcome)
    }

    #[tool(
        description = "Start a new task for the Intendant agent to execute. The agent will begin working on the task immediately. Only one task can run at a time — check get_status to see if a task is already running."
    )]
    async fn start_task(&self, Parameters(params): Parameters<StartTaskParams>) -> String {
        match self.start_task_internal(params.task, "MCP").await {
            Ok(()) => "ok".to_string(),
            Err(e) => format!("Cannot start task: {}", e),
        }
    }

    #[tool(
        description = "Schedule a controller restart workflow. Returns a restart ID and a completion token that must be passed to controller_turn_complete as the final controller action."
    )]
    async fn schedule_controller_restart(
        &self,
        Parameters(mut params): Parameters<ScheduleControllerRestartParams>,
    ) -> String {
        normalize_schedule_controller_restart_params(&mut params);
        if let Err(e) = validate_schedule_controller_restart_params(&params) {
            return schedule_error_response(e, None, None);
        }

        let restart = {
            let mut s = self.state.write().await;
            if let Some(active) = s.controller_restart.as_ref() {
                if matches!(
                    active.phase,
                    RestartPhase::AwaitingTurnComplete
                        | RestartPhase::Ready
                        | RestartPhase::Restarting
                ) {
                    return schedule_error_response(
                        format!(
                            "A restart is already active (id={}, phase={:?})",
                            active.restart_id, active.phase
                        ),
                        Some(active.restart_id.as_str()),
                        Some(active.phase),
                    );
                }
            }

            let restart = ControllerRestartState::new(&params);
            s.push_log(
                LogLevel::Info,
                format!(
                    "Controller restart scheduled for '{}' (id={})",
                    restart.controller_id, restart.restart_id
                ),
            );
            s.controller_restart = Some(restart.clone());
            persist_restart_state(&s.log_dir, &s.controller_restart);
            restart
        };

        let mut output = serde_json::json!({
            "status": "scheduled",
            "restart_id": restart.restart_id,
            "turn_complete_token": restart.turn_complete_token,
            "ok": true,
        });
        let mut command_ok = true;

        if matches!(restart.restart_after, RestartAfter::Now) {
            match self.run_scheduled_controller_restart().await {
                Ok(result) => {
                    output["execution"] = serde_json::Value::String(if result.is_empty() {
                        "ok".to_string()
                    } else {
                        result
                    });
                }
                Err(e) => {
                    command_ok = false;
                    output["execution_error"] = serde_json::Value::String(e);
                }
            }
        }
        output["ok"] = serde_json::Value::Bool(command_ok);
        let phase = {
            let s = self.state.read().await;
            s.controller_restart
                .as_ref()
                .map(restart_phase_value)
                .unwrap_or_else(|| {
                    serde_json::to_value(restart.phase).unwrap_or(serde_json::Value::Null)
                })
        };
        output["phase"] = phase;

        serde_json::to_string_pretty(&output).unwrap_or_else(|_| "{}".to_string())
    }

    #[tool(
        description = "Final handshake call from the controlling agent before ending its turn. Validates token and executes any pending scheduled restart."
    )]
    async fn controller_turn_complete(
        &self,
        Parameters(mut params): Parameters<ControllerTurnCompleteParams>,
    ) -> String {
        normalize_controller_turn_complete_params(&mut params);
        {
            let mut s = self.state.write().await;
            let log_dir = s.log_dir.clone();
            let Some(active) = s.controller_restart.as_mut() else {
                return restart_error_response(
                    "rejected",
                    &params.restart_id,
                    None,
                    "No controller restart is scheduled".to_string(),
                );
            };

            if active.restart_id != params.restart_id {
                return restart_error_response(
                    "rejected",
                    &params.restart_id,
                    Some(active.phase),
                    "restart_id does not match the active restart".to_string(),
                );
            }
            if active.turn_complete_token != params.turn_complete_token {
                return restart_error_response(
                    "rejected",
                    &params.restart_id,
                    Some(active.phase),
                    "turn_complete_token is invalid".to_string(),
                );
            }
            if !matches!(
                active.phase,
                RestartPhase::AwaitingTurnComplete | RestartPhase::Ready
            ) {
                return restart_error_response(
                    "rejected",
                    &params.restart_id,
                    Some(active.phase),
                    format!(
                        "Restart is not awaiting completion (phase={:?})",
                        active.phase
                    ),
                );
            }

            active.handoff_summary = params.handoff_summary.clone();
            active.completion_status = params.status.clone();
            active.phase = RestartPhase::Ready;
            active.updated_at = ControllerRestartState::now_string();
            let restart_id = active.restart_id.clone();
            let snapshot = s.controller_restart.clone();
            persist_restart_state(&log_dir, &snapshot);
            s.push_log(
                LogLevel::Info,
                format!("Controller turn complete acknowledged (id={})", restart_id),
            );
        }

        match self.run_scheduled_controller_restart().await {
            Ok(result) => {
                let mut output = serde_json::json!({
                    "status": "completed",
                    "restart_id": params.restart_id,
                    "ok": true,
                });
                output["execution"] = serde_json::Value::String(if result.is_empty() {
                    "ok".to_string()
                } else {
                    result
                });
                let phase = {
                    let s = self.state.read().await;
                    s.controller_restart
                        .as_ref()
                        .map(restart_phase_value)
                        .unwrap_or(serde_json::Value::Null)
                };
                output["phase"] = phase;
                serde_json::to_string_pretty(&output).unwrap_or_else(|_| "{}".to_string())
            }
            Err(e) => {
                let phase = {
                    let s = self.state.read().await;
                    s.controller_restart.as_ref().map(|r| r.phase)
                };
                restart_error_response("restart_pending", &params.restart_id, phase, e)
            }
        }
    }

    #[tool(
        description = "Get the current controller restart state, if any. Returns null when no restart is tracked."
    )]
    async fn get_restart_status(&self) -> String {
        let s = self.state.read().await;
        let value = restart_state_public_value(s.controller_restart.as_ref());
        serde_json::to_string_pretty(&value).unwrap_or_else(|_| "null".to_string())
    }

    #[tool(description = "Cancel a scheduled controller restart.")]
    async fn cancel_controller_restart(
        &self,
        Parameters(mut params): Parameters<CancelControllerRestartParams>,
    ) -> String {
        normalize_cancel_controller_restart_params(&mut params);
        let mut s = self.state.write().await;
        let log_dir = s.log_dir.clone();
        let requested_restart_id = params.restart_id.as_deref();
        let Some(active) = s.controller_restart.as_mut() else {
            return schedule_error_response(
                "No controller restart is scheduled".to_string(),
                requested_restart_id,
                None,
            );
        };

        if let Some(expected_id) = requested_restart_id {
            if expected_id != active.restart_id {
                return schedule_error_response(
                    format!(
                        "restart_id '{}' does not match active '{}'",
                        expected_id, active.restart_id
                    ),
                    Some(active.restart_id.as_str()),
                    Some(active.phase),
                );
            }
        }

        active.phase = RestartPhase::Cancelled;
        active.updated_at = ControllerRestartState::now_string();
        active.last_result = Some("Cancelled by operator".to_string());
        let restart_id = active.restart_id.clone();
        let snapshot = s.controller_restart.clone();
        persist_restart_state(&log_dir, &snapshot);
        s.push_log(
            LogLevel::Info,
            format!("Controller restart cancelled (id={})", restart_id),
        );
        serde_json::json!({
            "status": "cancelled",
            "ok": true,
            "restart_id": restart_id,
            "phase": RestartPhase::Cancelled,
        })
        .to_string()
    }

    #[tool(
        description = "Rebuild the intendant binary from source and hot-reload the MCP server. The server process is replaced in-place via exec() — the MCP connection survives seamlessly. Use this after making code changes so the running server picks them up without restarting Claude Code."
    )]
    async fn reload(&self) -> String {
        // Find the project root (where Cargo.toml lives)
        let exe = match std::env::current_exe() {
            Ok(e) => e,
            Err(e) => return format!("Failed to determine executable path: {}", e),
        };

        // Find project root by looking for Cargo.toml relative to the exe
        // or via the INTENDANT_PROJECT_ROOT env var
        let project_root = std::env::var("INTENDANT_PROJECT_ROOT")
            .map(std::path::PathBuf::from)
            .ok()
            .or_else(|| {
                // Walk up from exe to find Cargo.toml
                let mut dir = exe.parent()?;
                for _ in 0..10 {
                    if dir.join("Cargo.toml").exists() {
                        return Some(dir.to_path_buf());
                    }
                    dir = dir.parent()?;
                }
                None
            });

        // Build the binary
        if let Some(root) = &project_root {
            let output = std::process::Command::new("cargo")
                .args(["build", "--release"])
                .current_dir(root)
                .output();

            match output {
                Ok(o) if o.status.success() => {
                    let mut s = self.state.write().await;
                    s.push_log(
                        LogLevel::Info,
                        "Binary rebuilt successfully, exec'ing...".to_string(),
                    );
                }
                Ok(o) => {
                    let stderr = String::from_utf8_lossy(&o.stderr);
                    return format!("Build failed:\n{}", stderr);
                }
                Err(e) => {
                    return format!("Failed to run cargo build: {}", e);
                }
            }
        }

        // Schedule the exec() after a brief delay so the JSON-RPC response
        // can be flushed to stdout before the process is replaced.
        let args: Vec<String> = std::env::args().collect();
        tokio::spawn(async move {
            // Give rmcp time to write the response
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;

            // Flush stdout to ensure the response is delivered
            use std::io::Write;
            let _ = std::io::stdout().flush();

            // exec() replaces the process image. stdin/stdout fds survive.
            use std::os::unix::process::CommandExt;
            let exe = match std::env::current_exe() {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("reload failed: cannot determine current exe: {}", e);
                    return;
                }
            };
            let err = std::process::Command::new(exe)
                .args(&args[1..])
                .env("INTENDANT_MCP_RELOAD", "1")
                .exec();
            // exec only returns on error
            eprintln!("reload exec failed: {}", err);
            std::process::exit(1);
        });

        "ok - reloading in-place (MCP connection preserved)".to_string()
    }
}

fn format_outcome(outcome: ActionOutcome) -> String {
    match outcome {
        ActionOutcome::Ok => "ok".to_string(),
        ActionOutcome::NoOp { reason } => format!("no-op: {}", reason),
    }
}

// ---------------------------------------------------------------------------
// Resource definitions
// ---------------------------------------------------------------------------

const RESOURCE_STATUS_URI: &str = "intendant://status";
const RESOURCE_LOGS_URI: &str = "intendant://logs";
const RESOURCE_APPROVAL_URI: &str = "intendant://pending-approval";
const RESOURCE_INPUT_URI: &str = "intendant://pending-input";
const RESOURCE_RESTART_URI: &str = "intendant://controller-restart";

fn make_resource(uri: &str, name: &str, description: &str) -> Resource {
    Resource {
        raw: RawResource {
            uri: uri.to_string(),
            name: name.to_string(),
            title: None,
            description: Some(description.to_string()),
            mime_type: Some("application/json".to_string()),
            size: None,
            icons: None,
            meta: None,
        },
        annotations: None,
    }
}

fn resource_definitions() -> Vec<Resource> {
    vec![
        make_resource(
            RESOURCE_STATUS_URI,
            "status",
            "Current status: provider, model, turn, budget, phase, autonomy",
        ),
        make_resource(
            RESOURCE_LOGS_URI,
            "logs",
            "Chronological log entries (same as TUI log panel)",
        ),
        make_resource(
            RESOURCE_APPROVAL_URI,
            "pending-approval",
            "Current pending approval request, if any",
        ),
        make_resource(
            RESOURCE_INPUT_URI,
            "pending-input",
            "Current pending human question, if any",
        ),
        make_resource(
            RESOURCE_RESTART_URI,
            "controller-restart",
            "Controller restart schedule / execution state",
        ),
    ]
}

// ---------------------------------------------------------------------------
// ServerHandler implementation
// ---------------------------------------------------------------------------

#[tool_handler]
impl ServerHandler for IntendantServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Intendant AI agent runtime. This MCP server exposes the same controls \
                 and observations as the TUI. Use tools to control the agent (approve, \
                 deny, respond, set_autonomy, quit), manage controller restarts \
                 (schedule_controller_restart, controller_turn_complete, \
                 get_restart_status, cancel_controller_restart), and observe state \
                 (get_status, get_logs, get_pending_approval, get_pending_input). \
                 Resources provide push-based state updates via subscriptions."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .enable_resources_subscribe()
                .enable_resources_list_changed()
                .build(),
            server_info: Implementation {
                name: "intendant".to_string(),
                title: Some("Intendant AI Agent Runtime".to_string()),
                version: env!("CARGO_PKG_VERSION").to_string(),
                description: Some(
                    "MCP interface for controlling and observing the Intendant AI agent"
                        .to_string(),
                ),
                icons: None,
                website_url: None,
            },
            ..Default::default()
        }
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        Ok(ListResourcesResult {
            meta: None,
            resources: resource_definitions(),
            next_cursor: None,
        })
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        let s = self.state.read().await;
        let json = match request.uri.as_str() {
            RESOURCE_STATUS_URI => {
                let mut snap = s.status_snapshot();
                let autonomy_level = s.autonomy.read().await.level;
                snap.autonomy = autonomy_level.to_string().to_lowercase();
                serde_json::to_string_pretty(&StateResult::Status(snap))
                    .unwrap_or_else(|_| "{}".to_string())
            }
            RESOURCE_LOGS_URI => {
                // Return last 100 entries
                let entries: Vec<LogEntrySnapshot> = s
                    .log_entries
                    .iter()
                    .rev()
                    .take(100)
                    .rev()
                    .cloned()
                    .collect();
                serde_json::to_string_pretty(&StateResult::Logs { entries })
                    .unwrap_or_else(|_| "[]".to_string())
            }
            RESOURCE_APPROVAL_URI => {
                let snap = s.approval_snapshot();
                serde_json::to_string_pretty(&StateResult::PendingApproval { approval: snap })
                    .unwrap_or_else(|_| "null".to_string())
            }
            RESOURCE_INPUT_URI => {
                let snap = s.human_question_snapshot();
                serde_json::to_string_pretty(&StateResult::PendingInput { question: snap })
                    .unwrap_or_else(|_| "null".to_string())
            }
            RESOURCE_RESTART_URI => {
                let value = restart_state_public_value(s.controller_restart.as_ref());
                serde_json::to_string_pretty(&value).unwrap_or_else(|_| "null".to_string())
            }
            _ => {
                return Err(McpError::invalid_params(
                    format!("Unknown resource URI: {}", request.uri),
                    None,
                ));
            }
        };

        Ok(ReadResourceResult {
            contents: vec![ResourceContents::text(json, request.uri)],
        })
    }

    async fn subscribe(
        &self,
        _request: SubscribeRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        // We push notifications for all resources on every relevant event change
        // (handled in spawn_event_listener). Accept all subscriptions.
        Ok(())
    }

    async fn unsubscribe(
        &self,
        _request: UnsubscribeRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Reload transport: wraps stdio with fake initialization for post-exec reload
// ---------------------------------------------------------------------------

/// After an exec() reload, the MCP client (Claude Code) still considers the
/// connection initialized. But rmcp's `serve()` requires the full init handshake.
///
/// `ReloadTransport` solves this by injecting a synthetic `initialize` request
/// and `notifications/initialized` notification into the receive stream, and
/// swallowing the server's init response on the send side. After this two-message
/// preamble, all messages pass through to real stdio transparently.
mod reload_transport {
    use rmcp::model::{
        ClientJsonRpcMessage, ClientNotification, ClientRequest, Implementation,
        InitializeRequestParams, ProtocolVersion, ServerJsonRpcMessage,
    };
    use rmcp::service::RoleServer;
    use rmcp::transport::Transport;
    use std::borrow::Cow;
    use std::future::Future;
    use std::pin::Pin;

    /// Phases of the reload handshake injection.
    enum Phase {
        /// Next `receive()` returns a fake Initialize request.
        InjectInit,
        /// Next `send()` swallows the Initialize response, then transitions.
        SwallowInitResp,
        /// Next `receive()` returns a fake Initialized notification.
        InjectInitialized,
        /// All subsequent messages pass through to the inner transport.
        Normal,
    }

    pub struct ReloadTransport<T> {
        inner: T,
        phase: Phase,
    }

    impl<T> ReloadTransport<T> {
        pub fn new(inner: T) -> Self {
            Self {
                inner,
                phase: Phase::InjectInit,
            }
        }
    }

    fn fake_init_request() -> ClientJsonRpcMessage {
        let params = InitializeRequestParams {
            meta: None,
            protocol_version: ProtocolVersion::V_2025_03_26,
            capabilities: Default::default(),
            client_info: Implementation {
                name: "claude-code".to_string(),
                title: None,
                version: "1.0.0-reload".to_string(),
                description: None,
                icons: None,
                website_url: None,
            },
        };
        let request = ClientRequest::InitializeRequest(rmcp::model::InitializeRequest {
            method: Default::default(),
            params,
            extensions: Default::default(),
        });
        ClientJsonRpcMessage::request(request, rmcp::model::RequestId::Number(0))
    }

    fn fake_initialized_notification() -> ClientJsonRpcMessage {
        let notification =
            ClientNotification::InitializedNotification(rmcp::model::InitializedNotification {
                method: Default::default(),
                extensions: Default::default(),
            });
        ClientJsonRpcMessage::notification(notification)
    }

    impl<T: Transport<RoleServer>> Transport<RoleServer> for ReloadTransport<T> {
        type Error = T::Error;

        fn name() -> Cow<'static, str> {
            "ReloadTransport".into()
        }

        fn send(
            &mut self,
            item: ServerJsonRpcMessage,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
            match self.phase {
                Phase::SwallowInitResp => {
                    // Swallow the init response, advance to next phase
                    self.phase = Phase::InjectInitialized;
                    // Return a no-op future that succeeds
                    let fut: Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send>> =
                        Box::pin(async { Ok(()) });
                    fut
                }
                _ => {
                    // Normal passthrough
                    let inner_fut = self.inner.send(item);
                    let fut: Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send>> =
                        Box::pin(inner_fut);
                    fut
                }
            }
        }

        fn receive(&mut self) -> impl Future<Output = Option<ClientJsonRpcMessage>> + Send {
            match self.phase {
                Phase::InjectInit => {
                    self.phase = Phase::SwallowInitResp;
                    let msg = fake_init_request();
                    let fut: Pin<Box<dyn Future<Output = Option<ClientJsonRpcMessage>> + Send>> =
                        Box::pin(async move { Some(msg) });
                    fut
                }
                Phase::InjectInitialized => {
                    self.phase = Phase::Normal;
                    let msg = fake_initialized_notification();
                    let fut: Pin<Box<dyn Future<Output = Option<ClientJsonRpcMessage>> + Send>> =
                        Box::pin(async move { Some(msg) });
                    fut
                }
                _ => {
                    // Normal passthrough
                    let inner_fut = self.inner.receive();
                    let fut: Pin<Box<dyn Future<Output = Option<ClientJsonRpcMessage>> + Send>> =
                        Box::pin(inner_fut);
                    fut
                }
            }
        }

        fn close(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
            self.inner.close()
        }
    }
}

// ---------------------------------------------------------------------------
// Public API: start the MCP server on stdio
// ---------------------------------------------------------------------------

/// Exit code used to signal a reload request to a parent wrapper script.
#[allow(dead_code)]
pub const RELOAD_EXIT_CODE: i32 = 42;

/// Run the MCP server on stdio. This replaces the TUI — the external agent
/// communicates via MCP over stdin/stdout.
///
/// The server consumes AppEvents from the bus and exposes them as tools and
/// resources.
///
/// When `reloaded` is true, the server wraps stdio in a [`ReloadTransport`]
/// that injects a synthetic MCP initialization handshake, allowing seamless
/// operation after an exec() reload.
pub async fn run_mcp_server(
    state: SharedMcpState,
    bus: EventBus,
    event_rx: tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
    reloaded: bool,
    human_question_path: Option<crate::tui::event::SharedQuestionPath>,
    control_tx: Option<broadcast::Sender<String>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let server = IntendantServer::new(state.clone(), bus.clone());

    // Start serving on stdio, using ReloadTransport if this is a post-exec reload
    let running = if reloaded {
        use rmcp::transport::IntoTransport;
        let inner = rmcp::transport::io::stdio().into_transport();
        let transport = reload_transport::ReloadTransport::new(inner);
        server.serve(transport).await?
    } else {
        let transport = rmcp::transport::io::stdio();
        server.serve(transport).await?
    };

    // Store the peer for sending notifications
    let peer = Arc::new(Mutex::new(Some(running.peer().clone())));

    // Spawn event listener that mirrors AppEvents into McpAppState
    let _listener = spawn_event_listener(
        state,
        event_rx,
        peer,
        bus.clone(),
        human_question_path,
        control_tx,
    );

    // Wait until the service finishes (client disconnects or quit)
    running.waiting().await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autonomy::{self, AutonomyState};
    use tempfile::tempdir;
    use tokio::time::{timeout, Duration};

    fn test_state() -> SharedMcpState {
        test_state_with_log_dir(std::path::PathBuf::from("/tmp/test_session"))
    }

    fn test_state_with_log_dir(log_dir: std::path::PathBuf) -> SharedMcpState {
        let autonomy = autonomy::shared_autonomy(AutonomyState::default());
        Arc::new(RwLock::new(McpAppState::new(
            "openai".to_string(),
            "gpt-5".to_string(),
            autonomy,
            log_dir,
        )))
    }

    #[test]
    fn mcp_state_initial_values() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let s = state.read().await;
            assert_eq!(s.turn, 0);
            assert_eq!(s.budget_pct, 0.0);
            assert_eq!(s.phase, Phase::Idle);
            assert!(s.log_entries.is_empty());
            assert!(s.pending_approval.is_none());
            assert!(s.human_question.is_none());
            assert!(!s.should_quit);
        });
    }

    #[test]
    fn status_snapshot_has_correct_fields() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            s.turn = 5;
            s.budget_pct = 42.0;
            s.set_phase(Phase::Thinking);
            s.session_tokens = 1234;
            let snap = s.status_snapshot();
            assert_eq!(snap.provider, "openai");
            assert_eq!(snap.model, "gpt-5");
            assert_eq!(snap.turn, 5);
            assert_eq!(snap.budget_pct, 42.0);
            assert_eq!(snap.phase, "thinking");
            assert_eq!(snap.session_tokens, 1234);
        });
    }

    #[test]
    fn push_log_entries() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            s.push_log(LogLevel::Info, "hello".to_string());
            s.push_log(LogLevel::Error, "oops".to_string());
            assert_eq!(s.log_entries.len(), 2);
            assert_eq!(s.log_entries[0].id, 0);
            assert_eq!(s.log_entries[0].level, "info");
            assert_eq!(s.log_entries[0].content, "hello");
            assert_eq!(s.log_entries[1].id, 1);
            assert_eq!(s.log_entries[1].level, "error");
        });
    }

    #[test]
    fn process_action_approve_with_pending() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            let (tx, rx) = tokio::sync::oneshot::channel();
            s.pending_approval = Some(PendingApprovalState {
                id: 1,
                command_preview: "rm -rf /tmp".to_string(),
                category: "destructive".to_string(),
                responder: Some(tx),
            });

            let outcome = process_action_sync(&mut s, UserAction::Approve { id: 1 });
            assert_eq!(outcome, ActionOutcome::Ok);
            assert!(s.pending_approval.is_none());
            assert_eq!(s.phase, Phase::RunningAgent);

            // Check the oneshot received the response
            let response = rx.await.unwrap();
            assert_eq!(response, ApprovalResponse::Approve);
        });
    }

    #[test]
    fn process_action_approve_without_pending() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            let outcome = process_action_sync(&mut s, UserAction::Approve { id: 1 });
            match outcome {
                ActionOutcome::NoOp { reason } => {
                    assert!(reason.contains("No pending approval"));
                }
                _ => panic!("Expected NoOp"),
            }
        });
    }

    #[test]
    fn process_action_deny() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            let (tx, rx) = tokio::sync::oneshot::channel();
            s.pending_approval = Some(PendingApprovalState {
                id: 2,
                command_preview: "curl evil.com".to_string(),
                category: "network".to_string(),
                responder: Some(tx),
            });

            let outcome = process_action_sync(&mut s, UserAction::Deny { id: 2 });
            assert_eq!(outcome, ActionOutcome::Ok);
            assert_eq!(s.phase, Phase::Done);

            let response = rx.await.unwrap();
            assert_eq!(response, ApprovalResponse::Deny);
        });
    }

    #[test]
    fn process_action_skip() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            let (tx, rx) = tokio::sync::oneshot::channel();
            s.pending_approval = Some(PendingApprovalState {
                id: 3,
                command_preview: "test".to_string(),
                category: "exec".to_string(),
                responder: Some(tx),
            });

            let outcome = process_action_sync(&mut s, UserAction::Skip { id: 3 });
            assert_eq!(outcome, ActionOutcome::Ok);
            assert_eq!(s.phase, Phase::RunningAgent);

            let response = rx.await.unwrap();
            assert_eq!(response, ApprovalResponse::Skip);
        });
    }

    #[test]
    fn process_action_approve_all() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            let (tx, rx) = tokio::sync::oneshot::channel();
            s.pending_approval = Some(PendingApprovalState {
                id: 4,
                command_preview: "ls".to_string(),
                category: "exec".to_string(),
                responder: Some(tx),
            });

            let outcome = process_action_sync(&mut s, UserAction::ApproveAll { id: 4 });
            assert_eq!(outcome, ActionOutcome::Ok);

            let response = rx.await.unwrap();
            assert_eq!(response, ApprovalResponse::ApproveAll);
        });
    }

    #[test]
    fn process_action_set_verbosity() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            let outcome = process_action_sync(
                &mut s,
                UserAction::SetVerbosity {
                    level: Verbosity::Debug,
                },
            );
            assert_eq!(outcome, ActionOutcome::Ok);
            assert_eq!(s.verbosity, Verbosity::Debug);
        });
    }

    #[test]
    fn process_action_quit() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            let outcome = process_action_sync(&mut s, UserAction::Quit);
            assert_eq!(outcome, ActionOutcome::Ok);
            assert!(s.should_quit);
        });
    }

    #[test]
    fn process_action_respond_human_without_question() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            let outcome = process_action_sync(
                &mut s,
                UserAction::RespondHuman {
                    text: "hello".to_string(),
                },
            );
            match outcome {
                ActionOutcome::NoOp { reason } => {
                    assert!(reason.contains("No pending human question"));
                }
                _ => panic!("Expected NoOp"),
            }
        });
    }

    #[test]
    fn phase_to_str_all_variants() {
        assert_eq!(phase_to_str(&Phase::Thinking), "thinking");
        assert_eq!(phase_to_str(&Phase::RunningAgent), "running_agent");
        assert_eq!(phase_to_str(&Phase::Orchestrating), "orchestrating");
        assert_eq!(phase_to_str(&Phase::WaitingApproval), "waiting_approval");
        assert_eq!(phase_to_str(&Phase::WaitingHuman), "waiting_human");
        assert_eq!(phase_to_str(&Phase::Idle), "idle");
        assert_eq!(phase_to_str(&Phase::Done), "done");
    }

    #[test]
    fn parse_verbosity_all_variants() {
        assert_eq!(parse_verbosity("quiet"), Some(Verbosity::Quiet));
        assert_eq!(parse_verbosity("normal"), Some(Verbosity::Normal));
        assert_eq!(parse_verbosity("verbose"), Some(Verbosity::Verbose));
        assert_eq!(parse_verbosity("debug"), Some(Verbosity::Debug));
        assert_eq!(parse_verbosity("QUIET"), Some(Verbosity::Quiet));
        assert_eq!(parse_verbosity("unknown"), None);
    }

    #[test]
    fn parse_restart_after_defaults_to_turn_end() {
        assert_eq!(parse_restart_after(None).unwrap(), RestartAfter::TurnEnd);
        assert_eq!(
            parse_restart_after(Some("turn_end")).unwrap(),
            RestartAfter::TurnEnd
        );
        assert_eq!(parse_restart_after(Some("NOW")).unwrap(), RestartAfter::Now);
    }

    #[test]
    fn parse_restart_after_rejects_invalid_value() {
        let err = parse_restart_after(Some("later")).unwrap_err();
        assert!(err.contains("restart_after must be 'turn_end' or 'now'"));
    }

    #[test]
    fn normalize_optional_string_field_trims_and_drops_empty() {
        let mut value = Some("  hello  ".to_string());
        normalize_optional_string_field(&mut value);
        assert_eq!(value.as_deref(), Some("hello"));

        let mut empty = Some("   ".to_string());
        normalize_optional_string_field(&mut empty);
        assert!(empty.is_none());
    }

    #[test]
    fn controller_restart_state_defaults() {
        let params = ScheduleControllerRestartParams {
            controller_id: "codex".to_string(),
            north_star_goal: "audit and improve".to_string(),
            reason: Some("periodic refresh".to_string()),
            restart_after: None,
            restart_command: None,
            auto_start_task: None,
            max_attempts: None,
            cooldown_sec: None,
        };
        let state = ControllerRestartState::new(&params);
        assert_eq!(state.controller_id, "codex");
        assert_eq!(state.phase, RestartPhase::AwaitingTurnComplete);
        assert_eq!(state.max_attempts, 1);
        assert_eq!(state.cooldown_sec, 30);
        assert!(!state.auto_start_task);
    }

    #[test]
    fn restart_state_public_value_redacts_turn_complete_token() {
        let params = ScheduleControllerRestartParams {
            controller_id: "codex".to_string(),
            north_star_goal: "audit and improve".to_string(),
            reason: None,
            restart_after: None,
            restart_command: Some("true".to_string()),
            auto_start_task: Some(false),
            max_attempts: None,
            cooldown_sec: None,
        };
        let restart = ControllerRestartState::new(&params);
        let raw_token = restart.turn_complete_token.clone();

        let public = restart_state_public_value(Some(&restart));
        assert_eq!(
            public.get("turn_complete_token").and_then(|v| v.as_str()),
            Some("[redacted]")
        );
        assert_ne!(
            public.get("turn_complete_token").and_then(|v| v.as_str()),
            Some(raw_token.as_str())
        );
        assert_eq!(
            public.get("restart_id").and_then(|v| v.as_str()),
            Some(restart.restart_id.as_str())
        );
    }

    #[test]
    fn resource_definitions_has_five_entries() {
        let defs = resource_definitions();
        assert_eq!(defs.len(), 5);
    }

    #[test]
    fn format_outcome_ok() {
        assert_eq!(format_outcome(ActionOutcome::Ok), "ok");
    }

    #[test]
    fn format_outcome_noop() {
        let s = format_outcome(ActionOutcome::NoOp {
            reason: "test".to_string(),
        });
        assert!(s.starts_with("no-op:"));
        assert!(s.contains("test"));
    }

    #[test]
    fn server_info_has_correct_name() {
        let state = test_state();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (bus, _rx) = EventBus::new();
            let server = IntendantServer::new(state, bus);
            let info = server.get_info();
            assert_eq!(info.server_info.name, "intendant");
            assert!(info.instructions.is_some());
        });
    }

    #[test]
    fn all_user_actions_handled_by_process_action() {
        // This test ensures process_action_sync handles every UserAction variant.
        // If a new variant is added, the exhaustive match in process_action_sync
        // will cause a compile error, AND this test will need updating.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let actions: Vec<UserAction> = vec![
                UserAction::Approve { id: 1 },
                UserAction::Deny { id: 1 },
                UserAction::Skip { id: 1 },
                UserAction::ApproveAll { id: 1 },
                UserAction::RespondHuman {
                    text: "test".to_string(),
                },
                UserAction::SetAutonomy {
                    level: AutonomyLevel::High,
                },
                UserAction::SetVerbosity {
                    level: Verbosity::Normal,
                },
                UserAction::Quit,
            ];
            for action in actions {
                let mut s = state.write().await;
                // Should not panic for any variant
                let _ = process_action_sync(&mut s, action);
            }
        });
    }

    #[test]
    fn approval_snapshot_none_when_empty() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let s = state.read().await;
            assert!(s.approval_snapshot().is_none());
        });
    }

    #[test]
    fn approval_snapshot_present_when_set() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            let (tx, _rx) = tokio::sync::oneshot::channel();
            s.pending_approval = Some(PendingApprovalState {
                id: 42,
                command_preview: "rm -rf /".to_string(),
                category: "destructive".to_string(),
                responder: Some(tx),
            });
            let snap = s.approval_snapshot().unwrap();
            assert_eq!(snap.id, 42);
            assert_eq!(snap.category, "destructive");
        });
    }

    #[test]
    fn human_question_snapshot_roundtrip() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            assert!(s.human_question_snapshot().is_none());
            s.human_question = Some("Which database?".to_string());
            let snap = s.human_question_snapshot().unwrap();
            assert_eq!(snap.question, "Which database?");
        });
    }

    #[tokio::test]
    async fn schedule_restart_rejects_missing_actions() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let (bus, _rx) = EventBus::new();
        let server = IntendantServer::new(state, bus);

        let output = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("now".to_string()),
                restart_command: None,
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;

        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(false));
        assert_eq!(json["status"].as_str(), Some("rejected"));
        assert!(json["error"]
            .as_str()
            .unwrap_or("")
            .contains("configure at least one restart action"));
    }

    #[tokio::test]
    async fn schedule_restart_normalizes_string_fields() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let (bus, _rx) = EventBus::new();
        let server = IntendantServer::new(state.clone(), bus);

        let output = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "  codex  ".to_string(),
                north_star_goal: "  improve loop  ".to_string(),
                reason: Some("  periodic refresh  ".to_string()),
                restart_after: Some("  NOW  ".to_string()),
                restart_command: Some("  true  ".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(true));

        let s = state.read().await;
        let restart = s
            .controller_restart
            .as_ref()
            .expect("restart should be stored");
        assert_eq!(restart.controller_id, "codex");
        assert_eq!(restart.north_star_goal, "improve loop");
        assert_eq!(restart.reason.as_deref(), Some("periodic refresh"));
        assert_eq!(restart.restart_after, RestartAfter::Now);
        assert_eq!(restart.restart_command.as_deref(), Some("true"));
    }

    #[tokio::test]
    async fn schedule_restart_now_reports_completed_phase() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let (bus, _rx) = EventBus::new();
        let server = IntendantServer::new(state, bus);

        let output = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("now".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(true));
        assert_eq!(json["phase"].as_str(), Some("completed"));
        assert!(json["execution"].as_str().unwrap_or("").contains("spawned"));
    }

    #[tokio::test]
    async fn schedule_restart_now_reports_failed_phase() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let (bus, _rx) = EventBus::new();
        let server = IntendantServer::new(state, bus);

        let output = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("now".to_string()),
                restart_command: None,
                auto_start_task: Some(true),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(false));
        assert_eq!(json["phase"].as_str(), Some("failed"));
        assert!(json["execution_error"]
            .as_str()
            .unwrap_or("")
            .contains("Failed to start follow-up task"));
    }

    #[tokio::test]
    async fn control_schedule_restart_rejects_missing_actions() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let (bus, _rx) = EventBus::new();
        let (control_tx, mut control_rx) = broadcast::channel::<String>(8);

        let resource = handle_control_command_mcp(
            &state,
            &bus,
            &Some(control_tx),
            ControlMsg::ScheduleControllerRestart {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("now".to_string()),
                restart_command: None,
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            },
        )
        .await;

        assert_eq!(resource, Some(RESOURCE_RESTART_URI));

        let event = timeout(Duration::from_millis(200), control_rx.recv())
            .await
            .expect("timed out waiting for command_result")
            .expect("broadcast recv failed");
        let json: serde_json::Value = serde_json::from_str(&event).unwrap();
        assert_eq!(
            json.get("event").and_then(|v| v.as_str()),
            Some("command_result")
        );
        assert_eq!(
            json.get("action").and_then(|v| v.as_str()),
            Some("schedule_controller_restart")
        );
        assert_eq!(json.get("ok").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(
            json.get("message").and_then(|v| v.as_str()),
            Some(
                "Invalid request: configure at least one restart action (restart_command and/or auto_start_task=true)"
            )
        );
    }

    #[tokio::test]
    async fn control_schedule_restart_now_reports_completed_phase() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let (bus, _rx) = EventBus::new();
        let (control_tx, mut control_rx) = broadcast::channel::<String>(8);

        let resource = handle_control_command_mcp(
            &state,
            &bus,
            &Some(control_tx),
            ControlMsg::ScheduleControllerRestart {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("now".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            },
        )
        .await;

        assert_eq!(resource, Some(RESOURCE_RESTART_URI));
        let event = timeout(Duration::from_millis(200), control_rx.recv())
            .await
            .expect("timed out waiting for command_result")
            .expect("broadcast recv failed");
        let json: serde_json::Value = serde_json::from_str(&event).unwrap();
        assert_eq!(
            json.get("event").and_then(|v| v.as_str()),
            Some("command_result")
        );
        assert_eq!(
            json.get("action").and_then(|v| v.as_str()),
            Some("schedule_controller_restart")
        );
        assert_eq!(json.get("ok").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(
            json.get("data")
                .and_then(|v| v.get("phase"))
                .and_then(|v| v.as_str()),
            Some("completed")
        );
    }

    #[tokio::test]
    async fn schedule_restart_rejects_invalid_restart_after() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let (bus, _rx) = EventBus::new();
        let server = IntendantServer::new(state, bus);

        let output = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("later".to_string()),
                restart_command: None,
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;

        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(false));
        assert_eq!(json["status"].as_str(), Some("rejected"));
        assert!(json["error"]
            .as_str()
            .unwrap_or("")
            .contains("restart_after must be 'turn_end' or 'now'"));
    }

    #[tokio::test]
    async fn control_schedule_restart_rejects_zero_max_attempts() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let (bus, _rx) = EventBus::new();
        let (control_tx, mut control_rx) = broadcast::channel::<String>(8);

        let resource = handle_control_command_mcp(
            &state,
            &bus,
            &Some(control_tx),
            ControlMsg::ScheduleControllerRestart {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: None,
                auto_start_task: Some(false),
                max_attempts: Some(0),
                cooldown_sec: None,
            },
        )
        .await;

        assert_eq!(resource, Some(RESOURCE_RESTART_URI));

        let event = timeout(Duration::from_millis(200), control_rx.recv())
            .await
            .expect("timed out waiting for command_result")
            .expect("broadcast recv failed");
        let json: serde_json::Value = serde_json::from_str(&event).unwrap();
        assert_eq!(
            json.get("event").and_then(|v| v.as_str()),
            Some("command_result")
        );
        assert_eq!(
            json.get("action").and_then(|v| v.as_str()),
            Some("schedule_controller_restart")
        );
        assert_eq!(json.get("ok").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(
            json.get("message").and_then(|v| v.as_str()),
            Some("Invalid request: max_attempts must be >= 1")
        );
    }

    #[tokio::test]
    async fn schedule_restart_rejects_empty_restart_command() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let (bus, _rx) = EventBus::new();
        let server = IntendantServer::new(state, bus);

        let output = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: Some("   ".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;

        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(false));
        assert_eq!(json["status"].as_str(), Some("rejected"));
        assert_eq!(
            json["error"].as_str(),
            Some("Invalid request: restart_command must not be empty")
        );
    }

    #[tokio::test]
    async fn schedule_restart_rejects_when_active_with_json_payload() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let (bus, _rx) = EventBus::new();
        let server = IntendantServer::new(state, bus);

        let first = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let first_json: serde_json::Value = serde_json::from_str(&first).unwrap();
        let restart_id = first_json["restart_id"].as_str().unwrap().to_string();

        let second = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop again".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&second).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(false));
        assert_eq!(json["status"].as_str(), Some("rejected"));
        assert_eq!(json["phase"].as_str(), Some("awaiting_turn_complete"));
        assert_eq!(json["restart_id"].as_str(), Some(restart_id.as_str()));
        assert!(json["error"]
            .as_str()
            .unwrap_or("")
            .contains("A restart is already active"));
    }

    #[tokio::test]
    async fn spawn_detached_restart_command_returns_live_pid() {
        let pid = spawn_detached_restart_command("sleep 30")
            .await
            .expect("detached spawn should succeed");
        assert!(pid > 1);

        let probe = std::process::Command::new("bash")
            .args(["-lc", &format!("kill -0 {}", pid)])
            .status()
            .expect("kill -0 should run");
        assert!(probe.success(), "spawned pid should be alive");

        let _ = std::process::Command::new("bash")
            .args(["-lc", &format!("kill -TERM {}", pid)])
            .status();
    }

    #[tokio::test]
    async fn controller_turn_complete_returns_json_success_payload() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let (bus, _rx) = EventBus::new();
        let server = IntendantServer::new(state.clone(), bus);

        let scheduled = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let scheduled_json: serde_json::Value = serde_json::from_str(&scheduled).unwrap();
        let restart_id = scheduled_json["restart_id"].as_str().unwrap().to_string();
        let token = scheduled_json["turn_complete_token"]
            .as_str()
            .unwrap()
            .to_string();

        let output = server
            .controller_turn_complete(Parameters(ControllerTurnCompleteParams {
                restart_id: restart_id.clone(),
                turn_complete_token: token,
                status: Some("ok".to_string()),
                handoff_summary: Some("handoff".to_string()),
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(true));
        assert_eq!(json["status"].as_str(), Some("completed"));
        assert_eq!(json["restart_id"].as_str(), Some(restart_id.as_str()));
        assert_eq!(json["phase"].as_str(), Some("completed"));
        assert!(json["execution"].as_str().unwrap_or("").contains("spawned"));
    }

    #[tokio::test]
    async fn get_restart_status_redacts_turn_complete_token() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let (bus, _rx) = EventBus::new();
        let server = IntendantServer::new(state, bus);

        let scheduled = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let scheduled_json: serde_json::Value = serde_json::from_str(&scheduled).unwrap();
        let token = scheduled_json["turn_complete_token"]
            .as_str()
            .unwrap()
            .to_string();

        let status = server.get_restart_status().await;
        let status_json: serde_json::Value = serde_json::from_str(&status).unwrap();
        assert_eq!(
            status_json["turn_complete_token"].as_str(),
            Some("[redacted]")
        );
        assert_ne!(status_json["turn_complete_token"].as_str(), Some(token.as_str()));
    }

    #[tokio::test]
    async fn controller_turn_complete_marks_restart_failed_when_auto_start_task_fails() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let (bus, _rx) = EventBus::new();
        let server = IntendantServer::new(state.clone(), bus);

        let scheduled = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: None,
                auto_start_task: Some(true),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let scheduled_json: serde_json::Value = serde_json::from_str(&scheduled).unwrap();
        let restart_id = scheduled_json["restart_id"].as_str().unwrap().to_string();
        let token = scheduled_json["turn_complete_token"]
            .as_str()
            .unwrap()
            .to_string();

        {
            let mut s = state.write().await;
            s.set_phase(Phase::RunningAgent);
        }

        let output = server
            .controller_turn_complete(Parameters(ControllerTurnCompleteParams {
                restart_id: restart_id.clone(),
                turn_complete_token: token,
                status: None,
                handoff_summary: None,
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(false));
        assert_eq!(json["status"].as_str(), Some("restart_pending"));
        assert_eq!(json["phase"].as_str(), Some("failed"));
        assert!(json["error"]
            .as_str()
            .unwrap_or("")
            .contains("Failed to start follow-up task"));

        let restart_path = restart_state_path(dir.path());
        let persisted = std::fs::read_to_string(restart_path).expect("restart file should exist");
        let persisted_json: serde_json::Value = serde_json::from_str(&persisted).unwrap();
        let restart_json = persisted_json.as_object().expect("restart should persist");
        assert_eq!(
            restart_json.get("phase").and_then(|v| v.as_str()),
            Some("failed")
        );
        assert!(restart_json
            .get("last_error")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .contains("Failed to start follow-up task"));
    }

    #[tokio::test]
    async fn controller_turn_complete_returns_json_error_payload() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let (bus, _rx) = EventBus::new();
        let server = IntendantServer::new(state, bus);

        let scheduled = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let scheduled_json: serde_json::Value = serde_json::from_str(&scheduled).unwrap();
        let restart_id = scheduled_json["restart_id"].as_str().unwrap().to_string();

        let output = server
            .controller_turn_complete(Parameters(ControllerTurnCompleteParams {
                restart_id: restart_id.clone(),
                turn_complete_token: "wrong".to_string(),
                status: None,
                handoff_summary: None,
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(false));
        assert_eq!(json["status"].as_str(), Some("rejected"));
        assert_eq!(json["restart_id"].as_str(), Some(restart_id.as_str()));
        assert_eq!(json["phase"].as_str(), Some("awaiting_turn_complete"));
        assert_eq!(
            json["error"].as_str(),
            Some("turn_complete_token is invalid")
        );
    }

    #[tokio::test]
    async fn controller_turn_complete_normalizes_ids_and_optional_fields() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let (bus, _rx) = EventBus::new();
        let server = IntendantServer::new(state.clone(), bus);

        let scheduled = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let scheduled_json: serde_json::Value = serde_json::from_str(&scheduled).unwrap();
        let restart_id = scheduled_json["restart_id"].as_str().unwrap().to_string();
        let token = scheduled_json["turn_complete_token"]
            .as_str()
            .unwrap()
            .to_string();

        let output = server
            .controller_turn_complete(Parameters(ControllerTurnCompleteParams {
                restart_id: format!("  {}  ", restart_id),
                turn_complete_token: format!("  {}  ", token),
                status: Some("   ".to_string()),
                handoff_summary: Some("  handoff summary  ".to_string()),
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(true));

        let s = state.read().await;
        let restart = s
            .controller_restart
            .as_ref()
            .expect("restart should be stored");
        assert!(restart.completion_status.is_none());
        assert_eq!(restart.handoff_summary.as_deref(), Some("handoff summary"));
    }

    #[tokio::test]
    async fn cancel_controller_restart_returns_json_success_payload() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let (bus, _rx) = EventBus::new();
        let server = IntendantServer::new(state, bus);

        let scheduled = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let scheduled_json: serde_json::Value = serde_json::from_str(&scheduled).unwrap();
        let restart_id = scheduled_json["restart_id"].as_str().unwrap().to_string();

        let output = server
            .cancel_controller_restart(Parameters(CancelControllerRestartParams {
                restart_id: Some(restart_id.clone()),
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(true));
        assert_eq!(json["status"].as_str(), Some("cancelled"));
        assert_eq!(json["restart_id"].as_str(), Some(restart_id.as_str()));
        assert_eq!(json["phase"].as_str(), Some("cancelled"));
    }

    #[tokio::test]
    async fn cancel_controller_restart_returns_json_error_payload() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let (bus, _rx) = EventBus::new();
        let server = IntendantServer::new(state, bus);

        let output = server
            .cancel_controller_restart(Parameters(CancelControllerRestartParams {
                restart_id: Some("abc".to_string()),
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(false));
        assert_eq!(json["status"].as_str(), Some("rejected"));
        assert_eq!(
            json["error"].as_str(),
            Some("No controller restart is scheduled")
        );
        assert_eq!(json["restart_id"].as_str(), Some("abc"));
    }

    #[tokio::test]
    async fn cancel_controller_restart_treats_whitespace_guard_as_none() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let (bus, _rx) = EventBus::new();
        let server = IntendantServer::new(state, bus);

        let scheduled = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let scheduled_json: serde_json::Value = serde_json::from_str(&scheduled).unwrap();
        let restart_id = scheduled_json["restart_id"].as_str().unwrap().to_string();

        let output = server
            .cancel_controller_restart(Parameters(CancelControllerRestartParams {
                restart_id: Some("   ".to_string()),
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(true));
        assert_eq!(json["restart_id"].as_str(), Some(restart_id.as_str()));
    }
}
