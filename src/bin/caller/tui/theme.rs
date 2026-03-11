use ratatui::style::{Color, Modifier, Style};

// Status bar
pub const STATUS_BAR_BG: Color = Color::Rgb(30, 30, 46);
pub const STATUS_BAR_FG: Color = Color::Rgb(205, 214, 244);
pub const STATUS_PROVIDER_FG: Color = Color::Rgb(137, 180, 250);
pub const STATUS_MODEL_FG: Color = Color::Rgb(166, 227, 161);
pub const STATUS_TURN_FG: Color = Color::Rgb(249, 226, 175);

// Budget bar
pub const BUDGET_LOW_FG: Color = Color::Rgb(166, 227, 161); // green <50%
pub const BUDGET_MED_FG: Color = Color::Rgb(249, 226, 175); // yellow 50-85%
pub const BUDGET_HIGH_FG: Color = Color::Rgb(243, 139, 168); // red >85%

// Action panel
pub const ACTION_BG: Color = Color::Rgb(24, 24, 37);
pub const ACTION_THINKING_FG: Color = Color::Rgb(180, 190, 254);
pub const ACTION_RUNNING_FG: Color = Color::Rgb(148, 226, 213);
pub const ACTION_WAITING_FG: Color = Color::Rgb(249, 226, 175);
pub const ACTION_DONE_FG: Color = Color::Rgb(166, 227, 161);

// Log panel
#[allow(dead_code)]
pub const LOG_BG: Color = Color::Reset;
pub const LOG_FG: Color = Color::Rgb(205, 214, 244);
pub const LOG_DIM_FG: Color = Color::Rgb(127, 132, 156);
pub const LOG_MODEL_FG: Color = Color::Rgb(137, 180, 250);
pub const LOG_AGENT_FG: Color = Color::Rgb(148, 226, 213);
pub const LOG_ERROR_FG: Color = Color::Rgb(243, 139, 168);
pub const LOG_WARN_FG: Color = Color::Rgb(249, 226, 175);
pub const LOG_SUBAGENT_FG: Color = Color::Rgb(203, 166, 247);
pub const LOG_DETAIL_FG: Color = Color::Rgb(162, 168, 190);

// Approval panel
pub const APPROVAL_BG: Color = Color::Rgb(49, 50, 68);
pub const APPROVAL_FG: Color = Color::Rgb(249, 226, 175);
pub const APPROVAL_CMD_FG: Color = Color::Rgb(205, 214, 244);
pub const APPROVAL_HINT_FG: Color = Color::Rgb(127, 132, 156);

// Input panel
pub const INPUT_BG: Color = Color::Rgb(49, 50, 68);
#[allow(dead_code)]
pub const INPUT_FG: Color = Color::Rgb(205, 214, 244);
pub const INPUT_QUESTION_FG: Color = Color::Rgb(137, 180, 250);
pub const INPUT_HINT_FG: Color = Color::Rgb(127, 132, 156);

// Autonomy indicator
pub const AUTONOMY_LOW_FG: Color = Color::Rgb(243, 139, 168);
pub const AUTONOMY_MED_FG: Color = Color::Rgb(249, 226, 175);
pub const AUTONOMY_HIGH_FG: Color = Color::Rgb(148, 226, 213);
pub const AUTONOMY_FULL_FG: Color = Color::Rgb(166, 227, 161);

// Help overlay
pub const HELP_BG: Color = Color::Rgb(30, 30, 46);
pub const HELP_FG: Color = Color::Rgb(205, 214, 244);
pub const HELP_KEY_FG: Color = Color::Rgb(249, 226, 175);

// Spinner frames
pub const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn status_bar_style() -> Style {
    Style::default().bg(STATUS_BAR_BG).fg(STATUS_BAR_FG)
}

pub fn action_style_for_phase(phase: &str) -> Style {
    let fg = match phase {
        "thinking" => ACTION_THINKING_FG,
        "running" => ACTION_RUNNING_FG,
        "waiting" => ACTION_WAITING_FG,
        "done" => ACTION_DONE_FG,
        _ => ACTION_THINKING_FG,
    };
    Style::default().bg(ACTION_BG).fg(fg)
}

pub fn budget_color(pct: f64) -> Color {
    if pct < 50.0 {
        BUDGET_LOW_FG
    } else if pct < 85.0 {
        BUDGET_MED_FG
    } else {
        BUDGET_HIGH_FG
    }
}

pub fn autonomy_color(level: &str) -> Color {
    match level {
        "Low" => AUTONOMY_LOW_FG,
        "Medium" => AUTONOMY_MED_FG,
        "High" => AUTONOMY_HIGH_FG,
        "Full" => AUTONOMY_FULL_FG,
        _ => AUTONOMY_MED_FG,
    }
}

#[allow(dead_code)]
pub fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

#[allow(dead_code)]
pub fn dim() -> Style {
    Style::default().fg(LOG_DIM_FG)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_color_thresholds() {
        assert_eq!(budget_color(0.0), BUDGET_LOW_FG);
        assert_eq!(budget_color(49.9), BUDGET_LOW_FG);
        assert_eq!(budget_color(50.0), BUDGET_MED_FG);
        assert_eq!(budget_color(84.9), BUDGET_MED_FG);
        assert_eq!(budget_color(85.0), BUDGET_HIGH_FG);
        assert_eq!(budget_color(100.0), BUDGET_HIGH_FG);
    }

    #[test]
    fn spinner_frames_not_empty() {
        assert!(!SPINNER_FRAMES.is_empty());
    }

    #[test]
    fn action_style_variants() {
        let _ = action_style_for_phase("thinking");
        let _ = action_style_for_phase("running");
        let _ = action_style_for_phase("waiting");
        let _ = action_style_for_phase("done");
        let _ = action_style_for_phase("unknown");
    }

    #[test]
    fn autonomy_color_variants() {
        assert_eq!(autonomy_color("Low"), AUTONOMY_LOW_FG);
        assert_eq!(autonomy_color("Medium"), AUTONOMY_MED_FG);
        assert_eq!(autonomy_color("High"), AUTONOMY_HIGH_FG);
        assert_eq!(autonomy_color("Full"), AUTONOMY_FULL_FG);
        assert_eq!(autonomy_color("unknown"), AUTONOMY_MED_FG);
    }
}
