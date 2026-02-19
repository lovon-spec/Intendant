use ratatui::layout::{Constraint, Direction, Layout, Rect};

/// Panel visibility configuration.
#[derive(Debug, Clone)]
pub struct PanelConfig {
    pub show_status: bool,
    pub show_action: bool,
    pub show_log: bool,
}

impl Default for PanelConfig {
    fn default() -> Self {
        Self {
            show_status: true,
            show_action: true,
            show_log: true,
        }
    }
}

/// Result of layout calculation with named areas.
#[derive(Debug, Clone)]
pub struct AppLayout {
    pub status_bar: Option<Rect>,
    pub action_panel: Option<Rect>,
    pub log_panel: Option<Rect>,
    pub bottom_panel: Option<Rect>, // approval or input
}

/// Calculate the layout for the TUI given terminal size and panel config.
/// `bottom_height` is non-zero when approval or input panel is active.
pub fn calculate_layout(area: Rect, config: &PanelConfig, bottom_height: u16) -> AppLayout {
    let mut constraints = Vec::new();
    let mut slot_names: Vec<&str> = Vec::new();

    if config.show_status {
        constraints.push(Constraint::Length(1));
        slot_names.push("status");
    }

    if config.show_action {
        constraints.push(Constraint::Length(2));
        slot_names.push("action");
    }

    // Log fills remaining space
    if config.show_log {
        constraints.push(Constraint::Min(3));
        slot_names.push("log");
    }

    if bottom_height > 0 {
        constraints.push(Constraint::Length(bottom_height));
        slot_names.push("bottom");
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    let mut layout = AppLayout {
        status_bar: None,
        action_panel: None,
        log_panel: None,
        bottom_panel: None,
    };

    for (i, &name) in slot_names.iter().enumerate() {
        match name {
            "status" => layout.status_bar = Some(chunks[i]),
            "action" => layout.action_panel = Some(chunks[i]),
            "log" => layout.log_panel = Some(chunks[i]),
            "bottom" => layout.bottom_panel = Some(chunks[i]),
            _ => {}
        }
    }

    layout
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_area() -> Rect {
        Rect::new(0, 0, 80, 24)
    }

    #[test]
    fn default_layout_all_panels() {
        let config = PanelConfig::default();
        let layout = calculate_layout(test_area(), &config, 0);

        assert!(layout.status_bar.is_some());
        assert!(layout.action_panel.is_some());
        assert!(layout.log_panel.is_some());
        assert!(layout.bottom_panel.is_none());
    }

    #[test]
    fn layout_with_bottom_panel() {
        let config = PanelConfig::default();
        let layout = calculate_layout(test_area(), &config, 4);

        assert!(layout.status_bar.is_some());
        assert!(layout.log_panel.is_some());
        assert!(layout.bottom_panel.is_some());
        assert_eq!(layout.bottom_panel.unwrap().height, 4);
    }

    #[test]
    fn layout_status_bar_is_one_line() {
        let config = PanelConfig::default();
        let layout = calculate_layout(test_area(), &config, 0);

        assert_eq!(layout.status_bar.unwrap().height, 1);
    }

    #[test]
    fn layout_action_panel_is_two_lines() {
        let config = PanelConfig::default();
        let layout = calculate_layout(test_area(), &config, 0);

        assert_eq!(layout.action_panel.unwrap().height, 2);
    }

    #[test]
    fn layout_hidden_status() {
        let config = PanelConfig {
            show_status: false,
            show_action: true,
            show_log: true,
        };
        let layout = calculate_layout(test_area(), &config, 0);

        assert!(layout.status_bar.is_none());
        assert!(layout.action_panel.is_some());
        assert!(layout.log_panel.is_some());
    }

    #[test]
    fn layout_hidden_action() {
        let config = PanelConfig {
            show_status: true,
            show_action: false,
            show_log: true,
        };
        let layout = calculate_layout(test_area(), &config, 0);

        assert!(layout.status_bar.is_some());
        assert!(layout.action_panel.is_none());
        assert!(layout.log_panel.is_some());
    }

    #[test]
    fn layout_log_fills_remaining() {
        let config = PanelConfig::default();
        let layout = calculate_layout(test_area(), &config, 0);

        let log = layout.log_panel.unwrap();
        // status(1) + action(2) + log(remaining) = 24
        assert_eq!(log.height, 21);
    }

    #[test]
    fn layout_log_fills_with_bottom() {
        let config = PanelConfig::default();
        let layout = calculate_layout(test_area(), &config, 4);

        let log = layout.log_panel.unwrap();
        // status(1) + action(2) + log(remaining) + bottom(4) = 24
        assert_eq!(log.height, 17);
    }

    #[test]
    fn layout_small_terminal() {
        let small = Rect::new(0, 0, 40, 8);
        let config = PanelConfig::default();
        let layout = calculate_layout(small, &config, 0);

        assert!(layout.status_bar.is_some());
        assert!(layout.log_panel.is_some());
        let log = layout.log_panel.unwrap();
        assert!(log.height >= 3);
    }

    #[test]
    fn panel_config_default() {
        let config = PanelConfig::default();
        assert!(config.show_status);
        assert!(config.show_action);
        assert!(config.show_log);
    }
}
