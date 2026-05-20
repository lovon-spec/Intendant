//! Pure-Rust app state for the web dashboard.
//!
//! All event routing, log filtering, usage tracking, cost calculation,
//! and status bar state live here. Methods return `Vec<UiCommand>` which
//! the thin JS layer executes as DOM updates.

use serde::{Deserialize, Serialize};

fn is_false(value: &bool) -> bool {
    !*value
}

// ── UiCommand ──────────────────────────────────────────────────────

/// Commands sent from WASM to JS for DOM updates.
/// Batched as `Vec<UiCommand>` and serialized as a JSON array.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum UiCommand {
    AddLogEntry {
        ts: String,
        level: String,
        source: String,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_id: Option<String>,
        #[serde(default)]
        collapsible: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user_turn_index: Option<u32>,
        #[serde(default, skip_serializing_if = "is_false")]
        superseded: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replacement_for_user_turn_index: Option<u32>,
        /// Base64-encoded images (screenshots) associated with this entry.
        /// Sent separately from content so JS can lazy-load them on expand.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<String>,
    },
    MarkActivityContextRewind {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        user_turn_index: u32,
        turns_removed: u32,
    },
    ClearLogs,
    AddTurnSeparator {
        turn: u64,
    },
    UpdateStatusBar {
        #[serde(skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        turn: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        budget_pct: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        autonomy: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        external_agent: Option<String>,
    },
    SetPhase {
        phase: String,
    },
    ShowApproval {
        id: u64,
        command: String,
        category: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
    },
    HideApproval,
    ShowHumanInput {
        question: String,
    },
    HideHumanInput,
    HideAllPanels,
    UpdateUsage {
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        main_json: Option<String>,
        presence_json: Option<String>,
        live_json: Option<String>,
        cost_json: Option<String>,
        history_json: Option<String>,
    },
    AddDisplay {
        display_id: u64,
        #[serde(default)]
        width: u64,
        #[serde(default)]
        height: u64,
    },
    AddRecording {
        stream_name: String,
    },
    RemoveRecording {
        stream_name: String,
    },
    RecordingError {
        stream_name: String,
        message: String,
    },
    SessionStarted {
        session_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        task: Option<String>,
    },
    SessionAttached {
        session_id: String,
        source: String,
    },
    SessionEnded {
        session_id: String,
        reason: String,
    },
    DebugScreenReady {
        display_id: u64,
    },
    DebugScreenTornDown,
    ShowBadge {
        tab: String,
        text: String,
    },
    HideBadge {
        tab: String,
    },
    /// Write raw base64 ANSI data to the terminal.
    TermData {
        base64: String,
    },
    SetConnected {
        connected: bool,
    },
    FileChanged {
        path: String,
        kind: String,
        lines_added: u64,
        lines_removed: u64,
    },
    /// A user-uploaded file was committed to the session's upload store
    /// (via `POST /api/upload`). The dashboard adds it to the "pending
    /// attachments" panel under the task input.
    UploadReady {
        descriptor: serde_json::Value,
    },
    /// An upload was removed from the store (by this or another browser).
    UploadDeleted {
        id: String,
    },
    /// A peer was added to the registry. `peer` is the
    /// `PeerSnapshot` JSON unchanged from the wire — JS treats it
    /// the same shape as a `/api/peers` list entry.
    PeerAdded {
        peer: serde_json::Value,
    },
    /// A peer was removed from the registry. JS drops the matching
    /// row from the daemons list.
    PeerRemoved {
        id: String,
    },
    /// A peer's connection state, status, or card changed. `peer`
    /// is the fresh `PeerSnapshot` JSON; JS replaces the matching
    /// row in-place.
    PeerStateChanged {
        peer: serde_json::Value,
    },
    /// A log line emitted by a federated peer's PeerEvent stream.
    /// Same shape as AddLogEntry but `host_id`-tagged so JS routes it
    /// to the per-peer log surface instead of the local stream.
    /// Replaces the secondary AppState's per-host `add_log_entry +
    /// JS-side host_id mutation` pattern with a typed first-class field.
    PeerLog {
        host_id: String,
        ts: String,
        level: String,
        source: String,
        content: String,
    },
    /// A peer reported a usage snapshot. The `snapshot` is the
    /// PeerEvent::Usage payload JSON unchanged — JS treats it the
    /// same shape it gets from /api/peers usage queries (token counts,
    /// cost, optional per-model breakdown). Cached + conditionally
    /// rendered into the Stats host picker.
    PeerUsage {
        host_id: String,
        snapshot: serde_json::Value,
    },
    /// A peer is asking for approval. JS adds it to the per-peer
    /// pending-approvals list rendered in the Daemons row's controls
    /// panel. Replaces slice 4's secondary `show_approval` interception.
    PeerApprovalRequested {
        host_id: String,
        id: String,
        command: String,
        category: String,
    },
    /// A peer's approval got resolved (locally by us via
    /// /api/peers/{id}/approval, by the peer's own auto-approval, or
    /// by another dashboard session). JS drops the matching pending
    /// entry. Replaces slice 4's raw `approval_resolved` tap.
    PeerApprovalResolved {
        host_id: String,
        id: String,
    },
    /// One leg of a federation-driven WebRTC signaling exchange
    /// arriving from a peer. JS feeds the `signal` payload to the
    /// matching per-peer `RTCPeerConnection` keyed by
    /// `(host_id, display_id, session_id)`:
    ///
    /// - `Answer` → `pc.setRemoteDescription({type:"answer", sdp})`
    /// - `IceCandidate` → `pc.addIceCandidate(JSON.parse(candidate_json))`
    /// - other / unknown → ignored (forward-compat)
    ///
    /// The `signal` field is the raw `PeerEvent::WebRtcSignal::signal`
    /// JSON value forwarded verbatim — JS dispatches on `signal.kind`
    /// directly so newer signal kinds added to the wire don't require
    /// a coordinated WASM rebuild on the dashboard side.
    ///
    /// Explicit `rename`: serde's `rename_all = "snake_case"` mangles
    /// `WebRtc` to `web_rtc`, producing `peer_web_rtc_signal` on the
    /// wire — but JS dispatches on `case 'peer_webrtc_signal'` (and
    /// every primer / doc reference uses the unbroken `webrtc`
    /// spelling, matching the W3C name). Without this rename the
    /// answer SDP arrives at the WS layer, gets translated to a
    /// well-formed UiCommand, and then silently misses the JS switch
    /// — no error, just a dead pipeline. The
    /// `peer_webrtc_signal_wire_name` test below is the invariant
    /// guard. Same hazard the wire-format policy in
    /// `src/bin/caller/peer/mod.rs` calls out for `A2A`/`OpenClaw`/etc.
    #[serde(rename = "peer_webrtc_signal")]
    PeerWebRtcSignal {
        host_id: String,
        display_id: u32,
        session_id: String,
        signal: serde_json::Value,
    },
    /// A Codex thread action (/compact, /fork, /undo, /review, /init,
    /// /memory-reset, /new) finished. `success` + `message` drive a
    /// dashboard toast and the Activity log entry.
    CodexThreadActionResult {
        action: String,
        success: bool,
        message: String,
    },
    /// Codex runtime config changed. Fields not included were not changed.
    /// The `_cleared` booleans distinguish "no change" (field None, bool
    /// false) from "override removed" (field None, bool true) so the
    /// Control sub-tab can zero the corresponding input.
    CodexConfigChanged {
        #[serde(skip_serializing_if = "Option::is_none")]
        command: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        sandbox: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        approval_policy: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        model_cleared: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning_effort: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        reasoning_effort_cleared: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        web_search: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        network_access: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        writable_roots: Option<Vec<String>>,
    },
    /// Mirror of `CodexThreadActionResult` for Gemini's session actions.
    /// Currently only `"new"` is valid; shape matches for future growth.
    GeminiThreadActionResult {
        action: String,
        success: bool,
        message: String,
    },
    /// Mirror of `CodexConfigChanged` for Gemini CLI. Fields omitted (or
    /// `None`) mean "no change". `model_cleared` has the same semantics as
    /// on the Codex variant.
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
    /// Session history changed (snapshot_created / rolled_back / redone /
    /// history_pruned). The JS layer re-fetches `/api/session/current/history`
    /// and re-renders the Timeline UI in the Changes sub-tab.
    HistoryChanged,
    /// Status update for an in-flight mid-turn steer. Emitted when the
    /// browser sends a steer (pending), when the backend reports it
    /// queued, or when the backend reports it delivered. The JS layer
    /// maintains an "in-flight steers" strip above the activity log and
    /// updates the row keyed by `id`. `status` is one of
    /// `"pending"` | `"queued"` | `"delivered"`.
    SteerStatusUpdate {
        id: String,
        text: String,
        status: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

// ── File change tracking ──────────────────────────────────────────

/// Tracks a single file change for the Changes sub-tab.
#[derive(Debug, Clone)]
pub struct FileChangeEntry {
    pub kind: String,
    pub lines_added: u64,
    pub lines_removed: u64,
}

// ── Steer tracking ────────────────────────────────────────────────

/// Delivery state for an in-flight mid-turn steer message.
#[derive(Debug, Clone, PartialEq)]
pub enum SteerStatus {
    /// Sent by the browser, awaiting backend acknowledgement.
    Pending,
    /// Backend acknowledged but can't deliver mid-turn — message
    /// is queued and will be delivered at the next turn boundary.
    Queued,
    /// Agent actually received the message (mid-turn or on boundary).
    Delivered,
}

/// A mid-turn steer message awaiting or undergoing delivery.
/// Keyed in `AppState::queued_steers` by the client-generated id.
#[derive(Debug, Clone)]
pub struct QueuedSteer {
    pub text: String,
    pub status: SteerStatus,
    /// Backend-supplied reason for queuing (e.g. "agent does not
    /// support mid-turn steering"). Filled in when SteerQueued arrives.
    pub reason: Option<String>,
}

// ── Pricing ────────────────────────────────────────────────────────

/// Per-token pricing in USD.
#[derive(Debug, Clone, Copy)]
pub struct ModelPricing {
    pub input: f64,
    pub cached: f64,
    pub output: f64,
}

/// Static pricing table. Searched by exact match then longest version-prefix match.
const PRICING_TABLE: &[(&str, ModelPricing)] = &[
    // OpenAI
    (
        "gpt-5.5",
        ModelPricing {
            input: 5.0e-6,
            cached: 0.5e-6,
            output: 30.0e-6,
        },
    ),
    (
        "gpt-5.4",
        ModelPricing {
            input: 2.5e-6,
            cached: 0.25e-6,
            output: 15.0e-6,
        },
    ),
    (
        "gpt-5.4-mini",
        ModelPricing {
            input: 0.75e-6,
            cached: 0.075e-6,
            output: 4.5e-6,
        },
    ),
    (
        "gpt-5.4-nano",
        ModelPricing {
            input: 0.2e-6,
            cached: 0.02e-6,
            output: 1.25e-6,
        },
    ),
    (
        "gpt-5.2",
        ModelPricing {
            input: 1.75e-6,
            cached: 0.175e-6,
            output: 14.0e-6,
        },
    ),
    (
        "gpt-5.2-codex",
        ModelPricing {
            input: 1.75e-6,
            cached: 0.175e-6,
            output: 14.0e-6,
        },
    ),
    (
        "gpt-5.3-codex",
        ModelPricing {
            input: 1.75e-6,
            cached: 0.175e-6,
            output: 14.0e-6,
        },
    ),
    (
        "gpt-5",
        ModelPricing {
            input: 1.25e-6,
            cached: 0.125e-6,
            output: 10.0e-6,
        },
    ),
    (
        "gpt-5-mini",
        ModelPricing {
            input: 0.25e-6,
            cached: 0.025e-6,
            output: 2.0e-6,
        },
    ),
    (
        "gpt-4.1",
        ModelPricing {
            input: 2.0e-6,
            cached: 0.5e-6,
            output: 8.0e-6,
        },
    ),
    (
        "gpt-4.1-mini",
        ModelPricing {
            input: 0.4e-6,
            cached: 0.1e-6,
            output: 1.6e-6,
        },
    ),
    (
        "gpt-4.1-nano",
        ModelPricing {
            input: 0.1e-6,
            cached: 0.025e-6,
            output: 0.4e-6,
        },
    ),
    (
        "o3",
        ModelPricing {
            input: 2.0e-6,
            cached: 1.0e-6,
            output: 8.0e-6,
        },
    ),
    (
        "o3-pro",
        ModelPricing {
            input: 150.0e-6,
            cached: 75.0e-6,
            output: 600.0e-6,
        },
    ),
    (
        "o4-mini",
        ModelPricing {
            input: 1.1e-6,
            cached: 0.55e-6,
            output: 4.4e-6,
        },
    ),
    // Anthropic
    (
        "claude-opus-4-6",
        ModelPricing {
            input: 5.0e-6,
            cached: 0.5e-6,
            output: 25.0e-6,
        },
    ),
    (
        "claude-opus-4-7",
        ModelPricing {
            input: 5.0e-6,
            cached: 0.5e-6,
            output: 25.0e-6,
        },
    ),
    (
        "claude-sonnet-4-6",
        ModelPricing {
            input: 3.0e-6,
            cached: 0.3e-6,
            output: 15.0e-6,
        },
    ),
    (
        "claude-sonnet-4-5-20250929",
        ModelPricing {
            input: 3.0e-6,
            cached: 0.3e-6,
            output: 15.0e-6,
        },
    ),
    (
        "claude-opus-4-5-20250929",
        ModelPricing {
            input: 5.0e-6,
            cached: 0.5e-6,
            output: 25.0e-6,
        },
    ),
    (
        "claude-haiku-4-5",
        ModelPricing {
            input: 1.0e-6,
            cached: 0.1e-6,
            output: 5.0e-6,
        },
    ),
    // Gemini
    (
        "gemini-3-flash",
        ModelPricing {
            input: 0.5e-6,
            cached: 0.05e-6,
            output: 3.0e-6,
        },
    ),
    (
        "gemini-3.1-flash",
        ModelPricing {
            input: 0.5e-6,
            cached: 0.05e-6,
            output: 3.0e-6,
        },
    ),
    (
        "gemini-2.5-pro",
        ModelPricing {
            input: 1.25e-6,
            cached: 0.125e-6,
            output: 10.0e-6,
        },
    ),
    (
        "gemini-2.5-flash",
        ModelPricing {
            input: 0.3e-6,
            cached: 0.03e-6,
            output: 2.5e-6,
        },
    ),
    (
        "gemini-2.5-flash-lite",
        ModelPricing {
            input: 0.1e-6,
            cached: 0.01e-6,
            output: 0.4e-6,
        },
    ),
    (
        "gemini-2.0-flash",
        ModelPricing {
            input: 0.1e-6,
            cached: 0.01e-6,
            output: 0.4e-6,
        },
    ),
];

fn model_key_matches(model: &str, key: &str) -> bool {
    model == key || model.starts_with(&format!("{key}-"))
}

/// Find pricing for a model by exact match, then longest version-prefix match.
pub fn find_pricing(model: &str) -> Option<ModelPricing> {
    let model = model.rsplit('/').next().unwrap_or(model);
    for &(key, pricing) in PRICING_TABLE {
        if model == key {
            return Some(pricing);
        }
    }
    PRICING_TABLE
        .iter()
        .filter(|(key, _)| model_key_matches(model, key))
        .max_by_key(|(key, _)| key.len())
        .map(|(_, pricing)| *pricing)
}

/// Calculate cost from token counts and pricing.
pub fn calculate_cost(
    prompt_tokens: u64,
    completion_tokens: u64,
    cached_tokens: u64,
    pricing: &ModelPricing,
) -> CostBreakdown {
    let uncached = prompt_tokens.saturating_sub(cached_tokens);
    let input_cost = uncached as f64 * pricing.input + cached_tokens as f64 * pricing.cached;
    let output_cost = completion_tokens as f64 * pricing.output;
    CostBreakdown {
        input_cost,
        output_cost,
        total: input_cost + output_cost,
    }
}

/// Per-token pricing in USD for realtime/live multimodal models.
#[derive(Debug, Clone, Copy)]
pub struct LiveModelPricing {
    pub text_input: f64,
    pub text_cached: f64,
    pub text_output: f64,
    pub audio_input: f64,
    pub audio_cached: f64,
    pub audio_output: f64,
    pub image_input: f64,
    pub image_cached: f64,
}

const LIVE_PRICING_TABLE: &[(&str, LiveModelPricing)] = &[
    (
        "gpt-realtime-1.5",
        LiveModelPricing {
            text_input: 4.0e-6,
            text_cached: 0.4e-6,
            text_output: 16.0e-6,
            audio_input: 32.0e-6,
            audio_cached: 0.4e-6,
            audio_output: 64.0e-6,
            image_input: 5.0e-6,
            image_cached: 0.5e-6,
        },
    ),
    (
        "gpt-realtime",
        LiveModelPricing {
            text_input: 4.0e-6,
            text_cached: 0.4e-6,
            text_output: 16.0e-6,
            audio_input: 32.0e-6,
            audio_cached: 0.4e-6,
            audio_output: 64.0e-6,
            image_input: 5.0e-6,
            image_cached: 0.5e-6,
        },
    ),
    (
        "gpt-realtime-mini",
        LiveModelPricing {
            text_input: 0.6e-6,
            text_cached: 0.06e-6,
            text_output: 2.4e-6,
            audio_input: 10.0e-6,
            audio_cached: 0.3e-6,
            audio_output: 20.0e-6,
            image_input: 0.6e-6,
            image_cached: 0.06e-6,
        },
    ),
    (
        "gpt-4o-realtime-preview",
        LiveModelPricing {
            text_input: 5.0e-6,
            text_cached: 2.5e-6,
            text_output: 20.0e-6,
            audio_input: 40.0e-6,
            audio_cached: 2.5e-6,
            audio_output: 80.0e-6,
            image_input: 5.0e-6,
            image_cached: 2.5e-6,
        },
    ),
    (
        "gemini-3.1-flash-live-preview",
        LiveModelPricing {
            text_input: 0.75e-6,
            text_cached: 0.75e-6,
            text_output: 4.5e-6,
            audio_input: 3.0e-6,
            audio_cached: 3.0e-6,
            audio_output: 12.0e-6,
            image_input: 1.0e-6,
            image_cached: 1.0e-6,
        },
    ),
    (
        "gemini-2.5-flash-native-audio-preview-12-2025",
        LiveModelPricing {
            text_input: 0.5e-6,
            text_cached: 0.5e-6,
            text_output: 3.0e-6,
            audio_input: 3.0e-6,
            audio_cached: 3.0e-6,
            audio_output: 12.0e-6,
            image_input: 1.0e-6,
            image_cached: 1.0e-6,
        },
    ),
];

fn find_live_pricing(model: &str) -> Option<LiveModelPricing> {
    let model = model.rsplit('/').next().unwrap_or(model);
    for &(key, pricing) in LIVE_PRICING_TABLE {
        if model == key {
            return Some(pricing);
        }
    }
    LIVE_PRICING_TABLE
        .iter()
        .filter(|(key, _)| model_key_matches(model, key))
        .max_by_key(|(key, _)| key.len())
        .map(|(_, pricing)| *pricing)
}

fn billed_input_cost(tokens: u64, cached: u64, input_rate: f64, cached_rate: f64) -> f64 {
    let cached = cached.min(tokens);
    tokens.saturating_sub(cached) as f64 * input_rate + cached as f64 * cached_rate
}

pub fn calculate_live_cost(usage: &LiveUsageSnapshot) -> Option<CostBreakdown> {
    if let Some(pricing) = find_live_pricing(&usage.model) {
        let has_details = usage.input_text_tokens
            + usage.input_audio_tokens
            + usage.input_image_tokens
            + usage.cached_text_tokens
            + usage.cached_audio_tokens
            + usage.cached_image_tokens
            + usage.output_text_tokens
            + usage.output_audio_tokens
            > 0;
        if has_details {
            let input_cost = billed_input_cost(
                usage.input_text_tokens,
                usage.cached_text_tokens,
                pricing.text_input,
                pricing.text_cached,
            ) + billed_input_cost(
                usage.input_audio_tokens,
                usage.cached_audio_tokens,
                pricing.audio_input,
                pricing.audio_cached,
            ) + billed_input_cost(
                usage.input_image_tokens,
                usage.cached_image_tokens,
                pricing.image_input,
                pricing.image_cached,
            );
            let output_cost = (usage.output_text_tokens + usage.thinking_tokens) as f64
                * pricing.text_output
                + usage.output_audio_tokens as f64 * pricing.audio_output;
            return Some(CostBreakdown {
                input_cost,
                output_cost,
                total: input_cost + output_cost,
            });
        }

        let cached = usage.cached_tokens.min(usage.input_tokens);
        let input_cost = usage.input_tokens.saturating_sub(cached) as f64 * pricing.audio_input
            + cached as f64 * pricing.audio_cached;
        let output_cost =
            (usage.output_tokens + usage.thinking_tokens) as f64 * pricing.audio_output;
        return Some(CostBreakdown {
            input_cost,
            output_cost,
            total: input_cost + output_cost,
        });
    }

    find_pricing(&usage.model).map(|pricing| {
        calculate_cost(
            usage.input_tokens,
            usage.output_tokens + usage.thinking_tokens,
            usage.cached_tokens,
            &pricing,
        )
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostBreakdown {
    pub input_cost: f64,
    pub output_cost: f64,
    pub total: f64,
}

// ── Usage snapshot ─────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageSnapshot {
    pub provider: String,
    pub model: String,
    pub tokens_used: u64,
    pub context_window: u64,
    pub usage_pct: f64,
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub cached_tokens: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CostSummary {
    pub lines: Vec<CostLine>,
    pub total: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostLine {
    pub label: String,
    pub model: String,
    pub cost: f64,
    pub input_cost: f64,
    pub output_cost: f64,
}

// ── Live usage snapshot ───────────────────────────────────────────

/// Usage snapshot for live models (Gemini Live / OpenAI Realtime).
/// Separate from `UsageSnapshot` because live models report thinking_tokens
/// and don't have a context window concept.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LiveUsageSnapshot {
    pub provider: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    pub total_tokens: u64,
    pub thinking_tokens: u64,
    #[serde(default)]
    pub input_text_tokens: u64,
    #[serde(default)]
    pub input_audio_tokens: u64,
    #[serde(default)]
    pub input_image_tokens: u64,
    #[serde(default)]
    pub cached_text_tokens: u64,
    #[serde(default)]
    pub cached_audio_tokens: u64,
    #[serde(default)]
    pub cached_image_tokens: u64,
    #[serde(default)]
    pub output_text_tokens: u64,
    #[serde(default)]
    pub output_audio_tokens: u64,
}

// ── Token history entry ────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenHistoryEntry {
    pub turn: u64,
    pub tokens: u64,
}

// ── Source labels ──────────────────────────────────────────────────

fn source_label(source: &str) -> &str {
    match source {
        "system" => "\u{2139}", // ℹ
        "worker" => "Model",
        "agent" => "Run",
        "server" => "Servr",
        "presence" => "Prsnc",
        "live" => "Live",
        "sub" => "Sub",
        "orch" => "Orch",
        // External agent sources pass through as-is (e.g. "Codex", "Claude Code")
        other if !other.is_empty() => other,
        _ => "\u{2139}",
    }
}

// ── Verbosity ──────────────────────────────────────────────────────

fn visible_levels(verbosity: &str) -> &'static [&'static str] {
    match verbosity {
        "verbose" => &[
            "info", "model", "agent", "error", "warn", "subagent", "detail", "presence",
        ],
        "debug" => &[
            "info", "model", "agent", "error", "warn", "subagent", "detail", "debug", "presence",
        ],
        _ => &[
            "info", "model", "agent", "error", "warn", "subagent", "presence",
        ], // normal
    }
}

const COLLAPSE_LINE_THRESHOLD: usize = 3;
const COLLAPSE_CHAR_THRESHOLD: usize = 300;
const MAX_LOG_ENTRIES: usize = 10000;

// ── Log entry (stored for re-filtering) ────────────────────────────

#[derive(Debug, Clone)]
struct LogEntry {
    ts: String,
    level: String,
    source: String,
    content: String,
    session_id: Option<String>,
    kind: Option<String>,
    output_id: Option<String>,
    collapsible: bool,
    turn: Option<u64>,
    user_turn_index: Option<u32>,
    superseded: bool,
    replacement_for_user_turn_index: Option<u32>,
}

// ── AppState ───────────────────────────────────────────────────────

pub struct AppState {
    // Status bar
    provider: String,
    model: String,
    turn: u64,
    budget_pct: f64,
    autonomy: String,
    session_id: String,
    phase: String,

    // Approval
    pending_approval_id: Option<u64>,

    // Logs
    log_buffer: Vec<LogEntry>,
    verbosity: String,
    /// When set, `add_log_with_images` uses this as the timestamp for
    /// emitted entries instead of the wallclock.  Used by replay so the
    /// historical `ts` from session.jsonl flows through the live rendering
    /// path.  Live callers pass `None` and wallclock is used as before.
    ///
    /// Set at the top of `handle_event` when the inbound message carries
    /// a `ts` field; cleared by the guard returned from `begin_replay_ts`.
    replay_ts: Option<String>,
    event_session_id: Option<String>,

    // Usage
    main_usage: Option<UsageSnapshot>,
    session_main_usage: std::collections::HashMap<String, UsageSnapshot>,
    presence_usage: Option<UsageSnapshot>,
    live_usage: Option<LiveUsageSnapshot>,
    token_history: Vec<TokenHistoryEntry>,
    last_total_tokens: u64,

    // Active tab (for badge logic)
    active_tab: String,

    // Displays
    known_displays: Vec<u64>, // display_id

    // Recordings
    known_recordings: Vec<String>,

    /// Tracks files changed during this session for the Changes sub-tab.
    pub changed_files: std::collections::HashMap<String, FileChangeEntry>,

    /// In-flight mid-turn steer messages keyed by client-generated id.
    /// Populated when the browser sees `steer_requested`, updated on
    /// `steer_queued`, and removed on `steer_delivered`. The UI renders
    /// the remaining entries as a strip above the activity log so the
    /// user can see which interjections are still pending.
    pub queued_steers: std::collections::HashMap<String, QueuedSteer>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            provider: String::new(),
            model: String::new(),
            turn: 0,
            budget_pct: 0.0,
            autonomy: "Medium".to_string(),
            session_id: String::new(),
            phase: "idle".to_string(),
            pending_approval_id: None,
            log_buffer: Vec::new(),
            verbosity: "normal".to_string(),
            replay_ts: None,
            event_session_id: None,
            main_usage: None,
            session_main_usage: std::collections::HashMap::new(),
            presence_usage: None,
            live_usage: None,
            token_history: Vec::new(),
            last_total_tokens: 0,
            active_tab: "activity".to_string(),
            known_displays: Vec::new(),
            known_recordings: Vec::new(),
            changed_files: std::collections::HashMap::new(),
            queued_steers: std::collections::HashMap::new(),
        }
    }

    /// Notify the state which tab is active (for badge logic).
    pub fn set_active_tab(&mut self, tab: &str) -> Vec<UiCommand> {
        self.active_tab = tab.to_string();
        let mut cmds = Vec::new();
        if tab == "activity" {
            cmds.push(UiCommand::HideBadge {
                tab: "activity".into(),
            });
        }
        cmds
    }

    /// Select which agent session should drive session-scoped UI updates.
    pub fn select_session(&mut self, session_id: &str) -> Vec<UiCommand> {
        self.session_id = session_id.to_string();
        let mut cmds = Vec::new();
        if let Some(usage) = self.session_main_usage.get(session_id).cloned() {
            self.budget_pct = usage.usage_pct;
            self.main_usage = Some(usage.clone());
            cmds.push(UiCommand::UpdateStatusBar {
                provider: None,
                model: None,
                turn: None,
                budget_pct: Some(usage.usage_pct),
                autonomy: None,
                session_id: None,
                external_agent: None,
            });
        }
        cmds.push(self.build_usage_command_for_session(Some(session_id)));
        cmds
    }

    /// Change verbosity and return commands to re-filter visible logs.
    pub fn set_verbosity(&mut self, level: &str) -> Vec<UiCommand> {
        self.verbosity = level.to_string();
        // Re-emit all logs with new visibility
        let mut cmds = vec![UiCommand::ClearLogs];
        let visible = visible_levels(level);
        let mut last_turn: Option<u64> = None;

        for entry in &self.log_buffer {
            if !visible.contains(&entry.level.as_str()) {
                continue;
            }
            // Turn separator
            if let Some(t) = entry.turn {
                if last_turn != Some(t) {
                    cmds.push(UiCommand::AddTurnSeparator { turn: t });
                    last_turn = Some(t);
                }
            }
            cmds.push(UiCommand::AddLogEntry {
                ts: entry.ts.clone(),
                level: entry.level.clone(),
                source: entry.source.clone(),
                content: entry.content.clone(),
                session_id: entry.session_id.clone(),
                kind: entry.kind.clone(),
                output_id: entry.output_id.clone(),
                collapsible: entry.collapsible,
                turn: None, // separator already handled
                user_turn_index: entry.user_turn_index,
                superseded: entry.superseded,
                replacement_for_user_turn_index: entry.replacement_for_user_turn_index,
                images: vec![],
            });
        }
        cmds
    }

    /// Process a raw server message and return UI commands.
    pub fn handle_message(&mut self, msg: &serde_json::Value) -> Vec<UiCommand> {
        let t = msg.get("t").and_then(|v| v.as_str());

        match t {
            Some("term") => {
                if let Some(d) = msg["d"].as_str() {
                    vec![UiCommand::TermData {
                        base64: d.to_string(),
                    }]
                } else {
                    vec![]
                }
            }
            Some("state_snapshot") => self.handle_state_snapshot(msg),
            Some("log_replay") => {
                let entries = msg.get("entries").and_then(|v| v.as_array());
                match entries {
                    Some(arr) => self.handle_log_replay(arr),
                    None => vec![],
                }
            }
            _ => {
                // OutboundEvent (has "event" field)
                if msg.get("event").is_some() {
                    self.handle_event(msg)
                } else {
                    vec![]
                }
            }
        }
    }

    /// Bootstrap from state_snapshot.
    fn handle_state_snapshot(&mut self, msg: &serde_json::Value) -> Vec<UiCommand> {
        let mut cmds = Vec::new();
        let s = match msg.get("state") {
            Some(s) => s,
            None => return cmds,
        };

        let turn = s["turn"].as_u64().unwrap_or(0);
        let budget_pct = s["budget_pct"].as_f64().unwrap_or(0.0);
        let phase = s["phase"].as_str().unwrap_or("idle");

        self.turn = turn;
        self.budget_pct = budget_pct;
        self.phase = phase.to_string();

        cmds.push(UiCommand::UpdateStatusBar {
            provider: None,
            model: None,
            turn: Some(turn),
            budget_pct: Some(budget_pct),
            autonomy: None,
            session_id: None,
            external_agent: None,
        });

        // Provider/model from config
        if let Some(cfg) = msg.get("config") {
            if let Some(p) = cfg["provider"].as_str() {
                self.provider = p.to_string();
                cmds.push(UiCommand::UpdateStatusBar {
                    provider: Some(p.to_string()),
                    model: None,
                    turn: None,
                    budget_pct: None,
                    autonomy: None,
                    session_id: None,
                    external_agent: None,
                });
            }
            if let Some(m) = cfg["model"].as_str() {
                self.model = m.to_string();
                cmds.push(UiCommand::UpdateStatusBar {
                    provider: None,
                    model: Some(m.to_string()),
                    turn: None,
                    budget_pct: None,
                    autonomy: None,
                    session_id: None,
                    external_agent: None,
                });
            }
        }

        // Session ID
        if let Some(sid) = msg["session_id"].as_str() {
            self.session_id = sid.to_string();
            cmds.push(UiCommand::UpdateStatusBar {
                provider: None,
                model: None,
                turn: None,
                budget_pct: None,
                autonomy: None,
                session_id: Some(sid.to_string()),
                external_agent: None,
            });
        }

        cmds.push(UiCommand::SetPhase {
            phase: phase.to_string(),
        });

        // Restore pending approval
        if let Some(pa) = s.get("pending_approval") {
            if let Some(id) = pa["id"].as_u64() {
                if id > 0 {
                    self.pending_approval_id = Some(id);
                    let command = pa["command_preview"].as_str().unwrap_or("").to_string();
                    let category = pa["category"].as_str().unwrap_or("").to_string();
                    cmds.push(UiCommand::ShowApproval {
                        id,
                        command: command.clone(),
                        category,
                        session_id: None,
                    });
                    cmds.extend(self.add_log(
                        "warn",
                        &format!("Approval required: {}", command),
                        None,
                        "worker",
                    ));
                }
            }
        }

        cmds
    }

    /// Replay historical log entries on connect.
    ///
    /// The gateway converts each session.jsonl line into an `OutboundEvent`
    /// JSON object (matching the live broadcast shape) and prepends a
    /// `replay_start` marker carrying persisted provider/model/autonomy.
    /// This function clears the log buffer, seeds the status bar from the
    /// marker, and delegates every other entry to `handle_event` so the
    /// live rendering path is the single source of truth.
    fn handle_log_replay(&mut self, entries: &[serde_json::Value]) -> Vec<UiCommand> {
        let mut cmds = vec![UiCommand::ClearLogs];
        self.log_buffer.clear();

        for entry in entries {
            if entry.get("event").and_then(|v| v.as_str()) == Some("replay_start") {
                if let Some(p) = entry.get("provider").and_then(|v| v.as_str()) {
                    self.provider = p.to_string();
                    cmds.push(UiCommand::UpdateStatusBar {
                        provider: Some(p.to_string()),
                        model: None,
                        turn: None,
                        budget_pct: None,
                        autonomy: None,
                        session_id: None,
                        external_agent: None,
                    });
                }
                if let Some(m) = entry.get("model").and_then(|v| v.as_str()) {
                    self.model = m.to_string();
                    cmds.push(UiCommand::UpdateStatusBar {
                        provider: None,
                        model: Some(m.to_string()),
                        turn: None,
                        budget_pct: None,
                        autonomy: None,
                        session_id: None,
                        external_agent: None,
                    });
                }
                if let Some(a) = entry.get("autonomy").and_then(|v| v.as_str()) {
                    self.autonomy = a.to_string();
                    cmds.push(UiCommand::UpdateStatusBar {
                        provider: None,
                        model: None,
                        turn: None,
                        budget_pct: None,
                        autonomy: Some(a.to_string()),
                        session_id: None,
                        external_agent: None,
                    });
                }
                continue;
            }

            // All other entries are `OutboundEvent` JSON — run the live path.
            cmds.extend(self.handle_event(entry));
        }

        cmds
    }

    /// Handle an OutboundEvent.
    ///
    /// If `msg` carries a `ts` field (injected by replay), that timestamp is
    /// threaded through to the log entries emitted by this handler.  Live
    /// broadcasts don't include `ts`, so wallclock is used in that path.
    fn handle_event(&mut self, msg: &serde_json::Value) -> Vec<UiCommand> {
        let event = msg["event"].as_str().unwrap_or("");
        // Replay-path timestamp override: set for the duration of this call
        // so every add_log_with_images emission picks up the historical ts.
        self.replay_ts = msg.get("ts").and_then(|v| v.as_str()).map(String::from);
        self.event_session_id = msg
            .get("session_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);
        let current_session_event = self.current_event_matches_selected_session();
        let mut cmds = Vec::new();

        match event {
            "turn_started" => {
                let turn = msg["turn"].as_u64().unwrap_or(0);
                let budget = msg["budget_pct"].as_f64().unwrap_or(0.0);

                cmds.extend(self.add_log(
                    "info",
                    &format!("Turn {} started", turn),
                    Some(turn),
                    "system",
                ));
                if current_session_event {
                    self.turn = turn;
                    self.budget_pct = budget;
                    cmds.push(UiCommand::UpdateStatusBar {
                        provider: None,
                        model: None,
                        turn: Some(turn),
                        budget_pct: Some(budget),
                        autonomy: None,
                        session_id: self.event_session_id.clone(),
                        external_agent: None,
                    });
                    cmds.push(UiCommand::SetPhase {
                        phase: "thinking".into(),
                    });
                    self.phase = "thinking".to_string();

                    // Token history delta
                    if let Some(ref usage) = self.main_usage {
                        if turn > 1 {
                            let delta = usage.tokens_used.saturating_sub(self.last_total_tokens);
                            self.token_history.push(TokenHistoryEntry {
                                turn: turn - 1,
                                tokens: delta,
                            });
                            self.last_total_tokens = usage.tokens_used;
                        }
                    }
                }
            }

            "model_response" => {
                let summary = msg["summary"].as_str().unwrap_or("");
                let reasoning = msg["reasoning_summary"].as_str();
                let source = msg["source"].as_str().unwrap_or("worker");
                // Skip spurious empty "Model response" rows.  Replay emits
                // a reasoning-only ModelResponse (empty content + reasoning
                // set) when the on-disk session has a `reasoning` event
                // without a preceding model_response; rendering "Model
                // response" in that case is drift.
                if !summary.is_empty() {
                    cmds.extend(self.add_log("model", summary, None, source));
                } else if reasoning.is_none() {
                    // Live path with no summary and no reasoning — keep the
                    // old placeholder so debugging stays possible.
                    cmds.extend(self.add_log("model", "Model response", None, source));
                }
                if let Some(rs) = reasoning {
                    if !rs.is_empty() {
                        cmds.extend(self.add_log(
                            "detail",
                            &format!("Reasoning: {}", rs),
                            None,
                            source,
                        ));
                    }
                }
            }

            "model_response_delta" => {
                // Streaming text — no UI command needed
            }

            "agent_started" => {
                let preview = msg["commands_preview"].as_str().unwrap_or("");
                let source = msg["source"].as_str().unwrap_or("agent");
                if !self.known_displays.is_empty() {
                    cmds.extend(self.add_log("detail", "Running on display", None, source));
                }
                cmds.extend(self.add_log("agent", preview, None, source));
                if current_session_event {
                    cmds.push(UiCommand::SetPhase {
                        phase: "running".into(),
                    });
                    self.phase = "running".to_string();
                }
            }

            "agent_output" => {
                let source = msg["source"].as_str().unwrap_or("agent");
                let output_id = msg["output_id"].as_str().map(str::to_string);
                if let Some(stdout) = msg["stdout"].as_str() {
                    if !stdout.is_empty() {
                        let out = format_agent_output(stdout);
                        if !out.text.is_empty() || !out.images.is_empty() {
                            cmds.extend(self.add_log_with_metadata(
                                "agent",
                                &out.text,
                                None,
                                source,
                                out.images,
                                Some("agent_output"),
                                output_id.clone(),
                                None,
                                false,
                                None,
                            ));
                        }
                    }
                }
                if let Some(stderr) = msg["stderr"].as_str() {
                    if !stderr.is_empty() {
                        cmds.extend(self.add_log_with_metadata(
                            "warn",
                            stderr,
                            None,
                            source,
                            Vec::new(),
                            Some("agent_output"),
                            output_id.clone(),
                            None,
                            false,
                            None,
                        ));
                    }
                }
                if current_session_event {
                    cmds.push(UiCommand::SetPhase {
                        phase: "running".into(),
                    });
                    self.phase = "running".to_string();
                }
            }

            "auto_approved" => {
                let preview = msg["preview"].as_str().unwrap_or("");
                cmds.extend(self.add_log(
                    "info",
                    &format!("Auto-approved: {}", preview),
                    None,
                    "system",
                ));
            }

            "done_signal" => {
                let message = msg["message"].as_str().unwrap_or("");
                let text = if message.is_empty() {
                    "Done signal".to_string()
                } else {
                    format!("Done signal: {}", message)
                };
                cmds.extend(self.add_log("detail", &text, None, "worker"));
            }

            "context_management" => {
                let turn = msg["turn"].as_u64().unwrap_or(0);
                cmds.extend(self.add_log(
                    "info",
                    &format!("Context compacted at turn {}", turn),
                    None,
                    "system",
                ));
            }

            "budget_warning" => {
                let pct = msg["pct"].as_f64().unwrap_or(0.0);
                cmds.extend(self.add_log(
                    "warn",
                    &format!("Budget warning: {:.1}% used", pct),
                    None,
                    "system",
                ));
            }

            "budget_exhausted" => {
                let remaining = msg["remaining"].as_u64().unwrap_or(0);
                cmds.extend(self.add_log(
                    "error",
                    &format!("Budget exhausted ({} tokens remaining)", remaining),
                    None,
                    "system",
                ));
            }

            "loop_error" => {
                let message = msg["message"].as_str().unwrap_or("");
                cmds.extend(self.add_log("error", message, None, "system"));
            }

            "sub_agent_result" => {
                let summary = msg["summary"].as_str().unwrap_or("");
                cmds.extend(self.add_log("subagent", summary, None, "sub"));
            }

            "orchestrator_progress" => {
                let status = msg["status"].as_str().unwrap_or("");
                cmds.extend(self.add_log("info", status, None, "orch"));
            }

            "approval_required" => {
                let id = msg["id"].as_u64().unwrap_or(0);
                let command = msg["command"].as_str().unwrap_or("").to_string();
                let category = msg["category"].as_str().unwrap_or("").to_string();
                self.pending_approval_id = Some(id);
                self.phase = "waiting".to_string();

                cmds.push(UiCommand::ShowApproval {
                    id,
                    command: command.clone(),
                    category,
                    session_id: self.event_session_id.clone(),
                });
                cmds.push(UiCommand::SetPhase {
                    phase: "waiting".into(),
                });
                cmds.extend(self.add_log(
                    "warn",
                    &format!("Approval required: {}", command),
                    None,
                    "worker",
                ));

                if self.active_tab != "activity" {
                    cmds.push(UiCommand::ShowBadge {
                        tab: "activity".into(),
                        text: "!".into(),
                    });
                }
            }

            "ask_human" => {
                let question = msg["question"].as_str().unwrap_or("").to_string();
                self.phase = "waiting".to_string();

                cmds.push(UiCommand::ShowHumanInput {
                    question: question.clone(),
                });
                cmds.push(UiCommand::SetPhase {
                    phase: "waiting".into(),
                });
                cmds.extend(self.add_log(
                    "info",
                    &format!("Question: {}", question),
                    None,
                    "worker",
                ));

                if self.active_tab != "activity" {
                    cmds.push(UiCommand::ShowBadge {
                        tab: "activity".into(),
                        text: "?".into(),
                    });
                }
            }

            "task_complete" => {
                let reason = msg["reason"].as_str().unwrap_or("");
                let summary = msg["summary"].as_str();
                let text = match summary {
                    Some(s) if !s.is_empty() => format!("Task complete: {} \u{2014} {}", reason, s),
                    _ => format!("Task complete: {}", reason),
                };
                cmds.extend(self.add_log("info", &text, None, "worker"));
                if current_session_event {
                    self.phase = "done".to_string();
                    self.pending_approval_id = None;
                    cmds.push(UiCommand::HideAllPanels);
                    cmds.push(UiCommand::SetPhase {
                        phase: "done".into(),
                    });
                }
            }

            "interrupt_requested" => {
                // Log entry only — the `status` event carrying phase="interrupting"
                // drives the UI state transition. Keeping the log entry visible at
                // normal verbosity so users see their click was received.
                cmds.extend(self.add_log("info", "Interrupt requested", None, "system"));
            }

            "interrupted" => {
                let reason = msg["reason"]
                    .as_str()
                    .unwrap_or("user requested")
                    .to_string();
                cmds.extend(self.add_log(
                    "warn",
                    &format!("Agent interrupted: {}", reason),
                    None,
                    "system",
                ));
                if current_session_event {
                    self.phase = "interrupted".to_string();
                    self.pending_approval_id = None;
                    cmds.push(UiCommand::HideAllPanels);
                    cmds.push(UiCommand::SetPhase {
                        phase: "interrupted".into(),
                    });
                }
            }

            // ---- Mid-turn steering (interjection) ----
            //
            // The browser submits a steer by calling `send_steer(text)` in
            // WASM, which sends `{action: "steer", text, id}` to the
            // server. The backend dispatcher echoes three events back:
            //
            //   steer_requested  → we saw the request (matches what we sent)
            //   steer_queued     → backend can't deliver mid-turn, queued for turn boundary
            //   steer_delivered  → agent actually received it (mid_turn or as follow-up)
            //
            // For each event we update `queued_steers[id]` and emit a
            // `SteerStatusUpdate` UiCommand so the JS layer can refresh
            // the in-flight steer strip without reparsing raw WS messages.
            "steer_requested" => {
                let text = msg["text"].as_str().unwrap_or("").to_string();
                let id = msg["id"].as_str().unwrap_or("").to_string();
                self.queued_steers.insert(
                    id.clone(),
                    QueuedSteer {
                        text: text.clone(),
                        status: SteerStatus::Pending,
                        reason: None,
                    },
                );
                cmds.extend(self.add_log(
                    "info",
                    &format!("\u{23F3} Steer sent: {}", truncate(&text, 80)),
                    None,
                    "user",
                ));
                cmds.push(UiCommand::SteerStatusUpdate {
                    id,
                    text,
                    status: "pending".into(),
                    reason: None,
                });
            }

            "steer_queued" => {
                let id = msg["id"].as_str().unwrap_or("").to_string();
                let reason = msg["reason"].as_str().unwrap_or("").to_string();
                if let Some(q) = self.queued_steers.get_mut(&id) {
                    q.status = SteerStatus::Queued;
                    q.reason = Some(reason.clone());
                }
                cmds.extend(self.add_log(
                    "warn",
                    &format!("\u{23F0} Steer queued: {}", reason),
                    None,
                    "user",
                ));
                // Prefer the stored text so late/out-of-order queue
                // events still render the original message in the strip.
                let text = self
                    .queued_steers
                    .get(&id)
                    .map(|q| q.text.clone())
                    .unwrap_or_default();
                cmds.push(UiCommand::SteerStatusUpdate {
                    id,
                    text,
                    status: "queued".into(),
                    reason: Some(reason),
                });
            }

            "steer_delivered" => {
                let id = msg["id"].as_str().unwrap_or("").to_string();
                let mid_turn = msg["mid_turn"].as_bool().unwrap_or(false);
                let entry = self.queued_steers.remove(&id);
                let text = entry.as_ref().map(|q| q.text.clone()).unwrap_or_default();
                let where_ = if mid_turn { "mid-turn" } else { "as follow-up" };
                cmds.extend(self.add_log(
                    "info",
                    &format!(
                        "\u{2713} Steer delivered ({}): {}",
                        where_,
                        truncate(&text, 80)
                    ),
                    None,
                    "user",
                ));
                cmds.push(UiCommand::SteerStatusUpdate {
                    id,
                    text,
                    status: "delivered".into(),
                    reason: None,
                });
            }

            "round_complete" => {
                let round = msg["round"].as_u64().unwrap_or(0);
                let turns = msg["turns_in_round"].as_u64().unwrap_or(0);
                cmds.extend(self.add_log(
                    "info",
                    &format!("Round {} complete ({} turns)", round, turns),
                    None,
                    "system",
                ));
                if current_session_event {
                    self.phase = "idle".to_string();
                    cmds.push(UiCommand::SetPhase {
                        phase: "idle".into(),
                    });
                }
            }

            "status" => {
                if !current_session_event {
                    self.replay_ts = None;
                    self.event_session_id = None;
                    return cmds;
                }
                let sb = UiCommand::UpdateStatusBar {
                    provider: msg["provider"].as_str().map(String::from),
                    model: msg["model"].as_str().map(String::from),
                    turn: msg["turn"].as_u64(),
                    budget_pct: msg["budget_pct"].as_f64(),
                    autonomy: msg["autonomy"].as_str().map(String::from),
                    session_id: msg["session_id"].as_str().map(String::from),
                    external_agent: msg["external_agent"].as_str().map(String::from),
                };
                if let Some(p) = msg["provider"].as_str() {
                    self.provider = p.to_string();
                }
                if let Some(m) = msg["model"].as_str() {
                    self.model = m.to_string();
                }
                if let Some(t) = msg["turn"].as_u64() {
                    self.turn = t;
                }
                if let Some(a) = msg["autonomy"].as_str() {
                    self.autonomy = a.to_string();
                }
                if let Some(s) = msg["session_id"].as_str() {
                    self.session_id = s.to_string();
                }
                // Drop the binding and push
                cmds.push(sb);
                if let Some(phase) = msg["phase"].as_str() {
                    self.phase = phase.to_string();
                    cmds.push(UiCommand::SetPhase {
                        phase: phase.to_string(),
                    });
                }
            }

            "external_agent_changed" => {
                cmds.push(UiCommand::UpdateStatusBar {
                    provider: None,
                    model: None,
                    turn: None,
                    budget_pct: None,
                    autonomy: None,
                    session_id: None,
                    external_agent: Some(msg["agent"].as_str().unwrap_or("").to_string()),
                });
            }

            "codex_thread_action_result" => {
                let action = msg
                    .get("action")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let success = msg
                    .get("success")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let message = msg
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                cmds.push(UiCommand::CodexThreadActionResult {
                    action: action.clone(),
                    success,
                    message: message.clone(),
                });
                // Also surface in the Activity log so users have a record
                // beyond the transient toast.
                let level = if success { "info" } else { "warn" };
                let line = if success {
                    format!("Codex /{}: {}", action, message)
                } else {
                    format!("Codex /{}: FAILED — {}", action, message)
                };
                cmds.extend(self.add_log(level, &line, None, "server"));
            }

            "codex_config_changed" => {
                let command = msg
                    .get("command")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let sandbox = msg
                    .get("sandbox")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let approval_policy = msg
                    .get("approval_policy")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let model = msg.get("model").and_then(|v| v.as_str()).map(String::from);
                let model_cleared = msg
                    .get("model_cleared")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let reasoning_effort = msg
                    .get("reasoning_effort")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let reasoning_effort_cleared = msg
                    .get("reasoning_effort_cleared")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let web_search = msg.get("web_search").and_then(|v| v.as_bool());
                let network_access = msg.get("network_access").and_then(|v| v.as_bool());
                let writable_roots =
                    msg.get("writable_roots")
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str().map(String::from))
                                .collect()
                        });
                cmds.push(UiCommand::CodexConfigChanged {
                    command,
                    sandbox,
                    approval_policy,
                    model,
                    model_cleared,
                    reasoning_effort,
                    reasoning_effort_cleared,
                    web_search,
                    network_access,
                    writable_roots,
                });
            }

            "gemini_thread_action_result" => {
                let action = msg
                    .get("action")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let success = msg
                    .get("success")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let message = msg
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                cmds.push(UiCommand::GeminiThreadActionResult {
                    action: action.clone(),
                    success,
                    message: message.clone(),
                });
                let level = if success { "info" } else { "warn" };
                let line = if success {
                    format!("Gemini /{}: {}", action, message)
                } else {
                    format!("Gemini /{}: FAILED — {}", action, message)
                };
                cmds.extend(self.add_log(level, &line, None, "server"));
            }

            "gemini_config_changed" => {
                let model = msg.get("model").and_then(|v| v.as_str()).map(String::from);
                let model_cleared = msg
                    .get("model_cleared")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let approval_mode = msg
                    .get("approval_mode")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let sandbox = msg.get("sandbox").and_then(|v| v.as_bool());
                let extensions = msg.get("extensions").and_then(|v| v.as_array()).map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                });
                let allowed_mcp_servers = msg
                    .get("allowed_mcp_servers")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    });
                let include_directories = msg
                    .get("include_directories")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    });
                let debug = msg.get("debug").and_then(|v| v.as_bool());
                cmds.push(UiCommand::GeminiConfigChanged {
                    model,
                    model_cleared,
                    approval_mode,
                    sandbox,
                    extensions,
                    allowed_mcp_servers,
                    include_directories,
                    debug,
                });
            }

            "usage" | "usage_update" => {
                if let Some(main) = msg.get("main") {
                    if let Ok(u) = serde_json::from_value::<UsageSnapshot>(main.clone()) {
                        if let Some(sid) = self.event_session_id.clone() {
                            self.session_main_usage.insert(sid, u.clone());
                        }
                        cmds.extend(self.add_log(
                            "detail",
                            &format!(
                                "tokens: {} / {} ({:.1}%)",
                                format_number(u.tokens_used),
                                format_number(u.context_window),
                                u.usage_pct
                            ),
                            None,
                            "system",
                        ));
                        if current_session_event {
                            self.budget_pct = u.usage_pct;
                            cmds.push(UiCommand::UpdateStatusBar {
                                provider: None,
                                model: None,
                                turn: None,
                                budget_pct: Some(u.usage_pct),
                                autonomy: None,
                                session_id: self.event_session_id.clone(),
                                external_agent: None,
                            });
                            self.main_usage = Some(u);
                        }
                    }
                }
                if let Some(presence) = msg.get("presence") {
                    if let Ok(u) = serde_json::from_value::<UsageSnapshot>(presence.clone()) {
                        self.presence_usage = Some(u);
                    }
                }
                cmds.push(self.build_usage_command_for_session(self.event_session_id.as_deref()));
            }

            "display_ready" => {
                let display_id = msg["display_id"].as_u64().unwrap_or(0);
                let width = msg["width"].as_u64().unwrap_or(0);
                let height = msg["height"].as_u64().unwrap_or(0);
                cmds.extend(self.add_log(
                    "info",
                    &format!("Display :{} ready", display_id),
                    None,
                    "system",
                ));
                if !self.known_displays.iter().any(|&id| id == display_id) {
                    self.known_displays.push(display_id);
                }
                cmds.push(UiCommand::AddDisplay {
                    display_id,
                    width,
                    height,
                });
            }

            "display_taken" => {
                let id = msg["display_id"].as_u64().unwrap_or(0);
                cmds.extend(self.add_log(
                    "info",
                    &format!("Display :{} in use", id),
                    None,
                    "system",
                ));
            }

            "display_released" => {
                let id = msg["display_id"].as_u64().unwrap_or(0);
                let note = msg["note"].as_str().unwrap_or("");
                let text = if note.is_empty() {
                    format!("Display :{} released", id)
                } else {
                    format!("Display :{} released: {}", id, note)
                };
                cmds.extend(self.add_log("info", &text, None, "system"));
            }

            "recording_started" => {
                let stream = msg["stream_name"].as_str().unwrap_or("").to_string();
                cmds.extend(self.add_log(
                    "info",
                    &format!("Recording started: {}", stream),
                    None,
                    "system",
                ));
                if !self.known_recordings.contains(&stream) {
                    self.known_recordings.push(stream.clone());
                }
                cmds.push(UiCommand::AddRecording {
                    stream_name: stream,
                });
            }

            "recording_stopped" => {
                let stream = msg["stream_name"].as_str().unwrap_or("").to_string();
                cmds.extend(self.add_log(
                    "info",
                    &format!("Recording stopped: {}", stream),
                    None,
                    "system",
                ));
                self.known_recordings.retain(|s| s != &stream);
                cmds.push(UiCommand::RemoveRecording {
                    stream_name: stream,
                });
            }

            "recording_error" => {
                let stream = msg["stream_name"].as_str().unwrap_or("").to_string();
                let message = msg["message"].as_str().unwrap_or("").to_string();
                cmds.extend(self.add_log(
                    "warn",
                    &format!("Recording error ({}): {}", stream, message),
                    None,
                    "system",
                ));
                cmds.push(UiCommand::RecordingError {
                    stream_name: stream,
                    message,
                });
            }

            "session_started" => {
                let session_id = msg["session_id"].as_str().unwrap_or("").to_string();
                let task = msg["task"].as_str().map(|s| s.to_string());
                self.session_id = session_id.clone();
                cmds.extend(self.add_log(
                    "info",
                    &format!("Session started: {}", session_id),
                    None,
                    "system",
                ));
                cmds.push(UiCommand::SessionStarted { session_id, task });
            }

            "session_attached" => {
                let session_id = msg["session_id"].as_str().unwrap_or("").to_string();
                let source = msg["source"].as_str().unwrap_or("").to_string();
                self.session_id = session_id.clone();
                cmds.extend(self.add_log(
                    "info",
                    &format!("Session attached: {} ({})", session_id, source),
                    None,
                    "system",
                ));
                cmds.push(UiCommand::SessionAttached { session_id, source });
            }

            "session_ended" => {
                let session_id = msg["session_id"].as_str().unwrap_or("").to_string();
                let reason = msg["reason"].as_str().unwrap_or("").to_string();
                if self.session_id == session_id {
                    self.session_id.clear();
                }
                cmds.extend(self.add_log(
                    "info",
                    &format!("Session ended: {} — {}", session_id, reason),
                    None,
                    "system",
                ));
                cmds.push(UiCommand::SessionEnded { session_id, reason });
            }

            "debug_screen_ready" => {
                let display_id = msg["display_id"].as_u64().unwrap_or(0);
                cmds.extend(self.add_log(
                    "info",
                    &format!("Debug screen ready on :{}", display_id),
                    None,
                    "system",
                ));
                cmds.push(UiCommand::DebugScreenReady { display_id });
            }

            "debug_screen_torn_down" => {
                let display_id = msg["display_id"].as_u64().unwrap_or(0);
                cmds.extend(self.add_log(
                    "info",
                    &format!("Debug screen :{} torn down", display_id),
                    None,
                    "system",
                ));
                cmds.push(UiCommand::DebugScreenTornDown);
            }

            "command_result" => {
                let ok = msg["ok"].as_bool().unwrap_or(false);
                let action = msg["action"].as_str().unwrap_or("");
                let message = msg["message"].as_str().unwrap_or("");
                let level = if ok { "detail" } else { "error" };
                cmds.extend(self.add_log(
                    level,
                    &format!("[{}] {}", action, message),
                    None,
                    "system",
                ));
            }

            "presence_log" => {
                let level = msg["level"].as_str().unwrap_or("info");
                let message = msg["message"].as_str().unwrap_or("");
                cmds.extend(self.add_log(level, message, None, "presence"));
            }

            "presence_usage_update" => {
                let u = UsageSnapshot {
                    provider: msg["provider"].as_str().unwrap_or("").to_string(),
                    model: msg["model"].as_str().unwrap_or("").to_string(),
                    tokens_used: msg["total_tokens"].as_u64().unwrap_or(0),
                    context_window: msg["context_window"].as_u64().unwrap_or(0),
                    usage_pct: msg["usage_pct"].as_f64().unwrap_or(0.0),
                    prompt_tokens: msg["prompt_tokens"].as_u64().unwrap_or(0),
                    completion_tokens: msg["completion_tokens"].as_u64().unwrap_or(0),
                    cached_tokens: msg["cached_tokens"].as_u64().unwrap_or(0),
                };
                self.presence_usage = Some(u);
                cmds.push(self.build_usage_command());
            }

            "live_usage_update" => {
                self.live_usage = Some(LiveUsageSnapshot {
                    provider: msg["provider"].as_str().unwrap_or("").to_string(),
                    model: msg["model"].as_str().unwrap_or("").to_string(),
                    input_tokens: msg["input_tokens"].as_u64().unwrap_or(0),
                    output_tokens: msg["output_tokens"].as_u64().unwrap_or(0),
                    cached_tokens: msg["cached_tokens"].as_u64().unwrap_or(0),
                    total_tokens: msg["total_tokens"].as_u64().unwrap_or(0),
                    thinking_tokens: msg["thinking_tokens"].as_u64().unwrap_or(0),
                    input_text_tokens: msg["input_text_tokens"].as_u64().unwrap_or(0),
                    input_audio_tokens: msg["input_audio_tokens"].as_u64().unwrap_or(0),
                    input_image_tokens: msg["input_image_tokens"].as_u64().unwrap_or(0),
                    cached_text_tokens: msg["cached_text_tokens"].as_u64().unwrap_or(0),
                    cached_audio_tokens: msg["cached_audio_tokens"].as_u64().unwrap_or(0),
                    cached_image_tokens: msg["cached_image_tokens"].as_u64().unwrap_or(0),
                    output_text_tokens: msg["output_text_tokens"].as_u64().unwrap_or(0),
                    output_audio_tokens: msg["output_audio_tokens"].as_u64().unwrap_or(0),
                });
                cmds.push(self.build_usage_command());
            }

            "user_transcript" => {
                let text = msg["text"].as_str().unwrap_or("");
                cmds.extend(self.add_log("presence", &format!("[You] {}", text), None, "live"));
            }

            "human_response_sent" => {
                cmds.extend(self.add_log("detail", "Human response sent", None, "system"));
            }

            "safety_cap_reached" => {
                cmds.extend(self.add_log("error", "Safety cap reached", None, "system"));
                cmds.push(UiCommand::SetPhase {
                    phase: "done".into(),
                });
                self.phase = "done".to_string();
            }

            "log_entry" => {
                let level = msg["level"].as_str().unwrap_or("info");
                let source = msg["source"].as_str().unwrap_or("system");
                let content = msg["content"].as_str().unwrap_or("");
                let turn = msg["turn"].as_u64();
                let user_turn_index = msg["user_turn_index"]
                    .as_u64()
                    .and_then(|v| u32::try_from(v).ok());
                let superseded = msg["superseded"].as_bool().unwrap_or(false);
                let replacement_for_user_turn_index = msg["replacement_for_user_turn_index"]
                    .as_u64()
                    .and_then(|v| u32::try_from(v).ok());
                let kind = msg["kind"].as_str();
                cmds.extend(self.add_log_with_metadata(
                    level,
                    content,
                    turn,
                    source,
                    Vec::new(),
                    kind,
                    None,
                    user_turn_index,
                    superseded,
                    replacement_for_user_turn_index,
                ));
            }

            "user_message_rewind" => {
                let session_id = msg
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                let user_turn_index = msg["user_turn_index"]
                    .as_u64()
                    .and_then(|v| u32::try_from(v).ok())
                    .unwrap_or(0);
                let turns_removed = msg["turns_removed"]
                    .as_u64()
                    .and_then(|v| u32::try_from(v).ok())
                    .unwrap_or(0);
                if user_turn_index > 0 && turns_removed > 0 {
                    self.mark_log_buffer_rewound(
                        session_id.as_deref(),
                        user_turn_index,
                        turns_removed,
                    );
                    cmds.push(UiCommand::MarkActivityContextRewind {
                        session_id: session_id.clone(),
                        user_turn_index,
                        turns_removed,
                    });
                }
                let content = if turns_removed == 1 {
                    "Rewound 1 user turn; overwritten entries are no longer active context."
                        .to_string()
                } else {
                    format!(
                        "Rewound {turns_removed} user turns; overwritten entries are no longer active context."
                    )
                };
                cmds.extend(self.add_log_with_metadata(
                    "warn",
                    &content,
                    None,
                    "system",
                    Vec::new(),
                    Some("rollback_marker"),
                    None,
                    None,
                    false,
                    None,
                ));
            }

            "file_changed" => {
                let path = msg["path"].as_str().unwrap_or("").to_string();
                let kind = msg["kind"].as_str().unwrap_or("modified").to_string();
                let added = msg["lines_added"].as_u64().unwrap_or(0);
                let removed = msg["lines_removed"].as_u64().unwrap_or(0);

                self.changed_files.insert(
                    path.clone(),
                    FileChangeEntry {
                        kind: kind.clone(),
                        lines_added: added,
                        lines_removed: removed,
                    },
                );

                cmds.extend(self.add_log(
                    "detail",
                    &format!(
                        "{} {} (+{}/-{})",
                        match kind.as_str() {
                            "created" => "+",
                            "deleted" => "-",
                            _ => "*",
                        },
                        path,
                        added,
                        removed
                    ),
                    None,
                    "fs",
                ));

                cmds.push(UiCommand::FileChanged {
                    path,
                    kind,
                    lines_added: added,
                    lines_removed: removed,
                });
            }

            // ---- Upload store events ----
            //
            // Server broadcasts one `upload_ready` after `POST /api/upload`
            // finishes, and `upload_deleted` after `DELETE /api/uploads/<id>`.
            // We pass the full descriptor through to JS so it can render the
            // pending-attachments row without round-tripping to the
            // `/api/uploads` list endpoint.
            "upload_ready" => {
                let descriptor = msg
                    .get("descriptor")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let name = descriptor
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("(upload)");
                let dest = descriptor
                    .get("destination")
                    .and_then(|v| v.as_str())
                    .unwrap_or("task");
                cmds.extend(self.add_log(
                    "detail",
                    &format!("upload ready: {} ({})", name, dest),
                    None,
                    "fs",
                ));
                cmds.push(UiCommand::UploadReady { descriptor });
            }
            "upload_deleted" => {
                let id = msg
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                cmds.extend(self.add_log("detail", &format!("upload deleted: {}", id), None, "fs"));
                cmds.push(UiCommand::UploadDeleted { id });
            }

            // ---- Session history events (rollback / redo / prune) ----
            //
            // The authoritative timeline lives on the server. These events
            // are just a signal that it changed — the JS layer re-fetches
            // `/api/session/current/history` and re-renders the Timeline UI.
            // We also surface short log entries at `detail` verbosity so
            // users can trace rollback/redo chronologically in the Activity
            // log.
            "snapshot_created" => {
                let id = msg["round_id"].as_u64().unwrap_or(0);
                cmds.push(UiCommand::HistoryChanged);
                cmds.extend(self.add_log(
                    "detail",
                    &format!("Snapshot created (round {})", id),
                    None,
                    "fs",
                ));
            }
            "rolled_back" => {
                let from = msg["from_id"].as_u64().unwrap_or(0);
                let to = msg["to_id"].as_u64().unwrap_or(0);
                let n = msg["files_reverted"].as_u64().unwrap_or(0);
                cmds.push(UiCommand::HistoryChanged);
                cmds.extend(self.add_log(
                    "info",
                    &format!(
                        "Rolled back from round {} to round {} ({} files reverted)",
                        from, to, n
                    ),
                    None,
                    "fs",
                ));
            }
            // Conversation-side rollback. Fired in addition to (or instead
            // of) `rolled_back` when the user asked to revert the agent's
            // memory. The backend distinguishes two methods: "truncated"
            // means the provider supports in-place truncation; "session-
            // reset" means we had to scrap the whole session (Claude Code,
            // Gemini) — surface the heavier warning for that case.
            "conversation_rolled_back" => {
                let round_id = msg["round_id"].as_str().unwrap_or("").to_string();
                let turns_removed = msg["turns_removed"].as_u64().unwrap_or(0);
                let backend = msg["backend"].as_str().unwrap_or("").to_string();
                let method = msg["method"].as_str().unwrap_or("truncated").to_string();
                let summary = if method == "session-reset" {
                    format!(
                        "Session reset for {} ({} turns lost)",
                        backend, turns_removed
                    )
                } else {
                    format!("Conversation rolled back ({} turns removed)", turns_removed)
                };
                // round_id is kept in the message for future use (e.g.
                // correlating with the timeline); we don't currently need
                // it for the log text itself.
                let _ = round_id;
                cmds.extend(self.add_log("warn", &summary, None, "system"));
            }
            "redone" => {
                let to = msg["to_id"].as_u64().unwrap_or(0);
                cmds.push(UiCommand::HistoryChanged);
                cmds.extend(self.add_log("info", &format!("Redone to round {}", to), None, "fs"));
            }
            "history_pruned" => {
                let removed = msg["branches_removed"].as_u64().unwrap_or(0);
                let bytes = msg["bytes_freed"].as_u64().unwrap_or(0);
                let mb = bytes as f64 / (1024.0 * 1024.0);
                cmds.push(UiCommand::HistoryChanged);
                cmds.extend(self.add_log(
                    "info",
                    &format!(
                        "History pruned: {} branches removed, {:.1} MB freed",
                        removed, mb
                    ),
                    None,
                    "fs",
                ));
            }
            // ---- Peer registry push events ----
            //
            // Forwarded as opaque JSON: the WASM layer doesn't interpret
            // the snapshot — it hands it to JS, which treats it as the
            // same shape as a `/api/peers` list entry (one update path
            // for both surfaces).
            "peer_added" => {
                cmds.push(UiCommand::PeerAdded {
                    peer: msg["peer"].clone(),
                });
            }

            "peer_removed" => {
                cmds.push(UiCommand::PeerRemoved {
                    id: msg["id"].as_str().unwrap_or("").to_string(),
                });
            }

            "peer_state_changed" => {
                cmds.push(UiCommand::PeerStateChanged {
                    peer: msg["peer"].clone(),
                });
            }

            // ---- Per-peer event stream (phase B) ----
            //
            // Wraps a peer's PeerEvent for per-host rendering. The
            // WASM layer can't `use crate::peer::PeerEvent` from
            // intendant's binary (cross-crate boundary), so dispatch
            // happens on the inner JSON's `event` discriminator. Each
            // emitted UiCommand carries the peer's id as a typed
            // first-class field (vs. the legacy `c.host_id = hostId`
            // mutation hack the secondary path uses) so JS routes
            // straightforwardly per-host.
            "peer_event_forwarded" => {
                let host_id = msg["peer_id"].as_str().unwrap_or("").to_string();
                let payload = &msg["payload"];
                cmds.extend(render_peer_event(&host_id, payload));
            }

            _ => {
                // Unknown events at debug level
                let text = format!(
                    "[{}] {}",
                    event,
                    serde_json::to_string(msg).unwrap_or_default()
                );
                cmds.extend(self.add_log("debug", &text, None, "system"));
            }
        }

        // Clear replay timestamp override so subsequent live calls revert
        // to wallclock.
        self.replay_ts = None;
        self.event_session_id = None;
        cmds
    }

    fn mark_log_buffer_rewound(
        &mut self,
        session_id: Option<&str>,
        user_turn_index: u32,
        turns_removed: u32,
    ) {
        let end_turn = user_turn_index
            .saturating_add(turns_removed)
            .saturating_sub(1);
        let mut in_rewound_region = false;
        for entry in &mut self.log_buffer {
            if let Some(sid) = session_id {
                if entry.session_id.as_deref() != Some(sid) {
                    continue;
                }
            }
            if entry.kind.as_deref() == Some("rollback_marker") {
                continue;
            }
            if let Some(turn) = entry.user_turn_index {
                if !in_rewound_region && turn >= user_turn_index && turn <= end_turn {
                    in_rewound_region = true;
                } else if in_rewound_region && turn > end_turn {
                    break;
                }
            }
            if in_rewound_region {
                entry.superseded = true;
            }
        }
    }

    fn current_event_matches_selected_session(&self) -> bool {
        match self.event_session_id.as_deref() {
            Some(sid) if !self.session_id.is_empty() => sid == self.session_id,
            _ => true,
        }
    }

    /// Add a log entry, respecting verbosity. Returns AddLogEntry command if visible.
    fn add_log(
        &mut self,
        level: &str,
        content: &str,
        turn: Option<u64>,
        source: &str,
    ) -> Vec<UiCommand> {
        self.add_log_with_images(level, content, turn, source, Vec::new())
    }

    /// Add a log entry with optional images, respecting verbosity.
    ///
    /// When `self.replay_ts` is set (during replay), that timestamp is used
    /// for the emitted entry instead of the wallclock.  Callers in live mode
    /// leave `replay_ts` as `None` so wallclock is used as before.
    fn add_log_with_images(
        &mut self,
        level: &str,
        content: &str,
        turn: Option<u64>,
        source: &str,
        images: Vec<String>,
    ) -> Vec<UiCommand> {
        self.add_log_with_metadata(
            level, content, turn, source, images, None, None, None, false, None,
        )
    }

    fn add_log_with_metadata(
        &mut self,
        level: &str,
        content: &str,
        turn: Option<u64>,
        source: &str,
        images: Vec<String>,
        kind: Option<&str>,
        output_id: Option<String>,
        user_turn_index: Option<u32>,
        superseded: bool,
        replacement_for_user_turn_index: Option<u32>,
    ) -> Vec<UiCommand> {
        // Trim replay timestamps to HH:MM:SS so they render identically to
        // the old replay path (which truncated via `ts[..8.min(ts.len())]`).
        let ts = match &self.replay_ts {
            Some(t) => {
                let end = 8.min(t.len());
                t[..end].to_string()
            }
            None => current_time_str(),
        };
        let source_str = source_label(source).to_string();
        let is_collapsible = !images.is_empty()
            || content.split('\n').count() > COLLAPSE_LINE_THRESHOLD
            || content.len() > COLLAPSE_CHAR_THRESHOLD;
        let kind_string = kind.map(str::to_string);
        let discardable_output = kind == Some("agent_output") && output_id.is_some();
        let buffered_content = if discardable_output {
            String::new()
        } else {
            content.to_string()
        };

        let entry = LogEntry {
            ts: ts.clone(),
            level: level.to_string(),
            source: source_str.clone(),
            session_id: self.event_session_id.clone(),
            content: buffered_content,
            kind: kind_string.clone(),
            output_id: output_id.clone(),
            collapsible: is_collapsible,
            turn,
            user_turn_index,
            superseded,
            replacement_for_user_turn_index,
        };
        self.log_buffer.push(entry);

        // Cap buffer
        if self.log_buffer.len() > MAX_LOG_ENTRIES {
            self.log_buffer.remove(0);
        }

        let visible = visible_levels(&self.verbosity);
        if !visible.contains(&level) {
            return vec![];
        }

        let mut cmds = Vec::new();
        if let Some(t) = turn {
            cmds.push(UiCommand::AddTurnSeparator { turn: t });
        }
        cmds.push(UiCommand::AddLogEntry {
            ts,
            level: level.to_string(),
            source: source_str,
            content: content.to_string(),
            session_id: self.event_session_id.clone(),
            kind: kind_string,
            output_id,
            collapsible: is_collapsible,
            turn: None, // separator already emitted
            user_turn_index,
            superseded,
            replacement_for_user_turn_index,
            images,
        });
        cmds
    }

    /// Update live model usage and return commands to re-render the Usage tab.
    pub fn update_live_usage(&mut self, usage: LiveUsageSnapshot) -> Vec<UiCommand> {
        self.live_usage = Some(usage);
        vec![self.build_usage_command()]
    }

    /// Build an UpdateUsage command from current state.
    fn build_usage_command(&self) -> UiCommand {
        let selected_session = (!self.session_id.is_empty()).then_some(self.session_id.as_str());
        self.build_usage_command_for_session(selected_session)
    }

    fn build_usage_command_for_session(&self, session_id: Option<&str>) -> UiCommand {
        let selected_main = session_id
            .and_then(|sid| self.session_main_usage.get(sid))
            .or(self.main_usage.as_ref());
        let main_json = selected_main.and_then(|u| serde_json::to_string(u).ok());
        let presence_json = self
            .presence_usage
            .as_ref()
            .and_then(|u| serde_json::to_string(u).ok());
        let live_json = self
            .live_usage
            .as_ref()
            .and_then(|u| serde_json::to_string(u).ok());

        // Cost calculation
        let cost_json = {
            let mut summary = CostSummary::default();
            if let Some(u) = selected_main {
                if let Some(pricing) = find_pricing(&u.model) {
                    let cost = calculate_cost(
                        u.prompt_tokens,
                        u.completion_tokens,
                        u.cached_tokens,
                        &pricing,
                    );
                    summary.total += cost.total;
                    summary.lines.push(CostLine {
                        label: "Main Model".into(),
                        model: u.model.clone(),
                        cost: cost.total,
                        input_cost: cost.input_cost,
                        output_cost: cost.output_cost,
                    });
                }
            }
            if let Some(ref u) = self.presence_usage {
                if let Some(pricing) = find_pricing(&u.model) {
                    let cost = calculate_cost(
                        u.prompt_tokens,
                        u.completion_tokens,
                        u.cached_tokens,
                        &pricing,
                    );
                    summary.total += cost.total;
                    summary.lines.push(CostLine {
                        label: "Presence Model".into(),
                        model: u.model.clone(),
                        cost: cost.total,
                        input_cost: cost.input_cost,
                        output_cost: cost.output_cost,
                    });
                }
            }
            if let Some(ref u) = self.live_usage {
                if let Some(cost) = calculate_live_cost(u) {
                    summary.total += cost.total;
                    summary.lines.push(CostLine {
                        label: "Live Model".into(),
                        model: u.model.clone(),
                        cost: cost.total,
                        input_cost: cost.input_cost,
                        output_cost: cost.output_cost,
                    });
                }
            }
            if summary.lines.is_empty() {
                None
            } else {
                serde_json::to_string(&summary).ok()
            }
        };

        let history_json = if self.token_history.is_empty() {
            None
        } else {
            serde_json::to_string(&self.token_history).ok()
        };

        UiCommand::UpdateUsage {
            session_id: session_id.map(str::to_string),
            main_json,
            presence_json,
            live_json,
            cost_json,
            history_json,
        }
    }

    /// Process an approval action. Returns commands to send to server + update UI.
    pub fn approve_action(&mut self, action: &str) -> Option<(u64, Vec<UiCommand>)> {
        let id = self.pending_approval_id.take()?;
        let mut cmds = vec![
            UiCommand::HideAllPanels,
            UiCommand::SetPhase {
                phase: "running".into(),
            },
        ];
        cmds.extend(self.add_log("info", &format!("Action: {}", action), None, "system"));
        self.phase = "running".to_string();
        Some((id, cmds))
    }

    /// Process a human response. Returns commands.
    pub fn human_response(&mut self, text: &str) -> Vec<UiCommand> {
        let mut cmds = vec![
            UiCommand::HideAllPanels,
            UiCommand::SetPhase {
                phase: "thinking".into(),
            },
        ];
        cmds.extend(self.add_log("info", &format!("Response: {}", text), None, "system"));
        self.phase = "thinking".to_string();
        cmds
    }

    /// Process a follow-up message. Returns commands.
    pub fn follow_up(&mut self, text: &str) -> Vec<UiCommand> {
        let mut cmds = vec![
            UiCommand::HideAllPanels,
            UiCommand::SetPhase {
                phase: "thinking".into(),
            },
        ];
        cmds.extend(self.add_log("info", &format!("Follow-up: {}", text), None, "system"));
        self.phase = "thinking".to_string();
        cmds
    }

    /// Get the current pending approval id.
    pub fn pending_approval_id(&self) -> Option<u64> {
        self.pending_approval_id
    }
}

// ── Helpers ────────────────────────────────────────────────────────

// Agent output parsing is shared with the native TUI/MCP paths and lives in
// `presence_core::format` so there is exactly one parser for both targets.
// It is re-exported here so the existing call sites below don't need to
// qualify `presence_core::` at every use.
pub use presence_core::{format_agent_output, FormattedOutput};

/// Truncate a string to `max` characters, appending an ellipsis if truncated.
/// Char-boundary safe so we never split in the middle of a UTF-8 codepoint.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{}\u{2026}", cut)
    }
}

/// Translate one peer-emitted [`crate::peer::PeerEvent`] (in JSON form
/// — WASM can't import the type across the crate boundary) into zero
/// or more [`UiCommand`]s tagged with the originating peer's id.
///
/// Most variants render as host-tagged log entries via `UiCommand::PeerLog`
/// — the lean PeerEvent vocabulary doesn't carry the source-level richness
/// AppEvent does, so log entries pull `level` and `source` either from
/// the event's own fields (Log) or from synthetic mappings (ActivityKind
/// → source, MessageRole → level, etc.). The four variants that get
/// typed UiCommands of their own (`PeerUsage`, `PeerApprovalRequested`,
/// `PeerApprovalResolved`) feed dedicated dashboard surfaces (Stats
/// host picker, per-peer pending approvals).
///
/// Variants intentionally dropped:
/// - `connected`/`disconnected`/`status_changed` are already covered by
///   the registry's `PeerAdded`/`PeerRemoved`/`PeerStateChanged` push
///   events — surfacing them again here would duplicate.
/// - `task_update` has no per-peer task list yet to render into.
pub fn render_peer_event(host_id: &str, payload: &serde_json::Value) -> Vec<UiCommand> {
    let event = payload["event"].as_str().unwrap_or("");
    let now = current_time_str();
    let host = host_id.to_string();
    match event {
        // Connection lifecycle covered by PeerStateChanged.
        "connected" | "disconnected" | "status_changed" => vec![],

        "log" => {
            let ts = payload["ts"].as_str().unwrap_or(&now).to_string();
            let level = payload["level"].as_str().unwrap_or("info").to_string();
            let source = payload["source"].as_str().unwrap_or("peer").to_string();
            let content = payload["message"].as_str().unwrap_or("").to_string();
            vec![UiCommand::PeerLog {
                host_id: host,
                ts,
                level,
                source,
                content,
            }]
        }

        "approval_requested" => {
            let req = &payload["request"];
            let id = req["request_id"].as_str().unwrap_or("").to_string();
            let command = req["preview"].as_str().unwrap_or("").to_string();
            let category = req["category"].as_str().unwrap_or("").to_string();
            vec![UiCommand::PeerApprovalRequested {
                host_id: host,
                id,
                command,
                category,
            }]
        }

        "approval_resolved" => {
            let id = payload["request_id"].as_str().unwrap_or("").to_string();
            vec![UiCommand::PeerApprovalResolved { host_id: host, id }]
        }

        "usage" => {
            let snapshot = payload["snapshot"].clone();
            vec![UiCommand::PeerUsage {
                host_id: host,
                snapshot,
            }]
        }

        "activity_started" => {
            // Synthetic source from kind so the per-peer log threads
            // multi-turn activities under a meaningful column.
            let kind = payload["kind"].as_str().unwrap_or("activity");
            let label = payload["label"].as_str().unwrap_or("").to_string();
            vec![UiCommand::PeerLog {
                host_id: host,
                ts: now,
                level: "info".to_string(),
                source: peer_activity_source(kind).to_string(),
                content: label,
            }]
        }

        "activity_progress" => {
            let text = payload["text"].as_str().unwrap_or("");
            if text.is_empty() {
                // Heartbeat per PeerEvent::ActivityProgress docs — drop.
                return vec![];
            }
            vec![UiCommand::PeerLog {
                host_id: host,
                ts: now,
                level: "info".to_string(),
                source: "activity".to_string(),
                content: text.to_string(),
            }]
        }

        "activity_completed" => {
            // Render the outcome compactly. Failed/Cancelled get a
            // warn level so they stand out in the per-peer log.
            let outcome = &payload["outcome"];
            let status = outcome["status"].as_str().unwrap_or("");
            let (level, content) = match status {
                "success" => ("info", "Activity completed".to_string()),
                "failed" => (
                    "warn",
                    format!(
                        "Activity failed: {}",
                        outcome["message"].as_str().unwrap_or("(no message)")
                    ),
                ),
                "cancelled" => ("warn", "Activity cancelled".to_string()),
                "suspended" => (
                    "warn",
                    format!(
                        "Activity suspended: {}",
                        outcome["reason"].as_str().unwrap_or("(no reason)")
                    ),
                ),
                _ => ("info", format!("Activity ended ({status})")),
            };
            vec![UiCommand::PeerLog {
                host_id: host,
                ts: now,
                level: level.to_string(),
                source: "activity".to_string(),
                content,
            }]
        }

        "message" => {
            // Surface text/reasoning content as log entries; non-text
            // content (image, parts) collapses to a placeholder for
            // now since the per-peer log can't render them inline.
            let role = payload["role"].as_str().unwrap_or("assistant");
            let content_obj = &payload["content"];
            let ctype = content_obj["type"].as_str().unwrap_or("");
            let text = match ctype {
                "text" | "reasoning" => content_obj["text"].as_str().unwrap_or("").to_string(),
                "image" => "(image attachment)".to_string(),
                "parts" => "(multi-part content)".to_string(),
                _ => return vec![],
            };
            if text.is_empty() {
                return vec![];
            }
            vec![UiCommand::PeerLog {
                host_id: host,
                ts: now,
                level: peer_message_level(role).to_string(),
                source: peer_message_source(role).to_string(),
                content: text,
            }]
        }

        "capability_engaged" | "capability_released" => {
            let cap = payload["capability"].clone();
            let cap_label = cap["kind"].as_str().unwrap_or("capability").to_string();
            let verb = if event == "capability_engaged" {
                "engaged"
            } else {
                "released"
            };
            vec![UiCommand::PeerLog {
                host_id: host,
                ts: now,
                level: "detail".to_string(),
                source: "capability".to_string(),
                content: format!("{cap_label} {verb}"),
            }]
        }

        "session_started" => {
            let session = &payload["session"];
            let label = session["label"].as_str().unwrap_or("");
            let session_id = session["session_id"].as_str().unwrap_or("");
            let content = if label.is_empty() {
                format!("Session started: {session_id}")
            } else {
                format!("Session started: {label} ({session_id})")
            };
            vec![UiCommand::PeerLog {
                host_id: host,
                ts: now,
                level: "info".to_string(),
                source: "session".to_string(),
                content,
            }]
        }

        "session_ended" => {
            let session_id = payload["session_id"].as_str().unwrap_or("");
            let reason = payload["reason"].as_str().unwrap_or("");
            vec![UiCommand::PeerLog {
                host_id: host,
                ts: now,
                level: "info".to_string(),
                source: "session".to_string(),
                content: format!("Session ended ({reason}): {session_id}"),
            }]
        }

        // task_update: not surfaced per-peer yet (no task list UI).
        "task_update" => vec![],

        // WebRTC signaling forwarded to the per-peer RTCPeerConnection
        // glue in JS. The `signal` payload is opaque here — JS reads
        // its `kind` field to dispatch to setRemoteDescription /
        // addIceCandidate / close-cleanup.
        "webrtc_signal" => {
            let display_id = payload["display_id"].as_u64().unwrap_or(0) as u32;
            let session_id = payload["session_id"].as_str().unwrap_or("").to_string();
            let signal = payload["signal"].clone();
            if session_id.is_empty() {
                // No session_id means we can't route to the right
                // RTCPeerConnection; drop with a diagnostic log
                // entry so the operator can spot the protocol drift.
                return vec![UiCommand::PeerLog {
                    host_id: host,
                    ts: now,
                    level: "warn".to_string(),
                    source: "webrtc".to_string(),
                    content: format!("WebRTC signal missing session_id: {payload}"),
                }];
            }
            vec![UiCommand::PeerWebRtcSignal {
                host_id: host,
                display_id,
                session_id,
                signal,
            }]
        }

        // Unknown / forward-compat: render as a debug log so the user
        // can see something arrived rather than dropping silently.
        other => vec![UiCommand::PeerLog {
            host_id: host,
            ts: now,
            level: "debug".to_string(),
            source: "peer".to_string(),
            content: format!("[{other}] {}", payload),
        }],
    }
}

/// Map a `PeerEvent::ActivityKind` wire string to a per-peer log
/// source label so the activity column groups consistently in the UI.
fn peer_activity_source(kind: &str) -> &'static str {
    match kind {
        "model_turn" => "model",
        "tool_call" => "tool",
        "sub_agent" => "sub-agent",
        "delegated_task" => "task",
        _ => "activity",
    }
}

fn peer_message_level(role: &str) -> &'static str {
    match role {
        "assistant" => "model",
        "user" => "info",
        "system" => "info",
        "tool" => "info",
        _ => "info",
    }
}

fn peer_message_source(role: &str) -> &'static str {
    match role {
        "assistant" => "model",
        "user" => "user",
        "system" => "system",
        "tool" => "tool",
        _ => "peer",
    }
}

/// Format a number with commas (e.g. 1234567 → "1,234,567").
fn format_number(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}

/// Get current time as HH:MM:SS string.
/// In WASM, uses js_sys::Date. In tests, returns a fixed string.
#[cfg(target_arch = "wasm32")]
fn current_time_str() -> String {
    let d = js_sys::Date::new_0();
    format!(
        "{:02}:{:02}:{:02}",
        d.get_hours(),
        d.get_minutes(),
        d.get_seconds()
    )
}

#[cfg(not(target_arch = "wasm32"))]
fn current_time_str() -> String {
    "00:00:00".to_string()
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pricing_exact_match() {
        let p = find_pricing("claude-opus-4-6").unwrap();
        assert!((p.input - 5.0e-6).abs() < 1e-12);
        assert!((p.output - 25.0e-6).abs() < 1e-12);
    }

    #[test]
    fn pricing_prefix_match() {
        // Model with extra suffix
        let p = find_pricing("gemini-2.5-flash-preview").unwrap();
        assert!((p.input - 0.3e-6).abs() < 1e-12);
    }

    #[test]
    fn pricing_not_found() {
        assert!(find_pricing("unknown-model-xyz").is_none());
    }

    #[test]
    fn cost_calculation() {
        let pricing = ModelPricing {
            input: 1.0e-6,
            cached: 0.1e-6,
            output: 2.0e-6,
        };
        let cost = calculate_cost(1000, 500, 200, &pricing);
        // uncached = 800, input_cost = 800*1e-6 + 200*0.1e-6 = 0.00082
        // output_cost = 500*2e-6 = 0.001
        assert!((cost.input_cost - 0.00082).abs() < 1e-10);
        assert!((cost.output_cost - 0.001).abs() < 1e-10);
        assert!((cost.total - 0.00182).abs() < 1e-10);
    }

    #[test]
    fn format_number_with_commas() {
        assert_eq!(format_number(0), "0");
        assert_eq!(format_number(999), "999");
        assert_eq!(format_number(1000), "1,000");
        assert_eq!(format_number(1234567), "1,234,567");
    }

    #[test]
    fn truncate_short_and_long() {
        // Short strings pass through unchanged
        assert_eq!(truncate("abc", 80), "abc");
        // Long strings get an ellipsis — exactly max chars + the \u{2026}
        let long = "a".repeat(100);
        let t = truncate(&long, 80);
        assert_eq!(t.chars().count(), 81);
        assert!(t.ends_with('\u{2026}'));
        // UTF-8 safety: truncating mid-multibyte-char must not panic
        // and must respect char boundaries, not byte boundaries.
        let utf8 = "αβγδε".to_string();
        assert_eq!(truncate(&utf8, 3), "αβγ\u{2026}");
    }

    // `format_agent_output` tests live in `presence_core::format` — it's
    // the shared parser used by both this crate and the native TUI/MCP paths.

    #[test]
    fn app_state_new_defaults() {
        let s = AppState::new();
        assert_eq!(s.phase, "idle");
        assert_eq!(s.turn, 0);
        assert_eq!(s.verbosity, "normal");
        assert!(s.pending_approval_id.is_none());
        assert!(s.main_usage.is_none());
        assert!(s.log_buffer.is_empty());
    }

    #[test]
    fn handle_term_data() {
        let mut s = AppState::new();
        let msg = json!({"t": "term", "d": "SGVsbG8="});
        let cmds = s.handle_message(&msg);
        assert_eq!(cmds.len(), 1);
        match &cmds[0] {
            UiCommand::TermData { base64 } => assert_eq!(base64, "SGVsbG8="),
            _ => panic!("expected TermData"),
        }
    }

    #[test]
    fn handle_state_snapshot() {
        let mut s = AppState::new();
        let msg = json!({
            "t": "state_snapshot",
            "state": { "turn": 5, "budget_pct": 0.3, "phase": "thinking" },
            "config": { "provider": "openai", "model": "gpt-5" },
            "session_id": "abc-123-def"
        });
        let cmds = s.handle_message(&msg);
        assert_eq!(s.turn, 5);
        assert_eq!(s.phase, "thinking");
        assert_eq!(s.provider, "openai");
        assert_eq!(s.model, "gpt-5");
        assert!(!cmds.is_empty());
        // Should contain SetPhase
        assert!(cmds
            .iter()
            .any(|c| matches!(c, UiCommand::SetPhase { phase } if phase == "thinking")));
    }

    #[test]
    fn handle_state_snapshot_with_approval() {
        let mut s = AppState::new();
        let msg = json!({
            "t": "state_snapshot",
            "state": {
                "turn": 1,
                "budget_pct": 0.0,
                "phase": "waiting_approval",
                "pending_approval": { "id": 42, "command_preview": "rm -rf /tmp", "category": "Destructive" }
            }
        });
        let cmds = s.handle_message(&msg);
        assert_eq!(s.pending_approval_id, Some(42));
        assert!(cmds
            .iter()
            .any(|c| matches!(c, UiCommand::ShowApproval { id: 42, .. })));
    }

    #[test]
    fn handle_event_turn_started() {
        let mut s = AppState::new();
        let msg = json!({"event": "turn_started", "turn": 3, "budget_pct": 15.5});
        let cmds = s.handle_message(&msg);
        assert_eq!(s.turn, 3);
        assert!(cmds
            .iter()
            .any(|c| matches!(c, UiCommand::SetPhase { phase } if phase == "thinking")));
    }

    #[test]
    fn session_scoped_turn_started_tags_log_and_updates_matching_session() {
        let mut s = AppState::new();
        s.session_id = "sess-a".to_string();
        let msg =
            json!({"event": "turn_started", "turn": 4, "budget_pct": 22.5, "session_id": "sess-a"});
        let cmds = s.handle_message(&msg);

        assert_eq!(s.turn, 4);
        assert_eq!(s.phase, "thinking");
        assert!(cmds.iter().any(|c| matches!(
            c,
            UiCommand::UpdateStatusBar { session_id, .. }
                if session_id.as_deref() == Some("sess-a")
        )));
        assert!(cmds.iter().any(|c| matches!(
            c,
            UiCommand::AddLogEntry { session_id, content, .. }
                if session_id.as_deref() == Some("sess-a")
                    && content == "Turn 4 started"
        )));
    }

    #[test]
    fn session_scoped_background_turn_logs_without_switching_selected_session() {
        let mut s = AppState::new();
        s.session_id = "sess-a".to_string();
        s.turn = 2;
        s.phase = "running".to_string();
        let msg =
            json!({"event": "turn_started", "turn": 8, "budget_pct": 40.0, "session_id": "sess-b"});
        let cmds = s.handle_message(&msg);

        assert_eq!(s.session_id, "sess-a");
        assert_eq!(s.turn, 2);
        assert_eq!(s.phase, "running");
        assert!(!cmds
            .iter()
            .any(|c| matches!(c, UiCommand::SetPhase { phase } if phase == "thinking")));
        assert!(!cmds
            .iter()
            .any(|c| matches!(c, UiCommand::UpdateStatusBar { turn: Some(8), .. })));
        assert!(cmds.iter().any(|c| matches!(
            c,
            UiCommand::AddLogEntry { session_id, content, .. }
                if session_id.as_deref() == Some("sess-b")
                    && content == "Turn 8 started"
        )));
    }

    #[test]
    fn session_scoped_usage_updates_selected_session_status_only() {
        let mut s = AppState::new();
        s.session_id = "sess-a".to_string();
        let msg = json!({
            "event": "usage_update",
            "session_id": "sess-a",
            "main": {
                "provider": "openai",
                "model": "gpt-5",
                "tokens_used": 1250,
                "context_window": 10000,
                "usage_pct": 12.5,
                "prompt_tokens": 1000,
                "completion_tokens": 250,
                "cached_tokens": 100
            }
        });
        let cmds = s.handle_message(&msg);

        assert_eq!(s.budget_pct, 12.5);
        assert_eq!(s.main_usage.as_ref().unwrap().tokens_used, 1250);
        assert!(cmds.iter().any(|c| matches!(
            c,
            UiCommand::UpdateStatusBar { budget_pct: Some(pct), session_id, .. }
                if (*pct - 12.5).abs() < f64::EPSILON
                    && session_id.as_deref() == Some("sess-a")
        )));
        assert!(cmds.iter().any(|c| matches!(
            c,
            UiCommand::UpdateUsage { session_id, main_json: Some(_), .. }
                if session_id.as_deref() == Some("sess-a")
        )));
    }

    #[test]
    fn session_scoped_background_usage_is_cached_without_overwriting_status() {
        let mut s = AppState::new();
        s.session_id = "sess-a".to_string();
        s.main_usage = Some(UsageSnapshot {
            tokens_used: 500,
            usage_pct: 5.0,
            ..Default::default()
        });
        s.budget_pct = 5.0;
        let msg = json!({
            "event": "usage_update",
            "session_id": "sess-b",
            "main": {
                "provider": "openai",
                "model": "gpt-5",
                "tokens_used": 4000,
                "context_window": 10000,
                "usage_pct": 40.0,
                "prompt_tokens": 3000,
                "completion_tokens": 1000,
                "cached_tokens": 0
            }
        });
        let cmds = s.handle_message(&msg);

        assert_eq!(s.session_id, "sess-a");
        assert_eq!(s.budget_pct, 5.0);
        assert_eq!(s.main_usage.as_ref().unwrap().tokens_used, 500);
        assert_eq!(s.session_main_usage["sess-b"].tokens_used, 4000);
        assert!(!cmds.iter().any(|c| {
            matches!(
                c,
                UiCommand::UpdateStatusBar { budget_pct: Some(pct), .. }
                    if (*pct - 40.0).abs() < f64::EPSILON
            )
        }));
        assert!(cmds.iter().any(|c| matches!(
            c,
            UiCommand::UpdateUsage { session_id, main_json: Some(_), .. }
                if session_id.as_deref() == Some("sess-b")
        )));
    }

    #[test]
    fn handle_event_approval_required() {
        let mut s = AppState::new();
        let msg = json!({"event": "approval_required", "id": 7, "command": "echo hi", "category": "CommandExec"});
        let cmds = s.handle_message(&msg);
        assert_eq!(s.pending_approval_id, Some(7));
        assert!(cmds
            .iter()
            .any(|c| matches!(c, UiCommand::ShowApproval { id: 7, .. })));
    }

    #[test]
    fn session_scoped_approval_carries_target_session() {
        let mut s = AppState::new();
        let msg = json!({
            "event": "approval_required",
            "id": 9,
            "command": "echo scoped",
            "category": "CommandExec",
            "session_id": "sess-b"
        });
        let cmds = s.handle_message(&msg);

        assert!(cmds.iter().any(|c| matches!(
            c,
            UiCommand::ShowApproval { id: 9, session_id, .. }
                if session_id.as_deref() == Some("sess-b")
        )));
    }

    #[test]
    fn handle_event_task_complete() {
        let mut s = AppState::new();
        s.pending_approval_id = Some(5);
        let msg = json!({"event": "task_complete", "reason": "done", "summary": "all good"});
        let cmds = s.handle_message(&msg);
        assert_eq!(s.phase, "done");
        assert!(s.pending_approval_id.is_none());
        assert!(cmds.iter().any(|c| matches!(c, UiCommand::HideAllPanels)));
    }

    #[test]
    fn handle_codex_thread_action_result_success() {
        let mut s = AppState::new();
        let msg = json!({
            "event": "codex_thread_action_result",
            "action": "compact",
            "success": true,
            "message": "conversation compaction started"
        });
        let cmds = s.handle_message(&msg);
        let mut saw_result = false;
        let mut saw_log = false;
        for c in &cmds {
            match c {
                UiCommand::CodexThreadActionResult {
                    action,
                    success: true,
                    message,
                } => {
                    assert_eq!(action, "compact");
                    assert!(message.contains("compaction"));
                    saw_result = true;
                }
                UiCommand::AddLogEntry { content, level, .. } => {
                    if content.contains("/compact") {
                        assert_eq!(level, "info");
                        saw_log = true;
                    }
                }
                _ => {}
            }
        }
        assert!(saw_result, "expected CodexThreadActionResult command");
        assert!(saw_log, "expected Activity log entry");
    }

    #[test]
    fn handle_codex_thread_action_result_failure_surfaces_as_warn() {
        let mut s = AppState::new();
        let msg = json!({
            "event": "codex_thread_action_result",
            "action": "fork",
            "success": false,
            "message": "no active Codex thread"
        });
        let cmds = s.handle_message(&msg);
        let mut saw_result = false;
        let mut saw_log = false;
        for c in &cmds {
            match c {
                UiCommand::CodexThreadActionResult {
                    action,
                    success: false,
                    message,
                } => {
                    assert_eq!(action, "fork");
                    assert!(message.contains("no active"));
                    saw_result = true;
                }
                UiCommand::AddLogEntry { content, level, .. } => {
                    if content.contains("/fork") && content.contains("FAILED") {
                        assert_eq!(level, "warn");
                        saw_log = true;
                    }
                }
                _ => {}
            }
        }
        assert!(saw_result, "expected CodexThreadActionResult (failure)");
        assert!(saw_log, "expected warn Activity log entry");
    }

    #[test]
    fn handle_codex_config_changed_full() {
        let mut s = AppState::new();
        let msg = json!({
            "event": "codex_config_changed",
            "command": "/opt/bin/codex",
            "sandbox": "danger-full-access",
            "approval_policy": "never",
            "model": "gpt-5",
            "reasoning_effort": "high",
            "web_search": true,
            "network_access": true,
            "writable_roots": ["/tmp/extra"]
        });
        let cmds = s.handle_message(&msg);
        let matched = cmds.iter().any(|c| {
            matches!(
                c,
                UiCommand::CodexConfigChanged {
                    command: Some(cmd),
                    sandbox: Some(sand),
                    approval_policy: Some(p),
                    model: Some(m),
                    model_cleared: false,
                    reasoning_effort: Some(re),
                    reasoning_effort_cleared: false,
                    web_search: Some(true),
                    network_access: Some(true),
                    writable_roots: Some(roots),
                } if cmd == "/opt/bin/codex"
                    && sand == "danger-full-access"
                    && p == "never"
                    && m == "gpt-5"
                    && re == "high"
                    && roots.len() == 1
                    && roots[0] == "/tmp/extra"
            )
        });
        assert!(
            matched,
            "expected full CodexConfigChanged command, got {:?}",
            cmds
        );
    }

    #[test]
    fn handle_codex_config_changed_model_cleared() {
        let mut s = AppState::new();
        let msg = json!({
            "event": "codex_config_changed",
            "model_cleared": true
        });
        let cmds = s.handle_message(&msg);
        let matched = cmds.iter().any(|c| {
            matches!(
                c,
                UiCommand::CodexConfigChanged {
                    command: None,
                    sandbox: None,
                    approval_policy: None,
                    model: None,
                    model_cleared: true,
                    reasoning_effort: None,
                    reasoning_effort_cleared: false,
                    web_search: None,
                    network_access: None,
                    writable_roots: None,
                }
            )
        });
        assert!(
            matched,
            "expected model-cleared CodexConfigChanged, got {:?}",
            cmds
        );
    }

    #[test]
    fn handle_codex_config_changed_reasoning_cleared() {
        let mut s = AppState::new();
        let msg = json!({
            "event": "codex_config_changed",
            "reasoning_effort_cleared": true
        });
        let cmds = s.handle_message(&msg);
        let matched = cmds.iter().any(|c| {
            matches!(
                c,
                UiCommand::CodexConfigChanged {
                    reasoning_effort: None,
                    reasoning_effort_cleared: true,
                    ..
                }
            )
        });
        assert!(
            matched,
            "expected reasoning-cleared CodexConfigChanged, got {:?}",
            cmds
        );
    }

    #[test]
    fn handle_codex_config_changed_toggles_only() {
        let mut s = AppState::new();
        let msg = json!({
            "event": "codex_config_changed",
            "web_search": false,
            "network_access": true
        });
        let cmds = s.handle_message(&msg);
        let matched = cmds.iter().any(|c| {
            matches!(
                c,
                UiCommand::CodexConfigChanged {
                    web_search: Some(false),
                    network_access: Some(true),
                    sandbox: None,
                    approval_policy: None,
                    ..
                }
            )
        });
        assert!(
            matched,
            "expected toggles-only CodexConfigChanged, got {:?}",
            cmds
        );
    }

    #[test]
    fn handle_upload_ready_carries_descriptor_through() {
        let mut s = AppState::new();
        let msg = json!({
            "event": "upload_ready",
            "descriptor": {
                "id": "abc-123",
                "name": "report.pdf",
                "mime": "application/pdf",
                "size": 4096,
                "path": "/tmp/x/report.pdf",
                "destination": "task",
                "session_id": "sess-1",
                "created_at": 1_700_000_000
            }
        });
        let cmds = s.handle_message(&msg);
        let matched = cmds.iter().any(|c| {
            matches!(
                c,
                UiCommand::UploadReady { descriptor } if
                    descriptor.get("id").and_then(|v| v.as_str()) == Some("abc-123") &&
                    descriptor.get("name").and_then(|v| v.as_str()) == Some("report.pdf")
            )
        });
        assert!(
            matched,
            "expected UploadReady with descriptor, got {:?}",
            cmds
        );
    }

    #[test]
    fn handle_upload_deleted_emits_command() {
        let mut s = AppState::new();
        let msg = json!({"event": "upload_deleted", "id": "gone-1"});
        let cmds = s.handle_message(&msg);
        let matched = cmds.iter().any(|c| {
            matches!(
                c,
                UiCommand::UploadDeleted { id } if id == "gone-1"
            )
        });
        assert!(matched, "expected UploadDeleted, got {:?}", cmds);
    }

    #[test]
    fn handle_gemini_config_changed_full() {
        let mut s = AppState::new();
        let msg = json!({
            "event": "gemini_config_changed",
            "model": "gemini-2.5-pro",
            "approval_mode": "auto_edit",
            "sandbox": true,
            "extensions": ["web", "fs"],
            "allowed_mcp_servers": ["intendant"],
            "include_directories": ["/tmp/extra"],
            "debug": false
        });
        let cmds = s.handle_message(&msg);
        let matched = cmds.iter().any(|c| {
            matches!(
                c,
                UiCommand::GeminiConfigChanged {
                    model: Some(m),
                    model_cleared: false,
                    approval_mode: Some(am),
                    sandbox: Some(true),
                    extensions: Some(exts),
                    allowed_mcp_servers: Some(servers),
                    include_directories: Some(dirs),
                    debug: Some(false),
                } if m == "gemini-2.5-pro"
                    && am == "auto_edit"
                    && exts.len() == 2
                    && servers == &vec!["intendant".to_string()]
                    && dirs == &vec!["/tmp/extra".to_string()]
            )
        });
        assert!(matched, "expected full GeminiConfigChanged, got {:?}", cmds);
    }

    #[test]
    fn handle_gemini_config_changed_model_cleared() {
        let mut s = AppState::new();
        let msg = json!({
            "event": "gemini_config_changed",
            "model_cleared": true
        });
        let cmds = s.handle_message(&msg);
        let matched = cmds.iter().any(|c| {
            matches!(
                c,
                UiCommand::GeminiConfigChanged {
                    model: None,
                    model_cleared: true,
                    approval_mode: None,
                    sandbox: None,
                    extensions: None,
                    allowed_mcp_servers: None,
                    include_directories: None,
                    debug: None,
                }
            )
        });
        assert!(
            matched,
            "expected model-cleared GeminiConfigChanged, got {:?}",
            cmds
        );
    }

    #[test]
    fn handle_gemini_thread_action_result_success() {
        let mut s = AppState::new();
        let msg = json!({
            "event": "gemini_thread_action_result",
            "action": "new",
            "success": true,
            "message": "agent torn down; next task will spawn a fresh Gemini process"
        });
        let cmds = s.handle_message(&msg);
        let saw_result = cmds.iter().any(|c| {
            matches!(
                c,
                UiCommand::GeminiThreadActionResult { action, success: true, message }
                    if action == "new"
                        && message == "agent torn down; next task will spawn a fresh Gemini process"
            )
        });
        assert!(
            saw_result,
            "expected GeminiThreadActionResult, got {:?}",
            cmds
        );
        let saw_log = cmds.iter().any(|c| {
            matches!(
                c,
                UiCommand::AddLogEntry { content, .. } if content.starts_with("Gemini /new:")
            )
        });
        assert!(saw_log, "expected Gemini log entry, got {:?}", cmds);
    }

    #[test]
    fn handle_event_agent_output() {
        let mut s = AppState::new();
        let msg = json!({"event": "agent_output", "stdout": "hello world", "stderr": ""});
        let cmds = s.handle_message(&msg);
        assert!(cmds.iter().any(
            |c| matches!(c, UiCommand::AddLogEntry { content, .. } if content == "hello world")
        ));
    }

    #[test]
    fn handle_event_usage_update() {
        let mut s = AppState::new();
        let msg = json!({
            "event": "usage_update",
            "main": {
                "provider": "openai", "model": "gpt-5",
                "tokens_used": 5000, "context_window": 128000,
                "usage_pct": 3.9, "prompt_tokens": 4000,
                "completion_tokens": 1000, "cached_tokens": 500
            }
        });
        let cmds = s.handle_message(&msg);
        assert!(s.main_usage.is_some());
        let u = s.main_usage.as_ref().unwrap();
        assert_eq!(u.tokens_used, 5000);
        assert!(cmds
            .iter()
            .any(|c| matches!(c, UiCommand::UpdateUsage { .. })));
    }

    #[test]
    fn handle_event_display_ready() {
        let mut s = AppState::new();
        let msg = json!({"event": "display_ready", "display_id": 99});
        let cmds = s.handle_message(&msg);
        assert_eq!(s.known_displays.len(), 1);
        assert_eq!(s.known_displays[0], 99);
        assert!(cmds
            .iter()
            .any(|c| matches!(c, UiCommand::AddDisplay { display_id: 99, .. })));
    }

    #[test]
    fn handle_event_session_attached() {
        let mut s = AppState::new();
        let msg = json!({
            "event": "session_attached",
            "session_id": "session-1",
            "source": "codex"
        });
        let cmds = s.handle_message(&msg);

        assert!(cmds.iter().any(|c| matches!(
            c,
            UiCommand::SessionAttached { session_id, source }
                if session_id == "session-1" && source == "codex"
        )));
        assert!(cmds.iter().any(|c| matches!(
            c,
            UiCommand::AddLogEntry { content, .. }
                if content == "Session attached: session-1 (codex)"
        )));
    }

    #[test]
    fn handle_log_replay() {
        let mut s = AppState::new();
        // Entries are OutboundEvent-shaped JSON objects (what the gateway
        // emits after running session.jsonl through
        // session_log_entry_to_app_event → app_event_to_outbound).
        let msg = json!({
            "t": "log_replay",
            "entries": [
                {"event": "replay_start", "provider": "openai", "model": "gpt-5", "autonomy": "Medium"},
                {"event": "turn_started", "turn": 1, "budget_pct": 0.0, "ts": "10:00:00"},
                {"event": "agent_output", "stdout": "hello world", "stderr": "", "ts": "10:00:01"},
                {"event": "log_entry", "level": "debug", "source": "system", "content": "internal", "ts": "10:00:02"},
            ]
        });
        let cmds = s.handle_message(&msg);
        // ClearLogs emitted at the top.
        assert!(cmds.iter().any(|c| matches!(c, UiCommand::ClearLogs)));
        // replay_start marker propagated to status bar.
        assert_eq!(s.provider, "openai");
        assert_eq!(s.model, "gpt-5");
        // Debug entry hidden at normal verbosity → 2 visible entries
        // (turn started + agent output).
        let visible_entries: Vec<_> = cmds
            .iter()
            .filter(|c| matches!(c, UiCommand::AddLogEntry { .. }))
            .collect();
        assert_eq!(visible_entries.len(), 2);
    }

    #[test]
    fn handle_log_replay_applies_replay_start_marker() {
        let mut s = AppState::new();
        let msg = json!({
            "t": "log_replay",
            "entries": [
                {"event": "replay_start", "provider": "openai", "model": "gpt-5", "autonomy": "High"},
            ]
        });
        let cmds = s.handle_message(&msg);
        assert!(cmds.iter().any(|c| matches!(c, UiCommand::ClearLogs)));
        let status_updates: Vec<_> = cmds
            .iter()
            .filter(|c| matches!(c, UiCommand::UpdateStatusBar { .. }))
            .collect();
        // Three UpdateStatusBar calls — provider, model, autonomy.
        assert_eq!(status_updates.len(), 3);
        assert_eq!(s.provider, "openai");
        assert_eq!(s.model, "gpt-5");
        assert_eq!(s.autonomy, "High");
    }

    #[test]
    fn handle_event_respects_ts_override() {
        let mut s = AppState::new();
        let msg = json!({
            "event": "model_response",
            "turn": 1,
            "summary": "hello",
            "source": "worker",
            "ts": "12:34:56.789",
        });
        let cmds = s.handle_event(&msg);
        let ts = cmds
            .iter()
            .find_map(|c| match c {
                UiCommand::AddLogEntry { ts, content, .. } if content == "hello" => {
                    Some(ts.clone())
                }
                _ => None,
            })
            .expect("model_response should emit an AddLogEntry for 'hello'");
        // Trimmed to HH:MM:SS.
        assert_eq!(ts, "12:34:56");
        // After the call, replay_ts must be cleared so subsequent live calls
        // revert to wallclock.
        assert!(s.replay_ts.is_none());
    }

    #[test]
    fn round_complete_uses_system_source_on_replay() {
        let mut s = AppState::new();
        let entries = vec![
            json!({"event": "replay_start", "provider": "x", "model": "y", "autonomy": "Medium"}),
            json!({"event": "round_complete", "round": 2, "turns_in_round": 5, "ts": "01:00:00"}),
        ];
        let cmds = s.handle_log_replay(&entries);
        let source = cmds
            .iter()
            .find_map(|c| match c {
                UiCommand::AddLogEntry {
                    source, content, ..
                } if content.contains("Round 2 complete") => Some(source.clone()),
                _ => None,
            })
            .expect("round_complete should emit an AddLogEntry");
        // "system" → source_label("system") → ℹ glyph.
        assert_eq!(source, "\u{2139}");
    }

    #[test]
    fn auto_approved_prefix_preserved_on_replay() {
        let mut s = AppState::new();
        let entries = vec![
            json!({"event": "replay_start", "provider": "p", "model": "m", "autonomy": "Medium"}),
            json!({"event": "auto_approved", "preview": "exec: ls /tmp", "ts": "01:00:00"}),
        ];
        let cmds = s.handle_log_replay(&entries);
        let entry = cmds.iter().find_map(|c| match c {
            UiCommand::AddLogEntry {
                content, source, ..
            } => {
                if content.starts_with("Auto-approved: ") {
                    Some((content.clone(), source.clone()))
                } else {
                    None
                }
            }
            _ => None,
        });
        let (content, source) =
            entry.expect("auto_approved should emit an entry with the Auto-approved: prefix");
        assert_eq!(content, "Auto-approved: exec: ls /tmp");
        // Source label for "system" is the ℹ glyph.
        assert_eq!(source, "\u{2139}");
    }

    #[test]
    fn model_response_skips_empty_summary_and_reasoning() {
        // When a reasoning event is replayed as a ModelResponse with empty
        // content and no reasoning (Risk E), the WASM must NOT emit a
        // spurious empty "Model response" row.
        let mut s = AppState::new();
        let msg = json!({
            "event": "model_response",
            "turn": 1,
            "summary": "",
            "source": "worker",
            "ts": "01:00:00",
        });
        let cmds = s.handle_event(&msg);
        // With no summary and no reasoning, live path still emits the
        // placeholder so debug output stays visible.
        let lines: Vec<_> = cmds
            .iter()
            .filter(|c| matches!(c, UiCommand::AddLogEntry { .. }))
            .collect();
        assert_eq!(lines.len(), 1);

        // But with reasoning-only (replay path for a bare `reasoning`
        // session.jsonl event) we get only the reasoning row.
        let mut s2 = AppState::new();
        s2.verbosity = "verbose".to_string();
        let msg2 = json!({
            "event": "model_response",
            "turn": 1,
            "summary": "",
            "reasoning_summary": "thinking about X",
            "source": "worker",
            "ts": "01:00:00",
        });
        let cmds2 = s2.handle_event(&msg2);
        let lines2: Vec<_> = cmds2
            .iter()
            .filter_map(|c| match c {
                UiCommand::AddLogEntry { content, .. } => Some(content.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(lines2.len(), 1);
        assert!(lines2[0].starts_with("Reasoning: "));
    }

    #[test]
    fn set_verbosity_refilters() {
        let mut s = AppState::new();
        // Add some log entries
        s.add_log("info", "visible", None, "system");
        s.add_log("debug", "hidden", None, "system");
        assert_eq!(s.log_buffer.len(), 2);

        // Switch to debug verbosity
        let cmds = s.set_verbosity("debug");
        // Should clear and re-add both
        assert!(cmds.iter().any(|c| matches!(c, UiCommand::ClearLogs)));
        let entries: Vec<_> = cmds
            .iter()
            .filter(|c| matches!(c, UiCommand::AddLogEntry { .. }))
            .collect();
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn approve_action_clears_pending() {
        let mut s = AppState::new();
        s.pending_approval_id = Some(42);
        let result = s.approve_action("approve");
        assert!(result.is_some());
        let (id, cmds) = result.unwrap();
        assert_eq!(id, 42);
        assert!(s.pending_approval_id.is_none());
        assert!(cmds.iter().any(|c| matches!(c, UiCommand::HideAllPanels)));
    }

    #[test]
    fn approve_action_none_when_no_pending() {
        let mut s = AppState::new();
        assert!(s.approve_action("approve").is_none());
    }

    #[test]
    fn follow_up_and_human_response() {
        let mut s = AppState::new();
        let cmds = s.follow_up("do more");
        assert!(cmds
            .iter()
            .any(|c| matches!(c, UiCommand::SetPhase { phase } if phase == "thinking")));

        let cmds = s.human_response("yes");
        assert!(cmds.iter().any(|c| matches!(c, UiCommand::HideAllPanels)));
    }

    #[test]
    fn token_history_on_turn_started() {
        let mut s = AppState::new();
        s.main_usage = Some(UsageSnapshot {
            tokens_used: 1000,
            ..Default::default()
        });
        s.last_total_tokens = 500;

        let msg = json!({"event": "turn_started", "turn": 3, "budget_pct": 5.0});
        s.handle_message(&msg);
        assert_eq!(s.token_history.len(), 1);
        assert_eq!(s.token_history[0].turn, 2);
        assert_eq!(s.token_history[0].tokens, 500);
    }

    #[test]
    fn badge_on_approval_when_not_activity_tab() {
        let mut s = AppState::new();
        s.active_tab = "stats".to_string();
        let msg =
            json!({"event": "approval_required", "id": 1, "command": "test", "category": "exec"});
        let cmds = s.handle_message(&msg);
        assert!(cmds
            .iter()
            .any(|c| matches!(c, UiCommand::ShowBadge { tab, .. } if tab == "activity")));
    }

    #[test]
    fn no_badge_when_on_activity_tab() {
        let mut s = AppState::new();
        s.active_tab = "activity".to_string();
        let msg =
            json!({"event": "approval_required", "id": 1, "command": "test", "category": "exec"});
        let cmds = s.handle_message(&msg);
        assert!(!cmds
            .iter()
            .any(|c| matches!(c, UiCommand::ShowBadge { .. })));
    }

    #[test]
    fn handle_event_round_complete() {
        let mut s = AppState::new();
        let msg = json!({"event": "round_complete", "round": 2, "turns_in_round": 5});
        let cmds = s.handle_message(&msg);
        assert_eq!(s.phase, "idle");
        assert!(cmds
            .iter()
            .any(|c| matches!(c, UiCommand::SetPhase { phase } if phase == "idle")));
    }

    #[test]
    fn handle_event_unknown() {
        let mut s = AppState::new();
        s.verbosity = "debug".to_string(); // enable debug to see unknown events
        let msg = json!({"event": "some_new_event", "foo": "bar"});
        let cmds = s.handle_message(&msg);
        assert!(cmds
            .iter()
            .any(|c| matches!(c, UiCommand::AddLogEntry { level, .. } if level == "debug")));
    }

    #[test]
    fn log_entry_preserves_superseded_metadata() {
        let mut s = AppState::new();
        let msg = json!({
            "event": "log_entry",
            "level": "info",
            "source": "user",
            "content": "Old prompt",
            "session_id": "session-1",
            "user_turn_index": 3,
            "superseded": true,
            "kind": "rollback_marker",
            "replacement_for_user_turn_index": 3
        });

        let cmds = s.handle_message(&msg);
        let entry = cmds
            .iter()
            .find_map(|cmd| match cmd {
                UiCommand::AddLogEntry {
                    content,
                    kind,
                    user_turn_index,
                    superseded,
                    replacement_for_user_turn_index,
                    ..
                } => Some((
                    content,
                    kind,
                    user_turn_index,
                    superseded,
                    replacement_for_user_turn_index,
                )),
                _ => None,
            })
            .expect("log_entry should emit an AddLogEntry");

        assert_eq!(entry.0, "Old prompt");
        assert_eq!(entry.1.as_deref(), Some("rollback_marker"));
        assert_eq!(*entry.2, Some(3));
        assert!(*entry.3);
        assert_eq!(*entry.4, Some(3));
    }

    #[test]
    fn live_user_message_rewind_marks_buffer_and_emits_marker() {
        let mut s = AppState::new();
        for msg in [
            json!({
                "event": "log_entry",
                "level": "info",
                "source": "user",
                "content": "Old prompt",
                "session_id": "session-1",
                "user_turn_index": 1
            }),
            json!({
                "event": "log_entry",
                "level": "model",
                "source": "codex",
                "content": "Old answer",
                "session_id": "session-1"
            }),
        ] {
            s.handle_message(&msg);
        }

        let cmds = s.handle_message(&json!({
            "event": "user_message_rewind",
            "session_id": "session-1",
            "user_turn_index": 1,
            "turns_removed": 1
        }));

        assert!(cmds.iter().any(|cmd| matches!(
            cmd,
            UiCommand::MarkActivityContextRewind {
                session_id,
                user_turn_index: 1,
                turns_removed: 1,
            } if session_id.as_deref() == Some("session-1")
        )));
        assert!(cmds.iter().any(|cmd| matches!(
            cmd,
            UiCommand::AddLogEntry {
                kind,
                content,
                session_id,
                ..
            } if kind.as_deref() == Some("rollback_marker")
                && content.contains("Rewound 1 user turn")
                && session_id.as_deref() == Some("session-1")
        )));

        let refiltered = s.set_verbosity("normal");
        assert!(refiltered.iter().any(|cmd| matches!(
            cmd,
            UiCommand::AddLogEntry {
                content,
                superseded: true,
                ..
            } if content == "Old prompt"
        )));
        assert!(refiltered.iter().any(|cmd| matches!(
            cmd,
            UiCommand::AddLogEntry {
                content,
                superseded: true,
                ..
            } if content == "Old answer"
        )));
    }

    #[test]
    fn ui_command_serialization() {
        let cmd = UiCommand::AddLogEntry {
            ts: "10:00:00".into(),
            level: "info".into(),
            source: "Agent".into(),
            content: "hello".into(),
            session_id: None,
            kind: None,
            output_id: None,
            collapsible: false,
            turn: None,
            user_turn_index: None,
            superseded: false,
            replacement_for_user_turn_index: None,
            images: vec![],
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("\"cmd\":\"add_log_entry\""));
        assert!(json.contains("\"content\":\"hello\""));

        let cmd2 = UiCommand::SetPhase {
            phase: "thinking".into(),
        };
        let json2 = serde_json::to_string(&cmd2).unwrap();
        assert!(json2.contains("\"cmd\":\"set_phase\""));
    }

    #[test]
    fn cost_summary_serialization() {
        let summary = CostSummary {
            total: 0.05,
            lines: vec![CostLine {
                label: "Main".into(),
                model: "gpt-5".into(),
                cost: 0.05,
                input_cost: 0.03,
                output_cost: 0.02,
            }],
        };
        let json = serde_json::to_string(&summary).unwrap();
        assert!(json.contains("Main"));
        let back: CostSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(back.lines.len(), 1);
    }

    #[test]
    fn usage_snapshot_roundtrip() {
        let u = UsageSnapshot {
            provider: "openai".into(),
            model: "gpt-5".into(),
            tokens_used: 5000,
            context_window: 128000,
            usage_pct: 3.9,
            prompt_tokens: 4000,
            completion_tokens: 1000,
            cached_tokens: 500,
        };
        let json = serde_json::to_string(&u).unwrap();
        let back: UsageSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tokens_used, 5000);
    }

    #[test]
    fn presence_usage_update() {
        let mut s = AppState::new();
        let msg = json!({
            "event": "presence_usage_update",
            "provider": "gemini", "model": "gemini-2.5-flash",
            "total_tokens": 2000, "context_window": 1048576,
            "usage_pct": 0.2, "prompt_tokens": 1500,
            "completion_tokens": 500, "cached_tokens": 100
        });
        let cmds = s.handle_message(&msg);
        assert!(s.presence_usage.is_some());
        assert!(cmds
            .iter()
            .any(|c| matches!(c, UiCommand::UpdateUsage { .. })));
    }

    #[test]
    fn live_usage_update_via_handle_message() {
        let mut s = AppState::new();
        let msg = json!({
            "event": "live_usage_update",
            "provider": "gemini", "model": "gemini-2.5-flash",
            "input_tokens": 1000, "output_tokens": 500,
            "cached_tokens": 200, "total_tokens": 1500,
            "thinking_tokens": 0
        });
        let cmds = s.handle_message(&msg);
        assert!(s.live_usage.is_some());
        let lu = s.live_usage.as_ref().unwrap();
        assert_eq!(lu.input_tokens, 1000);
        assert_eq!(lu.output_tokens, 500);
        assert_eq!(lu.provider, "gemini");
        assert!(cmds
            .iter()
            .any(|c| matches!(c, UiCommand::UpdateUsage { live_json, .. } if live_json.is_some())));
    }

    #[test]
    fn update_live_usage_returns_commands() {
        let mut s = AppState::new();
        let cmds = s.update_live_usage(LiveUsageSnapshot {
            provider: "gemini".into(),
            model: "gemini-2.5-flash".into(),
            input_tokens: 100,
            output_tokens: 50,
            cached_tokens: 10,
            total_tokens: 150,
            thinking_tokens: 0,
            ..Default::default()
        });
        assert!(s.live_usage.is_some());
        assert!(cmds
            .iter()
            .any(|c| matches!(c, UiCommand::UpdateUsage { live_json, .. } if live_json.is_some())));
    }

    #[test]
    fn live_usage_included_in_cost() {
        let mut s = AppState::new();
        // Set main usage with a known-priced model
        let main_msg = json!({
            "event": "usage_update",
            "main": {
                "provider": "openai", "model": "gpt-5",
                "tokens_used": 5000, "context_window": 128000,
                "usage_pct": 3.9, "prompt_tokens": 4000,
                "completion_tokens": 1000, "cached_tokens": 0
            }
        });
        s.handle_message(&main_msg);

        // Set live usage with a known-priced realtime model and audio tokens.
        s.update_live_usage(LiveUsageSnapshot {
            provider: "openai".into(),
            model: "gpt-realtime-1.5".into(),
            input_tokens: 1000,
            output_tokens: 500,
            cached_tokens: 100,
            total_tokens: 1500,
            thinking_tokens: 0,
            input_audio_tokens: 1000,
            cached_audio_tokens: 100,
            output_audio_tokens: 500,
            ..Default::default()
        });

        let cmd = s.build_usage_command();
        if let UiCommand::UpdateUsage {
            cost_json,
            live_json,
            ..
        } = cmd
        {
            assert!(live_json.is_some());
            assert!(cost_json.is_some());
            let cost: CostSummary = serde_json::from_str(&cost_json.unwrap()).unwrap();
            // Should have both main and live cost lines
            assert_eq!(cost.lines.len(), 2);
            let live = cost
                .lines
                .iter()
                .find(|l| l.label == "Live Model")
                .expect("live cost line");
            assert!((live.cost - 0.06084).abs() < 1e-12);
        } else {
            panic!("Expected UpdateUsage");
        }
    }

    #[test]
    fn set_active_tab() {
        let mut s = AppState::new();
        let cmds = s.set_active_tab("stats");
        assert!(cmds.is_empty()); // no badge to clear
        assert_eq!(s.active_tab, "stats");

        let cmds = s.set_active_tab("activity");
        assert!(cmds
            .iter()
            .any(|c| matches!(c, UiCommand::HideBadge { tab } if tab == "activity")));
    }

    #[test]
    fn handle_status_event() {
        let mut s = AppState::new();
        let msg = json!({
            "event": "status",
            "provider": "anthropic",
            "model": "claude-sonnet-4-6",
            "turn": 10,
            "autonomy": "High",
            "phase": "orchestrating",
            "session_id": "sess-xyz"
        });
        let cmds = s.handle_message(&msg);
        assert_eq!(s.provider, "anthropic");
        assert_eq!(s.model, "claude-sonnet-4-6");
        assert_eq!(s.turn, 10);
        assert_eq!(s.autonomy, "High");
        assert_eq!(s.phase, "orchestrating");
        assert!(cmds
            .iter()
            .any(|c| matches!(c, UiCommand::SetPhase { phase } if phase == "orchestrating")));
    }

    #[test]
    fn handle_event_file_changed() {
        let mut s = AppState::new();
        let msg = json!({"event": "file_changed", "path": "src/main.rs", "kind": "modified", "lines_added": 5, "lines_removed": 2});
        let cmds = s.handle_message(&msg);
        assert!(s.changed_files.contains_key("src/main.rs"));
        let entry = &s.changed_files["src/main.rs"];
        assert_eq!(entry.kind, "modified");
        assert_eq!(entry.lines_added, 5);
        assert_eq!(entry.lines_removed, 2);
        assert!(cmds
            .iter()
            .any(|c| matches!(c, UiCommand::FileChanged { .. })));
    }

    #[test]
    fn handle_event_file_changed_on_replay() {
        let mut s = AppState::new();
        let entries = vec![
            json!({"event": "replay_start", "provider": "p", "model": "m", "autonomy": "Medium"}),
            json!({"event": "file_changed", "path": "src/lib.rs", "kind": "created", "lines_added": 10, "lines_removed": 0, "ts": "01:00:00"}),
        ];
        let cmds = s.handle_log_replay(&entries);
        assert!(s.changed_files.contains_key("src/lib.rs"));
        assert!(cmds
            .iter()
            .any(|c| matches!(c, UiCommand::FileChanged { .. })));
    }

    #[test]
    fn handle_event_snapshot_created_emits_history_changed() {
        let mut s = AppState::new();
        s.verbosity = "verbose".to_string();
        let msg = json!({"event": "snapshot_created", "round_id": 3});
        let cmds = s.handle_message(&msg);
        assert!(cmds.iter().any(|c| matches!(c, UiCommand::HistoryChanged)));
        assert!(cmds.iter().any(|c| matches!(
            c,
            UiCommand::AddLogEntry { content, .. } if content.contains("round 3")
        )));
    }

    #[test]
    fn handle_event_rolled_back_emits_history_changed() {
        let mut s = AppState::new();
        let msg = json!({
            "event": "rolled_back",
            "from_id": 5,
            "to_id": 2,
            "files_reverted": 7
        });
        let cmds = s.handle_message(&msg);
        assert!(cmds.iter().any(|c| matches!(c, UiCommand::HistoryChanged)));
        let saw_log = cmds.iter().any(|c| {
            matches!(
                c,
                UiCommand::AddLogEntry { content, level, .. }
                    if level == "info"
                        && content.contains("Rolled back")
                        && content.contains("round 5")
                        && content.contains("round 2")
                        && content.contains("7 files")
            )
        });
        assert!(saw_log, "expected rollback log entry, got {:?}", cmds);
    }

    #[test]
    fn handle_event_conversation_rolled_back_truncated_logs_warn() {
        let mut s = AppState::new();
        let msg = json!({
            "event": "conversation_rolled_back",
            "round_id": "round-abcdef12",
            "turns_removed": 3,
            "backend": "openai",
            "method": "truncated"
        });
        let cmds = s.handle_message(&msg);
        let saw_log = cmds.iter().any(|c| {
            matches!(
                c,
                UiCommand::AddLogEntry { content, level, .. }
                    if level == "warn"
                        && content.contains("Conversation rolled back")
                        && content.contains("3 turns")
            )
        });
        assert!(
            saw_log,
            "expected warn log for truncated rollback, got {:?}",
            cmds
        );
    }

    #[test]
    fn handle_event_conversation_rolled_back_session_reset_mentions_backend() {
        let mut s = AppState::new();
        let msg = json!({
            "event": "conversation_rolled_back",
            "round_id": "round-abcdef12",
            "turns_removed": 7,
            "backend": "claude-code",
            "method": "session-reset"
        });
        let cmds = s.handle_message(&msg);
        let saw_log = cmds.iter().any(|c| {
            matches!(
                c,
                UiCommand::AddLogEntry { content, level, .. }
                    if level == "warn"
                        && content.contains("Session reset")
                        && content.contains("claude-code")
                        && content.contains("7 turns")
            )
        });
        assert!(saw_log, "expected session-reset warn log, got {:?}", cmds);
    }

    #[test]
    fn handle_event_conversation_rolled_back_defaults_method_to_truncated() {
        // When the backend omits `method` the UI should still produce a
        // sensible log — we default to the truncated phrasing.
        let mut s = AppState::new();
        let msg = json!({
            "event": "conversation_rolled_back",
            "round_id": "r1",
            "turns_removed": 1,
            "backend": "openai"
        });
        let cmds = s.handle_message(&msg);
        let saw_log = cmds.iter().any(|c| {
            matches!(
                c,
                UiCommand::AddLogEntry { content, level, .. }
                    if level == "warn" && content.contains("Conversation rolled back")
            )
        });
        assert!(saw_log, "expected default-method warn log, got {:?}", cmds);
    }

    #[test]
    fn handle_event_redone_emits_history_changed() {
        let mut s = AppState::new();
        let msg = json!({"event": "redone", "to_id": 4});
        let cmds = s.handle_message(&msg);
        assert!(cmds.iter().any(|c| matches!(c, UiCommand::HistoryChanged)));
        assert!(cmds.iter().any(|c| matches!(
            c,
            UiCommand::AddLogEntry { content, .. }
                if content.contains("Redone") && content.contains("round 4")
        )));
    }

    #[test]
    fn handle_event_history_pruned_emits_history_changed() {
        let mut s = AppState::new();
        let msg = json!({
            "event": "history_pruned",
            "branches_removed": 3,
            "bytes_freed": 2097152
        });
        let cmds = s.handle_message(&msg);
        assert!(cmds.iter().any(|c| matches!(c, UiCommand::HistoryChanged)));
        // 2 MiB exactly = "2.0 MB" formatted.
        assert!(cmds.iter().any(|c| matches!(
            c,
            UiCommand::AddLogEntry { content, .. }
                if content.contains("3 branches")
                    && content.contains("2.0 MB")
        )));
    }

    #[test]
    fn history_changed_serializes_as_snake_case_cmd() {
        let cmd = UiCommand::HistoryChanged;
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(
            json.contains("\"cmd\":\"history_changed\""),
            "got: {}",
            json
        );
    }

    #[test]
    fn handle_event_conversation_rolled_back_does_not_refetch() {
        let mut s = AppState::new();
        let msg = json!({
            "event": "conversation_rolled_back",
            "round_id": "round-7",
            "turns_removed": 4,
            "backend": "native",
            "method": "truncated",
        });
        let cmds = s.handle_message(&msg);
        // No HistoryChanged — the file timeline isn't affected.
        assert!(!cmds.iter().any(|c| matches!(c, UiCommand::HistoryChanged)));
    }

    #[test]
    fn handle_event_interrupt_requested_logs() {
        let mut s = AppState::new();
        let msg = json!({"event": "interrupt_requested"});
        let cmds = s.handle_message(&msg);
        assert!(cmds.iter().any(|c| matches!(
            c,
            UiCommand::AddLogEntry { content, .. } if content == "Interrupt requested"
        )));
    }

    #[test]
    fn handle_event_interrupted_transitions_phase_and_logs() {
        let mut s = AppState::new();
        s.phase = "running".to_string();
        let msg = json!({"event": "interrupted", "reason": "test"});
        let cmds = s.handle_message(&msg);
        assert_eq!(s.phase, "interrupted");
        assert!(cmds.iter().any(|c| matches!(
            c,
            UiCommand::SetPhase { phase } if phase == "interrupted"
        )));
        assert!(cmds.iter().any(|c| matches!(
            c,
            UiCommand::AddLogEntry { content, level, .. }
                if content == "Agent interrupted: test" && level == "warn"
        )));
        assert!(cmds.iter().any(|c| matches!(c, UiCommand::HideAllPanels)));
    }

    #[test]
    fn handle_event_interrupted_default_reason() {
        let mut s = AppState::new();
        let msg = json!({"event": "interrupted"});
        let cmds = s.handle_message(&msg);
        assert_eq!(s.phase, "interrupted");
        assert!(cmds.iter().any(|c| matches!(
            c,
            UiCommand::AddLogEntry { content, .. }
                if content == "Agent interrupted: user requested"
        )));
    }

    #[test]
    fn handle_state_snapshot_interrupted_sets_phase() {
        let mut s = AppState::new();
        let msg = json!({
            "t": "state_snapshot",
            "state": { "turn": 2, "budget_pct": 10.0, "phase": "interrupted" },
        });
        let _cmds = s.handle_message(&msg);
        assert_eq!(s.phase, "interrupted");
    }

    #[test]
    fn status_event_with_interrupting_phase() {
        let mut s = AppState::new();
        // Interrupting is a transient phase — backend emits it via the `status`
        // event while cancellation is in flight. The handler already threads
        // arbitrary phase strings through, so no special handling is needed
        // beyond verifying the state transition works.
        let msg = json!({"event": "status", "phase": "interrupting"});
        let cmds = s.handle_message(&msg);
        assert_eq!(s.phase, "interrupting");
        assert!(cmds.iter().any(|c| matches!(
            c,
            UiCommand::SetPhase { phase } if phase == "interrupting"
        )));
    }

    // ── Mid-turn steering ──────────────────────────────────────────

    #[test]
    fn handle_event_steer_requested_tracks_pending() {
        let mut s = AppState::new();
        let msg = json!({
            "event": "steer_requested",
            "text": "please check the logs",
            "id": "steer-123-1"
        });
        let cmds = s.handle_message(&msg);
        // Entry stored as Pending
        let entry = s.queued_steers.get("steer-123-1").expect("steer tracked");
        assert_eq!(entry.status, SteerStatus::Pending);
        assert_eq!(entry.text, "please check the logs");
        assert!(entry.reason.is_none());
        // SteerStatusUpdate command emitted with status=pending
        let saw_update = cmds.iter().any(|c| {
            matches!(
                c,
                UiCommand::SteerStatusUpdate { id, status, reason, text }
                    if id == "steer-123-1"
                        && status == "pending"
                        && reason.is_none()
                        && text == "please check the logs"
            )
        });
        assert!(saw_update, "expected SteerStatusUpdate, got {:?}", cmds);
        // Log entry surfaced so the user sees their send
        assert!(cmds.iter().any(|c| matches!(
            c,
            UiCommand::AddLogEntry { content, level, .. }
                if level == "info" && content.contains("Steer sent")
        )));
    }

    #[test]
    fn handle_event_steer_queued_updates_status() {
        let mut s = AppState::new();
        // Seed with a pending steer as if steer_requested arrived first
        s.queued_steers.insert(
            "abc".into(),
            QueuedSteer {
                text: "retry the build".into(),
                status: SteerStatus::Pending,
                reason: None,
            },
        );
        let msg = json!({
            "event": "steer_queued",
            "id": "abc",
            "reason": "agent does not support mid-turn steering"
        });
        let cmds = s.handle_message(&msg);
        let entry = s.queued_steers.get("abc").expect("still tracked");
        assert_eq!(entry.status, SteerStatus::Queued);
        assert_eq!(
            entry.reason.as_deref(),
            Some("agent does not support mid-turn steering")
        );
        // UiCommand carries status=queued and echoes the backend reason
        let saw_update = cmds.iter().any(|c| {
            matches!(
                c,
                UiCommand::SteerStatusUpdate { id, status, reason, text }
                    if id == "abc"
                        && status == "queued"
                        && reason.as_deref() == Some("agent does not support mid-turn steering")
                        && text == "retry the build"
            )
        });
        assert!(
            saw_update,
            "expected queued SteerStatusUpdate, got {:?}",
            cmds
        );
        // Queued uses warn level so the user notices the delay
        assert!(cmds.iter().any(|c| matches!(
            c,
            UiCommand::AddLogEntry { content, level, .. }
                if level == "warn" && content.contains("Steer queued")
        )));
    }

    #[test]
    fn handle_event_steer_delivered_removes_entry() {
        let mut s = AppState::new();
        s.queued_steers.insert(
            "xyz".into(),
            QueuedSteer {
                text: "stop and summarize".into(),
                status: SteerStatus::Queued,
                reason: Some("queued by backend".into()),
            },
        );
        let msg = json!({
            "event": "steer_delivered",
            "id": "xyz",
            "mid_turn": true
        });
        let cmds = s.handle_message(&msg);
        // Entry removed from the in-flight map
        assert!(s.queued_steers.get("xyz").is_none());
        let saw_update = cmds.iter().any(|c| {
            matches!(
                c,
                UiCommand::SteerStatusUpdate { id, status, reason, text }
                    if id == "xyz"
                        && status == "delivered"
                        && reason.is_none()
                        && text == "stop and summarize"
            )
        });
        assert!(
            saw_update,
            "expected delivered SteerStatusUpdate, got {:?}",
            cmds
        );
        // Log contains "mid-turn" for mid_turn=true deliveries
        assert!(cmds.iter().any(|c| matches!(
            c,
            UiCommand::AddLogEntry { content, .. }
                if content.contains("Steer delivered")
                    && content.contains("mid-turn")
                    && content.contains("stop and summarize")
        )));
    }

    #[test]
    fn steer_delivered_followup_log_variant() {
        // When mid_turn=false (queued delivery at turn boundary), the log
        // line calls it out as "as follow-up" rather than mid-turn so the
        // user understands the interjection wasn't real-time.
        let mut s = AppState::new();
        s.queued_steers.insert(
            "late".into(),
            QueuedSteer {
                text: "boundary delivery".into(),
                status: SteerStatus::Queued,
                reason: None,
            },
        );
        let msg = json!({
            "event": "steer_delivered",
            "id": "late",
            "mid_turn": false
        });
        let cmds = s.handle_message(&msg);
        assert!(cmds.iter().any(|c| matches!(
            c,
            UiCommand::AddLogEntry { content, .. }
                if content.contains("as follow-up")
        )));
    }

    #[test]
    fn steer_status_update_serializes_snake_case() {
        // JS dispatch table uses the string form of the cmd tag, so
        // make sure serde emits `steer_status_update` and the fields we expect.
        let cmd = UiCommand::SteerStatusUpdate {
            id: "s1".into(),
            text: "hi".into(),
            status: "pending".into(),
            reason: None,
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(
            json.contains("\"cmd\":\"steer_status_update\""),
            "got: {}",
            json
        );
        assert!(json.contains("\"id\":\"s1\""));
        assert!(json.contains("\"status\":\"pending\""));
        // reason omitted when None
        assert!(!json.contains("\"reason\""));
    }

    // ── render_peer_event tests ────────────────────────────────────

    /// `peer_event_forwarded` carrying a `log` PeerEvent renders as a
    /// host-tagged PeerLog with level/source/content pulled straight
    /// from the inner payload.
    #[test]
    fn peer_event_forwarded_log_renders_host_tagged_peer_log() {
        let mut s = AppState::new();
        let msg = json!({
            "event": "peer_event_forwarded",
            "peer_id": "intendant:alpha",
            "payload": {
                "event": "log",
                "level": "info",
                "source": "agent",
                "message": "hello from alpha",
                "ts": "2026-04-18T12:00:00Z",
            },
        });
        let cmds = s.handle_message(&msg);
        let log = cmds.iter().find_map(|c| match c {
            UiCommand::PeerLog {
                host_id,
                level,
                source,
                content,
                ts,
            } => Some((host_id, level, source, content, ts)),
            _ => None,
        });
        let (host_id, level, source, content, ts) = log.expect("PeerLog emitted");
        assert_eq!(host_id, "intendant:alpha");
        assert_eq!(level, "info");
        assert_eq!(source, "agent");
        assert_eq!(content, "hello from alpha");
        assert_eq!(ts, "2026-04-18T12:00:00Z");
    }

    /// `approval_requested` renders as a PeerApprovalRequested
    /// targeting the originating peer's id, with the request
    /// preview/category surfaced for the dashboard's per-peer
    /// pending-approvals list.
    #[test]
    fn peer_event_forwarded_approval_requested_targets_peer() {
        let mut s = AppState::new();
        let msg = json!({
            "event": "peer_event_forwarded",
            "peer_id": "intendant:beta",
            "payload": {
                "event": "approval_requested",
                "request": {
                    "request_id": "42",
                    "category": "command",
                    "preview": "rm -rf /tmp/foo",
                    "auto_resolvable": false,
                },
            },
        });
        let cmds = s.handle_message(&msg);
        let req = cmds.iter().find_map(|c| match c {
            UiCommand::PeerApprovalRequested {
                host_id,
                id,
                command,
                category,
            } => Some((host_id, id, command, category)),
            _ => None,
        });
        let (host_id, id, command, category) = req.expect("PeerApprovalRequested emitted");
        assert_eq!(host_id, "intendant:beta");
        assert_eq!(id, "42");
        assert_eq!(command, "rm -rf /tmp/foo");
        assert_eq!(category, "command");
    }

    /// `approval_resolved` renders as a PeerApprovalResolved that JS
    /// uses to drop the matching pending entry from its list.
    #[test]
    fn peer_event_forwarded_approval_resolved_targets_peer() {
        let mut s = AppState::new();
        let msg = json!({
            "event": "peer_event_forwarded",
            "peer_id": "intendant:beta",
            "payload": {
                "event": "approval_resolved",
                "request_id": "42",
                "decision": "accept",
            },
        });
        let cmds = s.handle_message(&msg);
        assert!(cmds.iter().any(|c| matches!(
            c,
            UiCommand::PeerApprovalResolved { host_id, id }
                if host_id == "intendant:beta" && id == "42"
        )));
    }

    /// `usage` renders as a PeerUsage carrying the snapshot JSON
    /// untouched — the dashboard caches it under the peer's id and
    /// re-renders the Stats panel when that peer is selected.
    #[test]
    fn peer_event_forwarded_usage_carries_snapshot() {
        let mut s = AppState::new();
        let msg = json!({
            "event": "peer_event_forwarded",
            "peer_id": "intendant:alpha",
            "payload": {
                "event": "usage",
                "snapshot": {
                    "tokens_in": 1234,
                    "tokens_out": 567,
                    "tokens_cached": 100,
                    "cost_usd": 0.04,
                },
            },
        });
        let cmds = s.handle_message(&msg);
        let usage = cmds.iter().find_map(|c| match c {
            UiCommand::PeerUsage { host_id, snapshot } => Some((host_id, snapshot)),
            _ => None,
        });
        let (host_id, snapshot) = usage.expect("PeerUsage emitted");
        assert_eq!(host_id, "intendant:alpha");
        assert_eq!(snapshot["tokens_in"], 1234);
        assert_eq!(snapshot["tokens_out"], 567);
    }

    /// `connected` / `disconnected` / `status_changed` PeerEvents are
    /// dropped because the registry's PeerStateChanged push event
    /// already covers them — surfacing again would duplicate.
    #[test]
    fn peer_event_forwarded_connection_events_are_dropped() {
        let mut s = AppState::new();
        for inner in [
            json!({"event": "connected", "card": {}}),
            json!({"event": "disconnected", "reason": "test"}),
            json!({"event": "status_changed", "status": "working"}),
        ] {
            let msg = json!({
                "event": "peer_event_forwarded",
                "peer_id": "intendant:x",
                "payload": inner,
            });
            let cmds = s.handle_message(&msg);
            assert!(
                !cmds.iter().any(|c| matches!(
                    c,
                    UiCommand::PeerLog { .. }
                        | UiCommand::PeerApprovalRequested { .. }
                        | UiCommand::PeerApprovalResolved { .. }
                        | UiCommand::PeerUsage { .. }
                )),
                "expected no peer-* UiCommand for connection lifecycle event"
            );
        }
    }

    /// `activity_completed` with a failure outcome renders as a warn-
    /// level log so it stands out in the per-peer activity stream.
    #[test]
    fn peer_event_forwarded_activity_failure_warns() {
        let mut s = AppState::new();
        let msg = json!({
            "event": "peer_event_forwarded",
            "peer_id": "intendant:alpha",
            "payload": {
                "event": "activity_completed",
                "id": "act-1",
                "outcome": {"status": "failed", "message": "boom"},
            },
        });
        let cmds = s.handle_message(&msg);
        let log = cmds.iter().find_map(|c| match c {
            UiCommand::PeerLog {
                host_id,
                level,
                content,
                ..
            } => Some((host_id, level, content)),
            _ => None,
        });
        let (host_id, level, content) = log.expect("PeerLog emitted");
        assert_eq!(host_id, "intendant:alpha");
        assert_eq!(level, "warn");
        assert!(content.contains("boom"), "expected boom in {content}");
    }

    /// `webrtc_signal` renders as a typed `PeerWebRtcSignal` carrying
    /// the peer's host_id, the display_id, the session_id, and the
    /// raw signal payload — JS dispatches on `signal.kind` to feed
    /// the matching `RTCPeerConnection`.
    #[test]
    fn peer_event_forwarded_webrtc_signal_targets_session() {
        let mut s = AppState::new();
        let msg = json!({
            "event": "peer_event_forwarded",
            "peer_id": "intendant:alpha",
            "payload": {
                "event": "webrtc_signal",
                "display_id": 0,
                "session_id": "sess-uuid",
                "signal": {"kind": "answer", "sdp": "v=0\r\n..."},
            },
        });
        let cmds = s.handle_message(&msg);
        let sig = cmds.iter().find_map(|c| match c {
            UiCommand::PeerWebRtcSignal {
                host_id,
                display_id,
                session_id,
                signal,
            } => Some((host_id, display_id, session_id, signal)),
            _ => None,
        });
        let (host_id, display_id, session_id, signal) = sig.expect("PeerWebRtcSignal emitted");
        assert_eq!(host_id, "intendant:alpha");
        assert_eq!(*display_id, 0);
        assert_eq!(session_id, "sess-uuid");
        assert_eq!(signal["kind"], "answer");
        assert_eq!(signal["sdp"], "v=0\r\n...");
    }

    /// The `cmd` discriminator of `UiCommand::PeerWebRtcSignal` MUST
    /// serialize to exactly `"peer_webrtc_signal"` because the JS
    /// dispatch in `static/app.html` matches that literal string. Without
    /// the explicit `#[serde(rename = "peer_webrtc_signal")]`,
    /// `rename_all = "snake_case"` mangles `WebRtc` into two words and
    /// produces `peer_web_rtc_signal`, which silently misses the JS
    /// switch — answer SDP arrives at the WS layer, gets translated to
    /// a well-formed UiCommand, then drops on the floor with no error.
    /// This test catches the regression at compile-test time instead of
    /// at smoke-test time. Wire-format invariant — see the variant's
    /// docstring for the full failure narrative.
    #[test]
    fn peer_webrtc_signal_wire_name() {
        let cmd = UiCommand::PeerWebRtcSignal {
            host_id: "intendant:alpha".to_string(),
            display_id: 0,
            session_id: "sess-uuid".to_string(),
            signal: json!({"kind": "answer", "sdp": ""}),
        };
        let v = serde_json::to_value(&cmd).expect("serialize");
        assert_eq!(
            v["cmd"].as_str(),
            Some("peer_webrtc_signal"),
            "PeerWebRtcSignal must serialize cmd=\"peer_webrtc_signal\" \
             (without the rename, snake_case would mangle to \
             peer_web_rtc_signal and JS dispatch would silently miss)"
        );
    }

    /// `webrtc_signal` without a session_id can't route to any
    /// per-peer RTCPeerConnection — it falls back to a warning log
    /// rather than producing a malformed `PeerWebRtcSignal`.
    #[test]
    fn peer_event_forwarded_webrtc_signal_missing_session_falls_back_to_warn_log() {
        let mut s = AppState::new();
        let msg = json!({
            "event": "peer_event_forwarded",
            "peer_id": "intendant:alpha",
            "payload": {
                "event": "webrtc_signal",
                "display_id": 0,
                "signal": {"kind": "answer", "sdp": ""},
            },
        });
        let cmds = s.handle_message(&msg);
        // No PeerWebRtcSignal should be emitted.
        let any_signal = cmds
            .iter()
            .any(|c| matches!(c, UiCommand::PeerWebRtcSignal { .. }));
        assert!(
            !any_signal,
            "missing session_id must not produce a PeerWebRtcSignal"
        );
        // A warn-level log entry should explain the drop.
        let warn = cmds.iter().find_map(|c| match c {
            UiCommand::PeerLog {
                level,
                source,
                content,
                ..
            } if level == "warn" && source == "webrtc" => Some(content),
            _ => None,
        });
        let content = warn.expect("warn log entry for missing session_id");
        assert!(
            content.contains("session_id"),
            "warn log should mention session_id: {content}"
        );
    }
}
