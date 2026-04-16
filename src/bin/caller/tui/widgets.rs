use crate::tui::app::{App, LogEntry, LogSource, LogTab, ViewState};
use crate::tui::markdown;
use crate::types::{LogLevel, Phase};
use crate::tui::theme;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
};
use ratatui::Frame;

/// Render the status bar (1 line).
pub fn render_status_bar(f: &mut Frame, area: Rect, app: &App, view: &ViewState) {
    let autonomy_str = app.autonomy_display.clone();
    let autonomy_color = theme::autonomy_color(&autonomy_str);

    let budget_pct_display = if app.session_tokens > 0 {
        format!(" {:.1}%", app.budget_pct)
    } else {
        String::new()
    };
    let budget_color = theme::budget_color(app.budget_pct);

    let session_tokens_display = if app.session_tokens > 0 {
        format!(" {}", format_token_count(app.session_tokens))
    } else {
        String::new()
    };

    let mut spans = vec![
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
            session_tokens_display,
            Style::default()
                .fg(theme::STATUS_TURN_FG)
                .bg(theme::STATUS_BAR_BG),
        ),
        Span::styled(
            " autonomy:",
            Style::default()
                .fg(theme::LOG_DIM_FG)
                .bg(theme::STATUS_BAR_BG),
        ),
        Span::styled(
            autonomy_str,
            Style::default().fg(autonomy_color).bg(theme::STATUS_BAR_BG),
        ),
        Span::styled(
            "  verbosity:",
            Style::default()
                .fg(theme::LOG_DIM_FG)
                .bg(theme::STATUS_BAR_BG),
        ),
        Span::styled(
            view.verbosity.label().to_string(),
            Style::default()
                .fg(theme::LOG_DIM_FG)
                .bg(theme::STATUS_BAR_BG),
        ),
    ];

    // Show display info when vision is active
    if let Some(ref info) = app.display_info {
        spans.push(Span::styled(
            "  display:",
            Style::default()
                .fg(theme::LOG_DIM_FG)
                .bg(theme::STATUS_BAR_BG),
        ));
        spans.push(Span::styled(
            info.clone(),
            Style::default()
                .fg(theme::STATUS_PROVIDER_FG)
                .bg(theme::STATUS_BAR_BG),
        ));
    }

    // Show presence layer info when active
    if let Some(ref presence_model) = app.presence_model_name {
        let presence_pct = app.presence_usage_pct;
        let presence_color = theme::budget_color(presence_pct);
        spans.push(Span::styled(
            "  presence:",
            Style::default()
                .fg(theme::LOG_DIM_FG)
                .bg(theme::STATUS_BAR_BG),
        ));
        spans.push(Span::styled(
            format!("{}", presence_model),
            Style::default()
                .fg(theme::STATUS_MODEL_FG)
                .bg(theme::STATUS_BAR_BG),
        ));
        if app.presence_tokens > 0 {
            spans.push(Span::styled(
                format!(" {:.1}%", presence_pct),
                Style::default().fg(presence_color).bg(theme::STATUS_BAR_BG),
            ));
            spans.push(Span::styled(
                format!(" {}", format_token_count(app.presence_tokens)),
                Style::default()
                    .fg(theme::STATUS_TURN_FG)
                    .bg(theme::STATUS_BAR_BG),
            ));
        }
    }

    // Fill remaining with bg
    spans.push(Span::styled(
        " ".repeat(area.width.saturating_sub(40) as usize),
        Style::default().bg(theme::STATUS_BAR_BG),
    ));

    let line = Line::from(spans);
    let widget = Paragraph::new(line).style(theme::status_bar_style());
    f.render_widget(widget, area);
}

/// Render the action panel (2 lines).
pub fn render_action_panel(f: &mut Frame, area: Rect, app: &App) {
    let spinner = if app.current_phase != Phase::Done
        && app.current_phase != Phase::Idle
        && app.current_phase != Phase::Interrupted
    {
        let idx = app.tick_count % theme::SPINNER_FRAMES.len();
        theme::SPINNER_FRAMES[idx]
    } else {
        " "
    };

    let (phase_text, phase_key) = match &app.current_phase {
        Phase::Thinking => ("Thinking...".to_string(), "thinking"),
        Phase::RunningAgent => ("Running agent...".to_string(), "running"),
        Phase::Orchestrating => ("Orchestrating...".to_string(), "running"),
        Phase::WaitingApproval => ("Waiting for approval...".to_string(), "waiting"),
        Phase::WaitingHuman => ("Waiting for human input...".to_string(), "waiting"),
        Phase::WaitingFollowUp => ("Awaiting follow-up...".to_string(), "waiting"),
        Phase::Idle => ("Idle".to_string(), "done"),
        Phase::Done => ("Done".to_string(), "done"),
        Phase::Interrupting => ("Interrupting...".to_string(), "waiting"),
        Phase::Interrupted => ("Interrupted".to_string(), "done"),
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
pub fn render_log_panel(f: &mut Frame, area: Rect, app: &App, view: &ViewState) {
    let visible_height = area.height.saturating_sub(2) as usize; // minus borders
    let filtered = view.filtered_indices(app);
    let total = filtered.len();
    let scroll_offset = if view.auto_scroll {
        total.saturating_sub(visible_height)
    } else {
        view.scroll_offset.min(total.saturating_sub(1))
    };

    // Determine which filtered position is focused for highlight
    let focus_raw = view.focus_index(app);
    let focus_filtered_pos = focus_raw
        .and_then(|raw| filtered.iter().position(|&i| i == raw));

    // Precompute per-turn visible entry counts for collapsed turns (used for
    // the "+N more" indicator on collapsed turn summaries).
    let mut turn_counts: std::collections::HashMap<usize, usize> =
        std::collections::HashMap::new();
    for entry in &app.log_entries {
        if let Some(t) = entry.turn {
            if !view.expanded_turns.contains(&t)
                && view.verbosity.includes(&entry.level)
                && view.log_tab.includes(entry.source)
            {
                *turn_counts.entry(t).or_insert(0) += 1;
            }
        }
    }

    // Accumulate rendered lines from entries.  Each entry may produce multiple
    // terminal lines when its content contains newlines (multi-line rendering).
    let mut lines: Vec<Line> = Vec::with_capacity(visible_height);
    let mut entries_shown: usize = 0;
    for (vis_pos, &raw_idx) in filtered.iter().skip(scroll_offset).enumerate() {
        if lines.len() >= visible_height {
            break;
        }
        let entry = &app.log_entries[raw_idx];
        let is_focused = focus_filtered_pos
            .map(|fp| fp == scroll_offset + vis_pos)
            .unwrap_or(false);
        let hidden_count = entry
            .turn
            .and_then(|t| turn_counts.get(&t).map(|&c| c.saturating_sub(1)))
            .unwrap_or(0);
        let entry_lines =
            format_log_entry_with_turn(entry, &view.expanded_turns, is_focused, hidden_count);
        lines.extend(entry_lines);
        entries_shown += 1;
    }
    lines.truncate(visible_height);

    let scroll_info = if total > visible_height {
        let pos = scroll_offset + entries_shown.min(total - scroll_offset);
        format!(" {}/{} ", pos, total)
    } else {
        String::new()
    };

    // Build tab title: " Log [All | Agent | Presence] "
    let tab_title = build_tab_title(view.log_tab);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::LOG_DIM_FG))
        .title(tab_title)
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

/// Build the tab title spans for the log panel.
fn build_tab_title(active: LogTab) -> Line<'static> {
    use LogTab::*;
    let tabs = [All, Agent, Presence];
    let mut spans = vec![Span::styled(
        " Log ",
        Style::default()
            .fg(theme::LOG_FG)
            .add_modifier(Modifier::BOLD),
    )];
    for (i, tab) in tabs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("|", Style::default().fg(theme::LOG_DIM_FG)));
        }
        let label = tab.label();
        if *tab == active {
            spans.push(Span::styled(
                label.to_string(),
                Style::default()
                    .fg(theme::HELP_KEY_FG)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled(
                label.to_string(),
                Style::default().fg(theme::LOG_DIM_FG),
            ));
        }
    }
    spans.push(Span::styled(" ", Style::default()));
    Line::from(spans)
}

/// Width of the prefix area: marker(2) + timestamp(9) + label(6) = 17 display columns.
const LOG_PREFIX_WIDTH: usize = 17;

/// Format a log entry with turn collapse/expand indicator and optional focus highlight.
///
/// Returns multiple `Line`s when the entry content contains newlines.  The first
/// line carries the full prefix (marker + timestamp + level); continuation lines
/// are indented to align with the content column, producing a clean multi-line
/// block in the log panel.
///
/// For entries belonging to a **collapsed** turn, only the first line is returned
/// regardless of how many newlines the content contains — the turn expand/collapse
/// toggle controls whether the full text is visible.
fn format_log_entry_with_turn(
    entry: &LogEntry,
    expanded_turns: &std::collections::HashSet<usize>,
    is_focused: bool,
    hidden_count: usize,
) -> Vec<Line<'static>> {
    // Determine if this entry's turn is collapsed (only first entry shown by filter)
    let turn_collapsed = entry
        .turn
        .map(|t| !expanded_turns.contains(&t))
        .unwrap_or(false);

    // --- build prefix spans (shared for the first rendered line) ---

    let mut prefix = Vec::new();

    // Turn indicator
    if let Some(t) = entry.turn {
        let expanded = expanded_turns.contains(&t);
        let marker = if expanded { "▾ " } else { "▸ " };
        prefix.push(Span::styled(
            marker.to_string(),
            Style::default().fg(theme::STATUS_TURN_FG),
        ));
    } else {
        prefix.push(Span::styled("  ", Style::default()));
    }

    // Timestamp
    prefix.push(Span::styled(
        format!("{} ", entry.ts),
        Style::default().fg(theme::LOG_DIM_FG),
    ));

    // Source/level label: spelled-out names for clarity.
    // 6 chars wide (padded) to align content columns.
    let level_span = match (&entry.level, &entry.source) {
        // Browser live presence (voice/video)
        (_, LogSource::Live) =>
            Span::styled("Live  ", Style::default().fg(theme::LOG_PRESENCE_FG)),
        // Server-side text presence model
        (LogLevel::Info, LogSource::Presence)
        | (LogLevel::Detail, LogSource::Presence) =>
            Span::styled("Servr ", Style::default().fg(theme::LOG_SUBAGENT_FG)),
        (LogLevel::Info, _) =>
            Span::styled("    ℹ ", Style::default().fg(theme::LOG_DIM_FG)),
        (LogLevel::Model, _) =>
            Span::styled("Workr ", Style::default().fg(theme::LOG_MODEL_FG)),
        (LogLevel::Agent, _) =>
            Span::styled("Agent ", Style::default().fg(theme::LOG_AGENT_FG)),
        (LogLevel::Error, _) =>
            Span::styled("Error ", Style::default().fg(theme::LOG_ERROR_FG)),
        (LogLevel::Warn, _) =>
            Span::styled("Warn  ", Style::default().fg(theme::LOG_WARN_FG)),
        (LogLevel::SubAgent, _) =>
            Span::styled("Sub   ", Style::default().fg(theme::LOG_SUBAGENT_FG)),
        (LogLevel::Detail, _) =>
            Span::styled("    · ", Style::default().fg(theme::LOG_DETAIL_FG)),
        (LogLevel::Debug, _) =>
            Span::styled("Debug ", Style::default().fg(theme::LOG_DIM_FG)),
    };
    prefix.push(level_span);

    // Content styling
    let content_color = match entry.level {
        LogLevel::Info => theme::LOG_FG,
        LogLevel::Model => theme::LOG_MODEL_FG,
        LogLevel::Agent => theme::LOG_AGENT_FG,
        LogLevel::Error => theme::LOG_ERROR_FG,
        LogLevel::Warn => theme::LOG_WARN_FG,
        LogLevel::SubAgent => theme::LOG_SUBAGENT_FG,
        LogLevel::Detail => theme::LOG_DETAIL_FG,
        LogLevel::Debug => theme::LOG_DIM_FG,
    };
    let content_style = Style::default().fg(content_color);
    let focus_style = if is_focused {
        Some(Style::default().bg(Color::Rgb(40, 42, 60)))
    } else {
        None
    };

    // --- split content into lines ---

    let content_lines: Vec<&str> = entry.content.split('\n').collect();

    // Use markdown rendering for entry types that contain LLM output
    let use_markdown = matches!(
        entry.level,
        LogLevel::SubAgent | LogLevel::Model
    ) && content_lines.len() > 1;

    // First line: full prefix + first content line (always plain — it's the summary)
    let mut first_spans = prefix;
    first_spans.push(Span::styled(
        content_lines[0].to_string(),
        content_style,
    ));

    // For collapsed turns, append a hint showing how much is hidden
    if turn_collapsed {
        let extra_lines = content_lines.len().saturating_sub(1);
        let total_hidden = hidden_count + extra_lines;
        if total_hidden > 0 {
            first_spans.push(Span::styled(
                format!("  (+{total_hidden} more — Enter to expand)"),
                Style::default().fg(theme::LOG_DIM_FG),
            ));
        }
    }

    let mut first_line = Line::from(first_spans);
    if let Some(fs) = focus_style {
        first_line = first_line.style(fs);
    }

    // For collapsed turns or single-line content, return just the first line
    if turn_collapsed || content_lines.len() <= 1 {
        return vec![first_line];
    }

    // Expanded multi-line: continuation lines indented to the content column
    let indent = " ".repeat(LOG_PREFIX_WIDTH);
    let mut result = Vec::with_capacity(content_lines.len());
    result.push(first_line);

    if use_markdown {
        // Render remaining lines as markdown
        let remaining = content_lines[1..].join("\n");
        let md_lines = markdown::render_markdown(&remaining, content_style);
        for md_line in md_lines {
            let mut spans = vec![Span::styled(indent.clone(), Style::default())];
            spans.extend(md_line.spans);
            let mut cont_line = Line::from(spans);
            if let Some(fs) = focus_style {
                cont_line = cont_line.style(fs);
            }
            result.push(cont_line);
        }
    } else {
        for text in &content_lines[1..] {
            let mut cont_line = Line::from(vec![
                Span::styled(indent.clone(), Style::default()),
                Span::styled(text.to_string(), content_style),
            ]);
            if let Some(fs) = focus_style {
                cont_line = cont_line.style(fs);
            }
            result.push(cont_line);
        }
    }

    result
}

fn format_log_entry(entry: &LogEntry) -> Vec<Line<'static>> {
    format_log_entry_with_turn(entry, &std::collections::HashSet::new(), false, 0)
}

/// Render inspect overlay for one focused log entry.
pub fn render_inspect_overlay(f: &mut Frame, area: Rect, app: &App, view: &ViewState) {
    if !view.show_inspect {
        return;
    }

    let Some(selected_index) = view.inspect_index else {
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
        LogLevel::Detail => "DETAIL",
        LogLevel::Debug => "DEBUG",
    };

    let width = area.width.saturating_sub(4).max(20);
    let height = area.height.saturating_sub(4).max(6);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let centered = Rect::new(x, y, width, height);

    f.render_widget(ratatui::widgets::Clear, centered);

    let mut body = vec![
        Line::from(vec![Span::styled(
            format!(" [{}] entry #{} ", level_text, selected_index),
            Style::default()
                .fg(theme::HELP_KEY_FG)
                .add_modifier(Modifier::BOLD),
        )]),
        Line::default(),
    ];
    // Render content — use markdown for SubAgent/Model entries
    if matches!(entry.level, LogLevel::SubAgent | LogLevel::Model) {
        let base = Style::default().fg(theme::LOG_FG);
        body.extend(markdown::render_markdown(&entry.content, base));
    } else {
        for content_line in entry.content.split('\n') {
            body.push(Line::from(content_line.to_string()));
        }
    }
    body.push(Line::default());
    body.push(Line::from(vec![Span::styled(
        " ↑↓=scroll  ←→=prev/next entry  Esc/i=close ",
        Style::default().fg(theme::LOG_DIM_FG),
    )]));

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

    let widget = Paragraph::new(body)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((view.inspect_scroll, 0));
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

    // Command preview — one Line per sub-command for multi-line commands
    let cmd_lines: Vec<Line> = command
        .split('\n')
        .map(|line| {
            Line::from(vec![Span::styled(
                format!(" {}", line),
                Style::default().fg(theme::APPROVAL_CMD_FG),
            )])
        })
        .collect();
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

/// Render the follow-up input panel (conditional, at bottom).
pub fn render_follow_up_panel(f: &mut Frame, area: Rect, app: &mut App) {
    let chunks = ratatui::layout::Layout::default()
        .direction(ratatui::layout::Direction::Vertical)
        .constraints([
            ratatui::layout::Constraint::Length(1),
            ratatui::layout::Constraint::Min(1),
            ratatui::layout::Constraint::Length(1),
        ])
        .split(area);

    // Prompt
    let prompt_line = Line::from(vec![
        Span::styled(
            " Follow-up: ",
            Style::default()
                .fg(theme::INPUT_QUESTION_FG)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("(round {})", app.round),
            Style::default().fg(theme::LOG_DIM_FG),
        ),
    ]);
    f.render_widget(
        Paragraph::new(prompt_line).style(Style::default().bg(theme::INPUT_BG)),
        chunks[0],
    );

    // Text area
    if let Some(ref textarea) = app.follow_up_textarea {
        f.render_widget(textarea, chunks[1]);
    }

    // Hint
    let hint = Line::from(vec![Span::styled(
        " Enter=submit | Shift+Enter=newline | Esc/q=quit",
        Style::default().fg(theme::INPUT_HINT_FG),
    )]);
    f.render_widget(
        Paragraph::new(hint).style(Style::default().bg(theme::INPUT_BG)),
        chunks[2],
    );
}

/// Render a slim reminder bar when the follow-up input is hidden but the session is still waiting.
pub fn render_follow_up_reminder(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::INPUT_QUESTION_FG))
        .style(Style::default().bg(theme::INPUT_BG));

    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.height == 0 {
        return;
    }

    let line = Line::from(vec![
        Span::styled(
            " Press ",
            Style::default().fg(theme::LOG_DIM_FG),
        ),
        Span::styled(
            "f",
            Style::default()
                .fg(theme::HELP_KEY_FG)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" to write a follow-up (round {}), ", app.round),
            Style::default().fg(theme::LOG_DIM_FG),
        ),
        Span::styled(
            "q",
            Style::default()
                .fg(theme::HELP_KEY_FG)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " to quit",
            Style::default().fg(theme::LOG_DIM_FG),
        ),
    ]);
    f.render_widget(
        Paragraph::new(line).style(Style::default().bg(theme::INPUT_BG)),
        inner,
    );
}

/// Render help overlay.
pub fn render_help_overlay(f: &mut Frame, area: Rect) {
    let help_text = vec![
        ("q / Ctrl-C", "Quit"),
        ("v", "Cycle verbosity profile"),
        ("Tab/1/2/3", "Log tab: All / Agent / Presence"),
        ("Enter/\u{2192}", "Expand turn details"),
        ("\u{2190}", "Collapse turn details"),
        ("i", "Inspect focused log entry"),
        ("f", "Re-open follow-up input"),
        ("Esc", "Browse log (hides input panel)"),
        ("Up/Down", "Scroll log"),
        ("PgUp/PgDn", "Scroll log (page)"),
        ("Home/End", "Jump to start/end"),
        ("+/-", "Cycle autonomy level"),
        ("d", "Toggle user display access"),
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
    let width = 50.min(area.width);
    let height = (help_text.len() as u16 + 2).min(area.height);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let centered = Rect::new(x, y, width, height);

    // Clear background
    f.render_widget(ratatui::widgets::Clear, centered);

    let widget = Paragraph::new(lines).block(block);
    f.render_widget(widget, centered);
}

fn format_token_count(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M tok", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}K tok", tokens as f64 / 1_000.0)
    } else {
        format!("{} tok", tokens)
    }
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

pub(crate) fn truncate_utf8(s: &str, max: usize) -> &str {
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

    use crate::tui::app::LogSource;

    fn test_entry(level: LogLevel, content: &str) -> LogEntry {
        LogEntry {
            ts: "00:00:00".to_string(),
            level,
            content: content.to_string(),
            source: LogSource::System,
            turn: None,
        }
    }

    #[test]
    fn format_log_entry_info() {
        let entry = test_entry(LogLevel::Info, "test message");
        let lines = format_log_entry(&entry);
        assert_eq!(lines.len(), 1);
        // 4 spans: turn indicator, timestamp, level, content
        assert_eq!(lines[0].spans.len(), 4);
    }

    #[test]
    fn format_log_entry_debug_hidden() {
        let entry = test_entry(LogLevel::Debug, "debug msg");
        let lines = format_log_entry(&entry);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans.len(), 4);
    }

    #[test]
    fn format_log_entry_debug_shown() {
        let entry = test_entry(LogLevel::Debug, "debug msg");
        let lines = format_log_entry(&entry);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans.len(), 4);
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
            LogLevel::Detail,
            LogLevel::Debug,
        ];
        for level in levels {
            let entry = test_entry(level, "test");
            let _ = format_log_entry(&entry);
        }
    }

    #[test]
    fn format_log_entry_multiline() {
        let entry = test_entry(LogLevel::Model, "line one\nline two\nline three");
        let lines = format_log_entry(&entry);
        // No turn → never collapsed → all 3 lines rendered
        assert_eq!(lines.len(), 3);
        // First line: full prefix (4 spans)
        assert_eq!(lines[0].spans.len(), 4);
        // Continuation lines: indent + content (2 spans)
        assert_eq!(lines[1].spans.len(), 2);
        assert_eq!(lines[2].spans.len(), 2);
    }

    #[test]
    fn format_log_entry_multiline_collapsed_turn() {
        let entry = LogEntry {
            ts: "00:00:00".to_string(),
            level: LogLevel::Model,
            content: "first line\nsecond line\nthird line".to_string(),
            source: LogSource::System,
            turn: Some(1),
        };
        // Turn 1 not in expanded set → collapsed → single line
        let expanded = std::collections::HashSet::new();
        let lines = format_log_entry_with_turn(&entry, &expanded, false, 0);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn format_log_entry_multiline_expanded_turn() {
        let entry = LogEntry {
            ts: "00:00:00".to_string(),
            level: LogLevel::Model,
            content: "first line\nsecond line\nthird line".to_string(),
            source: LogSource::System,
            turn: Some(1),
        };
        // Turn 1 expanded → all lines
        let mut expanded = std::collections::HashSet::new();
        expanded.insert(1);
        let lines = format_log_entry_with_turn(&entry, &expanded, false, 0);
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn format_log_entry_collapsed_shows_hidden_hint() {
        let entry = LogEntry {
            ts: "00:00:00".to_string(),
            level: LogLevel::Model,
            content: "exec ls -la".to_string(),
            source: LogSource::Agent,
            turn: Some(1),
        };
        // Collapsed with 5 hidden entries → hint appended
        let expanded = std::collections::HashSet::new();
        let lines = format_log_entry_with_turn(&entry, &expanded, false, 5);
        assert_eq!(lines.len(), 1);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("+5 more"), "expected hidden hint, got: {}", text);
        assert!(text.contains("Enter to expand"), "expected expand instruction");
    }

    #[test]
    fn format_token_count_small() {
        assert_eq!(format_token_count(500), "500 tok");
    }

    #[test]
    fn format_token_count_thousands() {
        assert_eq!(format_token_count(15_200), "15.2K tok");
    }

    #[test]
    fn format_token_count_millions() {
        assert_eq!(format_token_count(2_500_000), "2.5M tok");
    }

    #[test]
    fn format_token_count_exact_k() {
        assert_eq!(format_token_count(1_000), "1.0K tok");
    }
}
