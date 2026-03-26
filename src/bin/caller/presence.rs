use crate::conversation::Conversation;
use crate::error::CallerError;
use crate::knowledge::{self, KnowledgeQuery};
use crate::provider::ChatProvider;
use crate::session_log;
use crate::event::{AppEvent, ControlMsg, EventBus};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

// Re-export types from presence-core so existing callers don't break.
#[allow(unused_imports)]
pub use presence_core::{
    AgentStateSnapshot, PresenceConfig, PresenceEvent, PresenceUsage,
    TaskEnvelope, DEFAULT_TEXT_MODEL, NARRATION_DEBOUNCE_MS, PRESENCE_TURN_OFFSET,
    PREFERRED_TEXT_MODEL, DEFAULT_TEXT_PROVIDER,
    format_event, truncate,
    PresenceAction, dispatch_tool_call,
    PresenceConnect, PresenceWelcome, PresenceCheckpoint, PresenceCheckpointAck,
    PresenceEventWindow, SequencedPresenceEvent, VoiceLog,
};

/// Convert a `PresenceAction` to a `(ControlMsg, confirmation_text)` pair.
/// Returns `None` for `TextResult` and `NeedsIO` which need separate handling.
pub fn action_to_control_msg(action: &PresenceAction) -> Option<(ControlMsg, String)> {
    let confirmation = presence_core::action_confirmation(action);
    match action {
        PresenceAction::SubmitTask(envelope) => {
            let orchestrate = if envelope.force_direct { Some(false) } else { None };
            Some((
                ControlMsg::StartTask {
                    task: envelope.task.clone(),
                    orchestrate,
                    reference_frame_ids: envelope.reference_frame_ids.clone(),
                    display_target: envelope.display_target.clone(),
                },
                confirmation,
            ))
        }
        PresenceAction::Approve { id } => {
            Some((ControlMsg::Approve { id: *id }, confirmation))
        }
        PresenceAction::Deny { id } => {
            Some((ControlMsg::Deny { id: *id }, confirmation))
        }
        PresenceAction::Skip { id } => {
            Some((ControlMsg::Skip { id: *id }, confirmation))
        }
        PresenceAction::Respond { text } => {
            Some((ControlMsg::Input { text: text.clone() }, confirmation))
        }
        PresenceAction::SetAutonomy { level } => {
            Some((ControlMsg::SetAutonomy { level: level.clone() }, confirmation))
        }
        PresenceAction::TextResult(_) | PresenceAction::NeedsIO { .. } => None,
    }
}

// Re-export presence_tools with conversion to the main crate's ToolDefinition.
pub fn presence_tools() -> Vec<crate::tools::ToolDefinition> {
    presence_core::presence_tools()
        .into_iter()
        .map(|t| crate::tools::ToolDefinition {
            name: t.name,
            description: t.description,
            parameters: t.parameters,
        })
        .collect()
}

/// Debounce duration for phase-change narrations (from presence-core constant).
const NARRATION_DEBOUNCE: std::time::Duration =
    std::time::Duration::from_millis(NARRATION_DEBOUNCE_MS);

/// The running presence layer instance (platform-specific, uses tokio + ChatProvider).
pub struct PresenceLayer {
    provider: Box<dyn ChatProvider>,
    conversation: Conversation,
    bus: EventBus,
    /// Channel to submit tasks to the agent loop.
    task_tx: mpsc::Sender<TaskEnvelope>,
    /// Channel to receive filtered events from the agent loop.
    event_rx: mpsc::Receiver<PresenceEvent>,
    /// Shared agent state snapshot, updated by the event listener.
    agent_state: Arc<Mutex<AgentStateSnapshot>>,
    /// Path to the knowledge store.
    knowledge_path: PathBuf,
    /// Session log directory for query_detail.
    log_dir: PathBuf,
    /// Project root for file reads and git operations.
    project_root: PathBuf,
    /// Presence interaction turn counter (incremented per process_user_input / handle_event).
    turn: usize,
    /// Timestamp of the last phase-change narration (for debounce).
    last_narration_at: std::time::Instant,
    /// When > 0, the presence layer is paused (one or more browser live models active).
    /// All events and user input are silently dropped — the browser live model
    /// IS the presence and dispatches tasks directly via task_tx.
    paused: Arc<AtomicUsize>,
    /// Shared context injection queue for mid-task interjections into the agent loop.
    context_injection: crate::event::ContextInjectionQueue,
    /// Cumulative prompt/completion/cached tokens across all presence turns.
    cumulative_prompt: u64,
    cumulative_completion: u64,
    cumulative_cached: u64,
}

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
        paused: Arc<AtomicUsize>,
        context_injection: crate::event::ContextInjectionQueue,
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
            turn: 0,
            last_narration_at: std::time::Instant::now() - NARRATION_DEBOUNCE,
            paused,
            context_injection,
            cumulative_prompt: 0,
            cumulative_completion: 0,
            cumulative_cached: 0,
        }
    }

    /// Return a shared handle to the paused flag for external pause/resume control.
    pub fn paused_flag(&self) -> Arc<AtomicUsize> {
        self.paused.clone()
    }

    /// Check if the presence layer is currently paused (any browser has active voice).
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed) > 0
    }

    /// Process text input from the user, returning the model's response.
    /// When paused (browser live model active), input is dropped — the browser
    /// live model dispatches tasks directly via task_tx.
    pub async fn process_user_input(&mut self, input: &str) -> Result<String, CallerError> {
        if self.is_paused() {
            return Ok(String::new());
        }
        self.turn += 1;
        self.conversation.add_user(input.to_string());
        let result = self.run_model_loop().await;
        self.emit_usage_update();
        result
    }

    /// Return current token usage stats for the presence conversation.
    pub fn usage_snapshot(&self) -> PresenceUsage {
        let last = self.conversation.last_usage();
        PresenceUsage {
            total_tokens: last.map(|u| u.total_tokens).unwrap_or(0),
            context_window: self.conversation.context_window(),
            usage_pct: self.conversation.usage_fraction() * 100.0,
            provider: self.provider.name().to_string(),
            model: self.provider.model().to_string(),
            prompt_tokens: self.cumulative_prompt,
            completion_tokens: self.cumulative_completion,
            cached_tokens: self.cumulative_cached,
        }
    }

    fn plog(&self, message: String, level: Option<crate::types::LogLevel>) {
        self.bus.send(AppEvent::PresenceLog {
            message: format!("[model] {}", message),
            level,
            turn: Some(PRESENCE_TURN_OFFSET + self.turn),
        });
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
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            cached_tokens: usage.cached_tokens,
        });
    }

    /// Inject a PresenceEvent into the conversation and let the model narrate.
    pub async fn handle_event(&mut self, event: PresenceEvent) -> Result<Option<String>, CallerError> {
        // When paused (browser voice model active), skip narration
        if self.is_paused() {
            return Ok(None);
        }
        // Debounce phase-change narrations
        if matches!(event, PresenceEvent::PhaseChanged { .. }) {
            let now = std::time::Instant::now();
            if now.duration_since(self.last_narration_at) < NARRATION_DEBOUNCE {
                return Ok(None);
            }
            self.last_narration_at = now;
        }
        self.turn += 1;
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
        use crate::types::LogLevel;
        const MAX_TOOL_ROUNDS: usize = 10;

        for round in 0..MAX_TOOL_ROUNDS {
            self.plog(
                if round == 0 {
                    format!("Thinking ({})...", self.provider.model())
                } else {
                    format!("Thinking (tool round {})...", round + 1)
                },
                Some(crate::types::LogLevel::Detail),
            );

            let messages = self.conversation.messages().to_vec();
            let response = self.provider.chat(&messages).await?;

            self.plog(
                format!(
                    "Tokens: {} prompt + {} completion = {} total",
                    response.usage.prompt_tokens,
                    response.usage.completion_tokens,
                    response.usage.total_tokens,
                ),
                Some(LogLevel::Debug),
            );

            self.cumulative_prompt += response.usage.prompt_tokens;
            self.cumulative_completion += response.usage.completion_tokens;
            self.cumulative_cached += response.usage.cached_tokens;
            self.conversation.set_usage(response.usage.clone());
            self.conversation.auto_compact();

            if response.tool_calls.is_empty() {
                if !response.content.is_empty() {
                    self.conversation.add_assistant(response.content.clone());
                }
                return Ok(response.content);
            }

            // Has tool calls — process them
            let tool_names: Vec<&str> = response.tool_calls.iter().map(|tc| tc.name.as_str()).collect();
            self.plog(format!("Tool call: {}", tool_names.join(", ")), Some(crate::types::LogLevel::Detail));

            if !response.content.is_empty() {
                self.plog(format!("Model text: {}", response.content), Some(LogLevel::Agent));
            }

            for tc in &response.tool_calls {
                self.plog(format!("{}({})", tc.name, tc.arguments), Some(LogLevel::Debug));
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
                let result_preview = if result.len() > 200 {
                    format!("{}...", &result[..200])
                } else {
                    result.clone()
                };
                self.plog(format!("{} → {}", tc.name, result_preview), Some(LogLevel::Debug));
                self.conversation.add_tool_result(
                    &tc.call_id,
                    &tc.name,
                    &result,
                );
            }
        }

        Ok("I've reached my tool call limit for this request.".to_string())
    }

    /// Execute a presence tool call using presence-core dispatch + platform I/O.
    pub async fn handle_presence_tool_call(&mut self, name: &str, args_json: &str) -> String {
        let args: Value = serde_json::from_str(args_json).unwrap_or(json!({}));
        if name == "submit_task" {
            self.plog(
                format!("[debug] submit_task args: {}", &args_json[..args_json.len().min(500)]),
                None,
            );
        }
        let state_snapshot = self.agent_state.lock().unwrap_or_else(|e| e.into_inner()).clone();

        let action = dispatch_tool_call(name, &args, &state_snapshot);

        // SubmitTask is special: it uses the dedicated task channel (preserves
        // the full TaskEnvelope including force_direct and context_hints).
        if let PresenceAction::SubmitTask(envelope) = action {
            let task = envelope.task.clone();
            return match self.task_tx.send(envelope).await {
                Ok(()) => {
                    self.plog(format!("Dispatched task: {}", task), None);
                    format!("Task submitted: {}", task)
                }
                Err(_) => "Error: task channel closed".to_string(),
            };
        }

        // Action variants → ControlMsg via canonical helper.
        if let Some((ctrl, msg)) = action_to_control_msg(&action) {
            self.bus.send(AppEvent::ControlCommand(ctrl));
            return msg;
        }

        // Remaining: TextResult, NeedsIO.
        match action {
            PresenceAction::TextResult(text) => text,
            PresenceAction::NeedsIO { tool_name, args } => {
                match tool_name.as_str() {
                    "query_detail" => self.handle_query_detail(&args).await,
                    "recall_memory" => self.handle_recall_memory(&args),
                    "send_message" => {
                        let msg = args["message"].as_str().unwrap_or("").to_string();
                        if msg.is_empty() {
                            "Error: message is required".to_string()
                        } else {
                            if let Ok(mut q) = self.context_injection.lock() {
                                q.push(crate::event::ContextInjection::text(
                                    format!("[Presence] {}", msg),
                                ));
                            }
                            format!("Message injected: {}", msg)
                        }
                    }
                    "inspect_frame" | "inspect_frames" => {
                        "Frame inspection is only available in live video mode (browser).".to_string()
                    }
                    _ => format!("Unknown IO tool: {}", tool_name),
                }
            }
            _ => unreachable!(), // SubmitTask and action variants handled above
        }
    }

    async fn handle_query_detail(&self, args: &Value) -> String {
        query_detail(&self.agent_state, &self.project_root, &self.log_dir, args).await
    }

    fn handle_recall_memory(&self, args: &Value) -> String {
        recall_memory(&self.knowledge_path, &self.log_dir, args)
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

// ── Standalone query functions (shared by PresenceLayer and WebSocket gateway) ──

/// Handle a `query_detail` tool call: returns agent state, git diff, logs, or file content.
pub async fn query_detail(
    agent_state: &Arc<Mutex<AgentStateSnapshot>>,
    project_root: &std::path::Path,
    log_dir: &std::path::Path,
    args: &Value,
) -> String {
    let scope = args["scope"].as_str().unwrap_or("current_turn");
    let target = args["target"].as_str();

    match scope {
        "current_turn" => {
            let state = agent_state.lock().unwrap_or_else(|e| e.into_inner());
            format!(
                "Turn: {}\nPhase: {}\nBudget: {:.0}%",
                state.turn, state.phase, state.budget_pct * 100.0
            )
        }
        "last_output" => {
            let state = agent_state.lock().unwrap_or_else(|e| e.into_inner());
            if state.last_output_summary.is_empty() {
                "No output yet.".to_string()
            } else {
                state.last_output_summary.clone()
            }
        }
        "worker" => {
            let state = agent_state.lock().unwrap_or_else(|e| e.into_inner());
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
                .current_dir(project_root)
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
            let entries = session_log::recent_entries(log_dir, 20);
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
        "task_result" => {
            let state = agent_state.lock().unwrap_or_else(|e| e.into_inner());
            match &state.last_task_result {
                Some(result) => result.clone(),
                None => "No task result available.".to_string(),
            }
        }
        _ => format!("Unknown scope: {}", scope),
    }
}

/// Handle a `recall_memory` tool call: query knowledge store AND voice transcripts.
/// Results from both sources are merged and returned.
pub fn recall_memory(
    knowledge_path: &std::path::Path,
    log_dir: &std::path::Path,
    args: &Value,
) -> String {
    let keywords: Option<Vec<String>> = args["keywords"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect());
    let tags = args["tags"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect());
    let channel = args["channel"].as_str().map(String::from);

    let query = KnowledgeQuery {
        keywords: keywords.clone(),
        tags,
        channel,
        ..Default::default()
    };

    let mut sections: Vec<String> = Vec::new();

    // Source 1: Knowledge store
    match knowledge::load(knowledge_path) {
        Ok(store) => {
            let results = knowledge::query(&store, &query);
            if !results.is_empty() {
                let formatted: Vec<String> = results
                    .iter()
                    .take(10)
                    .map(|e| format!("[{}] {}: {}", e.channel, e.key, e.summary))
                    .collect();
                sections.push(formatted.join("\n"));
            }
        }
        Err(_) => {}
    }

    // Source 2: Voice transcript search
    if let Some(ref kws) = keywords {
        let voice_results = session_log::search_voice_entries(log_dir, kws, 5);
        if !voice_results.is_empty() {
            sections.push(format!(
                "--- Conversation history ---\n{}",
                voice_results.join("\n")
            ));
        }
    }

    // Source 3: Fall back to raw session log if nothing found
    if sections.is_empty() {
        let entries = session_log::recent_entries(log_dir, 100);
        if let Some(ref kws) = keywords {
            let matched: Vec<&String> = entries
                .iter()
                .filter(|e| {
                    let lower = e.to_lowercase();
                    kws.iter().any(|kw| lower.contains(&kw.to_lowercase()))
                })
                .take(10)
                .collect();
            if !matched.is_empty() {
                sections.push(
                    matched.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n"),
                );
            }
        }
    }

    if sections.is_empty() {
        "No memories found.".to_string()
    } else {
        sections.join("\n\n")
    }
}

/// Build a formatted conversation context string from recent voice_log and
/// user_transcript entries. Returns `None` if no conversation turns are found.
pub fn build_conversation_context(log_dir: &std::path::Path, max_turns: usize) -> Option<String> {
    let turns = session_log::recent_conversation(log_dir, max_turns);
    if turns.is_empty() {
        return None;
    }
    let lines: Vec<String> = turns
        .iter()
        .map(|t| {
            let role = if t.role == "user" { "User" } else { "Model" };
            format!("{}: {}", role, t.text)
        })
        .collect();
    Some(lines.join("\n"))
}

/// Result of a tool query, containing text and optional images.
pub struct ToolQueryResult {
    pub text: String,
    pub images: Vec<crate::conversation::ImageData>,
}

impl ToolQueryResult {
    pub fn text(s: String) -> Self {
        Self { text: s, images: vec![] }
    }
}

/// Handle a tool query by name. Used by both the server-side PresenceLayer and
/// the WebSocket gateway for browser-side live model tool requests.
///
/// For action tools (approve, deny, submit_task, etc.), the caller must handle
/// dispatch separately — this function only handles read-only query tools.
pub async fn handle_tool_query(
    agent_state: &Arc<Mutex<AgentStateSnapshot>>,
    project_root: &std::path::Path,
    log_dir: &std::path::Path,
    knowledge_path: &std::path::Path,
    tool_name: &str,
    args: &Value,
    frame_registry: Option<&Arc<tokio::sync::RwLock<crate::frames::FrameRegistry>>>,
    context_injection: Option<&crate::event::ContextInjectionQueue>,
) -> Option<ToolQueryResult> {
    match tool_name {
        "check_status" => {
            let state = agent_state.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let action = dispatch_tool_call("check_status", args, &state);
            match action {
                PresenceAction::TextResult(text) => Some(ToolQueryResult::text(text)),
                _ => None,
            }
        }
        "query_detail" => {
            Some(ToolQueryResult::text(query_detail(agent_state, project_root, log_dir, args).await))
        }
        "recall_memory" => {
            Some(ToolQueryResult::text(recall_memory(knowledge_path, log_dir, args)))
        }
        "send_message" => {
            let msg = args["message"].as_str().unwrap_or("").to_string();
            if msg.is_empty() {
                return Some(ToolQueryResult::text("Error: message is required".to_string()));
            }
            // Resolve optional frame_ids to HQ images
            let mut images = Vec::new();
            if let Some(fids) = args["frame_ids"].as_array() {
                if let Some(reg) = frame_registry {
                    let reg = reg.read().await;
                    for fid_val in fids {
                        if let Some(fid) = fid_val.as_str() {
                            if let Ok(data) = reg.read_hq(fid) {
                                use base64::Engine;
                                images.push(crate::conversation::ImageData {
                                    media_type: "image/jpeg".to_string(),
                                    data: base64::engine::general_purpose::STANDARD.encode(&data),
                                });
                            }
                        }
                    }
                }
            }
            if let Some(q) = context_injection {
                if let Ok(mut q) = q.lock() {
                    q.push(crate::event::ContextInjection {
                        text: format!("[Presence] {}", msg),
                        images,
                    });
                }
            }
            Some(ToolQueryResult::text(format!("Message injected: {}", msg)))
        }
        "inspect_frame" => {
            let reg = frame_registry?;
            let reg = reg.read().await;
            let frame_id = args["frame_id"].as_str();
            let fid = match frame_id {
                Some(id) => id.to_string(),
                None => reg.latest(None)?.to_string(),
            };
            if let Some(meta) = reg.get(&fid) {
                // Read HQ image and return alongside metadata
                let mut images = Vec::new();
                if let Ok(data) = reg.read_hq(&fid) {
                    use base64::Engine;
                    images.push(crate::conversation::ImageData {
                        media_type: "image/jpeg".to_string(),
                        data: base64::engine::general_purpose::STANDARD.encode(&data),
                    });
                }
                Some(ToolQueryResult {
                    text: format!(
                        "Frame {} | stream={} | ts={} | hq_resolution={}",
                        meta.frame_id,
                        meta.stream,
                        meta.timestamp,
                        meta.hq_resolution.as_deref().unwrap_or("unknown"),
                    ),
                    images,
                })
            } else {
                Some(ToolQueryResult::text(format!("Frame {} not found in registry.", fid)))
            }
        }
        "inspect_frames" => {
            let reg = frame_registry?;
            let reg = reg.read().await;
            let query = args["query"].as_str().unwrap_or("");
            let count = args["count"].as_u64().unwrap_or(10) as usize;

            // Parse query: if it matches a stream name, filter by stream; otherwise return recent frames.
            let stream_filter = if query.starts_with("cam") || query.starts_with("display:") || query.starts_with("d") {
                Some(query)
            } else {
                None
            };

            let frames = reg.query(stream_filter, count);
            Some(ToolQueryResult::text(crate::frames::FrameRegistry::format_frame_list(&frames)))
        }
        _ => None,
    }
}

/// Filter an AppEvent into a PresenceEvent, returning None for pull-only events.
///
/// This function stays in the main crate because it depends on the AppEvent enum
/// which is tied to the TUI event system. The WASM client will deserialize
/// PresenceEvents directly from WebSocket JSON instead.
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
        AppEvent::TaskComplete { reason, summary } => {
            *last_phase = "done".to_string();
            Some(PresenceEvent::TaskComplete {
                reason: reason.clone(),
                summary: summary.clone(),
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
        AppEvent::ApprovalResolved { id, action } => {
            if action == "deny" {
                *last_phase = "done".to_string();
            } else {
                *last_phase = "running_agent".to_string();
            }
            Some(PresenceEvent::ApprovalResolved {
                id: *id,
                action: action.clone(),
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
        // Display lifecycle events — not pushed to presence (avoids unnecessary
        // inference calls). Display state is available via check_status when the
        // model needs it for routing decisions.
        AppEvent::DisplayReady { .. }
        | AppEvent::UserDisplayGranted
        | AppEvent::UserDisplayRevoked { .. }

        // Pull-only events — not pushed to presence
        | AppEvent::AgentOutput { .. }
        | AppEvent::ModelResponseDelta { .. }
        | AppEvent::JsonExtracted { .. }
        | AppEvent::DoneSignal { .. }
        | AppEvent::OrchestratorProgress { .. }
        | AppEvent::OrchestratorLog { .. }
        | AppEvent::AutoApproved { .. }
        | AppEvent::ContextManagement { .. }
        | AppEvent::SubAgentResult { .. }
        | AppEvent::HumanResponseSent
        | AppEvent::TurnStarted { .. }
        | AppEvent::DisplayTaken { .. }
        | AppEvent::DisplayReleased { .. }
        | AppEvent::SessionDirChanged { .. }
        | AppEvent::PresenceUsageUpdate { .. }
        | AppEvent::PresenceLog { .. }
        | AppEvent::PresenceReady
        | AppEvent::PresenceConnected { .. }
        | AppEvent::PresenceDisconnected
        | AppEvent::VoiceLog { .. }
        | AppEvent::PresenceCheckpointReceived { .. }
        | AppEvent::VoiceDiagnostic { .. }
        | AppEvent::UserTranscript { .. }
        | AppEvent::UsageSnapshot { .. }
        | AppEvent::LiveUsageUpdate { .. }
        | AppEvent::StatusUpdate { .. }
        | AppEvent::LogEntry { .. }
        | AppEvent::ControlCommand(_)
        | AppEvent::Key(_)
        | AppEvent::Resize(_, _)
        | AppEvent::Tick
        | AppEvent::Quit
        | AppEvent::RecordingStarted { .. }
        | AppEvent::RecordingStopped { .. }
        | AppEvent::RecordingError { .. }
        | AppEvent::RecordingDeleted { .. }
        | AppEvent::SessionStarted { .. }
        | AppEvent::SessionEnded { .. }
        | AppEvent::DebugScreenReady { .. }
        | AppEvent::DebugScreenTornDown { .. }
        | AppEvent::LiveAudioStarted { .. }
        | AppEvent::LiveAudioProgress { .. }
        | AppEvent::LiveAudioCompleted { .. } => None,
    }
}

// ── PresenceSession: server-authoritative presence state ──

/// Checkpoint state recorded from a browser presence model.
#[derive(Debug, Clone)]
pub struct CheckpointState {
    pub summary: String,
    pub last_event_seq: u64,
}

/// Server-side presence session state.
/// Tracks the event replay window and checkpoint state for browser reconnection.
pub struct PresenceSession {
    session_id: String,
    event_window: PresenceEventWindow,
    last_checkpoint: Option<CheckpointState>,
    connected_count: usize,
}

impl PresenceSession {
    /// Create a new presence session.
    pub fn new(session_id: String) -> Self {
        Self {
            session_id,
            event_window: PresenceEventWindow::default(),
            last_checkpoint: None,
            connected_count: 0,
        }
    }

    /// Record a presence event into the replay window. Returns the assigned seq.
    pub fn record_event(&mut self, event: PresenceEvent) -> u64 {
        self.event_window.push(event)
    }

    /// Build a welcome message for a connecting presence client.
    /// Replays all events since `last_event_seq` from the window.
    pub fn build_welcome(
        &self,
        last_event_seq: u64,
        state: &AgentStateSnapshot,
    ) -> PresenceWelcome {
        PresenceWelcome {
            session_id: self.session_id.clone(),
            state: state.clone(),
            events: self.event_window.since(last_event_seq),
            last_checkpoint_summary: self.last_checkpoint.as_ref().map(|c| c.summary.clone()),
            current_seq: self.event_window.current_seq(),
        }
    }

    /// Record a checkpoint from the browser presence model.
    pub fn record_checkpoint(&mut self, checkpoint: PresenceCheckpoint) -> PresenceCheckpointAck {
        let seq = checkpoint.last_event_seq;
        self.last_checkpoint = Some(CheckpointState {
            summary: checkpoint.summary,
            last_event_seq: checkpoint.last_event_seq,
        });
        PresenceCheckpointAck { seq }
    }

    pub fn set_connected(&mut self, connected: bool) {
        if connected {
            self.connected_count += 1;
        } else {
            self.connected_count = self.connected_count.saturating_sub(1);
        }
    }

    pub fn is_connected(&self) -> bool {
        self.connected_count > 0
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Get the last checkpoint summary, if any.
    pub fn last_checkpoint_summary(&self) -> Option<String> {
        self.last_checkpoint.as_ref().map(|c| c.summary.clone())
    }
}

pub fn update_agent_state(event: &AppEvent, state: &Arc<Mutex<AgentStateSnapshot>>) {
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
            s.pending_approval = None;
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
        AppEvent::TaskComplete { reason, summary } => {
            s.phase = format!("done: {}", reason);
            if let Some(text) = summary {
                s.last_task_result = Some(text.clone());
            }
        }
        AppEvent::RoundComplete { .. } => {
            s.phase = "waiting_followup".to_string();
        }
        AppEvent::ApprovalRequired {
            id,
            command_preview,
            category,
            ..
        } => {
            s.phase = "waiting_approval".to_string();
            s.pending_approval = Some(presence_core::PendingApprovalSnapshot {
                id: *id,
                command_preview: command_preview.clone(),
                category: format!("{:?}", category),
            });
        }
        AppEvent::ApprovalResolved { action, .. } => {
            s.pending_approval = None;
            if action == "deny" {
                s.phase = "done".to_string();
            } else {
                s.phase = "running_agent".to_string();
            }
        }
        AppEvent::HumanQuestionDetected { .. } => {
            s.phase = "waiting_human".to_string();
        }
        AppEvent::OrchestratorProgress { status, .. } => {
            s.phase = format!("orchestrating: {}", status);
        }
        AppEvent::SubAgentResult { formatted } => {
            s.last_output_summary = truncate(formatted, 500);
            s.last_task_result = Some(formatted.clone());
        }
        AppEvent::LoopError(msg) => {
            s.phase = format!("error: {}", msg);
        }
        AppEvent::DisplayReady {
            display_id, width, height, ..
        } => {
            let prefix = if *display_id == 0 {
                "user_session".to_string()
            } else {
                format!(":{}", display_id)
            };
            let label = if *display_id == 0 {
                format!("user_session ({}x{})", width, height)
            } else {
                format!(":{} ({}x{}, virtual)", display_id, width, height)
            };
            if !s.available_displays.iter().any(|d| d.starts_with(&prefix)) {
                s.available_displays.push(label);
            }
        }
        AppEvent::UserDisplayRevoked { .. } => {
            s.available_displays.retain(|d| !d.starts_with("user_session"));
        }
        _ => {}
    }
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
            reference_frame_ids: vec!["cam0-f00005".to_string()],
            display_target: Some("user_session".to_string()),
        };
        let json = serde_json::to_string(&envelope).unwrap();
        let parsed: TaskEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.task, "fix the tests");
        assert!(parsed.force_direct);
        assert_eq!(parsed.context_hints.len(), 1);
        assert_eq!(parsed.reference_frame_ids.len(), 1);
    }

    #[test]
    fn filter_event_push_events() {
        let mut last_phase = String::new();

        let event = AppEvent::TaskComplete {
            reason: "done".to_string(),
            summary: None,
        };
        assert!(filter_event(&event, &mut last_phase).is_some());

        let event = AppEvent::BudgetWarning {
            pct: 0.9,
            remaining: 1000,
        };
        assert!(filter_event(&event, &mut last_phase).is_some());

        let event = AppEvent::RoundComplete {
            round: 1,
            turns_in_round: 5,
        };
        assert!(filter_event(&event, &mut last_phase).is_some());

        let event = AppEvent::LoopError("oops".to_string());
        assert!(filter_event(&event, &mut last_phase).is_some());
    }

    #[test]
    fn filter_event_pull_only() {
        let mut last_phase = String::new();

        let event = AppEvent::AgentOutput {
            stdout: "hello".to_string(),
            stderr: String::new(),
        };
        assert!(filter_event(&event, &mut last_phase).is_none());

        assert!(filter_event(&AppEvent::Tick, &mut last_phase).is_none());

        let event = AppEvent::ModelResponseDelta {
            text: "hi".to_string(),
        };
        assert!(filter_event(&event, &mut last_phase).is_none());
    }

    #[test]
    fn filter_event_presence_ready_is_pull_only() {
        let mut last_phase = String::new();

        assert!(filter_event(&AppEvent::PresenceReady, &mut last_phase).is_none());

        let event = AppEvent::RoundComplete {
            round: 1,
            turns_in_round: 5,
        };
        assert!(filter_event(&event, &mut last_phase).is_some());
    }

    #[test]
    fn filter_event_phase_change_dedup() {
        let mut last_phase = String::new();

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
            &AppEvent::SubAgentResult {
                formatted: "Orchestrator completed: analyzed project structure".to_string(),
            },
            &state,
        );
        {
            let s = state.lock().unwrap();
            assert_eq!(
                s.last_output_summary,
                "Orchestrator completed: analyzed project structure"
            );
        }

        update_agent_state(
            &AppEvent::TaskComplete {
                reason: "done_signal".to_string(),
                summary: None,
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
        assert_eq!(tools.len(), 12);

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
        assert!(names.contains(&"send_message"));
        assert!(names.contains(&"inspect_frame"));
        assert!(names.contains(&"inspect_frames"));
    }

    #[test]
    fn format_event_variants() {
        let s = format_event(&PresenceEvent::PhaseChanged {
            phase: "thinking".to_string(),
        });
        assert!(s.contains("thinking"));

        let s = format_event(&PresenceEvent::TaskComplete {
            reason: "done".to_string(),
            summary: None,
        });
        assert!(s.contains("done"));

        let s = format_event(&PresenceEvent::Error {
            message: "oops".to_string(),
        });
        assert!(s.contains("oops"));
    }

    #[test]
    fn filter_event_presence_connected_is_pull_only() {
        let mut last_phase = String::new();
        assert!(filter_event(
            &AppEvent::PresenceConnected { server_session_id: None, last_event_seq: 0, live_provider: None, live_model: None },
            &mut last_phase
        ).is_none());
        assert!(filter_event(&AppEvent::PresenceDisconnected, &mut last_phase).is_none());
        assert!(filter_event(
            &AppEvent::VoiceLog { text: "hi".to_string(), seq: 1, tool_context: None },
            &mut last_phase
        ).is_none());
        assert!(filter_event(
            &AppEvent::PresenceCheckpointReceived { summary: "test".to_string(), last_event_seq: 1 },
            &mut last_phase
        ).is_none());
    }

    #[test]
    fn truncate_short_string() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_long_string() {
        assert_eq!(truncate("hello world", 5), "hello...");
    }

    // ── PresenceSession tests ──

    #[test]
    fn presence_session_new() {
        let session = PresenceSession::new("sess-1".to_string());
        assert_eq!(session.session_id(), "sess-1");
        assert!(!session.is_connected());
    }

    #[test]
    fn presence_session_record_event() {
        let mut session = PresenceSession::new("sess-1".to_string());
        let seq = session.record_event(PresenceEvent::PhaseChanged {
            phase: "thinking".to_string(),
        });
        assert_eq!(seq, 1);
        let seq2 = session.record_event(PresenceEvent::TaskComplete {
            reason: "done".to_string(),
            summary: None,
        });
        assert_eq!(seq2, 2);
    }

    #[test]
    fn presence_session_build_welcome() {
        let mut session = PresenceSession::new("sess-1".to_string());
        session.record_event(PresenceEvent::PhaseChanged {
            phase: "thinking".to_string(),
        });
        session.record_event(PresenceEvent::TaskComplete {
            reason: "done".to_string(),
            summary: None,
        });

        let state = AgentStateSnapshot {
            phase: "idle".to_string(),
            turn: 5,
            ..Default::default()
        };

        // Replay all events (last_event_seq = 0)
        let welcome = session.build_welcome(0, &state);
        assert_eq!(welcome.session_id, "sess-1");
        assert_eq!(welcome.events.len(), 2);
        assert_eq!(welcome.current_seq, 2);
        assert_eq!(welcome.state.phase, "idle");
        assert!(welcome.last_checkpoint_summary.is_none());

        // Replay since seq 1 (only event 2)
        let welcome = session.build_welcome(1, &state);
        assert_eq!(welcome.events.len(), 1);
        assert_eq!(welcome.events[0].seq, 2);
    }

    #[test]
    fn presence_session_checkpoint() {
        let mut session = PresenceSession::new("sess-1".to_string());
        session.record_event(PresenceEvent::PhaseChanged {
            phase: "thinking".to_string(),
        });

        let checkpoint = PresenceCheckpoint {
            summary: "Agent is running tests".to_string(),
            last_event_seq: 1,
        };
        let ack = session.record_checkpoint(checkpoint);
        assert_eq!(ack.seq, 1);

        let state = AgentStateSnapshot::default();
        let welcome = session.build_welcome(0, &state);
        assert_eq!(
            welcome.last_checkpoint_summary.as_deref(),
            Some("Agent is running tests")
        );
    }

    #[test]
    fn presence_session_connected_state() {
        let mut session = PresenceSession::new("sess-1".to_string());
        assert!(!session.is_connected());
        session.set_connected(true);
        assert!(session.is_connected());
        session.set_connected(false);
        assert!(!session.is_connected());
    }

    #[test]
    fn presence_session_multi_connect() {
        let mut session = PresenceSession::new("sess-1".to_string());
        session.set_connected(true); // browser A
        session.set_connected(true); // browser B
        assert!(session.is_connected());
        session.set_connected(false); // browser A disconnects
        assert!(session.is_connected()); // B still connected
        session.set_connected(false); // browser B disconnects
        assert!(!session.is_connected()); // now fully disconnected
    }

    #[test]
    fn presence_session_disconnect_underflow() {
        let mut session = PresenceSession::new("sess-1".to_string());
        session.set_connected(false); // spurious disconnect
        assert!(!session.is_connected()); // doesn't underflow
        session.set_connected(true);
        assert!(session.is_connected());
    }

    #[test]
    fn build_conversation_context_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(build_conversation_context(dir.path(), 10).is_none());
    }

    #[test]
    fn build_conversation_context_formats_turns() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = session_log::SessionLog::open(log_dir.clone()).unwrap();

        log.user_transcript("what's in this project?", 1);
        log.voice_log("It's an agent runtime.", 2, Some("transcript"));
        log.user_transcript("fix the bug", 3);

        let ctx = build_conversation_context(&log_dir, 10).unwrap();
        assert!(ctx.contains("User: what's in this project?"));
        assert!(ctx.contains("Model: It's an agent runtime."));
        assert!(ctx.contains("User: fix the bug"));
    }

    #[test]
    fn recall_memory_merges_voice_transcripts() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = session_log::SessionLog::open(log_dir.clone()).unwrap();

        log.user_transcript("fix the auth module", 1);
        log.voice_log("I'll check auth now.", 2, Some("transcript"));

        let knowledge_path = dir.path().join("knowledge.json");
        let args = serde_json::json!({"keywords": ["auth"]});
        let result = recall_memory(&knowledge_path, &log_dir, &args);

        // Should find voice transcript results
        assert!(result.contains("Conversation history"));
        assert!(result.contains("auth"));
    }
}
