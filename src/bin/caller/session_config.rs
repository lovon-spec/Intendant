use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::external_agent::AgentBackend;
use crate::project::Project;

pub const SESSION_AGENT_CONFIG_FILE: &str = "session_agent_config.json";
const OVERLAY_FILE: &str = "session_agent_config.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionAgentConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_sandbox: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_approval_policy: Option<String>,
    #[serde(
        default,
        alias = "codex_context_recovery",
        skip_serializing_if = "Option::is_none"
    )]
    pub codex_managed_context: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_context_archive: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_service_tier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_home: Option<String>,
}

impl SessionAgentConfig {
    pub fn is_empty(&self) -> bool {
        self.source.is_none()
            && self.project_root.is_none()
            && self.agent_command.is_none()
            && self.codex_sandbox.is_none()
            && self.codex_approval_policy.is_none()
            && self.codex_managed_context.is_none()
            && self.codex_context_archive.is_none()
            && self.codex_service_tier.is_none()
            && self.codex_home.is_none()
    }

    pub fn merge_missing_from(&mut self, fallback: SessionAgentConfig) {
        if self.source.is_none() {
            self.source = fallback.source;
        }
        if self.project_root.is_none() {
            self.project_root = fallback.project_root;
        }
        if self.agent_command.is_none() {
            self.agent_command = fallback.agent_command;
        }
        if self.codex_sandbox.is_none() {
            self.codex_sandbox = fallback.codex_sandbox;
        }
        if self.codex_approval_policy.is_none() {
            self.codex_approval_policy = fallback.codex_approval_policy;
        }
        if self.codex_managed_context.is_none() {
            self.codex_managed_context = fallback.codex_managed_context;
        }
        if self.codex_context_archive.is_none() {
            self.codex_context_archive = fallback.codex_context_archive;
        }
        if self.codex_service_tier.is_none() {
            self.codex_service_tier = fallback.codex_service_tier;
        }
        if self.codex_home.is_none() {
            self.codex_home = fallback.codex_home;
        }
    }
}

pub fn normalize_agent_command(command: Option<&str>) -> Option<String> {
    command
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

pub fn normalize_project_root(root: Option<&str>) -> Option<String> {
    root.map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

pub fn normalize_codex_service_tier(tier: Option<&str>) -> Option<String> {
    crate::project::normalize_codex_service_tier(tier)
}

pub fn normalize_codex_home(home: Option<&str>) -> Option<String> {
    home.map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

pub fn normalize_codex_sandbox(mode: Option<&str>) -> Option<String> {
    let trimmed = mode.map(str::trim).filter(|value| !value.is_empty())?;
    if matches!(trimmed, "inherit" | "default" | "global") {
        return None;
    }
    Some(crate::project::normalize_sandbox_mode(trimmed))
}

pub fn normalize_codex_approval_policy(policy: Option<&str>) -> Option<String> {
    let trimmed = policy.map(str::trim).filter(|value| !value.is_empty())?;
    if matches!(trimmed, "inherit" | "default" | "global") {
        return None;
    }
    Some(crate::project::normalize_approval_policy(trimmed))
}

pub fn effective_codex_home() -> Option<String> {
    let from_env = std::env::var_os("CODEX_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let home = from_env.unwrap_or_else(|| crate::platform::home_dir().join(".codex"));
    normalize_codex_home(Some(&home.to_string_lossy()))
}

pub fn from_wire(
    source: Option<&str>,
    agent_command: Option<&str>,
    codex_sandbox: Option<&str>,
    codex_approval_policy: Option<&str>,
    codex_managed_context: Option<&str>,
    codex_context_archive: Option<&str>,
    codex_service_tier: Option<&str>,
) -> SessionAgentConfig {
    let source = source
        .map(crate::session_names::normalize_source)
        .filter(|value| !value.is_empty());
    let codex_managed_context = match source.as_deref() {
        Some("codex") => codex_managed_context.map(crate::project::normalize_codex_managed_context),
        _ => None,
    };
    let codex_sandbox = match source.as_deref() {
        Some("codex") => normalize_codex_sandbox(codex_sandbox),
        _ => None,
    };
    let codex_approval_policy = match source.as_deref() {
        Some("codex") => normalize_codex_approval_policy(codex_approval_policy),
        _ => None,
    };
    let codex_context_archive = match source.as_deref() {
        Some("codex") => codex_context_archive.map(crate::project::normalize_codex_context_archive),
        _ => None,
    };
    let codex_service_tier = match source.as_deref() {
        Some("codex") => normalize_codex_service_tier(codex_service_tier),
        _ => None,
    };
    SessionAgentConfig {
        source,
        project_root: None,
        agent_command: normalize_agent_command(agent_command),
        codex_sandbox,
        codex_approval_policy,
        codex_managed_context,
        codex_context_archive,
        codex_service_tier,
        codex_home: None,
    }
}

pub fn from_project(backend: &AgentBackend, project: &Project) -> SessionAgentConfig {
    match backend {
        AgentBackend::Codex => SessionAgentConfig {
            source: Some("codex".to_string()),
            project_root: normalize_project_root(Some(&project.root.to_string_lossy())),
            agent_command: Some(project.config.agent.codex.command.clone()),
            codex_sandbox: Some(crate::project::normalize_sandbox_mode(
                &project.config.agent.codex.sandbox,
            )),
            codex_approval_policy: Some(crate::project::normalize_approval_policy(
                &project.config.agent.codex.approval_policy,
            )),
            codex_managed_context: Some(crate::project::normalize_codex_managed_context(
                &project.config.agent.codex.managed_context,
            )),
            codex_context_archive: Some(crate::project::normalize_codex_context_archive(
                &project.config.agent.codex.context_archive,
            )),
            codex_service_tier: crate::project::normalize_codex_service_tier(
                project.config.agent.codex.service_tier.as_deref(),
            ),
            codex_home: effective_codex_home(),
        },
        AgentBackend::ClaudeCode => SessionAgentConfig {
            source: Some("claude-code".to_string()),
            project_root: normalize_project_root(Some(&project.root.to_string_lossy())),
            agent_command: Some(project.config.agent.claude_code.command.clone()),
            codex_sandbox: None,
            codex_approval_policy: None,
            codex_managed_context: None,
            codex_context_archive: None,
            codex_service_tier: None,
            codex_home: None,
        },
        AgentBackend::GeminiCli => SessionAgentConfig {
            source: Some("gemini".to_string()),
            project_root: normalize_project_root(Some(&project.root.to_string_lossy())),
            agent_command: Some(project.config.agent.gemini_cli.command.clone()),
            codex_sandbox: None,
            codex_approval_policy: None,
            codex_managed_context: None,
            codex_context_archive: None,
            codex_service_tier: None,
            codex_home: None,
        },
    }
}

pub fn apply_to_project(
    project: &mut Project,
    backend: &AgentBackend,
    config: &SessionAgentConfig,
) {
    match backend {
        AgentBackend::Codex => {
            if let Some(command) = config.agent_command.clone() {
                project.config.agent.codex.command = command;
            }
            if let Some(mode) = config.codex_sandbox.clone() {
                project.config.agent.codex.sandbox = crate::project::normalize_sandbox_mode(&mode);
            }
            if let Some(policy) = config.codex_approval_policy.clone() {
                project.config.agent.codex.approval_policy =
                    crate::project::normalize_approval_policy(&policy);
            }
            if let Some(mode) = config.codex_managed_context.clone() {
                project.config.agent.codex.managed_context =
                    crate::project::normalize_codex_managed_context(&mode);
            }
            if let Some(mode) = config.codex_context_archive.clone() {
                project.config.agent.codex.context_archive =
                    crate::project::normalize_codex_context_archive(&mode);
            }
        }
        AgentBackend::ClaudeCode => {
            if let Some(command) = config.agent_command.clone() {
                project.config.agent.claude_code.command = command;
            }
        }
        AgentBackend::GeminiCli => {
            if let Some(command) = config.agent_command.clone() {
                project.config.agent.gemini_cli.command = command;
            }
        }
    }
}

pub fn write_log_dir_config(log_dir: &Path, config: &SessionAgentConfig) -> Result<(), String> {
    if config.is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(log_dir).map_err(|e| format!("create session dir: {e}"))?;
    let json =
        serde_json::to_string_pretty(config).map_err(|e| format!("serialize config: {e}"))?;
    crate::file_watcher::atomic_write(&log_dir.join(SESSION_AGENT_CONFIG_FILE), json.as_bytes())
        .map_err(|e| format!("write session config: {e}"))
}

pub fn read_log_dir_config(log_dir: &Path) -> Option<SessionAgentConfig> {
    let raw = std::fs::read_to_string(log_dir.join(SESSION_AGENT_CONFIG_FILE)).ok()?;
    let config: SessionAgentConfig = serde_json::from_str(&raw).ok()?;
    let mut config = normalize_session_agent_config(config, None);
    if config.project_root.is_none() {
        config.project_root = read_log_dir_project_root(log_dir);
    }
    (!config.is_empty()).then_some(config)
}

fn read_log_dir_project_root(log_dir: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(log_dir.join("session_meta.json")).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    normalize_project_root(value.get("project_root").and_then(|v| v.as_str()))
}

fn normalize_session_agent_config(
    mut config: SessionAgentConfig,
    default_source: Option<&str>,
) -> SessionAgentConfig {
    if config.source.is_none() {
        config.source = default_source
            .map(crate::session_names::normalize_source)
            .filter(|source| !source.is_empty());
    }
    if let Some(source) = config.source.take() {
        config.source = Some(crate::session_names::normalize_source(&source));
    }
    if let Some(root) = config.project_root.take() {
        config.project_root = normalize_project_root(Some(&root));
    }
    if let Some(command) = config.agent_command.take() {
        config.agent_command = normalize_agent_command(Some(&command));
    }
    if let Some(mode) = config.codex_sandbox.take() {
        config.codex_sandbox = normalize_codex_sandbox(Some(&mode));
    }
    if let Some(policy) = config.codex_approval_policy.take() {
        config.codex_approval_policy = normalize_codex_approval_policy(Some(&policy));
    }
    if let Some(mode) = config.codex_managed_context.take() {
        config.codex_managed_context = Some(crate::project::normalize_codex_managed_context(&mode));
    }
    if let Some(mode) = config.codex_context_archive.take() {
        config.codex_context_archive = Some(crate::project::normalize_codex_context_archive(&mode));
    }
    if let Some(tier) = config.codex_service_tier.take() {
        config.codex_service_tier = normalize_codex_service_tier(Some(&tier));
    }
    if let Some(home) = config.codex_home.take() {
        config.codex_home = normalize_codex_home(Some(&home));
    }
    config
}

pub fn write_external_overlay(
    home: &Path,
    source: &str,
    session_id: &str,
    config: &SessionAgentConfig,
) -> Result<(), String> {
    write_external_overlay_inner(home, source, session_id, config, true)
}

pub fn replace_external_overlay(
    home: &Path,
    source: &str,
    session_id: &str,
    config: &SessionAgentConfig,
) -> Result<(), String> {
    write_external_overlay_inner(home, source, session_id, config, false)
}

fn write_external_overlay_inner(
    home: &Path,
    source: &str,
    session_id: &str,
    config: &SessionAgentConfig,
    merge_existing: bool,
) -> Result<(), String> {
    let source = crate::session_names::normalize_source(source);
    let session_id = session_id.trim();
    if source == "intendant" || session_id.is_empty() || config.is_empty() {
        return Ok(());
    }

    // Serialize the read-modify-write across intendant processes that share this
    // single global overlay file, so concurrent writers don't lose each other's
    // entries (atomic_write alone prevents torn files, not lost updates).
    with_overlay_lock(home, || {
        let path = overlay_path(home);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create overlay dir: {e}"))?;
        }
        let mut root = match std::fs::read_to_string(&path) {
            Ok(raw) => match serde_json::from_str::<Value>(&raw) {
                Ok(value) => value,
                Err(err) => {
                    // Don't silently reset a corrupt overlay — that would discard
                    // every other session's config. Preserve it for forensics and warn.
                    let backup = path.with_extension("corrupt");
                    let _ = std::fs::rename(&path, &backup);
                    eprintln!(
                        "[session_config] agent-config overlay {} was corrupt ({err}); moved to {} and started fresh",
                        path.display(),
                        backup.display()
                    );
                    Value::Object(Map::new())
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Value::Object(Map::new()),
            Err(err) => return Err(format!("read overlay: {err}")),
        };
        if !root.is_object() {
            root = Value::Object(Map::new());
        }
        let root_obj = root.as_object_mut().expect("root is object");
        let source_value = root_obj
            .entry(source.clone())
            .or_insert_with(|| Value::Object(Map::new()));
        if !source_value.is_object() {
            *source_value = Value::Object(Map::new());
        }
        let source_entries = source_value.as_object_mut().expect("source is object");
        let mut merged = normalize_session_agent_config(config.clone(), Some(&source));
        if merge_existing {
            if let Some(existing) = source_entries
                .get(session_id)
                .and_then(|value| serde_json::from_value::<SessionAgentConfig>(value.clone()).ok())
            {
                merged.merge_missing_from(normalize_session_agent_config(existing, Some(&source)));
            }
        }
        source_entries.insert(
            session_id.to_string(),
            serde_json::to_value(&merged).map_err(|e| format!("serialize config: {e}"))?,
        );
        let json =
            serde_json::to_string_pretty(&root).map_err(|e| format!("serialize overlay: {e}"))?;
        // Atomic write so a concurrent reader never sees a torn file and collapses
        // every other session's managed-context flag to the default.
        crate::file_watcher::atomic_write(&path, json.as_bytes())
            .map_err(|e| format!("write overlay: {e}"))
    })
}

/// Run `write` while holding a best-effort cross-process advisory lock on the
/// shared overlay file, so concurrent intendant processes serialize their
/// read-modify-write. The lock is a pure-safe `O_CREAT|O_EXCL` lock file with a
/// stale-lock timeout (so a crashed holder can't wedge other writers); if it
/// can't be acquired within the bound, the write proceeds unlocked rather than
/// blocking forever.
fn with_overlay_lock<T>(
    home: &Path,
    write: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    use std::time::{Duration, Instant};
    const STALE_AFTER: Duration = Duration::from_secs(5);
    const GIVE_UP_AFTER: Duration = Duration::from_secs(15);
    const POLL: Duration = Duration::from_millis(25);

    let lock_path = overlay_path(home).with_extension("lock");
    if let Some(parent) = lock_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let start = Instant::now();
    let mut acquired = false;
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(_) => {
                acquired = true;
                break;
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = std::fs::metadata(&lock_path)
                    .and_then(|meta| meta.modified())
                    .ok()
                    .and_then(|modified| modified.elapsed().ok())
                    .map(|age| age > STALE_AFTER)
                    .unwrap_or(false);
                if stale {
                    let _ = std::fs::remove_file(&lock_path);
                    continue;
                }
                if start.elapsed() > GIVE_UP_AFTER {
                    break; // proceed unlocked rather than block forever
                }
                std::thread::sleep(POLL);
            }
            Err(_) => break, // cannot create a lock file (perms, etc.) — proceed unlocked
        }
    }
    let result = write();
    if acquired {
        let _ = std::fs::remove_file(&lock_path);
    }
    result
}

pub fn lookup_external_overlay(
    home: &Path,
    source: &str,
    session_id: &str,
) -> Option<SessionAgentConfig> {
    let source = crate::session_names::normalize_source(source);
    let session_id = session_id.trim();
    if source == "intendant" || session_id.is_empty() {
        return None;
    }
    read_overlay_map(home)
        .get(&source)
        .and_then(|by_id| by_id.get(session_id))
        .cloned()
}

pub fn load_for_resume(
    home: &Path,
    source: &str,
    session_id: &str,
    resume_id: Option<&str>,
) -> Option<SessionAgentConfig> {
    let source = crate::session_names::normalize_source(source);
    let ids = [
        resume_id.map(str::trim).filter(|id| !id.is_empty()),
        Some(session_id.trim()).filter(|id| !id.is_empty()),
    ];

    let mut found = SessionAgentConfig::default();
    for id in ids.into_iter().flatten() {
        if let Some(config) = lookup_external_overlay(home, &source, id) {
            found.merge_missing_from(config);
        }
    }
    if let Some(config) =
        find_wrapper_config_for_external_session(home, &source, session_id, resume_id)
    {
        found.merge_missing_from(config);
    }
    if !found.is_empty() {
        return Some(found);
    }
    None
}

pub fn apply_config_to_session_json(session: &mut Value, config: &SessionAgentConfig) {
    let Some(obj) = session.as_object_mut() else {
        return;
    };
    if let Some(source) = config.source.as_deref() {
        obj.entry("configured_source".to_string())
            .or_insert_with(|| Value::String(source.to_string()));
    }
    if let Some(root) = config.project_root.as_deref() {
        let should_insert = obj
            .get("project_root")
            .and_then(|value| value.as_str())
            .map(str::is_empty)
            .unwrap_or(true);
        if should_insert {
            obj.insert("project_root".to_string(), Value::String(root.to_string()));
        }
    }
    if let Some(command) = config.agent_command.as_deref() {
        obj.insert(
            "agent_command".to_string(),
            Value::String(command.to_string()),
        );
        if config.source.as_deref() == Some("codex") {
            obj.insert(
                "codex_command".to_string(),
                Value::String(command.to_string()),
            );
        }
    }
    if let Some(mode) = config.codex_managed_context.as_deref() {
        obj.insert(
            "codex_managed_context".to_string(),
            Value::String(crate::project::normalize_codex_managed_context(mode)),
        );
    }
    if let Some(mode) = config.codex_sandbox.as_deref() {
        obj.insert(
            "codex_sandbox".to_string(),
            Value::String(crate::project::normalize_sandbox_mode(mode)),
        );
    }
    if let Some(policy) = config.codex_approval_policy.as_deref() {
        obj.insert(
            "codex_approval_policy".to_string(),
            Value::String(crate::project::normalize_approval_policy(policy)),
        );
    }
    if let Some(mode) = config.codex_context_archive.as_deref() {
        obj.insert(
            "codex_context_archive".to_string(),
            Value::String(crate::project::normalize_codex_context_archive(mode)),
        );
    }
    if let Some(home) = config.codex_home.as_deref() {
        obj.insert("codex_home".to_string(), Value::String(home.to_string()));
    }
}

pub fn apply_overlays_to_sessions(home: &Path, sessions: &mut [Value]) {
    let overlays = read_overlay_map(home);
    if overlays.is_empty() {
        return;
    }
    for session in sessions {
        let source = session
            .get("source")
            .and_then(|v| v.as_str())
            .map(crate::session_names::normalize_source)
            .unwrap_or_default();
        if source == "intendant" || source.is_empty() {
            continue;
        }
        for key in ["session_id", "resume_id", "backend_session_id"] {
            let Some(session_id) = session.get(key).and_then(|v| v.as_str()) else {
                continue;
            };
            let Some(config) = overlays
                .get(&source)
                .and_then(|by_id| by_id.get(session_id))
            else {
                continue;
            };
            apply_config_to_session_json(session, config);
            break;
        }
    }
}

fn overlay_path(home: &Path) -> PathBuf {
    home.join(".intendant").join(OVERLAY_FILE)
}

fn read_overlay_map(home: &Path) -> HashMap<String, HashMap<String, SessionAgentConfig>> {
    let path = overlay_path(home);
    // Distinguish "absent" (normal — no overlay yet) from "present but unreadable/
    // corrupt". The latter must not be silently collapsed to empty, since that would
    // revert every external session's managed-context flag to the default with no signal.
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return HashMap::new(),
        Err(err) => {
            eprintln!(
                "[session_config] could not read agent-config overlay {}: {err}; sessions keep default managed-context",
                path.display()
            );
            return HashMap::new();
        }
    };
    let value = match serde_json::from_str::<Value>(&raw) {
        Ok(value) => value,
        Err(err) => {
            eprintln!(
                "[session_config] agent-config overlay {} is not valid JSON ({err}); ignoring it",
                path.display()
            );
            return HashMap::new();
        }
    };
    let Some(obj) = value.as_object() else {
        eprintln!(
            "[session_config] agent-config overlay {} is not a JSON object; ignoring it",
            path.display()
        );
        return HashMap::new();
    };
    let mut out = HashMap::new();
    for (source, entries) in obj {
        let source = crate::session_names::normalize_source(source);
        let Some(entries) = entries.as_object() else {
            continue;
        };
        let mut by_id = HashMap::new();
        for (session_id, value) in entries {
            let Ok(config) = serde_json::from_value::<SessionAgentConfig>(value.clone()) else {
                continue;
            };
            let config = normalize_session_agent_config(config, Some(&source));
            if !config.is_empty() {
                by_id.insert(session_id.clone(), config);
            }
        }
        if !by_id.is_empty() {
            out.insert(source, by_id);
        }
    }
    out
}

fn find_wrapper_config_for_external_session(
    home: &Path,
    source: &str,
    session_id: &str,
    resume_id: Option<&str>,
) -> Option<SessionAgentConfig> {
    let logs_dir = home.join(".intendant").join("logs");
    let ids: Vec<String> = [Some(session_id), resume_id]
        .into_iter()
        .flatten()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(ToString::to_string)
        .collect();
    if ids.is_empty() {
        return None;
    }

    let entries = std::fs::read_dir(logs_dir).ok()?;
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let Some(mut config) = read_log_dir_config(&dir) else {
            continue;
        };
        let config_source = config
            .source
            .as_deref()
            .map(crate::session_names::normalize_source)
            .unwrap_or_default();
        if config_source != source {
            continue;
        }
        let jsonl = dir.join("session.jsonl");
        let Ok(contents) = std::fs::read_to_string(jsonl) else {
            continue;
        };
        let mentions = ids.iter().any(|id| contents.contains(id));
        if mentions {
            if config.source.is_none() {
                config.source = Some(source.to_string());
            }
            return Some(config);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_codex_wire_config() {
        let cfg = from_wire(
            Some("Codex"),
            Some("  /tmp/codex  "),
            Some("danger-full-access"),
            Some("on-request"),
            Some("true"),
            Some("raw"),
            Some(" priority "),
        );
        assert_eq!(cfg.source.as_deref(), Some("codex"));
        assert_eq!(cfg.agent_command.as_deref(), Some("/tmp/codex"));
        assert_eq!(cfg.codex_sandbox.as_deref(), Some("danger-full-access"));
        assert_eq!(cfg.codex_approval_policy.as_deref(), Some("on-request"));
        assert_eq!(cfg.codex_managed_context.as_deref(), Some("managed"));
        assert_eq!(cfg.codex_context_archive.as_deref(), Some("exact"));
        assert_eq!(cfg.codex_service_tier.as_deref(), Some("priority"));

        let normal_cfg = from_wire(Some("codex"), None, None, None, None, None, Some("normal"));
        assert_eq!(
            normal_cfg.codex_service_tier.as_deref(),
            Some(crate::project::CODEX_STANDARD_SERVICE_TIER)
        );
    }

    #[test]
    fn overlay_round_trips_external_config() {
        let home = tempfile::tempdir().unwrap();
        let mut cfg = from_wire(
            Some("codex"),
            Some("/tmp/codex"),
            Some("danger-full-access"),
            Some("never"),
            Some("managed"),
            Some("summary"),
            Some("priority"),
        );
        cfg.codex_home = Some("/home/user/.codex-managed".to_string());
        write_external_overlay(home.path(), "codex", "thread-1", &cfg).unwrap();
        let loaded = lookup_external_overlay(home.path(), "codex", "thread-1").unwrap();
        assert_eq!(loaded, cfg);
    }

    #[test]
    fn partial_overlay_write_preserves_existing_launch_sandbox() {
        let home = tempfile::tempdir().unwrap();
        let full = from_wire(
            Some("codex"),
            Some("/tmp/codex"),
            Some("danger-full-access"),
            Some("never"),
            Some("vanilla"),
            Some("summary"),
            None,
        );
        write_external_overlay(home.path(), "codex", "thread-1", &full).unwrap();

        let partial = from_wire(Some("codex"), None, None, None, Some("managed"), None, None);
        write_external_overlay(home.path(), "codex", "thread-1", &partial).unwrap();

        let loaded = lookup_external_overlay(home.path(), "codex", "thread-1").unwrap();
        assert_eq!(loaded.agent_command.as_deref(), Some("/tmp/codex"));
        assert_eq!(loaded.codex_sandbox.as_deref(), Some("danger-full-access"));
        assert_eq!(loaded.codex_approval_policy.as_deref(), Some("never"));
        assert_eq!(loaded.codex_managed_context.as_deref(), Some("managed"));
        assert_eq!(loaded.codex_context_archive.as_deref(), Some("summary"));
    }

    #[test]
    fn replace_overlay_can_clear_launch_sandbox_override() {
        let home = tempfile::tempdir().unwrap();
        let full = from_wire(
            Some("codex"),
            Some("/tmp/codex"),
            Some("danger-full-access"),
            Some("never"),
            Some("vanilla"),
            Some("summary"),
            None,
        );
        write_external_overlay(home.path(), "codex", "thread-1", &full).unwrap();

        let inherit = from_wire(
            Some("codex"),
            Some("/tmp/codex"),
            Some("inherit"),
            Some("inherit"),
            Some("managed"),
            Some("summary"),
            None,
        );
        replace_external_overlay(home.path(), "codex", "thread-1", &inherit).unwrap();

        let loaded = lookup_external_overlay(home.path(), "codex", "thread-1").unwrap();
        assert_eq!(loaded.agent_command.as_deref(), Some("/tmp/codex"));
        assert_eq!(loaded.codex_sandbox, None);
        assert_eq!(loaded.codex_approval_policy, None);
        assert_eq!(loaded.codex_managed_context.as_deref(), Some("managed"));
    }

    #[test]
    fn log_config_round_trips_codex_home_and_applies_to_session_json() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = from_wire(
            Some("codex"),
            Some("/tmp/codex"),
            Some("danger-full-access"),
            Some("never"),
            Some("managed"),
            Some("exact"),
            Some("priority"),
        );
        cfg.project_root = Some("/tmp/intendant-project".to_string());
        cfg.codex_home = Some("  /home/user/.codex-managed  ".to_string());

        write_log_dir_config(dir.path(), &cfg).unwrap();
        let loaded = read_log_dir_config(dir.path()).unwrap();
        assert_eq!(
            loaded.project_root.as_deref(),
            Some("/tmp/intendant-project")
        );
        assert_eq!(
            loaded.codex_home.as_deref(),
            Some("/home/user/.codex-managed")
        );

        let mut session = serde_json::json!({"source": "codex", "session_id": "thread-1"});
        apply_config_to_session_json(&mut session, &loaded);
        assert_eq!(
            session.get("codex_home").and_then(|v| v.as_str()),
            Some("/home/user/.codex-managed")
        );
        assert_eq!(
            session.get("project_root").and_then(|v| v.as_str()),
            Some("/tmp/intendant-project")
        );
        assert_eq!(
            session.get("codex_sandbox").and_then(|v| v.as_str()),
            Some("danger-full-access")
        );
        assert_eq!(
            session
                .get("codex_approval_policy")
                .and_then(|v| v.as_str()),
            Some("never")
        );
    }

    #[test]
    fn log_config_uses_session_meta_project_root_for_legacy_config() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = from_wire(
            Some("codex"),
            Some("/tmp/codex"),
            Some("danger-full-access"),
            Some("never"),
            Some("managed"),
            Some("exact"),
            None,
        );
        write_log_dir_config(dir.path(), &cfg).unwrap();
        std::fs::write(
            dir.path().join("session_meta.json"),
            serde_json::json!({
                "session_id": "wrapper-id",
                "created_at": "2026-06-07T00:00:00Z",
                "project_root": "  /home/user/projects/intendant-station-mainline-123e28c  "
            })
            .to_string(),
        )
        .unwrap();

        let loaded = read_log_dir_config(dir.path()).unwrap();
        assert_eq!(
            loaded.project_root.as_deref(),
            Some("/home/user/projects/intendant-station-mainline-123e28c")
        );
    }

    #[test]
    fn resume_prefers_backend_overlay_over_stale_wrapper_overlay() {
        let home = tempfile::tempdir().unwrap();
        let mut stale_wrapper = from_wire(
            Some("codex"),
            Some("/tmp/stale-wrapper-codex"),
            None,
            None,
            Some("managed"),
            Some("summary"),
            None,
        );
        stale_wrapper.codex_home = Some("/home/user/.codex-wrapper".to_string());
        let mut backend = from_wire(
            Some("codex"),
            Some("/tmp/backend-codex"),
            None,
            None,
            Some("managed"),
            Some("summary"),
            None,
        );
        backend.codex_home = Some("/home/user/.codex-managed".to_string());
        backend.project_root =
            Some("/home/user/projects/intendant-station-mainline-123e28c".into());
        write_external_overlay(home.path(), "codex", "wrapper-id", &stale_wrapper).unwrap();
        write_external_overlay(home.path(), "codex", "backend-thread", &backend).unwrap();

        let loaded =
            load_for_resume(home.path(), "codex", "wrapper-id", Some("backend-thread")).unwrap();
        assert_eq!(loaded.agent_command.as_deref(), Some("/tmp/backend-codex"));
        assert_eq!(
            loaded.project_root.as_deref(),
            Some("/home/user/projects/intendant-station-mainline-123e28c")
        );
        assert_eq!(
            loaded.codex_home.as_deref(),
            Some("/home/user/.codex-managed")
        );
    }

    #[test]
    fn resume_merges_codex_home_from_wrapper_when_backend_overlay_lacks_it() {
        let home = tempfile::tempdir().unwrap();
        let mut wrapper = from_wire(
            Some("codex"),
            Some("/tmp/wrapper-codex"),
            None,
            None,
            Some("managed"),
            Some("summary"),
            None,
        );
        wrapper.codex_home = Some("/home/user/.codex-managed".to_string());
        let wrapper_log_dir = home
            .path()
            .join(".intendant")
            .join("logs")
            .join("wrapper-id");
        write_log_dir_config(&wrapper_log_dir, &wrapper).unwrap();
        std::fs::write(
            wrapper_log_dir.join("session_meta.json"),
            serde_json::json!({
                "session_id": "wrapper-id",
                "created_at": "2026-06-07T00:00:00Z",
                "project_root": "/home/user/projects/intendant-station-mainline-123e28c"
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            wrapper_log_dir.join("session.jsonl"),
            "debug: External agent thread: backend-thread\n",
        )
        .unwrap();
        let backend = from_wire(
            Some("codex"),
            Some("/tmp/backend-codex"),
            None,
            None,
            Some("managed"),
            Some("summary"),
            None,
        );
        write_external_overlay(home.path(), "codex", "backend-thread", &backend).unwrap();

        let loaded =
            load_for_resume(home.path(), "codex", "wrapper-id", Some("backend-thread")).unwrap();
        assert_eq!(loaded.agent_command.as_deref(), Some("/tmp/backend-codex"));
        assert_eq!(
            loaded.project_root.as_deref(),
            Some("/home/user/projects/intendant-station-mainline-123e28c")
        );
        assert_eq!(
            loaded.codex_home.as_deref(),
            Some("/home/user/.codex-managed")
        );
    }

    #[test]
    fn corrupt_overlay_is_preserved_and_overwritten_fresh() {
        let home = tempfile::tempdir().unwrap();
        let path = overlay_path(home.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ not json").unwrap();

        // Writing must not panic or silently wipe the file; it preserves the corrupt
        // copy and starts fresh so the new entry is still readable.
        let cfg = from_wire(
            Some("codex"),
            Some("/tmp/codex"),
            None,
            None,
            Some("managed"),
            Some("off"),
            None,
        );
        write_external_overlay(home.path(), "codex", "thread-1", &cfg).unwrap();

        assert!(path.with_extension("corrupt").exists());
        assert_eq!(
            lookup_external_overlay(home.path(), "codex", "thread-1").unwrap(),
            cfg
        );
        // The lock file is released after the write.
        assert!(!path.with_extension("lock").exists());
    }
}
