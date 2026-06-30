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
            "enforce_user_client_grants": false
        },
        "enforcement": {
            "user_client_grants": false,
            "principal_binding": "root_session_only",
            "reason": "The daemon currently distinguishes trusted owner/root dashboard sessions and daemon peer identities, but not stable scoped human/device principals."
        }
    })
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
                "transport_id": "transport:future-user-client-binding",
                "source": if grant.source.is_empty() { "local_iam_state" } else { grant.source.as_str() },
                "status": if grant.status.is_empty() { "draft" } else { grant.status.as_str() },
                "enforced": false,
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
            permissions: vec![
                "access.inspect".to_string(),
                "access.manage".to_string(),
                "peer.inspect".to_string(),
                "peer.manage".to_string(),
            ],
            source: "builtin".to_string(),
        },
        IamRole {
            id: "role:scoped-human".to_string(),
            label: "Scoped human".to_string(),
            status: "planned".to_string(),
            summary: "Future user/client IAM role once stable browser/passkey principals are bound to daemon requests.".to_string(),
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
}
