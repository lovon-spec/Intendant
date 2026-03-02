use std::path::{Path, PathBuf};

/// Configuration for Landlock filesystem sandboxing.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SandboxConfig {
    /// Paths the sandboxed process may read.
    pub read_paths: Vec<PathBuf>,
    /// Paths the sandboxed process may write (implies read).
    pub write_paths: Vec<PathBuf>,
    /// Whether sandboxing is enabled.
    pub enabled: bool,
}

#[allow(dead_code)]
impl SandboxConfig {
    /// Build a default config for the given project.
    /// - Read: `/` (everything)
    /// - Write: project root, `/tmp`, log directory, home `.intendant`
    pub fn default_for_project(project_root: &Path, log_dir: &Path) -> Self {
        let mut write_paths = vec![
            project_root.to_path_buf(),
            PathBuf::from("/tmp"),
            log_dir.to_path_buf(),
        ];

        // Allow writes to ~/.intendant
        if let Some(home) = dirs::home_dir() {
            write_paths.push(home.join(".intendant"));
        }

        Self {
            read_paths: vec![PathBuf::from("/")],
            write_paths,
            enabled: true,
        }
    }

    /// Apply Landlock restrictions to the current process.
    /// Returns Ok(true) if restrictions were applied, Ok(false) if Landlock
    /// is not supported by the kernel, Err on actual errors.
    pub fn apply_to_current_process(&self) -> Result<bool, String> {
        if !self.enabled {
            return Ok(false);
        }

        #[cfg(target_os = "linux")]
        {
            use landlock::{
                AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr, ABI,
            };

            let abi = ABI::V5;

            let read_access = AccessFs::from_read(abi);
            let write_access = AccessFs::from_read(abi) | AccessFs::from_write(abi);

            let mut ruleset_created = Ruleset::default()
                .handle_access(write_access)
                .map_err(|e| format!("Landlock ruleset creation failed: {}", e))?
                .create()
                .map_err(|e| format!("Landlock ruleset create failed: {}", e))?;

            // Add read-only paths
            for path in &self.read_paths {
                if path.exists() {
                    if let Ok(fd) = PathFd::new(path) {
                        let rule = PathBeneath::new(fd, read_access);
                        ruleset_created = ruleset_created
                            .add_rule(rule)
                            .map_err(|e| format!("Landlock add read rule failed: {}", e))?;
                    }
                }
            }

            // Add read-write paths
            for path in &self.write_paths {
                if path.exists() {
                    if let Ok(fd) = PathFd::new(path) {
                        let rule = PathBeneath::new(fd, write_access);
                        ruleset_created = ruleset_created
                            .add_rule(rule)
                            .map_err(|e| format!("Landlock add write rule failed: {}", e))?;
                    }
                }
            }

            let status = ruleset_created
                .restrict_self()
                .map_err(|e| format!("Landlock restrict_self failed: {}", e))?;

            Ok(status.ruleset != landlock::RulesetStatus::NotEnforced)
        }

        #[cfg(not(target_os = "linux"))]
        {
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_includes_project_and_tmp() {
        let config = SandboxConfig::default_for_project(
            Path::new("/home/user/project"),
            Path::new("/tmp/logs"),
        );
        assert!(config.enabled);
        assert!(config
            .write_paths
            .contains(&PathBuf::from("/home/user/project")));
        assert!(config.write_paths.contains(&PathBuf::from("/tmp")));
        assert!(config.write_paths.contains(&PathBuf::from("/tmp/logs")));
        assert!(config.read_paths.contains(&PathBuf::from("/")));
    }

    #[test]
    fn disabled_config_skips_apply() {
        let mut config =
            SandboxConfig::default_for_project(Path::new("/tmp/test"), Path::new("/tmp/logs"));
        config.enabled = false;
        assert_eq!(config.apply_to_current_process().unwrap(), false);
    }

    #[test]
    fn config_has_write_paths() {
        let config = SandboxConfig::default_for_project(
            Path::new("/home/user/myproject"),
            Path::new("/var/log/intendant"),
        );
        assert!(config.write_paths.len() >= 3);
    }
}
