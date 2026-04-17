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
/// NEXT task — an existing Codex thread keeps the sandbox/policy/model it
/// was spawned with because Codex locks those at `thread/start`.
#[derive(Debug, Clone)]
pub struct CodexRuntimeConfig {
    pub sandbox: String,
    pub approval_policy: String,
    pub model: Option<String>,
}

pub type SharedCodexConfig = Arc<RwLock<CodexRuntimeConfig>>;

pub struct ControlPlaneState {
    pub autonomy: SharedAutonomy,
    pub external_agent: Arc<RwLock<Option<external_agent::AgentBackend>>>,
    pub codex_config: SharedCodexConfig,
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
            state.bus.send(AppEvent::CodexConfigChanged {
                sandbox: Some(normalized),
                approval_policy: None,
                model: None,
                model_cleared: false,
            });
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
            state.bus.send(AppEvent::CodexConfigChanged {
                sandbox: None,
                approval_policy: Some(normalized),
                model: None,
                model_cleared: false,
            });
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
            state.bus.send(AppEvent::CodexConfigChanged {
                sandbox: None,
                approval_policy: None,
                model: normalized.clone(),
                model_cleared: normalized.is_none(),
            });
        }
        _ => {} // Other control messages don't update shared state
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
}
