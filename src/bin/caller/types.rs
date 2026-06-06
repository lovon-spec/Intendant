//! Shared type definitions used across all frontends (TUI, MCP, control socket, web gateway).
//!
//! These types were extracted from `tui/app.rs` and `control.rs` so that non-TUI
//! modules no longer need to reach into `tui::` for shared vocabulary.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Agent loop phases
// ---------------------------------------------------------------------------

/// Current phase of the agent loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    Thinking,
    RunningAgent,
    Orchestrating,
    WaitingApproval,
    WaitingHuman,
    WaitingFollowUp,
    Idle,
    Done,
    /// Transient state while an interrupt is being processed.
    Interrupting,
    /// The turn was interrupted by the user.
    Interrupted,
}

// ---------------------------------------------------------------------------
// Log levels and verbosity
// ---------------------------------------------------------------------------

/// Log entry severity / source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Model,
    Agent,
    Error,
    Warn,
    SubAgent,
    /// Operational detail — visible at Verbose and Debug, hidden at Normal.
    /// Use for token counts, auto-approved commands, presence lifecycle, etc.
    Detail,
    Debug,
}

/// Log verbosity profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verbosity {
    Quiet,
    Normal,
    Verbose,
    Debug,
}

impl Verbosity {
    pub fn next(self) -> Self {
        match self {
            Self::Quiet => Self::Normal,
            Self::Normal => Self::Verbose,
            Self::Verbose => Self::Debug,
            Self::Debug => Self::Quiet,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Quiet => "Quiet",
            Self::Normal => "Normal",
            Self::Verbose => "Verbose",
            Self::Debug => "Debug",
        }
    }

    pub fn includes(self, level: &LogLevel) -> bool {
        match self {
            Self::Quiet => matches!(level, LogLevel::Warn | LogLevel::Error),
            Self::Normal => matches!(
                level,
                LogLevel::Info
                    | LogLevel::Model
                    | LogLevel::Agent
                    | LogLevel::Warn
                    | LogLevel::Error
                    | LogLevel::SubAgent
            ),
            Self::Verbose => !matches!(level, LogLevel::Debug),
            Self::Debug => true,
        }
    }

    /// Short indicator shown in log panel for each verbosity level.
    pub fn hint(self) -> &'static str {
        match self {
            Self::Quiet => "Warn+Error only",
            Self::Normal => "Key events",
            Self::Verbose => "+detail, agent output",
            Self::Debug => "+raw model/JSON",
        }
    }
}

// ---------------------------------------------------------------------------
// Outbound events (control socket / web gateway / MCP)
// ---------------------------------------------------------------------------

/// Per-session frontend affordances advertised by the controller.
/// Missing capabilities mean the frontend should keep its legacy defaults.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionCapabilities {
    #[serde(default)]
    pub follow_up: bool,
    #[serde(default)]
    pub steer: bool,
    #[serde(default)]
    pub interrupt: bool,
    #[serde(default)]
    pub codex_thread_actions: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_managed_context: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_context_archive: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_command: Option<String>,
    /// Session-scoped Codex service-tier state. `Some(false)` is serialized so
    /// frontends can distinguish a known normal tier from unknown old replay
    /// data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_fast_mode: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_service_tier: Option<String>,
}

/// Per-session Codex `/goal` state shown by the dashboard.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionGoal {
    pub objective: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_used: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<u64>,
}

/// Normalized region in a shared display view.
///
/// Coordinates are fractions of the visible display frame, where `(0, 0)` is
/// the top-left and `(1, 1)` is the bottom-right. The dashboard renders this as
/// a focus box over the video stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedViewRegion {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Events sent to connected control socket clients, web gateway, and MCP.
///
/// Also deserialized by `crate::peer::upcast::OutboundEventUpcaster`
/// when reading a peer Intendant's `/ws` stream. `Unknown` is the
/// forward-compat fallback — a peer running a newer build that
/// emits an event variant we don't recognize parses to `Unknown`
/// and is dropped by the upcaster rather than failing the whole
/// wire parse. As with the other `#[serde(other)]` variants in the
/// peer module, `Unknown` cannot be *serialized* at runtime; local
/// code never constructs it, so that limitation doesn't matter.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
#[allow(dead_code)]
pub enum OutboundEvent {
    TurnStarted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        turn: usize,
        budget_pct: f64,
    },
    AgentOutput {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        stdout: String,
        stderr: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        source: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        output_id: Option<String>,
    },
    ApprovalRequired {
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        id: u64,
        command: String,
    },
    AskHuman {
        question: String,
    },
    TaskComplete {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        reason: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
    },
    SessionStarted {
        session_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        task: Option<String>,
    },
    SessionIdentity {
        session_id: String,
        source: String,
        backend_session_id: String,
    },
    SessionRelationship {
        parent_session_id: String,
        child_session_id: String,
        relationship: String,
        ephemeral: bool,
    },
    SessionCapabilities {
        session_id: String,
        capabilities: SessionCapabilities,
    },
    SessionGoal {
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        goal: Option<SessionGoal>,
    },
    SessionAttached {
        session_id: String,
        source: String,
    },
    SessionEnded {
        session_id: String,
        reason: String,
    },
    RoundComplete {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        round: usize,
        turns_in_round: usize,
    },
    DebugScreenReady {
        display_id: u32,
    },
    DebugScreenTornDown {
        display_id: u32,
    },
    DisplayReady {
        display_id: u32,
        width: u32,
        height: u32,
    },
    DisplayResize {
        display_id: u32,
        width: u32,
        height: u32,
    },
    DisplayTaken {
        display_id: u32,
    },
    DisplayReleased {
        display_id: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    UserDisplayGranted,
    UserDisplayRevoked {
        display_id: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    DisplayCaptureLost {
        display_id: u32,
        reason: String,
    },
    DisplayApprovalPending {
        display_id: u32,
        backend: String,
    },
    SharedView {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        action: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_target: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_id: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        region: Option<SharedViewRegion>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    BrowserWorkspaceChanged {
        kind: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<crate::browser_workspace::BrowserWorkspace>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    RecordingStarted {
        stream_name: String,
    },
    RecordingStopped {
        stream_name: String,
    },
    RecordingDeleted {
        stream_name: String,
    },
    RecordingError {
        stream_name: String,
        message: String,
    },
    Status {
        turn: usize,
        phase: String,
        autonomy: String,
        session_id: String,
        task: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        external_agent: Option<String>,
    },
    ExternalAgentChanged {
        #[serde(skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
    },
    AutonomyChanged {
        autonomy: String,
    },
    /// Delivered to browsers when a Codex thread-level action finishes
    /// (compact, fork, side, side-close, rollback, review, rename, goal, init, memory-reset).
    /// `success` + `message` are surfaced as a dashboard toast and logged.
    CodexThreadActionResult {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        action: String,
        success: bool,
        message: String,
    },
    SessionRenameResult {
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        success: bool,
        message: String,
    },
    SessionAgentConfigResult {
        session_id: String,
        source: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        backend_session_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        intendant_session_id: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        persisted_session_ids: Vec<String>,
        success: bool,
        message: String,
    },
    CodexConfigChanged {
        #[serde(skip_serializing_if = "Option::is_none")]
        command: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        sandbox: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        approval_policy: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// True when the message is "clear the model override" (the dashboard
        /// uses an empty input to mean that). Distinguishes from "no change
        /// to model" (which omits the field entirely).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        model_cleared: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning_effort: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        reasoning_effort_cleared: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        service_tier: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        service_tier_cleared: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        web_search: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        network_access: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        writable_roots: Option<Vec<String>>,
        #[serde(skip_serializing_if = "Option::is_none", alias = "context_recovery")]
        managed_context: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        context_archive: Option<String>,
    },
    /// Mirror of `CodexConfigChanged` for the Gemini CLI backend. Fields
    /// omitted (or `Option::None`) mean "no change since the last emission".
    /// See `ControlMsg::SetGemini*` variants for the write side.
    GeminiConfigChanged {
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        model_cleared: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        approval_mode: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        sandbox: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        extensions: Option<Vec<String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        allowed_mcp_servers: Option<Vec<String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        include_directories: Option<Vec<String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        debug: Option<bool>,
    },
    /// Result of a Gemini thread action (currently just `"new"`). Mirror
    /// of `CodexThreadActionResult` so the dashboard can reuse its toast
    /// path.
    GeminiThreadActionResult {
        action: String,
        success: bool,
        message: String,
    },
    Usage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        main: crate::frontend::ModelUsageSnapshot,
        #[serde(skip_serializing_if = "Option::is_none")]
        presence: Option<crate::frontend::ModelUsageSnapshot>,
    },
    UsageUpdate {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        main: crate::frontend::ModelUsageSnapshot,
        #[serde(skip_serializing_if = "Option::is_none")]
        presence: Option<crate::frontend::ModelUsageSnapshot>,
    },
    ContextSnapshot {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        source: String,
        label: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        request_index: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        turn: Option<usize>,
        format: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        token_count: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        token_count_kind: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        context_window: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        hard_context_window: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        item_count: Option<usize>,
        raw: serde_json::Value,
    },
    CommandResult {
        action: String,
        ok: bool,
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        data: Option<serde_json::Value>,
    },
    UserTranscript {
        text: String,
        seq: u64,
    },
    ModelSummary {
        turn: usize,
        summary: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning_summary: Option<String>,
    },
    // --- New variants for broadcast decoupling ---
    ModelResponseDelta {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        text: String,
    },
    AgentStarted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        turn: usize,
        commands_preview: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item_id: Option<String>,
        /// When set, overrides the default "agent"/"Run" source label (e.g. "Codex").
        #[serde(skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
    DoneSignal {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    AutoApproved {
        preview: String,
    },
    ApprovalResolved {
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        id: u64,
        action: String,
    },
    ContextManagement {
        turn: usize,
    },
    BudgetWarning {
        pct: f64,
        remaining: u64,
    },
    BudgetExhausted {
        remaining: u64,
    },
    LoopError {
        message: String,
    },
    SubAgentResult {
        summary: String,
    },
    OrchestratorProgress {
        status: String,
    },
    ModelResponse {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        turn: usize,
        summary: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning_summary: Option<String>,
        /// When set, overrides the default "worker"/"Model" source label.
        #[serde(skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
    HumanResponseSent,
    SafetyCapReached,
    PresenceLog {
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        level: Option<String>,
    },
    PresenceUsageUpdate {
        total_tokens: u64,
        context_window: u64,
        usage_pct: f64,
        provider: String,
        model: String,
        #[serde(default)]
        prompt_tokens: u64,
        #[serde(default)]
        completion_tokens: u64,
        #[serde(default)]
        cached_tokens: u64,
    },
    LiveUsageUpdate {
        provider: String,
        model: String,
        input_tokens: u64,
        output_tokens: u64,
        cached_tokens: u64,
        total_tokens: u64,
        thinking_tokens: u64,
        #[serde(default)]
        input_text_tokens: u64,
        #[serde(default)]
        input_audio_tokens: u64,
        #[serde(default)]
        input_image_tokens: u64,
        #[serde(default)]
        cached_text_tokens: u64,
        #[serde(default)]
        cached_audio_tokens: u64,
        #[serde(default)]
        cached_image_tokens: u64,
        #[serde(default)]
        output_text_tokens: u64,
        #[serde(default)]
        output_audio_tokens: u64,
    },
    /// App-originated log entry broadcast to external consumers.
    LogEntry {
        level: String,
        source: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        turn: Option<usize>,
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        user_turn_index: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        user_turn_revision: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        replacement_for_user_turn_index: Option<u32>,
    },
    /// Live user-message edit rewound an active external-agent session.
    UserMessageRewind {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        user_turn_index: u32,
        turns_removed: u32,
    },
    /// Display transport pipeline metrics snapshot.
    DisplayMetrics {
        display_id: u32,
        capture_fps: f64,
        capture_drops: u64,
        encode_fps: f64,
        encode_freshness_avg_ms: f64,
        encode_drops: u64,
        peer_count: u64,
        peer_drops: u64,
        resolution_width: u32,
        resolution_height: u32,
        tile_damage_samples: u64,
        tile_dirty_rects: u64,
        tile_dirty_tiles: u64,
        tile_dirty_fraction_avg: f64,
        tile_delta_cadence_skips: u64,
        tile_delta_records: u64,
        tile_delta_fps: f64,
        tile_delta_kbps: f64,
        tile_snapshot_records: u64,
        tile_snapshot_frames: u64,
        tile_snapshot_kbps: f64,
    },
    FileChanged {
        path: String,
        kind: String,
        lines_added: u32,
        lines_removed: u32,
    },
    /// A user-uploaded file is available for attachment. Mirror of the
    /// `AppEvent::UploadReady` emitted after `POST /api/upload` finishes.
    UploadReady {
        descriptor: crate::upload_store::UploadDescriptor,
    },
    /// An uploaded file was removed from the store.
    UploadDeleted {
        id: String,
    },
    /// A new per-round file snapshot was recorded.
    SnapshotCreated {
        round_id: u64,
    },
    /// Project tree was rolled back to a prior round.
    RolledBack {
        from_id: u64,
        to_id: u64,
        files_reverted: u32,
    },
    /// `current_head_id` advanced forward along the linear history.
    Redone {
        to_id: u64,
    },
    /// Abandoned branches were pruned and orphaned blobs GC'd.
    HistoryPruned {
        branches_removed: u32,
        bytes_freed: u64,
    },
    /// The agent's conversation was rolled back to a specific round.
    /// Emitted after `POST /api/session/current/rollback` with
    /// `revert_conversation: true`. `backend` is one of "native",
    /// "codex", "claude-code", "gemini"; `method` is "truncated"
    /// (native / Codex `thread/rollback`) or "session-reset"
    /// (CC / Gemini re-init).
    ConversationRolledBack {
        round_id: u64,
        turns_removed: u32,
        backend: String,
        method: String,
    },
    InterruptRequested {
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
    },
    Interrupted {
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        reason: String,
    },
    /// Mid-turn steering was requested by a user; surfaced so external
    /// consumers (dashboard) can show a pending steer row or toast.
    SteerRequested {
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        text: String,
        id: String,
    },
    /// Native steering was accepted by the backend/runtime, but may still be
    /// waiting for the backend's next checkpoint before the model sees it.
    SteerAccepted {
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        id: String,
        reason: String,
    },
    /// Mid-turn steering could not be delivered natively and the steer text
    /// was queued for the next turn instead. Paired with a later
    /// `SteerDelivered { mid_turn: false }` once the queue drains.
    SteerQueued {
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        id: String,
        reason: String,
    },
    /// Steer was observed in the agent conversation — either through a native
    /// backend echo or as a follow-up injection at turn boundary.
    SteerDelivered {
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        id: String,
        mid_turn: bool,
    },
    /// Status for an ordinary follow-up that was queued because the target
    /// session was active but does not support native mid-turn steering.
    FollowUpStatus {
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        status: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    // --- Peer registry push events ---
    //
    // Emitted by the gateway translator that subscribes to
    // `PeerRegistry::subscribe()` and converts each `RegistryEvent`
    // into the corresponding wire shape. The dashboard uses these to
    // update peer rows in-place without polling `GET /api/peers`.
    /// A peer was added to the registry.
    PeerAdded {
        peer: crate::peer::PeerSnapshot,
    },
    /// A peer was removed from the registry. Carries only the id;
    /// the browser drops the matching row from its local list.
    PeerRemoved {
        id: String,
    },
    /// A peer's connection state, status, or card changed. Carries a
    /// fresh snapshot reflecting the new values; the browser replaces
    /// the matching row.
    PeerStateChanged {
        peer: crate::peer::PeerSnapshot,
    },
    /// A peer-emitted [`crate::peer::PeerEvent`] forwarded by the
    /// local registry's translator. Lets the dashboard subscribe to
    /// per-peer activity (logs, model output, approval requests,
    /// etc.) through the same primary `/ws` stream as registry
    /// state events — eliminating the need for per-secondary
    /// WebSocket plumbing in the browser once the UI side migrates.
    ///
    /// The inner field is named `payload` (not `event`) because
    /// `OutboundEvent`'s serde tag is also `"event"`, and a struct
    /// field with the same name would collide with the variant
    /// discriminator at the same JSON nesting level.
    PeerEventForwarded {
        peer_id: String,
        payload: crate::peer::PeerEvent,
    },
    /// One leg of a federation-driven WebRTC signaling exchange,
    /// emitted *by* this daemon back toward a connector. Carries the
    /// daemon's `Answer` (in response to a browser `Offer` routed via
    /// `ControlMsg::WebRtcSignal`) or trickled `IceCandidate`s. The
    /// connecting peer's transport ([`crate::peer::transport::IntendantWsTransport`])
    /// upcasts this into [`crate::peer::PeerEvent::WebRtcSignal`] so
    /// the primary's registry can forward it to the browser through
    /// the existing `PeerEventForwarded` path.
    ///
    /// `session_id` is the same browser-generated UUID that came in
    /// on the corresponding `ControlMsg::WebRtcSignal` — round-trips
    /// verbatim so the browser's per-session `RTCPeerConnection` can
    /// match incoming answers/candidates to the right peer connection.
    ///
    /// Explicit `rename` because serde's default `rename_all = "snake_case"`
    /// mangles "Rtc" into `web_rtc_signal`. Canonical wire name is
    /// `webrtc_signal`.
    #[serde(rename = "webrtc_signal")]
    WebRtcSignal {
        display_id: u32,
        session_id: String,
        signal: crate::peer::WebRtcSignal,
    },
    /// Forward-compat fallback for wire events we don't recognize.
    /// Produced only by the deserializer; never constructed locally.
    /// Cannot be serialized.
    #[serde(other)]
    Unknown,
}

// ---------------------------------------------------------------------------
// Utility functions
// ---------------------------------------------------------------------------

/// Truncate a string to a maximum byte length, respecting character boundaries.
pub fn truncate_str(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        &s[..end]
    }
}

/// Format a human-readable summary of a model's JSON response.
/// Extracts command functions and their key parameters (command strings, paths, etc.)
/// instead of showing raw JSON.
pub fn format_model_summary(content: &str) -> String {
    let parsed: serde_json::Value = match serde_json::from_str(content) {
        Ok(v) => v,
        Err(_) => {
            // Not valid JSON — return the full text for multi-line rendering.
            return content.to_string();
        }
    };

    let commands = match parsed.get("commands").and_then(|c| c.as_array()) {
        Some(cmds) if !cmds.is_empty() => cmds,
        _ => {
            if parsed
                .get("done")
                .and_then(|d| d.as_bool())
                .unwrap_or(false)
            {
                return "done signal".to_string();
            }
            return "no commands".to_string();
        }
    };

    let summaries: Vec<String> = commands
        .iter()
        .map(|cmd| {
            let func = cmd.get("function").and_then(|f| f.as_str()).unwrap_or("?");
            match func {
                "execAsAgent" => {
                    let command = cmd.get("command").and_then(|c| c.as_str()).unwrap_or("?");
                    let truncated = truncate_str(command, 120);
                    format!("exec: {}", truncated)
                }
                "editFile" => {
                    let path = cmd.get("file_path").and_then(|p| p.as_str()).unwrap_or("?");
                    let op = cmd.get("operation").and_then(|o| o.as_str()).unwrap_or("?");
                    format!("edit: {} ({})", path, op)
                }
                "inspectPath" => {
                    let path = cmd.get("path").and_then(|p| p.as_str()).unwrap_or("?");
                    format!("inspect: {}", path)
                }
                "browse" => {
                    let url = cmd.get("url").and_then(|u| u.as_str()).unwrap_or("?");
                    format!("browse: {}", truncate_str(url, 80))
                }
                "askHuman" => {
                    let q = cmd.get("question").and_then(|q| q.as_str()).unwrap_or("?");
                    format!("ask: {}", truncate_str(q, 100))
                }
                "storeMemory" => {
                    let key = cmd
                        .get("memory_key")
                        .and_then(|k| k.as_str())
                        .unwrap_or("?");
                    format!("store: {}", key)
                }
                "recallMemory" => {
                    let q = cmd
                        .get("memory_query")
                        .and_then(|q| q.as_str())
                        .unwrap_or("?");
                    format!("recall: {}", q)
                }
                "execPty" => {
                    let command = cmd.get("command").and_then(|c| c.as_str()).unwrap_or("?");
                    format!("pty: {}", truncate_str(command, 120))
                }
                _ => func.to_string(),
            }
        })
        .collect();

    summaries.join(" | ")
}
