//! Skill discovery, parsing, and invocation.
//!
//! Skills are named instruction sets stored as `SKILL.md` files with YAML
//! frontmatter. They are discovered from two locations (project-scoped first):
//!
//! 1. `<project_root>/.intendant/skills/<name>/SKILL.md`
//! 2. `~/.intendant/skills/<name>/SKILL.md`
//!
//! The model can invoke skills via the `invoke_skill` tool, or the user can
//! trigger them via the control socket / TUI / presence layer.

use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Parsed SKILL.md frontmatter.
#[derive(Debug, Clone, Deserialize)]
pub struct SkillConfig {
    pub name: String,
    pub description: String,
    /// Override session autonomy level when this skill is active.
    #[serde(default)]
    pub autonomy: Option<String>,
    /// If true, the model cannot auto-invoke this skill — user must trigger it.
    #[serde(default, alias = "disable-auto-invocation")]
    pub disable_auto_invocation: bool,
    /// Override session sandbox setting.
    #[serde(default)]
    pub sandbox: Option<bool>,
}

/// Where a skill was discovered.
#[derive(Debug, Clone, PartialEq)]
pub enum SkillSource {
    /// `<project_root>/.intendant/skills/`
    Project,
    /// `~/.intendant/skills/`
    Personal,
}

/// A fully loaded skill.
#[derive(Debug, Clone)]
pub struct Skill {
    pub config: SkillConfig,
    /// Markdown instructions after the frontmatter.
    pub body: String,
    /// Path to the SKILL.md file.
    pub source_path: PathBuf,
    pub source: SkillSource,
}

/// Parse a SKILL.md file's content into config + body.
///
/// The file must start with `---`, followed by YAML frontmatter, closed by
/// another `---` line. Everything after is the body.
fn parse_skill_md(content: &str, source_path: &Path) -> Result<(SkillConfig, String), String> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return Err(format!(
            "{}: missing YAML frontmatter (must start with ---)",
            source_path.display()
        ));
    }

    // Find the closing ---
    let after_first = &trimmed[3..];
    let rest = after_first.trim_start_matches(['\r', '\n']);
    let closing = rest.find("\n---");
    let Some(closing_pos) = closing else {
        return Err(format!(
            "{}: unterminated YAML frontmatter (missing closing ---)",
            source_path.display()
        ));
    };

    let yaml_str = &rest[..closing_pos];
    let body_start = closing_pos + 4; // skip "\n---"
    let body = rest[body_start..].trim_start_matches(['\r', '\n']).to_string();

    // Parse YAML frontmatter manually (flat key-value) to avoid serde_yaml dependency.
    let config = parse_frontmatter(yaml_str).map_err(|e| {
        format!("{}: {}", source_path.display(), e)
    })?;

    Ok((config, body))
}

/// Parse flat YAML-like frontmatter into a SkillConfig.
///
/// Supports simple key: value pairs, `>` block scalars for multi-line strings,
/// and boolean values (true/false). This avoids a serde_yaml dependency for
/// what is deliberately a minimal format.
fn parse_frontmatter(yaml: &str) -> Result<SkillConfig, String> {
    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    let mut autonomy: Option<String> = None;
    let mut disable_auto_invocation = false;
    let mut sandbox: Option<bool> = None;

    let lines: Vec<&str> = yaml.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];

        // Skip blank lines and comments
        if line.trim().is_empty() || line.trim().starts_with('#') {
            i += 1;
            continue;
        }

        // Must be a key: value line
        let Some(colon_pos) = line.find(':') else {
            i += 1;
            continue;
        };

        let key = line[..colon_pos].trim();
        let raw_value = line[colon_pos + 1..].trim();

        // Handle block scalar (key: > or key: |)
        let value = if raw_value == ">" || raw_value == "|" {
            // Collect indented continuation lines
            let mut parts = Vec::new();
            i += 1;
            while i < lines.len() {
                let cont = lines[i];
                if cont.is_empty() || cont.starts_with(' ') || cont.starts_with('\t') {
                    parts.push(cont.trim());
                    i += 1;
                } else {
                    break;
                }
            }
            parts.join(if raw_value == ">" { " " } else { "\n" })
        } else {
            i += 1;
            // Strip surrounding quotes
            let v = raw_value.trim();
            if (v.starts_with('"') && v.ends_with('"'))
                || (v.starts_with('\'') && v.ends_with('\''))
            {
                v[1..v.len() - 1].to_string()
            } else {
                v.to_string()
            }
        };

        match key {
            "name" => name = Some(value),
            "description" => description = Some(value),
            "autonomy" => autonomy = Some(value),
            "disable-auto-invocation" | "disable_auto_invocation" => {
                disable_auto_invocation = value == "true";
            }
            "sandbox" => sandbox = Some(value == "true"),
            _ => {} // Ignore unknown fields for forward compatibility
        }
    }

    let name = name.ok_or("missing required field: name")?;
    let description = description.ok_or("missing required field: description")?;

    Ok(SkillConfig {
        name,
        description,
        autonomy,
        disable_auto_invocation,
        sandbox,
    })
}

/// Discover skills from project and personal directories.
///
/// Project skills take precedence over personal skills with the same name.
pub fn discover_skills(project_root: Option<&Path>) -> Vec<Skill> {
    let mut skills = Vec::new();
    let mut seen_names = std::collections::HashSet::new();

    // 1. Project-scoped skills (check both <root>/skills/ and <root>/.intendant/skills/)
    if let Some(root) = project_root {
        let skills_dir = root.join("skills");
        load_skills_from_dir(&skills_dir, SkillSource::Project, &mut skills, &mut seen_names);
        let dotdir = root.join(".intendant").join("skills");
        load_skills_from_dir(&dotdir, SkillSource::Project, &mut skills, &mut seen_names);
    }

    // 2. Personal skills (~/.intendant/skills/)
    if let Some(home) = dirs::home_dir() {
        let skills_dir = home.join(".intendant").join("skills");
        load_skills_from_dir(&skills_dir, SkillSource::Personal, &mut skills, &mut seen_names);
    }

    skills
}

fn load_skills_from_dir(
    skills_dir: &Path,
    source: SkillSource,
    skills: &mut Vec<Skill>,
    seen_names: &mut std::collections::HashSet<String>,
) {
    let Ok(entries) = std::fs::read_dir(skills_dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let skill_md = path.join("SKILL.md");
        if !skill_md.exists() {
            continue;
        }

        let Ok(content) = std::fs::read_to_string(&skill_md) else {
            eprintln!("Failed to read {}", skill_md.display());
            continue;
        };

        match parse_skill_md(&content, &skill_md) {
            Ok((config, body)) => {
                if seen_names.contains(&config.name) {
                    // Project skills take precedence
                    continue;
                }
                seen_names.insert(config.name.clone());
                skills.push(Skill {
                    config,
                    body,
                    source_path: skill_md,
                    source: source.clone(),
                });
            }
            Err(e) => {
                eprintln!("Skipping skill: {}", e);
            }
        }
    }
}

/// Format a skill catalog for injection into the system prompt / conversation.
///
/// Returns empty string if no skills are available.
pub fn format_skill_catalog(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }

    let auto_skills: Vec<&Skill> = skills
        .iter()
        .filter(|s| !s.config.disable_auto_invocation)
        .collect();

    let manual_skills: Vec<&Skill> = skills
        .iter()
        .filter(|s| s.config.disable_auto_invocation)
        .collect();

    let mut out = String::from("## Available Skills\n\n");
    out.push_str("You can invoke skills using the `invoke_skill` tool.\n\n");

    if !auto_skills.is_empty() {
        out.push_str("**Auto-invocable** (use when the task matches):\n");
        for s in &auto_skills {
            out.push_str(&format!("- **{}**: {}\n", s.config.name, s.config.description));
        }
        out.push('\n');
    }

    if !manual_skills.is_empty() {
        out.push_str("**Manual only** (only invoke when explicitly requested):\n");
        for s in &manual_skills {
            out.push_str(&format!("- **{}**: {}\n", s.config.name, s.config.description));
        }
        out.push('\n');
    }

    out
}

/// Load a skill body with `$ARGUMENTS` substitution.
pub fn load_skill_body(skill: &Skill, arguments: &str) -> String {
    skill.body.replace("$ARGUMENTS", arguments)
}

/// Resolve a skill invocation into a full task string with embedded instructions.
///
/// Used by control socket / TUI / presence to convert an `InvokeSkill` message
/// into a task the agent loop can process directly.
pub fn resolve_skill_as_task(
    skills: &[Skill],
    skill_name: &str,
    arguments: &str,
) -> Result<String, String> {
    let skill = skills
        .iter()
        .find(|s| s.config.name == skill_name)
        .ok_or_else(|| {
            format!(
                "Skill '{}' not found. Available: {}",
                skill_name,
                skills
                    .iter()
                    .map(|s| s.config.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;

    let body = load_skill_body(skill, arguments);
    Ok(format!(
        "[Skill: {}]\n\nFollow these instructions:\n\n{}",
        skill_name, body
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL_SKILL: &str = r#"---
name: deploy-staging
description: Deploy the current branch to staging
autonomy: high
disable-auto-invocation: false
sandbox: true
---

Run the deploy script:
```bash
./scripts/deploy.sh $ARGUMENTS
```
"#;

    const MINIMAL_SKILL: &str = r#"---
name: lint
description: Run linting on changed files
---

Run `cargo clippy` on all targets.
"#;

    const MULTILINE_DESC: &str = r#"---
name: complex-deploy
description: >
  Deploy the current branch to the staging environment
  with full integration test suite
autonomy: low
---

Instructions here.
"#;

    #[test]
    fn parse_full_frontmatter() {
        let (config, body) = parse_skill_md(FULL_SKILL, Path::new("test/SKILL.md")).unwrap();
        assert_eq!(config.name, "deploy-staging");
        assert_eq!(config.description, "Deploy the current branch to staging");
        assert_eq!(config.autonomy, Some("high".to_string()));
        assert!(!config.disable_auto_invocation);
        assert_eq!(config.sandbox, Some(true));
        assert!(body.contains("deploy.sh"));
    }

    #[test]
    fn parse_minimal_frontmatter() {
        let (config, body) = parse_skill_md(MINIMAL_SKILL, Path::new("test/SKILL.md")).unwrap();
        assert_eq!(config.name, "lint");
        assert_eq!(config.description, "Run linting on changed files");
        assert!(config.autonomy.is_none());
        assert!(!config.disable_auto_invocation);
        assert!(config.sandbox.is_none());
        assert!(body.contains("cargo clippy"));
    }

    #[test]
    fn parse_multiline_description() {
        let (config, _body) = parse_skill_md(MULTILINE_DESC, Path::new("test/SKILL.md")).unwrap();
        assert_eq!(config.name, "complex-deploy");
        assert!(config.description.contains("Deploy the current branch"));
        assert!(config.description.contains("integration test suite"));
        assert_eq!(config.autonomy, Some("low".to_string()));
    }

    #[test]
    fn parse_missing_frontmatter() {
        let content = "# Just a markdown file\n\nNo frontmatter here.";
        let result = parse_skill_md(content, Path::new("test/SKILL.md"));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("missing YAML frontmatter"));
    }

    #[test]
    fn parse_unterminated_frontmatter() {
        let content = "---\nname: broken\ndescription: no closing\n";
        let result = parse_skill_md(content, Path::new("test/SKILL.md"));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unterminated"));
    }

    #[test]
    fn parse_missing_required_fields() {
        let content = "---\nname: only-name\n---\nBody.";
        let result = parse_skill_md(content, Path::new("test/SKILL.md"));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("description"));
    }

    #[test]
    fn discover_skills_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let skills = discover_skills(Some(tmp.path()));
        assert!(skills.is_empty());
    }

    #[test]
    fn discover_skills_project() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp
            .path()
            .join(".intendant")
            .join("skills")
            .join("my-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), MINIMAL_SKILL).unwrap();

        let skills = discover_skills(Some(tmp.path()));
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].config.name, "lint");
        assert_eq!(skills[0].source, SkillSource::Project);
    }

    #[test]
    fn format_catalog_empty() {
        assert_eq!(format_skill_catalog(&[]), "");
    }

    #[test]
    fn format_catalog_multiple() {
        let skills = vec![
            Skill {
                config: SkillConfig {
                    name: "deploy".to_string(),
                    description: "Deploy to staging".to_string(),
                    autonomy: None,
                    disable_auto_invocation: false,
                    sandbox: None,
                },
                body: String::new(),
                source_path: PathBuf::new(),
                source: SkillSource::Project,
            },
            Skill {
                config: SkillConfig {
                    name: "test-e2e".to_string(),
                    description: "Run E2E tests".to_string(),
                    autonomy: None,
                    disable_auto_invocation: true,
                    sandbox: None,
                },
                body: String::new(),
                source_path: PathBuf::new(),
                source: SkillSource::Personal,
            },
        ];

        let catalog = format_skill_catalog(&skills);
        assert!(catalog.contains("deploy"));
        assert!(catalog.contains("test-e2e"));
        assert!(catalog.contains("Auto-invocable"));
        assert!(catalog.contains("Manual only"));
    }

    #[test]
    fn argument_substitution() {
        let skill = Skill {
            config: SkillConfig {
                name: "deploy".to_string(),
                description: "Deploy".to_string(),
                autonomy: None,
                disable_auto_invocation: false,
                sandbox: None,
            },
            body: "Deploy $ARGUMENTS to staging.".to_string(),
            source_path: PathBuf::new(),
            source: SkillSource::Project,
        };

        assert_eq!(
            load_skill_body(&skill, "production"),
            "Deploy production to staging."
        );
    }

    #[test]
    fn argument_substitution_no_placeholder() {
        let skill = Skill {
            config: SkillConfig {
                name: "lint".to_string(),
                description: "Lint".to_string(),
                autonomy: None,
                disable_auto_invocation: false,
                sandbox: None,
            },
            body: "Just run clippy.".to_string(),
            source_path: PathBuf::new(),
            source: SkillSource::Project,
        };

        assert_eq!(load_skill_body(&skill, "ignored"), "Just run clippy.");
    }

    #[test]
    fn resolve_skill_found() {
        let skills = vec![Skill {
            config: SkillConfig {
                name: "deploy".to_string(),
                description: "Deploy".to_string(),
                autonomy: None,
                disable_auto_invocation: false,
                sandbox: None,
            },
            body: "Deploy $ARGUMENTS now.".to_string(),
            source_path: PathBuf::new(),
            source: SkillSource::Project,
        }];

        let result = resolve_skill_as_task(&skills, "deploy", "staging");
        assert!(result.is_ok());
        let task = result.unwrap();
        assert!(task.contains("[Skill: deploy]"));
        assert!(task.contains("Deploy staging now."));
    }

    #[test]
    fn resolve_skill_not_found() {
        let skills = vec![Skill {
            config: SkillConfig {
                name: "deploy".to_string(),
                description: "Deploy".to_string(),
                autonomy: None,
                disable_auto_invocation: false,
                sandbox: None,
            },
            body: String::new(),
            source_path: PathBuf::new(),
            source: SkillSource::Project,
        }];

        let result = resolve_skill_as_task(&skills, "nonexistent", "");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[test]
    fn parse_quoted_values() {
        let content = "---\nname: \"quoted-name\"\ndescription: 'single quoted'\n---\nBody.";
        let (config, _) = parse_skill_md(content, Path::new("test/SKILL.md")).unwrap();
        assert_eq!(config.name, "quoted-name");
        assert_eq!(config.description, "single quoted");
    }
}
