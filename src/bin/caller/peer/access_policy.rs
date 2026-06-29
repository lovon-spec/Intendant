//! Peer relationship policy.
//!
//! Pairing produces a daemon-to-daemon mTLS identity; this module gives that
//! identity human meaning. Approved peer client certificates are recorded by
//! fingerprint with a peer profile. The gateway can then authorize daemon-mode HTTP/WS
//! operations from the certificate fingerprint instead of treating every cert
//! signed by the access CA as equivalent.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::CallerError;
use crate::event::ControlMsg;

pub const DEFAULT_PROFILE: &str = "peer-operator";
const POLICY_DIR: &str = "peer-access-identities";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PeerIdentityStatus {
    Approved,
    Revoked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerIdentityRecord {
    pub version: u8,
    pub fingerprint: String,
    pub label: String,
    pub profile: String,
    pub status: PeerIdentityStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub card_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "FilesystemAccessPolicy::is_empty")]
    pub filesystem: FilesystemAccessPolicy,
    pub created_at_unix: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at_unix: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FilesystemAccessPolicy {
    #[serde(default)]
    pub read_roots: Vec<PathBuf>,
    #[serde(default)]
    pub write_roots: Vec<PathBuf>,
}

impl FilesystemAccessPolicy {
    pub fn is_empty(&self) -> bool {
        self.read_roots.is_empty() && self.write_roots.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileClass {
    PresenceOnly,
    Stats,
    SessionReader,
    ReadOnlyDisplay,
    SharedSessionSpectator,
    FileReader,
    FileOperator,
    TerminalOperator,
    TaskRunner,
    Operator,
    AdminPeer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerOperation {
    PresenceRead,
    StatsRead,
    DisplayView,
    DisplayInput,
    Message,
    Task,
    Approval,
    PeerManage,
    SessionInspect,
    SessionManage,
    Terminal,
    Settings,
    RuntimeControl,
    FilesystemRead,
    FilesystemWrite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilesystemAccessKind {
    Read,
    Write,
}

pub fn normalize_profile(raw: &str) -> Result<String, CallerError> {
    let profile = raw.trim();
    if profile.is_empty() {
        return Err(CallerError::Config("profile cannot be empty".into()));
    }
    if profile.len() > 64 {
        return Err(CallerError::Config(
            "profile must be at most 64 bytes".into(),
        ));
    }
    let valid = profile
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b':' | b'.'));
    if !valid {
        return Err(CallerError::Config(
            "profile may contain only letters, numbers, '-', '_', ':', or '.'".into(),
        ));
    }
    Ok(profile.to_ascii_lowercase())
}

pub fn profile_class(profile: &str) -> ProfileClass {
    match profile.trim().to_ascii_lowercase().as_str() {
        "presence-only" | "presence" => ProfileClass::PresenceOnly,
        "stats" | "stats-only" => ProfileClass::Stats,
        "session-reader" | "sessions-read" | "session-inspect" | "logs-read" => {
            ProfileClass::SessionReader
        }
        "read-only-display" | "display-read-only" => ProfileClass::ReadOnlyDisplay,
        "shared-session-spectator" | "spectator" => ProfileClass::SharedSessionSpectator,
        "file-reader" | "files-read" | "filesystem-read-only" => ProfileClass::FileReader,
        "file-operator" | "files" | "filesystem-operator" => ProfileClass::FileOperator,
        "terminal-operator" | "peer-terminal-operator" | "terminal" | "shell" => {
            ProfileClass::TerminalOperator
        }
        "task-runner" => ProfileClass::TaskRunner,
        "operator" | "peer-operator" => ProfileClass::Operator,
        "peer-root" | "admin-peer" | "admin" | "peer-daemon" => ProfileClass::AdminPeer,
        _ => ProfileClass::PresenceOnly,
    }
}

pub fn profile_allows_operation(profile: &str, op: PeerOperation) -> bool {
    use PeerOperation::*;
    use ProfileClass::*;

    match profile_class(profile) {
        PresenceOnly => matches!(op, PresenceRead),
        Stats => matches!(op, PresenceRead | StatsRead),
        SessionReader => matches!(op, PresenceRead | StatsRead | SessionInspect),
        ReadOnlyDisplay => matches!(op, PresenceRead | StatsRead | DisplayView),
        SharedSessionSpectator => {
            matches!(op, PresenceRead | StatsRead | DisplayView | SessionInspect)
        }
        FileReader => matches!(op, PresenceRead | StatsRead | FilesystemRead),
        FileOperator => matches!(
            op,
            PresenceRead | StatsRead | FilesystemRead | FilesystemWrite
        ),
        TerminalOperator => matches!(op, PresenceRead | StatsRead | SessionInspect | Terminal),
        TaskRunner => matches!(op, PresenceRead | StatsRead | Message | Task),
        Operator => matches!(
            op,
            PresenceRead
                | StatsRead
                | SessionInspect
                | DisplayView
                | DisplayInput
                | Message
                | Task
                | Approval
        ),
        AdminPeer => true,
    }
}

pub fn profile_allows_control_msg(profile: &str, ctrl: &ControlMsg) -> bool {
    let op = control_msg_operation(ctrl);
    profile_allows_operation(profile, op)
}

pub fn control_msg_operation(ctrl: &ControlMsg) -> PeerOperation {
    match ctrl {
        ControlMsg::Status { .. } => PeerOperation::PresenceRead,
        ControlMsg::Usage => PeerOperation::StatsRead,
        ControlMsg::WebRtcSignal { .. } => PeerOperation::DisplayView,
        ControlMsg::PeerDashboardControlSignal { .. } => PeerOperation::SessionInspect,
        ControlMsg::PeerFileTransferSignal { .. } => PeerOperation::FilesystemRead,
        ControlMsg::RequestDisplayInputAuthority { .. }
        | ControlMsg::ReleaseDisplayInputAuthority { .. }
        | ControlMsg::TakeDisplay { .. }
        | ControlMsg::ReleaseDisplay { .. }
        | ControlMsg::GrantUserDisplay { .. }
        | ControlMsg::RevokeUserDisplay { .. }
        | ControlMsg::SetDiagnosticsVisualMarker { .. } => PeerOperation::DisplayInput,
        ControlMsg::Input { .. }
        | ControlMsg::FollowUp { .. }
        | ControlMsg::CancelFollowUp { .. } => PeerOperation::Message,
        ControlMsg::StartTask { .. }
        | ControlMsg::CreateSession { .. }
        | ControlMsg::ResumeSession { .. }
        | ControlMsg::EditUserMessage { .. } => PeerOperation::Task,
        ControlMsg::Approve { .. }
        | ControlMsg::Deny { .. }
        | ControlMsg::Skip { .. }
        | ControlMsg::ApproveAll { .. } => PeerOperation::Approval,
        ControlMsg::SetAutonomy { .. }
        | ControlMsg::SetApprovalRule { .. }
        | ControlMsg::SetExternalAgent { .. }
        | ControlMsg::SetCodexCommand { .. }
        | ControlMsg::SetCodexManagedCommand { .. }
        | ControlMsg::SetCodexSandbox { .. }
        | ControlMsg::SetCodexApprovalPolicy { .. }
        | ControlMsg::SetCodexModel { .. }
        | ControlMsg::SetCodexReasoningEffort { .. }
        | ControlMsg::SetCodexServiceTier { .. }
        | ControlMsg::SetCodexWebSearch { .. }
        | ControlMsg::SetCodexNetworkAccess { .. }
        | ControlMsg::SetCodexWritableRoots { .. }
        | ControlMsg::SetCodexManagedContext { .. }
        | ControlMsg::SetCodexContextArchive { .. }
        | ControlMsg::ConfigureSessionAgent { .. }
        | ControlMsg::SetGeminiModel { .. }
        | ControlMsg::SetGeminiApprovalMode { .. }
        | ControlMsg::SetGeminiSandbox { .. }
        | ControlMsg::SetGeminiExtensions { .. }
        | ControlMsg::SetGeminiAllowedMcpServers { .. }
        | ControlMsg::SetGeminiIncludeDirectories { .. }
        | ControlMsg::SetGeminiDebug { .. }
        | ControlMsg::SetVerbosity { .. } => PeerOperation::Settings,
        ControlMsg::CodexThreadAction { .. }
        | ControlMsg::GeminiThreadAction { .. }
        | ControlMsg::RenameSession { .. }
        | ControlMsg::StopSession { .. }
        | ControlMsg::RestartSession { .. }
        | ControlMsg::Interrupt { .. } => PeerOperation::SessionManage,
        ControlMsg::Steer { .. } | ControlMsg::CancelSteer { .. } => PeerOperation::Message,
        ControlMsg::ListDisplays => PeerOperation::DisplayView,
        ControlMsg::QueryDetail { .. } => PeerOperation::StatsRead,
        ControlMsg::CreateBrowserWorkspace { .. }
        | ControlMsg::CloseBrowserWorkspace { .. }
        | ControlMsg::AcquireBrowserWorkspace { .. }
        | ControlMsg::ReleaseBrowserWorkspace { .. }
        | ControlMsg::RecallMemory { .. }
        | ControlMsg::InvokeSkill { .. }
        | ControlMsg::Quit
        | ControlMsg::SetupDebugScreen
        | ControlMsg::TeardownDebugScreen
        | ControlMsg::StartDebugRecording
        | ControlMsg::StopDebugRecording
        | ControlMsg::StartRecording { .. }
        | ControlMsg::StopRecording { .. }
        | ControlMsg::DeleteRecording { .. } => PeerOperation::RuntimeControl,
        ControlMsg::ScheduleControllerRestart { .. }
        | ControlMsg::ControllerTurnComplete { .. }
        | ControlMsg::GetRestartStatus
        | ControlMsg::CancelControllerRestart { .. }
        | ControlMsg::RequestControllerLoopHalt { .. }
        | ControlMsg::ClearControllerLoopHalt
        | ControlMsg::InterveneControllerLoop { .. }
        | ControlMsg::GetControllerLoopStatus => PeerOperation::RuntimeControl,
    }
}

pub fn profile_allows_federated_display_input(profile: &str) -> bool {
    profile_allows_operation(profile, PeerOperation::DisplayInput)
}

pub fn filesystem_access_allowed(
    policy: &FilesystemAccessPolicy,
    kind: FilesystemAccessKind,
    path: &Path,
) -> Result<(), String> {
    let root_candidates: Vec<&PathBuf> = match kind {
        FilesystemAccessKind::Read => policy
            .read_roots
            .iter()
            .chain(policy.write_roots.iter())
            .collect(),
        FilesystemAccessKind::Write => policy.write_roots.iter().collect(),
    };
    if root_candidates.is_empty() {
        return Err(match kind {
            FilesystemAccessKind::Read => "peer identity has no filesystem read roots".to_string(),
            FilesystemAccessKind::Write => {
                "peer identity has no filesystem write roots".to_string()
            }
        });
    }

    let access_subject = match kind {
        FilesystemAccessKind::Read => path.to_path_buf(),
        FilesystemAccessKind::Write => nearest_existing_path(path)
            .ok_or_else(|| format!("{} has no existing parent", path.display()))?,
    };
    let canonical_subject = std::fs::canonicalize(&access_subject)
        .map_err(|e| format!("{} is not accessible: {e}", access_subject.display()))?;

    for root in root_candidates {
        let canonical_root = match std::fs::canonicalize(root) {
            Ok(root) => root,
            Err(_) => continue,
        };
        if canonical_subject == canonical_root || canonical_subject.starts_with(&canonical_root) {
            return Ok(());
        }
    }

    Err(format!(
        "{} is outside this peer identity's filesystem roots",
        canonical_subject.display()
    ))
}

pub fn profile_allows_federation_http(profile: &str, request_line: &str) -> bool {
    if request_line.contains(" /api/peers/pairing/") {
        return profile_allows_operation(profile, PeerOperation::PeerManage);
    }
    if request_line.contains(" /api/peers") {
        if request_line.starts_with("GET") {
            return profile_allows_operation(profile, PeerOperation::PresenceRead);
        }
        return profile_allows_operation(profile, PeerOperation::PeerManage);
    }
    if request_line.contains(" /api/coordinator/") {
        return profile_allows_operation(profile, PeerOperation::Task);
    }
    if request_line.contains(" /api/sessions") {
        return profile_allows_operation(profile, PeerOperation::SessionInspect);
    }
    if request_line.contains(" /api/worktrees") {
        return profile_allows_operation(profile, PeerOperation::SessionInspect);
    }
    true
}

pub fn write_approved_identity(
    cert_dir: &Path,
    fingerprint: &str,
    label: &str,
    profile: &str,
    card_url: Option<&str>,
    request_id: Option<&str>,
) -> Result<PeerIdentityRecord, CallerError> {
    let fingerprint = normalize_fingerprint(fingerprint)?;
    let profile = normalize_profile(profile)?;
    let record = PeerIdentityRecord {
        version: 1,
        fingerprint,
        label: label.trim().to_string(),
        profile,
        status: PeerIdentityStatus::Approved,
        card_url: card_url.map(str::to_string),
        request_id: request_id.map(str::to_string),
        filesystem: FilesystemAccessPolicy::default(),
        created_at_unix: crate::peer::pairing::unix_timestamp(),
        revoked_at_unix: None,
    };
    write_identity_record(cert_dir, &record)?;
    Ok(record)
}

pub fn lookup_identity(
    cert_dir: &Path,
    fingerprint: &str,
) -> Result<Option<PeerIdentityRecord>, CallerError> {
    let fingerprint = normalize_fingerprint(fingerprint)?;
    let path = identity_path(cert_dir, &fingerprint);
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path)?;
    let record: PeerIdentityRecord = serde_json::from_str(&text)?;
    Ok(Some(record))
}

pub fn list_identities(cert_dir: &Path) -> Result<Vec<PeerIdentityRecord>, CallerError> {
    let dir = identities_dir(cert_dir);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out: Vec<PeerIdentityRecord> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_file()
            && entry.path().extension().and_then(|s| s.to_str()) == Some("json")
        {
            let text = std::fs::read_to_string(entry.path())?;
            out.push(serde_json::from_str(&text)?);
        }
    }
    out.sort_by(|a, b| {
        a.label
            .cmp(&b.label)
            .then(a.fingerprint.cmp(&b.fingerprint))
    });
    Ok(out)
}

pub fn revoke_identity(
    cert_dir: &Path,
    fingerprint_or_label: &str,
) -> Result<PeerIdentityRecord, CallerError> {
    let needle = fingerprint_or_label.trim();
    if needle.is_empty() {
        return Err(CallerError::Config("peer identity is required".into()));
    }
    let mut record = if let Ok(fp) = normalize_fingerprint(needle) {
        lookup_identity(cert_dir, &fp)?.ok_or_else(|| {
            CallerError::Config(format!("no peer identity found for fingerprint {needle}"))
        })?
    } else {
        let matches: Vec<_> = list_identities(cert_dir)?
            .into_iter()
            .filter(|r| r.label == needle || r.request_id.as_deref() == Some(needle))
            .collect();
        match matches.len() {
            1 => matches.into_iter().next().unwrap(),
            0 => {
                return Err(CallerError::Config(format!(
                    "no peer identity found for {needle}"
                )))
            }
            _ => {
                return Err(CallerError::Config(format!(
                    "multiple peer identities match {needle}; use fingerprint"
                )))
            }
        }
    };
    record.status = PeerIdentityStatus::Revoked;
    record.revoked_at_unix = Some(crate::peer::pairing::unix_timestamp());
    write_identity_record(cert_dir, &record)?;
    Ok(record)
}

pub fn fingerprint_der(der: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(der);
    let fp: [u8; 32] = hasher.finalize().into();
    let mut s = String::with_capacity(64);
    for byte in fp {
        s.push_str(&format!("{byte:02x}"));
    }
    s
}

pub fn fingerprint_pem(pem_text: &str) -> Result<String, CallerError> {
    let pem = pem::parse(pem_text.as_bytes())
        .map_err(|e| CallerError::Config(format!("parse certificate PEM: {e}")))?;
    Ok(fingerprint_der(pem.contents()))
}

fn write_identity_record(cert_dir: &Path, record: &PeerIdentityRecord) -> Result<(), CallerError> {
    std::fs::create_dir_all(identities_dir(cert_dir))?;
    let body = serde_json::to_string_pretty(record)?;
    std::fs::write(identity_path(cert_dir, &record.fingerprint), body)?;
    Ok(())
}

fn identities_dir(cert_dir: &Path) -> PathBuf {
    cert_dir.join(POLICY_DIR)
}

fn identity_path(cert_dir: &Path, fingerprint: &str) -> PathBuf {
    identities_dir(cert_dir).join(format!("{fingerprint}.json"))
}

fn nearest_existing_path(path: &Path) -> Option<PathBuf> {
    let mut current = path.to_path_buf();
    loop {
        if current.exists() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

fn normalize_fingerprint(raw: &str) -> Result<String, CallerError> {
    let fp = raw
        .trim()
        .chars()
        .filter(|c| *c != ':')
        .collect::<String>()
        .to_ascii_lowercase();
    let valid = fp.len() == 64 && fp.bytes().all(|b| b.is_ascii_hexdigit());
    if !valid {
        return Err(CallerError::Config(format!(
            "invalid certificate fingerprint {raw:?}"
        )));
    }
    Ok(fp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_permissions_downgrade_task_runner() {
        let task = ControlMsg::StartTask {
            session_id: None,
            task: "run".into(),
            orchestrate: None,
            direct: None,
            reference_frame_ids: Vec::new(),
            display_target: None,
            attachments: Vec::new(),
            follow_up_id: None,
        };
        let approval = ControlMsg::Approve {
            session_id: None,
            id: 7,
        };

        assert!(profile_allows_control_msg("task-runner", &task));
        assert!(!profile_allows_control_msg("task-runner", &approval));
    }

    #[test]
    fn profile_permissions_read_only_display_cannot_request_input() {
        let view = ControlMsg::WebRtcSignal {
            display_id: 0,
            session_id: "s".into(),
            signal: crate::peer::WebRtcSignal::Unknown,
        };
        let input = ControlMsg::RequestDisplayInputAuthority { display_id: 0 };

        assert!(profile_allows_control_msg("read-only-display", &view));
        assert!(!profile_allows_control_msg("read-only-display", &input));
        assert!(!profile_allows_federated_display_input("read-only-display"));
        assert!(profile_allows_federated_display_input("operator"));
    }

    #[test]
    fn peer_prefixed_profile_aliases_keep_legacy_permissions() {
        assert_eq!(profile_class("peer-operator"), ProfileClass::Operator);
        assert_eq!(profile_class("peer-root"), ProfileClass::AdminPeer);
        assert_eq!(profile_class("peer-daemon"), ProfileClass::AdminPeer);
        assert!(profile_allows_operation(
            "peer-root",
            PeerOperation::RuntimeControl
        ));
        assert!(!profile_allows_operation(
            "peer-operator",
            PeerOperation::RuntimeControl
        ));
    }

    #[test]
    fn identity_round_trip_and_revoke() {
        let tmp = tempfile::TempDir::new().unwrap();
        let fp = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let record = write_approved_identity(
            tmp.path(),
            fp,
            "peer-a",
            "operator",
            Some("https://peer/.well-known/agent-card.json"),
            Some("req-1"),
        )
        .unwrap();
        assert_eq!(record.status, PeerIdentityStatus::Approved);

        let loaded = lookup_identity(tmp.path(), fp).unwrap().unwrap();
        assert_eq!(loaded.profile, "operator");
        assert!(loaded.filesystem.is_empty());

        let revoked = revoke_identity(tmp.path(), "peer-a").unwrap();
        assert_eq!(revoked.status, PeerIdentityStatus::Revoked);
        assert!(revoked.revoked_at_unix.is_some());
    }

    #[test]
    fn filesystem_access_requires_explicit_roots() {
        assert!(profile_allows_operation(
            "admin-peer",
            PeerOperation::FilesystemRead
        ));
        let tmp = tempfile::TempDir::new().unwrap();
        let policy = FilesystemAccessPolicy::default();
        let denied =
            filesystem_access_allowed(&policy, FilesystemAccessKind::Read, tmp.path()).unwrap_err();
        assert!(denied.contains("no filesystem read roots"));
    }

    #[test]
    fn filesystem_access_allows_canonical_child() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().join("allowed");
        let child = root.join("nested").join("file.txt");
        std::fs::create_dir_all(child.parent().unwrap()).unwrap();
        std::fs::write(&child, b"ok").unwrap();

        let policy = FilesystemAccessPolicy {
            read_roots: vec![root],
            write_roots: Vec::new(),
        };
        filesystem_access_allowed(&policy, FilesystemAccessKind::Read, &child).unwrap();
    }

    #[test]
    fn filesystem_access_rejects_dotdot_escape() {
        let tmp = tempfile::TempDir::new().unwrap();
        let allowed = tmp.path().join("allowed");
        let secret = tmp.path().join("secret");
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(&secret).unwrap();
        let escaped = allowed.join("..").join("secret").join("file.txt");
        std::fs::write(secret.join("file.txt"), b"secret").unwrap();

        let policy = FilesystemAccessPolicy {
            read_roots: vec![allowed],
            write_roots: Vec::new(),
        };
        let denied =
            filesystem_access_allowed(&policy, FilesystemAccessKind::Read, &escaped).unwrap_err();
        assert!(denied.contains("outside"));
    }

    #[cfg(unix)]
    #[test]
    fn filesystem_access_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::TempDir::new().unwrap();
        let allowed = tmp.path().join("allowed");
        let secret = tmp.path().join("secret");
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(&secret).unwrap();
        std::fs::write(secret.join("file.txt"), b"secret").unwrap();
        symlink(&secret, allowed.join("secret-link")).unwrap();

        let policy = FilesystemAccessPolicy {
            read_roots: vec![allowed.clone()],
            write_roots: Vec::new(),
        };
        let denied = filesystem_access_allowed(
            &policy,
            FilesystemAccessKind::Read,
            &allowed.join("secret-link").join("file.txt"),
        )
        .unwrap_err();
        assert!(denied.contains("outside"));
    }
}
