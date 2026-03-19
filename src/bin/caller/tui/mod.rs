pub mod app;
pub mod event;
pub mod layout;
pub mod markdown;
pub mod theme;
pub mod web;
pub mod widgets;

use app::App;
use crossterm::{
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use crate::event::AppEvent;
use ratatui::prelude::*;
use std::io;
/// Manages the terminal state and rendering.
pub struct Tui {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
}

impl Tui {
    pub fn new() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;

        // Set panic hook to restore terminal
        let original_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), LeaveAlternateScreen);
            original_hook(info);
        }));

        Ok(Self { terminal })
    }

    pub fn restore(&mut self) -> io::Result<()> {
        disable_raw_mode()?;
        execute!(self.terminal.backend_mut(), LeaveAlternateScreen)?;
        self.terminal.show_cursor()?;
        Ok(())
    }

    /// Render one frame of the TUI.
    pub fn draw(&mut self, app: &mut App, view: &app::ViewState) -> io::Result<()> {
        self.terminal.draw(|f| {
            render_frame(f, app, view);
        })?;
        Ok(())
    }

    /// Run the main TUI event loop until quit.
    pub async fn run(
        &mut self,
        app: &mut App,
        mut event_rx: tokio::sync::broadcast::Receiver<AppEvent>,
        bus: crate::event::EventBus,
    ) -> io::Result<()> {
        let mut view = app::ViewState::default();
        loop {
            // Apply any pending verbosity override from control socket
            if let Some(v) = app.pending_verbosity.take() {
                view.verbosity = v;
            }
            self.draw(app, &view)?;

            match event_rx.recv().await {
                Ok(event) => {
                    let derived = if let AppEvent::Key(key) = &event {
                        // Try view-only key handling first
                        if !view.handle_key(*key, app) {
                            // Fall through to shared state handling
                            app.handle_event(event)
                        } else {
                            Vec::new()
                        }
                    } else {
                        app.handle_event(event)
                    };
                    for d in derived {
                        bus.send(d);
                    }
                    if app.should_quit {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }

        Ok(())
    }
}

/// Shared render function used by both `Tui` (terminal) and `WebTui` (browser).
pub fn render_frame(f: &mut ratatui::Frame, app: &mut App, view: &app::ViewState) {
    let area = f.area();
    let bottom_height = app.bottom_panel_height();
    let app_layout = layout::calculate_layout(area, &app.panels, bottom_height);

    if let Some(status_area) = app_layout.status_bar {
        widgets::render_status_bar(f, status_area, app, view);
    }

    if let Some(action_area) = app_layout.action_panel {
        widgets::render_action_panel(f, action_area, app);
    }

    if let Some(log_area) = app_layout.log_panel {
        widgets::render_log_panel(f, log_area, app, view);
    }

    if let Some(bottom_area) = app_layout.bottom_panel {
        match app.mode {
            app::AppMode::Approval => {
                if let Some(pending) = app.pending_approvals.front() {
                    widgets::render_approval_panel(
                        f,
                        bottom_area,
                        &pending.command_preview,
                        &pending.category,
                    );
                }
            }
            app::AppMode::AskHuman => {
                let question = app.human_question.clone().unwrap_or_default();
                widgets::render_input_panel(f, bottom_area, &question, app);
            }
            app::AppMode::FollowUp => {
                widgets::render_follow_up_panel(f, bottom_area, app);
            }
            _ => {
                if app.is_follow_up_browsing() {
                    widgets::render_follow_up_reminder(f, bottom_area, app);
                }
            }
        }
    }

    // Per-connection overlays
    if view.show_help {
        widgets::render_help_overlay(f, area);
    }
    if view.show_inspect {
        widgets::render_inspect_overlay(f, area, app, view);
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autonomy::{self, AutonomyState};
    use ratatui::backend::TestBackend;

    fn test_terminal() -> Terminal<TestBackend> {
        let backend = TestBackend::new(80, 24);
        Terminal::new(backend).unwrap()
    }

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
    fn render_default_state() {
        let mut terminal = test_terminal();
        let app = test_app();
        let view = app::ViewState::default();

        terminal
            .draw(|f| {
                let area = f.area();
                let app_layout = layout::calculate_layout(area, &app.panels, 0);

                if let Some(status_area) = app_layout.status_bar {
                    widgets::render_status_bar(f, status_area, &app, &view);
                }
                if let Some(action_area) = app_layout.action_panel {
                    widgets::render_action_panel(f, action_area, &app);
                }
                if let Some(log_area) = app_layout.log_panel {
                    widgets::render_log_panel(f, log_area, &app, &view);
                }
            })
            .unwrap();
    }

    #[test]
    fn render_with_log_entries() {
        let mut terminal = test_terminal();
        let mut app = test_app();
        app.log(app::LogLevel::Info, "Hello world".to_string());
        app.log(app::LogLevel::Model, "Model response".to_string());
        app.log(app::LogLevel::Agent, "Agent output".to_string());
        app.log(app::LogLevel::Error, "Error message".to_string());
        let view = app::ViewState::default();

        terminal
            .draw(|f| {
                let area = f.area();
                let app_layout = layout::calculate_layout(area, &app.panels, 0);
                if let Some(log_area) = app_layout.log_panel {
                    widgets::render_log_panel(f, log_area, &app, &view);
                }
            })
            .unwrap();
    }

    #[test]
    fn render_approval_panel() {
        let mut terminal = test_terminal();
        let mut app = test_app();
        app.mode = app::AppMode::Approval;

        terminal
            .draw(|f| {
                let area = f.area();
                let app_layout = layout::calculate_layout(area, &app.panels, 4);
                if let Some(bottom_area) = app_layout.bottom_panel {
                    widgets::render_approval_panel(
                        f,
                        bottom_area,
                        "rm -rf /tmp/test",
                        "destructive",
                    );
                }
            })
            .unwrap();
    }

    #[test]
    fn render_help_overlay() {
        let mut terminal = test_terminal();

        terminal
            .draw(|f| {
                let area = f.area();
                widgets::render_help_overlay(f, area);
            })
            .unwrap();
    }

    #[test]
    fn render_with_phases() {
        let mut terminal = test_terminal();
        let mut app = test_app();

        let phases = vec![
            app::Phase::Thinking,
            app::Phase::RunningAgent,
            app::Phase::WaitingApproval,
            app::Phase::WaitingHuman,
            app::Phase::WaitingFollowUp,
            app::Phase::Idle,
            app::Phase::Done,
        ];

        for phase in phases {
            app.current_phase = phase;
            terminal
                .draw(|f| {
                    let area = f.area();
                    let app_layout = layout::calculate_layout(area, &app.panels, 0);
                    if let Some(action_area) = app_layout.action_panel {
                        widgets::render_action_panel(f, action_area, &app);
                    }
                })
                .unwrap();
        }
    }

    #[test]
    fn render_verbose_vs_non_verbose() {
        let mut terminal = test_terminal();
        let mut app = test_app();
        app.log(app::LogLevel::Debug, "debug only".to_string());
        app.log(app::LogLevel::Info, "always visible".to_string());

        // Normal verbosity (debug hidden)
        let mut view = app::ViewState::default();
        view.verbosity = app::Verbosity::Normal;
        terminal
            .draw(|f| {
                let area = f.area();
                let app_layout = layout::calculate_layout(area, &app.panels, 0);
                if let Some(log_area) = app_layout.log_panel {
                    widgets::render_log_panel(f, log_area, &app, &view);
                }
            })
            .unwrap();

        // Debug verbosity (debug shown)
        view.verbosity = app::Verbosity::Debug;
        terminal
            .draw(|f| {
                let area = f.area();
                let app_layout = layout::calculate_layout(area, &app.panels, 0);
                if let Some(log_area) = app_layout.log_panel {
                    widgets::render_log_panel(f, log_area, &app, &view);
                }
            })
            .unwrap();
    }

    #[test]
    fn render_small_terminal() {
        let backend = TestBackend::new(40, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = test_app();
        app.log(app::LogLevel::Info, "test".to_string());
        let view = app::ViewState::default();

        terminal
            .draw(|f| {
                let area = f.area();
                let app_layout = layout::calculate_layout(area, &app.panels, 0);
                if let Some(status_area) = app_layout.status_bar {
                    widgets::render_status_bar(f, status_area, &app, &view);
                }
                if let Some(log_area) = app_layout.log_panel {
                    widgets::render_log_panel(f, log_area, &app, &view);
                }
            })
            .unwrap();
    }

    #[test]
    fn render_follow_up_reminder_bar() {
        let mut terminal = test_terminal();
        let mut app = test_app();
        app.current_phase = app::Phase::WaitingFollowUp;
        app.mode = app::AppMode::FollowUp;
        // Simulate RoundComplete setting up textarea
        let textarea = tui_textarea::TextArea::default();
        app.follow_up_textarea = Some(textarea);
        app.round = 1;

        // Press Escape to enter browsing mode
        app.mode = app::AppMode::Normal;

        assert!(app.is_follow_up_browsing());
        assert_eq!(app.bottom_panel_height(), 3);

        terminal
            .draw(|f| {
                let area = f.area();
                let bottom_height = app.bottom_panel_height();
                let app_layout = layout::calculate_layout(area, &app.panels, bottom_height);
                assert!(app_layout.bottom_panel.is_some());
                let bottom_area = app_layout.bottom_panel.unwrap();
                assert_eq!(bottom_area.height, 3);
                widgets::render_follow_up_reminder(f, bottom_area, &app);
            })
            .unwrap();
    }
}
