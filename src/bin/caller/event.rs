//! Shared event infrastructure used across all frontends.
//!
//! `EventBus`, `AppEvent`, `ControlMsg`, and `ApprovalResponse` were extracted
//! from `tui/event.rs` so that non-TUI modules (MCP, control socket, web gateway,
//! presence) no longer depend on the `tui` module.

use crate::autonomy::ActionCategory;
use crate::provider::TokenUsage;
use crate::types::LogLevel;
use crossterm::event::KeyEvent;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};


/// A context injection item: text with optional images.
#[derive(Clone)]
pub struct ContextInjection {
    pub text: String,
    pub images: Vec<crate::conversation::ImageData>,
}

impl ContextInjection {
    /// Create a text-only injection (no images).
    pub fn text(msg: String) -> Self {
        Self { text: msg, images: vec![] }
    }
}

/// Shared queue for context messages to inject into the agent conversation.
/// Used by display takeover/release to notify the agent between turns,
/// and by presence to inject mid-task interjections.
pub type ContextInjectionQueue = Arc<Mutex<Vec<ContextInjection>>>;

/// Shared registry for pending approval responders.
///
/// When the agent loop needs approval, it inserts a `oneshot::Sender` here
/// keyed by approval ID, then sends an `ApprovalRequired` event (without the
/// responder). Whichever frontend resolves the approval (TUI key press, web
/// button, control socket command) removes the sender and calls `.send()`.
pub type ApprovalRegistry =
    Arc<Mutex<HashMap<u64, tokio::sync::oneshot::Sender<ApprovalResponse>>>>;

/// All events flowing through the system.
#[derive(Debug, Clone)]
pub enum AppEvent {
    // Terminal input
    Key(KeyEvent),
    #[allow(dead_code)]
    Resize(u16, u16),

    // Agent loop lifecycle
    TurnStarted {
        turn: usize,
        budget_pct: f64,
        #[allow(dead_code)]
        remaining: u64,
    },
    ModelResponse {
        turn: usize,
        content: String,
        usage: TokenUsage,
        reasoning: Option<String>,
    },
    /// Incremental text delta from streaming model response.
    ModelResponseDelta {
        text: String,
    },
    JsonExtracted {
        preview: String,
    },
    DoneSignal {
        message: Option<String>,
    },
    AgentStarted {
        turn: usize,
        commands_preview: String,
    },
    AgentOutput {
        stdout: String,
        stderr: String,
    },
    SubAgentResult {
        formatted: String,
    },
    OrchestratorProgress {
        turn: usize,
        status: String,
        last_action: String,
    },
    /// Detailed log entry from the orchestrator's session log (tailed by parent).
    OrchestratorLog {
        message: String,
        level: crate::types::LogLevel,
    },
    ContextManagement {
        turn: usize,
    },
    TaskComplete {
        reason: String,
        summary: Option<String>,
    },
    BudgetWarning {
        pct: f64,
        remaining: u64,
    },
    BudgetExhausted {
        remaining: u64,
    },
    SafetyCapReached,
    LoopError(String),

    // askHuman
    HumanQuestionDetected {
        question: String,
    },
    HumanResponseSent,

    // Autonomy / approval
    ApprovalRequired {
        id: u64,
        command_preview: String,
        category: ActionCategory,
    },

    // Vision display ready
    DisplayReady {
        display_id: u32,
        vnc_port: Option<u32>,
    },

    // Display takeover
    DisplayTaken {
        display_id: u32,
    },
    DisplayReleased {
        display_id: u32,
        note: Option<String>,
    },

    // Session directory changed (MCP per-task isolation)
    SessionDirChanged {
        path: std::path::PathBuf,
    },

    // Control socket
    ControlCommand(ControlMsg),

    // Auto-approved command visibility
    AutoApproved {
        preview: String,
    },

    // Presence layer token usage update
    PresenceUsageUpdate {
        total_tokens: u64,
        context_window: u64,
        usage_pct: f64,
        provider: String,
        model: String,
        prompt_tokens: u64,
        completion_tokens: u64,
        cached_tokens: u64,
    },

    // Live model (Gemini Live / OpenAI Realtime) usage update from browser
    LiveUsageUpdate {
        provider: String,
        model: String,
        input_tokens: u64,
        output_tokens: u64,
        cached_tokens: u64,
        total_tokens: u64,
        thinking_tokens: u64,
    },

    /// Presence layer log message (shown in TUI log panel).
    /// `level` controls visibility: None defaults to Info.
    PresenceLog {
        message: String,
        #[allow(dead_code)]
        level: Option<LogLevel>,
        /// Presence interaction turn (for log grouping/collapse).
        turn: Option<usize>,
    },

    // Round lifecycle
    RoundComplete {
        round: usize,
        turns_in_round: usize,
    },

    /// Presence layer responded — switch to follow-up mode without logging
    /// a fake round completion. Emitted by the response forwarder after each
    /// presence narration so the user can type a follow-up.
    PresenceReady,

    /// Browser-side presence connected — server-side presence should pause.
    PresenceConnected {
        server_session_id: Option<String>,
        last_event_seq: u64,
        /// Live model provider name (e.g. "gemini", "openai").
        live_provider: Option<String>,
        /// Live model name (e.g. "gemini-2.5-flash-native-audio-preview-12-2025").
        live_model: Option<String>,
    },
    /// Browser-side presence disconnected — server-side presence should resume.
    PresenceDisconnected,
    /// Voice transcript log from browser presence model.
    VoiceLog {
        text: String,
        seq: u64,
        tool_context: Option<String>,
    },
    /// Context checkpoint received from browser presence model.
    PresenceCheckpointReceived {
        summary: String,
        last_event_seq: u64,
    },
    /// Diagnostic from browser voice/presence layer (errors, silence, disconnects).
    VoiceDiagnostic {
        kind: String,
        detail: String,
    },

    /// Server-side transcription of user speech (from Whisper API).
    UserTranscript {
        text: String,
        seq: u64,
    },

    /// Computed usage snapshot (emitted by TUI after accumulating tokens).
    UsageSnapshot {
        main: crate::frontend::ModelUsageSnapshot,
        presence: Option<crate::frontend::ModelUsageSnapshot>,
    },

    /// Proactive status broadcast (emitted on turn start, phase change, etc.)
    StatusUpdate {
        turn: usize,
        phase: String,
        autonomy: String,
        session_id: String,
        task: String,
    },

    /// Log entry emitted by the App for broadcast to external consumers.
    /// The TUI renders these from its internal deque; external consumers
    /// (web UI, control socket) receive them via the outbound broadcaster.
    LogEntry {
        level: String,
        source: String,
        content: String,
        turn: Option<usize>,
    },

    // TUI internal
    Tick,
    #[allow(dead_code)]
    Quit,
}

/// Response from the approval system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalResponse {
    Approve,
    Skip,
    Deny,
    ApproveAll,
}

/// Commands received from the Unix control socket, MCP, and web gateway.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ControlMsg {
    Status,
    Usage,
    Approve {
        id: u64,
    },
    Deny {
        id: u64,
    },
    Skip {
        id: u64,
    },
    ApproveAll {
        id: u64,
    },
    Input {
        text: String,
    },
    SetAutonomy {
        level: String,
    },
    SetVerbosity {
        level: String,
    },
    ScheduleControllerRestart {
        controller_id: String,
        north_star_goal: String,
        #[serde(default)]
        reason: Option<String>,
        #[serde(default)]
        restart_after: Option<String>,
        #[serde(default)]
        restart_command: Option<String>,
        #[serde(default)]
        auto_start_task: Option<bool>,
        #[serde(default)]
        max_attempts: Option<u32>,
        #[serde(default)]
        cooldown_sec: Option<u64>,
    },
    ControllerTurnComplete {
        restart_id: String,
        turn_complete_token: String,
        #[serde(default)]
        status: Option<String>,
        #[serde(default)]
        handoff_summary: Option<String>,
    },
    GetRestartStatus,
    CancelControllerRestart {
        #[serde(default)]
        restart_id: Option<String>,
    },
    RequestControllerLoopHalt {
        #[serde(default)]
        persistent: Option<bool>,
    },
    ClearControllerLoopHalt,
    InterveneControllerLoop {
        mode: String,
    },
    GetControllerLoopStatus,
    StartTask {
        task: String,
        #[serde(default)]
        orchestrate: Option<bool>,
    },
    FollowUp {
        text: String,
    },
    TakeDisplay {
        display_id: u32,
    },
    ReleaseDisplay {
        display_id: u32,
        #[serde(default)]
        note: Option<String>,
    },
    QueryDetail {
        scope: String,
        #[serde(default)]
        target: Option<String>,
    },
    RecallMemory {
        #[serde(default)]
        keywords: Option<Vec<String>>,
        #[serde(default)]
        tags: Option<Vec<String>>,
        #[serde(default)]
        channel: Option<String>,
    },
    InvokeSkill {
        skill_name: String,
        #[serde(default)]
        arguments: Option<String>,
    },
    Quit,
}

/// The event bus sender. Cloneable for use in multiple tasks.
///
/// Backed by a `broadcast::channel` so multiple consumers can subscribe
/// independently. Each subscriber gets its own copy of every event.
#[derive(Clone)]
pub struct EventBus {
    tx: tokio::sync::broadcast::Sender<AppEvent>,
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _) = tokio::sync::broadcast::channel(4096);
        Self { tx }
    }

    pub fn send(&self, event: AppEvent) {
        let _ = self.tx.send(event);
    }

    /// Create a new subscriber that receives all future events.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<AppEvent> {
        self.tx.subscribe()
    }
}

/// Convert an AppEvent to an OutboundEvent for external consumers.
/// Returns `None` for terminal-only events (Key, Resize, Tick, Quit, etc.)
/// that external consumers don't need.
pub fn app_event_to_outbound(event: &AppEvent) -> Option<crate::types::OutboundEvent> {
    use crate::types::OutboundEvent;

    match event {
        AppEvent::TurnStarted {
            turn, budget_pct, ..
        } => Some(OutboundEvent::TurnStarted {
            turn: *turn,
            budget_pct: *budget_pct,
        }),
        AppEvent::ModelResponse {
            turn,
            content,
            reasoning,
            ..
        } => {
            let summary = crate::types::format_model_summary(content);
            Some(OutboundEvent::ModelResponse {
                turn: *turn,
                summary,
                reasoning_summary: reasoning.clone(),
            })
        }
        AppEvent::ModelResponseDelta { text } => {
            Some(OutboundEvent::ModelResponseDelta { text: text.clone() })
        }
        AppEvent::AgentStarted {
            turn,
            commands_preview,
        } => Some(OutboundEvent::AgentStarted {
            turn: *turn,
            commands_preview: commands_preview.clone(),
        }),
        AppEvent::AgentOutput { stdout, stderr } => Some(OutboundEvent::AgentOutput {
            stdout: stdout.clone(),
            stderr: stderr.clone(),
        }),
        AppEvent::DoneSignal { message } => Some(OutboundEvent::DoneSignal {
            message: message.clone(),
        }),
        AppEvent::TaskComplete { reason, summary } => Some(OutboundEvent::TaskComplete {
            reason: reason.clone(),
            summary: summary.clone(),
        }),
        AppEvent::ApprovalRequired {
            id,
            command_preview,
            ..
        } => Some(OutboundEvent::ApprovalRequired {
            id: *id,
            command: command_preview.clone(),
        }),
        AppEvent::AutoApproved { preview } => Some(OutboundEvent::AutoApproved {
            preview: preview.clone(),
        }),
        AppEvent::HumanQuestionDetected { question } => Some(OutboundEvent::AskHuman {
            question: question.clone(),
        }),
        AppEvent::HumanResponseSent => Some(OutboundEvent::HumanResponseSent),
        AppEvent::RoundComplete {
            round,
            turns_in_round,
        } => Some(OutboundEvent::RoundComplete {
            round: *round,
            turns_in_round: *turns_in_round,
        }),
        AppEvent::DisplayReady {
            display_id,
            vnc_port,
        } => Some(OutboundEvent::DisplayReady {
            display_id: *display_id,
            vnc_port: *vnc_port,
        }),
        AppEvent::DisplayTaken { display_id } => Some(OutboundEvent::DisplayTaken {
            display_id: *display_id,
        }),
        AppEvent::DisplayReleased { display_id, note } => {
            Some(OutboundEvent::DisplayReleased {
                display_id: *display_id,
                note: note.clone(),
            })
        }
        AppEvent::ContextManagement { turn } => Some(OutboundEvent::ContextManagement {
            turn: *turn,
        }),
        AppEvent::BudgetWarning { pct, remaining } => Some(OutboundEvent::BudgetWarning {
            pct: *pct,
            remaining: *remaining,
        }),
        AppEvent::BudgetExhausted { remaining } => Some(OutboundEvent::BudgetExhausted {
            remaining: *remaining,
        }),
        AppEvent::SafetyCapReached => Some(OutboundEvent::SafetyCapReached),
        AppEvent::LoopError(msg) => Some(OutboundEvent::LoopError {
            message: msg.clone(),
        }),
        AppEvent::SubAgentResult { formatted } => Some(OutboundEvent::SubAgentResult {
            summary: formatted.clone(),
        }),
        AppEvent::OrchestratorProgress { status, .. } => {
            Some(OutboundEvent::OrchestratorProgress {
                status: status.clone(),
            })
        }
        AppEvent::UserTranscript { text, seq } => Some(OutboundEvent::UserTranscript {
            text: text.clone(),
            seq: *seq,
        }),
        AppEvent::PresenceLog {
            message, level, ..
        } => Some(OutboundEvent::PresenceLog {
            message: message.clone(),
            level: level.as_ref().map(|l| crate::frontend::log_level_to_str(l).to_string()),
        }),
        AppEvent::PresenceUsageUpdate {
            total_tokens,
            context_window,
            usage_pct,
            provider,
            model,
            prompt_tokens,
            completion_tokens,
            cached_tokens,
        } => Some(OutboundEvent::PresenceUsageUpdate {
            total_tokens: *total_tokens,
            context_window: *context_window,
            usage_pct: *usage_pct,
            provider: provider.clone(),
            model: model.clone(),
            prompt_tokens: *prompt_tokens,
            completion_tokens: *completion_tokens,
            cached_tokens: *cached_tokens,
        }),
        AppEvent::LiveUsageUpdate {
            provider,
            model,
            input_tokens,
            output_tokens,
            cached_tokens,
            total_tokens,
            thinking_tokens,
        } => Some(OutboundEvent::LiveUsageUpdate {
            provider: provider.clone(),
            model: model.clone(),
            input_tokens: *input_tokens,
            output_tokens: *output_tokens,
            cached_tokens: *cached_tokens,
            total_tokens: *total_tokens,
            thinking_tokens: *thinking_tokens,
        }),
        AppEvent::UsageSnapshot { main, presence } => Some(OutboundEvent::UsageUpdate {
            main: main.clone(),
            presence: presence.clone(),
        }),
        AppEvent::StatusUpdate {
            turn,
            phase,
            autonomy,
            session_id,
            task,
        } => Some(OutboundEvent::Status {
            turn: *turn,
            phase: phase.clone(),
            autonomy: autonomy.clone(),
            session_id: session_id.clone(),
            task: task.clone(),
        }),
        AppEvent::LogEntry {
            level,
            source,
            content,
            turn,
        } => Some(OutboundEvent::LogEntry {
            level: level.clone(),
            source: source.clone(),
            content: content.clone(),
            turn: *turn,
        }),
        // Terminal-only / internal events — not broadcast to external consumers
        AppEvent::Key(_)
        | AppEvent::Resize(_, _)
        | AppEvent::Tick
        | AppEvent::Quit
        | AppEvent::JsonExtracted { .. }
        | AppEvent::OrchestratorLog { .. }
        | AppEvent::SessionDirChanged { .. }
        | AppEvent::ControlCommand(_)
        | AppEvent::PresenceReady
        | AppEvent::PresenceConnected { .. }
        | AppEvent::PresenceDisconnected
        | AppEvent::VoiceLog { .. }
        | AppEvent::PresenceCheckpointReceived { .. }
        | AppEvent::VoiceDiagnostic { .. } => None,
    }
}

/// Spawn a task that converts AppEvents to OutboundEvents and broadcasts them.
///
/// This is the single point where AppEvents are converted to the external
/// format used by the control socket, web gateway, and JSON stdout.
pub fn spawn_outbound_broadcaster(
    mut event_rx: tokio::sync::broadcast::Receiver<AppEvent>,
    outbound_tx: tokio::sync::broadcast::Sender<String>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match event_rx.recv().await {
                Ok(event) => {
                    if let Some(outbound) = app_event_to_outbound(&event) {
                        if let Ok(json) = serde_json::to_string(&outbound) {
                            let _ = outbound_tx.send(json);
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

/// Spawns a tick timer that sends Tick events at a regular interval.
pub fn spawn_tick_timer(bus: EventBus, interval_ms: u64) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(interval_ms));
        loop {
            interval.tick().await;
            bus.send(AppEvent::Tick);
        }
    })
}

/// Spawns a file monitor for askHuman question files.
/// The `question_path` is the session-scoped path to the human_question file.
/// Shared path that can be updated when MCP tasks change session directories.
pub type SharedQuestionPath = std::sync::Arc<tokio::sync::RwLock<std::path::PathBuf>>;

pub fn shared_question_path(path: std::path::PathBuf) -> SharedQuestionPath {
    std::sync::Arc::new(tokio::sync::RwLock::new(path))
}

pub fn spawn_human_question_monitor(
    bus: EventBus,
    question_path: SharedQuestionPath,
) -> tokio::task::JoinHandle<()> {
    use tokio::time::{interval, Duration};

    tokio::spawn(async move {
        let mut interval = interval(Duration::from_millis(250));
        let mut last_seen = false;

        loop {
            interval.tick().await;

            let path = question_path.read().await.clone();
            if path.exists() {
                if !last_seen {
                    if let Ok(question) = tokio::fs::read_to_string(&path).await {
                        let question = question.trim().to_string();
                        if !question.is_empty() {
                            bus.send(AppEvent::HumanQuestionDetected { question });
                        }
                    }
                    last_seen = true;
                }
            } else {
                if last_seen {
                    bus.send(AppEvent::HumanResponseSent);
                    last_seen = false;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_bus_send_receive() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            bus.send(AppEvent::Tick);
            bus.send(AppEvent::Quit);

            match rx.recv().await.unwrap() {
                AppEvent::Tick => {}
                _ => panic!("expected Tick"),
            }
            match rx.recv().await.unwrap() {
                AppEvent::Quit => {}
                _ => panic!("expected Quit"),
            }
        });
    }

    #[test]
    fn event_bus_clone() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let bus2 = bus.clone();
            bus.send(AppEvent::Tick);
            bus2.send(AppEvent::Quit);

            match rx.recv().await.unwrap() {
                AppEvent::Tick => {}
                _ => panic!("expected Tick"),
            }
            match rx.recv().await.unwrap() {
                AppEvent::Quit => {}
                _ => panic!("expected Quit"),
            }
        });
    }

    #[test]
    fn control_msg_status_deserialize() {
        let json = r#"{"action":"status"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::Status => {}
            _ => panic!("expected Status"),
        }
    }

    #[test]
    fn control_msg_approve_deserialize() {
        let json = r#"{"action":"approve","id":42}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::Approve { id } => assert_eq!(id, 42),
            _ => panic!("expected Approve"),
        }
    }

    #[test]
    fn control_msg_deny_deserialize() {
        let json = r#"{"action":"deny","id":7}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::Deny { id } => assert_eq!(id, 7),
            _ => panic!("expected Deny"),
        }
    }

    #[test]
    fn control_msg_input_deserialize() {
        let json = r#"{"action":"input","text":"PostgreSQL"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::Input { text } => assert_eq!(text, "PostgreSQL"),
            _ => panic!("expected Input"),
        }
    }

    #[test]
    fn control_msg_set_autonomy_deserialize() {
        let json = r#"{"action":"set_autonomy","level":"high"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::SetAutonomy { level } => assert_eq!(level, "high"),
            _ => panic!("expected SetAutonomy"),
        }
    }

    #[test]
    fn control_msg_quit_deserialize() {
        let json = r#"{"action":"quit"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::Quit => {}
            _ => panic!("expected Quit"),
        }
    }

    #[test]
    fn control_msg_schedule_restart_deserialize() {
        let json = r#"{"action":"schedule_controller_restart","controller_id":"codex","north_star_goal":"audit and improve","restart_after":"turn_end"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::ScheduleControllerRestart {
                controller_id,
                north_star_goal,
                restart_after,
                ..
            } => {
                assert_eq!(controller_id, "codex");
                assert_eq!(north_star_goal, "audit and improve");
                assert_eq!(restart_after.as_deref(), Some("turn_end"));
            }
            _ => panic!("expected ScheduleControllerRestart"),
        }
    }

    #[test]
    fn control_msg_controller_turn_complete_deserialize() {
        let json = r#"{"action":"controller_turn_complete","restart_id":"abc","turn_complete_token":"tok"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::ControllerTurnComplete {
                restart_id,
                turn_complete_token,
                ..
            } => {
                assert_eq!(restart_id, "abc");
                assert_eq!(turn_complete_token, "tok");
            }
            _ => panic!("expected ControllerTurnComplete"),
        }
    }

    #[test]
    fn control_msg_controller_loop_variants_deserialize() {
        let halt: ControlMsg =
            serde_json::from_str(r#"{"action":"request_controller_loop_halt","persistent":false}"#)
                .unwrap();
        match halt {
            ControlMsg::RequestControllerLoopHalt { persistent } => {
                assert_eq!(persistent, Some(false));
            }
            _ => panic!("expected RequestControllerLoopHalt"),
        }

        let intervene: ControlMsg =
            serde_json::from_str(r#"{"action":"intervene_controller_loop","mode":"stop"}"#)
                .unwrap();
        match intervene {
            ControlMsg::InterveneControllerLoop { mode } => assert_eq!(mode, "stop"),
            _ => panic!("expected InterveneControllerLoop"),
        }
    }

    #[test]
    fn control_msg_serialize_roundtrip() {
        let msgs = vec![
            ControlMsg::Status,
            ControlMsg::Approve { id: 1 },
            ControlMsg::Deny { id: 2 },
            ControlMsg::Input {
                text: "hello".to_string(),
            },
            ControlMsg::Skip { id: 3 },
            ControlMsg::ApproveAll { id: 4 },
            ControlMsg::SetAutonomy {
                level: "low".to_string(),
            },
            ControlMsg::SetVerbosity {
                level: "verbose".to_string(),
            },
            ControlMsg::ScheduleControllerRestart {
                controller_id: "codex".to_string(),
                north_star_goal: "improve".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: None,
                auto_start_task: Some(true),
                max_attempts: Some(1),
                cooldown_sec: Some(30),
            },
            ControlMsg::ControllerTurnComplete {
                restart_id: "id".to_string(),
                turn_complete_token: "token".to_string(),
                status: None,
                handoff_summary: None,
            },
            ControlMsg::GetRestartStatus,
            ControlMsg::CancelControllerRestart { restart_id: None },
            ControlMsg::RequestControllerLoopHalt {
                persistent: Some(true),
            },
            ControlMsg::ClearControllerLoopHalt,
            ControlMsg::InterveneControllerLoop {
                mode: "stop".to_string(),
            },
            ControlMsg::GetControllerLoopStatus,
            ControlMsg::StartTask {
                task: "fix bug".to_string(),
                orchestrate: None,
            },
            ControlMsg::FollowUp {
                text: "continue working".to_string(),
            },
            ControlMsg::QueryDetail {
                scope: "diff".to_string(),
                target: None,
            },
            ControlMsg::RecallMemory {
                keywords: Some(vec!["auth".to_string()]),
                tags: None,
                channel: Some("project_state".to_string()),
            },
            ControlMsg::TakeDisplay { display_id: 99 },
            ControlMsg::ReleaseDisplay {
                display_id: 99,
                note: Some("done testing".to_string()),
            },
            ControlMsg::InvokeSkill {
                skill_name: "deploy".to_string(),
                arguments: Some("staging".to_string()),
            },
            ControlMsg::Usage,
            ControlMsg::Quit,
        ];
        for msg in msgs {
            let json = serde_json::to_string(&msg).unwrap();
            let _: ControlMsg = serde_json::from_str(&json).unwrap();
        }
    }

    #[test]
    fn control_msg_usage_deserialize() {
        let json = r#"{"action":"usage"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        assert!(matches!(msg, ControlMsg::Usage));
    }

    #[test]
    fn control_msg_skip_deserialize() {
        let json = r#"{"action":"skip","id":5}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::Skip { id } => assert_eq!(id, 5),
            _ => panic!("expected Skip"),
        }
    }

    #[test]
    fn control_msg_approve_all_deserialize() {
        let json = r#"{"action":"approve_all","id":10}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::ApproveAll { id } => assert_eq!(id, 10),
            _ => panic!("expected ApproveAll"),
        }
    }

    #[test]
    fn control_msg_set_verbosity_deserialize() {
        let json = r#"{"action":"set_verbosity","level":"verbose"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::SetVerbosity { level } => assert_eq!(level, "verbose"),
            _ => panic!("expected SetVerbosity"),
        }
    }

    #[test]
    fn control_msg_start_task_deserialize() {
        let json = r#"{"action":"start_task","task":"fix bug"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::StartTask { task, orchestrate } => {
                assert_eq!(task, "fix bug");
                assert!(orchestrate.is_none());
            }
            _ => panic!("expected StartTask"),
        }
    }

    #[test]
    fn control_msg_start_task_roundtrip() {
        let msg = ControlMsg::StartTask {
            task: "deploy app".to_string(),
            orchestrate: Some(true),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: ControlMsg = serde_json::from_str(&json).unwrap();
        match parsed {
            ControlMsg::StartTask { task, orchestrate } => {
                assert_eq!(task, "deploy app");
                assert_eq!(orchestrate, Some(true));
            }
            _ => panic!("expected StartTask"),
        }
    }

    #[test]
    fn approval_response_variants() {
        assert_ne!(ApprovalResponse::Approve, ApprovalResponse::Deny);
        assert_ne!(ApprovalResponse::Skip, ApprovalResponse::ApproveAll);
        assert_eq!(ApprovalResponse::Approve, ApprovalResponse::Approve);
    }

    #[test]
    fn control_msg_query_detail_deserialize() {
        let json = r#"{"action":"query_detail","scope":"diff"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::QueryDetail { scope, target } => {
                assert_eq!(scope, "diff");
                assert!(target.is_none());
            }
            _ => panic!("expected QueryDetail"),
        }
    }

    #[test]
    fn control_msg_query_detail_with_target() {
        let json = r#"{"action":"query_detail","scope":"file","target":"src/main.rs"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::QueryDetail { scope, target } => {
                assert_eq!(scope, "file");
                assert_eq!(target.as_deref(), Some("src/main.rs"));
            }
            _ => panic!("expected QueryDetail"),
        }
    }

    #[test]
    fn control_msg_recall_memory_deserialize() {
        let json = r#"{"action":"recall_memory","keywords":["auth","login"],"channel":"project_state"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::RecallMemory {
                keywords,
                tags,
                channel,
            } => {
                assert_eq!(keywords, Some(vec!["auth".to_string(), "login".to_string()]));
                assert!(tags.is_none());
                assert_eq!(channel.as_deref(), Some("project_state"));
            }
            _ => panic!("expected RecallMemory"),
        }
    }

    #[test]
    fn control_msg_recall_memory_minimal() {
        let json = r#"{"action":"recall_memory"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::RecallMemory {
                keywords,
                tags,
                channel,
            } => {
                assert!(keywords.is_none());
                assert!(tags.is_none());
                assert!(channel.is_none());
            }
            _ => panic!("expected RecallMemory"),
        }
    }

    #[test]
    fn control_msg_invoke_skill_deserialize() {
        let json = r#"{"action":"invoke_skill","skill_name":"deploy","arguments":"staging"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::InvokeSkill {
                skill_name,
                arguments,
            } => {
                assert_eq!(skill_name, "deploy");
                assert_eq!(arguments, Some("staging".to_string()));
            }
            _ => panic!("expected InvokeSkill"),
        }
    }

    #[test]
    fn control_msg_invoke_skill_no_args() {
        let json = r#"{"action":"invoke_skill","skill_name":"lint"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::InvokeSkill {
                skill_name,
                arguments,
            } => {
                assert_eq!(skill_name, "lint");
                assert!(arguments.is_none());
            }
            _ => panic!("expected InvokeSkill"),
        }
    }

    // --- app_event_to_outbound tests ---

    #[test]
    fn outbound_turn_started() {
        let event = AppEvent::TurnStarted {
            turn: 5,
            budget_pct: 42.0,
            remaining: 100_000,
        };
        let outbound = app_event_to_outbound(&event).unwrap();
        let json = serde_json::to_string(&outbound).unwrap();
        assert!(json.contains("\"event\":\"turn_started\""));
        assert!(json.contains("\"turn\":5"));
    }

    #[test]
    fn outbound_done_signal() {
        let event = AppEvent::DoneSignal {
            message: Some("All done".to_string()),
        };
        let outbound = app_event_to_outbound(&event).unwrap();
        let json = serde_json::to_string(&outbound).unwrap();
        assert!(json.contains("\"event\":\"done_signal\""));
        assert!(json.contains("\"All done\""));
    }

    #[test]
    fn outbound_skips_tick() {
        assert!(app_event_to_outbound(&AppEvent::Tick).is_none());
    }

    #[test]
    fn outbound_skips_key() {
        let event = AppEvent::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('a'),
            crossterm::event::KeyModifiers::NONE,
        ));
        assert!(app_event_to_outbound(&event).is_none());
    }

    #[test]
    fn outbound_agent_output() {
        let event = AppEvent::AgentOutput {
            stdout: "hello".to_string(),
            stderr: "".to_string(),
        };
        let outbound = app_event_to_outbound(&event).unwrap();
        let json = serde_json::to_string(&outbound).unwrap();
        assert!(json.contains("\"event\":\"agent_output\""));
        assert!(json.contains("\"hello\""));
    }

    #[test]
    fn outbound_approval_required() {
        let event = AppEvent::ApprovalRequired {
            id: 42,
            command_preview: "rm -rf /tmp".to_string(),
            category: crate::autonomy::ActionCategory::Destructive,
        };
        let outbound = app_event_to_outbound(&event).unwrap();
        let json = serde_json::to_string(&outbound).unwrap();
        assert!(json.contains("\"event\":\"approval_required\""));
        assert!(json.contains("\"id\":42"));
    }

    #[test]
    fn outbound_budget_warning() {
        let event = AppEvent::BudgetWarning {
            pct: 85.0,
            remaining: 10_000,
        };
        let outbound = app_event_to_outbound(&event).unwrap();
        let json = serde_json::to_string(&outbound).unwrap();
        assert!(json.contains("\"event\":\"budget_warning\""));
    }

    #[test]
    fn outbound_model_response_delta() {
        let event = AppEvent::ModelResponseDelta {
            text: "hello".to_string(),
        };
        let outbound = app_event_to_outbound(&event).unwrap();
        let json = serde_json::to_string(&outbound).unwrap();
        assert!(json.contains("\"event\":\"model_response_delta\""));
    }

    #[test]
    fn outbound_auto_approved() {
        let event = AppEvent::AutoApproved {
            preview: "exec: ls".to_string(),
        };
        let outbound = app_event_to_outbound(&event).unwrap();
        let json = serde_json::to_string(&outbound).unwrap();
        assert!(json.contains("\"event\":\"auto_approved\""));
    }

    #[test]
    fn outbound_live_usage_update() {
        let event = AppEvent::LiveUsageUpdate {
            provider: "gemini".to_string(),
            model: "gemini-2.5-flash".to_string(),
            input_tokens: 1000,
            output_tokens: 500,
            cached_tokens: 200,
            total_tokens: 1700,
            thinking_tokens: 0,
        };
        let outbound = app_event_to_outbound(&event).unwrap();
        let json = serde_json::to_string(&outbound).unwrap();
        assert!(json.contains("\"event\":\"live_usage_update\""));
        assert!(json.contains("\"input_tokens\":1000"));
        assert!(json.contains("\"provider\":\"gemini\""));
    }

    #[test]
    fn outbound_broadcast_multiple_subscribers() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let bus = EventBus::new();
            let mut rx1 = bus.subscribe();
            let mut rx2 = bus.subscribe();
            bus.send(AppEvent::Tick);

            // Both subscribers should receive the event
            assert!(matches!(rx1.recv().await, Ok(AppEvent::Tick)));
            assert!(matches!(rx2.recv().await, Ok(AppEvent::Tick)));
        });
    }
}
