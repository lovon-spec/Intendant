//! Daemon-side session lifecycle supervisor.
//!
//! The supervisor is the long-lived owner for sessions launched from the
//! control plane. It accepts `StartTask`, `ResumeSession`, and targeted
//! follow-up commands from the shared `EventBus`, creates per-session runtime
//! resources, and tracks the follow-up channel for each managed session.

use std::collections::HashMap;
use std::path::PathBuf;
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
    active_session_id: Option<String>,
}

struct ManagedSession {
    session_id: String,
    source: String,
    project_root: PathBuf,
    follow_up_tx: mpsc::Sender<String>,
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
                    Ok(AppEvent::ControlCommand(msg)) => {
                        self.handle_control_msg(msg).await;
                    }
                    Ok(_) => {}
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
            event::ControlMsg::StartTask {
                session_id: Some(session_id),
                task,
                direct,
                reference_frame_ids,
                display_target,
                attachments,
                ..
            } => {
                if !reference_frame_ids.is_empty()
                    || display_target.is_some()
                    || !attachments.is_empty()
                {
                    self.warn(&format!(
                        "Targeted StartTask for {} dropped frame/display metadata; routing text only",
                        short_session(&session_id)
                    ));
                }
                self.route_follow_up(Some(session_id), task, direct).await;
            }
            event::ControlMsg::StartTask {
                session_id: None,
                task,
                orchestrate,
                direct,
                reference_frame_ids,
                display_target,
                attachments,
            } => {
                self.start_new_session(
                    task,
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
            } => {
                self.route_follow_up(session_id, text, direct).await;
            }
            _ => {}
        }
    }

    async fn start_new_session(
        &self,
        task: String,
        orchestrate: Option<bool>,
        direct: Option<bool>,
        reference_frame_ids: Vec<String>,
        display_target: Option<String>,
        attachments: Vec<String>,
    ) {
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
        let project = match Project::from_root(self.config.project_root.clone()) {
            Ok(project) => project,
            Err(e) => {
                self.loop_error(format!("Project load failed: {}", e));
                return;
            }
        };

        self.activate_shared_session(session_log.clone()).await;
        self.config.bus.send(AppEvent::SessionStarted {
            session_id: session_id.clone(),
            task: Some(task.clone()),
        });

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
                return;
            }
        }

        let use_direct = direct.unwrap_or(false)
            || orchestrate
                .map(|o| !o)
                .unwrap_or_else(|| self.config.flags_direct || is_simple_task(&task));
        let backend = resolve_agent_backend(&self.config.shared_external_agent, &project).await;
        let project = match self
            .project_with_runtime_config(project.root.clone(), backend.as_ref())
            .await
        {
            Ok(project) => project,
            Err(e) => {
                self.loop_error(format!("Project load failed: {}", e));
                return;
            }
        };
        let attachment_images = resolve_frame_ids(&attachments, &self.config.frame_registry).await;

        emit_task_dispatched_log(&self.config.bus, &session_log, &task, attachments.len());
        self.spawn_agent_session(
            session_id,
            "intendant".to_string(),
            task,
            project,
            session_log,
            log_dir,
            backend,
            use_direct,
            attachment_images,
            None,
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
        let resume_task =
            task.unwrap_or_else(|| format!("Continue the resumed {} session.", source_norm));
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

        self.activate_shared_session(session_log.clone()).await;
        self.config.bus.send(AppEvent::SessionStarted {
            session_id: intendant_session_id.clone(),
            task: Some(resume_task.clone()),
        });

        emit_task_dispatched_log(&self.config.bus, &session_log, &resume_task, 0);
        self.spawn_agent_session(
            intendant_session_id,
            source_norm,
            resume_task,
            project,
            session_log,
            log_dir,
            external_backend,
            direct.unwrap_or(true),
            vec![],
            Some(resume_id.unwrap_or(session_id)),
        )
        .await;
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
        attachment_images: Vec<conversation::ImageData>,
        resume_token: Option<String>,
    ) {
        let (follow_up_tx, follow_up_rx) = mpsc::channel::<String>(16);
        self.register_session(
            session_id.clone(),
            source.clone(),
            project.root.clone(),
            follow_up_tx,
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
                    event::ApprovalRegistry::default(),
                    event::ContextInjectionQueue::default(),
                    true,
                    web_port,
                    attachment_images,
                    resume_token,
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
                        event::ApprovalRegistry::default(),
                        event::ContextInjectionQueue::default(),
                        true,
                        attachment_images,
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
    ) {
        let (target_id, entry) = {
            let state = self.state.lock().await;
            let target_id = session_id.or_else(|| state.active_session_id.clone());
            let Some(target_id) = target_id else {
                drop(state);
                self.warn("FollowUp dropped: no active managed session");
                return;
            };
            let entry = state.sessions.get(&target_id).map(|s| {
                (
                    s.session_id.clone(),
                    s.source.clone(),
                    s.project_root.clone(),
                    s.follow_up_tx.clone(),
                )
            });
            (target_id, entry)
        };

        match entry {
            Some((managed_id, source, project_root, tx)) => {
                if tx.send(text).await.is_err() {
                    self.warn(&format!(
                        "FollowUp dropped: {} session {} in {} is not accepting input",
                        source,
                        short_session(&managed_id),
                        project_root.display()
                    ));
                }
            }
            None => {
                self.warn(&format!(
                    "FollowUp dropped: session {} is not managed by this daemon",
                    short_session(&target_id)
                ));
            }
        }
    }

    async fn register_session(
        &self,
        session_id: String,
        source: String,
        project_root: PathBuf,
        follow_up_tx: mpsc::Sender<String>,
    ) {
        let mut state = self.state.lock().await;
        state.active_session_id = Some(session_id.clone());
        state.sessions.insert(
            session_id.clone(),
            ManagedSession {
                session_id,
                source,
                project_root,
                follow_up_tx,
            },
        );
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

        self.config.bus.send(AppEvent::SessionEnded {
            session_id: session_id.clone(),
            reason,
        });

        {
            let mut state = self.state.lock().await;
            state.sessions.remove(&session_id);
            if state.active_session_id.as_deref() == Some(&session_id) {
                state.active_session_id = state.sessions.keys().next().cloned();
            }
        }

        if let Some(ref shared_session) = self.config.shared_session {
            let mut state = shared_session.write().await;
            let matches_current = state
                .session_log
                .as_ref()
                .and_then(|log| log.lock().ok().map(|log| log.session_id().to_string()))
                .as_deref()
                == Some(&session_id);
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
            level: "warn".to_string(),
            source: "session-supervisor".to_string(),
            content: message.to_string(),
            turn: None,
        });
    }
}

fn path_file_name(path: &std::path::Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("unknown")
        .to_string()
}

fn short_session(session_id: &str) -> String {
    session_id.chars().take(8).collect()
}

fn short_text(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}
