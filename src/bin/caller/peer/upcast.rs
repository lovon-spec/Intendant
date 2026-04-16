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
use crate::types::OutboundEvent;

// ---------------------------------------------------------------------------
// Shared stateless helpers
// ---------------------------------------------------------------------------
//
// Both upcasters consume the same peer vocabulary, so the small
// translation primitives (log level mapping, approval decision
// parsing, status phase mapping, etc.) live at module scope as
// `pub(crate)` functions. Factoring them out is the main defense
// against drift: if one upcaster starts interpreting "warn" or
// "waiting_approval" differently from the other, a parity test
// fires and points at the exact helper that needs fixing.

pub(crate) fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

pub(crate) fn log_event(level: LogLevel, source: &str, message: String) -> PeerEvent {
    PeerEvent::Log {
        level,
        source: source.to_string(),
        message,
        ts: now_rfc3339(),
    }
}

/// Map Intendant's internal multi-source `LogLevel` to the peer
/// module's 5-level vocabulary. Source-specific variants
/// (Model/Agent/SubAgent) collapse to `Info` because the peer Log
/// event has a separate `source` field that carries the
/// differentiation.
pub(crate) fn upcast_log_level(level: &crate::types::LogLevel) -> LogLevel {
    use crate::types::LogLevel as L;
    match level {
        L::Debug => LogLevel::Debug,
        L::Detail => LogLevel::Debug,
        L::Info | L::Model | L::Agent | L::SubAgent => LogLevel::Info,
        L::Warn => LogLevel::Warn,
        L::Error => LogLevel::Error,
    }
}

/// Map a wire-format log level string (as produced by
/// `OutboundEvent::LogEntry` and `OutboundEvent::PresenceLog`) to
/// the peer vocabulary. Same mapping table as `upcast_log_level`
/// but keyed on strings instead of the typed enum. Kept aligned
/// with `upcast_log_level` by the parity tests.
pub(crate) fn wire_log_level(s: &str) -> LogLevel {
    match s {
        "trace" => LogLevel::Trace,
        "debug" | "detail" => LogLevel::Debug,
        "info" | "model" | "agent" | "subagent" => LogLevel::Info,
        "warn" | "warning" => LogLevel::Warn,
        "error" => LogLevel::Error,
        _ => LogLevel::Info,
    }
}

/// Map Intendant's internal `ActionCategory` to a free-form string
/// for `ApprovalRequest.category`. Lowercase snake_case to match
/// the convention other autonomous daemons (OpenClaw) use for
/// category tags.
pub(crate) fn action_category_wire(cat: &crate::autonomy::ActionCategory) -> String {
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

/// Map the action string on `ApprovalResolved` (which is free-form
/// from the TUI's action labels or the `ApprovalResponse` variant
/// names) to a typed `ApprovalDecision`.
pub(crate) fn approval_decision_from_action(action: &str) -> ApprovalDecision {
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

/// Map the free-form `StatusUpdate.phase` / `OutboundEvent::Status.phase`
/// string to a typed `PeerStatus`. Unknown phases default to `Idle`
/// rather than `Unknown` because `Idle` is the more graceful render
/// when we're connected but don't recognize the phase label — the
/// peer is *there*, we just don't know what it's doing.
pub(crate) fn status_from_phase(phase: &str) -> PeerStatus {
    match phase {
        "idle" | "waiting_followup" | "done" => PeerStatus::Idle,
        "working" | "thinking" | "acting" | "executing" | "running" => PeerStatus::Working,
        "approval" | "waiting_approval" | "needs_approval" => PeerStatus::NeedsApproval,
        "error" | "failed" => PeerStatus::Error,
        _ => PeerStatus::Idle,
    }
}

// ---------------------------------------------------------------------------
// AppEventUpcaster — in-process AppEvent → PeerEvent
// ---------------------------------------------------------------------------

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
    /// Tracks the turn number of the currently-in-flight model turn
    /// activity. Set by `TurnStarted`; consumed by `DoneSignal` /
    /// `TaskComplete` / the next `TurnStarted` to emit a matching
    /// `ActivityCompleted` with the same id the Started event used.
    /// Without this, activities start as `turn-{turn}` but complete
    /// as `done-{seq}` — observers can't correlate start and end.
    current_turn: Option<usize>,
    /// Same tracking for in-flight agent (tool call) activities.
    /// `AgentStarted` carries the turn number; `AgentOutput` and
    /// the implicit completion (next `AgentStarted`, `DoneSignal`,
    /// or `TaskComplete`) reuse it so the progress/complete events
    /// match the started one. Without this, agents start as
    /// `agent-{turn}` but progress as `agent-latest`.
    current_agent_turn: Option<usize>,
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
            current_turn: None,
            current_agent_turn: None,
        }
    }

    /// Drain any in-flight agent activity before starting a new one
    /// or closing out a turn. Shared helper because the same cleanup
    /// logic runs from `AgentStarted` (close previous agent before
    /// this one begins), `DoneSignal`, `TaskComplete`, and `TurnStarted`
    /// (defensive, in case the agent wasn't explicitly closed).
    ///
    /// `outcome` is the outcome to stamp on the emitted
    /// `ActivityCompleted`. Success for the normal signals
    /// (DoneSignal, defensive closes from TurnStarted/AgentStarted)
    /// since in those cases we have no reason to believe the agent
    /// failed. TaskComplete propagates its own outcome — a failed
    /// task means the in-flight agent is failing too, so we don't
    /// want to stamp it Success alongside a failed turn.
    fn close_pending_agent(&mut self, outcome: ActivityOutcome) -> Option<PeerEvent> {
        self.current_agent_turn.take().map(|turn| {
            PeerEvent::ActivityCompleted {
                id: ActivityId(format!("agent-{turn}")),
                outcome,
            }
        })
    }

    /// Drain any in-flight turn activity. Called from `DoneSignal`,
    /// `TaskComplete`, and the next `TurnStarted` (defensive).
    fn close_pending_turn(&mut self, outcome: ActivityOutcome) -> Option<PeerEvent> {
        self.current_turn.take().map(|turn| {
            PeerEvent::ActivityCompleted {
                id: ActivityId(format!("turn-{turn}")),
                outcome,
            }
        })
    }

    fn next_seq(&mut self) -> u64 {
        self.seq = self.seq.saturating_add(1);
        self.seq
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
            | AppEvent::DisplayMetrics { .. }
            | AppEvent::FileChanged { .. } => vec![],

            // ---- Turn lifecycle ----
            AppEvent::TurnStarted { turn, .. } => {
                // Defensive cleanup: if a previous turn/agent never
                // closed explicitly (because the source dropped
                // DoneSignal, or emitted TurnStarted without a prior
                // closer), close them here so observers see a
                // consistent start/complete pairing instead of
                // orphaned Started events.
                let mut out = vec![];
                if let Some(closed) = self.close_pending_agent(ActivityOutcome::Success) {
                    out.push(closed);
                }
                if let Some(closed) = self.close_pending_turn(ActivityOutcome::Success)
                {
                    out.push(closed);
                }
                // Seed the shared message ID for this turn so subsequent
                // deltas and the final ModelResponse all line up on it.
                self.current_message_id = Some(MessageId(format!("msg-turn-{turn}")));
                self.current_turn = Some(*turn);
                out.push(PeerEvent::ActivityStarted {
                    id: ActivityId(format!("turn-{turn}")),
                    kind: ActivityKind::ModelTurn,
                    label: format!("turn {turn}"),
                });
                out
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
                self.current_message_id = None;
                let mut out = vec![];
                // Close the in-flight agent first (if any), then the
                // turn. Both use the same ids their Started events
                // used so observers see matching start/complete pairs.
                if let Some(closed) = self.close_pending_agent(ActivityOutcome::Success) {
                    out.push(closed);
                }
                if let Some(closed) = self.close_pending_turn(ActivityOutcome::Success)
                {
                    out.push(closed);
                } else {
                    // No turn tracked — DoneSignal arrived without a
                    // prior TurnStarted. Synthesize a completion so
                    // observers see *something*, but with a seq-based
                    // id since there's no turn to tie it to.
                    let seq = self.next_seq();
                    out.push(PeerEvent::ActivityCompleted {
                        id: ActivityId(format!("done-{seq}")),
                        outcome: ActivityOutcome::Success,
                    });
                }
                if let Some(msg) = message {
                    out.push(log_event(LogLevel::Info, "agent", format!("done: {msg}")));
                }
                out
            }

            AppEvent::RoundComplete {
                round,
                turns_in_round,
            } => vec![log_event(
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
                // Close any previous agent activity (defensive: if the
                // source emitted two AgentStarted events without an
                // intervening close signal, we don't want to leave an
                // orphaned Started event on the observer's feed).
                let mut out = vec![];
                if let Some(closed) = self.close_pending_agent(ActivityOutcome::Success) {
                    out.push(closed);
                }
                self.current_agent_turn = Some(*turn);
                out.push(PeerEvent::ActivityStarted {
                    id: ActivityId(format!("agent-{turn}")),
                    kind: ActivityKind::ToolCall,
                    label: format!("{label}: {commands_preview}"),
                });
                out
            }

            AppEvent::AgentOutput {
                stdout,
                stderr,
                source: _,
            } => {
                let mut out = vec![];
                if !stdout.is_empty() {
                    // Progress events reuse the id the matching
                    // AgentStarted used so observers can correlate
                    // them. If there's no tracked agent turn (output
                    // arrived without a prior AgentStarted — shouldn't
                    // happen but shouldn't crash either), fall back
                    // to a synthetic id so the event isn't dropped.
                    let id = match self.current_agent_turn {
                        Some(turn) => ActivityId(format!("agent-{turn}")),
                        None => {
                            let seq = self.next_seq();
                            ActivityId(format!("agent-orphan-{seq}"))
                        }
                    };
                    out.push(PeerEvent::ActivityProgress {
                        id,
                        text: Some(stdout.clone()),
                    });
                }
                if !stderr.is_empty() {
                    out.push(log_event(LogLevel::Warn, "agent", stderr.clone()));
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
                    log_event(LogLevel::Info, "subagent", formatted.clone()),
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

            AppEvent::OrchestratorLog { message, level } => vec![log_event(
                upcast_log_level(level),
                "orchestrator",
                message.clone(),
            )],

            AppEvent::ContextManagement { turn } => vec![log_event(
                LogLevel::Debug,
                "context",
                format!("context management turn {turn}"),
            )],

            AppEvent::TaskComplete { reason, summary } => {
                let outcome = match reason.as_str() {
                    "success" | "done" | "completed" => ActivityOutcome::Success,
                    "cancelled" | "canceled" => ActivityOutcome::Cancelled,
                    other => ActivityOutcome::Failed {
                        message: other.to_string(),
                    },
                };
                let mut out = vec![];
                // Close the in-flight agent and turn with the task's
                // outcome so the end-of-task state propagates through
                // the entire activity lifecycle. A failed TaskComplete
                // must *not* stamp the agent as Success — that would
                // produce contradictory completions (agent success,
                // turn failed) in the consumer's feed.
                if let Some(closed) = self.close_pending_agent(outcome.clone()) {
                    out.push(closed);
                }
                if let Some(closed) = self.close_pending_turn(outcome.clone()) {
                    out.push(closed);
                } else {
                    // No turn in flight — synthesize a task-level
                    // completion. Happens for direct-mode single-turn
                    // runs where TaskComplete is the only lifecycle
                    // signal.
                    let seq = self.next_seq();
                    out.push(PeerEvent::ActivityCompleted {
                        id: ActivityId(format!("task-{seq}")),
                        outcome,
                    });
                }
                self.current_message_id = None;
                if let Some(s) = summary {
                    out.push(log_event(LogLevel::Info, "task", s.clone()));
                }
                out
            }

            // ---- Session lifecycle ----
            AppEvent::SessionStarted { session_id, task } => {
                vec![PeerEvent::SessionStarted {
                    session: SessionInfo {
                        session_id: session_id.clone(),
                        label: task.clone(),
                        started_at: now_rfc3339(),
                    },
                }]
            }

            AppEvent::SessionEnded { session_id, reason } => vec![PeerEvent::SessionEnded {
                session_id: session_id.clone(),
                reason: reason.clone(),
            }],

            AppEvent::SessionDirChanged { path } => vec![log_event(
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

            AppEvent::HumanResponseSent => vec![log_event(
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
            } => vec![log_event(
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
            } => vec![log_event(
                LogLevel::Info,
                "display",
                format!("display approval pending on {backend}"),
            )],

            AppEvent::UserDisplayGranted { display_id } => vec![log_event(
                LogLevel::Info,
                "display",
                format!("user granted display {display_id}"),
            )],

            AppEvent::UserDisplayRevoked { display_id, note } => {
                let note_str = note.as_deref().unwrap_or("");
                vec![log_event(
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
            } => vec![log_event(
                LogLevel::Error,
                "recording",
                format!("{stream_name}: {message}"),
            )],

            AppEvent::RecordingDeleted { stream_name } => vec![log_event(
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

            AppEvent::PresenceReady => vec![log_event(
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
                vec![log_event(lvl, "presence", message.clone())]
            }

            AppEvent::PresenceCheckpointReceived {
                summary,
                last_event_seq,
            } => vec![log_event(
                LogLevel::Info,
                "presence",
                format!("checkpoint at seq {last_event_seq}: {summary}"),
            )],

            AppEvent::VoiceLog {
                text,
                seq: _,
                tool_context: _,
            } => vec![log_event(LogLevel::Info, "voice", text.clone())],

            AppEvent::VoiceDiagnostic { kind, detail } => vec![log_event(
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

            AppEvent::ExternalAgentChanged { agent } => vec![log_event(
                LogLevel::Info,
                "config",
                format!(
                    "external agent changed → {}",
                    agent.as_deref().unwrap_or("none")
                ),
            )],

            // ---- Budget / safety ----
            AppEvent::BudgetWarning { pct, remaining } => vec![log_event(
                LogLevel::Warn,
                "budget",
                format!("budget warning: {pct:.1}% remaining={remaining}"),
            )],

            AppEvent::BudgetExhausted { remaining } => vec![log_event(
                LogLevel::Warn,
                "budget",
                format!("budget exhausted, remaining={remaining}"),
            )],

            AppEvent::SafetyCapReached => vec![log_event(
                LogLevel::Warn,
                "safety",
                "safety cap reached".to_string(),
            )],

            AppEvent::LoopError(msg) => vec![log_event(LogLevel::Error, "agent", msg.clone())],

            AppEvent::JsonExtracted { preview } => vec![log_event(
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
                vec![log_event(log_level, source, content.clone())]
            }

            // ---- Terminal ----
            AppEvent::Quit => vec![PeerEvent::Disconnected {
                reason: "quit".to_string(),
            }],

            // ---- Interruption ----
            AppEvent::InterruptRequested => vec![log_event(
                LogLevel::Info,
                "agent",
                "interrupt requested".to_string(),
            )],
            AppEvent::Interrupted { reason } => vec![log_event(
                LogLevel::Info,
                "agent",
                format!("interrupted: {reason}"),
            )],
        }
    }
}

// ---------------------------------------------------------------------------
// WireEventUpcaster — OutboundEvent → PeerEvent
// ---------------------------------------------------------------------------
//
// Used by `IntendantWsTransport` to map a peer Intendant's `/ws`
// wire stream into the `PeerEvent` vocabulary. Operates on typed
// [`OutboundEvent`] (derived Deserialize + `#[serde(other)] Unknown`
// for forward-compat) rather than raw JSON — the transport parses
// frames through serde, then feeds them here.
//
// Drift-prevention strategy: every AppEvent variant that passes
// through `app_event_to_outbound()` should produce the same
// `Vec<PeerEvent>` whether you route it through `AppEventUpcaster`
// directly or through the wire (`app_event_to_outbound()` +
// `WireEventUpcaster`). The parity tests at the bottom of this
// module enforce that invariant; intentional information loss is
// marked explicitly in each case with a brief rationale.

/// Stateful `OutboundEvent` → `PeerEvent` upcaster for wire-format
/// input (from a peer Intendant's `/ws`).
///
/// Mirrors `AppEventUpcaster`'s state machine exactly — the same
/// `current_message_id` / `current_turn` / `current_agent_turn`
/// tracking for streaming deltas and activity lifecycle. This is
/// the mechanical half of the drift guard: both upcasters derive
/// activity ids from the same tracked state so a `Started` event's
/// id always matches the corresponding `Progress` and `Completed`
/// events. Same outcome-threading contract on `close_pending_agent`
/// as well — TaskComplete propagates its failure/cancel outcome
/// down to any in-flight agent instead of marking it Success.
pub struct WireEventUpcaster {
    seq: u64,
    current_message_id: Option<MessageId>,
    current_turn: Option<usize>,
    current_agent_turn: Option<usize>,
}

impl Default for WireEventUpcaster {
    fn default() -> Self {
        Self::new()
    }
}

impl WireEventUpcaster {
    pub fn new() -> Self {
        Self {
            seq: 0,
            current_message_id: None,
            current_turn: None,
            current_agent_turn: None,
        }
    }

    fn next_seq(&mut self) -> u64 {
        self.seq = self.seq.saturating_add(1);
        self.seq
    }

    fn current_or_new_message_id(&mut self) -> MessageId {
        if let Some(id) = &self.current_message_id {
            return id.clone();
        }
        let seq = self.next_seq();
        let id = MessageId(format!("msg-seq-{seq}"));
        self.current_message_id = Some(id.clone());
        id
    }

    /// Same contract as `AppEventUpcaster::close_pending_agent` —
    /// the caller supplies the outcome to stamp on the emitted
    /// `ActivityCompleted` so a failing task can propagate its
    /// failure down to the in-flight agent instead of contradicting
    /// it with Success.
    fn close_pending_agent(&mut self, outcome: ActivityOutcome) -> Option<PeerEvent> {
        self.current_agent_turn.take().map(|turn| {
            PeerEvent::ActivityCompleted {
                id: ActivityId(format!("agent-{turn}")),
                outcome,
            }
        })
    }

    fn close_pending_turn(&mut self, outcome: ActivityOutcome) -> Option<PeerEvent> {
        self.current_turn.take().map(|turn| {
            PeerEvent::ActivityCompleted {
                id: ActivityId(format!("turn-{turn}")),
                outcome,
            }
        })
    }

    /// Map a wire-format [`OutboundEvent`] to zero or more
    /// [`PeerEvent`]s.
    pub fn upcast(&mut self, event: &OutboundEvent) -> Vec<PeerEvent> {
        match event {
            // ---- Forward-compat + dropped metric streams ----
            OutboundEvent::Unknown
            | OutboundEvent::DisplayMetrics { .. }
            | OutboundEvent::FileChanged { .. } => vec![],

            // ---- Turn lifecycle ----
            OutboundEvent::TurnStarted { turn, .. } => {
                let mut out = vec![];
                if let Some(closed) = self.close_pending_agent(ActivityOutcome::Success) {
                    out.push(closed);
                }
                if let Some(closed) = self.close_pending_turn(ActivityOutcome::Success)
                {
                    out.push(closed);
                }
                self.current_message_id = Some(MessageId(format!("msg-turn-{turn}")));
                self.current_turn = Some(*turn);
                out.push(PeerEvent::ActivityStarted {
                    id: ActivityId(format!("turn-{turn}")),
                    kind: ActivityKind::ModelTurn,
                    label: format!("turn {turn}"),
                });
                out
            }

            OutboundEvent::ModelResponseDelta { text } => {
                let id = self.current_or_new_message_id();
                vec![PeerEvent::Message {
                    id,
                    role: MessageRole::Assistant,
                    content: MessageContent::Text { text: text.clone() },
                    partial: true,
                }]
            }

            // OutboundEvent::ModelResponse does NOT carry usage on the
            // wire — usage travels as a separate OutboundEvent::Usage /
            // UsageUpdate. That's the documented information-split: the
            // `AppEvent → AppEventUpcaster` path emits Message + Usage
            // from one ModelResponse, while the wire path emits
            // Message from this variant and relies on a sibling
            // OutboundEvent::Usage to carry the tokens. The parity
            // test `model_response_usage_accounting_drift` documents
            // this gap explicitly.
            OutboundEvent::ModelResponse {
                turn,
                summary,
                reasoning_summary,
                source: _,
            } => {
                let msg_id = self.current_or_new_message_id();
                let mut out = vec![PeerEvent::Message {
                    id: msg_id,
                    role: MessageRole::Assistant,
                    content: MessageContent::Text {
                        text: summary.clone(),
                    },
                    partial: false,
                }];
                if let Some(reasoning_text) = reasoning_summary {
                    out.push(PeerEvent::Message {
                        id: MessageId(format!("reasoning-turn-{turn}")),
                        role: MessageRole::Assistant,
                        content: MessageContent::Reasoning {
                            text: reasoning_text.clone(),
                        },
                        partial: false,
                    });
                }
                self.current_message_id = None;
                out
            }

            OutboundEvent::ModelSummary {
                turn,
                summary,
                reasoning_summary,
            } => {
                // Same shape as ModelResponse but without a source
                // override. Emitted by some paths as a distilled
                // summary rather than a full response. Maps to the
                // same Message + Reasoning shape.
                let msg_id = MessageId(format!("summary-turn-{turn}"));
                let mut out = vec![PeerEvent::Message {
                    id: msg_id,
                    role: MessageRole::Assistant,
                    content: MessageContent::Text {
                        text: summary.clone(),
                    },
                    partial: false,
                }];
                if let Some(reasoning_text) = reasoning_summary {
                    out.push(PeerEvent::Message {
                        id: MessageId(format!("summary-reasoning-turn-{turn}")),
                        role: MessageRole::Assistant,
                        content: MessageContent::Reasoning {
                            text: reasoning_text.clone(),
                        },
                        partial: false,
                    });
                }
                out
            }

            OutboundEvent::DoneSignal { message } => {
                self.current_message_id = None;
                let mut out = vec![];
                if let Some(closed) = self.close_pending_agent(ActivityOutcome::Success) {
                    out.push(closed);
                }
                if let Some(closed) = self.close_pending_turn(ActivityOutcome::Success)
                {
                    out.push(closed);
                } else {
                    let seq = self.next_seq();
                    out.push(PeerEvent::ActivityCompleted {
                        id: ActivityId(format!("done-{seq}")),
                        outcome: ActivityOutcome::Success,
                    });
                }
                if let Some(msg) = message {
                    out.push(log_event(LogLevel::Info, "agent", format!("done: {msg}")));
                }
                out
            }

            OutboundEvent::RoundComplete {
                round,
                turns_in_round,
            } => vec![log_event(
                LogLevel::Info,
                "agent",
                format!("round {round} complete ({turns_in_round} turns)"),
            )],

            // ---- Sub-agent / tool execution ----
            OutboundEvent::AgentStarted {
                turn,
                commands_preview,
                source,
            } => {
                let label = source.clone().unwrap_or_else(|| "agent".to_string());
                let mut out = vec![];
                if let Some(closed) = self.close_pending_agent(ActivityOutcome::Success) {
                    out.push(closed);
                }
                self.current_agent_turn = Some(*turn);
                out.push(PeerEvent::ActivityStarted {
                    id: ActivityId(format!("agent-{turn}")),
                    kind: ActivityKind::ToolCall,
                    label: format!("{label}: {commands_preview}"),
                });
                out
            }

            OutboundEvent::AgentOutput {
                stdout,
                stderr,
                source: _,
            } => {
                let mut out = vec![];
                if !stdout.is_empty() {
                    let id = match self.current_agent_turn {
                        Some(turn) => ActivityId(format!("agent-{turn}")),
                        None => {
                            let seq = self.next_seq();
                            ActivityId(format!("agent-orphan-{seq}"))
                        }
                    };
                    out.push(PeerEvent::ActivityProgress {
                        id,
                        text: Some(stdout.clone()),
                    });
                }
                if !stderr.is_empty() {
                    out.push(log_event(LogLevel::Warn, "agent", stderr.clone()));
                }
                out
            }

            OutboundEvent::SubAgentResult { summary } => {
                let seq = self.next_seq();
                vec![
                    PeerEvent::ActivityCompleted {
                        id: ActivityId(format!("subagent-{seq}")),
                        outcome: ActivityOutcome::Success,
                    },
                    log_event(LogLevel::Info, "subagent", summary.clone()),
                ]
            }

            // OutboundEvent::OrchestratorProgress only carries `status`
            // — the wire format loses the `turn` and `last_action`
            // fields that AppEvent carries. The parity test flags
            // this as intentional loss.
            OutboundEvent::OrchestratorProgress { status } => {
                vec![PeerEvent::ActivityProgress {
                    id: ActivityId("orchestrator".into()),
                    text: Some(status.clone()),
                }]
            }

            OutboundEvent::ContextManagement { turn } => vec![log_event(
                LogLevel::Debug,
                "context",
                format!("context management turn {turn}"),
            )],

            OutboundEvent::TaskComplete { reason, summary } => {
                let outcome = match reason.as_str() {
                    "success" | "done" | "completed" => ActivityOutcome::Success,
                    "cancelled" | "canceled" => ActivityOutcome::Cancelled,
                    other => ActivityOutcome::Failed {
                        message: other.to_string(),
                    },
                };
                let mut out = vec![];
                // Propagate the task's outcome to any in-flight
                // agent so a failed/cancelled task doesn't emit a
                // contradictory Success on the agent activity.
                if let Some(closed) = self.close_pending_agent(outcome.clone()) {
                    out.push(closed);
                }
                if let Some(closed) = self.close_pending_turn(outcome.clone()) {
                    out.push(closed);
                } else {
                    let seq = self.next_seq();
                    out.push(PeerEvent::ActivityCompleted {
                        id: ActivityId(format!("task-{seq}")),
                        outcome,
                    });
                }
                self.current_message_id = None;
                if let Some(s) = summary {
                    out.push(log_event(LogLevel::Info, "task", s.clone()));
                }
                out
            }

            // ---- Session lifecycle ----
            OutboundEvent::SessionStarted { session_id, task } => {
                vec![PeerEvent::SessionStarted {
                    session: SessionInfo {
                        session_id: session_id.clone(),
                        label: task.clone(),
                        started_at: now_rfc3339(),
                    },
                }]
            }

            OutboundEvent::SessionEnded { session_id, reason } => {
                vec![PeerEvent::SessionEnded {
                    session_id: session_id.clone(),
                    reason: reason.clone(),
                }]
            }

            // ---- Approval flow ----
            //
            // OutboundEvent::ApprovalRequired drops the ActionCategory
            // field from AppEvent (wire format only carries `command`,
            // not category). We default to "command_exec" which is
            // the overwhelmingly common case. Parity test documents
            // this as intentional loss — non-command-exec categories
            // (file_write, destructive, etc.) lose their specific
            // category name on the wire path.
            OutboundEvent::ApprovalRequired { id, command } => {
                vec![PeerEvent::ApprovalRequested {
                    request: ApprovalRequest {
                        request_id: id.to_string(),
                        category: "command_exec".to_string(),
                        preview: command.clone(),
                        auto_resolvable: false,
                    },
                }]
            }

            OutboundEvent::ApprovalResolved { id, action } => {
                vec![PeerEvent::ApprovalResolved {
                    request_id: id.to_string(),
                    decision: approval_decision_from_action(action),
                }]
            }

            OutboundEvent::AutoApproved { preview } => {
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

            OutboundEvent::AskHuman { question } => {
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

            OutboundEvent::HumanResponseSent => vec![log_event(
                LogLevel::Info,
                "human",
                "human response sent".to_string(),
            )],

            // ---- Display capability ----
            OutboundEvent::DisplayReady {
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

            OutboundEvent::DisplayResize {
                display_id,
                width,
                height,
            } => vec![log_event(
                LogLevel::Info,
                "display",
                format!("display {display_id} resized to {width}x{height}"),
            )],

            OutboundEvent::DisplayTaken { display_id } => vec![PeerEvent::CapabilityEngaged {
                capability: Capability::Display,
                detail: serde_json::json!({ "display_id": display_id, "state": "taken" }),
            }],

            OutboundEvent::DisplayReleased { display_id: _, note } => {
                vec![PeerEvent::CapabilityReleased {
                    capability: Capability::Display,
                    reason: note.clone(),
                }]
            }

            OutboundEvent::DisplayCaptureLost {
                display_id: _,
                reason,
            } => vec![PeerEvent::CapabilityReleased {
                capability: Capability::Display,
                reason: Some(format!("capture_lost: {reason}")),
            }],

            OutboundEvent::DisplayApprovalPending {
                display_id: _,
                backend,
            } => vec![log_event(
                LogLevel::Info,
                "display",
                format!("display approval pending on {backend}"),
            )],

            // OutboundEvent::UserDisplayGranted has NO fields on the
            // wire — AppEvent has `display_id: u32` but that's dropped
            // in app_event_to_outbound. Parity test documents this.
            OutboundEvent::UserDisplayGranted => vec![log_event(
                LogLevel::Info,
                "display",
                "user granted display".to_string(),
            )],

            OutboundEvent::UserDisplayRevoked { display_id, note } => {
                let note_str = note.as_deref().unwrap_or("");
                vec![log_event(
                    LogLevel::Info,
                    "display",
                    format!("user revoked display {display_id}: {note_str}"),
                )]
            }

            OutboundEvent::DebugScreenReady { display_id } => {
                vec![PeerEvent::CapabilityEngaged {
                    capability: Capability::Display,
                    detail: serde_json::json!({
                        "display_id": display_id,
                        "kind": "debug_screen",
                    }),
                }]
            }

            OutboundEvent::DebugScreenTornDown { display_id: _ } => {
                vec![PeerEvent::CapabilityReleased {
                    capability: Capability::Display,
                    reason: Some("debug_screen_torn_down".to_string()),
                }]
            }

            // ---- Recording capability ----
            OutboundEvent::RecordingStarted { stream_name } => {
                vec![PeerEvent::CapabilityEngaged {
                    capability: Capability::Recording,
                    detail: serde_json::json!({ "stream": stream_name }),
                }]
            }

            OutboundEvent::RecordingStopped { stream_name } => {
                vec![PeerEvent::CapabilityReleased {
                    capability: Capability::Recording,
                    reason: Some(format!("stopped: {stream_name}")),
                }]
            }

            OutboundEvent::RecordingError {
                stream_name,
                message,
            } => vec![log_event(
                LogLevel::Error,
                "recording",
                format!("{stream_name}: {message}"),
            )],

            OutboundEvent::RecordingDeleted { stream_name } => vec![log_event(
                LogLevel::Info,
                "recording",
                format!("{stream_name} deleted"),
            )],

            // ---- Presence (wire side has only PresenceLog, no
            // Connected/Disconnected — those are presence lifecycle
            // events that don't make it past app_event_to_outbound) ----
            OutboundEvent::PresenceLog { message, level } => {
                let lvl = level
                    .as_deref()
                    .map(wire_log_level)
                    .unwrap_or(LogLevel::Info);
                vec![log_event(lvl, "presence", message.clone())]
            }

            OutboundEvent::UserTranscript { text, seq: _ } => {
                let seq = self.next_seq();
                vec![PeerEvent::Message {
                    id: MessageId(format!("user-transcript-{seq}")),
                    role: MessageRole::User,
                    content: MessageContent::Text { text: text.clone() },
                    partial: false,
                }]
            }

            // ---- Usage accounting ----
            OutboundEvent::PresenceUsageUpdate {
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

            OutboundEvent::LiveUsageUpdate {
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

            OutboundEvent::Usage { main, presence: _ }
            | OutboundEvent::UsageUpdate { main, presence: _ } => vec![PeerEvent::Usage {
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
            OutboundEvent::Status { phase, .. } => {
                let status = status_from_phase(phase);
                vec![PeerEvent::StatusChanged { status }]
            }

            OutboundEvent::ExternalAgentChanged { agent } => vec![log_event(
                LogLevel::Info,
                "config",
                format!(
                    "external agent changed → {}",
                    agent.as_deref().unwrap_or("none")
                ),
            )],

            // ---- Budget / safety ----
            OutboundEvent::BudgetWarning { pct, remaining } => vec![log_event(
                LogLevel::Warn,
                "budget",
                format!("budget warning: {pct:.1}% remaining={remaining}"),
            )],

            OutboundEvent::BudgetExhausted { remaining } => vec![log_event(
                LogLevel::Warn,
                "budget",
                format!("budget exhausted, remaining={remaining}"),
            )],

            OutboundEvent::SafetyCapReached => vec![log_event(
                LogLevel::Warn,
                "safety",
                "safety cap reached".to_string(),
            )],

            OutboundEvent::LoopError { message } => {
                vec![log_event(LogLevel::Error, "agent", message.clone())]
            }

            // ---- Log passthrough ----
            OutboundEvent::LogEntry {
                level,
                source,
                content,
                turn: _,
            } => {
                vec![log_event(wire_log_level(level), source, content.clone())]
            }

            // ---- CommandResult: control-plane meta-event ----
            //
            // CommandResult is the ack for a ControlMsg — "the Approve
            // action succeeded", "the SetAutonomy call failed with
            // 'bad level'", etc. It has no direct AppEvent ancestor
            // (it's synthesized by control_plane.rs, not the agent
            // loop). For federation, it surfaces as an info/error
            // log so an observer sees what the peer's control plane
            // is doing.
            OutboundEvent::CommandResult {
                action,
                ok,
                message,
                data: _,
            } => {
                let level = if *ok { LogLevel::Info } else { LogLevel::Warn };
                vec![log_event(
                    level,
                    "control",
                    format!("{action}: {message}"),
                )]
            }

            // ---- Interruption ----
            OutboundEvent::InterruptRequested => vec![log_event(
                LogLevel::Info,
                "agent",
                "interrupt requested".to_string(),
            )],
            OutboundEvent::Interrupted { reason } => vec![log_event(
                LogLevel::Info,
                "agent",
                format!("interrupted: {reason}"),
            )],
        }
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

    /// ActivityId lifecycle contract: a model turn's Started and
    /// Completed events must share the same id so observers can
    /// correlate start→complete. Previously DoneSignal synthesized
    /// a `done-{seq}` id that had no relation to the TurnStarted's
    /// `turn-{N}` — two events with no way for the receiver to
    /// know they belonged to the same activity.
    #[test]
    fn model_turn_activity_ids_match_start_to_complete() {
        let mut u = AppEventUpcaster::new();
        let started = u.upcast(&AppEvent::TurnStarted {
            turn: 7,
            budget_pct: 0.5,
            remaining: 100,
        });
        let start_id = match started.last().unwrap() {
            PeerEvent::ActivityStarted { id, .. } => id.clone(),
            _ => panic!("expected ActivityStarted"),
        };
        assert_eq!(start_id.0, "turn-7");

        let done = u.upcast(&AppEvent::DoneSignal { message: None });
        let complete_id = done
            .iter()
            .find_map(|e| match e {
                PeerEvent::ActivityCompleted { id, .. } => Some(id.clone()),
                _ => None,
            })
            .expect("DoneSignal must emit an ActivityCompleted");
        assert_eq!(
            complete_id, start_id,
            "start and complete events must share the activity id"
        );
    }

    /// Agent activity lifecycle: started / progress / completed all
    /// share the same id so observers can correlate tool output
    /// with its tool call.
    #[test]
    fn agent_activity_ids_match_start_progress_complete() {
        let mut u = AppEventUpcaster::new();
        // Open a turn so the agent has a parent context (matches
        // typical usage; not strictly required).
        let _ = u.upcast(&AppEvent::TurnStarted {
            turn: 3,
            budget_pct: 0.5,
            remaining: 100,
        });
        // Start an agent activity.
        let started = u.upcast(&AppEvent::AgentStarted {
            turn: 3,
            commands_preview: "ls -la".into(),
            source: None,
        });
        let start_id = started
            .iter()
            .find_map(|e| match e {
                PeerEvent::ActivityStarted { id, kind, .. }
                    if *kind == ActivityKind::ToolCall =>
                {
                    Some(id.clone())
                }
                _ => None,
            })
            .expect("AgentStarted must emit an ActivityStarted(ToolCall)");
        assert_eq!(start_id.0, "agent-3");

        // Stream some output.
        let output = u.upcast(&AppEvent::AgentOutput {
            stdout: "file1\nfile2".into(),
            stderr: String::new(),
            source: None,
        });
        let progress_id = output
            .iter()
            .find_map(|e| match e {
                PeerEvent::ActivityProgress { id, .. } => Some(id.clone()),
                _ => None,
            })
            .expect("AgentOutput with stdout must emit an ActivityProgress");
        assert_eq!(
            progress_id, start_id,
            "progress id must match started id"
        );

        // Close the turn → agent activity should close with the same id.
        let done = u.upcast(&AppEvent::DoneSignal { message: None });
        let completed_ids: Vec<_> = done
            .iter()
            .filter_map(|e| match e {
                PeerEvent::ActivityCompleted { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect();
        // Both the agent and the turn should close. The agent
        // completes first with agent-3, the turn with turn-3.
        assert!(
            completed_ids.iter().any(|id| *id == start_id),
            "DoneSignal must close the agent with its started id. \
             Got: {completed_ids:?}"
        );
    }

    /// Same lifecycle guarantee on the wire upcaster. Parity with
    /// AppEventUpcaster is enforced mechanically because both
    /// upcasters derive ids from the same tracked state.
    #[test]
    fn wire_model_turn_activity_ids_match_start_to_complete() {
        let mut u = WireEventUpcaster::new();
        let started = u.upcast(&OutboundEvent::TurnStarted {
            turn: 7,
            budget_pct: 0.5,
        });
        let start_id = started
            .iter()
            .find_map(|e| match e {
                PeerEvent::ActivityStarted { id, .. } => Some(id.clone()),
                _ => None,
            })
            .expect("expected ActivityStarted");
        assert_eq!(start_id.0, "turn-7");

        let done = u.upcast(&OutboundEvent::DoneSignal { message: None });
        let complete_id = done
            .iter()
            .find_map(|e| match e {
                PeerEvent::ActivityCompleted { id, .. } => Some(id.clone()),
                _ => None,
            })
            .expect("DoneSignal must emit ActivityCompleted");
        assert_eq!(complete_id, start_id);
    }

    #[test]
    fn wire_agent_activity_ids_match_start_progress_complete() {
        let mut u = WireEventUpcaster::new();
        let _ = u.upcast(&OutboundEvent::TurnStarted {
            turn: 3,
            budget_pct: 0.5,
        });
        let started = u.upcast(&OutboundEvent::AgentStarted {
            turn: 3,
            commands_preview: "ls -la".into(),
            source: None,
        });
        let start_id = started
            .iter()
            .find_map(|e| match e {
                PeerEvent::ActivityStarted { id, .. } => Some(id.clone()),
                _ => None,
            })
            .expect("expected ActivityStarted");
        assert_eq!(start_id.0, "agent-3");

        let output = u.upcast(&OutboundEvent::AgentOutput {
            stdout: "file1".into(),
            stderr: String::new(),
            source: None,
        });
        let progress_id = output
            .iter()
            .find_map(|e| match e {
                PeerEvent::ActivityProgress { id, .. } => Some(id.clone()),
                _ => None,
            })
            .expect("expected ActivityProgress");
        assert_eq!(progress_id, start_id);
    }

    /// A failing `TaskComplete` must propagate its failure outcome
    /// to *both* the in-flight agent and the turn. Before the
    /// outcome-threading fix, `close_pending_agent` hardcoded
    /// `ActivityOutcome::Success`, so a failed task emitted a
    /// Success ActivityCompleted for the agent and a Failed
    /// ActivityCompleted for the turn — contradictory events in
    /// the consumer's feed that would render as "the tool
    /// succeeded but the turn it ran in failed." Verifies both
    /// upcasters behave consistently on both failure and cancel.
    #[test]
    fn task_complete_failure_propagates_to_agent_and_turn() {
        for (reason, expected) in &[
            ("failed", "failed"),
            ("cancelled", "cancelled"),
        ] {
            let mut u = AppEventUpcaster::new();
            // Open turn + agent, then fail.
            let _ = u.upcast(&AppEvent::TurnStarted {
                turn: 4,
                budget_pct: 0.5,
                remaining: 100,
            });
            let _ = u.upcast(&AppEvent::AgentStarted {
                turn: 4,
                commands_preview: "risky".into(),
                source: None,
            });
            let out = u.upcast(&AppEvent::TaskComplete {
                reason: (*reason).to_string(),
                summary: None,
            });
            let completions: Vec<_> = out
                .iter()
                .filter_map(|e| match e {
                    PeerEvent::ActivityCompleted { id, outcome } => {
                        Some((id.clone(), outcome.clone()))
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(
                completions.len(),
                2,
                "expected agent + turn completions for reason={reason}, \
                 got: {completions:?}"
            );
            for (id, outcome) in &completions {
                let outcome_matches = match (*expected, outcome) {
                    ("failed", ActivityOutcome::Failed { .. }) => true,
                    ("cancelled", ActivityOutcome::Cancelled) => true,
                    _ => false,
                };
                assert!(
                    outcome_matches,
                    "activity {id:?} for reason={reason} should have \
                     outcome matching {expected}, got {outcome:?}"
                );
            }
        }
    }

    /// Same guarantee on the wire upcaster.
    #[test]
    fn wire_task_complete_failure_propagates_to_agent_and_turn() {
        let mut u = WireEventUpcaster::new();
        let _ = u.upcast(&OutboundEvent::TurnStarted {
            turn: 4,
            budget_pct: 0.5,
        });
        let _ = u.upcast(&OutboundEvent::AgentStarted {
            turn: 4,
            commands_preview: "risky".into(),
            source: None,
        });
        let out = u.upcast(&OutboundEvent::TaskComplete {
            reason: "failed".to_string(),
            summary: None,
        });
        let completions: Vec<_> = out
            .iter()
            .filter_map(|e| match e {
                PeerEvent::ActivityCompleted { id, outcome } => {
                    Some((id.clone(), outcome.clone()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(completions.len(), 2);
        for (id, outcome) in &completions {
            assert!(
                matches!(outcome, ActivityOutcome::Failed { .. }),
                "wire path: activity {id:?} should be Failed, got {outcome:?}"
            );
        }
    }

    /// Parity on DoneSignal: given a TurnStarted + DoneSignal
    /// sequence, the app and wire paths must produce the same
    /// activity ids. Both upcasters track `current_turn` the same
    /// way, so this test catches any future drift in how that
    /// state is used.
    ///
    /// Note: we can't use the single-event `assert_parity` helper
    /// here because DoneSignal's id depends on prior state
    /// (TurnStarted seeded current_turn). Drive both upcasters
    /// through the full sequence manually.
    #[test]
    fn parity_done_signal_uses_tracked_turn() {
        let mut app = AppEventUpcaster::new();
        let mut wire = WireEventUpcaster::new();

        // Seed both with TurnStarted turn=5.
        let _ = app.upcast(&AppEvent::TurnStarted {
            turn: 5,
            budget_pct: 0.5,
            remaining: 100,
        });
        let _ = wire.upcast(&OutboundEvent::TurnStarted {
            turn: 5,
            budget_pct: 0.5,
        });

        // Both see DoneSignal.
        let app_out = app.upcast(&AppEvent::DoneSignal { message: None });
        let wire_out = wire.upcast(&OutboundEvent::DoneSignal { message: None });

        let app_completed_ids: Vec<_> = app_out
            .iter()
            .filter_map(|e| match e {
                PeerEvent::ActivityCompleted { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect();
        let wire_completed_ids: Vec<_> = wire_out
            .iter()
            .filter_map(|e| match e {
                PeerEvent::ActivityCompleted { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            app_completed_ids, wire_completed_ids,
            "DoneSignal activity ids must match across paths after \
             TurnStarted seeded current_turn"
        );
        assert!(app_completed_ids.iter().any(|id| id.0 == "turn-5"));
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

    // ===================================================================
    // WireEventUpcaster tests — OutboundEvent → PeerEvent
    // ===================================================================

    /// `OutboundEvent` forward-compat: an unknown wire tag
    /// deserializes to `OutboundEvent::Unknown` and the upcaster
    /// drops it silently. This is the guardrail that lets us
    /// evolve the wire protocol without breaking older peers.
    #[test]
    fn outbound_unknown_variant_deserializes_and_drops() {
        let json = r#"{"event":"holographic_projection_started","intensity":"high"}"#;
        let parsed: OutboundEvent = serde_json::from_str(json).unwrap();
        assert!(matches!(parsed, OutboundEvent::Unknown));
        let out = WireEventUpcaster::new().upcast(&parsed);
        assert!(out.is_empty());
    }

    /// Wire-format `TurnStarted` seeds current_message_id and emits
    /// ActivityStarted, same as the AppEvent path.
    #[test]
    fn wire_turn_started_emits_activity_started() {
        let mut u = WireEventUpcaster::new();
        let out = u.upcast(&OutboundEvent::TurnStarted {
            turn: 3,
            budget_pct: 0.5,
        });
        assert_eq!(out.len(), 1);
        assert!(matches!(
            &out[0],
            PeerEvent::ActivityStarted {
                kind: ActivityKind::ModelTurn,
                ..
            }
        ));
    }

    /// Wire-format streaming deltas share an id with the final
    /// ModelResponse within the same turn. Same state machine
    /// as the AppEvent path — the parity is *mechanical*, not
    /// coincidental, because both upcasters use the same turn-id
    /// scheme when seeded by `TurnStarted`.
    #[test]
    fn wire_streaming_deltas_share_id_with_final_response() {
        let mut u = WireEventUpcaster::new();
        let _ = u.upcast(&OutboundEvent::TurnStarted {
            turn: 5,
            budget_pct: 0.5,
        });
        let delta = u.upcast(&OutboundEvent::ModelResponseDelta {
            text: "Hel".into(),
        });
        let final_ = u.upcast(&OutboundEvent::ModelResponse {
            turn: 5,
            summary: "Hello".into(),
            reasoning_summary: None,
            source: None,
        });
        let delta_id = match &delta[0] {
            PeerEvent::Message { id, partial, .. } => {
                assert!(*partial);
                id.clone()
            }
            _ => panic!("expected delta Message"),
        };
        let final_id = match &final_[0] {
            PeerEvent::Message { id, partial, .. } => {
                assert!(!partial);
                id.clone()
            }
            _ => panic!("expected final Message"),
        };
        assert_eq!(delta_id, final_id);
    }

    /// Wire-format `Status` maps phase strings to `PeerStatus` via
    /// the shared `status_from_phase` helper (so it's impossible
    /// for wire and app paths to diverge on phase interpretation).
    #[test]
    fn wire_status_phase_mapping() {
        let mut u = WireEventUpcaster::new();
        let out = u.upcast(&OutboundEvent::Status {
            turn: 1,
            phase: "thinking".into(),
            autonomy: "medium".into(),
            session_id: "s".into(),
            task: "t".into(),
            external_agent: None,
        });
        assert!(matches!(
            &out[0],
            PeerEvent::StatusChanged {
                status: PeerStatus::Working,
            }
        ));
    }

    /// Wire-format `CommandResult` — control-plane ack event that
    /// has no AppEvent ancestor — surfaces as a log.
    #[test]
    fn wire_command_result_logs() {
        let mut u = WireEventUpcaster::new();
        let ok = u.upcast(&OutboundEvent::CommandResult {
            action: "approve".into(),
            ok: true,
            message: "resolved".into(),
            data: None,
        });
        match &ok[0] {
            PeerEvent::Log {
                level,
                source,
                message,
                ..
            } => {
                assert_eq!(*level, LogLevel::Info);
                assert_eq!(source, "control");
                assert!(message.contains("approve"));
            }
            _ => panic!("expected Log"),
        }
        let fail = u.upcast(&OutboundEvent::CommandResult {
            action: "deny".into(),
            ok: false,
            message: "bad id".into(),
            data: None,
        });
        match &fail[0] {
            PeerEvent::Log { level, .. } => assert_eq!(*level, LogLevel::Warn),
            _ => panic!("expected Log"),
        }
    }

    // ===================================================================
    // Parity tests — AppEvent → AppEventUpcaster ≡
    //                AppEvent → app_event_to_outbound → WireEventUpcaster
    // ===================================================================
    //
    // The drift guard for the two upcasters. For every AppEvent
    // variant where `app_event_to_outbound` returns `Some(..)` AND
    // the wire projection preserves enough information for the
    // mapping to be lossless, both paths must produce structurally
    // equivalent `Vec<PeerEvent>`. Intentional information loss is
    // marked with its own test and documented — those are expected
    // drift, not parity bugs.

    /// Normalize a list of PeerEvents into JSON with timestamp fields
    /// replaced by a constant. Timestamps (`ts`, `started_at`) are
    /// generated at upcast time via `chrono::Utc::now()` so two
    /// otherwise-equivalent calls will differ on them — the
    /// normalization is what makes structural parity checkable.
    fn normalize(events: &[PeerEvent]) -> Vec<serde_json::Value> {
        fn strip_timestamps(v: &mut serde_json::Value) {
            match v {
                serde_json::Value::Object(obj) => {
                    for key in ["ts", "started_at"] {
                        if obj.contains_key(key) {
                            obj.insert(
                                key.to_string(),
                                serde_json::Value::String("NORMALIZED".into()),
                            );
                        }
                    }
                    for (_, child) in obj.iter_mut() {
                        strip_timestamps(child);
                    }
                }
                serde_json::Value::Array(arr) => {
                    for child in arr.iter_mut() {
                        strip_timestamps(child);
                    }
                }
                _ => {}
            }
        }
        events
            .iter()
            .map(|e| {
                let mut v = serde_json::to_value(e).unwrap();
                strip_timestamps(&mut v);
                v
            })
            .collect()
    }

    /// Run an AppEvent through both paths and assert the normalized
    /// outputs match. Fresh upcasters ensure seq counters start at
    /// zero on both sides so synthesized IDs line up.
    fn assert_parity(app_event: AppEvent) {
        let mut app_upcaster = AppEventUpcaster::new();
        let mut wire_upcaster = WireEventUpcaster::new();

        let path_a = app_upcaster.upcast(&app_event);

        let outbound = crate::event::app_event_to_outbound(&app_event).unwrap_or_else(|| {
            panic!(
                "app_event_to_outbound returned None for {:?} — not eligible for parity check",
                app_event
            )
        });
        let path_b = wire_upcaster.upcast(&outbound);

        let a = normalize(&path_a);
        let b = normalize(&path_b);

        assert_eq!(
            a, b,
            "parity failure for {app_event:?}\npath A (app) = {a:#?}\npath B (wire) = {b:#?}"
        );
    }

    #[test]
    fn parity_turn_started() {
        assert_parity(AppEvent::TurnStarted {
            turn: 7,
            budget_pct: 0.5,
            remaining: 100,
        });
    }

    #[test]
    fn parity_session_started() {
        assert_parity(AppEvent::SessionStarted {
            session_id: "sess-99".into(),
            task: Some("research".into()),
        });
    }

    #[test]
    fn parity_session_ended() {
        assert_parity(AppEvent::SessionEnded {
            session_id: "sess-99".into(),
            reason: "done".into(),
        });
    }

    #[test]
    fn parity_display_ready() {
        assert_parity(AppEvent::DisplayReady {
            display_id: 1,
            width: 1920,
            height: 1080,
        });
    }

    #[test]
    fn parity_display_released() {
        assert_parity(AppEvent::DisplayReleased {
            display_id: 1,
            note: Some("user revoked".into()),
        });
    }

    #[test]
    fn parity_display_capture_lost() {
        assert_parity(AppEvent::DisplayCaptureLost {
            display_id: 1,
            reason: "backend_crashed".into(),
        });
    }

    #[test]
    fn parity_recording_started() {
        assert_parity(AppEvent::RecordingStarted {
            stream_name: "display-1".into(),
        });
    }

    #[test]
    fn parity_recording_stopped() {
        assert_parity(AppEvent::RecordingStopped {
            stream_name: "display-1".into(),
        });
    }

    #[test]
    fn parity_recording_error() {
        assert_parity(AppEvent::RecordingError {
            stream_name: "display-1".into(),
            message: "encoder lost".into(),
        });
    }

    #[test]
    fn parity_round_complete() {
        assert_parity(AppEvent::RoundComplete {
            round: 3,
            turns_in_round: 7,
        });
    }

    #[test]
    fn parity_human_response_sent() {
        assert_parity(AppEvent::HumanResponseSent);
    }

    #[test]
    fn parity_safety_cap_reached() {
        assert_parity(AppEvent::SafetyCapReached);
    }

    #[test]
    fn parity_context_management() {
        assert_parity(AppEvent::ContextManagement { turn: 5 });
    }

    #[test]
    fn parity_budget_warning() {
        assert_parity(AppEvent::BudgetWarning {
            pct: 12.5,
            remaining: 1000,
        });
    }

    #[test]
    fn parity_budget_exhausted() {
        assert_parity(AppEvent::BudgetExhausted { remaining: 0 });
    }

    #[test]
    fn parity_external_agent_changed() {
        assert_parity(AppEvent::ExternalAgentChanged {
            agent: Some("codex".into()),
        });
    }

    #[test]
    fn parity_log_entry() {
        assert_parity(AppEvent::LogEntry {
            level: "warn".into(),
            source: "presence".into(),
            content: "something funny".into(),
            turn: Some(3),
        });
    }

    #[test]
    fn parity_loop_error() {
        assert_parity(AppEvent::LoopError("kaboom".to_string()));
    }

    // -------------------------------------------------------------------
    // Documented drift — intentional information loss in the wire path.
    // These cases are NOT bugs; they're the wire protocol's documented
    // lossy projections. Each test captures the specific loss so a
    // future refactor that accidentally widens the drift trips one of
    // them.
    // -------------------------------------------------------------------

    /// `ModelResponse` emits Message + Usage on the app path, but on
    /// the wire path usage travels separately as `OutboundEvent::Usage`.
    /// Parity holds only on the Message prefix; Usage is verified
    /// separately to belong to the main-path output.
    #[test]
    fn drift_model_response_usage_is_separated_on_wire() {
        let app_event = AppEvent::ModelResponse {
            turn: 1,
            content: "Hello world".into(),
            usage: crate::provider::TokenUsage {
                prompt_tokens: 10,
                completion_tokens: 20,
                total_tokens: 30,
                cached_tokens: 2,
            },
            reasoning: None,
            source: None,
        };
        let mut app_upcaster = AppEventUpcaster::new();
        let path_a = app_upcaster.upcast(&app_event);
        // Path A: Message + Usage (2 events).
        assert_eq!(path_a.len(), 2);
        assert!(matches!(&path_a[0], PeerEvent::Message { .. }));
        assert!(matches!(&path_a[1], PeerEvent::Usage { .. }));

        let outbound =
            crate::event::app_event_to_outbound(&app_event).expect("ModelResponse maps");
        let mut wire_upcaster = WireEventUpcaster::new();
        let path_b = wire_upcaster.upcast(&outbound);
        // Path B: just Message — usage arrives in a separate event.
        assert_eq!(path_b.len(), 1);
        assert!(matches!(&path_b[0], PeerEvent::Message { .. }));
        // The Message content should still agree between the two.
        let a_msg = normalize(&path_a[..1]);
        let b_msg = normalize(&path_b[..1]);
        assert_eq!(
            a_msg, b_msg,
            "Message half of ModelResponse must agree across paths"
        );
    }

    /// `ApprovalRequired` loses its `ActionCategory` field on the wire
    /// — the wire format only carries `id` + `command`. The wire
    /// path fills in `"command_exec"` as the default category. Path A
    /// preserves the actual category (e.g. "file_delete").
    #[test]
    fn drift_approval_required_category_is_dropped_on_wire() {
        let app_event = AppEvent::ApprovalRequired {
            id: 42,
            command_preview: "rm -rf /tmp/foo".into(),
            category: crate::autonomy::ActionCategory::FileDelete,
        };
        let mut app_upcaster = AppEventUpcaster::new();
        let path_a = app_upcaster.upcast(&app_event);
        let category_a = match &path_a[0] {
            PeerEvent::ApprovalRequested { request } => request.category.clone(),
            _ => panic!("expected ApprovalRequested"),
        };
        assert_eq!(category_a, "file_delete");

        let outbound =
            crate::event::app_event_to_outbound(&app_event).expect("ApprovalRequired maps");
        let mut wire_upcaster = WireEventUpcaster::new();
        let path_b = wire_upcaster.upcast(&outbound);
        let category_b = match &path_b[0] {
            PeerEvent::ApprovalRequested { request } => request.category.clone(),
            _ => panic!("expected ApprovalRequested"),
        };
        assert_eq!(
            category_b, "command_exec",
            "wire path uses default category because ActionCategory isn't on the wire"
        );
    }

    /// `UserDisplayGranted` loses its `display_id` field on the wire
    /// — `OutboundEvent::UserDisplayGranted` has no fields at all.
    #[test]
    fn drift_user_display_granted_loses_display_id_on_wire() {
        let app_event = AppEvent::UserDisplayGranted { display_id: 99 };
        let mut app_upcaster = AppEventUpcaster::new();
        let path_a = app_upcaster.upcast(&app_event);
        let msg_a = match &path_a[0] {
            PeerEvent::Log { message, .. } => message.clone(),
            _ => panic!("expected Log"),
        };
        assert!(
            msg_a.contains("99"),
            "app path preserves display_id in log: {msg_a}"
        );

        let outbound =
            crate::event::app_event_to_outbound(&app_event).expect("UserDisplayGranted maps");
        let mut wire_upcaster = WireEventUpcaster::new();
        let path_b = wire_upcaster.upcast(&outbound);
        let msg_b = match &path_b[0] {
            PeerEvent::Log { message, .. } => message.clone(),
            _ => panic!("expected Log"),
        };
        assert!(
            !msg_b.contains("99"),
            "wire path cannot include display_id because wire variant has no fields: {msg_b}"
        );
    }

    /// `OrchestratorProgress` loses `turn` and `last_action` on the
    /// wire — the wire format only carries `status`.
    #[test]
    fn drift_orchestrator_progress_loses_turn_and_last_action() {
        let app_event = AppEvent::OrchestratorProgress {
            turn: 5,
            status: "analyzing".into(),
            last_action: "parsed response".into(),
        };
        let mut app_upcaster = AppEventUpcaster::new();
        let path_a = app_upcaster.upcast(&app_event);
        let text_a = match &path_a[0] {
            PeerEvent::ActivityProgress { text, .. } => text.clone().unwrap_or_default(),
            _ => panic!("expected ActivityProgress"),
        };
        assert!(text_a.contains("analyzing") && text_a.contains("parsed response"));

        let outbound = crate::event::app_event_to_outbound(&app_event)
            .expect("OrchestratorProgress maps");
        let mut wire_upcaster = WireEventUpcaster::new();
        let path_b = wire_upcaster.upcast(&outbound);
        let text_b = match &path_b[0] {
            PeerEvent::ActivityProgress { text, .. } => text.clone().unwrap_or_default(),
            _ => panic!("expected ActivityProgress"),
        };
        assert_eq!(
            text_b, "analyzing",
            "wire path has only `status`, loses `last_action`"
        );
    }
}
