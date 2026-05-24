//! Daemon-side session lifecycle supervisor.
//!
//! The supervisor is the long-lived owner for sessions launched from the
//! control plane. It accepts `StartTask`, `ResumeSession`, and targeted
//! follow-up commands from the shared `EventBus`, creates per-session runtime
//! resources, and tracks the follow-up channel for each managed session.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{mpsc, Mutex as AsyncMutex};
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

#[derive(Default)]
struct SupervisorState {
    sessions: HashMap<String, ManagedSession>,
    session_aliases: HashMap<String, String>,
    related_sessions: HashMap<String, RelatedSession>,
    active_session_id: Option<String>,
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

    async fn handle_control_msg(&self, msg: event::ControlMsg) {
        match msg {
            event::ControlMsg::CreateSession {
                task,
                name,
                project_root,
                agent,
                agent_command,
                orchestrate,
                direct,
                reference_frame_ids,
                display_target,
                attachments,
            } => {
                if parse_codex_slash_command(&task).is_some() {
                    if !reference_frame_ids.is_empty()
                        || display_target.is_some()
                        || agent.is_some()
                        || agent_command.is_some()
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
                    orchestrate,
                    direct,
                    reference_frame_ids,
                    display_target,
                    attachments,
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
                if parse_codex_slash_command(&task).is_some() {
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
                    orchestrate,
                    direct,
                    reference_frame_ids,
                    display_target,
                    attachments,
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
            } => {
                self.resume_session(source, session_id, resume_id, project_root, task, direct)
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
                user_turn_index,
                user_turn_revision,
                text,
                attachments,
            } => {
                self.route_edit_user_message(
                    session_id,
                    user_turn_index,
                    user_turn_revision,
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
            _ => {}
        }
    }

    async fn should_handle_session_control(&self, msg: &event::ControlMsg) -> bool {
        match msg {
            event::ControlMsg::CreateSession { .. } => true,
            event::ControlMsg::ResumeSession { .. } => true,
            event::ControlMsg::RenameSession { .. } => true,
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
        orchestrate: Option<bool>,
        direct: Option<bool>,
        reference_frame_ids: Vec<String>,
        display_target: Option<String>,
        attachments: Vec<String>,
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

        write_session_meta(
            &session_log,
            &project.root,
            Some(&task),
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
        let session_dir = session_log
            .lock()
            .map(|log| log.dir().to_path_buf())
            .unwrap_or_else(|_| log_dir.clone());
        let resolved_attachments = resolve_attachments(
            &attachments,
            &self.config.frame_registry,
            &session_dir,
            &project.root,
        )
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

        emit_task_dispatched_log(&self.config.bus, &session_log, &task, attachments.len());
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
        let project_root = project_root
            .map(PathBuf::from)
            .unwrap_or_else(|| self.config.project_root.clone());
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

        if resume_task.is_none() {
            if let Some(existing_id) = self
                .find_managed_session_id(&source_norm, &session_id, &resume_token)
                .await
            {
                let mut state = self.state.lock().await;
                state.active_session_id = Some(existing_id);
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
                let log_dir = session_log::SessionLog::resolve_path(None);
                let session_log = match session_log::SessionLog::open(log_dir.clone()) {
                    Ok(log) => Arc::new(Mutex::new(log)),
                    Err(e) => {
                        self.loop_error(format!("Session open failed: {}", e));
                        return;
                    }
                };
                let project = match self
                    .project_with_runtime_config(project_root.clone(), external_backend.as_ref())
                    .await
                {
                    Ok(project) => project,
                    Err(e) => {
                        self.loop_error(format!("Project load failed: {}", e));
                        return;
                    }
                };

                write_session_meta(&session_log, &project.root, None, None);
                self.activate_shared_session(session_log.clone()).await;
                self.spawn_agent_session(
                    resume_token.clone(),
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
                )
                .await;
                self.emit_attached_status(&resume_token, &source_norm).await;
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

        let log_dir = if external_backend.is_none() {
            match session_log::SessionLog::find_session_by_id(&session_id) {
                Some(dir) => dir,
                None => {
                    self.loop_error(format!("Session '{}' was not found", session_id));
                    return;
                }
            }
        } else {
            session_log::SessionLog::resolve_path(None)
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
        let project = match self
            .project_with_runtime_config(project_root.clone(), external_backend.as_ref())
            .await
        {
            Ok(project) => project,
            Err(e) => {
                self.loop_error(format!("Project load failed: {}", e));
                return;
            }
        };

        write_session_meta(&session_log, &project.root, Some(&resume_task), None);
        self.activate_shared_session(session_log.clone()).await;
        self.config.bus.send(AppEvent::SessionStarted {
            session_id: live_session_id.clone(),
            task: Some(resume_task.clone()),
        });

        emit_task_dispatched_log(&self.config.bus, &session_log, &resume_task, 0);
        self.spawn_agent_session(
            live_session_id,
            source_norm,
            resume_task,
            project,
            session_log,
            log_dir,
            external_backend,
            direct.unwrap_or(true),
            UserAttachments::default(),
            None,
            Some(resume_token),
            false,
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
    ) {
        let (follow_up_tx, follow_up_rx) = mpsc::channel::<FollowUpMessage>(16);
        let approval_registry = event::ApprovalRegistry::default();
        let context_injection = event::ContextInjectionQueue::default();
        self.register_session(
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
                    Some(session_id.clone()),
                    emit_session_started_after_identity,
                )
                .await
            } else {
                let provider = match provider::select_provider() {
                    Ok(provider) => provider,
                    Err(e) => {
                        return supervisor
                            .finish_session(session_id, session_log, task, Err(e))
                            .await;
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
                .finish_session(session_id, session_log, task, result)
                .await;
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
                .finish_session(session_id, session_log, task, summary)
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

                let resolved_attachments = if attachments.is_empty() {
                    Vec::new()
                } else {
                    resolve_attachments(
                        &attachments,
                        &self.config.frame_registry,
                        &session_dir,
                        &project_root,
                    )
                    .await
                };
                if resolved_attachments.len() < attachments.len() {
                    self.warn(&format!(
                        "Only resolved {} of {} requested attachment(s) for {} session {}",
                        resolved_attachments.len(),
                        attachments.len(),
                        source,
                        short_session(&managed_id)
                    ));
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
        user_turn_index: u32,
        user_turn_revision: Option<u32>,
        text: String,
        attachments: Vec<String>,
    ) {
        let (target_id, entry) = {
            let state = self.state.lock().await;
            let requested_id = session_id.or_else(|| state.active_session_id.clone());
            let Some(requested_id) = requested_id else {
                drop(state);
                self.warn("Edit dropped: no active managed session");
                return;
            };
            let target_id = state
                .resolve_session_id(&requested_id)
                .unwrap_or_else(|| requested_id.clone());
            let entry = state.sessions.get(&target_id).map(|s| {
                (
                    s.session_id.clone(),
                    s.source.clone(),
                    s.project_root.clone(),
                    s.session_dir.clone(),
                    s.follow_up_tx.clone(),
                )
            });
            (target_id, entry)
        };

        let Some((managed_id, source, project_root, session_dir, tx)) = entry else {
            self.warn(&format!(
                "Edit dropped: session {} is not managed by this daemon",
                short_session(&target_id)
            ));
            return;
        };
        let Some(backend) = external_agent::AgentBackend::from_str_loose(&source) else {
            self.warn(&format!(
                "Edit dropped: unknown external-agent source {} for session {}",
                source,
                short_session(&managed_id)
            ));
            return;
        };
        if !backend.supports_user_message_rewind() {
            self.warn(&format!(
                "Edit dropped: {} session {} does not support user-message rewind yet",
                backend,
                short_session(&managed_id)
            ));
            return;
        }
        if user_turn_index == 0 {
            self.warn(&format!(
                "Edit dropped: invalid user turn index 0 for {} session {}",
                backend,
                short_session(&managed_id)
            ));
            return;
        }
        let Some(user_turn_revision) = user_turn_revision else {
            self.warn(&format!(
                "Edit dropped: missing active-message revision for {} session {} user turn {}",
                backend,
                short_session(&managed_id),
                user_turn_index
            ));
            return;
        };

        let resolved_attachments = if attachments.is_empty() {
            Vec::new()
        } else {
            resolve_attachments(
                &attachments,
                &self.config.frame_registry,
                &session_dir,
                &project_root,
            )
            .await
        };
        if resolved_attachments.len() < attachments.len() {
            self.warn(&format!(
                "Only resolved {} of {} edit attachment(s) for {} session {}",
                resolved_attachments.len(),
                attachments.len(),
                backend,
                short_session(&managed_id)
            ));
        }
        let msg = FollowUpMessage::edit_user_message(
            text,
            UserAttachments::from_items(resolved_attachments),
            user_turn_index,
            user_turn_revision,
        );
        if tx.send(msg).await.is_err() {
            self.warn(&format!(
                "Edit dropped: {} session {} in {} is not accepting input",
                backend,
                short_session(&managed_id),
                project_root.display()
            ));
        }
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
        if attachments.is_empty() {
            self.config.bus.send(AppEvent::SteerRequested {
                session_id: event_session_id,
                text,
                id: steer_id,
            });
            return;
        }

        let resolved_attachments = resolve_attachments(
            &attachments,
            &self.config.frame_registry,
            &session_dir,
            &project_root,
        )
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
    ) {
        let rehydrated_related = load_related_sessions_from_log(&session_dir);
        let mut state = self.state.lock().await;
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
            },
        );
        for rel in rehydrated_related {
            state.apply_related_session(
                &rel.parent_session_id,
                &rel.child_session_id,
                &rel.relationship,
            );
        }
    }

    async fn finish_session(
        &self,
        session_id: String,
        session_log: SharedSessionLog,
        task: String,
        result: Result<LoopStats, CallerError>,
    ) {
        let reason = match &result {
            Ok(stats) => {
                slog(&session_log, |log| {
                    log.write_summary_with_rounds(
                        &task,
                        "completed",
                        stats.turns,
                        Some(stats.rounds),
                    );
                });
                "completed".to_string()
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
            state
                .remove_session(&session_id)
                .map(|(canonical, _)| canonical)
                .unwrap_or_else(|| session_id.clone())
        };

        self.config.bus.send(AppEvent::SessionEnded {
            session_id: ended_session_id.clone(),
            reason,
        });

        if let Some(ref shared_session) = self.config.shared_session {
            let mut state = shared_session.write().await;
            let matches_current = state
                .session_log
                .as_ref()
                .map(|log| {
                    let log_session_id = log.lock().ok().map(|log| log.session_id().to_string());
                    Arc::ptr_eq(log, &session_log)
                        || log_session_id.as_deref() == Some(&session_id)
                        || log_session_id.as_deref() == Some(&ended_session_id)
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
                cfg.web_search = current.web_search;
                cfg.network_access = current.network_access;
                cfg.writable_roots = current.writable_roots;
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

fn normalize_session_agent_command(command: Option<&str>) -> Option<String> {
    command
        .map(str::trim)
        .filter(|command| !command.is_empty())
        .map(ToOwned::to_owned)
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
        | event::ControlMsg::StartTask { session_id, .. }
        | event::ControlMsg::EditUserMessage { session_id, .. }
        | event::ControlMsg::FollowUp { session_id, .. } => session_id.as_deref(),
        event::ControlMsg::RenameSession { session_id, .. } => Some(session_id.as_str()),
        event::ControlMsg::ResumeSession { .. } => None,
        _ => None,
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
        }
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
    fn ignores_non_codex_slash_commands() {
        assert!(parse_codex_slash_command("/help").is_none());
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
    fn normalizes_optional_session_name() {
        assert_eq!(
            normalize_session_name_option(Some("  Dashboard   work  ")).unwrap(),
            Some("Dashboard work".to_string())
        );
        assert_eq!(normalize_session_name_option(Some("   ")).unwrap(), None);
        assert_eq!(normalize_session_name_option(None).unwrap(), None);
    }
}
