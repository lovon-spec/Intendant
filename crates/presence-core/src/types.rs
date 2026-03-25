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
    /// Frame IDs the user was looking at when they issued this task.
    /// Used to give the CU agent temporal context about what the user was referring to.
    #[serde(default)]
    pub reference_frame_ids: Vec<String>,
    /// Explicit display target for CU actions. When set, the CU pipeline
    /// targets this display instead of auto-resolving from env.
    /// Use "user_session" for the user's real display, or ":99" etc. for virtual.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_target: Option<String>,
}

/// Filtered events pushed to the presence layer from the agent loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PresenceEvent {
    PhaseChanged { phase: String },
    TaskComplete {
        reason: String,
        #[serde(default)]
        summary: Option<String>,
    },
    ApprovalNeeded { id: u64, preview: String, category: String },
    ApprovalResolved { id: u64, action: String },
    HumanQuestion { question: String },
    BudgetWarning { pct: f64, remaining: u64 },
    RoundComplete { round: usize, turns_in_round: usize },
    Error { message: String },
    /// A display became available.
    DisplayReady {
        display_id: u32,
        width: u32,
        height: u32,
        is_user_session: bool,
    },
    /// User granted agent access to their session display.
    UserDisplayGranted,
    /// User revoked agent access to their session display.
    UserDisplayRevoked,
}

/// Token usage snapshot from the presence layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresenceUsage {
    pub total_tokens: u64,
    pub context_window: u64,
    pub usage_pct: f64,
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub cached_tokens: u64,
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
    /// Pending approval details (set when phase is "waiting_approval").
    /// Cleared when the approval is resolved (agent starts running).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_approval: Option<PendingApprovalSnapshot>,
    /// Full result text from the last completed task (available via `query_detail` scope `task_result`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_task_result: Option<String>,
}

/// Serializable snapshot of a pending approval for the live model bootstrap.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingApprovalSnapshot {
    pub id: u64,
    pub command_preview: String,
    pub category: String,
}

impl AgentStateSnapshot {
    /// Update state from a server-sent event (OutboundEvent JSON).
    /// Returns an optional `PresenceEvent` if this event is worth narrating.
    pub fn update_from_server_event(&mut self, event: &serde_json::Value) -> Option<PresenceEvent> {
        let event_type = event.get("event")?.as_str()?;
        match event_type {
            "turn_started" => {
                if let Some(t) = event["turn"].as_u64() {
                    self.turn = t as usize;
                }
                if let Some(b) = event["budget_pct"].as_f64() {
                    self.budget_pct = b;
                }
                self.phase = "thinking".to_string();
                Some(PresenceEvent::PhaseChanged {
                    phase: "thinking".to_string(),
                })
            }
            "status" => {
                if let Some(p) = event["phase"].as_str() {
                    self.phase = p.to_string();
                    Some(PresenceEvent::PhaseChanged {
                        phase: p.to_string(),
                    })
                } else {
                    None
                }
            }
            "agent_output" => {
                let stdout = event["stdout"].as_str().unwrap_or("");
                self.last_output_summary = crate::truncate(stdout, 500);
                None // agent_output is not narrated by default
            }
            "approval_required" => {
                let id = event["id"].as_u64().unwrap_or(0);
                let command = event["command"].as_str().unwrap_or("").to_string();
                let category = event["category"].as_str().unwrap_or("").to_string();
                self.phase = "waiting_approval".to_string();
                self.pending_approval = Some(PendingApprovalSnapshot {
                    id,
                    command_preview: command.clone(),
                    category: category.clone(),
                });
                Some(PresenceEvent::ApprovalNeeded {
                    id,
                    preview: command,
                    category,
                })
            }
            "agent_started" => {
                let preview = event["commands_preview"].as_str().unwrap_or("").to_string();
                self.on_agent_started(&preview);
                Some(PresenceEvent::PhaseChanged {
                    phase: "running_agent".to_string(),
                })
            }
            "approval_resolved" => {
                let id = event["id"].as_u64().unwrap_or(0);
                let action = event["action"].as_str().unwrap_or("").to_string();
                self.pending_approval = None;
                if action == "deny" {
                    self.phase = "done".to_string();
                } else {
                    self.phase = "running_agent".to_string();
                }
                Some(PresenceEvent::ApprovalResolved { id, action })
            }
            "ask_human" => {
                let question = event["question"].as_str().unwrap_or("").to_string();
                self.phase = "waiting_human".to_string();
                Some(PresenceEvent::HumanQuestion { question })
            }
            "task_complete" => {
                let reason = event["reason"].as_str().unwrap_or("done").to_string();
                let summary = event["summary"].as_str().map(|s| s.to_string());
                self.phase = "idle".to_string();
                self.pending_approval = None;
                self.last_task_result = Some("(available via query_detail)".to_string());
                Some(PresenceEvent::TaskComplete { reason, summary })
            }
            "round_complete" => {
                self.phase = "idle".to_string();
                self.pending_approval = None;
                let round = event["round"].as_u64().unwrap_or(0) as usize;
                let turns = event["turns_in_round"].as_u64().unwrap_or(0) as usize;
                Some(PresenceEvent::RoundComplete {
                    round,
                    turns_in_round: turns,
                })
            }
            "error" => {
                let message = event["message"].as_str().unwrap_or("unknown error").to_string();
                Some(PresenceEvent::Error { message })
            }
            _ => None,
        }
    }

    /// Update state when agent starts running (clears pending approval).
    pub fn on_agent_started(&mut self, commands_preview: &str) {
        self.phase = "running_agent".to_string();
        self.last_command_preview = commands_preview.to_string();
        self.pending_approval = None;
    }
}

/// Minimum interval between phase-change narrations (in milliseconds).
/// Phase events arriving faster than this are skipped.
pub const NARRATION_DEBOUNCE_MS: u64 = 500;

/// Presence turn offset to avoid collisions with agent turns in TUI collapse logic.
pub const PRESENCE_TURN_OFFSET: usize = 100_000;

// ── Video / frame types ──

/// A unique frame identifier, assigned client-side.
/// Format: `{stream_prefix}-f{monotonic_counter}`, e.g. `cam0-f00047`.
pub type FrameId = String;

/// Metadata for a single captured frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameMeta {
    /// Unique frame identifier.
    pub frame_id: FrameId,
    /// Source stream (e.g. "cam0", "display:99").
    pub stream: String,
    /// UTC timestamp when the frame was captured.
    pub timestamp: String,
    /// Whether this frame was sent to the live model.
    pub sent_to_live: bool,
    /// Resolution of the live-res version sent to the model (e.g. "768x768").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_resolution: Option<String>,
    /// Resolution of the HQ version stored server-side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hq_resolution: Option<String>,
}

/// Summary of active video streams for the presence model.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VideoState {
    /// Currently active video streams.
    pub active_streams: Vec<String>,
    /// Most recent frame ID across all streams.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_frame_id: Option<FrameId>,
    /// Total frames captured this session.
    pub total_frames: u64,
}

// ── Presence session protocol types ──

/// Browser → server: initiate presence handshake.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresenceConnect {
    /// If reconnecting, the session ID from a previous `PresenceWelcome`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_session_id: Option<String>,
    /// Last event sequence number the client has seen (for replay).
    #[serde(default)]
    pub last_event_seq: u64,
}

/// Server → browser: welcome response with state + replay window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresenceWelcome {
    pub session_id: String,
    pub state: AgentStateSnapshot,
    pub events: Vec<SequencedPresenceEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_checkpoint_summary: Option<String>,
    pub current_seq: u64,
}

/// A `PresenceEvent` with a monotonic sequence number.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SequencedPresenceEvent {
    pub seq: u64,
    pub event: PresenceEvent,
}

/// Browser → server: voice transcript log entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceLog {
    pub text: String,
    pub seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_context: Option<String>,
}

/// Browser → server: context checkpoint from presence model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresenceCheckpoint {
    pub summary: String,
    pub last_event_seq: u64,
}

/// Server → browser: acknowledgement of a checkpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresenceCheckpointAck {
    pub seq: u64,
}

/// Bounded ring buffer of sequenced presence events for replay on reconnect.
#[derive(Debug, Clone)]
pub struct PresenceEventWindow {
    events: Vec<SequencedPresenceEvent>,
    capacity: usize,
    next_seq: u64,
}

impl PresenceEventWindow {
    /// Create a new event window with the given capacity (default 200).
    pub fn new(capacity: usize) -> Self {
        Self {
            events: Vec::with_capacity(capacity),
            capacity,
            next_seq: 1,
        }
    }

    /// Push a new event, assigning the next sequence number. Returns the assigned seq.
    pub fn push(&mut self, event: PresenceEvent) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.events.push(SequencedPresenceEvent { seq, event });
        // Trim from the front if over capacity
        if self.events.len() > self.capacity {
            let excess = self.events.len() - self.capacity;
            self.events.drain(..excess);
        }
        seq
    }

    /// Return all events with seq > `since_seq`.
    pub fn since(&self, since_seq: u64) -> Vec<SequencedPresenceEvent> {
        self.events
            .iter()
            .filter(|e| e.seq > since_seq)
            .cloned()
            .collect()
    }

    /// The current (latest assigned) sequence number, or 0 if no events pushed.
    pub fn current_seq(&self) -> u64 {
        self.next_seq.saturating_sub(1)
    }

    /// Clear all events and reset the sequence counter.
    pub fn clear(&mut self) {
        self.events.clear();
        self.next_seq = 1;
    }
}

impl Default for PresenceEventWindow {
    fn default() -> Self {
        Self::new(200)
    }
}

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
            reference_frame_ids: vec!["display:99-f00012".to_string()],
            display_target: Some("user_session".to_string()),
        };
        let json = serde_json::to_string(&envelope).unwrap();
        let back: TaskEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back.task, "fix the bug");
        assert!(back.force_direct);
        assert_eq!(back.context_hints.len(), 1);
        assert_eq!(back.reference_frame_ids.len(), 1);
        assert_eq!(back.display_target.as_deref(), Some("user_session"));
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
        assert!(s.pending_approval.is_none());
    }

    #[test]
    fn agent_state_snapshot_with_pending_approval() {
        let s = AgentStateSnapshot {
            phase: "waiting_approval".to_string(),
            pending_approval: Some(PendingApprovalSnapshot {
                id: 1,
                command_preview: "exec: ls -la /tmp".to_string(),
                category: "CommandExec".to_string(),
            }),
            ..Default::default()
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("pending_approval"));
        assert!(json.contains("exec: ls -la /tmp"));
        let back: AgentStateSnapshot = serde_json::from_str(&json).unwrap();
        assert!(back.pending_approval.is_some());
        let pa = back.pending_approval.unwrap();
        assert_eq!(pa.id, 1);
        assert_eq!(pa.command_preview, "exec: ls -la /tmp");
    }

    #[test]
    fn agent_state_snapshot_without_approval_omits_field() {
        let s = AgentStateSnapshot::default();
        let json = serde_json::to_string(&s).unwrap();
        // skip_serializing_if = "Option::is_none" should omit the field
        assert!(!json.contains("pending_approval"));
    }

    #[test]
    fn update_from_server_event_turn_started() {
        let mut s = AgentStateSnapshot::default();
        let event = serde_json::json!({"event": "turn_started", "turn": 5, "budget_pct": 0.3});
        let narration = s.update_from_server_event(&event);
        assert_eq!(s.turn, 5);
        assert!((s.budget_pct - 0.3).abs() < f64::EPSILON);
        assert_eq!(s.phase, "thinking");
        assert!(narration.is_some());
    }

    #[test]
    fn update_from_server_event_approval_required() {
        let mut s = AgentStateSnapshot::default();
        let event = serde_json::json!({
            "event": "approval_required",
            "id": 42,
            "command": "rm -rf /tmp",
            "category": "Destructive"
        });
        let narration = s.update_from_server_event(&event);
        assert_eq!(s.phase, "waiting_approval");
        assert!(s.pending_approval.is_some());
        let pa = s.pending_approval.as_ref().unwrap();
        assert_eq!(pa.id, 42);
        assert_eq!(pa.command_preview, "rm -rf /tmp");
        assert!(narration.is_some());
    }

    #[test]
    fn update_from_server_event_task_complete_clears_approval() {
        let mut s = AgentStateSnapshot {
            phase: "waiting_approval".to_string(),
            pending_approval: Some(PendingApprovalSnapshot {
                id: 1,
                command_preview: "test".to_string(),
                category: "exec".to_string(),
            }),
            ..Default::default()
        };
        let event = serde_json::json!({"event": "task_complete", "reason": "all done"});
        let narration = s.update_from_server_event(&event);
        assert_eq!(s.phase, "idle");
        assert!(s.pending_approval.is_none());
        assert!(narration.is_some());
    }

    #[test]
    fn update_from_server_event_agent_output_not_narrated() {
        let mut s = AgentStateSnapshot::default();
        let event = serde_json::json!({"event": "agent_output", "stdout": "hello world"});
        let narration = s.update_from_server_event(&event);
        assert!(narration.is_none());
        assert_eq!(s.last_output_summary, "hello world");
    }

    #[test]
    fn update_from_server_event_unknown_ignored() {
        let mut s = AgentStateSnapshot::default();
        let event = serde_json::json!({"event": "usage_update", "tokens": 1000});
        let narration = s.update_from_server_event(&event);
        assert!(narration.is_none());
    }

    #[test]
    fn on_agent_started_clears_approval() {
        let mut s = AgentStateSnapshot {
            pending_approval: Some(PendingApprovalSnapshot {
                id: 1,
                command_preview: "test".to_string(),
                category: "exec".to_string(),
            }),
            ..Default::default()
        };
        s.on_agent_started("cargo test");
        assert_eq!(s.phase, "running_agent");
        assert_eq!(s.last_command_preview, "cargo test");
        assert!(s.pending_approval.is_none());
    }

    // ── Presence protocol type tests ──

    #[test]
    fn presence_connect_roundtrip() {
        let msg = PresenceConnect {
            server_session_id: Some("sess-123".to_string()),
            last_event_seq: 42,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: PresenceConnect = serde_json::from_str(&json).unwrap();
        assert_eq!(back.server_session_id.as_deref(), Some("sess-123"));
        assert_eq!(back.last_event_seq, 42);
    }

    #[test]
    fn presence_connect_minimal() {
        let json = r#"{"last_event_seq":0}"#;
        let msg: PresenceConnect = serde_json::from_str(json).unwrap();
        assert!(msg.server_session_id.is_none());
        assert_eq!(msg.last_event_seq, 0);
    }

    #[test]
    fn presence_welcome_roundtrip() {
        let welcome = PresenceWelcome {
            session_id: "sess-abc".to_string(),
            state: AgentStateSnapshot::default(),
            events: vec![SequencedPresenceEvent {
                seq: 1,
                event: PresenceEvent::PhaseChanged {
                    phase: "thinking".to_string(),
                },
            }],
            last_checkpoint_summary: Some("All good".to_string()),
            current_seq: 1,
        };
        let json = serde_json::to_string(&welcome).unwrap();
        let back: PresenceWelcome = serde_json::from_str(&json).unwrap();
        assert_eq!(back.session_id, "sess-abc");
        assert_eq!(back.events.len(), 1);
        assert_eq!(back.events[0].seq, 1);
        assert_eq!(back.current_seq, 1);
        assert_eq!(back.last_checkpoint_summary.as_deref(), Some("All good"));
    }

    #[test]
    fn sequenced_presence_event_roundtrip() {
        let se = SequencedPresenceEvent {
            seq: 5,
            event: PresenceEvent::TaskComplete {
                reason: "done".to_string(),
                summary: Some("result text".to_string()),
            },
        };
        let json = serde_json::to_string(&se).unwrap();
        let back: SequencedPresenceEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back.seq, 5);
        match back.event {
            PresenceEvent::TaskComplete { reason, summary } => {
                assert_eq!(reason, "done");
                assert_eq!(summary.as_deref(), Some("result text"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn voice_log_roundtrip() {
        let vl = VoiceLog {
            text: "hello world".to_string(),
            seq: 3,
            tool_context: Some("check_status".to_string()),
        };
        let json = serde_json::to_string(&vl).unwrap();
        let back: VoiceLog = serde_json::from_str(&json).unwrap();
        assert_eq!(back.text, "hello world");
        assert_eq!(back.seq, 3);
        assert_eq!(back.tool_context.as_deref(), Some("check_status"));
    }

    #[test]
    fn voice_log_minimal() {
        let json = r#"{"text":"hi","seq":1}"#;
        let vl: VoiceLog = serde_json::from_str(json).unwrap();
        assert_eq!(vl.text, "hi");
        assert!(vl.tool_context.is_none());
    }

    #[test]
    fn presence_checkpoint_roundtrip() {
        let cp = PresenceCheckpoint {
            summary: "Agent completed 3 tasks".to_string(),
            last_event_seq: 15,
        };
        let json = serde_json::to_string(&cp).unwrap();
        let back: PresenceCheckpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(back.summary, "Agent completed 3 tasks");
        assert_eq!(back.last_event_seq, 15);
    }

    #[test]
    fn presence_checkpoint_ack_roundtrip() {
        let ack = PresenceCheckpointAck { seq: 15 };
        let json = serde_json::to_string(&ack).unwrap();
        let back: PresenceCheckpointAck = serde_json::from_str(&json).unwrap();
        assert_eq!(back.seq, 15);
    }

    // ── PresenceEventWindow tests ──

    #[test]
    fn event_window_push_and_since() {
        let mut w = PresenceEventWindow::new(10);
        assert_eq!(w.current_seq(), 0);

        let s1 = w.push(PresenceEvent::PhaseChanged {
            phase: "thinking".to_string(),
        });
        assert_eq!(s1, 1);
        assert_eq!(w.current_seq(), 1);

        let s2 = w.push(PresenceEvent::TaskComplete {
            reason: "done".to_string(),
            summary: None,
        });
        assert_eq!(s2, 2);
        assert_eq!(w.current_seq(), 2);

        // since(0) → all events
        let all = w.since(0);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].seq, 1);
        assert_eq!(all[1].seq, 2);

        // since(1) → only event 2
        let after1 = w.since(1);
        assert_eq!(after1.len(), 1);
        assert_eq!(after1[0].seq, 2);

        // since(2) → empty
        assert!(w.since(2).is_empty());
    }

    #[test]
    fn event_window_capacity_trimming() {
        let mut w = PresenceEventWindow::new(3);
        for i in 0..5 {
            w.push(PresenceEvent::PhaseChanged {
                phase: format!("phase_{}", i),
            });
        }
        assert_eq!(w.current_seq(), 5);
        // Only last 3 events should remain
        let all = w.since(0);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].seq, 3);
        assert_eq!(all[1].seq, 4);
        assert_eq!(all[2].seq, 5);
    }

    #[test]
    fn event_window_clear() {
        let mut w = PresenceEventWindow::new(10);
        w.push(PresenceEvent::PhaseChanged {
            phase: "thinking".to_string(),
        });
        w.push(PresenceEvent::TaskComplete {
            reason: "done".to_string(),
            summary: None,
        });
        assert_eq!(w.current_seq(), 2);

        w.clear();
        assert_eq!(w.current_seq(), 0);
        assert!(w.since(0).is_empty());

        // New events start from seq 1 again
        let s = w.push(PresenceEvent::PhaseChanged {
            phase: "idle".to_string(),
        });
        assert_eq!(s, 1);
    }

    #[test]
    fn event_window_default_capacity() {
        let w = PresenceEventWindow::default();
        assert_eq!(w.capacity, 200);
        assert_eq!(w.current_seq(), 0);
    }

    // ── Video / frame type tests ──

    #[test]
    fn frame_meta_roundtrip() {
        let meta = FrameMeta {
            frame_id: "cam0-f00047".to_string(),
            stream: "cam0".to_string(),
            timestamp: "2026-03-21T10:15:32Z".to_string(),
            sent_to_live: true,
            live_resolution: Some("768x768".to_string()),
            hq_resolution: Some("1920x1080".to_string()),
        };
        let json = serde_json::to_string(&meta).unwrap();
        let back: FrameMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back.frame_id, "cam0-f00047");
        assert_eq!(back.stream, "cam0");
        assert!(back.sent_to_live);
        assert_eq!(back.live_resolution.as_deref(), Some("768x768"));
        assert_eq!(back.hq_resolution.as_deref(), Some("1920x1080"));
    }

    #[test]
    fn frame_meta_minimal() {
        let json = r#"{"frame_id":"d99-f00001","stream":"display:99","timestamp":"2026-03-21T10:00:00Z","sent_to_live":false}"#;
        let meta: FrameMeta = serde_json::from_str(json).unwrap();
        assert_eq!(meta.frame_id, "d99-f00001");
        assert!(!meta.sent_to_live);
        assert!(meta.live_resolution.is_none());
        assert!(meta.hq_resolution.is_none());
    }

    #[test]
    fn video_state_defaults() {
        let vs = VideoState::default();
        assert!(vs.active_streams.is_empty());
        assert!(vs.current_frame_id.is_none());
        assert_eq!(vs.total_frames, 0);
    }

    #[test]
    fn video_state_roundtrip() {
        let vs = VideoState {
            active_streams: vec!["cam0".to_string()],
            current_frame_id: Some("cam0-f00100".to_string()),
            total_frames: 100,
        };
        let json = serde_json::to_string(&vs).unwrap();
        let back: VideoState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.active_streams, vec!["cam0"]);
        assert_eq!(back.current_frame_id.as_deref(), Some("cam0-f00100"));
        assert_eq!(back.total_frames, 100);
    }
}
