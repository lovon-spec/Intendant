//! MCP (Model Context Protocol) server for Intendant.
//!
//! This module implements an MCP server that exposes Intendant's full state and
//! controls via the standard protocol. It is architecturally a **peer** of the
//! TUI — both consume the same [`EventBus`] events and translate user/agent
//! actions through the shared [`UserAction`](crate::frontend::UserAction) enum.
//!
//! ## Parity Contract
//!
//! The [`IntendantServer`] uses the same [`UserAction`] enum and [`StateQuery`]
//! types as the TUI. Adding a new `UserAction` variant forces both this module
//! and the TUI key handler to handle it (Rust exhaustive match, no wildcards).

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, Content, Implementation, ListResourcesResult, PaginatedRequestParams,
        RawResource, ReadResourceRequestParams, ReadResourceResult, Resource, ResourceContents,
        ResourceUpdatedNotificationParam, ServerCapabilities, ServerInfo, SubscribeRequestParams,
        UnsubscribeRequestParams,
    },
    schemars,
    service::{RequestContext, RoleServer},
    tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler, ServiceExt,
};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

use crate::autonomy::{AutonomyLevel, SharedAutonomy};
use crate::control;
use crate::event::{AppEvent, ApprovalRegistry, ApprovalResponse, ControlMsg, EventBus};
use crate::frontend::{
    self, ActionOutcome, ApprovalSnapshot, HumanQuestionSnapshot, LogEntrySnapshot, StateResult,
    StatusSnapshot, UserAction,
};
use crate::types::OutboundEvent;
use crate::types::{LogLevel, Phase, Verbosity};
use crate::FollowUpMessage;

const CONTEXT_PRESSURE_REWIND_THRESHOLD_PCT: f64 = 85.0;

// ---------------------------------------------------------------------------
// Task launcher: allows MCP to start agent loops on demand
// ---------------------------------------------------------------------------

/// A boxed async closure that spawns an agent loop for the given task.
///
/// The closure receives the task string and an `EventBus` for communicating
/// events back to the MCP server. It returns a `JoinHandle` for the spawned
/// background task.
pub type TaskLauncher = Box<
    dyn Fn(String, EventBus) -> Pin<Box<dyn Future<Output = tokio::task::JoinHandle<()>> + Send>>
        + Send
        + Sync,
>;

// ---------------------------------------------------------------------------
// Shared state that both the event listener and MCP handlers access
// ---------------------------------------------------------------------------

/// Observable state mirroring what the TUI's App struct tracks.
/// Updated by the event listener task, read by MCP tool/resource handlers.
pub struct McpAppState {
    pub provider_name: String,
    pub model_name: String,
    pub turn: usize,
    pub budget_pct: f64,
    pub phase: Phase,
    pub phase_entered_at: std::time::Instant,
    pub autonomy: SharedAutonomy,
    pub verbosity: Verbosity,
    pub session_tokens: u64,
    pub session_prompt_tokens: u64,
    pub session_completion_tokens: u64,
    pub session_cached_tokens: u64,
    pub context_window: u64,
    pub hard_context_window: Option<u64>,
    pub session_id: String,
    pub task_description: String,
    pub log_entries: std::collections::VecDeque<LogEntrySnapshot>,
    next_log_id: u64,
    pub pending_approval: Option<PendingApprovalState>,
    pub approval_registry: ApprovalRegistry,
    pub human_question: Option<String>,
    pub should_quit: bool,
    /// Session log directory for askHuman files.
    pub log_dir: std::path::PathBuf,
    /// Optional launcher for starting tasks via MCP. Set by main.rs.
    pub launcher: Option<Arc<TaskLauncher>>,
    /// Handle to the currently running agent loop, if any.
    pub task_handle: Option<tokio::task::JoinHandle<()>>,
    /// Mode override for the next task: None = auto, Some(true) = orchestrate,
    /// Some(false) = direct. Consumed (reset to None) when a task starts.
    pub next_task_orchestrate: Option<bool>,
    /// Pending or completed controller restart plan.
    pub controller_restart: Option<ControllerRestartState>,
    /// Current round number (for multi-round support).
    pub round: usize,
    /// Sender for follow-up messages (multi-round support).
    pub follow_up_tx: Option<tokio::sync::mpsc::Sender<FollowUpMessage>>,
    // Presence layer usage tracking
    pub presence_provider_name: Option<String>,
    pub presence_model_name: Option<String>,
    pub presence_tokens: u64,
    pub presence_context_window: u64,
    pub presence_usage_pct: f64,
    /// Frame registry for display frame access.
    pub frame_registry: Option<Arc<tokio::sync::RwLock<crate::frames::FrameRegistry>>>,
    /// Display session registry for CU action dispatch.
    pub session_registry: Option<crate::display::SharedSessionRegistry>,
    /// Directory for screenshot output.
    pub screenshot_dir: Option<std::path::PathBuf>,
    /// Persistent counter for screenshot filenames (avoids overwriting).
    pub screenshot_counter: std::sync::atomic::AtomicU64,
    /// External agent backend selected via web UI (deferred: takes effect on next task).
    pub external_agent: Option<crate::external_agent::AgentBackend>,
    /// Desired Codex managed-context mode for the next managed Codex task.
    pub configured_codex_managed_context: bool,
    /// Whether the active Codex backend supports Intendant's managed-context
    /// protocol.
    pub codex_managed_context: bool,
    /// Managed-context capability latched per Intendant/backend session id.
    pub session_codex_managed_context: std::collections::HashMap<String, bool>,
    /// Bidirectional aliases between Intendant wrapper ids and backend thread ids.
    session_aliases: std::collections::HashMap<String, std::collections::HashSet<String>>,
    /// Latest backend usage sample by Intendant/backend session id.
    session_usage: std::collections::HashMap<String, frontend::ModelUsageSnapshot>,
    /// Latest observed phase by Intendant/backend session id.
    session_status: std::collections::HashMap<String, SessionStatusState>,
    /// Source for the currently active session, when it is known.
    pub active_session_source: Option<String>,
    /// Map Intendant wrapper session IDs and backend session IDs to their external source.
    pub session_sources: std::collections::HashMap<String, String>,
    /// Successful rewind records awaiting the next backend usage sample, keyed
    /// by Intendant/backend session id.
    pending_rewind_pressure_checks: std::collections::HashMap<String, String>,
    /// Last successful rewinds that did not reduce backend-reported pressure
    /// below the gate, keyed by Intendant/backend session id.
    insufficient_rewind_notices: std::collections::HashMap<String, InsufficientRewindNotice>,
}

#[derive(Debug, Clone)]
struct SessionStatusState {
    turn: usize,
    round: usize,
    phase: Phase,
    task: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InsufficientRewindNotice {
    record_id: String,
    used_tokens: u64,
    rewind_only_limit: u64,
    context_window: u64,
}

/// Tracks a pending approval info (responder is in the shared ApprovalRegistry).
pub struct PendingApprovalState {
    pub id: u64,
    pub command_preview: String,
    pub category: String,
}

impl McpAppState {
    pub fn new(
        provider_name: String,
        model_name: String,
        autonomy: SharedAutonomy,
        log_dir: std::path::PathBuf,
    ) -> Self {
        Self {
            provider_name,
            model_name,
            turn: 0,
            budget_pct: 0.0,
            phase: Phase::Idle,
            phase_entered_at: std::time::Instant::now(),
            autonomy,
            verbosity: Verbosity::Normal,
            session_tokens: 0,
            session_prompt_tokens: 0,
            session_completion_tokens: 0,
            session_cached_tokens: 0,
            context_window: 0,
            hard_context_window: None,
            session_id: String::new(),
            task_description: String::new(),
            log_entries: std::collections::VecDeque::new(),
            next_log_id: 0,
            pending_approval: None,
            approval_registry: ApprovalRegistry::default(),
            human_question: None,
            should_quit: false,
            log_dir,
            launcher: None,
            task_handle: None,
            controller_restart: None,
            next_task_orchestrate: None,
            round: 0,
            follow_up_tx: None,
            presence_provider_name: None,
            presence_model_name: None,
            presence_tokens: 0,
            presence_context_window: 0,
            presence_usage_pct: 0.0,
            frame_registry: None,
            session_registry: None,
            screenshot_dir: None,
            screenshot_counter: std::sync::atomic::AtomicU64::new(0),
            external_agent: None,
            configured_codex_managed_context: false,
            codex_managed_context: false,
            session_codex_managed_context: std::collections::HashMap::new(),
            session_aliases: std::collections::HashMap::new(),
            session_usage: std::collections::HashMap::new(),
            session_status: std::collections::HashMap::new(),
            active_session_source: None,
            session_sources: std::collections::HashMap::new(),
            pending_rewind_pressure_checks: std::collections::HashMap::new(),
            insufficient_rewind_notices: std::collections::HashMap::new(),
        }
    }

    fn set_phase(&mut self, phase: Phase) {
        if self.phase != phase {
            self.phase = phase;
            self.phase_entered_at = std::time::Instant::now();
        }
    }

    fn push_log(&mut self, level: LogLevel, content: String) {
        let id = self.next_log_id;
        self.next_log_id += 1;
        let ts = chrono::Local::now().format("%H:%M:%S").to_string();
        if self.log_entries.len() >= 10_000 {
            self.log_entries.pop_front();
        }
        self.log_entries.push_back(LogEntrySnapshot {
            id,
            ts,
            level: frontend::log_level_to_str(&level).to_string(),
            content,
        });
    }

    fn status_snapshot(&self) -> StatusSnapshot {
        StatusSnapshot {
            session_id: self.session_id.clone(),
            task: self.task_description.clone(),
            provider: self.provider_name.clone(),
            model: self.model_name.clone(),
            turn: self.turn,
            budget_pct: self.budget_pct,
            phase: phase_to_str(&self.phase).to_string(),
            autonomy: "unknown".to_string(), // filled by caller with async read
            verbosity: verbosity_to_str(self.verbosity).to_string(),
            session_tokens: self.session_tokens,
            round: self.round,
        }
    }

    fn usage_snapshot(&self) -> crate::frontend::UsageSnapshot {
        crate::frontend::UsageSnapshot {
            main: crate::frontend::ModelUsageSnapshot {
                provider: self.provider_name.clone(),
                model: self.model_name.clone(),
                tokens_used: self.session_tokens,
                context_window: self.context_window,
                hard_context_window: self.hard_context_window,
                usage_pct: self.budget_pct,
                prompt_tokens: self.session_prompt_tokens,
                completion_tokens: self.session_completion_tokens,
                cached_tokens: self.session_cached_tokens,
            },
            presence: self.presence_provider_name.as_ref().map(|p| {
                crate::frontend::ModelUsageSnapshot {
                    provider: p.clone(),
                    model: self.presence_model_name.clone().unwrap_or_default(),
                    tokens_used: self.presence_tokens,
                    context_window: self.presence_context_window,
                    hard_context_window: Some(self.presence_context_window),
                    usage_pct: self.presence_usage_pct,
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    cached_tokens: 0,
                }
            }),
        }
    }

    fn usage_snapshot_for(&self, session_id: Option<&str>) -> crate::frontend::UsageSnapshot {
        let mut usage = self.usage_snapshot();
        if let Some(id) = session_id.map(str::trim).filter(|id| !id.is_empty()) {
            if let Some(main) = self.session_usage_for_id(id) {
                usage.main = main.clone();
            } else if id != self.session_id {
                usage.main.provider.clear();
                usage.main.model.clear();
                usage.main.tokens_used = 0;
                usage.main.context_window = 0;
                usage.main.hard_context_window = None;
                usage.main.usage_pct = 0.0;
                usage.main.prompt_tokens = 0;
                usage.main.completion_tokens = 0;
                usage.main.cached_tokens = 0;
            }
        }
        usage
    }

    fn link_session_aliases(&mut self, session_id: &str, backend_session_id: &str) {
        let session_id = session_id.trim();
        let backend_session_id = backend_session_id.trim();
        if session_id.is_empty()
            || backend_session_id.is_empty()
            || session_id == backend_session_id
        {
            return;
        }
        self.session_aliases
            .entry(session_id.to_string())
            .or_default()
            .insert(backend_session_id.to_string());
        self.session_aliases
            .entry(backend_session_id.to_string())
            .or_default()
            .insert(session_id.to_string());
        if let Some(status) = self
            .session_status
            .get(session_id)
            .cloned()
            .or_else(|| self.session_status.get(backend_session_id).cloned())
        {
            self.session_status
                .insert(session_id.to_string(), status.clone());
            self.session_status
                .insert(backend_session_id.to_string(), status);
        }
        if let Some(usage) = self
            .session_usage
            .get(backend_session_id)
            .cloned()
            .or_else(|| self.session_usage.get(session_id).cloned())
        {
            self.session_usage
                .insert(session_id.to_string(), usage.clone());
            self.session_usage
                .insert(backend_session_id.to_string(), usage);
        }
    }

    fn session_related_ids(&self, session_id: &str) -> Vec<String> {
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(session_id.to_string());
        while let Some(id) = queue.pop_front() {
            if !seen.insert(id.clone()) {
                continue;
            }
            out.push(id.clone());
            if let Some(aliases) = self.session_aliases.get(&id) {
                for alias in aliases {
                    if !seen.contains(alias) {
                        queue.push_back(alias.clone());
                    }
                }
            }
        }
        out
    }

    fn session_usage_for_id(&self, session_id: &str) -> Option<&frontend::ModelUsageSnapshot> {
        for related in self.session_related_ids(session_id) {
            if let Some(usage) = self.session_usage.get(&related) {
                return Some(usage);
            }
        }
        None
    }

    fn record_session_usage_snapshot(
        &mut self,
        session_id: Option<&str>,
        usage: frontend::ModelUsageSnapshot,
    ) {
        let Some(id) = session_id.map(str::trim).filter(|id| !id.is_empty()) else {
            return;
        };
        let mut keys = self.session_related_ids(id);
        if keys.is_empty() {
            keys.push(id.to_string());
        }
        for key in keys {
            self.session_usage.insert(key, usage.clone());
        }
    }

    fn session_id_applies_to_current_session(&self, session_id: Option<&str>) -> bool {
        let Some(id) = session_id.map(str::trim).filter(|id| !id.is_empty()) else {
            return true;
        };
        self.session_id.is_empty()
            || id == self.session_id
            || self
                .session_related_ids(id)
                .iter()
                .any(|related| related == &self.session_id)
    }

    fn session_status_for_id(&self, session_id: &str) -> Option<&SessionStatusState> {
        for related in self.session_related_ids(session_id) {
            if let Some(status) = self.session_status.get(&related) {
                return Some(status);
            }
        }
        None
    }

    fn session_source_for_id(&self, session_id: &str) -> Option<&str> {
        for related in self.session_related_ids(session_id) {
            if let Some(source) = self.session_sources.get(&related) {
                return Some(source.as_str());
            }
        }
        None
    }

    fn note_session_phase(
        &mut self,
        session_id: Option<&str>,
        turn: Option<usize>,
        phase: Phase,
        task: Option<&str>,
    ) {
        let target_id = session_id
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .or_else(|| {
                let id = self.session_id.trim();
                (!id.is_empty()).then(|| id.to_string())
            });
        let Some(target_id) = target_id else {
            if let Some(turn) = turn {
                self.turn = turn;
            }
            self.set_phase(phase);
            if let Some(task) = task.map(str::trim).filter(|task| !task.is_empty()) {
                self.task_description = task.to_string();
            }
            return;
        };

        let keys = {
            let related = self.session_related_ids(&target_id);
            if related.is_empty() {
                vec![target_id.clone()]
            } else {
                related
            }
        };
        let existing = keys
            .iter()
            .find_map(|key| self.session_status.get(key))
            .cloned();
        let applies_to_current = self.session_id.is_empty()
            || keys.iter().any(|key| key == &self.session_id)
            || self
                .session_related_ids(&self.session_id)
                .iter()
                .any(|key| keys.contains(key));
        let turn = turn
            .or_else(|| existing.as_ref().map(|status| status.turn))
            .unwrap_or(self.turn);
        let round = existing
            .as_ref()
            .map(|status| status.round)
            .unwrap_or(self.round);
        let task = task
            .map(str::trim)
            .filter(|task| !task.is_empty())
            .map(str::to_string)
            .or_else(|| existing.as_ref().map(|status| status.task.clone()))
            .or_else(|| applies_to_current.then(|| self.task_description.clone()))
            .unwrap_or_default();
        let status = SessionStatusState {
            turn,
            round,
            phase: phase.clone(),
            task: task.clone(),
        };
        for key in keys {
            self.session_status.insert(key, status.clone());
        }
        if applies_to_current {
            self.turn = turn;
            if !task.is_empty() {
                self.task_description = task;
            }
            self.set_phase(phase);
        }
    }

    fn note_session_round(&mut self, session_id: Option<&str>, round: usize) {
        let target_id = session_id
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .or_else(|| {
                let id = self.session_id.trim();
                (!id.is_empty()).then(|| id.to_string())
            });
        let Some(target_id) = target_id else {
            self.round = round;
            return;
        };

        let keys = {
            let related = self.session_related_ids(&target_id);
            if related.is_empty() {
                vec![target_id.clone()]
            } else {
                related
            }
        };
        for key in &keys {
            let entry =
                self.session_status
                    .entry(key.clone())
                    .or_insert_with(|| SessionStatusState {
                        turn: self.turn,
                        round,
                        phase: self.phase.clone(),
                        task: self.task_description.clone(),
                    });
            entry.round = round;
        }
        if self.session_id.is_empty()
            || keys.iter().any(|key| key == &self.session_id)
            || self
                .session_related_ids(&self.session_id)
                .iter()
                .any(|key| keys.contains(key))
        {
            self.round = round;
        }
    }

    fn normalize_main_usage_snapshot(
        &self,
        session_id: Option<&str>,
        mut usage: frontend::ModelUsageSnapshot,
    ) -> frontend::ModelUsageSnapshot {
        let previous_hard = session_id
            .and_then(|id| {
                self.session_usage_for_id(id)
                    .and_then(|previous| previous.hard_context_window)
            })
            .or(self.hard_context_window)
            .filter(|hard| *hard > 0);
        let Some(previous_hard) = previous_hard else {
            return usage;
        };
        if usage.context_window == 0 {
            return usage;
        }

        let should_preserve = match usage.hard_context_window {
            Some(current_hard) if current_hard > 0 => {
                current_hard <= usage.context_window && previous_hard > current_hard
            }
            _ => previous_hard > usage.context_window,
        };
        if should_preserve {
            usage.hard_context_window = Some(previous_hard);
        }
        usage
    }

    fn apply_main_usage_snapshot(&mut self, usage: frontend::ModelUsageSnapshot) {
        let usage = self.normalize_main_usage_snapshot(None, usage);
        if !usage.provider.is_empty() {
            self.provider_name = usage.provider.clone();
        }
        if !usage.model.is_empty() {
            self.model_name = usage.model.clone();
        }
        self.session_tokens = usage.tokens_used;
        self.context_window = usage.context_window;
        self.hard_context_window = usage.hard_context_window;
        self.budget_pct = usage.usage_pct;
        self.session_prompt_tokens = usage.prompt_tokens;
        self.session_completion_tokens = usage.completion_tokens;
        self.session_cached_tokens = usage.cached_tokens;
        self.complete_pending_rewind_pressure_check();
    }

    fn rewind_session_key(&self, session_id: Option<&str>) -> Option<String> {
        session_id
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .or_else(|| {
                let id = self.session_id.trim();
                if id.is_empty() {
                    None
                } else {
                    Some(id.to_string())
                }
            })
    }

    fn rewind_related_keys(&self, key: &str) -> Vec<String> {
        let related = self.session_related_ids(key);
        if related.is_empty() {
            vec![key.to_string()]
        } else {
            related
        }
    }

    fn remove_pending_rewind_pressure_check_for_key(&mut self, key: &str) -> Option<String> {
        let mut record_id = None;
        for related in self.rewind_related_keys(key) {
            if let Some(found) = self.pending_rewind_pressure_checks.remove(&related) {
                record_id.get_or_insert(found);
            }
        }
        record_id
    }

    fn remove_insufficient_rewind_notice_for_key(&mut self, key: &str) {
        for related in self.rewind_related_keys(key) {
            self.insufficient_rewind_notices.remove(&related);
        }
    }

    fn insert_insufficient_rewind_notice_for_key(
        &mut self,
        key: &str,
        notice: InsufficientRewindNotice,
    ) {
        for related in self.rewind_related_keys(key) {
            self.insufficient_rewind_notices
                .insert(related, notice.clone());
        }
    }

    fn note_context_rewind_result_for(
        &mut self,
        session_id: Option<&str>,
        success: bool,
        message: &str,
    ) {
        let Some(key) = self.rewind_session_key(session_id) else {
            return;
        };
        if success {
            if let Some(record_id) = context_rewind_record_id_from_message(message) {
                for related in self.rewind_related_keys(&key) {
                    self.pending_rewind_pressure_checks
                        .insert(related, record_id.clone());
                }
                self.remove_insufficient_rewind_notice_for_key(&key);
            }
        } else {
            // A failed rewind must not leave a pending pressure check behind: a
            // later (possibly stale) usage sample could otherwise resolve it into a
            // false "insufficient" notice against a record that never committed.
            self.remove_pending_rewind_pressure_check_for_key(&key);
        }
    }

    fn complete_pending_rewind_pressure_check(&mut self) {
        self.complete_pending_rewind_pressure_check_for(None);
    }

    fn complete_pending_rewind_pressure_check_for(&mut self, session_id: Option<&str>) {
        let Some(key) = self.rewind_session_key(session_id) else {
            return;
        };
        let Some(record_id) = self.remove_pending_rewind_pressure_check_for_key(&key) else {
            return;
        };
        if !self.active_codex_managed_context_enabled_for(Some(&key), None) {
            return;
        }
        if let Some((used_tokens, rewind_only_limit, _status)) =
            self.context_pressure_rewind_only_for(Some(&key))
        {
            let context_window = self.session_usage_values(Some(&key)).1;
            self.insert_insufficient_rewind_notice_for_key(
                &key,
                InsufficientRewindNotice {
                    record_id,
                    used_tokens,
                    rewind_only_limit,
                    context_window,
                },
            );
        } else {
            self.remove_insufficient_rewind_notice_for_key(&key);
        }
    }

    fn insufficient_rewind_notice_for(
        &self,
        session_id: Option<&str>,
    ) -> Option<&InsufficientRewindNotice> {
        let key = self.rewind_session_key(session_id)?;
        self.rewind_related_keys(&key)
            .into_iter()
            .find_map(|related| self.insufficient_rewind_notices.get(&related))
    }

    fn managed_context_mode(enabled: bool) -> &'static str {
        if enabled {
            "managed"
        } else {
            "vanilla"
        }
    }

    fn session_usage_values(&self, session_id: Option<&str>) -> (u64, u64, Option<u64>) {
        if let Some(id) = session_id.map(str::trim).filter(|id| !id.is_empty()) {
            // A concrete session that has not reported usage yet is *unknown* — do
            // not borrow the globally-active session's totals. Borrowing would let a
            // starting session A inherit a saturated session B's pressure and be
            // wrongly forced into rewind-only mode during the startup race.
            if let Some(usage) = self.session_usage_for_id(id) {
                return (
                    usage.tokens_used,
                    usage.context_window,
                    usage.hard_context_window,
                );
            }

            if self
                .session_related_ids(id)
                .iter()
                .any(|candidate| candidate == &self.session_id)
                && self.context_window > 0
            {
                return (
                    self.session_tokens,
                    self.context_window,
                    self.hard_context_window,
                );
            }

            return (0, 0, None);
        }
        (
            self.session_tokens,
            self.context_window,
            self.hard_context_window,
        )
    }

    fn context_pressure_snapshot_for(
        &self,
        session_id: Option<&str>,
        managed_context_override: Option<bool>,
    ) -> serde_json::Value {
        let (used_tokens, context_window, hard_context_window) =
            self.session_usage_values(session_id);
        let managed_context =
            self.exposed_codex_managed_context_enabled_for(session_id, managed_context_override);
        if context_window == 0 {
            return serde_json::json!({
                "source": "backend_reported",
                "status": "unknown",
                "used_tokens": used_tokens,
                "context_window": null,
                "effective_context_window": null,
                "remaining_tokens": null,
                "remaining_hard_tokens": null,
                "remaining_percent": null,
                "recommended_rewind_limit": null,
                "rewind_only_limit": null,
                "hard_limit": null,
                "rewind_only": false,
                "density_pressure": false,
                "density_maintenance_recommended": false,
                "normal_tools_allowed": true,
                "broad_followup_allowed": true,
                "narrow_inflight_validation_allowed": true,
                "required_action": "continue",
                "message": "Backend-reported context pressure is unavailable. Normal tools are allowed unless a later status reports rewind_only=true; continue ordinary work while pressure is unknown.",
                "managed_context": Self::managed_context_mode(managed_context),
                "last_rewind_insufficient": null,
            });
        }

        let recommended_rewind_limit =
            (context_window as f64 * CONTEXT_PRESSURE_REWIND_THRESHOLD_PCT / 100.0).floor() as u64;
        let rewind_only_limit = context_window;
        let remaining_tokens = context_window.saturating_sub(used_tokens);
        let remaining_percent = (remaining_tokens as f64 / context_window as f64 * 100.0).max(0.0);
        let remaining_hard_tokens =
            hard_context_window.map(|hard| hard.saturating_sub(used_tokens));
        let status = if hard_context_window.is_some_and(|hard| hard > 0 && used_tokens >= hard) {
            "critical"
        } else if used_tokens >= rewind_only_limit {
            "high"
        } else if used_tokens >= recommended_rewind_limit {
            "watch"
        } else {
            "ok"
        };
        let rewind_only = managed_context && (status == "high" || status == "critical");
        let density_pressure = used_tokens >= recommended_rewind_limit;
        let density_maintenance_recommended = managed_context && density_pressure && !rewind_only;
        let normal_tools_allowed = !rewind_only;
        let broad_followup_allowed = normal_tools_allowed && !density_maintenance_recommended;
        let narrow_inflight_validation_allowed = normal_tools_allowed;
        let required_action = if rewind_only {
            "rewind_context"
        } else if density_maintenance_recommended {
            "density_handoff_before_broad_work"
        } else if density_pressure {
            "continue_or_rewind_optional"
        } else {
            "continue"
        };
        let message = if rewind_only {
            "Managed context is in rewind-only mode. Use rewind_context before ordinary model-facing tools."
        } else if density_pressure {
            if managed_context {
                "Managed context is above the recommended density threshold but below the rewind-only limit. Normal tools remain allowed for status/anchor inspection and one narrow in-flight validation or build to finish, but before broad follow-up work perform exact-anchor density maintenance when it materially improves density, or produce a concise no-rewind density handoff."
            } else {
                "Context is above the recommended density threshold but below the rewind-only limit. Normal tools are allowed; at handoff or before broad follow-up work, exact-anchor density maintenance is optional only if it materially improves density."
            }
        } else {
            "Context is below the recommended density threshold. Normal tools are allowed; no rewind preparation is needed unless a recent tool result was genuinely noisy or unexpectedly large."
        };

        serde_json::json!({
            "source": "backend_reported",
            "status": status,
            "used_tokens": used_tokens,
            "context_window": context_window,
            "effective_context_window": context_window,
            "remaining_tokens": remaining_tokens,
            "remaining_hard_tokens": remaining_hard_tokens,
            "remaining_percent": remaining_percent,
            "recommended_rewind_limit": recommended_rewind_limit,
            "rewind_only_limit": rewind_only_limit,
            "hard_limit": hard_context_window,
            "rewind_only": rewind_only,
            "density_pressure": density_pressure,
            "density_maintenance_recommended": density_maintenance_recommended,
            "normal_tools_allowed": normal_tools_allowed,
            "broad_followup_allowed": broad_followup_allowed,
            "narrow_inflight_validation_allowed": narrow_inflight_validation_allowed,
            "required_action": required_action,
            "message": message,
            "managed_context": Self::managed_context_mode(managed_context),
            "last_rewind_insufficient": self.insufficient_rewind_notice_for(session_id).map(|notice| {
                serde_json::json!({
                    "record_id": notice.record_id,
                    "used_tokens": notice.used_tokens,
                    "rewind_only_limit": notice.rewind_only_limit,
                    "context_window": notice.context_window,
                    "message": "The previous managed-context rewind did not reduce backend-reported pressure enough. Call list_rewind_anchors to inspect recovery candidates; pass include_non_recovery=true only for diagnostics, and never pass a recovery_eligible=false audit row to rewind_context. Use inspect_rewind_anchor when a compact row is ambiguous, then choose an exact returned item_id and position whose row or inspection supports enough pruning, with a denser carry-forward primer before using ordinary tools.",
                })
            }),
        })
    }

    fn context_pressure_snapshot(&self) -> serde_json::Value {
        self.context_pressure_snapshot_for(None, None)
    }

    fn is_active_codex_session(&self) -> bool {
        self.active_session_source
            .as_deref()
            .is_some_and(|source| source.eq_ignore_ascii_case("codex"))
    }

    fn active_codex_managed_context_enabled(&self) -> bool {
        self.is_active_codex_session() && self.codex_managed_context
    }

    fn exposed_codex_managed_context_enabled(&self) -> bool {
        if self.is_active_codex_session() {
            self.codex_managed_context
        } else {
            self.configured_codex_managed_context
        }
    }

    fn exposed_codex_managed_context_enabled_for(
        &self,
        session_id: Option<&str>,
        managed_context_override: Option<bool>,
    ) -> bool {
        if let Some(enabled) = managed_context_override {
            return enabled;
        }
        if let Some(id) = session_id.map(str::trim).filter(|id| !id.is_empty()) {
            for related in self.session_related_ids(id) {
                if let Some(enabled) = self.session_codex_managed_context.get(&related) {
                    return *enabled;
                }
            }
        }
        self.exposed_codex_managed_context_enabled()
    }

    fn active_codex_managed_context_enabled_for(
        &self,
        session_id: Option<&str>,
        managed_context_override: Option<bool>,
    ) -> bool {
        if let Some(enabled) = managed_context_override {
            return enabled;
        }
        if let Some(id) = session_id.map(str::trim).filter(|id| !id.is_empty()) {
            for related in self.session_related_ids(id) {
                if let Some(enabled) = self.session_codex_managed_context.get(&related) {
                    return *enabled;
                }
            }
        }
        self.active_codex_managed_context_enabled()
    }

    fn context_pressure_rewind_only_for(
        &self,
        session_id: Option<&str>,
    ) -> Option<(u64, u64, &'static str)> {
        let (used_tokens, context_window, hard_context_window) =
            self.session_usage_values(session_id);
        if context_window == 0 {
            return None;
        }
        let rewind_only_limit = context_window;
        let status = if hard_context_window.is_some_and(|hard| hard > 0 && used_tokens >= hard) {
            "critical"
        } else if used_tokens >= rewind_only_limit {
            "high"
        } else {
            return None;
        };
        Some((used_tokens, rewind_only_limit, status))
    }

    fn rewind_anchor_recovery_candidates_only_for(
        &self,
        _session_id: Option<&str>,
        _requested: Option<bool>,
        include_non_recovery: bool,
    ) -> bool {
        !include_non_recovery
    }

    fn rewind_only_gate_message(&self, tool_name: &str) -> Option<String> {
        self.rewind_only_gate_message_for(tool_name, None, None)
    }

    fn rewind_only_gate_message_for(
        &self,
        tool_name: &str,
        session_id: Option<&str>,
        managed_context_override: Option<bool>,
    ) -> Option<String> {
        if !self.active_codex_managed_context_enabled_for(session_id, managed_context_override)
            || rewind_only_allowed_tool(tool_name)
        {
            return None;
        }
        let (used_tokens, rewind_only_limit, status) =
            self.context_pressure_rewind_only_for(session_id)?;
        let mut message = format!(
            "Backend-reported Codex context pressure is {status} ({used_tokens}/{rewind_only_limit} tokens). Managed context is now in density-preservation mode: model-facing tools are limited to get_status, list_rewind_anchors, inspect_rewind_anchor, rewind_context, and rewind_backout until pressure is reduced below the threshold. Read-only supervisor observability tools such as get_logs and controller status remain available. The Intendant MCP tools list_rewind_anchors and inspect_rewind_anchor are available; any earlier transcript claim that either is unavailable is stale. Call list_rewind_anchors to inspect the compact valid recovery catalog; pass include_non_recovery=true only for diagnostics, and never pass a recovery_eligible=false audit row to rewind_context. Inspect a candidate if the compact row is ambiguous, then call rewind_context with an exact returned item_id, the returned position_hint or a value in positions, and a dense carry-forward primer before using other tools. A successful rewind only validates lineage; normal tools remain unavailable until backend-reported pressure is below the rewind-only limit. Do not synthesize anchor ids from prior failed tool calls."
        );
        if let Some(notice) = self.insufficient_rewind_notice_for(session_id) {
            message.push_str(&format!(
                " Previous managed-context record {} was insufficient; choose an exact returned item_id and position from list_rewind_anchors whose compact row or inspection supports enough additional pruning, with a denser carry-forward primer.",
                notice.record_id
            ));
        }
        Some(message)
    }

    fn approval_snapshot(&self) -> Option<ApprovalSnapshot> {
        self.pending_approval.as_ref().map(|p| ApprovalSnapshot {
            id: p.id,
            command_preview: p.command_preview.clone(),
            category: p.category.clone(),
        })
    }

    fn human_question_snapshot(&self) -> Option<HumanQuestionSnapshot> {
        self.human_question.as_ref().map(|q| HumanQuestionSnapshot {
            question: q.clone(),
        })
    }
}

fn rewind_only_allowed_tool(name: &str) -> bool {
    rewind_only_recovery_tool(name) || rewind_only_supervisor_observability_tool(name)
}

fn rewind_only_recovery_tool(name: &str) -> bool {
    matches!(
        name,
        "get_status"
            | "list_rewind_anchors"
            | "inspect_rewind_anchor"
            | "rewind_context"
            | "rewind_backout"
    )
}

fn rewind_only_supervisor_observability_tool(name: &str) -> bool {
    matches!(
        name,
        "get_logs"
            | "get_pending_approval"
            | "get_pending_input"
            | "get_restart_status"
            | "get_controller_loop_status"
    )
}

fn managed_context_tool(name: &str) -> bool {
    matches!(
        name,
        "list_rewind_anchors" | "inspect_rewind_anchor" | "rewind_context" | "rewind_backout"
    )
}

fn with_default_mcp_session_id(
    mut args: serde_json::Value,
    session_id: Option<&str>,
) -> serde_json::Value {
    let Some(session_id) = session_id.map(str::trim).filter(|id| !id.is_empty()) else {
        return args;
    };
    let Some(obj) = args.as_object_mut() else {
        return args;
    };
    let has_session_id = obj
        .get("session_id")
        .or_else(|| obj.get("sessionId"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .is_some_and(|value| !value.is_empty());
    if !has_session_id {
        obj.insert(
            "session_id".to_string(),
            serde_json::Value::String(session_id.to_string()),
        );
    }
    args
}

fn tool_allowed_for_profile(name: &str, managed_context: bool, profile: Option<&str>) -> bool {
    if !managed_context && managed_context_tool(name) {
        return false;
    }
    let Some(profile) = profile
        .map(str::trim)
        .filter(|profile| !profile.is_empty())
        .map(|profile| profile.to_ascii_lowercase())
    else {
        return true;
    };
    match profile.as_str() {
        "full" => true,
        // Codex should learn the broad Intendant surface lazily through
        // `intendant ctl --help` instead of receiving every MCP schema up front.
        // Keep the tiny always-useful status/collaboration set first-class.
        "core" | "codex-core" | "cli" | "minimal" => {
            matches!(
                name,
                "get_status"
                    | "show_shared_view"
                    | "focus_shared_view"
                    | "request_shared_view_input"
                    | "capture_shared_view_frame"
                    | "hide_shared_view"
            ) || (managed_context
                // Keep managed rewind tools reachable from Codex's small MCP
                // profile; descriptions and status decide when normal turns
                // should use them.
                && (managed_context_tool(name)
                    || matches!(
                        name,
                        "list_displays" | "take_screenshot" | "execute_cu_actions"
                    )))
        }
        "screen" | "display" => {
            matches!(
                name,
                "get_status"
                    | "list_displays"
                    | "list_browser_workspaces"
                    | "browser_workspace_providers"
                    | "create_browser_workspace"
                    | "close_browser_workspace"
                    | "acquire_browser_workspace"
                    | "release_browser_workspace"
                    | "take_screenshot"
                    | "execute_cu_actions"
                    | "list_frames"
                    | "read_frame"
                    | "show_shared_view"
                    | "focus_shared_view"
                    | "request_shared_view_input"
                    | "capture_shared_view_frame"
                    | "hide_shared_view"
            ) || (managed_context && managed_context_tool(name))
        }
        "managed" | "managed-context" => {
            matches!(name, "get_status") || (managed_context && managed_context_tool(name))
        }
        // Unknown profiles fail open so typoed third-party URLs do not silently
        // hide tools. Intendant-generated URLs use known profile names.
        _ => true,
    }
}

macro_rules! manual_http_tool_definition {
    ($name:literal, $description:literal, $params:ty) => {{
        let mut schema = serde_json::to_value(schemars::schema_for!($params)).unwrap_or_default();
        inline_schema_refs(&mut schema);
        serde_json::json!({
            "name": $name,
            "description": $description,
            "inputSchema": schema,
        })
    }};
}

fn append_manual_http_tool_definitions(
    tools: &mut Vec<serde_json::Value>,
    managed_context: bool,
    tool_profile: Option<&str>,
) {
    let mut push = |name: &'static str, definition: serde_json::Value| {
        if tool_allowed_for_profile(name, managed_context, tool_profile)
            && !tools
                .iter()
                .any(|tool| tool.get("name").and_then(serde_json::Value::as_str) == Some(name))
        {
            tools.push(definition);
        }
    };

    push(
        "rewind_context",
        manual_http_tool_definition!(
            "rewind_context",
            "Schedule a Codex context rewind to an exact item/tool-call anchor. Use only after managed-context recovery/density handoff guidance, rewind-only context pressure, a watch-pressure density decision, or genuinely noisy/unexpectedly large recent output makes a rewind necessary; do not use for ordinary low-pressure startup/search work. First call list_rewind_anchors and choose one returned item_id; call inspect_rewind_anchor when the compact row is ambiguous. Do not synthesize anchor ids from prior failed tool calls. The current turn will finish, Intendant will roll back Codex to the anchor, inject the primer as developer context, and resume the branch.",
            RewindContextParams
        ),
    );
    push(
        "list_rewind_anchors",
        manual_http_tool_definition!(
            "list_rewind_anchors",
            "Discover exact Codex rewind anchors only after you have already decided a managed-context rewind may be needed because recovery/density handoff guidance asked for it, context pressure is rewind-only or watch, or a recent completed tool result was genuinely noisy/unexpectedly large. Do not call during ordinary startup/status/search turns merely because managed_context=managed is enabled, or after bounded low-output searches while context_pressure.status is ok. By default returns one compact whole-catalog result covering all matching valid non-management anchors, with exact item_id values, accepted positions, item type/name/role, and short semantic summaries; it does not select or recommend one anchor. Use query or reverse only when you already have a semantic filter/order in mind. Use detail=true or explicit offset/limit for diagnostic detailed pages, and inspect_rewind_anchor for one anchor's before/after context. For density compaction, include_pruning_estimates=true adds approximate discard sizes to compact rows. The default catalog hides managed-context maintenance calls such as list_rewind_anchors, inspect_rewind_anchor, rewind_context, and rewind_backout so recovery does not target its own tool calls; pass include_management_tools=true only when intentionally targeting those internals. Normal model-facing results hide anchors known to remain at/above the rewind-only limit or without enough resume headroom, and narrow positions to values accepted by rewind_context; recovery_candidates_only=false alone is ignored. Pass include_non_recovery=true only for diagnostics/audit, and never pass a recovery_eligible=false audit row to rewind_context. Use inspect_rewind_anchor on a candidate when the compact summary is ambiguous, then copy the chosen item_id and position_hint, or a value in positions, into rewind_context.",
            ListRewindAnchorsParams
        ),
    );
    push(
        "inspect_rewind_anchor",
        manual_http_tool_definition!(
            "inspect_rewind_anchor",
            "Inspect a single exact Codex rewind anchor with a compact before/after context window. Use only after list_rewind_anchors returns a candidate for an already-needed rewind, when the row is too lossy to choose safely.",
            InspectRewindAnchorParams
        ),
    );
    push(
        "rewind_backout",
        manual_http_tool_definition!(
            "rewind_backout",
            "Inspect or restore a previous managed-context rewind/backout record. Restore mutates the active Codex thread in place; fork/backout create a lineage branch when the patched Codex binary is used.",
            RewindBackoutParams
        ),
    );
    push(
        "show_shared_view",
        manual_http_tool_definition!(
            "show_shared_view",
            "Open the dashboard shared display view for agent-human collaboration.",
            ShowSharedViewParams
        ),
    );
    push(
        "hide_shared_view",
        manual_http_tool_definition!(
            "hide_shared_view",
            "Dismiss the dashboard shared display view banner and focus overlay.",
            HideSharedViewParams
        ),
    );
    push(
        "focus_shared_view",
        manual_http_tool_definition!(
            "focus_shared_view",
            "Highlight a normalized region in the active dashboard shared display view.",
            FocusSharedViewParams
        ),
    );
    push(
        "request_shared_view_input",
        manual_http_tool_definition!(
            "request_shared_view_input",
            "Ask the user for input authority or human interaction on a shared display target.",
            RequestSharedViewInputParams
        ),
    );
    push(
        "capture_shared_view_frame",
        manual_http_tool_definition!(
            "capture_shared_view_frame",
            "Capture one frame from the active dashboard shared display view.",
            CaptureSharedViewFrameParams
        ),
    );
    push(
        "list_displays",
        manual_http_tool_definition!(
            "list_displays",
            "Enumerate available displays with their IDs, names, and resolutions.",
            EmptyToolParams
        ),
    );
    push(
        "take_screenshot",
        manual_http_tool_definition!(
            "take_screenshot",
            "Take a screenshot of a display. Returns an MCP image content block.",
            TakeScreenshotParams
        ),
    );
    push(
        "execute_cu_actions",
        manual_http_tool_definition!(
            "execute_cu_actions",
            "Execute computer-use actions on a display (click, type, scroll, etc). Returns action status plus an MCP image content block for the post-action screenshot. Set coordinate_space to \"normalized_1000\" if coordinates are on a 0-1000 grid.",
            ExecuteCuActionsParams
        ),
    );
}

fn apply_session_capabilities_to_mcp_state(
    s: &mut McpAppState,
    session_id: &str,
    capabilities: &crate::types::SessionCapabilities,
) -> bool {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return false;
    }
    let Some(mode) = capabilities.codex_managed_context.as_deref() else {
        return false;
    };
    let enabled = crate::project::codex_managed_context_enabled(mode);
    s.session_codex_managed_context
        .insert(session_id.to_string(), enabled);
    if s.session_id == session_id {
        s.codex_managed_context = enabled;
    }
    true
}

fn usage_snapshot_from_context_snapshot_event(
    source: &str,
    format: &str,
    token_count: Option<u64>,
    token_count_kind: Option<&str>,
    context_window: Option<u64>,
    hard_context_window: Option<u64>,
    raw: &serde_json::Value,
) -> Option<frontend::ModelUsageSnapshot> {
    if token_count_kind != Some("backend_reported") {
        return None;
    }
    let tokens_used = token_count?;
    let context_window = context_window?;
    if context_window == 0 {
        return None;
    }

    let provider = if format.starts_with("openai.") {
        "openai"
    } else if format.starts_with("anthropic.") {
        "anthropic"
    } else if format.starts_with("gemini.") {
        "gemini"
    } else {
        source
    };
    let model = raw
        .get("model")
        .and_then(|value| value.as_str())
        .filter(|model| !model.trim().is_empty())
        .unwrap_or(source);

    Some(frontend::ModelUsageSnapshot {
        provider: provider.to_string(),
        model: model.to_string(),
        tokens_used,
        context_window,
        hard_context_window,
        usage_pct: tokens_used as f64 / context_window as f64 * 100.0,
        prompt_tokens: tokens_used,
        completion_tokens: 0,
        cached_tokens: 0,
    })
}

fn apply_context_snapshot_usage_to_mcp_state(
    s: &mut McpAppState,
    session_id: Option<&str>,
    source: &str,
    format: &str,
    token_count: Option<u64>,
    token_count_kind: Option<&str>,
    context_window: Option<u64>,
    hard_context_window: Option<u64>,
    raw: &serde_json::Value,
) -> bool {
    let Some(main) = usage_snapshot_from_context_snapshot_event(
        source,
        format,
        token_count,
        token_count_kind,
        context_window,
        hard_context_window,
        raw,
    ) else {
        return false;
    };
    let main = s.normalize_main_usage_snapshot(session_id, main);
    s.record_session_usage_snapshot(session_id, main.clone());
    if s.session_id_applies_to_current_session(session_id) {
        s.apply_main_usage_snapshot(main);
    }
    s.complete_pending_rewind_pressure_check_for(session_id);
    true
}

fn context_rewind_record_id_from_message(message: &str) -> Option<String> {
    message
        .split(|ch: char| ch.is_whitespace() || matches!(ch, ',' | ';' | ')' | '('))
        .map(|part| {
            part.trim_matches(|ch: char| !(ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_')))
        })
        .find(|part| part.starts_with("rewind-") && part.len() > "rewind-".len())
        .map(str::to_string)
}

fn codex_thread_action_result_targets_session(
    requested_session_id: &Option<String>,
    result_session_id: &Option<String>,
) -> bool {
    match requested_session_id {
        Some(requested) => result_session_id.as_deref() == Some(requested.as_str()),
        None => true,
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RestartAfter {
    TurnEnd,
    Now,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RestartPhase {
    AwaitingTurnComplete,
    Ready,
    Restarting,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControllerRestartState {
    pub restart_id: String,
    pub controller_id: String,
    pub north_star_goal: String,
    pub reason: Option<String>,
    pub restart_after: RestartAfter,
    pub phase: RestartPhase,
    pub turn_complete_token: String,
    pub handoff_summary: Option<String>,
    pub completion_status: Option<String>,
    pub restart_command: Option<String>,
    pub auto_start_task: bool,
    pub max_attempts: u32,
    pub cooldown_sec: u64,
    pub attempts: u32,
    pub created_at: String,
    pub updated_at: String,
    pub last_attempt_at: Option<String>,
    pub last_error: Option<String>,
    pub last_result: Option<String>,
}

impl ControllerRestartState {
    fn now_string() -> String {
        chrono::Utc::now().to_rfc3339()
    }

    fn new(params: &ScheduleControllerRestartParams) -> Self {
        let now = Self::now_string();
        let restart_after = parse_restart_after(params.restart_after.as_deref())
            .expect("restart_after must be validated before creating ControllerRestartState");
        Self {
            restart_id: Uuid::new_v4().to_string(),
            controller_id: params.controller_id.clone(),
            north_star_goal: params.north_star_goal.clone(),
            reason: params.reason.clone(),
            restart_after,
            phase: match restart_after {
                RestartAfter::TurnEnd => RestartPhase::AwaitingTurnComplete,
                RestartAfter::Now => RestartPhase::Ready,
            },
            turn_complete_token: Uuid::new_v4().to_string(),
            handoff_summary: None,
            completion_status: None,
            restart_command: params.restart_command.clone(),
            auto_start_task: params.auto_start_task.unwrap_or(false),
            max_attempts: params.max_attempts.unwrap_or(1),
            cooldown_sec: params.cooldown_sec.unwrap_or(30),
            attempts: 0,
            created_at: now.clone(),
            updated_at: now,
            last_attempt_at: None,
            last_error: None,
            last_result: None,
        }
    }
}

fn parse_restart_after(raw: Option<&str>) -> Result<RestartAfter, String> {
    match raw.map(str::trim).map(str::to_lowercase).as_deref() {
        None | Some("") | Some("turn_end") => Ok(RestartAfter::TurnEnd),
        Some("now") => Ok(RestartAfter::Now),
        Some(other) => Err(format!(
            "Invalid request: restart_after must be 'turn_end' or 'now' (got '{}')",
            other
        )),
    }
}

fn normalize_string_field(value: &mut String) {
    *value = value.trim().to_string();
}

fn normalize_optional_string_field(value: &mut Option<String>) {
    if let Some(trimmed) = value.as_ref().map(|v| v.trim().to_string()) {
        if trimmed.is_empty() {
            *value = None;
        } else {
            *value = Some(trimmed);
        }
    }
}

fn normalize_schedule_controller_restart_params(params: &mut ScheduleControllerRestartParams) {
    normalize_string_field(&mut params.controller_id);
    normalize_string_field(&mut params.north_star_goal);
    normalize_optional_string_field(&mut params.reason);
    normalize_optional_string_field(&mut params.restart_after);
    if let Some(cmd) = params.restart_command.as_mut() {
        normalize_string_field(cmd);
    }
}

fn normalize_controller_turn_complete_params(params: &mut ControllerTurnCompleteParams) {
    normalize_string_field(&mut params.restart_id);
    normalize_string_field(&mut params.turn_complete_token);
    normalize_optional_string_field(&mut params.status);
    normalize_optional_string_field(&mut params.handoff_summary);
}

fn normalize_cancel_controller_restart_params(params: &mut CancelControllerRestartParams) {
    normalize_optional_string_field(&mut params.restart_id);
}

fn validate_schedule_controller_restart_params(
    params: &ScheduleControllerRestartParams,
) -> Result<(), String> {
    if params.controller_id.trim().is_empty() {
        return Err("Invalid request: controller_id must not be empty".to_string());
    }
    if params.north_star_goal.trim().is_empty() {
        return Err("Invalid request: north_star_goal must not be empty".to_string());
    }
    parse_restart_after(params.restart_after.as_deref())?;
    if matches!(params.max_attempts, Some(0)) {
        return Err("Invalid request: max_attempts must be >= 1".to_string());
    }
    if let Some(cmd) = params.restart_command.as_ref() {
        if cmd.trim().is_empty() {
            return Err("Invalid request: restart_command must not be empty".to_string());
        }
    }
    let has_restart_command = params
        .restart_command
        .as_ref()
        .map(|cmd| !cmd.trim().is_empty())
        .unwrap_or(false);
    let auto_start_task = params.auto_start_task.unwrap_or(false);
    if !has_restart_command && !auto_start_task {
        return Err(
            "Invalid request: configure at least one restart action (restart_command and/or auto_start_task=true)"
                .to_string(),
        );
    }
    Ok(())
}

fn restart_state_path(log_dir: &std::path::Path) -> std::path::PathBuf {
    log_dir.join("controller_restart.json")
}

fn persist_restart_state(log_dir: &std::path::Path, state: &Option<ControllerRestartState>) {
    let path = restart_state_path(log_dir);
    if let Some(s) = state {
        if let Ok(json) = serde_json::to_string_pretty(s) {
            let _ = std::fs::write(path, json);
        }
    } else {
        let _ = std::fs::remove_file(path);
    }
}

fn emit_control_result(
    control_tx: &Option<broadcast::Sender<String>>,
    action: &str,
    ok: bool,
    message: String,
    data: Option<serde_json::Value>,
) {
    if let Some(tx) = control_tx {
        let event = OutboundEvent::CommandResult {
            action: action.to_string(),
            ok,
            message,
            data,
        };
        control::broadcast_event(tx, &event);
    }
}

fn current_autonomy_label(level: AutonomyLevel) -> String {
    level.to_string().to_lowercase()
}

async fn emit_control_status(
    state: &SharedMcpState,
    control_tx: &Option<broadcast::Sender<String>>,
) {
    if let Some(tx) = control_tx {
        let s = state.read().await;
        let autonomy_level = s.autonomy.read().await.level;
        let event = OutboundEvent::Status {
            turn: s.turn,
            phase: phase_to_str(&s.phase).to_string(),
            autonomy: current_autonomy_label(autonomy_level),
            session_id: s.session_id.clone(),
            task: s.task_description.clone(),
            external_agent: s.external_agent.as_ref().map(|b| b.to_string()),
        };
        control::broadcast_event(tx, &event);
    }
}

async fn start_task_with_state(
    state: &SharedMcpState,
    bus: &EventBus,
    task: String,
    source: &str,
    orchestrate: Option<bool>,
) -> Result<(), String> {
    let mut s = state.write().await;

    match s.phase {
        Phase::Thinking
        | Phase::RunningAgent
        | Phase::Orchestrating
        | Phase::WaitingApproval
        | Phase::WaitingHuman
        | Phase::Interrupting => {
            return Err(format!(
                "agent is currently in '{}' phase",
                phase_to_str(&s.phase)
            ));
        }
        Phase::WaitingFollowUp => {
            // Send follow-up message to the existing round loop
            if let Some(ref tx) = s.follow_up_tx {
                let tx = tx.clone();
                let task_clone = task.clone();
                s.set_phase(Phase::Thinking);
                s.push_log(
                    LogLevel::Info,
                    format!("Follow-up submitted via {}: {}", source, task),
                );
                drop(s);
                tx.send(FollowUpMessage::text(task_clone))
                    .await
                    .map_err(|_| "follow-up channel closed".to_string())?;
                return Ok(());
            } else {
                // No follow-up channel — treat as fresh start
            }
        }
        Phase::Idle | Phase::Done | Phase::Interrupted => {}
    }

    let launcher = s
        .launcher
        .as_ref()
        .cloned()
        .ok_or_else(|| "no task launcher configured".to_string())?;

    s.turn = 0;
    s.budget_pct = 0.0;
    s.session_tokens = 0;
    s.session_prompt_tokens = 0;
    s.session_completion_tokens = 0;
    s.session_cached_tokens = 0;
    s.set_phase(Phase::Thinking);
    s.pending_approval = None;
    s.human_question = None;
    s.should_quit = false;
    s.next_task_orchestrate = orchestrate;
    s.push_log(
        LogLevel::Info,
        format!("Task started via {}: {}", source, task),
    );

    let bus = bus.clone();
    drop(s);

    let handle = (launcher)(task, bus).await;
    let mut s = state.write().await;
    s.task_handle = Some(handle);
    Ok(())
}

fn restart_state_public_value(state: Option<&ControllerRestartState>) -> serde_json::Value {
    let mut value = serde_json::to_value(state).unwrap_or(serde_json::Value::Null);
    if let Some(obj) = value.as_object_mut() {
        if obj.contains_key("turn_complete_token") {
            obj.insert(
                "turn_complete_token".to_string(),
                serde_json::Value::String("[redacted]".to_string()),
            );
        }
    }
    value
}

fn controller_loop_dir() -> std::path::PathBuf {
    if let Ok(root) = std::env::var("INTENDANT_PROJECT_ROOT") {
        return std::path::PathBuf::from(root).join(".intendant/controller-loop");
    }
    std::env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .join(".intendant/controller-loop")
}

fn read_trimmed(path: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn parse_pid_file(path: &std::path::Path) -> Option<u32> {
    read_trimmed(path)?.parse::<u32>().ok()
}

fn loop_run_dirs(loop_dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut runs: Vec<std::path::PathBuf> = std::fs::read_dir(loop_dir)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("20"))
                .unwrap_or(false)
        })
        .collect();
    runs.sort();
    runs
}

fn read_json_file(path: &std::path::Path) -> serde_json::Value {
    let Ok(text) = std::fs::read_to_string(path) else {
        return serde_json::Value::Null;
    };
    serde_json::from_str(&text).unwrap_or(serde_json::Value::Null)
}

fn intervention_order_report(run_dir: &std::path::Path) -> serde_json::Value {
    let path = run_dir.join("intervention.log");
    let Ok(text) = std::fs::read_to_string(path) else {
        return serde_json::json!({
            "has_log": false,
            "order_ok": true,
        });
    };

    let mut run_started: Option<usize> = None;
    let mut codex_started: Option<usize> = None;
    let mut cleanup_begin: Option<usize> = None;
    let mut cleanup_end: Option<usize> = None;

    for (idx, line) in text.lines().enumerate() {
        if run_started.is_none() && line.contains(" run_started ") {
            run_started = Some(idx);
        }
        if codex_started.is_none() && line.contains(" codex_started ") {
            codex_started = Some(idx);
        }
        if cleanup_begin.is_none() && line.contains(" cleanup_begin ") {
            cleanup_begin = Some(idx);
        }
        if cleanup_end.is_none() && line.contains(" cleanup_end ") {
            cleanup_end = Some(idx);
        }
    }

    let order_ok = match (run_started, codex_started, cleanup_begin, cleanup_end) {
        (Some(a), Some(b), Some(c), Some(d)) => a <= b && b <= c && c <= d,
        _ => true,
    };

    serde_json::json!({
        "has_log": true,
        "order_ok": order_ok,
        "run_started_line": run_started,
        "codex_started_line": codex_started,
        "cleanup_begin_line": cleanup_begin,
        "cleanup_end_line": cleanup_end,
    })
}

fn collect_controller_loop_status(loop_dir: &std::path::Path) -> serde_json::Value {
    collect_controller_loop_status_inner(loop_dir, None)
}

fn collect_controller_loop_status_for_mcp_state(
    loop_dir: &std::path::Path,
    state: &McpAppState,
) -> serde_json::Value {
    collect_controller_loop_status_inner(loop_dir, Some((state, current_unix_timestamp_secs())))
}

fn collect_controller_loop_status_inner(
    loop_dir: &std::path::Path,
    live_state: Option<(&McpAppState, u64)>,
) -> serde_json::Value {
    let halt = loop_dir.join("request_halt").exists();
    let halt_after_cycle = loop_dir.join("request_halt_after_cycle").exists();
    let mut stop_requested = loop_dir.join("request_stop").exists();
    let mut abort_requested = loop_dir.join("request_abort").exists();

    let lock_dir = loop_dir.join("active.lock");
    let lock_owner_pid = parse_pid_file(&lock_dir.join("pid"));
    let lock_owner_alive = lock_owner_pid
        .map(super::platform::process_alive)
        .unwrap_or(false);

    let mut active_wrappers = Vec::new();
    let mut active_codex = Vec::new();
    let mut known_codex_pids = HashSet::new();
    for run in loop_run_dirs(loop_dir) {
        let run_id = run
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        if let Some(pid) = parse_pid_file(&run.join("wrapper.pid")) {
            if super::platform::process_alive(pid) {
                active_wrappers.push(serde_json::json!({
                    "run_id": run_id,
                    "pid": pid
                }));
            }
        }
        if let Some(pid) = parse_pid_file(&run.join("codex.pid")) {
            known_codex_pids.insert(pid);
            if super::platform::process_alive(pid) {
                active_codex.push(serde_json::json!({
                    "run_id": run_id,
                    "pid": pid,
                    "source": "controller_loop",
                    "app_server_active": true,
                }));
            }
        }
    }
    let process_tree_codex = live_codex_app_server_processes(std::process::id(), &known_codex_pids);
    let process_tree_codex_pids: Vec<u32> = process_tree_codex
        .iter()
        .filter_map(|entry| entry.get("pid").and_then(|pid| pid.as_u64()))
        .filter_map(|pid| u32::try_from(pid).ok())
        .collect();
    active_codex.extend(process_tree_codex);
    active_wrappers.extend(active_external_wrappers_from_index(
        loop_dir,
        &process_tree_codex_pids,
    ));
    if let Some((state, now_secs)) = live_state {
        enrich_controller_loop_wrappers_with_mcp_state(&mut active_wrappers, state, now_secs);
    }

    let latest_run_id = read_trimmed(&loop_dir.join("latest.run_id"));
    let latest_status_file = read_json_file(&loop_dir.join("latest.status.json"));
    let latest_status = controller_loop_latest_status(latest_status_file, &active_wrappers);
    let latest_target_path = std::fs::read_link(loop_dir.join("latest")).ok().map(|p| {
        if p.is_absolute() {
            p
        } else {
            loop_dir.join(p)
        }
    });
    let latest_target = latest_target_path
        .as_ref()
        .map(|p| p.to_string_lossy().to_string());
    let latest_pid = parse_pid_file(&loop_dir.join("latest.pid"));
    let latest_pid_alive = latest_pid
        .map(super::platform::process_alive)
        .unwrap_or(false);
    let stale_intervention_cleared = (stop_requested || abort_requested)
        && controller_loop_intervention_markers_are_stale(
            lock_owner_alive,
            latest_pid_alive,
            &active_wrappers,
            &active_codex,
        );
    if stale_intervention_cleared {
        clear_loop_intervention_markers(loop_dir).ok();
        stop_requested = false;
        abort_requested = false;
    }
    let intervention_order = latest_target_path
        .as_ref()
        .map(|p| intervention_order_report(p))
        .unwrap_or_else(|| {
            serde_json::json!({
                "has_log": false,
                "order_ok": true,
            })
        });

    serde_json::json!({
        "loop_dir": loop_dir.to_string_lossy(),
        "flags": {
            "halt": halt,
            "halt_after_cycle": halt_after_cycle,
            "stop_requested": stop_requested,
            "abort_requested": abort_requested,
            "stale_intervention_cleared": stale_intervention_cleared,
        },
        "lock": {
            "present": lock_dir.exists(),
            "owner_pid": lock_owner_pid,
            "owner_alive": lock_owner_alive,
        },
        "latest": {
            "run_id": latest_run_id,
            "pid": latest_pid,
            "status": latest_status,
            "target": latest_target,
            "intervention_order": intervention_order,
        },
        "active": {
            "wrapper_count": active_wrappers.len(),
            "codex_count": active_codex.len(),
            "wrappers": active_wrappers,
            "codex": active_codex,
        }
    })
}

fn controller_loop_intervention_markers_are_stale(
    lock_owner_alive: bool,
    latest_pid_alive: bool,
    active_wrappers: &[serde_json::Value],
    active_codex: &[serde_json::Value],
) -> bool {
    if lock_owner_alive || latest_pid_alive {
        return false;
    }
    if active_wrappers.is_empty() && !active_codex.is_empty() {
        return false;
    }
    active_wrappers
        .iter()
        .all(controller_loop_active_wrapper_is_idle_external_app_server)
}

fn controller_loop_active_wrapper_is_idle_external_app_server(wrapper: &serde_json::Value) -> bool {
    if wrapper.get("source").and_then(|value| value.as_str()) != Some("external_wrapper_index") {
        return false;
    }
    wrapper
        .get("session_meta_status")
        .and_then(|value| value.as_str())
        .map(controller_loop_state_is_idle)
        .unwrap_or_else(|| {
            wrapper
                .get("status")
                .and_then(|value| value.as_str())
                .map(controller_loop_state_is_idle)
                .unwrap_or(false)
        })
}

fn active_external_wrappers_from_index(
    loop_dir: &std::path::Path,
    live_codex_pids: &[u32],
) -> Vec<serde_json::Value> {
    let candidate_homes = controller_loop_wrapper_index_homes(loop_dir);
    active_external_wrappers_from_index_homes(candidate_homes.iter(), live_codex_pids)
}

fn active_external_wrappers_from_index_homes<'a, I>(
    candidate_homes: I,
    live_codex_pids: &[u32],
) -> Vec<serde_json::Value>
where
    I: IntoIterator<Item = &'a std::path::PathBuf>,
{
    active_external_wrappers_from_index_homes_with_probe(
        candidate_homes,
        live_codex_pids,
        codex_app_server_process_tree_active,
    )
}

fn active_external_wrappers_from_index_homes_with_probe<'a, I, F>(
    candidate_homes: I,
    live_codex_pids: &[u32],
    process_tree_active: F,
) -> Vec<serde_json::Value>
where
    I: IntoIterator<Item = &'a std::path::PathBuf>,
    F: FnMut(u32) -> bool,
{
    active_external_wrappers_from_index_homes_with_probe_and_cwd(
        candidate_homes,
        live_codex_pids,
        process_tree_active,
        live_process_cwd,
    )
}

fn active_external_wrappers_from_index_homes_with_probe_and_cwd<'a, I, F, G>(
    candidate_homes: I,
    live_codex_pids: &[u32],
    mut process_tree_active: F,
    mut process_cwd: G,
) -> Vec<serde_json::Value>
where
    I: IntoIterator<Item = &'a std::path::PathBuf>,
    F: FnMut(u32) -> bool,
    G: FnMut(u32) -> Option<std::path::PathBuf>,
{
    if live_codex_pids.is_empty() {
        return Vec::new();
    }
    let mut seen_backend_ids = HashSet::new();
    let mut wrappers = Vec::new();
    for home in candidate_homes {
        for record in crate::external_wrapper_index::wrappers_for_source(home, "codex") {
            if wrappers.len() >= live_codex_pids.len() {
                break;
            }
            if !seen_backend_ids.insert(record.backend_session_id.clone()) {
                continue;
            }
            let status = session_meta_status(std::path::Path::new(&record.log_path));
            if external_wrapper_status_is_terminal(status.as_deref()) {
                continue;
            }
            let codex_pid = live_codex_pids.get(wrappers.len()).copied();
            let process_tree_active = codex_pid
                .map(|pid| process_tree_active(pid))
                .unwrap_or(false);
            let effective_status =
                effective_external_wrapper_status(status.as_deref(), process_tree_active);
            let cwd = codex_pid.and_then(|pid| process_cwd(pid));
            let cwd_string = cwd.as_ref().map(|path| path.to_string_lossy().to_string());
            let project_root = cwd
                .as_deref()
                .and_then(project_root_from_process_cwd)
                .map(|path| path.to_string_lossy().to_string())
                .or_else(|| record.project_root.clone());
            let updated_at_secs =
                fresh_external_wrapper_updated_at_secs(std::path::Path::new(&record.log_path))
                    .max(record.updated_at_secs);
            wrappers.push(serde_json::json!({
                "run_id": serde_json::Value::Null,
                "pid": serde_json::Value::Null,
                "codex_pid": codex_pid,
                "app_server_pid": codex_pid,
                "app_server_active": process_tree_active,
                "source": "external_wrapper_index",
                "backend_source": record.source,
                "backend_session_id": record.backend_session_id,
                "intendant_session_id": record.intendant_session_id,
                "log_path": record.log_path,
                "cwd": cwd_string,
                "project_root": project_root,
                "status": effective_status,
                "session_meta_status": status,
                "process_tree_active": process_tree_active,
                "updated_at_secs": updated_at_secs,
            }));
        }
        if wrappers.len() >= live_codex_pids.len() {
            break;
        }
    }
    wrappers
}

fn project_root_from_process_cwd(cwd: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut current = Some(cwd);
    while let Some(path) = current {
        if path.join(".git").exists() {
            return Some(path.to_path_buf());
        }
        current = path.parent();
    }
    Some(cwd.to_path_buf())
}

fn live_process_cwd(pid: u32) -> Option<std::path::PathBuf> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

fn controller_loop_latest_status(
    latest_status_file: serde_json::Value,
    wrappers: &[serde_json::Value],
) -> serde_json::Value {
    let active_wrapper_status = latest_status_from_active_wrappers(wrappers);
    if let Some(status) = active_wrapper_status.as_ref().filter(|status| {
        status
            .get("live_status_source")
            .and_then(|source| source.as_str())
            == Some("mcp_state")
    }) {
        return status.clone();
    }
    if latest_status_file.is_null() {
        return active_wrapper_status.unwrap_or(serde_json::Value::Null);
    }
    if controller_loop_status_state_is_idle(&latest_status_file) {
        if let Some(status) = active_wrapper_status {
            if !controller_loop_status_state_is_idle(&status) {
                return status;
            }
        }
    }
    latest_status_file
}

fn latest_status_from_active_wrappers(wrappers: &[serde_json::Value]) -> Option<serde_json::Value> {
    let wrapper = wrappers.iter().find(|wrapper| {
        wrapper.get("source").and_then(|v| v.as_str()) == Some("external_wrapper_index")
    })?;
    let state = wrapper
        .get("phase")
        .and_then(|v| v.as_str())
        .or_else(|| wrapper.get("status").and_then(|v| v.as_str()))
        .unwrap_or("active");
    Some(serde_json::json!({
        "run_id": serde_json::Value::Null,
        "state": state,
        "pid": serde_json::Value::Null,
        "codex_pid": wrapper.get("codex_pid").cloned().unwrap_or(serde_json::Value::Null),
        "source": "external_wrapper_index",
        "backend_source": wrapper.get("backend_source").cloned().unwrap_or(serde_json::Value::Null),
        "backend_session_id": wrapper.get("backend_session_id").cloned().unwrap_or(serde_json::Value::Null),
        "intendant_session_id": wrapper.get("intendant_session_id").cloned().unwrap_or(serde_json::Value::Null),
        "log_path": wrapper.get("log_path").cloned().unwrap_or(serde_json::Value::Null),
        "session_meta_status": wrapper.get("session_meta_status").cloned().unwrap_or(serde_json::Value::Null),
        "process_tree_active": wrapper.get("process_tree_active").cloned().unwrap_or(serde_json::Value::Null),
        "app_server_pid": wrapper.get("app_server_pid").cloned().unwrap_or_else(|| wrapper.get("codex_pid").cloned().unwrap_or(serde_json::Value::Null)),
        "app_server_active": wrapper.get("app_server_active").cloned().unwrap_or_else(|| wrapper.get("process_tree_active").cloned().unwrap_or(serde_json::Value::Null)),
        "phase": wrapper.get("phase").cloned().unwrap_or(serde_json::Value::Null),
        "turn": wrapper.get("turn").cloned().unwrap_or(serde_json::Value::Null),
        "round": wrapper.get("round").cloned().unwrap_or(serde_json::Value::Null),
        "task": wrapper.get("task").cloned().unwrap_or(serde_json::Value::Null),
        "updated_at_secs": wrapper.get("updated_at_secs").cloned().unwrap_or(serde_json::Value::Null),
        "live_status_source": wrapper.get("live_status_source").cloned().unwrap_or(serde_json::Value::Null),
    }))
}

async fn collect_controller_loop_status_with_state(
    loop_dir: &std::path::Path,
    state: &SharedMcpState,
) -> serde_json::Value {
    let s = state.read().await;
    collect_controller_loop_status_for_mcp_state(loop_dir, &s)
}

fn enrich_controller_loop_status_with_mcp_state_at(
    status: &mut serde_json::Value,
    state: &McpAppState,
    now_secs: u64,
) {
    let current_latest = status
        .pointer("/latest/status")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let latest = {
        let Some(wrappers) = status
            .pointer_mut("/active/wrappers")
            .and_then(serde_json::Value::as_array_mut)
        else {
            return;
        };

        enrich_controller_loop_wrappers_with_mcp_state(wrappers, state, now_secs);

        controller_loop_latest_status(current_latest, wrappers)
    };
    if let Some(latest_obj) = status
        .pointer_mut("/latest")
        .and_then(serde_json::Value::as_object_mut)
    {
        latest_obj.insert("status".to_string(), latest);
    }
}

fn enrich_controller_loop_wrappers_with_mcp_state(
    wrappers: &mut [serde_json::Value],
    state: &McpAppState,
    now_secs: u64,
) {
    for wrapper in wrappers {
        enrich_controller_loop_wrapper_with_mcp_state(wrapper, state, now_secs);
    }
}

fn enrich_controller_loop_wrapper_with_mcp_state(
    wrapper: &mut serde_json::Value,
    state: &McpAppState,
    now_secs: u64,
) {
    if wrapper.get("source").and_then(|value| value.as_str()) != Some("external_wrapper_index") {
        return;
    }
    let live_status = [
        wrapper
            .get("intendant_session_id")
            .and_then(serde_json::Value::as_str),
        wrapper
            .get("backend_session_id")
            .and_then(serde_json::Value::as_str),
    ]
    .into_iter()
    .flatten()
    .find_map(|session_id| state.session_status_for_id(session_id).cloned());
    let Some(live_status) = live_status else {
        return;
    };

    let phase = phase_to_str(&live_status.phase);
    let Some(obj) = wrapper.as_object_mut() else {
        return;
    };
    obj.insert(
        "phase".to_string(),
        serde_json::Value::String(phase.to_string()),
    );
    obj.insert(
        "turn".to_string(),
        serde_json::Value::Number(serde_json::Number::from(live_status.turn as u64)),
    );
    obj.insert(
        "round".to_string(),
        serde_json::Value::Number(serde_json::Number::from(live_status.round as u64)),
    );
    if !live_status.task.is_empty() {
        obj.insert(
            "task".to_string(),
            serde_json::Value::String(live_status.task),
        );
    }
    obj.insert(
        "live_status_source".to_string(),
        serde_json::Value::String("mcp_state".to_string()),
    );

    if !controller_loop_phase_is_active_turn(&live_status.phase) {
        return;
    }

    let raw_meta_status = obj
        .get("session_meta_status")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    obj.entry("raw_session_meta_status".to_string())
        .or_insert(raw_meta_status);
    let wrapper_index_updated_at_secs = obj
        .get("updated_at_secs")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    obj.entry("wrapper_index_updated_at_secs".to_string())
        .or_insert(wrapper_index_updated_at_secs);
    obj.insert(
        "status".to_string(),
        serde_json::Value::String(phase.to_string()),
    );
    obj.insert(
        "session_meta_status".to_string(),
        serde_json::Value::String(phase.to_string()),
    );
    obj.insert(
        "updated_at_secs".to_string(),
        serde_json::Value::Number(serde_json::Number::from(now_secs)),
    );
}

fn controller_loop_phase_is_active_turn(phase: &Phase) -> bool {
    matches!(
        phase,
        Phase::Thinking
            | Phase::RunningAgent
            | Phase::Orchestrating
            | Phase::WaitingApproval
            | Phase::WaitingHuman
            | Phase::Interrupting
    )
}

fn fresh_external_wrapper_updated_at_secs(log_dir: &std::path::Path) -> u64 {
    file_mtime_secs(&log_dir.join("session.jsonl")).max(file_mtime_secs(log_dir))
}

fn current_unix_timestamp_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn file_mtime_secs(path: &std::path::Path) -> u64 {
    path.metadata()
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn controller_loop_home(loop_dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let intendant_dir = loop_dir.parent()?;
    if intendant_dir.file_name().and_then(|name| name.to_str()) != Some(".intendant") {
        return None;
    }
    intendant_dir.parent().map(std::path::Path::to_path_buf)
}

fn controller_loop_wrapper_index_homes(loop_dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut homes = Vec::new();
    let mut seen = HashSet::new();
    for home in [
        controller_loop_home(loop_dir),
        Some(crate::platform::home_dir()),
    ]
    .into_iter()
    .flatten()
    {
        if seen.insert(home.clone()) {
            homes.push(home);
        }
    }
    homes
}

fn session_meta_status(log_dir: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(log_dir.join("session_meta.json")).ok()?;
    serde_json::from_str::<crate::session_log::SessionMeta>(&text)
        .ok()
        .and_then(|meta| meta.status)
}

fn external_wrapper_status_is_terminal(status: Option<&str>) -> bool {
    matches!(
        status,
        Some("completed" | "abandoned" | "interrupted" | "deleted")
    )
}

fn effective_external_wrapper_status(status: Option<&str>, process_tree_active: bool) -> String {
    let status = status.map(str::trim).filter(|status| !status.is_empty());
    if process_tree_active && status.map(controller_loop_state_is_idle).unwrap_or(true) {
        return "unknown_running".to_string();
    }
    status.unwrap_or("active").to_string()
}

fn controller_loop_status_state_is_idle(status: &serde_json::Value) -> bool {
    status
        .get("state")
        .or_else(|| status.get("status"))
        .and_then(|value| value.as_str())
        .map(controller_loop_state_is_idle)
        .unwrap_or(false)
}

fn controller_loop_state_is_idle(status: &str) -> bool {
    matches!(
        status.trim(),
        "" | "idle" | "waiting_follow_up" | "waiting_followup" | "waiting_for_task"
    )
}

fn codex_app_server_process_tree_active(pid: u32) -> bool {
    codex_app_server_process_tree_active_with_root(
        pid,
        super::platform::process_descendants(pid),
        super::platform::process_alive,
        super::platform::process_cmdline,
    )
}

fn codex_app_server_process_tree_active_with_root<I, A, C>(
    root_pid: u32,
    descendants: I,
    mut process_alive: A,
    process_cmdline: C,
) -> bool
where
    I: IntoIterator<Item = u32>,
    A: FnMut(u32) -> bool,
    C: FnMut(u32) -> Option<String>,
{
    if process_alive(root_pid) {
        return true;
    }
    codex_app_server_process_tree_active_from_descendants(
        descendants,
        process_alive,
        process_cmdline,
    )
}

fn codex_app_server_process_tree_active_from_descendants<I, A, C>(
    descendants: I,
    mut process_alive: A,
    mut process_cmdline: C,
) -> bool
where
    I: IntoIterator<Item = u32>,
    A: FnMut(u32) -> bool,
    C: FnMut(u32) -> Option<String>,
{
    descendants.into_iter().any(|pid| {
        process_alive(pid)
            && process_cmdline(pid)
                .map(|cmdline| !cmdline.trim().is_empty())
                .unwrap_or(false)
    })
}

fn live_codex_app_server_processes(
    root_pid: u32,
    known_codex_pids: &HashSet<u32>,
) -> Vec<serde_json::Value> {
    live_codex_app_server_pids(root_pid, known_codex_pids)
        .into_iter()
        .map(|pid| {
            serde_json::json!({
                "run_id": serde_json::Value::Null,
                "pid": pid,
                "source": "process_tree",
                "app_server_active": true,
            })
        })
        .collect()
}

fn live_codex_app_server_pids(root_pid: u32, known_codex_pids: &HashSet<u32>) -> Vec<u32> {
    live_codex_app_server_pids_from_descendants(
        super::platform::process_descendants(root_pid),
        known_codex_pids,
        super::platform::process_cmdline,
    )
}

fn live_codex_app_server_pids_from_descendants<I, F>(
    descendant_pids: I,
    known_codex_pids: &HashSet<u32>,
    mut cmdline_for_pid: F,
) -> Vec<u32>
where
    I: IntoIterator<Item = u32>,
    F: FnMut(u32) -> Option<String>,
{
    let mut pids = Vec::new();
    for pid in descendant_pids {
        if known_codex_pids.contains(&pid) {
            continue;
        }
        let Some(cmdline) = cmdline_for_pid(pid) else {
            continue;
        };
        if is_codex_app_server_cmdline(&cmdline) {
            pids.push(pid);
        }
    }
    pids.sort_unstable();
    pids.dedup();
    pids
}

fn is_codex_app_server_cmdline(cmdline: &str) -> bool {
    let mut args = cmdline.split_whitespace();
    args.any(|arg| arg.ends_with("codex")) && args.any(|arg| arg == "app-server")
}

fn request_loop_halt_marker(loop_dir: &std::path::Path, persistent: bool) -> Result<(), String> {
    std::fs::create_dir_all(loop_dir).map_err(|e| format!("Failed to create loop dir: {}", e))?;
    if persistent {
        std::fs::write(loop_dir.join("request_halt"), b"")
            .map_err(|e| format!("Failed to write request_halt: {}", e))?;
    } else {
        std::fs::write(loop_dir.join("request_halt_after_cycle"), b"")
            .map_err(|e| format!("Failed to write request_halt_after_cycle: {}", e))?;
    }
    Ok(())
}

fn clear_loop_halt_markers(loop_dir: &std::path::Path) -> Result<(), String> {
    std::fs::remove_file(loop_dir.join("request_halt")).ok();
    std::fs::remove_file(loop_dir.join("request_halt_after_cycle")).ok();
    clear_loop_intervention_markers(loop_dir)?;
    Ok(())
}

fn clear_loop_intervention_markers(loop_dir: &std::path::Path) -> Result<(), String> {
    std::fs::remove_file(loop_dir.join("request_stop")).ok();
    std::fs::remove_file(loop_dir.join("request_abort")).ok();
    Ok(())
}

fn normalize_intervention_mode(mode: &str) -> String {
    mode.trim().to_lowercase()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControllerLoopInterventionMode {
    Stop,
    Abort,
}

impl ControllerLoopInterventionMode {
    fn parse(mode: &str) -> Result<Self, String> {
        match normalize_intervention_mode(mode).as_str() {
            "stop" => Ok(Self::Stop),
            "abort" => Ok(Self::Abort),
            other => Err(format!(
                "Invalid mode '{}': expected 'stop' or 'abort'",
                other
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Abort => "abort",
        }
    }

    fn marker_name(self) -> &'static str {
        match self {
            Self::Stop => "request_stop",
            Self::Abort => "request_abort",
        }
    }

    fn process_signal(self) -> super::platform::ProcessSignal {
        match self {
            Self::Stop => super::platform::ProcessSignal::Terminate,
            Self::Abort => super::platform::ProcessSignal::Kill,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ControllerLoopIntervention {
    mode: ControllerLoopInterventionMode,
    signaled_codex_app_server_pids: Vec<u32>,
}

fn request_loop_intervention_marker(
    loop_dir: &std::path::Path,
    mode: &str,
) -> Result<ControllerLoopIntervention, String> {
    request_loop_intervention_marker_for_root(loop_dir, mode, std::process::id())
}

fn request_loop_intervention_marker_for_root(
    loop_dir: &std::path::Path,
    mode: &str,
    root_pid: u32,
) -> Result<ControllerLoopIntervention, String> {
    std::fs::create_dir_all(loop_dir).map_err(|e| format!("Failed to create loop dir: {}", e))?;
    let mode = ControllerLoopInterventionMode::parse(mode)?;
    let marker_name = mode.marker_name();
    std::fs::write(loop_dir.join(marker_name), b"")
        .map_err(|e| format!("Failed to write {}: {}", marker_name, e))?;

    let signaled_codex_app_server_pids = signal_live_codex_app_server_processes(root_pid, mode);
    Ok(ControllerLoopIntervention {
        mode,
        signaled_codex_app_server_pids,
    })
}

fn signal_live_codex_app_server_processes(
    root_pid: u32,
    mode: ControllerLoopInterventionMode,
) -> Vec<u32> {
    let known_codex_pids = HashSet::new();
    let pids = live_codex_app_server_pids(root_pid, &known_codex_pids);
    for pid in &pids {
        let _ = super::platform::signal_process_tree_now(*pid, mode.process_signal());
    }
    pids
}

fn controller_loop_intervention_report(
    intervention: &ControllerLoopIntervention,
) -> serde_json::Value {
    serde_json::json!({
        "mode": intervention.mode.as_str(),
        "signaled_codex_app_server_count": intervention.signaled_codex_app_server_pids.len(),
        "signaled_codex_app_server_pids": &intervention.signaled_codex_app_server_pids,
    })
}

fn add_controller_loop_intervention_report(
    status: &mut serde_json::Value,
    intervention: &ControllerLoopIntervention,
) {
    if let Some(obj) = status.as_object_mut() {
        obj.insert(
            "intervention".to_string(),
            controller_loop_intervention_report(intervention),
        );
    }
}

async fn spawn_detached_restart_command(cmd: &str) -> Result<u32, String> {
    // Delegate to the platform helper: `nohup setsid bash -lc` on Unix
    // (unchanged), a detached window-less `cmd.exe /C` child on Windows.
    super::platform::spawn_detached_restart(cmd).await
}

async fn run_scheduled_controller_restart_with_state(
    state: &SharedMcpState,
    bus: &EventBus,
) -> Result<String, String> {
    let (restart, log_dir) = {
        let mut s = state.write().await;
        let log_dir = s.log_dir.clone();
        let Some(active) = s.controller_restart.as_mut() else {
            return Err("No scheduled controller restart".to_string());
        };

        if !matches!(active.phase, RestartPhase::Ready) {
            return Err(format!(
                "Restart is not ready (current phase: {:?})",
                active.phase
            ));
        }

        if active.attempts >= active.max_attempts {
            active.phase = RestartPhase::Failed;
            active.last_error = Some("Max restart attempts reached".to_string());
            active.updated_at = ControllerRestartState::now_string();
            let snapshot = s.controller_restart.clone();
            persist_restart_state(&log_dir, &snapshot);
            return Err("Max restart attempts reached".to_string());
        }

        if let Some(last_attempt) = &active.last_attempt_at {
            if let Ok(last) = chrono::DateTime::parse_from_rfc3339(last_attempt) {
                let elapsed = chrono::Utc::now() - last.with_timezone(&chrono::Utc);
                if elapsed.num_seconds() < active.cooldown_sec as i64 {
                    return Err(format!(
                        "Restart cooldown active ({}s remaining)",
                        active
                            .cooldown_sec
                            .saturating_sub(elapsed.num_seconds() as u64)
                    ));
                }
            }
        }

        active.phase = RestartPhase::Restarting;
        active.attempts += 1;
        active.last_attempt_at = Some(ControllerRestartState::now_string());
        active.updated_at = ControllerRestartState::now_string();
        active.last_error = None;
        active.last_result = None;
        let restart = active.clone();
        let snapshot = s.controller_restart.clone();
        persist_restart_state(&log_dir, &snapshot);
        (restart, log_dir)
    };

    let mut result_parts = Vec::new();

    if let Some(cmd) = &restart.restart_command {
        match spawn_detached_restart_command(cmd).await {
            Ok(pid) => {
                result_parts.push(format!("spawned controller command (pid {})", pid));
            }
            Err(e) => {
                let mut s = state.write().await;
                if let Some(active) = s.controller_restart.as_mut() {
                    active.phase = RestartPhase::Failed;
                    active.last_error = Some(format!("Failed to spawn restart_command: {}", e));
                    active.updated_at = ControllerRestartState::now_string();
                }
                let snapshot = s.controller_restart.clone();
                persist_restart_state(&log_dir, &snapshot);
                return Err(format!("Failed to spawn restart_command: {}", e));
            }
        }
    }

    if restart.auto_start_task {
        if let Err(e) = start_task_with_state(
            state,
            bus,
            restart.north_star_goal.clone(),
            "controller_restart",
            None, // auto-start uses default mode selection
        )
        .await
        {
            let failure = format!("Failed to start follow-up task: {}", e);
            let mut s = state.write().await;
            if let Some(active) = s.controller_restart.as_mut() {
                active.phase = RestartPhase::Failed;
                active.last_error = Some(failure.clone());
                active.updated_at = ControllerRestartState::now_string();
            }
            let snapshot = s.controller_restart.clone();
            persist_restart_state(&log_dir, &snapshot);
            return Err(failure);
        }
        result_parts.push("started autonomous follow-up task".to_string());
    }

    if restart.restart_command.is_none() && !restart.auto_start_task {
        let mut s = state.write().await;
        if let Some(active) = s.controller_restart.as_mut() {
            active.phase = RestartPhase::Failed;
            active.last_error = Some(
                "No restart action configured: set restart_command and/or auto_start_task=true"
                    .to_string(),
            );
            active.updated_at = ControllerRestartState::now_string();
        }
        let snapshot = s.controller_restart.clone();
        persist_restart_state(&log_dir, &snapshot);
        return Err("No restart action configured".to_string());
    }

    let mut s = state.write().await;
    if let Some(active) = s.controller_restart.as_mut() {
        active.phase = RestartPhase::Completed;
        active.last_result = Some(if result_parts.is_empty() {
            "ok".to_string()
        } else {
            result_parts.join("; ")
        });
        active.updated_at = ControllerRestartState::now_string();
    }
    let snapshot = s.controller_restart.clone();
    persist_restart_state(&log_dir, &snapshot);

    Ok(result_parts.join("; "))
}

fn restart_phase_value(state: &ControllerRestartState) -> serde_json::Value {
    serde_json::to_value(state.phase).unwrap_or(serde_json::Value::Null)
}

fn restart_error_response(
    status: &str,
    restart_id: &str,
    phase: Option<RestartPhase>,
    error: String,
) -> String {
    let mut output = serde_json::json!({
        "status": status,
        "restart_id": restart_id,
        "ok": false,
        "error": error,
    });
    if let Some(phase) = phase {
        output["phase"] = serde_json::to_value(phase).unwrap_or(serde_json::Value::Null);
    }
    serde_json::to_string_pretty(&output).unwrap_or_else(|_| "{}".to_string())
}

fn schedule_error_response(
    error: String,
    restart_id: Option<&str>,
    phase: Option<RestartPhase>,
) -> String {
    let mut output = serde_json::json!({
        "status": "rejected",
        "ok": false,
        "error": error,
    });
    if let Some(restart_id) = restart_id {
        output["restart_id"] = serde_json::Value::String(restart_id.to_string());
    }
    if let Some(phase) = phase {
        output["phase"] = serde_json::to_value(phase).unwrap_or(serde_json::Value::Null);
    }
    serde_json::to_string_pretty(&output).unwrap_or_else(|_| "{}".to_string())
}

async fn handle_control_command_mcp(
    state: &SharedMcpState,
    bus: &EventBus,
    control_tx: &Option<broadcast::Sender<String>>,
    msg: ControlMsg,
) -> Option<&'static str> {
    match msg {
        ControlMsg::Status { .. } => {
            emit_control_status(state, control_tx).await;
            None
        }
        ControlMsg::Usage => {
            if let Some(tx) = control_tx {
                let s = state.read().await;
                let event = OutboundEvent::Usage {
                    session_id: None,
                    main: s.usage_snapshot().main,
                    presence: s.usage_snapshot().presence,
                };
                control::broadcast_event(tx, &event);
            }
            None
        }
        ControlMsg::Approve { id, .. } => {
            let mut s = state.write().await;
            let outcome = process_action_sync(&mut s, UserAction::Approve { id });
            if matches!(outcome, ActionOutcome::Ok) {
                bus.send(AppEvent::ApprovalResolved {
                    session_id: None,
                    id,
                    action: "approve".to_string(),
                });
            }
            emit_control_result(
                control_tx,
                "approve",
                matches!(outcome, ActionOutcome::Ok),
                format_outcome(outcome),
                None,
            );
            Some(RESOURCE_APPROVAL_URI)
        }
        ControlMsg::Deny { id, .. } => {
            let mut s = state.write().await;
            let outcome = process_action_sync(&mut s, UserAction::Deny { id });
            if matches!(outcome, ActionOutcome::Ok) {
                bus.send(AppEvent::ApprovalResolved {
                    session_id: None,
                    id,
                    action: "deny".to_string(),
                });
            }
            emit_control_result(
                control_tx,
                "deny",
                matches!(outcome, ActionOutcome::Ok),
                format_outcome(outcome),
                None,
            );
            Some(RESOURCE_APPROVAL_URI)
        }
        ControlMsg::Input { text } => {
            let mut s = state.write().await;
            let outcome = process_action_sync(&mut s, UserAction::RespondHuman { text });
            emit_control_result(
                control_tx,
                "input",
                matches!(outcome, ActionOutcome::Ok),
                format_outcome(outcome),
                None,
            );
            Some(RESOURCE_INPUT_URI)
        }
        ControlMsg::Skip { id, .. } => {
            let mut s = state.write().await;
            let outcome = process_action_sync(&mut s, UserAction::Skip { id });
            if matches!(outcome, ActionOutcome::Ok) {
                bus.send(AppEvent::ApprovalResolved {
                    session_id: None,
                    id,
                    action: "skip".to_string(),
                });
            }
            emit_control_result(
                control_tx,
                "skip",
                matches!(outcome, ActionOutcome::Ok),
                format_outcome(outcome),
                None,
            );
            Some(RESOURCE_APPROVAL_URI)
        }
        ControlMsg::ApproveAll { id, .. } => {
            let mut s = state.write().await;
            let outcome = process_action_sync(&mut s, UserAction::ApproveAll { id });
            if matches!(outcome, ActionOutcome::Ok) {
                bus.send(AppEvent::ApprovalResolved {
                    session_id: None,
                    id,
                    action: "approve_all".to_string(),
                });
            }
            emit_control_result(
                control_tx,
                "approve_all",
                matches!(outcome, ActionOutcome::Ok),
                format_outcome(outcome),
                None,
            );
            Some(RESOURCE_APPROVAL_URI)
        }
        ControlMsg::SetAutonomy { level } => {
            let parsed = AutonomyLevel::from_str_loose(&level);
            // Shared state updated by ControlPlane
            emit_control_result(
                control_tx,
                "set_autonomy",
                true,
                format!("Autonomy set to {}", parsed),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::SetApprovalRule { category, rule } => {
            // Live shared-state update + intendant.toml persistence are
            // handled by the control plane; MCP only surfaces the ack.
            emit_control_result(
                control_tx,
                "set_approval_rule",
                true,
                format!("Approval rule {} set to {}", category, rule),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::SetExternalAgent { agent } => {
            let parsed = agent
                .as_deref()
                .filter(|s| !s.is_empty())
                .and_then(crate::external_agent::AgentBackend::from_str_loose);
            {
                let mut s = state.write().await;
                s.external_agent = parsed.clone();
            }
            let label = parsed
                .as_ref()
                .map(|b| b.to_string())
                .unwrap_or_else(|| "none".to_string());
            emit_control_result(
                control_tx,
                "set_external_agent",
                true,
                format!("External agent set to {}", label),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::SetCodexCommand { command } => {
            let label = command
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or("codex");
            emit_control_result(
                control_tx,
                "set_codex_command",
                true,
                format!("Codex command set to {} (applies on next task)", label),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::SetCodexSandbox { mode } => {
            // Shared state + persistence is handled by the control plane;
            // MCP only surfaces acknowledgement to the caller.
            emit_control_result(
                control_tx,
                "set_codex_sandbox",
                true,
                format!("Codex sandbox set to {} (applies on next task)", mode),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::SetCodexApprovalPolicy { policy } => {
            emit_control_result(
                control_tx,
                "set_codex_approval_policy",
                true,
                format!(
                    "Codex approval policy set to {} (applies on next task)",
                    policy
                ),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::SetCodexModel { model } => {
            let label = model
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or("<default>");
            emit_control_result(
                control_tx,
                "set_codex_model",
                true,
                format!("Codex model set to {} (applies on next task)", label),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::SetCodexReasoningEffort { effort } => {
            let label = effort
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or("<default>");
            emit_control_result(
                control_tx,
                "set_codex_reasoning_effort",
                true,
                format!(
                    "Codex reasoning effort set to {} (applies on next task)",
                    label
                ),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::SetCodexServiceTier { service_tier } => {
            let label = crate::project::normalize_codex_service_tier(service_tier.as_deref())
                .unwrap_or_else(|| "<inherit>".to_string());
            emit_control_result(
                control_tx,
                "set_codex_service_tier",
                true,
                format!("Codex service tier set to {} (applies on next task)", label),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::SetCodexWebSearch { enabled } => {
            emit_control_result(
                control_tx,
                "set_codex_web_search",
                true,
                format!(
                    "Codex web_search tool {} (applies on next task)",
                    if enabled { "enabled" } else { "disabled" }
                ),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::SetCodexNetworkAccess { enabled } => {
            emit_control_result(
                control_tx,
                "set_codex_network_access",
                true,
                format!(
                    "Codex workspace-write network {} (applies on next task)",
                    if enabled { "enabled" } else { "disabled" }
                ),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::SetCodexWritableRoots { roots } => {
            emit_control_result(
                control_tx,
                "set_codex_writable_roots",
                true,
                format!(
                    "Codex writable roots set to {} path(s) (applies on next task)",
                    roots.len()
                ),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::SetCodexManagedContext { mode } => {
            let normalized = crate::project::normalize_codex_managed_context(&mode);
            emit_control_result(
                control_tx,
                "set_codex_managed_context",
                true,
                format!(
                    "Codex managed context set to {} (applies on next task)",
                    normalized
                ),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::SetCodexContextArchive { mode } => {
            let normalized = crate::project::normalize_codex_context_archive(&mode);
            emit_control_result(
                control_tx,
                "set_codex_context_archive",
                true,
                format!(
                    "Codex context replay set to {} (applies on next task)",
                    normalized
                ),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::CodexThreadAction { op, .. } => {
            // The actual RPC round-trip happens on the daemon-side action
            // watcher. Acknowledge dispatch here; the result will surface
            // as a CodexThreadActionResult event on the MCP event stream.
            emit_control_result(
                control_tx,
                "codex_thread_action",
                true,
                format!("Codex thread action dispatched: /{}", op),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::RenameSession {
            session_id, name, ..
        } => {
            emit_control_result(
                control_tx,
                "rename_session",
                true,
                format!("Session rename requested: {} → {}", session_id, name),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::ConfigureSessionAgent { session_id, .. } => {
            emit_control_result(
                control_tx,
                "configure_session_agent",
                true,
                format!("Session launch config save requested: {}", session_id),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::SetGeminiModel { model } => {
            let label = model
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or("<default>");
            emit_control_result(
                control_tx,
                "set_gemini_model",
                true,
                format!("Gemini model set to {} (applies on next task)", label),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::SetGeminiApprovalMode { mode } => {
            emit_control_result(
                control_tx,
                "set_gemini_approval_mode",
                true,
                format!(
                    "Gemini approval mode set to {} (applies on next task)",
                    mode
                ),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::SetGeminiSandbox { enabled } => {
            emit_control_result(
                control_tx,
                "set_gemini_sandbox",
                true,
                format!(
                    "Gemini sandbox {} (applies on next task)",
                    if enabled { "enabled" } else { "disabled" }
                ),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::SetGeminiExtensions { extensions } => {
            emit_control_result(
                control_tx,
                "set_gemini_extensions",
                true,
                format!(
                    "Gemini extensions set to {} entry/entries (applies on next task)",
                    extensions.len()
                ),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::SetGeminiAllowedMcpServers { servers } => {
            emit_control_result(
                control_tx,
                "set_gemini_allowed_mcp_servers",
                true,
                format!(
                    "Gemini MCP allowlist set to {} entry/entries (applies on next task)",
                    servers.len()
                ),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::SetGeminiIncludeDirectories { directories } => {
            emit_control_result(
                control_tx,
                "set_gemini_include_directories",
                true,
                format!(
                    "Gemini include-directories set to {} path(s) (applies on next task)",
                    directories.len()
                ),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::SetGeminiDebug { enabled } => {
            emit_control_result(
                control_tx,
                "set_gemini_debug",
                true,
                format!(
                    "Gemini debug {} (applies on next task)",
                    if enabled { "enabled" } else { "disabled" }
                ),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::GeminiThreadAction { op, .. } => {
            emit_control_result(
                control_tx,
                "gemini_thread_action",
                true,
                format!("Gemini thread action dispatched: /{}", op),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::SetVerbosity { level } => {
            let parsed = match level.to_lowercase().as_str() {
                "quiet" => Some(Verbosity::Quiet),
                "normal" => Some(Verbosity::Normal),
                "verbose" => Some(Verbosity::Verbose),
                "debug" => Some(Verbosity::Debug),
                _ => None,
            };
            if let Some(v) = parsed {
                let mut s = state.write().await;
                s.verbosity = v;
                emit_control_result(
                    control_tx,
                    "set_verbosity",
                    true,
                    format!("Verbosity set to {}", v.label()),
                    None,
                );
            } else {
                emit_control_result(
                    control_tx,
                    "set_verbosity",
                    false,
                    format!("Unknown verbosity level: {}", level),
                    None,
                );
            }
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::ScheduleControllerRestart {
            controller_id,
            north_star_goal,
            reason,
            restart_after,
            restart_command,
            auto_start_task,
            max_attempts,
            cooldown_sec,
        } => {
            let mut params = ScheduleControllerRestartParams {
                controller_id,
                north_star_goal,
                reason,
                restart_after,
                restart_command,
                auto_start_task,
                max_attempts,
                cooldown_sec,
            };
            normalize_schedule_controller_restart_params(&mut params);

            if let Err(e) = validate_schedule_controller_restart_params(&params) {
                emit_control_result(control_tx, "schedule_controller_restart", false, e, None);
                return Some(RESOURCE_RESTART_URI);
            }

            let restart = {
                let mut s = state.write().await;
                if let Some(active) = s.controller_restart.as_ref() {
                    if matches!(
                        active.phase,
                        RestartPhase::AwaitingTurnComplete
                            | RestartPhase::Ready
                            | RestartPhase::Restarting
                    ) {
                        emit_control_result(
                            control_tx,
                            "schedule_controller_restart",
                            false,
                            format!(
                                "A restart is already active (id={}, phase={:?})",
                                active.restart_id, active.phase
                            ),
                            None,
                        );
                        return Some(RESOURCE_RESTART_URI);
                    }
                }

                let restart = ControllerRestartState::new(&params);
                s.push_log(
                    LogLevel::Info,
                    format!(
                        "Controller restart scheduled for '{}' (id={})",
                        restart.controller_id, restart.restart_id
                    ),
                );
                s.controller_restart = Some(restart.clone());
                persist_restart_state(&s.log_dir, &s.controller_restart);
                restart
            };

            let mut payload = serde_json::json!({
                "status": "scheduled",
                "restart_id": restart.restart_id,
                "turn_complete_token": restart.turn_complete_token,
            });
            let mut command_ok = true;
            let mut command_message = "ok".to_string();

            if matches!(restart.restart_after, RestartAfter::Now) {
                match run_scheduled_controller_restart_with_state(state, bus).await {
                    Ok(result) => {
                        payload["execution"] = serde_json::Value::String(if result.is_empty() {
                            "ok".to_string()
                        } else {
                            result
                        });
                    }
                    Err(e) => {
                        command_ok = false;
                        command_message = "restart execution failed".to_string();
                        payload["execution_error"] = serde_json::Value::String(e);
                    }
                }
            }
            let phase = {
                let s = state.read().await;
                s.controller_restart
                    .as_ref()
                    .map(restart_phase_value)
                    .unwrap_or_else(|| {
                        serde_json::to_value(restart.phase).unwrap_or(serde_json::Value::Null)
                    })
            };
            payload["phase"] = phase;

            emit_control_result(
                control_tx,
                "schedule_controller_restart",
                command_ok,
                command_message,
                Some(payload),
            );
            Some(RESOURCE_RESTART_URI)
        }
        ControlMsg::ControllerTurnComplete {
            restart_id,
            turn_complete_token,
            status,
            handoff_summary,
        } => {
            let mut params = ControllerTurnCompleteParams {
                restart_id,
                turn_complete_token,
                status,
                handoff_summary,
            };
            normalize_controller_turn_complete_params(&mut params);
            {
                let mut s = state.write().await;
                let log_dir = s.log_dir.clone();
                let Some(active) = s.controller_restart.as_mut() else {
                    let error = "No controller restart is scheduled".to_string();
                    let data = serde_json::json!({
                        "status": "rejected",
                        "ok": false,
                        "restart_id": params.restart_id,
                        "error": error,
                    });
                    emit_control_result(
                        control_tx,
                        "controller_turn_complete",
                        false,
                        error,
                        Some(data),
                    );
                    return Some(RESOURCE_RESTART_URI);
                };
                if active.restart_id != params.restart_id {
                    let error = "restart_id does not match the active restart".to_string();
                    let data = serde_json::json!({
                        "status": "rejected",
                        "ok": false,
                        "restart_id": params.restart_id,
                        "phase": active.phase,
                        "error": error,
                    });
                    emit_control_result(
                        control_tx,
                        "controller_turn_complete",
                        false,
                        error,
                        Some(data),
                    );
                    return Some(RESOURCE_RESTART_URI);
                }
                if active.turn_complete_token != params.turn_complete_token {
                    let error = "turn_complete_token is invalid".to_string();
                    let data = serde_json::json!({
                        "status": "rejected",
                        "ok": false,
                        "restart_id": params.restart_id,
                        "phase": active.phase,
                        "error": error,
                    });
                    emit_control_result(
                        control_tx,
                        "controller_turn_complete",
                        false,
                        error,
                        Some(data),
                    );
                    return Some(RESOURCE_RESTART_URI);
                }
                if !matches!(active.phase, RestartPhase::AwaitingTurnComplete) {
                    let error = format!(
                        "Restart is not awaiting completion (phase={:?})",
                        active.phase
                    );
                    let data = serde_json::json!({
                        "status": "rejected",
                        "ok": false,
                        "restart_id": params.restart_id,
                        "phase": active.phase,
                        "error": error,
                    });
                    emit_control_result(
                        control_tx,
                        "controller_turn_complete",
                        false,
                        error,
                        Some(data),
                    );
                    return Some(RESOURCE_RESTART_URI);
                }

                active.handoff_summary = params.handoff_summary.clone();
                active.completion_status = params.status.clone();
                active.phase = RestartPhase::Ready;
                active.updated_at = ControllerRestartState::now_string();
                let snapshot = s.controller_restart.clone();
                persist_restart_state(&log_dir, &snapshot);
            }

            match run_scheduled_controller_restart_with_state(state, bus).await {
                Ok(result) => {
                    let execution = if result.is_empty() {
                        "ok".to_string()
                    } else {
                        result
                    };
                    let phase = {
                        let s = state.read().await;
                        s.controller_restart
                            .as_ref()
                            .map(restart_phase_value)
                            .unwrap_or(serde_json::Value::Null)
                    };
                    let data = serde_json::json!({
                        "status": "completed",
                        "ok": true,
                        "restart_id": params.restart_id,
                        "execution": execution,
                        "phase": phase,
                    });
                    emit_control_result(
                        control_tx,
                        "controller_turn_complete",
                        true,
                        "ok".to_string(),
                        Some(data),
                    )
                }
                Err(e) => {
                    let phase = {
                        let s = state.read().await;
                        s.controller_restart
                            .as_ref()
                            .map(restart_phase_value)
                            .unwrap_or(serde_json::Value::Null)
                    };
                    let data = serde_json::json!({
                        "status": "restart_pending",
                        "ok": false,
                        "restart_id": params.restart_id,
                        "phase": phase,
                        "error": e,
                    });
                    emit_control_result(
                        control_tx,
                        "controller_turn_complete",
                        false,
                        "restart execution failed".to_string(),
                        Some(data),
                    )
                }
            }
            Some(RESOURCE_RESTART_URI)
        }
        ControlMsg::GetRestartStatus => {
            let s = state.read().await;
            let data = Some(restart_state_public_value(s.controller_restart.as_ref()));
            emit_control_result(
                control_tx,
                "get_restart_status",
                true,
                "ok".to_string(),
                data,
            );
            Some(RESOURCE_RESTART_URI)
        }
        ControlMsg::CancelControllerRestart { restart_id } => {
            let mut params = CancelControllerRestartParams { restart_id };
            normalize_cancel_controller_restart_params(&mut params);
            let mut s = state.write().await;
            let log_dir = s.log_dir.clone();
            let Some(active) = s.controller_restart.as_mut() else {
                let error = "No controller restart is scheduled".to_string();
                let mut data = serde_json::json!({
                    "status": "rejected",
                    "ok": false,
                    "error": error,
                });
                if let Some(restart_id) = params.restart_id {
                    data["restart_id"] = serde_json::Value::String(restart_id);
                }
                emit_control_result(
                    control_tx,
                    "cancel_controller_restart",
                    false,
                    error,
                    Some(data),
                );
                return Some(RESOURCE_RESTART_URI);
            };

            if let Some(expected_id) = params.restart_id.as_deref() {
                if expected_id != active.restart_id {
                    let error = format!(
                        "restart_id '{}' does not match active '{}'",
                        expected_id, active.restart_id
                    );
                    let data = serde_json::json!({
                        "status": "rejected",
                        "ok": false,
                        "restart_id": active.restart_id.clone(),
                        "phase": active.phase,
                        "error": error,
                    });
                    emit_control_result(
                        control_tx,
                        "cancel_controller_restart",
                        false,
                        error,
                        Some(data),
                    );
                    return Some(RESOURCE_RESTART_URI);
                }
            }

            active.phase = RestartPhase::Cancelled;
            active.updated_at = ControllerRestartState::now_string();
            active.last_result = Some("Cancelled by operator".to_string());
            let restart_id = active.restart_id.clone();
            let phase = active.phase;
            let snapshot = s.controller_restart.clone();
            persist_restart_state(&log_dir, &snapshot);
            let data = serde_json::json!({
                "status": "cancelled",
                "ok": true,
                "restart_id": restart_id,
                "phase": phase,
            });
            emit_control_result(
                control_tx,
                "cancel_controller_restart",
                true,
                "ok".to_string(),
                Some(data),
            );
            Some(RESOURCE_RESTART_URI)
        }
        ControlMsg::RequestControllerLoopHalt { persistent } => {
            let loop_dir = controller_loop_dir();
            let persistent = persistent.unwrap_or(true);
            match request_loop_halt_marker(&loop_dir, persistent) {
                Ok(()) => {
                    let data = collect_controller_loop_status_with_state(&loop_dir, state).await;
                    emit_control_result(
                        control_tx,
                        "request_controller_loop_halt",
                        true,
                        if persistent {
                            "persistent halt requested".to_string()
                        } else {
                            "halt-after-cycle requested".to_string()
                        },
                        Some(data),
                    );
                }
                Err(e) => {
                    emit_control_result(control_tx, "request_controller_loop_halt", false, e, None);
                }
            }
            Some(RESOURCE_LOOP_URI)
        }
        ControlMsg::ClearControllerLoopHalt => {
            let loop_dir = controller_loop_dir();
            match clear_loop_halt_markers(&loop_dir) {
                Ok(()) => {
                    let data = collect_controller_loop_status_with_state(&loop_dir, state).await;
                    emit_control_result(
                        control_tx,
                        "clear_controller_loop_halt",
                        true,
                        "halt flags cleared".to_string(),
                        Some(data),
                    );
                }
                Err(e) => {
                    emit_control_result(control_tx, "clear_controller_loop_halt", false, e, None);
                }
            }
            Some(RESOURCE_LOOP_URI)
        }
        ControlMsg::InterveneControllerLoop { mode } => {
            let loop_dir = controller_loop_dir();
            match request_loop_intervention_marker(&loop_dir, &mode) {
                Ok(intervention) => {
                    let mut data =
                        collect_controller_loop_status_with_state(&loop_dir, state).await;
                    add_controller_loop_intervention_report(&mut data, &intervention);
                    emit_control_result(
                        control_tx,
                        "intervene_controller_loop",
                        true,
                        format!("{} requested", intervention.mode.as_str()),
                        Some(data),
                    );
                }
                Err(e) => {
                    emit_control_result(control_tx, "intervene_controller_loop", false, e, None);
                }
            }
            Some(RESOURCE_LOOP_URI)
        }
        ControlMsg::GetControllerLoopStatus => {
            let loop_dir = controller_loop_dir();
            let data = collect_controller_loop_status_with_state(&loop_dir, state).await;
            emit_control_result(
                control_tx,
                "get_controller_loop_status",
                true,
                "ok".to_string(),
                Some(data),
            );
            Some(RESOURCE_LOOP_URI)
        }
        ControlMsg::StartTask {
            task, orchestrate, ..
        } => {
            match start_task_with_state(state, bus, task, "voice", orchestrate).await {
                Ok(()) => {
                    emit_control_result(control_tx, "start_task", true, "ok".to_string(), None);
                }
                Err(e) => {
                    emit_control_result(control_tx, "start_task", false, e, None);
                }
            }
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::CreateSession {
            task, orchestrate, ..
        } => {
            match start_task_with_state(state, bus, task, "mcp", orchestrate).await {
                Ok(()) => {
                    emit_control_result(control_tx, "create_session", true, "ok".to_string(), None);
                }
                Err(e) => {
                    emit_control_result(control_tx, "create_session", false, e, None);
                }
            }
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::ResumeSession {
            source,
            session_id,
            task,
            ..
        } => {
            let action = if task
                .as_ref()
                .map(|t| t.trim())
                .filter(|t| !t.is_empty())
                .is_some()
            {
                "resume dispatched"
            } else {
                "session attach requested"
            };
            emit_control_result(
                control_tx,
                "resume_session",
                true,
                format!("{}: {} {}", action, source, session_id),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::StopSession { session_id } => {
            emit_control_result(
                control_tx,
                "stop_session",
                true,
                format!("Stop session requested: {}", session_id),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::RestartSession {
            source, session_id, ..
        } => {
            emit_control_result(
                control_tx,
                "restart_session",
                true,
                format!("Restart session requested: {} {}", source, session_id),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::FollowUp {
            text, direct: _, ..
        } => {
            // MCP has a single follow-up channel and no presence layer,
            // so the `direct` bit is a no-op here — follow-ups already
            // go straight to the agent loop in this mode.
            let mut s = state.write().await;
            if s.phase != Phase::WaitingFollowUp && s.phase != Phase::Done {
                emit_control_result(
                    control_tx,
                    "follow_up",
                    false,
                    format!(
                        "Not waiting for follow-up (phase: {})",
                        phase_to_str(&s.phase)
                    ),
                    None,
                );
                return Some(RESOURCE_STATUS_URI);
            }
            if let Some(ref tx) = s.follow_up_tx {
                let tx = tx.clone();
                s.set_phase(Phase::Thinking);
                s.push_log(LogLevel::Info, format!("Follow-up via socket: {}", text));
                drop(s);
                if tx.send(FollowUpMessage::text(text)).await.is_err() {
                    emit_control_result(
                        control_tx,
                        "follow_up",
                        false,
                        "follow-up channel closed".to_string(),
                        None,
                    );
                } else {
                    emit_control_result(control_tx, "follow_up", true, "ok".to_string(), None);
                }
            } else {
                emit_control_result(
                    control_tx,
                    "follow_up",
                    false,
                    "no follow-up channel available".to_string(),
                    None,
                );
            }
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::EditUserMessage {
            session_id,
            source,
            resume_id,
            project_root,
            direct,
            user_turn_index,
            user_turn_revision,
            original_text,
            text,
            attachments,
        } => {
            bus.send(AppEvent::ControlCommand(ControlMsg::EditUserMessage {
                session_id,
                source,
                resume_id,
                project_root,
                direct,
                user_turn_index,
                user_turn_revision,
                original_text,
                text,
                attachments,
            }));
            emit_control_result(
                control_tx,
                "edit_user_message",
                true,
                "edit requested".to_string(),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::QueryDetail { scope, target } => {
            // Log query detail requests; full handling via presence layer
            let msg = format!("query_detail: scope={}, target={:?}", scope, target);
            emit_control_result(control_tx, "query_detail", true, msg, None);
            None
        }
        ControlMsg::RecallMemory {
            keywords,
            tags,
            channel,
        } => {
            let msg = format!(
                "recall_memory: keywords={:?}, tags={:?}, channel={:?}",
                keywords, tags, channel
            );
            emit_control_result(control_tx, "recall_memory", true, msg, None);
            None
        }
        ControlMsg::TakeDisplay { display_id } => {
            bus.send(AppEvent::DisplayTaken { display_id });
            emit_control_result(
                control_tx,
                "take_display",
                true,
                format!("Took control of :{}", display_id),
                None,
            );
            Some(RESOURCE_LOGS_URI)
        }
        ControlMsg::ReleaseDisplay { display_id, note } => {
            bus.send(AppEvent::DisplayReleased {
                display_id,
                note: note.clone(),
            });
            emit_control_result(
                control_tx,
                "release_display",
                true,
                format!("Released control of :{}", display_id),
                None,
            );
            Some(RESOURCE_LOGS_URI)
        }
        ControlMsg::GrantUserDisplay { display_id } => {
            let did = display_id.unwrap_or(0);
            {
                let s = state.read().await;
                let autonomy = s.autonomy.clone();
                drop(s);
                let mut a = autonomy.write().await;
                a.user_display_granted = true;
            }
            std::env::set_var("INTENDANT_USER_DISPLAY_GRANTED", "1");
            bus.send(AppEvent::UserDisplayGranted { display_id: did });
            emit_control_result(
                control_tx,
                "grant_user_display",
                true,
                format!("User display access granted (display_id: {})", did),
                None,
            );
            Some(RESOURCE_LOGS_URI)
        }
        ControlMsg::ListDisplays => {
            let session_registry = state.read().await.session_registry.clone();
            let displays =
                crate::display::enumerate_displays_with_sessions(&session_registry).await;
            let json = serde_json::to_string_pretty(&displays).unwrap_or_else(|_| "[]".to_string());
            emit_control_result(control_tx, "list_displays", true, json, None);
            None
        }
        ControlMsg::RevokeUserDisplay { display_id, note } => {
            let did = display_id.unwrap_or(0);
            {
                let s = state.read().await;
                let autonomy = s.autonomy.clone();
                drop(s);
                let mut a = autonomy.write().await;
                a.user_display_granted = false;
            }
            std::env::remove_var("INTENDANT_USER_DISPLAY_GRANTED");
            bus.send(AppEvent::UserDisplayRevoked {
                display_id: did,
                note: note.clone(),
            });
            emit_control_result(
                control_tx,
                "revoke_user_display",
                true,
                format!("User display access revoked (display_id: {})", did),
                None,
            );
            Some(RESOURCE_LOGS_URI)
        }
        ControlMsg::InvokeSkill {
            skill_name,
            arguments,
        } => {
            // In MCP mode, convert skill invocation to a StartTask
            let discovered = crate::skills::discover_skills(None);
            let args = arguments.as_deref().unwrap_or("");
            match crate::skills::resolve_skill_as_task(&discovered, &skill_name, args) {
                Ok(task_text) => {
                    bus.send(AppEvent::ControlCommand(ControlMsg::StartTask {
                        session_id: None,
                        task: task_text,
                        orchestrate: Some(false),
                        direct: None,
                        reference_frame_ids: vec![],
                        display_target: None,
                        attachments: vec![],
                        follow_up_id: None,
                    }));
                    emit_control_result(
                        control_tx,
                        "invoke_skill",
                        true,
                        format!("Skill '{}' dispatched", skill_name),
                        None,
                    );
                }
                Err(e) => {
                    emit_control_result(control_tx, "invoke_skill", false, e, None);
                }
            }
            None
        }
        ControlMsg::Quit => {
            let action = UserAction::Quit;
            let mut s = state.write().await;
            let outcome = process_action_sync(&mut s, action);
            emit_control_result(
                control_tx,
                "quit",
                matches!(outcome, ActionOutcome::Ok),
                format_outcome(outcome),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        // Debug screen commands handled by dedicated handler task
        ControlMsg::SetupDebugScreen
        | ControlMsg::TeardownDebugScreen
        | ControlMsg::StartDebugRecording
        | ControlMsg::StopDebugRecording => {
            emit_control_result(
                control_tx,
                "debug_screen",
                true,
                "Dispatched".to_string(),
                None,
            );
            None
        }
        ControlMsg::StartRecording { ref stream_name } => {
            emit_control_result(
                control_tx,
                "start_recording",
                true,
                format!("Starting {}", stream_name),
                None,
            );
            None
        }
        ControlMsg::StopRecording { ref stream_name } => {
            emit_control_result(
                control_tx,
                "stop_recording",
                true,
                format!("Stopping {}", stream_name),
                None,
            );
            None
        }
        ControlMsg::DeleteRecording { ref stream_name } => {
            emit_control_result(
                control_tx,
                "delete_recording",
                true,
                format!("Deleting {}", stream_name),
                None,
            );
            None
        }
        ControlMsg::Interrupt {
            session_id,
            expected_turn: _,
        } => {
            // Re-broadcast as an AppEvent so the dispatcher / agent loops pick it up.
            bus.send(AppEvent::InterruptRequested { session_id });
            emit_control_result(
                control_tx,
                "interrupt",
                true,
                "Interrupt requested".to_string(),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::Steer {
            session_id,
            text,
            id,
            attachments: _,
        } => {
            // Mid-turn steering from an MCP client. Re-broadcast as an
            // `AppEvent::SteerRequested` so the running agent loop (if any)
            // decides whether to call `steer_turn` or fall back to queuing.
            bus.send(AppEvent::SteerRequested {
                session_id,
                text,
                id: id.unwrap_or_default(),
            });
            emit_control_result(
                control_tx,
                "steer",
                true,
                "Steer requested".to_string(),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::CancelSteer {
            session_id,
            id,
            reason,
        } => {
            bus.send(AppEvent::SteerCancelRequested {
                session_id,
                id,
                reason: reason.unwrap_or_else(|| "cleared by user".to_string()),
            });
            emit_control_result(
                control_tx,
                "cancel_steer",
                true,
                "Steer cancellation requested".to_string(),
                None,
            );
            Some(RESOURCE_STATUS_URI)
        }
        ControlMsg::WebRtcSignal { .. } => {
            // Federation-driven WebRTC signaling — handled by the
            // web gateway's per-peer WS dispatcher, not the MCP
            // control surface. MCP clients don't drive display
            // streams; this variant is a no-op here.
            None
        }
        ControlMsg::RequestDisplayInputAuthority { .. }
        | ControlMsg::ReleaseDisplayInputAuthority { .. } => {
            // Per-display input authority is a WebSocket-connection-
            // scoped concept (the gate uses the connection's identity
            // to allow/deny display_input messages). MCP doesn't have
            // a per-client connection identity in the same sense, so
            // there's no coherent way to grant authority to an MCP
            // caller here. Ignored.
            None
        }
        ControlMsg::CreateBrowserWorkspace { .. }
        | ControlMsg::CloseBrowserWorkspace { .. }
        | ControlMsg::AcquireBrowserWorkspace { .. }
        | ControlMsg::ReleaseBrowserWorkspace { .. } => {
            // Browser workspace commands are handled by the control plane and
            // by dedicated MCP tools. Replaying ControlCommand events here
            // would duplicate launch/lease side effects.
            None
        }
        ControlMsg::SetDiagnosticsVisualMarker { .. } => {
            // Phase 0 visual-freshness diagnostic toggle (task #83).
            // Handled inline by the web gateway's `/ws` dispatcher,
            // which has direct access to the per-display
            // `session_registry` to flip the matching DisplaySession's
            // diagnostic flag. MCP doesn't drive display sessions and
            // has no path to the registry from this dispatcher; no-op.
            None
        }
    }
}

pub type SharedMcpState = Arc<RwLock<McpAppState>>;

fn phase_to_str(phase: &Phase) -> &'static str {
    match phase {
        Phase::Thinking => "thinking",
        Phase::RunningAgent => "running_agent",
        Phase::Orchestrating => "orchestrating",
        Phase::WaitingApproval => "waiting_approval",
        Phase::WaitingHuman => "waiting_human",
        Phase::WaitingFollowUp => "waiting_follow_up",
        Phase::Idle => "idle",
        Phase::Done => "done",
        Phase::Interrupting => "interrupting",
        Phase::Interrupted => "interrupted",
    }
}

fn phase_from_status_str(phase: &str) -> Phase {
    match phase.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "thinking" => Phase::Thinking,
        "running" | "running_agent" => Phase::RunningAgent,
        "orchestrating" => Phase::Orchestrating,
        "waiting_approval" => Phase::WaitingApproval,
        "waiting_human" => Phase::WaitingHuman,
        "waiting_follow_up" | "waiting_followup" => Phase::WaitingFollowUp,
        "done" | "completed" => Phase::Done,
        "interrupting" => Phase::Interrupting,
        "interrupted" => Phase::Interrupted,
        _ => Phase::Idle,
    }
}

fn status_task_is_external_turn_progress(task: &str) -> bool {
    let normalized = task.trim().to_ascii_lowercase();
    (normalized.contains("turn") || normalized.contains("round"))
        && normalized.contains("in progress")
}

fn verbosity_to_str(v: Verbosity) -> &'static str {
    match v {
        Verbosity::Quiet => "quiet",
        Verbosity::Normal => "normal",
        Verbosity::Verbose => "verbose",
        Verbosity::Debug => "debug",
    }
}

fn parse_verbosity(s: &str) -> Option<Verbosity> {
    match s.to_lowercase().as_str() {
        "quiet" => Some(Verbosity::Quiet),
        "normal" => Some(Verbosity::Normal),
        "verbose" => Some(Verbosity::Verbose),
        "debug" => Some(Verbosity::Debug),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Event listener: consumes AppEvents and updates shared state
// ---------------------------------------------------------------------------

/// Spawn a background task that consumes AppEvents and mirrors them into
/// [`McpAppState`], exactly as the TUI's `handle_event` does.
///
/// Returns a handle for cleanup.
pub fn spawn_event_listener(
    state: SharedMcpState,
    mut event_rx: tokio::sync::broadcast::Receiver<AppEvent>,
    peer: Arc<Mutex<Option<rmcp::Peer<RoleServer>>>>,
    bus: EventBus,
    human_question_path: Option<crate::event::SharedQuestionPath>,
    control_tx: Option<broadcast::Sender<String>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let event = match event_rx.recv().await {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            let mut resource_changed: Option<&str> = None;
            let mut deferred_control_msg: Option<ControlMsg> = None;

            {
                let mut s = state.write().await;
                // Exhaustive match — no wildcard. Adding a new AppEvent variant
                // will cause a compile error here, enforcing parity.
                match event {
                    AppEvent::Key(_) => {} // MCP doesn't handle key events
                    AppEvent::Resize(_, _) => {}
                    AppEvent::LogEntry { .. }
                    | AppEvent::UserMessageRewind { .. }
                    | AppEvent::UserMessageLog { .. }
                    | AppEvent::ExternalAgentChanged { .. }
                    | AppEvent::AutonomyChanged { .. }
                    | AppEvent::CodexThreadActionRequested { .. }
                    | AppEvent::ExternalFollowUpRequested { .. }
                    | AppEvent::SessionStopRequested { .. }
                    | AppEvent::SessionRelationship { .. }
                    | AppEvent::SessionGoal { .. }
                    | AppEvent::SessionRenameResult { .. }
                    | AppEvent::SessionAgentConfigResult { .. }
                    | AppEvent::GeminiConfigChanged { .. }
                    | AppEvent::GeminiThreadActionRequested { .. }
                    | AppEvent::GeminiThreadActionResult { .. }
                    | AppEvent::SharedView { .. }
                    | AppEvent::BrowserWorkspaceChanged { .. } => {} // Derived events — handled by outbound broadcaster
                    AppEvent::CodexConfigChanged {
                        managed_context, ..
                    } => {
                        if let Some(mode) = managed_context {
                            s.configured_codex_managed_context =
                                crate::project::codex_managed_context_enabled(&mode);
                            if !s.is_active_codex_session() {
                                s.codex_managed_context = s.configured_codex_managed_context;
                            }
                            resource_changed = Some("intendant://status");
                        }
                    }
                    AppEvent::SessionCapabilities {
                        ref session_id,
                        ref capabilities,
                    } => {
                        if apply_session_capabilities_to_mcp_state(&mut s, session_id, capabilities)
                        {
                            resource_changed = Some("intendant://status");
                        }
                    }
                    AppEvent::ContextSnapshot {
                        ref session_id,
                        ref source,
                        ref format,
                        token_count,
                        ref token_count_kind,
                        context_window,
                        hard_context_window,
                        ref raw,
                        ..
                    } => {
                        if apply_context_snapshot_usage_to_mcp_state(
                            &mut s,
                            session_id.as_deref(),
                            source,
                            format,
                            token_count,
                            token_count_kind.as_deref(),
                            context_window,
                            hard_context_window,
                            raw,
                        ) {
                            resource_changed = Some("intendant://status");
                        }
                    }
                    AppEvent::SessionIdentity {
                        ref session_id,
                        ref source,
                        ref backend_session_id,
                    } => {
                        s.link_session_aliases(session_id, backend_session_id);
                        if !session_id.is_empty() {
                            s.session_sources.insert(session_id.clone(), source.clone());
                        }
                        if !backend_session_id.is_empty() {
                            s.session_sources
                                .insert(backend_session_id.clone(), source.clone());
                        }
                        if source.eq_ignore_ascii_case("codex") {
                            if let Some(enabled) =
                                s.session_codex_managed_context.get(session_id).copied()
                            {
                                s.session_codex_managed_context
                                    .insert(backend_session_id.clone(), enabled);
                            } else if let Some(enabled) = s
                                .session_codex_managed_context
                                .get(backend_session_id)
                                .copied()
                            {
                                s.session_codex_managed_context
                                    .insert(session_id.clone(), enabled);
                            }
                        }
                        if s.session_id.is_empty()
                            || s.session_id == session_id.as_str()
                            || s.session_id == backend_session_id.as_str()
                        {
                            s.active_session_source = Some(source.clone());
                        }
                        resource_changed = Some("intendant://status");
                    }
                    AppEvent::CodexThreadActionResult {
                        session_id,
                        action,
                        success,
                        message,
                    } => {
                        if action == "rewind_context" {
                            s.note_context_rewind_result_for(
                                session_id.as_deref(),
                                success,
                                &message,
                            );
                            resource_changed = Some("intendant://status");
                        }
                    }
                    AppEvent::UsageSnapshot {
                        session_id,
                        main,
                        presence,
                    } => {
                        let main =
                            s.normalize_main_usage_snapshot(session_id.as_deref(), main.clone());
                        s.record_session_usage_snapshot(session_id.as_deref(), main.clone());
                        let applies_to_current_session =
                            s.session_id_applies_to_current_session(session_id.as_deref());
                        if applies_to_current_session {
                            s.apply_main_usage_snapshot(main);
                            if let Some(presence) = presence {
                                s.presence_provider_name = Some(presence.provider);
                                s.presence_model_name = Some(presence.model);
                                s.presence_tokens = presence.tokens_used;
                                s.presence_context_window = presence.context_window;
                                s.presence_usage_pct = presence.usage_pct;
                            }
                        }
                        s.complete_pending_rewind_pressure_check_for(session_id.as_deref());
                        resource_changed = Some("intendant://status");
                    }
                    AppEvent::Tick => {
                        // Detect stuck phases — warn every 30s after 120s
                        if matches!(
                            s.phase,
                            Phase::Thinking | Phase::RunningAgent | Phase::Orchestrating
                        ) {
                            let elapsed = s.phase_entered_at.elapsed().as_secs();
                            if elapsed >= 120 && elapsed % 30 == 0 {
                                let phase_name = phase_to_str(&s.phase).to_string();
                                s.push_log(
                                    LogLevel::Warn,
                                    format!(
                                        "Phase '{}' active for {}s (possible stuck state)",
                                        phase_name, elapsed
                                    ),
                                );
                                resource_changed = Some("intendant://logs");
                            }
                        }
                    }
                    AppEvent::StatusUpdate {
                        turn,
                        ref phase,
                        ref session_id,
                        ref task,
                        ..
                    } => {
                        s.note_session_phase(
                            Some(session_id),
                            Some(turn),
                            phase_from_status_str(phase),
                            Some(task),
                        );
                        if status_task_is_external_turn_progress(task) {
                            s.note_session_round(Some(session_id), turn);
                        }
                        resource_changed = Some("intendant://status");
                    }
                    AppEvent::Quit => {
                        s.should_quit = true;
                        break;
                    }

                    AppEvent::TurnStarted {
                        turn,
                        budget_pct,
                        ref session_id,
                        remaining: _,
                    } => {
                        s.turn = turn;
                        s.budget_pct = budget_pct;
                        s.set_phase(Phase::Thinking);
                        s.note_session_phase(
                            session_id.as_deref(),
                            Some(turn),
                            Phase::Thinking,
                            None,
                        );
                        s.push_log(
                            LogLevel::Detail,
                            format!("Turn {} started (budget: {:.1}%)", turn, budget_pct),
                        );
                        resource_changed = Some("intendant://status");
                    }

                    AppEvent::ModelResponse {
                        turn,
                        content,
                        usage,
                        reasoning,
                        ..
                    } => {
                        s.session_tokens += usage.total_tokens;
                        let preview = if content.len() > 500 {
                            format!("{}...", &content[..500])
                        } else {
                            content
                        };
                        s.push_log(LogLevel::Model, format!("[T{}] {}", turn, preview));
                        if let Some(r) = reasoning {
                            s.push_log(
                                LogLevel::Debug,
                                format!("[T{}] reasoning: {}...", turn, &r[..r.len().min(100)]),
                            );
                        }
                        resource_changed = Some("intendant://logs");
                    }

                    AppEvent::ModelResponseDelta { .. } => {
                        // Streaming deltas: MCP doesn't need to handle incremental text
                    }

                    AppEvent::JsonExtracted { preview } => {
                        s.push_log(LogLevel::Debug, format!("JSON: {}", preview));
                    }

                    AppEvent::DoneSignal {
                        ref session_id,
                        message,
                    } => {
                        s.set_phase(Phase::Done);
                        s.note_session_phase(session_id.as_deref(), None, Phase::Done, None);
                        s.push_log(
                            LogLevel::Info,
                            format!("Done: {}", message.as_deref().unwrap_or("task complete")),
                        );
                        resource_changed = Some("intendant://status");
                    }

                    AppEvent::AgentStarted {
                        turn,
                        commands_preview,
                        ref session_id,
                        ..
                    } => {
                        s.set_phase(Phase::RunningAgent);
                        s.note_session_phase(
                            session_id.as_deref(),
                            Some(turn),
                            Phase::RunningAgent,
                            None,
                        );
                        s.push_log(LogLevel::Agent, format!("[T{}] {}", turn, commands_preview));
                        resource_changed = Some("intendant://status");
                    }

                    AppEvent::AgentOutput {
                        ref session_id,
                        stdout,
                        stderr,
                        ..
                    } => {
                        s.note_session_phase(
                            session_id.as_deref(),
                            None,
                            Phase::RunningAgent,
                            None,
                        );
                        let formatted =
                            crate::tui::app::format_agent_output_for_tui(&stdout, &stderr);
                        if !formatted.is_empty() {
                            let level = if !stderr.is_empty() {
                                LogLevel::Warn
                            } else {
                                LogLevel::Agent
                            };
                            s.push_log(level, formatted);
                        }
                        resource_changed = Some("intendant://logs");
                    }

                    AppEvent::SubAgentResult { formatted } => {
                        s.push_log(LogLevel::SubAgent, formatted);
                        resource_changed = Some("intendant://logs");
                    }

                    AppEvent::OrchestratorProgress {
                        turn,
                        status,
                        last_action,
                    } => {
                        s.set_phase(Phase::Orchestrating);
                        s.push_log(
                            LogLevel::SubAgent,
                            format!("[T{}] {} — {}", turn, status, last_action),
                        );
                        resource_changed = Some("intendant://status");
                    }

                    AppEvent::OrchestratorLog { message, level } => {
                        s.push_log(level, message);
                        resource_changed = Some("intendant://logs");
                    }

                    AppEvent::ContextManagement { turn } => {
                        s.push_log(LogLevel::Detail, format!("[T{}] Context management", turn));
                    }

                    AppEvent::TaskComplete {
                        ref session_id,
                        reason,
                        ..
                    } => {
                        s.set_phase(Phase::Done);
                        s.note_session_phase(session_id.as_deref(), None, Phase::Done, None);
                        s.push_log(LogLevel::Info, format!("Task complete: {}", reason));
                        resource_changed = Some("intendant://status");
                    }

                    AppEvent::BudgetWarning { pct, remaining } => {
                        s.budget_pct = pct;
                        s.push_log(
                            LogLevel::Warn,
                            format!(
                                "Budget warning: {:.1}% used ({} tokens remaining)",
                                pct, remaining
                            ),
                        );
                        resource_changed = Some("intendant://status");
                    }

                    AppEvent::BudgetExhausted { remaining } => {
                        s.budget_pct = 100.0;
                        s.push_log(
                            LogLevel::Error,
                            format!("Budget exhausted ({} tokens remaining)", remaining),
                        );
                        resource_changed = Some("intendant://status");
                    }

                    AppEvent::SafetyCapReached => {
                        s.set_phase(Phase::Done);
                        s.push_log(
                            LogLevel::Error,
                            "Safety cap reached (500 turns)".to_string(),
                        );
                        resource_changed = Some("intendant://status");
                    }

                    AppEvent::LoopError(msg) => {
                        s.set_phase(Phase::Done);
                        s.push_log(LogLevel::Error, format!("Error: {}", msg));
                        resource_changed = Some("intendant://status");
                    }

                    AppEvent::HumanQuestionDetected { question } => {
                        s.set_phase(Phase::WaitingHuman);
                        s.human_question = Some(question.clone());
                        s.push_log(LogLevel::Info, format!("Human question: {}", question));
                        resource_changed = Some("intendant://pending-input");
                    }

                    AppEvent::HumanResponseSent => {
                        s.human_question = None;
                        s.set_phase(Phase::RunningAgent);
                        s.push_log(LogLevel::Detail, "Human response sent".to_string());
                        resource_changed = Some("intendant://pending-input");
                    }

                    AppEvent::ApprovalRequired {
                        ref session_id,
                        id,
                        command_preview,
                        category,
                    } => {
                        s.set_phase(Phase::WaitingApproval);
                        s.note_session_phase(
                            session_id.as_deref(),
                            None,
                            Phase::WaitingApproval,
                            None,
                        );
                        s.push_log(
                            LogLevel::Info,
                            format!("Approval required [{}]: {}", category, command_preview),
                        );
                        s.pending_approval = Some(PendingApprovalState {
                            id,
                            command_preview,
                            category: category.to_string(),
                        });
                        resource_changed = Some("intendant://pending-approval");
                    }

                    AppEvent::DisplayReady { display_id, .. } => {
                        s.push_log(LogLevel::Detail, format!("Display :{}", display_id));
                        resource_changed = Some("intendant://logs");
                    }

                    AppEvent::DisplayResize {
                        display_id,
                        width,
                        height,
                    } => {
                        s.push_log(
                            LogLevel::Detail,
                            format!("Display :{} resized to {}x{}", display_id, width, height),
                        );
                        resource_changed = Some("intendant://logs");
                    }

                    AppEvent::DisplayTaken { display_id } => {
                        s.push_log(
                            LogLevel::Warn,
                            format!("User took control of display :{}", display_id),
                        );
                        resource_changed = Some("intendant://logs");
                    }

                    AppEvent::DisplayReleased {
                        display_id,
                        ref note,
                    } => {
                        let msg = format!(
                            "User released control of display :{}{}",
                            display_id,
                            note.as_ref()
                                .map(|n| format!(". Note: {}", n))
                                .unwrap_or_default()
                        );
                        s.push_log(LogLevel::Info, msg);
                        resource_changed = Some("intendant://logs");
                    }

                    AppEvent::UserDisplayGranted { display_id } => {
                        s.push_log(
                            LogLevel::Warn,
                            format!("User display access granted (display_id: {})", display_id),
                        );
                        resource_changed = Some("intendant://logs");
                    }

                    AppEvent::UserDisplayRevoked {
                        display_id,
                        ref note,
                    } => {
                        let msg = format!(
                            "User display access revoked (display_id: {}){}",
                            display_id,
                            note.as_ref()
                                .map(|n| format!(". Note: {}", n))
                                .unwrap_or_default()
                        );
                        s.push_log(LogLevel::Info, msg);
                        resource_changed = Some("intendant://logs");
                    }

                    AppEvent::SessionDirChanged { ref path } => {
                        s.log_dir = path.clone();
                        persist_restart_state(&s.log_dir, &s.controller_restart);
                        // Update the human question monitor's watched path
                        if let Some(ref hqp) = human_question_path {
                            if let Ok(mut p) = hqp.try_write() {
                                *p = path.join("human_question");
                            }
                        }
                    }

                    AppEvent::AutoApproved { ref preview } => {
                        s.push_log(LogLevel::Detail, format!("auto-approved: {}", preview));
                        resource_changed = Some("intendant://logs");
                    }

                    AppEvent::ApprovalResolved { id, ref action, .. } => {
                        s.pending_approval = None;
                        if action == "deny" {
                            s.set_phase(Phase::Done);
                        } else {
                            s.set_phase(Phase::RunningAgent);
                        }
                        s.push_log(LogLevel::Info, format!("Approval {} (turn {})", action, id));
                        resource_changed = Some(RESOURCE_APPROVAL_URI);
                    }

                    AppEvent::RoundComplete {
                        ref session_id,
                        round,
                        turns_in_round,
                        ..
                    } => {
                        s.round = round;
                        s.set_phase(Phase::WaitingFollowUp);
                        s.note_session_round(session_id.as_deref(), round);
                        s.note_session_phase(
                            session_id.as_deref(),
                            Some(round),
                            Phase::WaitingFollowUp,
                            None,
                        );
                        s.push_log(
                            LogLevel::Info,
                            format!(
                                "Round {} complete ({} turns). Awaiting follow-up.",
                                round, turns_in_round
                            ),
                        );
                        resource_changed = Some("intendant://status");
                    }

                    AppEvent::ControlCommand(msg) => deferred_control_msg = Some(msg),
                    AppEvent::PresenceUsageUpdate {
                        total_tokens,
                        context_window,
                        usage_pct,
                        provider,
                        model,
                        ..
                    } => {
                        s.presence_tokens = total_tokens;
                        s.presence_context_window = context_window;
                        s.presence_usage_pct = usage_pct;
                        if s.presence_provider_name.is_none() {
                            s.presence_provider_name = Some(provider);
                            s.presence_model_name = Some(model);
                        }
                    }
                    AppEvent::PresenceLog { message, level, .. } => {
                        s.push_log(
                            level.unwrap_or(LogLevel::Info),
                            format!("[presence] {}", message),
                        );
                    }
                    AppEvent::PresenceReady => {
                        if !matches!(s.phase, Phase::WaitingApproval) {
                            s.set_phase(Phase::WaitingFollowUp);
                        }
                    }
                    AppEvent::PresenceConnected { .. } => {
                        s.push_log(
                            LogLevel::Detail,
                            "Browser presence connected — server presence paused".to_string(),
                        );
                    }
                    AppEvent::PresenceDisconnected => {
                        s.push_log(
                            LogLevel::Detail,
                            "Browser presence disconnected — server presence resumed".to_string(),
                        );
                    }
                    AppEvent::VoiceLog { ref text, seq, .. } => {
                        s.push_log(
                            LogLevel::Detail,
                            format!("[presence voice #{}] {}", seq, text),
                        );
                    }
                    AppEvent::PresenceCheckpointReceived { .. } => {
                        // Detail-level, no user-visible log
                    }
                    AppEvent::VoiceDiagnostic { kind, detail } => {
                        s.push_log(LogLevel::Warn, format!("[voice:{}] {}", kind, detail));
                    }
                    AppEvent::UserTranscript { ref text, seq } => {
                        s.push_log(LogLevel::Info, format!("[transcript #{}] {}", seq, text));
                    }
                    AppEvent::LiveUsageUpdate { .. } => {
                        // Broadcast-only — handled by outbound event converter.
                    }
                    AppEvent::RecordingStarted { ref stream_name } => {
                        s.push_log(
                            LogLevel::Detail,
                            format!("Recording started: {}", stream_name),
                        );
                    }
                    AppEvent::RecordingStopped { ref stream_name } => {
                        s.push_log(
                            LogLevel::Detail,
                            format!("Recording stopped: {}", stream_name),
                        );
                    }
                    AppEvent::RecordingError {
                        ref stream_name,
                        ref message,
                    } => {
                        s.push_log(
                            LogLevel::Warn,
                            format!("Recording error ({}): {}", stream_name, message),
                        );
                    }
                    AppEvent::RecordingDeleted { ref stream_name } => {
                        s.push_log(
                            LogLevel::Info,
                            format!("Recording deleted: {}", stream_name),
                        );
                    }
                    AppEvent::SessionStarted {
                        ref session_id,
                        ref task,
                    } => {
                        s.session_id = session_id.clone();
                        s.task_description = task.clone().unwrap_or_default();
                        s.turn = 0;
                        s.session_tokens = 0;
                        s.session_prompt_tokens = 0;
                        s.session_completion_tokens = 0;
                        s.session_cached_tokens = 0;
                        s.active_session_source = s.session_sources.get(session_id).cloned();
                        if s.is_active_codex_session() {
                            let enabled = s
                                .session_codex_managed_context
                                .get(session_id)
                                .copied()
                                .unwrap_or(s.configured_codex_managed_context);
                            s.session_codex_managed_context
                                .insert(session_id.clone(), enabled);
                            s.codex_managed_context = enabled;
                        }
                        s.set_phase(Phase::Thinking);
                        s.note_session_phase(
                            Some(session_id),
                            Some(0),
                            Phase::Thinking,
                            task.as_deref(),
                        );
                        s.push_log(
                            LogLevel::Info,
                            format!(
                                "Session started: {} — {}",
                                session_id,
                                task.as_deref().unwrap_or("(no task)")
                            ),
                        );
                    }
                    AppEvent::SessionAttached {
                        ref session_id,
                        ref source,
                    } => {
                        s.push_log(
                            LogLevel::Info,
                            format!("Session attached: {} ({})", session_id, source),
                        );
                    }
                    AppEvent::SessionEnded {
                        ref session_id,
                        ref reason,
                    } => {
                        s.note_session_phase(Some(session_id), None, Phase::Done, None);
                        if s.session_id == session_id.as_str() {
                            s.set_phase(Phase::Done);
                            s.active_session_source = None;
                            s.codex_managed_context = s.configured_codex_managed_context;
                        }
                        s.pending_rewind_pressure_checks.remove(session_id);
                        s.insufficient_rewind_notices.remove(session_id);
                        s.push_log(
                            LogLevel::Info,
                            format!("Session ended: {} — {}", session_id, reason),
                        );
                    }
                    AppEvent::DebugScreenReady { display_id } => {
                        s.push_log(
                            LogLevel::Info,
                            format!("Debug screen ready on :{}", display_id),
                        );
                    }
                    AppEvent::DebugScreenTornDown { display_id } => {
                        s.push_log(
                            LogLevel::Info,
                            format!("Debug screen :{} torn down", display_id),
                        );
                    }
                    AppEvent::LiveAudioStarted { id, provider } => {
                        s.push_log(
                            LogLevel::Info,
                            format!("Live audio '{}' started ({})", id, provider),
                        );
                    }
                    AppEvent::LiveAudioProgress {
                        id,
                        state,
                        elapsed_secs,
                        ..
                    } => {
                        s.push_log(
                            LogLevel::Detail,
                            format!("Live audio '{}': {} ({:.0}s)", id, state, elapsed_secs),
                        );
                    }
                    AppEvent::LiveAudioCompleted {
                        id,
                        status,
                        quarantine_count,
                    } => {
                        let q_note = if quarantine_count > 0 {
                            format!(" ({} quarantined)", quarantine_count)
                        } else {
                            String::new()
                        };
                        s.push_log(
                            LogLevel::Info,
                            format!("Live audio '{}': {}{}", id, status, q_note),
                        );
                    }
                    AppEvent::DisplayMetrics { .. }
                    | AppEvent::FileChanged { .. }
                    | AppEvent::UploadReady { .. }
                    | AppEvent::UploadDeleted { .. }
                    | AppEvent::SnapshotCreated { .. }
                    | AppEvent::RolledBack { .. }
                    | AppEvent::Redone { .. }
                    | AppEvent::HistoryPruned { .. }
                    | AppEvent::ConversationRollbackRequested { .. }
                    | AppEvent::ConversationRolledBack { .. } => {
                        // Broadcast-only — handled by outbound event converter.
                    }
                    AppEvent::DisplayCaptureLost {
                        display_id,
                        ref reason,
                    } => {
                        s.push_log(
                            LogLevel::Warn,
                            format!("Display :{} capture lost: {}", display_id, reason),
                        );
                    }
                    AppEvent::DisplayApprovalPending {
                        display_id,
                        backend,
                    } => {
                        s.push_log(LogLevel::Info, format!("Display :{} waiting for OS screen-share approval ({backend} portal)", display_id));
                    }
                    AppEvent::InterruptRequested { ref session_id } => {
                        s.set_phase(Phase::Interrupting);
                        s.note_session_phase(
                            session_id.as_deref(),
                            None,
                            Phase::Interrupting,
                            None,
                        );
                        s.push_log(LogLevel::Info, "Interrupt requested".to_string());
                        resource_changed = Some("intendant://status");
                    }
                    AppEvent::Interrupted {
                        ref session_id,
                        ref reason,
                        ..
                    } => {
                        s.set_phase(Phase::Interrupted);
                        s.note_session_phase(session_id.as_deref(), None, Phase::Interrupted, None);
                        s.push_log(LogLevel::Info, format!("Interrupted: {}", reason));
                        resource_changed = Some("intendant://status");
                    }
                    AppEvent::SteerRequested {
                        ref text, ref id, ..
                    } => {
                        let preview: String = text.chars().take(80).collect();
                        let suffix = if text.chars().count() > 80 { "..." } else { "" };
                        let id_part = if id.is_empty() {
                            String::new()
                        } else {
                            format!(" [{}]", id)
                        };
                        s.push_log(
                            LogLevel::Info,
                            format!("Steer requested{}: {}{}", id_part, preview, suffix),
                        );
                        resource_changed = Some("intendant://logs");
                    }
                    AppEvent::SteerQueued {
                        ref id, ref reason, ..
                    } => {
                        let id_part = if id.is_empty() {
                            String::new()
                        } else {
                            format!(" [{}]", id)
                        };
                        s.push_log(
                            LogLevel::Info,
                            format!("Steer queued{}: {}", id_part, reason),
                        );
                        resource_changed = Some("intendant://logs");
                    }
                    AppEvent::SteerAccepted {
                        ref id, ref reason, ..
                    } => {
                        let id_part = if id.is_empty() {
                            String::new()
                        } else {
                            format!(" [{}]", id)
                        };
                        s.push_log(
                            LogLevel::Info,
                            format!("Steer accepted{}: {}", id_part, reason),
                        );
                        resource_changed = Some("intendant://logs");
                    }
                    AppEvent::SteerDelivered {
                        ref id, mid_turn, ..
                    } => {
                        let id_part = if id.is_empty() {
                            String::new()
                        } else {
                            format!(" [{}]", id)
                        };
                        let mode = if mid_turn {
                            "mid-turn"
                        } else {
                            "turn boundary"
                        };
                        s.push_log(
                            LogLevel::Info,
                            format!("Steer delivered{} ({})", id_part, mode),
                        );
                        resource_changed = Some("intendant://logs");
                    }
                    AppEvent::SteerCancelRequested { .. } => {}
                    AppEvent::SteerCancelled {
                        ref id, ref reason, ..
                    } => {
                        let id_part = if id.is_empty() {
                            String::new()
                        } else {
                            format!(" [{}]", id)
                        };
                        s.push_log(
                            LogLevel::Info,
                            format!("Steer cancelled{}: {}", id_part, reason),
                        );
                        resource_changed = Some("intendant://logs");
                    }
                    AppEvent::FollowUpStatus {
                        ref id,
                        ref status,
                        ref reason,
                        ..
                    } => {
                        let id_part = if id.is_empty() {
                            String::new()
                        } else {
                            format!(" [{}]", id)
                        };
                        let suffix = reason
                            .as_deref()
                            .filter(|s| !s.is_empty())
                            .map(|s| format!(": {}", s))
                            .unwrap_or_default();
                        s.push_log(
                            LogLevel::Info,
                            format!("Follow-up {}{}{}", status, id_part, suffix),
                        );
                        resource_changed = Some("intendant://logs");
                    }
                }
            }

            if let Some(msg) = deferred_control_msg {
                if let Some(uri) = handle_control_command_mcp(&state, &bus, &control_tx, msg).await
                {
                    resource_changed = Some(uri);
                }
            }

            // Send resource update notification if something changed
            if let Some(uri) = resource_changed {
                let peer_guard = peer.lock().await;
                if let Some(ref p) = *peer_guard {
                    let _ = p
                        .notify_resource_updated(ResourceUpdatedNotificationParam {
                            uri: uri.to_string(),
                        })
                        .await;
                }
            }
        }
    })
}

fn apply_observed_event_to_mcp_state(s: &mut McpAppState, event: &AppEvent) -> bool {
    match event {
        AppEvent::ExternalAgentChanged { agent } => {
            s.external_agent = agent
                .as_deref()
                .and_then(crate::external_agent::AgentBackend::from_str_loose);
            true
        }
        AppEvent::CodexConfigChanged {
            managed_context, ..
        } => {
            if let Some(mode) = managed_context {
                s.configured_codex_managed_context =
                    crate::project::codex_managed_context_enabled(mode);
                if !s.is_active_codex_session() {
                    s.codex_managed_context = s.configured_codex_managed_context;
                }
                return true;
            }
            false
        }
        AppEvent::SessionIdentity {
            session_id,
            source,
            backend_session_id,
        } => {
            s.link_session_aliases(session_id, backend_session_id);
            if !session_id.is_empty() {
                s.session_sources.insert(session_id.clone(), source.clone());
            }
            if !backend_session_id.is_empty() {
                s.session_sources
                    .insert(backend_session_id.clone(), source.clone());
            }
            if source.eq_ignore_ascii_case("codex") {
                if let Some(enabled) = s.session_codex_managed_context.get(session_id).copied() {
                    s.session_codex_managed_context
                        .insert(backend_session_id.clone(), enabled);
                } else if let Some(enabled) = s
                    .session_codex_managed_context
                    .get(backend_session_id)
                    .copied()
                {
                    s.session_codex_managed_context
                        .insert(session_id.clone(), enabled);
                }
            }
            if s.session_id.is_empty()
                || s.session_id == session_id.as_str()
                || s.session_id == backend_session_id.as_str()
            {
                s.active_session_source = Some(source.clone());
            }
            true
        }
        AppEvent::SessionCapabilities {
            session_id,
            capabilities,
        } => apply_session_capabilities_to_mcp_state(s, session_id, capabilities),
        AppEvent::ContextSnapshot {
            session_id,
            source,
            format,
            token_count,
            token_count_kind,
            context_window,
            hard_context_window,
            raw,
            ..
        } => apply_context_snapshot_usage_to_mcp_state(
            s,
            session_id.as_deref(),
            source,
            format,
            *token_count,
            token_count_kind.as_deref(),
            *context_window,
            *hard_context_window,
            raw,
        ),
        AppEvent::SessionStarted { session_id, task } => {
            s.session_id = session_id.clone();
            s.task_description = task.clone().unwrap_or_default();
            s.turn = 0;
            s.session_tokens = 0;
            s.session_prompt_tokens = 0;
            s.session_completion_tokens = 0;
            s.session_cached_tokens = 0;
            s.active_session_source = s.session_sources.get(session_id).cloned();
            if s.is_active_codex_session() {
                let enabled = s
                    .session_codex_managed_context
                    .get(session_id)
                    .copied()
                    .unwrap_or(s.configured_codex_managed_context);
                s.session_codex_managed_context
                    .insert(session_id.clone(), enabled);
                s.codex_managed_context = enabled;
            }
            s.set_phase(Phase::Thinking);
            s.note_session_phase(Some(session_id), Some(0), Phase::Thinking, task.as_deref());
            true
        }
        AppEvent::StatusUpdate {
            turn,
            phase,
            session_id,
            task,
            ..
        } => {
            s.note_session_phase(
                Some(session_id),
                Some(*turn),
                phase_from_status_str(phase),
                Some(task),
            );
            if status_task_is_external_turn_progress(task) {
                s.note_session_round(Some(session_id), *turn);
            }
            true
        }
        AppEvent::UsageSnapshot {
            session_id,
            main,
            presence,
        } => {
            let main = s.normalize_main_usage_snapshot(session_id.as_deref(), main.clone());
            if let Some(id) = session_id
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty())
            {
                s.record_session_usage_snapshot(Some(id), main.clone());
                if s.session_id.is_empty() {
                    s.session_id = id.to_string();
                }
                if let Some(source) = s.session_sources.get(id).cloned() {
                    s.active_session_source = Some(source);
                }
            }
            let applies_to_current_session =
                s.session_id_applies_to_current_session(session_id.as_deref());
            if applies_to_current_session {
                s.apply_main_usage_snapshot(main.clone());
                if let Some(presence) = presence {
                    s.presence_provider_name = Some(presence.provider.clone());
                    s.presence_model_name = Some(presence.model.clone());
                    s.presence_tokens = presence.tokens_used;
                    s.presence_context_window = presence.context_window;
                    s.presence_usage_pct = presence.usage_pct;
                }
                s.complete_pending_rewind_pressure_check_for(session_id.as_deref());
                return true;
            }
            s.complete_pending_rewind_pressure_check_for(session_id.as_deref());
            session_id.is_some()
        }
        AppEvent::CodexThreadActionResult {
            session_id,
            action,
            success,
            message,
        } => {
            if action == "rewind_context" {
                s.note_context_rewind_result_for(session_id.as_deref(), *success, message);
            }
            true
        }
        AppEvent::SessionEnded { session_id, .. } => {
            s.note_session_phase(Some(session_id), None, Phase::Done, None);
            if s.session_id == session_id.as_str() {
                s.set_phase(Phase::Done);
                s.active_session_source = None;
                s.codex_managed_context = s.configured_codex_managed_context;
            }
            s.pending_rewind_pressure_checks.remove(session_id);
            s.insufficient_rewind_notices.remove(session_id);
            true
        }
        AppEvent::SessionDirChanged { path } => {
            s.log_dir = path.clone();
            true
        }
        AppEvent::TurnStarted {
            session_id,
            turn,
            budget_pct,
            ..
        } => {
            s.turn = *turn;
            s.budget_pct = *budget_pct;
            s.set_phase(Phase::Thinking);
            s.note_session_phase(session_id.as_deref(), Some(*turn), Phase::Thinking, None);
            true
        }
        AppEvent::AgentStarted {
            session_id, turn, ..
        } => {
            s.note_session_phase(
                session_id.as_deref(),
                Some(*turn),
                Phase::RunningAgent,
                None,
            );
            true
        }
        AppEvent::AgentOutput { session_id, .. } => {
            s.note_session_phase(session_id.as_deref(), None, Phase::RunningAgent, None);
            true
        }
        AppEvent::DoneSignal { session_id, .. } | AppEvent::TaskComplete { session_id, .. } => {
            s.set_phase(Phase::Done);
            s.note_session_phase(session_id.as_deref(), None, Phase::Done, None);
            true
        }
        AppEvent::ApprovalRequired { session_id, .. } => {
            s.set_phase(Phase::WaitingApproval);
            s.note_session_phase(session_id.as_deref(), None, Phase::WaitingApproval, None);
            true
        }
        AppEvent::RoundComplete {
            session_id, round, ..
        } => {
            s.round = *round;
            s.set_phase(Phase::WaitingFollowUp);
            s.note_session_round(session_id.as_deref(), *round);
            s.note_session_phase(
                session_id.as_deref(),
                Some(*round),
                Phase::WaitingFollowUp,
                None,
            );
            true
        }
        AppEvent::InterruptRequested { session_id } => {
            s.set_phase(Phase::Interrupting);
            s.note_session_phase(session_id.as_deref(), None, Phase::Interrupting, None);
            true
        }
        AppEvent::Interrupted { session_id, .. } => {
            s.set_phase(Phase::Interrupted);
            s.note_session_phase(session_id.as_deref(), None, Phase::Interrupted, None);
            true
        }
        AppEvent::LoopError(_) => {
            s.set_phase(Phase::Done);
            true
        }
        _ => false,
    }
}

fn session_log_dir_matches_requested_session(log_dir: &std::path::Path, session_id: &str) -> bool {
    if log_dir
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == session_id)
    {
        return true;
    }

    let Ok(contents) = std::fs::read_to_string(log_dir.join("session_meta.json")) else {
        return false;
    };
    serde_json::from_str::<serde_json::Value>(&contents)
        .ok()
        .and_then(|meta| {
            meta.get("session_id")
                .and_then(|value| value.as_str())
                .map(str::to_string)
        })
        .is_some_and(|id| id == session_id)
}

fn requested_session_log_dirs(
    current_log_dir: &std::path::Path,
    session_id: &str,
) -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    if current_log_dir.join("session.jsonl").is_file()
        && session_log_dir_matches_requested_session(current_log_dir, session_id)
    {
        dirs.push(current_log_dir.to_path_buf());
    }
    if let Some(dir) = crate::session_log::SessionLog::find_session_by_id(session_id) {
        if !dirs.iter().any(|existing| existing == &dir) {
            dirs.push(dir);
        }
    }
    dirs
}

fn hydrate_requested_session_status_from_logs(s: &mut McpAppState, session_id: &str) -> bool {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return false;
    }
    let dirs = requested_session_log_dirs(&s.log_dir, session_id);
    if dirs.is_empty() {
        return false;
    }

    let provider_name = s.provider_name.clone();
    let model_name = s.model_name.clone();
    let turn = s.turn;
    let budget_pct = s.budget_pct;
    let phase = s.phase.clone();
    let phase_entered_at = s.phase_entered_at;
    let session_tokens = s.session_tokens;
    let session_prompt_tokens = s.session_prompt_tokens;
    let session_completion_tokens = s.session_completion_tokens;
    let session_cached_tokens = s.session_cached_tokens;
    let context_window = s.context_window;
    let hard_context_window = s.hard_context_window;
    let active_session_id = s.session_id.clone();
    let task_description = s.task_description.clone();
    let active_session_source = s.active_session_source.clone();
    let codex_managed_context = s.codex_managed_context;

    let mut changed = false;
    for dir in dirs {
        let Ok(contents) = std::fs::read_to_string(dir.join("session.jsonl")) else {
            continue;
        };
        for line in contents.lines() {
            let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let Some(event) = crate::session_log::session_log_entry_to_app_event(&entry, &dir)
            else {
                continue;
            };
            changed |= apply_observed_event_to_mcp_state(s, &event);
        }
    }

    s.provider_name = provider_name;
    s.model_name = model_name;
    s.turn = turn;
    s.budget_pct = budget_pct;
    s.phase = phase;
    s.phase_entered_at = phase_entered_at;
    s.session_tokens = session_tokens;
    s.session_prompt_tokens = session_prompt_tokens;
    s.session_completion_tokens = session_completion_tokens;
    s.session_cached_tokens = session_cached_tokens;
    s.context_window = context_window;
    s.hard_context_window = hard_context_window;
    s.session_id = active_session_id;
    s.task_description = task_description;
    s.active_session_source = active_session_source;
    s.codex_managed_context = codex_managed_context;

    changed
}

/// Lightweight event mirror for the stateless HTTP MCP endpoint used by
/// external agents. It intentionally observes state only; it does not dispatch
/// `ControlMsg`s, because the normal control plane remains the single writer.
pub fn spawn_http_observation_listener(
    state: SharedMcpState,
    mut event_rx: tokio::sync::broadcast::Receiver<AppEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let event = match event_rx.recv().await {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            let mut s = state.write().await;
            apply_observed_event_to_mcp_state(&mut s, &event);
        }
    })
}

// ---------------------------------------------------------------------------
// Tool parameter types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EmptyToolParams {}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ApproveParams {
    /// The approval ID (turn number) to approve.
    pub id: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DenyParams {
    /// The approval ID (turn number) to deny.
    pub id: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SkipParams {
    /// The approval ID (turn number) to skip.
    pub id: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ApproveAllParams {
    /// The approval ID (turn number) to approve (also sets autonomy to Full).
    pub id: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RespondParams {
    /// The text response to the askHuman question.
    pub text: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SetAutonomyParams {
    /// The autonomy level: "low", "medium", "high", or "full".
    pub level: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SetVerbosityParams {
    /// The verbosity level: "quiet", "normal", "verbose", or "debug".
    pub level: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct StartTaskParams {
    /// Optional target session. When present, route the text as a follow-up
    /// turn for that managed session instead of starting a brand-new task.
    #[serde(default, alias = "sessionId")]
    pub session_id: Option<String>,
    /// The task description for the AI agent to execute.
    pub task: String,
    /// When true, use orchestration mode (spawns orchestrator + sub-agents)
    /// instead of direct mode. When false or omitted, the mode is chosen
    /// automatically: complex tasks use orchestration, simple tasks use direct.
    #[serde(default)]
    pub orchestrate: Option<bool>,
    /// Frame IDs the user was looking at when they made this request.
    /// When present, routes to the ephemeral CU task runner with a fast
    /// CU-capable model instead of the regular agent loop.
    #[serde(default)]
    pub reference_frame_ids: Vec<String>,
    /// Explicit display target for CU tasks: "user_session", "display_99", etc.
    #[serde(default)]
    pub display_target: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RewindContextAnchorParams {
    /// Exact Codex thread item/tool-call id to roll back to. Once a rewind is needed, use list_rewind_anchors first when the id is not already known.
    pub item_id: String,
    /// Whether the anchored item itself should survive rollback: "before" or "after".
    pub position: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RewindContextParams {
    /// Optional Intendant or backend session id. Omit for the active Codex session.
    #[serde(default, alias = "sessionId")]
    pub session_id: Option<String>,
    /// Exact item anchor for the rollback target.
    pub anchor: RewindContextAnchorParams,
    /// Why the current branch should be rewound.
    pub reason: String,
    /// Carry-forward context for the resumed branch. Include only useful facts from the pruned span.
    pub primer: String,
    /// Optional facts, decisions, or artifacts to preserve.
    #[serde(default)]
    pub preserve: Vec<String>,
    /// Optional dead ends, assumptions, or work to discard.
    #[serde(default)]
    pub discard: Vec<String>,
    /// Optional files, commits, logs, or outputs created before the rewind.
    #[serde(default)]
    pub artifacts: Vec<String>,
    /// Optional recommended next actions for the resumed branch.
    #[serde(default)]
    pub next_steps: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListRewindAnchorsParams {
    /// Optional Intendant or backend session id. Omit for the active Codex session.
    #[serde(default, alias = "sessionId")]
    pub session_id: Option<String>,
    /// Diagnostic/detail page offset. Omit for the default compact whole-catalog result.
    #[serde(default)]
    pub offset: Option<usize>,
    /// Diagnostic/detail page size. Omit for the default compact whole-catalog result.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Optional case-insensitive search over anchor ids, item types, tool names, roles, and summaries.
    #[serde(default)]
    pub query: Option<String>,
    /// Return anchors from newest to oldest when true. This only changes ordering; choose
    /// an exact returned row based on its positions, summary, and optional estimates.
    #[serde(default)]
    pub reverse: bool,
    /// Include per-anchor rollout-size estimates for how much recent context each
    /// before/after position would discard. This is included automatically for
    /// query and reverse listings.
    #[serde(default, alias = "includePruningEstimates")]
    pub include_pruning_estimates: bool,
    /// Return detailed paged rows instead of the default compact whole-catalog rows.
    #[serde(default)]
    pub detail: bool,
    /// Include managed-context maintenance calls such as list_rewind_anchors or rewind_context.
    /// Omit this during ordinary recovery so discovery does not target its own tool calls.
    #[serde(default, alias = "includeManagementTools")]
    pub include_management_tools: bool,
    /// Deprecated bypass flag. Normal model-facing listings keep this enabled unless
    /// include_non_recovery=true is set for an explicit diagnostic audit.
    #[serde(default, alias = "recoveryCandidatesOnly")]
    pub recovery_candidates_only: Option<bool>,
    /// Diagnostic-only audit mode. Includes anchors/positions known to still be
    /// at/above the rewind-only limit or without enough restore headroom; these
    /// rows are not valid rewind_context targets when recovery_eligible=false or
    /// the requested position is absent from default positions / audit
    /// recovery_eligible_positions.
    #[serde(default, alias = "includeNonRecovery")]
    pub include_non_recovery: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct InspectRewindAnchorParams {
    /// Optional Intendant or backend session id. Omit for the active Codex session.
    #[serde(default, alias = "sessionId")]
    pub session_id: Option<String>,
    /// Exact Codex thread item/tool-call id to inspect.
    pub item_id: String,
    /// Number of neighboring response items to include on each side. The backend caps this.
    #[serde(default)]
    pub radius: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RewindBackoutParams {
    /// Optional Intendant or backend session id. Omit for the active Codex session.
    #[serde(default, alias = "sessionId")]
    pub session_id: Option<String>,
    /// Context rewind record id returned by rewind_context.
    pub record_id: String,
    /// Backout mode: "inspect" (default) returns the saved rollout path; "restore" restores the active Codex thread in place; "fork"/"backout" create a new Codex thread that inherits the lineage prompt-cache key when the patched Codex binary is used.
    #[serde(default)]
    pub mode: Option<String>,
    /// Optional display name for the recovery fork.
    #[serde(default)]
    pub name: Option<String>,
    /// Legacy compatibility flag. Fork/backout no longer require this with the patched Codex lineage-cache-key support.
    #[serde(default)]
    pub allow_cache_reset: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ClaimFissionCanonicalParams {
    /// Fission group id from get_status().fission_ledger.groups[].group_id.
    pub group_id: String,
    /// Branch/session id to claim as the canonical continuation for this group.
    pub branch_session_id: String,
    /// Optional compare-and-swap guard. Omit for first-writer-wins behavior; provide the current canonical id to reassign deliberately.
    #[serde(default)]
    pub expected_canonical_session_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TakeDisplayParams {
    /// Display ID to claim (e.g. 99 for virtual display 99).
    pub display_id: u32,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReleaseDisplayParams {
    /// Display ID to release.
    pub display_id: u32,
    /// Optional note explaining why control was released.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SpawnLiveAudioParams {
    /// Unique identifier for this live audio session.
    pub id: String,
    /// Live audio model provider: "openai" or "gemini".
    pub provider: String,
    /// System prompt with goal, talking points, and decision tree for the conversation.
    pub playbook: String,
    /// Schema defining the structured response fields. Must be an object with a
    /// "fields" array. Each field has: name (string), field_type (object with
    /// "type": "string"|"integer"|"boolean"|"array"), required (bool), description (string).
    pub response_schema: McpResponseSchema,
    /// Hard timeout in seconds. Default: 300.
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// Voice name (e.g. "alloy" for OpenAI, "Aoede" for Gemini).
    #[serde(default)]
    pub voice: Option<String>,
    /// Optional model override (e.g. "gpt-4o-realtime-preview").
    #[serde(default)]
    pub model: Option<String>,
    /// Optional text sent to the model after setup, before audio bridging.
    #[serde(default)]
    pub initial_message: Option<String>,
}

/// Response schema for spawn_live_audio. Mirrors live_audio_types::ResponseSchema
/// but derives JsonSchema so MCP advertises concrete types instead of "any".
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct McpResponseSchema {
    /// Array of field definitions.
    pub fields: Vec<McpFieldSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct McpFieldSpec {
    /// Field name.
    pub name: String,
    /// Field type definition (e.g. {"type":"string","max_length":100,"tainted":true}).
    pub field_type: McpFieldType,
    /// Whether this field is required for submission.
    #[serde(default)]
    pub required: bool,
    /// Description of the field.
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpFieldType {
    String {
        #[serde(default)]
        max_length: Option<usize>,
        #[serde(default)]
        allowed_values: Option<Vec<String>>,
        #[serde(default)]
        tainted: bool,
    },
    Integer {
        #[serde(default)]
        min: Option<i64>,
        #[serde(default)]
        max: Option<i64>,
    },
    Boolean,
    Array {
        /// Element type for the array. Non-recursive: arrays of arrays are
        /// not supported in response schemas.
        element_type: McpArrayElement,
        #[serde(default)]
        max_items: Option<usize>,
    },
}

/// Non-recursive array element type. Keeps the MCP schema free of self-
/// referencing `$ref`s so inlining is straightforward.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpArrayElement {
    String {
        #[serde(default)]
        max_length: Option<usize>,
        #[serde(default)]
        tainted: bool,
    },
    Integer {
        #[serde(default)]
        min: Option<i64>,
        #[serde(default)]
        max: Option<i64>,
    },
    Boolean,
}

fn default_timeout() -> u64 {
    300
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TakeScreenshotParams {
    /// Display target: "user_session", "display_99", etc. Auto-detects if omitted.
    #[serde(default)]
    pub display_target: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CreateBrowserWorkspaceParams {
    /// URL to open in the browser workspace. Omit for about:blank.
    #[serde(default)]
    pub url: Option<String>,
    /// Human label shown in the dashboard.
    #[serde(default)]
    pub label: Option<String>,
    /// Provider: auto, cdp, system_cdp, playwright, agent_browser, or stream. The default cdp backend uses managed Chromium; system_cdp deliberately launches the user's installed browser.
    #[serde(default)]
    pub provider: Option<String>,
    /// Optional federation peer id. Remote placement is part of the contract but not wired yet.
    #[serde(default)]
    pub peer_id: Option<String>,
    /// Session or agent that owns this workspace.
    #[serde(default)]
    pub owner_session_id: Option<String>,
    /// Explicit browser profile directory. If omitted, Intendant creates one under its data dir.
    #[serde(default)]
    pub profile_dir: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CloseBrowserWorkspaceParams {
    pub workspace_id: String,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AcquireBrowserWorkspaceParams {
    pub workspace_id: String,
    pub holder_id: String,
    #[serde(default)]
    pub holder_kind: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReleaseBrowserWorkspaceParams {
    pub workspace_id: String,
    #[serde(default)]
    pub holder_id: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ExecuteCuActionsParams {
    /// Array of computer-use actions to execute. Each action is a tagged object
    /// with "type" (click, double_click, type, key, scroll, move_mouse, drag,
    /// screenshot, wait) and type-specific fields.
    pub actions: Vec<crate::computer_use::CuAction>,
    /// Display target. Auto-detects if omitted.
    #[serde(default)]
    pub display_target: Option<String>,
    /// Coordinate space for click/scroll/move coordinates. Default: "pixel"
    /// (coordinates are in display logical points). Set to "normalized_1000"
    /// if the model outputs coordinates on a 0-1000 grid (e.g. Gemini CU).
    #[serde(default)]
    pub coordinate_space: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListFramesParams {
    /// Filter by stream name (e.g. "display_99", "display_user_session").
    #[serde(default)]
    pub stream: Option<String>,
    /// Maximum number of frames to return. Default: 20.
    #[serde(default)]
    pub count: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadFrameParams {
    /// Frame ID to read. Use "latest" for the most recent frame.
    pub frame_id: String,
    /// Stream filter (used when frame_id is "latest").
    #[serde(default)]
    pub stream: Option<String>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct SharedViewRegionParams {
    /// Normalized left coordinate, from 0.0 to 1.0.
    pub x: f64,
    /// Normalized top coordinate, from 0.0 to 1.0.
    pub y: f64,
    /// Normalized width, from 0.0 to 1.0.
    pub width: f64,
    /// Normalized height, from 0.0 to 1.0.
    pub height: f64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ShowSharedViewParams {
    /// Display target to foreground, such as "user_session" or "display_99".
    #[serde(default)]
    pub display_target: Option<String>,
    /// Numeric display id. Prefer this when known.
    #[serde(default)]
    pub display_id: Option<u32>,
    /// Why the agent wants the user to watch or collaborate.
    #[serde(default)]
    pub reason: Option<String>,
    /// Optional normalized region to highlight after the view opens.
    #[serde(default)]
    pub focus_region: Option<SharedViewRegionParams>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct HideSharedViewParams {
    /// Optional reason for dismissing the collaboration view.
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FocusSharedViewParams {
    /// Display target to focus, such as "user_session" or "display_99".
    #[serde(default)]
    pub display_target: Option<String>,
    /// Numeric display id. Prefer this when known.
    #[serde(default)]
    pub display_id: Option<u32>,
    /// Normalized region to highlight.
    pub region: SharedViewRegionParams,
    /// Short label for what the user should look at.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RequestSharedViewInputParams {
    /// Display target where user input is useful, such as "user_session" or "display_99".
    #[serde(default)]
    pub display_target: Option<String>,
    /// Numeric display id. Prefer this when known.
    #[serde(default)]
    pub display_id: Option<u32>,
    /// Why the agent wants input authority or human interaction.
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CaptureSharedViewFrameParams {
    /// Display target to capture, such as "user_session" or "display_99". Auto-detects if omitted.
    #[serde(default)]
    pub display_target: Option<String>,
    /// Numeric display id. Prefer this when known.
    #[serde(default)]
    pub display_id: Option<u32>,
    /// Optional note that appears in the dashboard shared-view banner.
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ScheduleControllerRestartParams {
    /// Identifier for the controlling agent/client (e.g. "codex", "claude_code").
    pub controller_id: String,
    /// Goal for the next controller session / autonomous cycle.
    pub north_star_goal: String,
    /// Optional operator-provided reason.
    #[serde(default)]
    pub reason: Option<String>,
    /// When to execute restart: "turn_end" (default) or "now".
    #[serde(default)]
    pub restart_after: Option<String>,
    /// Optional command to spawn for controller restart.
    #[serde(default)]
    pub restart_command: Option<String>,
    /// Auto-start the next intendant task with north_star_goal (default: false).
    #[serde(default)]
    pub auto_start_task: Option<bool>,
    /// Maximum restart attempts before failing (default: 1).
    #[serde(default)]
    pub max_attempts: Option<u32>,
    /// Cooldown between restart attempts in seconds (default: 30).
    #[serde(default)]
    pub cooldown_sec: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ControllerTurnCompleteParams {
    /// Restart ID returned by schedule_controller_restart.
    pub restart_id: String,
    /// Completion token returned by schedule_controller_restart.
    pub turn_complete_token: String,
    /// Optional completion status from the controller.
    #[serde(default)]
    pub status: Option<String>,
    /// Optional final handoff summary from the controller.
    #[serde(default)]
    pub handoff_summary: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CancelControllerRestartParams {
    /// Optional restart ID guard. If provided and mismatched, cancellation is rejected.
    #[serde(default)]
    pub restart_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RequestControllerLoopHaltParams {
    /// When true (default), block all future loop cycles until cleared.
    /// When false, request a one-shot halt after the next cycle boundary.
    #[serde(default)]
    pub persistent: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct InterveneControllerLoopParams {
    /// Intervention mode: "stop" (graceful TERM) or "abort" (immediate KILL).
    pub mode: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetLogsParams {
    /// Optional Intendant session id. HTTP MCP requests also default this from the session_id query parameter.
    #[serde(default, alias = "sessionId")]
    pub session_id: Option<String>,
    /// Only return log entries with IDs greater than this value (cursor-based pagination).
    #[serde(default)]
    pub since_id: Option<u64>,
    /// Filter by log level: "info", "model", "agent", "error", "warn", "subagent", "debug".
    #[serde(default)]
    pub level_filter: Option<String>,
    /// Maximum number of entries to return (default: 100).
    #[serde(default)]
    pub limit: Option<usize>,
}

fn read_persisted_log_entries_for_session(
    session_id: Option<&str>,
    params: &GetLogsParams,
) -> Option<Vec<LogEntrySnapshot>> {
    let session_id = session_id.map(str::trim).filter(|id| !id.is_empty())?;
    let log_dir = crate::session_log::SessionLog::find_session_by_id(session_id)?;
    let contents = std::fs::read_to_string(log_dir.join("session.jsonl")).ok()?;
    let limit = params.limit.unwrap_or(100);
    let mut entries = Vec::new();

    for (line_idx, line) in contents.lines().enumerate() {
        if entries.len() >= limit {
            break;
        }
        let line_id = line_idx as u64;
        if params
            .since_id
            .map(|since| line_id <= since)
            .unwrap_or(false)
        {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let level = persisted_log_entry_level(&value);
        if params
            .level_filter
            .as_deref()
            .map(|filter| filter != level)
            .unwrap_or(false)
        {
            continue;
        }
        entries.push(LogEntrySnapshot {
            id: line_id,
            ts: persisted_log_entry_ts(&value),
            level,
            content: persisted_log_entry_content(&value),
        });
    }

    Some(entries)
}

fn persisted_log_entry_level(value: &serde_json::Value) -> String {
    match value.get("event").and_then(serde_json::Value::as_str) {
        Some("model_response") | Some("reasoning") => "model".to_string(),
        Some("agent_output") | Some("agent_input") => "agent".to_string(),
        Some("error") => "error".to_string(),
        Some("warn") => "warn".to_string(),
        _ => value
            .get("level")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("info")
            .to_string(),
    }
}

fn persisted_log_entry_ts(value: &serde_json::Value) -> String {
    value
        .get("ts")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn persisted_log_entry_content(value: &serde_json::Value) -> String {
    let event = value
        .get("event")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("log");
    if let Some(message) = value
        .get("message")
        .and_then(serde_json::Value::as_str)
        .filter(|message| !message.is_empty())
    {
        return message.to_string();
    }
    if let Some(turn) = value.get("turn").and_then(serde_json::Value::as_u64) {
        return format!("{event} (turn {turn})");
    }
    event.to_string()
}

// ---------------------------------------------------------------------------
// MCP Server
// ---------------------------------------------------------------------------

/// The Intendant MCP server. Exposes tools (actions) and resources (observations)
/// that mirror the TUI exactly.
#[derive(Clone)]
pub struct IntendantServer {
    state: SharedMcpState,
    bus: EventBus,
    tool_router: ToolRouter<Self>,
}

impl IntendantServer {
    pub fn new(state: SharedMcpState, bus: EventBus) -> Self {
        Self {
            state,
            bus,
            tool_router: Self::tool_router(),
        }
    }

    pub fn new_http(state: SharedMcpState, bus: EventBus) -> Self {
        spawn_http_observation_listener(state.clone(), bus.subscribe());
        Self::new(state, bus)
    }

    async fn start_task_internal(
        &self,
        task: String,
        source: &str,
        orchestrate: Option<bool>,
    ) -> Result<(), String> {
        start_task_with_state(&self.state, &self.bus, task, source, orchestrate).await
    }

    async fn run_scheduled_controller_restart(&self) -> Result<String, String> {
        run_scheduled_controller_restart_with_state(&self.state, &self.bus).await
    }

    async fn dispatch_codex_thread_action_and_wait(
        &self,
        session_id: Option<String>,
        op: String,
        params: serde_json::Value,
        timeout_message: String,
    ) -> String {
        let mut result_rx = self.bus.subscribe();
        self.bus
            .send(AppEvent::ControlCommand(ControlMsg::CodexThreadAction {
                session_id: session_id.clone(),
                op: op.clone(),
                params,
            }));

        match tokio::time::timeout(std::time::Duration::from_secs(20), async {
            loop {
                match result_rx.recv().await {
                    Ok(AppEvent::CodexThreadActionResult {
                        session_id: result_session_id,
                        action,
                        success,
                        message,
                    }) if action == op
                        && codex_thread_action_result_targets_session(
                            &session_id,
                            &result_session_id,
                        ) =>
                    {
                        if success {
                            return message;
                        }
                        return format!("{op} failed: {message}");
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        return format!("{timeout_message}; event bus closed before result");
                    }
                }
            }
        })
        .await
        {
            Ok(message) => message,
            Err(_) => format!("{timeout_message}; timed out waiting for result"),
        }
    }

    /// Return MCP tool definitions as JSON for the HTTP transport.
    /// Schemas are flattened (all `$ref`/`$defs` inlined) for compatibility
    /// with clients that don't resolve JSON Schema references (e.g. Codex).
    pub async fn list_tools_json(&self) -> serde_json::Value {
        self.list_tools_json_for_session(None, None, None).await
    }

    pub async fn list_tools_json_for_session(
        &self,
        session_id: Option<&str>,
        managed_context_override: Option<bool>,
        tool_profile: Option<&str>,
    ) -> serde_json::Value {
        let managed_context = self
            .state
            .read()
            .await
            .exposed_codex_managed_context_enabled_for(session_id, managed_context_override);
        let mut tools: Vec<serde_json::Value> = self
            .tool_router
            .list_all()
            .iter()
            .filter(|tool| {
                tool_allowed_for_profile(tool.name.as_ref(), managed_context, tool_profile)
            })
            .map(|tool| {
                let mut schema = serde_json::to_value(&*tool.input_schema).unwrap_or_default();
                inline_schema_refs(&mut schema);
                serde_json::json!({
                    "name": tool.name,
                    "description": tool.description,
                    "inputSchema": schema,
                })
            })
            .collect();
        append_manual_http_tool_definitions(&mut tools, managed_context, tool_profile);
        serde_json::json!({ "tools": tools })
    }

    /// Dispatch a tool call by name for the HTTP transport.
    pub async fn call_tool_by_name(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<CallToolResult, String> {
        self.call_tool_by_name_for_session(name, args, None, None)
            .await
    }

    pub async fn call_tool_by_name_for_session(
        &self,
        name: &str,
        args: serde_json::Value,
        session_id: Option<&str>,
        managed_context_override: Option<bool>,
    ) -> Result<CallToolResult, String> {
        fn parse_params<T: serde::de::DeserializeOwned>(
            args: serde_json::Value,
        ) -> Result<Parameters<T>, String> {
            serde_json::from_value(args)
                .map(Parameters)
                .map_err(|e| e.to_string())
        }

        if let Some(message) = self.state.read().await.rewind_only_gate_message_for(
            name,
            session_id,
            managed_context_override,
        ) {
            return Ok(text_tool_error(message));
        }
        if managed_context_tool(name)
            && !self
                .state
                .read()
                .await
                .exposed_codex_managed_context_enabled_for(session_id, managed_context_override)
        {
            return Ok(text_tool_error(
                "Codex managed context is disabled for this session. Set `[agent.codex] managed_context = \"managed\"` before starting the task, or choose Managed context = managed in the dashboard, to enable list_rewind_anchors/inspect_rewind_anchor/rewind_context/rewind_backout.".to_string(),
            ));
        }

        match name {
            "get_status" => Ok(text_tool_result(
                self.get_status_for_session(session_id, managed_context_override)
                    .await,
            )),
            "get_logs" => {
                let Parameters(params) = parse_params::<GetLogsParams>(args)?;
                Ok(text_tool_result(
                    self.get_logs_for_session(params, session_id).await,
                ))
            }
            "get_pending_approval" => Ok(text_tool_result(self.get_pending_approval().await)),
            "get_pending_input" => Ok(text_tool_result(self.get_pending_input().await)),
            "approve" => {
                let params = parse_params::<ApproveParams>(args)?;
                Ok(text_tool_result(self.approve(params).await))
            }
            "deny" => {
                let params = parse_params::<DenyParams>(args)?;
                Ok(text_tool_result(self.deny(params).await))
            }
            "skip" => {
                let params = parse_params::<SkipParams>(args)?;
                Ok(text_tool_result(self.skip(params).await))
            }
            "approve_all" => {
                let params = parse_params::<ApproveAllParams>(args)?;
                Ok(text_tool_result(self.approve_all(params).await))
            }
            "respond" => {
                let params = parse_params::<RespondParams>(args)?;
                Ok(text_tool_result(self.respond(params).await))
            }
            "set_autonomy" => {
                let params = parse_params::<SetAutonomyParams>(args)?;
                Ok(text_tool_result(self.set_autonomy(params).await))
            }
            "set_verbosity" => {
                let params = parse_params::<SetVerbosityParams>(args)?;
                Ok(text_tool_result(self.set_verbosity(params).await))
            }
            "quit" => Ok(text_tool_result(self.quit().await)),
            "start_task" => {
                let params =
                    parse_params::<StartTaskParams>(with_default_mcp_session_id(args, session_id))?;
                Ok(text_tool_result(self.start_task(params).await))
            }
            "rewind_context" => {
                let params = parse_params::<RewindContextParams>(with_default_mcp_session_id(
                    args, session_id,
                ))?;
                Ok(text_tool_result(self.rewind_context(params).await))
            }
            "list_rewind_anchors" => {
                let params = parse_params::<ListRewindAnchorsParams>(with_default_mcp_session_id(
                    args, session_id,
                ))?;
                Ok(text_tool_result(self.list_rewind_anchors(params).await))
            }
            "inspect_rewind_anchor" => {
                let params = parse_params::<InspectRewindAnchorParams>(
                    with_default_mcp_session_id(args, session_id),
                )?;
                Ok(text_tool_result(self.inspect_rewind_anchor(params).await))
            }
            "rewind_backout" => {
                let params = parse_params::<RewindBackoutParams>(with_default_mcp_session_id(
                    args, session_id,
                ))?;
                Ok(text_tool_result(self.rewind_backout(params).await))
            }
            "claim_fission_canonical" => {
                let params = parse_params::<ClaimFissionCanonicalParams>(args)?;
                Ok(text_tool_result(self.claim_fission_canonical(params).await))
            }
            "schedule_controller_restart" => {
                let params = parse_params::<ScheduleControllerRestartParams>(args)?;
                Ok(text_tool_result(
                    self.schedule_controller_restart(params).await,
                ))
            }
            "controller_turn_complete" => {
                let params = parse_params::<ControllerTurnCompleteParams>(args)?;
                Ok(text_tool_result(
                    self.controller_turn_complete(params).await,
                ))
            }
            "get_restart_status" => Ok(text_tool_result(self.get_restart_status().await)),
            "cancel_controller_restart" => {
                let params = parse_params::<CancelControllerRestartParams>(args)?;
                Ok(text_tool_result(
                    self.cancel_controller_restart(params).await,
                ))
            }
            "request_controller_loop_halt" => {
                let params = parse_params::<RequestControllerLoopHaltParams>(args)?;
                Ok(text_tool_result(
                    self.request_controller_loop_halt(params).await,
                ))
            }
            "clear_controller_loop_halt" => {
                Ok(text_tool_result(self.clear_controller_loop_halt().await))
            }
            "intervene_controller_loop" => {
                let params = parse_params::<InterveneControllerLoopParams>(args)?;
                Ok(text_tool_result(
                    self.intervene_controller_loop(params).await,
                ))
            }
            "get_controller_loop_status" => {
                Ok(text_tool_result(self.get_controller_loop_status().await))
            }
            "browser_workspace_providers" => {
                Ok(text_tool_result(self.browser_workspace_providers().await))
            }
            "list_browser_workspaces" => Ok(text_tool_result(self.list_browser_workspaces().await)),
            "create_browser_workspace" => {
                let params = parse_params::<CreateBrowserWorkspaceParams>(args)?;
                Ok(text_tool_result(
                    self.create_browser_workspace(params).await,
                ))
            }
            "close_browser_workspace" => {
                let params = parse_params::<CloseBrowserWorkspaceParams>(args)?;
                Ok(text_tool_result(self.close_browser_workspace(params).await))
            }
            "acquire_browser_workspace" => {
                let params = parse_params::<AcquireBrowserWorkspaceParams>(args)?;
                Ok(text_tool_result(
                    self.acquire_browser_workspace(params).await,
                ))
            }
            "release_browser_workspace" => {
                let params = parse_params::<ReleaseBrowserWorkspaceParams>(args)?;
                Ok(text_tool_result(
                    self.release_browser_workspace(params).await,
                ))
            }
            "list_displays" => Ok(text_tool_result(self.list_displays().await)),
            "take_display" => {
                let params = parse_params::<TakeDisplayParams>(args)?;
                Ok(text_tool_result(self.take_display(params).await))
            }
            "release_display" => {
                let params = parse_params::<ReleaseDisplayParams>(args)?;
                Ok(text_tool_result(self.release_display(params).await))
            }
            "show_shared_view" => {
                let Parameters(params) = parse_params::<ShowSharedViewParams>(args)?;
                Ok(text_tool_result(
                    self.show_shared_view_for_session(params, session_id).await,
                ))
            }
            "hide_shared_view" => {
                let Parameters(params) = parse_params::<HideSharedViewParams>(args)?;
                Ok(text_tool_result(
                    self.hide_shared_view_for_session(params, session_id).await,
                ))
            }
            "focus_shared_view" => {
                let Parameters(params) = parse_params::<FocusSharedViewParams>(args)?;
                Ok(text_tool_result(
                    self.focus_shared_view_for_session(params, session_id).await,
                ))
            }
            "request_shared_view_input" => {
                let Parameters(params) = parse_params::<RequestSharedViewInputParams>(args)?;
                Ok(text_tool_result(
                    self.request_shared_view_input_for_session(params, session_id)
                        .await,
                ))
            }
            "capture_shared_view_frame" => {
                let Parameters(params) = parse_params::<CaptureSharedViewFrameParams>(args)?;
                self.capture_shared_view_frame_for_session(params, session_id)
                    .await
                    .map_err(|e| e.to_string())
            }
            "take_screenshot" => {
                let params = parse_params::<TakeScreenshotParams>(args)?;
                self.take_screenshot(params)
                    .await
                    .map_err(|e| e.to_string())
            }
            "execute_cu_actions" => {
                let params = parse_params::<ExecuteCuActionsParams>(args)?;
                self.execute_cu_actions(params)
                    .await
                    .map_err(|e| e.to_string())
            }
            "list_frames" => {
                let params = parse_params::<ListFramesParams>(args)?;
                Ok(text_tool_result(self.list_frames(params).await))
            }
            "read_frame" => {
                let params = parse_params::<ReadFrameParams>(args)?;
                Ok(text_tool_result(self.read_frame(params).await))
            }
            "spawn_live_audio" => {
                let params = parse_params::<SpawnLiveAudioParams>(args)?;
                Ok(text_tool_result(self.spawn_live_audio(params).await))
            }
            _ => Err(format!("Unknown tool: {}", name)),
        }
    }
}

/// Process a [`UserAction`] against the shared state. This is the **single**
/// handler that both TUI and MCP feed into.
///
/// Note: for actions that need async access (like writing autonomy), the caller
/// must handle the async parts. This function handles the state-mutation and
/// oneshot-sending synchronously.
fn resolve_approval(registry: &ApprovalRegistry, id: u64, response: ApprovalResponse) {
    if let Ok(mut reg) = registry.lock() {
        if let Some(responder) = reg.remove(&id) {
            let _ = responder.send(response);
        }
    }
}

fn process_action_sync(state: &mut McpAppState, action: UserAction) -> ActionOutcome {
    // Exhaustive match — no wildcard. Compile-time parity enforcement.
    match action {
        UserAction::Approve { id: _ } => {
            if let Some(pending) = state.pending_approval.take() {
                resolve_approval(
                    &state.approval_registry,
                    pending.id,
                    ApprovalResponse::Approve,
                );
                state.set_phase(Phase::RunningAgent);
                state.push_log(LogLevel::Info, "Approved by MCP agent".to_string());
                ActionOutcome::Ok
            } else {
                ActionOutcome::NoOp {
                    reason: "No pending approval".to_string(),
                }
            }
        }
        UserAction::Deny { id: _ } => {
            if let Some(pending) = state.pending_approval.take() {
                resolve_approval(&state.approval_registry, pending.id, ApprovalResponse::Deny);
                state.set_phase(Phase::Done);
                state.push_log(LogLevel::Info, "Denied by MCP agent".to_string());
                ActionOutcome::Ok
            } else {
                ActionOutcome::NoOp {
                    reason: "No pending approval".to_string(),
                }
            }
        }
        UserAction::Skip { id: _ } => {
            if let Some(pending) = state.pending_approval.take() {
                resolve_approval(&state.approval_registry, pending.id, ApprovalResponse::Skip);
                state.set_phase(Phase::RunningAgent);
                state.push_log(LogLevel::Info, "Skipped by MCP agent".to_string());
                ActionOutcome::Ok
            } else {
                ActionOutcome::NoOp {
                    reason: "No pending approval".to_string(),
                }
            }
        }
        UserAction::ApproveAll { id: _ } => {
            if let Some(pending) = state.pending_approval.take() {
                resolve_approval(
                    &state.approval_registry,
                    pending.id,
                    ApprovalResponse::ApproveAll,
                );
                state.set_phase(Phase::RunningAgent);
                state.push_log(
                    LogLevel::Info,
                    "Approved all (autonomy → Full) by MCP agent".to_string(),
                );
                ActionOutcome::Ok
            } else {
                ActionOutcome::NoOp {
                    reason: "No pending approval".to_string(),
                }
            }
        }
        UserAction::RespondHuman { text } => {
            if state.human_question.is_some() {
                // Write response to session-scoped file (same mechanism as TUI)
                let response_path = state.log_dir.join("human_response");
                if std::fs::write(&response_path, &text).is_ok() {
                    state.human_question = None;
                    state.set_phase(Phase::RunningAgent);
                    state.push_log(LogLevel::Info, format!("Human response (MCP): {}", text));
                    ActionOutcome::Ok
                } else {
                    ActionOutcome::NoOp {
                        reason: "Failed to write response file".to_string(),
                    }
                }
            } else {
                ActionOutcome::NoOp {
                    reason: "No pending human question".to_string(),
                }
            }
        }
        UserAction::SetAutonomy { level: _ } => {
            // Autonomy is set asynchronously by the caller after this returns.
            ActionOutcome::Ok
        }
        UserAction::SetVerbosity { level } => {
            state.verbosity = level;
            state.push_log(
                LogLevel::Info,
                format!("Verbosity set to {} by MCP agent", verbosity_to_str(level)),
            );
            ActionOutcome::Ok
        }
        UserAction::SubmitFollowUp { .. } => {
            // Follow-up is handled asynchronously via the channel, not here.
            ActionOutcome::NoOp {
                reason: "SubmitFollowUp must be sent via follow-up channel".to_string(),
            }
        }
        UserAction::Quit => {
            state.should_quit = true;
            state.push_log(LogLevel::Info, "Quit requested by MCP agent".to_string());
            ActionOutcome::Ok
        }
    }
}

// ---------------------------------------------------------------------------
// Tool implementations
// ---------------------------------------------------------------------------

fn text_tool_result(text: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![Content::text(text.into())])
}

fn text_tool_error(text: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![Content::text(text.into())])
}

fn image_tool_result(text: impl Into<String>, base64_png: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![
        Content::text(text.into()),
        Content::image(base64_png.into(), "image/png"),
    ])
}

fn clamp_shared_view_unit(value: f64) -> f64 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn normalize_shared_view_region(region: SharedViewRegionParams) -> crate::types::SharedViewRegion {
    let x = clamp_shared_view_unit(region.x);
    let y = clamp_shared_view_unit(region.y);
    let width = clamp_shared_view_unit(region.width).min(1.0 - x);
    let height = clamp_shared_view_unit(region.height).min(1.0 - y);
    crate::types::SharedViewRegion {
        x,
        y,
        width,
        height,
    }
}

fn shared_view_display_target(
    display_target: Option<String>,
    display_id: Option<u32>,
) -> Option<String> {
    display_target
        .map(|target| target.trim().to_string())
        .filter(|target| !target.is_empty())
        .or_else(|| display_id.map(|id| format!(":{}", id)))
}

fn shared_view_display_id(display_target: Option<&str>, display_id: Option<u32>) -> Option<u32> {
    if display_id.is_some() {
        return display_id;
    }
    let target = display_target?.trim();
    if target.eq_ignore_ascii_case("user_session") || target.eq_ignore_ascii_case("primary") {
        return Some(0);
    }
    target
        .strip_prefix(':')
        .or_else(|| target.strip_prefix("display_"))
        .unwrap_or(target)
        .parse::<u32>()
        .ok()
}

fn shared_view_target_label(display_id: Option<u32>, display_target: Option<&str>) -> String {
    if let Some(id) = display_id {
        return if id == 0 {
            "primary display".to_string()
        } else {
            format!("display {}", id)
        };
    }
    let Some(target) = display_target
        .map(str::trim)
        .filter(|target| !target.is_empty())
    else {
        return "default display".to_string();
    };
    if target.eq_ignore_ascii_case("user_session")
        || target.eq_ignore_ascii_case("user")
        || target.eq_ignore_ascii_case("primary")
    {
        return "primary display".to_string();
    }
    let parsed = target
        .strip_prefix(':')
        .or_else(|| target.strip_prefix("display_"))
        .unwrap_or(target)
        .parse::<u32>()
        .ok();
    match parsed {
        Some(0) => "primary display".to_string(),
        Some(id) => format!("display {}", id),
        None => target.to_string(),
    }
}

fn shared_view_user_display_id(
    display_target: Option<&str>,
    display_id: Option<u32>,
) -> Option<u32> {
    if let Some(display_id) = display_id {
        return Some(display_id);
    }
    let Some(target) = display_target
        .map(str::trim)
        .filter(|target| !target.is_empty())
    else {
        return Some(0);
    };
    if target.eq_ignore_ascii_case("user_session")
        || target.eq_ignore_ascii_case("user")
        || target.eq_ignore_ascii_case("primary")
        || target == ":0"
        || target == "0"
        || target.eq_ignore_ascii_case("display_0")
    {
        return Some(0);
    }
    None
}

#[tool_router]
impl IntendantServer {
    #[tool(
        description = "Get current status: provider, model, turn, budget, phase, autonomy, verbosity, tokens, and any compact lineage/fission ledger derived from the session log."
    )]
    async fn get_status(&self) -> String {
        self.get_status_for_session(None, None).await
    }

    async fn get_status_for_session(
        &self,
        session_id_override: Option<&str>,
        managed_context_override: Option<bool>,
    ) -> String {
        if let Some(requested_session_id) = session_id_override
            .map(str::trim)
            .filter(|id| !id.is_empty())
        {
            let mut s = self.state.write().await;
            hydrate_requested_session_status_from_logs(&mut s, requested_session_id);
        }
        let s = self.state.read().await;
        let mut snap = s.status_snapshot();
        let log_dir = s.log_dir.clone();
        let session_id = session_id_override
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| s.session_id.clone());
        let session_status =
            session_id_override.and_then(|_| s.session_status_for_id(&session_id).cloned());
        let autonomy = s.autonomy.clone();
        // Fill autonomy from shared state
        drop(s);
        let autonomy_level = autonomy.read().await.level;
        snap.autonomy = autonomy_level.to_string().to_lowercase();
        if let Some(status) = session_status {
            snap.turn = status.turn;
            snap.round = status.round;
            snap.phase = phase_to_str(&status.phase).to_string();
            if !status.task.is_empty() {
                snap.task = status.task;
            }
        }
        let mut value = serde_json::to_value(&snap).unwrap_or_else(|_| serde_json::json!({}));
        if let Some(obj) = value.as_object_mut() {
            let s = self.state.read().await;
            let usage = s.usage_snapshot_for(Some(&session_id));
            obj.insert(
                "session_id".to_string(),
                serde_json::Value::String(session_id.clone()),
            );
            obj.insert(
                "provider".to_string(),
                serde_json::Value::String(usage.main.provider.clone()),
            );
            obj.insert(
                "model".to_string(),
                serde_json::Value::String(usage.main.model.clone()),
            );
            obj.insert(
                "session_tokens".to_string(),
                serde_json::Value::Number(usage.main.tokens_used.into()),
            );
            obj.insert(
                "budget_pct".to_string(),
                serde_json::Number::from_f64(usage.main.usage_pct)
                    .map(serde_json::Value::Number)
                    .unwrap_or(serde_json::Value::Null),
            );
            obj.insert(
                "usage".to_string(),
                serde_json::to_value(usage).unwrap_or_else(|_| serde_json::json!({})),
            );
            obj.insert(
                "context_pressure".to_string(),
                s.context_pressure_snapshot_for(Some(&session_id), managed_context_override),
            );
        }
        if let Ok(Some(ledger)) = crate::lineage_ledger::read_lineage_ledger(&log_dir, &session_id)
        {
            if let Some(obj) = value.as_object_mut() {
                obj.insert(
                    "lineage_ledger".to_string(),
                    serde_json::to_value(ledger).unwrap_or_else(|_| serde_json::json!({})),
                );
            }
        }
        if let Ok(Some(ledger)) =
            crate::fission_ledger::read_fission_ledger_for_session(&log_dir, &session_id)
        {
            if let Some(obj) = value.as_object_mut() {
                obj.insert(
                    "fission_ledger".to_string(),
                    serde_json::to_value(ledger).unwrap_or_else(|_| serde_json::json!({})),
                );
            }
        }
        serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_string())
    }

    #[tool(
        description = "Get log entries. Supports cursor-based pagination via since_id and filtering by level."
    )]
    async fn get_logs(&self, Parameters(params): Parameters<GetLogsParams>) -> String {
        self.get_logs_for_session(params, None).await
    }

    async fn get_logs_for_session(
        &self,
        params: GetLogsParams,
        session_id: Option<&str>,
    ) -> String {
        let target_session_id = params.session_id.as_deref().or(session_id);
        if let Some(entries) = read_persisted_log_entries_for_session(target_session_id, &params) {
            return serde_json::to_string_pretty(&entries).unwrap_or_else(|_| "[]".to_string());
        }

        let s = self.state.read().await;
        let limit = params.limit.unwrap_or(100);
        let entries: Vec<&LogEntrySnapshot> = s
            .log_entries
            .iter()
            .filter(|e| {
                if let Some(since) = params.since_id {
                    if e.id <= since {
                        return false;
                    }
                }
                if let Some(ref filter) = params.level_filter {
                    if e.level != *filter {
                        return false;
                    }
                }
                true
            })
            .take(limit)
            .collect();
        serde_json::to_string_pretty(&entries).unwrap_or_else(|_| "[]".to_string())
    }

    #[tool(
        description = "Get the current pending approval request, if any. Returns null if no approval is pending."
    )]
    async fn get_pending_approval(&self) -> String {
        let s = self.state.read().await;
        match s.approval_snapshot() {
            Some(snap) => {
                serde_json::to_string_pretty(&snap).unwrap_or_else(|_| "null".to_string())
            }
            None => "null".to_string(),
        }
    }

    #[tool(
        description = "Get the current pending human question, if any. Returns null if no question is pending."
    )]
    async fn get_pending_input(&self) -> String {
        let s = self.state.read().await;
        match s.human_question_snapshot() {
            Some(snap) => {
                serde_json::to_string_pretty(&snap).unwrap_or_else(|_| "null".to_string())
            }
            None => "null".to_string(),
        }
    }

    #[tool(
        description = "Approve a pending command execution. Equivalent to pressing 'y' in the TUI."
    )]
    async fn approve(&self, Parameters(params): Parameters<ApproveParams>) -> String {
        let action = UserAction::Approve { id: params.id };
        let mut s = self.state.write().await;
        let outcome = process_action_sync(&mut s, action);
        if outcome == ActionOutcome::Ok {
            self.bus.send(AppEvent::ApprovalResolved {
                session_id: None,
                id: params.id,
                action: "approve".to_string(),
            });
        }
        format_outcome(outcome)
    }

    #[tool(
        description = "Deny a pending command execution. Stops the agent loop. Equivalent to pressing 'n' in the TUI."
    )]
    async fn deny(&self, Parameters(params): Parameters<DenyParams>) -> String {
        let action = UserAction::Deny { id: params.id };
        let mut s = self.state.write().await;
        let outcome = process_action_sync(&mut s, action);
        if outcome == ActionOutcome::Ok {
            self.bus.send(AppEvent::ApprovalResolved {
                session_id: None,
                id: params.id,
                action: "deny".to_string(),
            });
        }
        format_outcome(outcome)
    }

    #[tool(
        description = "Skip a pending command execution. The agent continues with the next command. Equivalent to pressing 's' in the TUI."
    )]
    async fn skip(&self, Parameters(params): Parameters<SkipParams>) -> String {
        let action = UserAction::Skip { id: params.id };
        let mut s = self.state.write().await;
        let outcome = process_action_sync(&mut s, action);
        if outcome == ActionOutcome::Ok {
            self.bus.send(AppEvent::ApprovalResolved {
                session_id: None,
                id: params.id,
                action: "skip".to_string(),
            });
        }
        format_outcome(outcome)
    }

    #[tool(
        description = "Approve this and all future commands (sets autonomy to Full). Equivalent to pressing 'a' in the TUI."
    )]
    async fn approve_all(&self, Parameters(params): Parameters<ApproveAllParams>) -> String {
        let action = UserAction::ApproveAll { id: params.id };
        let mut s = self.state.write().await;
        let outcome = process_action_sync(&mut s, action);
        if outcome == ActionOutcome::Ok {
            self.bus.send(AppEvent::ApprovalResolved {
                session_id: None,
                id: params.id,
                action: "approve_all".to_string(),
            });
            let autonomy = s.autonomy.clone();
            drop(s);
            let mut a = autonomy.write().await;
            a.level = AutonomyLevel::Full;
        }
        format_outcome(outcome)
    }

    #[tool(
        description = "Respond to an askHuman question. Equivalent to typing a response and pressing Enter in the TUI."
    )]
    async fn respond(&self, Parameters(params): Parameters<RespondParams>) -> String {
        let action = UserAction::RespondHuman { text: params.text };
        let mut s = self.state.write().await;
        let outcome = process_action_sync(&mut s, action);
        format_outcome(outcome)
    }

    #[tool(
        description = "Set the autonomy level. Controls how much approval is required. Equivalent to +/- keys in the TUI."
    )]
    async fn set_autonomy(&self, Parameters(params): Parameters<SetAutonomyParams>) -> String {
        let level = AutonomyLevel::from_str_loose(&params.level);
        let s = self.state.read().await;
        let autonomy = s.autonomy.clone();
        drop(s);
        {
            let mut a = autonomy.write().await;
            a.level = level;
        }
        let mut s = self.state.write().await;
        let _ = process_action_sync(&mut s, UserAction::SetAutonomy { level });
        s.push_log(
            LogLevel::Info,
            format!("Autonomy set to {} by MCP agent", level),
        );
        format!("Autonomy set to {}", level)
    }

    #[tool(
        description = "Set log verbosity level. Controls which log entries are shown. Equivalent to pressing 'v' in the TUI."
    )]
    async fn set_verbosity(&self, Parameters(params): Parameters<SetVerbosityParams>) -> String {
        match parse_verbosity(&params.level) {
            Some(level) => {
                let action = UserAction::SetVerbosity { level };
                let mut s = self.state.write().await;
                let outcome = process_action_sync(&mut s, action);
                format_outcome(outcome)
            }
            None => format!(
                "Invalid verbosity level '{}'. Use: quiet, normal, verbose, debug",
                params.level
            ),
        }
    }

    #[tool(
        description = "Shut down the Intendant agent. Equivalent to pressing 'q' or Ctrl-C in the TUI."
    )]
    async fn quit(&self) -> String {
        let action = UserAction::Quit;
        let mut s = self.state.write().await;
        let outcome = process_action_sync(&mut s, action);
        format_outcome(outcome)
    }

    #[tool(
        description = "Start a new task for the Intendant agent to execute. The agent will begin working on the task immediately. Only one task can run at a time — check get_status to see if a task is already running."
    )]
    async fn start_task(&self, Parameters(params): Parameters<StartTaskParams>) -> String {
        let session_id = params
            .session_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string);
        if let Some(session_id) = session_id {
            let target_phase = self.target_session_phase(&session_id).await;
            let target_accepts_follow_up = target_phase
                .as_ref()
                .is_some_and(target_phase_accepts_follow_up);
            if !target_accepts_follow_up
                && params.reference_frame_ids.is_empty()
                && params.display_target.is_none()
            {
                match resolve_persisted_start_target(&session_id) {
                    PersistedStartTarget::External(target) => {
                        self.bus
                            .send(AppEvent::ControlCommand(ControlMsg::ResumeSession {
                                source: target.source.clone(),
                                session_id: session_id.clone(),
                                resume_id: Some(target.resume_id.clone()),
                                project_root: target.project_root,
                                task: Some(params.task),
                                direct: params.orchestrate.map(|orchestrate| !orchestrate),
                                attachments: vec![],
                                agent_command: target.agent_command,
                                codex_sandbox: target.codex_sandbox,
                                codex_approval_policy: target.codex_approval_policy,
                                codex_managed_context: target.codex_managed_context,
                                codex_context_archive: target.codex_context_archive,
                            }));
                        return format!(
                            "ok (session resume dispatched for {} {})",
                            target.source, target.resume_id
                        );
                    }
                    PersistedStartTarget::ExternalMissingResume { source } => {
                        let source = source.unwrap_or_else(|| "external-agent".to_string());
                        return format!(
                            "Cannot start task: session {} is a persisted {} wrapper, but its backend resume id was not found; use dashboard Resume with an explicit source/resume_id or restart with saved config",
                            session_id, source
                        );
                    }
                    PersistedStartTarget::NonExternal if !target_accepts_follow_up => {
                        return format!(
                            "Cannot start task: session {} is not active in this daemon and is not a persisted external-agent wrapper; use resume/restart from the dashboard or start a new session",
                            session_id
                        );
                    }
                    PersistedStartTarget::NotFound | PersistedStartTarget::NonExternal => {}
                }
            }
            if let Some(phase) = target_phase
                .as_ref()
                .filter(|phase| !target_phase_accepts_follow_up(phase))
            {
                return format!(
                    "Cannot start task: session {} is not active (phase {}); use restart/resume before sending a follow-up",
                    session_id,
                    phase_to_str(&phase)
                );
            }
            self.bus
                .send(AppEvent::ControlCommand(ControlMsg::StartTask {
                    session_id: Some(session_id.clone()),
                    task: params.task,
                    orchestrate: params.orchestrate,
                    direct: None,
                    reference_frame_ids: params.reference_frame_ids,
                    display_target: params.display_target,
                    attachments: vec![],
                    follow_up_id: None,
                }));
            if target_phase
                .as_ref()
                .is_some_and(target_phase_is_active_turn)
            {
                let source = self.target_session_source(&session_id).await;
                if source
                    .as_deref()
                    .is_some_and(|source| source.eq_ignore_ascii_case("codex"))
                {
                    return "ok (follow-up queued for next turn; active Codex turn is still running)"
                        .to_string();
                }
                return "ok (follow-up queued for next turn; active turn is still running)"
                    .to_string();
            }
            return "ok (task dispatched)".to_string();
        }

        // If reference_frame_ids are present, dispatch as a CU task via ControlMsg
        // so the main loop can route it to the ephemeral CU runner.
        if !params.reference_frame_ids.is_empty() || params.display_target.is_some() {
            self.bus
                .send(AppEvent::ControlCommand(ControlMsg::StartTask {
                    session_id: None,
                    task: params.task,
                    orchestrate: params.orchestrate,
                    direct: None,
                    reference_frame_ids: params.reference_frame_ids,
                    display_target: params.display_target,
                    attachments: vec![],
                    follow_up_id: None,
                }));
            return "ok (CU task dispatched)".to_string();
        }
        match self
            .start_task_internal(params.task, "MCP", params.orchestrate)
            .await
        {
            Ok(()) => "ok".to_string(),
            Err(e) => format!("Cannot start task: {}", e),
        }
    }

    async fn target_session_phase(&self, session_id: &str) -> Option<Phase> {
        let s = self.state.read().await;
        s.session_status_for_id(session_id)
            .map(|status| status.phase.clone())
            .or_else(|| (s.session_id == session_id).then(|| s.phase.clone()))
    }

    async fn target_session_source(&self, session_id: &str) -> Option<String> {
        let s = self.state.read().await;
        s.session_source_for_id(session_id)
            .map(str::to_string)
            .or_else(|| {
                (s.session_id == session_id)
                    .then(|| s.active_session_source.clone())
                    .flatten()
            })
    }
}

fn target_phase_accepts_follow_up(phase: &Phase) -> bool {
    !matches!(phase, Phase::Idle | Phase::Done | Phase::Interrupted)
}

fn target_phase_is_active_turn(phase: &Phase) -> bool {
    matches!(
        phase,
        Phase::Thinking
            | Phase::RunningAgent
            | Phase::Orchestrating
            | Phase::WaitingApproval
            | Phase::WaitingHuman
            | Phase::Interrupting
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PersistedExternalStartTarget {
    source: String,
    resume_id: String,
    project_root: Option<String>,
    agent_command: Option<String>,
    codex_sandbox: Option<String>,
    codex_approval_policy: Option<String>,
    codex_managed_context: Option<String>,
    codex_context_archive: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PersistedStartTarget {
    NotFound,
    NonExternal,
    External(PersistedExternalStartTarget),
    ExternalMissingResume { source: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExternalIdentity {
    wrapper_id: Option<String>,
    source: String,
    resume_id: String,
}

fn resolve_persisted_start_target(session_id: &str) -> PersistedStartTarget {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return PersistedStartTarget::NotFound;
    }
    let Some(log_dir) = crate::session_log::SessionLog::find_session_by_id(session_id) else {
        return PersistedStartTarget::NotFound;
    };

    let (canonical_session_id, project_root) = persisted_session_meta(&log_dir);
    let config = crate::session_config::read_log_dir_config(&log_dir);
    let mut source = config
        .as_ref()
        .and_then(|config| config.source.as_deref())
        .and_then(normalized_external_source);
    let mut resume_id = None;

    let contents = std::fs::read_to_string(log_dir.join("session.jsonl")).unwrap_or_default();
    let mut exact_identity = None;
    let mut any_identity = None;
    let mut identity_count = 0usize;
    for line in contents.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let event = value.get("event").and_then(|v| v.as_str()).unwrap_or("");
        let message = value.get("message").and_then(|v| v.as_str()).unwrap_or("");
        if event == "session_identity" {
            if let Some(identity) = persisted_external_identity_from_event(&value) {
                identity_count += 1;
                if identity_matches_requested_wrapper(
                    identity.wrapper_id.as_deref(),
                    session_id,
                    canonical_session_id.as_deref(),
                ) {
                    exact_identity = Some(identity.clone());
                }
                if any_identity.is_none() {
                    any_identity = Some(identity);
                }
            }
        }
        if source.is_none() {
            source = external_agent_source_from_log_message(message);
        }
        if resume_id.is_none() {
            resume_id = external_agent_resume_id_from_log_message(message);
        }
    }

    let identity = exact_identity.or_else(|| (identity_count == 1).then(|| any_identity).flatten());
    if let Some(identity) = identity {
        source = Some(identity.source);
        resume_id = Some(identity.resume_id);
    }

    let Some(source) = source else {
        return PersistedStartTarget::NonExternal;
    };
    let Some(resume_id) = resume_id else {
        return PersistedStartTarget::ExternalMissingResume {
            source: Some(source),
        };
    };

    PersistedStartTarget::External(PersistedExternalStartTarget {
        source,
        resume_id,
        project_root,
        agent_command: config
            .as_ref()
            .and_then(|config| config.agent_command.clone()),
        codex_sandbox: config
            .as_ref()
            .and_then(|config| config.codex_sandbox.clone()),
        codex_approval_policy: config
            .as_ref()
            .and_then(|config| config.codex_approval_policy.clone()),
        codex_managed_context: config
            .as_ref()
            .and_then(|config| config.codex_managed_context.clone()),
        codex_context_archive: config
            .as_ref()
            .and_then(|config| config.codex_context_archive.clone()),
    })
}

fn persisted_session_meta(log_dir: &std::path::Path) -> (Option<String>, Option<String>) {
    let raw = std::fs::read_to_string(log_dir.join("session_meta.json")).ok();
    let value = raw.and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok());
    let session_id = value
        .as_ref()
        .and_then(|value| json_str_field(value, "session_id"));
    let project_root = value
        .as_ref()
        .and_then(|value| json_str_field(value, "project_root"));
    (session_id, project_root)
}

fn persisted_external_identity_from_event(value: &serde_json::Value) -> Option<ExternalIdentity> {
    let data = value.get("data")?;
    let source =
        json_str_field(data, "source").and_then(|source| normalized_external_source(&source))?;
    let resume_id =
        json_str_field(data, "backend_session_id").and_then(|id| clean_external_resume_id(&id))?;
    Some(ExternalIdentity {
        wrapper_id: json_str_field(data, "session_id"),
        source,
        resume_id,
    })
}

fn identity_matches_requested_wrapper(
    identity_session_id: Option<&str>,
    requested_id: &str,
    canonical_session_id: Option<&str>,
) -> bool {
    let Some(identity_session_id) = identity_session_id
        .map(str::trim)
        .filter(|id| !id.is_empty())
    else {
        return false;
    };
    identity_session_id == requested_id
        || identity_session_id.starts_with(requested_id)
        || canonical_session_id
            .map(|canonical| {
                identity_session_id == canonical || canonical.starts_with(requested_id)
            })
            .unwrap_or(false)
}

fn normalized_external_source(source: &str) -> Option<String> {
    let normalized = crate::session_names::normalize_source(source);
    crate::external_agent::AgentBackend::from_str_loose(&normalized)
        .map(|backend| backend.as_short_str().to_string())
}

fn json_str_field(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn external_agent_source_from_log_message(message: &str) -> Option<String> {
    let mode = message.strip_prefix("Mode: external agent (")?;
    let (source, _) = mode.split_once(')')?;
    normalized_external_source(source)
}

fn external_agent_resume_id_from_log_message(message: &str) -> Option<String> {
    if let Some(thread_id) = message.strip_prefix("External agent thread: ") {
        return clean_external_resume_id(thread_id);
    }
    if message.starts_with("Mode: external agent") {
        if let Some((_, thread_id)) = message.rsplit_once("thread: ") {
            return clean_external_resume_id(thread_id);
        }
    }
    None
}

fn clean_external_resume_id(value: &str) -> Option<String> {
    let value = value
        .trim()
        .trim_matches(|c: char| c.is_whitespace() || matches!(c, '`' | '"' | '\'' | ',' | ';'));
    (!value.is_empty()).then(|| value.to_string())
}

impl IntendantServer {
    #[tool(
        description = "Schedule a Codex context rewind to an exact item/tool-call anchor. Use only after managed-context recovery/density handoff guidance, rewind-only context pressure, a watch-pressure density decision, or genuinely noisy/unexpectedly large recent output makes a rewind necessary; do not use for ordinary low-pressure startup/search work. First call list_rewind_anchors and choose one returned item_id; call inspect_rewind_anchor when the compact row is ambiguous. Do not synthesize anchor ids from prior failed tool calls. The current turn will finish, Intendant will roll back Codex to the anchor, inject the primer as developer context, and resume the branch."
    )]
    async fn rewind_context(&self, Parameters(params): Parameters<RewindContextParams>) -> String {
        let reason = params.reason.trim();
        if reason.is_empty() {
            return "rewind_context requires a non-empty reason".to_string();
        }
        let primer = params.primer.trim();
        if primer.is_empty() {
            return "rewind_context requires a non-empty primer".to_string();
        }
        let item_id = params.anchor.item_id.trim();
        if item_id.is_empty() {
            return "rewind_context anchor.item_id must not be empty".to_string();
        }
        // Normalize case to match the action layer (RollbackAnchorPosition::from_str
        // lowercases), so `After`/`BEFORE` are accepted consistently end-to-end.
        let position = params.anchor.position.trim().to_ascii_lowercase();
        if !matches!(position.as_str(), "before" | "after") {
            return "rewind_context anchor.position must be `before` or `after`".to_string();
        }

        self.dispatch_codex_thread_action_and_wait(
            params.session_id.clone(),
            "rewind_context".to_string(),
            serde_json::json!({
                "anchor": {
                    "item_id": item_id,
                    "position": position,
                },
                "reason": reason,
                "primer": primer,
                "preserve": params.preserve,
                "discard": params.discard,
                "artifacts": params.artifacts,
                "next_steps": params.next_steps,
            }),
            "rewind_context dispatched but no validation result was observed".to_string(),
        )
        .await
    }

    #[tool(
        description = "Discover exact Codex rewind anchors only after you have already decided a managed-context rewind may be needed because recovery/density handoff guidance asked for it, context pressure is rewind-only or watch, or a recent completed tool result was genuinely noisy/unexpectedly large. Do not call during ordinary startup/status/search turns merely because managed_context=managed is enabled, or after bounded low-output searches while context_pressure.status is ok. By default returns one compact whole-catalog result covering all matching valid non-management anchors, with exact item_id values, accepted positions, item type/name/role, and short semantic summaries; it does not select or recommend one anchor. Use query or reverse only when you already have a semantic filter/order in mind. Use detail=true or explicit offset/limit for diagnostic detailed pages, and inspect_rewind_anchor for one anchor's before/after context. For density compaction, include_pruning_estimates=true adds approximate discard sizes to compact rows. The default catalog hides managed-context maintenance calls such as list_rewind_anchors, inspect_rewind_anchor, rewind_context, and rewind_backout so recovery does not target its own tool calls; pass include_management_tools=true only when intentionally targeting those internals. Normal model-facing results hide anchors known to remain at/above the rewind-only limit or without enough resume headroom, and narrow positions to values accepted by rewind_context; recovery_candidates_only=false alone is ignored. Pass include_non_recovery=true only for diagnostics/audit, and never pass a recovery_eligible=false audit row to rewind_context. Use inspect_rewind_anchor on a candidate when the compact summary is ambiguous, then copy the chosen item_id and position_hint, or a value in positions, into rewind_context."
    )]
    async fn list_rewind_anchors(
        &self,
        Parameters(params): Parameters<ListRewindAnchorsParams>,
    ) -> String {
        let state = self.state.read().await;
        let recovery_candidates_only = state.rewind_anchor_recovery_candidates_only_for(
            params.session_id.as_deref(),
            params.recovery_candidates_only,
            params.include_non_recovery,
        );
        drop(state);
        let mut payload = serde_json::json!({
            "offset": params.offset.unwrap_or(0),
            "reverse": params.reverse,
            "include_management_tools": params.include_management_tools,
            "recovery_candidates_only": recovery_candidates_only,
            "include_non_recovery": params.include_non_recovery,
            "compact_catalog": !params.detail && params.offset.is_none() && params.limit.is_none() && !params.include_non_recovery,
        });
        if let Some(limit) = params.limit {
            if let Some(obj) = payload.as_object_mut() {
                obj.insert(
                    "limit".to_string(),
                    serde_json::Value::Number(serde_json::Number::from(limit)),
                );
            }
        }
        if params.include_pruning_estimates {
            if let Some(obj) = payload.as_object_mut() {
                obj.insert(
                    "include_pruning_estimates".to_string(),
                    serde_json::Value::Bool(true),
                );
            }
        }
        if params.detail {
            if let Some(obj) = payload.as_object_mut() {
                obj.insert("detail".to_string(), serde_json::Value::Bool(true));
            }
        }
        if let Some(query) = params
            .query
            .as_deref()
            .map(str::trim)
            .filter(|query| !query.is_empty())
        {
            if let Some(obj) = payload.as_object_mut() {
                obj.insert(
                    "query".to_string(),
                    serde_json::Value::String(query.to_string()),
                );
            }
        }
        self.dispatch_codex_thread_action_and_wait(
            params.session_id.clone(),
            "list_rewind_anchors".to_string(),
            payload,
            "ok (managed-context rewind anchor listing dispatched)".to_string(),
        )
        .await
    }

    #[tool(
        description = "Inspect a single exact Codex rewind anchor with a compact before/after context window. Use only after list_rewind_anchors returns a candidate for an already-needed rewind, when the row is too lossy to choose safely."
    )]
    async fn inspect_rewind_anchor(
        &self,
        Parameters(params): Parameters<InspectRewindAnchorParams>,
    ) -> String {
        let item_id = params.item_id.trim();
        if item_id.is_empty() {
            return "inspect_rewind_anchor item_id must not be empty".to_string();
        }
        let mut payload = serde_json::json!({
            "anchor": {
                "item_id": item_id,
            },
            "radius": params.radius.unwrap_or(2),
        });
        if let Some(obj) = payload.as_object_mut() {
            if let Some(session_id) = params
                .session_id
                .as_deref()
                .map(str::trim)
                .filter(|session_id| !session_id.is_empty())
            {
                obj.insert(
                    "session_id".to_string(),
                    serde_json::Value::String(session_id.to_string()),
                );
            }
        }
        self.dispatch_codex_thread_action_and_wait(
            params.session_id.clone(),
            "inspect_rewind_anchor".to_string(),
            payload,
            "ok (managed-context rewind anchor inspection dispatched)".to_string(),
        )
        .await
    }

    #[tool(
        description = "Recover a prior context rewind record. mode=\"inspect\" reports the saved pre-rewind rollout path. mode=\"restore\" restores the active Codex thread in place. mode=\"fork\"/\"backout\" creates a new Codex thread that inherits the lineage prompt-cache key when using the patched managed Codex binary."
    )]
    async fn rewind_backout(&self, Parameters(params): Parameters<RewindBackoutParams>) -> String {
        let record_id = params.record_id.trim();
        if record_id.is_empty() {
            return "rewind_backout requires a non-empty record_id".to_string();
        }
        let mode = params
            .mode
            .as_deref()
            .map(str::trim)
            .filter(|mode| !mode.is_empty())
            .unwrap_or("inspect");
        if !matches!(mode, "inspect" | "fork" | "backout" | "restore") {
            return "rewind_backout mode must be `inspect`, `fork`, `backout`, or `restore`"
                .to_string();
        }
        let name = params
            .name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty());
        let mut payload = serde_json::json!({
            "record_id": record_id,
            "mode": mode,
            "allow_cache_reset": params.allow_cache_reset,
        });
        if let Some(name) = name {
            if let Some(obj) = payload.as_object_mut() {
                obj.insert(
                    "name".to_string(),
                    serde_json::Value::String(name.to_string()),
                );
            }
        }

        let timeout_message = if mode == "inspect" {
            "ok (managed-context rewind record inspection dispatched)".to_string()
        } else if mode == "restore" {
            "ok (same-thread managed-context restore dispatched)".to_string()
        } else {
            "ok (managed-context lineage fork dispatched)".to_string()
        };
        self.dispatch_codex_thread_action_and_wait(
            params.session_id.clone(),
            "rewind_backout".to_string(),
            payload,
            timeout_message,
        )
        .await
    }

    #[tool(
        description = "Claim a fission group's canonical branch. Omit expected_canonical_session_id for first-writer-wins; provide it to deliberately compare-and-swap from the current canonical branch."
    )]
    async fn claim_fission_canonical(
        &self,
        Parameters(params): Parameters<ClaimFissionCanonicalParams>,
    ) -> String {
        let group_id = params.group_id.trim();
        if group_id.is_empty() {
            return "claim_fission_canonical requires a non-empty group_id".to_string();
        }
        let branch_session_id = params.branch_session_id.trim();
        if branch_session_id.is_empty() {
            return "claim_fission_canonical requires a non-empty branch_session_id".to_string();
        }
        let expected = params
            .expected_canonical_session_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let log_dir = self.state.read().await.log_dir.clone();
        match crate::fission_ledger::claim_canonical(
            &log_dir,
            group_id,
            branch_session_id,
            expected,
        ) {
            Ok(group) => serde_json::to_string_pretty(&group)
                .unwrap_or_else(|_| "ok (canonical branch claimed)".to_string()),
            Err(err) => format!("claim_fission_canonical failed: {err}"),
        }
    }

    #[tool(
        description = "Schedule a controller restart workflow. Returns a restart ID and a completion token that must be passed to controller_turn_complete as the final controller action."
    )]
    async fn schedule_controller_restart(
        &self,
        Parameters(mut params): Parameters<ScheduleControllerRestartParams>,
    ) -> String {
        normalize_schedule_controller_restart_params(&mut params);
        if let Err(e) = validate_schedule_controller_restart_params(&params) {
            return schedule_error_response(e, None, None);
        }

        let restart = {
            let mut s = self.state.write().await;
            if let Some(active) = s.controller_restart.as_ref() {
                if matches!(
                    active.phase,
                    RestartPhase::AwaitingTurnComplete
                        | RestartPhase::Ready
                        | RestartPhase::Restarting
                ) {
                    return schedule_error_response(
                        format!(
                            "A restart is already active (id={}, phase={:?})",
                            active.restart_id, active.phase
                        ),
                        Some(active.restart_id.as_str()),
                        Some(active.phase),
                    );
                }
            }

            let restart = ControllerRestartState::new(&params);
            s.push_log(
                LogLevel::Info,
                format!(
                    "Controller restart scheduled for '{}' (id={})",
                    restart.controller_id, restart.restart_id
                ),
            );
            s.controller_restart = Some(restart.clone());
            persist_restart_state(&s.log_dir, &s.controller_restart);
            restart
        };

        let mut output = serde_json::json!({
            "status": "scheduled",
            "restart_id": restart.restart_id,
            "turn_complete_token": restart.turn_complete_token,
            "ok": true,
        });
        let mut command_ok = true;

        if matches!(restart.restart_after, RestartAfter::Now) {
            match self.run_scheduled_controller_restart().await {
                Ok(result) => {
                    output["execution"] = serde_json::Value::String(if result.is_empty() {
                        "ok".to_string()
                    } else {
                        result
                    });
                }
                Err(e) => {
                    command_ok = false;
                    output["execution_error"] = serde_json::Value::String(e);
                }
            }
        }
        output["ok"] = serde_json::Value::Bool(command_ok);
        let phase = {
            let s = self.state.read().await;
            s.controller_restart
                .as_ref()
                .map(restart_phase_value)
                .unwrap_or_else(|| {
                    serde_json::to_value(restart.phase).unwrap_or(serde_json::Value::Null)
                })
        };
        output["phase"] = phase;

        serde_json::to_string_pretty(&output).unwrap_or_else(|_| "{}".to_string())
    }

    #[tool(
        description = "Final handshake call from the controlling agent before ending its turn. Validates token and executes any pending scheduled restart."
    )]
    async fn controller_turn_complete(
        &self,
        Parameters(mut params): Parameters<ControllerTurnCompleteParams>,
    ) -> String {
        normalize_controller_turn_complete_params(&mut params);
        {
            let mut s = self.state.write().await;
            let log_dir = s.log_dir.clone();
            let Some(active) = s.controller_restart.as_mut() else {
                return restart_error_response(
                    "rejected",
                    &params.restart_id,
                    None,
                    "No controller restart is scheduled".to_string(),
                );
            };

            if active.restart_id != params.restart_id {
                return restart_error_response(
                    "rejected",
                    &params.restart_id,
                    Some(active.phase),
                    "restart_id does not match the active restart".to_string(),
                );
            }
            if active.turn_complete_token != params.turn_complete_token {
                return restart_error_response(
                    "rejected",
                    &params.restart_id,
                    Some(active.phase),
                    "turn_complete_token is invalid".to_string(),
                );
            }
            if !matches!(active.phase, RestartPhase::AwaitingTurnComplete) {
                return restart_error_response(
                    "rejected",
                    &params.restart_id,
                    Some(active.phase),
                    format!(
                        "Restart is not awaiting completion (phase={:?})",
                        active.phase
                    ),
                );
            }

            active.handoff_summary = params.handoff_summary.clone();
            active.completion_status = params.status.clone();
            active.phase = RestartPhase::Ready;
            active.updated_at = ControllerRestartState::now_string();
            let restart_id = active.restart_id.clone();
            let snapshot = s.controller_restart.clone();
            persist_restart_state(&log_dir, &snapshot);
            s.push_log(
                LogLevel::Info,
                format!("Controller turn complete acknowledged (id={})", restart_id),
            );
        }

        match self.run_scheduled_controller_restart().await {
            Ok(result) => {
                let mut output = serde_json::json!({
                    "status": "completed",
                    "restart_id": params.restart_id,
                    "ok": true,
                });
                output["execution"] = serde_json::Value::String(if result.is_empty() {
                    "ok".to_string()
                } else {
                    result
                });
                let phase = {
                    let s = self.state.read().await;
                    s.controller_restart
                        .as_ref()
                        .map(restart_phase_value)
                        .unwrap_or(serde_json::Value::Null)
                };
                output["phase"] = phase;
                serde_json::to_string_pretty(&output).unwrap_or_else(|_| "{}".to_string())
            }
            Err(e) => {
                let phase = {
                    let s = self.state.read().await;
                    s.controller_restart.as_ref().map(|r| r.phase)
                };
                restart_error_response("restart_pending", &params.restart_id, phase, e)
            }
        }
    }

    #[tool(
        description = "Get the current controller restart state, if any. Returns null when no restart is tracked."
    )]
    async fn get_restart_status(&self) -> String {
        let s = self.state.read().await;
        let value = restart_state_public_value(s.controller_restart.as_ref());
        serde_json::to_string_pretty(&value).unwrap_or_else(|_| "null".to_string())
    }

    #[tool(description = "Cancel a scheduled controller restart.")]
    async fn cancel_controller_restart(
        &self,
        Parameters(mut params): Parameters<CancelControllerRestartParams>,
    ) -> String {
        normalize_cancel_controller_restart_params(&mut params);
        let mut s = self.state.write().await;
        let log_dir = s.log_dir.clone();
        let requested_restart_id = params.restart_id.as_deref();
        let Some(active) = s.controller_restart.as_mut() else {
            return schedule_error_response(
                "No controller restart is scheduled".to_string(),
                requested_restart_id,
                None,
            );
        };

        if let Some(expected_id) = requested_restart_id {
            if expected_id != active.restart_id {
                return schedule_error_response(
                    format!(
                        "restart_id '{}' does not match active '{}'",
                        expected_id, active.restart_id
                    ),
                    Some(active.restart_id.as_str()),
                    Some(active.phase),
                );
            }
        }

        active.phase = RestartPhase::Cancelled;
        active.updated_at = ControllerRestartState::now_string();
        active.last_result = Some("Cancelled by operator".to_string());
        let restart_id = active.restart_id.clone();
        let snapshot = s.controller_restart.clone();
        persist_restart_state(&log_dir, &snapshot);
        s.push_log(
            LogLevel::Info,
            format!("Controller restart cancelled (id={})", restart_id),
        );
        serde_json::json!({
            "status": "cancelled",
            "ok": true,
            "restart_id": restart_id,
            "phase": RestartPhase::Cancelled,
        })
        .to_string()
    }

    #[tool(
        description = "Request graceful controller-loop halt. By default this blocks all future cycles until cleared; set persistent=false for one-shot halt-after-cycle behavior."
    )]
    async fn request_controller_loop_halt(
        &self,
        Parameters(params): Parameters<RequestControllerLoopHaltParams>,
    ) -> String {
        let loop_dir = controller_loop_dir();
        let persistent = params.persistent.unwrap_or(true);
        if let Err(e) = request_loop_halt_marker(&loop_dir, persistent) {
            return serde_json::json!({
                "ok": false,
                "error": e,
            })
            .to_string();
        }
        collect_controller_loop_status(&loop_dir).to_string()
    }

    #[tool(description = "Clear controller-loop halt flags so future cycles may start again.")]
    async fn clear_controller_loop_halt(&self) -> String {
        let loop_dir = controller_loop_dir();
        if let Err(e) = clear_loop_halt_markers(&loop_dir) {
            return serde_json::json!({
                "ok": false,
                "error": e,
            })
            .to_string();
        }
        collect_controller_loop_status(&loop_dir).to_string()
    }

    #[tool(
        description = "Intervene in the active controller loop: mode='stop' requests graceful stop; mode='abort' requests immediate kill."
    )]
    async fn intervene_controller_loop(
        &self,
        Parameters(params): Parameters<InterveneControllerLoopParams>,
    ) -> String {
        let loop_dir = controller_loop_dir();
        match request_loop_intervention_marker(&loop_dir, &params.mode) {
            Ok(intervention) => {
                let mut status = collect_controller_loop_status(&loop_dir);
                add_controller_loop_intervention_report(&mut status, &intervention);
                serde_json::json!({
                    "ok": true,
                    "mode": intervention.mode.as_str(),
                    "intervention": controller_loop_intervention_report(&intervention),
                    "status": status,
                })
                .to_string()
            }
            Err(e) => serde_json::json!({
                "ok": false,
                "error": e,
            })
            .to_string(),
        }
    }

    #[tool(
        description = "Get normalized controller-loop health: latest run pointers, halt/intervention flags, lock owner, and active wrapper/codex PID counts."
    )]
    async fn get_controller_loop_status(&self) -> String {
        collect_controller_loop_status_with_state(&controller_loop_dir(), &self.state)
            .await
            .to_string()
    }

    #[tool(
        description = "List browser workspace provider availability for local semantic browser control and streamed fallback."
    )]
    async fn browser_workspace_providers(&self) -> String {
        let providers = crate::browser_workspace::provider_statuses().await;
        serde_json::to_string_pretty(&providers).unwrap_or_else(|_| "[]".to_string())
    }

    #[tool(
        description = "List active browser workspaces. Browser workspaces are addressable CDP/Playwright/Agent Browser surfaces with per-workspace leases."
    )]
    async fn list_browser_workspaces(&self) -> String {
        let workspaces = crate::browser_workspace::list_workspaces().await;
        serde_json::to_string_pretty(&workspaces).unwrap_or_else(|_| "[]".to_string())
    }

    #[tool(
        description = "Create a browser workspace. provider=cdp launches a managed local Chromium-family browser with an isolated profile and CDP endpoint; provider=system_cdp deliberately uses the installed system browser."
    )]
    async fn create_browser_workspace(
        &self,
        Parameters(params): Parameters<CreateBrowserWorkspaceParams>,
    ) -> String {
        let request = crate::browser_workspace::CreateBrowserWorkspaceRequest {
            url: params.url,
            label: params.label,
            provider: params.provider,
            peer_id: params.peer_id,
            owner_session_id: params.owner_session_id,
            profile_dir: params.profile_dir,
        };
        match crate::browser_workspace::create_workspace(request).await {
            Ok(workspace) => {
                self.bus.send(AppEvent::BrowserWorkspaceChanged {
                    kind: "created".to_string(),
                    workspace: Some(workspace.clone()),
                    workspace_id: Some(workspace.id.clone()),
                    message: None,
                });
                serde_json::to_string_pretty(&workspace).unwrap_or_else(|_| "{}".to_string())
            }
            Err(err) => {
                let message = err.to_string();
                self.bus.send(AppEvent::BrowserWorkspaceChanged {
                    kind: "error".to_string(),
                    workspace: None,
                    workspace_id: None,
                    message: Some(message.clone()),
                });
                serde_json::json!({ "ok": false, "error": message }).to_string()
            }
        }
    }

    #[tool(
        description = "Close a browser workspace and terminate its owned browser process tree when Intendant launched it."
    )]
    async fn close_browser_workspace(
        &self,
        Parameters(params): Parameters<CloseBrowserWorkspaceParams>,
    ) -> String {
        match crate::browser_workspace::close_workspace(&params.workspace_id, params.reason).await {
            Ok(workspace) => {
                self.bus.send(AppEvent::BrowserWorkspaceChanged {
                    kind: "closed".to_string(),
                    workspace_id: Some(workspace.id.clone()),
                    workspace: Some(workspace.clone()),
                    message: None,
                });
                serde_json::to_string_pretty(&workspace).unwrap_or_else(|_| "{}".to_string())
            }
            Err(err) => {
                let message = err.to_string();
                self.bus.send(AppEvent::BrowserWorkspaceChanged {
                    kind: "error".to_string(),
                    workspace: None,
                    workspace_id: Some(params.workspace_id),
                    message: Some(message.clone()),
                });
                serde_json::json!({ "ok": false, "error": message }).to_string()
            }
        }
    }

    #[tool(
        description = "Acquire the exclusive control lease for a browser workspace. Use force=true only when intentionally taking over from another holder."
    )]
    async fn acquire_browser_workspace(
        &self,
        Parameters(params): Parameters<AcquireBrowserWorkspaceParams>,
    ) -> String {
        let request = crate::browser_workspace::AcquireBrowserWorkspaceRequest {
            workspace_id: params.workspace_id,
            holder_id: params.holder_id,
            holder_kind: params.holder_kind,
            note: params.note,
            force: params.force,
        };
        match crate::browser_workspace::acquire_workspace(request).await {
            Ok(workspace) => {
                self.bus.send(AppEvent::BrowserWorkspaceChanged {
                    kind: "lease_acquired".to_string(),
                    workspace_id: Some(workspace.id.clone()),
                    workspace: Some(workspace.clone()),
                    message: None,
                });
                serde_json::to_string_pretty(&workspace).unwrap_or_else(|_| "{}".to_string())
            }
            Err(err) => serde_json::json!({ "ok": false, "error": err.to_string() }).to_string(),
        }
    }

    #[tool(description = "Release a browser workspace control lease.")]
    async fn release_browser_workspace(
        &self,
        Parameters(params): Parameters<ReleaseBrowserWorkspaceParams>,
    ) -> String {
        let request = crate::browser_workspace::ReleaseBrowserWorkspaceRequest {
            workspace_id: params.workspace_id,
            holder_id: params.holder_id,
            note: params.note,
        };
        match crate::browser_workspace::release_workspace(request).await {
            Ok(workspace) => {
                self.bus.send(AppEvent::BrowserWorkspaceChanged {
                    kind: "lease_released".to_string(),
                    workspace_id: Some(workspace.id.clone()),
                    workspace: Some(workspace.clone()),
                    message: None,
                });
                serde_json::to_string_pretty(&workspace).unwrap_or_else(|_| "{}".to_string())
            }
            Err(err) => serde_json::json!({ "ok": false, "error": err.to_string() }).to_string(),
        }
    }

    #[tool(description = "Enumerate available displays with their IDs, names, and resolutions.")]
    async fn list_displays(&self) -> String {
        let session_registry = self.state.read().await.session_registry.clone();
        let displays = crate::display::enumerate_displays_with_sessions(&session_registry).await;
        serde_json::to_string_pretty(&displays).unwrap_or_else(|_| "[]".to_string())
    }

    #[tool(
        description = "Signal that you are using a display. Optional — notifies the dashboard UI but is NOT required before taking screenshots or executing CU actions."
    )]
    async fn take_display(&self, Parameters(params): Parameters<TakeDisplayParams>) -> String {
        self.bus.send(AppEvent::DisplayTaken {
            display_id: params.display_id,
        });
        format!("Took control of :{}", params.display_id)
    }

    #[tool(description = "Release control of a virtual display.")]
    async fn release_display(
        &self,
        Parameters(params): Parameters<ReleaseDisplayParams>,
    ) -> String {
        self.bus.send(AppEvent::DisplayReleased {
            display_id: params.display_id,
            note: params.note.clone(),
        });
        format!("Released control of :{}", params.display_id)
    }

    async fn emit_shared_view(
        &self,
        session_id: Option<&str>,
        action: &str,
        display_target: Option<String>,
        display_id: Option<u32>,
        reason: Option<String>,
        region: Option<crate::types::SharedViewRegion>,
        note: Option<String>,
    ) -> String {
        self.bus.send(AppEvent::SharedView {
            session_id: session_id
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_string),
            action: action.to_string(),
            display_target: display_target.clone(),
            display_id,
            reason: reason.clone(),
            region,
            note: note.clone(),
        });
        let target = shared_view_target_label(display_id, display_target.as_deref());
        let detail = reason
            .or(note)
            .filter(|s| !s.trim().is_empty())
            .map(|s| format!(" ({})", s))
            .unwrap_or_default();
        format!("shared view {} requested for {}{}", action, target, detail)
    }

    async fn ensure_shared_view_display_active(
        &self,
        display_target: Option<&str>,
        display_id: Option<u32>,
    ) {
        let Some(display_id) = shared_view_user_display_id(display_target, display_id) else {
            return;
        };

        let (autonomy, session_registry) = {
            let state = self.state.read().await;
            (state.autonomy.clone(), state.session_registry.clone())
        };
        if let Some(registry) = session_registry {
            if registry.read().await.get(display_id).is_some() {
                return;
            }
        }

        {
            let mut guard = autonomy.write().await;
            guard.user_display_granted = true;
        }
        std::env::set_var("INTENDANT_USER_DISPLAY_GRANTED", "1");
        self.bus.send(AppEvent::UserDisplayGranted { display_id });
    }

    async fn show_shared_view_for_session(
        &self,
        params: ShowSharedViewParams,
        session_id: Option<&str>,
    ) -> String {
        let display_target = shared_view_display_target(params.display_target, params.display_id);
        let display_id = shared_view_display_id(display_target.as_deref(), params.display_id);
        let region = params.focus_region.map(normalize_shared_view_region);
        self.ensure_shared_view_display_active(display_target.as_deref(), display_id)
            .await;
        self.emit_shared_view(
            session_id,
            "show",
            display_target,
            display_id,
            params.reason,
            region,
            None,
        )
        .await
    }

    #[tool(
        description = "Open the dashboard shared display view for agent-human collaboration. For user_session / primary-display targets, this also requests display-stream activation. This does not grant input authority; it asks connected dashboards to show the relevant display and optional focus region."
    )]
    async fn show_shared_view(
        &self,
        Parameters(params): Parameters<ShowSharedViewParams>,
    ) -> String {
        self.show_shared_view_for_session(params, None).await
    }

    async fn hide_shared_view_for_session(
        &self,
        params: HideSharedViewParams,
        session_id: Option<&str>,
    ) -> String {
        self.emit_shared_view(session_id, "hide", None, None, params.reason, None, None)
            .await
    }

    #[tool(description = "Dismiss the dashboard shared display view banner and focus overlay.")]
    async fn hide_shared_view(
        &self,
        Parameters(params): Parameters<HideSharedViewParams>,
    ) -> String {
        self.hide_shared_view_for_session(params, None).await
    }

    async fn focus_shared_view_for_session(
        &self,
        params: FocusSharedViewParams,
        session_id: Option<&str>,
    ) -> String {
        let display_target = shared_view_display_target(params.display_target, params.display_id);
        let display_id = shared_view_display_id(display_target.as_deref(), params.display_id);
        self.ensure_shared_view_display_active(display_target.as_deref(), display_id)
            .await;
        self.emit_shared_view(
            session_id,
            "focus",
            display_target,
            display_id,
            None,
            Some(normalize_shared_view_region(params.region)),
            params.note,
        )
        .await
    }

    #[tool(
        description = "Highlight a normalized region in the dashboard shared display view. For user_session / primary-display targets, this also requests display-stream activation. Use this to point the user at a specific UI element or area."
    )]
    async fn focus_shared_view(
        &self,
        Parameters(params): Parameters<FocusSharedViewParams>,
    ) -> String {
        self.focus_shared_view_for_session(params, None).await
    }

    async fn request_shared_view_input_for_session(
        &self,
        params: RequestSharedViewInputParams,
        session_id: Option<&str>,
    ) -> String {
        let display_target = shared_view_display_target(params.display_target, params.display_id);
        let display_id = shared_view_display_id(display_target.as_deref(), params.display_id);
        self.ensure_shared_view_display_active(display_target.as_deref(), display_id)
            .await;
        self.emit_shared_view(
            session_id,
            "input_request",
            display_target,
            display_id,
            params.reason,
            None,
            None,
        )
        .await
    }

    #[tool(
        description = "Ask the dashboard user to take input authority for the shared display. For user_session / primary-display targets, this also requests display-stream activation. This is advisory: the user must click the dashboard control before keyboard/mouse input is granted."
    )]
    async fn request_shared_view_input(
        &self,
        Parameters(params): Parameters<RequestSharedViewInputParams>,
    ) -> String {
        self.request_shared_view_input_for_session(params, None)
            .await
    }

    async fn capture_shared_view_frame_for_session(
        &self,
        params: CaptureSharedViewFrameParams,
        session_id: Option<&str>,
    ) -> Result<CallToolResult, McpError> {
        let display_target = shared_view_display_target(params.display_target, params.display_id);
        let display_id = shared_view_display_id(display_target.as_deref(), params.display_id);
        self.ensure_shared_view_display_active(display_target.as_deref(), display_id)
            .await;
        self.emit_shared_view(
            session_id,
            "capture",
            display_target.clone(),
            display_id,
            params.reason,
            None,
            None,
        )
        .await;
        self.take_screenshot(Parameters(TakeScreenshotParams { display_target }))
            .await
    }

    #[tool(
        description = "Capture the currently shared display as an MCP image. Also foregrounds the dashboard shared view so the user can see what was captured."
    )]
    async fn capture_shared_view_frame(
        &self,
        Parameters(params): Parameters<CaptureSharedViewFrameParams>,
    ) -> Result<CallToolResult, McpError> {
        self.capture_shared_view_frame_for_session(params, None)
            .await
    }

    #[tool(description = "Take a screenshot of a display. Returns an MCP image content block.")]
    async fn take_screenshot(
        &self,
        Parameters(params): Parameters<TakeScreenshotParams>,
    ) -> Result<CallToolResult, McpError> {
        use crate::computer_use::{execute_actions, CuAction, DisplayBackend};

        #[cfg(target_os = "linux")]
        crate::linux_display_env::ensure_gui_session_env("mcp take_screenshot");

        let target = resolve_display_target(params.display_target.as_deref());
        let backend = DisplayBackend::detect();

        let state = self.state.read().await;
        let screenshot_dir = state
            .screenshot_dir
            .clone()
            .unwrap_or_else(|| state.log_dir.join("screenshots"));
        let session_registry = state.session_registry.clone();
        drop(state);

        let _ = std::fs::create_dir_all(&screenshot_dir);
        let mut counter = self
            .state
            .read()
            .await
            .screenshot_counter
            .fetch_add(10, std::sync::atomic::Ordering::Relaxed);
        let results = execute_actions(
            &[CuAction::Screenshot],
            target,
            backend,
            &screenshot_dir,
            &mut counter,
            &session_registry,
            None,
        )
        .await;

        if let Some(result) = results.first() {
            if let Some(ref screenshot) = result.screenshot {
                return Ok(image_tool_result(
                    "screenshot captured",
                    screenshot.base64_png.clone(),
                ));
            }
            if let Some(ref err) = result.error {
                return Ok(text_tool_error(format!("Screenshot error: {}", err)));
            }
        }

        Ok(text_tool_error("No screenshot result"))
    }

    #[tool(
        description = "Execute computer-use actions on a display (click, type, scroll, etc). Returns action status plus an MCP image content block for the post-action screenshot. Set coordinate_space to \"normalized_1000\" if coordinates are on a 0-1000 grid."
    )]
    async fn execute_cu_actions(
        &self,
        Parameters(params): Parameters<ExecuteCuActionsParams>,
    ) -> Result<CallToolResult, McpError> {
        use crate::computer_use::{execute_actions, DisplayBackend};

        #[cfg(target_os = "linux")]
        crate::linux_display_env::ensure_gui_session_env("mcp execute_cu_actions");

        let mut actions = params.actions;

        if actions.is_empty() {
            return Ok(text_tool_error("No actions provided"));
        }

        let target = resolve_display_target(params.display_target.as_deref());
        let backend = DisplayBackend::detect();

        let state = self.state.read().await;
        let screenshot_dir = state
            .screenshot_dir
            .clone()
            .unwrap_or_else(|| state.log_dir.join("screenshots"));
        let session_registry = state.session_registry.clone();
        drop(state);

        // Denormalize 0-1000 grid coordinates to pixel coordinates.
        // Reference size comes from the live capture session when one exists
        // (required on Wayland, where the portal grants an arbitrary stream
        // size that the model's screenshot is in). Falls back to platform
        // enumeration / logical_display_size when no session is active.
        //
        // The snapshot is also forwarded to execute_via_session so it uses
        // the same divisor for re-normalization — this prevents a TOCTOU
        // race if the portal stream resizes between the two reads.
        let denorm_ref = if params.coordinate_space.as_deref() == Some("normalized_1000") {
            let size = crate::computer_use::target_pixel_size(target, &session_registry).await;
            for action in &mut actions {
                denormalize_action(action, size.0, size.1);
            }
            Some(size)
        } else {
            None
        };

        let _ = std::fs::create_dir_all(&screenshot_dir);
        let mut counter = self
            .state
            .read()
            .await
            .screenshot_counter
            .fetch_add(10, std::sync::atomic::Ordering::Relaxed);
        let results = execute_actions(
            &actions,
            target,
            backend,
            &screenshot_dir,
            &mut counter,
            &session_registry,
            denorm_ref,
        )
        .await;

        // Format results with action details (type, coordinates) for debugging.
        let mut summaries = Vec::new();
        for (i, (action, result)) in actions.iter().zip(results.iter()).enumerate() {
            let status = if result.error.is_some() {
                "failed"
            } else {
                "ok"
            };
            let action_desc = format_cu_action_brief(action);
            let detail = result.error.as_deref().unwrap_or("");
            if detail.is_empty() {
                summaries.push(format!("action[{}] {}: {}", i, action_desc, status));
            } else {
                summaries.push(format!(
                    "action[{}] {}: {}: {}",
                    i, action_desc, status, detail
                ));
            }
        }

        // Attach the last screenshot inline, annotated with click markers.
        // Also save the annotated version to disk so substitute_screenshot_from_disk
        // picks it up for the Activity tab.
        let last_screenshot = results.iter().rev().find_map(|r| r.screenshot.as_ref());
        if let Some(ss) = last_screenshot {
            let annotated = annotate_screenshot_with_clicks(&ss.base64_png, &actions);
            // Save annotated screenshot to disk (overwrite the raw one)
            if let Ok(bytes) =
                base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &annotated)
            {
                let _ = std::fs::write(&ss.path, &bytes);
            }
            summaries.push("post-action screenshot captured".to_string());
            return Ok(image_tool_result(summaries.join("\n"), annotated));
        }

        Ok(text_tool_result(summaries.join("\n")))
    }

    #[tool(
        description = "List available display frames with metadata. Frames are captured from display streams."
    )]
    async fn list_frames(&self, Parameters(params): Parameters<ListFramesParams>) -> String {
        let state = self.state.read().await;
        let registry = match &state.frame_registry {
            Some(r) => r.clone(),
            None => return "Frame registry not available".to_string(),
        };
        drop(state);

        let reg = registry.read().await;
        let count = params.count.unwrap_or(20);
        let frames = reg.query(params.stream.as_deref(), count);

        if frames.is_empty() {
            let streams = reg.active_streams();
            if streams.is_empty() {
                return "No frames available. No active display streams.".to_string();
            }
            return format!(
                "No frames matching filter. Active streams: {}",
                streams.join(", ")
            );
        }

        crate::frames::FrameRegistry::format_frame_list(&frames)
    }

    #[tool(
        description = "Read a specific frame's image data as base64-encoded JPEG. Use frame_id='latest' for the most recent."
    )]
    async fn read_frame(&self, Parameters(params): Parameters<ReadFrameParams>) -> String {
        use base64::Engine;

        let state = self.state.read().await;
        let registry = match &state.frame_registry {
            Some(r) => r.clone(),
            None => return "Frame registry not available".to_string(),
        };
        drop(state);

        let reg = registry.read().await;

        let frame_id = if params.frame_id == "latest" {
            match reg.latest(params.stream.as_deref()) {
                Some(id) => id.to_string(),
                None => return "No frames available".to_string(),
            }
        } else {
            params.frame_id.clone()
        };

        match reg.read_hq(&frame_id) {
            Ok(data) => {
                let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
                format!("data:image/jpeg;base64,{}", b64)
            }
            Err(e) => format!("Error reading frame '{}': {}", frame_id, e),
        }
    }

    #[tool(
        description = "Spawn a live audio voice conversation. Connects to OpenAI Realtime or Gemini Live via WebSocket and routes audio through the virtual audio bridge (Vortex/PulseAudio). The voice model follows a playbook and returns structured data matching the response_schema. Blocks until the conversation completes or times out. The voice model has two functions: submit_response (with schema fields) and end_call."
    )]
    async fn spawn_live_audio(
        &self,
        Parameters(params): Parameters<SpawnLiveAudioParams>,
    ) -> String {
        use crate::{audio_routing, live_audio, live_audio_types, prompts};

        let spec_json = serde_json::to_value(&params).unwrap_or_default();
        let spec_result = serde_json::from_value::<live_audio_types::LiveAudioSpec>(spec_json);
        let mut spec = match spec_result {
            Ok(s) => s,
            Err(e) => return format!("Error parsing LiveAudioSpec: {}", e),
        };

        // Build system prompt from playbook + schema
        let project_root = std::env::var("INTENDANT_PROJECT_ROOT")
            .ok()
            .map(std::path::PathBuf::from);
        let system_prompt = prompts::build_live_audio_prompt(
            &spec.playbook,
            &spec.response_schema,
            project_root.as_deref(),
        );
        spec.playbook = system_prompt;

        // Resolve API key
        let api_key_var = match spec.provider {
            live_audio_types::LiveAudioProvider::Gemini => "GEMINI_API_KEY",
            live_audio_types::LiveAudioProvider::OpenAI => "OPENAI_API_KEY",
        };
        let api_key = match std::env::var(api_key_var) {
            Ok(k) => k,
            Err(_) => return format!("Error: {} not set", api_key_var),
        };

        // Create audio bridge
        // Vortex shared-memory probe is POSIX-only (`shm_open`). On
        // Windows the Vortex bridge isn't available, so the probe is
        // compiled out and we fall through to the regular audio bridge.
        #[cfg(unix)]
        let vortex_shm_available = unsafe {
            let fd = libc::shm_open(
                b"/vortex-audio\0".as_ptr() as *const libc::c_char,
                libc::O_RDONLY,
                0,
            );
            if fd >= 0 {
                libc::close(fd);
                true
            } else {
                false
            }
        };
        #[cfg(not(unix))]
        let vortex_shm_available = false;
        let mut bridge = if vortex_shm_available {
            audio_routing::create_vortex_bridge("shm")
        } else {
            match audio_routing::create_bridge(&spec.id).await {
                Ok(b) => b,
                Err(e) => return format!("Error creating audio bridge: {}", e),
            }
        };
        if bridge.vortex_socket_path().is_none() {
            let _ = audio_routing::set_as_default(&mut bridge).await;
        }

        let log_dir = {
            let state = self.state.read().await;
            state.log_dir.clone()
        };

        self.bus.send(crate::event::AppEvent::PresenceLog {
            message: format!(
                "Live audio session '{}' starting ({:?})",
                spec.id, spec.provider
            ),
            level: None,
            turn: None,
        });

        let result =
            live_audio::run_session(&spec, &api_key, &bridge, &log_dir, Some(&self.bus)).await;

        drop(bridge);

        match result {
            Ok(la_result) => serde_json::to_string_pretty(&la_result)
                .unwrap_or_else(|_| format!("{:?}", la_result)),
            Err(e) => format!("Error: {}", e),
        }
    }
}

fn resolve_display_target(target: Option<&str>) -> crate::computer_use::DisplayTarget {
    use crate::computer_use::DisplayTarget;
    match target {
        Some("user_session") | Some("user") | Some("primary") | Some(":0") | Some("0")
        | Some("display_0") => DisplayTarget::UserSession,
        Some(s) if s.starts_with(':') => {
            let id: u32 = s[1..].parse().unwrap_or(99);
            DisplayTarget::Virtual { id }
        }
        Some(s) if s.starts_with("display_") => {
            let id: u32 = s["display_".len()..].parse().unwrap_or(99);
            DisplayTarget::Virtual { id }
        }
        Some(s) => {
            let id: u32 = s.parse().unwrap_or(99);
            DisplayTarget::Virtual { id }
        }
        None => {
            // Default: first virtual display
            DisplayTarget::Virtual { id: 99 }
        }
    }
}

fn format_outcome(outcome: ActionOutcome) -> String {
    match outcome {
        ActionOutcome::Ok => "ok".to_string(),
        ActionOutcome::NoOp { reason } => format!("no-op: {}", reason),
    }
}

// ---------------------------------------------------------------------------
// Resource definitions
// ---------------------------------------------------------------------------

const RESOURCE_STATUS_URI: &str = "intendant://status";
const RESOURCE_USAGE_URI: &str = "intendant://usage";
const RESOURCE_LOGS_URI: &str = "intendant://logs";
const RESOURCE_APPROVAL_URI: &str = "intendant://pending-approval";
const RESOURCE_INPUT_URI: &str = "intendant://pending-input";
const RESOURCE_RESTART_URI: &str = "intendant://controller-restart";
const RESOURCE_LOOP_URI: &str = "intendant://controller-loop";

fn make_resource(uri: &str, name: &str, description: &str) -> Resource {
    Resource {
        raw: RawResource {
            uri: uri.to_string(),
            name: name.to_string(),
            title: None,
            description: Some(description.to_string()),
            mime_type: Some("application/json".to_string()),
            size: None,
            icons: None,
            meta: None,
        },
        annotations: None,
    }
}

fn resource_definitions() -> Vec<Resource> {
    vec![
        make_resource(
            RESOURCE_STATUS_URI,
            "status",
            "Current status: session_id, task, provider, model, turn, budget, phase, autonomy",
        ),
        make_resource(
            RESOURCE_USAGE_URI,
            "usage",
            "Token usage for all models: main (provider, model, tokens_used, context_window, usage_pct) and optional presence",
        ),
        make_resource(
            RESOURCE_LOGS_URI,
            "logs",
            "Chronological log entries (same as TUI log panel)",
        ),
        make_resource(
            RESOURCE_APPROVAL_URI,
            "pending-approval",
            "Current pending approval request, if any",
        ),
        make_resource(
            RESOURCE_INPUT_URI,
            "pending-input",
            "Current pending human question, if any",
        ),
        make_resource(
            RESOURCE_RESTART_URI,
            "controller-restart",
            "Controller restart schedule / execution state",
        ),
        make_resource(
            RESOURCE_LOOP_URI,
            "controller-loop",
            "Controller loop health and intervention state",
        ),
    ]
}

// ---------------------------------------------------------------------------
// ServerHandler implementation
// ---------------------------------------------------------------------------

#[tool_handler]
impl ServerHandler for IntendantServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Intendant AI agent runtime. This MCP server exposes the same controls \
                 and observations as the TUI. Use tools to control the agent (approve, \
                 deny, respond, set_autonomy, quit), manage controller restarts \
                 (schedule_controller_restart, controller_turn_complete, \
                 get_restart_status, cancel_controller_restart), manage loop \
                 intervention (request_controller_loop_halt, clear_controller_loop_halt, \
                 intervene_controller_loop, get_controller_loop_status), and observe state \
                 (get_status, get_logs, get_pending_approval, get_pending_input). \
                 Resources provide push-based state updates via subscriptions."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .enable_resources()
                .enable_resources_subscribe()
                .enable_resources_list_changed()
                .build(),
            server_info: Implementation {
                name: "intendant".to_string(),
                title: Some("Intendant AI Agent Runtime".to_string()),
                version: env!("CARGO_PKG_VERSION").to_string(),
                description: Some(
                    "MCP interface for controlling and observing the Intendant AI agent"
                        .to_string(),
                ),
                icons: None,
                website_url: None,
            },
            ..Default::default()
        }
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        Ok(ListResourcesResult {
            meta: None,
            resources: resource_definitions(),
            next_cursor: None,
        })
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        if request.uri == RESOURCE_LOOP_URI {
            let value = collect_controller_loop_status(&controller_loop_dir());
            let json = serde_json::to_string_pretty(&value).unwrap_or_else(|_| "null".to_string());
            return Ok(ReadResourceResult {
                contents: vec![ResourceContents::text(json, request.uri)],
            });
        }

        let s = self.state.read().await;
        let json = match request.uri.as_str() {
            RESOURCE_STATUS_URI => {
                let mut snap = s.status_snapshot();
                let autonomy_level = s.autonomy.read().await.level;
                snap.autonomy = autonomy_level.to_string().to_lowercase();
                serde_json::to_string_pretty(&StateResult::Status(snap))
                    .unwrap_or_else(|_| "{}".to_string())
            }
            RESOURCE_USAGE_URI => {
                let usage = s.usage_snapshot();
                serde_json::to_string_pretty(&StateResult::Usage(usage))
                    .unwrap_or_else(|_| "{}".to_string())
            }
            RESOURCE_LOGS_URI => {
                // Return last 100 entries
                let entries: Vec<LogEntrySnapshot> = s
                    .log_entries
                    .iter()
                    .rev()
                    .take(100)
                    .rev()
                    .cloned()
                    .collect();
                serde_json::to_string_pretty(&StateResult::Logs { entries })
                    .unwrap_or_else(|_| "[]".to_string())
            }
            RESOURCE_APPROVAL_URI => {
                let snap = s.approval_snapshot();
                serde_json::to_string_pretty(&StateResult::PendingApproval { approval: snap })
                    .unwrap_or_else(|_| "null".to_string())
            }
            RESOURCE_INPUT_URI => {
                let snap = s.human_question_snapshot();
                serde_json::to_string_pretty(&StateResult::PendingInput { question: snap })
                    .unwrap_or_else(|_| "null".to_string())
            }
            RESOURCE_RESTART_URI => {
                let value = restart_state_public_value(s.controller_restart.as_ref());
                serde_json::to_string_pretty(&value).unwrap_or_else(|_| "null".to_string())
            }
            _ => {
                return Err(McpError::invalid_params(
                    format!("Unknown resource URI: {}", request.uri),
                    None,
                ));
            }
        };

        Ok(ReadResourceResult {
            contents: vec![ResourceContents::text(json, request.uri)],
        })
    }

    async fn subscribe(
        &self,
        _request: SubscribeRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        // We push notifications for all resources on every relevant event change
        // (handled in spawn_event_listener). Accept all subscriptions.
        Ok(())
    }

    async fn unsubscribe(
        &self,
        _request: UnsubscribeRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Public API: start the MCP server on stdio
// ---------------------------------------------------------------------------

/// Run the MCP server on stdio. This replaces the TUI — the external agent
/// communicates via MCP over stdin/stdout.
///
/// The server consumes AppEvents from the bus and exposes them as tools and
/// resources.
pub async fn run_mcp_server(
    state: SharedMcpState,
    bus: EventBus,
    event_rx: tokio::sync::broadcast::Receiver<AppEvent>,
    human_question_path: Option<crate::event::SharedQuestionPath>,
    control_tx: Option<broadcast::Sender<String>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let server = IntendantServer::new(state.clone(), bus.clone());

    let transport = rmcp::transport::io::stdio();
    let running = server.serve(transport).await?;

    // Store the peer for sending notifications
    let peer = Arc::new(Mutex::new(Some(running.peer().clone())));

    // Spawn event listener that mirrors AppEvents into McpAppState
    let _listener = spawn_event_listener(
        state,
        event_rx,
        peer,
        bus.clone(),
        human_question_path,
        control_tx,
    );

    // Wait until the service finishes (client disconnects or quit)
    running.waiting().await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Screenshot click annotation
// ---------------------------------------------------------------------------

/// Denormalize a CU action's coordinates from 0-1000 grid to pixel space.
fn denormalize_action(action: &mut crate::computer_use::CuAction, screen_w: u32, screen_h: u32) {
    use crate::computer_use::CuAction;
    let dn_x = |x: &mut i32| *x = (*x as f64 * screen_w as f64 / 1000.0) as i32;
    let dn_y = |y: &mut i32| *y = (*y as f64 * screen_h as f64 / 1000.0) as i32;
    match action {
        CuAction::Click { x, y, .. } | CuAction::DoubleClick { x, y, .. } => {
            dn_x(x);
            dn_y(y);
        }
        CuAction::Scroll { x, y, .. } => {
            dn_x(x);
            dn_y(y);
        }
        CuAction::MoveMouse { x, y } => {
            dn_x(x);
            dn_y(y);
        }
        CuAction::Drag {
            start_x,
            start_y,
            end_x,
            end_y,
        } => {
            dn_x(start_x);
            dn_y(start_y);
            dn_x(end_x);
            dn_y(end_y);
        }
        _ => {} // Type, Key, Screenshot, Wait — no coordinates
    }
}

/// Format a CU action as a short description for logs.
fn format_cu_action_brief(action: &crate::computer_use::CuAction) -> String {
    use crate::computer_use::CuAction;
    match action {
        CuAction::Click { x, y, button } => format!("(click {},{} {:?})", x, y, button),
        CuAction::DoubleClick { x, y, button } => format!("(dblclick {},{} {:?})", x, y, button),
        CuAction::Type { text } => {
            let preview = if text.len() > 30 { &text[..30] } else { text };
            format!("(type \"{}\")", preview)
        }
        CuAction::Key { key } => format!("(key {})", key),
        CuAction::Scroll {
            x,
            y,
            direction,
            amount,
        } => {
            format!("(scroll {},{} {:?} {})", x, y, direction, amount)
        }
        CuAction::MoveMouse { x, y } => format!("(move {},{})", x, y),
        CuAction::Drag {
            start_x,
            start_y,
            end_x,
            end_y,
        } => {
            format!("(drag {},{}->{},{})", start_x, start_y, end_x, end_y)
        }
        CuAction::Screenshot => "(screenshot)".to_string(),
        CuAction::Wait { ms } => format!("(wait {}ms)", ms),
    }
}

/// Draw red crosshairs on a screenshot at click/double_click coordinates.
/// Returns annotated base64 PNG, or the original if annotation fails.
fn annotate_screenshot_with_clicks(
    base64_png: &str,
    actions: &[crate::computer_use::CuAction],
) -> String {
    use crate::computer_use::CuAction;

    // Collect click coordinates
    let clicks: Vec<(i32, i32)> = actions
        .iter()
        .filter_map(|a| match a {
            CuAction::Click { x, y, .. } | CuAction::DoubleClick { x, y, .. } => Some((*x, *y)),
            _ => None,
        })
        .collect();

    if clicks.is_empty() {
        return base64_png.to_string();
    }

    // Decode PNG
    let png_bytes =
        match base64::Engine::decode(&base64::engine::general_purpose::STANDARD, base64_png) {
            Ok(b) => b,
            Err(_) => return base64_png.to_string(),
        };

    let mut img = match image::load_from_memory_with_format(&png_bytes, image::ImageFormat::Png) {
        Ok(i) => i.to_rgba8(),
        Err(_) => return base64_png.to_string(),
    };

    let (w, h) = (img.width() as i32, img.height() as i32);
    let red = image::Rgba([255u8, 0, 0, 255]);
    let yellow = image::Rgba([255u8, 255, 0, 255]);
    let arm = 20i32;
    let thickness = 3i32;

    for (cx, cy) in &clicks {
        // Clamp to image bounds; use yellow for out-of-bounds clicks
        let oob = *cx < 0 || *cx >= w || *cy < 0 || *cy >= h;
        let color = if oob { yellow } else { red };
        let dx = (*cx).max(0).min(w - 1);
        let dy = (*cy).max(0).min(h - 1);

        // Draw crosshair at clamped position
        for offset in -arm..=arm {
            for t in -thickness..=thickness {
                let hx = dx + offset;
                let hy = dy + t;
                if hx >= 0 && hx < w && hy >= 0 && hy < h {
                    img.put_pixel(hx as u32, hy as u32, color);
                }
                let vx = dx + t;
                let vy = dy + offset;
                if vx >= 0 && vx < w && vy >= 0 && vy < h {
                    img.put_pixel(vx as u32, vy as u32, color);
                }
            }
        }
        // Draw circle (radius 12)
        let r = 12i32;
        for angle in 0..360 {
            let rad = (angle as f64) * std::f64::consts::PI / 180.0;
            let px = dx + (r as f64 * rad.cos()) as i32;
            let py = dy + (r as f64 * rad.sin()) as i32;
            for t in 0..=2 {
                let px2 = px + t;
                let py2 = py + t;
                if px2 >= 0 && px2 < w && py2 >= 0 && py2 < h {
                    img.put_pixel(px2 as u32, py2 as u32, color);
                }
            }
        }

        // Draw "OOB" indicator at top-left if out of bounds
        if oob {
            // Draw a solid yellow bar at the top of the image as a warning
            for bx in 0..80i32 {
                for by in 0..6i32 {
                    if bx < w && by < h {
                        img.put_pixel(bx as u32, by as u32, yellow);
                    }
                }
            }
        }
    }

    // Re-encode to PNG
    let mut buf = std::io::Cursor::new(Vec::new());
    if img.write_to(&mut buf, image::ImageFormat::Png).is_err() {
        return base64_png.to_string();
    }
    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, buf.into_inner())
}

// ---------------------------------------------------------------------------
// Schema $ref inlining
// ---------------------------------------------------------------------------

/// Resolve all `$ref`/`$defs` in a JSON Schema by inlining referenced
/// definitions. This produces an equivalent schema with no `$ref` pointers,
/// which is needed for clients that don't resolve references (e.g. Codex).
///
/// The function modifies the schema in place:
/// 1. Collects all definitions from `$defs` (or `definitions`)
/// 2. Recursively replaces every `{"$ref": "#/$defs/Foo"}` with the
///    corresponding definition (also recursively resolved)
/// 3. Removes the top-level `$defs`/`definitions` key
fn inline_schema_refs(schema: &mut serde_json::Value) {
    // Extract $defs / definitions from the top level
    let defs = schema
        .as_object_mut()
        .and_then(|obj| obj.remove("$defs").or_else(|| obj.remove("definitions")))
        .and_then(|v| match v {
            serde_json::Value::Object(map) => Some(map),
            _ => None,
        })
        .unwrap_or_default();

    if !defs.is_empty() {
        resolve_refs(schema, &defs);
    }
}

/// Recursively walk a JSON value and replace `{"$ref": "#/$defs/Name"}` or
/// `{"$ref": "#/definitions/Name"}` with the corresponding definition.
///
/// Safe from infinite recursion because our MCP schema types are non-recursive
/// (McpFieldType uses McpArrayElement for array elements instead of Box<Self>).
fn resolve_refs(value: &mut serde_json::Value, defs: &serde_json::Map<String, serde_json::Value>) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(ref_val) = map.get("$ref").and_then(|v| v.as_str()).map(String::from) {
                let name = ref_val
                    .strip_prefix("#/$defs/")
                    .or_else(|| ref_val.strip_prefix("#/definitions/"));
                if let Some(def_name) = name {
                    if let Some(def) = defs.get(def_name) {
                        let mut resolved = def.clone();
                        resolve_refs(&mut resolved, defs);
                        *value = resolved;
                        return;
                    }
                }
            }
            for v in map.values_mut() {
                resolve_refs(v, defs);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr.iter_mut() {
                resolve_refs(v, defs);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autonomy::{self, AutonomyState};
    use tempfile::tempdir;
    use tokio::time::{timeout, Duration};

    static TEST_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn test_state() -> SharedMcpState {
        test_state_with_log_dir(std::path::PathBuf::from("/tmp/test_session"))
    }

    fn test_state_with_log_dir(log_dir: std::path::PathBuf) -> SharedMcpState {
        let autonomy = autonomy::shared_autonomy(AutonomyState::default());
        Arc::new(RwLock::new(McpAppState::new(
            "openai".to_string(),
            "gpt-5".to_string(),
            autonomy,
            log_dir,
        )))
    }

    fn spawn_codex_thread_action_result(
        bus: EventBus,
        expected_action: &'static str,
        message: &'static str,
    ) -> tokio::task::JoinHandle<()> {
        let mut rx = bus.subscribe();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(AppEvent::ControlCommand(ControlMsg::CodexThreadAction {
                        session_id,
                        op,
                        ..
                    })) if op == expected_action => {
                        bus.send(AppEvent::CodexThreadActionResult {
                            session_id,
                            action: op,
                            success: true,
                            message: message.to_string(),
                        });
                        break;
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    }

    #[test]
    fn mcp_state_initial_values() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let s = state.read().await;
            assert_eq!(s.turn, 0);
            assert_eq!(s.budget_pct, 0.0);
            assert_eq!(s.phase, Phase::Idle);
            assert!(s.log_entries.is_empty());
            assert!(s.pending_approval.is_none());
            assert!(s.human_question.is_none());
            assert!(!s.should_quit);
        });
    }

    #[test]
    fn status_snapshot_has_correct_fields() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            s.turn = 5;
            s.budget_pct = 42.0;
            s.set_phase(Phase::Thinking);
            s.session_tokens = 1234;
            let snap = s.status_snapshot();
            assert_eq!(snap.provider, "openai");
            assert_eq!(snap.model, "gpt-5");
            assert_eq!(snap.turn, 5);
            assert_eq!(snap.budget_pct, 42.0);
            assert_eq!(snap.phase, "thinking");
            assert_eq!(snap.session_tokens, 1234);
        });
    }

    #[test]
    fn shared_view_tool_activates_target_and_emits_dashboard_event() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            std::env::remove_var("INTENDANT_USER_DISPLAY_GRANTED");
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let server = IntendantServer::new(test_state(), bus.clone());

            let result = server
                .call_tool_by_name_for_session(
                    "show_shared_view",
                    serde_json::json!({
                        "display_target": ":99",
                        "reason": "show the failing login screen",
                        "focus_region": { "x": 0.9, "y": 0.9, "width": 0.4, "height": 0.4 }
                    }),
                    Some("session-a"),
                    None,
                )
                .await
                .expect("tool should dispatch");
            assert!(!result.is_error.unwrap_or(false));

            match timeout(Duration::from_secs(1), rx.recv()).await {
                Ok(Ok(AppEvent::UserDisplayGranted { display_id })) => {
                    assert_eq!(display_id, 99);
                }
                other => panic!("expected UserDisplayGranted event, got {other:?}"),
            }

            match timeout(Duration::from_secs(1), rx.recv()).await {
                Ok(Ok(AppEvent::SharedView {
                    session_id,
                    action,
                    display_target,
                    display_id,
                    reason,
                    region: Some(region),
                    ..
                })) => {
                    assert_eq!(session_id.as_deref(), Some("session-a"));
                    assert_eq!(action, "show");
                    assert_eq!(display_target.as_deref(), Some(":99"));
                    assert_eq!(display_id, Some(99));
                    assert_eq!(reason.as_deref(), Some("show the failing login screen"));
                    assert_eq!(region.x, 0.9);
                    assert_eq!(region.y, 0.9);
                    assert!((region.width - 0.1).abs() < f64::EPSILON);
                    assert!((region.height - 0.1).abs() < f64::EPSILON);
                }
                other => panic!("expected SharedView event, got {other:?}"),
            }
            std::env::remove_var("INTENDANT_USER_DISPLAY_GRANTED");
        });
    }

    #[test]
    fn shared_view_user_session_requests_display_activation() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            std::env::remove_var("INTENDANT_USER_DISPLAY_GRANTED");
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let server = IntendantServer::new(test_state(), bus.clone());

            let result = server
                .call_tool_by_name_for_session(
                    "show_shared_view",
                    serde_json::json!({
                        "display_target": "user_session",
                        "reason": "show the user's screen"
                    }),
                    Some("session-a"),
                    None,
                )
                .await
                .expect("tool should dispatch");
            assert!(!result.is_error.unwrap_or(false));

            match timeout(Duration::from_secs(1), rx.recv()).await {
                Ok(Ok(AppEvent::UserDisplayGranted { display_id })) => {
                    assert_eq!(display_id, 0);
                }
                other => panic!("expected UserDisplayGranted event, got {other:?}"),
            }
            match timeout(Duration::from_secs(1), rx.recv()).await {
                Ok(Ok(AppEvent::SharedView {
                    session_id,
                    action,
                    display_target,
                    display_id,
                    ..
                })) => {
                    assert_eq!(session_id.as_deref(), Some("session-a"));
                    assert_eq!(action, "show");
                    assert_eq!(display_target.as_deref(), Some("user_session"));
                    assert_eq!(display_id, Some(0));
                }
                other => panic!("expected SharedView event, got {other:?}"),
            }
            assert_eq!(
                std::env::var("INTENDANT_USER_DISPLAY_GRANTED").as_deref(),
                Ok("1")
            );
            std::env::remove_var("INTENDANT_USER_DISPLAY_GRANTED");
        });
    }

    #[test]
    fn shared_view_labels_are_platform_neutral() {
        assert_eq!(
            shared_view_target_label(Some(0), Some(":0")),
            "primary display"
        );
        assert_eq!(
            shared_view_target_label(None, Some("user_session")),
            "primary display"
        );
        assert_eq!(
            shared_view_target_label(None, Some("display_99")),
            "display 99"
        );
        assert_eq!(
            shared_view_target_label(Some(99), Some(":99")),
            "display 99"
        );
    }

    #[test]
    fn usage_snapshot_updates_real_context_pressure() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            s.apply_main_usage_snapshot(frontend::ModelUsageSnapshot {
                provider: "openai".to_string(),
                model: "gpt-5.2-codex".to_string(),
                tokens_used: 86_000,
                context_window: 100_000,
                hard_context_window: Some(120_000),
                usage_pct: 86.0,
                prompt_tokens: 80_000,
                completion_tokens: 6_000,
                cached_tokens: 10_000,
            });
            let usage = s.usage_snapshot();
            assert_eq!(usage.main.tokens_used, 86_000);
            assert_eq!(usage.main.prompt_tokens, 80_000);
            assert_eq!(usage.main.completion_tokens, 6_000);
            assert_eq!(usage.main.cached_tokens, 10_000);

            let pressure = s.context_pressure_snapshot();
            assert_eq!(pressure["source"], "backend_reported");
            assert_eq!(pressure["status"], "watch");
            assert_eq!(pressure["used_tokens"], 86_000);
            assert_eq!(pressure["context_window"], 100_000);
            assert_eq!(pressure["effective_context_window"], 100_000);
            assert_eq!(pressure["hard_limit"], 120_000);
            assert_eq!(pressure["recommended_rewind_limit"], 85_000);
            assert_eq!(pressure["rewind_only_limit"], 100_000);
            assert_eq!(pressure["rewind_only"], false);
            assert_eq!(pressure["density_pressure"], true);
            assert_eq!(pressure["density_maintenance_recommended"], false);
            assert_eq!(pressure["normal_tools_allowed"], true);
            assert_eq!(pressure["broad_followup_allowed"], true);
            assert_eq!(pressure["narrow_inflight_validation_allowed"], true);
            assert_eq!(pressure["required_action"], "continue_or_rewind_optional");
            assert_eq!(pressure["managed_context"], "vanilla");
        });
    }

    #[test]
    fn managed_watch_context_pressure_allows_normal_tools_after_rewind() {
        let mut s = McpAppState::new(
            "none".to_string(),
            "none".to_string(),
            autonomy::shared_autonomy(AutonomyState::default()),
            std::path::PathBuf::from("/tmp/test_session"),
        );
        s.session_id = "codex-thread".to_string();
        s.active_session_source = Some("codex".to_string());
        s.codex_managed_context = true;
        s.insufficient_rewind_notices.insert(
            "codex-thread".to_string(),
            InsufficientRewindNotice {
                record_id: "rewind-old".to_string(),
                used_tokens: 258_400,
                rewind_only_limit: 258_400,
                context_window: 258_400,
            },
        );

        apply_observed_event_to_mcp_state(
            &mut s,
            &AppEvent::CodexThreadActionResult {
                session_id: Some("codex-thread".to_string()),
                action: "rewind_context".to_string(),
                success: true,
                message: "Rewound Codex thread and saved record rewind-watch.".to_string(),
            },
        );
        apply_observed_event_to_mcp_state(
            &mut s,
            &AppEvent::UsageSnapshot {
                session_id: Some("codex-thread".to_string()),
                main: frontend::ModelUsageSnapshot {
                    provider: "openai".to_string(),
                    model: "gpt-5.2-codex".to_string(),
                    tokens_used: 220_385,
                    context_window: 258_400,
                    hard_context_window: Some(272_000),
                    usage_pct: 85.3,
                    prompt_tokens: 220_000,
                    completion_tokens: 385,
                    cached_tokens: 0,
                },
                presence: None,
            },
        );

        let pressure = s.context_pressure_snapshot();
        assert_eq!(pressure["status"], "watch");
        assert_eq!(pressure["rewind_only"], false);
        assert_eq!(pressure["density_maintenance_recommended"], true);
        assert_eq!(pressure["normal_tools_allowed"], true);
        assert_eq!(pressure["broad_followup_allowed"], false);
        assert_eq!(pressure["narrow_inflight_validation_allowed"], true);
        assert_eq!(
            pressure["required_action"],
            "density_handoff_before_broad_work"
        );
        assert_eq!(
            pressure["last_rewind_insufficient"],
            serde_json::Value::Null
        );
        let message = pressure["message"].as_str().unwrap_or_default();
        assert!(message.contains("Normal tools remain allowed"));
        assert!(message.contains("one narrow in-flight validation or build"));
        assert!(message.contains("concise no-rewind density handoff"));
        assert!(!message.contains("recovery"));
        assert!(!message.contains("Use rewind_context before ordinary"));
        assert!(s.rewind_only_gate_message("execute_cu_actions").is_none());
    }

    #[test]
    fn managed_ok_context_pressure_discourages_rewind_preparation() {
        let mut s = McpAppState::new(
            "none".to_string(),
            "none".to_string(),
            autonomy::shared_autonomy(AutonomyState::default()),
            std::path::PathBuf::from("/tmp/test_session"),
        );
        s.session_id = "codex-thread".to_string();
        s.active_session_source = Some("codex".to_string());
        s.codex_managed_context = true;
        s.apply_main_usage_snapshot(frontend::ModelUsageSnapshot {
            provider: "openai".to_string(),
            model: "gpt-5.2-codex".to_string(),
            tokens_used: 42_000,
            context_window: 258_400,
            hard_context_window: Some(272_000),
            usage_pct: 16.3,
            prompt_tokens: 40_000,
            completion_tokens: 2_000,
            cached_tokens: 0,
        });

        let pressure = s.context_pressure_snapshot();
        assert_eq!(pressure["status"], "ok");
        assert_eq!(pressure["rewind_only"], false);
        assert_eq!(pressure["normal_tools_allowed"], true);
        assert_eq!(pressure["required_action"], "continue");
        let message = pressure["message"].as_str().unwrap_or_default();
        assert!(message.contains("no rewind preparation is needed"));
        assert!(message.contains("genuinely noisy or unexpectedly large"));
        assert!(!message.contains("list_rewind_anchors"));
    }

    #[test]
    fn usage_snapshot_preserves_known_hard_limit_when_backend_collapses_to_soft_limit() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            s.active_session_source = Some("codex".to_string());
            s.codex_managed_context = true;
            s.apply_main_usage_snapshot(frontend::ModelUsageSnapshot {
                provider: "openai".to_string(),
                model: "codex".to_string(),
                tokens_used: 245_915,
                context_window: 258_400,
                hard_context_window: Some(272_000),
                usage_pct: 95.2,
                prompt_tokens: 245_915,
                completion_tokens: 0,
                cached_tokens: 0,
            });
            s.apply_main_usage_snapshot(frontend::ModelUsageSnapshot {
                provider: "openai".to_string(),
                model: "codex".to_string(),
                tokens_used: 258_400,
                context_window: 258_400,
                hard_context_window: Some(258_400),
                usage_pct: 100.0,
                prompt_tokens: 258_400,
                completion_tokens: 0,
                cached_tokens: 0,
            });

            let pressure = s.context_pressure_snapshot();
            assert_eq!(pressure["status"], "high");
            assert_eq!(pressure["used_tokens"], 258_400);
            assert_eq!(pressure["context_window"], 258_400);
            assert_eq!(pressure["hard_limit"], 272_000);
            assert_eq!(pressure["remaining_hard_tokens"], 13_600);
            assert_eq!(pressure["rewind_only"], true);
            assert_eq!(pressure["normal_tools_allowed"], false);
            assert_eq!(pressure["required_action"], "rewind_context");
        });
    }

    #[test]
    fn context_pressure_marks_rewind_only_only_when_managed_context_enabled() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            s.active_session_source = Some("codex".to_string());
            s.codex_managed_context = true;
            s.apply_main_usage_snapshot(frontend::ModelUsageSnapshot {
                provider: "openai".to_string(),
                model: "gpt-5.2-codex".to_string(),
                tokens_used: 100_000,
                context_window: 100_000,
                hard_context_window: Some(120_000),
                usage_pct: 100.0,
                prompt_tokens: 80_000,
                completion_tokens: 20_000,
                cached_tokens: 10_000,
            });
            let pressure = s.context_pressure_snapshot();
            assert_eq!(pressure["rewind_only"], true);
            assert_eq!(pressure["managed_context"], "managed");
        });
    }

    #[test]
    fn rewind_anchor_catalog_forces_recovery_filter_under_managed_pressure() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            s.active_session_source = Some("codex".to_string());
            s.codex_managed_context = true;
            s.apply_main_usage_snapshot(frontend::ModelUsageSnapshot {
                provider: "openai".to_string(),
                model: "gpt-5.2-codex".to_string(),
                tokens_used: 100_001,
                context_window: 100_000,
                hard_context_window: Some(120_000),
                usage_pct: 100.0,
                prompt_tokens: 100_001,
                completion_tokens: 0,
                cached_tokens: 0,
            });

            assert!(s.rewind_anchor_recovery_candidates_only_for(None, Some(false), false));
            assert!(!s.rewind_anchor_recovery_candidates_only_for(None, Some(false), true));
            assert!(!s.rewind_anchor_recovery_candidates_only_for(None, Some(true), true));
        });
    }

    #[test]
    fn rewind_anchor_catalog_requires_explicit_non_recovery_audit() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            s.active_session_source = Some("codex".to_string());
            s.codex_managed_context = true;
            s.apply_main_usage_snapshot(frontend::ModelUsageSnapshot {
                provider: "openai".to_string(),
                model: "gpt-5.2-codex".to_string(),
                tokens_used: 99_999,
                context_window: 100_000,
                hard_context_window: Some(120_000),
                usage_pct: 99.9,
                prompt_tokens: 99_999,
                completion_tokens: 0,
                cached_tokens: 0,
            });

            assert!(s.rewind_anchor_recovery_candidates_only_for(None, Some(false), false));
            assert!(s.rewind_anchor_recovery_candidates_only_for(None, None, false));
            assert!(!s.rewind_anchor_recovery_candidates_only_for(None, Some(false), true));
        });
    }

    #[test]
    fn rewind_only_gate_blocks_non_rewind_tools_for_active_codex_session() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            s.active_session_source = Some("codex".to_string());
            s.codex_managed_context = true;
            s.apply_main_usage_snapshot(frontend::ModelUsageSnapshot {
                provider: "openai".to_string(),
                model: "gpt-5.2-codex".to_string(),
                tokens_used: 100_000,
                context_window: 100_000,
                hard_context_window: Some(120_000),
                usage_pct: 100.0,
                prompt_tokens: 80_000,
                completion_tokens: 20_000,
                cached_tokens: 0,
            });

            let message = s
                .rewind_only_gate_message("take_screenshot")
                .expect("Codex action tool should be gated");
            assert!(message.contains(
                "model-facing tools are limited to get_status, list_rewind_anchors, inspect_rewind_anchor, rewind_context, and rewind_backout"
            ));
            assert!(message.contains("Read-only supervisor observability tools"));
            assert!(s.rewind_only_gate_message("get_status").is_none());
            assert!(s.rewind_only_gate_message("list_rewind_anchors").is_none());
            assert!(s.rewind_only_gate_message("inspect_rewind_anchor").is_none());
            assert!(s.rewind_only_gate_message("rewind_context").is_none());
            assert!(s.rewind_only_gate_message("rewind_backout").is_none());
        });
    }

    #[test]
    fn rewind_only_gate_allows_supervisor_observability_tools() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            s.active_session_source = Some("codex".to_string());
            s.codex_managed_context = true;
            s.apply_main_usage_snapshot(frontend::ModelUsageSnapshot {
                provider: "openai".to_string(),
                model: "gpt-5.2-codex".to_string(),
                tokens_used: 258_400,
                context_window: 258_400,
                hard_context_window: Some(272_000),
                usage_pct: 100.0,
                prompt_tokens: 258_000,
                completion_tokens: 400,
                cached_tokens: 0,
            });

            assert!(s.rewind_only_gate_message("get_logs").is_none());
            assert!(s.rewind_only_gate_message("get_pending_approval").is_none());
            assert!(s.rewind_only_gate_message("get_pending_input").is_none());
            assert!(s
                .rewind_only_gate_message("get_controller_loop_status")
                .is_none());
            assert!(s.rewind_only_gate_message("get_restart_status").is_none());
            assert!(s
                .rewind_only_gate_message("request_controller_loop_halt")
                .is_some());
        });
    }

    #[test]
    fn get_logs_remains_callable_under_managed_rewind_only_pressure() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            {
                let mut s = state.write().await;
                s.active_session_source = Some("codex".to_string());
                s.codex_managed_context = true;
                s.apply_main_usage_snapshot(frontend::ModelUsageSnapshot {
                    provider: "openai".to_string(),
                    model: "gpt-5.2-codex".to_string(),
                    tokens_used: 258_400,
                    context_window: 258_400,
                    hard_context_window: Some(272_000),
                    usage_pct: 100.0,
                    prompt_tokens: 258_000,
                    completion_tokens: 400,
                    cached_tokens: 0,
                });
                s.push_log(
                    LogLevel::Info,
                    "supervisor log is still readable".to_string(),
                );
            }
            let server = IntendantServer::new(state, EventBus::new());

            let result = server
                .call_tool_by_name_for_session(
                    "get_logs",
                    serde_json::json!({ "limit": 160 }),
                    None,
                    None,
                )
                .await
                .unwrap();
            assert!(!result.is_error.unwrap_or(false));
            let result_json = serde_json::to_value(&result).unwrap();
            let text = result_json
                .pointer("/content/0/text")
                .and_then(serde_json::Value::as_str)
                .unwrap();
            let entries: Vec<LogEntrySnapshot> = serde_json::from_str(text).unwrap();
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].content, "supervisor log is still readable");

            let controller_status = server
                .call_tool_by_name_for_session(
                    "get_controller_loop_status",
                    serde_json::json!({}),
                    None,
                    None,
                )
                .await
                .unwrap();
            assert!(!controller_status.is_error.unwrap_or(false));
        });
    }

    #[test]
    fn rewind_only_gate_does_not_block_internal_session() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            s.active_session_source = Some("internal".to_string());
            s.apply_main_usage_snapshot(frontend::ModelUsageSnapshot {
                provider: "openai".to_string(),
                model: "gpt-5.2".to_string(),
                tokens_used: 95_000,
                context_window: 100_000,
                hard_context_window: Some(120_000),
                usage_pct: 95.0,
                prompt_tokens: 90_000,
                completion_tokens: 5_000,
                cached_tokens: 0,
            });

            assert!(s.rewind_only_gate_message("take_screenshot").is_none());
        });
    }

    #[test]
    fn rewind_only_gate_does_not_block_vanilla_codex_session() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            s.active_session_source = Some("codex".to_string());
            s.apply_main_usage_snapshot(frontend::ModelUsageSnapshot {
                provider: "openai".to_string(),
                model: "gpt-5.2-codex".to_string(),
                tokens_used: 95_000,
                context_window: 100_000,
                hard_context_window: Some(120_000),
                usage_pct: 95.0,
                prompt_tokens: 90_000,
                completion_tokens: 5_000,
                cached_tokens: 0,
            });

            assert!(s.rewind_only_gate_message("take_screenshot").is_none());
        });
    }

    #[test]
    fn list_tools_hides_rewind_tools_until_managed_context_is_enabled() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let server = IntendantServer::new(state.clone(), EventBus::new());

            let tools = server.list_tools_json().await;
            let names: Vec<_> = tools["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|tool| tool["name"].as_str())
                .collect();
            assert!(names.contains(&"get_status"));
            assert!(!names.contains(&"list_rewind_anchors"));
            assert!(!names.contains(&"rewind_context"));
            assert!(!names.contains(&"rewind_backout"));

            {
                let mut s = state.write().await;
                s.configured_codex_managed_context = true;
                s.codex_managed_context = true;
            }
            let tools = server.list_tools_json().await;
            let names: Vec<_> = tools["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|tool| tool["name"].as_str())
                .collect();
            assert!(names.contains(&"list_rewind_anchors"));
            assert!(names.contains(&"rewind_context"));
            assert!(names.contains(&"rewind_backout"));
        });
    }

    #[test]
    fn list_tools_uses_session_scoped_managed_context_override() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            {
                let mut s = state.write().await;
                s.configured_codex_managed_context = false;
                s.session_codex_managed_context
                    .insert("vanilla-session".to_string(), false);
                s.session_codex_managed_context
                    .insert("managed-session".to_string(), true);
            }
            let server = IntendantServer::new(state, EventBus::new());

            let vanilla = server
                .list_tools_json_for_session(Some("vanilla-session"), None, None)
                .await;
            let vanilla_names: Vec<_> = vanilla["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|tool| tool["name"].as_str())
                .collect();
            assert!(!vanilla_names.contains(&"list_rewind_anchors"));
            assert!(!vanilla_names.contains(&"rewind_context"));
            assert!(!vanilla_names.contains(&"rewind_backout"));

            let managed = server
                .list_tools_json_for_session(Some("managed-session"), None, None)
                .await;
            let managed_names: Vec<_> = managed["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|tool| tool["name"].as_str())
                .collect();
            assert!(managed_names.contains(&"list_rewind_anchors"));
            assert!(managed_names.contains(&"rewind_context"));
            assert!(managed_names.contains(&"rewind_backout"));

            let managed_by_url = server
                .list_tools_json_for_session(Some("vanilla-session"), Some(true), None)
                .await;
            let managed_by_url_names: Vec<_> = managed_by_url["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|tool| tool["name"].as_str())
                .collect();
            assert!(managed_by_url_names.contains(&"list_rewind_anchors"));
            assert!(managed_by_url_names.contains(&"rewind_context"));
        });
    }

    #[test]
    fn list_tools_core_profile_keeps_only_bootstrap_tools() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            state
                .write()
                .await
                .session_codex_managed_context
                .insert("managed-session".to_string(), true);
            let server = IntendantServer::new(state, EventBus::new());

            let vanilla = server
                .list_tools_json_for_session(None, Some(false), Some("core"))
                .await;
            let vanilla_names: Vec<_> = vanilla["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|tool| tool["name"].as_str())
                .collect();
            assert!(vanilla_names.contains(&"get_status"));
            assert!(vanilla_names.contains(&"show_shared_view"));
            assert!(vanilla_names.contains(&"focus_shared_view"));
            assert!(vanilla_names.contains(&"request_shared_view_input"));
            assert!(vanilla_names.contains(&"capture_shared_view_frame"));
            assert!(vanilla_names.contains(&"hide_shared_view"));
            assert!(!vanilla_names.contains(&"execute_cu_actions"));
            assert!(!vanilla_names.contains(&"spawn_live_audio"));
            assert!(!vanilla_names.contains(&"list_rewind_anchors"));
            assert!(!vanilla_names.contains(&"rewind_context"));

            let managed = server
                .list_tools_json_for_session(Some("managed-session"), None, Some("core"))
                .await;
            let managed_names: Vec<_> = managed["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|tool| tool["name"].as_str())
                .collect();
            assert!(managed_names.contains(&"list_rewind_anchors"));
            assert!(managed_names.contains(&"inspect_rewind_anchor"));
            assert!(managed_names.contains(&"rewind_context"));
            assert!(managed_names.contains(&"rewind_backout"));
            assert!(managed_names.contains(&"list_displays"));
            assert!(managed_names.contains(&"take_screenshot"));
            assert!(managed_names.contains(&"execute_cu_actions"));
            assert!(!managed_names.contains(&"spawn_live_audio"));

            let list_description = managed["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|tool| tool["name"] == "list_rewind_anchors")
                .and_then(|tool| tool["description"].as_str())
                .expect("list_rewind_anchors description");
            assert!(list_description
                .contains("Do not call during ordinary startup/status/search turns"));
            assert!(list_description.contains("bounded low-output searches"));
            assert!(list_description.contains("genuinely noisy/unexpectedly large"));
            assert!(!list_description.contains("call_"));

            let rewind_description = managed["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|tool| tool["name"] == "rewind_context")
                .and_then(|tool| tool["description"].as_str())
                .expect("rewind_context description");
            assert!(rewind_description.contains("do not use for ordinary low-pressure"));
        });
    }

    #[test]
    fn call_tool_rejects_rewind_tools_when_managed_context_is_disabled() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let server = IntendantServer::new(test_state(), EventBus::new());
            let result = server
                .call_tool_by_name(
                    "rewind_context",
                    serde_json::json!({
                        "item_id": "call-1",
                        "primer": "carry forward enough state"
                    }),
                )
                .await
                .unwrap();
            let rendered = format!("{result:?}");
            assert!(rendered.contains("managed context is disabled"));
        });
    }

    #[test]
    fn call_tool_respects_session_scoped_managed_context_override() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            {
                let mut s = state.write().await;
                s.configured_codex_managed_context = true;
            }
            let server = IntendantServer::new(state, EventBus::new());
            let result = server
                .call_tool_by_name_for_session(
                    "rewind_context",
                    serde_json::json!({}),
                    Some("vanilla-session"),
                    Some(false),
                )
                .await
                .unwrap();
            let rendered = format!("{result:?}");
            assert!(rendered.contains("managed context is disabled"));
        });
    }

    #[test]
    fn get_logs_reads_session_scoped_wrapper_session_jsonl() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let _guard = TEST_ENV_LOCK.lock().await;
            let prior_home = std::env::var_os("HOME");
            let prior_userprofile = std::env::var_os("USERPROFILE");
            let home = tempdir().unwrap();
            std::env::set_var("HOME", home.path());
            std::env::set_var("USERPROFILE", home.path());

            let wrapper_session_id = "6eee2a11-51f2-453b-b993-b47744f34792";
            let wrapper_dir = home
                .path()
                .join(".intendant")
                .join("logs")
                .join(wrapper_session_id);
            std::fs::create_dir_all(&wrapper_dir).unwrap();
            std::fs::write(
                wrapper_dir.join("session.jsonl"),
                [
                    serde_json::json!({
                        "ts": "2026-06-06T12:00:00",
                        "event": "info",
                        "level": "info",
                        "message": "wrapper started"
                    })
                    .to_string(),
                    serde_json::json!({
                        "ts": "2026-06-06T12:00:01",
                        "event": "agent_output",
                        "level": "info",
                        "message": "codex output"
                    })
                    .to_string(),
                ]
                .join("\n")
                    + "\n",
            )
            .unwrap();

            let server = IntendantServer::new(test_state(), EventBus::new());
            let result = server
                .call_tool_by_name_for_session(
                    "get_logs",
                    serde_json::json!({ "limit": 40 }),
                    Some(wrapper_session_id),
                    None,
                )
                .await
                .unwrap();
            assert!(!result.is_error.unwrap_or(false));

            let result_json = serde_json::to_value(&result).unwrap();
            let text = result_json
                .pointer("/content/0/text")
                .and_then(serde_json::Value::as_str)
                .unwrap();
            let entries: Vec<LogEntrySnapshot> = serde_json::from_str(text).unwrap();
            assert_eq!(entries.len(), 2);
            assert_eq!(entries[0].content, "wrapper started");
            assert_eq!(entries[1].level, "agent");
            assert_eq!(entries[1].content, "codex output");

            let result = server
                .call_tool_by_name(
                    "get_logs",
                    serde_json::json!({
                        "session_id": wrapper_session_id,
                        "since_id": 0,
                        "level_filter": "agent"
                    }),
                )
                .await
                .unwrap();
            assert!(!result.is_error.unwrap_or(false));
            let result_json = serde_json::to_value(&result).unwrap();
            let text = result_json
                .pointer("/content/0/text")
                .and_then(serde_json::Value::as_str)
                .unwrap();
            let entries: Vec<LogEntrySnapshot> = serde_json::from_str(text).unwrap();
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].id, 1);
            assert_eq!(entries[0].level, "agent");
            assert_eq!(entries[0].content, "codex output");

            match prior_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match prior_userprofile {
                Some(value) => std::env::set_var("USERPROFILE", value),
                None => std::env::remove_var("USERPROFILE"),
            }
        });
    }

    #[test]
    fn rewind_context_defaults_to_http_session_id() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let responder_bus = bus.clone();
            let server = IntendantServer::new(state, bus.clone());

            let event_task = tokio::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(AppEvent::ControlCommand(ControlMsg::CodexThreadAction {
                            session_id,
                            op,
                            params,
                        })) if op == "rewind_context" => {
                            let event = (session_id.clone(), op.clone(), params);
                            responder_bus.send(AppEvent::CodexThreadActionResult {
                                session_id,
                                action: op,
                                success: true,
                                message: "context rewind scheduled".to_string(),
                            });
                            break event;
                        }
                        Ok(_) => continue,
                        Err(e) => panic!("event bus closed: {e}"),
                    }
                }
            });

            let result = server
                .call_tool_by_name_for_session(
                    "rewind_context",
                    serde_json::json!({
                        "anchor": {"item_id": "call-1", "position": "after"},
                        "reason": "trim noisy branch",
                        "primer": "carry forward the durable facts"
                    }),
                    Some("backend-session-1"),
                    Some(true),
                )
                .await
                .unwrap();
            assert!(!result.is_error.unwrap_or(false));
            let result_json = serde_json::to_value(&result).unwrap();
            assert_eq!(
                result_json
                    .pointer("/content/0/text")
                    .and_then(|value| value.as_str()),
                Some("context rewind scheduled")
            );

            let event = timeout(Duration::from_secs(1), event_task)
                .await
                .expect("expected CodexThreadAction control command")
                .unwrap();

            assert_eq!(event.0.as_deref(), Some("backend-session-1"));
            assert_eq!(event.1, "rewind_context");
            assert_eq!(event.2["anchor"]["item_id"], "call-1");
        });
    }

    #[test]
    fn rewind_context_surfaces_validation_failure() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let responder_bus = bus.clone();
            let server = IntendantServer::new(state, bus);

            let result_task = tokio::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(AppEvent::ControlCommand(ControlMsg::CodexThreadAction {
                            session_id,
                            op,
                            ..
                        })) if op == "rewind_context" => {
                            responder_bus.send(AppEvent::CodexThreadActionResult {
                                session_id,
                                action: op,
                                success: false,
                                message:
                                    "rollback anchor item_id `rewind_context-call_6` was not found; call list_rewind_anchors"
                                        .to_string(),
                            });
                            break;
                        }
                        Ok(_) => continue,
                        Err(e) => panic!("event bus closed: {e}"),
                    }
                }
            });

            let result = server
                .call_tool_by_name_for_session(
                    "rewind_context",
                    serde_json::json!({
                        "anchor": {"item_id": "rewind_context-call_6", "position": "after"},
                        "reason": "recover pressure",
                        "primer": "dense continuation"
                    }),
                    Some("backend-session-1"),
                    Some(true),
                )
                .await
                .unwrap();

            let result_json = serde_json::to_value(&result).unwrap();
            let text = result_json
                .pointer("/content/0/text")
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            assert!(text.contains("rewind_context failed"), "got: {text}");
            assert!(text.contains("call list_rewind_anchors"), "got: {text}");
            result_task.await.unwrap();
        });
    }

    #[test]
    fn start_task_defaults_to_http_session_id_and_dispatches_targeted_start() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let server = IntendantServer::new(state, bus.clone());

            let result = server
                .call_tool_by_name_for_session(
                    "start_task",
                    serde_json::json!({
                        "task": "continue existing managed session"
                    }),
                    Some("managed-session-1"),
                    None,
                )
                .await
                .expect("tool should dispatch");
            assert!(!result.is_error.unwrap_or(false));
            assert!(format!("{result:?}").contains("ok (task dispatched)"));

            match timeout(Duration::from_secs(1), rx.recv()).await {
                Ok(Ok(AppEvent::ControlCommand(ControlMsg::StartTask {
                    session_id,
                    task,
                    orchestrate,
                    direct,
                    reference_frame_ids,
                    display_target,
                    attachments,
                    follow_up_id,
                }))) => {
                    assert_eq!(session_id.as_deref(), Some("managed-session-1"));
                    assert_eq!(task, "continue existing managed session");
                    assert_eq!(orchestrate, None);
                    assert_eq!(direct, None);
                    assert!(reference_frame_ids.is_empty());
                    assert!(display_target.is_none());
                    assert!(attachments.is_empty());
                    assert!(follow_up_id.is_none());
                }
                other => panic!("expected targeted StartTask control event, got {other:?}"),
            }
        });
    }

    #[test]
    fn start_task_resumes_persisted_external_wrapper_session() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let _guard = TEST_ENV_LOCK.lock().await;
            let prior_home = std::env::var_os("HOME");
            let prior_userprofile = std::env::var_os("USERPROFILE");
            let home = tempdir().unwrap();
            std::env::set_var("HOME", home.path());
            std::env::set_var("USERPROFILE", home.path());

            let wrapper_session_id = "724fafac-36d7-41e5-b822-e0a08c1f4701";
            let backend_session_id = "019e9f80-bd44-7a00-bcef-f28ff529514e";
            let project_root = home.path().join("project");
            std::fs::create_dir_all(&project_root).unwrap();
            let wrapper_dir = home
                .path()
                .join(".intendant")
                .join("logs")
                .join(wrapper_session_id);
            {
                let mut log = crate::session_log::SessionLog::open(wrapper_dir.clone()).unwrap();
                log.write_meta(Some(&project_root), Some("old task"));
                log.session_identity(wrapper_session_id, "codex", backend_session_id);
            }
            crate::session_config::write_log_dir_config(
                &wrapper_dir,
                &crate::session_config::SessionAgentConfig {
                    source: Some("codex".to_string()),
                    project_root: Some(project_root.to_string_lossy().to_string()),
                    agent_command: Some("/tmp/patched-codex".to_string()),
                    codex_sandbox: Some("danger-full-access".to_string()),
                    codex_approval_policy: Some("never".to_string()),
                    codex_managed_context: Some("managed".to_string()),
                    codex_context_archive: Some("summary".to_string()),
                    codex_service_tier: None,
                    codex_home: Some(home.path().join(".codex").to_string_lossy().to_string()),
                },
            )
            .unwrap();

            let state = test_state();
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let server = IntendantServer::new(state, bus);
            let result = server
                .call_tool_by_name_for_session(
                    "start_task",
                    serde_json::json!({
                        "task": "continue managed station work",
                        "orchestrate": false
                    }),
                    Some(wrapper_session_id),
                    None,
                )
                .await
                .expect("tool should dispatch resume");
            assert!(!result.is_error.unwrap_or(false));
            let rendered = format!("{result:?}");
            assert!(
                rendered.contains("ok (session resume dispatched"),
                "got: {rendered}"
            );

            match timeout(Duration::from_secs(1), rx.recv()).await {
                Ok(Ok(AppEvent::ControlCommand(ControlMsg::ResumeSession {
                    source,
                    session_id,
                    resume_id,
                    project_root: resumed_project_root,
                    task,
                    direct,
                    attachments,
                    agent_command,
                    codex_sandbox,
                    codex_approval_policy,
                    codex_managed_context,
                    codex_context_archive,
                }))) => {
                    assert_eq!(source, "codex");
                    assert_eq!(session_id, wrapper_session_id);
                    assert_eq!(resume_id.as_deref(), Some(backend_session_id));
                    assert_eq!(
                        resumed_project_root.as_deref(),
                        Some(project_root.to_string_lossy().as_ref())
                    );
                    assert_eq!(task.as_deref(), Some("continue managed station work"));
                    assert_eq!(direct, Some(true));
                    assert!(attachments.is_empty());
                    assert_eq!(agent_command.as_deref(), Some("/tmp/patched-codex"));
                    assert_eq!(codex_sandbox.as_deref(), Some("danger-full-access"));
                    assert_eq!(codex_approval_policy.as_deref(), Some("never"));
                    assert_eq!(codex_managed_context.as_deref(), Some("managed"));
                    assert_eq!(codex_context_archive.as_deref(), Some("summary"));
                }
                other => panic!("expected ResumeSession control event, got {other:?}"),
            }

            match prior_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match prior_userprofile {
                Some(value) => std::env::set_var("USERPROFILE", value),
                None => std::env::remove_var("USERPROFILE"),
            }
        });
    }

    #[test]
    fn start_task_resumes_known_idle_persisted_external_wrapper_session() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let _guard = TEST_ENV_LOCK.lock().await;
            let prior_home = std::env::var_os("HOME");
            let prior_userprofile = std::env::var_os("USERPROFILE");
            let home = tempdir().unwrap();
            std::env::set_var("HOME", home.path());
            std::env::set_var("USERPROFILE", home.path());

            let wrapper_session_id = "540b8411-4fd1-4210-9374-c9d58430f6e6";
            let backend_session_id = "019ea0a9-92fc-7471-85d8-0a281fc54250";
            let project_root = home.path().join("project");
            std::fs::create_dir_all(&project_root).unwrap();
            let wrapper_dir = home
                .path()
                .join(".intendant")
                .join("logs")
                .join(wrapper_session_id);
            {
                let mut log = crate::session_log::SessionLog::open(wrapper_dir.clone()).unwrap();
                log.write_meta(Some(&project_root), Some("previous external task"));
                log.session_identity(wrapper_session_id, "codex", backend_session_id);
            }
            crate::session_config::write_log_dir_config(
                &wrapper_dir,
                &crate::session_config::SessionAgentConfig {
                    source: Some("codex".to_string()),
                    project_root: Some(project_root.to_string_lossy().to_string()),
                    agent_command: Some("/tmp/patched-codex".to_string()),
                    codex_sandbox: Some("danger-full-access".to_string()),
                    codex_approval_policy: Some("never".to_string()),
                    codex_managed_context: Some("managed".to_string()),
                    codex_context_archive: Some("summary".to_string()),
                    codex_service_tier: None,
                    codex_home: Some(home.path().join(".codex").to_string_lossy().to_string()),
                },
            )
            .unwrap();

            let state = test_state();
            {
                let mut s = state.write().await;
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::StatusUpdate {
                        turn: 0,
                        phase: "idle".to_string(),
                        autonomy: "medium".to_string(),
                        session_id: wrapper_session_id.to_string(),
                        task: "previous external task".to_string(),
                    },
                );
            }
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let server = IntendantServer::new(state, bus);

            let result = server
                .call_tool_by_name_for_session(
                    "start_task",
                    serde_json::json!({
                        "task": "continue idle wrapper",
                        "orchestrate": false
                    }),
                    Some(wrapper_session_id),
                    None,
                )
                .await
                .expect("tool should dispatch resume for an idle persisted external wrapper");
            assert!(!result.is_error.unwrap_or(false));
            let rendered = format!("{result:?}");
            assert!(
                rendered.contains("ok (session resume dispatched"),
                "got: {rendered}"
            );

            match timeout(Duration::from_secs(1), rx.recv()).await {
                Ok(Ok(AppEvent::ControlCommand(ControlMsg::ResumeSession {
                    source,
                    session_id,
                    resume_id,
                    project_root: resumed_project_root,
                    task,
                    direct,
                    attachments,
                    agent_command,
                    codex_sandbox,
                    codex_approval_policy,
                    codex_managed_context,
                    codex_context_archive,
                }))) => {
                    assert_eq!(source, "codex");
                    assert_eq!(session_id, wrapper_session_id);
                    assert_eq!(resume_id.as_deref(), Some(backend_session_id));
                    assert_eq!(
                        resumed_project_root.as_deref(),
                        Some(project_root.to_string_lossy().as_ref())
                    );
                    assert_eq!(task.as_deref(), Some("continue idle wrapper"));
                    assert_eq!(direct, Some(true));
                    assert!(attachments.is_empty());
                    assert_eq!(agent_command.as_deref(), Some("/tmp/patched-codex"));
                    assert_eq!(codex_sandbox.as_deref(), Some("danger-full-access"));
                    assert_eq!(codex_approval_policy.as_deref(), Some("never"));
                    assert_eq!(codex_managed_context.as_deref(), Some("managed"));
                    assert_eq!(codex_context_archive.as_deref(), Some("summary"));
                }
                other => panic!("expected ResumeSession control event, got {other:?}"),
            }

            match prior_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match prior_userprofile {
                Some(value) => std::env::set_var("USERPROFILE", value),
                None => std::env::remove_var("USERPROFILE"),
            }
        });
    }

    #[test]
    fn start_task_targets_active_external_session_without_re_resuming() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let _guard = TEST_ENV_LOCK.lock().await;
            let prior_home = std::env::var_os("HOME");
            let prior_userprofile = std::env::var_os("USERPROFILE");
            let home = tempdir().unwrap();
            std::env::set_var("HOME", home.path());
            std::env::set_var("USERPROFILE", home.path());

            let wrapper_session_id = "62e6f9d9-06e9-420b-9245-9d0221e47c78";
            let backend_session_id = "019e9f97-active-backend";
            let project_root = home.path().join("project");
            std::fs::create_dir_all(&project_root).unwrap();
            let wrapper_dir = home
                .path()
                .join(".intendant")
                .join("logs")
                .join(wrapper_session_id);
            {
                let mut log = crate::session_log::SessionLog::open(wrapper_dir.clone()).unwrap();
                log.write_meta(Some(&project_root), Some("old task"));
                log.session_identity(wrapper_session_id, "codex", backend_session_id);
            }

            let state = test_state();
            {
                let mut s = state.write().await;
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::StatusUpdate {
                        turn: 7,
                        phase: "waiting_follow_up".to_string(),
                        autonomy: "medium".to_string(),
                        session_id: wrapper_session_id.to_string(),
                        task: "active managed Codex session".to_string(),
                    },
                );
            }
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let server = IntendantServer::new(state, bus);

            let result = server
                .call_tool_by_name_for_session(
                    "start_task",
                    serde_json::json!({
                        "task": "continue active managed station work"
                    }),
                    Some(wrapper_session_id),
                    None,
                )
                .await
                .expect("tool should dispatch active follow-up");
            assert!(!result.is_error.unwrap_or(false));
            assert!(format!("{result:?}").contains("ok (task dispatched)"));

            match timeout(Duration::from_secs(1), rx.recv()).await {
                Ok(Ok(AppEvent::ControlCommand(ControlMsg::StartTask {
                    session_id,
                    task,
                    ..
                }))) => {
                    assert_eq!(session_id.as_deref(), Some(wrapper_session_id));
                    assert_eq!(task, "continue active managed station work");
                }
                other => panic!("expected active StartTask control event, got {other:?}"),
            }

            match prior_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match prior_userprofile {
                Some(value) => std::env::set_var("USERPROFILE", value),
                None => std::env::remove_var("USERPROFILE"),
            }
        });
    }

    #[test]
    fn start_task_targeting_running_codex_reports_follow_up_queued() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let wrapper_session_id = "17ea6240-138a-4db6-8954-22f11437aa0d";
            let backend_session_id = "019e9fa2-active-turn";
            let state = test_state();
            {
                let mut s = state.write().await;
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::SessionIdentity {
                        session_id: wrapper_session_id.to_string(),
                        source: "codex".to_string(),
                        backend_session_id: backend_session_id.to_string(),
                    },
                );
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::StatusUpdate {
                        turn: 9,
                        phase: "thinking".to_string(),
                        autonomy: "medium".to_string(),
                        session_id: wrapper_session_id.to_string(),
                        task: "active managed Codex turn".to_string(),
                    },
                );
            }
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let server = IntendantServer::new(state, bus);

            let result = server
                .call_tool_by_name_for_session(
                    "start_task",
                    serde_json::json!({
                        "task": "please prioritize the harness status fix"
                    }),
                    Some(wrapper_session_id),
                    None,
                )
                .await
                .expect("tool should queue active-turn follow-up");
            assert!(!result.is_error.unwrap_or(false));
            let rendered = format!("{result:?}");
            assert!(
                rendered.contains(
                    "ok (follow-up queued for next turn; active Codex turn is still running)"
                ),
                "got: {rendered}"
            );
            assert!(
                !rendered.contains("ok (task dispatched)"),
                "active-turn follow-up must not look actively dispatched: {rendered}"
            );

            match timeout(Duration::from_secs(1), rx.recv()).await {
                Ok(Ok(AppEvent::ControlCommand(ControlMsg::StartTask {
                    session_id,
                    task,
                    ..
                }))) => {
                    assert_eq!(session_id.as_deref(), Some(wrapper_session_id));
                    assert_eq!(task, "please prioritize the harness status fix");
                }
                other => panic!("expected queued StartTask control event, got {other:?}"),
            }
        });
    }

    #[test]
    fn start_task_rejects_persisted_non_external_inactive_session_without_silent_ok() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let _guard = TEST_ENV_LOCK.lock().await;
            let prior_home = std::env::var_os("HOME");
            let prior_userprofile = std::env::var_os("USERPROFILE");
            let home = tempdir().unwrap();
            std::env::set_var("HOME", home.path());
            std::env::set_var("USERPROFILE", home.path());

            let session_id = "b74df098-9823-4f73-8ddf-e27bcb92f923";
            let project_root = home.path().join("project");
            std::fs::create_dir_all(&project_root).unwrap();
            let log_dir = home.path().join(".intendant").join("logs").join(session_id);
            {
                let log = crate::session_log::SessionLog::open(log_dir).unwrap();
                log.write_meta(Some(&project_root), Some("old native task"));
            }

            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let server = IntendantServer::new(test_state(), bus);
            let result = server
                .call_tool_by_name_for_session(
                    "start_task",
                    serde_json::json!({
                        "task": "continue native session"
                    }),
                    Some(session_id),
                    None,
                )
                .await
                .expect("tool should return a clear rejection");
            let rendered = format!("{result:?}");
            assert!(rendered.contains("Cannot start task"), "got: {rendered}");
            assert!(
                rendered.contains("not a persisted external-agent wrapper"),
                "got: {rendered}"
            );
            assert!(
                timeout(Duration::from_millis(100), rx.recv())
                    .await
                    .is_err(),
                "inactive persisted non-external session should not broadcast a misleading StartTask"
            );

            match prior_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match prior_userprofile {
                Some(value) => std::env::set_var("USERPROFILE", value),
                None => std::env::remove_var("USERPROFILE"),
            }
        });
    }

    #[test]
    fn start_task_rejects_known_terminal_target_session() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            {
                let mut s = state.write().await;
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::StatusUpdate {
                        turn: 3,
                        phase: "done".to_string(),
                        autonomy: "medium".to_string(),
                        session_id: "724fafac-36d7-41e5-b822-e0a08c1f4701".to_string(),
                        task: "stopped managed Codex session".to_string(),
                    },
                );
            }
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let server = IntendantServer::new(state, bus);

            let result = server
                .call_tool_by_name_for_session(
                    "start_task",
                    serde_json::json!({
                        "task": "continue existing managed session"
                    }),
                    Some("724fafac-36d7-41e5-b822-e0a08c1f4701"),
                    None,
                )
                .await
                .expect("tool should return a rejection");
            let text = format!("{result:?}");
            assert!(text.contains("Cannot start task"), "got: {text}");
            assert!(text.contains("phase done"), "got: {text}");
            assert!(
                timeout(Duration::from_millis(100), rx.recv())
                    .await
                    .is_err(),
                "terminal targeted start should not broadcast a StartTask event"
            );
        });
    }

    #[test]
    fn start_task_without_session_still_requires_launcher_for_new_task() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let server = IntendantServer::new(state, EventBus::new());

            let result = server
                .call_tool_by_name_for_session(
                    "start_task",
                    serde_json::json!({
                        "task": "start a new task"
                    }),
                    None,
                    None,
                )
                .await
                .expect("tool should return a text result");
            assert!(
                format!("{result:?}").contains("Cannot start task: no task launcher configured")
            );
        });
    }

    #[test]
    fn list_rewind_anchors_defaults_to_http_session_id_and_returns_result() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let responder_bus = bus.clone();
            let server = IntendantServer::new(state, bus);

            let result_task = tokio::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(AppEvent::ControlCommand(ControlMsg::CodexThreadAction {
                            session_id,
                            op,
                            params,
                        })) if op == "list_rewind_anchors" => {
                            assert_eq!(session_id.as_deref(), Some("backend-session-1"));
                            assert_eq!(params["offset"], 25);
                            assert_eq!(params["limit"], 50);
                            assert_eq!(params["query"], "tool");
                            assert_eq!(params["reverse"], true);
                            assert_eq!(params["include_pruning_estimates"], true);
                            assert_eq!(params["recovery_candidates_only"], true);
                            assert_eq!(params["include_non_recovery"], false);
                            responder_bus.send(AppEvent::CodexThreadActionResult {
                                session_id,
                                action: op,
                                success: true,
                                message: "{\"anchors\":[]}".to_string(),
                            });
                            break;
                        }
                        Ok(_) => continue,
                        Err(e) => panic!("event bus closed: {e}"),
                    }
                }
            });

            let result = server
                .call_tool_by_name_for_session(
                    "list_rewind_anchors",
                    serde_json::json!({
                        "offset": 25,
                        "limit": 50,
                        "query": "tool",
                        "reverse": true,
                        "include_pruning_estimates": true,
                        "recovery_candidates_only": false
                    }),
                    Some("backend-session-1"),
                    Some(true),
                )
                .await
                .unwrap();

            assert!(!result.is_error.unwrap_or(false));
            let result_json = serde_json::to_value(&result).unwrap();
            assert_eq!(
                result_json
                    .pointer("/content/0/text")
                    .and_then(|value| value.as_str()),
                Some("{\"anchors\":[]}")
            );
            result_task.await.unwrap();
        });
    }

    #[test]
    fn list_rewind_anchors_omits_limit_when_unspecified() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let responder_bus = bus.clone();
            let server = IntendantServer::new(state, bus);

            let result_task = tokio::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(AppEvent::ControlCommand(ControlMsg::CodexThreadAction {
                            session_id,
                            op,
                            params,
                        })) if op == "list_rewind_anchors" => {
                            assert_eq!(session_id.as_deref(), Some("backend-session-1"));
                            assert_eq!(params["offset"], 0);
                            assert!(
                                params.get("limit").is_none(),
                                "unspecified limit should let the backend compact default apply: {params}"
                            );
                            responder_bus.send(AppEvent::CodexThreadActionResult {
                                session_id,
                                action: op,
                                success: true,
                                message: "{\"anchors\":[],\"limit\":5}".to_string(),
                            });
                            break;
                        }
                        Ok(_) => continue,
                        Err(e) => panic!("event bus closed: {e}"),
                    }
                }
            });

            let result = server
                .call_tool_by_name_for_session(
                    "list_rewind_anchors",
                    serde_json::json!({}),
                    Some("backend-session-1"),
                    Some(true),
                )
                .await
                .unwrap();

            assert!(!result.is_error.unwrap_or(false));
            let result_json = serde_json::to_value(&result).unwrap();
            assert_eq!(
                result_json
                    .pointer("/content/0/text")
                    .and_then(|value| value.as_str()),
                Some("{\"anchors\":[],\"limit\":5}")
            );
            result_task.await.unwrap();
        });
    }

    #[test]
    fn inspect_rewind_anchor_defaults_to_http_session_id_and_returns_result() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let responder_bus = bus.clone();
            let server = IntendantServer::new(state, bus);

            let result_task = tokio::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(AppEvent::ControlCommand(ControlMsg::CodexThreadAction {
                            session_id,
                            op,
                            params,
                        })) if op == "inspect_rewind_anchor" => {
                            assert_eq!(session_id.as_deref(), Some("backend-session-1"));
                            assert_eq!(params["anchor"]["item_id"], "call-1");
                            assert_eq!(params["radius"], 3);
                            responder_bus.send(AppEvent::CodexThreadActionResult {
                                session_id,
                                action: op,
                                success: true,
                                message: "{\"anchor\":{\"item_id\":\"call-1\"}}".to_string(),
                            });
                            break;
                        }
                        Ok(_) => continue,
                        Err(e) => panic!("event bus closed: {e}"),
                    }
                }
            });

            let result = server
                .call_tool_by_name_for_session(
                    "inspect_rewind_anchor",
                    serde_json::json!({
                        "item_id": "call-1",
                        "radius": 3
                    }),
                    Some("backend-session-1"),
                    Some(true),
                )
                .await
                .unwrap();

            assert!(!result.is_error.unwrap_or(false));
            let result_json = serde_json::to_value(&result).unwrap();
            assert_eq!(
                result_json
                    .pointer("/content/0/text")
                    .and_then(|value| value.as_str()),
                Some("{\"anchor\":{\"item_id\":\"call-1\"}}")
            );
            result_task.await.unwrap();
        });
    }

    #[test]
    fn observed_codex_config_change_toggles_managed_context() {
        let mut s = McpAppState::new(
            "none".to_string(),
            "none".to_string(),
            autonomy::shared_autonomy(AutonomyState::default()),
            std::path::PathBuf::from("/tmp/test_session"),
        );

        assert!(!s.codex_managed_context);
        assert!(apply_observed_event_to_mcp_state(
            &mut s,
            &AppEvent::CodexConfigChanged {
                command: None,
                sandbox: None,
                approval_policy: None,
                model: None,
                model_cleared: false,
                reasoning_effort: None,
                reasoning_effort_cleared: false,
                service_tier: None,
                service_tier_cleared: false,
                web_search: None,
                network_access: None,
                writable_roots: None,
                managed_context: Some("managed".to_string()),
                context_archive: None,
            },
        ));
        assert!(s.codex_managed_context);
    }

    #[test]
    fn observed_codex_config_change_does_not_mutate_active_session_capability() {
        let mut s = McpAppState::new(
            "none".to_string(),
            "none".to_string(),
            autonomy::shared_autonomy(AutonomyState::default()),
            std::path::PathBuf::from("/tmp/test_session"),
        );
        s.active_session_source = Some("codex".to_string());

        assert!(apply_observed_event_to_mcp_state(
            &mut s,
            &AppEvent::CodexConfigChanged {
                command: None,
                sandbox: None,
                approval_policy: None,
                model: None,
                model_cleared: false,
                reasoning_effort: None,
                reasoning_effort_cleared: false,
                service_tier: None,
                service_tier_cleared: false,
                web_search: None,
                network_access: None,
                writable_roots: None,
                managed_context: Some("managed".to_string()),
                context_archive: None,
            },
        ));
        assert!(s.configured_codex_managed_context);
        assert!(
            !s.codex_managed_context,
            "active Codex session capability should not flip until next task"
        );
    }

    #[test]
    fn context_rewind_record_id_from_message_extracts_rewind_id() {
        assert_eq!(
            context_rewind_record_id_from_message(
                "Rewound Codex thread to item call-old and saved record rewind-abc_123.",
            )
            .as_deref(),
            Some("rewind-abc_123")
        );
        assert_eq!(
            context_rewind_record_id_from_message("rewind completed without a durable record"),
            None
        );
    }

    #[test]
    fn observed_successful_rewind_then_high_usage_marks_rewind_insufficient() {
        let mut s = McpAppState::new(
            "none".to_string(),
            "none".to_string(),
            autonomy::shared_autonomy(AutonomyState::default()),
            std::path::PathBuf::from("/tmp/test_session"),
        );
        s.session_id = "codex-thread".to_string();
        s.active_session_source = Some("codex".to_string());
        s.codex_managed_context = true;

        apply_observed_event_to_mcp_state(
            &mut s,
            &AppEvent::CodexThreadActionResult {
                session_id: Some("codex-thread".to_string()),
                action: "rewind_context".to_string(),
                success: true,
                message: "Rewound Codex thread to item call-old and saved record rewind-high."
                    .to_string(),
            },
        );
        apply_observed_event_to_mcp_state(
            &mut s,
            &AppEvent::UsageSnapshot {
                session_id: Some("codex-thread".to_string()),
                main: frontend::ModelUsageSnapshot {
                    provider: "openai".to_string(),
                    model: "gpt-5.2-codex".to_string(),
                    tokens_used: 101_000,
                    context_window: 100_000,
                    hard_context_window: Some(120_000),
                    usage_pct: 101.0,
                    prompt_tokens: 96_000,
                    completion_tokens: 5_000,
                    cached_tokens: 0,
                },
                presence: None,
            },
        );

        let notice = s
            .insufficient_rewind_notices
            .get("codex-thread")
            .expect("high pressure after rewind should be remembered");
        assert_eq!(notice.record_id, "rewind-high");
        assert_eq!(notice.used_tokens, 101_000);
        assert_eq!(notice.rewind_only_limit, 100_000);
        assert!(s
            .pending_rewind_pressure_checks
            .get("codex-thread")
            .is_none());

        let pressure = s.context_pressure_snapshot();
        assert_eq!(
            pressure.pointer("/last_rewind_insufficient/record_id"),
            Some(&serde_json::Value::String("rewind-high".to_string()))
        );
        let gate = s
            .rewind_only_gate_message("execute_cu_actions")
            .expect("high Codex pressure should gate non-rewind tools");
        assert!(gate.contains("was insufficient"));
        assert!(gate.contains("rewind-high"));
        assert!(
            !gate.contains("call-old"),
            "gate should not prescribe the stale insufficient anchor"
        );
    }

    #[test]
    fn successful_rewind_then_low_usage_clears_pending_insufficient_notice() {
        let mut s = McpAppState::new(
            "none".to_string(),
            "none".to_string(),
            autonomy::shared_autonomy(AutonomyState::default()),
            std::path::PathBuf::from("/tmp/test_session"),
        );
        s.session_id = "codex-thread".to_string();
        s.active_session_source = Some("codex".to_string());
        s.codex_managed_context = true;
        s.insufficient_rewind_notices.insert(
            "codex-thread".to_string(),
            InsufficientRewindNotice {
                record_id: "rewind-old".to_string(),
                used_tokens: 95_000,
                rewind_only_limit: 100_000,
                context_window: 100_000,
            },
        );

        apply_observed_event_to_mcp_state(
            &mut s,
            &AppEvent::CodexThreadActionResult {
                session_id: Some("codex-thread".to_string()),
                action: "rewind_context".to_string(),
                success: true,
                message: "Rewound Codex thread and saved record rewind-ok.".to_string(),
            },
        );
        apply_observed_event_to_mcp_state(
            &mut s,
            &AppEvent::UsageSnapshot {
                session_id: Some("codex-thread".to_string()),
                main: frontend::ModelUsageSnapshot {
                    provider: "openai".to_string(),
                    model: "gpt-5.2-codex".to_string(),
                    tokens_used: 70_000,
                    context_window: 100_000,
                    hard_context_window: Some(120_000),
                    usage_pct: 70.0,
                    prompt_tokens: 68_000,
                    completion_tokens: 2_000,
                    cached_tokens: 0,
                },
                presence: None,
            },
        );

        assert!(s
            .pending_rewind_pressure_checks
            .get("codex-thread")
            .is_none());
        assert!(s.insufficient_rewind_notices.get("codex-thread").is_none());
        assert_eq!(
            s.context_pressure_snapshot()["last_rewind_insufficient"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn insufficient_rewind_notice_is_session_scoped() {
        let mut s = McpAppState::new(
            "none".to_string(),
            "none".to_string(),
            autonomy::shared_autonomy(AutonomyState::default()),
            std::path::PathBuf::from("/tmp/test_session"),
        );
        s.session_id = "session-a".to_string();
        s.active_session_source = Some("codex".to_string());
        s.codex_managed_context = true;
        s.session_codex_managed_context
            .insert("session-a".to_string(), true);
        s.session_codex_managed_context
            .insert("session-b".to_string(), true);
        s.session_usage.insert(
            "session-b".to_string(),
            frontend::ModelUsageSnapshot {
                provider: "openai".to_string(),
                model: "gpt-5.2-codex".to_string(),
                tokens_used: 101_000,
                context_window: 100_000,
                hard_context_window: Some(120_000),
                usage_pct: 101.0,
                prompt_tokens: 97_000,
                completion_tokens: 4_000,
                cached_tokens: 0,
            },
        );

        s.note_context_rewind_result_for(
            Some("session-b"),
            true,
            "Rewound Codex thread and saved record rewind-b.",
        );
        s.complete_pending_rewind_pressure_check_for(Some("session-b"));

        assert_eq!(
            s.context_pressure_snapshot_for(Some("session-a"), None)
                .pointer("/last_rewind_insufficient"),
            Some(&serde_json::Value::Null)
        );
        assert_eq!(
            s.context_pressure_snapshot_for(Some("session-b"), None)
                .pointer("/last_rewind_insufficient/record_id"),
            Some(&serde_json::Value::String("rewind-b".to_string()))
        );
    }

    #[test]
    fn insufficient_rewind_notice_resolves_through_session_identity_alias() {
        let mut s = McpAppState::new(
            "none".to_string(),
            "none".to_string(),
            autonomy::shared_autonomy(AutonomyState::default()),
            std::path::PathBuf::from("/tmp/test_session"),
        );
        apply_observed_event_to_mcp_state(
            &mut s,
            &AppEvent::SessionCapabilities {
                session_id: "wrapper-session".to_string(),
                capabilities: crate::types::SessionCapabilities {
                    follow_up: true,
                    steer: true,
                    interrupt: true,
                    codex_thread_actions: vec!["rewind_context".to_string()],
                    codex_managed_context: Some("managed".to_string()),
                    codex_sandbox: Some("danger-full-access".to_string()),
                    codex_approval_policy: Some("never".to_string()),
                    codex_context_archive: None,
                    codex_command: Some("/tmp/codex".to_string()),
                    codex_fast_mode: None,
                    codex_service_tier: None,
                },
            },
        );
        apply_observed_event_to_mcp_state(
            &mut s,
            &AppEvent::SessionIdentity {
                session_id: "wrapper-session".to_string(),
                source: "codex".to_string(),
                backend_session_id: "codex-thread".to_string(),
            },
        );
        s.session_usage.insert(
            "codex-thread".to_string(),
            frontend::ModelUsageSnapshot {
                provider: "openai".to_string(),
                model: "gpt-5.2-codex".to_string(),
                tokens_used: 101_000,
                context_window: 100_000,
                hard_context_window: Some(120_000),
                usage_pct: 101.0,
                prompt_tokens: 97_000,
                completion_tokens: 4_000,
                cached_tokens: 0,
            },
        );

        s.note_context_rewind_result_for(
            Some("wrapper-session"),
            true,
            "Rewound Codex thread and saved record rewind-alias.",
        );
        s.complete_pending_rewind_pressure_check_for(Some("codex-thread"));

        assert_eq!(
            s.context_pressure_snapshot_for(Some("wrapper-session"), None)
                .pointer("/last_rewind_insufficient/record_id"),
            Some(&serde_json::Value::String("rewind-alias".to_string()))
        );
        assert_eq!(
            s.context_pressure_snapshot_for(Some("codex-thread"), None)
                .pointer("/last_rewind_insufficient/record_id"),
            Some(&serde_json::Value::String("rewind-alias".to_string()))
        );
    }

    #[test]
    fn spawn_event_listener_tracks_rewind_result_for_stdio_mcp_state() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            {
                let mut s = state.write().await;
                s.session_id = "codex-thread".to_string();
                s.active_session_source = Some("codex".to_string());
                s.codex_managed_context = true;
            }
            let bus = EventBus::new();
            let listener = spawn_event_listener(
                state.clone(),
                bus.subscribe(),
                Arc::new(Mutex::new(None)),
                bus.clone(),
                None,
                None,
            );

            bus.send(AppEvent::CodexThreadActionResult {
                session_id: Some("codex-thread".to_string()),
                action: "rewind_context".to_string(),
                success: true,
                message: "Rewound Codex thread and saved record rewind-listener.".to_string(),
            });
            bus.send(AppEvent::UsageSnapshot {
                session_id: Some("codex-thread".to_string()),
                main: frontend::ModelUsageSnapshot {
                    provider: "openai".to_string(),
                    model: "gpt-5.2-codex".to_string(),
                    tokens_used: 101_000,
                    context_window: 100_000,
                    hard_context_window: Some(120_000),
                    usage_pct: 101.0,
                    prompt_tokens: 96_000,
                    completion_tokens: 5_000,
                    cached_tokens: 0,
                },
                presence: None,
            });

            timeout(Duration::from_secs(1), async {
                loop {
                    if state
                        .read()
                        .await
                        .insufficient_rewind_notices
                        .get("codex-thread")
                        .is_some_and(|notice| notice.record_id == "rewind-listener")
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("listener should mirror rewind pressure state");

            listener.abort();
        });
    }

    #[test]
    fn spawn_event_listener_updates_wrapper_usage_from_backend_alias_sample() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            {
                let mut s = state.write().await;
                s.session_id = "wrapper-session".to_string();
                s.active_session_source = Some("codex".to_string());
                s.codex_managed_context = true;
                s.configured_codex_managed_context = true;
            }
            let bus = EventBus::new();
            let listener = spawn_event_listener(
                state.clone(),
                bus.subscribe(),
                Arc::new(Mutex::new(None)),
                bus.clone(),
                None,
                None,
            );

            bus.send(AppEvent::SessionCapabilities {
                session_id: "wrapper-session".to_string(),
                capabilities: crate::types::SessionCapabilities {
                    follow_up: true,
                    steer: true,
                    interrupt: true,
                    codex_thread_actions: vec!["rewind_context".to_string()],
                    codex_managed_context: Some("managed".to_string()),
                    codex_sandbox: Some("danger-full-access".to_string()),
                    codex_approval_policy: Some("never".to_string()),
                    codex_context_archive: None,
                    codex_command: Some("/tmp/codex".to_string()),
                    codex_fast_mode: None,
                    codex_service_tier: None,
                },
            });
            bus.send(AppEvent::SessionIdentity {
                session_id: "wrapper-session".to_string(),
                source: "codex".to_string(),
                backend_session_id: "codex-thread".to_string(),
            });
            bus.send(AppEvent::UsageSnapshot {
                session_id: Some("wrapper-session".to_string()),
                main: frontend::ModelUsageSnapshot {
                    provider: "openai".to_string(),
                    model: "gpt-5.2-codex".to_string(),
                    tokens_used: 260_000,
                    context_window: 258_400,
                    hard_context_window: Some(272_000),
                    usage_pct: 100.6,
                    prompt_tokens: 259_000,
                    completion_tokens: 1_000,
                    cached_tokens: 10_000,
                },
                presence: None,
            });
            bus.send(AppEvent::UsageSnapshot {
                session_id: Some("codex-thread".to_string()),
                main: frontend::ModelUsageSnapshot {
                    provider: "openai".to_string(),
                    model: "gpt-5.2-codex".to_string(),
                    tokens_used: 70_046,
                    context_window: 258_400,
                    hard_context_window: Some(272_000),
                    usage_pct: 27.1,
                    prompt_tokens: 69_000,
                    completion_tokens: 1_046,
                    cached_tokens: 50_000,
                },
                presence: None,
            });

            timeout(Duration::from_secs(1), async {
                loop {
                    let backend_seen = state
                        .read()
                        .await
                        .session_usage
                        .get("codex-thread")
                        .is_some_and(|usage| usage.tokens_used == 70_046);
                    if backend_seen {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("listener should observe backend usage sample");

            let server = IntendantServer::new(state.clone(), EventBus::new());
            let status = server
                .get_status_for_session(Some("wrapper-session"), None)
                .await;
            let value: serde_json::Value = serde_json::from_str(&status).unwrap();
            assert_eq!(
                value.pointer("/context_pressure/used_tokens"),
                Some(&70_046.into())
            );
            assert_eq!(
                value.pointer("/context_pressure/status"),
                Some(&"ok".into())
            );
            assert_eq!(
                value.pointer("/usage/main/tokens_used"),
                Some(&70_046.into())
            );
            assert!(
                state
                    .read()
                    .await
                    .rewind_only_gate_message("execute_cu_actions")
                    .is_none(),
                "latest backend alias usage should clear the default active-session gate"
            );

            listener.abort();
        });
    }

    #[test]
    fn observed_session_identity_and_usage_enable_codex_gate() {
        let mut s = McpAppState::new(
            "none".to_string(),
            "none".to_string(),
            autonomy::shared_autonomy(AutonomyState::default()),
            std::path::PathBuf::from("/tmp/test_session"),
        );
        s.configured_codex_managed_context = true;
        s.codex_managed_context = true;

        apply_observed_event_to_mcp_state(
            &mut s,
            &AppEvent::SessionIdentity {
                session_id: "wrapper-session".to_string(),
                source: "codex".to_string(),
                backend_session_id: "codex-thread".to_string(),
            },
        );
        apply_observed_event_to_mcp_state(
            &mut s,
            &AppEvent::SessionStarted {
                session_id: "codex-thread".to_string(),
                task: Some("audit".to_string()),
            },
        );
        apply_observed_event_to_mcp_state(
            &mut s,
            &AppEvent::UsageSnapshot {
                session_id: Some("codex-thread".to_string()),
                main: frontend::ModelUsageSnapshot {
                    provider: "openai".to_string(),
                    model: "gpt-5.2-codex".to_string(),
                    tokens_used: 100_000,
                    context_window: 100_000,
                    hard_context_window: Some(120_000),
                    usage_pct: 100.0,
                    prompt_tokens: 95_000,
                    completion_tokens: 5_000,
                    cached_tokens: 0,
                },
                presence: None,
            },
        );

        assert_eq!(s.active_session_source.as_deref(), Some("codex"));
        assert!(s.rewind_only_gate_message("execute_cu_actions").is_some());
    }

    #[test]
    fn observed_session_capabilities_follow_codex_backend_identity() {
        let mut s = McpAppState::new(
            "none".to_string(),
            "none".to_string(),
            autonomy::shared_autonomy(AutonomyState::default()),
            std::path::PathBuf::from("/tmp/test_session"),
        );

        apply_observed_event_to_mcp_state(
            &mut s,
            &AppEvent::SessionCapabilities {
                session_id: "intendant-session".to_string(),
                capabilities: crate::types::SessionCapabilities {
                    follow_up: true,
                    steer: true,
                    interrupt: true,
                    codex_thread_actions: vec!["undo".to_string()],
                    codex_managed_context: Some("managed".to_string()),
                    codex_sandbox: Some("danger-full-access".to_string()),
                    codex_approval_policy: Some("never".to_string()),
                    codex_context_archive: None,
                    codex_command: Some("/opt/codex/bin/codex".to_string()),
                    codex_fast_mode: Some(true),
                    codex_service_tier: Some("priority".to_string()),
                },
            },
        );
        assert_eq!(
            s.session_codex_managed_context
                .get("intendant-session")
                .copied(),
            Some(true)
        );

        apply_observed_event_to_mcp_state(
            &mut s,
            &AppEvent::SessionIdentity {
                session_id: "intendant-session".to_string(),
                source: "codex".to_string(),
                backend_session_id: "codex-thread".to_string(),
            },
        );
        apply_observed_event_to_mcp_state(
            &mut s,
            &AppEvent::SessionStarted {
                session_id: "codex-thread".to_string(),
                task: Some("managed dashboard e2e".to_string()),
            },
        );

        assert_eq!(s.session_id, "codex-thread");
        assert_eq!(s.active_session_source.as_deref(), Some("codex"));
        assert!(s.codex_managed_context);
        assert_eq!(
            s.context_pressure_snapshot()
                .pointer("/managed_context")
                .and_then(serde_json::Value::as_str),
            Some("managed")
        );
    }

    #[test]
    fn get_status_includes_usage_and_context_pressure() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            {
                let mut s = state.write().await;
                s.apply_main_usage_snapshot(frontend::ModelUsageSnapshot {
                    provider: "openai".to_string(),
                    model: "gpt-5.2-codex".to_string(),
                    tokens_used: 125,
                    context_window: 1_000,
                    hard_context_window: Some(1_200),
                    usage_pct: 12.5,
                    prompt_tokens: 100,
                    completion_tokens: 25,
                    cached_tokens: 50,
                });
            }
            let server = IntendantServer::new(state, EventBus::new());
            let status = server.get_status().await;
            let value: serde_json::Value = serde_json::from_str(&status).unwrap();
            assert_eq!(value.pointer("/usage/main/tokens_used"), Some(&125.into()));
            assert_eq!(
                value.pointer("/usage/main/context_window"),
                Some(&1000.into())
            );
            assert_eq!(
                value.pointer("/context_pressure/status"),
                Some(&"ok".into())
            );
            assert_eq!(
                value.pointer("/context_pressure/source"),
                Some(&"backend_reported".into())
            );
        });
    }

    #[test]
    fn get_status_uses_session_scoped_usage_and_managed_context() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            {
                let mut s = state.write().await;
                s.session_id = "global-session".to_string();
                s.session_tokens = 10;
                s.context_window = 1_000;
                s.session_usage.insert(
                    "managed-session".to_string(),
                    frontend::ModelUsageSnapshot {
                        provider: "openai".to_string(),
                        model: "gpt-5.2-codex".to_string(),
                        tokens_used: 1_000,
                        context_window: 1_000,
                        hard_context_window: Some(1_200),
                        usage_pct: 100.0,
                        prompt_tokens: 900,
                        completion_tokens: 100,
                        cached_tokens: 250,
                    },
                );
                s.session_codex_managed_context
                    .insert("managed-session".to_string(), true);
            }
            let server = IntendantServer::new(state, EventBus::new());
            let status = server
                .get_status_for_session(Some("managed-session"), None)
                .await;
            let value: serde_json::Value = serde_json::from_str(&status).unwrap();
            assert_eq!(
                value.pointer("/session_id"),
                Some(&"managed-session".into())
            );
            assert_eq!(value.pointer("/usage/main/tokens_used"), Some(&1000.into()));
            assert_eq!(value.pointer("/session_tokens"), Some(&1000.into()));
            assert_eq!(
                value.pointer("/context_pressure/status"),
                Some(&"high".into())
            );
            assert_eq!(
                value.pointer("/context_pressure/managed_context"),
                Some(&"managed".into())
            );
        });
    }

    #[test]
    fn get_status_for_active_session_uses_global_usage_for_context_pressure() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            {
                let mut s = state.write().await;
                s.session_id = "active-managed-session".to_string();
                s.active_session_source = Some("codex".to_string());
                s.codex_managed_context = true;
                s.session_codex_managed_context
                    .insert("active-managed-session".to_string(), true);
                s.apply_main_usage_snapshot(frontend::ModelUsageSnapshot {
                    provider: "openai".to_string(),
                    model: "gpt-5.2-codex".to_string(),
                    tokens_used: 950,
                    context_window: 1_000,
                    hard_context_window: Some(1_200),
                    usage_pct: 95.0,
                    prompt_tokens: 900,
                    completion_tokens: 50,
                    cached_tokens: 400,
                });
            }
            let server = IntendantServer::new(state, EventBus::new());
            let status = server
                .get_status_for_session(Some("active-managed-session"), Some(true))
                .await;
            let value: serde_json::Value = serde_json::from_str(&status).unwrap();
            assert_eq!(value.pointer("/usage/main/tokens_used"), Some(&950.into()));
            assert_eq!(
                value.pointer("/context_pressure/used_tokens"),
                Some(&950.into())
            );
            assert_eq!(
                value.pointer("/context_pressure/status"),
                Some(&"watch".into())
            );
            assert_eq!(
                value.pointer("/context_pressure/normal_tools_allowed"),
                Some(&true.into())
            );
            assert_eq!(
                value.pointer("/context_pressure/required_action"),
                Some(&"density_handoff_before_broad_work".into())
            );
            assert_eq!(
                value.pointer("/context_pressure/broad_followup_allowed"),
                Some(&false.into())
            );
            assert_eq!(
                value.pointer("/context_pressure/managed_context"),
                Some(&"managed".into())
            );
        });
    }

    #[test]
    fn get_status_resolves_backend_usage_through_session_identity_alias() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            {
                let mut s = state.write().await;
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::SessionCapabilities {
                        session_id: "wrapper-session".to_string(),
                        capabilities: crate::types::SessionCapabilities {
                            follow_up: true,
                            steer: true,
                            interrupt: true,
                            codex_thread_actions: vec!["rewind_context".to_string()],
                            codex_managed_context: Some("managed".to_string()),
                            codex_sandbox: Some("danger-full-access".to_string()),
                            codex_approval_policy: Some("never".to_string()),
                            codex_context_archive: None,
                            codex_command: Some("/tmp/codex".to_string()),
                            codex_fast_mode: None,
                            codex_service_tier: None,
                        },
                    },
                );
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::SessionIdentity {
                        session_id: "wrapper-session".to_string(),
                        source: "codex".to_string(),
                        backend_session_id: "codex-thread".to_string(),
                    },
                );
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::UsageSnapshot {
                        session_id: Some("codex-thread".to_string()),
                        main: frontend::ModelUsageSnapshot {
                            provider: "openai".to_string(),
                            model: "gpt-5.2-codex".to_string(),
                            tokens_used: 990,
                            context_window: 1_000,
                            hard_context_window: Some(1_200),
                            usage_pct: 99.0,
                            prompt_tokens: 950,
                            completion_tokens: 40,
                            cached_tokens: 500,
                        },
                        presence: None,
                    },
                );
            }

            let server = IntendantServer::new(state, EventBus::new());
            let status = server
                .get_status_for_session(Some("wrapper-session"), None)
                .await;
            let value: serde_json::Value = serde_json::from_str(&status).unwrap();
            assert_eq!(value.pointer("/usage/main/tokens_used"), Some(&990.into()));
            assert_eq!(
                value.pointer("/context_pressure/used_tokens"),
                Some(&990.into())
            );
            assert_eq!(
                value.pointer("/context_pressure/status"),
                Some(&"watch".into())
            );
            assert_eq!(
                value.pointer("/context_pressure/normal_tools_allowed"),
                Some(&true.into())
            );
            assert_eq!(
                value.pointer("/context_pressure/managed_context"),
                Some(&"managed".into())
            );
        });
    }

    #[test]
    fn get_status_for_wrapper_uses_latest_related_usage_after_rewind() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            {
                let mut s = state.write().await;
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::SessionCapabilities {
                        session_id: "wrapper-session".to_string(),
                        capabilities: crate::types::SessionCapabilities {
                            follow_up: true,
                            steer: true,
                            interrupt: true,
                            codex_thread_actions: vec!["rewind_context".to_string()],
                            codex_managed_context: Some("managed".to_string()),
                            codex_sandbox: Some("danger-full-access".to_string()),
                            codex_approval_policy: Some("never".to_string()),
                            codex_context_archive: None,
                            codex_command: Some("/tmp/codex".to_string()),
                            codex_fast_mode: None,
                            codex_service_tier: None,
                        },
                    },
                );
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::SessionIdentity {
                        session_id: "wrapper-session".to_string(),
                        source: "codex".to_string(),
                        backend_session_id: "codex-thread".to_string(),
                    },
                );
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::UsageSnapshot {
                        session_id: Some("wrapper-session".to_string()),
                        main: frontend::ModelUsageSnapshot {
                            provider: "openai".to_string(),
                            model: "gpt-5.2-codex".to_string(),
                            tokens_used: 225_000,
                            context_window: 258_400,
                            hard_context_window: Some(272_000),
                            usage_pct: 87.0,
                            prompt_tokens: 224_000,
                            completion_tokens: 1_000,
                            cached_tokens: 10_000,
                        },
                        presence: None,
                    },
                );
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::UsageSnapshot {
                        session_id: Some("codex-thread".to_string()),
                        main: frontend::ModelUsageSnapshot {
                            provider: "openai".to_string(),
                            model: "gpt-5.2-codex".to_string(),
                            tokens_used: 70_046,
                            context_window: 258_400,
                            hard_context_window: Some(272_000),
                            usage_pct: 27.1,
                            prompt_tokens: 69_000,
                            completion_tokens: 1_046,
                            cached_tokens: 50_000,
                        },
                        presence: None,
                    },
                );
            }

            let server = IntendantServer::new(state, EventBus::new());
            let status = server
                .get_status_for_session(Some("wrapper-session"), None)
                .await;
            let value: serde_json::Value = serde_json::from_str(&status).unwrap();
            assert_eq!(
                value.pointer("/context_pressure/used_tokens"),
                Some(&70_046.into())
            );
            assert_eq!(
                value.pointer("/context_pressure/status"),
                Some(&"ok".into())
            );
            assert_eq!(
                value.pointer("/usage/main/tokens_used"),
                Some(&70_046.into())
            );
        });
    }

    #[test]
    fn get_status_for_wrapper_after_identity_without_usage_reports_unknown_pressure() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            {
                let mut s = state.write().await;
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::SessionCapabilities {
                        session_id: "wrapper-session".to_string(),
                        capabilities: crate::types::SessionCapabilities {
                            follow_up: true,
                            steer: true,
                            interrupt: true,
                            codex_thread_actions: vec!["rewind_context".to_string()],
                            codex_managed_context: Some("managed".to_string()),
                            codex_sandbox: Some("danger-full-access".to_string()),
                            codex_approval_policy: Some("never".to_string()),
                            codex_context_archive: None,
                            codex_command: Some("/tmp/codex".to_string()),
                            codex_fast_mode: None,
                            codex_service_tier: None,
                        },
                    },
                );
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::SessionIdentity {
                        session_id: "wrapper-session".to_string(),
                        source: "codex".to_string(),
                        backend_session_id: "codex-thread".to_string(),
                    },
                );
            }

            let server = IntendantServer::new(state, EventBus::new());
            let status = server
                .get_status_for_session(Some("wrapper-session"), None)
                .await;
            let value: serde_json::Value = serde_json::from_str(&status).unwrap();
            assert_eq!(
                value.pointer("/context_pressure/status"),
                Some(&"unknown".into())
            );
            assert_eq!(
                value.pointer("/context_pressure/used_tokens"),
                Some(&0.into())
            );
            assert_eq!(
                value.pointer("/context_pressure/context_window"),
                Some(&serde_json::Value::Null)
            );
            assert_eq!(
                value.pointer("/context_pressure/managed_context"),
                Some(&"managed".into())
            );
        });
    }

    #[test]
    fn get_status_uses_backend_context_snapshot_before_usage_snapshot() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            {
                let mut s = state.write().await;
                s.provider_name = "none".to_string();
                s.model_name = "none".to_string();
                s.configured_codex_managed_context = true;
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::SessionCapabilities {
                        session_id: "wrapper-session".to_string(),
                        capabilities: crate::types::SessionCapabilities {
                            follow_up: true,
                            steer: true,
                            interrupt: true,
                            codex_thread_actions: vec!["rewind_context".to_string()],
                            codex_managed_context: Some("managed".to_string()),
                            codex_sandbox: Some("danger-full-access".to_string()),
                            codex_approval_policy: Some("never".to_string()),
                            codex_context_archive: None,
                            codex_command: Some("/tmp/codex".to_string()),
                            codex_fast_mode: None,
                            codex_service_tier: None,
                        },
                    },
                );
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::SessionIdentity {
                        session_id: "wrapper-session".to_string(),
                        source: "codex".to_string(),
                        backend_session_id: "codex-thread".to_string(),
                    },
                );
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::SessionStarted {
                        session_id: "codex-thread".to_string(),
                        task: Some("managed Codex task".to_string()),
                    },
                );
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::AgentStarted {
                        session_id: Some("codex-thread".to_string()),
                        turn: 3,
                        commands_preview: "edit static/app.html".to_string(),
                        item_id: None,
                        source: Some("Codex".to_string()),
                    },
                );
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::ContextSnapshot {
                        session_id: Some("codex-thread".to_string()),
                        source: "codex".to_string(),
                        label: "Codex resolved request payload".to_string(),
                        request_id: Some("req-1".to_string()),
                        request_index: Some(1),
                        turn: Some(3),
                        format: "openai.responses.resolved_request.v1".to_string(),
                        token_count: Some(990),
                        token_count_kind: Some("backend_reported".to_string()),
                        context_window: Some(1_000),
                        hard_context_window: Some(1_200),
                        item_count: Some(12),
                        raw: serde_json::json!({ "model": "gpt-5.2-codex" }),
                    },
                );
            }

            let server = IntendantServer::new(state, EventBus::new());
            let status = server.get_status().await;
            let value: serde_json::Value = serde_json::from_str(&status).unwrap();
            assert_eq!(value.pointer("/phase"), Some(&"running_agent".into()));
            assert_eq!(value.pointer("/provider"), Some(&"openai".into()));
            assert_eq!(value.pointer("/model"), Some(&"gpt-5.2-codex".into()));
            assert_eq!(value.pointer("/session_tokens"), Some(&990.into()));
            assert_eq!(value.pointer("/usage/main/tokens_used"), Some(&990.into()));
            assert_eq!(
                value.pointer("/context_pressure/used_tokens"),
                Some(&990.into())
            );
            assert_eq!(
                value.pointer("/context_pressure/status"),
                Some(&"watch".into())
            );
            assert_eq!(
                value.pointer("/context_pressure/context_window"),
                Some(&1000.into())
            );
            assert_eq!(
                value.pointer("/context_pressure/managed_context"),
                Some(&"managed".into())
            );
        });
    }

    #[test]
    fn get_status_for_wrapper_hydrates_backend_context_snapshot_from_session_log() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let dir = tempdir().unwrap();
            let wrapper_dir = dir.path().join("wrapper-session");
            let mut log = crate::session_log::SessionLog::open(wrapper_dir.clone()).unwrap();
            log.write_meta(None, Some("managed Codex task"));
            let capabilities = crate::types::SessionCapabilities {
                follow_up: true,
                steer: true,
                interrupt: true,
                codex_thread_actions: vec!["rewind_context".to_string()],
                codex_managed_context: Some("managed".to_string()),
                codex_sandbox: Some("danger-full-access".to_string()),
                codex_approval_policy: Some("never".to_string()),
                codex_context_archive: None,
                codex_command: Some("/tmp/codex".to_string()),
                codex_fast_mode: None,
                codex_service_tier: None,
            };
            log.session_capabilities("wrapper-session", &capabilities);
            {
                use std::io::Write as _;
                let mut file = std::fs::OpenOptions::new()
                    .append(true)
                    .open(wrapper_dir.join("session.jsonl"))
                    .unwrap();
                writeln!(
                    file,
                    "{}",
                    serde_json::json!({
                        "ts": "00:00:00.000",
                        "event": "session_identity",
                        "level": "info",
                        "message": "Session identity: wrapper-session -> codex:codex-thread",
                        "data": {
                            "session_id": "wrapper-session",
                            "source": "codex",
                            "backend_session_id": "codex-thread",
                        },
                    })
                )
                .unwrap();
            }
            log.session_started("codex-thread", Some("managed Codex task"));
            log.agent_started_with_session_id(
                Some("codex-thread"),
                5,
                "edit src/bin/caller/mcp.rs",
                None,
                Some("Codex"),
            );
            log.context_snapshot_for_session(
                Some("codex-thread"),
                "codex",
                "Codex resolved request payload",
                Some("req-1"),
                Some(1),
                Some(5),
                "openai.responses.resolved_request.v1",
                Some(50_332),
                Some("backend_reported"),
                Some(258_400),
                Some(272_000),
                Some(64),
                &serde_json::json!({ "model": "gpt-5.2-codex" }),
            );

            let state = test_state_with_log_dir(wrapper_dir);
            {
                let mut s = state.write().await;
                s.provider_name = "none".to_string();
                s.model_name = "none".to_string();
                s.configured_codex_managed_context = true;
            }
            let server = IntendantServer::new(state, EventBus::new());
            let status = server
                .get_status_for_session(Some("wrapper-session"), Some(true))
                .await;
            let value: serde_json::Value = serde_json::from_str(&status).unwrap();

            assert_eq!(
                value.pointer("/session_id"),
                Some(&"wrapper-session".into())
            );
            assert_eq!(value.pointer("/phase"), Some(&"running_agent".into()));
            assert_eq!(value.pointer("/provider"), Some(&"openai".into()));
            assert_eq!(value.pointer("/model"), Some(&"gpt-5.2-codex".into()));
            assert_eq!(
                value.pointer("/usage/main/tokens_used"),
                Some(&50_332.into())
            );
            assert_eq!(
                value.pointer("/context_pressure/used_tokens"),
                Some(&50_332.into())
            );
            assert_eq!(
                value.pointer("/context_pressure/context_window"),
                Some(&258_400.into())
            );
            assert_eq!(
                value.pointer("/context_pressure/managed_context"),
                Some(&"managed".into())
            );
        });
    }

    #[test]
    fn get_status_log_hydration_does_not_leak_unrelated_backend_usage() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let dir = tempdir().unwrap();
            let wrapper_dir = dir.path().join("wrapper-session");
            let mut log = crate::session_log::SessionLog::open(wrapper_dir.clone()).unwrap();
            log.write_meta(None, Some("managed Codex task"));
            log.context_snapshot_for_session(
                Some("other-codex-thread"),
                "codex",
                "Other Codex resolved request payload",
                Some("req-other"),
                Some(1),
                Some(2),
                "openai.responses.resolved_request.v1",
                Some(200_000),
                Some("backend_reported"),
                Some(258_400),
                Some(272_000),
                Some(64),
                &serde_json::json!({ "model": "gpt-5.2-codex" }),
            );

            let state = test_state_with_log_dir(wrapper_dir);
            {
                let mut s = state.write().await;
                s.provider_name = "none".to_string();
                s.model_name = "none".to_string();
            }
            let server = IntendantServer::new(state, EventBus::new());
            let status = server
                .get_status_for_session(Some("wrapper-session"), Some(true))
                .await;
            let value: serde_json::Value = serde_json::from_str(&status).unwrap();

            assert_eq!(value.pointer("/usage/main/provider"), Some(&"".into()));
            assert_eq!(value.pointer("/usage/main/model"), Some(&"".into()));
            assert_eq!(value.pointer("/usage/main/tokens_used"), Some(&0.into()));
            assert_eq!(
                value.pointer("/context_pressure/status"),
                Some(&"unknown".into())
            );
            assert_eq!(
                value.pointer("/context_pressure/used_tokens"),
                Some(&0.into())
            );
        });
    }

    #[test]
    fn get_status_resolves_backend_phase_through_session_identity_alias() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            {
                let mut s = state.write().await;
                s.session_id = "wrapper-session".to_string();
                s.task_description = "managed Codex task".to_string();
                s.set_phase(Phase::WaitingFollowUp);
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::SessionIdentity {
                        session_id: "wrapper-session".to_string(),
                        source: "codex".to_string(),
                        backend_session_id: "codex-thread".to_string(),
                    },
                );
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::StatusUpdate {
                        turn: 14,
                        phase: "thinking".to_string(),
                        autonomy: "medium".to_string(),
                        session_id: "codex-thread".to_string(),
                        task: "Codex follow-up round 14 in progress: fix the controller status"
                            .to_string(),
                    },
                );
            }

            let server = IntendantServer::new(state.clone(), EventBus::new());
            let active_status: serde_json::Value =
                serde_json::from_str(&server.get_status().await).unwrap();
            assert_eq!(active_status.pointer("/phase"), Some(&"thinking".into()));
            assert_eq!(active_status.pointer("/round"), Some(&14.into()));

            let wrapper_status: serde_json::Value = serde_json::from_str(
                &server
                    .get_status_for_session(Some("wrapper-session"), None)
                    .await,
            )
            .unwrap();
            assert_eq!(wrapper_status.pointer("/phase"), Some(&"thinking".into()));
            assert_eq!(wrapper_status.pointer("/turn"), Some(&14.into()));
            assert_eq!(wrapper_status.pointer("/round"), Some(&14.into()));
            assert_eq!(
                wrapper_status.pointer("/task"),
                Some(&"Codex follow-up round 14 in progress: fix the controller status".into())
            );

            {
                let mut s = state.write().await;
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::RoundComplete {
                        session_id: Some("codex-thread".to_string()),
                        round: 14,
                        turns_in_round: 1,
                        native_message_count: None,
                    },
                );
            }

            let idle_status: serde_json::Value = serde_json::from_str(
                &server
                    .get_status_for_session(Some("wrapper-session"), None)
                    .await,
            )
            .unwrap();
            assert_eq!(
                idle_status.pointer("/phase"),
                Some(&"waiting_follow_up".into())
            );
            assert_eq!(idle_status.pointer("/round"), Some(&14.into()));
        });
    }

    #[test]
    fn get_status_for_unknown_session_does_not_inherit_active_pressure() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            {
                let mut s = state.write().await;
                s.session_id = "active-managed-session".to_string();
                s.apply_main_usage_snapshot(frontend::ModelUsageSnapshot {
                    provider: "openai".to_string(),
                    model: "gpt-5.2-codex".to_string(),
                    tokens_used: 1_000,
                    context_window: 1_000,
                    hard_context_window: Some(1_200),
                    usage_pct: 100.0,
                    prompt_tokens: 900,
                    completion_tokens: 100,
                    cached_tokens: 400,
                });
            }
            let server = IntendantServer::new(state, EventBus::new());
            let status = server
                .get_status_for_session(Some("new-session-without-usage"), Some(true))
                .await;
            let value: serde_json::Value = serde_json::from_str(&status).unwrap();
            assert_eq!(value.pointer("/usage/main/provider"), Some(&"".into()));
            assert_eq!(value.pointer("/usage/main/model"), Some(&"".into()));
            assert_eq!(value.pointer("/usage/main/tokens_used"), Some(&0.into()));
            assert_eq!(
                value.pointer("/context_pressure/status"),
                Some(&"unknown".into())
            );
            assert_eq!(
                value.pointer("/context_pressure/used_tokens"),
                Some(&0.into())
            );
        });
    }

    #[test]
    fn observed_usage_retains_non_active_session_snapshot() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            {
                let mut s = state.write().await;
                s.session_id = "active-session".to_string();
                s.session_sources
                    .insert("managed-session".to_string(), "codex".to_string());
                s.session_codex_managed_context
                    .insert("managed-session".to_string(), true);
                apply_observed_event_to_mcp_state(
                    &mut s,
                    &AppEvent::UsageSnapshot {
                        session_id: Some("managed-session".to_string()),
                        main: frontend::ModelUsageSnapshot {
                            provider: "openai".to_string(),
                            model: "gpt-5.2-codex".to_string(),
                            tokens_used: 850,
                            context_window: 1_000,
                            hard_context_window: Some(1_200),
                            usage_pct: 85.0,
                            prompt_tokens: 800,
                            completion_tokens: 50,
                            cached_tokens: 200,
                        },
                        presence: None,
                    },
                );
            }
            let server = IntendantServer::new(state, EventBus::new());
            let status = server
                .get_status_for_session(Some("managed-session"), None)
                .await;
            let value: serde_json::Value = serde_json::from_str(&status).unwrap();
            assert_eq!(value.pointer("/usage/main/tokens_used"), Some(&850.into()));
            assert_eq!(
                value.pointer("/context_pressure/managed_context"),
                Some(&"managed".into())
            );
        });
    }

    #[test]
    fn rewind_backout_fork_dispatches_without_cache_reset_opt_in() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let bus = EventBus::new();
            let server = IntendantServer::new(test_state(), bus.clone());
            let result_task = spawn_codex_thread_action_result(
                bus,
                "rewind_backout",
                "forked context rewind record rewind-1 with inherited lineage prompt-cache key into thread thread-2",
            );
            let forked = server
                .rewind_backout(Parameters(RewindBackoutParams {
                    session_id: None,
                    record_id: "rewind-1".to_string(),
                    mode: Some("fork".to_string()),
                    name: None,
                    allow_cache_reset: false,
                }))
                .await;
            assert_eq!(
                forked,
                "forked context rewind record rewind-1 with inherited lineage prompt-cache key into thread thread-2"
            );
            result_task.await.unwrap();
        });
    }

    #[test]
    fn rewind_backout_returns_thread_action_result_to_caller() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let bus = EventBus::new();
            let server = IntendantServer::new(test_state(), bus.clone());
            let result_task = spawn_codex_thread_action_result(
                bus,
                "rewind_backout",
                "context rewind record rewind-1: pre-rewind rollout copied from source to recovery; restore uses same-thread Codex thread/restore when available",
            );

            let inspected = server
                .rewind_backout(Parameters(RewindBackoutParams {
                    session_id: None,
                    record_id: "rewind-1".to_string(),
                    mode: Some("inspect".to_string()),
                    name: None,
                    allow_cache_reset: false,
                }))
                .await;

            assert!(inspected.contains("same-thread Codex thread/restore"));
            assert!(!inspected.contains("dispatched"));
            result_task.await.unwrap();
        });
    }

    #[test]
    fn get_status_includes_lineage_ledger_when_sessions_are_related() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let dir = tempdir().unwrap();
            std::fs::write(
                dir.path().join("session.jsonl"),
                concat!(
                    r#"{"event":"session_identity","data":{"session_id":"child","source":"codex","backend_session_id":"thread-child"}}"#,
                    "\n",
                    r#"{"event":"session_relationship","data":{"parent_session_id":"parent","child_session_id":"child","relationship":"subagent","ephemeral":false}}"#,
                    "\n",
                ),
            )
            .unwrap();
            let state = test_state_with_log_dir(dir.path().to_path_buf());
            {
                let mut s = state.write().await;
                s.session_id = "parent".to_string();
            }
            let server = IntendantServer::new(state, EventBus::new());
            let status = server.get_status().await;
            let value: serde_json::Value = serde_json::from_str(&status).unwrap();
            assert_eq!(
                value.pointer("/lineage_ledger/groups/0/branches/0/session_id"),
                Some(&serde_json::Value::String("child".to_string()))
            );
        });
    }

    #[test]
    fn get_status_includes_fission_ledger_when_sessions_are_related() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let dir = tempdir().unwrap();
            crate::fission_ledger::record_fission_observation(
                dir.path(),
                crate::fission_ledger::FissionObservation {
                    parent_session_id: "parent".to_string(),
                    anchor_item_id: "call-1".to_string(),
                    tool: "spawn_agent".to_string(),
                    status: "running".to_string(),
                    prompt: Some("inspect parser".to_string()),
                    model: None,
                    reasoning_effort: None,
                    branches: vec![crate::fission_ledger::FissionBranchObservation {
                        session_id: "child".to_string(),
                        status: "running".to_string(),
                        summary: None,
                    }],
                },
            )
            .unwrap();
            let state = test_state_with_log_dir(dir.path().to_path_buf());
            {
                let mut s = state.write().await;
                s.session_id = "child".to_string();
            }
            let server = IntendantServer::new(state, EventBus::new());
            let status = server.get_status().await;
            let value: serde_json::Value = serde_json::from_str(&status).unwrap();
            assert_eq!(
                value.pointer("/fission_ledger/groups/0/anchor_item_id"),
                Some(&serde_json::Value::String("call-1".to_string()))
            );
            assert_eq!(
                value.pointer("/fission_ledger/groups/0/branches/0/session_id"),
                Some(&serde_json::Value::String("child".to_string()))
            );
        });
    }

    #[test]
    fn claim_fission_canonical_tool_updates_ledger() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let dir = tempdir().unwrap();
            crate::fission_ledger::record_fission_observation(
                dir.path(),
                crate::fission_ledger::FissionObservation {
                    parent_session_id: "parent".to_string(),
                    anchor_item_id: "call-1".to_string(),
                    tool: "spawn_agent".to_string(),
                    status: "running".to_string(),
                    prompt: None,
                    model: None,
                    reasoning_effort: None,
                    branches: vec![crate::fission_ledger::FissionBranchObservation {
                        session_id: "child".to_string(),
                        status: "running".to_string(),
                        summary: None,
                    }],
                },
            )
            .unwrap();
            let state = test_state_with_log_dir(dir.path().to_path_buf());
            let server = IntendantServer::new(state, EventBus::new());
            let group_id = crate::fission_ledger::group_id("parent", "call-1");

            let result = server
                .claim_fission_canonical(Parameters(ClaimFissionCanonicalParams {
                    group_id: group_id.clone(),
                    branch_session_id: "child".to_string(),
                    expected_canonical_session_id: None,
                }))
                .await;
            let value: serde_json::Value = serde_json::from_str(&result).unwrap();
            assert_eq!(
                value.pointer("/canonical_session_id"),
                Some(&serde_json::Value::String("child".to_string()))
            );

            let status = server.get_status().await;
            let value: serde_json::Value = serde_json::from_str(&status).unwrap();
            assert_eq!(
                value.pointer("/fission_ledger/groups/0/canonical_session_id"),
                Some(&serde_json::Value::String("child".to_string()))
            );
        });
    }

    #[test]
    fn push_log_entries() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            s.push_log(LogLevel::Info, "hello".to_string());
            s.push_log(LogLevel::Error, "oops".to_string());
            assert_eq!(s.log_entries.len(), 2);
            assert_eq!(s.log_entries[0].id, 0);
            assert_eq!(s.log_entries[0].level, "info");
            assert_eq!(s.log_entries[0].content, "hello");
            assert_eq!(s.log_entries[1].id, 1);
            assert_eq!(s.log_entries[1].level, "error");
        });
    }

    #[test]
    fn process_action_approve_with_pending() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            let (tx, rx) = tokio::sync::oneshot::channel();
            s.approval_registry.lock().unwrap().insert(1, tx);
            s.pending_approval = Some(PendingApprovalState {
                id: 1,
                command_preview: "rm -rf /tmp".to_string(),
                category: "destructive".to_string(),
            });

            let outcome = process_action_sync(&mut s, UserAction::Approve { id: 1 });
            assert_eq!(outcome, ActionOutcome::Ok);
            assert!(s.pending_approval.is_none());
            assert_eq!(s.phase, Phase::RunningAgent);

            // Check the oneshot received the response
            let response = rx.await.unwrap();
            assert_eq!(response, ApprovalResponse::Approve);
        });
    }

    #[test]
    fn process_action_approve_without_pending() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            let outcome = process_action_sync(&mut s, UserAction::Approve { id: 1 });
            match outcome {
                ActionOutcome::NoOp { reason } => {
                    assert!(reason.contains("No pending approval"));
                }
                _ => panic!("Expected NoOp"),
            }
        });
    }

    #[test]
    fn process_action_deny() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            let (tx, rx) = tokio::sync::oneshot::channel();
            s.approval_registry.lock().unwrap().insert(2, tx);
            s.pending_approval = Some(PendingApprovalState {
                id: 2,
                command_preview: "curl evil.com".to_string(),
                category: "network".to_string(),
            });

            let outcome = process_action_sync(&mut s, UserAction::Deny { id: 2 });
            assert_eq!(outcome, ActionOutcome::Ok);
            assert_eq!(s.phase, Phase::Done);

            let response = rx.await.unwrap();
            assert_eq!(response, ApprovalResponse::Deny);
        });
    }

    #[test]
    fn process_action_skip() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            let (tx, rx) = tokio::sync::oneshot::channel();
            s.approval_registry.lock().unwrap().insert(3, tx);
            s.pending_approval = Some(PendingApprovalState {
                id: 3,
                command_preview: "test".to_string(),
                category: "exec".to_string(),
            });

            let outcome = process_action_sync(&mut s, UserAction::Skip { id: 3 });
            assert_eq!(outcome, ActionOutcome::Ok);
            assert_eq!(s.phase, Phase::RunningAgent);

            let response = rx.await.unwrap();
            assert_eq!(response, ApprovalResponse::Skip);
        });
    }

    #[test]
    fn process_action_approve_all() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            let (tx, rx) = tokio::sync::oneshot::channel();
            s.approval_registry.lock().unwrap().insert(4, tx);
            s.pending_approval = Some(PendingApprovalState {
                id: 4,
                command_preview: "ls".to_string(),
                category: "exec".to_string(),
            });

            let outcome = process_action_sync(&mut s, UserAction::ApproveAll { id: 4 });
            assert_eq!(outcome, ActionOutcome::Ok);

            let response = rx.await.unwrap();
            assert_eq!(response, ApprovalResponse::ApproveAll);
        });
    }

    #[test]
    fn process_action_set_verbosity() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            let outcome = process_action_sync(
                &mut s,
                UserAction::SetVerbosity {
                    level: Verbosity::Debug,
                },
            );
            assert_eq!(outcome, ActionOutcome::Ok);
            assert_eq!(s.verbosity, Verbosity::Debug);
        });
    }

    #[test]
    fn process_action_quit() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            let outcome = process_action_sync(&mut s, UserAction::Quit);
            assert_eq!(outcome, ActionOutcome::Ok);
            assert!(s.should_quit);
        });
    }

    #[test]
    fn process_action_respond_human_without_question() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            let outcome = process_action_sync(
                &mut s,
                UserAction::RespondHuman {
                    text: "hello".to_string(),
                },
            );
            match outcome {
                ActionOutcome::NoOp { reason } => {
                    assert!(reason.contains("No pending human question"));
                }
                _ => panic!("Expected NoOp"),
            }
        });
    }

    #[test]
    fn phase_to_str_all_variants() {
        assert_eq!(phase_to_str(&Phase::Thinking), "thinking");
        assert_eq!(phase_to_str(&Phase::RunningAgent), "running_agent");
        assert_eq!(phase_to_str(&Phase::Orchestrating), "orchestrating");
        assert_eq!(phase_to_str(&Phase::WaitingApproval), "waiting_approval");
        assert_eq!(phase_to_str(&Phase::WaitingHuman), "waiting_human");
        assert_eq!(phase_to_str(&Phase::WaitingFollowUp), "waiting_follow_up");
        assert_eq!(phase_to_str(&Phase::Idle), "idle");
        assert_eq!(phase_to_str(&Phase::Done), "done");
        assert_eq!(phase_to_str(&Phase::Interrupting), "interrupting");
        assert_eq!(phase_to_str(&Phase::Interrupted), "interrupted");
    }

    #[test]
    fn parse_verbosity_all_variants() {
        assert_eq!(parse_verbosity("quiet"), Some(Verbosity::Quiet));
        assert_eq!(parse_verbosity("normal"), Some(Verbosity::Normal));
        assert_eq!(parse_verbosity("verbose"), Some(Verbosity::Verbose));
        assert_eq!(parse_verbosity("debug"), Some(Verbosity::Debug));
        assert_eq!(parse_verbosity("QUIET"), Some(Verbosity::Quiet));
        assert_eq!(parse_verbosity("unknown"), None);
    }

    #[test]
    fn parse_restart_after_defaults_to_turn_end() {
        assert_eq!(parse_restart_after(None).unwrap(), RestartAfter::TurnEnd);
        assert_eq!(
            parse_restart_after(Some("turn_end")).unwrap(),
            RestartAfter::TurnEnd
        );
        assert_eq!(parse_restart_after(Some("NOW")).unwrap(), RestartAfter::Now);
    }

    #[test]
    fn parse_restart_after_rejects_invalid_value() {
        let err = parse_restart_after(Some("later")).unwrap_err();
        assert!(err.contains("restart_after must be 'turn_end' or 'now'"));
    }

    #[test]
    fn normalize_optional_string_field_trims_and_drops_empty() {
        let mut value = Some("  hello  ".to_string());
        normalize_optional_string_field(&mut value);
        assert_eq!(value.as_deref(), Some("hello"));

        let mut empty = Some("   ".to_string());
        normalize_optional_string_field(&mut empty);
        assert!(empty.is_none());
    }

    #[test]
    fn controller_restart_state_defaults() {
        let params = ScheduleControllerRestartParams {
            controller_id: "codex".to_string(),
            north_star_goal: "audit and improve".to_string(),
            reason: Some("periodic refresh".to_string()),
            restart_after: None,
            restart_command: None,
            auto_start_task: None,
            max_attempts: None,
            cooldown_sec: None,
        };
        let state = ControllerRestartState::new(&params);
        assert_eq!(state.controller_id, "codex");
        assert_eq!(state.phase, RestartPhase::AwaitingTurnComplete);
        assert_eq!(state.max_attempts, 1);
        assert_eq!(state.cooldown_sec, 30);
        assert!(!state.auto_start_task);
    }

    #[test]
    fn restart_state_public_value_redacts_turn_complete_token() {
        let params = ScheduleControllerRestartParams {
            controller_id: "codex".to_string(),
            north_star_goal: "audit and improve".to_string(),
            reason: None,
            restart_after: None,
            restart_command: Some("true".to_string()),
            auto_start_task: Some(false),
            max_attempts: None,
            cooldown_sec: None,
        };
        let restart = ControllerRestartState::new(&params);
        let raw_token = restart.turn_complete_token.clone();

        let public = restart_state_public_value(Some(&restart));
        assert_eq!(
            public.get("turn_complete_token").and_then(|v| v.as_str()),
            Some("[redacted]")
        );
        assert_ne!(
            public.get("turn_complete_token").and_then(|v| v.as_str()),
            Some(raw_token.as_str())
        );
        assert_eq!(
            public.get("restart_id").and_then(|v| v.as_str()),
            Some(restart.restart_id.as_str())
        );
    }

    #[test]
    fn controller_loop_halt_markers_roundtrip() {
        let dir = tempdir().unwrap();
        let loop_dir = dir.path().join(".intendant/controller-loop");

        request_loop_halt_marker(&loop_dir, true).expect("persistent halt should succeed");
        assert!(loop_dir.join("request_halt").exists());

        request_loop_halt_marker(&loop_dir, false).expect("one-shot halt should succeed");
        assert!(loop_dir.join("request_halt_after_cycle").exists());

        clear_loop_halt_markers(&loop_dir).expect("clear halt should succeed");
        assert!(!loop_dir.join("request_halt").exists());
        assert!(!loop_dir.join("request_halt_after_cycle").exists());
    }

    #[test]
    fn controller_loop_clear_halt_resets_stop_marker_status() {
        let dir = tempdir().unwrap();
        let loop_dir = dir.path().join(".intendant/controller-loop");
        let intervention =
            request_loop_intervention_marker_for_root(&loop_dir, "stop", u32::MAX).unwrap();
        assert_eq!(intervention.mode, ControllerLoopInterventionMode::Stop);
        assert!(loop_dir.join("request_stop").exists());

        clear_loop_halt_markers(&loop_dir).expect("clear halt should succeed");

        assert!(!loop_dir.join("request_stop").exists());
        assert_eq!(
            collect_controller_loop_status(&loop_dir)
                .get("flags")
                .and_then(|v| v.get("stop_requested"))
                .and_then(|v| v.as_bool()),
            Some(false)
        );
    }

    #[test]
    fn controller_loop_status_clears_stale_intervention_markers_without_active_owner() {
        let dir = tempdir().unwrap();
        let loop_dir = dir.path().join(".intendant/controller-loop");
        std::fs::create_dir_all(&loop_dir).unwrap();
        std::fs::write(loop_dir.join("request_stop"), b"").unwrap();
        std::fs::write(loop_dir.join("request_abort"), b"").unwrap();

        let status = collect_controller_loop_status(&loop_dir);

        assert!(!loop_dir.join("request_stop").exists());
        assert!(!loop_dir.join("request_abort").exists());
        assert_eq!(
            status
                .get("flags")
                .and_then(|v| v.get("stop_requested"))
                .and_then(|v| v.as_bool()),
            Some(false)
        );
        assert_eq!(
            status
                .get("flags")
                .and_then(|v| v.get("abort_requested"))
                .and_then(|v| v.as_bool()),
            Some(false)
        );
        assert_eq!(
            status
                .get("flags")
                .and_then(|v| v.get("stale_intervention_cleared"))
                .and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn controller_loop_intervention_markers_are_stale_for_idle_external_app_server_wrapper() {
        let idle_wrapper = serde_json::json!({
            "source": "external_wrapper_index",
            "status": "unknown_running",
            "session_meta_status": "idle",
            "process_tree_active": true,
            "app_server_active": true,
        });
        let running_wrapper = serde_json::json!({
            "source": "external_wrapper_index",
            "status": "unknown_running",
            "session_meta_status": "running",
            "process_tree_active": true,
            "app_server_active": true,
        });
        let controller_loop_wrapper = serde_json::json!({
            "source": "controller_loop",
            "pid": std::process::id(),
        });

        assert!(controller_loop_intervention_markers_are_stale(
            false,
            false,
            &[idle_wrapper],
            &[serde_json::json!({"pid": 8894})]
        ));
        assert!(!controller_loop_intervention_markers_are_stale(
            false,
            false,
            &[running_wrapper],
            &[serde_json::json!({"pid": 8894})]
        ));
        assert!(!controller_loop_intervention_markers_are_stale(
            false,
            false,
            &[controller_loop_wrapper],
            &[]
        ));
        assert!(!controller_loop_intervention_markers_are_stale(
            false,
            false,
            &[],
            &[serde_json::json!({"pid": 8894})]
        ));
        assert!(!controller_loop_intervention_markers_are_stale(
            true,
            false,
            &[],
            &[]
        ));
        assert!(!controller_loop_intervention_markers_are_stale(
            false,
            true,
            &[],
            &[]
        ));
    }

    #[test]
    fn controller_loop_status_reports_live_pid_counts() {
        let dir = tempdir().unwrap();
        let loop_dir = dir.path().join(".intendant/controller-loop");
        let run_dir = loop_dir.join("20260101T000000Z-1234");
        std::fs::create_dir_all(&run_dir).unwrap();
        std::fs::write(run_dir.join("wrapper.pid"), std::process::id().to_string()).unwrap();
        std::fs::write(run_dir.join("codex.pid"), std::process::id().to_string()).unwrap();
        std::fs::write(loop_dir.join("latest.run_id"), "20260101T000000Z-1234").unwrap();
        std::fs::write(
            loop_dir.join("latest.status.json"),
            r#"{"run_id":"20260101T000000Z-1234","state":"running"}"#,
        )
        .unwrap();

        let status = collect_controller_loop_status(&loop_dir);
        assert_eq!(
            status
                .get("active")
                .and_then(|v| v.get("wrapper_count"))
                .and_then(|v| v.as_u64()),
            Some(1)
        );
        assert_eq!(
            status
                .get("active")
                .and_then(|v| v.get("codex_count"))
                .and_then(|v| v.as_u64()),
            Some(1)
        );
        assert_eq!(
            status
                .get("latest")
                .and_then(|v| v.get("run_id"))
                .and_then(|v| v.as_str()),
            Some("20260101T000000Z-1234")
        );
    }

    #[test]
    fn controller_loop_status_enriches_live_app_server_from_wrapper_index() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let loop_dir = home.join(".intendant/controller-loop");
        let log_dir = home.join(".intendant/logs/wrapper-session");
        std::fs::create_dir_all(&loop_dir).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        std::fs::write(
            log_dir.join("session_meta.json"),
            serde_json::json!({
                "session_id": "wrapper-session",
                "created_at": "2026-01-01T00:00:00Z",
                "status": "running"
            })
            .to_string(),
        )
        .unwrap();
        crate::external_wrapper_index::upsert(
            home,
            "codex",
            "3addb0e1-b533-4836-8165-d8ad0c198e4b",
            "wrapper-session",
            &log_dir,
            None,
        )
        .unwrap();

        let wrappers = active_external_wrappers_from_index(&loop_dir, &[1084559]);
        assert_eq!(wrappers.len(), 1);
        assert_eq!(
            wrappers[0]
                .get("backend_session_id")
                .and_then(|value| value.as_str()),
            Some("3addb0e1-b533-4836-8165-d8ad0c198e4b")
        );
        assert_eq!(
            wrappers[0]
                .get("intendant_session_id")
                .and_then(|value| value.as_str()),
            Some("wrapper-session")
        );
        assert_eq!(
            wrappers[0].get("status").and_then(|value| value.as_str()),
            Some("running")
        );
        let latest = latest_status_from_active_wrappers(&wrappers).unwrap();
        assert_eq!(
            latest
                .get("backend_session_id")
                .and_then(|value| value.as_str()),
            Some("3addb0e1-b533-4836-8165-d8ad0c198e4b")
        );
        assert_eq!(
            latest.get("state").and_then(|value| value.as_str()),
            Some("running")
        );
    }

    #[test]
    fn controller_loop_status_does_not_report_idle_for_active_codex_process_tree() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let loop_dir = home.join(".intendant/controller-loop");
        let log_dir = home.join(".intendant/logs/wrapper-session");
        std::fs::create_dir_all(&loop_dir).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        std::fs::write(
            log_dir.join("session_meta.json"),
            serde_json::json!({
                "session_id": "wrapper-session",
                "created_at": "2026-01-01T00:00:00Z",
                "status": "idle"
            })
            .to_string(),
        )
        .unwrap();
        crate::external_wrapper_index::upsert(
            home,
            "codex",
            "019e9b9a-8557-7b01-99ef-187e8840327f",
            "wrapper-session",
            &log_dir,
            None,
        )
        .unwrap();

        let wrappers = active_external_wrappers_from_index_homes_with_probe(
            [home.to_path_buf()].iter(),
            &[8892],
            |pid| pid == 8892,
        );

        assert_eq!(wrappers.len(), 1);
        assert_eq!(
            wrappers[0].get("status").and_then(|value| value.as_str()),
            Some("unknown_running")
        );
        assert_eq!(
            wrappers[0]
                .get("session_meta_status")
                .and_then(|value| value.as_str()),
            Some("idle")
        );
        assert_eq!(
            wrappers[0]
                .get("process_tree_active")
                .and_then(|value| value.as_bool()),
            Some(true)
        );

        let latest = controller_loop_latest_status(
            serde_json::json!({
                "run_id": "stale-run",
                "state": "idle"
            }),
            &wrappers,
        );
        assert_eq!(
            latest.get("state").and_then(|value| value.as_str()),
            Some("unknown_running")
        );
        assert_eq!(
            latest.get("source").and_then(|value| value.as_str()),
            Some("external_wrapper_index")
        );
        assert_eq!(
            latest
                .get("session_meta_status")
                .and_then(|value| value.as_str()),
            Some("idle")
        );
        assert_eq!(
            latest
                .get("process_tree_active")
                .and_then(|value| value.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn controller_loop_status_enriches_index_wrapper_from_live_mcp_state() {
        let mut app_state = McpAppState::new(
            "openai".to_string(),
            "gpt-5.2-codex".to_string(),
            autonomy::shared_autonomy(AutonomyState::default()),
            std::path::PathBuf::from("/tmp/test-session"),
        );
        app_state.link_session_aliases("wrapper-session", "codex-thread");
        app_state.note_session_phase(
            Some("codex-thread"),
            Some(14),
            Phase::RunningAgent,
            Some("Codex follow-up round 14 in progress: fix the controller status"),
        );
        app_state.note_session_round(Some("codex-thread"), 14);

        let mut status = serde_json::json!({
            "latest": {
                "status": {
                    "run_id": "stale-run",
                    "state": "idle"
                }
            },
            "active": {
                "wrappers": [{
                    "run_id": null,
                    "pid": null,
                    "codex_pid": 8892,
                    "app_server_pid": 8892,
                    "app_server_active": true,
                    "source": "external_wrapper_index",
                    "backend_source": "codex",
                    "backend_session_id": "codex-thread",
                    "intendant_session_id": "wrapper-session",
                    "log_path": "/tmp/test-session",
                    "status": "unknown_running",
                    "session_meta_status": "idle",
                    "process_tree_active": true,
                    "updated_at_secs": 10
                }]
            }
        });

        enrich_controller_loop_status_with_mcp_state_at(&mut status, &app_state, 12345);

        let wrapper = status.pointer("/active/wrappers/0").unwrap();
        assert_eq!(
            wrapper.get("status").and_then(|value| value.as_str()),
            Some("running_agent")
        );
        assert_eq!(
            wrapper
                .get("session_meta_status")
                .and_then(|value| value.as_str()),
            Some("running_agent")
        );
        assert_eq!(
            wrapper
                .get("raw_session_meta_status")
                .and_then(|value| value.as_str()),
            Some("idle")
        );
        assert_eq!(
            wrapper.get("turn").and_then(|value| value.as_u64()),
            Some(14)
        );
        assert_eq!(
            wrapper.get("round").and_then(|value| value.as_u64()),
            Some(14)
        );
        assert_eq!(
            wrapper
                .get("updated_at_secs")
                .and_then(|value| value.as_u64()),
            Some(12345)
        );
        assert_eq!(
            status
                .pointer("/latest/status/state")
                .and_then(|value| value.as_str()),
            Some("running_agent")
        );
        assert_eq!(
            status
                .pointer("/latest/status/turn")
                .and_then(|value| value.as_u64()),
            Some(14)
        );
        assert!(!controller_loop_intervention_markers_are_stale(
            false,
            false,
            &[wrapper.clone()],
            &[serde_json::json!({"pid": 8892})]
        ));
    }

    #[test]
    fn controller_loop_status_preserves_idle_app_server_residency_after_round() {
        let mut app_state = McpAppState::new(
            "openai".to_string(),
            "gpt-5.2-codex".to_string(),
            autonomy::shared_autonomy(AutonomyState::default()),
            std::path::PathBuf::from("/tmp/test-session"),
        );
        app_state.link_session_aliases("wrapper-session", "codex-thread");
        app_state.note_session_phase(
            Some("codex-thread"),
            Some(14),
            Phase::WaitingFollowUp,
            Some("Codex follow-up round 14 complete"),
        );
        app_state.note_session_round(Some("codex-thread"), 14);

        let mut status = serde_json::json!({
            "latest": {
                "status": {
                    "run_id": "stale-run",
                    "state": "idle"
                }
            },
            "active": {
                "wrappers": [{
                    "run_id": null,
                    "pid": null,
                    "codex_pid": 8892,
                    "app_server_pid": 8892,
                    "app_server_active": true,
                    "source": "external_wrapper_index",
                    "backend_source": "codex",
                    "backend_session_id": "codex-thread",
                    "intendant_session_id": "wrapper-session",
                    "log_path": "/tmp/test-session",
                    "status": "unknown_running",
                    "session_meta_status": "idle",
                    "process_tree_active": true,
                    "updated_at_secs": 10
                }]
            }
        });

        enrich_controller_loop_status_with_mcp_state_at(&mut status, &app_state, 12345);

        let wrapper = status.pointer("/active/wrappers/0").unwrap();
        assert_eq!(
            wrapper.get("phase").and_then(|value| value.as_str()),
            Some("waiting_follow_up")
        );
        assert_eq!(
            wrapper.get("status").and_then(|value| value.as_str()),
            Some("unknown_running")
        );
        assert_eq!(
            wrapper
                .get("session_meta_status")
                .and_then(|value| value.as_str()),
            Some("idle")
        );
        assert!(wrapper.get("raw_session_meta_status").is_none());
        assert_eq!(
            wrapper
                .get("updated_at_secs")
                .and_then(|value| value.as_u64()),
            Some(10)
        );
        assert_eq!(
            status
                .pointer("/latest/status/state")
                .and_then(|value| value.as_str()),
            Some("waiting_follow_up")
        );
        assert_eq!(
            status
                .pointer("/latest/status/phase")
                .and_then(|value| value.as_str()),
            Some("waiting_follow_up")
        );
        assert_eq!(
            status
                .pointer("/latest/status/turn")
                .and_then(|value| value.as_u64()),
            Some(14)
        );
        assert!(controller_loop_intervention_markers_are_stale(
            false,
            false,
            &[wrapper.clone()],
            &[serde_json::json!({"pid": 8892})]
        ));
    }

    #[test]
    fn controller_loop_status_reports_live_interrupted_phase_over_app_server_residency() {
        let mut app_state = McpAppState::new(
            "openai".to_string(),
            "gpt-5.2-codex".to_string(),
            autonomy::shared_autonomy(AutonomyState::default()),
            std::path::PathBuf::from("/tmp/test-session"),
        );
        app_state.link_session_aliases("wrapper-session", "codex-thread");
        app_state.note_session_phase(
            Some("codex-thread"),
            Some(14),
            Phase::Interrupted,
            Some("Codex follow-up round 14 interrupted"),
        );
        app_state.note_session_round(Some("codex-thread"), 14);

        let mut status = serde_json::json!({
            "latest": {
                "status": {
                    "run_id": "stale-run",
                    "state": "running"
                }
            },
            "active": {
                "wrappers": [{
                    "run_id": null,
                    "pid": null,
                    "codex_pid": 8892,
                    "app_server_pid": 8892,
                    "app_server_active": true,
                    "source": "external_wrapper_index",
                    "backend_source": "codex",
                    "backend_session_id": "codex-thread",
                    "intendant_session_id": "wrapper-session",
                    "log_path": "/tmp/test-session",
                    "status": "unknown_running",
                    "session_meta_status": "idle",
                    "process_tree_active": true,
                    "updated_at_secs": 10
                }]
            }
        });

        enrich_controller_loop_status_with_mcp_state_at(&mut status, &app_state, 12345);

        let wrapper = status.pointer("/active/wrappers/0").unwrap();
        assert_eq!(
            wrapper.get("phase").and_then(|value| value.as_str()),
            Some("interrupted")
        );
        assert_eq!(
            wrapper.get("status").and_then(|value| value.as_str()),
            Some("unknown_running")
        );
        assert_eq!(
            status
                .pointer("/latest/status/state")
                .and_then(|value| value.as_str()),
            Some("interrupted")
        );
        assert_eq!(
            status
                .pointer("/latest/status/turn")
                .and_then(|value| value.as_u64()),
            Some(14)
        );
    }

    #[test]
    fn controller_loop_status_uses_indexed_app_server_when_wrapper_pid_is_alive() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let loop_dir = home.join(".intendant/controller-loop");
        let log_dir = home.join(".intendant/logs/wrapper-session");
        std::fs::create_dir_all(&loop_dir).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        std::fs::write(
            log_dir.join("session_meta.json"),
            serde_json::json!({
                "session_id": "wrapper-session",
                "created_at": "2026-01-01T00:00:00Z",
                "status": "idle"
            })
            .to_string(),
        )
        .unwrap();
        crate::external_wrapper_index::upsert(
            home,
            "codex",
            "724fafac-36d7-41e5-b822-e0a08c1f4701",
            "wrapper-session",
            &log_dir,
            None,
        )
        .unwrap();

        let mut wrappers = vec![serde_json::json!({
            "run_id": "20260101T000000Z-1297050",
            "pid": 1297050
        })];
        wrappers.extend(active_external_wrappers_from_index_homes_with_probe(
            [home.to_path_buf()].iter(),
            &[1298123],
            |pid| pid == 1298123,
        ));

        let latest = controller_loop_latest_status(
            serde_json::json!({
                "run_id": "20260101T000000Z-1297050",
                "state": "idle",
                "process_tree_active": false
            }),
            &wrappers,
        );
        assert_eq!(
            latest.get("state").and_then(|value| value.as_str()),
            Some("unknown_running")
        );
        assert_eq!(
            latest
                .get("app_server_pid")
                .and_then(|value| value.as_u64()),
            Some(1298123)
        );
        assert_eq!(
            latest
                .get("app_server_active")
                .and_then(|value| value.as_bool()),
            Some(true)
        );
        assert_eq!(
            latest
                .get("session_meta_status")
                .and_then(|value| value.as_str()),
            Some("idle")
        );
    }

    #[test]
    fn codex_app_server_process_tree_active_includes_root_pid_liveness() {
        assert!(codex_app_server_process_tree_active_with_root(
            1298123,
            std::iter::empty(),
            |pid| pid == 1298123,
            |_| None,
        ));
    }

    #[test]
    fn codex_app_server_process_tree_active_requires_live_descendant_cmdline_when_root_dead() {
        let cmdlines = std::collections::HashMap::from([
            (101, "cargo build --release".to_string()),
            (102, String::new()),
            (103, "sleep 60".to_string()),
        ]);

        assert!(codex_app_server_process_tree_active_with_root(
            100,
            [101],
            |pid| pid == 101,
            |pid| cmdlines.get(&pid).cloned(),
        ));
        assert!(codex_app_server_process_tree_active_from_descendants(
            [101],
            |_| true,
            |pid| cmdlines.get(&pid).cloned(),
        ));
        assert!(!codex_app_server_process_tree_active_from_descendants(
            [101],
            |_| false,
            |pid| cmdlines.get(&pid).cloned(),
        ));
        assert!(!codex_app_server_process_tree_active_from_descendants(
            [102],
            |_| true,
            |pid| cmdlines.get(&pid).cloned(),
        ));
    }

    #[test]
    fn controller_loop_status_normalizes_stale_wrapper_index_identity_from_log_path() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let loop_dir = home.join(".intendant/controller-loop");
        let log_dir = home.join(".intendant/logs/resumed-wrapper-session");
        std::fs::create_dir_all(&loop_dir).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        std::fs::write(
            log_dir.join("session_meta.json"),
            serde_json::json!({
                "session_id": "resumed-wrapper-session",
                "created_at": "2026-01-01T00:00:00Z",
                "status": "running"
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            crate::external_wrapper_index::index_path(home),
            serde_json::json!({
                "version": 1,
                "wrappers": [{
                    "source": "codex",
                    "backend_session_id": "8b008615-9bf6-44a6-9d26-751e4fd7d87f",
                    "intendant_session_id": "5f979c8d-65e7-4210-be22-e4012242b745",
                    "log_path": log_dir,
                    "updated_at_secs": 1
                }]
            })
            .to_string(),
        )
        .unwrap();

        let wrappers = active_external_wrappers_from_index(&loop_dir, &[1084559]);
        assert_eq!(wrappers.len(), 1);
        assert_eq!(
            wrappers[0]
                .get("intendant_session_id")
                .and_then(|value| value.as_str()),
            Some("resumed-wrapper-session")
        );
        assert_eq!(
            wrappers[0].get("log_path").and_then(|value| value.as_str()),
            Some(log_dir.to_string_lossy().as_ref())
        );
        let latest = latest_status_from_active_wrappers(&wrappers).unwrap();
        assert_eq!(
            latest
                .get("intendant_session_id")
                .and_then(|value| value.as_str()),
            Some("resumed-wrapper-session")
        );
    }

    #[test]
    fn controller_loop_status_searches_user_home_wrapper_index_for_project_local_loop_dir() {
        let dir = tempdir().unwrap();
        let project_home = dir.path().join("project");
        let user_home = dir.path().join("home");
        let loop_dir = project_home.join(".intendant/controller-loop");
        let project_root = dir.path().join("workspace");
        let log_dir = user_home.join(".intendant/logs/wrapper-session");
        std::fs::create_dir_all(&loop_dir).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        std::fs::create_dir_all(&project_root).unwrap();
        std::fs::write(
            log_dir.join("session_meta.json"),
            serde_json::json!({
                "session_id": "wrapper-session",
                "created_at": "2026-01-01T00:00:00Z",
                "status": "running",
                "project_root": project_root
            })
            .to_string(),
        )
        .unwrap();
        crate::external_wrapper_index::upsert(
            &user_home,
            "codex",
            "019e9b9a-8557-7b01-99ef-187e8840327f",
            "wrapper-session",
            &log_dir,
            Some(&project_root),
        )
        .unwrap();

        let candidate_homes = vec![project_home, user_home];
        let wrappers = active_external_wrappers_from_index_homes(candidate_homes.iter(), &[8892]);
        assert_eq!(wrappers.len(), 1);
        assert_eq!(
            wrappers[0]
                .get("backend_session_id")
                .and_then(|value| value.as_str()),
            Some("019e9b9a-8557-7b01-99ef-187e8840327f")
        );
        assert_eq!(
            wrappers[0]
                .get("intendant_session_id")
                .and_then(|value| value.as_str()),
            Some("wrapper-session")
        );
        assert_eq!(
            wrappers[0]
                .get("codex_pid")
                .and_then(|value| value.as_u64()),
            Some(8892)
        );
        let project_root_string = project_root.to_string_lossy().to_string();
        assert_eq!(
            wrappers[0]
                .get("project_root")
                .and_then(|value| value.as_str()),
            Some(project_root_string.as_str())
        );
    }

    #[test]
    fn controller_loop_status_prefers_live_codex_cwd_project_root() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let helper_root = home.join("helper-root");
        let station_root = home.join("station-worktree");
        let log_dir = home.join(".intendant/logs/wrapper-session");
        std::fs::create_dir_all(&helper_root).unwrap();
        std::fs::create_dir_all(station_root.join(".git")).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        std::fs::write(
            log_dir.join("session_meta.json"),
            serde_json::json!({
                "session_id": "wrapper-session",
                "created_at": "2026-01-01T00:00:00Z",
                "status": "running",
                "project_root": helper_root
            })
            .to_string(),
        )
        .unwrap();
        crate::external_wrapper_index::upsert(
            home,
            "codex",
            "019ea0a9-92fc-7471-85d8-0a281fc54250",
            "wrapper-session",
            &log_dir,
            Some(&helper_root),
        )
        .unwrap();

        let wrappers = active_external_wrappers_from_index_homes_with_probe_and_cwd(
            [home.to_path_buf()].iter(),
            &[1588453],
            |pid| pid == 1588453,
            |pid| (pid == 1588453).then(|| station_root.clone()),
        );
        assert_eq!(wrappers.len(), 1);
        let station_root_string = station_root.to_string_lossy().to_string();
        assert_eq!(
            wrappers[0].get("cwd").and_then(|value| value.as_str()),
            Some(station_root_string.as_str())
        );
        assert_eq!(
            wrappers[0]
                .get("project_root")
                .and_then(|value| value.as_str()),
            Some(station_root_string.as_str())
        );
    }

    #[test]
    fn controller_loop_status_does_not_overreport_index_wrappers_without_live_pids() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let loop_dir = home.join(".intendant/controller-loop");
        std::fs::create_dir_all(&loop_dir).unwrap();
        for idx in 0..2 {
            let log_dir = home.join(format!(".intendant/logs/wrapper-session-{idx}"));
            std::fs::create_dir_all(&log_dir).unwrap();
            std::fs::write(
                log_dir.join("session_meta.json"),
                serde_json::json!({
                    "session_id": format!("wrapper-session-{idx}"),
                    "created_at": "2026-01-01T00:00:00Z",
                    "status": "running"
                })
                .to_string(),
            )
            .unwrap();
            crate::external_wrapper_index::upsert(
                home,
                "codex",
                &format!("019e9b9a-8557-7b01-99ef-187e8840327{idx}"),
                &format!("wrapper-session-{idx}"),
                &log_dir,
                None,
            )
            .unwrap();
        }

        let wrappers = active_external_wrappers_from_index(&loop_dir, &[1084559]);
        assert_eq!(wrappers.len(), 1);
        assert_eq!(
            wrappers[0]
                .get("codex_pid")
                .and_then(|value| value.as_u64()),
            Some(1084559)
        );
    }

    #[test]
    fn controller_loop_status_recognizes_codex_app_server_cmdlines() {
        assert!(is_codex_app_server_cmdline(
            "/home/user/projects/codex/codex-rs/target/debug/codex --dangerously-bypass-approvals-and-sandbox app-server -c mcp_servers.intendant.url=..."
        ));
        assert!(is_codex_app_server_cmdline(
            "/opt/homebrew/bin/codex app-server -c model_auto_compact_token_limit=9223372036854775807"
        ));
        assert!(!is_codex_app_server_cmdline(
            "/home/user/projects/intendant/target/release/intendant --web 8892"
        ));
        assert!(!is_codex_app_server_cmdline("[codex] <defunct>"));
        assert!(!is_codex_app_server_cmdline(
            "/usr/bin/codex completion bash"
        ));
    }

    #[test]
    fn controller_loop_status_selects_process_tree_codex_app_servers() {
        let known = HashSet::from([22]);
        let cmdlines = std::collections::HashMap::from([
            (
                11,
                "/opt/homebrew/bin/codex app-server -c foo=bar".to_string(),
            ),
            (22, "/home/user/bin/codex app-server".to_string()),
            (33, "/home/user/bin/codex exec --json".to_string()),
            (44, "/bin/sh -c sleep 60".to_string()),
            (55, "/tmp/codex app-server".to_string()),
        ]);

        let pids =
            live_codex_app_server_pids_from_descendants([55, 44, 33, 22, 11, 11], &known, |pid| {
                cmdlines.get(&pid).cloned()
            });

        assert_eq!(pids, vec![11, 55]);
    }

    #[test]
    fn controller_loop_intervention_mode_validation() {
        let dir = tempdir().unwrap();
        let loop_dir = dir.path().join(".intendant/controller-loop");
        let intervention =
            request_loop_intervention_marker_for_root(&loop_dir, "stop", u32::MAX).unwrap();
        assert_eq!(intervention.mode.as_str(), "stop");
        assert!(intervention.signaled_codex_app_server_pids.is_empty());
        assert!(loop_dir.join("request_stop").exists());

        let err =
            request_loop_intervention_marker_for_root(&loop_dir, "bad", u32::MAX).unwrap_err();
        assert!(err.contains("expected 'stop' or 'abort'"));
    }

    #[test]
    fn intervention_order_report_detects_out_of_order_events() {
        let dir = tempdir().unwrap();
        let run_dir = dir.path().join("run");
        std::fs::create_dir_all(&run_dir).unwrap();
        std::fs::write(
            run_dir.join("intervention.log"),
            "2026-01-01T00:00:00Z run_started run_id=x\n\
             2026-01-01T00:00:01Z cleanup_begin state=exited\n\
             2026-01-01T00:00:02Z codex_started codex_pid=1\n\
             2026-01-01T00:00:03Z cleanup_end state=exited\n",
        )
        .unwrap();
        let report = intervention_order_report(&run_dir);
        assert_eq!(report["has_log"].as_bool(), Some(true));
        assert_eq!(report["order_ok"].as_bool(), Some(false));
    }

    #[test]
    fn resource_definitions_has_seven_entries() {
        let defs = resource_definitions();
        assert_eq!(defs.len(), 7);
    }

    #[test]
    fn format_outcome_ok() {
        assert_eq!(format_outcome(ActionOutcome::Ok), "ok");
    }

    #[test]
    fn format_outcome_noop() {
        let s = format_outcome(ActionOutcome::NoOp {
            reason: "test".to_string(),
        });
        assert!(s.starts_with("no-op:"));
        assert!(s.contains("test"));
    }

    #[test]
    fn server_info_has_correct_name() {
        let state = test_state();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let bus = EventBus::new();
            let server = IntendantServer::new(state, bus);
            let info = server.get_info();
            assert_eq!(info.server_info.name, "intendant");
            assert!(info.instructions.is_some());
        });
    }

    #[test]
    fn all_user_actions_handled_by_process_action() {
        // This test ensures process_action_sync handles every UserAction variant.
        // If a new variant is added, the exhaustive match in process_action_sync
        // will cause a compile error, AND this test will need updating.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let actions: Vec<UserAction> = vec![
                UserAction::Approve { id: 1 },
                UserAction::Deny { id: 1 },
                UserAction::Skip { id: 1 },
                UserAction::ApproveAll { id: 1 },
                UserAction::RespondHuman {
                    text: "test".to_string(),
                },
                UserAction::SetAutonomy {
                    level: AutonomyLevel::High,
                },
                UserAction::SetVerbosity {
                    level: Verbosity::Normal,
                },
                UserAction::Quit,
            ];
            for action in actions {
                let mut s = state.write().await;
                // Should not panic for any variant
                let _ = process_action_sync(&mut s, action);
            }
        });
    }

    #[test]
    fn approval_snapshot_none_when_empty() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let s = state.read().await;
            assert!(s.approval_snapshot().is_none());
        });
    }

    #[test]
    fn approval_snapshot_present_when_set() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            s.pending_approval = Some(PendingApprovalState {
                id: 42,
                command_preview: "rm -rf /".to_string(),
                category: "destructive".to_string(),
            });
            let snap = s.approval_snapshot().unwrap();
            assert_eq!(snap.id, 42);
            assert_eq!(snap.category, "destructive");
        });
    }

    #[test]
    fn human_question_snapshot_roundtrip() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let state = test_state();
            let mut s = state.write().await;
            assert!(s.human_question_snapshot().is_none());
            s.human_question = Some("Which database?".to_string());
            let snap = s.human_question_snapshot().unwrap();
            assert_eq!(snap.question, "Which database?");
        });
    }

    #[tokio::test]
    async fn schedule_restart_rejects_missing_actions() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let server = IntendantServer::new(state, bus);

        let output = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("now".to_string()),
                restart_command: None,
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;

        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(false));
        assert_eq!(json["status"].as_str(), Some("rejected"));
        assert!(json["error"]
            .as_str()
            .unwrap_or("")
            .contains("configure at least one restart action"));
    }

    #[tokio::test]
    async fn schedule_restart_normalizes_string_fields() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let server = IntendantServer::new(state.clone(), bus);

        let output = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "  codex  ".to_string(),
                north_star_goal: "  improve loop  ".to_string(),
                reason: Some("  periodic refresh  ".to_string()),
                restart_after: Some("  NOW  ".to_string()),
                restart_command: Some("  true  ".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(true));

        let s = state.read().await;
        let restart = s
            .controller_restart
            .as_ref()
            .expect("restart should be stored");
        assert_eq!(restart.controller_id, "codex");
        assert_eq!(restart.north_star_goal, "improve loop");
        assert_eq!(restart.reason.as_deref(), Some("periodic refresh"));
        assert_eq!(restart.restart_after, RestartAfter::Now);
        assert_eq!(restart.restart_command.as_deref(), Some("true"));
    }

    #[tokio::test]
    async fn schedule_restart_now_reports_completed_phase() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let server = IntendantServer::new(state, bus);

        let output = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("now".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(true));
        assert_eq!(json["phase"].as_str(), Some("completed"));
        assert!(json["execution"].as_str().unwrap_or("").contains("spawned"));
    }

    #[tokio::test]
    async fn schedule_restart_now_reports_failed_phase() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let server = IntendantServer::new(state, bus);

        let output = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("now".to_string()),
                restart_command: None,
                auto_start_task: Some(true),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(false));
        assert_eq!(json["phase"].as_str(), Some("failed"));
        assert!(json["execution_error"]
            .as_str()
            .unwrap_or("")
            .contains("Failed to start follow-up task"));
    }

    #[tokio::test]
    async fn control_schedule_restart_rejects_missing_actions() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let (control_tx, mut control_rx) = broadcast::channel::<String>(8);

        let resource = handle_control_command_mcp(
            &state,
            &bus,
            &Some(control_tx),
            ControlMsg::ScheduleControllerRestart {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("now".to_string()),
                restart_command: None,
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            },
        )
        .await;

        assert_eq!(resource, Some(RESOURCE_RESTART_URI));

        let event = timeout(Duration::from_millis(200), control_rx.recv())
            .await
            .expect("timed out waiting for command_result")
            .expect("broadcast recv failed");
        let json: serde_json::Value = serde_json::from_str(&event).unwrap();
        assert_eq!(
            json.get("event").and_then(|v| v.as_str()),
            Some("command_result")
        );
        assert_eq!(
            json.get("action").and_then(|v| v.as_str()),
            Some("schedule_controller_restart")
        );
        assert_eq!(json.get("ok").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(
            json.get("message").and_then(|v| v.as_str()),
            Some(
                "Invalid request: configure at least one restart action (restart_command and/or auto_start_task=true)"
            )
        );
    }

    #[tokio::test]
    async fn control_schedule_restart_now_reports_completed_phase() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let (control_tx, mut control_rx) = broadcast::channel::<String>(8);

        let resource = handle_control_command_mcp(
            &state,
            &bus,
            &Some(control_tx),
            ControlMsg::ScheduleControllerRestart {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("now".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            },
        )
        .await;

        assert_eq!(resource, Some(RESOURCE_RESTART_URI));
        let event = timeout(Duration::from_millis(200), control_rx.recv())
            .await
            .expect("timed out waiting for command_result")
            .expect("broadcast recv failed");
        let json: serde_json::Value = serde_json::from_str(&event).unwrap();
        assert_eq!(
            json.get("event").and_then(|v| v.as_str()),
            Some("command_result")
        );
        assert_eq!(
            json.get("action").and_then(|v| v.as_str()),
            Some("schedule_controller_restart")
        );
        assert_eq!(json.get("ok").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(
            json.get("data")
                .and_then(|v| v.get("phase"))
                .and_then(|v| v.as_str()),
            Some("completed")
        );
    }

    #[tokio::test]
    async fn control_get_restart_status_redacts_turn_complete_token() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let (control_tx, mut control_rx) = broadcast::channel::<String>(8);

        let resource = handle_control_command_mcp(
            &state,
            &bus,
            &Some(control_tx.clone()),
            ControlMsg::ScheduleControllerRestart {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            },
        )
        .await;
        assert_eq!(resource, Some(RESOURCE_RESTART_URI));

        let scheduled_event = timeout(Duration::from_millis(200), control_rx.recv())
            .await
            .expect("timed out waiting for scheduled command_result")
            .expect("broadcast recv failed");
        let scheduled_json: serde_json::Value = serde_json::from_str(&scheduled_event).unwrap();
        let token = scheduled_json
            .get("data")
            .and_then(|v| v.get("turn_complete_token"))
            .and_then(|v| v.as_str())
            .expect("schedule payload should include raw token")
            .to_string();

        let resource = handle_control_command_mcp(
            &state,
            &bus,
            &Some(control_tx),
            ControlMsg::GetRestartStatus,
        )
        .await;
        assert_eq!(resource, Some(RESOURCE_RESTART_URI));

        let status_event = timeout(Duration::from_millis(200), control_rx.recv())
            .await
            .expect("timed out waiting for status command_result")
            .expect("broadcast recv failed");
        let status_json: serde_json::Value = serde_json::from_str(&status_event).unwrap();
        assert_eq!(
            status_json.get("action").and_then(|v| v.as_str()),
            Some("get_restart_status")
        );
        assert_eq!(
            status_json
                .get("data")
                .and_then(|v| v.get("turn_complete_token"))
                .and_then(|v| v.as_str()),
            Some("[redacted]")
        );
        assert_ne!(
            status_json
                .get("data")
                .and_then(|v| v.get("turn_complete_token"))
                .and_then(|v| v.as_str()),
            Some(token.as_str())
        );
    }

    #[tokio::test]
    async fn control_controller_turn_complete_returns_structured_data_payload() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let (control_tx, mut control_rx) = broadcast::channel::<String>(8);

        let resource = handle_control_command_mcp(
            &state,
            &bus,
            &Some(control_tx.clone()),
            ControlMsg::ScheduleControllerRestart {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            },
        )
        .await;
        assert_eq!(resource, Some(RESOURCE_RESTART_URI));

        let scheduled_event = timeout(Duration::from_millis(200), control_rx.recv())
            .await
            .expect("timed out waiting for schedule command_result")
            .expect("broadcast recv failed");
        let scheduled_json: serde_json::Value = serde_json::from_str(&scheduled_event).unwrap();
        let restart_id = scheduled_json
            .get("data")
            .and_then(|v| v.get("restart_id"))
            .and_then(|v| v.as_str())
            .expect("schedule payload should include restart_id")
            .to_string();
        let token = scheduled_json
            .get("data")
            .and_then(|v| v.get("turn_complete_token"))
            .and_then(|v| v.as_str())
            .expect("schedule payload should include token")
            .to_string();

        let resource = handle_control_command_mcp(
            &state,
            &bus,
            &Some(control_tx),
            ControlMsg::ControllerTurnComplete {
                restart_id: restart_id.clone(),
                turn_complete_token: token,
                status: None,
                handoff_summary: None,
            },
        )
        .await;
        assert_eq!(resource, Some(RESOURCE_RESTART_URI));

        let event = timeout(Duration::from_millis(200), control_rx.recv())
            .await
            .expect("timed out waiting for turn_complete command_result")
            .expect("broadcast recv failed");
        let json: serde_json::Value = serde_json::from_str(&event).unwrap();
        assert_eq!(
            json.get("action").and_then(|v| v.as_str()),
            Some("controller_turn_complete")
        );
        assert_eq!(json.get("ok").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(
            json.get("data")
                .and_then(|v| v.get("status"))
                .and_then(|v| v.as_str()),
            Some("completed")
        );
        assert_eq!(
            json.get("data")
                .and_then(|v| v.get("restart_id"))
                .and_then(|v| v.as_str()),
            Some(restart_id.as_str())
        );
        assert_eq!(
            json.get("data")
                .and_then(|v| v.get("phase"))
                .and_then(|v| v.as_str()),
            Some("completed")
        );
    }

    #[tokio::test]
    async fn control_cancel_controller_restart_returns_structured_data_payloads() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let (control_tx, mut control_rx) = broadcast::channel::<String>(8);

        let resource = handle_control_command_mcp(
            &state,
            &bus,
            &Some(control_tx.clone()),
            ControlMsg::CancelControllerRestart {
                restart_id: Some("abc".to_string()),
            },
        )
        .await;
        assert_eq!(resource, Some(RESOURCE_RESTART_URI));
        let rejected_event = timeout(Duration::from_millis(200), control_rx.recv())
            .await
            .expect("timed out waiting for rejected cancel command_result")
            .expect("broadcast recv failed");
        let rejected_json: serde_json::Value = serde_json::from_str(&rejected_event).unwrap();
        assert_eq!(
            rejected_json.get("ok").and_then(|v| v.as_bool()),
            Some(false)
        );
        assert_eq!(
            rejected_json
                .get("data")
                .and_then(|v| v.get("status"))
                .and_then(|v| v.as_str()),
            Some("rejected")
        );
        assert_eq!(
            rejected_json
                .get("data")
                .and_then(|v| v.get("restart_id"))
                .and_then(|v| v.as_str()),
            Some("abc")
        );

        let resource = handle_control_command_mcp(
            &state,
            &bus,
            &Some(control_tx.clone()),
            ControlMsg::ScheduleControllerRestart {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            },
        )
        .await;
        assert_eq!(resource, Some(RESOURCE_RESTART_URI));
        let scheduled_event = timeout(Duration::from_millis(200), control_rx.recv())
            .await
            .expect("timed out waiting for schedule command_result")
            .expect("broadcast recv failed");
        let scheduled_json: serde_json::Value = serde_json::from_str(&scheduled_event).unwrap();
        let restart_id = scheduled_json
            .get("data")
            .and_then(|v| v.get("restart_id"))
            .and_then(|v| v.as_str())
            .expect("schedule payload should include restart_id")
            .to_string();

        let resource = handle_control_command_mcp(
            &state,
            &bus,
            &Some(control_tx),
            ControlMsg::CancelControllerRestart { restart_id: None },
        )
        .await;
        assert_eq!(resource, Some(RESOURCE_RESTART_URI));
        let cancelled_event = timeout(Duration::from_millis(200), control_rx.recv())
            .await
            .expect("timed out waiting for successful cancel command_result")
            .expect("broadcast recv failed");
        let cancelled_json: serde_json::Value = serde_json::from_str(&cancelled_event).unwrap();
        assert_eq!(
            cancelled_json.get("ok").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            cancelled_json
                .get("data")
                .and_then(|v| v.get("status"))
                .and_then(|v| v.as_str()),
            Some("cancelled")
        );
        assert_eq!(
            cancelled_json
                .get("data")
                .and_then(|v| v.get("restart_id"))
                .and_then(|v| v.as_str()),
            Some(restart_id.as_str())
        );
        assert_eq!(
            cancelled_json
                .get("data")
                .and_then(|v| v.get("phase"))
                .and_then(|v| v.as_str()),
            Some("cancelled")
        );
    }

    #[tokio::test]
    async fn schedule_restart_rejects_invalid_restart_after() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let server = IntendantServer::new(state, bus);

        let output = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("later".to_string()),
                restart_command: None,
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;

        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(false));
        assert_eq!(json["status"].as_str(), Some("rejected"));
        assert!(json["error"]
            .as_str()
            .unwrap_or("")
            .contains("restart_after must be 'turn_end' or 'now'"));
    }

    #[tokio::test]
    async fn control_schedule_restart_rejects_zero_max_attempts() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let (control_tx, mut control_rx) = broadcast::channel::<String>(8);

        let resource = handle_control_command_mcp(
            &state,
            &bus,
            &Some(control_tx),
            ControlMsg::ScheduleControllerRestart {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: None,
                auto_start_task: Some(false),
                max_attempts: Some(0),
                cooldown_sec: None,
            },
        )
        .await;

        assert_eq!(resource, Some(RESOURCE_RESTART_URI));

        let event = timeout(Duration::from_millis(200), control_rx.recv())
            .await
            .expect("timed out waiting for command_result")
            .expect("broadcast recv failed");
        let json: serde_json::Value = serde_json::from_str(&event).unwrap();
        assert_eq!(
            json.get("event").and_then(|v| v.as_str()),
            Some("command_result")
        );
        assert_eq!(
            json.get("action").and_then(|v| v.as_str()),
            Some("schedule_controller_restart")
        );
        assert_eq!(json.get("ok").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(
            json.get("message").and_then(|v| v.as_str()),
            Some("Invalid request: max_attempts must be >= 1")
        );
    }

    #[tokio::test]
    async fn schedule_restart_rejects_empty_restart_command() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let server = IntendantServer::new(state, bus);

        let output = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: Some("   ".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;

        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(false));
        assert_eq!(json["status"].as_str(), Some("rejected"));
        assert_eq!(
            json["error"].as_str(),
            Some("Invalid request: restart_command must not be empty")
        );
    }

    #[tokio::test]
    async fn schedule_restart_rejects_when_active_with_json_payload() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let server = IntendantServer::new(state, bus);

        let first = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let first_json: serde_json::Value = serde_json::from_str(&first).unwrap();
        let restart_id = first_json["restart_id"].as_str().unwrap().to_string();

        let second = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop again".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&second).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(false));
        assert_eq!(json["status"].as_str(), Some("rejected"));
        assert_eq!(json["phase"].as_str(), Some("awaiting_turn_complete"));
        assert_eq!(json["restart_id"].as_str(), Some(restart_id.as_str()));
        assert!(json["error"]
            .as_str()
            .unwrap_or("")
            .contains("A restart is already active"));
    }

    #[tokio::test]
    async fn spawn_detached_restart_command_returns_live_pid() {
        // A long-lived command per platform: `sleep` on Unix, a long `timeout`
        // on Windows (the cmd.exe-resolvable form, /T seconds, /NOBREAK so it
        // doesn't consume our detached stdin).
        #[cfg(windows)]
        let long_running = "timeout /T 30 /NOBREAK";
        #[cfg(not(windows))]
        let long_running = "sleep 30";

        let pid = spawn_detached_restart_command(long_running)
            .await
            .expect("detached spawn should succeed");
        assert!(pid > 1);

        // Liveness via the platform helper (kill(pid,0) on Unix,
        // OpenProcess/GetExitCodeProcess on Windows) rather than shelling to
        // bash, which doesn't exist on a stock Windows host.
        assert!(
            crate::platform::process_alive(pid),
            "spawned pid should be alive"
        );

        // Best-effort cleanup of the detached child.
        #[cfg(windows)]
        {
            let _ = std::process::Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/F", "/T"])
                .status();
        }
        #[cfg(not(windows))]
        {
            let _ = std::process::Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .status();
        }
    }

    #[tokio::test]
    async fn controller_turn_complete_returns_json_success_payload() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let server = IntendantServer::new(state.clone(), bus);

        let scheduled = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let scheduled_json: serde_json::Value = serde_json::from_str(&scheduled).unwrap();
        let restart_id = scheduled_json["restart_id"].as_str().unwrap().to_string();
        let token = scheduled_json["turn_complete_token"]
            .as_str()
            .unwrap()
            .to_string();

        let output = server
            .controller_turn_complete(Parameters(ControllerTurnCompleteParams {
                restart_id: restart_id.clone(),
                turn_complete_token: token,
                status: Some("ok".to_string()),
                handoff_summary: Some("handoff".to_string()),
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(true));
        assert_eq!(json["status"].as_str(), Some("completed"));
        assert_eq!(json["restart_id"].as_str(), Some(restart_id.as_str()));
        assert_eq!(json["phase"].as_str(), Some("completed"));
        assert!(json["execution"].as_str().unwrap_or("").contains("spawned"));
    }

    #[tokio::test]
    async fn get_restart_status_redacts_turn_complete_token() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let server = IntendantServer::new(state, bus);

        let scheduled = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let scheduled_json: serde_json::Value = serde_json::from_str(&scheduled).unwrap();
        let token = scheduled_json["turn_complete_token"]
            .as_str()
            .unwrap()
            .to_string();

        let status = server.get_restart_status().await;
        let status_json: serde_json::Value = serde_json::from_str(&status).unwrap();
        assert_eq!(
            status_json["turn_complete_token"].as_str(),
            Some("[redacted]")
        );
        assert_ne!(
            status_json["turn_complete_token"].as_str(),
            Some(token.as_str())
        );
    }

    #[tokio::test]
    async fn controller_turn_complete_marks_restart_failed_when_auto_start_task_fails() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let server = IntendantServer::new(state.clone(), bus);

        let scheduled = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: None,
                auto_start_task: Some(true),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let scheduled_json: serde_json::Value = serde_json::from_str(&scheduled).unwrap();
        let restart_id = scheduled_json["restart_id"].as_str().unwrap().to_string();
        let token = scheduled_json["turn_complete_token"]
            .as_str()
            .unwrap()
            .to_string();

        {
            let mut s = state.write().await;
            s.set_phase(Phase::RunningAgent);
        }

        let output = server
            .controller_turn_complete(Parameters(ControllerTurnCompleteParams {
                restart_id: restart_id.clone(),
                turn_complete_token: token,
                status: None,
                handoff_summary: None,
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(false));
        assert_eq!(json["status"].as_str(), Some("restart_pending"));
        assert_eq!(json["phase"].as_str(), Some("failed"));
        assert!(json["error"]
            .as_str()
            .unwrap_or("")
            .contains("Failed to start follow-up task"));

        let restart_path = restart_state_path(dir.path());
        let persisted = std::fs::read_to_string(restart_path).expect("restart file should exist");
        let persisted_json: serde_json::Value = serde_json::from_str(&persisted).unwrap();
        let restart_json = persisted_json.as_object().expect("restart should persist");
        assert_eq!(
            restart_json.get("phase").and_then(|v| v.as_str()),
            Some("failed")
        );
        assert!(restart_json
            .get("last_error")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .contains("Failed to start follow-up task"));
    }

    #[tokio::test]
    async fn controller_turn_complete_returns_json_error_payload() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let server = IntendantServer::new(state, bus);

        let scheduled = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let scheduled_json: serde_json::Value = serde_json::from_str(&scheduled).unwrap();
        let restart_id = scheduled_json["restart_id"].as_str().unwrap().to_string();

        let output = server
            .controller_turn_complete(Parameters(ControllerTurnCompleteParams {
                restart_id: restart_id.clone(),
                turn_complete_token: "wrong".to_string(),
                status: None,
                handoff_summary: None,
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(false));
        assert_eq!(json["status"].as_str(), Some("rejected"));
        assert_eq!(json["restart_id"].as_str(), Some(restart_id.as_str()));
        assert_eq!(json["phase"].as_str(), Some("awaiting_turn_complete"));
        assert_eq!(
            json["error"].as_str(),
            Some("turn_complete_token is invalid")
        );
    }

    #[tokio::test]
    async fn controller_turn_complete_rejects_ready_phase() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let server = IntendantServer::new(state.clone(), bus);

        let scheduled = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let scheduled_json: serde_json::Value = serde_json::from_str(&scheduled).unwrap();
        let restart_id = scheduled_json["restart_id"].as_str().unwrap().to_string();
        let token = scheduled_json["turn_complete_token"]
            .as_str()
            .unwrap()
            .to_string();

        {
            let mut s = state.write().await;
            let restart = s
                .controller_restart
                .as_mut()
                .expect("restart should be tracked");
            restart.phase = RestartPhase::Ready;
        }

        let output = server
            .controller_turn_complete(Parameters(ControllerTurnCompleteParams {
                restart_id: restart_id.clone(),
                turn_complete_token: token,
                status: None,
                handoff_summary: None,
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(false));
        assert_eq!(json["status"].as_str(), Some("rejected"));
        assert_eq!(json["restart_id"].as_str(), Some(restart_id.as_str()));
        assert_eq!(json["phase"].as_str(), Some("ready"));
        assert!(json["error"]
            .as_str()
            .unwrap_or("")
            .contains("Restart is not awaiting completion"));
    }

    #[tokio::test]
    async fn controller_turn_complete_normalizes_ids_and_optional_fields() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let server = IntendantServer::new(state.clone(), bus);

        let scheduled = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let scheduled_json: serde_json::Value = serde_json::from_str(&scheduled).unwrap();
        let restart_id = scheduled_json["restart_id"].as_str().unwrap().to_string();
        let token = scheduled_json["turn_complete_token"]
            .as_str()
            .unwrap()
            .to_string();

        let output = server
            .controller_turn_complete(Parameters(ControllerTurnCompleteParams {
                restart_id: format!("  {}  ", restart_id),
                turn_complete_token: format!("  {}  ", token),
                status: Some("   ".to_string()),
                handoff_summary: Some("  handoff summary  ".to_string()),
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(true));

        let s = state.read().await;
        let restart = s
            .controller_restart
            .as_ref()
            .expect("restart should be stored");
        assert!(restart.completion_status.is_none());
        assert_eq!(restart.handoff_summary.as_deref(), Some("handoff summary"));
    }

    #[tokio::test]
    async fn cancel_controller_restart_returns_json_success_payload() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let server = IntendantServer::new(state, bus);

        let scheduled = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let scheduled_json: serde_json::Value = serde_json::from_str(&scheduled).unwrap();
        let restart_id = scheduled_json["restart_id"].as_str().unwrap().to_string();

        let output = server
            .cancel_controller_restart(Parameters(CancelControllerRestartParams {
                restart_id: Some(restart_id.clone()),
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(true));
        assert_eq!(json["status"].as_str(), Some("cancelled"));
        assert_eq!(json["restart_id"].as_str(), Some(restart_id.as_str()));
        assert_eq!(json["phase"].as_str(), Some("cancelled"));
    }

    #[tokio::test]
    async fn cancel_controller_restart_returns_json_error_payload() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let server = IntendantServer::new(state, bus);

        let output = server
            .cancel_controller_restart(Parameters(CancelControllerRestartParams {
                restart_id: Some("abc".to_string()),
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(false));
        assert_eq!(json["status"].as_str(), Some("rejected"));
        assert_eq!(
            json["error"].as_str(),
            Some("No controller restart is scheduled")
        );
        assert_eq!(json["restart_id"].as_str(), Some("abc"));
    }

    #[tokio::test]
    async fn cancel_controller_restart_treats_whitespace_guard_as_none() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let server = IntendantServer::new(state, bus);

        let scheduled = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let scheduled_json: serde_json::Value = serde_json::from_str(&scheduled).unwrap();
        let restart_id = scheduled_json["restart_id"].as_str().unwrap().to_string();

        let output = server
            .cancel_controller_restart(Parameters(CancelControllerRestartParams {
                restart_id: Some("   ".to_string()),
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(true));
        assert_eq!(json["restart_id"].as_str(), Some(restart_id.as_str()));
    }

    #[test]
    fn inline_schema_refs_resolves_defs() {
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": {
                "actions": {
                    "type": "array",
                    "items": { "$ref": "#/$defs/CuAction" }
                }
            },
            "$defs": {
                "CuAction": {
                    "type": "object",
                    "properties": {
                        "type": { "type": "string" },
                        "x": { "type": "integer" }
                    }
                }
            }
        });
        inline_schema_refs(&mut schema);
        // $defs should be removed
        assert!(schema.get("$defs").is_none());
        // $ref should be replaced with the actual definition
        let items = &schema["properties"]["actions"]["items"];
        assert_eq!(items["type"], "object");
        assert!(items["properties"]["x"]["type"] == "integer");
        assert!(items.get("$ref").is_none());
    }

    #[test]
    fn inline_schema_refs_nested() {
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": {
                "field": { "$ref": "#/$defs/Outer" }
            },
            "$defs": {
                "Outer": {
                    "type": "object",
                    "properties": {
                        "inner": { "$ref": "#/$defs/Inner" }
                    }
                },
                "Inner": {
                    "type": "string",
                    "maxLength": 100
                }
            }
        });
        inline_schema_refs(&mut schema);
        let inner = &schema["properties"]["field"]["properties"]["inner"];
        assert_eq!(inner["type"], "string");
        assert_eq!(inner["maxLength"], 100);
        assert!(inner.get("$ref").is_none());
    }

    #[test]
    fn inline_schema_refs_noop_without_defs() {
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" }
            }
        });
        let original = schema.clone();
        inline_schema_refs(&mut schema);
        assert_eq!(schema, original);
    }
}
