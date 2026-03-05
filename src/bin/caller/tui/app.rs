use crate::autonomy::SharedAutonomy;
use crate::control::{self, OutboundEvent};
use crate::tui::event::{AppEvent, ApprovalResponse, ControlMsg};
use crate::tui::layout::PanelConfig;
use chrono::Local;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::VecDeque;
use tokio::sync::oneshot;

const MAX_LOG_ENTRIES: usize = 10_000;

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

/// The current interactive mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppMode {
    Normal,
    AskHuman,
    Help,
    Approval,
    Inspect,
    FollowUp,
}

/// Log entry severity / source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Model,
    Agent,
    Error,
    Warn,
    SubAgent,
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
                    | LogLevel::Warn
                    | LogLevel::Error
                    | LogLevel::SubAgent
            ),
            Self::Verbose => !matches!(level, LogLevel::Debug),
            Self::Debug => true,
        }
    }
}

/// A single log entry.
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub ts: String,
    pub level: LogLevel,
    pub content: String,
}

/// Pending approval waiting for user response.
pub struct PendingApproval {
    #[allow(dead_code)]
    pub id: u64,
    pub command_preview: String,
    pub category: String,
    pub responder: oneshot::Sender<ApprovalResponse>,
}

/// The main application state.
pub struct App {
    // Display
    pub log_entries: VecDeque<LogEntry>,
    pub scroll_offset: usize,
    pub auto_scroll: bool,
    pub verbosity: Verbosity,
    pub inspect_index: Option<usize>,

    // Status
    pub provider_name: String,
    pub model_name: String,
    pub turn: usize,
    pub budget_pct: f64,
    pub current_phase: Phase,
    pub autonomy_display: String,

    // Panels
    pub panels: PanelConfig,
    pub mode: AppMode,
    pub should_quit: bool,

    // askHuman
    pub human_question: Option<String>,
    pub human_textarea: Option<tui_textarea::TextArea<'static>>,

    // Approval queue (FIFO)
    pub pending_approvals: VecDeque<PendingApproval>,

    // Shared autonomy state
    pub autonomy: SharedAutonomy,
    pub control_tx: Option<tokio::sync::broadcast::Sender<String>>,

    // Session log directory for askHuman files
    pub log_dir: std::path::PathBuf,

    // Token tracking
    pub session_tokens: u64,
    pub context_window: u64,

    // Session metadata
    pub session_id: String,
    pub task_description: String,

    // Animation
    pub tick_count: usize,

    // Streaming buffer for incremental text deltas
    pub streaming_buffer: String,

    // Multi-round follow-up
    pub round: usize,
    pub follow_up_textarea: Option<tui_textarea::TextArea<'static>>,
    pub follow_up_tx: Option<tokio::sync::mpsc::Sender<String>>,
    /// When presence layer is active, follow-up input goes here instead.
    pub presence_tx: Option<tokio::sync::mpsc::Sender<String>>,

    // Vision display info (shown in status bar when active)
    pub display_info: Option<String>,

    // Presence layer usage tracking (parallel to main model)
    pub presence_provider_name: Option<String>,
    pub presence_model_name: Option<String>,
    pub presence_tokens: u64,
    pub presence_usage_pct: f64,
    pub presence_context_window: u64,

    // Sender for forwarding filtered events to the presence layer
    pub presence_event_tx: Option<tokio::sync::mpsc::Sender<crate::presence::PresenceEvent>>,
}

impl App {
    pub fn new(
        provider_name: String,
        model_name: String,
        autonomy: SharedAutonomy,
        log_dir: std::path::PathBuf,
    ) -> Self {
        Self {
            log_entries: VecDeque::new(),
            scroll_offset: 0,
            auto_scroll: true,
            verbosity: Verbosity::Normal,
            inspect_index: None,
            provider_name,
            model_name,
            turn: 0,
            budget_pct: 0.0,
            current_phase: Phase::Idle,
            autonomy_display: "Medium".to_string(),
            panels: PanelConfig::default(),
            mode: AppMode::Normal,
            should_quit: false,
            human_question: None,
            human_textarea: None,
            pending_approvals: VecDeque::new(),
            autonomy,
            control_tx: None,
            log_dir,
            session_tokens: 0,
            context_window: 0,
            session_id: String::new(),
            task_description: String::new(),
            tick_count: 0,
            streaming_buffer: String::new(),
            round: 1,
            follow_up_textarea: None,
            follow_up_tx: None,
            presence_tx: None,
            display_info: None,
            presence_provider_name: None,
            presence_model_name: None,
            presence_tokens: 0,
            presence_usage_pct: 0.0,
            presence_context_window: 0,
            presence_event_tx: None,
        }
    }

    pub fn set_follow_up_sender(&mut self, tx: tokio::sync::mpsc::Sender<String>) {
        self.follow_up_tx = Some(tx);
    }

    pub fn set_presence_sender(&mut self, tx: tokio::sync::mpsc::Sender<String>) {
        self.presence_tx = Some(tx);
    }

    pub fn set_presence_event_sender(
        &mut self,
        tx: tokio::sync::mpsc::Sender<crate::presence::PresenceEvent>,
    ) {
        self.presence_event_tx = Some(tx);
    }

    /// Build a usage snapshot for the main model.
    fn main_usage_snapshot(&self) -> crate::frontend::ModelUsageSnapshot {
        crate::frontend::ModelUsageSnapshot {
            provider: self.provider_name.clone(),
            model: self.model_name.clone(),
            tokens_used: self.session_tokens,
            context_window: self.context_window,
            usage_pct: self.budget_pct,
        }
    }

    /// Build a usage snapshot for the presence model, if active.
    fn presence_usage_snapshot(&self) -> Option<crate::frontend::ModelUsageSnapshot> {
        self.presence_provider_name.as_ref().map(|provider| {
            crate::frontend::ModelUsageSnapshot {
                provider: provider.clone(),
                model: self.presence_model_name.clone().unwrap_or_default(),
                tokens_used: self.presence_tokens,
                context_window: self.presence_context_window,
                usage_pct: self.presence_usage_pct,
            }
        })
    }

    /// Broadcast a usage update to all connected control socket clients.
    fn broadcast_usage_update(&self) {
        self.broadcast_control(OutboundEvent::UsageUpdate {
            main: self.main_usage_snapshot(),
            presence: self.presence_usage_snapshot(),
        });
    }

    /// Forward a filtered event to the presence layer (non-blocking).
    fn forward_to_presence(&self, event: &AppEvent) {
        use crate::presence;
        if let Some(ref tx) = self.presence_event_tx {
            // Reuse the static last_phase tracking via a simple approach:
            // We only forward push-worthy events. The filter is stateless here
            // (no phase dedup) to keep it simple — presence can handle duplicates.
            let pe = match event {
                AppEvent::TaskComplete { reason } => Some(presence::PresenceEvent::TaskComplete {
                    reason: reason.clone(),
                }),
                AppEvent::ApprovalRequired {
                    id,
                    command_preview,
                    category,
                    ..
                } => Some(presence::PresenceEvent::ApprovalNeeded {
                    id: *id,
                    preview: command_preview.clone(),
                    category: format!("{:?}", category),
                }),
                AppEvent::HumanQuestionDetected { question } => {
                    Some(presence::PresenceEvent::HumanQuestion {
                        question: question.clone(),
                    })
                }
                AppEvent::BudgetWarning { pct, remaining } => {
                    Some(presence::PresenceEvent::BudgetWarning {
                        pct: *pct,
                        remaining: *remaining,
                    })
                }
                AppEvent::RoundComplete {
                    round,
                    turns_in_round,
                } => Some(presence::PresenceEvent::RoundComplete {
                    round: *round,
                    turns_in_round: *turns_in_round,
                }),
                AppEvent::LoopError(msg) => Some(presence::PresenceEvent::Error {
                    message: msg.clone(),
                }),
                _ => None,
            };
            if let Some(pe) = pe {
                let _ = tx.try_send(pe);
            }
        }
    }

    pub fn set_control_socket(&mut self, tx: tokio::sync::broadcast::Sender<String>) {
        self.control_tx = Some(tx);
    }

    pub fn log(&mut self, level: LogLevel, content: String) {
        if self.log_entries.len() >= MAX_LOG_ENTRIES {
            self.log_entries.pop_front();
        }
        self.log_entries.push_back(LogEntry {
            ts: Local::now().format("%H:%M:%S").to_string(),
            level,
            content,
        });
        if self.auto_scroll {
            self.scroll_to_bottom();
        }
    }

    pub fn filtered_indices(&self) -> Vec<usize> {
        self.log_entries
            .iter()
            .enumerate()
            .filter_map(|(idx, entry)| self.verbosity.includes(&entry.level).then_some(idx))
            .collect()
    }

    pub fn scroll_to_bottom(&mut self) {
        let total = self.filtered_indices().len();
        self.scroll_offset = total.saturating_sub(1);
    }

    pub fn scroll_up(&mut self, n: usize) {
        self.auto_scroll = false;
        self.scroll_offset = self.scroll_offset.saturating_sub(n);
    }

    pub fn scroll_down(&mut self, n: usize) {
        self.auto_scroll = false;
        let total = self.filtered_indices().len();
        self.scroll_offset = (self.scroll_offset + n).min(total.saturating_sub(1));
    }

    pub fn scroll_page_up(&mut self, page_size: usize) {
        self.scroll_up(page_size);
    }

    pub fn scroll_page_down(&mut self, page_size: usize) {
        self.scroll_down(page_size);
    }

    pub fn scroll_home(&mut self) {
        self.auto_scroll = false;
        self.scroll_offset = 0;
    }

    pub fn scroll_end(&mut self) {
        self.auto_scroll = true;
        self.scroll_to_bottom();
    }

    /// Get the height available for the bottom panel (0 if none active).
    pub fn bottom_panel_height(&self) -> u16 {
        match self.mode {
            AppMode::Approval => {
                // Dynamic height: command lines + 3 (border top/bottom + hint line)
                let cmd_lines = self
                    .pending_approvals
                    .front()
                    .map(|p| p.command_preview.split('\n').count())
                    .unwrap_or(3);
                let height = (cmd_lines + 3) as u16;
                height.clamp(6, 20)
            }
            AppMode::AskHuman => 5,
            AppMode::FollowUp => 5,
            _ => 0,
        }
    }

    /// Handle a key event. Returns true if the event was consumed.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        // Global quit
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return true;
        }

        match self.mode {
            AppMode::Help => {
                // Any key closes help
                self.mode = AppMode::Normal;
                true
            }
            AppMode::Approval => self.handle_approval_key(key),
            AppMode::AskHuman => self.handle_human_key(key),
            AppMode::FollowUp => self.handle_follow_up_key(key),
            AppMode::Inspect => self.handle_inspect_key(key),
            AppMode::Normal => self.handle_normal_key(key),
        }
    }

    fn handle_normal_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char('q') => {
                self.should_quit = true;
                true
            }
            KeyCode::Char('v') => {
                self.verbosity = self.verbosity.next();
                self.clamp_view_to_filtered();
                true
            }
            KeyCode::Char('i') | KeyCode::Enter => {
                if self.open_inspect_mode() {
                    true
                } else {
                    false
                }
            }
            KeyCode::Char('?') => {
                self.mode = AppMode::Help;
                true
            }
            KeyCode::Char('+') | KeyCode::Char('=') => {
                self.cycle_autonomy_up();
                true
            }
            KeyCode::Char('-') => {
                self.cycle_autonomy_down();
                true
            }
            KeyCode::Up => {
                self.scroll_up(1);
                true
            }
            KeyCode::Down => {
                self.scroll_down(1);
                true
            }
            KeyCode::PageUp => {
                self.scroll_page_up(20);
                true
            }
            KeyCode::PageDown => {
                self.scroll_page_down(20);
                true
            }
            KeyCode::Home => {
                self.scroll_home();
                true
            }
            KeyCode::End => {
                self.scroll_end();
                true
            }
            _ => false,
        }
    }

    fn handle_inspect_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc | KeyCode::Char('i') | KeyCode::Enter => {
                self.mode = AppMode::Normal;
                true
            }
            KeyCode::Up => {
                self.move_inspect(-1);
                true
            }
            KeyCode::Down => {
                self.move_inspect(1);
                true
            }
            KeyCode::PageUp => {
                self.move_inspect(-20);
                true
            }
            KeyCode::PageDown => {
                self.move_inspect(20);
                true
            }
            KeyCode::Home => {
                self.jump_inspect_to_edge(true);
                true
            }
            KeyCode::End => {
                self.jump_inspect_to_edge(false);
                true
            }
            KeyCode::Char('v') => {
                self.verbosity = self.verbosity.next();
                self.clamp_view_to_filtered();
                self.ensure_inspect_index();
                true
            }
            _ => false,
        }
    }

    fn clamp_view_to_filtered(&mut self) {
        let total = self.filtered_indices().len();
        if total == 0 {
            self.scroll_offset = 0;
            self.inspect_index = None;
            return;
        }
        self.scroll_offset = self.scroll_offset.min(total.saturating_sub(1));
    }

    fn focus_index(&self) -> Option<usize> {
        let filtered = self.filtered_indices();
        if filtered.is_empty() {
            return None;
        }
        if self.auto_scroll {
            return filtered.last().copied();
        }
        filtered.get(self.scroll_offset).copied()
    }

    fn open_inspect_mode(&mut self) -> bool {
        self.inspect_index = self.focus_index();
        if self.inspect_index.is_some() {
            self.mode = AppMode::Inspect;
            return true;
        }
        false
    }

    fn ensure_inspect_index(&mut self) {
        let filtered = self.filtered_indices();
        if filtered.is_empty() {
            self.inspect_index = None;
            return;
        }
        if let Some(current) = self.inspect_index {
            if filtered.contains(&current) {
                return;
            }
        }
        self.inspect_index = filtered.last().copied();
    }

    fn move_inspect(&mut self, delta: isize) {
        let filtered = self.filtered_indices();
        if filtered.is_empty() {
            self.inspect_index = None;
            return;
        }

        let current_pos = self
            .inspect_index
            .and_then(|idx| filtered.iter().position(|&i| i == idx))
            .unwrap_or(filtered.len().saturating_sub(1));

        let next_pos = (current_pos as isize + delta)
            .clamp(0, filtered.len().saturating_sub(1) as isize) as usize;
        self.inspect_index = Some(filtered[next_pos]);
        self.auto_scroll = false;
        self.scroll_offset = next_pos.saturating_sub(2);
    }

    fn jump_inspect_to_edge(&mut self, start: bool) {
        let filtered = self.filtered_indices();
        if filtered.is_empty() {
            self.inspect_index = None;
            return;
        }

        let pos = if start {
            0
        } else {
            filtered.len().saturating_sub(1)
        };
        self.inspect_index = Some(filtered[pos]);
        self.auto_scroll = !start;
        self.scroll_offset = if start { 0 } else { pos.saturating_sub(2) };
    }

    fn handle_approval_key(&mut self, key: KeyEvent) -> bool {
        let response = match key.code {
            KeyCode::Char('y') | KeyCode::Enter => Some(ApprovalResponse::Approve),
            KeyCode::Char('s') => Some(ApprovalResponse::Skip),
            KeyCode::Char('a') => Some(ApprovalResponse::ApproveAll),
            KeyCode::Char('n') => Some(ApprovalResponse::Deny),
            _ => None,
        };

        if let Some(resp) = response {
            if let Some(pending) = self.pending_approvals.pop_front() {
                let _ = pending.responder.send(resp);
            }
            if self.pending_approvals.is_empty() {
                self.mode = AppMode::Normal;
                self.current_phase = Phase::RunningAgent;
            }
            true
        } else {
            false
        }
    }

    fn handle_human_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc => {
                self.human_textarea = None;
                self.human_question = None;
                self.mode = AppMode::Normal;
                self.current_phase = Phase::RunningAgent;
                true
            }
            KeyCode::Enter if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                // Submit the response
                if let Some(ref textarea) = self.human_textarea {
                    let response = textarea.lines().join("\n");
                    if response.trim().is_empty() {
                        self.log(
                            LogLevel::Warn,
                            "Response cannot be empty. Enter text or press Esc to cancel."
                                .to_string(),
                        );
                        return true;
                    }

                    match std::fs::write(self.log_dir.join("human_response"), &response) {
                        Ok(_) => {
                            self.log(
                                LogLevel::Info,
                                format!("Human response sent: {}", truncate_str(&response, 80)),
                            );
                        }
                        Err(e) => {
                            self.log(
                                LogLevel::Error,
                                format!("Failed to send human response: {}", e),
                            );
                            return true;
                        }
                    }
                }
                self.human_textarea = None;
                self.human_question = None;
                self.mode = AppMode::Normal;
                self.current_phase = Phase::RunningAgent;
                true
            }
            _ => {
                // Forward to textarea
                if let Some(ref mut textarea) = self.human_textarea {
                    textarea.input(key);
                }
                true
            }
        }
    }

    fn handle_follow_up_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char('q') => {
                // Quit: drop the sender so recv() returns None
                self.follow_up_textarea = None;
                self.follow_up_tx = None;
                self.mode = AppMode::Normal;
                self.current_phase = Phase::Done;
                self.should_quit = true;
                true
            }
            KeyCode::Esc => {
                // Cancel follow-up, end session
                self.follow_up_textarea = None;
                self.follow_up_tx = None;
                self.mode = AppMode::Normal;
                self.current_phase = Phase::Done;
                true
            }
            KeyCode::Enter if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                // Submit the follow-up
                if let Some(ref textarea) = self.follow_up_textarea {
                    let text = textarea.lines().join("\n");
                    if text.trim().is_empty() {
                        self.log(
                            LogLevel::Warn,
                            "Follow-up cannot be empty. Enter text or press Esc/q to quit."
                                .to_string(),
                        );
                        return true;
                    }

                    // Route through presence layer if active, else direct follow-up
                    if let Some(ref tx) = self.presence_tx {
                        let _ = tx.try_send(text.clone());
                    } else if let Some(ref tx) = self.follow_up_tx {
                        let _ = tx.try_send(text.clone());
                    }
                    self.log(LogLevel::Info, format!("Follow-up: {}", truncate_str(&text, 80)));
                }
                self.follow_up_textarea = None;
                self.mode = AppMode::Normal;
                self.current_phase = Phase::Thinking;
                self.round += 1;
                true
            }
            _ => {
                // Forward to textarea
                if let Some(ref mut textarea) = self.follow_up_textarea {
                    textarea.input(key);
                }
                true
            }
        }
    }

    fn cycle_autonomy_up(&mut self) {
        let rt = tokio::runtime::Handle::try_current();
        if let Ok(handle) = rt {
            let autonomy = self.autonomy.clone();
            handle.spawn(async move {
                let mut state = autonomy.write().await;
                state.level = state.level.cycle_up();
            });
        }
    }

    fn cycle_autonomy_down(&mut self) {
        let rt = tokio::runtime::Handle::try_current();
        if let Ok(handle) = rt {
            let autonomy = self.autonomy.clone();
            handle.spawn(async move {
                let mut state = autonomy.write().await;
                state.level = state.level.cycle_down();
            });
        }
    }

    fn set_autonomy_level(&self, level: &str) {
        let parsed = crate::autonomy::AutonomyLevel::from_str_loose(level);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let autonomy = self.autonomy.clone();
            handle.spawn(async move {
                let mut state = autonomy.write().await;
                state.level = parsed;
            });
        }
    }

    fn broadcast_control(&self, event: OutboundEvent) {
        if let Some(tx) = &self.control_tx {
            control::broadcast_event(tx, &event);
        }
    }

    fn handle_control_command(&mut self, msg: ControlMsg) {
        match msg {
            ControlMsg::Status => {
                self.broadcast_control(OutboundEvent::Status {
                    turn: self.turn,
                    phase: format!("{:?}", self.current_phase).to_lowercase(),
                    autonomy: self.autonomy_display.to_lowercase(),
                    session_id: self.session_id.clone(),
                    task: self.task_description.clone(),
                });
            }
            ControlMsg::Usage => {
                self.broadcast_control(OutboundEvent::Usage {
                    main: self.main_usage_snapshot(),
                    presence: self.presence_usage_snapshot(),
                });
            }
            ControlMsg::Approve { id } => {
                if let Some(pos) = self.pending_approvals.iter().position(|p| p.id == id) {
                    let pending = self.pending_approvals.remove(pos).unwrap();
                    self.log(
                        LogLevel::Info,
                        format!("Approved via control socket (turn {})", id),
                    );
                    let _ = pending.responder.send(ApprovalResponse::Approve);
                    if self.pending_approvals.is_empty() {
                        self.mode = AppMode::Normal;
                        self.current_phase = Phase::RunningAgent;
                    }
                }
            }
            ControlMsg::Deny { id } => {
                if let Some(pos) = self.pending_approvals.iter().position(|p| p.id == id) {
                    let pending = self.pending_approvals.remove(pos).unwrap();
                    self.log(
                        LogLevel::Info,
                        format!("Denied via control socket (turn {})", id),
                    );
                    let _ = pending.responder.send(ApprovalResponse::Deny);
                    if self.pending_approvals.is_empty() {
                        self.mode = AppMode::Normal;
                        self.current_phase = Phase::Done;
                    }
                }
            }
            ControlMsg::Skip { id } => {
                if let Some(pos) = self.pending_approvals.iter().position(|p| p.id == id) {
                    let pending = self.pending_approvals.remove(pos).unwrap();
                    self.log(
                        LogLevel::Info,
                        format!("Skipped via control socket (turn {})", id),
                    );
                    let _ = pending.responder.send(ApprovalResponse::Skip);
                    if self.pending_approvals.is_empty() {
                        self.mode = AppMode::Normal;
                        self.current_phase = Phase::RunningAgent;
                    }
                }
            }
            ControlMsg::ApproveAll { id } => {
                if let Some(pos) = self.pending_approvals.iter().position(|p| p.id == id) {
                    let pending = self.pending_approvals.remove(pos).unwrap();
                    self.log(
                        LogLevel::Info,
                        format!("Approve-all via control socket (turn {})", id),
                    );
                    let _ = pending.responder.send(ApprovalResponse::ApproveAll);
                    self.set_autonomy_level("full");
                    if self.pending_approvals.is_empty() {
                        self.mode = AppMode::Normal;
                        self.current_phase = Phase::RunningAgent;
                    }
                }
            }
            ControlMsg::Input { text } => {
                if self.mode == AppMode::AskHuman {
                    let _ = std::fs::write(self.log_dir.join("human_response"), text.as_bytes());
                    self.human_textarea = None;
                    self.human_question = None;
                    self.mode = AppMode::Normal;
                    self.current_phase = Phase::RunningAgent;
                }
            }
            ControlMsg::SetAutonomy { level } => {
                self.set_autonomy_level(&level);
            }
            ControlMsg::SetVerbosity { level } => {
                let new_verbosity = match level.to_lowercase().as_str() {
                    "quiet" => Verbosity::Quiet,
                    "normal" => Verbosity::Normal,
                    "verbose" => Verbosity::Verbose,
                    "debug" => Verbosity::Debug,
                    _ => {
                        self.log(
                            LogLevel::Warn,
                            format!("Unknown verbosity level: {}", level),
                        );
                        return;
                    }
                };
                self.verbosity = new_verbosity;
                self.clamp_view_to_filtered();
                self.log(
                    LogLevel::Info,
                    format!("Verbosity set to {} via control socket", new_verbosity.label()),
                );
            }
            ControlMsg::StartTask { .. } => {
                self.log(
                    LogLevel::Warn,
                    "start_task is only supported in MCP/voice mode".to_string(),
                );
            }
            ControlMsg::ScheduleControllerRestart { .. }
            | ControlMsg::ControllerTurnComplete { .. }
            | ControlMsg::GetRestartStatus
            | ControlMsg::CancelControllerRestart { .. }
            | ControlMsg::RequestControllerLoopHalt { .. }
            | ControlMsg::ClearControllerLoopHalt
            | ControlMsg::InterveneControllerLoop { .. }
            | ControlMsg::GetControllerLoopStatus => {
                self.log(
                    LogLevel::Warn,
                    "Controller control commands are only supported in MCP mode".to_string(),
                );
            }
            ControlMsg::FollowUp { text } => {
                if self.mode == AppMode::FollowUp {
                    // Route through presence layer if active
                    if let Some(ref tx) = self.presence_tx {
                        let _ = tx.try_send(text.clone());
                    } else if let Some(ref tx) = self.follow_up_tx {
                        let _ = tx.try_send(text.clone());
                    }
                    self.follow_up_textarea = None;
                    self.mode = AppMode::Normal;
                    self.current_phase = Phase::Thinking;
                    self.round += 1;
                    self.log(LogLevel::Info, format!("Follow-up via control socket: {}", truncate_str(&text, 80)));
                }
            }
            ControlMsg::QueryDetail { scope, .. } => {
                self.log(
                    LogLevel::Info,
                    format!("Query detail request: {}", scope),
                );
            }
            ControlMsg::RecallMemory { keywords, .. } => {
                let kws = keywords.as_deref().unwrap_or(&[]);
                self.log(
                    LogLevel::Info,
                    format!("Memory recall request: {:?}", kws),
                );
            }
            ControlMsg::Quit => {
                self.should_quit = true;
            }
        }
    }

    /// Process an AppEvent and update state accordingly.
    pub fn handle_event(&mut self, event: AppEvent) {
        // Forward filtered events to the presence layer (non-blocking)
        self.forward_to_presence(&event);

        match event {
            AppEvent::TurnStarted {
                turn, budget_pct, ..
            } => {
                self.turn = turn;
                self.budget_pct = budget_pct;
                self.current_phase = Phase::Thinking;
                self.broadcast_control(OutboundEvent::TurnStarted { turn, budget_pct });
                self.log(
                    LogLevel::Debug,
                    format!("Turn {} started ({:.0}% budget)", turn, budget_pct),
                );
            }
            AppEvent::ModelResponse {
                turn,
                content,
                usage,
                reasoning,
            } => {
                self.turn = turn;
                self.session_tokens += usage.total_tokens;
                self.streaming_buffer.clear();
                self.broadcast_usage_update();
                // Show human-readable command summary at Model level (visible at Normal verbosity)
                let summary = format_model_summary(&content);
                self.log(LogLevel::Model, format!("T{}: {}", turn, summary));
                if let Some(ref reasoning_text) = reasoning {
                    self.log(LogLevel::Model, format!("Reasoning: {}", reasoning_text));
                }
                self.log(
                    LogLevel::Info,
                    format!(
                        "tokens: prompt={} completion={} total={}",
                        usage.prompt_tokens, usage.completion_tokens, usage.total_tokens
                    ),
                );
                self.log(LogLevel::Debug, format!("Raw model response: {}", content));
            }
            AppEvent::ModelResponseDelta { text } => {
                // Accumulate streaming text; shown at Debug level to avoid noise
                self.streaming_buffer.push_str(&text);
            }
            AppEvent::JsonExtracted { preview } => {
                self.log(LogLevel::Debug, format!("JSON: {}", preview));
            }
            AppEvent::DoneSignal { message } => {
                if let Some(msg) = message {
                    self.log(LogLevel::Info, msg);
                }
                self.current_phase = Phase::Done;
            }
            AppEvent::AgentStarted {
                turn,
                commands_preview,
            } => {
                self.current_phase = Phase::RunningAgent;
                self.log(
                    LogLevel::Debug,
                    format!("Agent running (turn {}): {}", turn, commands_preview),
                );
            }
            AppEvent::AgentOutput { stdout, stderr } => {
                self.broadcast_control(OutboundEvent::AgentOutput {
                    stdout: stdout.clone(),
                    stderr: stderr.clone(),
                });
                if !stdout.is_empty() {
                    for line in stdout.lines() {
                        self.log(LogLevel::Agent, line.to_string());
                    }
                }
                if !stderr.is_empty() {
                    for line in stderr.lines() {
                        self.log(LogLevel::Warn, format!("stderr: {}", line));
                    }
                }
            }
            AppEvent::SubAgentResult { formatted } => {
                self.log(LogLevel::SubAgent, formatted);
            }
            AppEvent::OrchestratorProgress {
                turn,
                status,
                last_action,
            } => {
                self.turn = turn;
                self.current_phase = Phase::Orchestrating;
                let summary = if last_action.is_empty() {
                    format!("Orchestrator T{}: {}", turn, status)
                } else {
                    format!("Orchestrator T{}: {} — {}", turn, status, last_action)
                };
                self.log(LogLevel::SubAgent, summary);
            }
            AppEvent::ContextManagement { turn } => {
                self.log(
                    LogLevel::Debug,
                    format!("Context management (turn {})", turn),
                );
            }
            AppEvent::TaskComplete { reason } => {
                self.current_phase = Phase::Done;
                self.broadcast_control(OutboundEvent::TaskComplete {
                    reason: reason.clone(),
                });
                self.log(LogLevel::Info, format!("--- {} ---", reason));
            }
            AppEvent::BudgetWarning { pct, remaining } => {
                self.budget_pct = pct;
                self.log(
                    LogLevel::Warn,
                    format!("Budget warning: {:.0}% used, {} remaining", pct, remaining),
                );
            }
            AppEvent::BudgetExhausted { remaining } => {
                self.budget_pct = 100.0;
                self.log(
                    LogLevel::Error,
                    format!("Budget exhausted ({} remaining)", remaining),
                );
                self.current_phase = Phase::Done;
            }
            AppEvent::SafetyCapReached => {
                self.log(LogLevel::Error, "Safety cap reached".to_string());
                self.current_phase = Phase::Done;
            }
            AppEvent::LoopError(msg) => {
                self.log(LogLevel::Error, msg);
                self.current_phase = Phase::Done;
            }
            AppEvent::PresenceLog { message } => {
                self.log(LogLevel::Info, format!("[presence] {}", message));
            }
            AppEvent::HumanQuestionDetected { question } => {
                self.human_question = Some(question.clone());
                self.current_phase = Phase::WaitingHuman;
                self.mode = AppMode::AskHuman;
                self.broadcast_control(OutboundEvent::AskHuman {
                    question: question.clone(),
                });
                let mut textarea = tui_textarea::TextArea::default();
                textarea.set_cursor_line_style(ratatui::style::Style::default());
                self.human_textarea = Some(textarea);
                self.log(LogLevel::Info, format!("Human question: {}", question));
            }
            AppEvent::HumanResponseSent => {
                self.log(LogLevel::Info, "Human prompt closed by runtime".to_string());
            }
            AppEvent::ApprovalRequired {
                id,
                command_preview,
                category,
                responder,
            } => {
                self.current_phase = Phase::WaitingApproval;
                self.mode = AppMode::Approval;
                self.pending_approvals.push_back(PendingApproval {
                    id,
                    command_preview: command_preview.clone(),
                    category: category.to_string(),
                    responder,
                });
                self.log(
                    LogLevel::Warn,
                    format!(
                        "Approval needed [{}]: {}",
                        category,
                        truncate_str(&command_preview, 80)
                    ),
                );
                self.broadcast_control(OutboundEvent::ApprovalRequired {
                    id,
                    command: command_preview,
                });
            }
            AppEvent::ControlCommand(msg) => {
                self.handle_control_command(msg);
            }
            AppEvent::AutoApproved { preview } => {
                self.log(LogLevel::Info, format!("auto-approved: {}", preview));
            }
            AppEvent::RoundComplete {
                round,
                turns_in_round,
            } => {
                self.round = round;
                self.current_phase = Phase::WaitingFollowUp;
                self.mode = AppMode::FollowUp;
                self.broadcast_control(OutboundEvent::RoundComplete {
                    round,
                    turns_in_round,
                });
                let mut textarea = tui_textarea::TextArea::default();
                textarea.set_cursor_line_style(ratatui::style::Style::default());
                self.follow_up_textarea = Some(textarea);
                self.log(
                    LogLevel::Info,
                    format!(
                        "Round {} complete ({} turns). Enter follow-up or q to quit.",
                        round, turns_in_round
                    ),
                );
            }
            AppEvent::DisplayReady {
                display_id,
                vnc_port,
            } => {
                let info = if let Some(port) = vnc_port {
                    format!(":{}  VNC:{}", display_id, port)
                } else {
                    format!(":{}", display_id)
                };
                self.display_info = Some(info.clone());
                let log_msg = if let Some(port) = vnc_port {
                    format!("Display :{} ready, VNC on port {}", display_id, port)
                } else {
                    format!("Display :{} ready", display_id)
                };
                self.log(LogLevel::Info, log_msg);
                self.broadcast_control(OutboundEvent::DisplayReady {
                    display_id,
                    vnc_port,
                });
            }
            AppEvent::SessionDirChanged { .. } => {
                // Only relevant for MCP mode; TUI ignores this.
            }
            AppEvent::PresenceUsageUpdate {
                total_tokens,
                context_window,
                usage_pct,
                provider,
                model,
            } => {
                self.presence_tokens = total_tokens;
                self.presence_context_window = context_window;
                self.presence_usage_pct = usage_pct;
                if self.presence_provider_name.is_none() {
                    self.presence_provider_name = Some(provider);
                    self.presence_model_name = Some(model);
                }
                self.broadcast_usage_update();
            }
            AppEvent::Tick => {
                self.tick_count += 1;
                // Update autonomy display
                let autonomy = self.autonomy.clone();
                if let Ok(state) = autonomy.try_read() {
                    self.autonomy_display = state.level.to_string();
                };
            }
            AppEvent::Key(key) => {
                self.handle_key(key);
            }
            AppEvent::Resize(_, _) => {}
            AppEvent::Quit => {
                self.should_quit = true;
            }
        }
    }
}

/// Format a human-readable summary of a model's JSON response.
/// Extracts command functions and their key parameters (command strings, paths, etc.)
/// instead of showing raw JSON.
fn format_model_summary(content: &str) -> String {
    let parsed: serde_json::Value = match serde_json::from_str(content) {
        Ok(v) => v,
        Err(_) => {
            // Not valid JSON; just show a truncated preview
            let preview = truncate_str(content, 200);
            return preview.to_string();
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

fn truncate_str(s: &str, max: usize) -> &str {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autonomy::{self, AutonomyState};

    fn test_app() -> App {
        let autonomy = autonomy::shared_autonomy(AutonomyState::default());
        App::new(
            "openai".to_string(),
            "gpt-5".to_string(),
            autonomy,
            std::path::PathBuf::from("/tmp/test_session"),
        )
    }

    #[test]
    fn app_new_defaults() {
        let app = test_app();
        assert_eq!(app.turn, 0);
        assert_eq!(app.budget_pct, 0.0);
        assert_eq!(app.session_tokens, 0);
        assert_eq!(app.current_phase, Phase::Idle);
        assert_eq!(app.mode, AppMode::Normal);
        assert!(!app.should_quit);
        assert_eq!(app.verbosity, Verbosity::Normal);
        assert!(app.auto_scroll);
        assert!(app.log_entries.is_empty());
    }

    #[test]
    fn app_log_adds_entries() {
        let mut app = test_app();
        app.log(LogLevel::Info, "hello".to_string());
        app.log(LogLevel::Error, "oops".to_string());
        assert_eq!(app.log_entries.len(), 2);
        assert_eq!(app.log_entries[0].content, "hello");
        assert_eq!(app.log_entries[1].level, LogLevel::Error);
    }

    #[test]
    fn app_log_ring_buffer() {
        let mut app = test_app();
        for i in 0..MAX_LOG_ENTRIES + 100 {
            app.log(LogLevel::Info, format!("msg {}", i));
        }
        assert_eq!(app.log_entries.len(), MAX_LOG_ENTRIES);
        // Oldest entries should be removed
        assert!(app.log_entries[0].content.contains("100"));
    }

    #[test]
    fn scroll_up_down() {
        let mut app = test_app();
        for i in 0..50 {
            app.log(LogLevel::Info, format!("line {}", i));
        }
        app.scroll_offset = 30;
        app.auto_scroll = false;

        app.scroll_up(5);
        assert_eq!(app.scroll_offset, 25);

        app.scroll_down(10);
        assert_eq!(app.scroll_offset, 35);
    }

    #[test]
    fn scroll_up_clamps_to_zero() {
        let mut app = test_app();
        app.scroll_offset = 3;
        app.scroll_up(10);
        assert_eq!(app.scroll_offset, 0);
    }

    #[test]
    fn scroll_home_end() {
        let mut app = test_app();
        for i in 0..50 {
            app.log(LogLevel::Info, format!("line {}", i));
        }
        app.scroll_home();
        assert_eq!(app.scroll_offset, 0);
        assert!(!app.auto_scroll);

        app.scroll_end();
        assert!(app.auto_scroll);
    }

    #[test]
    fn handle_key_quit() {
        let mut app = test_app();
        let key = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(app.handle_key(key));
        assert!(app.should_quit);
    }

    #[test]
    fn handle_key_ctrl_c() {
        let mut app = test_app();
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(app.handle_key(key));
        assert!(app.should_quit);
    }

    #[test]
    fn handle_key_verbosity_cycle() {
        let mut app = test_app();
        assert_eq!(app.verbosity, Verbosity::Normal);
        let key = KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE);
        app.handle_key(key);
        assert_eq!(app.verbosity, Verbosity::Verbose);
        app.handle_key(key);
        assert_eq!(app.verbosity, Verbosity::Debug);
        app.handle_key(key);
        assert_eq!(app.verbosity, Verbosity::Quiet);
    }

    #[test]
    fn handle_key_help_toggle() {
        let mut app = test_app();
        let key = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE);
        app.handle_key(key);
        assert_eq!(app.mode, AppMode::Help);

        // Any key closes help
        let key = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        app.handle_key(key);
        assert_eq!(app.mode, AppMode::Normal);
    }

    #[test]
    fn handle_key_scroll() {
        let mut app = test_app();
        for i in 0..50 {
            app.log(LogLevel::Info, format!("line {}", i));
        }
        app.scroll_offset = 25;
        app.auto_scroll = false;

        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        app.handle_key(up);
        assert_eq!(app.scroll_offset, 24);

        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        app.handle_key(down);
        assert_eq!(app.scroll_offset, 25);
    }

    #[test]
    fn handle_event_turn_started() {
        let mut app = test_app();
        app.handle_event(AppEvent::TurnStarted {
            turn: 3,
            budget_pct: 25.0,
            remaining: 150_000,
        });
        assert_eq!(app.turn, 3);
        assert_eq!(app.budget_pct, 25.0);
        assert_eq!(app.current_phase, Phase::Thinking);
    }

    #[test]
    fn handle_event_agent_output() {
        let mut app = test_app();
        app.handle_event(AppEvent::AgentOutput {
            stdout: "line1\nline2".to_string(),
            stderr: "warn".to_string(),
        });
        assert_eq!(app.log_entries.len(), 3);
        assert_eq!(app.log_entries[0].level, LogLevel::Agent);
        assert_eq!(app.log_entries[2].level, LogLevel::Warn);
    }

    #[test]
    fn handle_event_task_complete() {
        let mut app = test_app();
        app.handle_event(AppEvent::TaskComplete {
            reason: "Task complete".to_string(),
        });
        assert_eq!(app.current_phase, Phase::Done);
    }

    #[test]
    fn handle_event_human_question() {
        let mut app = test_app();
        app.handle_event(AppEvent::HumanQuestionDetected {
            question: "Which database?".to_string(),
        });
        assert_eq!(app.mode, AppMode::AskHuman);
        assert_eq!(app.current_phase, Phase::WaitingHuman);
        assert!(app.human_textarea.is_some());
        assert_eq!(app.human_question.as_deref(), Some("Which database?"));
    }

    #[test]
    fn handle_key_human_enter_empty_keeps_mode() {
        let mut app = test_app();
        app.handle_event(AppEvent::HumanQuestionDetected {
            question: "Which database?".to_string(),
        });

        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.handle_key(key));
        assert_eq!(app.mode, AppMode::AskHuman);
        assert_eq!(app.current_phase, Phase::WaitingHuman);
        assert!(app.human_textarea.is_some());
        assert!(app
            .log_entries
            .iter()
            .any(|e| e.content.contains("Response cannot be empty")));
    }

    #[test]
    fn handle_event_budget_warning() {
        let mut app = test_app();
        app.handle_event(AppEvent::BudgetWarning {
            pct: 87.5,
            remaining: 25_000,
        });
        assert_eq!(app.budget_pct, 87.5);
        assert_eq!(app.log_entries.len(), 1);
    }

    #[test]
    fn handle_event_budget_exhausted() {
        let mut app = test_app();
        app.handle_event(AppEvent::BudgetExhausted { remaining: 100 });
        assert_eq!(app.budget_pct, 100.0);
        assert_eq!(app.current_phase, Phase::Done);
    }

    #[test]
    fn handle_event_safety_cap() {
        let mut app = test_app();
        app.handle_event(AppEvent::SafetyCapReached);
        assert_eq!(app.current_phase, Phase::Done);
    }

    #[test]
    fn handle_event_done_signal_with_message() {
        let mut app = test_app();
        app.handle_event(AppEvent::DoneSignal {
            message: Some("All done!".to_string()),
        });
        assert_eq!(app.current_phase, Phase::Done);
        assert_eq!(app.log_entries.len(), 1);
        assert_eq!(app.log_entries[0].content, "All done!");
    }

    #[test]
    fn handle_event_done_signal_without_message() {
        let mut app = test_app();
        app.handle_event(AppEvent::DoneSignal { message: None });
        assert_eq!(app.current_phase, Phase::Done);
        assert!(app.log_entries.is_empty());
    }

    #[test]
    fn handle_event_tick() {
        let mut app = test_app();
        app.handle_event(AppEvent::Tick);
        assert_eq!(app.tick_count, 1);
    }

    #[test]
    fn handle_event_quit() {
        let mut app = test_app();
        app.handle_event(AppEvent::Quit);
        assert!(app.should_quit);
    }

    #[test]
    fn bottom_panel_height_normal() {
        let app = test_app();
        assert_eq!(app.bottom_panel_height(), 0);
    }

    #[test]
    fn bottom_panel_height_approval() {
        let mut app = test_app();
        app.mode = AppMode::Approval;
        assert_eq!(app.bottom_panel_height(), 6);
    }

    #[test]
    fn bottom_panel_height_ask_human() {
        let mut app = test_app();
        app.mode = AppMode::AskHuman;
        assert_eq!(app.bottom_panel_height(), 5);
    }

    #[test]
    fn bottom_panel_height_approval_multiline() {
        let mut app = test_app();
        app.mode = AppMode::Approval;
        let (tx, _rx) = oneshot::channel();
        app.pending_approvals.push_back(PendingApproval {
            id: 1,
            command_preview: "echo a\necho b\necho c\necho d\necho e".to_string(),
            category: "command_exec".to_string(),
            responder: tx,
        });
        // 5 lines + 3 = 8
        assert_eq!(app.bottom_panel_height(), 8);
    }

    #[test]
    fn bottom_panel_height_approval_clamped() {
        let mut app = test_app();
        app.mode = AppMode::Approval;
        let (tx, _rx) = oneshot::channel();
        let long_cmd = (0..30).map(|i| format!("echo {}", i)).collect::<Vec<_>>().join("\n");
        app.pending_approvals.push_back(PendingApproval {
            id: 1,
            command_preview: long_cmd,
            category: "command_exec".to_string(),
            responder: tx,
        });
        // 30 lines + 3 = 33, but clamped to 20
        assert_eq!(app.bottom_panel_height(), 20);
    }

    #[test]
    fn phase_display() {
        assert_eq!(Phase::Thinking, Phase::Thinking);
        assert_ne!(Phase::Thinking, Phase::Done);
    }

    #[test]
    fn truncate_str_short() {
        assert_eq!(truncate_str("hello", 10), "hello");
    }

    #[test]
    fn truncate_str_long() {
        assert_eq!(truncate_str("hello world", 5), "hello");
    }

    #[test]
    fn format_model_summary_exec() {
        let json = r#"{"commands":[{"function":"execAsAgent","nonce":1,"command":"ls -la /tmp"}]}"#;
        let summary = format_model_summary(json);
        assert!(summary.contains("exec: ls -la /tmp"));
    }

    #[test]
    fn format_model_summary_edit() {
        let json = r#"{"commands":[{"function":"editFile","nonce":3,"file_path":"/tmp/test.rs","operation":"write","content":"fn main(){}"}]}"#;
        let summary = format_model_summary(json);
        assert!(summary.contains("edit: /tmp/test.rs (write)"));
    }

    #[test]
    fn format_model_summary_multiple() {
        let json = r#"{"commands":[{"function":"execAsAgent","nonce":1,"command":"ls"},{"function":"inspectPath","nonce":2,"path":"/tmp"}]}"#;
        let summary = format_model_summary(json);
        assert!(summary.contains("exec: ls"));
        assert!(summary.contains("inspect: /tmp"));
        assert!(summary.contains(" | "));
    }

    #[test]
    fn format_model_summary_done() {
        let json = r#"{"commands":[],"done":true}"#;
        let summary = format_model_summary(json);
        assert_eq!(summary, "done signal");
    }

    #[test]
    fn format_model_summary_invalid_json() {
        let text = "This is not JSON";
        let summary = format_model_summary(text);
        assert_eq!(summary, "This is not JSON");
    }

    #[test]
    fn format_model_summary_ask_human() {
        let json =
            r#"{"commands":[{"function":"askHuman","nonce":1,"question":"What should I do?"}]}"#;
        let summary = format_model_summary(json);
        assert!(summary.contains("ask: What should I do?"));
    }

    #[test]
    fn approval_key_approve() {
        let mut app = test_app();
        let (tx, rx) = oneshot::channel();
        app.mode = AppMode::Approval;
        app.pending_approvals.push_back(PendingApproval {
            id: 1,
            command_preview: "rm -rf /tmp".to_string(),
            category: "destructive".to_string(),
            responder: tx,
        });

        let key = KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE);
        assert!(app.handle_key(key));
        assert_eq!(app.mode, AppMode::Normal);

        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let resp = rt.block_on(async { rx.await.unwrap() });
        assert_eq!(resp, ApprovalResponse::Approve);
    }

    #[test]
    fn approval_key_deny() {
        let mut app = test_app();
        let (tx, rx) = oneshot::channel();
        app.mode = AppMode::Approval;
        app.pending_approvals.push_back(PendingApproval {
            id: 2,
            command_preview: "rm -rf /".to_string(),
            category: "destructive".to_string(),
            responder: tx,
        });

        let key = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE);
        assert!(app.handle_key(key));

        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let resp = rt.block_on(async { rx.await.unwrap() });
        assert_eq!(resp, ApprovalResponse::Deny);
    }

    #[test]
    fn approval_key_skip() {
        let mut app = test_app();
        let (tx, rx) = oneshot::channel();
        app.mode = AppMode::Approval;
        app.pending_approvals.push_back(PendingApproval {
            id: 3,
            command_preview: "test".to_string(),
            category: "command_exec".to_string(),
            responder: tx,
        });

        let key = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE);
        assert!(app.handle_key(key));

        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let resp = rt.block_on(async { rx.await.unwrap() });
        assert_eq!(resp, ApprovalResponse::Skip);
    }

    #[test]
    fn approval_key_approve_all() {
        let mut app = test_app();
        let (tx, rx) = oneshot::channel();
        app.mode = AppMode::Approval;
        app.pending_approvals.push_back(PendingApproval {
            id: 4,
            command_preview: "test".to_string(),
            category: "command_exec".to_string(),
            responder: tx,
        });

        let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        assert!(app.handle_key(key));

        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let resp = rt.block_on(async { rx.await.unwrap() });
        assert_eq!(resp, ApprovalResponse::ApproveAll);
    }

    #[test]
    fn handle_event_orchestrator_progress() {
        let mut app = test_app();
        app.handle_event(AppEvent::OrchestratorProgress {
            turn: 7,
            status: "running".to_string(),
            last_action: "Analyzing codebase".to_string(),
        });
        assert_eq!(app.turn, 7);
        assert_eq!(app.current_phase, Phase::Orchestrating);
        assert_eq!(app.log_entries.len(), 1);
        assert_eq!(app.log_entries[0].level, LogLevel::SubAgent);
        assert!(app.log_entries[0].content.contains("Orchestrator T7"));
        assert!(app.log_entries[0].content.contains("Analyzing codebase"));
    }

    #[test]
    fn handle_event_orchestrator_progress_empty_action() {
        let mut app = test_app();
        app.handle_event(AppEvent::OrchestratorProgress {
            turn: 3,
            status: "spawning".to_string(),
            last_action: String::new(),
        });
        assert_eq!(app.turn, 3);
        assert_eq!(app.current_phase, Phase::Orchestrating);
        assert!(app.log_entries[0].content.contains("spawning"));
        assert!(!app.log_entries[0].content.contains("—"));
    }

    #[test]
    fn handle_event_streaming_delta_accumulates() {
        let mut app = test_app();
        app.handle_event(AppEvent::ModelResponseDelta {
            text: "Hello ".to_string(),
        });
        app.handle_event(AppEvent::ModelResponseDelta {
            text: "world".to_string(),
        });
        assert_eq!(app.streaming_buffer, "Hello world");
    }

    #[test]
    fn handle_event_model_response_clears_streaming_buffer() {
        let mut app = test_app();
        app.streaming_buffer = "partial text".to_string();
        app.handle_event(AppEvent::ModelResponse {
            turn: 1,
            content: r#"{"commands":[]}"#.to_string(),
            usage: crate::provider::TokenUsage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
            },
            reasoning: None,
        });
        assert!(app.streaming_buffer.is_empty());
    }
}
