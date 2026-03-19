use crate::autonomy::SharedAutonomy;
use crate::control;
use crate::event::{AppEvent, ApprovalResponse, ControlMsg};
pub use crate::types::{LogLevel, Phase, Verbosity};
use crate::types::OutboundEvent;
use crate::{knowledge, session_log};
use crate::tui::layout::PanelConfig;
use chrono::Local;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::{HashSet, VecDeque};
use tokio::sync::oneshot;

const MAX_LOG_ENTRIES: usize = 10_000;

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

/// Which subsystem produced a log entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogSource {
    System,
    Agent,
    /// Server-side text presence model.
    Presence,
    /// Browser-side live presence (voice/video).
    Live,
}

/// Which log tab is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogTab {
    All,
    Agent,
    Presence,
}

impl LogTab {
    pub fn next(self) -> Self {
        match self {
            Self::All => Self::Agent,
            Self::Agent => Self::Presence,
            Self::Presence => Self::All,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Agent => "Agent",
            Self::Presence => "Presence",
        }
    }

    pub fn includes(self, source: LogSource) -> bool {
        match self {
            Self::All => true,
            Self::Agent => !matches!(source, LogSource::Presence | LogSource::Live),
            Self::Presence => source != LogSource::Agent,
        }
    }
}

/// A single log entry.
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub ts: String,
    pub level: LogLevel,
    pub content: String,
    pub source: LogSource,
    pub turn: Option<usize>,
}

/// Pending approval waiting for user response.
pub struct PendingApproval {
    #[allow(dead_code)]
    pub id: u64,
    pub command_preview: String,
    pub category: String,
    pub responder: oneshot::Sender<ApprovalResponse>,
}

/// Per-connection view state: scroll position, verbosity, inspect mode, etc.
/// Each browser tab / TUI terminal gets its own `ViewState`.
///
/// `App::mode` tracks the shared interactive mode (Approval, AskHuman,
/// FollowUp).  `ViewState` adds per-connection overlays: Help and Inspect.
#[derive(Debug, Clone)]
pub struct ViewState {
    pub scroll_offset: usize,
    pub auto_scroll: bool,
    pub verbosity: Verbosity,
    pub inspect_index: Option<usize>,
    pub inspect_scroll: u16,
    pub log_tab: LogTab,
    pub expanded_turns: HashSet<usize>,
    pub focused_line: Option<usize>,
    /// Per-connection overlays on top of the shared `App::mode`.
    pub show_help: bool,
    pub show_inspect: bool,
}

impl Default for ViewState {
    fn default() -> Self {
        Self {
            scroll_offset: 0,
            auto_scroll: true,
            verbosity: Verbosity::Normal,
            inspect_index: None,
            inspect_scroll: 0,
            log_tab: LogTab::All,
            expanded_turns: HashSet::new(),
            focused_line: None,
            show_help: false,
            show_inspect: false,
        }
    }
}

impl ViewState {
    /// Return indices into `app.log_entries` that pass the current view filters.
    pub fn filtered_indices(&self, app: &App) -> Vec<usize> {
        app.log_entries
            .iter()
            .enumerate()
            .filter_map(|(idx, entry)| {
                if !self.verbosity.includes(&entry.level) {
                    return None;
                }
                if !self.log_tab.includes(entry.source) {
                    return None;
                }
                // Turn grouping: if entry belongs to a collapsed turn, hide it
                // unless it's the first entry of that turn (the summary line).
                if let Some(t) = entry.turn {
                    if !self.expanded_turns.contains(&t) {
                        let dominated = app
                            .log_entries
                            .iter()
                            .take(idx)
                            .any(|e| {
                                e.turn == Some(t)
                                    && self.verbosity.includes(&e.level)
                                    && self.log_tab.includes(e.source)
                            });
                        if dominated {
                            return None;
                        }
                    }
                }
                Some(idx)
            })
            .collect()
    }

    pub fn scroll_to_bottom(&mut self, app: &App) {
        let total = self.filtered_indices(app).len();
        self.scroll_offset = total.saturating_sub(1);
    }

    pub fn scroll_up(&mut self, n: usize) {
        self.auto_scroll = false;
        self.scroll_offset = self.scroll_offset.saturating_sub(n);
    }

    pub fn scroll_down(&mut self, n: usize, app: &App) {
        self.auto_scroll = false;
        let total = self.filtered_indices(app).len();
        self.scroll_offset = (self.scroll_offset + n).min(total.saturating_sub(1));
    }

    pub fn scroll_page_up(&mut self, page_size: usize) {
        self.scroll_up(page_size);
    }

    pub fn scroll_page_down(&mut self, page_size: usize, app: &App) {
        self.scroll_down(page_size, app);
    }

    pub fn scroll_home(&mut self) {
        self.auto_scroll = false;
        self.scroll_offset = 0;
    }

    pub fn scroll_end(&mut self, app: &App) {
        self.auto_scroll = true;
        self.scroll_to_bottom(app);
    }

    pub fn focus_index(&self, app: &App) -> Option<usize> {
        let filtered = self.filtered_indices(app);
        if filtered.is_empty() {
            return None;
        }
        if self.auto_scroll {
            return filtered.last().copied();
        }
        filtered.get(self.scroll_offset).copied()
    }

    fn clamp_view_to_filtered(&mut self, app: &App) {
        let total = self.filtered_indices(app).len();
        if total == 0 {
            self.scroll_offset = 0;
            self.inspect_index = None;
            return;
        }
        self.scroll_offset = self.scroll_offset.min(total.saturating_sub(1));
    }

    fn open_inspect_mode(&mut self, app: &App) -> bool {
        self.inspect_index = self.focus_index(app);
        if self.inspect_index.is_some() {
            self.inspect_scroll = 0;
            self.show_inspect = true;
            return true;
        }
        false
    }

    fn ensure_inspect_index(&mut self, app: &App) {
        let filtered = self.filtered_indices(app);
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

    fn move_inspect(&mut self, delta: isize, app: &App) {
        let filtered = self.filtered_indices(app);
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

    fn jump_inspect_to_edge(&mut self, start: bool, app: &App) {
        let filtered = self.filtered_indices(app);
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

    /// Get the turn number of the currently focused log entry.
    fn focused_turn(&self, app: &App) -> Option<usize> {
        let idx = self.focus_index(app)?;
        app.log_entries.get(idx).and_then(|e| e.turn)
    }

    /// Toggle expand/collapse for the turn of the currently focused log entry.
    fn toggle_focused_turn_expand(&mut self, app: &App) {
        if let Some(turn) = self.focused_turn(app) {
            if self.expanded_turns.contains(&turn) {
                self.expanded_turns.remove(&turn);
            } else {
                self.expanded_turns.insert(turn);
            }
            self.clamp_view_to_filtered(app);
        }
    }

    /// Collapse the turn of the currently focused log entry.
    fn collapse_focused_turn(&mut self, app: &App) {
        if let Some(turn) = self.focused_turn(app) {
            self.expanded_turns.remove(&turn);
            self.clamp_view_to_filtered(app);
        }
    }

    /// Handle a key event that only affects view state.
    /// Returns true if the event was consumed (view-only key).
    /// Returns false if the key should be handled by App (shared state key).
    pub fn handle_key(&mut self, key: KeyEvent, app: &App) -> bool {
        if self.show_help {
            // Any key closes help
            self.show_help = false;
            return true;
        }
        if self.show_inspect {
            return self.handle_inspect_key(key, app);
        }
        // In shared-mode states (Approval, AskHuman, FollowUp), let App handle keys
        match app.mode {
            AppMode::Approval | AppMode::AskHuman | AppMode::FollowUp => false,
            AppMode::Normal | AppMode::Help | AppMode::Inspect => {
                self.handle_normal_view_key(key, app)
            }
        }
    }

    fn handle_normal_view_key(&mut self, key: KeyEvent, app: &App) -> bool {
        match key.code {
            KeyCode::Char('v') => {
                self.verbosity = self.verbosity.next();
                self.clamp_view_to_filtered(app);
                true
            }
            KeyCode::Char('i') => {
                self.open_inspect_mode(app)
            }
            KeyCode::Char('?') => {
                self.show_help = true;
                true
            }
            // Enter or Right arrow: toggle turn expansion on the focused entry
            KeyCode::Enter | KeyCode::Right => {
                self.toggle_focused_turn_expand(app);
                true
            }
            // Left arrow: collapse focused turn
            KeyCode::Left => {
                self.collapse_focused_turn(app);
                true
            }
            // Tab: cycle log tab
            KeyCode::Tab => {
                self.log_tab = self.log_tab.next();
                self.clamp_view_to_filtered(app);
                true
            }
            KeyCode::Char('1') => {
                self.log_tab = LogTab::All;
                self.clamp_view_to_filtered(app);
                true
            }
            KeyCode::Char('2') => {
                self.log_tab = LogTab::Agent;
                self.clamp_view_to_filtered(app);
                true
            }
            KeyCode::Char('3') => {
                self.log_tab = LogTab::Presence;
                self.clamp_view_to_filtered(app);
                true
            }
            KeyCode::Up => {
                self.scroll_up(1);
                true
            }
            KeyCode::Down => {
                self.scroll_down(1, app);
                true
            }
            KeyCode::PageUp => {
                self.scroll_page_up(20);
                true
            }
            KeyCode::PageDown => {
                self.scroll_page_down(20, app);
                true
            }
            KeyCode::Home => {
                self.scroll_home();
                true
            }
            KeyCode::End => {
                self.scroll_end(app);
                true
            }
            _ => false,
        }
    }

    fn handle_inspect_key(&mut self, key: KeyEvent, app: &App) -> bool {
        match key.code {
            KeyCode::Esc | KeyCode::Char('i') | KeyCode::Enter => {
                self.show_inspect = false;
                true
            }
            // Up/Down scroll within the current entry
            KeyCode::Up | KeyCode::Char('k') => {
                self.inspect_scroll = self.inspect_scroll.saturating_sub(1);
                true
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.inspect_scroll = self.inspect_scroll.saturating_add(1);
                true
            }
            // Left/Right navigate between entries
            KeyCode::Left | KeyCode::PageUp => {
                self.move_inspect(-1, app);
                self.inspect_scroll = 0;
                true
            }
            KeyCode::Right | KeyCode::PageDown => {
                self.move_inspect(1, app);
                self.inspect_scroll = 0;
                true
            }
            KeyCode::Home => {
                self.jump_inspect_to_edge(true, app);
                self.inspect_scroll = 0;
                true
            }
            KeyCode::End => {
                self.jump_inspect_to_edge(false, app);
                self.inspect_scroll = 0;
                true
            }
            KeyCode::Char('v') => {
                self.verbosity = self.verbosity.next();
                self.clamp_view_to_filtered(app);
                self.ensure_inspect_index(app);
                true
            }
            _ => false,
        }
    }
}

/// The main application state.
pub struct App {
    // Display
    pub log_entries: VecDeque<LogEntry>,

    // Status
    pub provider_name: String,
    pub model_name: String,
    pub turn: usize,
    pub budget_pct: f64,
    pub current_phase: Phase,
    pub autonomy_display: String,

    // Panels
    pub panels: PanelConfig,
    /// Shared interactive mode (Approval, AskHuman, FollowUp, Normal).
    /// Per-connection overlays (Help, Inspect) are in `ViewState`.
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

    // Animation (kept in App for server-side idle tick tracking)
    pub tick_count: usize,

    /// Verbosity override — consumed by the TUI event loop and applied to all
    /// connections' ViewStates. Set by --verbose flag at startup or by control socket.
    pub pending_verbosity: Option<Verbosity>,

    // Streaming buffer for incremental text deltas
    pub streaming_buffer: String,

    // Multi-round follow-up
    pub round: usize,
    pub follow_up_textarea: Option<tui_textarea::TextArea<'static>>,
    pub follow_up_tx: Option<tokio::sync::mpsc::Sender<String>>,
    /// When presence layer is active, follow-up input goes here instead.
    pub presence_tx: Option<tokio::sync::mpsc::Sender<String>>,
    /// Direct task dispatch channel — bypasses server-side presence entirely.
    /// Used by StartTask (from browser live model, control socket, MCP) so
    /// tasks don't get re-processed by the server-side presence model.
    pub task_tx: Option<tokio::sync::mpsc::Sender<presence_core::TaskEnvelope>>,

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
    // Stateful phase tracking for presence event dedup (used by filter_event)
    pub last_presence_phase: String,
    // Shared agent state snapshot for presence layer status queries (check_status, query_detail)
    pub presence_agent_state: Option<std::sync::Arc<std::sync::Mutex<crate::presence::AgentStateSnapshot>>>,
    /// Shared flag to pause/resume server-side presence (voice mutual exclusion).
    pub presence_paused: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,

    // Project paths for query_detail and recall_memory socket commands
    pub project_root: Option<std::path::PathBuf>,
    pub knowledge_path: Option<std::path::PathBuf>,

    // Shared session log for voice log persistence
    pub session_log: Option<std::sync::Arc<std::sync::Mutex<crate::session_log::SessionLog>>>,

    // Shared presence session for event window population
    pub presence_session: Option<std::sync::Arc<std::sync::Mutex<crate::presence::PresenceSession>>>,

    // Voice turn counter — increments on each voice model response (thinking block).
    // Used to group voice logs per response for collapse in the TUI.
    pub voice_turn: usize,

    // Voice transcript accumulation buffer — fragments are buffered and flushed
    // as a single log entry when a boundary event arrives or after an idle period.
    voice_transcript_buffer: String,
    voice_transcript_idle_ticks: usize,
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
            pending_verbosity: None,
            streaming_buffer: String::new(),
            round: 1,
            follow_up_textarea: None,
            follow_up_tx: None,
            presence_tx: None,
            task_tx: None,
            display_info: None,
            presence_provider_name: None,
            presence_model_name: None,
            presence_tokens: 0,
            presence_usage_pct: 0.0,
            presence_context_window: 0,
            presence_event_tx: None,
            last_presence_phase: String::new(),
            presence_agent_state: None,
            presence_paused: None,
            project_root: None,
            knowledge_path: None,
            session_log: None,
            presence_session: None,
            voice_turn: 0,
            voice_transcript_buffer: String::new(),
            voice_transcript_idle_ticks: 0,
        }
    }

    pub fn set_follow_up_sender(&mut self, tx: tokio::sync::mpsc::Sender<String>) {
        self.follow_up_tx = Some(tx);
    }

    pub fn set_presence_sender(&mut self, tx: tokio::sync::mpsc::Sender<String>) {
        self.presence_tx = Some(tx);
    }

    pub fn set_task_sender(&mut self, tx: tokio::sync::mpsc::Sender<presence_core::TaskEnvelope>) {
        self.task_tx = Some(tx);
    }

    pub fn set_presence_event_sender(
        &mut self,
        tx: tokio::sync::mpsc::Sender<crate::presence::PresenceEvent>,
    ) {
        self.presence_event_tx = Some(tx);
    }

    pub fn set_presence_agent_state(
        &mut self,
        state: std::sync::Arc<std::sync::Mutex<crate::presence::AgentStateSnapshot>>,
    ) {
        self.presence_agent_state = Some(state);
    }

    pub fn set_presence_paused_flag(&mut self, flag: std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        self.presence_paused = Some(flag);
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
    ///
    /// Delegates to `presence::filter_event()` which provides stateful phase
    /// dedup and covers all push-worthy event types (including ModelResponse and
    /// AgentStarted, which are needed for phase narration).
    ///
    /// Also updates the shared `AgentStateSnapshot` so presence tool calls
    /// like `check_status` and `query_detail` return accurate data.
    fn forward_to_presence(&mut self, event: &AppEvent) {
        // Update the agent state snapshot (sees ALL events, not just filtered ones)
        if let Some(ref state) = self.presence_agent_state {
            crate::presence::update_agent_state(event, state);
        }
        // Filter and forward push-worthy events to presence
        if let Some(ref tx) = self.presence_event_tx {
            if let Some(pe) = crate::presence::filter_event(event, &mut self.last_presence_phase) {
                // Record into the presence session event window (for browser replay)
                if let Some(ref ps) = self.presence_session {
                    if let Ok(mut session) = ps.lock() {
                        session.record_event(pe.clone());
                    }
                }
                let _ = tx.try_send(pe);
            }
        }
    }

    pub fn set_control_socket(&mut self, tx: tokio::sync::broadcast::Sender<String>) {
        self.control_tx = Some(tx);
    }

    pub fn log(&mut self, level: LogLevel, content: String) {
        self.log_sourced(level, content, LogSource::System, None);
    }

    pub fn log_sourced(
        &mut self,
        level: LogLevel,
        content: String,
        source: LogSource,
        turn: Option<usize>,
    ) {
        if self.log_entries.len() >= MAX_LOG_ENTRIES {
            self.log_entries.pop_front();
        }
        self.log_entries.push_back(LogEntry {
            ts: Local::now().format("%H:%M:%S").to_string(),
            level,
            content,
            source,
            turn,
        });
        // Note: auto_scroll is now per-connection in ViewState.
        // Each connection's render loop handles its own scroll position.
    }

    /// Flush accumulated voice transcript fragments into a single Info log entry.
    fn flush_voice_transcript(&mut self) {
        if !self.voice_transcript_buffer.is_empty() {
            let text = self.voice_transcript_buffer.trim().to_string();
            if !text.is_empty() {
                let vt = if self.voice_turn > 0 { Some(self.voice_turn) } else { None };
                self.log_sourced(LogLevel::Info, format!("[Presence] {}", text), LogSource::Live, vt);
            }
            self.voice_transcript_buffer.clear();
            self.voice_transcript_idle_ticks = 0;
        }
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
            _ => {
                // Show a slim reminder bar when browsing during follow-up or after task done
                if self.is_follow_up_browsing()
                {
                    3
                } else {
                    0
                }
            }
        }
    }

    /// Handle a key event that modifies shared App state.
    /// Returns true if the event was consumed.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        // Global quit
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return true;
        }

        match self.mode {
            AppMode::Approval => self.handle_approval_key(key),
            AppMode::AskHuman => self.handle_human_key(key),
            AppMode::FollowUp => self.handle_follow_up_key(key),
            AppMode::Normal | AppMode::Help | AppMode::Inspect => {
                // Shared-state keys in Normal mode
                match key.code {
                    KeyCode::Char('q') => {
                        self.should_quit = true;
                        true
                    }
                    KeyCode::Char('f') => {
                        // Re-open follow-up input if a round is waiting or task is done
                        if (self.current_phase == Phase::WaitingFollowUp
                            || self.current_phase == Phase::Done)
                            && self.follow_up_textarea.is_some()
                        {
                            self.mode = AppMode::FollowUp;
                            true
                        } else {
                            false
                        }
                    }
                    KeyCode::Char('+') | KeyCode::Char('=') => {
                        self.cycle_autonomy_up();
                        true
                    }
                    KeyCode::Char('-') => {
                        self.cycle_autonomy_down();
                        true
                    }
                    _ => false,
                }
            }
        }
    }

    /// Human-readable source for approval log messages.
    /// If a live browser voice model is active, approvals come from voice.
    fn approval_source(&self) -> &'static str {
        if let Some(ref flag) = self.presence_paused {
            if flag.load(std::sync::atomic::Ordering::Relaxed) > 0 {
                return "voice";
            }
        }
        "control socket"
    }

    /// Whether we're browsing the log but a follow-up is still pending.
    pub fn is_follow_up_browsing(&self) -> bool {
        self.mode != AppMode::FollowUp
            && (self.current_phase == Phase::WaitingFollowUp
                || self.current_phase == Phase::Done)
            && self.follow_up_textarea.is_some()
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
                // Temporarily leave follow-up input to browse the log.
                // The follow-up panel hides but the session stays alive.
                // Press 'f' in Normal mode to re-open the follow-up input.
                self.mode = AppMode::Normal;
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
                    let via = self.approval_source();
                    self.log(LogLevel::Info, format!("Approved via {} (turn {})", via, id));
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
                    let via = self.approval_source();
                    self.log(LogLevel::Info, format!("Denied via {} (turn {})", via, id));
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
                    let via = self.approval_source();
                    self.log(LogLevel::Info, format!("Skipped via {} (turn {})", via, id));
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
                    let via = self.approval_source();
                    self.log(LogLevel::Info, format!("Approve-all via {} (turn {})", via, id));
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
                    "quiet" => Some(Verbosity::Quiet),
                    "normal" => Some(Verbosity::Normal),
                    "verbose" => Some(Verbosity::Verbose),
                    "debug" => Some(Verbosity::Debug),
                    _ => {
                        self.log(
                            LogLevel::Warn,
                            format!("Unknown verbosity level: {}", level),
                        );
                        None
                    }
                };
                if let Some(v) = new_verbosity {
                    self.pending_verbosity = Some(v);
                    self.log(
                        LogLevel::Info,
                        format!("Verbosity set to {} via control socket", v.label()),
                    );
                }
            }
            ControlMsg::StartTask { task, orchestrate } => {
                // StartTask is an explicit command — bypass server-side presence
                // and dispatch directly to the worker task channel. This avoids
                // the server-side presence re-processing decisions the browser
                // live model (or control socket / MCP) already made.
                if self.current_phase == Phase::WaitingFollowUp
                    || self.current_phase == Phase::Done
                    || self.current_phase == Phase::Idle
                {
                    let dispatched = if let Some(ref tx) = self.task_tx {
                        let envelope = presence_core::TaskEnvelope {
                            task: task.clone(),
                            force_direct: orchestrate == Some(false),
                            context_hints: vec![],
                        };
                        tx.try_send(envelope).is_ok()
                    } else if let Some(ref tx) = self.follow_up_tx {
                        tx.try_send(task.clone()).is_ok()
                    } else {
                        false
                    };
                    if dispatched {
                        self.follow_up_textarea = None;
                        self.mode = AppMode::Normal;
                        self.current_phase = Phase::Thinking;
                        self.round += 1;
                        self.log_sourced(LogLevel::Info, format!("[runtime] Task dispatched: {}", truncate_str(&task, 80)), LogSource::Live, None);
                    } else {
                        self.log(
                            LogLevel::Warn,
                            format!(
                                "start_task: dispatch failed (phase: {:?}, task_tx: {}, follow_up_tx: {})",
                                self.current_phase,
                                self.task_tx.is_some(),
                                self.follow_up_tx.is_some(),
                            ),
                        );
                    }
                } else {
                    self.log(
                        LogLevel::Warn,
                        format!("start_task: agent is busy (phase: {:?})", self.current_phase),
                    );
                }
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
                // Accept follow-ups when waiting for follow-up or task is done,
                // regardless of AppMode (user may have pressed Esc to browse logs).
                if self.current_phase == Phase::WaitingFollowUp
                    || self.current_phase == Phase::Done
                {
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
            ControlMsg::QueryDetail { scope, target } => {
                self.log(
                    LogLevel::Info,
                    format!("Query detail request: {}", scope),
                );
                let result = match scope.as_str() {
                    "current_turn" => format!("Turn: {}\nPhase: {:?}\nBudget: {:.0}%", self.turn, self.current_phase, self.budget_pct),
                    "logs" => {
                        let entries = session_log::recent_entries(&self.log_dir, 20);
                        if entries.is_empty() { "No log entries yet.".to_string() } else { entries.join("\n") }
                    }
                    "diff" => {
                        if let Some(ref root) = self.project_root {
                            match std::process::Command::new("git")
                                .args(["diff", "--stat"])
                                .current_dir(root)
                                .output()
                            {
                                Ok(o) => {
                                    let stdout = String::from_utf8_lossy(&o.stdout);
                                    if stdout.trim().is_empty() { "No changes.".to_string() } else { stdout.to_string() }
                                }
                                Err(e) => format!("Failed to run git diff: {}", e),
                            }
                        } else {
                            "No project root available.".to_string()
                        }
                    }
                    "file" => {
                        match target.as_deref() {
                            Some(path) => match std::fs::read_to_string(path) {
                                Ok(content) => content.lines().take(200).collect::<Vec<_>>().join("\n"),
                                Err(e) => format!("Failed to read file: {}", e),
                            },
                            None => "Error: target file path is required".to_string(),
                        }
                    }
                    other => format!("Unknown scope: {}", other),
                };
                self.broadcast_control(OutboundEvent::CommandResult {
                    action: "query_detail".to_string(),
                    ok: true,
                    message: result,
                    data: None,
                });
            }
            ControlMsg::RecallMemory { keywords, tags, channel } => {
                let kws = keywords.as_deref().unwrap_or(&[]);
                self.log(
                    LogLevel::Info,
                    format!("Memory recall request: {:?}", kws),
                );
                let result = if let Some(ref kp) = self.knowledge_path {
                    let query = knowledge::KnowledgeQuery {
                        keywords: if kws.is_empty() { None } else { Some(kws.to_vec()) },
                        tags,
                        channel,
                        ..Default::default()
                    };
                    match knowledge::load(kp) {
                        Ok(store) => {
                            let results = knowledge::query(&store, &query);
                            if results.is_empty() {
                                // Fall back to session log search
                                let entries = session_log::recent_entries(&self.log_dir, 100);
                                if let Some(ref kw_list) = query.keywords {
                                    let matched: Vec<&String> = entries.iter()
                                        .filter(|e| {
                                            let lower = e.to_lowercase();
                                            kw_list.iter().any(|kw| lower.contains(&kw.to_lowercase()))
                                        })
                                        .collect();
                                    if matched.is_empty() {
                                        "No matching memories found.".to_string()
                                    } else {
                                        matched.into_iter().take(10).cloned().collect::<Vec<_>>().join("\n")
                                    }
                                } else {
                                    "No matching memories found.".to_string()
                                }
                            } else {
                                results.iter()
                                    .map(|e| format!("[{}] {}: {}", e.channel, e.key, e.summary))
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            }
                        }
                        Err(_) => "Failed to load knowledge store.".to_string(),
                    }
                } else {
                    "No knowledge path configured.".to_string()
                };
                self.broadcast_control(OutboundEvent::CommandResult {
                    action: "recall_memory".to_string(),
                    ok: true,
                    message: result,
                    data: None,
                });
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
                self.log_sourced(
                    LogLevel::Detail,
                    format!("Turn {} started ({:.0}% budget)", turn, budget_pct),
                    LogSource::Agent,
                    Some(turn),
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
                self.log_sourced(
                    LogLevel::Model,
                    format!("T{}: {}", turn, summary),
                    LogSource::Agent,
                    Some(turn),
                );
                if let Some(ref reasoning_text) = reasoning {
                    self.log_sourced(
                        LogLevel::Model,
                        format!("Reasoning: {}", reasoning_text),
                        LogSource::Agent,
                        Some(turn),
                    );
                }
                self.log_sourced(
                    LogLevel::Detail,
                    format!(
                        "tokens: prompt={} completion={} total={}",
                        usage.prompt_tokens, usage.completion_tokens, usage.total_tokens
                    ),
                    LogSource::Agent,
                    Some(turn),
                );
                self.log_sourced(
                    LogLevel::Debug,
                    format!("Raw model response: {}", content),
                    LogSource::Agent,
                    Some(turn),
                );
            }
            AppEvent::ModelResponseDelta { text } => {
                // Accumulate streaming text; shown at Debug level to avoid noise
                self.streaming_buffer.push_str(&text);
            }
            AppEvent::JsonExtracted { preview } => {
                let t = self.turn;
                self.log_sourced(
                    LogLevel::Debug,
                    format!("JSON: {}", preview),
                    LogSource::Agent,
                    if t > 0 { Some(t) } else { None },
                );
            }
            AppEvent::DoneSignal { message } => {
                if let Some(msg) = message {
                    let t = self.turn;
                    self.log_sourced(
                        LogLevel::Info,
                        msg,
                        LogSource::Agent,
                        if t > 0 { Some(t) } else { None },
                    );
                }
                self.current_phase = Phase::Done;
            }
            AppEvent::AgentStarted {
                turn,
                commands_preview,
            } => {
                self.current_phase = Phase::RunningAgent;
                self.log_sourced(
                    LogLevel::Detail,
                    format!("Agent running (turn {}): {}", turn, commands_preview),
                    LogSource::Agent,
                    Some(turn),
                );
            }
            AppEvent::AgentOutput { stdout, stderr } => {
                self.broadcast_control(OutboundEvent::AgentOutput {
                    stdout: stdout.clone(),
                    stderr: stderr.clone(),
                });
                let t = self.turn;
                let turn_opt = if t > 0 { Some(t) } else { None };
                if !stdout.is_empty() {
                    for line in stdout.lines() {
                        self.log_sourced(
                            LogLevel::Agent,
                            line.to_string(),
                            LogSource::Agent,
                            turn_opt,
                        );
                    }
                }
                if !stderr.is_empty() {
                    for line in stderr.lines() {
                        self.log_sourced(
                            LogLevel::Warn,
                            format!("stderr: {}", line),
                            LogSource::Agent,
                            turn_opt,
                        );
                    }
                }
            }
            AppEvent::SubAgentResult { formatted } => {
                self.turn += 1;
                self.log_sourced(LogLevel::SubAgent, formatted, LogSource::Agent, Some(self.turn));
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
                self.log_sourced(LogLevel::SubAgent, summary, LogSource::Agent, Some(turn));
            }
            AppEvent::OrchestratorLog { message, level } => {
                self.log_sourced(level, message, LogSource::Agent, Some(self.turn));
            }
            AppEvent::ContextManagement { turn } => {
                self.log_sourced(
                    LogLevel::Detail,
                    format!("Context management (turn {})", turn),
                    LogSource::Agent,
                    Some(turn),
                );
            }
            AppEvent::TaskComplete { reason, summary } => {
                self.current_phase = Phase::Done;
                self.broadcast_control(OutboundEvent::TaskComplete {
                    reason: reason.clone(),
                    summary: summary.clone(),
                });
                self.log(LogLevel::Info, format!("--- {} ---", reason));
                if let Some(ref brief) = summary {
                    self.log(LogLevel::Detail, format!("Task brief: {}", brief));
                }
                // Create a follow-up textarea so the user can submit follow-ups
                // after task completion (press f to reopen).
                if self.follow_up_textarea.is_none()
                    && (self.follow_up_tx.is_some() || self.presence_tx.is_some())
                {
                    let mut textarea = tui_textarea::TextArea::default();
                    textarea.set_cursor_line_style(ratatui::style::Style::default());
                    self.follow_up_textarea = Some(textarea);
                }
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
            AppEvent::PresenceLog { message, level, turn } => {
                let lvl = level.unwrap_or(LogLevel::Info);
                let is_debug = lvl == LogLevel::Debug;
                // Persist debug-level presence logs (tool_request, tool_response, etc.) to session log
                if is_debug {
                    if let Some(ref sl) = self.session_log {
                        if let Ok(mut log) = sl.lock() {
                            log.debug(&message);
                        }
                    }
                }
                self.log_sourced(
                    lvl,
                    message,
                    LogSource::Presence,
                    turn,
                );
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
                self.log(LogLevel::Detail, "Human prompt closed by runtime".to_string());
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
                let t = self.turn;
                self.log_sourced(
                    LogLevel::Warn,
                    format!(
                        "Approval needed [{}]: {}",
                        category,
                        truncate_str(&command_preview, 80)
                    ),
                    LogSource::Agent,
                    if t > 0 { Some(t) } else { None },
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
                let t = self.turn;
                self.log_sourced(
                    LogLevel::Detail,
                    format!("auto-approved: {}", preview),
                    LogSource::Agent,
                    if t > 0 { Some(t) } else { None },
                );
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
                        "Round {} complete ({} turns). Press f to write a follow-up, q to quit.",
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
                self.log(LogLevel::Detail, log_msg);
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
            AppEvent::PresenceReady => {
                // Switch to follow-up mode so the user can respond to presence,
                // but don't log a fake round completion.
                if self.current_phase != Phase::WaitingApproval {
                    self.current_phase = Phase::WaitingFollowUp;
                    self.mode = AppMode::FollowUp;
                    if self.follow_up_textarea.is_none() {
                        let mut textarea = tui_textarea::TextArea::default();
                        textarea.set_cursor_line_style(ratatui::style::Style::default());
                        self.follow_up_textarea = Some(textarea);
                    }
                }
            }
            AppEvent::PresenceConnected { live_provider, live_model, .. } => {
                // New voice session — increment turn for collapsing
                self.voice_turn += 1;
                let p_display = live_provider.as_deref().unwrap_or("unknown");
                let m_display = live_model.as_deref().unwrap_or("unknown");
                if let Some(ref flag) = self.presence_paused {
                    let count = flag.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    self.log_sourced(LogLevel::Detail, format!(
                        "Browser presence connected ({}:{}) — server presence paused ({})",
                        p_display, m_display, count
                    ), LogSource::Live, Some(self.voice_turn));
                } else {
                    self.log_sourced(LogLevel::Detail, format!(
                        "Browser presence connected ({}:{})",
                        p_display, m_display
                    ), LogSource::Live, Some(self.voice_turn));
                }
                // Update displayed model/provider to the live model
                if let Some(ref provider) = live_provider {
                    self.presence_provider_name = Some(provider.clone());
                }
                if let Some(ref model) = live_model {
                    self.presence_model_name = Some(model.clone());
                }
                // Persist to session log
                if let Some(ref sl) = self.session_log {
                    if let Ok(mut log) = sl.lock() {
                        log.presence_connected(
                            live_provider.as_deref(),
                            live_model.as_deref(),
                        );
                    }
                }
            }
            AppEvent::PresenceDisconnected => {
                self.flush_voice_transcript();
                let vt = if self.voice_turn > 0 { Some(self.voice_turn) } else { None };
                if let Some(ref flag) = self.presence_paused {
                    let _ = flag.fetch_update(
                        std::sync::atomic::Ordering::Relaxed,
                        std::sync::atomic::Ordering::Relaxed,
                        |v| Some(v.saturating_sub(1)),
                    );
                    let count = flag.load(std::sync::atomic::Ordering::Relaxed);
                    if count == 0 {
                        self.log_sourced(LogLevel::Detail, "Browser presence disconnected — server presence resumed".to_string(), LogSource::Live, vt);
                    } else {
                        self.log_sourced(LogLevel::Detail, format!("Browser presence disconnected ({} still connected)", count), LogSource::Live, vt);
                    }
                } else {
                    self.log_sourced(LogLevel::Detail, "Browser presence disconnected".to_string(), LogSource::Live, vt);
                }
                // Persist to session log
                if let Some(ref sl) = self.session_log {
                    if let Ok(mut log) = sl.lock() {
                        log.presence_disconnected();
                    }
                }
            }
            AppEvent::VoiceLog { ref text, seq, ref tool_context } => {
                // Always persist individual fragments to session log on disk.
                if let Some(ref sl) = self.session_log {
                    if let Ok(mut log) = sl.lock() {
                        log.voice_log(text, seq, tool_context.as_deref());
                    }
                }

                let ctx = tool_context.as_deref().unwrap_or("");
                match ctx {
                    "transcript" => {
                        // Accumulate spoken-word fragments; flushed as a single
                        // log entry on the next boundary event or after idle.
                        self.voice_transcript_buffer.push_str(text);
                        self.voice_transcript_idle_ticks = 0;
                    }
                    "" => {
                        // Thinking block — flush pending transcript, start a
                        // new voice turn so each response is independently
                        // collapsible.
                        self.flush_voice_transcript();
                        self.voice_turn += 1;
                        let vt = Some(self.voice_turn);
                        self.log_sourced(
                            LogLevel::Detail,
                            format!("[voice] {}", text.trim()),
                            LogSource::Live,
                            vt,
                        );
                    }
                    _ => {
                        // Tool call — flush pending transcript, log at Detail.
                        self.flush_voice_transcript();
                        let vt = if self.voice_turn > 0 { Some(self.voice_turn) } else { None };
                        self.log_sourced(
                            LogLevel::Detail,
                            format!("[voice] {}", text),
                            LogSource::Live,
                            vt,
                        );
                    }
                }
            }
            AppEvent::PresenceCheckpointReceived { ref summary, last_event_seq } => {
                self.log_sourced(
                    LogLevel::Detail,
                    format!("Presence checkpoint at seq {}: {}", last_event_seq, summary),
                    LogSource::Live,
                    None,
                );
                // Persist to session log
                if let Some(ref sl) = self.session_log {
                    if let Ok(mut log) = sl.lock() {
                        log.presence_checkpoint(summary, last_event_seq);
                    }
                }
            }
            AppEvent::VoiceDiagnostic { ref kind, ref detail } => {
                let msg = format!("[voice:{}] {}", kind, detail);
                // Errors/disconnects at Warn (always visible), routine at Debug
                let lvl = match kind.as_str() {
                    "error" | "gemini_close" => LogLevel::Warn,
                    _ => LogLevel::Debug,
                };
                let vt = if self.voice_turn > 0 { Some(self.voice_turn) } else { None };
                self.log_sourced(lvl, msg, LogSource::Live, vt);
            }
            AppEvent::UserTranscript { ref text, seq } => {
                // Persist to session log
                if let Some(ref sl) = self.session_log {
                    if let Ok(mut log) = sl.lock() {
                        log.user_transcript(text, seq);
                    }
                }
                // Log at Info level with Presence source (shows "Voice" label)
                let vt = if self.voice_turn > 0 { Some(self.voice_turn) } else { None };
                self.log_sourced(LogLevel::Info, format!("[You] {}", text), LogSource::Live, vt);
                // Broadcast as outbound event
                self.broadcast_control(OutboundEvent::UserTranscript {
                    text: text.clone(),
                    seq,
                });
            }
            AppEvent::Tick => {
                self.tick_count += 1;
                // Update autonomy display
                let autonomy = self.autonomy.clone();
                if let Ok(state) = autonomy.try_read() {
                    self.autonomy_display = state.level.to_string();
                };
                // Flush voice transcript after idle period (500ms = 5 ticks)
                if !self.voice_transcript_buffer.is_empty() {
                    self.voice_transcript_idle_ticks += 1;
                    if self.voice_transcript_idle_ticks >= 5 {
                        self.flush_voice_transcript();
                    }
                }
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
            // Not valid JSON — return the full text for multi-line rendering.
            // The rendering layer handles showing one line (collapsed turn) vs
            // all lines (expanded turn) with continuation indentation.
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
        let view = ViewState::default();
        assert_eq!(app.turn, 0);
        assert_eq!(app.budget_pct, 0.0);
        assert_eq!(app.session_tokens, 0);
        assert_eq!(app.current_phase, Phase::Idle);
        assert_eq!(app.mode, AppMode::Normal);
        assert!(!app.should_quit);
        assert_eq!(view.verbosity, Verbosity::Normal);
        assert!(view.auto_scroll);
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
        let mut view = ViewState::default();
        view.scroll_offset = 30;
        view.auto_scroll = false;

        view.scroll_up(5);
        assert_eq!(view.scroll_offset, 25);

        view.scroll_down(10, &app);
        assert_eq!(view.scroll_offset, 35);
    }

    #[test]
    fn scroll_up_clamps_to_zero() {
        let app = test_app();
        let mut view = ViewState::default();
        view.scroll_offset = 3;
        view.scroll_up(10);
        assert_eq!(view.scroll_offset, 0);
        let _ = app; // suppress unused warning
    }

    #[test]
    fn scroll_home_end() {
        let mut app = test_app();
        for i in 0..50 {
            app.log(LogLevel::Info, format!("line {}", i));
        }
        let mut view = ViewState::default();
        view.scroll_home();
        assert_eq!(view.scroll_offset, 0);
        assert!(!view.auto_scroll);

        view.scroll_end(&app);
        assert!(view.auto_scroll);
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
        let app = test_app();
        let mut view = ViewState::default();
        assert_eq!(view.verbosity, Verbosity::Normal);
        let key = KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE);
        view.handle_key(key, &app);
        assert_eq!(view.verbosity, Verbosity::Verbose);
        view.handle_key(key, &app);
        assert_eq!(view.verbosity, Verbosity::Debug);
        view.handle_key(key, &app);
        assert_eq!(view.verbosity, Verbosity::Quiet);
    }

    #[test]
    fn handle_key_help_toggle() {
        let app = test_app();
        let mut view = ViewState::default();
        let key = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE);
        view.handle_key(key, &app);
        assert!(view.show_help);

        // Any key closes help
        let key = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        view.handle_key(key, &app);
        assert!(!view.show_help);
    }

    #[test]
    fn handle_key_scroll() {
        let mut app = test_app();
        for i in 0..50 {
            app.log(LogLevel::Info, format!("line {}", i));
        }
        let mut view = ViewState::default();
        view.scroll_offset = 25;
        view.auto_scroll = false;

        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        view.handle_key(up, &app);
        assert_eq!(view.scroll_offset, 24);

        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        view.handle_key(down, &app);
        assert_eq!(view.scroll_offset, 25);
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
            summary: None,
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
    fn format_model_summary_text_preserves_newlines() {
        let text = "Here are the files:\nfoo.rs\nbar.rs\nbaz.rs";
        let summary = format_model_summary(text);
        // Non-JSON text is returned as-is (newlines preserved for multi-line rendering)
        assert_eq!(summary, text);
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

    // --- LogSource and LogTab tests ---

    #[test]
    fn log_tab_cycle() {
        assert_eq!(LogTab::All.next(), LogTab::Agent);
        assert_eq!(LogTab::Agent.next(), LogTab::Presence);
        assert_eq!(LogTab::Presence.next(), LogTab::All);
    }

    #[test]
    fn log_tab_includes() {
        assert!(LogTab::All.includes(LogSource::Agent));
        assert!(LogTab::All.includes(LogSource::Presence));
        assert!(LogTab::All.includes(LogSource::System));

        assert!(LogTab::Agent.includes(LogSource::Agent));
        assert!(LogTab::Agent.includes(LogSource::System));
        assert!(!LogTab::Agent.includes(LogSource::Presence));

        assert!(LogTab::Presence.includes(LogSource::Presence));
        assert!(LogTab::Presence.includes(LogSource::System));
        assert!(!LogTab::Presence.includes(LogSource::Agent));
    }

    #[test]
    fn log_sourced_tags_entries() {
        let mut app = test_app();
        app.log_sourced(
            LogLevel::Model,
            "T1: exec: ls".to_string(),
            LogSource::Agent,
            Some(1),
        );
        app.log_sourced(
            LogLevel::Info,
            "[presence] tool call".to_string(),
            LogSource::Presence,
            None,
        );
        assert_eq!(app.log_entries[0].source, LogSource::Agent);
        assert_eq!(app.log_entries[0].turn, Some(1));
        assert_eq!(app.log_entries[1].source, LogSource::Presence);
        assert_eq!(app.log_entries[1].turn, None);
    }

    #[test]
    fn filtered_indices_respects_tab() {
        let mut app = test_app();
        app.log_sourced(
            LogLevel::Model,
            "agent msg".to_string(),
            LogSource::Agent,
            Some(1),
        );
        app.log_sourced(
            LogLevel::Info,
            "presence msg".to_string(),
            LogSource::Presence,
            None,
        );
        app.log_sourced(
            LogLevel::Info,
            "system msg".to_string(),
            LogSource::System,
            None,
        );

        let mut view = ViewState::default();
        view.log_tab = LogTab::All;
        assert_eq!(view.filtered_indices(&app).len(), 3);

        view.log_tab = LogTab::Agent;
        let indices = view.filtered_indices(&app);
        assert_eq!(indices.len(), 2); // agent + system
        assert_eq!(app.log_entries[indices[0]].source, LogSource::Agent);
        assert_eq!(app.log_entries[indices[1]].source, LogSource::System);

        view.log_tab = LogTab::Presence;
        let indices = view.filtered_indices(&app);
        assert_eq!(indices.len(), 2); // presence + system
        assert_eq!(app.log_entries[indices[0]].source, LogSource::Presence);
        assert_eq!(app.log_entries[indices[1]].source, LogSource::System);
    }

    #[test]
    fn turn_collapse_hides_subsequent_entries() {
        let mut app = test_app();
        // Add 3 entries for turn 1 (all visible at Normal verbosity)
        app.log_sourced(
            LogLevel::Model,
            "T1: exec: ls".to_string(),
            LogSource::Agent,
            Some(1),
        );
        app.log_sourced(
            LogLevel::Info,
            "tokens: 100".to_string(),
            LogSource::Agent,
            Some(1),
        );
        app.log_sourced(
            LogLevel::Warn,
            "Approval needed".to_string(),
            LogSource::Agent,
            Some(1),
        );
        // Add 1 system entry (no turn)
        app.log(LogLevel::Info, "system msg".to_string());

        let mut view = ViewState::default();
        // By default, turn 1 is collapsed: only first entry of turn + system
        assert!(!view.expanded_turns.contains(&1));
        let indices = view.filtered_indices(&app);
        assert_eq!(indices.len(), 2); // first turn entry + system entry
        assert_eq!(app.log_entries[indices[0]].content, "T1: exec: ls");
        assert_eq!(app.log_entries[indices[1]].content, "system msg");

        // Expand turn 1: all entries visible
        view.expanded_turns.insert(1);
        let indices = view.filtered_indices(&app);
        assert_eq!(indices.len(), 4); // all 3 turn entries + system
    }

    #[test]
    fn toggle_focused_turn_expand() {
        let mut app = test_app();
        app.log_sourced(
            LogLevel::Model,
            "T1: exec: ls".to_string(),
            LogSource::Agent,
            Some(1),
        );
        app.log_sourced(
            LogLevel::Info,
            "tokens: 100".to_string(),
            LogSource::Agent,
            Some(1),
        );
        // Focus on first entry (auto_scroll=true means last filtered entry is focused)
        let mut view = ViewState::default();
        view.auto_scroll = true;

        // Toggle expand
        view.toggle_focused_turn_expand(&app);
        assert!(view.expanded_turns.contains(&1));

        // Toggle collapse
        view.toggle_focused_turn_expand(&app);
        assert!(!view.expanded_turns.contains(&1));
    }

    #[test]
    fn tab_switch_keys() {
        let app = test_app();
        let mut view = ViewState::default();
        assert_eq!(view.log_tab, LogTab::All);

        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        view.handle_key(tab, &app);
        assert_eq!(view.log_tab, LogTab::Agent);

        let key2 = KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE);
        view.handle_key(key2, &app);
        assert_eq!(view.log_tab, LogTab::Presence);

        let key1 = KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE);
        view.handle_key(key1, &app);
        assert_eq!(view.log_tab, LogTab::All);
    }

    #[test]
    fn model_response_tagged_as_agent_source() {
        let mut app = test_app();
        app.handle_event(AppEvent::ModelResponse {
            turn: 2,
            content: r#"{"commands":[{"function":"execAsAgent","nonce":1,"command":"ls"}]}"#
                .to_string(),
            usage: crate::provider::TokenUsage {
                prompt_tokens: 100,
                completion_tokens: 20,
                total_tokens: 120,
            },
            reasoning: None,
        });
        // All entries from ModelResponse should be Agent source with turn 2
        for entry in &app.log_entries {
            assert_eq!(entry.source, LogSource::Agent);
            assert_eq!(entry.turn, Some(2));
        }
    }

    #[test]
    fn presence_log_tagged_as_presence_source() {
        let mut app = test_app();
        app.handle_event(AppEvent::PresenceLog {
            message: "Tool call: recall_memory".to_string(),
            level: None,
            turn: Some(1),
        });
        assert_eq!(app.log_entries.len(), 1);
        assert_eq!(app.log_entries[0].source, LogSource::Presence);
        assert_eq!(app.log_entries[0].level, LogLevel::Info);
        assert_eq!(app.log_entries[0].turn, Some(1));

        // Debug-level presence log
        app.handle_event(AppEvent::PresenceLog {
            message: "Tokens: 100 + 50 = 150".to_string(),
            level: Some(LogLevel::Debug),
            turn: Some(1),
        });
        assert_eq!(app.log_entries.len(), 2);
        assert_eq!(app.log_entries[1].level, LogLevel::Debug);
        assert_eq!(app.log_entries[1].source, LogSource::Presence);
        assert_eq!(app.log_entries[1].turn, Some(1));

        // Presence log without turn (e.g. error fallback)
        app.handle_event(AppEvent::PresenceLog {
            message: "error".to_string(),
            level: Some(LogLevel::Warn),
            turn: None,
        });
        assert_eq!(app.log_entries[2].turn, None);
    }

    #[test]
    fn voice_transcript_accumulates_and_flushes_on_thinking() {
        let mut app = test_app();
        // Simulate PresenceConnected (sets voice_turn=1)
        app.voice_turn = 1;

        // First thinking block → starts voice_turn=2
        app.handle_event(AppEvent::VoiceLog {
            text: "**Thinking**\n\nReasoning here.\n\n\n".to_string(),
            seq: 1,
            tool_context: None,
        });
        assert_eq!(app.voice_turn, 2);
        // Thinking block logged at Detail
        assert_eq!(app.log_entries.len(), 1);
        assert_eq!(app.log_entries[0].level, LogLevel::Detail);
        assert_eq!(app.log_entries[0].turn, Some(2));

        // Transcript fragments accumulate
        app.handle_event(AppEvent::VoiceLog {
            text: "Hello".to_string(),
            seq: 2,
            tool_context: Some("transcript".to_string()),
        });
        app.handle_event(AppEvent::VoiceLog {
            text: " world.".to_string(),
            seq: 3,
            tool_context: Some("transcript".to_string()),
        });
        // Still only 1 log entry — fragments are buffered
        assert_eq!(app.log_entries.len(), 1);

        // Next thinking block flushes the buffer and starts voice_turn=3
        app.handle_event(AppEvent::VoiceLog {
            text: "**Next thought**\n\n".to_string(),
            seq: 4,
            tool_context: None,
        });
        // Buffer flushed as single entry, then thinking logged
        assert_eq!(app.log_entries.len(), 3);
        assert_eq!(app.log_entries[1].level, LogLevel::Info);
        assert_eq!(app.log_entries[1].content, "[Presence] Hello world.");
        assert_eq!(app.log_entries[1].turn, Some(2)); // belongs to previous turn
        assert_eq!(app.log_entries[2].level, LogLevel::Detail);
        assert_eq!(app.log_entries[2].turn, Some(3));
        assert_eq!(app.voice_turn, 3);
    }

    #[test]
    fn voice_transcript_flushes_on_tool_call() {
        let mut app = test_app();
        app.voice_turn = 2;

        // Transcript fragments
        app.handle_event(AppEvent::VoiceLog {
            text: "Sure, ".to_string(),
            seq: 1,
            tool_context: Some("transcript".to_string()),
        });
        app.handle_event(AppEvent::VoiceLog {
            text: "doing it.".to_string(),
            seq: 2,
            tool_context: Some("transcript".to_string()),
        });
        assert_eq!(app.log_entries.len(), 0);

        // Tool call flushes buffer
        app.handle_event(AppEvent::VoiceLog {
            text: "[tool] submit_task({})".to_string(),
            seq: 3,
            tool_context: Some("submit_task".to_string()),
        });
        assert_eq!(app.log_entries.len(), 2);
        assert_eq!(app.log_entries[0].content, "[Presence] Sure, doing it.");
        assert_eq!(app.log_entries[0].level, LogLevel::Info);
        assert_eq!(app.log_entries[1].level, LogLevel::Detail);
    }

    #[test]
    fn voice_transcript_flushes_on_tick_idle() {
        let mut app = test_app();
        app.voice_turn = 1;

        app.handle_event(AppEvent::VoiceLog {
            text: "Done.".to_string(),
            seq: 1,
            tool_context: Some("transcript".to_string()),
        });
        assert_eq!(app.log_entries.len(), 0);

        // 4 ticks — not enough
        for _ in 0..4 {
            app.handle_event(AppEvent::Tick);
        }
        assert_eq!(app.log_entries.len(), 0);

        // 5th tick — flush
        app.handle_event(AppEvent::Tick);
        assert_eq!(app.log_entries.len(), 1);
        assert_eq!(app.log_entries[0].content, "[Presence] Done.");
    }
}
