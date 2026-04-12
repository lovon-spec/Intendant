//! Pure-Rust app state for the web dashboard.
//!
//! All event routing, log filtering, usage tracking, cost calculation,
//! and status bar state live here. Methods return `Vec<UiCommand>` which
//! the thin JS layer executes as DOM updates.

use serde::{Deserialize, Serialize};

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
        #[serde(default)]
        collapsible: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn: Option<u64>,
        /// Base64-encoded images (screenshots) associated with this entry.
        /// Sent separately from content so JS can lazy-load them on expand.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<String>,
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
    },
    HideApproval,
    ShowHumanInput {
        question: String,
    },
    HideHumanInput,
    ShowFollowUp,
    HideFollowUp,
    HideAllPanels,
    UpdateUsage {
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
}

// ── Pricing ────────────────────────────────────────────────────────

/// Per-token pricing in USD.
#[derive(Debug, Clone, Copy)]
pub struct ModelPricing {
    pub input: f64,
    pub cached: f64,
    pub output: f64,
}

/// Static pricing table. Searched by exact match then prefix/contains.
const PRICING_TABLE: &[(&str, ModelPricing)] = &[
    // OpenAI
    ("gpt-5.4", ModelPricing { input: 2.5e-6, cached: 1.25e-6, output: 15.0e-6 }),
    ("gpt-5.4-mini", ModelPricing { input: 0.5e-6, cached: 0.25e-6, output: 3.0e-6 }),
    ("gpt-5.4-nano", ModelPricing { input: 0.15e-6, cached: 0.075e-6, output: 0.6e-6 }),
    ("gpt-5.2-codex", ModelPricing { input: 1.75e-6, cached: 0.175e-6, output: 7.0e-6 }),
    ("gpt-5", ModelPricing { input: 1.25e-6, cached: 0.625e-6, output: 10.0e-6 }),
    ("gpt-5-mini", ModelPricing { input: 0.25e-6, cached: 0.125e-6, output: 2.0e-6 }),
    ("gpt-4.1", ModelPricing { input: 2.0e-6, cached: 1.0e-6, output: 8.0e-6 }),
    ("gpt-4.1-mini", ModelPricing { input: 0.4e-6, cached: 0.2e-6, output: 1.6e-6 }),
    ("gpt-4.1-nano", ModelPricing { input: 0.1e-6, cached: 0.05e-6, output: 0.4e-6 }),
    ("o3", ModelPricing { input: 2.0e-6, cached: 1.0e-6, output: 8.0e-6 }),
    ("o3-pro", ModelPricing { input: 150.0e-6, cached: 75.0e-6, output: 600.0e-6 }),
    ("o4-mini", ModelPricing { input: 1.1e-6, cached: 0.55e-6, output: 4.4e-6 }),
    // Anthropic
    ("claude-opus-4-6", ModelPricing { input: 5.0e-6, cached: 0.5e-6, output: 25.0e-6 }),
    ("claude-sonnet-4-6", ModelPricing { input: 3.0e-6, cached: 0.3e-6, output: 15.0e-6 }),
    ("claude-sonnet-4-5-20250929", ModelPricing { input: 3.0e-6, cached: 0.3e-6, output: 15.0e-6 }),
    ("claude-opus-4-5-20250929", ModelPricing { input: 15.0e-6, cached: 1.5e-6, output: 75.0e-6 }),
    ("claude-haiku-4-5", ModelPricing { input: 0.25e-6, cached: 0.025e-6, output: 1.25e-6 }),
    // Gemini
    ("gemini-2.5-pro", ModelPricing { input: 1.25e-6, cached: 0.125e-6, output: 10.0e-6 }),
    ("gemini-2.5-flash", ModelPricing { input: 0.3e-6, cached: 0.03e-6, output: 2.5e-6 }),
    ("gemini-2.5-flash-lite", ModelPricing { input: 0.1e-6, cached: 0.01e-6, output: 0.4e-6 }),
    ("gemini-2.0-flash", ModelPricing { input: 0.1e-6, cached: 0.01e-6, output: 0.4e-6 }),
];

/// Find pricing for a model by exact match, then prefix/contains.
pub fn find_pricing(model: &str) -> Option<ModelPricing> {
    // Exact match
    for &(key, pricing) in PRICING_TABLE {
        if model == key {
            return Some(pricing);
        }
    }
    // Prefix/contains match
    for &(key, pricing) in PRICING_TABLE {
        if model.starts_with(key) || model.contains(key) {
            return Some(pricing);
        }
    }
    None
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
        "system" => "\u{2139}",  // ℹ
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
        "verbose" => &["info", "model", "agent", "error", "warn", "subagent", "detail", "presence"],
        "debug" => &["info", "model", "agent", "error", "warn", "subagent", "detail", "debug", "presence"],
        _ => &["info", "model", "agent", "error", "warn", "subagent", "presence"], // normal
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
    collapsible: bool,
    turn: Option<u64>,
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

    // Usage
    main_usage: Option<UsageSnapshot>,
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
            main_usage: None,
            presence_usage: None,
            live_usage: None,
            token_history: Vec::new(),
            last_total_tokens: 0,
            active_tab: "activity".to_string(),
            known_displays: Vec::new(),
            known_recordings: Vec::new(),
        }
    }

    /// Notify the state which tab is active (for badge logic).
    pub fn set_active_tab(&mut self, tab: &str) -> Vec<UiCommand> {
        self.active_tab = tab.to_string();
        let mut cmds = Vec::new();
        if tab == "activity" {
            cmds.push(UiCommand::HideBadge { tab: "activity".into() });
        }
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
                collapsible: entry.collapsible,
                turn: None, // separator already handled
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
                    vec![UiCommand::TermData { base64: d.to_string() }]
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
                    model: None, turn: None, budget_pct: None, autonomy: None, session_id: None, external_agent: None,
                });
            }
            if let Some(m) = cfg["model"].as_str() {
                self.model = m.to_string();
                cmds.push(UiCommand::UpdateStatusBar {
                    provider: None,
                    model: Some(m.to_string()),
                    turn: None, budget_pct: None, autonomy: None, session_id: None, external_agent: None,
                });
            }
        }

        // Session ID
        if let Some(sid) = msg["session_id"].as_str() {
            self.session_id = sid.to_string();
            cmds.push(UiCommand::UpdateStatusBar {
                provider: None, model: None, turn: None, budget_pct: None,
                autonomy: None, session_id: Some(sid.to_string()), external_agent: None,
            });
        }

        cmds.push(UiCommand::SetPhase { phase: phase.to_string() });

        // Restore pending approval
        if let Some(pa) = s.get("pending_approval") {
            if let Some(id) = pa["id"].as_u64() {
                if id > 0 {
                    self.pending_approval_id = Some(id);
                    let command = pa["command_preview"].as_str().unwrap_or("").to_string();
                    let category = pa["category"].as_str().unwrap_or("").to_string();
                    cmds.push(UiCommand::ShowApproval { id, command: command.clone(), category });
                    cmds.extend(self.add_log("warn", &format!("Approval required: {}", command), None, "worker"));
                }
            }
        }

        // Follow-up panel for idle/done phases
        let np = phase.replace('_', "");
        if np == "waitingfollowup" || np == "idle" || np == "done" {
            cmds.push(UiCommand::ShowFollowUp);
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
        self.replay_ts = msg
            .get("ts")
            .and_then(|v| v.as_str())
            .map(String::from);
        let mut cmds = Vec::new();

        match event {
            "turn_started" => {
                let turn = msg["turn"].as_u64().unwrap_or(0);
                let budget = msg["budget_pct"].as_f64().unwrap_or(0.0);
                self.turn = turn;
                self.budget_pct = budget;

                cmds.extend(self.add_log("info", &format!("Turn {} started", turn), Some(turn), "system"));
                cmds.push(UiCommand::UpdateStatusBar {
                    provider: None, model: None,
                    turn: Some(turn),
                    budget_pct: Some(budget),
                    autonomy: None, session_id: None, external_agent: None,
                });
                cmds.push(UiCommand::SetPhase { phase: "thinking".into() });
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
                        cmds.extend(self.add_log("detail", &format!("Reasoning: {}", rs), None, source));
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
                cmds.push(UiCommand::SetPhase { phase: "running".into() });
                self.phase = "running".to_string();
            }

            "agent_output" => {
                let source = msg["source"].as_str().unwrap_or("agent");
                if let Some(stdout) = msg["stdout"].as_str() {
                    if !stdout.is_empty() {
                        let out = format_agent_output(stdout);
                        if !out.text.is_empty() || !out.images.is_empty() {
                            cmds.extend(self.add_log_with_images(
                                "agent", &out.text, None, source, out.images,
                            ));
                        }
                    }
                }
                if let Some(stderr) = msg["stderr"].as_str() {
                    if !stderr.is_empty() {
                        cmds.extend(self.add_log("warn", stderr, None, "agent"));
                    }
                }
                cmds.push(UiCommand::SetPhase { phase: "running".into() });
                self.phase = "running".to_string();
            }

            "auto_approved" => {
                let preview = msg["preview"].as_str().unwrap_or("");
                cmds.extend(self.add_log("info", &format!("Auto-approved: {}", preview), None, "system"));
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
                cmds.extend(self.add_log("info", &format!("Context compacted at turn {}", turn), None, "system"));
            }

            "budget_warning" => {
                let pct = msg["pct"].as_f64().unwrap_or(0.0);
                cmds.extend(self.add_log("warn", &format!("Budget warning: {:.1}% used", pct), None, "system"));
            }

            "budget_exhausted" => {
                let remaining = msg["remaining"].as_u64().unwrap_or(0);
                cmds.extend(self.add_log("error", &format!("Budget exhausted ({} tokens remaining)", remaining), None, "system"));
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

                cmds.push(UiCommand::ShowApproval { id, command: command.clone(), category });
                cmds.push(UiCommand::SetPhase { phase: "waiting".into() });
                cmds.extend(self.add_log("warn", &format!("Approval required: {}", command), None, "worker"));

                if self.active_tab != "activity" {
                    cmds.push(UiCommand::ShowBadge { tab: "activity".into(), text: "!".into() });
                }
            }

            "ask_human" => {
                let question = msg["question"].as_str().unwrap_or("").to_string();
                self.phase = "waiting".to_string();

                cmds.push(UiCommand::ShowHumanInput { question: question.clone() });
                cmds.push(UiCommand::SetPhase { phase: "waiting".into() });
                cmds.extend(self.add_log("info", &format!("Question: {}", question), None, "worker"));

                if self.active_tab != "activity" {
                    cmds.push(UiCommand::ShowBadge { tab: "activity".into(), text: "?".into() });
                }
            }

            "task_complete" => {
                let reason = msg["reason"].as_str().unwrap_or("");
                let summary = msg["summary"].as_str();
                self.phase = "done".to_string();
                self.pending_approval_id = None;

                cmds.push(UiCommand::HideAllPanels);
                cmds.push(UiCommand::SetPhase { phase: "done".into() });

                let text = match summary {
                    Some(s) if !s.is_empty() => format!("Task complete: {} \u{2014} {}", reason, s),
                    _ => format!("Task complete: {}", reason),
                };
                cmds.extend(self.add_log("info", &text, None, "worker"));
                cmds.push(UiCommand::ShowFollowUp);
            }

            "round_complete" => {
                let round = msg["round"].as_u64().unwrap_or(0);
                let turns = msg["turns_in_round"].as_u64().unwrap_or(0);
                self.phase = "idle".to_string();

                cmds.push(UiCommand::SetPhase { phase: "idle".into() });
                cmds.extend(self.add_log("info", &format!("Round {} complete ({} turns)", round, turns), None, "system"));
                cmds.push(UiCommand::ShowFollowUp);
            }

            "status" => {
                let sb = UiCommand::UpdateStatusBar {
                    provider: msg["provider"].as_str().map(String::from),
                    model: msg["model"].as_str().map(String::from),
                    turn: msg["turn"].as_u64(),
                    budget_pct: msg["budget_pct"].as_f64(),
                    autonomy: msg["autonomy"].as_str().map(String::from),
                    session_id: msg["session_id"].as_str().map(String::from),
                    external_agent: msg["external_agent"].as_str().map(String::from),
                };
                if let Some(p) = msg["provider"].as_str() { self.provider = p.to_string(); }
                if let Some(m) = msg["model"].as_str() { self.model = m.to_string(); }
                if let Some(t) = msg["turn"].as_u64() { self.turn = t; }
                if let Some(a) = msg["autonomy"].as_str() { self.autonomy = a.to_string(); }
                if let Some(s) = msg["session_id"].as_str() { self.session_id = s.to_string(); }
                // Drop the binding and push
                cmds.push(sb);
                if let Some(phase) = msg["phase"].as_str() {
                    self.phase = phase.to_string();
                    cmds.push(UiCommand::SetPhase { phase: phase.to_string() });
                }
            }

            "external_agent_changed" => {
                cmds.push(UiCommand::UpdateStatusBar {
                    provider: None, model: None, turn: None, budget_pct: None,
                    autonomy: None, session_id: None,
                    external_agent: Some(msg["agent"].as_str().unwrap_or("").to_string()),
                });
            }

            "usage" | "usage_update" => {
                if let Some(main) = msg.get("main") {
                    if let Ok(u) = serde_json::from_value::<UsageSnapshot>(main.clone()) {
                        self.budget_pct = u.usage_pct;
                        cmds.push(UiCommand::UpdateStatusBar {
                            provider: None, model: None, turn: None,
                            budget_pct: Some(u.usage_pct),
                            autonomy: None, session_id: None, external_agent: None,
                        });
                        cmds.extend(self.add_log(
                            "detail",
                            &format!("tokens: {} / {} ({:.1}%)",
                                format_number(u.tokens_used),
                                format_number(u.context_window),
                                u.usage_pct),
                            None, "system",
                        ));
                        self.main_usage = Some(u);
                    }
                }
                if let Some(presence) = msg.get("presence") {
                    if let Ok(u) = serde_json::from_value::<UsageSnapshot>(presence.clone()) {
                        self.presence_usage = Some(u);
                    }
                }
                cmds.push(self.build_usage_command());
            }

            "display_ready" => {
                let display_id = msg["display_id"].as_u64().unwrap_or(0);
                let width = msg["width"].as_u64().unwrap_or(0);
                let height = msg["height"].as_u64().unwrap_or(0);
                cmds.extend(self.add_log("info", &format!("Display :{} ready", display_id), None, "system"));
                if !self.known_displays.iter().any(|&id| id == display_id) {
                    self.known_displays.push(display_id);
                }
                cmds.push(UiCommand::AddDisplay { display_id, width, height });
            }

            "display_taken" => {
                let id = msg["display_id"].as_u64().unwrap_or(0);
                cmds.extend(self.add_log("info", &format!("Display :{} in use", id), None, "system"));
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
                cmds.extend(self.add_log("info", &format!("Recording started: {}", stream), None, "system"));
                if !self.known_recordings.contains(&stream) {
                    self.known_recordings.push(stream.clone());
                }
                cmds.push(UiCommand::AddRecording { stream_name: stream });
            }

            "recording_stopped" => {
                let stream = msg["stream_name"].as_str().unwrap_or("").to_string();
                cmds.extend(self.add_log("info", &format!("Recording stopped: {}", stream), None, "system"));
                self.known_recordings.retain(|s| s != &stream);
                cmds.push(UiCommand::RemoveRecording { stream_name: stream });
            }

            "recording_error" => {
                let stream = msg["stream_name"].as_str().unwrap_or("").to_string();
                let message = msg["message"].as_str().unwrap_or("").to_string();
                cmds.extend(self.add_log("warn", &format!("Recording error ({}): {}", stream, message), None, "system"));
                cmds.push(UiCommand::RecordingError { stream_name: stream, message });
            }

            "session_started" => {
                let session_id = msg["session_id"].as_str().unwrap_or("").to_string();
                let task = msg["task"].as_str().map(|s| s.to_string());
                cmds.extend(self.add_log("info", &format!("Session started: {}", session_id), None, "system"));
                cmds.push(UiCommand::SessionStarted { session_id, task });
            }

            "session_ended" => {
                let session_id = msg["session_id"].as_str().unwrap_or("").to_string();
                let reason = msg["reason"].as_str().unwrap_or("").to_string();
                cmds.extend(self.add_log("info", &format!("Session ended: {} — {}", session_id, reason), None, "system"));
                cmds.push(UiCommand::SessionEnded { session_id, reason });
            }

            "debug_screen_ready" => {
                let display_id = msg["display_id"].as_u64().unwrap_or(0);
                cmds.extend(self.add_log("info", &format!("Debug screen ready on :{}", display_id), None, "system"));
                cmds.push(UiCommand::DebugScreenReady { display_id });
            }

            "debug_screen_torn_down" => {
                let display_id = msg["display_id"].as_u64().unwrap_or(0);
                cmds.extend(self.add_log("info", &format!("Debug screen :{} torn down", display_id), None, "system"));
                cmds.push(UiCommand::DebugScreenTornDown);
            }

            "command_result" => {
                let ok = msg["ok"].as_bool().unwrap_or(false);
                let action = msg["action"].as_str().unwrap_or("");
                let message = msg["message"].as_str().unwrap_or("");
                let level = if ok { "detail" } else { "error" };
                cmds.extend(self.add_log(level, &format!("[{}] {}", action, message), None, "system"));
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
                cmds.push(UiCommand::SetPhase { phase: "done".into() });
                self.phase = "done".to_string();
            }

            "log_entry" => {
                let level = msg["level"].as_str().unwrap_or("info");
                let source = msg["source"].as_str().unwrap_or("system");
                let content = msg["content"].as_str().unwrap_or("");
                let turn = msg["turn"].as_u64();
                cmds.extend(self.add_log(level, content, turn, source));
            }

            _ => {
                // Unknown events at debug level
                let text = format!("[{}] {}", event, serde_json::to_string(msg).unwrap_or_default());
                cmds.extend(self.add_log("debug", &text, None, "system"));
            }
        }

        // Clear replay timestamp override so subsequent live calls revert
        // to wallclock.
        self.replay_ts = None;
        cmds
    }

    /// Add a log entry, respecting verbosity. Returns AddLogEntry command if visible.
    fn add_log(&mut self, level: &str, content: &str, turn: Option<u64>, source: &str) -> Vec<UiCommand> {
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

        let entry = LogEntry {
            ts: ts.clone(),
            level: level.to_string(),
            source: source_str.clone(),
            content: content.to_string(),
            collapsible: is_collapsible,
            turn,
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
            collapsible: is_collapsible,
            turn: None, // separator already emitted
            images,
        });
        cmds
    }

    /// Update live model usage and return commands to re-render the Usage tab.
    pub fn update_live_usage(
        &mut self,
        provider: &str,
        model: &str,
        input_tokens: u64,
        output_tokens: u64,
        cached_tokens: u64,
        total_tokens: u64,
        thinking_tokens: u64,
    ) -> Vec<UiCommand> {
        self.live_usage = Some(LiveUsageSnapshot {
            provider: provider.to_string(),
            model: model.to_string(),
            input_tokens,
            output_tokens,
            cached_tokens,
            total_tokens,
            thinking_tokens,
        });
        vec![self.build_usage_command()]
    }

    /// Build an UpdateUsage command from current state.
    fn build_usage_command(&self) -> UiCommand {
        let main_json = self.main_usage.as_ref()
            .and_then(|u| serde_json::to_string(u).ok());
        let presence_json = self.presence_usage.as_ref()
            .and_then(|u| serde_json::to_string(u).ok());
        let live_json = self.live_usage.as_ref()
            .and_then(|u| serde_json::to_string(u).ok());

        // Cost calculation
        let cost_json = {
            let mut summary = CostSummary::default();
            if let Some(ref u) = self.main_usage {
                if let Some(pricing) = find_pricing(&u.model) {
                    let cost = calculate_cost(u.prompt_tokens, u.completion_tokens, u.cached_tokens, &pricing);
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
                    let cost = calculate_cost(u.prompt_tokens, u.completion_tokens, u.cached_tokens, &pricing);
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
                if let Some(pricing) = find_pricing(&u.model) {
                    let cost = calculate_cost(u.input_tokens, u.output_tokens, u.cached_tokens, &pricing);
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
            if summary.lines.is_empty() { None } else { serde_json::to_string(&summary).ok() }
        };

        let history_json = if self.token_history.is_empty() {
            None
        } else {
            serde_json::to_string(&self.token_history).ok()
        };

        UiCommand::UpdateUsage { main_json, presence_json, live_json, cost_json, history_json }
    }

    /// Process an approval action. Returns commands to send to server + update UI.
    pub fn approve_action(&mut self, action: &str) -> Option<(u64, Vec<UiCommand>)> {
        let id = self.pending_approval_id.take()?;
        let mut cmds = vec![
            UiCommand::HideAllPanels,
            UiCommand::SetPhase { phase: "running".into() },
        ];
        cmds.extend(self.add_log("info", &format!("Action: {}", action), None, "system"));
        self.phase = "running".to_string();
        Some((id, cmds))
    }

    /// Process a human response. Returns commands.
    pub fn human_response(&mut self, text: &str) -> Vec<UiCommand> {
        let mut cmds = vec![
            UiCommand::HideAllPanels,
            UiCommand::SetPhase { phase: "thinking".into() },
        ];
        cmds.extend(self.add_log("info", &format!("Response: {}", text), None, "system"));
        self.phase = "thinking".to_string();
        cmds
    }

    /// Process a follow-up message. Returns commands.
    pub fn follow_up(&mut self, text: &str) -> Vec<UiCommand> {
        let mut cmds = vec![
            UiCommand::HideAllPanels,
            UiCommand::SetPhase { phase: "thinking".into() },
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
        let pricing = ModelPricing { input: 1.0e-6, cached: 0.1e-6, output: 2.0e-6 };
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
        assert!(cmds.iter().any(|c| matches!(c, UiCommand::SetPhase { phase } if phase == "thinking")));
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
        assert!(cmds.iter().any(|c| matches!(c, UiCommand::ShowApproval { id: 42, .. })));
    }

    #[test]
    fn handle_event_turn_started() {
        let mut s = AppState::new();
        let msg = json!({"event": "turn_started", "turn": 3, "budget_pct": 15.5});
        let cmds = s.handle_message(&msg);
        assert_eq!(s.turn, 3);
        assert!(cmds.iter().any(|c| matches!(c, UiCommand::SetPhase { phase } if phase == "thinking")));
    }

    #[test]
    fn handle_event_approval_required() {
        let mut s = AppState::new();
        let msg = json!({"event": "approval_required", "id": 7, "command": "echo hi", "category": "CommandExec"});
        let cmds = s.handle_message(&msg);
        assert_eq!(s.pending_approval_id, Some(7));
        assert!(cmds.iter().any(|c| matches!(c, UiCommand::ShowApproval { id: 7, .. })));
    }

    #[test]
    fn handle_event_task_complete() {
        let mut s = AppState::new();
        s.pending_approval_id = Some(5);
        let msg = json!({"event": "task_complete", "reason": "done", "summary": "all good"});
        let cmds = s.handle_message(&msg);
        assert_eq!(s.phase, "done");
        assert!(s.pending_approval_id.is_none());
        assert!(cmds.iter().any(|c| matches!(c, UiCommand::ShowFollowUp)));
        assert!(cmds.iter().any(|c| matches!(c, UiCommand::HideAllPanels)));
    }

    #[test]
    fn handle_event_agent_output() {
        let mut s = AppState::new();
        let msg = json!({"event": "agent_output", "stdout": "hello world", "stderr": ""});
        let cmds = s.handle_message(&msg);
        assert!(cmds.iter().any(|c| matches!(c, UiCommand::AddLogEntry { content, .. } if content == "hello world")));
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
        assert!(cmds.iter().any(|c| matches!(c, UiCommand::UpdateUsage { .. })));
    }

    #[test]
    fn handle_event_display_ready() {
        let mut s = AppState::new();
        let msg = json!({"event": "display_ready", "display_id": 99});
        let cmds = s.handle_message(&msg);
        assert_eq!(s.known_displays.len(), 1);
        assert_eq!(s.known_displays[0], 99);
        assert!(cmds.iter().any(|c| matches!(c, UiCommand::AddDisplay { display_id: 99, .. })));
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
        let visible_entries: Vec<_> = cmds.iter().filter(|c| matches!(c, UiCommand::AddLogEntry { .. })).collect();
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
                UiCommand::AddLogEntry { source, content, .. }
                    if content.contains("Round 2 complete") =>
                {
                    Some(source.clone())
                }
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
            UiCommand::AddLogEntry { content, source, .. } => {
                if content.starts_with("Auto-approved: ") {
                    Some((content.clone(), source.clone()))
                } else {
                    None
                }
            }
            _ => None,
        });
        let (content, source) = entry
            .expect("auto_approved should emit an entry with the Auto-approved: prefix");
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
        let entries: Vec<_> = cmds.iter().filter(|c| matches!(c, UiCommand::AddLogEntry { .. })).collect();
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
        assert!(cmds.iter().any(|c| matches!(c, UiCommand::SetPhase { phase } if phase == "thinking")));

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
        let msg = json!({"event": "approval_required", "id": 1, "command": "test", "category": "exec"});
        let cmds = s.handle_message(&msg);
        assert!(cmds.iter().any(|c| matches!(c, UiCommand::ShowBadge { tab, .. } if tab == "activity")));
    }

    #[test]
    fn no_badge_when_on_activity_tab() {
        let mut s = AppState::new();
        s.active_tab = "activity".to_string();
        let msg = json!({"event": "approval_required", "id": 1, "command": "test", "category": "exec"});
        let cmds = s.handle_message(&msg);
        assert!(!cmds.iter().any(|c| matches!(c, UiCommand::ShowBadge { .. })));
    }

    #[test]
    fn handle_event_round_complete() {
        let mut s = AppState::new();
        let msg = json!({"event": "round_complete", "round": 2, "turns_in_round": 5});
        let cmds = s.handle_message(&msg);
        assert_eq!(s.phase, "idle");
        assert!(cmds.iter().any(|c| matches!(c, UiCommand::ShowFollowUp)));
    }

    #[test]
    fn handle_event_unknown() {
        let mut s = AppState::new();
        s.verbosity = "debug".to_string(); // enable debug to see unknown events
        let msg = json!({"event": "some_new_event", "foo": "bar"});
        let cmds = s.handle_message(&msg);
        assert!(cmds.iter().any(|c| matches!(c, UiCommand::AddLogEntry { level, .. } if level == "debug")));
    }

    #[test]
    fn ui_command_serialization() {
        let cmd = UiCommand::AddLogEntry {
            ts: "10:00:00".into(),
            level: "info".into(),
            source: "Agent".into(),
            content: "hello".into(),
            collapsible: false,
            turn: None,
            images: vec![],
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("\"cmd\":\"add_log_entry\""));
        assert!(json.contains("\"content\":\"hello\""));

        let cmd2 = UiCommand::SetPhase { phase: "thinking".into() };
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
        assert!(cmds.iter().any(|c| matches!(c, UiCommand::UpdateUsage { .. })));
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
        assert!(cmds.iter().any(|c| matches!(c, UiCommand::UpdateUsage { live_json, .. } if live_json.is_some())));
    }

    #[test]
    fn update_live_usage_returns_commands() {
        let mut s = AppState::new();
        let cmds = s.update_live_usage("gemini", "gemini-2.5-flash", 100, 50, 10, 150, 0);
        assert!(s.live_usage.is_some());
        assert!(cmds.iter().any(|c| matches!(c, UiCommand::UpdateUsage { live_json, .. } if live_json.is_some())));
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

        // Set live usage with a known-priced model
        s.update_live_usage("gemini", "gemini-2.0-flash", 1000, 500, 0, 1500, 0);

        let cmd = s.build_usage_command();
        if let UiCommand::UpdateUsage { cost_json, live_json, .. } = cmd {
            assert!(live_json.is_some());
            assert!(cost_json.is_some());
            let cost: CostSummary = serde_json::from_str(&cost_json.unwrap()).unwrap();
            // Should have both main and live cost lines
            assert_eq!(cost.lines.len(), 2);
            assert!(cost.lines.iter().any(|l| l.label == "Live Model"));
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
        assert!(cmds.iter().any(|c| matches!(c, UiCommand::HideBadge { tab } if tab == "activity")));
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
        assert!(cmds.iter().any(|c| matches!(c, UiCommand::SetPhase { phase } if phase == "orchestrating")));
    }
}
