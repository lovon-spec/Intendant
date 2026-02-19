pub mod app;
pub mod event;
pub mod layout;
pub mod theme;
pub mod widgets;

use app::App;
use crossterm::{
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use event::AppEvent;
use ratatui::prelude::*;
use std::io;
use tokio::sync::mpsc;

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
    pub fn draw(&mut self, app: &mut App) -> io::Result<()> {
        self.terminal.draw(|f| {
            let area = f.area();
            let bottom_height = app.bottom_panel_height();
            let app_layout = layout::calculate_layout(area, &app.panels, bottom_height);

            if let Some(status_area) = app_layout.status_bar {
                widgets::render_status_bar(f, status_area, app);
            }

            if let Some(action_area) = app_layout.action_panel {
                widgets::render_action_panel(f, action_area, app);
            }

            if let Some(log_area) = app_layout.log_panel {
                widgets::render_log_panel(f, log_area, app);
            }

            if let Some(bottom_area) = app_layout.bottom_panel {
                match app.mode {
                    app::AppMode::Approval => {
                        if let Some(ref pending) = app.pending_approval {
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
                    _ => {}
                }
            }

            // Help overlay on top
            if app.mode == app::AppMode::Help {
                widgets::render_help_overlay(f, area);
            }
            if app.mode == app::AppMode::Inspect {
                widgets::render_inspect_overlay(f, area, app);
            }
        })?;
        Ok(())
    }

    /// Run the main TUI event loop until quit.
    pub async fn run(
        &mut self,
        app: &mut App,
        mut event_rx: mpsc::UnboundedReceiver<AppEvent>,
    ) -> io::Result<()> {
        loop {
            self.draw(app)?;

            if let Some(event) = event_rx.recv().await {
                app.handle_event(event);
                if app.should_quit {
                    break;
                }
            } else {
                break;
            }
        }

        Ok(())
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
        App::new("openai".to_string(), "gpt-5".to_string(), autonomy)
    }

    #[test]
    fn render_default_state() {
        let mut terminal = test_terminal();
        let app = test_app();

        terminal
            .draw(|f| {
                let area = f.area();
                let app_layout = layout::calculate_layout(area, &app.panels, 0);

                if let Some(status_area) = app_layout.status_bar {
                    widgets::render_status_bar(f, status_area, &app);
                }
                if let Some(action_area) = app_layout.action_panel {
                    widgets::render_action_panel(f, action_area, &app);
                }
                if let Some(log_area) = app_layout.log_panel {
                    widgets::render_log_panel(f, log_area, &app);
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

        terminal
            .draw(|f| {
                let area = f.area();
                let app_layout = layout::calculate_layout(area, &app.panels, 0);
                if let Some(log_area) = app_layout.log_panel {
                    widgets::render_log_panel(f, log_area, &app);
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
        app.verbosity = app::Verbosity::Normal;
        terminal
            .draw(|f| {
                let area = f.area();
                let app_layout = layout::calculate_layout(area, &app.panels, 0);
                if let Some(log_area) = app_layout.log_panel {
                    widgets::render_log_panel(f, log_area, &app);
                }
            })
            .unwrap();

        // Debug verbosity (debug shown)
        app.verbosity = app::Verbosity::Debug;
        terminal
            .draw(|f| {
                let area = f.area();
                let app_layout = layout::calculate_layout(area, &app.panels, 0);
                if let Some(log_area) = app_layout.log_panel {
                    widgets::render_log_panel(f, log_area, &app);
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

        terminal
            .draw(|f| {
                let area = f.area();
                let app_layout = layout::calculate_layout(area, &app.panels, 0);
                if let Some(status_area) = app_layout.status_bar {
                    widgets::render_status_bar(f, status_area, &app);
                }
                if let Some(log_area) = app_layout.log_panel {
                    widgets::render_log_panel(f, log_area, &app);
                }
            })
            .unwrap();
    }
}
