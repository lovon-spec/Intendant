use crate::error::CallerError;
use crate::project::Project;
use crate::sub_agent::{SubAgentProgress, SubAgentRole, SubAgentSpec};
use std::path::{Path, PathBuf};

pub fn spawn_orchestrator_spec(task: &str, project: &Project, _caller_path: &Path) -> SubAgentSpec {
    let sub_agent_dir = project.sub_agent_dir();
    let orch_dir = sub_agent_dir.join("orchestrator");

    SubAgentSpec {
        id: "orchestrator".to_string(),
        task: task.to_string(),
        role: SubAgentRole::Orchestrator,
        working_dir: project.root.clone(),
        result_file: orch_dir.join("result.json"),
        progress_file: orch_dir.join("progress.json"),
        system_prompt: None,
        inherit_memory: true,
    }
}

pub fn format_progress_for_user(progress: &SubAgentProgress) -> String {
    let mut msg = format!("[Status: turn {}, {}]", progress.turn, progress.status);

    if !progress.last_action.is_empty() {
        let action = if progress.last_action.len() > 100 {
            format!("{}...", truncate_utf8(&progress.last_action, 100))
        } else {
            progress.last_action.clone()
        };
        msg.push_str(&format!(" {}", action));
    }

    if let Some(ref question) = progress.question {
        msg.push_str(&format!("\n\nQuestion from orchestrator: {}", question));
    }

    msg
}

#[allow(dead_code)]
pub fn relay_user_input(spec: &SubAgentSpec, input: &str) -> Result<(), CallerError> {
    let command_file = spec.working_dir.join(".intendant").join("user_input.txt");
    if let Some(parent) = command_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&command_file, input)?;
    Ok(())
}

pub fn get_caller_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("intendant")))
        .unwrap_or_else(|| PathBuf::from("./target/debug/intendant"))
}

fn truncate_utf8(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::ProjectConfig;
    use crate::sub_agent::SubAgentProgress;

    fn make_project(root: PathBuf) -> Project {
        Project {
            root,
            config: ProjectConfig::default(),
        }
    }

    #[test]
    fn spawn_orchestrator_spec_basic() {
        let project = make_project(PathBuf::from("/tmp/proj"));
        let spec = spawn_orchestrator_spec(
            "Build the application",
            &project,
            Path::new("/usr/local/bin/caller"),
        );
        assert_eq!(spec.id, "orchestrator");
        assert_eq!(spec.role, SubAgentRole::Orchestrator);
        assert_eq!(spec.task, "Build the application");
        assert_eq!(spec.working_dir, PathBuf::from("/tmp/proj"));
        assert!(spec.inherit_memory);
        assert!(spec.result_file.to_string_lossy().contains("orchestrator"));
        assert!(spec
            .progress_file
            .to_string_lossy()
            .contains("orchestrator"));
    }

    #[test]
    fn format_progress_basic() {
        let progress = SubAgentProgress {
            id: "orch".to_string(),
            turn: 5,
            status: "running".to_string(),
            last_action: "Analyzing codebase".to_string(),
            question: None,
        };
        let msg = format_progress_for_user(&progress);
        assert!(msg.contains("turn 5"));
        assert!(msg.contains("running"));
        assert!(msg.contains("Analyzing codebase"));
        assert!(!msg.contains("Question"));
    }

    #[test]
    fn format_progress_with_question() {
        let progress = SubAgentProgress {
            id: "orch".to_string(),
            turn: 3,
            status: "blocked".to_string(),
            last_action: "Needs clarification".to_string(),
            question: Some("Which database backend?".to_string()),
        };
        let msg = format_progress_for_user(&progress);
        assert!(msg.contains("Question from orchestrator"));
        assert!(msg.contains("Which database backend?"));
    }

    #[test]
    fn format_progress_long_action_truncated() {
        let long_action = "x".repeat(200);
        let progress = SubAgentProgress {
            id: "orch".to_string(),
            turn: 1,
            status: "running".to_string(),
            last_action: long_action,
            question: None,
        };
        let msg = format_progress_for_user(&progress);
        assert!(msg.contains("..."));
        assert!(msg.len() < 300);
    }

    #[test]
    fn relay_user_input_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let spec = SubAgentSpec {
            id: "orch".to_string(),
            task: "test".to_string(),
            role: SubAgentRole::Orchestrator,
            working_dir: dir.path().to_path_buf(),
            result_file: dir.path().join("result.json"),
            progress_file: dir.path().join("progress.json"),
            system_prompt: None,
            inherit_memory: false,
        };

        relay_user_input(&spec, "Use PostgreSQL").unwrap();

        let content =
            std::fs::read_to_string(dir.path().join(".intendant/user_input.txt")).unwrap();
        assert_eq!(content, "Use PostgreSQL");
    }

    #[test]
    fn get_caller_path_returns_path() {
        let path = get_caller_path();
        // Just verify it returns a non-empty path
        assert!(!path.to_string_lossy().is_empty());
    }
}
