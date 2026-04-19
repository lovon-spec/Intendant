//! On-disk store for user-uploaded files.
//!
//! Uploads come in two flavours:
//!
//! - **Task-scoped** (default for ephemeral attachments like "look at this
//!   screenshot"). Stored in `<session_dir>/uploads/` so the session zip
//!   bundle picks them up and they disappear when the session is garbage-
//!   collected.
//! - **Workspace-durable** (files the agent should be able to reference
//!   across turns — "read this CSV", "edit this config"). Stored under
//!   `<project_root>/workspace_files/` so the agent's existing filesystem
//!   tools (Read, Grep, etc.) find them under a stable path.
//!
//! The browser-facing POST endpoint picks the destination based on a
//! query param; both variants produce an [`UploadDescriptor`] that the
//! dashboard can attach to a task via `attachments: ["upload:<id>", ...]`
//! on [`crate::event::ControlMsg::StartTask`] / `FollowUp`.

use crate::error::CallerError;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Where an upload lives on disk. Changes the directory we write into and,
/// more importantly, the lifetime policy (session-scoped vs project-scoped).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UploadDestination {
    /// Dropped in the session dir, cleaned up when the session ends.
    Task,
    /// Dropped in the project root under `workspace_files/`, persists across
    /// sessions and is visible to the agent's file-read tools.
    Workspace,
}

impl UploadDestination {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "task" => Some(Self::Task),
            "workspace" => Some(Self::Workspace),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Task => "task",
            Self::Workspace => "workspace",
        }
    }
}

/// Descriptor for a single uploaded file, as returned by the upload endpoint
/// and broadcast via `AppEvent::UploadReady` / `OutboundEvent::UploadReady`.
///
/// The dashboard holds a list of these and passes `id`s back in
/// `ControlMsg::StartTask.attachments` (prefixed `upload:<id>`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadDescriptor {
    /// Stable identifier. Currently a UUIDv4 as a hyphenated string.
    pub id: String,
    /// Original filename from the browser, sanitized for disk use.
    pub name: String,
    /// MIME type the browser sent in `Content-Type`, or
    /// `application/octet-stream` if none.
    pub mime: String,
    /// Size in bytes of the stored file.
    pub size: u64,
    /// Absolute path on disk where the bytes live.
    pub path: PathBuf,
    /// Task- vs workspace-scope.
    pub destination: UploadDestination,
    /// Session that owns this upload (mostly for Task scope; Workspace
    /// uploads still record which session created them for audit).
    pub session_id: String,
    /// Unix epoch seconds when the upload was created.
    pub created_at: u64,
}

impl UploadDescriptor {
    /// True if the upload is an image MIME type (image/png, image/jpeg, ...).
    /// Used by the agent delivery path to decide whether to pass via
    /// `localImage` / ACP image block or fall back to "stage + point".
    pub fn is_image(&self) -> bool {
        self.mime.starts_with("image/")
    }
}

/// Sanitize a user-supplied filename so it's safe to write to disk.
/// Strips any path separators, keeps the extension, and replaces anything
/// outside `[A-Za-z0-9._-]` with an underscore.
pub fn sanitize_name(raw: &str) -> String {
    // Strip any path component the browser may have sent (defence in depth;
    // File objects only expose the basename but we don't trust that).
    let base = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    // Collapse runs of "_" and strip leading dots so we can't write dotfiles.
    let cleaned = cleaned.trim_start_matches('.').to_string();
    if cleaned.is_empty() {
        "upload.bin".to_string()
    } else {
        cleaned
    }
}

/// Directory where task-scoped uploads live.
fn task_uploads_dir(session_dir: &Path) -> PathBuf {
    session_dir.join("uploads")
}

/// Directory where workspace-durable uploads live.
fn workspace_uploads_dir(project_root: &Path) -> PathBuf {
    project_root.join("workspace_files")
}

/// Pick the target directory for a given destination.
fn target_dir(
    destination: UploadDestination,
    session_dir: &Path,
    project_root: &Path,
) -> PathBuf {
    match destination {
        UploadDestination::Task => task_uploads_dir(session_dir),
        UploadDestination::Workspace => workspace_uploads_dir(project_root),
    }
}

/// Commit a pending temp file into the upload store as a new descriptor.
///
/// The caller is responsible for having streamed the bytes into a tempfile
/// with a size cap already applied (so we don't need to reread + measure
/// here). The tempfile is moved (rename-if-possible, otherwise copy+delete)
/// into the target directory under a unique name.
pub fn commit_upload(
    temp_file: tempfile::NamedTempFile,
    original_name: &str,
    mime: &str,
    size: u64,
    destination: UploadDestination,
    session_dir: &Path,
    session_id: &str,
    project_root: &Path,
) -> Result<UploadDescriptor, CallerError> {
    let id = uuid::Uuid::new_v4().to_string();
    let safe_name = sanitize_name(original_name);
    let dir = target_dir(destination, session_dir, project_root);
    fs::create_dir_all(&dir)?;

    // Filename layout:
    //   <id-prefix>__<safe_name>
    // The prefix stops clashes when two files share the same name (common:
    // "screenshot.png"), and keeps the extension intact so downstream tools
    // (agent file-read, OS preview) infer the type correctly.
    let prefix = &id[..id.len().min(8)];
    let filename = format!("{prefix}__{safe_name}");
    let dest_path = dir.join(&filename);

    // Prefer rename (atomic on the same filesystem); fall back to copy if
    // the tempdir lives elsewhere (common on Linux when TMPDIR is tmpfs
    // and the session dir is on a regular disk).
    temp_file
        .persist(&dest_path)
        .map_err(|e| CallerError::Io(e.error))?;

    let created_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let descriptor = UploadDescriptor {
        id,
        name: safe_name,
        mime: mime.to_string(),
        size,
        path: dest_path,
        destination,
        session_id: session_id.to_string(),
        created_at,
    };

    // Write a sidecar .json next to each upload so we can rehydrate
    // descriptors after daemon restart without a central index.
    let sidecar = descriptor.path.with_extension(descriptor_extension(&descriptor.path));
    if let Ok(json) = serde_json::to_string_pretty(&descriptor) {
        let _ = fs::write(&sidecar, json);
    }

    Ok(descriptor)
}

/// Compute the sidecar `.json` path for a given upload path. Keeps both
/// files under the same basename (`<id>__<name>` and `<id>__<name>.json`)
/// so a `ls` of the upload dir lines them up.
fn descriptor_extension(path: &Path) -> String {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) if !ext.is_empty() => format!("{ext}.json"),
        _ => "json".to_string(),
    }
}

/// Read all descriptors currently stored for a session, across both Task
/// and Workspace destinations. Order: newest first (by `created_at`).
pub fn list_uploads(
    session_dir: &Path,
    project_root: &Path,
) -> Vec<UploadDescriptor> {
    let mut out: Vec<UploadDescriptor> = Vec::new();
    for dir in [
        task_uploads_dir(session_dir),
        workspace_uploads_dir(project_root),
    ] {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            // Sidecar JSON files end in `.ext.json` for typed uploads, or
            // just `.json` for extensionless uploads. Both match here.
            if let Ok(content) = fs::read_to_string(&path) {
                if let Ok(descriptor) = serde_json::from_str::<UploadDescriptor>(&content) {
                    out.push(descriptor);
                }
            }
        }
    }
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    out
}

/// Look up a single upload by id. `None` if no descriptor matches.
pub fn find_upload(
    id: &str,
    session_dir: &Path,
    project_root: &Path,
) -> Option<UploadDescriptor> {
    list_uploads(session_dir, project_root)
        .into_iter()
        .find(|u| u.id == id)
}

/// Remove an upload and its sidecar. Returns `Ok(false)` if no descriptor
/// matched (idempotent — the caller can treat "already gone" the same as
/// "just deleted").
pub fn delete_upload(
    id: &str,
    session_dir: &Path,
    project_root: &Path,
) -> io::Result<bool> {
    let Some(descriptor) = find_upload(id, session_dir, project_root) else {
        return Ok(false);
    };
    let sidecar = descriptor.path.with_extension(descriptor_extension(&descriptor.path));
    let _ = fs::remove_file(&sidecar);
    match fs::remove_file(&descriptor.path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn mk_tempfile(bytes: &[u8]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn sanitize_strips_path_components_and_bad_chars() {
        assert_eq!(sanitize_name("/etc/passwd"), "passwd");
        assert_eq!(sanitize_name("..\\..\\foo.txt"), "foo.txt");
        assert_eq!(sanitize_name("hello world!.txt"), "hello_world_.txt");
        assert_eq!(sanitize_name(""), "upload.bin");
        assert_eq!(sanitize_name("...."), "upload.bin");
    }

    #[test]
    fn commit_and_list_task_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("session");
        let project_root = tmp.path().join("project");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::create_dir_all(&project_root).unwrap();

        let pending = mk_tempfile(b"hello world");
        let descriptor = commit_upload(
            pending,
            "notes.txt",
            "text/plain",
            11,
            UploadDestination::Task,
            &session_dir,
            "sess-1",
            &project_root,
        )
        .unwrap();

        assert!(descriptor.path.exists(), "upload file must exist on disk");
        assert!(
            descriptor.path.starts_with(&session_dir),
            "task-scope upload must live under session dir, got {}",
            descriptor.path.display()
        );
        assert_eq!(std::fs::read(&descriptor.path).unwrap(), b"hello world");

        let listed = list_uploads(&session_dir, &project_root);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, descriptor.id);
        assert_eq!(listed[0].name, "notes.txt");
        assert_eq!(listed[0].destination, UploadDestination::Task);
    }

    #[test]
    fn commit_workspace_scope_lands_in_project_root() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("session");
        let project_root = tmp.path().join("project");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::create_dir_all(&project_root).unwrap();

        let pending = mk_tempfile(b"pdf bytes");
        let descriptor = commit_upload(
            pending,
            "report.pdf",
            "application/pdf",
            9,
            UploadDestination::Workspace,
            &session_dir,
            "sess-1",
            &project_root,
        )
        .unwrap();

        assert!(
            descriptor.path.starts_with(project_root.join("workspace_files")),
            "workspace upload must land under workspace_files/, got {}",
            descriptor.path.display()
        );
        // Agent path: file is directly readable via the agent's file-read
        // tool because it's inside the project root.
        assert_eq!(std::fs::read(&descriptor.path).unwrap(), b"pdf bytes");
    }

    #[test]
    fn delete_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("session");
        let project_root = tmp.path().join("project");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::create_dir_all(&project_root).unwrap();

        let pending = mk_tempfile(b"bye");
        let descriptor = commit_upload(
            pending,
            "gone.txt",
            "text/plain",
            3,
            UploadDestination::Task,
            &session_dir,
            "sess-1",
            &project_root,
        )
        .unwrap();

        assert!(delete_upload(&descriptor.id, &session_dir, &project_root).unwrap());
        assert!(!descriptor.path.exists());
        // Second delete: also Ok, returns false.
        assert!(!delete_upload(&descriptor.id, &session_dir, &project_root).unwrap());
    }

    #[test]
    fn is_image_matches_mime_prefix() {
        let mut d = UploadDescriptor {
            id: "x".into(),
            name: "a".into(),
            mime: "image/png".into(),
            size: 0,
            path: PathBuf::new(),
            destination: UploadDestination::Task,
            session_id: "s".into(),
            created_at: 0,
        };
        assert!(d.is_image());
        d.mime = "application/pdf".into();
        assert!(!d.is_image());
        d.mime = "text/plain".into();
        assert!(!d.is_image());
    }
}
