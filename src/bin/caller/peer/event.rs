//! Lean transport-neutral event vocabulary for peer federation.
//!
//! The federation layer must work uniformly across heterogeneous peers
//! (Intendant, OpenClaw, A2A, MCP), so this enum is the convex hull of
//! what every transport can map into. It deliberately does NOT carry
//! Intendant-specific concepts like [`crate::event::AppEvent`] — the
//! native Intendant transport upcasts `AppEvent` into these variants in
//! `transport/intendant.rs` (via `crate::event::app_event_to_peer_event`).
//!
//! Variants are organized into categories that map to the dashboard UI:
//! lifecycle, activity stream, conversation, task delegation, approval,
//! capability state, usage, session, and log. Every category corresponds
//! to a renderable surface — there is no "miscellaneous" variant and no
//! `Native(AppEvent)` escape hatch.
//!
//! ## Forward-compat fallback variants
//!
//! `PeerEvent` itself is constructed in Rust by transport adapters, not
//! deserialized from raw wire JSON, so it does *not* need an `Unknown`
//! fallback — any wire event a transport doesn't recognize fails at the
//! transport-parse layer where the diagnostic is actionable. The inner
//! content enums (`PeerStatus`, `ActivityKind`, `ActivityOutcome`,
//! `TaskUpdate`, `MessageContent`, `MessagePart`) *do* get forward-compat
//! `Unknown` variants, because those fields are parsed out of wire
//! content and older builds must tolerate new peer-side values without
//! failing the whole event. `MessageRole`, `LogLevel`, and
//! `ApprovalDecision` are deliberately kept closed — they map to
//! ecosystem-wide stable vocabularies (OpenAI/Anthropic roles, RFC 5424
//! levels, four-way approval) that don't evolve on our timescale.

use crate::peer::card::{AgentCard, Capability};
use crate::peer::id::PeerId;
use serde::{Deserialize, Serialize};

/// One event from a peer. The originating `PeerId` is attached at the
/// registry layer via [`TaggedPeerEvent`] — the inner enum stays unaware
/// of which peer produced it so transport adapters can construct events
/// without round-tripping the id.
///
/// The serde tag is `event` (not `kind`) so it doesn't collide with
/// inner fields named `kind` (e.g. `ActivityStarted::kind: ActivityKind`).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum PeerEvent {
    // ---- Connection lifecycle ----
    /// Peer just completed handshake and sent its (possibly updated) card.
    Connected {
        card: AgentCard,
    },

    /// Peer disconnected. The transport may auto-reconnect; if so, a
    /// follow-up `Connected` will arrive when the handshake completes.
    Disconnected {
        reason: String,
    },

    /// Peer's overall status changed.
    StatusChanged {
        status: PeerStatus,
    },

    // ---- Activity stream — what the peer is doing right now ----
    /// A unit of work has begun (turn, tool call, sub-agent run, delegated
    /// task, etc). Activities have an opaque id and a kind for routing.
    ActivityStarted {
        id: ActivityId,
        kind: ActivityKind,
        label: String,
    },

    /// Incremental progress on an in-flight activity. `text` is
    /// kind-specific — model output for `ModelTurn`, stdout for `ToolCall`,
    /// progress messages for `SubAgent`. Empty `text` is a heartbeat.
    ActivityProgress {
        id: ActivityId,
        text: Option<String>,
    },

    /// Activity completed.
    ActivityCompleted {
        id: ActivityId,
        outcome: ActivityOutcome,
    },

    // ---- Conversation — user-visible messages ----
    /// A message in the peer's conversation. `partial: true` signals a
    /// streaming chunk; `false` signals a complete message (final or
    /// non-streaming). Streaming chunks share the same `id` so the
    /// renderer can assemble them.
    Message {
        id: MessageId,
        role: MessageRole,
        content: MessageContent,
        partial: bool,
    },

    // ---- Task delegation lifecycle ----
    /// Update for a task that was delegated *to* this peer (i.e. the
    /// federation coordinator initiated the work, peer is reporting back).
    /// Distinct from `ActivityStarted`/etc. which are the peer's own
    /// internal activities — task updates are scoped to delegated work.
    TaskUpdate {
        task: TaskId,
        update: TaskUpdate,
    },

    // ---- Approval flow (federated) ----
    /// Peer wants to do something that requires approval. May be forwarded
    /// to a human via the local presence layer or auto-resolved by policy.
    ApprovalRequested {
        request: ApprovalRequest,
    },

    /// Approval was resolved (locally or remotely). Echoed so observers
    /// can update UI consistently regardless of which side made the call.
    ApprovalResolved {
        request_id: String,
        decision: ApprovalDecision,
    },

    // ---- Capability state ----
    /// Peer engaged a capability (started using its display, opened a
    /// voice session, started recording, picked up a chat channel). The
    /// typed replacement for `AppEvent::DisplayTaken` / `RecordingStarted`
    /// / `PresenceConnected` and OpenClaw's analogous events. `detail` is
    /// capability-specific structured data.
    CapabilityEngaged {
        capability: Capability,
        detail: serde_json::Value,
    },

    /// Peer released a capability. `reason` is optional structured context
    /// (e.g. `Some("capture_lost")` for an involuntary release).
    CapabilityReleased {
        capability: Capability,
        reason: Option<String>,
    },

    // ---- Resource accounting ----
    Usage {
        snapshot: UsageSnapshot,
    },

    // ---- Session lifecycle ----
    SessionStarted {
        session: SessionInfo,
    },
    SessionEnded {
        session_id: String,
        reason: String,
    },

    // ---- Structured log line ----
    /// Levelled, sourced log entry. Replaces `AppEvent::LogEntry`,
    /// `PresenceLog`, `VoiceLog`, `OrchestratorLog`, `ContextManagement`.
    /// `source` is a free-form tag like `"orchestrator"` / `"voice"` /
    /// `"presence"` so the renderer can group/filter.
    Log {
        level: LogLevel,
        source: String,
        message: String,
        /// RFC3339 timestamp string, matching the existing session_log
        /// convention (see `web_gateway::replay_jsonl_to_outbound_entries`).
        ts: String,
    },

    // ---- Per-peer WebRTC signaling (display sharing) ----
    /// One leg of a WebRTC signaling exchange between the connecting
    /// browser and this peer. Routed by the federation transport so
    /// the browser can establish a *direct* WebRTC media path to the
    /// peer's display, with the primary acting as signaling middleman
    /// only — encoded video flows browser↔peer, never through primary
    /// (slice 3a). 3b adds a primary-relay fallback for the no-direct-
    /// path case.
    ///
    /// `display_id` identifies which of the peer's displays this signal
    /// pertains to; `session_id` is a browser-generated UUID that scopes
    /// the WebRTC session so multiple sessions to the same display
    /// don't collide and a stale session from a previous browser tab
    /// doesn't interfere with a fresh one.
    ///
    /// In the peer→primary direction, this carries the peer's `Answer`
    /// and trickled `IceCandidate`s. In the primary→peer direction
    /// (via [`crate::peer::traits::PeerOp::WebRtcSignal`]), it carries
    /// the browser's `Offer` and trickled `IceCandidate`s.
    ///
    /// Explicit `rename` because serde's default `rename_all = "snake_case"`
    /// mangles the "Rtc" acronym into `web_rtc_signal` — same class of
    /// bug as `PeerKind::A2A → a2_a`. Canonical wire name is
    /// `webrtc_signal` (no underscore in the acronym); the WASM
    /// dashboard's `render_peer_event` dispatches on this string.
    #[serde(rename = "webrtc_signal")]
    WebRtcSignal {
        display_id: u32,
        session_id: WebRtcSessionId,
        signal: WebRtcSignal,
    },
}

/// Browser-generated UUID identifying one WebRTC session to one peer's
/// one display. Newtype so it doesn't get confused with `MessageId` /
/// `TaskId` / `ActivityId` at type level.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WebRtcSessionId(pub String);

impl WebRtcSessionId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for WebRtcSessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// One leg of a WebRTC signaling exchange. Transport-neutral —
/// transports map this onto whatever wire frame they speak (the native
/// Intendant WebSocket transport maps to `ControlMsg::WebRtcSignal` in
/// one direction and consumes `OutboundEvent::WebRtcSignal` via the
/// upcaster in the other).
///
/// Forward-compat fallback variant `Unknown` so future signal kinds
/// don't fail the whole event parse on older builds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WebRtcSignal {
    /// Browser-side SDP offer. The browser is the offerer (mirrors
    /// the local browser→daemon WebRTC flow); peer creates a
    /// `WebRtcPeer` in response and emits an `Answer`.
    ///
    /// `advertise_tcp_via_url` is the browser's view of how to reach
    /// this peer's HTTP port — supplied by the dashboard as the URL
    /// the operator typed into "Add Peer" (stored as `d.ws_url` on
    /// the local daemon row). The peer derives its ICE-TCP host
    /// candidate address from this URL and registers against its own
    /// `TcpPeerRegistry` multiplex so the browser can form a TCP
    /// pair through whatever tunnel / port-forward / Tailscale path
    /// the operator already set up for the dashboard. `None` means
    /// UDP-only direct paths — the 3a baseline when the field was
    /// absent on the wire.
    ///
    /// Backward-compatible wire addition: `#[serde(default)]` so
    /// older daemons that don't set the field deserialize to `None`
    /// and fall through to the UDP-only path; `skip_serializing_if`
    /// so local code that sends `None` doesn't bloat the wire frame.
    Offer {
        sdp: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        advertise_tcp_via_url: Option<String>,
    },
    /// Peer-side SDP answer in response to an offer.
    Answer { sdp: String },
    /// Trickled ICE candidate. Either direction. The wire format is
    /// the JSON object the browser/server already exchanges over the
    /// local /ws path (`{"candidate": "...", "sdpMid": "...", ...}`)
    /// passed through verbatim — no transport-layer reinterpretation.
    IceCandidate { candidate_json: String },
    /// Browser-side close request. Tells the peer to tear down the
    /// `WebRtcPeer` for this session_id. Sent on tab unmount,
    /// explicit close button, or peer-disconnect cleanup.
    Close,
    /// Forward-compat fallback. Unknown signal kinds parse to this
    /// variant rather than failing; peer/browser ignore it.
    #[serde(other)]
    Unknown,
}

/// Operational status reported by a peer.
///
/// Deliberately scoped to *what the peer is doing*, not *whether the
/// connection is up*. Transport lifecycle lives on
/// [`crate::peer::handle::ConnectionState`] — the two are separate
/// concerns and separate watch channels on the handle. The dashboard
/// composes both: a peer can be `ConnectionState::Reconnecting` while
/// its last observed `PeerStatus` was `Working`.
///
/// Custom Serialize/Deserialize for forward-compat — unknown status
/// strings fall through to [`PeerStatus::Unknown`] rather than failing
/// the parent event parse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeerStatus {
    Idle,
    Working,
    NeedsApproval,
    Error,
    /// Forward-compat fallback for peer-reported statuses we don't
    /// recognize.
    Unknown,
}

impl PeerStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::NeedsApproval => "needs_approval",
            Self::Error => "error",
            Self::Unknown => "unknown",
        }
    }

    pub fn from_wire(s: &str) -> Self {
        match s {
            "idle" => Self::Idle,
            "working" => Self::Working,
            "needs_approval" => Self::NeedsApproval,
            "error" => Self::Error,
            _ => Self::Unknown,
        }
    }
}

impl Serialize for PeerStatus {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for PeerStatus {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = <String>::deserialize(d)?;
        Ok(Self::from_wire(&s))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ActivityId(pub String);

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MessageId(pub String);

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskId(pub String);

/// What kind of activity a peer is doing.
///
/// Custom Serialize/Deserialize for forward-compat — a future peer
/// that starts emitting `"background_reflection"` activities parses
/// cleanly as [`ActivityKind::Unknown`] on older builds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivityKind {
    /// A model turn (request → streamed response → completion).
    ModelTurn,
    /// A tool / command execution.
    ToolCall,
    /// A sub-agent run.
    SubAgent,
    /// A task this peer is executing on behalf of a delegating peer.
    DelegatedTask,
    /// Custom or transport-specific activity kind (peer's explicit
    /// "I'm doing something not in the standard vocabulary" signal).
    Other,
    /// Forward-compat fallback for activity kinds we don't recognize.
    Unknown,
}

impl ActivityKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ModelTurn => "model_turn",
            Self::ToolCall => "tool_call",
            Self::SubAgent => "sub_agent",
            Self::DelegatedTask => "delegated_task",
            Self::Other => "other",
            Self::Unknown => "unknown",
        }
    }

    pub fn from_wire(s: &str) -> Self {
        match s {
            "model_turn" => Self::ModelTurn,
            "tool_call" => Self::ToolCall,
            "sub_agent" => Self::SubAgent,
            "delegated_task" => Self::DelegatedTask,
            "other" => Self::Other,
            _ => Self::Unknown,
        }
    }
}

impl Serialize for ActivityKind {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ActivityKind {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = <String>::deserialize(d)?;
        Ok(Self::from_wire(&s))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ActivityOutcome {
    Success,
    Failed {
        message: String,
    },
    Cancelled,
    /// Activity was paused mid-flight (e.g. waiting on an approval, or
    /// hit a budget cap that requires human resolution).
    Suspended {
        reason: String,
    },
    /// Forward-compat fallback for outcomes we don't recognize.
    /// Cannot be serialized.
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub enum TaskUpdate {
    Accepted,
    Progress {
        pct: Option<f32>,
        message: Option<String>,
    },
    Completed {
        result: serde_json::Value,
    },
    Failed {
        message: String,
    },
    Cancelled,
    /// Forward-compat fallback for task update stages we don't
    /// recognize. Cannot be serialized.
    #[serde(other)]
    Unknown,
}

/// Role of a message in a conversation. Deliberately kept closed —
/// user/assistant/system/tool is the cross-ecosystem stable vocabulary
/// (OpenAI, Anthropic, etc.) and won't evolve on our timescale.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
    System,
    Tool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessageContent {
    Text {
        text: String,
    },
    /// Reasoning / chain-of-thought trace from a model that emits one.
    Reasoning {
        text: String,
    },
    /// Image attachment.
    Image {
        mime_type: String,
        base64: String,
    },
    /// Multi-part content (mix of text + images + tool calls).
    Parts {
        parts: Vec<MessagePart>,
    },
    /// Forward-compat fallback for message content types we don't
    /// recognize. Cannot be serialized.
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessagePart {
    Text {
        text: String,
    },
    Image {
        mime_type: String,
        base64: String,
    },
    ToolCall {
        name: String,
        args: serde_json::Value,
    },
    ToolResult {
        name: String,
        result: serde_json::Value,
    },
    /// Forward-compat fallback for message part types we don't
    /// recognize. Cannot be serialized.
    #[serde(other)]
    Unknown,
}

/// A message to send *to* a peer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PeerMessage {
    /// Optional session/thread to scope the message to. If `None`, the
    /// transport picks the default (peer's current session, or starts a
    /// new one).
    pub session: Option<String>,
    pub role: MessageRole,
    pub content: MessageContent,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub request_id: String,
    /// Free-form category tag — `"command"`, `"file_change"`,
    /// `"human_question"`, etc. Free-form because peer kinds have
    /// different category sets and a closed enum would either bloat
    /// or leak details.
    pub category: String,
    /// Human-readable preview of what's being approved (e.g. the command
    /// line, the file diff, the question).
    pub preview: String,
    /// Whether local autonomy policy is allowed to auto-resolve this.
    pub auto_resolvable: bool,
}

/// Re-export of the shared approval decision type. The canonical
/// definition lives in [`crate::approval`] — both this module and
/// `external_agent` consume it so the four-way vocabulary stays in
/// exactly one place. Deliberately closed (no Unknown fallback):
/// cross-ecosystem stable, any other wire value is a bug worth
/// failing on.
pub use crate::approval::ApprovalDecision;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageSnapshot {
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub tokens_cached: u64,
    pub cost_usd: Option<f64>,
    /// Optional per-model breakdown.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub by_model: Vec<ModelUsage>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelUsage {
    pub provider: String,
    pub model: String,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost_usd: Option<f64>,
}

/// Log level. Deliberately kept closed — trace/debug/info/warn/error
/// is the RFC 5424-adjacent stable vocabulary used by every logging
/// ecosystem. New levels won't appear in peer feeds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub label: Option<String>,
    /// RFC3339 timestamp string.
    pub started_at: String,
}

/// `PeerEvent` tagged with the originating `PeerId` and a per-peer
/// monotonic sequence number. Produced by the registry; consumed by the
/// dashboard renderer and the session log. The inner event lives under
/// `payload` (not `event`) so the wire JSON doesn't have two `event`
/// keys at different nesting levels.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaggedPeerEvent {
    pub peer: PeerId,
    pub payload: PeerEvent,
    pub seq: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_serde_round_trip() {
        let evt = PeerEvent::Message {
            id: MessageId("msg-1".into()),
            role: MessageRole::Assistant,
            content: MessageContent::Text {
                text: "hello".into(),
            },
            partial: false,
        };
        let json = serde_json::to_string(&evt).unwrap();
        let parsed: PeerEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            PeerEvent::Message { content, .. } => match content {
                MessageContent::Text { text } => assert_eq!(text, "hello"),
                _ => panic!("wrong content variant"),
            },
            _ => panic!("wrong event variant"),
        }
    }

    #[test]
    fn capability_engaged_carries_detail() {
        let evt = PeerEvent::CapabilityEngaged {
            capability: Capability::Display,
            detail: serde_json::json!({"display_id": ":99", "resolution": "1920x1080"}),
        };
        let json = serde_json::to_string(&evt).unwrap();
        let parsed: PeerEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            PeerEvent::CapabilityEngaged { detail, .. } => {
                assert_eq!(detail["display_id"], ":99");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn activity_lifecycle_round_trip() {
        let id = ActivityId("act-1".into());
        let started = PeerEvent::ActivityStarted {
            id: id.clone(),
            kind: ActivityKind::ModelTurn,
            label: "turn 7".into(),
        };
        let progress = PeerEvent::ActivityProgress {
            id: id.clone(),
            text: Some("partial response".into()),
        };
        let completed = PeerEvent::ActivityCompleted {
            id,
            outcome: ActivityOutcome::Success,
        };
        for evt in [started, progress, completed] {
            let json = serde_json::to_string(&evt).unwrap();
            let _: PeerEvent = serde_json::from_str(&json).unwrap();
        }
    }

    #[test]
    fn tagged_event_carries_peer_and_seq() {
        use crate::peer::id::PeerKind;
        let tagged = TaggedPeerEvent {
            peer: PeerId::new(PeerKind::Intendant, "nicks-mac"),
            payload: PeerEvent::StatusChanged {
                status: PeerStatus::Working,
            },
            seq: 42,
        };
        let json = serde_json::to_string(&tagged).unwrap();
        let parsed: TaggedPeerEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.seq, 42);
        assert_eq!(parsed.peer.as_str(), "intendant:nicks-mac");
    }

    // ---- Forward-compat tests ----

    /// `PeerStatus` with an unknown string must fall through to
    /// `Unknown`, not fail the parent event parse.
    #[test]
    fn unknown_peer_status_parses() {
        let evt_json = r#"{"event": "status_changed", "status": "meditating"}"#;
        let parsed: PeerEvent = serde_json::from_str(evt_json).unwrap();
        match parsed {
            PeerEvent::StatusChanged { status } => {
                assert_eq!(status, PeerStatus::Unknown);
            }
            _ => panic!("expected StatusChanged"),
        }
    }

    /// `ActivityKind` with an unknown string must fall through to
    /// `Unknown`.
    #[test]
    fn unknown_activity_kind_parses() {
        let evt_json = r#"{
            "event": "activity_started",
            "id": "a1",
            "kind": "background_reflection",
            "label": "thinking"
        }"#;
        let parsed: PeerEvent = serde_json::from_str(evt_json).unwrap();
        match parsed {
            PeerEvent::ActivityStarted { kind, .. } => {
                assert_eq!(kind, ActivityKind::Unknown);
            }
            _ => panic!("expected ActivityStarted"),
        }
    }

    /// `ActivityOutcome` with an unknown `status` tag must fall
    /// through to `Unknown`.
    #[test]
    fn unknown_activity_outcome_parses() {
        let evt_json = r#"{
            "event": "activity_completed",
            "id": "a1",
            "outcome": { "status": "partially_completed", "info": "..." }
        }"#;
        let parsed: PeerEvent = serde_json::from_str(evt_json).unwrap();
        match parsed {
            PeerEvent::ActivityCompleted { outcome, .. } => {
                assert!(matches!(outcome, ActivityOutcome::Unknown));
            }
            _ => panic!("expected ActivityCompleted"),
        }
    }

    /// `TaskUpdate` with an unknown `stage` tag must fall through to
    /// `Unknown`.
    #[test]
    fn unknown_task_update_stage_parses() {
        let evt_json = r#"{
            "event": "task_update",
            "task": "t1",
            "update": { "stage": "queued_behind_other_task" }
        }"#;
        let parsed: PeerEvent = serde_json::from_str(evt_json).unwrap();
        match parsed {
            PeerEvent::TaskUpdate { update, .. } => {
                assert!(matches!(update, TaskUpdate::Unknown));
            }
            _ => panic!("expected TaskUpdate"),
        }
    }

    /// `MessageContent` with an unknown `type` tag must fall through
    /// to `Unknown`, preserving the rest of the parent Message.
    #[test]
    fn unknown_message_content_parses() {
        let evt_json = r#"{
            "event": "message",
            "id": "m1",
            "role": "assistant",
            "content": { "type": "holographic", "data": "..." },
            "partial": false
        }"#;
        let parsed: PeerEvent = serde_json::from_str(evt_json).unwrap();
        match parsed {
            PeerEvent::Message {
                id,
                role,
                content,
                partial,
            } => {
                assert_eq!(id, MessageId("m1".into()));
                assert_eq!(role, MessageRole::Assistant);
                assert!(!partial);
                assert!(matches!(content, MessageContent::Unknown));
            }
            _ => panic!("expected Message"),
        }
    }

    /// Wire-format consistency for the custom-Serialize unit enums in
    /// this module — as_str() must match what serde produces.
    #[test]
    fn unit_enums_as_str_matches_serde_wire_format() {
        for s in [
            PeerStatus::Idle,
            PeerStatus::Working,
            PeerStatus::NeedsApproval,
            PeerStatus::Error,
            PeerStatus::Unknown,
        ] {
            let wire = serde_json::to_string(&s).unwrap();
            assert_eq!(wire, format!("\"{}\"", s.as_str()));
        }
        for k in [
            ActivityKind::ModelTurn,
            ActivityKind::ToolCall,
            ActivityKind::SubAgent,
            ActivityKind::DelegatedTask,
            ActivityKind::Other,
            ActivityKind::Unknown,
        ] {
            let wire = serde_json::to_string(&k).unwrap();
            assert_eq!(wire, format!("\"{}\"", k.as_str()));
        }
    }

    /// WebRtcSignal::Offer with `advertise_tcp_via_url = Some(...)`
    /// round-trips through serde with the URL preserved.
    #[test]
    fn webrtc_offer_round_trips_with_tcp_via_url() {
        let sig = WebRtcSignal::Offer {
            sdp: "v=0\r\n".into(),
            advertise_tcp_via_url: Some("ws://localhost:8766/ws".into()),
        };
        let json = serde_json::to_string(&sig).unwrap();
        assert!(
            json.contains("advertise_tcp_via_url"),
            "field should be on wire when Some: {json}"
        );
        let parsed: WebRtcSignal = serde_json::from_str(&json).unwrap();
        match parsed {
            WebRtcSignal::Offer {
                advertise_tcp_via_url,
                ..
            } => {
                assert_eq!(
                    advertise_tcp_via_url.as_deref(),
                    Some("ws://localhost:8766/ws")
                );
            }
            other => panic!("expected Offer, got {other:?}"),
        }
    }

    /// When the URL hint is `None`, the field is skipped on the wire
    /// (`skip_serializing_if`) so older peers / minimal clients that
    /// predate 3a.2 don't see an unexpected null.
    #[test]
    fn webrtc_offer_skips_tcp_via_url_when_none() {
        let sig = WebRtcSignal::Offer {
            sdp: "v=0\r\n".into(),
            advertise_tcp_via_url: None,
        };
        let json = serde_json::to_string(&sig).unwrap();
        assert!(
            !json.contains("advertise_tcp_via_url"),
            "field should be omitted when None: {json}"
        );
    }

    /// Deserializing an offer from a pre-3a.2 producer (no
    /// `advertise_tcp_via_url` field at all) succeeds and parses the
    /// URL as `None` — backward-compatible wire addition.
    #[test]
    fn webrtc_offer_deserializes_without_tcp_via_url() {
        let json = r#"{"kind":"offer","sdp":"v=0\r\n"}"#;
        let parsed: WebRtcSignal = serde_json::from_str(json).unwrap();
        match parsed {
            WebRtcSignal::Offer {
                advertise_tcp_via_url,
                sdp,
            } => {
                assert_eq!(advertise_tcp_via_url, None);
                assert_eq!(sdp, "v=0\r\n");
            }
            other => panic!("expected Offer, got {other:?}"),
        }
    }
}
