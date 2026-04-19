//! Shared event infrastructure used across all frontends.
//!
//! `EventBus`, `AppEvent`, `ControlMsg`, and `ApprovalResponse` were extracted
//! from `tui/event.rs` so that non-TUI modules (MCP, control socket, web gateway,
//! presence) no longer depend on the `tui` module.

use crate::autonomy::ActionCategory;
use crate::provider::TokenUsage;
use crate::types::LogLevel;
use crossterm::event::KeyEvent;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};


/// Source of a context injection item.
///
/// Used by the agent loop to decide which queued injections to discard between
/// tasks.  System injections (display take/release notifications, etc.) are
/// purged when a new task starts so they don't pollute the next task's context.
/// User injections (annotations sent from the web dashboard) are preserved so
/// the agent always sees them, even if they were queued while idle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InjectionSource {
    /// Originated from a human action (annotation Send button, etc.).
    User,
    /// Generated automatically by the runtime (display grants, etc.).
    System,
}

/// A context injection item: text with optional images.
#[derive(Clone)]
pub struct ContextInjection {
    pub text: String,
    pub images: Vec<crate::conversation::ImageData>,
    pub source: InjectionSource,
    /// When this injection was queued by a fallback steer (the backend didn't
    /// support mid-turn steering), this carries the steer id so a
    /// `SteerDelivered` event can be emitted with the right correlation id
    /// when the item is eventually drained into the agent's conversation.
    ///
    /// `None` for non-steer injections (display take/release, presence
    /// annotations, etc.) — those don't participate in the steer protocol.
    pub steer_id: Option<String>,
}

impl ContextInjection {
    /// Create a text-only system injection (no images).
    pub fn text(msg: String) -> Self {
        Self {
            text: msg,
            images: vec![],
            source: InjectionSource::System,
            steer_id: None,
        }
    }

    /// Create a text-only user injection (no images).
    #[allow(dead_code)]
    pub fn user_text(msg: String) -> Self {
        Self {
            text: msg,
            images: vec![],
            source: InjectionSource::User,
            steer_id: None,
        }
    }

    /// Create a user injection that originated from a queued steer. The id
    /// round-trips back out via `AppEvent::SteerDelivered` when the item is
    /// drained so frontends can correlate their pending-steer UI.
    pub fn text_with_steer_id(msg: String, steer_id: String) -> Self {
        Self {
            text: msg,
            images: vec![],
            source: InjectionSource::User,
            steer_id: Some(steer_id),
        }
    }
}

/// Shared queue for context messages to inject into the agent conversation.
/// Used by display takeover/release to notify the agent between turns,
/// and by presence to inject mid-task interjections.
pub type ContextInjectionQueue = Arc<Mutex<Vec<ContextInjection>>>;

/// Shared registry for pending approval responders.
///
/// When the agent loop needs approval, it inserts a `oneshot::Sender` here
/// keyed by approval ID, then sends an `ApprovalRequired` event (without the
/// responder). Whichever frontend resolves the approval (TUI key press, web
/// button, control socket command) removes the sender and calls `.send()`.
pub type ApprovalRegistry =
    Arc<Mutex<HashMap<u64, tokio::sync::oneshot::Sender<ApprovalResponse>>>>;

/// All events flowing through the system.
#[derive(Debug, Clone)]
pub enum AppEvent {
    // Terminal input
    Key(KeyEvent),
    #[allow(dead_code)]
    Resize(u16, u16),

    // Agent loop lifecycle
    TurnStarted {
        turn: usize,
        budget_pct: f64,
        #[allow(dead_code)]
        remaining: u64,
    },
    ModelResponse {
        turn: usize,
        content: String,
        usage: TokenUsage,
        reasoning: Option<String>,
        /// Override source label in Activity tab (e.g. "Codex").
        #[allow(dead_code)]
        source: Option<String>,
    },
    /// Incremental text delta from streaming model response.
    ModelResponseDelta {
        text: String,
    },
    JsonExtracted {
        preview: String,
    },
    DoneSignal {
        message: Option<String>,
    },
    AgentStarted {
        turn: usize,
        commands_preview: String,
        source: Option<String>,
    },
    AgentOutput {
        stdout: String,
        stderr: String,
        source: Option<String>,
    },
    SubAgentResult {
        formatted: String,
    },
    OrchestratorProgress {
        turn: usize,
        status: String,
        last_action: String,
    },
    /// Detailed log entry from the orchestrator's session log (tailed by parent).
    OrchestratorLog {
        message: String,
        level: crate::types::LogLevel,
    },
    ContextManagement {
        turn: usize,
    },
    TaskComplete {
        reason: String,
        summary: Option<String>,
    },
    /// User requested interruption; broadcast to agent loops so they can cancel.
    InterruptRequested,
    /// The agent turn was interrupted. Emitted by the loop once cancellation completes.
    Interrupted {
        reason: String,
    },

    // ---- Mid-turn steering (interjection) ----
    /// User requested mid-turn steering. Re-emitted by the task dispatcher
    /// from `ControlMsg::Steer`. Agent loops subscribe to this and either
    /// call `ExternalAgent::steer_turn()` (for backends that support it) or
    /// fall back to queuing onto `context_injection`.
    ///
    /// `id` is always a String (empty = no correlation). This keeps
    /// downstream consumers simple — they never need to deal with a
    /// missing id, and comparing `id.is_empty()` is cheap.
    SteerRequested {
        text: String,
        id: String,
    },
    /// Mid-turn steering could not be delivered natively by the current
    /// backend and was queued as a follow-up injection instead. Emitted
    /// after the fallback push to `context_injection`; paired with a later
    /// `SteerDelivered { mid_turn: false }` once the queue drains.
    SteerQueued {
        id: String,
        /// Short human-readable explanation of why the queue fallback was
        /// used (e.g. "Claude Code doesn't support mid-turn steering;
        /// queued as follow-up").
        reason: String,
    },
    /// Mid-turn steering reached the agent. `mid_turn = true` means the
    /// backend injected it into the currently running turn (no queue
    /// fallback); `mid_turn = false` means a queued item was drained at
    /// turn boundary and delivered as part of the next user message.
    SteerDelivered {
        id: String,
        mid_turn: bool,
    },
    SessionStarted {
        session_id: String,
        task: Option<String>,
    },
    SessionEnded {
        session_id: String,
        reason: String,
    },
    DebugScreenReady {
        display_id: u32,
    },
    DebugScreenTornDown {
        display_id: u32,
    },
    BudgetWarning {
        pct: f64,
        remaining: u64,
    },
    BudgetExhausted {
        remaining: u64,
    },
    SafetyCapReached,
    LoopError(String),

    // askHuman
    HumanQuestionDetected {
        question: String,
    },
    HumanResponseSent,

    // Autonomy / approval
    ApprovalRequired {
        id: u64,
        command_preview: String,
        category: ActionCategory,
    },
    ApprovalResolved {
        id: u64,
        action: String,
    },

    // Vision display ready
    DisplayReady {
        display_id: u32,
        width: u32,
        height: u32,
    },

    /// Resolution changed on a live display (e.g. monitor mode switch, scale
    /// factor change).  Emitted by the capture bridge when incoming frame
    /// dimensions diverge from the current encoder dimensions.
    DisplayResize {
        display_id: u32,
        width: u32,
        height: u32,
    },

    // Display takeover
    DisplayTaken {
        display_id: u32,
    },
    DisplayReleased {
        display_id: u32,
        note: Option<String>,
    },

    // Display capture lost (backend crashed or portal session ended)
    DisplayCaptureLost {
        display_id: u32,
        reason: String,
    },

    /// The OS screen-share approval dialog has been raised on the guest
    /// desktop and we're waiting for the user to approve. The dashboard
    /// surfaces this as a banner so a remote user (e.g. via the web UI)
    /// knows to look at the physical display, not just sit on a blank
    /// video tab.  Cleared by `DisplayReady` (success) or
    /// `DisplayCaptureLost` (timeout / denial).
    DisplayApprovalPending {
        display_id: u32,
        backend: &'static str,
    },

    // User session display grant/revoke
    UserDisplayGranted {
        /// The display ID that was granted.  0 = primary (default).
        display_id: u32,
    },
    UserDisplayRevoked {
        /// The display ID being revoked.  0 = primary (default).
        display_id: u32,
        note: Option<String>,
    },

    // Recording lifecycle
    RecordingStarted {
        stream_name: String,
    },
    RecordingStopped {
        stream_name: String,
    },
    RecordingError {
        stream_name: String,
        message: String,
    },
    RecordingDeleted {
        stream_name: String,
    },

    // Session directory changed (MCP per-task isolation)
    SessionDirChanged {
        path: std::path::PathBuf,
    },

    // Control socket
    ControlCommand(ControlMsg),

    // Auto-approved command visibility
    AutoApproved {
        preview: String,
    },

    // Presence layer token usage update
    PresenceUsageUpdate {
        total_tokens: u64,
        context_window: u64,
        usage_pct: f64,
        provider: String,
        model: String,
        prompt_tokens: u64,
        completion_tokens: u64,
        cached_tokens: u64,
    },

    // Live model (Gemini Live / OpenAI Realtime) usage update from browser
    LiveUsageUpdate {
        provider: String,
        model: String,
        input_tokens: u64,
        output_tokens: u64,
        cached_tokens: u64,
        total_tokens: u64,
        thinking_tokens: u64,
    },

    /// Presence layer log message (shown in TUI log panel).
    /// `level` controls visibility: None defaults to Info.
    PresenceLog {
        message: String,
        #[allow(dead_code)]
        level: Option<LogLevel>,
        /// Presence interaction turn (for log grouping/collapse).
        turn: Option<usize>,
    },

    // Round lifecycle
    RoundComplete {
        round: usize,
        turns_in_round: usize,
        /// Length of the native `Conversation.messages` at the end of this
        /// round. Populated only when emitted from the native agent loop;
        /// the external-agent paths emit `None` because their conversation
        /// state lives inside the backend process (Codex thread, CC/Gemini
        /// session) rather than in a `Conversation` struct.
        ///
        /// The file watcher stores this on `HistoryRound.native_message_count`
        /// so a conversation-rollback request can truncate back to that
        /// length when rolling back to this round.
        native_message_count: Option<u32>,
    },

    /// Presence layer responded — switch to follow-up mode without logging
    /// a fake round completion. Emitted by the response forwarder after each
    /// presence narration so the user can type a follow-up.
    PresenceReady,

    /// Browser-side presence connected — server-side presence should pause.
    PresenceConnected {
        server_session_id: Option<String>,
        last_event_seq: u64,
        /// Live model provider name (e.g. "gemini", "openai").
        live_provider: Option<String>,
        /// Live model name (e.g. "gemini-2.5-flash-native-audio-preview-12-2025").
        live_model: Option<String>,
    },
    /// Browser-side presence disconnected — server-side presence should resume.
    PresenceDisconnected,
    /// Voice transcript log from browser presence model.
    VoiceLog {
        text: String,
        seq: u64,
        tool_context: Option<String>,
    },
    /// Context checkpoint received from browser presence model.
    PresenceCheckpointReceived {
        summary: String,
        last_event_seq: u64,
    },
    /// Diagnostic from browser voice/presence layer (errors, silence, disconnects).
    VoiceDiagnostic {
        kind: String,
        detail: String,
    },

    // Live audio sub-agent lifecycle
    LiveAudioStarted {
        id: String,
        provider: String,
    },
    LiveAudioProgress {
        id: String,
        state: String,
        elapsed_secs: f64,
        transcript_preview: String,
    },
    LiveAudioCompleted {
        id: String,
        status: String,
        quarantine_count: usize,
    },

    /// Server-side transcription of user speech (from Whisper API).
    UserTranscript {
        text: String,
        seq: u64,
    },

    /// Computed usage snapshot (emitted by TUI after accumulating tokens).
    UsageSnapshot {
        main: crate::frontend::ModelUsageSnapshot,
        presence: Option<crate::frontend::ModelUsageSnapshot>,
    },

    /// Proactive status broadcast (emitted on turn start, phase change, etc.)
    StatusUpdate {
        turn: usize,
        phase: String,
        autonomy: String,
        session_id: String,
        task: String,
    },

    /// Emitted when the external agent backend changes (via Settings dropdown).
    /// Broadcasted by the ControlPlane after updating shared state.
    ExternalAgentChanged {
        agent: Option<String>,
    },

    /// Emitted by the control plane when a `ControlMsg::CodexThreadAction`
    /// arrives, so the daemon-side action watcher (which owns the
    /// persistent agent) can pick it up. Carries the dispatched action and
    /// its params — one-way, no ack on this variant. The watcher emits a
    /// `CodexThreadActionResult` after the agent call returns.
    CodexThreadActionRequested {
        action: String,
        params: serde_json::Value,
    },

    /// Emitted by the daemon-side action watcher after a CodexThreadAction
    /// has been executed (or failed). `success=true` → message is an
    /// informational status line; `success=false` → message is the error
    /// surfaced to the caller. Dashboard consumes to flash a toast.
    CodexThreadActionResult {
        action: String,
        success: bool,
        message: String,
    },

    /// Emitted when one or more fields of the Codex runtime configuration
    /// change. Fields not included in the event are unchanged from the
    /// previous state. Broadcast by the control plane; consumed by the
    /// dashboard's Control sub-tab to refresh its displayed values.
    ///
    /// The `_cleared` booleans distinguish "unchanged" (field None, bool
    /// false) from "explicitly set to empty / use Codex default" (field
    /// None, bool true). `Option<Option<_>>` would be cleaner in Rust
    /// but doesn't round-trip through our JSON as obviously.
    CodexConfigChanged {
        sandbox: Option<String>,
        approval_policy: Option<String>,
        model: Option<String>,
        model_cleared: bool,
        reasoning_effort: Option<String>,
        reasoning_effort_cleared: bool,
        web_search: Option<bool>,
        network_access: Option<bool>,
        writable_roots: Option<Vec<String>>,
    },

    /// Emitted when one or more Gemini CLI runtime fields change. Mirror of
    /// `CodexConfigChanged` — fields omitted were not touched, `model_cleared`
    /// distinguishes "no change" from "model override removed".
    GeminiConfigChanged {
        model: Option<String>,
        model_cleared: bool,
        approval_mode: Option<String>,
        sandbox: Option<bool>,
        extensions: Option<Vec<String>>,
        allowed_mcp_servers: Option<Vec<String>>,
        include_directories: Option<Vec<String>>,
        debug: Option<bool>,
    },

    /// Emitted by the control plane when a `ControlMsg::GeminiThreadAction`
    /// arrives. Mirrors `CodexThreadActionRequested`. The outer loop in
    /// `run_with_presence` picks this up to dispatch daemon-side (`/new`)
    /// or agent-side (currently none; Gemini ACP doesn't expose them).
    GeminiThreadActionRequested {
        action: String,
        params: serde_json::Value,
    },

    /// Result of a Gemini thread action, emitted after the daemon-side
    /// watcher finishes dispatching. Carries the status back to the
    /// dashboard for a success/error toast.
    GeminiThreadActionResult {
        action: String,
        success: bool,
        message: String,
    },

    /// Log entry broadcast to external consumers (web UI, control socket).
    /// Emitted by the TUI's `log_sourced` for events without their own
    /// `OutboundEvent` variant, and by backend code (e.g.
    /// `emit_task_dispatched_log`) so dispatch-style messages reach external
    /// consumers in headless mode where no TUI is running.
    LogEntry {
        level: String,
        source: String,
        content: String,
        turn: Option<usize>,
    },

    /// Display transport pipeline metrics snapshot.
    DisplayMetrics {
        snapshot: crate::display::DisplayMetricsSnapshot,
    },

    /// A file in the project directory was created, modified, or deleted.
    FileChanged {
        path: String,
        kind: crate::file_watcher::FileChangeKind,
        lines_added: u32,
        lines_removed: u32,
    },

    // ---- File snapshot history (per-round) ----
    /// A new per-round snapshot was recorded.
    SnapshotCreated {
        round_id: u64,
    },
    /// The project tree was rolled back to a prior round.
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

    // ---- Conversation rollback ----
    /// Emitted by the web gateway's rollback handler when the user
    /// requested `revert_conversation: true`. The active agent loop
    /// (native via `run_agent_loop` / `run_with_presence`, or the
    /// external-agent drain) subscribes to the bus and handles this by
    /// truncating / session-resetting its conversation state, then
    /// emits `ConversationRolledBack` with the result.
    ///
    /// The handler looks up the round's stored `native_message_count`
    /// (from `HistoryRound`) and `turn_count`; both are passed through
    /// here so the consumer doesn't have to look them up again.
    ConversationRollbackRequested {
        /// The round we are rolling back to. Echoed through to the
        /// completion event for UI correlation.
        round_id: u64,
        /// For the native agent: truncate `Conversation.messages` to
        /// this length. `None` for rollbacks targeting external-agent
        /// rounds (those fall back to backend rollback or session reset).
        target_native_message_count: Option<u32>,
        /// Number of turns that occurred between the current head and
        /// the target round. Passed through to external-agent backends
        /// that accept a `turnsToRollback` parameter (Codex).
        turns_to_drop: u32,
    },

    /// Conversation was rolled back to a specific round.
    ConversationRolledBack {
        round_id: u64,
        /// Number of messages/turns removed from the conversation.
        /// Semantics are backend-dependent:
        ///   - native/Codex: turns dropped from the conversation tail.
        ///   - session-reset backends: best-effort estimate (the count
        ///     passed in `turns_to_drop`).
        turns_removed: u32,
        /// Which backend performed the rollback: `"native"`, `"codex"`,
        /// `"claude-code"`, or `"gemini"`.
        backend: String,
        /// How the rollback was performed: `"truncated"` for the native
        /// agent and Codex's `thread/rollback`, or `"session-reset"` for
        /// backends (CC, Gemini) that don't expose a protocol-level
        /// rollback and re-initialize the session from scratch.
        method: String,
    },

    // TUI internal
    Tick,
    #[allow(dead_code)]
    Quit,
}

/// Response from the approval system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalResponse {
    Approve,
    Skip,
    Deny,
    ApproveAll,
}

/// Commands received from the Unix control socket, MCP, and web gateway.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ControlMsg {
    Status,
    Usage,
    Approve {
        id: u64,
    },
    Deny {
        id: u64,
    },
    Skip {
        id: u64,
    },
    ApproveAll {
        id: u64,
    },
    Input {
        text: String,
    },
    SetAutonomy {
        level: String,
    },
    SetExternalAgent {
        #[serde(default)]
        agent: Option<String>,
    },
    /// Set the Codex sandbox mode. Applies to the NEXT task because Codex
    /// locks the sandbox at `thread/start`. Valid values match
    /// `codex --sandbox <MODE>`: `"read-only"`, `"workspace-write"`,
    /// `"danger-full-access"`.
    SetCodexSandbox {
        mode: String,
    },
    /// Set the Codex approval policy. Applies to the NEXT task. Valid
    /// values match `codex --ask-for-approval <POLICY>`: `"untrusted"`,
    /// `"on-request"`, `"never"`.
    SetCodexApprovalPolicy {
        policy: String,
    },
    /// Set the Codex model override. `None` (or a JSON null / missing) lets
    /// Codex pick its default; otherwise the string is passed as `-m <MODEL>`
    /// equivalent via `thread/start` params. Applies to the NEXT task.
    SetCodexModel {
        #[serde(default)]
        model: Option<String>,
    },
    /// Set Codex reasoning effort. `None` or missing = use the model's
    /// default. Values match `-c model_reasoning_effort=...`. Applies to
    /// the NEXT task.
    SetCodexReasoningEffort {
        #[serde(default)]
        effort: Option<String>,
    },
    /// Toggle the Responses API `web_search` tool for Codex.
    /// Maps to `codex --search`. Applies to the NEXT task.
    SetCodexWebSearch {
        enabled: bool,
    },
    /// Toggle outbound network access inside the `workspace-write` sandbox.
    /// Maps to `-c sandbox_workspace_write.network_access=true|false`.
    /// Ignored by `read-only` and `danger-full-access`. Applies to the
    /// NEXT task.
    SetCodexNetworkAccess {
        enabled: bool,
    },
    /// Replace the list of extra writable roots (paths beyond the project
    /// root that Codex's sandbox may also write to). Maps to `--add-dir`.
    /// Applies to the NEXT task.
    SetCodexWritableRoots {
        #[serde(default)]
        roots: Vec<String>,
    },
    /// Invoke one of Codex's thread-level actions against the persistent
    /// agent. Mirrors the raw-codex slash-command surface: `/new`, `/compact`,
    /// `/fork`, `/undo`, `/review`, `/init`, `/memory-reset`. Applies
    /// immediately (not "next task") because Codex's app-server accepts
    /// these as mid-session RPCs.
    ///
    /// `params` is a free-form JSON object whose shape depends on `op`:
    /// `/fork` accepts `{"name": "..."}`, `/undo` accepts `{"turns": N}`,
    /// `/review` accepts `{"prompt": "..."}`, the rest ignore it. Callers
    /// that don't need params may omit the field entirely.
    ///
    /// The variant's field is named `op` (not `action`) because ControlMsg's
    /// serde tag is already `action`, and nested fields can't share the tag.
    CodexThreadAction {
        op: String,
        #[serde(default)]
        params: serde_json::Value,
    },
    /// Set the Gemini model override. `None`/missing lets Gemini pick.
    /// Applies to the NEXT task because Gemini latches `--model` at
    /// process spawn.
    SetGeminiModel {
        #[serde(default)]
        model: Option<String>,
    },
    /// Set the Gemini approval mode. Matches `gemini --approval-mode`:
    /// `"default" | "auto_edit" | "yolo" | "plan"`. Applies to the NEXT task.
    SetGeminiApprovalMode {
        mode: String,
    },
    /// Toggle Gemini's `--sandbox` flag. Applies to the NEXT task.
    SetGeminiSandbox {
        enabled: bool,
    },
    /// Replace the list of Gemini extensions to enable (`--extensions`).
    /// Empty list means "use all installed extensions" (Gemini's default).
    /// Applies to the NEXT task.
    SetGeminiExtensions {
        #[serde(default)]
        extensions: Vec<String>,
    },
    /// Replace the list of allowed MCP server names
    /// (`--allowed-mcp-server-names`). Empty list = all servers allowed.
    /// Applies to the NEXT task.
    SetGeminiAllowedMcpServers {
        #[serde(default)]
        servers: Vec<String>,
    },
    /// Replace the list of extra workspace directories
    /// (`--include-directories`). Applies to the NEXT task.
    SetGeminiIncludeDirectories {
        #[serde(default)]
        directories: Vec<String>,
    },
    /// Toggle Gemini's `--debug` flag (opens DevTools console). Applies
    /// to the NEXT task.
    SetGeminiDebug {
        enabled: bool,
    },
    /// Invoke a Gemini session-level action. Currently only `"new"` is
    /// supported (tears down the persistent agent so the next task starts
    /// a fresh Gemini process). Mirrors the shape of `CodexThreadAction`
    /// so frontends can use the same pattern.
    GeminiThreadAction {
        op: String,
        #[serde(default)]
        params: serde_json::Value,
    },
    SetVerbosity {
        level: String,
    },
    ScheduleControllerRestart {
        controller_id: String,
        north_star_goal: String,
        #[serde(default)]
        reason: Option<String>,
        #[serde(default)]
        restart_after: Option<String>,
        #[serde(default)]
        restart_command: Option<String>,
        #[serde(default)]
        auto_start_task: Option<bool>,
        #[serde(default)]
        max_attempts: Option<u32>,
        #[serde(default)]
        cooldown_sec: Option<u64>,
    },
    ControllerTurnComplete {
        restart_id: String,
        turn_complete_token: String,
        #[serde(default)]
        status: Option<String>,
        #[serde(default)]
        handoff_summary: Option<String>,
    },
    GetRestartStatus,
    CancelControllerRestart {
        #[serde(default)]
        restart_id: Option<String>,
    },
    RequestControllerLoopHalt {
        #[serde(default)]
        persistent: Option<bool>,
    },
    ClearControllerLoopHalt,
    InterveneControllerLoop {
        mode: String,
    },
    GetControllerLoopStatus,
    StartTask {
        task: String,
        #[serde(default)]
        orchestrate: Option<bool>,
        /// When true, run in direct mode (no presence layer). Use for
        /// programmatic clients that submit tasks via WebSocket/control
        /// socket without a human on the other end.
        #[serde(default)]
        direct: Option<bool>,
        /// When present, routes to the ephemeral CU task runner instead of the
        /// regular agent loop.
        #[serde(default)]
        reference_frame_ids: Vec<String>,
        /// Explicit display target for CU actions (e.g. "user_session", ":99").
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_target: Option<String>,
        /// Frame IDs that the user attached to this task via the dashboard's
        /// "Attach" buttons (annotation toolbar, Video tab, clip toolbar).
        ///
        /// Resolved against the frame registry and prepended as image content
        /// to the first user message of the agent conversation. Works for both
        /// the internal agent (`add_user_with_images`) and external agents
        /// (Codex `LocalImage`, Gemini ACP `ContentBlock::Image`).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<String>,
    },
    FollowUp {
        text: String,
        /// When true, bypass the presence layer for this follow-up and
        /// dispatch it directly to the agent — mirrors `direct: true`
        /// on `StartTask`. Frontends set this when the user has the
        /// Direct toggle checked at the time of the follow-up. Absent
        /// (or `Some(false)`) means route through presence as before.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        direct: Option<bool>,
    },
    TakeDisplay {
        display_id: u32,
    },
    ReleaseDisplay {
        display_id: u32,
        #[serde(default)]
        note: Option<String>,
    },
    GrantUserDisplay {
        /// Optional display ID to grant.  When `None`, grants the primary
        /// display (id 0) -- backwards-compatible with single-monitor setups.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_id: Option<u32>,
    },
    RevokeUserDisplay {
        /// Optional display ID to revoke.  When `None`, revokes the primary
        /// display (id 0) -- backwards-compatible with single-monitor setups.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_id: Option<u32>,
        #[serde(default)]
        note: Option<String>,
    },
    ListDisplays,
    QueryDetail {
        scope: String,
        #[serde(default)]
        target: Option<String>,
    },
    RecallMemory {
        #[serde(default)]
        keywords: Option<Vec<String>>,
        #[serde(default)]
        tags: Option<Vec<String>>,
        #[serde(default)]
        channel: Option<String>,
    },
    InvokeSkill {
        skill_name: String,
        #[serde(default)]
        arguments: Option<String>,
    },
    Quit,
    SetupDebugScreen,
    TeardownDebugScreen,
    StartDebugRecording,
    StopDebugRecording,
    StartRecording {
        stream_name: String,
    },
    StopRecording {
        stream_name: String,
    },
    DeleteRecording {
        stream_name: String,
    },
    /// Request interruption of the current agent turn.
    Interrupt {
        /// Optional precondition: only interrupt if this turn id is active.
        /// When None, interrupts whatever is currently running.
        #[serde(default)]
        expected_turn: Option<u64>,
    },
    /// Mid-turn steering: nudge the currently running agent turn with new
    /// user text without interrupting it. Backends that support native
    /// mid-turn steering (Codex `turn/steer`) inject the text into the
    /// in-progress turn; backends that don't queue the text onto the
    /// shared `context_injection` queue so it's delivered at the start of
    /// the next turn.
    Steer {
        text: String,
        /// Optional correlation id supplied by the frontend. Frontends use
        /// it to track the lifecycle of one steer request across the
        /// `SteerRequested` → `SteerQueued`/`SteerDelivered` events. An
        /// empty string or missing id is treated as "no correlation" —
        /// the backend still honors the request, but frontends that didn't
        /// tag it can't match the delivery event back to a pending UI row.
        #[serde(default)]
        id: Option<String>,
    },
}

/// The event bus sender. Cloneable for use in multiple tasks.
///
/// Backed by a `broadcast::channel` so multiple consumers can subscribe
/// independently. Each subscriber gets its own copy of every event.
#[derive(Clone)]
pub struct EventBus {
    tx: tokio::sync::broadcast::Sender<AppEvent>,
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _) = tokio::sync::broadcast::channel(4096);
        Self { tx }
    }

    pub fn send(&self, event: AppEvent) {
        let _ = self.tx.send(event);
    }

    /// Create a new subscriber that receives all future events.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<AppEvent> {
        self.tx.subscribe()
    }
}

/// Convert an AppEvent to an OutboundEvent for external consumers.
/// Returns `None` for terminal-only events (Key, Resize, Tick, Quit, etc.)
/// that external consumers don't need.
pub fn app_event_to_outbound(event: &AppEvent) -> Option<crate::types::OutboundEvent> {
    use crate::types::OutboundEvent;

    match event {
        AppEvent::TurnStarted {
            turn, budget_pct, ..
        } => Some(OutboundEvent::TurnStarted {
            turn: *turn,
            budget_pct: *budget_pct,
        }),
        AppEvent::ModelResponse {
            turn,
            content,
            reasoning,
            source,
            ..
        } => {
            let summary = crate::types::format_model_summary(content);
            Some(OutboundEvent::ModelResponse {
                turn: *turn,
                summary,
                reasoning_summary: reasoning.clone(),
                source: source.clone(),
            })
        }
        AppEvent::ModelResponseDelta { text } => {
            Some(OutboundEvent::ModelResponseDelta { text: text.clone() })
        }
        AppEvent::AgentStarted {
            turn,
            commands_preview,
            source,
        } => Some(OutboundEvent::AgentStarted {
            turn: *turn,
            commands_preview: commands_preview.clone(),
            source: source.clone(),
        }),
        AppEvent::AgentOutput { stdout, stderr, source } => Some(OutboundEvent::AgentOutput {
            stdout: stdout.clone(),
            stderr: stderr.clone(),
            source: source.clone(),
        }),
        AppEvent::DoneSignal { message } => Some(OutboundEvent::DoneSignal {
            message: message.clone(),
        }),
        AppEvent::TaskComplete { reason, summary } => Some(OutboundEvent::TaskComplete {
            reason: reason.clone(),
            summary: summary.clone(),
        }),
        AppEvent::InterruptRequested => Some(OutboundEvent::InterruptRequested),
        AppEvent::Interrupted { reason } => Some(OutboundEvent::Interrupted {
            reason: reason.clone(),
        }),
        AppEvent::SteerRequested { text, id } => Some(OutboundEvent::SteerRequested {
            text: text.clone(),
            id: id.clone(),
        }),
        AppEvent::SteerQueued { id, reason } => Some(OutboundEvent::SteerQueued {
            id: id.clone(),
            reason: reason.clone(),
        }),
        AppEvent::SteerDelivered { id, mid_turn } => Some(OutboundEvent::SteerDelivered {
            id: id.clone(),
            mid_turn: *mid_turn,
        }),
        AppEvent::SessionStarted { session_id, task } => {
            Some(OutboundEvent::SessionStarted {
                session_id: session_id.clone(),
                task: task.clone(),
            })
        }
        AppEvent::SessionEnded { session_id, reason } => {
            Some(OutboundEvent::SessionEnded {
                session_id: session_id.clone(),
                reason: reason.clone(),
            })
        }
        AppEvent::DebugScreenReady { display_id } => {
            Some(OutboundEvent::DebugScreenReady {
                display_id: *display_id,
            })
        }
        AppEvent::DebugScreenTornDown { display_id } => {
            Some(OutboundEvent::DebugScreenTornDown {
                display_id: *display_id,
            })
        }
        AppEvent::ApprovalRequired {
            id,
            command_preview,
            ..
        } => Some(OutboundEvent::ApprovalRequired {
            id: *id,
            command: command_preview.clone(),
        }),
        AppEvent::AutoApproved { preview } => Some(OutboundEvent::AutoApproved {
            preview: preview.clone(),
        }),
        AppEvent::ApprovalResolved { id, action } => Some(OutboundEvent::ApprovalResolved {
            id: *id,
            action: action.clone(),
        }),
        AppEvent::HumanQuestionDetected { question } => Some(OutboundEvent::AskHuman {
            question: question.clone(),
        }),
        AppEvent::HumanResponseSent => Some(OutboundEvent::HumanResponseSent),
        AppEvent::RoundComplete {
            round,
            turns_in_round,
            ..
        } => Some(OutboundEvent::RoundComplete {
            round: *round,
            turns_in_round: *turns_in_round,
        }),
        AppEvent::DisplayReady {
            display_id,
            width,
            height,
        } => Some(OutboundEvent::DisplayReady {
            display_id: *display_id,
            width: *width,
            height: *height,
        }),
        AppEvent::DisplayResize {
            display_id,
            width,
            height,
        } => Some(OutboundEvent::DisplayResize {
            display_id: *display_id,
            width: *width,
            height: *height,
        }),
        AppEvent::DisplayTaken { display_id } => Some(OutboundEvent::DisplayTaken {
            display_id: *display_id,
        }),
        AppEvent::DisplayReleased { display_id, note } => {
            Some(OutboundEvent::DisplayReleased {
                display_id: *display_id,
                note: note.clone(),
            })
        }
        AppEvent::UserDisplayGranted { .. } => Some(OutboundEvent::UserDisplayGranted),
        AppEvent::UserDisplayRevoked { display_id, note } => Some(OutboundEvent::UserDisplayRevoked {
            display_id: *display_id,
            note: note.clone(),
        }),
        AppEvent::ContextManagement { turn } => Some(OutboundEvent::ContextManagement {
            turn: *turn,
        }),
        AppEvent::BudgetWarning { pct, remaining } => Some(OutboundEvent::BudgetWarning {
            pct: *pct,
            remaining: *remaining,
        }),
        AppEvent::BudgetExhausted { remaining } => Some(OutboundEvent::BudgetExhausted {
            remaining: *remaining,
        }),
        AppEvent::SafetyCapReached => Some(OutboundEvent::SafetyCapReached),
        AppEvent::LoopError(msg) => Some(OutboundEvent::LoopError {
            message: msg.clone(),
        }),
        AppEvent::SubAgentResult { formatted } => Some(OutboundEvent::SubAgentResult {
            summary: formatted.clone(),
        }),
        AppEvent::OrchestratorProgress { status, .. } => {
            Some(OutboundEvent::OrchestratorProgress {
                status: status.clone(),
            })
        }
        AppEvent::UserTranscript { text, seq } => Some(OutboundEvent::UserTranscript {
            text: text.clone(),
            seq: *seq,
        }),
        AppEvent::PresenceLog {
            message, level, ..
        } => Some(OutboundEvent::PresenceLog {
            message: message.clone(),
            level: level.as_ref().map(|l| crate::frontend::log_level_to_str(l).to_string()),
        }),
        AppEvent::PresenceUsageUpdate {
            total_tokens,
            context_window,
            usage_pct,
            provider,
            model,
            prompt_tokens,
            completion_tokens,
            cached_tokens,
        } => Some(OutboundEvent::PresenceUsageUpdate {
            total_tokens: *total_tokens,
            context_window: *context_window,
            usage_pct: *usage_pct,
            provider: provider.clone(),
            model: model.clone(),
            prompt_tokens: *prompt_tokens,
            completion_tokens: *completion_tokens,
            cached_tokens: *cached_tokens,
        }),
        AppEvent::LiveUsageUpdate {
            provider,
            model,
            input_tokens,
            output_tokens,
            cached_tokens,
            total_tokens,
            thinking_tokens,
        } => Some(OutboundEvent::LiveUsageUpdate {
            provider: provider.clone(),
            model: model.clone(),
            input_tokens: *input_tokens,
            output_tokens: *output_tokens,
            cached_tokens: *cached_tokens,
            total_tokens: *total_tokens,
            thinking_tokens: *thinking_tokens,
        }),
        AppEvent::UsageSnapshot { main, presence } => Some(OutboundEvent::UsageUpdate {
            main: main.clone(),
            presence: presence.clone(),
        }),
        AppEvent::StatusUpdate {
            turn,
            phase,
            autonomy,
            session_id,
            task,
        } => Some(OutboundEvent::Status {
            turn: *turn,
            phase: phase.clone(),
            autonomy: autonomy.clone(),
            session_id: session_id.clone(),
            task: task.clone(),
            external_agent: None,
        }),
        AppEvent::ExternalAgentChanged { agent } => Some(OutboundEvent::ExternalAgentChanged {
            agent: agent.clone(),
        }),
        AppEvent::CodexThreadActionResult {
            action,
            success,
            message,
        } => Some(OutboundEvent::CodexThreadActionResult {
            action: action.clone(),
            success: *success,
            message: message.clone(),
        }),
        // The "requested" half is server-internal (daemon action watcher
        // consumes it directly); browsers don't need it.
        AppEvent::CodexThreadActionRequested { .. } => None,
        AppEvent::CodexConfigChanged {
            sandbox,
            approval_policy,
            model,
            model_cleared,
            reasoning_effort,
            reasoning_effort_cleared,
            web_search,
            network_access,
            writable_roots,
        } => Some(OutboundEvent::CodexConfigChanged {
            sandbox: sandbox.clone(),
            approval_policy: approval_policy.clone(),
            model: model.clone(),
            model_cleared: *model_cleared,
            reasoning_effort: reasoning_effort.clone(),
            reasoning_effort_cleared: *reasoning_effort_cleared,
            web_search: *web_search,
            network_access: *network_access,
            writable_roots: writable_roots.clone(),
        }),
        AppEvent::GeminiConfigChanged {
            model,
            model_cleared,
            approval_mode,
            sandbox,
            extensions,
            allowed_mcp_servers,
            include_directories,
            debug,
        } => Some(OutboundEvent::GeminiConfigChanged {
            model: model.clone(),
            model_cleared: *model_cleared,
            approval_mode: approval_mode.clone(),
            sandbox: *sandbox,
            extensions: extensions.clone(),
            allowed_mcp_servers: allowed_mcp_servers.clone(),
            include_directories: include_directories.clone(),
            debug: *debug,
        }),
        AppEvent::GeminiThreadActionResult {
            action,
            success,
            message,
        } => Some(OutboundEvent::GeminiThreadActionResult {
            action: action.clone(),
            success: *success,
            message: message.clone(),
        }),
        AppEvent::GeminiThreadActionRequested { .. } => None,
        AppEvent::LogEntry {
            level,
            source,
            content,
            turn,
        } => Some(OutboundEvent::LogEntry {
            level: level.clone(),
            source: source.clone(),
            content: content.clone(),
            turn: *turn,
        }),
        AppEvent::RecordingStarted { stream_name } => Some(OutboundEvent::RecordingStarted {
            stream_name: stream_name.clone(),
        }),
        AppEvent::RecordingStopped { stream_name } => Some(OutboundEvent::RecordingStopped {
            stream_name: stream_name.clone(),
        }),
        AppEvent::RecordingDeleted { stream_name } => Some(OutboundEvent::RecordingDeleted {
            stream_name: stream_name.clone(),
        }),
        AppEvent::RecordingError {
            stream_name,
            message,
        } => Some(OutboundEvent::RecordingError {
            stream_name: stream_name.clone(),
            message: message.clone(),
        }),
        AppEvent::DisplayMetrics { snapshot } => Some(OutboundEvent::DisplayMetrics {
            display_id: snapshot.display_id,
            capture_fps: snapshot.capture_fps,
            capture_drops: snapshot.capture_drops,
            encode_fps: snapshot.encode_fps,
            encode_latency_avg_ms: snapshot.encode_latency_avg_ms,
            encode_drops: snapshot.encode_drops,
            peer_count: snapshot.peer_count,
            peer_drops: snapshot.peer_drops,
            resolution_width: snapshot.resolution.0,
            resolution_height: snapshot.resolution.1,
        }),
        AppEvent::DisplayCaptureLost { display_id, reason } => {
            Some(OutboundEvent::DisplayCaptureLost {
                display_id: *display_id,
                reason: reason.clone(),
            })
        }
        AppEvent::DisplayApprovalPending { display_id, backend } => {
            Some(OutboundEvent::DisplayApprovalPending {
                display_id: *display_id,
                backend: backend.to_string(),
            })
        }
        AppEvent::FileChanged { path, kind, lines_added, lines_removed } => {
            let kind_str = serde_json::to_value(kind)
                .ok()
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_else(|| "modified".to_string());
            Some(OutboundEvent::FileChanged {
                path: path.clone(),
                kind: kind_str,
                lines_added: *lines_added,
                lines_removed: *lines_removed,
            })
        }
        AppEvent::SnapshotCreated { round_id } => {
            Some(OutboundEvent::SnapshotCreated { round_id: *round_id })
        }
        AppEvent::RolledBack { from_id, to_id, files_reverted } => {
            Some(OutboundEvent::RolledBack {
                from_id: *from_id,
                to_id: *to_id,
                files_reverted: *files_reverted,
            })
        }
        AppEvent::Redone { to_id } => Some(OutboundEvent::Redone { to_id: *to_id }),
        AppEvent::HistoryPruned { branches_removed, bytes_freed } => {
            Some(OutboundEvent::HistoryPruned {
                branches_removed: *branches_removed,
                bytes_freed: *bytes_freed,
            })
        }
        AppEvent::ConversationRolledBack {
            round_id,
            turns_removed,
            backend,
            method,
        } => Some(OutboundEvent::ConversationRolledBack {
            round_id: *round_id,
            turns_removed: *turns_removed,
            backend: backend.clone(),
            method: method.clone(),
        }),
        // Input event for the agent loop — not broadcast to browsers.
        AppEvent::ConversationRollbackRequested { .. } => None,
        // Terminal-only / internal events — not broadcast to external consumers
        AppEvent::Key(_)
        | AppEvent::Resize(_, _)
        | AppEvent::Tick
        | AppEvent::Quit
        | AppEvent::JsonExtracted { .. }
        | AppEvent::OrchestratorLog { .. }
        | AppEvent::SessionDirChanged { .. }
        | AppEvent::ControlCommand(_)
        | AppEvent::PresenceReady
        | AppEvent::PresenceConnected { .. }
        | AppEvent::PresenceDisconnected
        | AppEvent::VoiceLog { .. }
        | AppEvent::PresenceCheckpointReceived { .. }
        | AppEvent::VoiceDiagnostic { .. }
        | AppEvent::LiveAudioStarted { .. }
        | AppEvent::LiveAudioProgress { .. }
        | AppEvent::LiveAudioCompleted { .. } => None,
    }
}

/// Spawn a task that converts AppEvents to OutboundEvents and broadcasts them.
///
/// This is the single point where AppEvents are converted to the external
/// format used by the control socket, web gateway, and JSON stdout.
pub fn spawn_outbound_broadcaster(
    mut event_rx: tokio::sync::broadcast::Receiver<AppEvent>,
    outbound_tx: tokio::sync::broadcast::Sender<String>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match event_rx.recv().await {
                Ok(event) => {
                    if let Some(outbound) = app_event_to_outbound(&event) {
                        if let Ok(json) = serde_json::to_string(&outbound) {
                            let _ = outbound_tx.send(json);
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

/// Spawn a task that persists AppEvents to the session log on disk.
///
/// This is the single point where events flowing through the bus are written
/// to `session.jsonl`. Events that are already logged inline by the agent loop
/// (turn_start, model_response, agent_input/output, approval decisions, etc.)
/// are skipped here to avoid duplication — only events that would otherwise be
/// lost are handled.
///
/// Counterpart to `spawn_outbound_broadcaster` which handles the WebSocket/
/// control-socket broadcast path.
pub fn spawn_session_log_writer(
    mut event_rx: tokio::sync::broadcast::Receiver<AppEvent>,
    session_log: crate::SharedSessionLog,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match event_rx.recv().await {
                Ok(event) => write_event_to_session_log(&session_log, &event),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

/// Write a single AppEvent to the session log if it isn't already logged
/// inline by the agent loop.
fn write_event_to_session_log(
    session_log: &crate::SharedSessionLog,
    event: &AppEvent,
) {
    let Ok(mut log) = session_log.lock() else {
        return;
    };

    match event {
        // ---- Events NOT logged inline — this writer is their only path to disk ----

        // Agent lifecycle
        AppEvent::AgentStarted { turn, commands_preview, .. } => {
            log.agent_started(*turn, commands_preview);
        }
        AppEvent::DoneSignal { message } => {
            log.done_signal(message.as_deref());
        }
        AppEvent::TaskComplete { reason, summary } => {
            log.task_complete(reason, summary.as_deref());
        }
        AppEvent::InterruptRequested => {
            log.info("Interrupt requested");
        }
        AppEvent::Interrupted { reason } => {
            log.info(&format!("Interrupted: {}", reason));
        }
        AppEvent::SessionStarted { session_id, task } => {
            log.session_started(session_id, task.as_deref());
        }
        AppEvent::SessionEnded { session_id, reason } => {
            log.session_ended(session_id, reason);
        }
        AppEvent::SafetyCapReached => {
            log.safety_cap_reached();
        }
        AppEvent::SubAgentResult { formatted } => {
            log.sub_agent_result(formatted);
        }
        AppEvent::OrchestratorProgress { status, .. } => {
            log.orchestrator_progress(status);
        }
        AppEvent::RoundComplete { round, turns_in_round, .. } => {
            log.round_complete(*round, *turns_in_round);
        }

        // Approval / human interaction
        AppEvent::AutoApproved { preview } => {
            log.auto_approved(preview);
        }
        AppEvent::ApprovalResolved { id, action } => {
            log.approval_resolved(*id, action);
        }
        AppEvent::HumanQuestionDetected { question } => {
            log.human_question(question);
        }
        AppEvent::HumanResponseSent => {
            log.human_response_sent();
        }

        // Display / vision
        AppEvent::DisplayReady { display_id, width, height } => {
            log.display_ready(*display_id, *width, *height);
        }
        AppEvent::DisplayResize { display_id, width, height } => {
            log.display_resize(*display_id, *width, *height);
        }
        AppEvent::DisplayTaken { display_id } => {
            log.display_taken(*display_id);
        }
        AppEvent::DisplayReleased { display_id, note } => {
            log.display_released(*display_id, note.as_deref());
        }
        AppEvent::DisplayCaptureLost { display_id, reason } => {
            log.warn(&format!(
                "Display :{} capture lost: {}",
                display_id, reason
            ));
        }
        AppEvent::DisplayApprovalPending { display_id, backend } => {
            log.info(&format!(
                "Display :{} waiting for OS approval ({backend} portal)",
                display_id
            ));
        }
        AppEvent::UserDisplayGranted { display_id } => {
            log.info(&format!("User display access granted (display_id: {})", display_id));
        }
        AppEvent::UserDisplayRevoked { display_id, note } => {
            let msg = if let Some(n) = note {
                format!("User display access revoked (display_id: {}): {}", display_id, n)
            } else {
                format!("User display access revoked (display_id: {})", display_id)
            };
            log.info(&msg);
        }
        AppEvent::DebugScreenReady { display_id } => {
            log.debug_screen_ready(*display_id);
        }
        AppEvent::DebugScreenTornDown { display_id } => {
            log.debug_screen_torn_down(*display_id);
        }

        // Recording
        AppEvent::RecordingStarted { stream_name } => {
            log.recording_started(stream_name);
        }
        AppEvent::RecordingStopped { stream_name } => {
            log.recording_stopped(stream_name);
        }
        AppEvent::RecordingError { stream_name, message } => {
            log.recording_error(stream_name, message);
        }
        AppEvent::RecordingDeleted { stream_name } => {
            log.info(&format!("Recording deleted: {}", stream_name));
        }

        // Presence / voice
        AppEvent::PresenceLog { message, level, .. } => {
            let level_str = level.as_ref().map(|l| {
                crate::frontend::log_level_to_str(l)
            });
            log.presence_log(message, level_str);
        }
        AppEvent::PresenceUsageUpdate {
            provider, model, total_tokens, context_window, usage_pct, ..
        } => {
            log.presence_usage_update(provider, model, *total_tokens, *context_window, *usage_pct);
        }
        AppEvent::LiveUsageUpdate {
            provider, model, total_tokens, ..
        } => {
            log.live_usage_update(provider, model, *total_tokens);
        }

        // Live audio sub-agent lifecycle
        AppEvent::LiveAudioStarted { id, provider } => {
            log.live_audio_started(id, provider);
        }
        AppEvent::LiveAudioProgress { id, state, elapsed_secs, transcript_preview } => {
            log.live_audio_progress(id, state, *elapsed_secs, transcript_preview);
        }
        AppEvent::LiveAudioCompleted { id, status, quarantine_count, .. } => {
            log.live_audio_completed(id, status, *quarantine_count);
        }

        // External agent paths emit these AppEvents without inline slog()
        // calls, so the EventBus writer is the only path to disk.
        AppEvent::AgentOutput { stdout, stderr, source } => {
            log.agent_output(stdout, stderr, source.as_deref());
        }
        AppEvent::ModelResponse { content, usage, reasoning, source, .. } => {
            if !content.is_empty() {
                log.model_response(
                    content,
                    usage.prompt_tokens,
                    usage.completion_tokens,
                    usage.total_tokens,
                    usage.cached_tokens,
                    source.as_deref(),
                );
            }
            if let Some(ref r) = reasoning {
                log.reasoning_content(Some(r.as_str()), None);
            }
        }

        // File watcher
        AppEvent::FileChanged { path, kind, lines_added, lines_removed } => {
            let kind_str = serde_json::to_value(kind)
                .ok()
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_else(|| "modified".to_string());
            log.info(&format!("file_{}: {} (+{}/-{})", kind_str, path, lines_added, lines_removed));
        }
        AppEvent::SnapshotCreated { round_id } => {
            log.snapshot_created(*round_id);
        }
        AppEvent::RolledBack { from_id, to_id, files_reverted } => {
            log.rolled_back(*from_id, *to_id, *files_reverted);
        }
        AppEvent::Redone { to_id } => {
            log.redone(*to_id);
        }
        AppEvent::HistoryPruned { branches_removed, bytes_freed } => {
            log.history_pruned(*branches_removed, *bytes_freed);
        }
        AppEvent::ConversationRolledBack {
            round_id,
            turns_removed,
            backend,
            method,
        } => {
            log.conversation_rolled_back(*round_id, *turns_removed, backend, method);
        }
        AppEvent::ConversationRollbackRequested { .. } => {
            // Input event — the agent loop logs the completion
            // (ConversationRolledBack). No point logging the request.
        }

        // ---- Events already logged inline by the agent loop or web_gateway ----
        // turn_start, model_response, agent_input, approval decisions,
        // json_extracted, reasoning, budget warnings, loop errors, context
        // management, voice_log, voice_diagnostic, presence_connected/disconnected,
        // presence_checkpoint, user_transcript — all have slog() calls at their
        // point of origin.
        //
        // ---- Terminal-only / internal / high-frequency events ----
        // Key, Resize, Tick, Quit, ControlCommand, SessionDirChanged,
        // ModelResponseDelta (too chatty), StatusUpdate (every tick),
        // UsageSnapshot (periodic, mainly for UI), LogEntry (meta/circular).
        _ => {}
    }
}

/// Spawns a tick timer that sends Tick events at a regular interval.
pub fn spawn_tick_timer(bus: EventBus, interval_ms: u64) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(interval_ms));
        loop {
            interval.tick().await;
            bus.send(AppEvent::Tick);
        }
    })
}

/// Spawns a file monitor for askHuman question files.
/// The `question_path` is the session-scoped path to the human_question file.
/// Shared path that can be updated when MCP tasks change session directories.
pub type SharedQuestionPath = std::sync::Arc<tokio::sync::RwLock<std::path::PathBuf>>;

pub fn shared_question_path(path: std::path::PathBuf) -> SharedQuestionPath {
    std::sync::Arc::new(tokio::sync::RwLock::new(path))
}

pub fn spawn_human_question_monitor(
    bus: EventBus,
    question_path: SharedQuestionPath,
) -> tokio::task::JoinHandle<()> {
    use tokio::time::{interval, Duration};

    tokio::spawn(async move {
        let mut interval = interval(Duration::from_millis(250));
        let mut last_seen = false;

        loop {
            interval.tick().await;

            let path = question_path.read().await.clone();
            if path.exists() {
                if !last_seen {
                    if let Ok(question) = tokio::fs::read_to_string(&path).await {
                        let question = question.trim().to_string();
                        if !question.is_empty() {
                            bus.send(AppEvent::HumanQuestionDetected { question });
                        }
                    }
                    last_seen = true;
                }
            } else {
                if last_seen {
                    bus.send(AppEvent::HumanResponseSent);
                    last_seen = false;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_bus_send_receive() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            bus.send(AppEvent::Tick);
            bus.send(AppEvent::Quit);

            match rx.recv().await.unwrap() {
                AppEvent::Tick => {}
                _ => panic!("expected Tick"),
            }
            match rx.recv().await.unwrap() {
                AppEvent::Quit => {}
                _ => panic!("expected Quit"),
            }
        });
    }

    #[test]
    fn event_bus_clone() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let bus2 = bus.clone();
            bus.send(AppEvent::Tick);
            bus2.send(AppEvent::Quit);

            match rx.recv().await.unwrap() {
                AppEvent::Tick => {}
                _ => panic!("expected Tick"),
            }
            match rx.recv().await.unwrap() {
                AppEvent::Quit => {}
                _ => panic!("expected Quit"),
            }
        });
    }

    #[test]
    fn control_msg_status_deserialize() {
        let json = r#"{"action":"status"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::Status => {}
            _ => panic!("expected Status"),
        }
    }

    #[test]
    fn control_msg_approve_deserialize() {
        let json = r#"{"action":"approve","id":42}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::Approve { id } => assert_eq!(id, 42),
            _ => panic!("expected Approve"),
        }
    }

    #[test]
    fn control_msg_deny_deserialize() {
        let json = r#"{"action":"deny","id":7}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::Deny { id } => assert_eq!(id, 7),
            _ => panic!("expected Deny"),
        }
    }

    #[test]
    fn control_msg_input_deserialize() {
        let json = r#"{"action":"input","text":"PostgreSQL"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::Input { text } => assert_eq!(text, "PostgreSQL"),
            _ => panic!("expected Input"),
        }
    }

    #[test]
    fn control_msg_set_autonomy_deserialize() {
        let json = r#"{"action":"set_autonomy","level":"high"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::SetAutonomy { level } => assert_eq!(level, "high"),
            _ => panic!("expected SetAutonomy"),
        }
    }

    #[test]
    fn control_msg_set_external_agent_deserialize() {
        let json = r#"{"action":"set_external_agent","agent":"codex"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::SetExternalAgent { agent } => assert_eq!(agent, Some("codex".to_string())),
            _ => panic!("expected SetExternalAgent"),
        }
    }

    #[test]
    fn control_msg_set_external_agent_null_deserialize() {
        let json = r#"{"action":"set_external_agent","agent":null}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::SetExternalAgent { agent } => assert_eq!(agent, None),
            _ => panic!("expected SetExternalAgent"),
        }
    }

    #[test]
    fn control_msg_quit_deserialize() {
        let json = r#"{"action":"quit"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::Quit => {}
            _ => panic!("expected Quit"),
        }
    }

    #[test]
    fn control_msg_schedule_restart_deserialize() {
        let json = r#"{"action":"schedule_controller_restart","controller_id":"codex","north_star_goal":"audit and improve","restart_after":"turn_end"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::ScheduleControllerRestart {
                controller_id,
                north_star_goal,
                restart_after,
                ..
            } => {
                assert_eq!(controller_id, "codex");
                assert_eq!(north_star_goal, "audit and improve");
                assert_eq!(restart_after.as_deref(), Some("turn_end"));
            }
            _ => panic!("expected ScheduleControllerRestart"),
        }
    }

    #[test]
    fn control_msg_controller_turn_complete_deserialize() {
        let json = r#"{"action":"controller_turn_complete","restart_id":"abc","turn_complete_token":"tok"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::ControllerTurnComplete {
                restart_id,
                turn_complete_token,
                ..
            } => {
                assert_eq!(restart_id, "abc");
                assert_eq!(turn_complete_token, "tok");
            }
            _ => panic!("expected ControllerTurnComplete"),
        }
    }

    #[test]
    fn control_msg_controller_loop_variants_deserialize() {
        let halt: ControlMsg =
            serde_json::from_str(r#"{"action":"request_controller_loop_halt","persistent":false}"#)
                .unwrap();
        match halt {
            ControlMsg::RequestControllerLoopHalt { persistent } => {
                assert_eq!(persistent, Some(false));
            }
            _ => panic!("expected RequestControllerLoopHalt"),
        }

        let intervene: ControlMsg =
            serde_json::from_str(r#"{"action":"intervene_controller_loop","mode":"stop"}"#)
                .unwrap();
        match intervene {
            ControlMsg::InterveneControllerLoop { mode } => assert_eq!(mode, "stop"),
            _ => panic!("expected InterveneControllerLoop"),
        }
    }

    #[test]
    fn control_msg_serialize_roundtrip() {
        let msgs = vec![
            ControlMsg::Status,
            ControlMsg::Approve { id: 1 },
            ControlMsg::Deny { id: 2 },
            ControlMsg::Input {
                text: "hello".to_string(),
            },
            ControlMsg::Skip { id: 3 },
            ControlMsg::ApproveAll { id: 4 },
            ControlMsg::SetAutonomy {
                level: "low".to_string(),
            },
            ControlMsg::SetExternalAgent {
                agent: Some("codex".to_string()),
            },
            ControlMsg::SetVerbosity {
                level: "verbose".to_string(),
            },
            ControlMsg::ScheduleControllerRestart {
                controller_id: "codex".to_string(),
                north_star_goal: "improve".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: None,
                auto_start_task: Some(true),
                max_attempts: Some(1),
                cooldown_sec: Some(30),
            },
            ControlMsg::ControllerTurnComplete {
                restart_id: "id".to_string(),
                turn_complete_token: "token".to_string(),
                status: None,
                handoff_summary: None,
            },
            ControlMsg::GetRestartStatus,
            ControlMsg::CancelControllerRestart { restart_id: None },
            ControlMsg::RequestControllerLoopHalt {
                persistent: Some(true),
            },
            ControlMsg::ClearControllerLoopHalt,
            ControlMsg::InterveneControllerLoop {
                mode: "stop".to_string(),
            },
            ControlMsg::GetControllerLoopStatus,
            ControlMsg::StartTask {
                task: "fix bug".to_string(),
                orchestrate: None,
                direct: None,
                reference_frame_ids: vec![],
                display_target: None,
                attachments: vec![],
            },
            ControlMsg::FollowUp {
                text: "continue working".to_string(),
                direct: None,
            },
            ControlMsg::QueryDetail {
                scope: "diff".to_string(),
                target: None,
            },
            ControlMsg::RecallMemory {
                keywords: Some(vec!["auth".to_string()]),
                tags: None,
                channel: Some("project_state".to_string()),
            },
            ControlMsg::TakeDisplay { display_id: 99 },
            ControlMsg::ReleaseDisplay {
                display_id: 99,
                note: Some("done testing".to_string()),
            },
            ControlMsg::GrantUserDisplay { display_id: None },
            ControlMsg::RevokeUserDisplay {
                display_id: None,
                note: Some("done with user display".to_string()),
            },
            ControlMsg::InvokeSkill {
                skill_name: "deploy".to_string(),
                arguments: Some("staging".to_string()),
            },
            ControlMsg::Usage,
            ControlMsg::Quit,
            ControlMsg::Interrupt { expected_turn: None },
            ControlMsg::Steer {
                text: "use Python".to_string(),
                id: Some("s-1".to_string()),
            },
            ControlMsg::Steer {
                text: "never mind".to_string(),
                id: None,
            },
        ];
        for msg in msgs {
            let json = serde_json::to_string(&msg).unwrap();
            let _: ControlMsg = serde_json::from_str(&json).unwrap();
        }
    }

    #[test]
    fn control_msg_steer_deserialize() {
        // Wire shape: `{"action":"steer","text":"...","id":"..."}`.
        // `id` is optional — frontends may omit it, in which case the
        // dispatcher will treat it as "no correlation".
        let json = r#"{"action":"steer","text":"switch to Python","id":"s-42"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::Steer { text, id } => {
                assert_eq!(text, "switch to Python");
                assert_eq!(id.as_deref(), Some("s-42"));
            }
            _ => panic!("expected Steer"),
        }
    }

    #[test]
    fn control_msg_steer_without_id_deserialize() {
        let json = r#"{"action":"steer","text":"never mind"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::Steer { text, id } => {
                assert_eq!(text, "never mind");
                assert_eq!(id, None);
            }
            _ => panic!("expected Steer"),
        }
    }

    #[test]
    fn control_msg_usage_deserialize() {
        let json = r#"{"action":"usage"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        assert!(matches!(msg, ControlMsg::Usage));
    }

    #[test]
    fn control_msg_skip_deserialize() {
        let json = r#"{"action":"skip","id":5}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::Skip { id } => assert_eq!(id, 5),
            _ => panic!("expected Skip"),
        }
    }

    #[test]
    fn control_msg_approve_all_deserialize() {
        let json = r#"{"action":"approve_all","id":10}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::ApproveAll { id } => assert_eq!(id, 10),
            _ => panic!("expected ApproveAll"),
        }
    }

    #[test]
    fn control_msg_set_verbosity_deserialize() {
        let json = r#"{"action":"set_verbosity","level":"verbose"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::SetVerbosity { level } => assert_eq!(level, "verbose"),
            _ => panic!("expected SetVerbosity"),
        }
    }

    #[test]
    fn control_msg_start_task_deserialize() {
        let json = r#"{"action":"start_task","task":"fix bug"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::StartTask { task, orchestrate, reference_frame_ids, display_target, .. } => {
                assert_eq!(task, "fix bug");
                assert!(orchestrate.is_none());
                assert!(reference_frame_ids.is_empty());
                assert!(display_target.is_none());
            }
            _ => panic!("expected StartTask"),
        }
    }

    #[test]
    fn control_msg_start_task_roundtrip() {
        let msg = ControlMsg::StartTask {
            task: "deploy app".to_string(),
            orchestrate: Some(true),
            direct: None,
            reference_frame_ids: vec!["display_99-f00001".to_string()],
            display_target: Some("user_session".to_string()),
            attachments: vec!["ann-recording-1".to_string(), "ann-recording-2".to_string()],
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: ControlMsg = serde_json::from_str(&json).unwrap();
        match parsed {
            ControlMsg::StartTask { task, orchestrate, reference_frame_ids, display_target, attachments, .. } => {
                assert_eq!(task, "deploy app");
                assert_eq!(orchestrate, Some(true));
                assert_eq!(reference_frame_ids.len(), 1);
                assert_eq!(display_target.as_deref(), Some("user_session"));
                assert_eq!(attachments.len(), 2);
                assert_eq!(attachments[0], "ann-recording-1");
            }
            _ => panic!("expected StartTask"),
        }
    }

    #[test]
    fn control_msg_start_task_attachments_default_empty() {
        let json = r#"{"action":"start_task","task":"do thing"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::StartTask { attachments, .. } => {
                assert!(attachments.is_empty());
            }
            _ => panic!("expected StartTask"),
        }
    }

    #[test]
    fn control_msg_start_task_attachments_parse() {
        let json = r#"{"action":"start_task","task":"do thing","attachments":["a","b","c"]}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::StartTask { attachments, .. } => {
                assert_eq!(attachments.len(), 3);
                assert_eq!(attachments[1], "b");
            }
            _ => panic!("expected StartTask"),
        }
    }

    #[test]
    fn approval_response_variants() {
        assert_ne!(ApprovalResponse::Approve, ApprovalResponse::Deny);
        assert_ne!(ApprovalResponse::Skip, ApprovalResponse::ApproveAll);
        assert_eq!(ApprovalResponse::Approve, ApprovalResponse::Approve);
    }

    #[test]
    fn control_msg_query_detail_deserialize() {
        let json = r#"{"action":"query_detail","scope":"diff"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::QueryDetail { scope, target } => {
                assert_eq!(scope, "diff");
                assert!(target.is_none());
            }
            _ => panic!("expected QueryDetail"),
        }
    }

    #[test]
    fn control_msg_query_detail_with_target() {
        let json = r#"{"action":"query_detail","scope":"file","target":"src/main.rs"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::QueryDetail { scope, target } => {
                assert_eq!(scope, "file");
                assert_eq!(target.as_deref(), Some("src/main.rs"));
            }
            _ => panic!("expected QueryDetail"),
        }
    }

    #[test]
    fn control_msg_recall_memory_deserialize() {
        let json = r#"{"action":"recall_memory","keywords":["auth","login"],"channel":"project_state"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::RecallMemory {
                keywords,
                tags,
                channel,
            } => {
                assert_eq!(keywords, Some(vec!["auth".to_string(), "login".to_string()]));
                assert!(tags.is_none());
                assert_eq!(channel.as_deref(), Some("project_state"));
            }
            _ => panic!("expected RecallMemory"),
        }
    }

    #[test]
    fn control_msg_recall_memory_minimal() {
        let json = r#"{"action":"recall_memory"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::RecallMemory {
                keywords,
                tags,
                channel,
            } => {
                assert!(keywords.is_none());
                assert!(tags.is_none());
                assert!(channel.is_none());
            }
            _ => panic!("expected RecallMemory"),
        }
    }

    #[test]
    fn control_msg_invoke_skill_deserialize() {
        let json = r#"{"action":"invoke_skill","skill_name":"deploy","arguments":"staging"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::InvokeSkill {
                skill_name,
                arguments,
            } => {
                assert_eq!(skill_name, "deploy");
                assert_eq!(arguments, Some("staging".to_string()));
            }
            _ => panic!("expected InvokeSkill"),
        }
    }

    #[test]
    fn control_msg_invoke_skill_no_args() {
        let json = r#"{"action":"invoke_skill","skill_name":"lint"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::InvokeSkill {
                skill_name,
                arguments,
            } => {
                assert_eq!(skill_name, "lint");
                assert!(arguments.is_none());
            }
            _ => panic!("expected InvokeSkill"),
        }
    }

    #[test]
    fn control_msg_grant_user_display_deserialize() {
        let json = r#"{"action":"grant_user_display"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        assert!(matches!(msg, ControlMsg::GrantUserDisplay { display_id: None }));

        // With explicit display_id
        let json = r#"{"action":"grant_user_display","display_id":2}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        assert!(matches!(msg, ControlMsg::GrantUserDisplay { display_id: Some(2) }));
    }

    #[test]
    fn control_msg_revoke_user_display_deserialize() {
        let json = r#"{"action":"revoke_user_display","note":"testing done"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::RevokeUserDisplay { display_id, note } => {
                assert_eq!(display_id, None);
                assert_eq!(note.as_deref(), Some("testing done"));
            }
            _ => panic!("expected RevokeUserDisplay"),
        }
    }

    #[test]
    fn control_msg_revoke_user_display_no_note() {
        let json = r#"{"action":"revoke_user_display"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::RevokeUserDisplay { display_id, note } => {
                assert_eq!(display_id, None);
                assert!(note.is_none());
            }
            _ => panic!("expected RevokeUserDisplay"),
        }
    }

    #[test]
    fn control_msg_revoke_user_display_with_id() {
        let json = r#"{"action":"revoke_user_display","display_id":3,"note":"done"}"#;
        let msg: ControlMsg = serde_json::from_str(json).unwrap();
        match msg {
            ControlMsg::RevokeUserDisplay { display_id, note } => {
                assert_eq!(display_id, Some(3));
                assert_eq!(note.as_deref(), Some("done"));
            }
            _ => panic!("expected RevokeUserDisplay"),
        }
    }

    // --- app_event_to_outbound tests ---

    #[test]
    fn outbound_turn_started() {
        let event = AppEvent::TurnStarted {
            turn: 5,
            budget_pct: 42.0,
            remaining: 100_000,
        };
        let outbound = app_event_to_outbound(&event).unwrap();
        let json = serde_json::to_string(&outbound).unwrap();
        assert!(json.contains("\"event\":\"turn_started\""));
        assert!(json.contains("\"turn\":5"));
    }

    #[test]
    fn outbound_done_signal() {
        let event = AppEvent::DoneSignal {
            message: Some("All done".to_string()),
        };
        let outbound = app_event_to_outbound(&event).unwrap();
        let json = serde_json::to_string(&outbound).unwrap();
        assert!(json.contains("\"event\":\"done_signal\""));
        assert!(json.contains("\"All done\""));
    }

    #[test]
    fn outbound_skips_tick() {
        assert!(app_event_to_outbound(&AppEvent::Tick).is_none());
    }

    #[test]
    fn outbound_skips_key() {
        let event = AppEvent::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('a'),
            crossterm::event::KeyModifiers::NONE,
        ));
        assert!(app_event_to_outbound(&event).is_none());
    }

    #[test]
    fn outbound_agent_output() {
        let event = AppEvent::AgentOutput {
            stdout: "hello".to_string(),
            stderr: "".to_string(),
            source: None,
        };
        let outbound = app_event_to_outbound(&event).unwrap();
        let json = serde_json::to_string(&outbound).unwrap();
        assert!(json.contains("\"event\":\"agent_output\""));
        assert!(json.contains("\"hello\""));
    }

    #[test]
    fn outbound_approval_required() {
        let event = AppEvent::ApprovalRequired {
            id: 42,
            command_preview: "rm -rf /tmp".to_string(),
            category: crate::autonomy::ActionCategory::Destructive,
        };
        let outbound = app_event_to_outbound(&event).unwrap();
        let json = serde_json::to_string(&outbound).unwrap();
        assert!(json.contains("\"event\":\"approval_required\""));
        assert!(json.contains("\"id\":42"));
    }

    #[test]
    fn outbound_budget_warning() {
        let event = AppEvent::BudgetWarning {
            pct: 85.0,
            remaining: 10_000,
        };
        let outbound = app_event_to_outbound(&event).unwrap();
        let json = serde_json::to_string(&outbound).unwrap();
        assert!(json.contains("\"event\":\"budget_warning\""));
    }

    #[test]
    fn outbound_model_response_delta() {
        let event = AppEvent::ModelResponseDelta {
            text: "hello".to_string(),
        };
        let outbound = app_event_to_outbound(&event).unwrap();
        let json = serde_json::to_string(&outbound).unwrap();
        assert!(json.contains("\"event\":\"model_response_delta\""));
    }

    #[test]
    fn outbound_auto_approved() {
        let event = AppEvent::AutoApproved {
            preview: "exec: ls".to_string(),
        };
        let outbound = app_event_to_outbound(&event).unwrap();
        let json = serde_json::to_string(&outbound).unwrap();
        assert!(json.contains("\"event\":\"auto_approved\""));
    }

    #[test]
    fn outbound_live_usage_update() {
        let event = AppEvent::LiveUsageUpdate {
            provider: "gemini".to_string(),
            model: "gemini-2.5-flash".to_string(),
            input_tokens: 1000,
            output_tokens: 500,
            cached_tokens: 200,
            total_tokens: 1700,
            thinking_tokens: 0,
        };
        let outbound = app_event_to_outbound(&event).unwrap();
        let json = serde_json::to_string(&outbound).unwrap();
        assert!(json.contains("\"event\":\"live_usage_update\""));
        assert!(json.contains("\"input_tokens\":1000"));
        assert!(json.contains("\"provider\":\"gemini\""));
    }

    #[test]
    fn outbound_broadcast_multiple_subscribers() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let bus = EventBus::new();
            let mut rx1 = bus.subscribe();
            let mut rx2 = bus.subscribe();
            bus.send(AppEvent::Tick);

            // Both subscribers should receive the event
            assert!(matches!(rx1.recv().await, Ok(AppEvent::Tick)));
            assert!(matches!(rx2.recv().await, Ok(AppEvent::Tick)));
        });
    }
}
