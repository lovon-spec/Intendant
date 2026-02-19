use crate::autonomy::ApprovalConfig;
use crate::error::CallerError;
use serde::Deserialize;
use std::path::PathBuf;
use std::process::Command;

#[derive(Debug, Deserialize)]
pub struct MemoryConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, Default)]
#[allow(dead_code)]
pub struct ModelConfig {
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u64>,
}

#[derive(Debug, Deserialize, Default)]
#[allow(dead_code)]
pub struct OrchestratorConfig {
    pub max_parallel_agents: Option<usize>,
    pub sub_agent_dir: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct ProjectConfig {
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    #[allow(dead_code)]
    pub model: ModelConfig,
    #[serde(default)]
    pub orchestrator: OrchestratorConfig,
    #[serde(default)]
    pub approval: ApprovalConfig,
}

#[derive(Debug)]
pub struct Project {
    pub root: PathBuf,
    pub config: ProjectConfig,
}

impl Project {
    pub fn detect() -> Result<Self, CallerError> {
        let root = detect_project_root()?;
        let config_path = root.join("intendant.toml");
        let config = if config_path.exists() {
            let content = std::fs::read_to_string(&config_path).map_err(|e| {
                CallerError::Config(format!("Failed to read intendant.toml: {}", e))
            })?;
            toml::from_str(&content)
                .map_err(|e| CallerError::Toml(format!("Failed to parse intendant.toml: {}", e)))?
        } else {
            ProjectConfig::default()
        };
        Ok(Self { root, config })
    }

    pub fn memory_path(&self) -> PathBuf {
        self.root.join(".intendant").join("memory.json")
    }

    #[allow(dead_code)]
    pub fn agent_dir(&self) -> PathBuf {
        self.root.join(".intendant")
    }

    pub fn sub_agent_dir(&self) -> PathBuf {
        match &self.config.orchestrator.sub_agent_dir {
            Some(dir) => self.root.join(dir),
            None => self.root.join(".intendant").join("subagents"),
        }
    }
}

fn detect_project_root() -> Result<PathBuf, CallerError> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output();

    if let Ok(output) = output {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Ok(PathBuf::from(path));
            }
        }
    }

    std::env::current_dir()
        .map_err(|e| CallerError::Config(format!("Failed to get current directory: {}", e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_project_config() {
        let config = ProjectConfig::default();
        assert!(config.memory.enabled);
        assert!(config.model.context_window.is_none());
        assert!(config.model.max_output_tokens.is_none());
        assert!(config.orchestrator.max_parallel_agents.is_none());
        assert!(config.orchestrator.sub_agent_dir.is_none());
    }

    #[test]
    fn parse_full_config() {
        let toml_str = r#"
[memory]
enabled = true

[model]
context_window = 200000
max_output_tokens = 16384

[orchestrator]
max_parallel_agents = 4
sub_agent_dir = ".custom/agents"
"#;
        let config: ProjectConfig = toml::from_str(toml_str).unwrap();
        assert!(config.memory.enabled);
        assert_eq!(config.model.context_window, Some(200_000));
        assert_eq!(config.model.max_output_tokens, Some(16_384));
        assert_eq!(config.orchestrator.max_parallel_agents, Some(4));
        assert_eq!(
            config.orchestrator.sub_agent_dir.as_deref(),
            Some(".custom/agents")
        );
    }

    #[test]
    fn parse_empty_config() {
        let config: ProjectConfig = toml::from_str("").unwrap();
        assert!(config.memory.enabled); // default_true
        assert!(config.model.context_window.is_none());
    }

    #[test]
    fn parse_partial_config() {
        let toml_str = r#"
[memory]
enabled = false
"#;
        let config: ProjectConfig = toml::from_str(toml_str).unwrap();
        assert!(!config.memory.enabled);
        assert!(config.model.context_window.is_none());
    }

    #[test]
    fn parse_model_config_only() {
        let toml_str = r#"
[model]
context_window = 128000
"#;
        let config: ProjectConfig = toml::from_str(toml_str).unwrap();
        assert!(config.memory.enabled); // default
        assert_eq!(config.model.context_window, Some(128_000));
        assert!(config.model.max_output_tokens.is_none());
    }

    #[test]
    fn project_paths() {
        let project = Project {
            root: PathBuf::from("/tmp/myproject"),
            config: ProjectConfig::default(),
        };
        assert_eq!(
            project.memory_path(),
            PathBuf::from("/tmp/myproject/.intendant/memory.json")
        );
        assert_eq!(
            project.agent_dir(),
            PathBuf::from("/tmp/myproject/.intendant")
        );
        assert_eq!(
            project.sub_agent_dir(),
            PathBuf::from("/tmp/myproject/.intendant/subagents")
        );
    }

    #[test]
    fn sub_agent_dir_custom() {
        let toml_str = r#"
[orchestrator]
sub_agent_dir = ".custom/agents"
"#;
        let config: ProjectConfig = toml::from_str(toml_str).unwrap();
        let project = Project {
            root: PathBuf::from("/tmp/myproject"),
            config,
        };
        assert_eq!(
            project.sub_agent_dir(),
            PathBuf::from("/tmp/myproject/.custom/agents")
        );
    }

    #[test]
    fn parse_orchestrator_config() {
        let toml_str = r#"
[orchestrator]
max_parallel_agents = 8
"#;
        let config: ProjectConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.orchestrator.max_parallel_agents, Some(8));
        assert!(config.orchestrator.sub_agent_dir.is_none());
    }

    #[test]
    fn parse_approval_config() {
        let toml_str = r#"
[approval]
file_read = "auto"
file_write = "ask"
file_delete = "deny"
command_exec = "auto"
network = "ask"
destructive = "deny"
"#;
        let config: ProjectConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(
            config.approval.file_read,
            crate::autonomy::ApprovalRule::Auto
        );
        assert_eq!(
            config.approval.file_write,
            crate::autonomy::ApprovalRule::Ask
        );
        assert_eq!(
            config.approval.file_delete,
            crate::autonomy::ApprovalRule::Deny
        );
        assert_eq!(
            config.approval.destructive,
            crate::autonomy::ApprovalRule::Deny
        );
    }

    #[test]
    fn parse_approval_config_defaults() {
        let config: ProjectConfig = toml::from_str("").unwrap();
        assert_eq!(
            config.approval.file_read,
            crate::autonomy::ApprovalRule::Auto
        );
        assert_eq!(
            config.approval.file_write,
            crate::autonomy::ApprovalRule::Ask
        );
        assert_eq!(
            config.approval.command_exec,
            crate::autonomy::ApprovalRule::Auto
        );
    }
}
