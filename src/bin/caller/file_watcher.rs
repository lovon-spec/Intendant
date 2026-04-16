//! Live filesystem watcher: observes file changes in the project directory,
//! stores copy-on-write baseline snapshots, and emits `AppEvent::FileChanged`
//! events. Works for all agent types (native, Codex, Claude Code, Gemini CLI)
//! by watching the filesystem directly rather than relying on git.

use crate::error::CallerError;
use crate::event::{AppEvent, EventBus};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::task::JoinHandle;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeKind {
    Created,
    Modified,
    Deleted,
}

// ---------------------------------------------------------------------------
// Ignore filter
// ---------------------------------------------------------------------------

const IGNORED_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".intendant",
    "__pycache__",
    ".pytest_cache",
    ".codex",
    ".gemini",
    ".claude",
    ".agents",
    "dist",
    "build",
    ".next",
    ".nuxt",
];

const IGNORED_EXTENSIONS: &[&str] = &[
    "o", "so", "dylib", "class", "pyc", "wasm", "exe", "bin", "png", "jpg", "jpeg", "gif",
    "ico", "svg", "webp", "zip", "tar", "gz", "bz2",
];

fn should_ignore(rel_path: &Path) -> bool {
    for component in rel_path.components() {
        if let std::path::Component::Normal(name) = component {
            let name_str = name.to_string_lossy();
            if IGNORED_DIRS.contains(&name_str.as_ref()) {
                return true;
            }
        }
    }
    if let Some(ext) = rel_path.extension() {
        let ext_str = ext.to_string_lossy();
        if IGNORED_EXTENSIONS.contains(&ext_str.as_ref()) {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Returns true if content looks like binary (has a null byte in the first 8KB).
fn is_binary(data: &[u8]) -> bool {
    let check_len = data.len().min(8192);
    data[..check_len].contains(&0)
}

fn sha256_hash(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let result = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&result);
    out
}

/// Produce a unified diff between `baseline` and `current` with standard
/// `--- a/` / `+++ b/` headers and `@@ ... @@` hunk markers.
pub fn compute_unified_diff(baseline: &str, current: &str, path: &str) -> String {
    let diff = similar::TextDiff::from_lines(baseline, current);
    let mut out = String::new();
    out.push_str(&format!("--- a/{}\n", path));
    out.push_str(&format!("+++ b/{}\n", path));
    for hunk in diff.unified_diff().context_radius(3).iter_hunks() {
        out.push_str(&hunk.to_string());
    }
    out
}

/// Count added and removed lines between two text blobs.
fn diff_stats(baseline: &str, current: &str) -> (u32, u32) {
    let diff = similar::TextDiff::from_lines(baseline, current);
    let mut added: u32 = 0;
    let mut removed: u32 = 0;
    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Insert => added += 1,
            similar::ChangeTag::Delete => removed += 1,
            similar::ChangeTag::Equal => {}
        }
    }
    (added, removed)
}

// ---------------------------------------------------------------------------
// FileWatcher
// ---------------------------------------------------------------------------

pub struct FileWatcher {
    project_root: PathBuf,
    snapshot_dir: PathBuf,
    bus: EventBus,
    /// Baseline file content (original at session start), keyed by relative path.
    baselines: HashMap<PathBuf, Vec<u8>>,
    /// SHA-256 hashes of last-known content, for change deduplication.
    hashes: HashMap<PathBuf, [u8; 32]>,
}

impl FileWatcher {
    /// Scan the project tree and build baseline snapshots of all text files.
    pub fn new(
        project_root: PathBuf,
        snapshot_dir: PathBuf,
        bus: EventBus,
    ) -> Result<Self, CallerError> {
        let baseline_dir = snapshot_dir.join("baseline");
        std::fs::create_dir_all(&baseline_dir)
            .map_err(|e| CallerError::Config(format!("create snapshot dir: {}", e)))?;

        let mut baselines = HashMap::new();
        let mut hashes = HashMap::new();

        let mut stack = vec![project_root.clone()];
        while let Some(dir) = stack.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let ft = match entry.file_type() {
                    Ok(ft) => ft,
                    Err(_) => continue,
                };
                if ft.is_dir() {
                    // Check if this directory should be ignored.
                    if let Ok(rel) = path.strip_prefix(&project_root) {
                        if !should_ignore(rel) {
                            stack.push(path);
                        }
                    }
                    continue;
                }
                if !ft.is_file() {
                    continue;
                }
                let rel = match path.strip_prefix(&project_root) {
                    Ok(r) => r.to_path_buf(),
                    Err(_) => continue,
                };
                if should_ignore(&rel) {
                    continue;
                }
                // Check file size (skip >100KB for initial scan).
                let meta = match std::fs::metadata(&path) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                if meta.len() > 100_000 {
                    continue;
                }
                let content = match std::fs::read(&path) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                if is_binary(&content) {
                    continue;
                }
                let hash = sha256_hash(&content);
                baselines.insert(rel.clone(), content);
                hashes.insert(rel, hash);
            }
        }

        Ok(Self {
            project_root,
            snapshot_dir,
            bus,
            baselines,
            hashes,
        })
    }

    /// Spawn the watcher loop as a tokio task. Returns the join handle.
    pub fn start(self) -> JoinHandle<()> {
        tokio::task::spawn(async move {
            if let Err(e) = self.run().await {
                eprintln!("[file_watcher] watcher error: {}", e);
            }
        })
    }

    async fn run(mut self) -> Result<(), CallerError> {
        use notify::Watcher;

        // Bridge notify's std::sync callback into an async tokio channel.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut watcher = notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
            if let Ok(event) = res {
                let _ = tx.send(event);
            }
        })
        .map_err(|e| CallerError::Config(format!("notify watcher init: {}", e)))?;

        watcher
            .watch(&self.project_root, notify::RecursiveMode::Recursive)
            .map_err(|e| CallerError::Config(format!("notify watch: {}", e)))?;

        // Keep `_watcher` alive for the duration of the loop. When the
        // watcher is dropped, the tx side is dropped and rx.recv() returns
        // None, cleanly exiting the loop.
        let _watcher = watcher;

        while let Some(notify_event) = rx.recv().await {
            for path in &notify_event.paths {
                self.process_change(path, &notify_event.kind);
            }
        }

        Ok(())
    }

    fn process_change(&mut self, abs_path: &Path, kind: &notify::EventKind) {
        // Compute relative path.
        let rel = match abs_path.strip_prefix(&self.project_root) {
            Ok(r) => r.to_path_buf(),
            Err(_) => return,
        };

        if should_ignore(&rel) {
            return;
        }

        let change_kind = match kind {
            notify::EventKind::Create(_) => {
                if !abs_path.is_file() {
                    return;
                }
                FileChangeKind::Created
            }
            notify::EventKind::Modify(_) => {
                if !abs_path.is_file() {
                    return;
                }
                FileChangeKind::Modified
            }
            notify::EventKind::Remove(_) => FileChangeKind::Deleted,
            _ => return,
        };

        match change_kind {
            FileChangeKind::Created | FileChangeKind::Modified => {
                let content = match std::fs::read(abs_path) {
                    Ok(c) => c,
                    Err(_) => return, // file gone or permission denied
                };

                // Skip binary files or files >1MB.
                if content.len() > 1_000_000 || is_binary(&content) {
                    return;
                }

                let hash = sha256_hash(&content);
                if self.hashes.get(&rel) == Some(&hash) {
                    return; // no actual change
                }
                self.hashes.insert(rel.clone(), hash);

                // Save baseline if we don't have one yet.
                if !self.baselines.contains_key(&rel) {
                    let baseline_path = self.snapshot_dir.join("baseline").join(&rel);
                    if let Some(parent) = baseline_path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    if change_kind == FileChangeKind::Created {
                        // New file — baseline is empty.
                        let _ = std::fs::write(&baseline_path, b"");
                        self.baselines.insert(rel.clone(), Vec::new());
                    } else {
                        // Modified file we didn't track at startup — save
                        // current content as baseline (best-effort).
                        let _ = std::fs::write(&baseline_path, &content);
                        self.baselines.insert(rel.clone(), content.clone());
                    }
                }

                // Compute diff stats.
                let empty = Vec::new();
                let baseline_bytes = self.baselines.get(&rel).unwrap_or(&empty);
                let baseline_str = String::from_utf8_lossy(baseline_bytes);
                let current_str = String::from_utf8_lossy(&content);
                let (lines_added, lines_removed) = diff_stats(&baseline_str, &current_str);

                self.bus.send(AppEvent::FileChanged {
                    path: rel.to_string_lossy().to_string(),
                    kind: change_kind,
                    lines_added,
                    lines_removed,
                });
            }
            FileChangeKind::Deleted => {
                if self.baselines.contains_key(&rel) || self.hashes.contains_key(&rel) {
                    self.bus.send(AppEvent::FileChanged {
                        path: rel.to_string_lossy().to_string(),
                        kind: FileChangeKind::Deleted,
                        lines_added: 0,
                        lines_removed: 0,
                    });
                }
                self.hashes.remove(&rel);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_ignore() {
        assert!(should_ignore(Path::new(".git/config")));
        assert!(should_ignore(Path::new("target/debug/foo")));
        assert!(should_ignore(Path::new("node_modules/pkg/index.js")));
        assert!(should_ignore(Path::new("src/main.wasm")));
        assert!(should_ignore(Path::new("images/logo.png")));
        assert!(should_ignore(Path::new("archive.tar.gz")));
        assert!(should_ignore(Path::new(".claude/settings.json")));

        assert!(!should_ignore(Path::new("src/main.rs")));
        assert!(!should_ignore(Path::new("Cargo.toml")));
        assert!(!should_ignore(Path::new("README.md")));
        assert!(!should_ignore(Path::new("src/lib.rs")));
    }

    #[test]
    fn test_binary_detection() {
        assert!(is_binary(&[0x00, 0x01, 0x02]));
        assert!(is_binary(b"hello\x00world"));
        assert!(!is_binary(b"hello world"));
        assert!(!is_binary(b"fn main() {}"));
        assert!(!is_binary(b""));
    }

    #[test]
    fn test_compute_unified_diff() {
        let baseline = "line1\nline2\nline3\n";
        let current = "line1\nline2-modified\nline3\nline4\n";
        let diff = compute_unified_diff(baseline, current, "test.txt");

        assert!(diff.contains("--- a/test.txt"));
        assert!(diff.contains("+++ b/test.txt"));
        assert!(diff.contains("@@"));
        assert!(diff.contains("-line2"));
        assert!(diff.contains("+line2-modified"));
        assert!(diff.contains("+line4"));
    }

    #[test]
    fn test_diff_stats() {
        let baseline = "line1\nline2\nline3\n";
        let current = "line1\nline2-modified\nline3\nline4\n";
        let (added, removed) = diff_stats(baseline, current);
        // line2 removed, line2-modified added, line4 added
        assert_eq!(removed, 1);
        assert_eq!(added, 2);
    }

    #[test]
    fn test_diff_stats_no_change() {
        let text = "line1\nline2\n";
        let (added, removed) = diff_stats(text, text);
        assert_eq!(added, 0);
        assert_eq!(removed, 0);
    }

    #[test]
    fn test_diff_stats_all_new() {
        let (added, removed) = diff_stats("", "a\nb\nc\n");
        assert_eq!(added, 3);
        assert_eq!(removed, 0);
    }

    #[test]
    fn test_diff_stats_all_deleted() {
        let (added, removed) = diff_stats("a\nb\nc\n", "");
        assert_eq!(added, 0);
        assert_eq!(removed, 3);
    }
}
