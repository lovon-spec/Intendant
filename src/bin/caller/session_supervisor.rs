//! Daemon-side session lifecycle supervisor.
//!
//! The supervisor is the long-lived owner for sessions launched from the
//! control plane. It accepts `StartTask`, `ResumeSession`, and targeted
//! follow-up commands from the shared `EventBus`, creates per-session runtime
//! resources, and tracks the follow-up channel for each managed session.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot, Mutex as AsyncMutex};
use tokio::task::JoinHandle;

use super::*;

#[derive(Clone)]
pub struct SessionSupervisorConfig {
    pub bus: EventBus,
    pub project_root: PathBuf,
    pub autonomy: SharedAutonomy,
    pub shared_external_agent: Arc<tokio::sync::RwLock<Option<external_agent::AgentBackend>>>,
    pub shared_codex_config: control_plane::SharedCodexConfig,
    pub shared_gemini_config: control_plane::SharedGeminiConfig,
    pub frame_registry: Arc<tokio::sync::RwLock<frames::FrameRegistry>>,
    pub web_port: Option<u16>,
    pub flags_direct: bool,
    pub shared_session: Option<web_gateway::SharedActiveSession>,
}

#[derive(Clone)]
pub struct SessionSupervisor {
    config: Arc<SessionSupervisorConfig>,
    state: Arc<AsyncMutex<SupervisorState>>,
}

const EXTERNAL_ATTACH_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const SESSION_STOP_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
const SESSION_RESTART_DEDUPE_WINDOW: std::time::Duration = std::time::Duration::from_secs(5);
const EXTERNAL_ATTACH_DEDUPE_WINDOW: std::time::Duration = EXTERNAL_ATTACH_READY_TIMEOUT;
#[cfg(not(test))]
const EDIT_ATTACH_ROUTE_TIMEOUT: std::time::Duration = EXTERNAL_ATTACH_READY_TIMEOUT;
#[cfg(test)]
const EDIT_ATTACH_ROUTE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(200);
const EDIT_ATTACH_ROUTE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(20);
#[cfg(not(test))]
const TEXT_STEER_FALLBACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
#[cfg(test)]
const TEXT_STEER_FALLBACK_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(20);

#[derive(Default)]
struct SupervisorState {
    sessions: HashMap<String, ManagedSession>,
    session_aliases: HashMap<String, String>,
    related_sessions: HashMap<String, RelatedSession>,
    active_session_id: Option<String>,
    next_session_instance: u64,
    restart_dedupe: HashMap<String, std::time::Instant>,
    external_attach_dedupe: HashMap<String, std::time::Instant>,
}

#[derive(Debug, Clone)]
struct RelatedSession {
    parent_session_id: String,
    relationship: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RelatedSessionRecord {
    parent_session_id: String,
    child_session_id: String,
    relationship: String,
}

struct ManagedSession {
    session_id: String,
    source: String,
    name: Option<String>,
    phase: String,
    project_root: PathBuf,
    session_dir: PathBuf,
    follow_up_tx: mpsc::Sender<FollowUpMessage>,
    approval_registry: event::ApprovalRegistry,
    instance_id: u64,
    finished_rx: Option<oneshot::Receiver<()>>,
}

struct StoppedManagedSession {
    session_id: String,
    source: String,
    finished_rx: Option<oneshot::Receiver<()>>,
}

#[derive(Clone)]
struct EditRouteTarget {
    managed_id: String,
    source: String,
    project_root: PathBuf,
    session_dir: PathBuf,
    follow_up_tx: mpsc::Sender<FollowUpMessage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EditAttachRequest {
    source: String,
    resume_id: Option<String>,
    project_root: Option<String>,
    direct: Option<bool>,
}

#[derive(Debug, Clone)]
struct EditUserMessageRequest {
    requested_id: String,
    user_turn_index: u32,
    user_turn_revision: Option<u32>,
    original_text: Option<String>,
    text: String,
    attachments: Vec<String>,
}

impl SupervisorState {
    fn resolve_session_id(&self, session_id: &str) -> Option<String> {
        if self.sessions.contains_key(session_id) {
            return Some(session_id.to_string());
        }

        let mut current = session_id;
        for _ in 0..8 {
            let Some(next) = self.session_aliases.get(current) else {
                return None;
            };
            if self.sessions.contains_key(next) {
                return Some(next.clone());
            }
            if next == current {
                return None;
            }
            current = next;
        }
        None
    }

    fn session_is_managed(&self, session_id: &str) -> bool {
        self.resolve_session_id(session_id).is_some()
    }

    fn apply_related_session(
        &mut self,
        parent_session_id: &str,
        child_session_id: &str,
        relationship: &str,
    ) -> bool {
        let relationship = relationship.trim().to_ascii_lowercase();
        if !matches!(relationship.as_str(), "side" | "subagent") {
            return false;
        }
        let parent = parent_session_id.trim();
        let child = child_session_id.trim();
        if parent.is_empty() || child.is_empty() || parent == child {
            return false;
        }
        let Some(parent_key) = self.resolve_session_id(parent) else {
            return false;
        };
        self.session_aliases
            .insert(child.to_string(), parent_key.clone());
        self.related_sessions.insert(
            child.to_string(),
            RelatedSession {
                parent_session_id: parent_key,
                relationship,
            },
        );
        true
    }

    fn remove_session(&mut self, session_id: &str) -> Option<(String, ManagedSession)> {
        let canonical = self.resolve_session_id(session_id)?;
        let removed = self.sessions.remove(&canonical)?;
        self.session_aliases
            .retain(|alias, target| alias != &canonical && target != &canonical);
        self.related_sessions
            .retain(|child, rel| child != &canonical && rel.parent_session_id != canonical);
        if self.active_session_id.as_deref() == Some(&canonical)
            || self.active_session_id.as_deref() == Some(session_id)
        {
            self.active_session_id = self.sessions.keys().next().cloned();
        }
        Some((canonical, removed))
    }

    fn remove_session_instance(
        &mut self,
        session_id: &str,
        instance_id: u64,
    ) -> Option<(String, ManagedSession)> {
        let canonical = self.resolve_session_id(session_id)?;
        if self
            .sessions
            .get(&canonical)
            .map(|session| session.instance_id != instance_id)
            .unwrap_or(true)
        {
            return None;
        }
        self.remove_session(&canonical)
    }

    fn mark_restart_requested(&mut self, key: &str) -> bool {
        let now = std::time::Instant::now();
        self.restart_dedupe
            .retain(|_, expires_at| *expires_at > now);
        if self.restart_dedupe.contains_key(key) {
            return false;
        }
        self.restart_dedupe
            .insert(key.to_string(), now + SESSION_RESTART_DEDUPE_WINDOW);
        true
    }

    fn mark_external_attach_requested(&mut self, keys: &[String]) -> bool {
        if keys.is_empty() {
            return false;
        }
        let now = std::time::Instant::now();
        self.external_attach_dedupe
            .retain(|_, expires_at| *expires_at > now);
        if keys
            .iter()
            .any(|key| self.external_attach_dedupe.contains_key(key))
        {
            return false;
        }
        let expires_at = now + EXTERNAL_ATTACH_DEDUPE_WINDOW;
        for key in keys {
            self.external_attach_dedupe
                .insert(key.to_string(), expires_at);
        }
        true
    }

    fn clear_external_attach_requested(&mut self, keys: &[String]) {
        for key in keys {
            self.external_attach_dedupe.remove(key);
        }
    }
}

impl SessionSupervisor {
    pub fn new(config: SessionSupervisorConfig) -> Self {
        Self {
            config: Arc::new(config),
            state: Arc::new(AsyncMutex::new(SupervisorState::default())),
        }
    }

    pub fn spawn(self) -> JoinHandle<()> {
        let mut rx = self.config.bus.subscribe();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        self.observe_lifecycle_event(&event).await;
                        match event {
                            AppEvent::ControlCommand(msg) => {
                                self.handle_control_msg(msg).await;
                            }
                            AppEvent::SessionIdentity {
                                session_id,
                                source,
                                backend_session_id,
                            } => {
                                self.apply_session_identity(session_id, source, backend_session_id)
                                    .await;
                            }
                            AppEvent::SessionRelationship {
                                parent_session_id,
                                child_session_id,
                                relationship,
                                ..
                            } => {
                                self.apply_session_relationship(
                                    parent_session_id,
                                    child_session_id,
                                    relationship,
                                )
                                .await;
                            }
                            AppEvent::SessionEnded { session_id, .. } => {
                                self.remove_session_alias(&session_id).await;
                            }
                            _ => {}
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    }

    pub fn spawn_resume_listener(self) -> JoinHandle<()> {
        let mut rx = self.config.bus.subscribe();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        self.observe_lifecycle_event(&event).await;
                        match event {
                            AppEvent::ControlCommand(msg) => {
                                if self.should_handle_session_control(&msg).await {
                                    self.handle_control_msg(msg).await;
                                }
                            }
                            AppEvent::SessionIdentity {
                                session_id,
                                source,
                                backend_session_id,
                            } => {
                                self.apply_session_identity(session_id, source, backend_session_id)
                                    .await;
                            }
                            AppEvent::SessionRelationship {
                                parent_session_id,
                                child_session_id,
                                relationship,
                                ..
                            } => {
                                self.apply_session_relationship(
                                    parent_session_id,
                                    child_session_id,
                                    relationship,
                                )
                                .await;
                            }
                            AppEvent::SessionEnded { session_id, .. } => {
                                self.remove_session_alias(&session_id).await;
                            }
                            _ => {}
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    }

    pub async fn run(self) {
        let handle = self.spawn();
        let _ = handle.await;
    }

    fn attachment_project_roots(&self, primary: &Path) -> Vec<PathBuf> {
        let mut roots = vec![primary.to_path_buf()];
        if self.config.project_root != primary {
            roots.push(self.config.project_root.clone());
        }
        roots
    }

    async fn resolve_session_attachments(
        &self,
        attachments: &[String],
        session_dir: &Path,
        primary_project_root: &Path,
    ) -> Vec<external_agent::AgentAttachment> {
        if attachments.is_empty() {
            return Vec::new();
        }
        let roots = self.attachment_project_roots(primary_project_root);
        resolve_attachments_with_project_roots(
            attachments,
            &self.config.frame_registry,
            session_dir,
            &roots,
        )
        .await
    }

    async fn handle_control_msg(&self, msg: event::ControlMsg) {
        match msg {
            event::ControlMsg::CreateSession {
                task,
                name,
                project_root,
                agent,
                agent_command,
                codex_sandbox,
                codex_approval_policy,
                codex_managed_context,
                codex_context_archive,
                codex_service_tier,
                orchestrate,
                direct,
                reference_frame_ids,
                display_target,
                attachments,
            } => {
                if let Some(parsed) = parse_codex_slash_command(&task) {
                    match parsed {
                        Ok(command) if command.op == "fast" => {
                            let agent = match codex_fast_new_session_agent(agent.as_deref()) {
                                Ok(agent) => Some(agent),
                                Err(message) => {
                                    self.loop_error(message);
                                    return;
                                }
                            };
                            if !reference_frame_ids.is_empty()
                                || display_target.is_some()
                                || !attachments.is_empty()
                            {
                                self.warn(
                                    "/fast creates an idle Codex session; attachments and display metadata were ignored",
                                );
                            }
                            self.start_new_session(
                                String::new(),
                                name,
                                project_root,
                                agent,
                                agent_command,
                                codex_sandbox,
                                codex_approval_policy,
                                codex_managed_context,
                                codex_context_archive,
                                orchestrate,
                                direct,
                                Vec::new(),
                                None,
                                Vec::new(),
                                Some(
                                    crate::external_agent::codex::CODEX_FAST_SERVICE_TIER
                                        .to_string(),
                                ),
                            )
                            .await;
                            return;
                        }
                        Ok(_) | Err(_) => {}
                    }
                    if !reference_frame_ids.is_empty()
                        || display_target.is_some()
                        || agent.is_some()
                        || agent_command.is_some()
                        || codex_sandbox.is_some()
                        || codex_approval_policy.is_some()
                        || codex_managed_context.is_some()
                        || codex_context_archive.is_some()
                        || codex_service_tier.is_some()
                        || name.is_some()
                    {
                        self.warn(
                            "Slash command dropped new-session metadata; routing to active Codex session",
                        );
                    }
                    self.route_follow_up(None, task, direct, attachments, None)
                        .await;
                    return;
                }
                self.start_new_session(
                    task,
                    name,
                    project_root,
                    agent,
                    agent_command,
                    codex_sandbox,
                    codex_approval_policy,
                    codex_managed_context,
                    codex_context_archive,
                    orchestrate,
                    direct,
                    reference_frame_ids,
                    display_target,
                    attachments,
                    codex_service_tier,
                )
                .await;
            }
            event::ControlMsg::StartTask {
                session_id: Some(session_id),
                task,
                direct,
                reference_frame_ids,
                display_target,
                attachments,
                follow_up_id,
                ..
            } => {
                if !reference_frame_ids.is_empty() || display_target.is_some() {
                    self.warn(&format!(
                        "Targeted StartTask for {} dropped reference frame/display metadata; routing text as follow-up",
                        short_session(&session_id)
                    ));
                }
                self.route_follow_up(Some(session_id), task, direct, attachments, follow_up_id)
                    .await;
            }
            event::ControlMsg::StartTask {
                session_id: None,
                task,
                orchestrate,
                direct,
                reference_frame_ids,
                display_target,
                attachments,
                follow_up_id: _,
            } => {
                if let Some(parsed) = parse_codex_slash_command(&task) {
                    match parsed {
                        Ok(command) if command.op == "fast" => {
                            if !reference_frame_ids.is_empty()
                                || display_target.is_some()
                                || !attachments.is_empty()
                            {
                                self.warn(
                                    "/fast creates an idle Codex session; attachments and display metadata were ignored",
                                );
                            }
                            self.start_new_session(
                                String::new(),
                                None,
                                None,
                                Some("codex".to_string()),
                                None,
                                None,
                                None,
                                None,
                                None,
                                orchestrate,
                                direct,
                                Vec::new(),
                                None,
                                Vec::new(),
                                Some(
                                    crate::external_agent::codex::CODEX_FAST_SERVICE_TIER
                                        .to_string(),
                                ),
                            )
                            .await;
                            return;
                        }
                        Ok(_) | Err(_) => {}
                    }
                    if !reference_frame_ids.is_empty() || display_target.is_some() {
                        self.warn(
                            "Slash command dropped reference frame/display metadata; routing to active Codex session",
                        );
                    }
                    self.route_follow_up(None, task, direct, attachments, None)
                        .await;
                    return;
                }
                self.start_new_session(
                    task,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    orchestrate,
                    direct,
                    reference_frame_ids,
                    display_target,
                    attachments,
                    None,
                )
                .await;
            }
            event::ControlMsg::ResumeSession {
                source,
                session_id,
                resume_id,
                project_root,
                task,
                direct,
                attachments,
                agent_command,
                codex_sandbox,
                codex_approval_policy,
                codex_managed_context,
                codex_context_archive,
            } => {
                self.resume_session(
                    source,
                    session_id,
                    resume_id,
                    project_root,
                    task,
                    direct,
                    attachments,
                    agent_command,
                    codex_sandbox,
                    codex_approval_policy,
                    codex_managed_context,
                    codex_context_archive,
                    false,
                )
                .await;
            }
            event::ControlMsg::StopSession { session_id } => {
                self.stop_managed_session(Some(session_id), "stopped by user")
                    .await;
            }
            event::ControlMsg::RestartSession {
                source,
                session_id,
                resume_id,
                project_root,
                task,
                direct,
                attachments,
                agent_command,
                codex_sandbox,
                codex_approval_policy,
                codex_managed_context,
                codex_context_archive,
            } => {
                self.restart_session(
                    source,
                    session_id,
                    resume_id,
                    project_root,
                    task,
                    direct,
                    attachments,
                    agent_command,
                    codex_sandbox,
                    codex_approval_policy,
                    codex_managed_context,
                    codex_context_archive,
                )
                .await;
            }
            event::ControlMsg::FollowUp {
                session_id,
                text,
                direct,
                follow_up_id,
            } => {
                self.route_follow_up(session_id, text, direct, vec![], follow_up_id)
                    .await;
            }
            event::ControlMsg::EditUserMessage {
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
                self.route_edit_user_message(
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
                )
                .await;
            }
            event::ControlMsg::Interrupt {
                session_id,
                expected_turn: _,
            } => {
                self.route_interrupt(session_id).await;
            }
            event::ControlMsg::Steer {
                session_id,
                text,
                id,
                attachments,
            } => {
                self.route_steer(session_id, text, id, attachments).await;
            }
            event::ControlMsg::CancelSteer {
                session_id,
                id,
                reason,
            } => {
                self.route_cancel_steer(session_id, id, reason).await;
            }
            event::ControlMsg::CancelFollowUp {
                session_id,
                id,
                reason,
            } => {
                self.route_cancel_follow_up(session_id, id, reason).await;
            }
            event::ControlMsg::Approve { session_id, id } => {
                self.resolve_approval(session_id, id, event::ApprovalResponse::Approve, "approve")
                    .await;
            }
            event::ControlMsg::Deny { session_id, id } => {
                self.resolve_approval(session_id, id, event::ApprovalResponse::Deny, "deny")
                    .await;
            }
            event::ControlMsg::Skip { session_id, id } => {
                self.resolve_approval(session_id, id, event::ApprovalResponse::Skip, "skip")
                    .await;
            }
            event::ControlMsg::ApproveAll { session_id, id } => {
                self.resolve_approval(
                    session_id,
                    id,
                    event::ApprovalResponse::ApproveAll,
                    "approve_all",
                )
                .await;
            }
            event::ControlMsg::RenameSession {
                session_id,
                backend_session_id,
                source,
                name,
            } => {
                self.rename_session(session_id, backend_session_id, source, name)
                    .await;
            }
            event::ControlMsg::ConfigureSessionAgent {
                session_id,
                source,
                backend_session_id,
                intendant_session_id,
                agent_command,
                codex_sandbox,
                codex_approval_policy,
                codex_managed_context,
                codex_context_archive,
            } => {
                self.configure_session_agent(
                    session_id,
                    source,
                    backend_session_id,
                    intendant_session_id,
                    agent_command,
                    codex_sandbox,
                    codex_approval_policy,
                    codex_managed_context,
                    codex_context_archive,
                )
                .await;
            }
            _ => {}
        }
    }

    async fn should_handle_session_control(&self, msg: &event::ControlMsg) -> bool {
        match msg {
            event::ControlMsg::CreateSession { .. } => true,
            event::ControlMsg::ResumeSession { .. } => true,
            event::ControlMsg::RestartSession { .. } => true,
            event::ControlMsg::StopSession { .. } => true,
            event::ControlMsg::RenameSession { .. } => true,
            event::ControlMsg::ConfigureSessionAgent { .. } => true,
            msg if control_msg_can_attach_unmanaged_session(msg) => true,
            _ => {
                if let Some(session_id) = control_target_session_id(msg) {
                    self.session_is_managed(session_id).await
                } else {
                    false
                }
            }
        }
    }

    async fn start_new_session(
        &self,
        task: String,
        name: Option<String>,
        project_root: Option<String>,
        agent: Option<String>,
        agent_command: Option<String>,
        codex_sandbox: Option<String>,
        codex_approval_policy: Option<String>,
        codex_managed_context: Option<String>,
        codex_context_archive: Option<String>,
        orchestrate: Option<bool>,
        direct: Option<bool>,
        reference_frame_ids: Vec<String>,
        display_target: Option<String>,
        attachments: Vec<String>,
        codex_service_tier: Option<String>,
    ) {
        let session_name = match normalize_session_name_option(name.as_deref()) {
            Ok(name) => name,
            Err(e) => {
                self.loop_error(format!("Session create failed: {}", e));
                return;
            }
        };
        let log_dir = session_log::SessionLog::resolve_path(None);
        let session_log = match session_log::SessionLog::open(log_dir.clone()) {
            Ok(log) => Arc::new(Mutex::new(log)),
            Err(e) => {
                self.loop_error(format!("Session create failed: {}", e));
                return;
            }
        };

        let session_id = session_log
            .lock()
            .map(|log| log.session_id().to_string())
            .unwrap_or_else(|_| path_file_name(&log_dir));
        let project_root =
            match resolve_project_root_override(project_root, &self.config.project_root) {
                Ok(root) => root,
                Err(e) => {
                    self.loop_error(format!("Project load failed: {}", e));
                    return;
                }
            };
        let project = match Project::from_root(project_root) {
            Ok(project) => project,
            Err(e) => {
                self.loop_error(format!("Project load failed: {}", e));
                return;
            }
        };

        let task_meta = if task.trim().is_empty() {
            None
        } else {
            Some(task.as_str())
        };
        write_session_meta(
            &session_log,
            &project.root,
            task_meta,
            session_name.as_deref(),
        );
        self.activate_shared_session(session_log.clone()).await;

        if !reference_frame_ids.is_empty() {
            if self
                .spawn_cu_task(
                    &session_id,
                    &task,
                    &project,
                    &session_log,
                    &log_dir,
                    reference_frame_ids,
                    display_target,
                )
                .await
            {
                self.config.bus.send(AppEvent::SessionStarted {
                    session_id: session_id.clone(),
                    task: Some(task.clone()),
                });
                return;
            }
        }

        let use_direct = direct.unwrap_or(false)
            || orchestrate
                .map(|o| !o)
                .unwrap_or_else(|| self.config.flags_direct || is_simple_task(&task));
        let agent_selection = match SessionAgentSelection::from_wire(agent.as_deref()) {
            Ok(selection) => selection,
            Err(e) => {
                self.loop_error(format!("Session create failed: {}", e));
                return;
            }
        };
        let backend = match agent_selection {
            SessionAgentSelection::Configured => {
                resolve_agent_backend(&self.config.shared_external_agent, &project).await
            }
            SessionAgentSelection::Internal => None,
            SessionAgentSelection::External(backend) => Some(backend),
        };
        let mut project = match self
            .project_with_runtime_config(project.root.clone(), backend.as_ref())
            .await
        {
            Ok(project) => project,
            Err(e) => {
                self.loop_error(format!("Project load failed: {}", e));
                return;
            }
        };
        let agent_command = normalize_session_agent_command(agent_command.as_deref());
        if let Some(command) = agent_command {
            let Some(ref backend) = backend else {
                self.loop_error(
                    "Session create failed: agent_command requires an external agent".to_string(),
                );
                return;
            };
            apply_session_agent_command(&mut project, backend, command);
        }
        if let Some(mode) = normalize_session_codex_sandbox(codex_sandbox.as_deref()) {
            let Some(ref backend) = backend else {
                self.loop_error("Session create failed: codex_sandbox requires Codex".to_string());
                return;
            };
            if let Err(e) = apply_session_codex_sandbox(&mut project, backend, mode) {
                self.loop_error(format!("Session create failed: {}", e));
                return;
            }
        }
        if let Some(policy) =
            normalize_session_codex_approval_policy(codex_approval_policy.as_deref())
        {
            let Some(ref backend) = backend else {
                self.loop_error(
                    "Session create failed: codex_approval_policy requires Codex".to_string(),
                );
                return;
            };
            if let Err(e) = apply_session_codex_approval_policy(&mut project, backend, policy) {
                self.loop_error(format!("Session create failed: {}", e));
                return;
            }
        }
        if let Some(mode) =
            normalize_session_codex_managed_context(codex_managed_context.as_deref())
        {
            let Some(ref backend) = backend else {
                self.loop_error(
                    "Session create failed: codex_managed_context requires Codex".to_string(),
                );
                return;
            };
            if let Err(e) = apply_session_codex_managed_context(&mut project, backend, mode) {
                self.loop_error(format!("Session create failed: {}", e));
                return;
            }
        }
        if let Some(mode) =
            normalize_session_codex_context_archive(codex_context_archive.as_deref())
        {
            let Some(ref backend) = backend else {
                self.loop_error(
                    "Session create failed: codex_context_archive requires Codex".to_string(),
                );
                return;
            };
            if let Err(e) = apply_session_codex_context_archive(&mut project, backend, mode) {
                self.loop_error(format!("Session create failed: {}", e));
                return;
            }
        }
        let codex_service_tier =
            normalize_session_codex_service_tier(codex_service_tier.as_deref());
        if codex_service_tier.is_some() {
            match backend.as_ref() {
                Some(external_agent::AgentBackend::Codex) => {}
                Some(_) | None => {
                    self.loop_error(
                        "Session create failed: codex_service_tier requires Codex".to_string(),
                    );
                    return;
                }
            }
        }
        let mut codex_home = None;
        if let Some(backend) = backend.as_ref() {
            let mut config = crate::session_config::from_project(backend, &project);
            if matches!(backend, external_agent::AgentBackend::Codex)
                && codex_service_tier.is_some()
            {
                config.codex_service_tier = codex_service_tier.clone();
            }
            if matches!(backend, external_agent::AgentBackend::Codex) {
                codex_home = config.codex_home.clone();
            }
            if let Err(e) = crate::session_config::write_log_dir_config(&log_dir, &config) {
                self.warn(&format!(
                    "Session launch config was not persisted for {}: {}",
                    short_session(&session_id),
                    e
                ));
            }
        }
        let session_dir = session_log
            .lock()
            .map(|log| log.dir().to_path_buf())
            .unwrap_or_else(|_| log_dir.clone());
        let resolved_attachments = self
            .resolve_session_attachments(&attachments, &session_dir, &project.root)
            .await;
        if resolved_attachments.len() < attachments.len() {
            self.warn(&format!(
                "Only resolved {} of {} requested attachment(s) for new session",
                resolved_attachments.len(),
                attachments.len()
            ));
        }
        let attachments_for_agent = UserAttachments::from_items(resolved_attachments);

        let source = backend
            .as_ref()
            .map(|b| b.as_short_str().to_string())
            .unwrap_or_else(|| "intendant".to_string());

        let emit_session_started_after_identity = backend.is_some();
        if !emit_session_started_after_identity {
            self.config.bus.send(AppEvent::SessionStarted {
                session_id: session_id.clone(),
                task: Some(task.clone()),
            });
        }

        if !task.trim().is_empty() {
            emit_task_dispatched_log(&self.config.bus, &session_log, &task, attachments.len());
        }
        self.spawn_agent_session(
            session_id,
            source,
            task,
            project,
            session_log,
            log_dir,
            backend,
            use_direct,
            attachments_for_agent,
            session_name,
            None,
            emit_session_started_after_identity,
            None,
            codex_service_tier,
            codex_home,
        )
        .await;
    }

    async fn resume_session(
        &self,
        source: String,
        session_id: String,
        resume_id: Option<String>,
        project_root: Option<String>,
        task: Option<String>,
        direct: Option<bool>,
        attachments: Vec<String>,
        agent_command: Option<String>,
        codex_sandbox: Option<String>,
        codex_approval_policy: Option<String>,
        codex_managed_context: Option<String>,
        codex_context_archive: Option<String>,
        force_new: bool,
    ) {
        let source_norm = source.trim().to_lowercase();
        let resume_task = task.and_then(|task| {
            let trimmed = task.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        });
        let external_backend = if source_norm == "intendant" {
            None
        } else {
            match external_agent::AgentBackend::from_str_loose(&source_norm) {
                Some(backend) => Some(backend),
                None => {
                    self.loop_error(format!("Unsupported session source: {}", source));
                    return;
                }
            }
        };
        let is_external = external_backend.is_some();
        let resume_token = resume_id.unwrap_or_else(|| session_id.clone());
        let external_attach_keys = if is_external && resume_task.is_none() && !force_new {
            external_attach_dedupe_keys(&source_norm, &session_id, &resume_token)
        } else {
            Vec::new()
        };
        let session_agent_config = external_backend.as_ref().map(|backend| {
            let mut config = crate::session_config::from_wire(
                Some(backend.as_short_str()),
                agent_command.as_deref(),
                codex_sandbox.as_deref(),
                codex_approval_policy.as_deref(),
                codex_managed_context.as_deref(),
                codex_context_archive.as_deref(),
                None,
            );
            if let Some(persisted) = crate::session_config::load_for_resume(
                &crate::platform::home_dir(),
                backend.as_short_str(),
                &session_id,
                Some(&resume_token),
            ) {
                config.merge_missing_from(persisted);
            }
            config
        });
        let project_root = if external_backend.is_some() {
            match resolve_external_resume_project_root(
                project_root,
                session_agent_config.as_ref(),
                &self.config.project_root,
            ) {
                Ok(root) => root,
                Err(e) => {
                    self.loop_error(format!("Project load failed: {}", e));
                    return;
                }
            }
        } else {
            project_root
                .map(PathBuf::from)
                .unwrap_or_else(|| self.config.project_root.clone())
        };

        if resume_task.is_none() {
            if let Some(existing_id) = self
                .find_managed_session_id(&source_norm, &session_id, &resume_token)
                .await
                .filter(|_| !force_new)
            {
                {
                    let mut state = self.state.lock().await;
                    state.active_session_id = Some(existing_id);
                }
                self.emit_attached_status(&resume_token, &source_norm).await;
            } else if external_backend.is_none() {
                match session_log::SessionLog::find_session_by_id(&session_id) {
                    Some(dir) => match session_log::SessionLog::open(dir) {
                        Ok(log) => {
                            self.activate_shared_session(Arc::new(Mutex::new(log)))
                                .await
                        }
                        Err(e) => {
                            self.loop_error(format!("Session open failed: {}", e));
                            return;
                        }
                    },
                    None => {
                        self.loop_error(format!("Session '{}' was not found", session_id));
                        return;
                    }
                }
                self.emit_attached_status(&session_id, &source_norm).await;
            } else {
                if !external_attach_keys.is_empty() {
                    let mut state = self.state.lock().await;
                    if !state.mark_external_attach_requested(&external_attach_keys) {
                        drop(state);
                        self.info(&format!(
                            "Attach ignored: {} session {} is already attaching",
                            source_norm,
                            short_session(&resume_token)
                        ));
                        return;
                    }
                }
                let (ready_tx, ready_rx) = oneshot::channel();
                let log_dir = external_resume_log_dir(&session_id, force_new);
                let session_log = match session_log::SessionLog::open(log_dir.clone()) {
                    Ok(log) => Arc::new(Mutex::new(log)),
                    Err(e) => {
                        self.clear_external_attach_request(&external_attach_keys)
                            .await;
                        self.loop_error(format!("Session open failed: {}", e));
                        return;
                    }
                };
                let mut project = match self
                    .project_with_runtime_config(project_root.clone(), external_backend.as_ref())
                    .await
                {
                    Ok(project) => project,
                    Err(e) => {
                        self.clear_external_attach_request(&external_attach_keys)
                            .await;
                        self.loop_error(format!("Project load failed: {}", e));
                        return;
                    }
                };
                if let (Some(backend), Some(config)) =
                    (external_backend.as_ref(), session_agent_config.as_ref())
                {
                    crate::session_config::apply_to_project(&mut project, backend, config);
                }
                let effective_session_agent_config = external_backend.as_ref().map(|backend| {
                    effective_session_agent_config_from_project(
                        backend,
                        &project,
                        session_agent_config.as_ref(),
                    )
                });

                write_session_meta(&session_log, &project.root, None, None);
                if let Some(config) = effective_session_agent_config.as_ref() {
                    let _ = crate::session_config::write_log_dir_config(&log_dir, config);
                }
                let codex_service_tier = effective_session_agent_config
                    .as_ref()
                    .and_then(|config| config.codex_service_tier.clone());
                let codex_home = effective_session_agent_config
                    .as_ref()
                    .and_then(|config| config.codex_home.clone());
                let intendant_session_id = session_log
                    .lock()
                    .map(|log| log.session_id().to_string())
                    .unwrap_or_else(|_| path_file_name(&log_dir));
                self.activate_shared_session(session_log.clone()).await;
                self.spawn_agent_session(
                    intendant_session_id,
                    source_norm.clone(),
                    String::new(),
                    project,
                    session_log,
                    log_dir,
                    external_backend.clone(),
                    direct.unwrap_or(true),
                    UserAttachments::default(),
                    None,
                    Some(resume_token.clone()),
                    false,
                    Some(ready_tx),
                    codex_service_tier,
                    codex_home,
                )
                .await;
                self.clear_external_attach_request(&external_attach_keys)
                    .await;
                self.emit_external_attached_when_ready(resume_token, source_norm, ready_rx);
                return;
            }

            self.config.bus.send(AppEvent::SessionAttached {
                session_id: if is_external {
                    resume_token
                } else {
                    session_id
                },
                source: source_norm,
            });
            return;
        }
        let resume_task = resume_task.expect("checked above");

        if external_backend.is_some() && !force_new {
            if self
                .find_managed_session_id(&source_norm, &session_id, &resume_token)
                .await
                .is_some()
            {
                self.route_follow_up(Some(session_id), resume_task, direct, attachments, None)
                    .await;
                return;
            }
        }

        let log_dir = if external_backend.is_none() {
            match session_log::SessionLog::find_session_by_id(&session_id) {
                Some(dir) => dir,
                None => {
                    self.loop_error(format!("Session '{}' was not found", session_id));
                    return;
                }
            }
        } else {
            external_resume_log_dir(&session_id, force_new)
        };
        let session_log = match session_log::SessionLog::open(log_dir.clone()) {
            Ok(log) => Arc::new(Mutex::new(log)),
            Err(e) => {
                self.loop_error(format!("Session open failed: {}", e));
                return;
            }
        };
        let intendant_session_id = session_log
            .lock()
            .map(|log| log.session_id().to_string())
            .unwrap_or_else(|_| path_file_name(&log_dir));
        let live_session_id = if external_backend.is_some() {
            resume_token.clone()
        } else {
            intendant_session_id.clone()
        };
        let mut project = match self
            .project_with_runtime_config(project_root.clone(), external_backend.as_ref())
            .await
        {
            Ok(project) => project,
            Err(e) => {
                self.loop_error(format!("Project load failed: {}", e));
                return;
            }
        };
        if let (Some(backend), Some(config)) =
            (external_backend.as_ref(), session_agent_config.as_ref())
        {
            crate::session_config::apply_to_project(&mut project, backend, config);
        }
        let effective_session_agent_config = external_backend.as_ref().map(|backend| {
            effective_session_agent_config_from_project(
                backend,
                &project,
                session_agent_config.as_ref(),
            )
        });

        write_session_meta(&session_log, &project.root, Some(&resume_task), None);
        if let Some(config) = effective_session_agent_config.as_ref() {
            let _ = crate::session_config::write_log_dir_config(&log_dir, config);
        }
        let codex_service_tier = effective_session_agent_config
            .as_ref()
            .and_then(|config| config.codex_service_tier.clone());
        let codex_home = effective_session_agent_config
            .as_ref()
            .and_then(|config| config.codex_home.clone());
        self.activate_shared_session(session_log.clone()).await;
        self.config.bus.send(AppEvent::SessionStarted {
            session_id: live_session_id.clone(),
            task: Some(resume_task.clone()),
        });

        let session_dir = session_log
            .lock()
            .map(|log| log.dir().to_path_buf())
            .unwrap_or_else(|_| log_dir.clone());
        let resolved_attachments = self
            .resolve_session_attachments(&attachments, &session_dir, &project.root)
            .await;
        if resolved_attachments.len() < attachments.len() {
            self.warn(&format!(
                "Only resolved {} of {} requested attachment(s) while resuming {} session {}",
                resolved_attachments.len(),
                attachments.len(),
                if external_backend.is_some() {
                    source_norm.as_str()
                } else {
                    "intendant"
                },
                short_session(&live_session_id)
            ));
        }

        emit_task_dispatched_log(
            &self.config.bus,
            &session_log,
            &resume_task,
            attachments.len(),
        );
        self.spawn_agent_session(
            if external_backend.is_some() {
                intendant_session_id
            } else {
                live_session_id
            },
            source_norm,
            resume_task,
            project,
            session_log,
            log_dir,
            external_backend,
            direct.unwrap_or(true),
            UserAttachments::from_items(resolved_attachments),
            None,
            Some(resume_token),
            false,
            None,
            codex_service_tier,
            codex_home,
        )
        .await;
    }

    async fn find_managed_session_id(
        &self,
        source: &str,
        session_id: &str,
        resume_token: &str,
    ) -> Option<String> {
        let state = self.state.lock().await;
        state
            .sessions
            .values()
            .find(|session| {
                session.source == source
                    && (session.session_id == session_id || session.session_id == resume_token)
            })
            .map(|session| session.session_id.clone())
            .or_else(|| {
                state
                    .resolve_session_id(session_id)
                    .or_else(|| state.resolve_session_id(resume_token))
            })
    }

    async fn spawn_agent_session(
        &self,
        session_id: String,
        source: String,
        task: String,
        project: Project,
        session_log: SharedSessionLog,
        log_dir: PathBuf,
        backend: Option<external_agent::AgentBackend>,
        use_direct: bool,
        attachments: UserAttachments,
        session_name: Option<String>,
        resume_token: Option<String>,
        emit_session_started_after_identity: bool,
        ready_for_thread_actions: Option<oneshot::Sender<()>>,
        codex_service_tier: Option<String>,
        codex_home: Option<String>,
    ) {
        let (follow_up_tx, follow_up_rx) = mpsc::channel::<FollowUpMessage>(16);
        let (finished_tx, finished_rx) = oneshot::channel();
        let approval_registry = event::ApprovalRegistry::default();
        let context_injection = event::ContextInjectionQueue::default();
        let session_instance_id = self
            .register_session(
                session_id.clone(),
                source.clone(),
                if task.trim().is_empty() {
                    "idle".to_string()
                } else {
                    "thinking".to_string()
                },
                project.root.clone(),
                log_dir.clone(),
                follow_up_tx,
                approval_registry.clone(),
                session_name,
                Some(finished_rx),
            )
            .await;

        let supervisor = self.clone();
        let bus = self.config.bus.clone();
        let autonomy = self.config.autonomy.clone();
        let web_port = self.config.web_port;
        tokio::spawn(async move {
            let result = if let Some(backend) = backend {
                run_external_agent_mode(
                    backend,
                    task.clone(),
                    project,
                    bus.clone(),
                    autonomy,
                    session_log.clone(),
                    log_dir,
                    follow_up_rx,
                    None,
                    approval_registry,
                    context_injection,
                    true,
                    web_port,
                    attachments,
                    resume_token,
                    codex_service_tier,
                    codex_home,
                    Some(session_id.clone()),
                    emit_session_started_after_identity,
                    ready_for_thread_actions,
                )
                .await
            } else {
                let provider = match provider::select_provider() {
                    Ok(provider) => provider,
                    Err(e) => {
                        supervisor
                            .finish_session(
                                session_id,
                                session_instance_id,
                                session_log,
                                task,
                                Err(e),
                            )
                            .await;
                        let _ = finished_tx.send(());
                        return;
                    }
                };
                if use_direct {
                    run_direct_mode(
                        provider,
                        task.clone(),
                        project,
                        bus.clone(),
                        autonomy,
                        session_log.clone(),
                        log_dir,
                        None,
                        follow_up_rx,
                        None,
                        approval_registry,
                        context_injection,
                        true,
                        attachments,
                    )
                    .await
                } else {
                    run_user_mode(
                        provider,
                        task.clone(),
                        project,
                        bus.clone(),
                        autonomy,
                        session_log.clone(),
                    )
                    .await
                }
            };

            supervisor
                .finish_session(session_id, session_instance_id, session_log, task, result)
                .await;
            let _ = finished_tx.send(());
        });
    }

    fn emit_external_attached_when_ready(
        &self,
        session_id: String,
        source: String,
        ready_rx: oneshot::Receiver<()>,
    ) {
        let supervisor = self.clone();
        tokio::spawn(async move {
            match tokio::time::timeout(EXTERNAL_ATTACH_READY_TIMEOUT, ready_rx).await {
                Ok(Ok(())) => {
                    supervisor.emit_attached_status(&session_id, &source).await;
                    supervisor
                        .config
                        .bus
                        .send(AppEvent::SessionAttached { session_id, source });
                }
                Ok(Err(_)) => {
                    supervisor.loop_error(format!(
                        "{} session {} stopped before it was ready for thread actions",
                        source,
                        short_session(&session_id)
                    ));
                }
                Err(_) => {
                    supervisor.loop_error(format!(
                        "{} session {} did not become ready for thread actions within {}s",
                        source,
                        short_session(&session_id),
                        EXTERNAL_ATTACH_READY_TIMEOUT.as_secs()
                    ));
                }
            }
        });
    }

    async fn spawn_cu_task(
        &self,
        session_id: &str,
        task: &str,
        project: &Project,
        session_log: &SharedSessionLog,
        log_dir: &std::path::Path,
        reference_frame_ids: Vec<String>,
        display_target: Option<String>,
    ) -> bool {
        let reference_images =
            resolve_frame_ids(&reference_frame_ids, &self.config.frame_registry).await;
        if reference_images.is_empty() {
            return false;
        }
        let cu_provider = match provider::select_cu_provider(&project.config.computer_use) {
            Ok(provider) => provider,
            Err(e) => {
                self.loop_error(format!("CU provider failed: {}", e));
                return true;
            }
        };
        let supervisor = self.clone();
        let session_id = session_id.to_string();
        let task = task.to_string();
        let session_log = session_log.clone();
        let log_dir = log_dir.to_path_buf();
        let bus = self.config.bus.clone();
        let cu_config = project.config.computer_use.clone();
        tokio::spawn(async move {
            bus.send(AppEvent::PresenceLog {
                message: format!("Starting CU task: {}", task),
                level: None,
                turn: None,
            });
            let cu_target = display_target.as_deref().map(parse_display_target_str);
            let result = run_cu_task(
                cu_provider.as_ref(),
                &task,
                reference_images,
                vec![],
                &session_log,
                &log_dir,
                &bus,
                &cu_config,
                cu_target,
            )
            .await;

            let summary = match result {
                Ok(CuTaskResult::Completed(stats)) => {
                    bus.send(AppEvent::PresenceLog {
                        message: format!("CU task complete ({} turns)", stats.turns),
                        level: None,
                        turn: None,
                    });
                    Ok(stats)
                }
                Ok(CuTaskResult::Escalate { task }) => {
                    bus.send(AppEvent::PresenceLog {
                        message: format!(
                            "CU escalated (not a display task): {}",
                            short_text(&task, 80)
                        ),
                        level: None,
                        turn: None,
                    });
                    Ok(LoopStats::default())
                }
                Err(e) => {
                    bus.send(AppEvent::PresenceLog {
                        message: format!("CU task error: {}", e),
                        level: Some(types::LogLevel::Error),
                        turn: None,
                    });
                    Err(e)
                }
            };
            supervisor
                .finish_session(session_id, 0, session_log, task, summary)
                .await;
        });
        true
    }

    async fn route_follow_up(
        &self,
        session_id: Option<String>,
        text: String,
        _direct: Option<bool>,
        attachments: Vec<String>,
        follow_up_id: Option<String>,
    ) {
        let (target_id, entry) = {
            let state = self.state.lock().await;
            let requested_id = session_id.or_else(|| state.active_session_id.clone());
            let Some(requested_id) = requested_id else {
                drop(state);
                self.warn("FollowUp dropped: no active managed session");
                return;
            };
            let target_id = state
                .resolve_session_id(&requested_id)
                .unwrap_or_else(|| requested_id.clone());
            let entry = state.sessions.get(&target_id).map(|s| {
                let relation = state.related_sessions.get(&requested_id).cloned();
                (
                    s.session_id.clone(),
                    s.source.clone(),
                    s.project_root.clone(),
                    s.session_dir.clone(),
                    s.follow_up_tx.clone(),
                    requested_id.clone(),
                    relation,
                )
            });
            (target_id, entry)
        };

        match entry {
            Some((managed_id, source, project_root, session_dir, tx, requested_id, relation)) => {
                if let Some(parsed) = parse_codex_slash_command(&text) {
                    match parsed {
                        Ok(command) => {
                            if source == "codex" {
                                if relation
                                    .as_ref()
                                    .is_some_and(|rel| rel.relationship == "subagent")
                                {
                                    self.warn(&format!(
                                        "Slash command /{} is not supported for Codex subagent session {}",
                                        command.op,
                                        short_session(&requested_id)
                                    ));
                                    return;
                                }
                                if !attachments.is_empty() {
                                    self.warn(&format!(
                                        "Slash command /{} for Codex session {} ignored {} attachment(s)",
                                        command.op,
                                        short_session(&managed_id),
                                        attachments.len()
                                    ));
                                }
                                self.config.bus.send(AppEvent::ControlCommand(
                                    event::ControlMsg::CodexThreadAction {
                                        session_id: Some(managed_id),
                                        op: command.op,
                                        params: command.params,
                                    },
                                ));
                            } else {
                                self.warn(&format!(
                                    "Slash command /{} is only supported for Codex sessions; target {} session {}",
                                    command.op,
                                    source,
                                    short_session(&managed_id)
                                ));
                            }
                        }
                        Err(message) => self.warn(&message),
                    }
                    return;
                }

                let resolved_attachments = self
                    .resolve_session_attachments(&attachments, &session_dir, &project_root)
                    .await;
                if resolved_attachments.len() < attachments.len() {
                    self.warn(&format!(
                        "Only resolved {} of {} requested attachment(s) for {} session {}",
                        resolved_attachments.len(),
                        attachments.len(),
                        source,
                        short_session(&managed_id)
                    ));
                }
                if relation
                    .as_ref()
                    .is_some_and(|rel| rel.relationship == "side")
                    && source == "codex"
                {
                    if tx.is_closed() {
                        emit_follow_up_status(
                            &self.config.bus,
                            Some(requested_id.clone()),
                            &follow_up_id,
                            None,
                            "failed",
                            Some("target session is not accepting input"),
                        );
                        self.warn(&format!(
                            "FollowUp dropped: {} side session {} in {} is not accepting input",
                            source,
                            short_session(&requested_id),
                            project_root.display()
                        ));
                    } else {
                        self.config.bus.send(AppEvent::ExternalFollowUpRequested {
                            session_id: requested_id.clone(),
                            text: text.clone(),
                            attachments: resolved_attachments,
                            follow_up_id: follow_up_id.clone(),
                        });
                        emit_follow_up_status(
                            &self.config.bus,
                            Some(requested_id),
                            &follow_up_id,
                            Some(&text),
                            "queued",
                            Some("queued for side conversation"),
                        );
                    }
                    return;
                }
                let msg = FollowUpMessage::with_attachments(
                    text.clone(),
                    UserAttachments::from_items(resolved_attachments),
                )
                .for_target(Some(requested_id.clone()))
                .with_follow_up_id(follow_up_id.clone());
                if tx.send(msg).await.is_err() {
                    emit_follow_up_status(
                        &self.config.bus,
                        Some(requested_id.clone()),
                        &follow_up_id,
                        None,
                        "failed",
                        Some("target session is not accepting input"),
                    );
                    self.warn(&format!(
                        "FollowUp dropped: {} session {} in {} is not accepting input",
                        source,
                        short_session(&managed_id),
                        project_root.display()
                    ));
                } else {
                    emit_follow_up_status(
                        &self.config.bus,
                        Some(requested_id),
                        &follow_up_id,
                        Some(&text),
                        "queued",
                        Some("queued for next turn"),
                    );
                }
            }
            None => {
                emit_follow_up_status(
                    &self.config.bus,
                    Some(target_id.clone()),
                    &follow_up_id,
                    Some(&text),
                    "failed",
                    Some("target session is not managed by this daemon"),
                );
                self.warn(&format!(
                    "FollowUp dropped: session {} is not managed by this daemon",
                    short_session(&target_id)
                ));
            }
        }
    }

    async fn route_edit_user_message(
        &self,
        session_id: Option<String>,
        source: Option<String>,
        resume_id: Option<String>,
        project_root: Option<String>,
        direct: Option<bool>,
        user_turn_index: u32,
        user_turn_revision: Option<u32>,
        original_text: Option<String>,
        text: String,
        attachments: Vec<String>,
    ) {
        let requested_id = {
            let state = self.state.lock().await;
            let requested_id = session_id.or_else(|| state.active_session_id.clone());
            let Some(requested_id) = requested_id else {
                drop(state);
                self.warn("Edit dropped: no active managed session");
                return;
            };
            requested_id
        };

        let request = EditUserMessageRequest {
            requested_id: requested_id.clone(),
            user_turn_index,
            user_turn_revision,
            original_text,
            text,
            attachments,
        };

        let (target_id, entry, relation) = self.lookup_edit_route_target(&requested_id).await;
        if entry.is_none() {
            if let Some(attach) = edit_attach_request(source, resume_id, project_root, direct) {
                let lookup_id = attach
                    .resume_id
                    .as_deref()
                    .filter(|id| !id.trim().is_empty())
                    .unwrap_or(&requested_id)
                    .to_string();
                self.resume_session(
                    attach.source,
                    requested_id.clone(),
                    Some(lookup_id.clone()),
                    attach.project_root,
                    None,
                    Some(attach.direct.unwrap_or(true)),
                    Vec::new(),
                    None,
                    None,
                    None,
                    None,
                    None,
                    false,
                )
                .await;
                self.queue_edit_user_message_after_attach(lookup_id, request);
                return;
            }
        }

        let Some(target) = entry else {
            self.warn(&format!(
                "Edit dropped: session {} is not managed by this daemon",
                short_session(&target_id)
            ));
            return;
        };
        self.deliver_edit_user_message(request, target, relation)
            .await;
    }

    fn queue_edit_user_message_after_attach(
        &self,
        lookup_id: String,
        request: EditUserMessageRequest,
    ) {
        let supervisor = self.clone();
        tokio::spawn(async move {
            let (target_id, entry, relation) = supervisor
                .wait_for_edit_route_target(&lookup_id, Some(&request.requested_id))
                .await;
            let Some(target) = entry else {
                supervisor.warn(&format!(
                    "Edit dropped: session {} was not routable after attach",
                    short_session(&target_id)
                ));
                return;
            };
            supervisor
                .deliver_edit_user_message(request, target, relation)
                .await;
        });
    }

    async fn wait_for_edit_route_target(
        &self,
        primary_id: &str,
        fallback_id: Option<&str>,
    ) -> (String, Option<EditRouteTarget>, Option<RelatedSession>) {
        let started_at = std::time::Instant::now();
        loop {
            let primary = self.lookup_edit_route_target(primary_id).await;
            if primary.1.is_some() {
                return primary;
            }

            if let Some(fallback_id) = fallback_id.filter(|id| *id != primary_id) {
                let fallback = self.lookup_edit_route_target(fallback_id).await;
                if fallback.1.is_some() {
                    return fallback;
                }
            }

            if started_at.elapsed() >= EDIT_ATTACH_ROUTE_TIMEOUT {
                return primary;
            }
            tokio::time::sleep(EDIT_ATTACH_ROUTE_POLL_INTERVAL).await;
        }
    }

    async fn deliver_edit_user_message(
        &self,
        request: EditUserMessageRequest,
        target: EditRouteTarget,
        relation: Option<RelatedSession>,
    ) {
        let Some(backend) = external_agent::AgentBackend::from_str_loose(&target.source) else {
            self.warn(&format!(
                "Edit dropped: unknown external-agent source {} for session {}",
                target.source,
                short_session(&target.managed_id)
            ));
            return;
        };
        if !backend.supports_user_message_rewind() {
            self.warn(&format!(
                "Edit dropped: {} session {} does not support user-message rewind yet",
                backend,
                short_session(&target.managed_id)
            ));
            return;
        }
        if request.user_turn_index == 0 {
            self.warn(&format!(
                "Edit dropped: invalid user turn index 0 for {} session {}",
                backend,
                short_session(&target.managed_id)
            ));
            return;
        }
        let Some(user_turn_revision) = request.user_turn_revision else {
            self.warn(&format!(
                "Edit dropped: missing active-message revision for {} session {} user turn {}",
                backend,
                short_session(&target.managed_id),
                request.user_turn_index
            ));
            return;
        };

        let resolved_attachments = self
            .resolve_session_attachments(
                &request.attachments,
                &target.session_dir,
                &target.project_root,
            )
            .await;
        if resolved_attachments.len() < request.attachments.len() {
            self.warn(&format!(
                "Only resolved {} of {} edit attachment(s) for {} session {}",
                resolved_attachments.len(),
                request.attachments.len(),
                backend,
                short_session(&target.managed_id)
            ));
        }
        let target_session_id = relation
            .as_ref()
            .filter(|rel| matches!(rel.relationship.as_str(), "side" | "subagent"))
            .map(|_| request.requested_id.clone());
        let msg = FollowUpMessage::edit_user_message(
            request.text,
            UserAttachments::from_items(resolved_attachments),
            request.user_turn_index,
            user_turn_revision,
            request.original_text,
            request.attachments,
        )
        .for_target(target_session_id);
        if target.follow_up_tx.send(msg).await.is_err() {
            self.warn(&format!(
                "Edit dropped: {} session {} in {} is not accepting input",
                backend,
                short_session(&target.managed_id),
                target.project_root.display()
            ));
        }
    }

    async fn lookup_edit_route_target(
        &self,
        requested_id: &str,
    ) -> (String, Option<EditRouteTarget>, Option<RelatedSession>) {
        let state = self.state.lock().await;
        let relation = state.related_sessions.get(requested_id).cloned();
        let target_id = state
            .resolve_session_id(requested_id)
            .unwrap_or_else(|| requested_id.to_string());
        let entry = state.sessions.get(&target_id).map(|s| EditRouteTarget {
            managed_id: s.session_id.clone(),
            source: s.source.clone(),
            project_root: s.project_root.clone(),
            session_dir: s.session_dir.clone(),
            follow_up_tx: s.follow_up_tx.clone(),
        });
        (target_id, entry, relation)
    }

    async fn route_interrupt(&self, session_id: Option<String>) {
        let requested_id = session_id.clone();
        let Some(target_id) = self.resolve_target_session_id(session_id).await else {
            self.warn("Interrupt dropped: no active managed session");
            return;
        };
        if let Some(requested_id) = requested_id.as_deref() {
            let state = self.state.lock().await;
            if state
                .related_sessions
                .get(requested_id)
                .is_some_and(|rel| rel.relationship == "subagent")
            {
                drop(state);
                self.warn(&format!(
                    "Interrupt dropped: Codex subagent session {} does not support interrupts",
                    short_session(requested_id)
                ));
                return;
            }
        }
        if !self.session_is_managed(&target_id).await {
            self.warn(&format!(
                "Interrupt dropped: session {} is not managed by this daemon",
                short_session(&target_id)
            ));
            return;
        }
        self.config.bus.send(AppEvent::InterruptRequested {
            session_id: requested_id.or(Some(target_id)),
        });
    }

    async fn stop_managed_session(
        &self,
        session_id: Option<String>,
        reason: &str,
    ) -> Option<StoppedManagedSession> {
        let reason = reason.trim();
        let reason = if reason.is_empty() {
            "stopped by user"
        } else {
            reason
        };
        let removed = {
            let mut state = self.state.lock().await;
            let requested_id = session_id
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_string)
                .or_else(|| state.active_session_id.clone());
            let Some(requested_id) = requested_id else {
                drop(state);
                self.warn("Stop session dropped: no active managed session");
                return None;
            };
            if state.related_sessions.contains_key(&requested_id) {
                drop(state);
                self.warn(&format!(
                    "Stop session dropped: {} is a related Codex thread; stop the parent session instead",
                    short_session(&requested_id)
                ));
                return None;
            }
            let Some(target_id) = state.resolve_session_id(&requested_id) else {
                drop(state);
                self.warn(&format!(
                    "Stop session dropped: session {} is not managed by this daemon",
                    short_session(&requested_id)
                ));
                return None;
            };
            state.remove_session(&target_id)
        };

        let Some((canonical, session)) = removed else {
            self.warn("Stop session dropped: no matching managed session");
            return None;
        };
        self.config.bus.send(AppEvent::SessionStopRequested {
            session_id: Some(canonical.clone()),
            reason: reason.to_string(),
        });
        self.config.bus.send(AppEvent::SessionEnded {
            session_id: canonical.clone(),
            reason: reason.to_string(),
        });
        Some(StoppedManagedSession {
            session_id: canonical,
            source: session.source,
            finished_rx: session.finished_rx,
        })
    }

    async fn wait_for_stopped_session(&self, mut stopped: StoppedManagedSession) {
        let Some(finished_rx) = stopped.finished_rx.take() else {
            return;
        };
        match tokio::time::timeout(SESSION_STOP_WAIT_TIMEOUT, finished_rx).await {
            Ok(Ok(())) | Ok(Err(_)) => {}
            Err(_) => {
                self.warn(&format!(
                    "Restarting {} session {} before the previous backend confirmed shutdown",
                    stopped.source,
                    short_session(&stopped.session_id)
                ));
            }
        }
    }

    async fn restart_session(
        &self,
        source: String,
        session_id: String,
        resume_id: Option<String>,
        project_root: Option<String>,
        task: Option<String>,
        direct: Option<bool>,
        attachments: Vec<String>,
        agent_command: Option<String>,
        codex_sandbox: Option<String>,
        codex_approval_policy: Option<String>,
        codex_managed_context: Option<String>,
        codex_context_archive: Option<String>,
    ) {
        let source_norm = source.trim().to_lowercase();
        if source_norm == "intendant" {
            self.warn("Restart with saved config is only available for external-agent sessions");
            return;
        }
        if external_agent::AgentBackend::from_str_loose(&source_norm).is_none() {
            self.loop_error(format!("Unsupported session source: {}", source));
            return;
        }
        let resume_token = resume_id.clone().unwrap_or_else(|| session_id.clone());
        let restart_key = format!("{}:{}", source_norm, resume_token);
        {
            let mut state = self.state.lock().await;
            if !state.mark_restart_requested(&restart_key) {
                drop(state);
                self.warn(&format!(
                    "Restart session ignored: {} was already restarted recently",
                    short_session(&resume_token)
                ));
                return;
            }
        }
        if let Some(existing_id) = self
            .find_managed_session_id(&source_norm, &session_id, &resume_token)
            .await
        {
            if let Some(stopped) = self
                .stop_managed_session(Some(existing_id), "restarting session")
                .await
            {
                self.wait_for_stopped_session(stopped).await;
            }
        }
        self.resume_session(
            source_norm,
            session_id,
            resume_id,
            project_root,
            task,
            direct,
            attachments,
            agent_command,
            codex_sandbox,
            codex_approval_policy,
            codex_managed_context,
            codex_context_archive,
            true,
        )
        .await;
    }

    async fn route_steer(
        &self,
        session_id: Option<String>,
        text: String,
        id: Option<String>,
        attachments: Vec<String>,
    ) {
        let requested_id = session_id.clone();
        let Some(target_id) = self.resolve_target_session_id(session_id).await else {
            self.warn("Steer dropped: no active managed session");
            return;
        };
        let entry = {
            let state = self.state.lock().await;
            let target_id = state
                .resolve_session_id(&target_id)
                .unwrap_or_else(|| target_id.clone());
            state.sessions.get(&target_id).map(|s| {
                let relation = requested_id
                    .as_deref()
                    .and_then(|id| state.related_sessions.get(id))
                    .cloned();
                (
                    s.session_id.clone(),
                    s.source.clone(),
                    s.project_root.clone(),
                    s.session_dir.clone(),
                    s.follow_up_tx.clone(),
                    relation,
                )
            })
        };
        let Some((managed_id, source, project_root, session_dir, tx, relation)) = entry else {
            self.warn(&format!(
                "Steer dropped: session {} is not managed by this daemon",
                short_session(&target_id)
            ));
            return;
        };
        if relation
            .as_ref()
            .is_some_and(|rel| rel.relationship == "subagent")
        {
            self.warn(&format!(
                "Steer dropped: Codex subagent session {} does not support mid-turn steering; send a follow-up instead",
                short_session(requested_id.as_deref().unwrap_or(&managed_id))
            ));
            return;
        }

        let steer_id = id.unwrap_or_default();
        let event_session_id = requested_id.clone().or(Some(managed_id.clone()));
        if let Some(parsed) = parse_codex_slash_command(&text) {
            match parsed {
                Ok(command) => {
                    if source == "codex" {
                        if relation
                            .as_ref()
                            .is_some_and(|rel| rel.relationship == "side")
                        {
                            self.warn(&format!(
                                "Slash command /{} is not supported for Codex side session {}; use the parent thread instead",
                                command.op,
                                short_session(requested_id.as_deref().unwrap_or(&managed_id))
                            ));
                            return;
                        }
                        if !attachments.is_empty() {
                            self.warn(&format!(
                                "Slash command /{} for Codex session {} ignored {} steer attachment(s)",
                                command.op,
                                short_session(&managed_id),
                                attachments.len()
                            ));
                        }
                        self.config.bus.send(AppEvent::ControlCommand(
                            event::ControlMsg::CodexThreadAction {
                                session_id: Some(managed_id),
                                op: command.op,
                                params: command.params,
                            },
                        ));
                        if !steer_id.trim().is_empty() {
                            self.config.bus.send(AppEvent::SteerDelivered {
                                session_id: event_session_id,
                                id: steer_id,
                                mid_turn: false,
                            });
                        }
                    } else {
                        self.warn(&format!(
                            "Slash command /{} is only supported for Codex sessions; target {} session {}",
                            command.op,
                            source,
                            short_session(&managed_id)
                        ));
                    }
                }
                Err(message) => self.warn(&message),
            }
            return;
        }
        if attachments.is_empty() {
            let ack_rx = self.config.bus.subscribe();
            self.config.bus.send(AppEvent::SteerRequested {
                session_id: event_session_id.clone(),
                text: text.clone(),
                id: steer_id.clone(),
            });
            if !steer_id.trim().is_empty() {
                spawn_text_steer_fallback(
                    self.config.bus.clone(),
                    ack_rx,
                    tx,
                    text,
                    steer_id,
                    event_session_id,
                );
            }
            return;
        }

        let resolved_attachments = self
            .resolve_session_attachments(&attachments, &session_dir, &project_root)
            .await;
        if resolved_attachments.len() < attachments.len() {
            self.warn(&format!(
                "Only resolved {} of {} steer attachment(s) for {} session {}",
                resolved_attachments.len(),
                attachments.len(),
                source,
                short_session(&managed_id)
            ));
        }
        let msg = FollowUpMessage::steer(
            text,
            UserAttachments::from_items(resolved_attachments),
            steer_id.clone(),
        )
        .for_target(requested_id.clone().or(Some(managed_id.clone())));
        if tx.send(msg).await.is_err() {
            self.warn(&format!(
                "Steer dropped: {} session {} in {} is not accepting input",
                source,
                short_session(&managed_id),
                project_root.display()
            ));
            return;
        }
        self.config.bus.send(AppEvent::SteerQueued {
            session_id: requested_id.or(Some(managed_id)),
            id: steer_id,
            reason: "attachments are queued for the next turn".to_string(),
        });
    }

    async fn route_cancel_steer(
        &self,
        session_id: Option<String>,
        id: Option<String>,
        reason: Option<String>,
    ) {
        let requested_id = session_id.clone();
        let event_session_id =
            if let Some(target_id) = self.resolve_target_session_id(session_id).await {
                let state = self.state.lock().await;
                let managed_id = state.resolve_session_id(&target_id).unwrap_or(target_id);
                requested_id.or(Some(managed_id))
            } else {
                requested_id
            };
        self.config.bus.send(AppEvent::SteerCancelRequested {
            session_id: event_session_id,
            id,
            reason: reason.unwrap_or_else(|| "cleared by user".to_string()),
        });
    }

    async fn route_cancel_follow_up(
        &self,
        session_id: Option<String>,
        id: Option<String>,
        reason: Option<String>,
    ) {
        let requested_id = session_id.clone();
        let event_session_id =
            if let Some(target_id) = self.resolve_target_session_id(session_id).await {
                let state = self.state.lock().await;
                let managed_id = state.resolve_session_id(&target_id).unwrap_or(target_id);
                requested_id.or(Some(managed_id))
            } else {
                requested_id
            };
        let reason = reason.unwrap_or_else(|| "cleared by user".to_string());
        self.config.bus.send(AppEvent::FollowUpCancelRequested {
            session_id: event_session_id.clone(),
            id: id.clone(),
            reason: reason.clone(),
        });
        emit_follow_up_status(
            &self.config.bus,
            event_session_id,
            &id,
            None,
            "cancelled",
            Some(&reason),
        );
    }

    async fn resolve_approval(
        &self,
        session_id: Option<String>,
        approval_id: u64,
        response: event::ApprovalResponse,
        action: &str,
    ) {
        let Some(target_id) = self.resolve_target_session_id(session_id).await else {
            self.warn("Approval response dropped: no active managed session");
            return;
        };
        let registry = {
            let state = self.state.lock().await;
            let target_id = state
                .resolve_session_id(&target_id)
                .unwrap_or_else(|| target_id.clone());
            state
                .sessions
                .get(&target_id)
                .map(|session| session.approval_registry.clone())
        };
        let Some(registry) = registry else {
            self.warn(&format!(
                "Approval response dropped: session {} is not managed by this daemon",
                short_session(&target_id)
            ));
            return;
        };
        let responder = registry.lock().unwrap().remove(&approval_id);
        match responder {
            Some(tx) => {
                let _ = tx.send(response);
                self.config.bus.send(AppEvent::ApprovalResolved {
                    session_id: Some(target_id),
                    id: approval_id,
                    action: action.to_string(),
                });
            }
            None => {
                self.warn(&format!(
                    "Approval response dropped: id {} is not pending for session {}",
                    approval_id,
                    short_session(&target_id)
                ));
            }
        }
    }

    async fn rename_session(
        &self,
        session_id: String,
        backend_session_id: Option<String>,
        source: Option<String>,
        name: String,
    ) {
        let managed = {
            let state = self.state.lock().await;
            let resolved_id = state
                .resolve_session_id(&session_id)
                .unwrap_or_else(|| session_id.clone());
            state
                .sessions
                .get(&resolved_id)
                .map(|session| (session.session_id.clone(), session.source.clone()))
        };

        if let Some((managed_id, managed_source)) = managed.as_ref() {
            if managed_source == "codex" {
                self.config.bus.send(AppEvent::ControlCommand(
                    event::ControlMsg::CodexThreadAction {
                        session_id: Some(managed_id.clone()),
                        op: "rename".to_string(),
                        params: serde_json::json!({ "name": name }),
                    },
                ));
                return;
            }
        }

        let source = managed
            .map(|(_, source)| source)
            .or(source)
            .unwrap_or_else(|| "intendant".to_string());
        let normalized_source = crate::session_names::normalize_source(&source);
        let persistence_session_id = if normalized_source == "intendant" {
            session_id.as_str()
        } else {
            backend_session_id.as_deref().unwrap_or(&session_id)
        };
        let result = match dirs::home_dir() {
            Some(home) => crate::session_names::rename_session(
                &home,
                &normalized_source,
                persistence_session_id,
                &name,
            ),
            None => Err("could not resolve home directory".to_string()),
        };

        match result {
            Ok(name) => {
                self.config.bus.send(AppEvent::SessionRenameResult {
                    session_id,
                    source: Some(normalized_source),
                    name: Some(name.clone()),
                    success: true,
                    message: format!("Renamed session to {}", name),
                });
            }
            Err(message) => {
                self.config.bus.send(AppEvent::SessionRenameResult {
                    session_id,
                    source: Some(normalized_source),
                    name: None,
                    success: false,
                    message,
                });
            }
        }
    }

    async fn configure_session_agent(
        &self,
        session_id: String,
        source: Option<String>,
        backend_session_id: Option<String>,
        intendant_session_id: Option<String>,
        agent_command: Option<String>,
        codex_sandbox: Option<String>,
        codex_approval_policy: Option<String>,
        codex_managed_context: Option<String>,
        codex_context_archive: Option<String>,
    ) {
        let managed = {
            let state = self.state.lock().await;
            state
                .resolve_session_id(&session_id)
                .and_then(|resolved_id| state.sessions.get(&resolved_id))
                .map(|session| {
                    (
                        session.session_id.clone(),
                        session.source.clone(),
                        session.session_dir.clone(),
                    )
                })
        };

        let normalized_source = managed
            .as_ref()
            .map(|(_, source, _)| source.clone())
            .or(source)
            .map(|source| crate::session_names::normalize_source(&source))
            .unwrap_or_default();
        let Some(backend) = external_agent::AgentBackend::from_str_loose(&normalized_source) else {
            let message = "Session config failed: choose an external agent session".to_string();
            self.config.bus.send(AppEvent::SessionAgentConfigResult {
                session_id,
                source: normalized_source,
                backend_session_id,
                intendant_session_id,
                persisted_session_ids: Vec::new(),
                success: false,
                message: message.clone(),
            });
            self.loop_error(message);
            return;
        };
        let clear_codex_sandbox = matches!(backend, external_agent::AgentBackend::Codex)
            && session_config_clear_value(codex_sandbox.as_deref());
        let clear_codex_approval_policy = matches!(backend, external_agent::AgentBackend::Codex)
            && session_config_clear_value(codex_approval_policy.as_deref());
        let mut config = crate::session_config::from_wire(
            Some(backend.as_short_str()),
            agent_command.as_deref(),
            codex_sandbox.as_deref(),
            codex_approval_policy.as_deref(),
            codex_managed_context.as_deref(),
            codex_context_archive.as_deref(),
            None,
        );
        let home = crate::platform::home_dir();
        if let Some(existing) = crate::session_config::load_for_resume(
            &home,
            backend.as_short_str(),
            &session_id,
            backend_session_id.as_deref(),
        ) {
            config.merge_missing_from(existing);
        }
        if let Some((_, _, session_dir)) = managed.as_ref() {
            if let Some(existing) = crate::session_config::read_log_dir_config(session_dir) {
                config.merge_missing_from(existing);
            }
        }
        if let Some(intendant_id) = intendant_session_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
        {
            if let Some(dir) = session_log::SessionLog::find_session_by_id(intendant_id) {
                if let Some(existing) = crate::session_config::read_log_dir_config(&dir) {
                    config.merge_missing_from(existing);
                }
            }
        }
        if clear_codex_sandbox {
            config.codex_sandbox = None;
        }
        if clear_codex_approval_policy {
            config.codex_approval_policy = None;
        }
        if config.is_empty() {
            let message = "Session config failed: no launch settings supplied".to_string();
            self.config.bus.send(AppEvent::SessionAgentConfigResult {
                session_id,
                source: backend.as_short_str().to_string(),
                backend_session_id,
                intendant_session_id,
                persisted_session_ids: Vec::new(),
                success: false,
                message: message.clone(),
            });
            self.loop_error(message);
            return;
        }

        let mut errors = Vec::new();
        let mut persisted_session_ids = Vec::new();
        let mut note_persisted = |id: &str| {
            let id = id.trim();
            if !id.is_empty() && !persisted_session_ids.iter().any(|existing| existing == id) {
                persisted_session_ids.push(id.to_string());
            }
        };
        if let Some((managed_id, _, session_dir)) = managed.as_ref() {
            if let Err(e) = crate::session_config::write_log_dir_config(session_dir, &config) {
                errors.push(e);
            } else {
                note_persisted(managed_id);
            }
        }
        let intendant_id = intendant_session_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty());
        if let Some(intendant_id) = intendant_id {
            if let Some(dir) = session_log::SessionLog::find_session_by_id(intendant_id) {
                if let Err(e) = crate::session_config::write_log_dir_config(&dir, &config) {
                    errors.push(e);
                } else {
                    note_persisted(intendant_id);
                }
            }
        }

        let external_ids = [
            backend_session_id.as_deref(),
            Some(session_id.as_str()),
            managed
                .as_ref()
                .map(|(managed_id, _, _)| managed_id.as_str()),
        ];
        let mut wrote_external = false;
        for external_id in external_ids
            .into_iter()
            .flatten()
            .map(str::trim)
            .filter(|id| !id.is_empty())
        {
            if !external_agent::source_session_id_is_canonical(backend.as_short_str(), external_id)
            {
                continue;
            }
            wrote_external = true;
            if let Err(e) = crate::session_config::replace_external_overlay(
                &home,
                backend.as_short_str(),
                external_id,
                &config,
            ) {
                errors.push(e);
            } else {
                note_persisted(external_id);
            }
        }

        if !wrote_external && managed.is_none() && intendant_id.is_none() {
            let message = "Session config failed: no persistable session id".to_string();
            self.config.bus.send(AppEvent::SessionAgentConfigResult {
                session_id,
                source: backend.as_short_str().to_string(),
                backend_session_id,
                intendant_session_id,
                persisted_session_ids,
                success: false,
                message: message.clone(),
            });
            self.loop_error(message);
            return;
        }
        if errors.is_empty() {
            let message = format!(
                "Session {} launch config saved for {} (takes effect on next attach/resume)",
                short_session(&session_id),
                backend.as_short_str()
            );
            self.info(&message);
            self.config.bus.send(AppEvent::SessionAgentConfigResult {
                session_id,
                source: backend.as_short_str().to_string(),
                backend_session_id,
                intendant_session_id,
                persisted_session_ids,
                success: true,
                message,
            });
        } else {
            let message = format!("Session config partially failed: {}", errors.join("; "));
            self.config.bus.send(AppEvent::SessionAgentConfigResult {
                session_id,
                source: backend.as_short_str().to_string(),
                backend_session_id,
                intendant_session_id,
                persisted_session_ids,
                success: false,
                message: message.clone(),
            });
            self.loop_error(message);
        }
    }

    async fn apply_session_identity(
        &self,
        session_id: String,
        source: String,
        backend_session_id: String,
    ) {
        let source = crate::session_names::normalize_source(&source);
        if !external_agent::source_session_id_is_canonical(&source, &backend_session_id) {
            return;
        }
        if session_id == backend_session_id {
            return;
        }

        let name_to_persist = {
            let mut state = self.state.lock().await;
            let Some(current_key) = state.resolve_session_id(&session_id) else {
                return;
            };
            if current_key == backend_session_id {
                state
                    .session_aliases
                    .insert(session_id, backend_session_id.clone());
                state
                    .sessions
                    .get(&backend_session_id)
                    .and_then(|session| session.name.clone())
            } else if state.sessions.contains_key(&backend_session_id) {
                let name = state
                    .sessions
                    .get(&backend_session_id)
                    .and_then(|session| session.name.clone())
                    .or_else(|| {
                        state
                            .sessions
                            .get(&current_key)
                            .and_then(|session| session.name.clone())
                    });
                state
                    .session_aliases
                    .insert(session_id.clone(), backend_session_id.clone());
                state
                    .session_aliases
                    .insert(current_key, backend_session_id.clone());
                if state.active_session_id.as_deref() == Some(&session_id) {
                    state.active_session_id = Some(backend_session_id.clone());
                }
                name
            } else {
                let Some(mut session) = state.sessions.remove(&current_key) else {
                    return;
                };
                let name = session.name.clone();
                session.session_id = backend_session_id.clone();
                session.source = source.clone();
                state.sessions.insert(backend_session_id.clone(), session);
                state
                    .session_aliases
                    .insert(session_id.clone(), backend_session_id.clone());
                state
                    .session_aliases
                    .insert(current_key.clone(), backend_session_id.clone());
                if state.active_session_id.as_deref() == Some(&session_id)
                    || state.active_session_id.as_deref() == Some(&current_key)
                {
                    state.active_session_id = Some(backend_session_id.clone());
                }
                name
            }
        };

        if let Some(name) = name_to_persist {
            persist_external_session_name(&self.config.bus, &source, &backend_session_id, &name);
        }
    }

    async fn observe_lifecycle_event(&self, event: &AppEvent) {
        match event {
            AppEvent::SessionStarted { session_id, .. } => {
                self.update_session_phase(Some(session_id), "thinking")
                    .await;
            }
            AppEvent::TurnStarted { session_id, .. } => {
                self.update_session_phase(session_id.as_deref(), "thinking")
                    .await;
            }
            AppEvent::AgentStarted { session_id, .. }
            | AppEvent::AgentOutput { session_id, .. } => {
                self.update_session_phase(session_id.as_deref(), "running")
                    .await;
            }
            AppEvent::ApprovalRequired { session_id, .. } => {
                self.update_session_phase(session_id.as_deref(), "waiting_approval")
                    .await;
            }
            AppEvent::HumanQuestionDetected { .. } => {
                self.update_session_phase(None, "waiting_human").await;
            }
            AppEvent::InterruptRequested { session_id } => {
                self.update_session_phase(session_id.as_deref(), "interrupting")
                    .await;
            }
            AppEvent::Interrupted { session_id, .. } => {
                self.update_session_phase(session_id.as_deref(), "interrupted")
                    .await;
            }
            AppEvent::RoundComplete { session_id, .. } => {
                self.update_session_phase(session_id.as_deref(), "idle")
                    .await;
            }
            AppEvent::TaskComplete { session_id, .. } => {
                self.update_session_phase(session_id.as_deref(), "done")
                    .await;
            }
            AppEvent::StatusUpdate {
                session_id, phase, ..
            } => {
                self.update_session_phase(Some(session_id), phase).await;
            }
            _ => {}
        }
    }

    async fn apply_session_relationship(
        &self,
        parent_session_id: String,
        child_session_id: String,
        relationship: String,
    ) {
        let mut state = self.state.lock().await;
        state.apply_related_session(&parent_session_id, &child_session_id, &relationship);
    }

    async fn remove_session_alias(&self, session_id: &str) {
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return;
        }
        let mut state = self.state.lock().await;
        state.session_aliases.remove(session_id);
        state.related_sessions.remove(session_id);
    }

    async fn update_session_phase(&self, session_id: Option<&str>, phase: &str) {
        let phase = normalize_supervisor_phase(phase);
        let mut state = self.state.lock().await;
        let target_id = session_id
            .and_then(|id| state.resolve_session_id(id))
            .or_else(|| state.active_session_id.clone());
        let Some(target_id) = target_id else {
            return;
        };
        if let Some(session) = state.sessions.get_mut(&target_id) {
            session.phase = phase;
        }
    }

    async fn resolve_target_session_id(&self, session_id: Option<String>) -> Option<String> {
        let state = self.state.lock().await;
        let requested = session_id.or_else(|| state.active_session_id.clone())?;
        Some(state.resolve_session_id(&requested).unwrap_or(requested))
    }

    async fn session_is_managed(&self, session_id: &str) -> bool {
        let state = self.state.lock().await;
        state.session_is_managed(session_id)
    }

    async fn clear_external_attach_request(&self, keys: &[String]) {
        if keys.is_empty() {
            return;
        }
        let mut state = self.state.lock().await;
        state.clear_external_attach_requested(keys);
    }

    async fn register_session(
        &self,
        session_id: String,
        source: String,
        phase: String,
        project_root: PathBuf,
        session_dir: PathBuf,
        follow_up_tx: mpsc::Sender<FollowUpMessage>,
        approval_registry: event::ApprovalRegistry,
        name: Option<String>,
        finished_rx: Option<oneshot::Receiver<()>>,
    ) -> u64 {
        let rehydrated_related = load_related_sessions_from_log(&session_dir);
        let mut state = self.state.lock().await;
        state.next_session_instance = state.next_session_instance.saturating_add(1);
        let instance_id = state.next_session_instance;
        state.active_session_id = Some(session_id.clone());
        state.session_aliases.remove(&session_id);
        state.sessions.insert(
            session_id.clone(),
            ManagedSession {
                session_id,
                source,
                name,
                phase,
                project_root,
                session_dir,
                follow_up_tx,
                approval_registry,
                instance_id,
                finished_rx,
            },
        );
        for rel in rehydrated_related {
            state.apply_related_session(
                &rel.parent_session_id,
                &rel.child_session_id,
                &rel.relationship,
            );
        }
        instance_id
    }

    async fn finish_session(
        &self,
        session_id: String,
        session_instance_id: u64,
        session_log: SharedSessionLog,
        task: String,
        result: Result<LoopStats, CallerError>,
    ) {
        let reason = match &result {
            Ok(stats) => {
                let outcome = stats.terminal_outcome.as_deref().unwrap_or("completed");
                slog(&session_log, |log| {
                    log.write_summary_with_rounds(&task, outcome, stats.turns, Some(stats.rounds));
                });
                outcome.to_string()
            }
            Err(e) => {
                slog(&session_log, |log| {
                    log.write_summary(&task, &format!("error: {}", e), 0);
                });
                format!("error: {}", e)
            }
        };

        let ended_session_id = {
            let mut state = self.state.lock().await;
            if session_instance_id == 0 {
                Some(
                    state
                        .remove_session(&session_id)
                        .map(|(canonical, _)| canonical)
                        .unwrap_or_else(|| session_id.clone()),
                )
            } else {
                state
                    .remove_session_instance(&session_id, session_instance_id)
                    .map(|(canonical, _)| canonical)
            }
        };

        if let Some(ended_session_id) = ended_session_id.clone() {
            self.config.bus.send(AppEvent::SessionEnded {
                session_id: ended_session_id.clone(),
                reason,
            });
        }

        if let Some(ref shared_session) = self.config.shared_session {
            let mut state = shared_session.write().await;
            let matches_current = state
                .session_log
                .as_ref()
                .map(|log| {
                    let log_session_id = log.lock().ok().map(|log| log.session_id().to_string());
                    Arc::ptr_eq(log, &session_log)
                        || log_session_id.as_deref() == Some(&session_id)
                        || ended_session_id
                            .as_deref()
                            .is_some_and(|id| log_session_id.as_deref() == Some(id))
                })
                .unwrap_or(false);
            if matches_current {
                state.session_log = None;
                state.query_ctx = None;
            }
        }
    }

    async fn activate_shared_session(&self, session_log: SharedSessionLog) {
        if let Some(ref shared_session) = self.config.shared_session {
            let mut state = shared_session.write().await;
            state.session_log = Some(session_log);
        }
    }

    async fn project_with_runtime_config(
        &self,
        root: PathBuf,
        backend: Option<&external_agent::AgentBackend>,
    ) -> Result<Project, CallerError> {
        let mut project = Project::from_root(root)?;
        match backend {
            Some(external_agent::AgentBackend::Codex) => {
                let current = self.config.shared_codex_config.read().await.clone();
                let cfg = &mut project.config.agent.codex;
                cfg.command = current.command;
                cfg.sandbox = current.sandbox;
                cfg.approval_policy = current.approval_policy;
                cfg.model = current.model;
                cfg.reasoning_effort = current.reasoning_effort;
                cfg.service_tier = current.service_tier;
                cfg.web_search = current.web_search;
                cfg.network_access = current.network_access;
                cfg.writable_roots = current.writable_roots;
                cfg.managed_context = current.managed_context;
                cfg.context_archive = current.context_archive;
            }
            Some(external_agent::AgentBackend::GeminiCli) => {
                let current = self.config.shared_gemini_config.read().await.clone();
                let cfg = &mut project.config.agent.gemini_cli;
                cfg.model = current.model;
                cfg.approval_mode = current.approval_mode;
                cfg.sandbox = current.sandbox;
                cfg.extensions = current.extensions;
                cfg.allowed_mcp_servers = current.allowed_mcp_servers;
                cfg.include_directories = current.include_directories;
                cfg.debug = current.debug;
            }
            Some(external_agent::AgentBackend::ClaudeCode) | None => {}
        }
        Ok(project)
    }

    fn loop_error(&self, message: String) {
        self.config.bus.send(AppEvent::LoopError(message));
    }

    fn warn(&self, message: &str) {
        self.config.bus.send(AppEvent::LogEntry {
            session_id: None,
            level: "warn".to_string(),
            source: "session-supervisor".to_string(),
            content: message.to_string(),
            turn: None,
        });
    }

    fn info(&self, message: &str) {
        self.config.bus.send(AppEvent::LogEntry {
            session_id: None,
            level: "info".to_string(),
            source: "session-supervisor".to_string(),
            content: message.to_string(),
            turn: None,
        });
    }

    async fn emit_attached_status(&self, session_id: &str, source: &str) {
        let autonomy = self.config.autonomy.read().await.level.to_string();
        let phase = {
            let state = self.state.lock().await;
            state
                .resolve_session_id(session_id)
                .and_then(|id| state.sessions.get(&id).map(|session| session.phase.clone()))
                .unwrap_or_else(|| "idle".to_string())
        };
        self.config.bus.send(AppEvent::StatusUpdate {
            turn: 0,
            phase,
            autonomy,
            session_id: session_id.to_string(),
            task: format!("Open {} session {}", source, short_session(session_id)),
        });
    }
}

fn normalize_supervisor_phase(phase: &str) -> String {
    match phase.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "" => "idle".to_string(),
        "running_agent" => "running".to_string(),
        "waiting_follow_up" | "waiting_followup" => "idle".to_string(),
        other => other.to_string(),
    }
}

fn path_file_name(path: &std::path::Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("unknown")
        .to_string()
}

fn resolve_project_root_override(
    project_root: Option<String>,
    default_root: &Path,
) -> Result<PathBuf, String> {
    let Some(raw) = project_root else {
        return Ok(default_root.to_path_buf());
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(default_root.to_path_buf());
    }
    let path = if trimmed == "~" {
        dirs::home_dir().ok_or_else(|| "could not resolve home directory".to_string())?
    } else if let Some(rest) = trimmed.strip_prefix("~/") {
        dirs::home_dir()
            .ok_or_else(|| "could not resolve home directory".to_string())?
            .join(rest)
    } else {
        PathBuf::from(trimmed)
    };
    if !path.is_absolute() {
        return Err(format!(
            "project directory must be absolute or start with ~/ (got {})",
            trimmed
        ));
    }
    let canonical = std::fs::canonicalize(&path)
        .map_err(|e| format!("{} is not accessible: {}", path.display(), e))?;
    if !canonical.is_dir() {
        return Err(format!("{} is not a directory", canonical.display()));
    }
    Ok(canonical)
}

fn resolve_external_resume_project_root(
    project_root: Option<String>,
    config: Option<&crate::session_config::SessionAgentConfig>,
    default_root: &Path,
) -> Result<PathBuf, String> {
    if let Some(root) = project_root
        .as_deref()
        .and_then(|root| crate::session_config::normalize_project_root(Some(root)))
    {
        return Ok(PathBuf::from(root));
    }
    if let Some(root) = config
        .and_then(|config| config.project_root.as_deref())
        .and_then(|root| crate::session_config::normalize_project_root(Some(root)))
    {
        return resolve_project_root_override(Some(root), default_root);
    }
    Ok(default_root.to_path_buf())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionAgentSelection {
    Configured,
    Internal,
    External(external_agent::AgentBackend),
}

impl SessionAgentSelection {
    fn from_wire(agent: Option<&str>) -> Result<Self, String> {
        let Some(agent) = agent else {
            return Ok(Self::Configured);
        };
        let trimmed = agent.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("configured") {
            return Ok(Self::Configured);
        }
        let lowered = trimmed.to_ascii_lowercase();
        if matches!(
            lowered.as_str(),
            "internal" | "intendant" | "native" | "none"
        ) {
            return Ok(Self::Internal);
        }
        external_agent::AgentBackend::from_str_loose(trimmed)
            .map(Self::External)
            .ok_or_else(|| {
                format!(
                    "unknown agent '{}' (expected internal, codex, claude-code, or gemini)",
                    trimmed
                )
            })
    }
}

fn codex_fast_new_session_agent(agent: Option<&str>) -> Result<String, String> {
    match SessionAgentSelection::from_wire(agent)? {
        SessionAgentSelection::Configured => Ok("codex".to_string()),
        SessionAgentSelection::External(external_agent::AgentBackend::Codex) => {
            Ok("codex".to_string())
        }
        SessionAgentSelection::Internal => {
            Err("/fast can only start a new Codex external-agent session".to_string())
        }
        SessionAgentSelection::External(other) => Err(format!(
            "/fast can only start a new Codex external-agent session; selected {other}"
        )),
    }
}

fn normalize_session_agent_command(command: Option<&str>) -> Option<String> {
    command
        .map(str::trim)
        .filter(|command| !command.is_empty())
        .map(ToOwned::to_owned)
}

fn normalize_session_codex_managed_context(mode: Option<&str>) -> Option<String> {
    mode.map(crate::project::normalize_codex_managed_context)
}

fn session_config_clear_value(value: Option<&str>) -> bool {
    value
        .map(str::trim)
        .map(|value| value.is_empty() || matches!(value, "inherit" | "default" | "global"))
        .unwrap_or(false)
}

fn normalize_session_codex_sandbox(mode: Option<&str>) -> Option<String> {
    crate::session_config::normalize_codex_sandbox(mode)
}

fn normalize_session_codex_approval_policy(policy: Option<&str>) -> Option<String> {
    crate::session_config::normalize_codex_approval_policy(policy)
}

fn normalize_session_codex_context_archive(mode: Option<&str>) -> Option<String> {
    mode.map(crate::project::normalize_codex_context_archive)
}

fn normalize_session_codex_service_tier(tier: Option<&str>) -> Option<String> {
    crate::project::normalize_codex_service_tier(tier)
}

fn normalize_session_name_option(name: Option<&str>) -> Result<Option<String>, String> {
    match name.map(str::trim).filter(|name| !name.is_empty()) {
        Some(name) => crate::session_names::normalize_session_name(name).map(Some),
        None => Ok(None),
    }
}

fn apply_session_agent_command(
    project: &mut Project,
    backend: &external_agent::AgentBackend,
    command: String,
) {
    match backend {
        external_agent::AgentBackend::Codex => {
            project.config.agent.codex.command = command;
        }
        external_agent::AgentBackend::ClaudeCode => {
            project.config.agent.claude_code.command = command;
        }
        external_agent::AgentBackend::GeminiCli => {
            project.config.agent.gemini_cli.command = command;
        }
    }
}

fn apply_session_codex_managed_context(
    project: &mut Project,
    backend: &external_agent::AgentBackend,
    mode: String,
) -> Result<(), String> {
    match backend {
        external_agent::AgentBackend::Codex => {
            project.config.agent.codex.managed_context =
                crate::project::normalize_codex_managed_context(&mode);
            Ok(())
        }
        _ => Err("codex_managed_context requires Codex".to_string()),
    }
}

fn apply_session_codex_sandbox(
    project: &mut Project,
    backend: &external_agent::AgentBackend,
    mode: String,
) -> Result<(), String> {
    match backend {
        external_agent::AgentBackend::Codex => {
            project.config.agent.codex.sandbox = crate::project::normalize_sandbox_mode(&mode);
            Ok(())
        }
        _ => Err("codex_sandbox requires Codex".to_string()),
    }
}

fn apply_session_codex_approval_policy(
    project: &mut Project,
    backend: &external_agent::AgentBackend,
    policy: String,
) -> Result<(), String> {
    match backend {
        external_agent::AgentBackend::Codex => {
            project.config.agent.codex.approval_policy =
                crate::project::normalize_approval_policy(&policy);
            Ok(())
        }
        _ => Err("codex_approval_policy requires Codex".to_string()),
    }
}

fn apply_session_codex_context_archive(
    project: &mut Project,
    backend: &external_agent::AgentBackend,
    mode: String,
) -> Result<(), String> {
    match backend {
        external_agent::AgentBackend::Codex => {
            project.config.agent.codex.context_archive =
                crate::project::normalize_codex_context_archive(&mode);
            Ok(())
        }
        _ => Err("codex_context_archive requires Codex".to_string()),
    }
}

fn effective_session_agent_config_from_project(
    backend: &external_agent::AgentBackend,
    project: &Project,
    overrides: Option<&crate::session_config::SessionAgentConfig>,
) -> crate::session_config::SessionAgentConfig {
    let mut config = crate::session_config::from_project(backend, project);
    if matches!(backend, external_agent::AgentBackend::Codex) {
        if let Some(overrides) = overrides {
            if overrides.codex_service_tier.is_some() {
                config.codex_service_tier = overrides.codex_service_tier.clone();
            }
            if overrides.codex_home.is_some() {
                config.codex_home = overrides.codex_home.clone();
            }
        }
    }
    config
}

fn write_session_meta(
    session_log: &Arc<std::sync::Mutex<session_log::SessionLog>>,
    project_root: &Path,
    task: Option<&str>,
    name: Option<&str>,
) {
    if let Ok(log) = session_log.lock() {
        log.write_meta_with_name(Some(project_root), task, name);
    }
}

fn persist_external_session_name(bus: &EventBus, source: &str, session_id: &str, name: &str) {
    let source = crate::session_names::normalize_source(source);
    if source == "intendant" || name.trim().is_empty() {
        return;
    }
    let result = dirs::home_dir()
        .ok_or_else(|| "could not resolve home directory".to_string())
        .and_then(|home| crate::session_names::rename_session(&home, &source, session_id, name));
    if let Err(message) = result {
        bus.send(AppEvent::LogEntry {
            session_id: Some(session_id.to_string()),
            level: "warn".to_string(),
            source: "session-supervisor".to_string(),
            content: format!("Failed to persist session name: {}", message),
            turn: None,
        });
    }
}

#[derive(Debug, Clone, PartialEq)]
struct CodexSlashCommand {
    op: String,
    params: serde_json::Value,
}

fn parse_codex_slash_command(text: &str) -> Option<Result<CodexSlashCommand, String>> {
    let trimmed = text.trim();
    let rest = trimmed.strip_prefix('/')?;
    let mut split = rest.splitn(2, char::is_whitespace);
    let name = split.next()?.trim().to_ascii_lowercase();
    let args = split.next().unwrap_or("").trim();

    match name.as_str() {
        "fork" => {
            let mut params = serde_json::Map::new();
            let fork_name = unquote_slash_value(args);
            if !fork_name.is_empty() {
                params.insert("name".to_string(), serde_json::Value::String(fork_name));
            }
            Some(Ok(CodexSlashCommand {
                op: "fork".to_string(),
                params: serde_json::Value::Object(params),
            }))
        }
        "side" | "btw" => {
            let mut params = serde_json::Map::new();
            let prompt = unquote_slash_value(args);
            if !prompt.is_empty() {
                params.insert("prompt".to_string(), serde_json::Value::String(prompt));
            }
            Some(Ok(CodexSlashCommand {
                op: "side".to_string(),
                params: serde_json::Value::Object(params),
            }))
        }
        "fast" => {
            if !args.is_empty() {
                return Some(Err("/fast does not accept arguments".to_string()));
            }
            Some(Ok(CodexSlashCommand {
                op: "fast".to_string(),
                params: serde_json::json!({}),
            }))
        }
        "goal" => Some(parse_goal_slash_command(args)),
        _ => None,
    }
}

fn parse_goal_slash_command(args: &str) -> Result<CodexSlashCommand, String> {
    let exact = args.trim().to_ascii_lowercase();
    let exact_op = match exact.as_str() {
        "" | "status" | "show" | "get" => Some("goal"),
        "clear" | "reset" => Some("goal-clear"),
        "pause" | "paused" => Some("goal-pause"),
        "resume" | "active" => Some("goal-resume"),
        "complete" | "completed" | "done" => Some("goal-complete"),
        "budget-limited" | "budget_limited" => Some("goal-budget-limited"),
        _ => None,
    };
    if let Some(op) = exact_op {
        return Ok(CodexSlashCommand {
            op: op.to_string(),
            params: serde_json::json!({}),
        });
    }

    let mut op = "goal".to_string();
    let mut params = serde_json::Map::new();
    let mut objective_parts = Vec::new();
    let mut parts = args.split_whitespace().peekable();

    while let Some(part) = parts.next() {
        match part {
            "--clear" => {
                return Ok(CodexSlashCommand {
                    op: "goal-clear".to_string(),
                    params: serde_json::json!({}),
                });
            }
            "--pause" => op = "goal-pause".to_string(),
            "--resume" => op = "goal-resume".to_string(),
            "--complete" => op = "goal-complete".to_string(),
            "--budget-limited" => op = "goal-budget-limited".to_string(),
            "--clear-budget" | "--no-budget" => {
                params.insert("tokenBudget".to_string(), serde_json::Value::Null);
            }
            "--status" => {
                let Some(value) = parts.next() else {
                    return Err("/goal failed: --status requires a value".to_string());
                };
                params.insert(
                    "status".to_string(),
                    serde_json::Value::String(value.to_string()),
                );
            }
            "--budget" | "--token-budget" | "--tokens" => {
                let Some(value) = parts.next() else {
                    return Err("/goal failed: token budget must be a positive integer".to_string());
                };
                let budget = parse_positive_budget(value)?;
                params.insert(
                    "tokenBudget".to_string(),
                    serde_json::Value::Number(budget.into()),
                );
            }
            other if other.starts_with("--status=") => {
                let value = other.trim_start_matches("--status=");
                if value.is_empty() {
                    return Err("/goal failed: --status requires a value".to_string());
                }
                params.insert(
                    "status".to_string(),
                    serde_json::Value::String(value.to_string()),
                );
            }
            other if other.starts_with("--budget=") => {
                let budget = parse_positive_budget(other.trim_start_matches("--budget="))?;
                params.insert(
                    "tokenBudget".to_string(),
                    serde_json::Value::Number(budget.into()),
                );
            }
            other if other.starts_with("--token-budget=") => {
                let budget = parse_positive_budget(other.trim_start_matches("--token-budget="))?;
                params.insert(
                    "tokenBudget".to_string(),
                    serde_json::Value::Number(budget.into()),
                );
            }
            other if other.starts_with("--tokens=") => {
                let budget = parse_positive_budget(other.trim_start_matches("--tokens="))?;
                params.insert(
                    "tokenBudget".to_string(),
                    serde_json::Value::Number(budget.into()),
                );
            }
            other => objective_parts.push(other),
        }
    }

    let objective = unquote_slash_value(&objective_parts.join(" "));
    if !objective.is_empty() {
        let chars = objective.chars().count();
        if chars > 4000 {
            return Err("/goal failed: objective must be 4000 characters or fewer".to_string());
        }
        params.insert(
            "objective".to_string(),
            serde_json::Value::String(objective),
        );
    }

    Ok(CodexSlashCommand {
        op,
        params: serde_json::Value::Object(params),
    })
}

fn parse_positive_budget(value: &str) -> Result<u64, String> {
    match value.parse::<u64>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err("/goal failed: token budget must be a positive integer".to_string()),
    }
}

fn unquote_slash_value(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.len() >= 2 {
        let bytes = trimmed.as_bytes();
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return trimmed[1..trimmed.len() - 1].trim().to_string();
        }
    }
    trimmed.to_string()
}

fn control_target_session_id(msg: &event::ControlMsg) -> Option<&str> {
    match msg {
        event::ControlMsg::Status { session_id }
        | event::ControlMsg::Approve { session_id, .. }
        | event::ControlMsg::Deny { session_id, .. }
        | event::ControlMsg::Skip { session_id, .. }
        | event::ControlMsg::ApproveAll { session_id, .. }
        | event::ControlMsg::Interrupt { session_id, .. }
        | event::ControlMsg::Steer { session_id, .. }
        | event::ControlMsg::CancelSteer { session_id, .. }
        | event::ControlMsg::StartTask { session_id, .. }
        | event::ControlMsg::EditUserMessage { session_id, .. }
        | event::ControlMsg::FollowUp { session_id, .. }
        | event::ControlMsg::CancelFollowUp { session_id, .. } => session_id.as_deref(),
        event::ControlMsg::RenameSession { session_id, .. } => Some(session_id.as_str()),
        event::ControlMsg::ConfigureSessionAgent { session_id, .. } => Some(session_id.as_str()),
        event::ControlMsg::StopSession { session_id } => Some(session_id.as_str()),
        event::ControlMsg::ResumeSession { .. } | event::ControlMsg::RestartSession { .. } => None,
        _ => None,
    }
}

fn edit_attach_request(
    source: Option<String>,
    resume_id: Option<String>,
    project_root: Option<String>,
    direct: Option<bool>,
) -> Option<EditAttachRequest> {
    let backend = source
        .as_deref()
        .and_then(external_agent::AgentBackend::from_str_loose)?;
    if !backend.supports_user_message_rewind() {
        return None;
    }

    Some(EditAttachRequest {
        source: backend.as_short_str().to_string(),
        resume_id: resume_id
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty()),
        project_root: project_root
            .map(|root| root.trim().to_string())
            .filter(|root| !root.is_empty()),
        direct,
    })
}

fn control_msg_can_attach_unmanaged_session(msg: &event::ControlMsg) -> bool {
    match msg {
        event::ControlMsg::EditUserMessage {
            source: Some(source),
            ..
        } => external_agent::AgentBackend::from_str_loose(source)
            .is_some_and(|backend| backend.supports_user_message_rewind()),
        _ => false,
    }
}

fn short_session(session_id: &str) -> String {
    session_id.chars().take(8).collect()
}

fn emit_follow_up_status(
    bus: &EventBus,
    session_id: Option<String>,
    id: &Option<String>,
    text: Option<&str>,
    status: &str,
    reason: Option<&str>,
) {
    let Some(id) = id.as_deref().map(str::trim).filter(|id| !id.is_empty()) else {
        return;
    };
    bus.send(AppEvent::FollowUpStatus {
        session_id,
        id: id.to_string(),
        text: text.map(str::to_string),
        status: status.to_string(),
        reason: reason.map(str::to_string),
    });
}

fn external_resume_log_dir(session_id: &str, force_new: bool) -> PathBuf {
    if !force_new {
        if let Some(dir) = session_log::SessionLog::find_session_by_id(session_id) {
            return dir;
        }
    }
    session_log::SessionLog::resolve_path(None)
}

fn spawn_text_steer_fallback(
    bus: EventBus,
    mut ack_rx: tokio::sync::broadcast::Receiver<AppEvent>,
    follow_up_tx: mpsc::Sender<FollowUpMessage>,
    text: String,
    steer_id: String,
    target_session_id: Option<String>,
) {
    tokio::spawn(async move {
        let timeout = tokio::time::sleep(TEXT_STEER_FALLBACK_TIMEOUT);
        tokio::pin!(timeout);
        loop {
            tokio::select! {
                _ = &mut timeout => break,
                event = ack_rx.recv() => {
                    match event {
                        Ok(AppEvent::SteerAccepted { session_id, id, .. })
                        | Ok(AppEvent::SteerQueued { session_id, id, .. })
                        | Ok(AppEvent::SteerDelivered { session_id, id, .. })
                        | Ok(AppEvent::SteerCancelled { session_id, id, .. })
                            if id == steer_id
                                && steer_ack_targets_session(
                                    &session_id,
                                    &target_session_id,
                                ) =>
                        {
                            return;
                        }
                        Ok(AppEvent::SteerCancelRequested { session_id, id, .. })
                            if id
                                .as_deref()
                                .map(|id| id == steer_id.as_str())
                                .unwrap_or(true)
                                && steer_ack_targets_session(
                                    &session_id,
                                    &target_session_id,
                                ) =>
                        {
                            return;
                        }
                        Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            continue;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }

        let msg = FollowUpMessage::steer(text, UserAttachments::default(), steer_id.clone())
            .for_target(target_session_id.clone());
        match follow_up_tx.send(msg).await {
            Ok(()) => bus.send(AppEvent::SteerQueued {
                session_id: target_session_id,
                id: steer_id,
                reason: "native steer was not acknowledged; queued as follow-up".to_string(),
            }),
            Err(_) => bus.send(AppEvent::LogEntry {
                session_id: target_session_id,
                level: "warn".to_string(),
                source: "Intendant".to_string(),
                content:
                    "Steer dropped: target session stopped before native steer was acknowledged"
                        .to_string(),
                turn: None,
            }),
        }
    });
}

fn steer_ack_targets_session(actual: &Option<String>, expected: &Option<String>) -> bool {
    match (actual.as_deref(), expected.as_deref()) {
        (Some(actual), Some(expected)) => actual == expected,
        (None, _) | (_, None) => true,
    }
}

fn load_related_sessions_from_log(session_dir: &Path) -> Vec<RelatedSessionRecord> {
    let path = session_dir.join("session.jsonl");
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Vec::new();
    };

    contents
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|entry| entry.get("event").and_then(|v| v.as_str()) == Some("session_relationship"))
        .filter_map(|entry| {
            let data = entry.get("data")?;
            let parent_session_id = data
                .get("parent_session_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let child_session_id = data
                .get("child_session_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let relationship = data
                .get("relationship")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            if parent_session_id.is_empty()
                || child_session_id.is_empty()
                || parent_session_id == child_session_id
                || !matches!(relationship.as_str(), "side" | "subagent")
            {
                return None;
            }
            Some(RelatedSessionRecord {
                parent_session_id,
                child_session_id,
                relationship,
            })
        })
        .collect()
}

fn short_text(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

fn external_attach_dedupe_keys(source: &str, session_id: &str, resume_token: &str) -> Vec<String> {
    let source = source.trim().to_lowercase();
    if source.is_empty() {
        return Vec::new();
    }
    let mut ids = Vec::new();
    for id in [session_id, resume_token] {
        let id = id.trim();
        if id.is_empty() || ids.iter().any(|existing: &String| existing.as_str() == id) {
            continue;
        }
        ids.push(id.to_string());
    }
    ids.into_iter().map(|id| format!("{source}:{id}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn managed_session(id: &str, source: &str) -> ManagedSession {
        let (tx, _rx) = mpsc::channel(1);
        ManagedSession {
            session_id: id.to_string(),
            source: source.to_string(),
            name: None,
            phase: "idle".to_string(),
            project_root: PathBuf::from("/tmp/project"),
            session_dir: PathBuf::from("/tmp/session"),
            follow_up_tx: tx,
            approval_registry: event::ApprovalRegistry::default(),
            instance_id: 0,
            finished_rx: None,
        }
    }

    fn test_supervisor(project_root: PathBuf, bus: EventBus) -> SessionSupervisor {
        SessionSupervisor::new(SessionSupervisorConfig {
            bus,
            project_root,
            autonomy: crate::autonomy::shared_autonomy(crate::autonomy::AutonomyState::default()),
            shared_external_agent: Arc::new(tokio::sync::RwLock::new(None)),
            shared_codex_config: Arc::new(tokio::sync::RwLock::new(
                control_plane::CodexRuntimeConfig {
                    command: "codex".to_string(),
                    sandbox: "workspace-write".to_string(),
                    approval_policy: "on-request".to_string(),
                    model: None,
                    reasoning_effort: None,
                    service_tier: None,
                    web_search: false,
                    network_access: false,
                    writable_roots: Vec::new(),
                    managed_context: "vanilla".to_string(),
                    context_archive: "summary".to_string(),
                },
            )),
            shared_gemini_config: Arc::new(tokio::sync::RwLock::new(
                control_plane::GeminiRuntimeConfig {
                    model: None,
                    approval_mode: "default".to_string(),
                    sandbox: false,
                    extensions: Vec::new(),
                    allowed_mcp_servers: Vec::new(),
                    include_directories: Vec::new(),
                    debug: false,
                },
            )),
            frame_registry: Arc::new(tokio::sync::RwLock::new(frames::FrameRegistry::new(
                std::env::temp_dir().as_path(),
            ))),
            web_port: None,
            flags_direct: false,
            shared_session: None,
        })
    }

    fn slash(text: &str) -> CodexSlashCommand {
        parse_codex_slash_command(text)
            .expect("recognized slash command")
            .expect("valid slash command")
    }

    #[test]
    fn supervisor_state_resolves_and_removes_session_aliases() {
        let mut state = SupervisorState::default();
        state
            .sessions
            .insert("backend".to_string(), managed_session("backend", "codex"));
        state
            .session_aliases
            .insert("wrapper".to_string(), "backend".to_string());
        state.active_session_id = Some("backend".to_string());

        assert_eq!(
            state.resolve_session_id("wrapper").as_deref(),
            Some("backend")
        );
        assert!(state.session_is_managed("wrapper"));

        let removed = state.remove_session("wrapper");
        assert!(removed.is_some());
        assert!(!state.session_is_managed("wrapper"));
        assert!(!state.session_is_managed("backend"));
    }

    #[tokio::test]
    async fn external_identity_moves_wrapper_session_to_backend_id() {
        let bus = EventBus::new();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus);
        {
            let mut state = supervisor.state.lock().await;
            let mut session = managed_session("wrapper", "codex");
            session.phase = "thinking".to_string();
            state.sessions.insert("wrapper".to_string(), session);
            state.active_session_id = Some("wrapper".to_string());
        }

        supervisor
            .apply_session_identity(
                "wrapper".to_string(),
                "codex".to_string(),
                "backend".to_string(),
            )
            .await;

        let state = supervisor.state.lock().await;
        assert!(!state.sessions.contains_key("wrapper"));
        assert_eq!(
            state.resolve_session_id("wrapper").as_deref(),
            Some("backend")
        );
        assert_eq!(
            state.resolve_session_id("backend").as_deref(),
            Some("backend")
        );
        assert_eq!(state.active_session_id.as_deref(), Some("backend"));
        assert_eq!(
            state
                .sessions
                .get("backend")
                .map(|session| session.phase.as_str()),
            Some("thinking")
        );
    }

    #[test]
    fn supervisor_state_resolves_side_child_alias_to_parent_session() {
        let mut state = SupervisorState::default();
        state
            .sessions
            .insert("parent".to_string(), managed_session("parent", "codex"));
        state
            .session_aliases
            .insert("side-child".to_string(), "parent".to_string());

        assert_eq!(
            state.resolve_session_id("side-child").as_deref(),
            Some("parent")
        );
        state.session_aliases.remove("side-child");
        assert!(!state.session_is_managed("side-child"));
        assert!(state.session_is_managed("parent"));
    }

    #[test]
    fn supervisor_state_tracks_subagent_child_as_related_parent_target() {
        let mut state = SupervisorState::default();
        state
            .sessions
            .insert("parent".to_string(), managed_session("parent", "codex"));
        assert!(state.apply_related_session("parent", "sub-child", "subagent"));

        assert_eq!(
            state.resolve_session_id("sub-child").as_deref(),
            Some("parent")
        );
        assert_eq!(
            state
                .related_sessions
                .get("sub-child")
                .map(|rel| rel.relationship.as_str()),
            Some("subagent")
        );

        let removed = state.remove_session("parent");
        assert!(removed.is_some());
        assert!(!state.session_is_managed("sub-child"));
        assert!(!state.related_sessions.contains_key("sub-child"));
    }

    #[test]
    fn supervisor_state_does_not_remove_newer_session_instance() {
        let mut state = SupervisorState::default();
        let mut session = managed_session("thread", "codex");
        session.instance_id = 1;
        state.sessions.insert("thread".to_string(), session);

        assert!(state.remove_session_instance("thread", 2).is_none());
        assert!(state.session_is_managed("thread"));
        assert!(state.remove_session_instance("thread", 1).is_some());
        assert!(!state.session_is_managed("thread"));
    }

    #[test]
    fn supervisor_state_dedupes_concurrent_restart_requests() {
        let mut state = SupervisorState::default();

        assert!(state.mark_restart_requested("codex:thread"));
        assert!(!state.mark_restart_requested("codex:thread"));
        assert!(state.mark_restart_requested("codex:other-thread"));
    }

    #[test]
    fn external_attach_dedupe_keys_include_session_and_resume_ids() {
        assert_eq!(
            external_attach_dedupe_keys(" Codex ", "wrapper", "thread"),
            vec!["codex:wrapper".to_string(), "codex:thread".to_string()]
        );
        assert_eq!(
            external_attach_dedupe_keys("codex", "thread", "thread"),
            vec!["codex:thread".to_string()]
        );
    }

    #[test]
    fn supervisor_state_dedupes_in_flight_external_attaches_by_alias() {
        let mut state = SupervisorState::default();
        let first = external_attach_dedupe_keys("codex", "wrapper", "thread");
        let duplicate_by_resume = external_attach_dedupe_keys("codex", "thread", "thread");

        assert!(state.mark_external_attach_requested(&first));
        assert!(!state.mark_external_attach_requested(&duplicate_by_resume));
        state.clear_external_attach_requested(&first);
        assert!(state.mark_external_attach_requested(&duplicate_by_resume));
    }

    #[test]
    fn external_resume_log_dir_reuses_requested_wrapper_log() {
        let dir = tempfile::tempdir().unwrap();
        let wrapper_dir = dir.path().join("wrapper-session");
        let log = session_log::SessionLog::open(wrapper_dir.clone()).unwrap();
        log.write_meta(Some(dir.path()), Some("previous external task"));

        let resolved = external_resume_log_dir(wrapper_dir.to_str().unwrap(), false);
        assert_eq!(resolved, wrapper_dir);
    }

    #[test]
    fn external_resume_project_root_uses_persisted_launch_root() {
        let dir = tempfile::tempdir().unwrap();
        let helper_root = dir.path().join("intendant-helper-main-5770");
        let station_root = dir.path().join("intendant-station-mainline-123e28c");
        std::fs::create_dir_all(&helper_root).unwrap();
        std::fs::create_dir_all(&station_root).unwrap();
        let mut config = crate::session_config::from_wire(
            Some("codex"),
            Some("/tmp/codex"),
            Some("danger-full-access"),
            Some("never"),
            Some("managed"),
            Some("summary"),
            None,
        );
        config.project_root = Some(station_root.to_string_lossy().to_string());

        let resolved =
            resolve_external_resume_project_root(None, Some(&config), &helper_root).unwrap();
        assert_eq!(resolved, station_root.canonicalize().unwrap());
    }

    #[test]
    fn external_resume_project_root_prefers_explicit_request() {
        let dir = tempfile::tempdir().unwrap();
        let helper_root = dir.path().join("intendant-helper-main-5770");
        let station_root = dir.path().join("intendant-station-mainline-123e28c");
        let requested_root = dir.path().join("requested-worktree");
        std::fs::create_dir_all(&helper_root).unwrap();
        std::fs::create_dir_all(&station_root).unwrap();
        std::fs::create_dir_all(&requested_root).unwrap();
        let mut config = crate::session_config::from_wire(
            Some("codex"),
            Some("/tmp/codex"),
            Some("danger-full-access"),
            Some("never"),
            Some("managed"),
            Some("summary"),
            None,
        );
        config.project_root = Some(station_root.to_string_lossy().to_string());

        let resolved = resolve_external_resume_project_root(
            Some(requested_root.to_string_lossy().to_string()),
            Some(&config),
            &helper_root,
        )
        .unwrap();
        assert_eq!(resolved, requested_root);
    }

    #[tokio::test]
    async fn stop_managed_session_broadcasts_stop_and_removes_live_session() {
        let bus = EventBus::new();
        let mut bus_rx = bus.subscribe();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus.clone());
        {
            let mut state = supervisor.state.lock().await;
            state.sessions.insert(
                "parent-thread".to_string(),
                managed_session("parent-thread", "codex"),
            );
        }

        let stopped = supervisor
            .stop_managed_session(Some("parent-thread".to_string()), "stopped by user")
            .await
            .expect("managed session should stop");
        assert_eq!(stopped.session_id, "parent-thread");

        {
            let state = supervisor.state.lock().await;
            assert!(!state.session_is_managed("parent-thread"));
        }

        let mut saw_stop_request = false;
        let mut saw_session_ended = false;
        while let Ok(event) = bus_rx.try_recv() {
            match event {
                AppEvent::SessionStopRequested { session_id, reason }
                    if session_id.as_deref() == Some("parent-thread")
                        && reason == "stopped by user" =>
                {
                    saw_stop_request = true;
                }
                AppEvent::SessionEnded { session_id, reason }
                    if session_id == "parent-thread" && reason == "stopped by user" =>
                {
                    saw_session_ended = true;
                }
                _ => {}
            }
        }
        assert!(saw_stop_request, "expected SessionStopRequested");
        assert!(saw_session_ended, "expected SessionEnded");
    }

    #[tokio::test]
    async fn finish_session_writes_terminal_outcome_to_summary() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let session_log = Arc::new(std::sync::Mutex::new(
            session_log::SessionLog::open(log_dir.clone()).unwrap(),
        ));
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), EventBus::new());
        let mut stats = LoopStats::default();
        stats.turns = 1;
        stats.rounds = 1;
        stats.terminal_outcome = Some("stopped by user".to_string());

        supervisor
            .finish_session(
                "session-id".to_string(),
                0,
                session_log,
                "task".to_string(),
                Ok(stats),
            )
            .await;

        let summary: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(log_dir.join("summary.json")).unwrap())
                .unwrap();
        assert_eq!(summary["outcome"], "stopped by user");
    }

    #[tokio::test]
    async fn resume_managed_external_session_with_task_routes_follow_up() {
        let bus = EventBus::new();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus.clone());
        let (tx, mut rx) = mpsc::channel(1);
        {
            let mut state = supervisor.state.lock().await;
            state.sessions.insert(
                "parent-thread".to_string(),
                ManagedSession {
                    session_id: "parent-thread".to_string(),
                    source: "codex".to_string(),
                    name: None,
                    phase: "idle".to_string(),
                    project_root: PathBuf::from("/tmp/project"),
                    session_dir: PathBuf::from("/tmp/session"),
                    follow_up_tx: tx,
                    approval_registry: event::ApprovalRegistry::default(),
                    instance_id: 0,
                    finished_rx: None,
                },
            );
        }

        supervisor
            .resume_session(
                "codex".to_string(),
                "parent-thread".to_string(),
                Some("parent-thread".to_string()),
                Some("/tmp/project".to_string()),
                Some("continue parent".to_string()),
                Some(true),
                Vec::new(),
                None,
                None,
                None,
                None,
                None,
                false,
            )
            .await;

        let msg = rx
            .try_recv()
            .expect("resume task should route to existing runner");
        assert_eq!(msg.text, "continue parent");
        assert_eq!(msg.target_session_id.as_deref(), Some("parent-thread"));

        let state = supervisor.state.lock().await;
        assert!(state.session_is_managed("parent-thread"));
    }

    #[tokio::test]
    async fn resume_managed_external_session_without_task_attaches_without_deadlock() {
        let bus = EventBus::new();
        let mut bus_rx = bus.subscribe();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus.clone());
        let (tx, _rx) = mpsc::channel(1);
        {
            let mut state = supervisor.state.lock().await;
            state.sessions.insert(
                "parent-thread".to_string(),
                ManagedSession {
                    session_id: "parent-thread".to_string(),
                    source: "codex".to_string(),
                    name: None,
                    phase: "idle".to_string(),
                    project_root: PathBuf::from("/tmp/project"),
                    session_dir: PathBuf::from("/tmp/session"),
                    follow_up_tx: tx,
                    approval_registry: event::ApprovalRegistry::default(),
                    instance_id: 0,
                    finished_rx: None,
                },
            );
        }

        tokio::time::timeout(
            std::time::Duration::from_millis(200),
            supervisor.resume_session(
                "codex".to_string(),
                "parent-thread".to_string(),
                Some("parent-thread".to_string()),
                Some("/tmp/project".to_string()),
                None,
                Some(true),
                Vec::new(),
                None,
                None,
                None,
                None,
                None,
                false,
            ),
        )
        .await
        .expect("attach-only resume should not deadlock");

        {
            let state = supervisor.state.lock().await;
            assert_eq!(state.active_session_id.as_deref(), Some("parent-thread"));
        }

        let mut saw_status = false;
        let mut saw_attach = false;
        while let Ok(event) = bus_rx.try_recv() {
            match event {
                AppEvent::StatusUpdate {
                    session_id, phase, ..
                } if session_id == "parent-thread" && phase == "idle" => {
                    saw_status = true;
                }
                AppEvent::SessionAttached { session_id, source }
                    if session_id == "parent-thread" && source == "codex" =>
                {
                    saw_attach = true;
                }
                _ => {}
            }
        }
        assert!(saw_status, "attach-only resume should emit current status");
        assert!(saw_attach, "attach-only resume should emit SessionAttached");
    }

    #[tokio::test]
    async fn resume_managed_external_session_with_task_preserves_attachments() {
        use std::io::Write as _;

        let tmp = tempfile::TempDir::new().unwrap();
        let project_root = tmp.path().join("project");
        let session_dir = tmp.path().join("session");
        std::fs::create_dir_all(&project_root).unwrap();
        std::fs::create_dir_all(&session_dir).unwrap();

        let bus = EventBus::new();
        let supervisor = test_supervisor(project_root.clone(), bus);
        let (tx, mut rx) = mpsc::channel(1);
        {
            let mut state = supervisor.state.lock().await;
            state.sessions.insert(
                "parent-thread".to_string(),
                ManagedSession {
                    session_id: "parent-thread".to_string(),
                    source: "codex".to_string(),
                    name: None,
                    phase: "idle".to_string(),
                    project_root: project_root.clone(),
                    session_dir: session_dir.clone(),
                    follow_up_tx: tx,
                    approval_registry: event::ApprovalRegistry::default(),
                    instance_id: 0,
                    finished_rx: None,
                },
            );
        }

        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"needle").unwrap();
        file.flush().unwrap();
        let upload = crate::upload_store::commit_upload(
            file,
            "note.txt",
            "text/plain",
            6,
            crate::upload_store::UploadDestination::Task,
            &session_dir,
            "parent-thread",
            &project_root,
        )
        .unwrap();

        supervisor
            .resume_session(
                "codex".to_string(),
                "parent-thread".to_string(),
                Some("parent-thread".to_string()),
                Some(project_root.to_string_lossy().to_string()),
                Some("read attachment".to_string()),
                Some(true),
                vec![format!("upload:{}", upload.id)],
                None,
                None,
                None,
                None,
                None,
                false,
            )
            .await;

        let msg = rx
            .try_recv()
            .expect("resume task should route to existing runner");
        assert_eq!(msg.text, "read attachment");
        assert_eq!(msg.attachments.len(), 1);
        match &msg.attachments.items[0] {
            external_agent::AgentAttachment::File(file) => {
                assert_eq!(file.name, "note.txt");
                assert_eq!(file.mime_type, "text/plain");
                assert_eq!(file.size, 6);
                assert_eq!(file.local_path, upload.path);
            }
            other => panic!("expected file attachment, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn side_follow_up_routes_to_external_follow_up_event() {
        let bus = EventBus::new();
        let mut bus_rx = bus.subscribe();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus.clone());
        let (tx, mut rx) = mpsc::channel(1);
        {
            let mut state = supervisor.state.lock().await;
            state.sessions.insert(
                "parent-thread".to_string(),
                ManagedSession {
                    session_id: "parent-thread".to_string(),
                    source: "codex".to_string(),
                    name: None,
                    phase: "idle".to_string(),
                    project_root: PathBuf::from("/tmp/project"),
                    session_dir: PathBuf::from("/tmp/session"),
                    follow_up_tx: tx,
                    approval_registry: event::ApprovalRegistry::default(),
                    instance_id: 0,
                    finished_rx: None,
                },
            );
            assert!(state.apply_related_session("parent-thread", "side-thread", "side"));
        }

        supervisor
            .route_follow_up(
                Some("side-thread".to_string()),
                "continue side".to_string(),
                Some(true),
                Vec::new(),
                Some("follow-1".to_string()),
            )
            .await;

        assert!(rx.try_recv().is_err());
        match bus_rx.recv().await.expect("side follow-up event") {
            AppEvent::ExternalFollowUpRequested {
                session_id,
                text,
                attachments,
                follow_up_id,
            } => {
                assert_eq!(session_id, "side-thread");
                assert_eq!(text, "continue side");
                assert!(attachments.is_empty());
                assert_eq!(follow_up_id.as_deref(), Some("follow-1"));
            }
            other => panic!("expected external follow-up request, got {other:?}"),
        }

        let state = supervisor.state.lock().await;
        assert!(state.session_is_managed("parent-thread"));
        assert!(state.session_is_managed("side-thread"));
    }

    #[tokio::test]
    async fn side_edit_preserves_child_target_on_parent_channel() {
        let bus = EventBus::new();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus);
        let (tx, mut rx) = mpsc::channel(1);
        {
            let mut state = supervisor.state.lock().await;
            state.sessions.insert(
                "parent-thread".to_string(),
                ManagedSession {
                    session_id: "parent-thread".to_string(),
                    source: "codex".to_string(),
                    name: None,
                    phase: "idle".to_string(),
                    project_root: PathBuf::from("/tmp/project"),
                    session_dir: PathBuf::from("/tmp/session"),
                    follow_up_tx: tx,
                    approval_registry: event::ApprovalRegistry::default(),
                    instance_id: 0,
                    finished_rx: None,
                },
            );
            assert!(state.apply_related_session("parent-thread", "side-thread", "side"));
        }

        supervisor
            .route_edit_user_message(
                Some("side-thread".to_string()),
                None,
                None,
                None,
                Some(true),
                1,
                Some(1),
                None,
                "replacement side prompt".to_string(),
                Vec::new(),
            )
            .await;

        let msg = rx
            .try_recv()
            .expect("side edit should queue on parent runner");
        assert_eq!(msg.text, "replacement side prompt");
        assert_eq!(msg.edit_user_turn_index, Some(1));
        assert_eq!(msg.edit_user_turn_revision, Some(1));
        assert_eq!(msg.target_session_id.as_deref(), Some("side-thread"));
    }

    #[tokio::test]
    async fn edit_queued_before_attach_delivers_after_session_identity() {
        let bus = EventBus::new();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus);
        let (tx, mut rx) = mpsc::channel(1);

        supervisor.queue_edit_user_message_after_attach(
            "codex-thread".to_string(),
            EditUserMessageRequest {
                requested_id: "codex-thread".to_string(),
                user_turn_index: 2,
                user_turn_revision: Some(5),
                original_text: Some("continue".to_string()),
                text: "edited continue".to_string(),
                attachments: Vec::new(),
            },
        );

        tokio::time::sleep(EDIT_ATTACH_ROUTE_POLL_INTERVAL * 2).await;
        {
            let mut state = supervisor.state.lock().await;
            state.sessions.insert(
                "wrapper-session".to_string(),
                ManagedSession {
                    session_id: "wrapper-session".to_string(),
                    source: "codex".to_string(),
                    name: None,
                    phase: "idle".to_string(),
                    project_root: PathBuf::from("/tmp/project"),
                    session_dir: PathBuf::from("/tmp/session"),
                    follow_up_tx: tx,
                    approval_registry: event::ApprovalRegistry::default(),
                    instance_id: 0,
                    finished_rx: None,
                },
            );
            state
                .session_aliases
                .insert("codex-thread".to_string(), "wrapper-session".to_string());
        }

        let msg = tokio::time::timeout(std::time::Duration::from_millis(300), rx.recv())
            .await
            .expect("queued edit should be delivered after alias registration")
            .expect("follow-up channel should stay open");
        assert_eq!(msg.text, "edited continue");
        assert_eq!(msg.edit_user_turn_index, Some(2));
        assert_eq!(msg.edit_user_turn_revision, Some(5));
        assert_eq!(msg.edit_original_text.as_deref(), Some("continue"));
        assert_eq!(msg.target_session_id, None);
    }

    #[tokio::test]
    async fn text_steer_falls_back_to_follow_up_without_native_ack() {
        let bus = EventBus::new();
        let mut bus_rx = bus.subscribe();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus.clone());
        let (tx, mut rx) = mpsc::channel(1);
        {
            let mut state = supervisor.state.lock().await;
            state.sessions.insert(
                "parent-thread".to_string(),
                ManagedSession {
                    session_id: "parent-thread".to_string(),
                    source: "codex".to_string(),
                    name: None,
                    phase: "thinking".to_string(),
                    project_root: PathBuf::from("/tmp/project"),
                    session_dir: PathBuf::from("/tmp/session"),
                    follow_up_tx: tx,
                    approval_registry: event::ApprovalRegistry::default(),
                    instance_id: 0,
                    finished_rx: None,
                },
            );
        }

        supervisor
            .route_steer(
                Some("parent-thread".to_string()),
                "Pause for a moment".to_string(),
                Some("steer-1".to_string()),
                Vec::new(),
            )
            .await;

        match bus_rx.recv().await.expect("steer requested event") {
            AppEvent::SteerRequested {
                session_id,
                text,
                id,
            } => {
                assert_eq!(session_id.as_deref(), Some("parent-thread"));
                assert_eq!(text, "Pause for a moment");
                assert_eq!(id, "steer-1");
            }
            other => panic!("expected steer requested event, got {other:?}"),
        }

        let msg = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
            .await
            .expect("unacknowledged steer should be queued")
            .expect("follow-up channel should stay open");
        assert_eq!(msg.text, "Pause for a moment");
        assert_eq!(msg.steer_id.as_deref(), Some("steer-1"));
        assert_eq!(msg.target_session_id.as_deref(), Some("parent-thread"));

        let mut saw_queued = false;
        while let Ok(event) = bus_rx.try_recv() {
            if let AppEvent::SteerQueued {
                session_id,
                id,
                reason,
            } = event
            {
                assert_eq!(session_id.as_deref(), Some("parent-thread"));
                assert_eq!(id, "steer-1");
                assert!(reason.contains("not acknowledged"), "got: {reason}");
                saw_queued = true;
            }
        }
        assert!(saw_queued, "fallback should emit SteerQueued");
    }

    #[tokio::test]
    async fn text_steer_native_ack_prevents_follow_up_fallback() {
        let bus = EventBus::new();
        let mut bus_rx = bus.subscribe();
        let supervisor = test_supervisor(PathBuf::from("/tmp/project"), bus.clone());
        let (tx, mut rx) = mpsc::channel(1);
        {
            let mut state = supervisor.state.lock().await;
            state.sessions.insert(
                "parent-thread".to_string(),
                ManagedSession {
                    session_id: "parent-thread".to_string(),
                    source: "codex".to_string(),
                    name: None,
                    phase: "thinking".to_string(),
                    project_root: PathBuf::from("/tmp/project"),
                    session_dir: PathBuf::from("/tmp/session"),
                    follow_up_tx: tx,
                    approval_registry: event::ApprovalRegistry::default(),
                    instance_id: 0,
                    finished_rx: None,
                },
            );
        }

        supervisor
            .route_steer(
                Some("parent-thread".to_string()),
                "pause for a moment".to_string(),
                Some("steer-2".to_string()),
                Vec::new(),
            )
            .await;

        match bus_rx.recv().await.expect("steer requested event") {
            AppEvent::SteerRequested { id, .. } => assert_eq!(id, "steer-2"),
            other => panic!("expected steer requested event, got {other:?}"),
        }
        bus.send(AppEvent::SteerAccepted {
            session_id: Some("parent-thread".to_string()),
            id: "steer-2".to_string(),
            reason: "Codex accepted the steer".to_string(),
        });

        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        assert!(
            rx.try_recv().is_err(),
            "acknowledged steer should not also queue a follow-up"
        );

        while let Ok(event) = bus_rx.try_recv() {
            if let AppEvent::SteerQueued { id, .. } = event {
                assert_ne!(id, "steer-2", "acknowledged steer should not queue");
            }
        }
    }

    #[test]
    fn loads_related_sessions_from_session_log() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("session");
        let mut log = session_log::SessionLog::open(log_dir.clone()).unwrap();
        log.session_relationship("parent", "sub-child", "subagent", false);
        log.session_relationship("parent", "side-child", "side", true);
        log.session_relationship("parent", "fork-child", "fork", false);
        drop(log);

        let related = load_related_sessions_from_log(&log_dir);
        assert_eq!(
            related,
            vec![
                RelatedSessionRecord {
                    parent_session_id: "parent".to_string(),
                    child_session_id: "sub-child".to_string(),
                    relationship: "subagent".to_string(),
                },
                RelatedSessionRecord {
                    parent_session_id: "parent".to_string(),
                    child_session_id: "side-child".to_string(),
                    relationship: "side".to_string(),
                },
            ]
        );
    }

    #[test]
    fn parses_fork_slash_command_with_name() {
        let command = slash("/fork dashboard branch");
        assert_eq!(command.op, "fork");
        assert_eq!(command.params["name"], "dashboard branch");
    }

    #[test]
    fn parses_side_slash_command_with_prompt() {
        let command = slash("/side why is this failing?");
        assert_eq!(command.op, "side");
        assert_eq!(command.params["prompt"], "why is this failing?");
    }

    #[test]
    fn parses_btw_alias_as_side_slash_command() {
        let command = slash("/btw \"quick context check\"");
        assert_eq!(command.op, "side");
        assert_eq!(command.params["prompt"], "quick context check");
    }

    #[test]
    fn parses_goal_slash_command_with_objective_and_budget() {
        let command = slash("/goal Ship multi-session UX --budget 200000");
        assert_eq!(command.op, "goal");
        assert_eq!(command.params["objective"], "Ship multi-session UX");
        assert_eq!(command.params["tokenBudget"], 200000);
    }

    #[test]
    fn parses_goal_status_aliases() {
        assert_eq!(slash("/goal clear").op, "goal-clear");
        assert_eq!(slash("/goal pause").op, "goal-pause");
        assert_eq!(slash("/goal resume").op, "goal-resume");
        assert_eq!(slash("/goal done").op, "goal-complete");
    }

    #[test]
    fn parses_fast_slash_command() {
        let command = slash("/fast");
        assert_eq!(command.op, "fast");
        assert_eq!(command.params, serde_json::json!({}));

        let err = parse_codex_slash_command("/fast now")
            .expect("recognized slash command")
            .unwrap_err();
        assert!(err.contains("does not accept arguments"), "got: {err}");
    }

    #[test]
    fn fast_new_session_forces_or_accepts_codex_agent() {
        assert_eq!(
            codex_fast_new_session_agent(None).unwrap(),
            "codex".to_string()
        );
        assert_eq!(
            codex_fast_new_session_agent(Some("configured")).unwrap(),
            "codex".to_string()
        );
        assert_eq!(
            codex_fast_new_session_agent(Some("codex")).unwrap(),
            "codex".to_string()
        );

        let err = codex_fast_new_session_agent(Some("gemini")).unwrap_err();
        assert!(err.contains("Codex"), "got: {err}");
        let err = codex_fast_new_session_agent(Some("internal")).unwrap_err();
        assert!(err.contains("Codex"), "got: {err}");
    }

    #[test]
    fn ignores_non_codex_slash_commands() {
        assert!(parse_codex_slash_command("/help").is_none());
    }

    #[test]
    fn edit_attach_request_accepts_only_rewind_capable_external_sources() {
        let attach = edit_attach_request(
            Some("Codex".to_string()),
            Some(" 019e5c7a ".to_string()),
            Some(" /tmp/project ".to_string()),
            None,
        )
        .expect("codex edit should be attachable");
        assert_eq!(attach.source, "codex");
        assert_eq!(attach.resume_id.as_deref(), Some("019e5c7a"));
        assert_eq!(attach.project_root.as_deref(), Some("/tmp/project"));

        assert!(edit_attach_request(
            Some("gemini".to_string()),
            Some("gemini-session".to_string()),
            None,
            None,
        )
        .is_none());
        assert!(edit_attach_request(None, None, None, None).is_none());
    }

    #[test]
    fn external_codex_edit_control_can_be_handled_before_attach() {
        let msg = event::ControlMsg::EditUserMessage {
            session_id: Some("019e5c7a".to_string()),
            source: Some("codex".to_string()),
            resume_id: Some("019e5c7a".to_string()),
            project_root: Some("/tmp/project".to_string()),
            direct: Some(true),
            user_turn_index: 1,
            user_turn_revision: Some(1),
            original_text: None,
            text: "replacement".to_string(),
            attachments: Vec::new(),
        };
        assert!(control_msg_can_attach_unmanaged_session(&msg));
    }

    #[test]
    fn parses_session_agent_selection() {
        assert_eq!(
            SessionAgentSelection::from_wire(None).unwrap(),
            SessionAgentSelection::Configured
        );
        assert_eq!(
            SessionAgentSelection::from_wire(Some("internal")).unwrap(),
            SessionAgentSelection::Internal
        );
        assert_eq!(
            SessionAgentSelection::from_wire(Some("gemini")).unwrap(),
            SessionAgentSelection::External(external_agent::AgentBackend::GeminiCli)
        );
        assert!(SessionAgentSelection::from_wire(Some("unknown")).is_err());
    }

    #[test]
    fn applies_session_agent_command_to_selected_backend() {
        let mut project = Project {
            root: PathBuf::from("/tmp/project"),
            config: crate::project::ProjectConfig::default(),
        };
        apply_session_agent_command(
            &mut project,
            &external_agent::AgentBackend::ClaudeCode,
            "/opt/claude/bin/claude".to_string(),
        );
        assert_eq!(
            project.config.agent.claude_code.command,
            "/opt/claude/bin/claude"
        );
    }

    #[test]
    fn applies_session_codex_managed_context_to_codex_only() {
        let mut project = Project {
            root: PathBuf::from("/tmp/project"),
            config: crate::project::ProjectConfig::default(),
        };
        apply_session_codex_managed_context(
            &mut project,
            &external_agent::AgentBackend::Codex,
            "on".to_string(),
        )
        .unwrap();
        assert_eq!(project.config.agent.codex.managed_context, "managed");

        let err = apply_session_codex_managed_context(
            &mut project,
            &external_agent::AgentBackend::GeminiCli,
            "managed".to_string(),
        )
        .unwrap_err();
        assert!(err.contains("requires Codex"));
    }

    #[test]
    fn applies_session_codex_context_archive_to_codex_only() {
        let mut project = Project {
            root: PathBuf::from("/tmp/project"),
            config: crate::project::ProjectConfig::default(),
        };
        apply_session_codex_context_archive(
            &mut project,
            &external_agent::AgentBackend::Codex,
            "raw".to_string(),
        )
        .unwrap();
        assert_eq!(project.config.agent.codex.context_archive, "exact");

        let err = apply_session_codex_context_archive(
            &mut project,
            &external_agent::AgentBackend::GeminiCli,
            "summary".to_string(),
        )
        .unwrap_err();
        assert!(err.contains("requires Codex"));
    }

    #[test]
    fn normalizes_optional_session_name() {
        assert_eq!(
            normalize_session_name_option(Some("  Dashboard   work  ")).unwrap(),
            Some("Dashboard work".to_string())
        );
        assert_eq!(normalize_session_name_option(Some("   ")).unwrap(), None);
        assert_eq!(normalize_session_name_option(None).unwrap(), None);
    }
}
