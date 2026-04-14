//! `AppEvent` → `PeerEvent` upcaster.
//!
//! Translates Intendant's internal [`crate::event::AppEvent`] stream
//! into the transport-neutral [`PeerEvent`] vocabulary so the
//! federation layer can render a peer Intendant's activity uniformly
//! alongside non-Intendant peers (OpenClaw, Hermes, A2A). Used by the
//! `IntendantWsTransport` to map the full-fidelity wire stream a peer
//! Intendant emits into the lean `PeerEvent` shape consumers can
//! handle without knowing the source type.
//!
//! ## Why a struct, not a free function
//!
//! Streaming model output is the key driver. `AppEvent::ModelResponseDelta`
//! chunks don't carry a turn number or any other natural correlation
//! field, so a stateless mapping can't tell the receiver "these
//! deltas all belong to the same message." The upcaster tracks the
//! current turn's streaming message ID in its own state, then reuses
//! it across deltas and stamps the final `ModelResponse` with the
//! same ID — the receiving dashboard can aggregate cleanly. A
//! stateless `fn(AppEvent) -> Vec<PeerEvent>` would either drop the
//! ID problem on the consumer or force every transport to carry its
//! own sequencing state.
//!
//! ## Return shape
//!
//! `upcast` returns `Vec<PeerEvent>` because the 1:N fan-out is
//! genuine: `AppEvent::ModelResponse` naturally produces a Message
//! (completed) *and* a Usage snapshot (token accounting) *and* (when
//! reasoning is present) a second Message with the reasoning content.
//! Events that have no federation-relevant content (`Tick`, `Key`,
//! `Resize`, `ControlCommand`, high-frequency `DisplayMetrics`)
//! return an empty vec and are dropped.
//!
//! ## Policy notes
//!
//! - LogLevel mapping: Intendant's internal `LogLevel` has more
//!   variants than the peer vocabulary (Model, Agent, SubAgent,
//!   Detail). These are all source-specific and collapse to `Info`
//!   or `Debug` on the wire — the `source` field on the peer Log
//!   event carries the differentiation.
//! - ActionCategory → string: peer's `ApprovalRequest.category` is
//!   deliberately free-form because non-Intendant peers have
//!   different category vocabularies. Intendant's own categories
//!   serialize as lowercase snake_case names.
//! - DisplayMetrics: dropped entirely. It's a high-frequency metric
//!   stream, not an event, and the federation layer doesn't need
//!   per-peer display metrics visible in the aggregate feed.

use crate::event::AppEvent;
use crate::peer::{
    ActivityId, ActivityKind, ActivityOutcome, ApprovalDecision, ApprovalRequest, Capability,
    LogLevel, MessageContent, MessageId, MessageRole, ModelUsage, PeerEvent, PeerStatus,
    SessionInfo, UsageSnapshot,
};

/// Stateful `AppEvent` → `PeerEvent` upcaster.
pub struct AppEventUpcaster {
    /// Monotonic counter for synthesizing stable IDs when the source
    /// event doesn't carry a natural one.
    seq: u64,
    /// The current streaming-message ID. Seeded by `TurnStarted` so
    /// subsequent `ModelResponseDelta` chunks and the final
    /// `ModelResponse` within one turn all share it. Cleared by
    /// `ModelResponse` (end of the model's stream for this turn),
    /// `DoneSignal` (end of the whole turn), or a new `TurnStarted`.
    /// Deltas that arrive without a prior `TurnStarted` synthesize a
    /// fresh seq-based ID and store it here so follow-up deltas reuse it.
    current_message_id: Option<MessageId>,
}

impl Default for AppEventUpcaster {
    fn default() -> Self {
        Self::new()
    }
}

impl AppEventUpcaster {
    pub fn new() -> Self {
        Self {
            seq: 0,
            current_message_id: None,
        }
    }

    fn next_seq(&mut self) -> u64 {
        self.seq = self.seq.saturating_add(1);
        self.seq
    }

    fn now_rfc3339() -> String {
        chrono::Utc::now().to_rfc3339()
    }

    fn log(level: LogLevel, source: &str, message: String) -> PeerEvent {
        PeerEvent::Log {
            level,
            source: source.to_string(),
            message,
            ts: Self::now_rfc3339(),
        }
    }

    /// Return the current message ID, creating a fresh seq-based one
    /// (and storing it) if no stream is in flight. Used by both
    /// `ModelResponseDelta` and `ModelResponse` so they share state
    /// seamlessly — whichever arrives first seeds the ID, subsequent
    /// events reuse it.
    fn current_or_new_message_id(&mut self) -> MessageId {
        if let Some(id) = &self.current_message_id {
            return id.clone();
        }
        let seq = self.next_seq();
        let id = MessageId(format!("msg-seq-{seq}"));
        self.current_message_id = Some(id.clone());
        id
    }

    /// Map an `AppEvent` to zero or more `PeerEvent`s.
    pub fn upcast(&mut self, event: &AppEvent) -> Vec<PeerEvent> {
        match event {
            // ---- Dropped internal events ----
            AppEvent::Key(_)
            | AppEvent::Resize(_, _)
            | AppEvent::Tick
            | AppEvent::ControlCommand(_)
            | AppEvent::DisplayMetrics { .. } => vec![],

            // ---- Turn lifecycle ----
            AppEvent::TurnStarted { turn, .. } => {
                // Seed the shared message ID for this turn so subsequent
                // deltas and the final ModelResponse all line up on it.
                self.current_message_id = Some(MessageId(format!("msg-turn-{turn}")));
                vec![PeerEvent::ActivityStarted {
                    id: ActivityId(format!("turn-{turn}")),
                    kind: ActivityKind::ModelTurn,
                    label: format!("turn {turn}"),
                }]
            }

            AppEvent::ModelResponseDelta { text } => {
                let id = self.current_or_new_message_id();
                vec![PeerEvent::Message {
                    id,
                    role: MessageRole::Assistant,
                    content: MessageContent::Text { text: text.clone() },
                    partial: true,
                }]
            }

            AppEvent::ModelResponse {
                turn,
                content,
                usage,
                reasoning,
                source: _,
            } => {
                let msg_id = self.current_or_new_message_id();
                let mut out = vec![PeerEvent::Message {
                    id: msg_id,
                    role: MessageRole::Assistant,
                    content: MessageContent::Text {
                        text: content.clone(),
                    },
                    partial: false,
                }];
                if let Some(reasoning_text) = reasoning {
                    out.push(PeerEvent::Message {
                        id: MessageId(format!("reasoning-turn-{turn}")),
                        role: MessageRole::Assistant,
                        content: MessageContent::Reasoning {
                            text: reasoning_text.clone(),
                        },
                        partial: false,
                    });
                }
                out.push(PeerEvent::Usage {
                    snapshot: UsageSnapshot {
                        tokens_in: usage.prompt_tokens,
                        tokens_out: usage.completion_tokens,
                        tokens_cached: usage.cached_tokens,
                        cost_usd: None,
                        by_model: vec![],
                    },
                });
                // End of the model's stream for this turn — clear so any
                // subsequent deltas (shouldn't happen, but be safe) start
                // a fresh message rather than silently reusing this ID.
                self.current_message_id = None;
                out
            }

            AppEvent::DoneSignal { message } => {
                // We don't know the turn number here (DoneSignal doesn't
                // carry one) so synthesize an activity id from the
                // sequence counter. The Activity model accepts this —
                // the id only needs to be stable across
                // started/progress/completed for the *same* activity,
                // and DoneSignal marks the end of a turn whose
                // ActivityStarted used a turn-based id that we can't
                // recover here. This is a documented looseness of the
                // AppEvent → PeerEvent mapping.
                self.current_message_id = None;
                let seq = self.next_seq();
                let mut out = vec![PeerEvent::ActivityCompleted {
                    id: ActivityId(format!("done-{seq}")),
                    outcome: ActivityOutcome::Success,
                }];
                if let Some(msg) = message {
                    out.push(Self::log(LogLevel::Info, "agent", format!("done: {msg}")));
                }
                out
            }

            AppEvent::RoundComplete {
                round,
                turns_in_round,
            } => vec![Self::log(
                LogLevel::Info,
                "agent",
                format!("round {round} complete ({turns_in_round} turns)"),
            )],

            // ---- Sub-agent / tool execution ----
            AppEvent::AgentStarted {
                turn,
                commands_preview,
                source,
            } => {
                let label = source.clone().unwrap_or_else(|| "agent".to_string());
                vec![PeerEvent::ActivityStarted {
                    id: ActivityId(format!("agent-{turn}")),
                    kind: ActivityKind::ToolCall,
                    label: format!("{label}: {commands_preview}"),
                }]
            }

            AppEvent::AgentOutput {
                stdout,
                stderr,
                source: _,
            } => {
                let mut out = vec![];
                if !stdout.is_empty() {
                    out.push(PeerEvent::ActivityProgress {
                        id: ActivityId("agent-latest".into()),
                        text: Some(stdout.clone()),
                    });
                }
                if !stderr.is_empty() {
                    out.push(Self::log(LogLevel::Warn, "agent", stderr.clone()));
                }
                out
            }

            AppEvent::SubAgentResult { formatted } => {
                let seq = self.next_seq();
                vec![
                    PeerEvent::ActivityCompleted {
                        id: ActivityId(format!("subagent-{seq}")),
                        outcome: ActivityOutcome::Success,
                    },
                    Self::log(LogLevel::Info, "subagent", formatted.clone()),
                ]
            }

            AppEvent::OrchestratorProgress {
                turn,
                status,
                last_action,
            } => vec![PeerEvent::ActivityProgress {
                id: ActivityId(format!("orchestrator-{turn}")),
                text: Some(format!("{status}: {last_action}")),
            }],

            AppEvent::OrchestratorLog { message, level } => vec![Self::log(
                upcast_log_level(level),
                "orchestrator",
                message.clone(),
            )],

            AppEvent::ContextManagement { turn } => vec![Self::log(
                LogLevel::Debug,
                "context",
                format!("context management turn {turn}"),
            )],

            AppEvent::TaskComplete { reason, summary } => {
                let seq = self.next_seq();
                let outcome = match reason.as_str() {
                    "success" | "done" | "completed" => ActivityOutcome::Success,
                    "cancelled" | "canceled" => ActivityOutcome::Cancelled,
                    other => ActivityOutcome::Failed {
                        message: other.to_string(),
                    },
                };
                let mut out = vec![PeerEvent::ActivityCompleted {
                    id: ActivityId(format!("task-{seq}")),
                    outcome,
                }];
                if let Some(s) = summary {
                    out.push(Self::log(LogLevel::Info, "task", s.clone()));
                }
                out
            }

            // ---- Session lifecycle ----
            AppEvent::SessionStarted { session_id, task } => {
                vec![PeerEvent::SessionStarted {
                    session: SessionInfo {
                        session_id: session_id.clone(),
                        label: task.clone(),
                        started_at: Self::now_rfc3339(),
                    },
                }]
            }

            AppEvent::SessionEnded { session_id, reason } => vec![PeerEvent::SessionEnded {
                session_id: session_id.clone(),
                reason: reason.clone(),
            }],

            AppEvent::SessionDirChanged { path } => vec![Self::log(
                LogLevel::Info,
                "session",
                format!("session dir → {}", path.display()),
            )],

            // ---- Approval flow ----
            AppEvent::ApprovalRequired {
                id,
                command_preview,
                category,
            } => vec![PeerEvent::ApprovalRequested {
                request: ApprovalRequest {
                    request_id: id.to_string(),
                    category: action_category_wire(category),
                    preview: command_preview.clone(),
                    auto_resolvable: false,
                },
            }],

            AppEvent::ApprovalResolved { id, action } => vec![PeerEvent::ApprovalResolved {
                request_id: id.to_string(),
                decision: approval_decision_from_action(action),
            }],

            AppEvent::AutoApproved { preview } => {
                let seq = self.next_seq();
                vec![
                    PeerEvent::ApprovalRequested {
                        request: ApprovalRequest {
                            request_id: format!("auto-{seq}"),
                            category: "auto".to_string(),
                            preview: preview.clone(),
                            auto_resolvable: true,
                        },
                    },
                    PeerEvent::ApprovalResolved {
                        request_id: format!("auto-{seq}"),
                        decision: ApprovalDecision::Accept,
                    },
                ]
            }

            AppEvent::HumanQuestionDetected { question } => {
                let seq = self.next_seq();
                vec![PeerEvent::ApprovalRequested {
                    request: ApprovalRequest {
                        request_id: format!("human-{seq}"),
                        category: "human_question".to_string(),
                        preview: question.clone(),
                        auto_resolvable: false,
                    },
                }]
            }

            AppEvent::HumanResponseSent => vec![Self::log(
                LogLevel::Info,
                "human",
                "human response sent".to_string(),
            )],

            // ---- Display capability ----
            AppEvent::DisplayReady {
                display_id,
                width,
                height,
            } => vec![PeerEvent::CapabilityEngaged {
                capability: Capability::Display,
                detail: serde_json::json!({
                    "display_id": display_id,
                    "width": width,
                    "height": height,
                }),
            }],

            AppEvent::DisplayResize {
                display_id,
                width,
                height,
            } => vec![Self::log(
                LogLevel::Info,
                "display",
                format!("display {display_id} resized to {width}x{height}"),
            )],

            AppEvent::DisplayTaken { display_id } => vec![PeerEvent::CapabilityEngaged {
                capability: Capability::Display,
                detail: serde_json::json!({ "display_id": display_id, "state": "taken" }),
            }],

            AppEvent::DisplayReleased { display_id: _, note } => vec![PeerEvent::CapabilityReleased {
                capability: Capability::Display,
                reason: note.clone(),
            }],

            AppEvent::DisplayCaptureLost {
                display_id: _,
                reason,
            } => vec![PeerEvent::CapabilityReleased {
                capability: Capability::Display,
                reason: Some(format!("capture_lost: {reason}")),
            }],

            AppEvent::DisplayApprovalPending {
                display_id: _,
                backend,
            } => vec![Self::log(
                LogLevel::Info,
                "display",
                format!("display approval pending on {backend}"),
            )],

            AppEvent::UserDisplayGranted { display_id } => vec![Self::log(
                LogLevel::Info,
                "display",
                format!("user granted display {display_id}"),
            )],

            AppEvent::UserDisplayRevoked { display_id, note } => {
                let note_str = note.as_deref().unwrap_or("");
                vec![Self::log(
                    LogLevel::Info,
                    "display",
                    format!("user revoked display {display_id}: {note_str}"),
                )]
            }

            AppEvent::DebugScreenReady { display_id } => vec![PeerEvent::CapabilityEngaged {
                capability: Capability::Display,
                detail: serde_json::json!({
                    "display_id": display_id,
                    "kind": "debug_screen",
                }),
            }],

            AppEvent::DebugScreenTornDown { display_id: _ } => {
                vec![PeerEvent::CapabilityReleased {
                    capability: Capability::Display,
                    reason: Some("debug_screen_torn_down".to_string()),
                }]
            }

            // ---- Recording capability ----
            AppEvent::RecordingStarted { stream_name } => vec![PeerEvent::CapabilityEngaged {
                capability: Capability::Recording,
                detail: serde_json::json!({ "stream": stream_name }),
            }],

            AppEvent::RecordingStopped { stream_name } => vec![PeerEvent::CapabilityReleased {
                capability: Capability::Recording,
                reason: Some(format!("stopped: {stream_name}")),
            }],

            AppEvent::RecordingError {
                stream_name,
                message,
            } => vec![Self::log(
                LogLevel::Error,
                "recording",
                format!("{stream_name}: {message}"),
            )],

            AppEvent::RecordingDeleted { stream_name } => vec![Self::log(
                LogLevel::Info,
                "recording",
                format!("{stream_name} deleted"),
            )],

            // ---- Presence / voice ----
            AppEvent::PresenceConnected { .. } => vec![PeerEvent::CapabilityEngaged {
                capability: Capability::Voice,
                detail: serde_json::json!({ "kind": "presence" }),
            }],

            AppEvent::PresenceDisconnected => vec![PeerEvent::CapabilityReleased {
                capability: Capability::Voice,
                reason: Some("presence_disconnected".to_string()),
            }],

            AppEvent::PresenceReady => vec![Self::log(
                LogLevel::Info,
                "presence",
                "presence ready".to_string(),
            )],

            AppEvent::PresenceLog {
                message,
                level,
                turn: _,
            } => {
                let lvl = level
                    .as_ref()
                    .map(upcast_log_level)
                    .unwrap_or(LogLevel::Info);
                vec![Self::log(lvl, "presence", message.clone())]
            }

            AppEvent::PresenceCheckpointReceived {
                summary,
                last_event_seq,
            } => vec![Self::log(
                LogLevel::Info,
                "presence",
                format!("checkpoint at seq {last_event_seq}: {summary}"),
            )],

            AppEvent::VoiceLog {
                text,
                seq: _,
                tool_context: _,
            } => vec![Self::log(LogLevel::Info, "voice", text.clone())],

            AppEvent::VoiceDiagnostic { kind, detail } => vec![Self::log(
                LogLevel::Warn,
                "voice",
                format!("{kind}: {detail}"),
            )],

            AppEvent::UserTranscript { text, seq: _ } => {
                let seq = self.next_seq();
                vec![PeerEvent::Message {
                    id: MessageId(format!("user-transcript-{seq}")),
                    role: MessageRole::User,
                    content: MessageContent::Text { text: text.clone() },
                    partial: false,
                }]
            }

            AppEvent::LiveAudioStarted { id, provider } => vec![PeerEvent::ActivityStarted {
                id: ActivityId(format!("live-audio-{id}")),
                kind: ActivityKind::Other,
                label: format!("live audio ({provider})"),
            }],

            AppEvent::LiveAudioProgress {
                id,
                state,
                elapsed_secs: _,
                transcript_preview,
            } => vec![PeerEvent::ActivityProgress {
                id: ActivityId(format!("live-audio-{id}")),
                text: Some(format!("{state}: {transcript_preview}")),
            }],

            AppEvent::LiveAudioCompleted {
                id,
                status,
                quarantine_count,
            } => {
                let outcome = if status == "ok" || status == "success" {
                    ActivityOutcome::Success
                } else {
                    ActivityOutcome::Failed {
                        message: format!("{status} (quarantined: {quarantine_count})"),
                    }
                };
                vec![PeerEvent::ActivityCompleted {
                    id: ActivityId(format!("live-audio-{id}")),
                    outcome,
                }]
            }

            // ---- Usage accounting ----
            AppEvent::PresenceUsageUpdate {
                total_tokens: _,
                context_window: _,
                usage_pct: _,
                provider: _,
                model: _,
                prompt_tokens,
                completion_tokens,
                cached_tokens,
            } => vec![PeerEvent::Usage {
                snapshot: UsageSnapshot {
                    tokens_in: *prompt_tokens,
                    tokens_out: *completion_tokens,
                    tokens_cached: *cached_tokens,
                    cost_usd: None,
                    by_model: vec![],
                },
            }],

            AppEvent::LiveUsageUpdate {
                provider,
                model,
                input_tokens,
                output_tokens,
                cached_tokens,
                total_tokens: _,
                thinking_tokens: _,
            } => vec![PeerEvent::Usage {
                snapshot: UsageSnapshot {
                    tokens_in: *input_tokens,
                    tokens_out: *output_tokens,
                    tokens_cached: *cached_tokens,
                    cost_usd: None,
                    by_model: vec![ModelUsage {
                        provider: provider.clone(),
                        model: model.clone(),
                        tokens_in: *input_tokens,
                        tokens_out: *output_tokens,
                        cost_usd: None,
                    }],
                },
            }],

            AppEvent::UsageSnapshot { main, presence: _ } => vec![PeerEvent::Usage {
                snapshot: UsageSnapshot {
                    tokens_in: main.prompt_tokens,
                    tokens_out: main.completion_tokens,
                    tokens_cached: main.cached_tokens,
                    cost_usd: None,
                    by_model: vec![ModelUsage {
                        provider: main.provider.clone(),
                        model: main.model.clone(),
                        tokens_in: main.prompt_tokens,
                        tokens_out: main.completion_tokens,
                        cost_usd: None,
                    }],
                },
            }],

            // ---- Status ----
            AppEvent::StatusUpdate { phase, .. } => {
                let status = status_from_phase(phase);
                vec![PeerEvent::StatusChanged { status }]
            }

            AppEvent::ExternalAgentChanged { agent } => vec![Self::log(
                LogLevel::Info,
                "config",
                format!(
                    "external agent changed → {}",
                    agent.as_deref().unwrap_or("none")
                ),
            )],

            // ---- Budget / safety ----
            AppEvent::BudgetWarning { pct, remaining } => vec![Self::log(
                LogLevel::Warn,
                "budget",
                format!("budget warning: {pct:.1}% remaining={remaining}"),
            )],

            AppEvent::BudgetExhausted { remaining } => vec![Self::log(
                LogLevel::Warn,
                "budget",
                format!("budget exhausted, remaining={remaining}"),
            )],

            AppEvent::SafetyCapReached => vec![Self::log(
                LogLevel::Warn,
                "safety",
                "safety cap reached".to_string(),
            )],

            AppEvent::LoopError(msg) => vec![Self::log(LogLevel::Error, "agent", msg.clone())],

            AppEvent::JsonExtracted { preview } => vec![Self::log(
                LogLevel::Debug,
                "agent",
                format!("json: {preview}"),
            )],

            // ---- Log passthrough ----
            AppEvent::LogEntry {
                level,
                source,
                content,
                turn: _,
            } => {
                let log_level = match level.as_str() {
                    "trace" => LogLevel::Trace,
                    "debug" | "detail" => LogLevel::Debug,
                    "info" | "model" | "agent" | "subagent" => LogLevel::Info,
                    "warn" | "warning" => LogLevel::Warn,
                    "error" => LogLevel::Error,
                    _ => LogLevel::Info,
                };
                vec![Self::log(log_level, source, content.clone())]
            }

            // ---- Terminal ----
            AppEvent::Quit => vec![PeerEvent::Disconnected {
                reason: "quit".to_string(),
            }],
        }
    }
}

/// Map Intendant's internal multi-source `LogLevel` to the peer
/// module's 5-level vocabulary. Source-specific variants
/// (Model/Agent/SubAgent) collapse to `Info` because the peer Log
/// event has a separate `source` field that carries the differentiation.
fn upcast_log_level(level: &crate::types::LogLevel) -> LogLevel {
    use crate::types::LogLevel as L;
    match level {
        L::Debug => LogLevel::Debug,
        L::Detail => LogLevel::Debug,
        L::Info | L::Model | L::Agent | L::SubAgent => LogLevel::Info,
        L::Warn => LogLevel::Warn,
        L::Error => LogLevel::Error,
    }
}

/// Map Intendant's internal `ActionCategory` to a free-form string
/// for `ApprovalRequest.category`. Lowercase snake_case to match the
/// convention other autonomous daemons (OpenClaw) use for category tags.
fn action_category_wire(cat: &crate::autonomy::ActionCategory) -> String {
    use crate::autonomy::ActionCategory as C;
    match cat {
        C::FileRead => "file_read",
        C::FileWrite => "file_write",
        C::FileDelete => "file_delete",
        C::CommandExec => "command_exec",
        C::NetworkRequest => "network_request",
        C::Destructive => "destructive",
        C::HumanInput => "human_input",
        C::LiveAudioSpawn => "live_audio_spawn",
        C::DisplayControl => "display_control",
    }
    .to_string()
}

/// Map the action string on `ApprovalResolved` to a typed
/// `ApprovalDecision`. The string names come from `ApprovalResponse`
/// variant names and the TUI's action labels (approve / deny / skip /
/// approve_all) which the event loop emits without normalization.
fn approval_decision_from_action(action: &str) -> ApprovalDecision {
    match action {
        "approve" | "accept" => ApprovalDecision::Accept,
        "approve_all" | "accept_for_session" | "approveall" => {
            ApprovalDecision::AcceptForSession
        }
        "deny" | "decline" => ApprovalDecision::Decline,
        "skip" | "cancel" => ApprovalDecision::Cancel,
        _ => ApprovalDecision::Decline,
    }
}

/// Map the free-form `StatusUpdate.phase` string to a typed `PeerStatus`.
/// The phase vocabulary isn't formally documented anywhere — values in
/// the wild include `idle`, `thinking`, `acting`, `executing`,
/// `waiting_approval`, `waiting_followup`, `done`, `failed`. Unknown
/// phases default to `Idle` rather than `Unknown` because `Idle` is
/// the more graceful render when we're not sure — the peer is
/// connected, we just don't recognize its phase label.
fn status_from_phase(phase: &str) -> PeerStatus {
    match phase {
        "idle" | "waiting_followup" | "done" => PeerStatus::Idle,
        "working" | "thinking" | "acting" | "executing" | "running" => PeerStatus::Working,
        "approval" | "waiting_approval" | "needs_approval" => PeerStatus::NeedsApproval,
        "error" | "failed" => PeerStatus::Error,
        _ => PeerStatus::Idle,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::TokenUsage;

    fn token_usage(prompt: u64, completion: u64) -> TokenUsage {
        TokenUsage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: prompt + completion,
            cached_tokens: 0,
        }
    }

    /// Turn start emits a single `ActivityStarted` with kind ModelTurn
    /// and a deterministic id derived from the turn number.
    #[test]
    fn turn_started_emits_activity_started() {
        let mut u = AppEventUpcaster::new();
        let out = u.upcast(&AppEvent::TurnStarted {
            turn: 3,
            budget_pct: 0.5,
            remaining: 1000,
        });
        assert_eq!(out.len(), 1);
        match &out[0] {
            PeerEvent::ActivityStarted { id, kind, .. } => {
                assert_eq!(id.0, "turn-3");
                assert_eq!(*kind, ActivityKind::ModelTurn);
            }
            _ => panic!("expected ActivityStarted, got {:?}", out[0]),
        }
    }

    /// Streaming deltas share a message ID across calls until a
    /// `ModelResponse` closes the turn. This is the core state
    /// mechanic of the upcaster.
    #[test]
    fn streaming_deltas_share_message_id_within_turn() {
        let mut u = AppEventUpcaster::new();
        // Open turn 5.
        let _ = u.upcast(&AppEvent::TurnStarted {
            turn: 5,
            budget_pct: 0.5,
            remaining: 100,
        });
        // Prime the current-turn message ID by emitting a ModelResponse
        // first — wait, that clears state. Instead, deltas without a
        // prior ModelResponse synthesize a fresh ID that's stable
        // across subsequent deltas in the same conversation-turn.
        let a = u.upcast(&AppEvent::ModelResponseDelta {
            text: "Hello ".into(),
        });
        let b = u.upcast(&AppEvent::ModelResponseDelta {
            text: "world".into(),
        });
        let id_a = match &a[0] {
            PeerEvent::Message { id, partial, .. } => {
                assert!(*partial, "delta should be partial");
                id.clone()
            }
            _ => panic!("expected Message"),
        };
        let id_b = match &b[0] {
            PeerEvent::Message { id, partial, .. } => {
                assert!(*partial);
                id.clone()
            }
            _ => panic!("expected Message"),
        };
        assert_eq!(id_a, id_b, "deltas within same turn must share id");
    }

    /// `ModelResponse` emits Message(final) + Usage and, when a
    /// streaming id was tracked, reuses it for the final message.
    #[test]
    fn model_response_final_shares_id_with_deltas() {
        let mut u = AppEventUpcaster::new();
        // Prime current_turn_message via a delta-first path.
        let _ = u.upcast(&AppEvent::TurnStarted {
            turn: 7,
            budget_pct: 0.5,
            remaining: 100,
        });
        let delta = u.upcast(&AppEvent::ModelResponseDelta {
            text: "Hello ".into(),
        });
        let delta_id = match &delta[0] {
            PeerEvent::Message { id, .. } => id.clone(),
            _ => panic!(),
        };
        // Now close the turn.
        let out = u.upcast(&AppEvent::ModelResponse {
            turn: 7,
            content: "Hello world".into(),
            usage: token_usage(10, 20),
            reasoning: None,
            source: None,
        });
        // Expect: Message(final) + Usage.
        assert_eq!(out.len(), 2);
        match &out[0] {
            PeerEvent::Message { id, partial, .. } => {
                assert!(!partial);
                // Current implementation: final uses message_id_for_turn
                // which looks up by turn #, and the delta used the same
                // turn's streaming id, so they should match.
                assert_eq!(
                    *id, delta_id,
                    "final message should reuse the streaming delta id"
                );
            }
            _ => panic!("expected Message, got {:?}", out[0]),
        }
        assert!(matches!(out[1], PeerEvent::Usage { .. }));
    }

    /// `ModelResponse` with reasoning emits Message + Reasoning + Usage.
    #[test]
    fn model_response_with_reasoning_adds_reasoning_message() {
        let mut u = AppEventUpcaster::new();
        let out = u.upcast(&AppEvent::ModelResponse {
            turn: 1,
            content: "final".into(),
            usage: token_usage(5, 5),
            reasoning: Some("thinking...".into()),
            source: None,
        });
        assert_eq!(out.len(), 3);
        assert!(matches!(
            &out[1],
            PeerEvent::Message {
                content: MessageContent::Reasoning { .. },
                ..
            }
        ));
    }

    /// Internal TUI events get dropped entirely — no noise on the
    /// peer event stream.
    #[test]
    fn internal_events_are_dropped() {
        let mut u = AppEventUpcaster::new();
        assert!(u.upcast(&AppEvent::Tick).is_empty());
        assert!(u.upcast(&AppEvent::Resize(80, 24)).is_empty());
        // ControlCommand carries arbitrary ControlMsg variants; Status is
        // a trivially-constructable one.
        assert!(u
            .upcast(&AppEvent::ControlCommand(
                crate::event::ControlMsg::Status
            ))
            .is_empty());
    }

    /// `DisplayReady` engages the Display capability with detail
    /// carrying width/height, and `DisplayReleased` releases it.
    #[test]
    fn display_capability_lifecycle() {
        let mut u = AppEventUpcaster::new();
        let engaged = u.upcast(&AppEvent::DisplayReady {
            display_id: 1,
            width: 1920,
            height: 1080,
        });
        assert_eq!(engaged.len(), 1);
        match &engaged[0] {
            PeerEvent::CapabilityEngaged {
                capability,
                detail,
            } => {
                assert_eq!(*capability, Capability::Display);
                assert_eq!(detail["width"], 1920);
                assert_eq!(detail["height"], 1080);
            }
            _ => panic!("expected CapabilityEngaged"),
        }
        let released = u.upcast(&AppEvent::DisplayReleased {
            display_id: 1,
            note: Some("user revoked".into()),
        });
        match &released[0] {
            PeerEvent::CapabilityReleased {
                capability,
                reason,
            } => {
                assert_eq!(*capability, Capability::Display);
                assert_eq!(reason.as_deref(), Some("user revoked"));
            }
            _ => panic!("expected CapabilityReleased"),
        }
    }

    /// Recording lifecycle engages/releases the Recording capability.
    #[test]
    fn recording_capability_lifecycle() {
        let mut u = AppEventUpcaster::new();
        let started = u.upcast(&AppEvent::RecordingStarted {
            stream_name: "display-1".into(),
        });
        assert!(matches!(
            &started[0],
            PeerEvent::CapabilityEngaged {
                capability: Capability::Recording,
                ..
            }
        ));
        let stopped = u.upcast(&AppEvent::RecordingStopped {
            stream_name: "display-1".into(),
        });
        assert!(matches!(
            &stopped[0],
            PeerEvent::CapabilityReleased {
                capability: Capability::Recording,
                ..
            }
        ));
    }

    /// Presence connect/disconnect engages/releases the Voice capability.
    #[test]
    fn presence_voice_capability_lifecycle() {
        let mut u = AppEventUpcaster::new();
        let connected = u.upcast(&AppEvent::PresenceConnected {
            server_session_id: None,
            last_event_seq: 0,
            live_provider: Some("gemini".into()),
            live_model: Some("gemini-2.5-flash".into()),
        });
        assert!(matches!(
            &connected[0],
            PeerEvent::CapabilityEngaged {
                capability: Capability::Voice,
                ..
            }
        ));
        let disconnected = u.upcast(&AppEvent::PresenceDisconnected);
        assert!(matches!(
            &disconnected[0],
            PeerEvent::CapabilityReleased {
                capability: Capability::Voice,
                ..
            }
        ));
    }

    /// Approval required/resolved flow maps to ApprovalRequested/Resolved.
    #[test]
    fn approval_flow_maps_cleanly() {
        let mut u = AppEventUpcaster::new();
        let req = u.upcast(&AppEvent::ApprovalRequired {
            id: 42,
            command_preview: "rm -rf /tmp/foo".into(),
            category: crate::autonomy::ActionCategory::FileDelete,
        });
        match &req[0] {
            PeerEvent::ApprovalRequested { request } => {
                assert_eq!(request.request_id, "42");
                assert_eq!(request.category, "file_delete");
                assert!(!request.auto_resolvable);
            }
            _ => panic!("expected ApprovalRequested"),
        }
        let res = u.upcast(&AppEvent::ApprovalResolved {
            id: 42,
            action: "approve".into(),
        });
        match &res[0] {
            PeerEvent::ApprovalResolved {
                request_id,
                decision,
            } => {
                assert_eq!(request_id, "42");
                assert_eq!(*decision, ApprovalDecision::Accept);
            }
            _ => panic!("expected ApprovalResolved"),
        }
    }

    /// Status update with a known phase maps to the corresponding
    /// `PeerStatus` variant. Unknown phases collapse to `Idle`.
    #[test]
    fn status_update_phase_mapping() {
        let mut u = AppEventUpcaster::new();
        let cases: &[(&str, PeerStatus)] = &[
            ("idle", PeerStatus::Idle),
            ("thinking", PeerStatus::Working),
            ("waiting_approval", PeerStatus::NeedsApproval),
            ("failed", PeerStatus::Error),
            ("holographic", PeerStatus::Idle), // unknown → Idle
        ];
        for (phase, expected) in cases {
            let out = u.upcast(&AppEvent::StatusUpdate {
                turn: 0,
                phase: phase.to_string(),
                autonomy: "medium".into(),
                session_id: "s".into(),
                task: "t".into(),
            });
            match &out[0] {
                PeerEvent::StatusChanged { status } => assert_eq!(
                    status, expected,
                    "phase={phase}: expected {expected:?}, got {status:?}"
                ),
                _ => panic!("expected StatusChanged"),
            }
        }
    }

    /// Session start/end maps to SessionStarted/SessionEnded with
    /// the id preserved.
    #[test]
    fn session_lifecycle() {
        let mut u = AppEventUpcaster::new();
        let start = u.upcast(&AppEvent::SessionStarted {
            session_id: "sess-1".into(),
            task: Some("research".into()),
        });
        match &start[0] {
            PeerEvent::SessionStarted { session } => {
                assert_eq!(session.session_id, "sess-1");
                assert_eq!(session.label.as_deref(), Some("research"));
            }
            _ => panic!("expected SessionStarted"),
        }
        let end = u.upcast(&AppEvent::SessionEnded {
            session_id: "sess-1".into(),
            reason: "done".into(),
        });
        match &end[0] {
            PeerEvent::SessionEnded {
                session_id, reason,
            } => {
                assert_eq!(session_id, "sess-1");
                assert_eq!(reason, "done");
            }
            _ => panic!("expected SessionEnded"),
        }
    }

    /// `LogEntry` passes through with level/source/message preserved.
    #[test]
    fn log_entry_passthrough() {
        let mut u = AppEventUpcaster::new();
        let out = u.upcast(&AppEvent::LogEntry {
            level: "warn".into(),
            source: "presence".into(),
            content: "something funny".into(),
            turn: Some(3),
        });
        match &out[0] {
            PeerEvent::Log {
                level,
                source,
                message,
                ..
            } => {
                assert_eq!(*level, LogLevel::Warn);
                assert_eq!(source, "presence");
                assert_eq!(message, "something funny");
            }
            _ => panic!("expected Log"),
        }
    }
}
