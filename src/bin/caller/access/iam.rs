//! Local Access/IAM state.
//!
//! This is deliberately a data model, not an enforcement engine. Today the
//! daemon can distinguish trusted owner/root dashboard sessions and daemon peer
//! identities. It cannot yet bind every browser/passkey request to a stable
//! scoped human/device principal, so local IAM grants loaded here are exposed as
//! managed/draft model data until that binding exists.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{AccessError, AccessResult};

pub const IAM_STATE_FILE: &str = "iam.json";
pub const IAM_SCHEMA_VERSION: u32 = 1;

fn default_schema_version() -> u32 {
    IAM_SCHEMA_VERSION
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LocalIamState {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub principals: Vec<IamPrincipal>,
    #[serde(default)]
    pub roles: Vec<IamRole>,
    #[serde(default)]
    pub grants: Vec<IamGrant>,
    #[serde(default)]
    pub audit_events: Vec<IamAuditEvent>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IamPrincipal {
    pub id: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub account: Option<Value>,
    #[serde(default)]
    pub organization: Option<Value>,
    #[serde(default)]
    pub authn: Vec<Value>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub created_at_unix_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IamRole {
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default)]
    pub source: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IamGrant {
    pub id: String,
    pub principal_id: String,
    #[serde(default)]
    pub target_id: String,
    #[serde(default)]
    pub role_id: String,
    #[serde(default)]
    pub policy_id: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub created_at_unix_ms: Option<u64>,
    #[serde(default)]
    pub revoked_at_unix_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IamAuditEvent {
    pub id: String,
    #[serde(default)]
    pub at_unix_ms: Option<u64>,
    #[serde(default)]
    pub actor_principal_id: String,
    #[serde(default)]
    pub action: String,
    #[serde(default)]
    pub target_id: String,
    #[serde(default)]
    pub summary: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum IamStateStatus {
    Missing,
    Loaded,
    Error(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct LoadedIamState {
    pub path: PathBuf,
    pub state: LocalIamState,
    pub status: IamStateStatus,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AccessPrincipal {
    pub id: String,
    pub kind: String,
    pub label: String,
    pub source: String,
    pub role_id: String,
    #[serde(default)]
    pub grant_id: Option<String>,
    #[serde(default)]
    pub transport: String,
    #[serde(default)]
    pub peer_profile: Option<String>,
    #[serde(default)]
    pub account: Option<Value>,
    #[serde(default)]
    pub authn: Vec<Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AccessDecision {
    pub allowed: bool,
    pub principal_id: String,
    pub principal_kind: String,
    pub permission: String,
    pub reason: String,
}

impl AccessPrincipal {
    pub fn root_dashboard_session(source: impl Into<String>, transport: impl Into<String>) -> Self {
        Self {
            id: "principal:root:dashboard".to_string(),
            kind: "root_session".to_string(),
            label: "Root dashboard session".to_string(),
            source: source.into(),
            role_id: "role:root".to_string(),
            grant_id: Some("grant:root:dashboard".to_string()),
            transport: transport.into(),
            peer_profile: None,
            account: None,
            authn: Vec::new(),
        }
    }

    pub fn peer_daemon(
        fingerprint: impl Into<String>,
        label: impl Into<String>,
        profile: impl Into<String>,
        transport: impl Into<String>,
    ) -> Self {
        let fingerprint = fingerprint.into();
        let profile = profile.into();
        let label = label.into();
        Self {
            id: format!("principal:peer-daemon:{fingerprint}"),
            kind: "peer_daemon".to_string(),
            label: if label.trim().is_empty() {
                fingerprint.clone()
            } else {
                label
            },
            source: "peer_identity_store".to_string(),
            role_id: format!("role:peer-profile:{profile}"),
            grant_id: Some(format!("grant:peer-profile:{fingerprint}")),
            transport: transport.into(),
            peer_profile: Some(profile),
            account: None,
            authn: Vec::new(),
        }
    }

    pub fn local_user_client(
        principal: &IamPrincipal,
        grant: &IamGrant,
        transport: impl Into<String>,
    ) -> Self {
        let role_id = if grant.role_id.trim().is_empty() {
            "role:scoped-human".to_string()
        } else {
            grant.role_id.clone()
        };
        let kind = if principal.kind.trim().is_empty() {
            "human_user".to_string()
        } else {
            principal.kind.clone()
        };
        Self {
            id: principal.id.clone(),
            kind,
            label: if principal.label.trim().is_empty() {
                principal.id.clone()
            } else {
                principal.label.clone()
            },
            source: if principal.source.trim().is_empty() {
                "local_iam_state".to_string()
            } else {
                principal.source.clone()
            },
            role_id,
            grant_id: Some(grant.id.clone()),
            transport: transport.into(),
            peer_profile: None,
            account: principal.account.clone(),
            authn: principal.authn.clone(),
        }
    }

    pub fn as_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or_else(|_| json!({}))
    }
}

impl AccessDecision {
    pub fn allowed(
        principal: &AccessPrincipal,
        op: crate::peer::access_policy::PeerOperation,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            allowed: true,
            principal_id: principal.id.clone(),
            principal_kind: principal.kind.clone(),
            permission: operation_permission_id(op).to_string(),
            reason: reason.into(),
        }
    }

    pub fn denied(
        principal: &AccessPrincipal,
        op: crate::peer::access_policy::PeerOperation,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            allowed: false,
            principal_id: principal.id.clone(),
            principal_kind: principal.kind.clone(),
            permission: operation_permission_id(op).to_string(),
            reason: reason.into(),
        }
    }

    pub fn ensure_allowed(self) -> Result<(), String> {
        if self.allowed {
            Ok(())
        } else {
            Err(self.reason)
        }
    }
}

impl IamStateStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Loaded => "loaded",
            Self::Error(_) => "error",
        }
    }

    pub fn error(&self) -> Option<&str> {
        match self {
            Self::Error(err) => Some(err.as_str()),
            _ => None,
        }
    }
}

impl Default for LocalIamState {
    fn default() -> Self {
        Self {
            schema_version: IAM_SCHEMA_VERSION,
            principals: Vec::new(),
            roles: builtin_role_templates(),
            grants: Vec::new(),
            audit_events: Vec::new(),
        }
    }
}

impl LocalIamState {
    fn normalize(mut self) -> Self {
        if self.schema_version == 0 {
            self.schema_version = IAM_SCHEMA_VERSION;
        }
        for role in builtin_role_templates() {
            if !self.roles.iter().any(|existing| existing.id == role.id) {
                self.roles.push(role);
            }
        }
        self.principals.retain(|p| !p.id.trim().is_empty());
        self.roles.retain(|r| !r.id.trim().is_empty());
        self.grants
            .retain(|g| !g.id.trim().is_empty() && !g.principal_id.trim().is_empty());
        self.audit_events.retain(|e| !e.id.trim().is_empty());
        self
    }

    pub fn managed_principal_count(&self) -> usize {
        self.principals
            .iter()
            .filter(|p| p.source != "builtin")
            .count()
    }

    pub fn managed_grant_count(&self) -> usize {
        self.grants.iter().filter(|g| g.source != "builtin").count()
    }
}

pub fn iam_state_path(cert_dir: &Path) -> PathBuf {
    cert_dir.join(IAM_STATE_FILE)
}

pub fn load_state(cert_dir: &Path) -> AccessResult<LocalIamState> {
    let path = iam_state_path(cert_dir);
    if !path.exists() {
        return Ok(LocalIamState::default());
    }
    let contents = std::fs::read_to_string(&path)
        .map_err(|e| AccessError(format!("read {}: {e}", path.display())))?;
    let state: LocalIamState = serde_json::from_str(&contents)
        .map_err(|e| AccessError(format!("parse {}: {e}", path.display())))?;
    Ok(state.normalize())
}

pub fn load_state_for_overview(cert_dir: &Path) -> LoadedIamState {
    let path = iam_state_path(cert_dir);
    if !path.exists() {
        return LoadedIamState {
            path,
            state: LocalIamState::default(),
            status: IamStateStatus::Missing,
        };
    }
    match load_state(cert_dir) {
        Ok(state) => LoadedIamState {
            path,
            state,
            status: IamStateStatus::Loaded,
        },
        Err(err) => LoadedIamState {
            path,
            state: LocalIamState::default(),
            status: IamStateStatus::Error(err.to_string()),
        },
    }
}

#[allow(dead_code)]
pub fn save_state(cert_dir: &Path, state: &LocalIamState) -> AccessResult<()> {
    std::fs::create_dir_all(cert_dir)?;
    let path = iam_state_path(cert_dir);
    let tmp = path.with_extension("json.tmp");
    let normalized = state.clone().normalize();
    let mut contents = serde_json::to_vec_pretty(&normalized)
        .map_err(|e| AccessError(format!("serialize {}: {e}", path.display())))?;
    contents.push(b'\n');
    std::fs::write(&tmp, contents)?;
    set_private_perms(&tmp)?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        AccessError(format!(
            "rename {} to {}: {e}",
            tmp.display(),
            path.display()
        ))
    })?;
    Ok(())
}

pub fn overview_metadata(load: &LoadedIamState) -> Value {
    json!({
        "schema_version": load.state.schema_version,
        "state_path": load.path.display().to_string(),
        "load_status": load.status.as_str(),
        "load_error": load.status.error(),
        "managed_principals": load.state.managed_principal_count(),
        "managed_grants": load.state.managed_grant_count(),
        "roles": load.state.roles.clone(),
        "audit_events": load.state.audit_events.clone(),
        "capabilities": {
            "state_file_supported": true,
            "read_local_state": true,
            "write_api_available": false,
            "operation_evaluator": true,
            "enforce_root_and_peer_grants": true,
            "enforce_user_client_grants": true
        },
        "enforcement": {
            "root_session_grants": true,
            "peer_profile_grants": true,
            "user_client_grants": true,
            "principal_binding": "root_peer_and_local_user_client",
            "enforced_principal_kinds": ["root_session", "peer_daemon", "human_user", "browser_certificate", "connect_account"],
            "reason": "The daemon enforces trusted owner/root dashboard sessions, daemon peer profiles, and active local IAM user/client grants when requests bind to browser mTLS or Connect account identities."
        }
    })
}

pub fn evaluate_principal_operation(
    principal: &AccessPrincipal,
    op: crate::peer::access_policy::PeerOperation,
) -> AccessDecision {
    match principal.kind.as_str() {
        "root_session" => AccessDecision::allowed(
            principal,
            op,
            "root dashboard session grants all operations",
        ),
        "peer_daemon" => {
            let Some(profile) = principal.peer_profile.as_deref() else {
                return AccessDecision::denied(
                    principal,
                    op,
                    "peer daemon principal has no profile",
                );
            };
            if crate::peer::access_policy::profile_allows_operation(profile, op) {
                AccessDecision::allowed(
                    principal,
                    op,
                    format!(
                        "peer profile {profile} allows {}",
                        operation_permission_id(op)
                    ),
                )
            } else {
                AccessDecision::denied(
                    principal,
                    op,
                    format!(
                        "peer profile {profile} does not allow {}",
                        operation_permission_id(op)
                    ),
                )
            }
        }
        _ => AccessDecision::denied(
            principal,
            op,
            "scoped user/client principal requires local IAM state evaluation",
        ),
    }
}

pub fn evaluate_principal_operation_with_state(
    state: &LocalIamState,
    principal: &AccessPrincipal,
    op: crate::peer::access_policy::PeerOperation,
) -> AccessDecision {
    if matches!(principal.kind.as_str(), "root_session" | "peer_daemon") {
        return evaluate_principal_operation(principal, op);
    }

    let Some(grant_id) = principal.grant_id.as_deref() else {
        return AccessDecision::denied(principal, op, "principal has no local IAM grant");
    };
    let Some(grant) = state.grants.iter().find(|grant| grant.id == grant_id) else {
        return AccessDecision::denied(
            principal,
            op,
            format!("local IAM grant {grant_id} was not found"),
        );
    };
    if grant.principal_id != principal.id {
        return AccessDecision::denied(
            principal,
            op,
            format!(
                "local IAM grant {} belongs to {}",
                grant.id, grant.principal_id
            ),
        );
    }
    if !is_enforced_status(&grant.status) {
        return AccessDecision::denied(
            principal,
            op,
            format!("local IAM grant {} is not active", grant.id),
        );
    }

    let Some(principal_record) = state
        .principals
        .iter()
        .find(|record| record.id == principal.id)
    else {
        return AccessDecision::denied(
            principal,
            op,
            format!("local IAM principal {} was not found", principal.id),
        );
    };
    if !is_enforced_status(&principal_record.status) {
        return AccessDecision::denied(
            principal,
            op,
            format!("local IAM principal {} is not active", principal.id),
        );
    }

    let role_id = if grant.role_id.trim().is_empty() {
        "role:scoped-human"
    } else {
        grant.role_id.as_str()
    };
    let Some(role) = state.roles.iter().find(|role| role.id == role_id) else {
        return AccessDecision::denied(
            principal,
            op,
            format!("local IAM role {role_id} was not found"),
        );
    };
    let permission = operation_permission_id(op);
    if role
        .permissions
        .iter()
        .any(|candidate| candidate == permission)
    {
        AccessDecision::allowed(
            principal,
            op,
            format!("local IAM role {role_id} allows {permission}"),
        )
    } else {
        AccessDecision::denied(
            principal,
            op,
            format!("local IAM role {role_id} does not allow {permission}"),
        )
    }
}

pub fn operation_permission_id(op: crate::peer::access_policy::PeerOperation) -> &'static str {
    use crate::peer::access_policy::PeerOperation;
    match op {
        PeerOperation::PresenceRead => "presence.read",
        PeerOperation::StatsRead => "stats.read",
        PeerOperation::DisplayView => "display.view",
        PeerOperation::DisplayInput => "display.input",
        PeerOperation::Message => "message.send",
        PeerOperation::Task => "task.run",
        PeerOperation::Approval => "approval.resolve",
        PeerOperation::AccessInspect => "access.inspect",
        PeerOperation::AccessManage => "access.manage",
        PeerOperation::PeerInspect => "peer.inspect",
        PeerOperation::PeerManage => "peer.manage",
        PeerOperation::SessionInspect => "session.inspect",
        PeerOperation::SessionManage => "session.manage",
        PeerOperation::Terminal => "terminal.use",
        PeerOperation::Settings => "settings.manage",
        PeerOperation::RuntimeControl => "runtime.control",
        PeerOperation::FilesystemRead => "filesystem.read",
        PeerOperation::FilesystemWrite => "filesystem.write",
    }
}

pub fn principal_overview_values(state: &LocalIamState) -> Vec<Value> {
    state
        .principals
        .iter()
        .map(|principal| {
            json!({
                "id": principal.id.clone(),
                "kind": if principal.kind.is_empty() { "human_user" } else { principal.kind.as_str() },
                "kind_label": principal_kind_label(&principal.kind),
                "label": if principal.label.is_empty() { principal.id.as_str() } else { principal.label.as_str() },
                "source": if principal.source.is_empty() { "local_iam_state" } else { principal.source.as_str() },
                "status": if principal.status.is_empty() { "draft" } else { principal.status.as_str() },
                "local": false,
                "account": principal.account.clone(),
                "organization": principal.organization.clone(),
                "authn": principal.authn.clone(),
                "notes": principal.notes.clone(),
                "created_at_unix_ms": principal.created_at_unix_ms
            })
        })
        .collect()
}

pub fn grant_overview_values(state: &LocalIamState, default_target_id: &str) -> Vec<Value> {
    state
        .grants
        .iter()
        .map(|grant| {
            let role_id = if grant.role_id.is_empty() {
                "role:scoped-human"
            } else {
                grant.role_id.as_str()
            };
            json!({
                "id": grant.id.clone(),
                "principal_id": grant.principal_id.clone(),
                "target_id": if grant.target_id.is_empty() { default_target_id } else { grant.target_id.as_str() },
                "kind": "user_client_local_iam",
                "kind_label": "Local IAM user/client grant",
                "policy_id": if grant.policy_id.is_empty() { "policy:scoped-human" } else { grant.policy_id.as_str() },
                "role": role_id,
                "role_label": role_label(state, role_id),
                "transport_id": "transport:local-user-client-binding",
                "source": if grant.source.is_empty() { "local_iam_state" } else { grant.source.as_str() },
                "status": if grant.status.is_empty() { "draft" } else { grant.status.as_str() },
                "enforced": is_enforced_status(&grant.status),
                "reason": grant.reason.clone(),
                "created_at_unix_ms": grant.created_at_unix_ms,
                "revoked_at_unix_ms": grant.revoked_at_unix_ms
            })
        })
        .collect()
}

fn builtin_role_templates() -> Vec<IamRole> {
    vec![
        IamRole {
            id: "role:root".to_string(),
            label: "Root".to_string(),
            status: "enforced".to_string(),
            summary: "Current owner/root dashboard authority.".to_string(),
            permissions: root_permission_ids(),
            source: "builtin".to_string(),
        },
        IamRole {
            id: "role:peer-profile".to_string(),
            label: "Peer profile".to_string(),
            status: "enforced".to_string(),
            summary: "Daemon-to-daemon grants enforced by the approved peer identity profile."
                .to_string(),
            permissions: vec![
                "presence.read".to_string(),
                "stats.read".to_string(),
                "display.view".to_string(),
                "display.input".to_string(),
                "message.send".to_string(),
                "task.run".to_string(),
                "approval.resolve".to_string(),
                "access.inspect".to_string(),
                "peer.inspect".to_string(),
                "peer.manage".to_string(),
                "session.inspect".to_string(),
                "session.manage".to_string(),
                "terminal.use".to_string(),
                "settings.manage".to_string(),
                "runtime.control".to_string(),
                "filesystem.read".to_string(),
                "filesystem.write".to_string(),
            ],
            source: "builtin".to_string(),
        },
        IamRole {
            id: "role:scoped-human".to_string(),
            label: "Scoped human".to_string(),
            status: "enforced".to_string(),
            summary: "Minimal user/client IAM role for stable browser mTLS and Connect account request bindings.".to_string(),
            permissions: vec!["access.inspect".to_string()],
            source: "builtin".to_string(),
        },
        IamRole {
            id: "role:directory-files".to_string(),
            label: "Directory scoped files".to_string(),
            status: "planned".to_string(),
            summary: "Future file role bounded by selected roots and operations.".to_string(),
            permissions: vec!["filesystem.read".to_string()],
            source: "builtin".to_string(),
        },
    ]
}

fn root_permission_ids() -> Vec<String> {
    [
        "presence.read",
        "stats.read",
        "display.view",
        "display.input",
        "message.send",
        "task.run",
        "approval.resolve",
        "access.inspect",
        "access.manage",
        "peer.inspect",
        "peer.manage",
        "session.inspect",
        "session.manage",
        "terminal.use",
        "settings.manage",
        "runtime.control",
        "filesystem.read",
        "filesystem.write",
    ]
    .iter()
    .map(|permission| (*permission).to_string())
    .collect()
}

fn principal_kind_label(kind: &str) -> &'static str {
    match kind {
        "browser_certificate" => "Browser certificate",
        "passkey_account" => "Passkey account",
        "human_user" | "" => "Human user",
        "organization_group" => "Organization group",
        _ => "IAM principal",
    }
}

fn role_label(state: &LocalIamState, role_id: &str) -> String {
    state
        .roles
        .iter()
        .find(|role| role.id == role_id)
        .map(|role| {
            if role.label.is_empty() {
                role.id.clone()
            } else {
                role.label.clone()
            }
        })
        .unwrap_or_else(|| role_id.to_string())
}

pub fn is_enforced_status(status: &str) -> bool {
    matches!(
        status.trim().to_ascii_lowercase().as_str(),
        "active" | "enforced"
    )
}

pub fn principal_for_browser_mtls_cert(
    state: &LocalIamState,
    fingerprint: &str,
    transport: impl Into<String>,
) -> Option<AccessPrincipal> {
    principal_for_authn(
        state,
        "browser_mtls_cert",
        "fingerprint",
        fingerprint,
        transport,
    )
}

pub fn principal_for_connect_account(
    state: &LocalIamState,
    user_id: &str,
    account_name: Option<&str>,
    transport: impl Into<String>,
) -> Option<AccessPrincipal> {
    let transport = transport.into();
    principal_for_authn(
        state,
        "connect_account",
        "user_id",
        user_id,
        transport.clone(),
    )
    .or_else(|| {
        account_name.and_then(|name| {
            principal_for_authn(state, "connect_account", "account_name", name, transport)
        })
    })
}

fn principal_for_authn(
    state: &LocalIamState,
    authn_kind: &str,
    key: &str,
    value: &str,
    transport: impl Into<String>,
) -> Option<AccessPrincipal> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let principal = state.principals.iter().find(|principal| {
        is_enforced_status(&principal.status)
            && principal.authn.iter().any(|authn| {
                authn.get("kind").and_then(Value::as_str) == Some(authn_kind)
                    && authn.get(key).and_then(Value::as_str) == Some(value)
            })
    })?;
    let grant = state
        .grants
        .iter()
        .find(|grant| grant.principal_id == principal.id && is_enforced_status(&grant.status))?;
    Some(AccessPrincipal::local_user_client(
        principal, grant, transport,
    ))
}

#[allow(dead_code)]
fn set_private_perms(path: &Path) -> AccessResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(path, perms)?;
    }
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_state_loads_default_foundation() {
        let tmp = tempfile::TempDir::new().unwrap();
        let loaded = load_state_for_overview(tmp.path());

        assert_eq!(loaded.status, IamStateStatus::Missing);
        assert_eq!(loaded.state.schema_version, IAM_SCHEMA_VERSION);
        assert!(loaded.state.roles.iter().any(|r| r.id == "role:root"));
    }

    #[test]
    fn save_load_round_trips_managed_records() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut state = LocalIamState::default();
        state.principals.push(IamPrincipal {
            id: "principal:human:alice".to_string(),
            kind: "human_user".to_string(),
            label: "Alice".to_string(),
            status: "draft".to_string(),
            source: "local_iam_state".to_string(),
            account: None,
            organization: None,
            authn: Vec::new(),
            notes: Some("not enforced yet".to_string()),
            created_at_unix_ms: Some(123),
        });
        state.grants.push(IamGrant {
            id: "grant:alice:local:scoped".to_string(),
            principal_id: "principal:human:alice".to_string(),
            target_id: "local".to_string(),
            role_id: "role:scoped-human".to_string(),
            policy_id: "policy:scoped-human".to_string(),
            status: "draft".to_string(),
            source: "local_iam_state".to_string(),
            reason: "example".to_string(),
            created_at_unix_ms: Some(124),
            revoked_at_unix_ms: None,
        });

        save_state(tmp.path(), &state).unwrap();
        let loaded = load_state(tmp.path()).unwrap();

        assert_eq!(loaded.managed_principal_count(), 1);
        assert_eq!(loaded.managed_grant_count(), 1);
        assert!(iam_state_path(tmp.path()).exists());
    }

    #[test]
    fn malformed_state_reports_error_for_overview() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(iam_state_path(tmp.path()), b"{not json").unwrap();

        let loaded = load_state_for_overview(tmp.path());

        assert!(matches!(loaded.status, IamStateStatus::Error(_)));
        assert_eq!(loaded.state.managed_grant_count(), 0);
    }

    #[test]
    fn overview_values_mark_local_iam_grants_unenforced() {
        let mut state = LocalIamState::default();
        state.principals.push(IamPrincipal {
            id: "principal:human:alice".to_string(),
            kind: "human_user".to_string(),
            label: "Alice".to_string(),
            status: "draft".to_string(),
            source: "local_iam_state".to_string(),
            account: None,
            organization: None,
            authn: Vec::new(),
            notes: None,
            created_at_unix_ms: None,
        });
        state.grants.push(IamGrant {
            id: "grant:alice".to_string(),
            principal_id: "principal:human:alice".to_string(),
            target_id: String::new(),
            role_id: "role:scoped-human".to_string(),
            policy_id: String::new(),
            status: String::new(),
            source: String::new(),
            reason: String::new(),
            created_at_unix_ms: None,
            revoked_at_unix_ms: None,
        });

        let grants = grant_overview_values(&state, "local-daemon");

        assert_eq!(grants[0]["target_id"], "local-daemon");
        assert_eq!(grants[0]["status"], "draft");
        assert_eq!(grants[0]["enforced"], false);
    }

    fn active_browser_cert_state() -> LocalIamState {
        let mut state = LocalIamState::default();
        state.principals.push(IamPrincipal {
            id: "principal:browser-cert:fp123".to_string(),
            kind: "browser_certificate".to_string(),
            label: "Alice laptop browser".to_string(),
            status: "active".to_string(),
            source: "local_iam_state".to_string(),
            account: None,
            organization: None,
            authn: vec![json!({
                "kind": "browser_mtls_cert",
                "fingerprint": "fp123"
            })],
            notes: None,
            created_at_unix_ms: Some(100),
        });
        state.grants.push(IamGrant {
            id: "grant:browser-cert:fp123:inspect".to_string(),
            principal_id: "principal:browser-cert:fp123".to_string(),
            target_id: "local".to_string(),
            role_id: "role:scoped-human".to_string(),
            policy_id: "policy:local-user-client".to_string(),
            status: "active".to_string(),
            source: "local_iam_state".to_string(),
            reason: "test scoped browser certificate".to_string(),
            created_at_unix_ms: Some(101),
            revoked_at_unix_ms: None,
        });
        state
    }

    #[test]
    fn active_browser_cert_binding_uses_local_role_permissions() {
        let state = active_browser_cert_state();
        let principal = principal_for_browser_mtls_cert(&state, "fp123", "https").unwrap();

        assert_eq!(principal.kind, "browser_certificate");
        assert_eq!(
            principal.grant_id.as_deref(),
            Some("grant:browser-cert:fp123:inspect")
        );
        assert!(
            evaluate_principal_operation_with_state(
                &state,
                &principal,
                crate::peer::access_policy::PeerOperation::AccessInspect,
            )
            .allowed
        );
        assert!(
            !evaluate_principal_operation_with_state(
                &state,
                &principal,
                crate::peer::access_policy::PeerOperation::AccessManage,
            )
            .allowed
        );
    }

    #[test]
    fn draft_browser_cert_binding_is_not_resolved() {
        let mut state = active_browser_cert_state();
        state.principals[0].status = "draft".to_string();

        assert!(principal_for_browser_mtls_cert(&state, "fp123", "https").is_none());
    }

    #[test]
    fn root_principal_allows_every_current_operation() {
        let principal = AccessPrincipal::root_dashboard_session("test", "dashboard-control");
        for op in [
            crate::peer::access_policy::PeerOperation::PresenceRead,
            crate::peer::access_policy::PeerOperation::StatsRead,
            crate::peer::access_policy::PeerOperation::DisplayView,
            crate::peer::access_policy::PeerOperation::DisplayInput,
            crate::peer::access_policy::PeerOperation::Message,
            crate::peer::access_policy::PeerOperation::Task,
            crate::peer::access_policy::PeerOperation::Approval,
            crate::peer::access_policy::PeerOperation::AccessInspect,
            crate::peer::access_policy::PeerOperation::AccessManage,
            crate::peer::access_policy::PeerOperation::PeerInspect,
            crate::peer::access_policy::PeerOperation::PeerManage,
            crate::peer::access_policy::PeerOperation::SessionInspect,
            crate::peer::access_policy::PeerOperation::SessionManage,
            crate::peer::access_policy::PeerOperation::Terminal,
            crate::peer::access_policy::PeerOperation::Settings,
            crate::peer::access_policy::PeerOperation::RuntimeControl,
            crate::peer::access_policy::PeerOperation::FilesystemRead,
            crate::peer::access_policy::PeerOperation::FilesystemWrite,
        ] {
            assert!(
                evaluate_principal_operation(&principal, op).allowed,
                "{op:?} should be allowed for root principal"
            );
        }
    }

    #[test]
    fn peer_principal_uses_peer_profile_permissions() {
        let principal =
            AccessPrincipal::peer_daemon("abc123", "peer", "peer-operator", "dashboard-control");

        assert!(
            evaluate_principal_operation(
                &principal,
                crate::peer::access_policy::PeerOperation::DisplayView,
            )
            .allowed
        );
        assert!(
            !evaluate_principal_operation(
                &principal,
                crate::peer::access_policy::PeerOperation::AccessManage,
            )
            .allowed
        );
    }
}
