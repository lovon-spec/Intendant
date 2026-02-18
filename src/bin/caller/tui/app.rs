use crate::autonomy::SharedAutonomy;
use crate::tui::event::{AppEvent, ApprovalResponse};
use crate::tui::layout::PanelConfig;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::oneshot;

const MAX_LOG_ENTRIES: usize = 10_000;

/// Current phase of the agent loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    Thinking,
    RunningAgent,
    WaitingApproval,
    WaitingHuman,
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

/// A single log entry.
#[derive(Debug, Clone)]
pub struct LogEntry {
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
    pub log_entries: Vec<LogEntry>,
    pub scroll_offset: usize,
    pub auto_scroll: bool,
    pub verbose: bool,

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

    // Approval
    pub pending_approval: Option<PendingApproval>,

    // Shared autonomy state
    pub autonomy: SharedAutonomy,

    // Animation
    pub tick_count: usize,
}

impl App {
    pub fn new(
        provider_name: String,
        model_name: String,
        autonomy: SharedAutonomy,
    ) -> Self {
        Self {
            log_entries: Vec::new(),
            scroll_offset: 0,
            auto_scroll: true,
            verbose: false,
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
            pending_approval: None,
            autonomy,
            tick_count: 0,
        }
    }

    pub fn log(&mut self, level: LogLevel, content: String) {
        self.log_entries.push(LogEntry { level, content });
        if self.log_entries.len() > MAX_LOG_ENTRIES {
            self.log_entries.drain(..self.log_entries.len() - MAX_LOG_ENTRIES);
        }
        if self.auto_scroll {
            self.scroll_to_bottom();
        }
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll_offset = self.log_entries.len().saturating_sub(1);
    }

    pub fn scroll_up(&mut self, n: usize) {
        self.auto_scroll = false;
        self.scroll_offset = self.scroll_offset.saturating_sub(n);
    }

    pub fn scroll_down(&mut self, n: usize) {
        self.auto_scroll = false;
        self.scroll_offset = (self.scroll_offset + n).min(self.log_entries.len().saturating_sub(1));
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
            AppMode::Approval => 6,
            AppMode::AskHuman => 5,
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
                self.verbose = !self.verbose;
                true
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

    fn handle_approval_key(&mut self, key: KeyEvent) -> bool {
        let response = match key.code {
            KeyCode::Char('y') | KeyCode::Enter => Some(ApprovalResponse::Approve),
            KeyCode::Char('s') => Some(ApprovalResponse::Skip),
            KeyCode::Char('a') => Some(ApprovalResponse::ApproveAll),
            KeyCode::Char('n') => Some(ApprovalResponse::Deny),
            _ => None,
        };

        if let Some(resp) = response {
            if let Some(pending) = self.pending_approval.take() {
                let _ = pending.responder.send(resp);
            }
            self.mode = AppMode::Normal;
            self.current_phase = Phase::RunningAgent;
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
                    if !response.trim().is_empty() {
                        let _ = std::fs::write("/dev/shm/intendant_human_response", &response);
                        self.log(LogLevel::Info, format!("Human response sent: {}", truncate_str(&response, 80)));
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

    /// Process an AppEvent and update state accordingly.
    pub fn handle_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::TurnStarted { turn, budget_pct, .. } => {
                self.turn = turn;
                self.budget_pct = budget_pct;
                self.current_phase = Phase::Thinking;
                self.log(LogLevel::Debug, format!("Turn {} started ({:.0}% budget)", turn, budget_pct));
            }
            AppEvent::ModelResponse { content, usage } => {
                let preview = truncate_str(&content, 200);
                self.log(LogLevel::Model, preview.to_string());
                self.log(LogLevel::Debug, format!(
                    "Tokens: {} prompt + {} completion = {} total",
                    usage.prompt_tokens, usage.completion_tokens, usage.total_tokens
                ));
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
            AppEvent::AgentStarted { turn } => {
                self.current_phase = Phase::RunningAgent;
                self.log(LogLevel::Debug, format!("Agent running (turn {})", turn));
            }
            AppEvent::AgentOutput { stdout, stderr } => {
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
            AppEvent::ContextManagement { turn } => {
                self.log(LogLevel::Debug, format!("Context management (turn {})", turn));
            }
            AppEvent::TaskComplete { reason } => {
                self.current_phase = Phase::Done;
                self.log(LogLevel::Info, format!("--- {} ---", reason));
            }
            AppEvent::BudgetWarning { pct, remaining } => {
                self.budget_pct = pct;
                self.log(LogLevel::Warn, format!(
                    "Budget warning: {:.0}% used, {} remaining",
                    pct, remaining
                ));
            }
            AppEvent::BudgetExhausted { remaining } => {
                self.budget_pct = 100.0;
                self.log(LogLevel::Error, format!("Budget exhausted ({} remaining)", remaining));
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
            AppEvent::HumanQuestionDetected { question } => {
                self.human_question = Some(question.clone());
                self.current_phase = Phase::WaitingHuman;
                self.mode = AppMode::AskHuman;
                let mut textarea = tui_textarea::TextArea::default();
                textarea.set_cursor_line_style(ratatui::style::Style::default());
                self.human_textarea = Some(textarea);
                self.log(LogLevel::Info, format!("Human question: {}", question));
            }
            AppEvent::HumanResponseSent => {
                self.log(LogLevel::Info, "Human response acknowledged".to_string());
            }
            AppEvent::ApprovalRequired { id, command_preview, category, responder } => {
                self.current_phase = Phase::WaitingApproval;
                self.mode = AppMode::Approval;
                self.pending_approval = Some(PendingApproval {
                    id,
                    command_preview: command_preview.clone(),
                    category: category.to_string(),
                    responder,
                });
                self.log(LogLevel::Warn, format!("Approval needed [{}]: {}", category, truncate_str(&command_preview, 80)));
            }
            AppEvent::ControlCommand(msg) => {
                self.log(LogLevel::Debug, format!("Control: {:?}", msg));
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

fn truncate_str(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        &s[..max]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autonomy::{self, AutonomyState};

    fn test_app() -> App {
        let autonomy = autonomy::shared_autonomy(AutonomyState::default());
        App::new("openai".to_string(), "gpt-5".to_string(), autonomy)
    }

    #[test]
    fn app_new_defaults() {
        let app = test_app();
        assert_eq!(app.turn, 0);
        assert_eq!(app.budget_pct, 0.0);
        assert_eq!(app.current_phase, Phase::Idle);
        assert_eq!(app.mode, AppMode::Normal);
        assert!(!app.should_quit);
        assert!(!app.verbose);
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
    fn handle_key_verbose() {
        let mut app = test_app();
        assert!(!app.verbose);
        let key = KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE);
        app.handle_key(key);
        assert!(app.verbose);
        app.handle_key(key);
        assert!(!app.verbose);
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
    fn approval_key_approve() {
        let mut app = test_app();
        let (tx, rx) = oneshot::channel();
        app.mode = AppMode::Approval;
        app.pending_approval = Some(PendingApproval {
            id: 1,
            command_preview: "rm -rf /tmp".to_string(),
            category: "destructive".to_string(),
            responder: tx,
        });

        let key = KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE);
        assert!(app.handle_key(key));
        assert_eq!(app.mode, AppMode::Normal);

        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let resp = rt.block_on(async { rx.await.unwrap() });
        assert_eq!(resp, ApprovalResponse::Approve);
    }

    #[test]
    fn approval_key_deny() {
        let mut app = test_app();
        let (tx, rx) = oneshot::channel();
        app.mode = AppMode::Approval;
        app.pending_approval = Some(PendingApproval {
            id: 2,
            command_preview: "rm -rf /".to_string(),
            category: "destructive".to_string(),
            responder: tx,
        });

        let key = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE);
        assert!(app.handle_key(key));

        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let resp = rt.block_on(async { rx.await.unwrap() });
        assert_eq!(resp, ApprovalResponse::Deny);
    }

    #[test]
    fn approval_key_skip() {
        let mut app = test_app();
        let (tx, rx) = oneshot::channel();
        app.mode = AppMode::Approval;
        app.pending_approval = Some(PendingApproval {
            id: 3,
            command_preview: "test".to_string(),
            category: "command_exec".to_string(),
            responder: tx,
        });

        let key = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE);
        assert!(app.handle_key(key));

        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let resp = rt.block_on(async { rx.await.unwrap() });
        assert_eq!(resp, ApprovalResponse::Skip);
    }

    #[test]
    fn approval_key_approve_all() {
        let mut app = test_app();
        let (tx, rx) = oneshot::channel();
        app.mode = AppMode::Approval;
        app.pending_approval = Some(PendingApproval {
            id: 4,
            command_preview: "test".to_string(),
            category: "command_exec".to_string(),
            responder: tx,
        });

        let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        assert!(app.handle_key(key));

        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let resp = rt.block_on(async { rx.await.unwrap() });
        assert_eq!(resp, ApprovalResponse::ApproveAll);
    }
}
