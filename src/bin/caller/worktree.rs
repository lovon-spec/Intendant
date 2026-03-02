use crate::error::CallerError;
use std::path::{Path, PathBuf};
use std::process::Command;

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Worktree {
    pub branch_name: String,
    pub path: PathBuf,
    pub base_branch: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub enum MergeResult {
    Clean,
    Conflict(String),
}

#[allow(dead_code)]
pub fn create(project_root: &Path, branch: &str, base: &str) -> Result<Worktree, CallerError> {
    let worktree_path = project_root
        .join(".intendant")
        .join("worktrees")
        .join(branch);

    let output = Command::new("git")
        .args([
            "worktree",
            "add",
            "-b",
            branch,
            &worktree_path.to_string_lossy(),
            base,
        ])
        .current_dir(project_root)
        .output()
        .map_err(|e| CallerError::SubAgent(format!("Failed to run git worktree add: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CallerError::SubAgent(format!(
            "git worktree add failed: {}",
            stderr.trim()
        )));
    }

    Ok(Worktree {
        branch_name: branch.to_string(),
        path: worktree_path,
        base_branch: base.to_string(),
    })
}

#[allow(dead_code)]
pub fn remove(project_root: &Path, wt: &Worktree) -> Result<(), CallerError> {
    let output = Command::new("git")
        .args(["worktree", "remove", &wt.path.to_string_lossy()])
        .current_dir(project_root)
        .output()
        .map_err(|e| CallerError::SubAgent(format!("Failed to run git worktree remove: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CallerError::SubAgent(format!(
            "git worktree remove failed: {}",
            stderr.trim()
        )));
    }

    // Clean up the branch
    let _ = Command::new("git")
        .args(["branch", "-D", &wt.branch_name])
        .current_dir(project_root)
        .output();

    Ok(())
}

#[allow(dead_code)]
pub fn merge(project_root: &Path, wt: &Worktree, target: &str) -> Result<MergeResult, CallerError> {
    let output = Command::new("git")
        .args(["merge", &wt.branch_name, "--no-edit"])
        .current_dir(project_root)
        .env("GIT_WORK_TREE", project_root)
        .output()
        .map_err(|e| CallerError::SubAgent(format!("Failed to run git merge: {}", e)))?;

    if output.status.success() {
        Ok(MergeResult::Clean)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();

        // Abort the failed merge to leave repo in clean state
        let _ = Command::new("git")
            .args(["merge", "--abort"])
            .current_dir(project_root)
            .output();

        Ok(MergeResult::Conflict(format!(
            "Merge conflict merging {} into {}: {} {}",
            wt.branch_name,
            target,
            stdout.trim(),
            stderr.trim()
        )))
    }
}

#[allow(dead_code)]
pub fn list(project_root: &Path) -> Result<Vec<Worktree>, CallerError> {
    let output = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(project_root)
        .output()
        .map_err(|e| CallerError::SubAgent(format!("Failed to run git worktree list: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CallerError::SubAgent(format!(
            "git worktree list failed: {}",
            stderr.trim()
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut worktrees = Vec::new();
    let mut current_path: Option<PathBuf> = None;
    let mut current_branch: Option<String> = None;

    for line in stdout.lines() {
        if let Some(path_str) = line.strip_prefix("worktree ") {
            current_path = Some(PathBuf::from(path_str));
        } else if let Some(branch_ref) = line.strip_prefix("branch ") {
            // branch refs/heads/branch_name
            let branch_name = branch_ref
                .strip_prefix("refs/heads/")
                .unwrap_or(branch_ref)
                .to_string();
            current_branch = Some(branch_name);
        } else if line.is_empty() {
            if let (Some(path), Some(branch)) = (current_path.take(), current_branch.take()) {
                worktrees.push(Worktree {
                    branch_name: branch,
                    path,
                    base_branch: String::new(), // not available from list output
                });
            }
        }
    }

    // Handle last entry (may not end with empty line)
    if let (Some(path), Some(branch)) = (current_path, current_branch) {
        worktrees.push(Worktree {
            branch_name: branch,
            path,
            base_branch: String::new(),
        });
    }

    Ok(worktrees)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_test_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();

        Command::new("git")
            .args(["init"])
            .current_dir(repo)
            .output()
            .unwrap();

        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(repo)
            .output()
            .unwrap();

        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(repo)
            .output()
            .unwrap();

        // Create initial commit
        std::fs::write(repo.join("README.md"), "# Test\n").unwrap();
        Command::new("git")
            .args(["add", "README.md"])
            .current_dir(repo)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(repo)
            .output()
            .unwrap();

        dir
    }

    #[test]
    fn create_worktree() {
        let dir = init_test_repo();
        let repo = dir.path();

        let wt = create(repo, "feature-1", "HEAD").unwrap();
        assert_eq!(wt.branch_name, "feature-1");
        assert!(wt.path.exists());
        assert_eq!(wt.base_branch, "HEAD");

        // Verify the worktree has files
        assert!(wt.path.join("README.md").exists());
    }

    #[test]
    fn create_worktree_duplicate_branch_fails() {
        let dir = init_test_repo();
        let repo = dir.path();

        create(repo, "dup-branch", "HEAD").unwrap();
        let result = create(repo, "dup-branch", "HEAD");
        assert!(result.is_err());
    }

    #[test]
    fn list_worktrees() {
        let dir = init_test_repo();
        let repo = dir.path();

        create(repo, "list-test-1", "HEAD").unwrap();
        create(repo, "list-test-2", "HEAD").unwrap();

        let wts = list(repo).unwrap();
        // Main worktree + 2 created
        assert!(wts.len() >= 3);

        let branch_names: Vec<&str> = wts.iter().map(|w| w.branch_name.as_str()).collect();
        assert!(branch_names.contains(&"list-test-1"));
        assert!(branch_names.contains(&"list-test-2"));
    }

    #[test]
    fn remove_worktree() {
        let dir = init_test_repo();
        let repo = dir.path();

        let wt = create(repo, "to-remove", "HEAD").unwrap();
        assert!(wt.path.exists());

        remove(repo, &wt).unwrap();
        assert!(!wt.path.exists());
    }

    #[test]
    fn merge_clean() {
        let dir = init_test_repo();
        let repo = dir.path();

        let wt = create(repo, "merge-clean", "HEAD").unwrap();

        // Make a change in the worktree
        std::fs::write(wt.path.join("new_file.txt"), "hello\n").unwrap();
        Command::new("git")
            .args(["add", "new_file.txt"])
            .current_dir(&wt.path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "add new file"])
            .current_dir(&wt.path)
            .output()
            .unwrap();

        // Merge into main
        let result = merge(repo, &wt, "master").unwrap();
        assert_eq!(result, MergeResult::Clean);

        // Verify the file exists in main
        assert!(repo.join("new_file.txt").exists());
    }

    #[test]
    fn merge_conflict() {
        let dir = init_test_repo();
        let repo = dir.path();

        let wt = create(repo, "merge-conflict", "HEAD").unwrap();

        // Modify same file in main
        std::fs::write(repo.join("README.md"), "# Main changes\n").unwrap();
        Command::new("git")
            .args(["add", "README.md"])
            .current_dir(repo)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "main change"])
            .current_dir(repo)
            .output()
            .unwrap();

        // Modify same file in worktree
        std::fs::write(wt.path.join("README.md"), "# Worktree changes\n").unwrap();
        Command::new("git")
            .args(["add", "README.md"])
            .current_dir(&wt.path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "worktree change"])
            .current_dir(&wt.path)
            .output()
            .unwrap();

        // Merge should detect conflict
        let result = merge(repo, &wt, "master").unwrap();
        match result {
            MergeResult::Conflict(msg) => {
                assert!(msg.contains("merge-conflict"));
            }
            MergeResult::Clean => panic!("Expected conflict"),
        }
    }

    #[test]
    fn full_worktree_lifecycle() {
        let dir = init_test_repo();
        let repo = dir.path();

        // Create
        let wt = create(repo, "lifecycle", "HEAD").unwrap();
        assert!(wt.path.exists());

        // Modify
        std::fs::write(wt.path.join("lifecycle.txt"), "test\n").unwrap();
        Command::new("git")
            .args(["add", "lifecycle.txt"])
            .current_dir(&wt.path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "lifecycle change"])
            .current_dir(&wt.path)
            .output()
            .unwrap();

        // List
        let wts = list(repo).unwrap();
        let names: Vec<&str> = wts.iter().map(|w| w.branch_name.as_str()).collect();
        assert!(names.contains(&"lifecycle"));

        // Merge
        let result = merge(repo, &wt, "master").unwrap();
        assert_eq!(result, MergeResult::Clean);

        // Remove
        remove(repo, &wt).unwrap();
        assert!(!wt.path.exists());

        // Verify merged content
        assert!(repo.join("lifecycle.txt").exists());
    }
}
