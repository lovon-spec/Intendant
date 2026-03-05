use crate::conversation::Conversation;
use crate::error::CallerError;
use crate::knowledge::{self, KnowledgeQuery};
use crate::provider::ChatProvider;
use crate::session_log;
use crate::tools::ToolDefinition;
use crate::tui::event::{AppEvent, ControlMsg, EventBus};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// Configuration for the presence layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresenceConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,

    // --- Text mode (TUI / MCP) ---
    /// Provider name for text mode (e.g. "gemini", "anthropic", "openai").
    /// Default: auto-detect (prefers gemini when GEMINI_API_KEY is set).
    #[serde(default)]
    pub provider: Option<String>,
    /// Model for text mode. Default: "gemini-2.5-flash".
    #[serde(default)]
    pub model: Option<String>,
    /// Context window size for the text-mode presence conversation.
    #[serde(default = "default_context_window")]
    pub context_window: u64,

    // --- Live mode (audio/video gateway) ---
    /// Provider for live (audio/video) mode. Default: auto-detect.
    #[serde(default)]
    pub live_provider: Option<String>,
    /// Model for live (audio/video) mode.
    #[serde(default)]
    pub live_model: Option<String>,
    /// Context window for the live-mode conversation.
    #[serde(default = "default_live_context_window")]
    pub live_context_window: u64,
}

fn default_true() -> bool {
    true
}

fn default_context_window() -> u64 {
    1_048_576
}

fn default_live_context_window() -> u64 {
    32_768
}

/// Default text presence model.
pub const DEFAULT_TEXT_MODEL: &str = "gemini-2.5-flash";
/// Preferred text presence model (Gemini 3 Flash, when available).
#[allow(dead_code)]
pub const PREFERRED_TEXT_MODEL: &str = "gemini-3-flash-preview";
/// Default text presence provider.
#[allow(dead_code)]
pub const DEFAULT_TEXT_PROVIDER: &str = "gemini";

impl Default for PresenceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            provider: None,
            model: None,
            context_window: default_context_window(),
            live_provider: None,
            live_model: None,
            live_context_window: default_live_context_window(),
        }
    }
}

/// A structured task submission from presence to the agent loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskEnvelope {
    pub task: String,
    #[serde(default)]
    pub force_direct: bool,
    #[serde(default)]
    pub context_hints: Vec<String>,
}

/// Filtered events pushed to the presence layer from the agent loop.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum PresenceEvent {
    PhaseChanged { phase: String },
    TaskComplete { reason: String },
    ApprovalNeeded { id: u64, preview: String, category: String },
    HumanQuestion { question: String },
    BudgetWarning { pct: f64, remaining: u64 },
    RoundComplete { round: usize, turns_in_round: usize },
    Error { message: String },
}

/// Token usage snapshot from the presence layer.
#[derive(Debug, Clone)]
pub struct PresenceUsage {
    pub total_tokens: u64,
    pub context_window: u64,
    pub usage_pct: f64,
    pub provider: String,
    pub model: String,
}

/// Queryable snapshot of the agent's current state.
#[derive(Debug, Clone, Default)]
pub struct AgentStateSnapshot {
    pub phase: String,
    pub turn: usize,
    pub budget_pct: f64,
    pub last_output_summary: String,
    pub last_command_preview: String,
    pub active_workers: Vec<String>,
}

/// The running presence layer instance.
pub struct PresenceLayer {
    provider: Box<dyn ChatProvider>,
    conversation: Conversation,
    bus: EventBus,
    /// Channel to submit tasks to the agent loop.
    task_tx: mpsc::Sender<TaskEnvelope>,
    /// Channel to receive filtered events from the agent loop.
    #[allow(dead_code)]
    event_rx: mpsc::Receiver<PresenceEvent>,
    /// Shared agent state snapshot, updated by the event listener.
    agent_state: Arc<Mutex<AgentStateSnapshot>>,
    /// Path to the knowledge store.
    knowledge_path: PathBuf,
    /// Session log directory for query_detail.
    log_dir: PathBuf,
    /// Project root for file reads and git operations.
    project_root: PathBuf,
}

#[allow(dead_code)]
impl PresenceLayer {
    /// Create a new presence layer.
    pub fn new(
        provider: Box<dyn ChatProvider>,
        system_prompt: String,
        context_window: u64,
        bus: EventBus,
        task_tx: mpsc::Sender<TaskEnvelope>,
        event_rx: mpsc::Receiver<PresenceEvent>,
        agent_state: Arc<Mutex<AgentStateSnapshot>>,
        knowledge_path: PathBuf,
        log_dir: PathBuf,
        project_root: PathBuf,
    ) -> Self {
        let conversation = Conversation::new(system_prompt, context_window);
        Self {
            provider,
            conversation,
            bus,
            task_tx,
            event_rx,
            agent_state,
            knowledge_path,
            log_dir,
            project_root,
        }
    }

    /// Process text input from the user, returning the model's response.
    pub async fn process_user_input(&mut self, input: &str) -> Result<String, CallerError> {
        self.conversation.add_user(input.to_string());
        let result = self.run_model_loop().await;
        self.emit_usage_update();
        result
    }

    /// Return current token usage stats for the presence conversation.
    pub fn usage_snapshot(&self) -> PresenceUsage {
        PresenceUsage {
            total_tokens: self.conversation.last_usage().map(|u| u.total_tokens).unwrap_or(0),
            context_window: self.conversation.context_window(),
            usage_pct: self.conversation.usage_fraction() * 100.0,
            provider: self.provider.name().to_string(),
            model: self.provider.model().to_string(),
        }
    }

    /// Emit a PresenceUsageUpdate event to the TUI.
    fn emit_usage_update(&self) {
        let usage = self.usage_snapshot();
        self.bus.send(AppEvent::PresenceUsageUpdate {
            total_tokens: usage.total_tokens,
            context_window: usage.context_window,
            usage_pct: usage.usage_pct,
            provider: usage.provider,
            model: usage.model,
        });
    }

    /// Inject a PresenceEvent into the conversation and let the model narrate.
    pub async fn handle_event(&mut self, event: PresenceEvent) -> Result<Option<String>, CallerError> {
        let event_text = format_event(&event);
        self.conversation.add_user(format!("[Event] {}", event_text));
        let response = self.run_model_loop().await?;
        if response.trim().is_empty() {
            Ok(None)
        } else {
            Ok(Some(response))
        }
    }

    /// Run the model in a loop, handling tool calls until a text response is produced.
    async fn run_model_loop(&mut self) -> Result<String, CallerError> {
        use crate::tui::app::LogLevel;
        const MAX_TOOL_ROUNDS: usize = 10;

        for round in 0..MAX_TOOL_ROUNDS {
            // Emit a visible log before the (potentially slow) API call so the
            // TUI isn't blank while waiting.
            self.bus.send(AppEvent::PresenceLog {
                message: if round == 0 {
                    format!("Thinking ({})...", self.provider.model())
                } else {
                    format!("Thinking (tool round {})...", round + 1)
                },
                level: None, // Info — visible at Normal verbosity
            });

            let messages = self.conversation.messages().to_vec();
            let response = self.provider.chat(&messages).await?;

            // Debug: token usage per call
            self.bus.send(AppEvent::PresenceLog {
                message: format!(
                    "Tokens: {} prompt + {} completion = {} total",
                    response.usage.prompt_tokens,
                    response.usage.completion_tokens,
                    response.usage.total_tokens,
                ),
                level: Some(LogLevel::Debug),
            });

            self.conversation.set_usage(response.usage.clone());
            self.conversation.auto_compact();

            if response.tool_calls.is_empty() {
                // Pure text response — return it
                if !response.content.is_empty() {
                    self.conversation.add_assistant(response.content.clone());
                }
                return Ok(response.content);
            }

            // Has tool calls — process them
            let tool_names: Vec<&str> = response.tool_calls.iter().map(|tc| tc.name.as_str()).collect();
            self.bus.send(AppEvent::PresenceLog {
                message: format!("Tool call: {}", tool_names.join(", ")),
                level: None,
            });

            // Verbose: model reasoning text alongside tool calls
            if !response.content.is_empty() {
                self.bus.send(AppEvent::PresenceLog {
                    message: format!("Model text: {}", response.content),
                    level: Some(LogLevel::Agent),
                });
            }

            // Debug: full tool call arguments
            for tc in &response.tool_calls {
                self.bus.send(AppEvent::PresenceLog {
                    message: format!("{}({})", tc.name, tc.arguments),
                    level: Some(LogLevel::Debug),
                });
            }

            let tool_call_refs: Vec<crate::conversation::ToolCallRef> = response
                .tool_calls
                .iter()
                .map(|tc| crate::conversation::ToolCallRef {
                    id: tc.id.clone(),
                    call_id: tc.call_id.clone(),
                    name: tc.name.clone(),
                    arguments: tc.arguments.clone(),
                })
                .collect();

            self.conversation.add_assistant_tool_calls(
                response.content.clone(),
                tool_call_refs,
                response.raw_output.clone(),
            );

            for tc in &response.tool_calls {
                let result = self.handle_presence_tool_call(&tc.name, &tc.arguments).await;
                // Debug: tool call result
                let result_preview = if result.len() > 200 {
                    format!("{}...", &result[..200])
                } else {
                    result.clone()
                };
                self.bus.send(AppEvent::PresenceLog {
                    message: format!("{} → {}", tc.name, result_preview),
                    level: Some(LogLevel::Debug),
                });
                self.conversation.add_tool_result(
                    &tc.call_id,
                    &tc.name,
                    &result,
                );
            }
        }

        Ok("I've reached my tool call limit for this request.".to_string())
    }

    /// Execute a presence tool call and return the result string.
    pub async fn handle_presence_tool_call(&mut self, name: &str, args_json: &str) -> String {
        let args: Value = serde_json::from_str(args_json).unwrap_or(json!({}));

        match name {
            "submit_task" => self.handle_submit_task(&args).await,
            "check_status" => self.handle_check_status(),
            "query_detail" => self.handle_query_detail(&args).await,
            "recall_memory" => self.handle_recall_memory(&args),
            "approve_action" => self.handle_approve(&args),
            "deny_action" => self.handle_deny(&args),
            "skip_action" => self.handle_skip(&args),
            "respond_to_question" => self.handle_respond(&args),
            "set_autonomy" => self.handle_set_autonomy(&args),
            _ => format!("Unknown tool: {}", name),
        }
    }

    async fn handle_submit_task(&self, args: &Value) -> String {
        let task = args["task"].as_str().unwrap_or("").to_string();
        if task.is_empty() {
            return "Error: task is required".to_string();
        }
        let force_direct = args["force_direct"].as_bool().unwrap_or(false);
        let context_hints = args["context_hints"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();

        let envelope = TaskEnvelope {
            task: task.clone(),
            force_direct,
            context_hints,
        };

        match self.task_tx.send(envelope).await {
            Ok(()) => {
                self.bus.send(AppEvent::PresenceLog {
                    message: format!("Dispatched task: {}", task),
                    level: None,
                });
                format!("Task submitted: {}", task)
            }
            Err(_) => "Error: task channel closed".to_string(),
        }
    }

    fn handle_check_status(&self) -> String {
        let state = self.agent_state.lock().unwrap_or_else(|e| e.into_inner());
        let s = &*state;
        let mut parts = Vec::new();
        parts.push(format!("Phase: {}", s.phase));
        parts.push(format!("Turn: {}", s.turn));
        parts.push(format!("Budget: {:.0}%", s.budget_pct * 100.0));
        if !s.last_command_preview.is_empty() {
            parts.push(format!("Last command: {}", s.last_command_preview));
        }
        if !s.last_output_summary.is_empty() {
            parts.push(format!("Last output: {}", s.last_output_summary));
        }
        if !s.active_workers.is_empty() {
            parts.push(format!("Workers: {}", s.active_workers.join(", ")));
        }
        parts.join("\n")
    }

    async fn handle_query_detail(&self, args: &Value) -> String {
        let scope = args["scope"].as_str().unwrap_or("current_turn");
        let target = args["target"].as_str();

        match scope {
            "current_turn" => {
                let state = self.agent_state.lock().unwrap_or_else(|e| e.into_inner());
                format!(
                    "Turn: {}\nPhase: {}\nBudget: {:.0}%",
                    state.turn, state.phase, state.budget_pct * 100.0
                )
            }
            "last_output" => {
                let state = self.agent_state.lock().unwrap_or_else(|e| e.into_inner());
                if state.last_output_summary.is_empty() {
                    "No output yet.".to_string()
                } else {
                    state.last_output_summary.clone()
                }
            }
            "worker" => {
                let state = self.agent_state.lock().unwrap_or_else(|e| e.into_inner());
                if state.active_workers.is_empty() {
                    "No active workers.".to_string()
                } else {
                    state
                        .active_workers
                        .iter()
                        .enumerate()
                        .map(|(i, w)| format!("{}. {}", i + 1, w))
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            }
            "diff" => {
                let output = tokio::process::Command::new("git")
                    .args(["diff", "--stat"])
                    .current_dir(&self.project_root)
                    .output()
                    .await;
                match output {
                    Ok(o) => {
                        let stdout = String::from_utf8_lossy(&o.stdout);
                        if stdout.trim().is_empty() {
                            "No changes.".to_string()
                        } else {
                            stdout.to_string()
                        }
                    }
                    Err(e) => format!("Failed to run git diff: {}", e),
                }
            }
            "logs" => {
                let entries = session_log::recent_entries(&self.log_dir, 20);
                if entries.is_empty() {
                    "No log entries yet.".to_string()
                } else {
                    entries.join("\n")
                }
            }
            "file" => {
                let path = match target {
                    Some(p) => p,
                    None => return "Error: target file path is required".to_string(),
                };
                match tokio::fs::read_to_string(path).await {
                    Ok(content) => {
                        let lines: Vec<&str> = content.lines().take(200).collect();
                        lines.join("\n")
                    }
                    Err(e) => format!("Failed to read file: {}", e),
                }
            }
            _ => format!("Unknown scope: {}", scope),
        }
    }

    fn handle_recall_memory(&self, args: &Value) -> String {
        let keywords = args["keywords"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect());
        let tags = args["tags"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect());
        let channel = args["channel"].as_str().map(String::from);

        let query = KnowledgeQuery {
            keywords,
            tags,
            channel,
            ..Default::default()
        };

        match knowledge::load(&self.knowledge_path) {
            Ok(store) => {
                let results = knowledge::query(&store, &query);
                if results.is_empty() {
                    // Fall back to session log search
                    let entries = session_log::recent_entries(&self.log_dir, 100);
                    if let Some(ref kws) = query.keywords {
                        let matched: Vec<&String> = entries
                            .iter()
                            .filter(|e| {
                                let lower = e.to_lowercase();
                                kws.iter().any(|kw| lower.contains(&kw.to_lowercase()))
                            })
                            .take(10)
                            .collect();
                        if matched.is_empty() {
                            "No memories found.".to_string()
                        } else {
                            matched.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n")
                        }
                    } else {
                        "No memories found.".to_string()
                    }
                } else {
                    results
                        .iter()
                        .take(10)
                        .map(|e| format!("[{}] {}: {}", e.channel, e.key, e.summary))
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            }
            Err(e) => format!("Failed to load knowledge: {}", e),
        }
    }

    fn handle_approve(&self, args: &Value) -> String {
        let id = args["id"].as_u64().unwrap_or(0);
        self.bus.send(AppEvent::ControlCommand(ControlMsg::Approve { id }));
        format!("Approved action {}", id)
    }

    fn handle_deny(&self, args: &Value) -> String {
        let id = args["id"].as_u64().unwrap_or(0);
        self.bus.send(AppEvent::ControlCommand(ControlMsg::Deny { id }));
        format!("Denied action {}", id)
    }

    fn handle_skip(&self, args: &Value) -> String {
        let id = args["id"].as_u64().unwrap_or(0);
        self.bus.send(AppEvent::ControlCommand(ControlMsg::Skip { id }));
        format!("Skipped action {}", id)
    }

    fn handle_respond(&self, args: &Value) -> String {
        let text = args["text"].as_str().unwrap_or("").to_string();
        if text.is_empty() {
            return "Error: text is required".to_string();
        }
        self.bus.send(AppEvent::ControlCommand(ControlMsg::Input {
            text: text.clone(),
        }));
        format!("Sent response: {}", text)
    }

    fn handle_set_autonomy(&self, args: &Value) -> String {
        let level = args["level"].as_str().unwrap_or("medium").to_string();
        self.bus.send(AppEvent::ControlCommand(ControlMsg::SetAutonomy {
            level: level.clone(),
        }));
        format!("Autonomy set to {}", level)
    }

    /// Main run loop: listen for events and user input, process both.
    pub async fn run(
        &mut self,
        mut user_rx: mpsc::Receiver<String>,
        response_tx: mpsc::Sender<String>,
    ) {
        loop {
            tokio::select! {
                Some(input) = user_rx.recv() => {
                    match self.process_user_input(&input).await {
                        Ok(response) => {
                            let _ = response_tx.send(response).await;
                        }
                        Err(e) => {
                            let _ = response_tx.send(format!("Error: {}", e)).await;
                        }
                    }
                }
                Some(event) = self.event_rx.recv() => {
                    match self.handle_event(event).await {
                        Ok(Some(narration)) => {
                            let _ = response_tx.send(narration).await;
                        }
                        Ok(None) => {}
                        Err(e) => {
                            let _ = response_tx.send(format!("Error: {}", e)).await;
                        }
                    }
                }
                else => break,
            }
        }
    }
}

/// Filter an AppEvent into a PresenceEvent, returning None for pull-only events.
pub fn filter_event(event: &AppEvent, last_phase: &mut String) -> Option<PresenceEvent> {
    match event {
        AppEvent::ModelResponse { .. } => {
            let new_phase = "thinking".to_string();
            if *last_phase != new_phase {
                *last_phase = new_phase.clone();
                Some(PresenceEvent::PhaseChanged { phase: new_phase })
            } else {
                None
            }
        }
        AppEvent::AgentStarted { commands_preview: _, .. } => {
            let new_phase = "running_agent".to_string();
            let changed = *last_phase != new_phase;
            *last_phase = new_phase.clone();
            if changed {
                Some(PresenceEvent::PhaseChanged { phase: new_phase })
            } else {
                None
            }
        }
        AppEvent::TaskComplete { reason } => {
            *last_phase = "done".to_string();
            Some(PresenceEvent::TaskComplete {
                reason: reason.clone(),
            })
        }
        AppEvent::ApprovalRequired {
            id,
            command_preview,
            category,
            ..
        } => {
            *last_phase = "waiting_approval".to_string();
            Some(PresenceEvent::ApprovalNeeded {
                id: *id,
                preview: command_preview.clone(),
                category: format!("{:?}", category),
            })
        }
        AppEvent::HumanQuestionDetected { question } => {
            *last_phase = "waiting_human".to_string();
            Some(PresenceEvent::HumanQuestion {
                question: question.clone(),
            })
        }
        AppEvent::BudgetWarning { pct, remaining } => Some(PresenceEvent::BudgetWarning {
            pct: *pct,
            remaining: *remaining,
        }),
        AppEvent::RoundComplete {
            round,
            turns_in_round,
        } => Some(PresenceEvent::RoundComplete {
            round: *round,
            turns_in_round: *turns_in_round,
        }),
        AppEvent::LoopError(msg) => Some(PresenceEvent::Error {
            message: msg.clone(),
        }),
        AppEvent::BudgetExhausted { remaining } => {
            *last_phase = "done".to_string();
            Some(PresenceEvent::Error {
                message: format!("Budget exhausted ({} tokens remaining)", remaining),
            })
        }
        AppEvent::SafetyCapReached => {
            *last_phase = "done".to_string();
            Some(PresenceEvent::Error {
                message: "Safety cap reached (500 turns)".to_string(),
            })
        }
        // Pull-only events — not pushed to presence
        AppEvent::AgentOutput { .. }
        | AppEvent::ModelResponseDelta { .. }
        | AppEvent::JsonExtracted { .. }
        | AppEvent::DoneSignal { .. }
        | AppEvent::OrchestratorProgress { .. }
        | AppEvent::AutoApproved { .. }
        | AppEvent::ContextManagement { .. }
        | AppEvent::SubAgentResult { .. }
        | AppEvent::HumanResponseSent
        | AppEvent::TurnStarted { .. }
        | AppEvent::DisplayReady { .. }
        | AppEvent::SessionDirChanged { .. }
        | AppEvent::PresenceUsageUpdate { .. }
        | AppEvent::PresenceLog { .. }
        | AppEvent::ControlCommand(_)
        | AppEvent::Key(_)
        | AppEvent::Resize(_, _)
        | AppEvent::Tick
        | AppEvent::Quit => None,
    }
}

/// Spawn a background task that listens to an EventBus receiver and forwards
/// filtered events to the presence layer, while updating the AgentStateSnapshot.
pub fn spawn_event_listener(
    mut event_rx: mpsc::UnboundedReceiver<AppEvent>,
    presence_tx: mpsc::Sender<PresenceEvent>,
    agent_state: Arc<Mutex<AgentStateSnapshot>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut last_phase = String::new();
        while let Some(event) = event_rx.recv().await {
            // Update agent state snapshot
            update_agent_state(&event, &agent_state);

            // Filter and forward to presence
            if let Some(pe) = filter_event(&event, &mut last_phase) {
                if presence_tx.send(pe).await.is_err() {
                    break; // presence layer dropped
                }
            }
        }
    })
}

fn update_agent_state(event: &AppEvent, state: &Arc<Mutex<AgentStateSnapshot>>) {
    let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
    match event {
        AppEvent::TurnStarted {
            turn, budget_pct, ..
        } => {
            s.turn = *turn;
            s.budget_pct = *budget_pct;
            s.phase = "thinking".to_string();
        }
        AppEvent::ModelResponse { .. } => {
            s.phase = "thinking".to_string();
        }
        AppEvent::AgentStarted {
            commands_preview, ..
        } => {
            s.phase = "running_agent".to_string();
            s.last_command_preview = commands_preview.clone();
        }
        AppEvent::AgentOutput { stdout, stderr } => {
            // Keep a truncated summary
            let combined = if stderr.is_empty() {
                stdout.clone()
            } else {
                format!("{}\n{}", stdout, stderr)
            };
            s.last_output_summary = truncate(&combined, 500);
        }
        AppEvent::TaskComplete { reason } => {
            s.phase = format!("done: {}", reason);
        }
        AppEvent::RoundComplete { .. } => {
            s.phase = "waiting_followup".to_string();
        }
        AppEvent::ApprovalRequired { .. } => {
            s.phase = "waiting_approval".to_string();
        }
        AppEvent::HumanQuestionDetected { .. } => {
            s.phase = "waiting_human".to_string();
        }
        AppEvent::OrchestratorProgress { status, .. } => {
            s.phase = format!("orchestrating: {}", status);
        }
        AppEvent::LoopError(msg) => {
            s.phase = format!("error: {}", msg);
        }
        _ => {}
    }
}

/// Update the agent state snapshot from a PresenceEvent (used by the TUI-side
/// forwarder to keep the snapshot in sync without needing the EventBus receiver).
pub fn update_agent_state_from_presence_event(
    event: &PresenceEvent,
    state: &Arc<Mutex<AgentStateSnapshot>>,
) {
    let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
    match event {
        PresenceEvent::PhaseChanged { phase } => {
            s.phase = phase.clone();
        }
        PresenceEvent::TaskComplete { reason } => {
            s.phase = format!("done: {}", reason);
        }
        PresenceEvent::ApprovalNeeded { preview, .. } => {
            s.phase = "waiting_approval".to_string();
            s.last_command_preview = preview.clone();
        }
        PresenceEvent::HumanQuestion { .. } => {
            s.phase = "waiting_human".to_string();
        }
        PresenceEvent::BudgetWarning { pct, .. } => {
            s.budget_pct = *pct;
        }
        PresenceEvent::RoundComplete { .. } => {
            s.phase = "waiting_followup".to_string();
        }
        PresenceEvent::Error { .. } => {
            s.phase = "error".to_string();
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max])
    }
}

#[allow(dead_code)]
fn format_event(event: &PresenceEvent) -> String {
    match event {
        PresenceEvent::PhaseChanged { phase } => format!("Phase changed to: {}", phase),
        PresenceEvent::TaskComplete { reason } => format!("Task complete: {}", reason),
        PresenceEvent::ApprovalNeeded {
            id,
            preview,
            category,
        } => format!(
            "Approval needed (id={}, category={}): {}",
            id, category, preview
        ),
        PresenceEvent::HumanQuestion { question } => {
            format!("Worker has a question: {}", question)
        }
        PresenceEvent::BudgetWarning { pct, remaining } => {
            format!(
                "Budget warning: {:.0}% used, {} tokens remaining",
                pct * 100.0,
                remaining
            )
        }
        PresenceEvent::RoundComplete {
            round,
            turns_in_round,
        } => format!("Round {} complete ({} turns)", round, turns_in_round),
        PresenceEvent::Error { message } => format!("Error: {}", message),
    }
}

/// Return the 9 presence tool definitions for native tool calling.
pub fn presence_tools() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "submit_task".to_string(),
            description: "Submit a coding task for workers to execute. Use for any multi-step work like implementing features, fixing bugs, running tests, or research.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "The task description."
                    },
                    "force_direct": {
                        "type": "boolean",
                        "description": "Force single-agent mode (no orchestrator). Default false."
                    },
                    "context_hints": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional hints to inject into the worker's context."
                    }
                },
                "required": ["task"]
            }),
        },
        ToolDefinition {
            name: "check_status".to_string(),
            description: "Check current agent status: phase, turn, budget, last command, workers."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        },
        ToolDefinition {
            name: "query_detail".to_string(),
            description: "Query detailed information. Scopes: current_turn, last_output, worker, diff, logs, file.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "scope": {
                        "type": "string",
                        "enum": ["current_turn", "last_output", "worker", "diff", "logs", "file"],
                        "description": "What to query."
                    },
                    "target": {
                        "type": "string",
                        "description": "Target path (required for 'file' scope)."
                    }
                },
                "required": ["scope"]
            }),
        },
        ToolDefinition {
            name: "recall_memory".to_string(),
            description:
                "Search knowledge store and session logs for past context, decisions, and findings."
                    .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "keywords": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Keywords to search for."
                    },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Filter by tags."
                    },
                    "channel": {
                        "type": "string",
                        "description": "Filter by knowledge channel."
                    }
                },
                "required": []
            }),
        },
        ToolDefinition {
            name: "approve_action".to_string(),
            description: "Approve a pending action that requires user consent.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "integer",
                        "description": "The approval ID."
                    }
                },
                "required": ["id"]
            }),
        },
        ToolDefinition {
            name: "deny_action".to_string(),
            description: "Deny a pending action, stopping the current command.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "integer",
                        "description": "The approval ID."
                    }
                },
                "required": ["id"]
            }),
        },
        ToolDefinition {
            name: "skip_action".to_string(),
            description: "Skip a pending action, continuing with the next command.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "integer",
                        "description": "The approval ID."
                    }
                },
                "required": ["id"]
            }),
        },
        ToolDefinition {
            name: "respond_to_question".to_string(),
            description: "Respond to a question from the worker (askHuman).".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "The response text."
                    }
                },
                "required": ["text"]
            }),
        },
        ToolDefinition {
            name: "set_autonomy".to_string(),
            description: "Set the autonomy level: low (ask for everything), medium (ask for writes/deletes), high (ask for destructive only), full (no approval needed).".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "level": {
                        "type": "string",
                        "enum": ["low", "medium", "high", "full"],
                        "description": "The autonomy level."
                    }
                },
                "required": ["level"]
            }),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider;

    #[test]
    fn presence_config_defaults() {
        let config = PresenceConfig::default();
        assert!(config.enabled);
        assert!(config.provider.is_none());
        assert!(config.model.is_none());
        assert!(config.live_provider.is_none());
        assert!(config.live_model.is_none());
        assert_eq!(config.context_window, 1_048_576);
        assert_eq!(config.live_context_window, 32_768);
    }

    #[test]
    fn presence_config_deserialize() {
        let toml_str = r#"
            enabled = true
            provider = "gemini"
            model = "gemini-3.0-flash"
            context_window = 1048576
            live_provider = "gemini"
            live_model = "gemini-2.5-flash-native-audio-preview-12-2025"
            live_context_window = 32768
        "#;
        let config: PresenceConfig = toml::from_str(toml_str).unwrap();
        assert!(config.enabled);
        assert_eq!(config.provider.as_deref(), Some("gemini"));
        assert_eq!(config.model.as_deref(), Some("gemini-3.0-flash"));
        assert_eq!(config.context_window, 1_048_576);
        assert_eq!(config.live_provider.as_deref(), Some("gemini"));
        assert_eq!(
            config.live_model.as_deref(),
            Some("gemini-2.5-flash-native-audio-preview-12-2025")
        );
        assert_eq!(config.live_context_window, 32_768);
    }

    #[test]
    fn task_envelope_roundtrip() {
        let envelope = TaskEnvelope {
            task: "fix the tests".to_string(),
            force_direct: true,
            context_hints: vec!["check src/main.rs".to_string()],
        };
        let json = serde_json::to_string(&envelope).unwrap();
        let parsed: TaskEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.task, "fix the tests");
        assert!(parsed.force_direct);
        assert_eq!(parsed.context_hints.len(), 1);
    }

    #[test]
    fn filter_event_push_events() {
        let mut last_phase = String::new();

        // TaskComplete → push
        let event = AppEvent::TaskComplete {
            reason: "done".to_string(),
        };
        assert!(filter_event(&event, &mut last_phase).is_some());

        // BudgetWarning → push
        let event = AppEvent::BudgetWarning {
            pct: 0.9,
            remaining: 1000,
        };
        assert!(filter_event(&event, &mut last_phase).is_some());

        // RoundComplete → push
        let event = AppEvent::RoundComplete {
            round: 1,
            turns_in_round: 5,
        };
        assert!(filter_event(&event, &mut last_phase).is_some());

        // LoopError → push
        let event = AppEvent::LoopError("oops".to_string());
        assert!(filter_event(&event, &mut last_phase).is_some());
    }

    #[test]
    fn filter_event_pull_only() {
        let mut last_phase = String::new();

        // AgentOutput → pull only
        let event = AppEvent::AgentOutput {
            stdout: "hello".to_string(),
            stderr: String::new(),
        };
        assert!(filter_event(&event, &mut last_phase).is_none());

        // Tick → pull only
        assert!(filter_event(&AppEvent::Tick, &mut last_phase).is_none());

        // ModelResponseDelta → pull only
        let event = AppEvent::ModelResponseDelta {
            text: "hi".to_string(),
        };
        assert!(filter_event(&event, &mut last_phase).is_none());
    }

    #[test]
    fn filter_event_phase_change_dedup() {
        let mut last_phase = String::new();

        // First ModelResponse → phase change
        let event = AppEvent::ModelResponse {
            turn: 1,
            content: "hi".to_string(),
            usage: provider::TokenUsage::default(),
            reasoning: None,
        };
        assert!(filter_event(&event, &mut last_phase).is_some());
        assert_eq!(last_phase, "thinking");

        // Second ModelResponse → same phase, no event
        assert!(filter_event(&event, &mut last_phase).is_none());
    }

    #[test]
    fn agent_state_snapshot_defaults() {
        let state = AgentStateSnapshot::default();
        assert!(state.phase.is_empty());
        assert_eq!(state.turn, 0);
        assert_eq!(state.budget_pct, 0.0);
        assert!(state.last_output_summary.is_empty());
        assert!(state.last_command_preview.is_empty());
        assert!(state.active_workers.is_empty());
    }

    #[test]
    fn agent_state_update_from_events() {
        let state = Arc::new(Mutex::new(AgentStateSnapshot::default()));

        update_agent_state(
            &AppEvent::TurnStarted {
                turn: 5,
                budget_pct: 0.42,
                remaining: 50_000,
            },
            &state,
        );
        {
            let s = state.lock().unwrap();
            assert_eq!(s.turn, 5);
            assert_eq!(s.budget_pct, 0.42);
            assert_eq!(s.phase, "thinking");
        }

        update_agent_state(
            &AppEvent::AgentStarted {
                turn: 5,
                commands_preview: "echo hello".to_string(),
            },
            &state,
        );
        {
            let s = state.lock().unwrap();
            assert_eq!(s.phase, "running_agent");
            assert_eq!(s.last_command_preview, "echo hello");
        }

        update_agent_state(
            &AppEvent::AgentOutput {
                stdout: "hello world".to_string(),
                stderr: String::new(),
            },
            &state,
        );
        {
            let s = state.lock().unwrap();
            assert_eq!(s.last_output_summary, "hello world");
        }

        update_agent_state(
            &AppEvent::TaskComplete {
                reason: "done_signal".to_string(),
            },
            &state,
        );
        {
            let s = state.lock().unwrap();
            assert_eq!(s.phase, "done: done_signal");
        }
    }

    #[test]
    fn presence_tools_count_and_names() {
        let tools = presence_tools();
        assert_eq!(tools.len(), 9);

        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"submit_task"));
        assert!(names.contains(&"check_status"));
        assert!(names.contains(&"query_detail"));
        assert!(names.contains(&"recall_memory"));
        assert!(names.contains(&"approve_action"));
        assert!(names.contains(&"deny_action"));
        assert!(names.contains(&"skip_action"));
        assert!(names.contains(&"respond_to_question"));
        assert!(names.contains(&"set_autonomy"));
    }

    #[test]
    fn format_event_variants() {
        let s = format_event(&PresenceEvent::PhaseChanged {
            phase: "thinking".to_string(),
        });
        assert!(s.contains("thinking"));

        let s = format_event(&PresenceEvent::TaskComplete {
            reason: "done".to_string(),
        });
        assert!(s.contains("done"));

        let s = format_event(&PresenceEvent::Error {
            message: "oops".to_string(),
        });
        assert!(s.contains("oops"));
    }

    #[test]
    fn truncate_short_string() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_long_string() {
        let long = "a".repeat(100);
        let result = truncate(&long, 10);
        assert_eq!(result.len(), 13); // 10 + "..."
        assert!(result.ends_with("..."));
    }
}
