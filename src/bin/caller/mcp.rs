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
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        Implementation, ListResourcesResult, PaginatedRequestParams, RawResource,
        ReadResourceRequestParams, ReadResourceResult, Resource, ResourceContents,
        ResourceUpdatedNotificationParam, ServerCapabilities, ServerInfo,
        SubscribeRequestParams, UnsubscribeRequestParams,
    },
    schemars, tool, tool_handler, tool_router,
    service::{RequestContext, RoleServer},
};
use serde::Deserialize;
use tokio::sync::{Mutex, RwLock};

use crate::autonomy::{AutonomyLevel, SharedAutonomy};
use crate::frontend::{
    self, ActionOutcome, ApprovalSnapshot, HumanQuestionSnapshot, LogEntrySnapshot,
    StateResult, StatusSnapshot, UserAction,
};
use crate::tui::app::{LogLevel, Phase, Verbosity};
use crate::tui::event::{AppEvent, ApprovalResponse, EventBus};

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
    pub log_entries: Vec<LogEntrySnapshot>,
    next_log_id: u64,
    pub pending_approval: Option<PendingApprovalState>,
    pub human_question: Option<String>,
    pub should_quit: bool,
    /// Optional launcher for starting tasks via MCP. Set by main.rs.
    pub launcher: Option<Arc<TaskLauncher>>,
    /// Handle to the currently running agent loop, if any.
    pub task_handle: Option<tokio::task::JoinHandle<()>>,
}

/// Tracks a pending approval along with the oneshot sender.
pub struct PendingApprovalState {
    pub id: u64,
    pub command_preview: String,
    pub category: String,
    pub responder: Option<tokio::sync::oneshot::Sender<ApprovalResponse>>,
}

impl McpAppState {
    pub fn new(provider_name: String, model_name: String, autonomy: SharedAutonomy) -> Self {
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
            log_entries: Vec::new(),
            next_log_id: 0,
            pending_approval: None,
            human_question: None,
            should_quit: false,
            launcher: None,
            task_handle: None,
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
        self.log_entries.push(LogEntrySnapshot {
            id,
            ts,
            level: frontend::log_level_to_str(&level).to_string(),
            content,
        });
        // Cap at 10k entries (same as TUI)
        if self.log_entries.len() > 10_000 {
            self.log_entries.drain(..1000);
        }
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
        self.human_question
            .as_ref()
            .map(|q| HumanQuestionSnapshot {
                question: q.clone(),
            })
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
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
            let mut resource_changed: Option<&str> = None;

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

                    AppEvent::JsonExtracted { preview } => {
                        s.push_log(LogLevel::Debug, format!("JSON: {}", preview));
                    }

                    AppEvent::DoneSignal { message } => {
                        s.set_phase(Phase::Done);
                        s.push_log(
                            LogLevel::Info,
                            format!(
                                "Done: {}",
                                message.as_deref().unwrap_or("task complete")
                            ),
                        );
                        resource_changed = Some("intendant://status");
                    }

                    AppEvent::AgentStarted { turn, commands_preview } => {
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
                        s.push_log(
                            LogLevel::Debug,
                            format!("[T{}] Context management", turn),
                        );
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
                        s.push_log(LogLevel::Error, "Safety cap reached (500 turns)".to_string());
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

                    AppEvent::ControlCommand(_) => {
                        // Control socket commands are handled by the control socket;
                        // the MCP server is a separate interface.
                    }
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
                // Write response to shared file (same mechanism as TUI)
                let response_path = shared_file_path("intendant_human_response");
                if std::fs::write(&response_path, &text).is_ok() {
                    state.human_question = None;
                    state.set_phase(Phase::RunningAgent);
                    state.push_log(
                        LogLevel::Info,
                        format!("Human response (MCP): {}", text),
                    );
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

fn shared_file_path(name: &str) -> std::path::PathBuf {
    let base = std::env::var("INTENDANT_SHARED_DIR")
        .map(std::path::PathBuf::from)
        .ok()
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| {
            let shm = std::path::PathBuf::from("/dev/shm");
            if shm.exists() {
                Some(shm)
            } else {
                None
            }
        })
        .unwrap_or_else(std::env::temp_dir);
    base.join(name)
}

// ---------------------------------------------------------------------------
// Tool implementations
// ---------------------------------------------------------------------------

#[tool_router]
impl IntendantServer {
    #[tool(description = "Get current status: provider, model, turn, budget, phase, autonomy, verbosity, tokens.")]
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

    #[tool(description = "Get log entries. Supports cursor-based pagination via since_id and filtering by level.")]
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

    #[tool(description = "Get the current pending approval request, if any. Returns null if no approval is pending.")]
    async fn get_pending_approval(&self) -> String {
        let s = self.state.read().await;
        match s.approval_snapshot() {
            Some(snap) => serde_json::to_string_pretty(&snap).unwrap_or_else(|_| "null".to_string()),
            None => "null".to_string(),
        }
    }

    #[tool(description = "Get the current pending human question, if any. Returns null if no question is pending.")]
    async fn get_pending_input(&self) -> String {
        let s = self.state.read().await;
        match s.human_question_snapshot() {
            Some(snap) => serde_json::to_string_pretty(&snap).unwrap_or_else(|_| "null".to_string()),
            None => "null".to_string(),
        }
    }

    #[tool(description = "Approve a pending command execution. Equivalent to pressing 'y' in the TUI.")]
    async fn approve(&self, Parameters(params): Parameters<ApproveParams>) -> String {
        let action = UserAction::Approve { id: params.id };
        let mut s = self.state.write().await;
        let outcome = process_action_sync(&mut s, action);
        format_outcome(outcome)
    }

    #[tool(description = "Deny a pending command execution. Stops the agent loop. Equivalent to pressing 'n' in the TUI.")]
    async fn deny(&self, Parameters(params): Parameters<DenyParams>) -> String {
        let action = UserAction::Deny { id: params.id };
        let mut s = self.state.write().await;
        let outcome = process_action_sync(&mut s, action);
        format_outcome(outcome)
    }

    #[tool(description = "Skip a pending command execution. The agent continues with the next command. Equivalent to pressing 's' in the TUI.")]
    async fn skip(&self, Parameters(params): Parameters<SkipParams>) -> String {
        let action = UserAction::Skip { id: params.id };
        let mut s = self.state.write().await;
        let outcome = process_action_sync(&mut s, action);
        format_outcome(outcome)
    }

    #[tool(description = "Approve this and all future commands (sets autonomy to Full). Equivalent to pressing 'a' in the TUI.")]
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

    #[tool(description = "Respond to an askHuman question. Equivalent to typing a response and pressing Enter in the TUI.")]
    async fn respond(&self, Parameters(params): Parameters<RespondParams>) -> String {
        let action = UserAction::RespondHuman { text: params.text };
        let mut s = self.state.write().await;
        let outcome = process_action_sync(&mut s, action);
        format_outcome(outcome)
    }

    #[tool(description = "Set the autonomy level. Controls how much approval is required. Equivalent to +/- keys in the TUI.")]
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
        let _ = process_action_sync(
            &mut s,
            UserAction::SetAutonomy { level },
        );
        s.push_log(
            LogLevel::Info,
            format!("Autonomy set to {} by MCP agent", level),
        );
        format!("Autonomy set to {}", level)
    }

    #[tool(description = "Set log verbosity level. Controls which log entries are shown. Equivalent to pressing 'v' in the TUI.")]
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

    #[tool(description = "Shut down the Intendant agent. Equivalent to pressing 'q' or Ctrl-C in the TUI.")]
    async fn quit(&self) -> String {
        let action = UserAction::Quit;
        let mut s = self.state.write().await;
        let outcome = process_action_sync(&mut s, action);
        format_outcome(outcome)
    }

    #[tool(description = "Start a new task for the Intendant agent to execute. The agent will begin working on the task immediately. Only one task can run at a time — check get_status to see if a task is already running.")]
    async fn start_task(&self, Parameters(params): Parameters<StartTaskParams>) -> String {
        let mut s = self.state.write().await;

        // Check if a task is already running
        match s.phase {
            Phase::Thinking | Phase::RunningAgent | Phase::Orchestrating
            | Phase::WaitingApproval | Phase::WaitingHuman => {
                return format!(
                    "Cannot start task: agent is currently in '{}' phase. \
                     Wait for it to finish or call quit first.",
                    phase_to_str(&s.phase)
                );
            }
            Phase::Idle | Phase::Done => {} // OK to start
        }

        let launcher = match s.launcher.as_ref() {
            Some(l) => Arc::clone(l),
            None => {
                return "Cannot start task: no task launcher configured. \
                        This MCP server was not started with launcher support."
                    .to_string();
            }
        };

        // Reset state for the new task
        s.turn = 0;
        s.budget_pct = 0.0;
        s.session_tokens = 0;
        s.set_phase(Phase::Thinking);
        s.pending_approval = None;
        s.human_question = None;
        s.should_quit = false;
        s.push_log(
            LogLevel::Info,
            format!("Task started via MCP: {}", params.task),
        );

        // We need to drop the write lock before calling the async launcher
        let bus = self.bus.clone();
        drop(s);

        let handle = (launcher)(params.task, bus).await;

        // Store the handle
        let mut s = self.state.write().await;
        s.task_handle = Some(handle);

        "ok".to_string()
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
                 deny, respond, set_autonomy, quit) and to observe its state (get_status, \
                 get_logs, get_pending_approval, get_pending_input). Resources provide \
                 push-based state updates via subscriptions."
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
// Public API: start the MCP server on stdio
// ---------------------------------------------------------------------------

/// Run the MCP server on stdio. This replaces the TUI — the external agent
/// communicates via MCP over stdin/stdout.
///
/// The server consumes AppEvents from the bus and exposes them as tools and
/// resources.
pub async fn run_mcp_server(
    state: SharedMcpState,
    bus: EventBus,
    event_rx: tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let server = IntendantServer::new(state.clone(), bus);

    // Start serving on stdio
    let transport = rmcp::transport::io::stdio();
    let running = server.serve(transport).await?;

    // Store the peer for sending notifications
    let peer = Arc::new(Mutex::new(Some(running.peer().clone())));

    // Spawn event listener that mirrors AppEvents into McpAppState
    let _listener = spawn_event_listener(state, event_rx, peer);

    // Wait until the service finishes (client disconnects or quit)
    running.waiting().await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autonomy::{self, AutonomyState};

    fn test_state() -> SharedMcpState {
        let autonomy = autonomy::shared_autonomy(AutonomyState::default());
        Arc::new(RwLock::new(McpAppState::new(
            "openai".to_string(),
            "gpt-5".to_string(),
            autonomy,
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

            let outcome =
                process_action_sync(&mut s, UserAction::Approve { id: 1 });
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
            let outcome =
                process_action_sync(&mut s, UserAction::Approve { id: 1 });
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

            let outcome =
                process_action_sync(&mut s, UserAction::ApproveAll { id: 4 });
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
    fn resource_definitions_has_four_entries() {
        let defs = resource_definitions();
        assert_eq!(defs.len(), 4);
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
}
