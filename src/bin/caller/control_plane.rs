//! Centralized control plane for shared state updates.
//!
//! Subscribes to the EventBus and handles ControlMsg events that update
//! shared state (autonomy level, external agent backend, etc.). This ensures
//! state is updated regardless of which frontend (TUI, web, MCP) is active.
//! Frontends remain display-only — they render state changes but never write
//! to shared state from ControlMsg handlers.

use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::autonomy::SharedAutonomy;
use crate::event::{AppEvent, ControlMsg, EventBus};
use crate::external_agent;

/// Runtime Codex configuration shared between the daemon loop and the
/// control plane. The daemon loop re-reads this at the start of every task;
/// the control plane writes here (and to `intendant.toml`) when a frontend
/// dispatches `SetCodex*` messages. Changes to any field take effect on the
/// NEXT task — an existing Codex thread keeps these values for the rest of
/// its life because Codex locks sandbox / approval / model / tool config at
/// `thread/start`.
#[derive(Debug, Clone)]
pub struct CodexRuntimeConfig {
    pub sandbox: String,
    pub approval_policy: String,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub web_search: bool,
    pub network_access: bool,
    pub writable_roots: Vec<String>,
}

pub type SharedCodexConfig = Arc<RwLock<CodexRuntimeConfig>>;

/// Runtime Gemini configuration, mirror of `CodexRuntimeConfig`. All of
/// these map to Gemini CLI flags and are applied when the agent process is
/// spawned — there's no mid-session way to flip them, so a change here
/// forces the daemon loop to tear down the persistent agent.
#[derive(Debug, Clone)]
pub struct GeminiRuntimeConfig {
    pub model: Option<String>,
    pub approval_mode: String,
    pub sandbox: bool,
    pub extensions: Vec<String>,
    pub allowed_mcp_servers: Vec<String>,
    pub include_directories: Vec<String>,
    pub debug: bool,
}

pub type SharedGeminiConfig = Arc<RwLock<GeminiRuntimeConfig>>;

pub struct ControlPlaneState {
    pub autonomy: SharedAutonomy,
    pub external_agent: Arc<RwLock<Option<external_agent::AgentBackend>>>,
    pub codex_config: SharedCodexConfig,
    pub gemini_config: SharedGeminiConfig,
    pub bus: EventBus,
    /// Project root for `intendant.toml` writes. When set, changes to
    /// `external_agent` (from any frontend) also persist to the config
    /// file so the setting survives daemon restarts. `None` in tests
    /// or when no project context is available.
    pub project_root: Option<PathBuf>,
}

/// Spawn the control plane as a background task. Returns a JoinHandle.
pub fn spawn(
    mut event_rx: tokio::sync::broadcast::Receiver<AppEvent>,
    state: ControlPlaneState,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match event_rx.recv().await {
                Ok(AppEvent::ControlCommand(msg)) => {
                    handle_control_msg(&msg, &state).await;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                _ => {} // Other events, lagged -- ignore
            }
        }
    })
}

async fn handle_control_msg(msg: &ControlMsg, state: &ControlPlaneState) {
    match msg {
        ControlMsg::SetAutonomy { level } => {
            use crate::autonomy::AutonomyLevel;
            let new_level = AutonomyLevel::from_str_loose(level);
            let mut guard = state.autonomy.write().await;
            guard.level = new_level;
        }
        ControlMsg::SetExternalAgent { agent } => {
            let parsed = agent
                .as_deref()
                .filter(|s| !s.is_empty())
                .and_then(external_agent::AgentBackend::from_str_loose);
            *state.external_agent.write().await = parsed.clone();
            // Persist to intendant.toml so the setting survives daemon
            // restarts. Any frontend (dashboard, TUI, MCP) that sends
            // this control message gets persistence for free. Always
            // write the canonical SHORT form ("codex" | "claude-code" |
            // "gemini") — the TOML round-trip must preserve identity,
            // and from_str_loose needs a form it'll parse back. The
            // Display form ("Gemini CLI") used to slip through here,
            // which broke the next daemon startup because from_str_loose
            // didn't match the spaced lowercase variant.
            if let Some(ref root) = state.project_root {
                let canonical = parsed.as_ref().map(|b| b.as_short_str().to_string());
                if let Err(e) = persist_external_agent(root, canonical.as_deref()) {
                    eprintln!(
                        "[control_plane] failed to persist external_agent to intendant.toml: {e}"
                    );
                }
            }
            // Broadcast so frontends can update their status bars. The
            // Display form is intentional here — the dashboard uses it
            // as human-readable badge text.
            state.bus.send(AppEvent::ExternalAgentChanged {
                agent: parsed.map(|b| b.to_string()),
            });
        }
        ControlMsg::SetCodexSandbox { mode } => {
            let normalized = crate::project::normalize_sandbox_mode(mode);
            {
                let mut guard = state.codex_config.write().await;
                guard.sandbox = normalized.clone();
            }
            if let Some(ref root) = state.project_root {
                if let Err(e) = persist_codex_field(root, |cfg| {
                    cfg.sandbox = normalized.clone();
                }) {
                    eprintln!(
                        "[control_plane] failed to persist codex.sandbox to intendant.toml: {e}"
                    );
                }
            }
            state.bus.send(codex_config_changed_event(
                CodexConfigDelta { sandbox: Some(normalized), ..Default::default() },
            ));
        }
        ControlMsg::SetCodexApprovalPolicy { policy } => {
            let normalized = crate::project::normalize_approval_policy(policy);
            {
                let mut guard = state.codex_config.write().await;
                guard.approval_policy = normalized.clone();
            }
            if let Some(ref root) = state.project_root {
                if let Err(e) = persist_codex_field(root, |cfg| {
                    cfg.approval_policy = normalized.clone();
                }) {
                    eprintln!(
                        "[control_plane] failed to persist codex.approval_policy to intendant.toml: {e}"
                    );
                }
            }
            state.bus.send(codex_config_changed_event(
                CodexConfigDelta { approval_policy: Some(normalized), ..Default::default() },
            ));
        }
        ControlMsg::SetCodexModel { model } => {
            // Treat empty/whitespace string as "clear the override" — matches
            // the dashboard input semantics where an empty text field means
            // "let Codex pick its default".
            let normalized: Option<String> = model
                .as_ref()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            {
                let mut guard = state.codex_config.write().await;
                guard.model = normalized.clone();
            }
            if let Some(ref root) = state.project_root {
                if let Err(e) = persist_codex_field(root, |cfg| {
                    cfg.model = normalized.clone();
                }) {
                    eprintln!(
                        "[control_plane] failed to persist codex.model to intendant.toml: {e}"
                    );
                }
            }
            let cleared = normalized.is_none();
            state.bus.send(codex_config_changed_event(CodexConfigDelta {
                model: normalized,
                model_cleared: cleared,
                ..Default::default()
            }));
        }
        ControlMsg::SetCodexReasoningEffort { effort } => {
            let normalized = crate::project::normalize_reasoning_effort(effort.as_deref());
            {
                let mut guard = state.codex_config.write().await;
                guard.reasoning_effort = normalized.clone();
            }
            if let Some(ref root) = state.project_root {
                if let Err(e) = persist_codex_field(root, |cfg| {
                    cfg.reasoning_effort = normalized.clone();
                }) {
                    eprintln!(
                        "[control_plane] failed to persist codex.reasoning_effort to intendant.toml: {e}"
                    );
                }
            }
            let cleared = normalized.is_none();
            state.bus.send(codex_config_changed_event(CodexConfigDelta {
                reasoning_effort: normalized,
                reasoning_effort_cleared: cleared,
                ..Default::default()
            }));
        }
        ControlMsg::SetCodexWebSearch { enabled } => {
            {
                let mut guard = state.codex_config.write().await;
                guard.web_search = *enabled;
            }
            if let Some(ref root) = state.project_root {
                if let Err(e) = persist_codex_field(root, |cfg| {
                    cfg.web_search = *enabled;
                }) {
                    eprintln!(
                        "[control_plane] failed to persist codex.web_search to intendant.toml: {e}"
                    );
                }
            }
            state.bus.send(codex_config_changed_event(CodexConfigDelta {
                web_search: Some(*enabled),
                ..Default::default()
            }));
        }
        ControlMsg::SetCodexNetworkAccess { enabled } => {
            {
                let mut guard = state.codex_config.write().await;
                guard.network_access = *enabled;
            }
            if let Some(ref root) = state.project_root {
                if let Err(e) = persist_codex_field(root, |cfg| {
                    cfg.network_access = *enabled;
                }) {
                    eprintln!(
                        "[control_plane] failed to persist codex.network_access to intendant.toml: {e}"
                    );
                }
            }
            state.bus.send(codex_config_changed_event(CodexConfigDelta {
                network_access: Some(*enabled),
                ..Default::default()
            }));
        }
        ControlMsg::CodexThreadAction { op, params } => {
            // Republish as an AppEvent so the daemon-side watcher (which
            // owns the persistent Codex agent) can pick it up and run the
            // RPC. We don't own the agent here, so we only translate.
            state.bus.send(AppEvent::CodexThreadActionRequested {
                action: op.clone(),
                params: params.clone(),
            });
        }
        ControlMsg::SetCodexWritableRoots { roots } => {
            let normalized = normalize_writable_roots(roots);
            {
                let mut guard = state.codex_config.write().await;
                guard.writable_roots = normalized.clone();
            }
            if let Some(ref root) = state.project_root {
                if let Err(e) = persist_codex_field(root, |cfg| {
                    cfg.writable_roots = normalized.clone();
                }) {
                    eprintln!(
                        "[control_plane] failed to persist codex.writable_roots to intendant.toml: {e}"
                    );
                }
            }
            state.bus.send(codex_config_changed_event(CodexConfigDelta {
                writable_roots: Some(normalized),
                ..Default::default()
            }));
        }
        ControlMsg::SetGeminiModel { model } => {
            // Treat empty/whitespace as "clear the override", matching the
            // dashboard input semantics for the Codex-model field.
            let normalized: Option<String> = model
                .as_ref()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            {
                let mut guard = state.gemini_config.write().await;
                guard.model = normalized.clone();
            }
            if let Some(ref root) = state.project_root {
                if let Err(e) = persist_gemini_field(root, |cfg| {
                    cfg.model = normalized.clone();
                }) {
                    eprintln!(
                        "[control_plane] failed to persist gemini.model to intendant.toml: {e}"
                    );
                }
            }
            state.bus.send(gemini_config_changed_event(GeminiConfigDelta {
                model: normalized.clone(),
                model_cleared: normalized.is_none(),
                ..Default::default()
            }));
        }
        ControlMsg::SetGeminiApprovalMode { mode } => {
            let normalized = crate::project::normalize_gemini_approval_mode(mode);
            {
                let mut guard = state.gemini_config.write().await;
                guard.approval_mode = normalized.clone();
            }
            if let Some(ref root) = state.project_root {
                if let Err(e) = persist_gemini_field(root, |cfg| {
                    cfg.approval_mode = normalized.clone();
                }) {
                    eprintln!(
                        "[control_plane] failed to persist gemini.approval_mode to intendant.toml: {e}"
                    );
                }
            }
            state.bus.send(gemini_config_changed_event(GeminiConfigDelta {
                approval_mode: Some(normalized),
                ..Default::default()
            }));
        }
        ControlMsg::SetGeminiSandbox { enabled } => {
            {
                let mut guard = state.gemini_config.write().await;
                guard.sandbox = *enabled;
            }
            if let Some(ref root) = state.project_root {
                if let Err(e) = persist_gemini_field(root, |cfg| {
                    cfg.sandbox = *enabled;
                }) {
                    eprintln!(
                        "[control_plane] failed to persist gemini.sandbox to intendant.toml: {e}"
                    );
                }
            }
            state.bus.send(gemini_config_changed_event(GeminiConfigDelta {
                sandbox: Some(*enabled),
                ..Default::default()
            }));
        }
        ControlMsg::SetGeminiExtensions { extensions } => {
            let normalized = normalize_name_list(extensions);
            {
                let mut guard = state.gemini_config.write().await;
                guard.extensions = normalized.clone();
            }
            if let Some(ref root) = state.project_root {
                if let Err(e) = persist_gemini_field(root, |cfg| {
                    cfg.extensions = normalized.clone();
                }) {
                    eprintln!(
                        "[control_plane] failed to persist gemini.extensions to intendant.toml: {e}"
                    );
                }
            }
            state.bus.send(gemini_config_changed_event(GeminiConfigDelta {
                extensions: Some(normalized),
                ..Default::default()
            }));
        }
        ControlMsg::SetGeminiAllowedMcpServers { servers } => {
            let normalized = normalize_name_list(servers);
            {
                let mut guard = state.gemini_config.write().await;
                guard.allowed_mcp_servers = normalized.clone();
            }
            if let Some(ref root) = state.project_root {
                if let Err(e) = persist_gemini_field(root, |cfg| {
                    cfg.allowed_mcp_servers = normalized.clone();
                }) {
                    eprintln!(
                        "[control_plane] failed to persist gemini.allowed_mcp_servers to intendant.toml: {e}"
                    );
                }
            }
            state.bus.send(gemini_config_changed_event(GeminiConfigDelta {
                allowed_mcp_servers: Some(normalized),
                ..Default::default()
            }));
        }
        ControlMsg::SetGeminiIncludeDirectories { directories } => {
            // Reuse writable-roots normalizer — same "drop empty / dedupe,
            // preserve order" policy applies.
            let normalized = normalize_writable_roots(directories);
            {
                let mut guard = state.gemini_config.write().await;
                guard.include_directories = normalized.clone();
            }
            if let Some(ref root) = state.project_root {
                if let Err(e) = persist_gemini_field(root, |cfg| {
                    cfg.include_directories = normalized.clone();
                }) {
                    eprintln!(
                        "[control_plane] failed to persist gemini.include_directories to intendant.toml: {e}"
                    );
                }
            }
            state.bus.send(gemini_config_changed_event(GeminiConfigDelta {
                include_directories: Some(normalized),
                ..Default::default()
            }));
        }
        ControlMsg::SetGeminiDebug { enabled } => {
            {
                let mut guard = state.gemini_config.write().await;
                guard.debug = *enabled;
            }
            if let Some(ref root) = state.project_root {
                if let Err(e) = persist_gemini_field(root, |cfg| {
                    cfg.debug = *enabled;
                }) {
                    eprintln!(
                        "[control_plane] failed to persist gemini.debug to intendant.toml: {e}"
                    );
                }
            }
            state.bus.send(gemini_config_changed_event(GeminiConfigDelta {
                debug: Some(*enabled),
                ..Default::default()
            }));
        }
        ControlMsg::GeminiThreadAction { op, params } => {
            // Rebroadcast for the daemon-side watcher, same pattern as
            // CodexThreadAction. The watcher dispatches `/new` (agent
            // teardown) or returns an "unsupported" result.
            state.bus.send(AppEvent::GeminiThreadActionRequested {
                action: op.clone(),
                params: params.clone(),
            });
        }
        ControlMsg::ResumeSession { .. } => {
            // Routed by the daemon loop; there is no persistent config state
            // to update here.
        }
        ControlMsg::GrantUserDisplay { display_id } => {
            // Moved out of `tui/app.rs::handle_control_command` — the TUI is
            // now display-only and the display-control path shouldn't depend
            // on a rendering loop running to process revokes/grants. Before
            // this, a grant/revoke dispatched to a web-only daemon had to
            // wait behind `tui::web::WebTui`'s render cadence (one full
            // redraw per event loop iteration, rendered to every attached
            // web terminal connection), which is what surfaced as the
            // asymmetric 60-second lag on revoke that dashboard toggles
            // experienced. Grant was hitting the same code path but usually
            // appeared instant because the first grant typically arrives
            // before any web terminal connects; subsequent grants after
            // churn would have shown the same lag.
            let did = display_id.unwrap_or(0);
            {
                let mut guard = state.autonomy.write().await;
                guard.user_display_granted = true;
            }
            // Keep the env var in sync so subprocesses that inspect it
            // (agent runners, etc.) observe the same state the autonomy
            // guard reports. Matches the tui/mcp paths that set it.
            std::env::set_var("INTENDANT_USER_DISPLAY_GRANTED", "1");
            state.bus.send(AppEvent::UserDisplayGranted { display_id: did });
        }
        ControlMsg::RevokeUserDisplay { display_id, note } => {
            let did = display_id.unwrap_or(0);
            {
                let mut guard = state.autonomy.write().await;
                guard.user_display_granted = false;
            }
            std::env::remove_var("INTENDANT_USER_DISPLAY_GRANTED");
            state.bus.send(AppEvent::UserDisplayRevoked {
                display_id: did,
                note: note.clone(),
            });
        }
        _ => {} // Other control messages don't update shared state
    }
}

/// Normalize a list of names (extension IDs, MCP server names, etc.): trim
/// whitespace, drop empty entries, dedupe while preserving order.
fn normalize_name_list(raw: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(raw.len());
    for entry in raw {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            continue;
        }
        let s = trimmed.to_string();
        if !out.iter().any(|existing| existing == &s) {
            out.push(s);
        }
    }
    out
}

/// Drop blank entries and duplicates (case-preserving but order-preserving)
/// so the persisted TOML + the broadcast event both reflect a clean list.
fn normalize_writable_roots(raw: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(raw.len());
    for entry in raw {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            continue;
        }
        let s = trimmed.to_string();
        if !out.iter().any(|existing| existing == &s) {
            out.push(s);
        }
    }
    out
}

/// Delta describing which Codex config fields changed. Everything defaults
/// to "unchanged" so callers can populate only the field they touched.
#[derive(Debug, Default)]
struct CodexConfigDelta {
    sandbox: Option<String>,
    approval_policy: Option<String>,
    model: Option<String>,
    model_cleared: bool,
    reasoning_effort: Option<String>,
    reasoning_effort_cleared: bool,
    web_search: Option<bool>,
    network_access: Option<bool>,
    writable_roots: Option<Vec<String>>,
}

fn codex_config_changed_event(delta: CodexConfigDelta) -> AppEvent {
    AppEvent::CodexConfigChanged {
        sandbox: delta.sandbox,
        approval_policy: delta.approval_policy,
        model: delta.model,
        model_cleared: delta.model_cleared,
        reasoning_effort: delta.reasoning_effort,
        reasoning_effort_cleared: delta.reasoning_effort_cleared,
        web_search: delta.web_search,
        network_access: delta.network_access,
        writable_roots: delta.writable_roots,
    }
}

/// Re-read `intendant.toml`, apply a closure to the `[agent.codex]` section,
/// and save. Re-reading (rather than mutating a cached config) is the
/// simplest way to avoid stepping on concurrent writes from other parts of
/// the daemon. Mirrors `persist_external_agent` below.
fn persist_codex_field<F>(
    project_root: &std::path::Path,
    mutate: F,
) -> Result<(), crate::error::CallerError>
where
    F: FnOnce(&mut crate::project::CodexConfig),
{
    let mut proj = crate::project::Project::from_root(project_root.to_path_buf())?;
    mutate(&mut proj.config.agent.codex);
    proj.save_config()
}

/// Sibling of `persist_codex_field` for the `[agent.gemini_cli]` section.
fn persist_gemini_field<F>(
    project_root: &std::path::Path,
    mutate: F,
) -> Result<(), crate::error::CallerError>
where
    F: FnOnce(&mut crate::project::GeminiCliConfig),
{
    let mut proj = crate::project::Project::from_root(project_root.to_path_buf())?;
    mutate(&mut proj.config.agent.gemini_cli);
    proj.save_config()
}

/// Delta describing which Gemini config fields changed. Mirrors
/// `CodexConfigDelta`; `Option::None` across the board means "no change".
#[derive(Debug, Default)]
struct GeminiConfigDelta {
    model: Option<String>,
    model_cleared: bool,
    approval_mode: Option<String>,
    sandbox: Option<bool>,
    extensions: Option<Vec<String>>,
    allowed_mcp_servers: Option<Vec<String>>,
    include_directories: Option<Vec<String>>,
    debug: Option<bool>,
}

fn gemini_config_changed_event(delta: GeminiConfigDelta) -> AppEvent {
    AppEvent::GeminiConfigChanged {
        model: delta.model,
        model_cleared: delta.model_cleared,
        approval_mode: delta.approval_mode,
        sandbox: delta.sandbox,
        extensions: delta.extensions,
        allowed_mcp_servers: delta.allowed_mcp_servers,
        include_directories: delta.include_directories,
        debug: delta.debug,
    }
}

/// Re-read intendant.toml, update `[agent] default_backend`, and save
/// it back. Re-reading (instead of caching a mutable ProjectConfig) is
/// the simplest way to avoid races with other writers to the TOML.
fn persist_external_agent(
    project_root: &std::path::Path,
    backend: Option<&str>,
) -> Result<(), crate::error::CallerError> {
    let mut proj = crate::project::Project::from_root(project_root.to_path_buf())?;
    proj.config.agent.default_backend = backend.map(|s| s.to_string());
    proj.save_config()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autonomy::{AutonomyLevel, AutonomyState};
    use crate::event::EventBus;

    fn test_codex_config() -> SharedCodexConfig {
        Arc::new(RwLock::new(CodexRuntimeConfig {
            sandbox: "workspace-write".to_string(),
            approval_policy: "on-request".to_string(),
            model: None,
            reasoning_effort: None,
            web_search: false,
            network_access: false,
            writable_roots: Vec::new(),
        }))
    }

    fn test_gemini_config() -> SharedGeminiConfig {
        Arc::new(RwLock::new(GeminiRuntimeConfig {
            model: None,
            approval_mode: "default".to_string(),
            sandbox: false,
            extensions: Vec::new(),
            allowed_mcp_servers: Vec::new(),
            include_directories: Vec::new(),
            debug: false,
        }))
    }

    #[tokio::test]
    async fn set_autonomy_updates_shared_state() {
        let bus = EventBus::new();
        let autonomy = crate::autonomy::shared_autonomy(AutonomyState::default());
        let external_agent = Arc::new(RwLock::new(None));

        let handle = spawn(
            bus.subscribe(),
            ControlPlaneState {
                autonomy: autonomy.clone(),
                external_agent: external_agent.clone(),
                codex_config: test_codex_config(),
                gemini_config: test_gemini_config(),
                bus: bus.clone(),
                project_root: None,
            },
        );

        // Verify initial state
        assert_eq!(autonomy.read().await.level, AutonomyLevel::Medium);

        // Send SetAutonomy
        bus.send(AppEvent::ControlCommand(ControlMsg::SetAutonomy {
            level: "high".to_string(),
        }));

        // Give the spawned task time to process
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(autonomy.read().await.level, AutonomyLevel::High);

        handle.abort();
    }

    #[tokio::test]
    async fn set_external_agent_updates_shared_state() {
        let bus = EventBus::new();
        let autonomy = crate::autonomy::shared_autonomy(AutonomyState::default());
        let external_agent = Arc::new(RwLock::new(None));

        let handle = spawn(
            bus.subscribe(),
            ControlPlaneState {
                autonomy: autonomy.clone(),
                external_agent: external_agent.clone(),
                codex_config: test_codex_config(),
                gemini_config: test_gemini_config(),
                bus: bus.clone(),
                project_root: None,
            },
        );

        // Verify initial state
        assert!(external_agent.read().await.is_none());

        // Send SetExternalAgent with a value
        bus.send(AppEvent::ControlCommand(ControlMsg::SetExternalAgent {
            agent: Some("codex".to_string()),
        }));

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(
            *external_agent.read().await,
            Some(external_agent::AgentBackend::Codex)
        );

        // Send SetExternalAgent with None to clear
        bus.send(AppEvent::ControlCommand(ControlMsg::SetExternalAgent {
            agent: None,
        }));

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert!(external_agent.read().await.is_none());

        handle.abort();
    }

    #[tokio::test]
    async fn set_autonomy_invalid_level_ignored() {
        let bus = EventBus::new();
        let autonomy = crate::autonomy::shared_autonomy(AutonomyState::default());
        let external_agent = Arc::new(RwLock::new(None));

        let handle = spawn(
            bus.subscribe(),
            ControlPlaneState {
                autonomy: autonomy.clone(),
                external_agent: external_agent.clone(),
                codex_config: test_codex_config(),
                gemini_config: test_gemini_config(),
                bus: bus.clone(),
                project_root: None,
            },
        );

        // AutonomyLevel::from_str_loose returns Medium for unknown strings
        bus.send(AppEvent::ControlCommand(ControlMsg::SetAutonomy {
            level: "unknown_level".to_string(),
        }));

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // from_str_loose defaults to Medium for unknown strings
        assert_eq!(autonomy.read().await.level, AutonomyLevel::Medium);

        handle.abort();
    }

    #[tokio::test]
    async fn set_external_agent_empty_string_clears() {
        let bus = EventBus::new();
        let autonomy = crate::autonomy::shared_autonomy(AutonomyState::default());
        let external_agent = Arc::new(RwLock::new(Some(external_agent::AgentBackend::Codex)));

        let handle = spawn(
            bus.subscribe(),
            ControlPlaneState {
                autonomy: autonomy.clone(),
                external_agent: external_agent.clone(),
                codex_config: test_codex_config(),
                gemini_config: test_gemini_config(),
                bus: bus.clone(),
                project_root: None,
            },
        );

        // Send SetExternalAgent with empty string -- should clear
        bus.send(AppEvent::ControlCommand(ControlMsg::SetExternalAgent {
            agent: Some(String::new()),
        }));

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert!(external_agent.read().await.is_none());

        handle.abort();
    }

    #[tokio::test]
    async fn set_codex_sandbox_updates_shared_state() {
        let bus = EventBus::new();
        let autonomy = crate::autonomy::shared_autonomy(AutonomyState::default());
        let external_agent = Arc::new(RwLock::new(None));
        let codex_config = test_codex_config();

        let handle = spawn(
            bus.subscribe(),
            ControlPlaneState {
                autonomy,
                external_agent,
                codex_config: codex_config.clone(),
                gemini_config: test_gemini_config(),
                bus: bus.clone(),
                project_root: None,
            },
        );

        assert_eq!(codex_config.read().await.sandbox, "workspace-write");

        bus.send(AppEvent::ControlCommand(ControlMsg::SetCodexSandbox {
            mode: "danger-full-access".to_string(),
        }));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(codex_config.read().await.sandbox, "danger-full-access");

        // Unknown value → normalized back to workspace-write (safe fallback).
        bus.send(AppEvent::ControlCommand(ControlMsg::SetCodexSandbox {
            mode: "banana".to_string(),
        }));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(codex_config.read().await.sandbox, "workspace-write");

        handle.abort();
    }

    #[tokio::test]
    async fn set_codex_approval_policy_updates_shared_state() {
        let bus = EventBus::new();
        let autonomy = crate::autonomy::shared_autonomy(AutonomyState::default());
        let external_agent = Arc::new(RwLock::new(None));
        let codex_config = test_codex_config();

        let handle = spawn(
            bus.subscribe(),
            ControlPlaneState {
                autonomy,
                external_agent,
                codex_config: codex_config.clone(),
                gemini_config: test_gemini_config(),
                bus: bus.clone(),
                project_root: None,
            },
        );

        bus.send(AppEvent::ControlCommand(ControlMsg::SetCodexApprovalPolicy {
            policy: "never".to_string(),
        }));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(codex_config.read().await.approval_policy, "never");

        handle.abort();
    }

    #[tokio::test]
    async fn set_codex_model_empty_string_clears() {
        let bus = EventBus::new();
        let autonomy = crate::autonomy::shared_autonomy(AutonomyState::default());
        let external_agent = Arc::new(RwLock::new(None));
        let codex_config = test_codex_config();

        let handle = spawn(
            bus.subscribe(),
            ControlPlaneState {
                autonomy,
                external_agent,
                codex_config: codex_config.clone(),
                gemini_config: test_gemini_config(),
                bus: bus.clone(),
                project_root: None,
            },
        );

        bus.send(AppEvent::ControlCommand(ControlMsg::SetCodexModel {
            model: Some("gpt-5".to_string()),
        }));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(codex_config.read().await.model.as_deref(), Some("gpt-5"));

        // Empty string / whitespace → clear the override.
        bus.send(AppEvent::ControlCommand(ControlMsg::SetCodexModel {
            model: Some("   ".to_string()),
        }));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(codex_config.read().await.model, None);

        handle.abort();
    }

    #[tokio::test]
    async fn set_codex_reasoning_effort_normalizes_and_clears() {
        let bus = EventBus::new();
        let autonomy = crate::autonomy::shared_autonomy(AutonomyState::default());
        let external_agent = Arc::new(RwLock::new(None));
        let codex_config = test_codex_config();

        let handle = spawn(
            bus.subscribe(),
            ControlPlaneState {
                autonomy,
                external_agent,
                codex_config: codex_config.clone(),
                gemini_config: test_gemini_config(),
                bus: bus.clone(),
                project_root: None,
            },
        );

        bus.send(AppEvent::ControlCommand(ControlMsg::SetCodexReasoningEffort {
            effort: Some("high".to_string()),
        }));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(codex_config.read().await.reasoning_effort.as_deref(), Some("high"));

        // Unknown value → cleared (normalized to None, don't silently pass garbage to Codex).
        bus.send(AppEvent::ControlCommand(ControlMsg::SetCodexReasoningEffort {
            effort: Some("ultra-galaxy".to_string()),
        }));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(codex_config.read().await.reasoning_effort, None);

        handle.abort();
    }

    #[tokio::test]
    async fn set_codex_web_search_and_network_access_toggle() {
        let bus = EventBus::new();
        let autonomy = crate::autonomy::shared_autonomy(AutonomyState::default());
        let external_agent = Arc::new(RwLock::new(None));
        let codex_config = test_codex_config();

        let handle = spawn(
            bus.subscribe(),
            ControlPlaneState {
                autonomy,
                external_agent,
                codex_config: codex_config.clone(),
                gemini_config: test_gemini_config(),
                bus: bus.clone(),
                project_root: None,
            },
        );

        bus.send(AppEvent::ControlCommand(ControlMsg::SetCodexWebSearch { enabled: true }));
        bus.send(AppEvent::ControlCommand(ControlMsg::SetCodexNetworkAccess { enabled: true }));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let g = codex_config.read().await;
        assert!(g.web_search);
        assert!(g.network_access);
        drop(g);

        bus.send(AppEvent::ControlCommand(ControlMsg::SetCodexWebSearch { enabled: false }));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!codex_config.read().await.web_search);

        handle.abort();
    }

    #[tokio::test]
    async fn codex_thread_action_rebroadcasts_as_requested_event() {
        let bus = EventBus::new();
        let autonomy = crate::autonomy::shared_autonomy(AutonomyState::default());
        let external_agent = Arc::new(RwLock::new(None));
        let codex_config = test_codex_config();

        // Subscribe BEFORE spawning so we don't miss the broadcast.
        let mut rx = bus.subscribe();

        let handle = spawn(
            bus.subscribe(),
            ControlPlaneState {
                autonomy,
                external_agent,
                codex_config,
                gemini_config: test_gemini_config(),
                bus: bus.clone(),
                project_root: None,
            },
        );

        bus.send(AppEvent::ControlCommand(ControlMsg::CodexThreadAction {
            op: "compact".to_string(),
            params: serde_json::json!({"extra": "data"}),
        }));

        // Drain up to a handful of events looking for the broadcast.
        let mut found = false;
        for _ in 0..20 {
            match tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await {
                Ok(Ok(AppEvent::CodexThreadActionRequested { action, params })) => {
                    assert_eq!(action, "compact");
                    assert_eq!(params["extra"], "data");
                    found = true;
                    break;
                }
                Ok(Ok(_)) => continue,
                _ => break,
            }
        }
        assert!(found, "expected CodexThreadActionRequested on bus");

        handle.abort();
    }

    #[tokio::test]
    async fn set_codex_writable_roots_normalizes_blank_and_dupes() {
        let bus = EventBus::new();
        let autonomy = crate::autonomy::shared_autonomy(AutonomyState::default());
        let external_agent = Arc::new(RwLock::new(None));
        let codex_config = test_codex_config();

        let handle = spawn(
            bus.subscribe(),
            ControlPlaneState {
                autonomy,
                external_agent,
                codex_config: codex_config.clone(),
                gemini_config: test_gemini_config(),
                bus: bus.clone(),
                project_root: None,
            },
        );

        bus.send(AppEvent::ControlCommand(ControlMsg::SetCodexWritableRoots {
            roots: vec![
                "/tmp/a".into(),
                "  ".into(),
                "/tmp/a".into(),
                "/tmp/b".into(),
                "".into(),
            ],
        }));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let got = codex_config.read().await.writable_roots.clone();
        assert_eq!(got, vec!["/tmp/a".to_string(), "/tmp/b".to_string()]);

        handle.abort();
    }

    // ── Gemini control-plane handlers ─────────────────────────────────

    #[tokio::test]
    async fn set_gemini_model_updates_shared_state_and_clears_on_blank() {
        let bus = EventBus::new();
        let autonomy = crate::autonomy::shared_autonomy(AutonomyState::default());
        let external_agent = Arc::new(RwLock::new(None));
        let gemini_config = test_gemini_config();

        let handle = spawn(
            bus.subscribe(),
            ControlPlaneState {
                autonomy,
                external_agent,
                codex_config: test_codex_config(),
                gemini_config: gemini_config.clone(),
                bus: bus.clone(),
                project_root: None,
            },
        );

        bus.send(AppEvent::ControlCommand(ControlMsg::SetGeminiModel {
            model: Some("gemini-2.5-pro".to_string()),
        }));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            gemini_config.read().await.model.as_deref(),
            Some("gemini-2.5-pro")
        );

        // Whitespace → clear override.
        bus.send(AppEvent::ControlCommand(ControlMsg::SetGeminiModel {
            model: Some("   ".to_string()),
        }));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(gemini_config.read().await.model.is_none());

        handle.abort();
    }

    #[tokio::test]
    async fn set_gemini_approval_mode_normalizes_unknown_back_to_default() {
        let bus = EventBus::new();
        let gemini_config = test_gemini_config();

        let handle = spawn(
            bus.subscribe(),
            ControlPlaneState {
                autonomy: crate::autonomy::shared_autonomy(AutonomyState::default()),
                external_agent: Arc::new(RwLock::new(None)),
                codex_config: test_codex_config(),
                gemini_config: gemini_config.clone(),
                bus: bus.clone(),
                project_root: None,
            },
        );

        bus.send(AppEvent::ControlCommand(ControlMsg::SetGeminiApprovalMode {
            mode: "yolo".to_string(),
        }));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(gemini_config.read().await.approval_mode, "yolo");

        // Unknown → default (safe fallback — don't silently leave yolo).
        bus.send(AppEvent::ControlCommand(ControlMsg::SetGeminiApprovalMode {
            mode: "banana".to_string(),
        }));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(gemini_config.read().await.approval_mode, "default");

        handle.abort();
    }

    #[tokio::test]
    async fn set_gemini_sandbox_toggles() {
        let bus = EventBus::new();
        let gemini_config = test_gemini_config();

        let handle = spawn(
            bus.subscribe(),
            ControlPlaneState {
                autonomy: crate::autonomy::shared_autonomy(AutonomyState::default()),
                external_agent: Arc::new(RwLock::new(None)),
                codex_config: test_codex_config(),
                gemini_config: gemini_config.clone(),
                bus: bus.clone(),
                project_root: None,
            },
        );
        bus.send(AppEvent::ControlCommand(ControlMsg::SetGeminiSandbox {
            enabled: true,
        }));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(gemini_config.read().await.sandbox);
        bus.send(AppEvent::ControlCommand(ControlMsg::SetGeminiSandbox {
            enabled: false,
        }));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!gemini_config.read().await.sandbox);
        handle.abort();
    }

    #[tokio::test]
    async fn set_gemini_extensions_dedupes_and_drops_blanks() {
        let bus = EventBus::new();
        let gemini_config = test_gemini_config();

        let handle = spawn(
            bus.subscribe(),
            ControlPlaneState {
                autonomy: crate::autonomy::shared_autonomy(AutonomyState::default()),
                external_agent: Arc::new(RwLock::new(None)),
                codex_config: test_codex_config(),
                gemini_config: gemini_config.clone(),
                bus: bus.clone(),
                project_root: None,
            },
        );

        bus.send(AppEvent::ControlCommand(ControlMsg::SetGeminiExtensions {
            extensions: vec![
                "web".into(),
                "  ".into(),
                "web".into(),
                "fs".into(),
                "".into(),
            ],
        }));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            gemini_config.read().await.extensions,
            vec!["web".to_string(), "fs".to_string()]
        );
        handle.abort();
    }

    #[tokio::test]
    async fn gemini_thread_action_rebroadcasts_as_requested_event() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();

        let handle = spawn(
            bus.subscribe(),
            ControlPlaneState {
                autonomy: crate::autonomy::shared_autonomy(AutonomyState::default()),
                external_agent: Arc::new(RwLock::new(None)),
                codex_config: test_codex_config(),
                gemini_config: test_gemini_config(),
                bus: bus.clone(),
                project_root: None,
            },
        );

        bus.send(AppEvent::ControlCommand(ControlMsg::GeminiThreadAction {
            op: "new".to_string(),
            params: serde_json::Value::Null,
        }));

        let mut found = false;
        for _ in 0..10 {
            match tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await {
                Ok(Ok(AppEvent::GeminiThreadActionRequested { action, .. })) => {
                    assert_eq!(action, "new");
                    found = true;
                    break;
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        assert!(found, "expected GeminiThreadActionRequested on bus");

        handle.abort();
    }
}
