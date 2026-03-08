use serde::{Deserialize, Serialize};

/// Configuration for the presence layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresenceConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,

    // --- Text mode (TUI / MCP) ---
    /// Provider name for text mode (e.g. "gemini", "anthropic", "openai").
    /// Default: auto-detect (prefers gemini when GEMINI_API_KEY is set).
    #[serde(default)]
    pub provider: Option<String>,
    /// Model for text mode. Default: "gemini-2.5-flash".
    #[serde(default)]
    pub model: Option<String>,
    /// Context window size for the text-mode presence conversation.
    #[serde(default = "default_context_window")]
    pub context_window: u64,

    // --- Live mode (browser-side voice/realtime) ---
    /// Provider for the browser-side live model (e.g. "gemini", "openai").
    #[serde(default)]
    pub live_provider: Option<String>,
    /// Model name for live mode.
    #[serde(default)]
    pub live_model: Option<String>,
    /// Context window for the live model.
    #[serde(default = "default_live_context_window")]
    pub live_context_window: u64,
}

fn default_true() -> bool {
    true
}

fn default_context_window() -> u64 {
    1_048_576
}

fn default_live_context_window() -> u64 {
    32_768
}

/// Default text presence model.
pub const DEFAULT_TEXT_MODEL: &str = "gemini-2.5-flash";
/// Preferred text presence model (Gemini 3 Flash, when available).
#[allow(dead_code)]
pub const PREFERRED_TEXT_MODEL: &str = "gemini-3-flash-preview";
/// Default text presence provider.
#[allow(dead_code)]
pub const DEFAULT_TEXT_PROVIDER: &str = "gemini";

impl Default for PresenceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            provider: None,
            model: None,
            context_window: default_context_window(),
            live_provider: None,
            live_model: None,
            live_context_window: default_live_context_window(),
        }
    }
}

/// A structured task submission from presence to the agent loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskEnvelope {
    pub task: String,
    #[serde(default)]
    pub force_direct: bool,
    #[serde(default)]
    pub context_hints: Vec<String>,
}

/// Filtered events pushed to the presence layer from the agent loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PresenceEvent {
    PhaseChanged { phase: String },
    TaskComplete { reason: String },
    ApprovalNeeded { id: u64, preview: String, category: String },
    HumanQuestion { question: String },
    BudgetWarning { pct: f64, remaining: u64 },
    RoundComplete { round: usize, turns_in_round: usize },
    Error { message: String },
}

/// Token usage snapshot from the presence layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresenceUsage {
    pub total_tokens: u64,
    pub context_window: u64,
    pub usage_pct: f64,
    pub provider: String,
    pub model: String,
}

/// Queryable snapshot of the agent's current state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentStateSnapshot {
    pub phase: String,
    pub turn: usize,
    pub budget_pct: f64,
    pub last_output_summary: String,
    pub last_command_preview: String,
    pub active_workers: Vec<String>,
}

/// Minimum interval between phase-change narrations (in milliseconds).
/// Phase events arriving faster than this are skipped.
pub const NARRATION_DEBOUNCE_MS: u64 = 500;

/// Presence turn offset to avoid collisions with agent turns in TUI collapse logic.
pub const PRESENCE_TURN_OFFSET: usize = 100_000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presence_config_defaults() {
        let config = PresenceConfig::default();
        assert!(config.enabled);
        assert!(config.provider.is_none());
        assert!(config.model.is_none());
        assert_eq!(config.context_window, 1_048_576);
        assert!(config.live_provider.is_none());
        assert!(config.live_model.is_none());
        assert_eq!(config.live_context_window, 32_768);
    }

    #[test]
    fn presence_config_deserialize_json() {
        let json_str = r#"{
            "enabled": false,
            "provider": "anthropic",
            "model": "claude-sonnet-4-5-20250929",
            "context_window": 200000
        }"#;
        let config: PresenceConfig = serde_json::from_str(json_str).unwrap();
        assert!(!config.enabled);
        assert_eq!(config.provider.as_deref(), Some("anthropic"));
        assert_eq!(config.model.as_deref(), Some("claude-sonnet-4-5-20250929"));
        assert_eq!(config.context_window, 200000);
    }

    #[test]
    fn task_envelope_roundtrip() {
        let envelope = TaskEnvelope {
            task: "fix the bug".to_string(),
            force_direct: true,
            context_hints: vec!["src/main.rs".to_string()],
        };
        let json = serde_json::to_string(&envelope).unwrap();
        let back: TaskEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back.task, "fix the bug");
        assert!(back.force_direct);
        assert_eq!(back.context_hints.len(), 1);
    }

    #[test]
    fn presence_event_serialize_roundtrip() {
        let event = PresenceEvent::ApprovalNeeded {
            id: 42,
            preview: "exec: rm -rf /tmp".to_string(),
            category: "Destructive".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: PresenceEvent = serde_json::from_str(&json).unwrap();
        match back {
            PresenceEvent::ApprovalNeeded { id, preview, category } => {
                assert_eq!(id, 42);
                assert_eq!(preview, "exec: rm -rf /tmp");
                assert_eq!(category, "Destructive");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn agent_state_snapshot_defaults() {
        let s = AgentStateSnapshot::default();
        assert!(s.phase.is_empty());
        assert_eq!(s.turn, 0);
        assert_eq!(s.budget_pct, 0.0);
        assert!(s.last_output_summary.is_empty());
        assert!(s.last_command_preview.is_empty());
        assert!(s.active_workers.is_empty());
    }
}
