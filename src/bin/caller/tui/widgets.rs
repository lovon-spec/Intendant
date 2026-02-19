use crate::tui::app::{App, AppMode, LogEntry, LogLevel, Phase};
use crate::tui::theme;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
};
use ratatui::Frame;

/// Render the status bar (1 line).
pub fn render_status_bar(f: &mut Frame, area: Rect, app: &App) {
    let autonomy_str = app.autonomy_display.clone();
    let autonomy_color = theme::autonomy_color(&autonomy_str);

    let budget_pct_display = if app.budget_pct > 0.0 {
        format!(" {:.0}%", app.budget_pct)
    } else {
        String::new()
    };
    let budget_color = theme::budget_color(app.budget_pct);

    let spans = vec![
        Span::styled(
            " Agent ",
            Style::default()
                .fg(theme::STATUS_BAR_FG)
                .bg(theme::STATUS_BAR_BG)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {} ", app.provider_name),
            Style::default()
                .fg(theme::STATUS_PROVIDER_FG)
                .bg(theme::STATUS_BAR_BG),
        ),
        Span::styled(
            format!("{} ", app.model_name),
            Style::default()
                .fg(theme::STATUS_MODEL_FG)
                .bg(theme::STATUS_BAR_BG),
        ),
        Span::styled(
            format!("T{} ", app.turn),
            Style::default()
                .fg(theme::STATUS_TURN_FG)
                .bg(theme::STATUS_BAR_BG),
        ),
        Span::styled(
            budget_pct_display,
            Style::default().fg(budget_color).bg(theme::STATUS_BAR_BG),
        ),
        Span::styled(
            format!(" [{}:{}]", autonomy_str, app.verbosity.label()),
            Style::default().fg(autonomy_color).bg(theme::STATUS_BAR_BG),
        ),
        // Fill remaining with bg
        Span::styled(
            " ".repeat(area.width.saturating_sub(40) as usize),
            Style::default().bg(theme::STATUS_BAR_BG),
        ),
    ];

    let line = Line::from(spans);
    let widget = Paragraph::new(line).style(theme::status_bar_style());
    f.render_widget(widget, area);
}

/// Render the action panel (2 lines).
pub fn render_action_panel(f: &mut Frame, area: Rect, app: &App) {
    let spinner = if app.current_phase != Phase::Done && app.current_phase != Phase::Idle {
        let idx = app.tick_count % theme::SPINNER_FRAMES.len();
        theme::SPINNER_FRAMES[idx]
    } else {
        " "
    };

    let (phase_text, phase_key) = match &app.current_phase {
        Phase::Thinking => ("Thinking...".to_string(), "thinking"),
        Phase::RunningAgent => ("Running agent...".to_string(), "running"),
        Phase::WaitingApproval => ("Waiting for approval...".to_string(), "waiting"),
        Phase::WaitingHuman => ("Waiting for human input...".to_string(), "waiting"),
        Phase::Idle => ("Idle".to_string(), "done"),
        Phase::Done => ("Done".to_string(), "done"),
    };

    let style = theme::action_style_for_phase(phase_key);

    let line1 = Line::from(vec![
        Span::styled(format!(" {} ", spinner), style),
        Span::styled(phase_text, style.add_modifier(Modifier::BOLD)),
    ]);

    let line2 = Line::from(vec![Span::styled(
        "   ?=help  q=quit  v=verbosity  i=inspect  +/-=autonomy",
        Style::default().fg(theme::LOG_DIM_FG).bg(theme::ACTION_BG),
    )]);

    let widget = Paragraph::new(vec![line1, line2]).style(Style::default().bg(theme::ACTION_BG));
    f.render_widget(widget, area);
}

/// Render the log panel (scrollable).
pub fn render_log_panel(f: &mut Frame, area: Rect, app: &App) {
    let visible_height = area.height.saturating_sub(2) as usize; // minus borders
    let filtered: Vec<&LogEntry> = app
        .log_entries
        .iter()
        .filter(|entry| app.verbosity.includes(&entry.level))
        .collect();
    let total = filtered.len();
    let scroll_offset = if app.auto_scroll {
        total.saturating_sub(visible_height)
    } else {
        app.scroll_offset.min(total.saturating_sub(1))
    };

    let lines: Vec<Line> = app
        .log_entries
        .iter()
        .filter(|entry| app.verbosity.includes(&entry.level))
        .skip(scroll_offset)
        .take(visible_height)
        .map(format_log_entry)
        .collect();

    let scroll_info = if total > visible_height {
        let pos = scroll_offset + visible_height.min(total - scroll_offset);
        format!(" {}/{} ", pos, total)
    } else {
        String::new()
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::LOG_DIM_FG))
        .title(Span::styled(
            " Log ",
            Style::default()
                .fg(theme::LOG_FG)
                .add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Span::styled(
            scroll_info,
            Style::default().fg(theme::LOG_DIM_FG),
        ));

    let widget = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    f.render_widget(widget, area);

    // Scrollbar
    if total > visible_height {
        let mut scrollbar_state = ScrollbarState::new(total)
            .position(scroll_offset)
            .viewport_content_length(visible_height);

        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None);

        f.render_stateful_widget(
            scrollbar,
            area.inner(ratatui::layout::Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut scrollbar_state,
        );
    }
}

fn format_log_entry(entry: &LogEntry) -> Line<'static> {
    let level_span = match entry.level {
        LogLevel::Info => Span::styled("  ", Style::default().fg(theme::LOG_FG)),
        LogLevel::Model => Span::styled("M ", Style::default().fg(theme::LOG_MODEL_FG)),
        LogLevel::Agent => Span::styled("A ", Style::default().fg(theme::LOG_AGENT_FG)),
        LogLevel::Error => Span::styled("E ", Style::default().fg(theme::LOG_ERROR_FG)),
        LogLevel::Warn => Span::styled("W ", Style::default().fg(theme::LOG_WARN_FG)),
        LogLevel::SubAgent => Span::styled("S ", Style::default().fg(theme::LOG_SUBAGENT_FG)),
        LogLevel::Debug => Span::styled("D ", Style::default().fg(theme::LOG_DIM_FG)),
    };

    let content_color = match entry.level {
        LogLevel::Info => theme::LOG_FG,
        LogLevel::Model => theme::LOG_MODEL_FG,
        LogLevel::Agent => theme::LOG_AGENT_FG,
        LogLevel::Error => theme::LOG_ERROR_FG,
        LogLevel::Warn => theme::LOG_WARN_FG,
        LogLevel::SubAgent => theme::LOG_SUBAGENT_FG,
        LogLevel::Debug => theme::LOG_DIM_FG,
    };

    Line::from(vec![
        Span::styled(
            format!("{} ", entry.ts),
            Style::default().fg(theme::LOG_DIM_FG),
        ),
        level_span,
        Span::styled(entry.content.clone(), Style::default().fg(content_color)),
    ])
}

/// Render inspect overlay for one focused log entry.
pub fn render_inspect_overlay(f: &mut Frame, area: Rect, app: &App) {
    if app.mode != AppMode::Inspect {
        return;
    }

    let Some(selected_index) = app.inspect_index else {
        return;
    };
    let Some(entry) = app.log_entries.get(selected_index) else {
        return;
    };

    let level_text = match entry.level {
        LogLevel::Info => "INFO",
        LogLevel::Model => "MODEL",
        LogLevel::Agent => "AGENT",
        LogLevel::Error => "ERROR",
        LogLevel::Warn => "WARN",
        LogLevel::SubAgent => "SUB",
        LogLevel::Debug => "DEBUG",
    };

    let width = area.width.saturating_sub(4).max(20);
    let height = area.height.saturating_sub(4).max(6);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let centered = Rect::new(x, y, width, height);

    f.render_widget(ratatui::widgets::Clear, centered);

    let body = vec![
        Line::from(vec![Span::styled(
            format!(" [{}] entry #{} ", level_text, selected_index),
            Style::default()
                .fg(theme::HELP_KEY_FG)
                .add_modifier(Modifier::BOLD),
        )]),
        Line::default(),
        Line::from(entry.content.clone()),
        Line::default(),
        Line::from(vec![Span::styled(
            " Up/Down=next entry  PgUp/PgDn=jump  Enter/Esc/i=close ",
            Style::default().fg(theme::LOG_DIM_FG),
        )]),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::HELP_KEY_FG))
        .title(Span::styled(
            " Inspect Log ",
            Style::default()
                .fg(theme::HELP_FG)
                .add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(theme::HELP_BG));

    let widget = Paragraph::new(body).block(block).wrap(Wrap { trim: false });
    f.render_widget(widget, centered);
}

/// Render the approval panel (conditional, at bottom).
pub fn render_approval_panel(f: &mut Frame, area: Rect, command: &str, category: &str) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::APPROVAL_FG))
        .title(Span::styled(
            format!(" Approval required [{}] ", category),
            Style::default()
                .fg(theme::APPROVAL_FG)
                .add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(theme::APPROVAL_BG));

    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.height == 0 {
        return;
    }

    // Split inner area: command text (flexible) + hint line (fixed 1 line)
    let chunks = ratatui::layout::Layout::default()
        .direction(ratatui::layout::Direction::Vertical)
        .constraints([
            ratatui::layout::Constraint::Min(1),
            ratatui::layout::Constraint::Length(1),
        ])
        .split(inner);

    // Command preview (with word wrap)
    let cmd_lines = vec![Line::from(vec![Span::styled(
        format!(" {}", command),
        Style::default().fg(theme::APPROVAL_CMD_FG),
    )])];
    let cmd_widget = Paragraph::new(cmd_lines)
        .wrap(Wrap { trim: false })
        .style(Style::default().bg(theme::APPROVAL_BG));
    f.render_widget(cmd_widget, chunks[0]);

    // Key hints (always visible at bottom)
    let hint = Line::from(vec![Span::styled(
        " [y]approve  [s]skip  [a]approve-all  [n]deny",
        Style::default()
            .fg(theme::APPROVAL_HINT_FG)
            .add_modifier(Modifier::BOLD),
    )]);
    let hint_widget = Paragraph::new(hint).style(Style::default().bg(theme::APPROVAL_BG));
    f.render_widget(hint_widget, chunks[1]);
}

/// Render the human input panel (conditional, at bottom).
pub fn render_input_panel(f: &mut Frame, area: Rect, question: &str, app: &mut App) {
    // Split area: question line + text input + hint
    let chunks = ratatui::layout::Layout::default()
        .direction(ratatui::layout::Direction::Vertical)
        .constraints([
            ratatui::layout::Constraint::Length(1),
            ratatui::layout::Constraint::Min(1),
            ratatui::layout::Constraint::Length(1),
        ])
        .split(area);

    // Question
    let q_line = Line::from(vec![
        Span::styled(
            " Q: ",
            Style::default()
                .fg(theme::INPUT_QUESTION_FG)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            truncate(question, (chunks[0].width as usize).saturating_sub(6)).to_string(),
            Style::default().fg(theme::INPUT_QUESTION_FG),
        ),
    ]);
    f.render_widget(
        Paragraph::new(q_line).style(Style::default().bg(theme::INPUT_BG)),
        chunks[0],
    );

    // Text area
    if let Some(ref textarea) = app.human_textarea {
        f.render_widget(textarea, chunks[1]);
    }

    // Hint
    let hint = Line::from(vec![Span::styled(
        " Enter=submit  Esc=cancel",
        Style::default().fg(theme::INPUT_HINT_FG),
    )]);
    f.render_widget(
        Paragraph::new(hint).style(Style::default().bg(theme::INPUT_BG)),
        chunks[2],
    );
}

/// Render help overlay.
pub fn render_help_overlay(f: &mut Frame, area: Rect) {
    let help_text = vec![
        ("q / Ctrl-C", "Quit"),
        ("v", "Cycle verbosity profile"),
        ("i / Enter", "Inspect focused log entry"),
        ("Up/Down", "Scroll log"),
        ("PgUp/PgDn", "Scroll log (page)"),
        ("Home/End", "Jump to start/end"),
        ("+/-", "Cycle autonomy level"),
        ("?", "Toggle this help"),
        ("y/Enter", "Approve pending action"),
        ("s", "Skip pending action"),
        ("a", "Approve all remaining"),
        ("n", "Deny and stop"),
    ];

    let lines: Vec<Line> = help_text
        .iter()
        .map(|(key, desc)| {
            Line::from(vec![
                Span::styled(
                    format!(" {:14}", key),
                    Style::default().fg(theme::HELP_KEY_FG),
                ),
                Span::styled(desc.to_string(), Style::default().fg(theme::HELP_FG)),
            ])
        })
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::HELP_KEY_FG))
        .title(Span::styled(
            " Help ",
            Style::default()
                .fg(theme::HELP_FG)
                .add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(theme::HELP_BG));

    // Center the help overlay
    let width = 44.min(area.width);
    let height = (help_text.len() as u16 + 2).min(area.height);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let centered = Rect::new(x, y, width, height);

    // Clear background
    f.render_widget(ratatui::widgets::Clear, centered);

    let widget = Paragraph::new(lines).block(block);
    f.render_widget(widget, centered);
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else if max >= 3 {
        truncate_utf8(s, max - 3)
    } else {
        truncate_utf8(s, max)
    }
}

fn truncate_utf8(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_exact() {
        assert_eq!(truncate("hello", 5), "hello");
    }

    #[test]
    fn truncate_long() {
        assert_eq!(truncate("hello world", 8), "hello");
    }

    #[test]
    fn truncate_very_short() {
        assert_eq!(truncate("hello", 2), "he");
    }

    #[test]
    fn format_log_entry_info() {
        let entry = LogEntry {
            ts: "00:00:00".to_string(),
            level: LogLevel::Info,
            content: "test message".to_string(),
        };
        let line = format_log_entry(&entry);
        assert_eq!(line.spans.len(), 3);
    }

    #[test]
    fn format_log_entry_debug_hidden() {
        let entry = LogEntry {
            ts: "00:00:00".to_string(),
            level: LogLevel::Debug,
            content: "debug msg".to_string(),
        };
        let line = format_log_entry(&entry);
        assert_eq!(line.spans.len(), 3);
    }

    #[test]
    fn format_log_entry_debug_shown() {
        let entry = LogEntry {
            ts: "00:00:00".to_string(),
            level: LogLevel::Debug,
            content: "debug msg".to_string(),
        };
        let line = format_log_entry(&entry);
        assert_eq!(line.spans.len(), 3);
    }

    #[test]
    fn format_log_entry_all_levels() {
        let levels = vec![
            LogLevel::Info,
            LogLevel::Model,
            LogLevel::Agent,
            LogLevel::Error,
            LogLevel::Warn,
            LogLevel::SubAgent,
            LogLevel::Debug,
        ];
        for level in levels {
            let entry = LogEntry {
                ts: "00:00:00".to_string(),
                level,
                content: "test".to_string(),
            };
            let _ = format_log_entry(&entry);
        }
    }
}
