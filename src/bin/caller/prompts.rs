use crate::error::CallerError;
use crate::sub_agent::SubAgentRole;
use std::path::Path;

const DEFAULT_PROMPT: &str = include_str!("../../../SysPrompt.md");
const DEFAULT_PROMPT_TOOLS: &str = include_str!("../../../SysPrompt_tools.md");
#[allow(dead_code)]
const DEFAULT_USER_PROMPT: &str = include_str!("../../../SysPrompt_user.md");
const DEFAULT_ORCHESTRATOR_PROMPT: &str = include_str!("../../../SysPrompt_orchestrator.md");
const DEFAULT_RESEARCH_PROMPT: &str = include_str!("../../../SysPrompt_research.md");
const DEFAULT_IMPLEMENTATION_PROMPT: &str = include_str!("../../../SysPrompt_implementation.md");

/// Resolve a prompt file using a 3-layer cascade:
/// 1. Project root (`<project_root>/<filename>`)
/// 2. Global config (`~/.config/intendant/<filename>`)
/// 3. Compiled-in default
fn resolve_prompt(filename: &str, default: &str, project_root: Option<&Path>) -> String {
    // 1. Check project root
    if let Some(root) = project_root {
        let path = root.join(filename);
        if let Ok(content) = std::fs::read_to_string(&path) {
            return content;
        }
    }

    // 2. Check ~/.config/intendant/
    if let Some(config_dir) = dirs::config_dir() {
        let path = config_dir.join("intendant").join(filename);
        if let Ok(content) = std::fs::read_to_string(&path) {
            return content;
        }
    }

    // 3. Compiled-in default
    default.to_string()
}

/// Resolve the full system prompt for a given role.
///
/// When `use_tools` is true, resolves the tools-mode prompt (`SysPrompt_tools.md`)
/// which omits JSON schema and per-function docs (those live in native tool definitions).
///
/// Always loads the base prompt, then appends a role-specific prompt for
/// Orchestrator, Research, and Implementation roles.
pub fn resolve_system_prompt(
    role: &SubAgentRole,
    project_root: Option<&Path>,
) -> Result<String, CallerError> {
    resolve_system_prompt_inner(role, project_root, false)
}

/// Like `resolve_system_prompt` but uses the condensed tools-mode base prompt.
pub fn resolve_system_prompt_for_tools(
    role: &SubAgentRole,
    project_root: Option<&Path>,
) -> Result<String, CallerError> {
    resolve_system_prompt_inner(role, project_root, true)
}

fn resolve_system_prompt_inner(
    role: &SubAgentRole,
    project_root: Option<&Path>,
    use_tools: bool,
) -> Result<String, CallerError> {
    let (filename, default) = if use_tools {
        ("SysPrompt_tools.md", DEFAULT_PROMPT_TOOLS)
    } else {
        ("SysPrompt.md", DEFAULT_PROMPT)
    };
    let base_prompt = resolve_prompt(filename, default, project_root);

    let role_addition: Option<(&str, &str)> = match role {
        SubAgentRole::Orchestrator => {
            Some(("SysPrompt_orchestrator.md", DEFAULT_ORCHESTRATOR_PROMPT))
        }
        SubAgentRole::Research => Some(("SysPrompt_research.md", DEFAULT_RESEARCH_PROMPT)),
        SubAgentRole::Implementation => {
            Some(("SysPrompt_implementation.md", DEFAULT_IMPLEMENTATION_PROMPT))
        }
        _ => None,
    };

    match role_addition {
        Some((filename, default)) => {
            let role_prompt = resolve_prompt(filename, default, project_root);
            Ok(format!("{}\n\n{}", base_prompt, role_prompt))
        }
        None => Ok(base_prompt),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiled_defaults_are_non_empty() {
        assert!(!DEFAULT_PROMPT.is_empty());
        assert!(!DEFAULT_PROMPT_TOOLS.is_empty());
        assert!(!DEFAULT_USER_PROMPT.is_empty());
        assert!(!DEFAULT_ORCHESTRATOR_PROMPT.is_empty());
        assert!(!DEFAULT_RESEARCH_PROMPT.is_empty());
        assert!(!DEFAULT_IMPLEMENTATION_PROMPT.is_empty());
    }

    #[test]
    fn tools_prompt_is_shorter_than_default() {
        assert!(DEFAULT_PROMPT_TOOLS.len() < DEFAULT_PROMPT.len());
    }

    #[test]
    fn resolve_prompt_uses_compiled_default() {
        // With no project root and no ~/.config, returns the compiled default
        let result = resolve_prompt("SysPrompt.md", DEFAULT_PROMPT, None);
        assert_eq!(result, DEFAULT_PROMPT);
    }

    #[test]
    fn resolve_prompt_project_root_override() {
        let dir = tempfile::tempdir().unwrap();
        let custom = "Custom project prompt";
        std::fs::write(dir.path().join("SysPrompt.md"), custom).unwrap();

        let result = resolve_prompt("SysPrompt.md", DEFAULT_PROMPT, Some(dir.path()));
        assert_eq!(result, custom);
    }

    #[test]
    fn resolve_prompt_project_root_missing_falls_through() {
        let dir = tempfile::tempdir().unwrap();
        // No SysPrompt.md in the dir — should fall through to default
        let result = resolve_prompt("SysPrompt.md", DEFAULT_PROMPT, Some(dir.path()));
        assert_eq!(result, DEFAULT_PROMPT);
    }

    #[test]
    fn resolve_system_prompt_direct_role() {
        let result = resolve_system_prompt(&SubAgentRole::Custom("direct".into()), None).unwrap();
        // Direct role should return just the base prompt (compiled-in default)
        assert_eq!(result, DEFAULT_PROMPT);
    }

    #[test]
    fn resolve_system_prompt_orchestrator_appends() {
        let result = resolve_system_prompt(&SubAgentRole::Orchestrator, None).unwrap();
        assert!(result.contains(DEFAULT_PROMPT));
        assert!(result.contains(DEFAULT_ORCHESTRATOR_PROMPT));
        assert!(result.len() > DEFAULT_PROMPT.len());
    }

    #[test]
    fn resolve_system_prompt_research_appends() {
        let result = resolve_system_prompt(&SubAgentRole::Research, None).unwrap();
        assert!(result.contains(DEFAULT_PROMPT));
        assert!(result.contains(DEFAULT_RESEARCH_PROMPT));
    }

    #[test]
    fn resolve_system_prompt_implementation_appends() {
        let result = resolve_system_prompt(&SubAgentRole::Implementation, None).unwrap();
        assert!(result.contains(DEFAULT_PROMPT));
        assert!(result.contains(DEFAULT_IMPLEMENTATION_PROMPT));
    }

    #[test]
    fn resolve_system_prompt_testing_returns_base_only() {
        let result = resolve_system_prompt(&SubAgentRole::Testing, None).unwrap();
        assert_eq!(result, DEFAULT_PROMPT);
    }

    #[test]
    fn resolve_system_prompt_project_override_base() {
        let dir = tempfile::tempdir().unwrap();
        let custom_base = "Custom base prompt";
        std::fs::write(dir.path().join("SysPrompt.md"), custom_base).unwrap();

        let result =
            resolve_system_prompt(&SubAgentRole::Custom("direct".into()), Some(dir.path()))
                .unwrap();
        assert_eq!(result, custom_base);
    }

    #[test]
    fn resolve_system_prompt_project_override_role() {
        let dir = tempfile::tempdir().unwrap();
        let custom_orch = "Custom orchestrator instructions";
        std::fs::write(dir.path().join("SysPrompt_orchestrator.md"), custom_orch).unwrap();
        // Base prompt not overridden — should use compiled-in default

        let result = resolve_system_prompt(&SubAgentRole::Orchestrator, Some(dir.path())).unwrap();
        assert!(result.contains(DEFAULT_PROMPT));
        assert!(result.contains(custom_orch));
        assert!(!result.contains(DEFAULT_ORCHESTRATOR_PROMPT));
    }

    #[test]
    fn resolve_system_prompt_project_override_both() {
        let dir = tempfile::tempdir().unwrap();
        let custom_base = "Custom base";
        let custom_research = "Custom research";
        std::fs::write(dir.path().join("SysPrompt.md"), custom_base).unwrap();
        std::fs::write(dir.path().join("SysPrompt_research.md"), custom_research).unwrap();

        let result = resolve_system_prompt(&SubAgentRole::Research, Some(dir.path())).unwrap();
        assert_eq!(result, format!("{}\n\n{}", custom_base, custom_research));
    }

    #[test]
    fn resolve_tools_prompt_direct_role() {
        let result = resolve_system_prompt_for_tools(
            &SubAgentRole::Custom("direct".into()),
            None,
        )
        .unwrap();
        assert_eq!(result, DEFAULT_PROMPT_TOOLS);
        assert!(result.contains("Tool Calling Protocol"));
        assert!(!result.contains("JSON Schema"));
    }

    #[test]
    fn resolve_tools_prompt_with_role_appends() {
        let result =
            resolve_system_prompt_for_tools(&SubAgentRole::Orchestrator, None).unwrap();
        assert!(result.contains(DEFAULT_PROMPT_TOOLS));
        assert!(result.contains(DEFAULT_ORCHESTRATOR_PROMPT));
    }

    #[test]
    fn resolve_tools_prompt_project_override() {
        let dir = tempfile::tempdir().unwrap();
        let custom_tools = "Custom tools prompt";
        std::fs::write(dir.path().join("SysPrompt_tools.md"), custom_tools).unwrap();

        let result = resolve_system_prompt_for_tools(
            &SubAgentRole::Custom("direct".into()),
            Some(dir.path()),
        )
        .unwrap();
        assert_eq!(result, custom_tools);
    }
}
