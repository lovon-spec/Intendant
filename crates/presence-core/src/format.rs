use crate::types::PresenceEvent;

/// Format a PresenceEvent into a human-readable string for model context.
pub fn format_event(event: &PresenceEvent) -> String {
    match event {
        PresenceEvent::PhaseChanged { phase } => format!("Phase changed to: {}", phase),
        PresenceEvent::TaskComplete { reason, summary } => {
            if let Some(s) = summary {
                format!("Task complete ({}): {}", reason, s)
            } else {
                format!("Task complete: {}", reason)
            }
        }
        PresenceEvent::ApprovalNeeded {
            id,
            preview,
            category,
        } => format!(
            "Approval needed (id={}, category={}): {}",
            id, category, preview
        ),
        PresenceEvent::HumanQuestion { question } => {
            format!("Worker has a question: {}", question)
        }
        PresenceEvent::BudgetWarning { pct, remaining } => {
            format!(
                "Budget warning: {:.0}% used, {} tokens remaining",
                pct * 100.0,
                remaining
            )
        }
        PresenceEvent::RoundComplete {
            round,
            turns_in_round,
        } => format!("Round {} complete ({} turns)", round, turns_in_round),
        PresenceEvent::Error { message } => format!("Error: {}", message),
    }
}

/// Truncate a string to `max` characters, appending "..." if truncated.
/// Uses char boundaries to avoid panics on non-ASCII input.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let end = s
            .char_indices()
            .nth(max)
            .map(|(i, _)| i)
            .unwrap_or(s.len());
        format!("{}...", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_event_variants() {
        let event = PresenceEvent::PhaseChanged {
            phase: "thinking".to_string(),
        };
        assert_eq!(format_event(&event), "Phase changed to: thinking");

        let event = PresenceEvent::TaskComplete {
            reason: "done".to_string(),
            summary: None,
        };
        assert_eq!(format_event(&event), "Task complete: done");

        let event = PresenceEvent::TaskComplete {
            reason: "done".to_string(),
            summary: Some("analyzed project".to_string()),
        };
        assert_eq!(
            format_event(&event),
            "Task complete (done): analyzed project"
        );

        let event = PresenceEvent::Error {
            message: "oops".to_string(),
        };
        assert_eq!(format_event(&event), "Error: oops");
    }

    #[test]
    fn truncate_short_string() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_long_string() {
        assert_eq!(truncate("hello world", 5), "hello...");
    }

    #[test]
    fn truncate_unicode_safe() {
        // 3-char string, truncate at 2 — should not panic
        let s = "a\u{00e9}b"; // "aéb"
        assert_eq!(truncate(s, 2), "aé...");
    }
}
